//! Compile + run gate for **value-position `if`-expressions** — the feature the
//! real `Json.hx` port needs for an array comprehension whose body is an
//! `if`/`else if`/`else` (each branch a block ending in a value). Exercises the
//! comprehension body, a no-`else` filter, and a plain value `if` in assignment
//! position, then compiles the generated C++ under `g++ -std=c++98` and runs it.
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

class Maker {
	public function new() {}

	// Array comprehension whose body is an if / else-if / else, each branch a
	// block ending in a trailing value expression — the shape `Json.hx` uses.
	public function classify(nums:Array<Int>):Array<Int> {
		return [
			for (n in nums)
				if (n < 0) {
					-1;
				} else if (n == 0) {
					var z = 0;
					z;
				} else {
					var p = n * 2;
					p;
				}
		];
	}

	// A leading `if` with no `else` stays a *filter* — only positives survive.
	public function positives(nums:Array<Int>):Array<Int> {
		return [for (n in nums) if (n > 0) n];
	}

	// A value `if`-expression in plain assignment position.
	public function pick(flag:Bool):String {
		var s = if (flag) "yes" else "no";
		return s;
	}
}
"#;

const MAIN_CPP: &str = r#"#include <stdio.h>
#include "lib/Maker.h"
using namespace lib;
int main() {
	Maker m;
	std::vector<int> in;
	in.push_back(-5); in.push_back(0); in.push_back(3);
	std::vector<int> out = m.classify(in);
	printf("classify=%d,%d,%d\n", out[0], out[1], out[2]);
	std::vector<int> pos = m.positives(in);
	printf("positives=%d,%d\n", (int)pos.size(), pos[0]);
	printf("pick=%s,%s\n", m.pick(true).c_str(), m.pick(false).c_str());
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
fn value_if_expressions_compile_and_run() {
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++ compiler");
        return;
    };

    let root = std::env::temp_dir().join(format!("hatchet_ifexpr_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let lib = root.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("Maker.hx"), SRC).unwrap();
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
    assert!(gen_ok, "transpiling the value-if demo failed");

    let exe = out.join(if cfg!(windows) { "ifexpr.exe" } else { "ifexpr" });
    let mut cmd = Command::new(&gxx);
    cmd.args(["-std=c++98", "-pedantic", "-Wall"]).arg("-I").arg(&out).arg(&main_cpp);
    for f in cpp_files(&out) {
        cmd.arg(f);
    }
    cmd.arg("-o").arg(&exe);
    let compile = cmd.output().expect("run g++");
    assert!(
        compile.status.success(),
        "the value-if demo did not compile under g++ -std=c++98:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&exe).output().expect("run the value-if demo");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let _ = std::fs::remove_dir_all(&root);

    assert!(stdout.contains("classify=-1,0,6"), "if/else-if/else comprehension body wrong:\n{stdout}");
    assert!(stdout.contains("positives=1,3"), "no-else filter wrong:\n{stdout}");
    assert!(stdout.contains("pick=yes,no"), "value if-expression wrong:\n{stdout}");
}
