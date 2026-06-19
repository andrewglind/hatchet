//! Compile + run gate for parameterized (algebraic) enums: the tagged value
//! class lowering. Verifies the C++ shape (tag struct, payload fields, static
//! factories) and the behavior: construction via bare and qualified variant
//! calls, `switch` destructuring with typed bindings and `_` skips, paramless
//! variants via factories, ADT values stored in arrays, and a value-position
//! `switch` over an ADT subject.
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
        Command::new(c)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

const OP_HX: &str = r#"package lib;

enum Op {
	Halt;
	Add(a:Int, b:Int);
	Scale(f:Float, label:String);
}

class Calc {
	public function new() {}

	public function eval(op:Op):Float {
		switch (op) {
			case Halt: return 0.0;
			case Add(a, b): return a + b;
			case Scale(f, _): return f * 10.0;
		}
		return -1.0;
	}

	public function name(op:Op):String {
		// value-position switch over an ADT subject
		var n = switch (op) {
			case Halt: "halt";
			case Add(_, _): "add";
			case Scale(_, label): label;
		};
		return n;
	}

	public function run():Float {
		var total = 0.0;
		total += eval(Add(1, 2));          // 3
		total += eval(Scale(0.5, "half")); // 5
		total += eval(Op.Halt);            // 0
		var ops:Array<Op> = [Halt, Add(10, 20)];
		for (o in ops) total += eval(o);   // 0 + 30
		return total;                      // 38
	}
}
"#;

const MAIN_CPP: &str = r#"#include <stdio.h>
#include "lib/Op.h"
int main() {
	lib::Calc c;
	printf("total=%g\n", c.run());
	printf("name=%s\n", c.name(lib::Op::Scale(1.0, "scaled")).c_str());
	/* structural equality: same tag + equal payload */
	int eq = lib::Op::Add(1, 2) == lib::Op::Add(1, 2)
		&& lib::Op::Add(1, 2) != lib::Op::Add(1, 3)
		&& lib::Op::Halt() == lib::Op::Halt()
		&& lib::Op::Halt() != lib::Op::Add(1, 2);
	printf("eq=%d\n", eq);
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
fn adt_enums_compile_and_run() {
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++ compiler");
        return;
    };

    let root = std::env::temp_dir().join(format!("hatchet_adt_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let lib = root.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("Op.hx"), OP_HX).unwrap();
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
    assert!(gen_ok, "transpiling the ADT demo failed");

    // Header shape: tag struct, per-variant payload fields, inline factories.
    let header = std::fs::read_to_string(out.join("lib").join("Op.h")).unwrap();
    assert!(
        header.contains("struct Op_ {"),
        "tag struct emitted:\n{header}"
    );
    assert!(
        header.contains("Op_::Enum kind;"),
        "value class holds the tag:\n{header}"
    );
    assert!(
        header.contains("int Add_a;"),
        "payload fields per variant:\n{header}"
    );
    assert!(
        header.contains("std::string Scale_label;"),
        "non-POD payloads are plain members (no union):\n{header}"
    );
    assert!(
        header.contains("static Op Add(int a, int b)"),
        "static factory per variant:\n{header}"
    );
    assert!(
        header.contains("bool operator==(const Op& o) const"),
        "structural equality emitted:\n{header}"
    );

    let exe = out.join(if cfg!(windows) { "adt.exe" } else { "adt" });
    let mut cmd = Command::new(&gxx);
    cmd.args(["-std=c++98", "-pedantic", "-Wall"])
        .arg("-I")
        .arg(&out)
        .arg(&main_cpp);
    for f in cpp_files(&out) {
        cmd.arg(f);
    }
    cmd.arg("-o").arg(&exe);
    let compile = cmd.output().expect("run g++");
    assert!(
        compile.status.success(),
        "the ADT demo did not compile under g++ -std=c++98:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&exe).output().expect("run the ADT demo");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let _ = std::fs::remove_dir_all(&root);

    // 3 + 5 + 0 + (0 + 30) — construction, destructuring, arrays of ADTs.
    assert!(
        stdout.contains("total=38"),
        "ADT construction/dispatch wrong:\n{stdout}"
    );
    // value-position switch binding a payload capture
    assert!(
        stdout.contains("name=scaled"),
        "value-position ADT switch wrong:\n{stdout}"
    );
    // structural equality / inequality
    assert!(
        stdout.contains("eq=1"),
        "ADT structural equality wrong:\n{stdout}"
    );
}
