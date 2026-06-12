//! Compile + run gate for `String`-subject `switch` lowering (→ an `if`/`else if`
//! chain). Like the other compile gates it synthesises a small Haxe program,
//! transpiles it with the built `hatchet` binary, compiles the output under
//! `g++ -std=c++98`, runs it, and checks the results.
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

const DEMO_HX: &str = r#"package lib;

class Demo {
	public function new() {}

	public function classify(s:String):Int {
		switch (s) {
			case "one": return 1;
			case "two", "deux": return 2;   // multi-pattern
			case "three": return 3;
			default: return -1;
		}
	}

	public function tag(s:String):String {
		switch (s) {
			case "a": return "A";
			case "b": return "B";
		}
		return "?";   // no default in the switch
	}
}
"#;

const MAIN_CPP: &str = r#"#include <stdio.h>
#include "lib/Demo.h"
int main() {
	lib::Demo d;
	printf("%d%d%d%d%d\n",
		d.classify("one"), d.classify("two"), d.classify("deux"),
		d.classify("three"), d.classify("zzz"));
	printf("%s%s%s\n", d.tag("a").c_str(), d.tag("b").c_str(), d.tag("x").c_str());
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
fn string_switch_compiles_and_runs() {
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++ compiler");
        return;
    };

    let root = std::env::temp_dir().join(format!("hatchet_strsw_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let lib = root.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("Demo.hx"), DEMO_HX).unwrap();
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
    assert!(gen_ok, "transpiling the string-switch demo failed");

    let exe = out.join(if cfg!(windows) { "strsw.exe" } else { "strsw" });
    let mut cmd = Command::new(&gxx);
    cmd.args(["-std=c++98", "-pedantic", "-Wall"]).arg("-I").arg(&out).arg(&main_cpp);
    for f in cpp_files(&out) {
        cmd.arg(f);
    }
    cmd.arg("-o").arg(&exe);
    let compile = cmd.output().expect("run g++");
    assert!(
        compile.status.success(),
        "the string-switch demo did not compile under g++ -std=c++98:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&exe).output().expect("run the string-switch demo");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let _ = std::fs::remove_dir_all(&root);

    // one=1, two=2, deux=2 (multi-pattern), three=3, zzz=-1 (default).
    assert!(stdout.contains("1223-1"), "string switch dispatch wrong:\n{stdout}");
    // a=A, b=B, x falls through the default-less switch to "?".
    assert!(stdout.contains("AB?"), "default-less string switch wrong:\n{stdout}");
}
