//! `--header-only` cross-module ordering. The amalgamation topologically sorts the
//! `--src` modules so that whenever one needs a type from another **complete** — a
//! base class (`extends`/`implements`) or a value (non-pointer) field — the defining
//! module is emitted first, regardless of `--src` order. A genuine cross-module cycle
//! of such dependencies fails loud instead of emitting non-compiling C++.
//!
//! The compile+run case is skipped (passes vacuously) when no C++ compiler is found.

use std::path::{Path, PathBuf};
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

const POINT: &str = "package shapes;\ntypedef Point = { var x:Int; var y:Int; }\n";

const SHAPE: &str = r#"package shapes;
class Shape {
	public function new() {}
	public function area():Int { return 0; }
	public function name():String { return "shape"; }
}
"#;

// Depends on `Shape` (base, another file) and `Point` (by-value field, another file).
const CIRCLE: &str = r#"package shapes;
class Circle extends Shape {
	var r:Int;
	public var center:Point;
	public function new(r:Int, cx:Int, cy:Int) {
		super();
		this.r = r;
		this.center = { x: cx, y: cy };
	}
	override public function area():Int { return r * r * 3; }
	override public function name():String { return "circle"; }
}
"#;

const MAIN_CPP: &str = r#"#include <stdio.h>
#include "shapes.h"
using namespace shapes;
int main() {
	Shape* s = new Circle(2, 5, 7);
	Circle* c = (Circle*)s;
	printf("%s area=%d center=(%d,%d)\n", s->name().c_str(), s->area(), c->center.x, c->center.y);
	delete s;
	return 0;
}
"#;

fn cpp_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(cpp_files(&p));
            } else if p.extension().and_then(|s| s.to_str()) == Some("cpp") {
                out.push(p);
            }
        }
    }
    out
}

#[test]
fn cross_module_base_and_value_deps_are_ordered() {
    let root = std::env::temp_dir().join(format!("hatchet_ord_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let shapes = root.join("shapes");
    std::fs::create_dir_all(&shapes).unwrap();
    std::fs::write(shapes.join("Point.hx"), POINT).unwrap();
    std::fs::write(shapes.join("Shape.hx"), SHAPE).unwrap();
    std::fs::write(shapes.join("Circle.hx"), CIRCLE).unwrap();

    let out = root.join("out");
    // Pass the most-dependent module FIRST, so only the topological sort can fix it.
    let gen_ok = Command::new(env!("CARGO_BIN_EXE_hatchet"))
        .arg("--src")
        .arg(shapes.join("Circle.hx"))
        .arg(shapes.join("Shape.hx"))
        .arg(shapes.join("Point.hx"))
        .arg("--header-only")
        .arg("shapes")
        .arg("--out")
        .arg(&out)
        .arg("--force")
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(gen_ok, "transpiling the ordering demo failed");

    // The base `Shape` (and the value type `Point`) must be declared before `Circle`.
    let text = std::fs::read_to_string(out.join("shapes.h")).unwrap();
    let shape_at = text.find("class Shape ").expect("Shape declared");
    let circle_at = text.find("class Circle ").expect("Circle declared");
    let point_at = text.find("struct Point ").expect("Point declared");
    assert!(shape_at < circle_at, "base Shape must precede Circle:\n{text}");
    assert!(point_at < circle_at, "value type Point must precede Circle:\n{text}");

    let Some(gxx) = find_gxx() else {
        eprintln!("skipping compile/run: no C++ compiler");
        let _ = std::fs::remove_dir_all(&root);
        return;
    };
    let main_cpp = root.join("main.cpp");
    std::fs::write(&main_cpp, MAIN_CPP).unwrap();
    let exe = out.join(if cfg!(windows) { "ord.exe" } else { "ord" });
    let mut cmd = Command::new(&gxx);
    cmd.args(["-std=c++98", "-pedantic", "-Wall"]).arg("-I").arg(&out).arg(&main_cpp);
    for f in cpp_files(&out) {
        cmd.arg(f);
    }
    cmd.arg("-o").arg(&exe);
    let compile = cmd.output().expect("run g++");
    assert!(
        compile.status.success(),
        "the ordered amalgamation did not compile under g++ -std=c++98:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let run = Command::new(&exe).output().expect("run the ordering demo");
    let stdout = String::from_utf8_lossy(&run.stdout).to_string();
    let _ = std::fs::remove_dir_all(&root);
    assert!(
        stdout.contains("circle area=12 center=(5,7)"),
        "wrong result:\n{stdout}"
    );
}

#[test]
fn cross_module_dependency_cycle_fails_loud() {
    // `A` and `C` share a file; `C extends B` and `B extends A` (other file) — a
    // module-level cycle the topological sort cannot resolve. It must fail loud, not
    // emit non-compiling C++.
    let root = std::env::temp_dir().join(format!("hatchet_cyc_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let m = root.join("m");
    std::fs::create_dir_all(&m).unwrap();
    std::fs::write(
        m.join("Acore.hx"),
        "package m;\nclass A { public function new() {} }\nclass C extends B { public function new() { super(); } }\n",
    )
    .unwrap();
    std::fs::write(
        m.join("Bmid.hx"),
        "package m;\nclass B extends A { public function new() { super(); } }\n",
    )
    .unwrap();

    let out = root.join("out");
    let result = Command::new(env!("CARGO_BIN_EXE_hatchet"))
        .arg("--src")
        .arg(&m)
        .arg("--header-only")
        .arg("m")
        .arg("--out")
        .arg(&out)
        .arg("--force")
        .output()
        .expect("run hatchet");

    assert!(!result.status.success(), "a cross-module cycle must fail the run");
    let stderr = String::from_utf8_lossy(&result.stderr).to_string();
    assert!(
        stderr.contains("circular cross-module dependency"),
        "expected a clear cycle diagnostic, got:\n{stderr}"
    );
    assert!(!out.join("m.h").is_file(), "no header should be written on a cycle");
    let _ = std::fs::remove_dir_all(&root);
}
