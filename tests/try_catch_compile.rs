//! Compile + run gate for `try`/`catch` emission. Synthesises a small Haxe program
//! that throws and catches a `String`, a class instance, and an `Int` (via a
//! `Dynamic` catch-all), transpiles it with the built `hatchet` binary, compiles
//! the output under `g++ -std=c++98`, runs it, and checks the results.
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

const ERR_HX: &str = r#"package lib;
class Err {
	public var msg(default, null):String;
	public function new(msg:String) { this.msg = msg; }
}
"#;

const DEMO_HX: &str = r#"package lib;
import lib.Err;

class Demo {
	public function new() {}

	// throw + catch a String (the throw is coerced to std::string).
	public function str(bad:Bool):String {
		try {
			if (bad) throw "boom";
			return "ok";
		} catch (e:String) {
			return "caught:" + e;
		}
	}

	// throw + catch a class instance (caught by pointer).
	public function obj(bad:Bool):String {
		try {
			if (bad) throw new Err("nope");
			return "fine";
		} catch (e:Err) {
			return "err:" + e.msg;
		}
	}

	// a Dynamic catch → the non-binding catch(...).
	public function any(bad:Bool):String {
		try {
			if (bad) throw 42;
			return "noerr";
		} catch (e:Dynamic) {
			return "any";
		}
	}
}
"#;

const MAIN_CPP: &str = r#"#include <stdio.h>
#include "lib/Demo.h"
int main() {
	lib::Demo d;
	printf("%s|%s\n", d.str(false).c_str(), d.str(true).c_str());
	printf("%s|%s\n", d.obj(false).c_str(), d.obj(true).c_str());
	printf("%s|%s\n", d.any(false).c_str(), d.any(true).c_str());
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
fn try_catch_compiles_and_runs() {
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++ compiler");
        return;
    };

    let root = std::env::temp_dir().join(format!("hatchet_trycatch_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let lib = root.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("Err.hx"), ERR_HX).unwrap();
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
    assert!(gen_ok, "transpiling the try/catch demo failed");

    let exe = out.join(if cfg!(windows) {
        "trycatch.exe"
    } else {
        "trycatch"
    });
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
        "the try/catch demo did not compile under g++ -std=c++98:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&exe).output().expect("run the try/catch demo");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let _ = std::fs::remove_dir_all(&root);

    assert!(
        stdout.contains("ok|caught:boom"),
        "String throw/catch wrong:\n{stdout}"
    );
    assert!(
        stdout.contains("fine|err:nope"),
        "class throw/catch wrong:\n{stdout}"
    );
    assert!(
        stdout.contains("noerr|any"),
        "Dynamic catch-all wrong:\n{stdout}"
    );
}
