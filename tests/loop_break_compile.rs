//! Compile + run gate for Haxe's `break`-in-`switch` semantics: Haxe `switch`
//! has no break of its own, so a `break` in a case body exits the enclosing
//! *loop*. A bare C++ `break` inside the generated `switch` would exit only the
//! switch — Hatchet routes it through a hoisted flag checked after the switch.
//! Covers: the plain case, `continue` (which C++ gets right natively), a nested
//! loop inside a case (whose `break` binds to itself), and a switch nested in
//! another switch's case (the flags chain).
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

const BRK_HX: &str = r#"package lib;

class Brk {
	public function new() {}

	public function scan(items:Array<Int>):Int {
		var n = 0;
		for (i in items) {
			switch (i) {
				case 0: break;       // Haxe: breaks the LOOP
				case 1: continue;    // skips the n++ below
				case 2:
					for (j in 0...10) {
						if (j == 3) break;   // breaks the INNER loop only
						n++;
					}
				default: n += i;
			}
			n++;
		}
		return n;
	}

	public function nested(items:Array<Int>):Int {
		var n = 0;
		for (i in items) {
			switch (i) {
				case 1:
					switch (n) {
						case 0: break;   // switch-in-switch: still breaks the LOOP
						default: n += 100;
					}
				default: n += i;
			}
			n++;
		}
		return n;
	}
}
"#;

const MAIN_CPP: &str = r#"#include <stdio.h>
#include "lib/Brk.h"
int main() {
	lib::Brk b;
	std::vector<int> v;
	v.push_back(5); v.push_back(1); v.push_back(2);
	v.push_back(7); v.push_back(0); v.push_back(9);
	/* 5: +5,+1=6; 1: continue; 2: inner +3, then +1=10; 7: +7,+1=18; 0: loop break; 9: never */
	printf("scan=%d\n", b.scan(v));
	std::vector<int> w;
	w.push_back(1); w.push_back(9);
	/* i=1, n=0: inner case 0 -> chained flags -> loop break; 9: never */
	printf("nested=%d\n", b.nested(w));
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
fn loop_breaks_in_switches_compile_and_run() {
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++ compiler");
        return;
    };

    let root = std::env::temp_dir().join(format!("hatchet_brk_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let lib = root.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("Brk.hx"), BRK_HX).unwrap();
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
    assert!(gen_ok, "transpiling the loop-break demo failed");

    let exe = out.join(if cfg!(windows) { "brk.exe" } else { "brk" });
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
        "the loop-break demo did not compile under g++ -std=c++98:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&exe)
        .output()
        .expect("run the loop-break demo");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let _ = std::fs::remove_dir_all(&root);

    assert!(
        stdout.contains("scan=18"),
        "loop-bound break/continue semantics wrong:\n{stdout}"
    );
    assert!(
        stdout.contains("nested=0"),
        "chained switch-in-switch break wrong:\n{stdout}"
    );
}
