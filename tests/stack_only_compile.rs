//! Compile + run gate for `@value` value classes — a class with methods
//! that Hatchet emits as a C++ **value type** (stack, no `new`/heap/ownership).
//! Models the value-semantic shape the hand-written JSON header uses: a
//! recursive-by-value tree (`JValue` holding `Array<JValue>`) with methods,
//! composed and queried entirely by value. Asserts the generated C++ has **no**
//! `new`/`delete` for these types, that the value layout carries no vtable
//! pointer, and that it round-trips correctly.
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

enum JType { STRING; OBJECT; }

@value
class JValue {
	public var type:JType;
	public var s:String;
	public var keys:Array<String>;
	public var vals:Array<JValue>;   // recursive-by-value via a container

	public function new(s:String = "") {
		type = STRING; this.s = s; keys = []; vals = [];
	}
	public function setKey(key:String, v:JValue):Void {
		keys.push(key);
		vals.push(v);
	}
	public function get(key:String):String {
		for (i in 0...keys.length) if (keys[i] == key) return vals[i].s;
		return "";
	}
}

@value
class JObject {
	public var root:JValue;
	public function new() { root = new JValue(); root.type = OBJECT; }
	public function set(key:String, value:String):Void {
		root.setKey(key, new JValue(value));
	}
	public function get(key:String):String { return root.get(key); }
}
"#;

const MAIN_CPP: &str = r#"#include <stdio.h>
#include "lib/Json.h"
using namespace lib;
int main() {
	JObject o;                 /* a value object, on the stack */
	o.set("name", "hatchy");
	o.set("kind", "value");
	/* a nested value object, composed by value into another */
	JObject inner;
	inner.set("deep", "ok");
	o.root.setKey("child", inner.root);
	printf("name=%s kind=%s child.deep=%s\n",
	       o.get("name").c_str(), o.get("kind").c_str(),
	       o.root.vals[o.root.keys.size() - 1].get("deep").c_str());
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
fn stack_only_value_classes_compile_and_run() {
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++ compiler");
        return;
    };

    let root = std::env::temp_dir().join(format!("hatchet_so_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let lib = root.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("Json.hx"), SRC).unwrap();
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
    assert!(gen_ok, "transpiling the stack-only demo failed");

    // Value semantics: no heap, no frees anywhere in the generated body.
    let body = std::fs::read_to_string(out.join("lib").join("Json.cpp")).unwrap();
    assert!(!body.contains("new "), "a value class must not heap-allocate:\n{body}");
    assert!(!body.contains("delete"), "a value class owns nothing to free:\n{body}");
    let header = std::fs::read_to_string(out.join("lib").join("Json.h")).unwrap();
    assert!(header.contains("std::vector<JValue>"), "recursive-by-value container:\n{header}");
    assert!(!header.contains("virtual ~JValue"), "value class has no virtual destructor:\n{header}");

    let exe = out.join(if cfg!(windows) { "so.exe" } else { "so" });
    let mut cmd = Command::new(&gxx);
    cmd.args(["-std=c++98", "-pedantic", "-Wall"]).arg("-I").arg(&out).arg(&main_cpp);
    for f in cpp_files(&out) {
        cmd.arg(f);
    }
    cmd.arg("-o").arg(&exe);
    let compile = cmd.output().expect("run g++");
    assert!(
        compile.status.success(),
        "the stack-only demo did not compile under g++ -std=c++98:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&exe).output().expect("run the stack-only demo");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let _ = std::fs::remove_dir_all(&root);

    assert!(
        stdout.contains("name=hatchy kind=value child.deep=ok"),
        "value-semantic compose/query round-trip wrong:\n{stdout}"
    );
}
