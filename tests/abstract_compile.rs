//! Compile + run gate for `abstract` newtypes (Milestone 10): a value class
//! wrapping an underlying value, with `@:op` operators, `@:to` conversions, and
//! `@:from` converting constructors — all resolved by the C++ compiler from the
//! emitted operators. Exercises subscript, binary, and conversion operators
//! from hand-written C++, exactly as a consumer would.
//!
//! Skipped (passes vacuously) when no C++ compiler is available.

use std::path::{Path, PathBuf};
use std::process::Command;

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

const SRC: &str = r#"package lib;

typedef Vec2Data = { var x:Int; var y:Int; }

abstract Vec2(Vec2Data) {
	public function new(x:Int, y:Int) { this = { x: x, y: y }; }

	// `v[0]` / `v[1]` → x / y
	@:op([]) public function at(i:Int):Int { return i == 0 ? this.x : this.y; }

	// v1 + v2 (component sum's x as a quick scalar) → demonstrates binary @:op
	@:op(A + B) public function plus(o:Vec2):Int { return this.x + o.at(0); }

	// implicit conversions
	@:to public function toInt():Int { return this.x + this.y; }
	@:to public function toStr():String { return "vec"; }

	// converting constructor from a scalar
	@:from public static function ofScalar(n:Int):Vec2 { return new Vec2(n, n); }
}
"#;

const MAIN_CPP: &str = r#"#include <stdio.h>
#include "lib/Vec2.h"
using namespace lib;
int main() {
	Vec2 a(3, 4);
	Vec2 b(10, 20);
	printf("at=%d,%d\n", a[0], a[1]);          /* 3,4   via operator[] */
	printf("plus=%d\n", a + b);                /* 3 + 10 = 13  via operator+ */
	int sum = a;                               /* operator int → 7 */
	printf("sum=%d\n", sum);
	std::string s = a;                         /* operator std::string → "vec" */
	printf("s=%s\n", s.c_str());
	Vec2 d = 5;                                /* @:from converting ctor → (5,5) */
	printf("d=%d,%d\n", d[0], d[1]);
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
fn abstract_operators_and_conversions_compile_and_run() {
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++ compiler");
        return;
    };

    let root = std::env::temp_dir().join(format!("hatchet_abs_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let lib = root.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("Vec2.hx"), SRC).unwrap();
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
    assert!(gen_ok, "transpiling the abstract demo failed");

    // The abstract lowered to a value class with the operators/conversions.
    let header = std::fs::read_to_string(out.join("lib").join("Vec2.h")).unwrap();
    assert!(header.contains("Vec2Data __this;"), "underlying wrapped in __this:\n{header}");
    assert!(header.contains("operator[](int i)"), "@:op([]) → operator[]:\n{header}");
    assert!(header.contains("operator+(Vec2"), "@:op(A + B) → operator+:\n{header}");
    assert!(header.contains("operator int()"), "@:to Int → operator int:\n{header}");
    assert!(header.contains("operator std::string()"), "@:to String → conversion:\n{header}");

    let exe = out.join(if cfg!(windows) { "abs.exe" } else { "abs" });
    let mut cmd = Command::new(&gxx);
    cmd.args(["-std=c++98", "-pedantic", "-Wall"]).arg("-I").arg(&out).arg(&main_cpp);
    for f in cpp_files(&out) {
        cmd.arg(f);
    }
    cmd.arg("-o").arg(&exe);
    let compile = cmd.output().expect("run g++");
    assert!(
        compile.status.success(),
        "the abstract demo did not compile under g++ -std=c++98:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&exe).output().expect("run the abstract demo");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let _ = std::fs::remove_dir_all(&root);

    assert!(stdout.contains("at=3,4"), "subscript operator wrong:\n{stdout}");
    assert!(stdout.contains("plus=13"), "binary operator wrong:\n{stdout}");
    assert!(stdout.contains("sum=7"), "@:to int conversion wrong:\n{stdout}");
    assert!(stdout.contains("s=vec"), "@:to string conversion wrong:\n{stdout}");
    assert!(stdout.contains("d=5,5"), "@:from converting ctor wrong:\n{stdout}");
}
