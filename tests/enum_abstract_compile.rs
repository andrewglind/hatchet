//! Compile + run gate for `Int`-backed `enum abstract` lowering. Like
//! `stdlib_compile`, it synthesises a small Haxe program in a temp dir, transpiles
//! it with the built `hatchet` binary, compiles the output under `g++ -std=c++98`,
//! runs it, and checks the results — validating that the generated C++ enums (with
//! explicit values, auto-increment, sibling bit-flag expressions, and `switch`)
//! both compile *and* behave.
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

enum abstract Dir(Int) {
	var North;        // 0
	var East;         // 1
	var South;        // 2
	var West;         // 3
}

enum abstract Flag(Int) {
	var None = 0;
	var A = 1;
	var B = 2;
	var AB = A | B;     // 3
	var Shift = 1 << 4; // 16
}

// A String-backed enum abstract → a namespace of `static const std::string`.
enum abstract Suit(String) {
	var Hearts = "H";
	var Spades = "S";
}

// A Float-backed enum abstract → `static const double`.
enum abstract Ratio(Float) {
	var Half = 0.5;
	var Quarter = 0.25;
}

class Demo {
	public function new() {}

	public function name(d:Dir):String {
		switch (d) {
			case North: return "N";
			case East: return "E";
			case South: return "S";
			case West: return "W";
		}
		return "?";
	}

	public function flags():Int {
		var f = AB;
		if (f == Flag.AB) {
			return f + Shift;   // 3 + 16 = 19
		}
		return -1;
	}

	public function dirVal():Int {
		return South;   // 2
	}

	// String enum abstract: a `switch` on the String subject → if/else chain.
	public function suit(s:Suit):String {
		switch (s) {
			case Hearts: return "hearts";
			case Spades: return "spades";
			default: return "?";
		}
	}

	// Float enum abstract value.
	public function ratio():Float {
		return Half + Quarter;   // 0.75
	}
}
"#;

const MAIN_CPP: &str = r#"#include <stdio.h>
#include "lib/Demo.h"
int main() {
	lib::Demo d;
	printf("%s%s%s%s\n",
		d.name(lib::Dir_::North).c_str(), d.name(lib::Dir_::East).c_str(),
		d.name(lib::Dir_::South).c_str(), d.name(lib::Dir_::West).c_str());
	printf("%d %d\n", d.flags(), d.dirVal());
	printf("%s|%s|%.2f\n", d.suit(lib::Suit_::Hearts).c_str(), d.suit(lib::Suit_::Spades).c_str(), d.ratio());
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
fn int_enum_abstract_compiles_and_runs() {
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++ compiler");
        return;
    };

    let root = std::env::temp_dir().join(format!("hatchet_enumabs_{}", std::process::id()));
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
    assert!(gen_ok, "transpiling the enum-abstract demo failed");

    let exe = out.join(if cfg!(windows) { "enumabs.exe" } else { "enumabs" });
    let mut cmd = Command::new(&gxx);
    cmd.args(["-std=c++98", "-pedantic", "-Wall"]).arg("-I").arg(&out).arg(&main_cpp);
    for f in cpp_files(&out) {
        cmd.arg(f);
    }
    cmd.arg("-o").arg(&exe);
    let compile = cmd.output().expect("run g++");
    assert!(
        compile.status.success(),
        "the enum-abstract demo did not compile under g++ -std=c++98:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&exe).output().expect("run the enum-abstract demo");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let _ = std::fs::remove_dir_all(&root);

    // Auto-incremented Dir + switch dispatch.
    assert!(stdout.contains("NESW"), "enum auto-increment / switch wrong:\n{stdout}");
    // AB (A|B = 3) + Shift (1<<4 = 16) = 19; South = 2.
    assert!(stdout.contains("19 2"), "explicit values / bit-flag expressions wrong:\n{stdout}");
    // String-backed enum abstract (switch on the String subject) + Float-backed
    // (Half + Quarter = 0.75).
    assert!(stdout.contains("hearts|spades|0.75"), "String/Float enum abstract wrong:\n{stdout}");
}
