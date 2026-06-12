//! Compile + run gate for plain module-level free functions (`function f(...) {...}`
//! — not the `final NAME = (...) -> ...` lambda form). Exercises a public function
//! used cross-file, a public function calling a private (file-local `static`) one,
//! and a `var` whose type is inferred from a free-function return.
//!
//! Skipped (passes vacuously) when no C++ compiler is available.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Locate a C++98 compiler: `$HATCHET_GXX`, else `g++` on `PATH`, else the MSYS2
/// mingw32 default. Returns `None` if none can be run.
fn find_gxx() -> Option<String> {
    let candidates = [
        std::env::var("HATCHET_GXX").ok(),
        Some("g++".to_string()),
        Some(r"C:\msys64\mingw32\bin\g++.exe".to_string()),
    ];
    candidates.into_iter().flatten().find(|c| {
        Command::new(c).arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
    })
}

const GEOM_HX: &str = r#"package lib;

typedef Vec2 = { x:Float, y:Float };

// public module-level function — declared in the header, callable cross-file
function makeVec(x:Float, y:Float):Vec2 {
	return { x: x, y: y };
}

// public function calling a private (file-local) helper
function distance(a:Vec2, b:Vec2):Float {
	return Math.sqrt(sq(a.x - b.x) + sq(a.y - b.y));
}

// private → emitted `static` in the .cpp, not in the header
private function sq(v:Float):Float {
	return v * v;
}
"#;

const USER_HX: &str = r#"package lib;
import lib.Geom;

class User {
	public function new() {}
	public function dist():Float {
		var a = makeVec(0.0, 0.0);   // var type inferred from the free fn's return
		var b = makeVec(3.0, 4.0);
		return distance(a, b);        // 5.0
	}
}
"#;

const MAIN_CPP: &str = r#"#include <stdio.h>
#include "lib/User.h"
int main() {
	lib::User u;
	printf("%.1f\n", u.dist());
	return 0;
}
"#;

/// Every generated `.cpp` under `dir`, recursively.
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
fn module_level_functions_compile_and_run() {
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++ compiler");
        return;
    };

    let root = std::env::temp_dir().join(format!("hatchet_modfn_compile_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let lib = root.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("Geom.hx"), GEOM_HX).unwrap();
    std::fs::write(lib.join("User.hx"), USER_HX).unwrap();
    let main_cpp = root.join("main.cpp");
    std::fs::write(&main_cpp, MAIN_CPP).unwrap();

    let out = root.join("out");
    let gen_ok = Command::new(env!("CARGO_BIN_EXE_hatchet"))
        .arg("--src")
        .arg(&lib)
        .arg("--out")
        .arg(&out)
        .arg("--force")
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(gen_ok, "transpiling the module-function demo failed");

    let exe = out.join(if cfg!(windows) { "modfn.exe" } else { "modfn" });
    let mut cmd = Command::new(&gxx);
    cmd.args(["-std=c++98", "-pedantic", "-Wall"]).arg("-I").arg(&out).arg(&main_cpp);
    for f in cpp_files(&out) {
        cmd.arg(f);
    }
    cmd.arg("-o").arg(&exe);
    let compile = cmd.output().expect("run g++");
    assert!(
        compile.status.success(),
        "the module-function demo did not compile under g++ -std=c++98:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&exe).output().expect("run the module-function demo");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let _ = std::fs::remove_dir_all(&root);

    assert!(stdout.trim() == "5.0", "module functions wrong: {stdout:?}");
}
