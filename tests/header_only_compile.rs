//! Compile + run gate for `--header-only`: several `--src` modules amalgamated into
//! one self-contained header. Exercises (a) the prelude being inlined (no separate
//! `StdAfx.h`, no `#include "StdAfx.h"`), (b) no `.cpp` emission, (c) the global
//! forward-declaration block resolving a cross-module reference-type member, and
//! (d) the two-pass ordering — a method whose inline body *dereferences* a class
//! declared in another module must see that class's complete definition.
//!
//! Skipped (passes vacuously) when no C++ compiler is available.

use std::path::Path;
use std::process::Command;

fn find_gxx() -> Option<String> {
    let candidates = [
        std::env::var("HATCHET_GXX").ok(),
        Some("g++".to_string()),
        Some(r"C:\msys64\mingw32\bin\g++.exe".to_string()),
    ];
    candidates.into_iter().flatten().find(|c| {
        Command::new(c)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

const POINT: &str = r#"package shapes;

class Point {
	public var x:Int;
	public var y:Int;
	public function new(x:Int, y:Int) { this.x = x; this.y = y; }
}
"#;

// `Line` is passed to hatchet *before* `Point`, and `dx()` dereferences a `Point`
// (`this.b.x`) — so the amalgamation must emit all declarations before any body.
const LINE: &str = r#"package shapes;

class Line {
	var a:Point;
	var b:Point;
	public function new(a:Point, b:Point) { this.a = a; this.b = b; }
	public function dx():Int { return this.b.x - this.a.x; }
}
"#;

const MAIN_CPP: &str = r#"#include <stdio.h>
#include "Shapes.h"
int main() {
	shapes::Point p1(1, 2);
	shapes::Point p2(10, 20);
	shapes::Line line(&p1, &p2);
	printf("dx=%d\n", line.dx());
	return 0;
}
"#;

fn has_ext(dir: &Path, ext: &str) -> bool {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten().any(|e| {
                let p = e.path();
                (p.is_file() && p.extension().and_then(|s| s.to_str()) == Some(ext))
                    || (p.is_dir() && has_ext(&p, ext))
            })
        })
        .unwrap_or(false)
}

#[test]
fn header_only_amalgamation_compiles_and_runs() {
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++ compiler");
        return;
    };

    let root = std::env::temp_dir().join(format!("hatchet_honly_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let shapes = root.join("shapes");
    std::fs::create_dir_all(&shapes).unwrap();
    std::fs::write(shapes.join("Point.hx"), POINT).unwrap();
    std::fs::write(shapes.join("Line.hx"), LINE).unwrap();
    let main_cpp = root.join("main.cpp");
    std::fs::write(&main_cpp, MAIN_CPP).unwrap();

    let out = root.join("out");
    // Pass Line before Point on purpose, to exercise definition ordering.
    let gen_ok = Command::new(env!("CARGO_BIN_EXE_hatchet"))
        .arg("--src")
        .arg(shapes.join("Line.hx"))
        .arg(shapes.join("Point.hx"))
        .arg("--header-only")
        .arg("Shapes")
        .arg("--out")
        .arg(&out)
        .arg("--force")
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(gen_ok, "transpiling the header-only demo failed");

    // Exactly one self-contained header: `Shapes.h`, no `.cpp`, no `StdAfx.h`.
    let header = out.join("Shapes.h");
    assert!(header.is_file(), "Shapes.h was not generated");
    assert!(!has_ext(&out, "cpp"), "header-only must not emit any .cpp");
    assert!(!out.join("StdAfx.h").is_file(), "header-only must not emit StdAfx.h");
    let text = std::fs::read_to_string(&header).unwrap();
    assert!(
        !text.contains("#include \"StdAfx.h\""),
        "the prelude must be inlined, not #included:\n{text}"
    );
    assert!(
        text.contains("class Line;") && text.contains("class Point;"),
        "expected a global forward-declaration block:\n{text}"
    );

    let exe = out.join(if cfg!(windows) { "honly.exe" } else { "honly" });
    let compile = Command::new(&gxx)
        .args(["-std=c++98", "-pedantic", "-Wall"])
        .arg("-I")
        .arg(&out)
        .arg(&main_cpp)
        .arg("-o")
        .arg(&exe)
        .output()
        .expect("run g++");
    assert!(
        compile.status.success(),
        "the header-only demo did not compile under g++ -std=c++98:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&exe).output().expect("run the header-only demo");
    let stdout = String::from_utf8_lossy(&run.stdout).to_string();
    let _ = std::fs::remove_dir_all(&root);
    assert!(stdout.contains("dx=9"), "wrong result:\n{stdout}");
}
