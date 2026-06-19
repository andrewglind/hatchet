//! Compile + run gate for a **recursive-by-value tree** built from `abstract`
//! newtypes — the value-semantic shape the hand-written JSON header uses: a
//! `JValue` wrapping a struct that holds `Array<JValue>`, with methods, composed
//! and queried entirely by value. This is the pattern the real `Json.hx` port
//! relies on, so it guards that abstracts cover the value-tree use case that the
//! retired `@value` tag used to serve. Asserts the generated C++ has **no**
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
        Command::new(c)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

const SRC: &str = r#"package lib;

enum JType { STRING; OBJECT; }

// The wrapped record. It holds `Array<JValue>`, so the value tree is recursive
// through a container (a `std::vector<JValue>`) — exactly as the C++ original.
typedef JValueData = {
	var type:JType;
	var s:String;
	var keys:Array<String>;
	var vals:Array<JValue>;
}

// A value type with methods, over the record. Inside its methods `this` *is* the
// underlying `JValueData`, so members are reached as `this.field`. Everything is
// fully encapsulated (no public fields), which is what lets an abstract stand in
// for a plain value class.
abstract JValue(JValueData) {
	public function new(s:String = "") {
		this = { type: STRING, s: s, keys: [], vals: [] };
	}

	public function makeObject():Void { this.type = OBJECT; }
	public function isObject():Bool { return this.type == OBJECT; }
	public function str():String { return this.s; }

	public function setKey(key:String, v:JValue):Void {
		this.keys.push(key);
		this.vals.push(v);
	}

	public function get(key:String):String {
		for (i in 0...this.keys.length) {
			if (this.keys[i] == key) return this.vals[i].str();
		}
		return "";
	}

	public function child(key:String):JValue {
		for (i in 0...this.keys.length) {
			if (this.keys[i] == key) return this.vals[i];
		}
		return new JValue();
	}
}
"#;

const MAIN_CPP: &str = r#"#include <stdio.h>
#include "lib/JValue.h"
using namespace lib;
int main() {
	JValue o;                  /* a value object, on the stack (STRING, empty) */
	o.makeObject();
	o.setKey("name", JValue("hatchy"));
	o.setKey("kind", JValue("value"));
	/* a nested value object, composed by value into another */
	JValue inner;
	inner.makeObject();
	inner.setKey("deep", JValue("ok"));
	o.setKey("child", inner);
	printf("name=%s kind=%s child.deep=%s\n",
	       o.get("name").c_str(), o.get("kind").c_str(),
	       o.child("child").get("deep").c_str());
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
fn abstract_recursive_value_tree_compiles_and_runs() {
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++ compiler");
        return;
    };

    let root = std::env::temp_dir().join(format!("hatchet_abstree_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let lib = root.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("JValue.hx"), SRC).unwrap();
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
    assert!(gen_ok, "transpiling the abstract value-tree demo failed");

    // Value semantics: no heap, no frees anywhere in the generated body.
    let body = std::fs::read_to_string(out.join("lib").join("JValue.cpp")).unwrap();
    assert!(
        !body.contains("new "),
        "a value class must not heap-allocate:\n{body}"
    );
    assert!(
        !body.contains("delete"),
        "a value class owns nothing to free:\n{body}"
    );
    let header = std::fs::read_to_string(out.join("lib").join("JValue.h")).unwrap();
    assert!(
        header.contains("std::vector<JValue>"),
        "recursive-by-value container:\n{header}"
    );
    assert!(
        !header.contains("virtual ~JValue"),
        "value class has no virtual destructor:\n{header}"
    );

    let exe = out.join(if cfg!(windows) {
        "abstree.exe"
    } else {
        "abstree"
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
        "the abstract value-tree demo did not compile under g++ -std=c++98:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&exe)
        .output()
        .expect("run the abstract value-tree demo");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let _ = std::fs::remove_dir_all(&root);

    assert!(
        stdout.contains("name=hatchy kind=value child.deep=ok"),
        "value-semantic compose/query round-trip wrong:\n{stdout}"
    );
}
