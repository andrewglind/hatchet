//! Compile + run gate for the Tier-1 standard-library lowerings (`String.substr`/
//! `substring`, `StringBuf`, `StringTools`, `Std.random`, and the extra `Array`
//! methods). Like `example_compile`, it needs nothing outside the repository: it
//! synthesises a small Haxe program in a temp dir, transpiles it with the built
//! `hatchet` binary, compiles the output under `g++ -std=c++98`, runs it, and
//! checks the printed results — so it validates that the generated C++ both
//! compiles *and* behaves.
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

	public function strings():String {
		var s = "hello world";
		var a = s.substr(0, 5);           // "hello"
		var b = s.substr(-5);             // "world"
		var c = s.substring(6, 11);       // "world"
		var d = s.substring(11, 6);       // swapped -> "world"
		var buf = new StringBuf();
		buf.add("x=");
		buf.add(42);
		buf.addChar(33);                  // '!'
		var rep = StringTools.replace("a.b.c", ".", "-");  // "a-b-c"
		var tr = StringTools.trim("  hi  ");               // "hi"
		var sw = StringTools.startsWith("foobar", "foo");  // true
		var ew = StringTools.endsWith("foobar", "bar");    // true
		var hx = StringTools.hex(255, 4);                  // "00FF"
		return a + "|" + b + "|" + c + "|" + d + "|" + buf.toString() + "|" + rep + "|" + tr + "|" + (sw ? "T" : "F") + (ew ? "T" : "F") + "|" + hx;
	}

	public function arrays():String {
		var xs = [1, 2, 3];
		var ys = [4, 5];
		var cat = xs.concat(ys);          // [1,2,3,4,5]
		var sl = cat.slice(1, 4);         // [2,3,4]
		var first = cat.shift();          // 1, cat=[2,3,4,5]
		cat.unshift(9);                   // [9,2,3,4,5]
		var li = [7, 3, 7, 1].lastIndexOf(7); // 2
		// filter (predicate lambda) then sort (comparator lambda, in-place).
		var fs = [5, 2, 8, 1].filter(n -> n > 1); // [5, 2, 8]
		fs.sort((a, b) -> a - b);                 // [2, 5, 8]
		return Std.string(cat.length) + "|" + Std.string(sl.length) + "|" + Std.string(first) + "|" + Std.string(li) + "|" + fs.join(",");
	}
}
"#;

const MAIN_CPP: &str = r#"#include <stdio.h>
#include <string>
#include "lib/Demo.h"
int main() {
	lib::Demo d;
	printf("%s\n", d.strings().c_str());
	printf("%s\n", d.arrays().c_str());
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
fn stdlib_lowerings_compile_and_run() {
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++ compiler");
        return;
    };

    // Lay out a tiny project (`lib/Demo.hx` + a hand-written `main.cpp`).
    let root = std::env::temp_dir().join(format!("hatchet_stdlib_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let lib = root.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("Demo.hx"), DEMO_HX).unwrap();
    let main_cpp = root.join("main.cpp");
    std::fs::write(&main_cpp, MAIN_CPP).unwrap();

    // Transpile with the built binary.
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
    assert!(gen_ok, "transpiling the stdlib demo failed");

    // Compile the generated C++ together with the entry point.
    let exe = out.join(if cfg!(windows) { "stdlib.exe" } else { "stdlib" });
    let mut cmd = Command::new(&gxx);
    cmd.args(["-std=c++98", "-pedantic", "-Wall"]).arg("-I").arg(&out).arg(&main_cpp);
    for f in cpp_files(&out) {
        cmd.arg(f);
    }
    cmd.arg("-o").arg(&exe);
    let compile = cmd.output().expect("run g++");
    assert!(
        compile.status.success(),
        "the stdlib demo did not compile under g++ -std=c++98:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    // Run it and check behaviour.
    let run = Command::new(&exe).output().expect("run the stdlib demo");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let _ = std::fs::remove_dir_all(&root);

    assert!(
        stdout.contains("hello|world|world|world|x=42!|a-b-c|hi|TT|00FF"),
        "string lowerings wrong:\n{stdout}"
    );
    assert!(stdout.contains("5|3|1|2|2,5,8"), "array lowerings wrong:\n{stdout}");
}
