//! Compile + run gate for null-safe navigation collapsed with null-coalescing:
//! `recv?.method() ?? default` (and `recv?.field ?? default`). This is the
//! `anachrjsonistic` `Proxy` idiom — an `abstract` over a reference type whose
//! accessors read `(this?.isX() ?? false) ? … : …`. The regression: the safe call
//! was lowered to the discardable comma form `(recv != NULL ? (call, 0) : 0)`,
//! which threw the navigated value away and always read `0`/`false`, so every
//! accessor returned an empty/default result. The value form
//! `(recv != NULL ? recv->method() : default)` must flow the real result through.
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
        Command::new(c)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

const SRC: &str = r#"package lib;

class Node {
	public var v:Int;
	public function new(v:Int) { this.v = v; }
	public function value():Int { return v; }
	public function positive():Bool { return v > 0; }
}

// An abstract over a reference type — `this` is the underlying `Node*`, so
// `this?.positive()` is a genuine NULL-guarded call whose Bool result must survive.
abstract Box(Node) {
	public function new(n:Node) { this = n; }

	// `recv?.method() ?? default` used as a condition.
	public function read():Int {
		return (this?.positive() ?? false) ? this.value() : -1;
	}

	// `recv?.method() ?? default` used as the whole return value.
	public function isPos():Bool {
		return this?.positive() ?? false;
	}
}
"#;

const MAIN_CPP: &str = r#"#include <stdio.h>
#include "lib/Box.h"
using namespace lib;
int main() {
	Box pos(new Node(42));
	Box neg(new Node(-7));
	Box nul((Node*)0);
	printf("%d %d %d %d %d %d\n",
		pos.read(), neg.read(), nul.read(),
		(int)pos.isPos(), (int)neg.isPos(), (int)nul.isPos());
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
fn safe_nav_coalesce_flows_the_value_through() {
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++ compiler");
        return;
    };

    let root = std::env::temp_dir().join(format!("hatchet_nullco_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let lib = root.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("Box.hx"), SRC).unwrap();
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
    assert!(gen_ok, "transpiling the safe-nav demo failed");

    // The generated condition must keep the call result, not discard it via `(…, 0)`.
    let body = std::fs::read_to_string(out.join("lib").join("Box.cpp")).unwrap();
    assert!(
        !body.contains(", 0)"),
        "safe call was lowered to the discardable comma form (value thrown away):\n{body}"
    );
    assert!(
        body.contains("this->__this != NULL ? this->__this->positive()"),
        "expected the value form of the safe call:\n{body}"
    );

    let exe = out.join(if cfg!(windows) { "nullco.exe" } else { "nullco" });
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
        "the safe-nav demo did not compile under g++ -std=c++98:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&exe).output().expect("run the safe-nav demo");
    let stdout = String::from_utf8_lossy(&run.stdout).to_string();
    let _ = std::fs::remove_dir_all(&root);

    // read(): 42 (positive→value), -1 (negative→else), -1 (null→else)
    // isPos(): 1 (positive), 0 (negative), 0 (null)
    assert!(
        stdout.contains("42 -1 -1 1 0 0"),
        "safe-nav/coalesce read the wrong values:\n{stdout}"
    );
}
