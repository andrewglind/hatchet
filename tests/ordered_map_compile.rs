//! Compile + run gate for **`@orderedMap`** — a `Map<K,V>` field stored as two
//! insertion-ordered parallel `std::vector`s (`m_keys`/`m_vals`) instead of a
//! `std::map` (key-sorted, and fragile on VC6). Exercises construction, `set`
//! (find-or-append, in-place replace preserving order), `get`/`exists`/`remove`,
//! `keys()`, and both `for (k => v in m)` / `for (v in m)` iteration plus a
//! comprehension. The point of the representation is **insertion order**, which a
//! `std::map` cannot give — the test asserts it.
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

const DEMO: &str = r#"package demo;

class JV {
	public var s:String;
	public function new(v:String) { this.s = v; }
}

class Bag {
	@orderedMap public var object:Map<String, JV>;
	public function new() { this.object = new Map(); }

	public function set(k:String, v:JV):Void { this.object.set(k, v); }
	public function get(k:String):JV { return this.object.get(k); }
	public function has(k:String):Bool { return this.object.exists(k); }
	public function drop(k:String):Bool { return this.object.remove(k); }

	// `for (k => v in m)` — insertion order
	public function dump():String {
		var out = "";
		for (k => v in this.object) { out += k + "=" + v.s + ";"; }
		return out;
	}

	// `for (v in m)` binds the value (Haxe iterates a map's values)
	public function valsCsv():String {
		var out = "";
		for (v in this.object) { out += v.s + ","; }
		return out;
	}

	// comprehension over the ordered map
	public function keyList():Array<String> {
		return [for (k => v in this.object) k];
	}
}
"#;

const MAIN_CPP: &str = r#"#include <stdio.h>
#include "Demo.h"
int main() {
	demo::Bag b;
	b.set("alpha", new demo::JV("1"));
	b.set("beta",  new demo::JV("2"));
	b.set("gamma", new demo::JV("3"));
	b.set("beta",  new demo::JV("9")); // replace in place, keep position
	b.drop("gamma");                   // remove from both vectors
	printf("dump=%s\n", b.dump().c_str());
	printf("vals=%s\n", b.valsCsv().c_str());
	printf("get=%s has=%d miss=%d\n", b.get("alpha")->s.c_str(),
	       (int)b.has("beta"), (int)b.has("zzz"));
	std::vector<std::string> ks = b.keyList();
	printf("keys=");
	for (size_t i = 0; i < ks.size(); ++i) printf("%s,", ks[i].c_str());
	printf("\n");
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
fn ordered_map_compiles_and_runs() {
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++ compiler");
        return;
    };

    let root = std::env::temp_dir().join(format!("hatchet_ordmap_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("Demo.hx"), DEMO).unwrap();
    let main_cpp = root.join("main.cpp");
    std::fs::write(&main_cpp, MAIN_CPP).unwrap();

    let out = root.join("out");
    let gen_ok = Command::new(env!("CARGO_BIN_EXE_hatchet"))
        .arg("--src")
        .arg(&src)
        .arg("--out")
        .arg(&out)
        .arg("--force")
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(gen_ok, "transpiling the @orderedMap demo failed");

    // No std::map: the field is two parallel vectors.
    let demo_cpp = std::fs::read_to_string(cpp_files(&out).first().expect("a .cpp")).unwrap();
    let demo_h =
        std::fs::read_to_string(cpp_files(&out).first().unwrap().with_extension("h")).unwrap();
    assert!(
        demo_h.contains("std::vector<std::string> object_keys")
            && demo_h.contains("object_vals"),
        "expected parallel key/value vectors, not a std::map:\n{demo_h}"
    );
    assert!(
        !demo_cpp.contains("std::map<std::string, JV"),
        "the @orderedMap field must not lower to a std::map:\n{demo_cpp}"
    );

    let exe = out.join(if cfg!(windows) { "om.exe" } else { "om" });
    let mut cc = Command::new(&gxx);
    cc.args(["-std=c++98", "-pedantic", "-Wall"])
        .arg("-I")
        .arg(&out)
        .arg(&main_cpp);
    for f in cpp_files(&out) {
        cc.arg(f);
    }
    let compile = cc.arg("-o").arg(&exe).output().expect("run g++");
    assert!(
        compile.status.success(),
        "the @orderedMap demo did not compile under g++ -std=c++98:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&exe).output().expect("run the demo");
    let stdout = String::from_utf8_lossy(&run.stdout).to_string();
    let _ = std::fs::remove_dir_all(&root);

    // Insertion order preserved; `beta` replaced in place; `gamma` removed.
    for expected in [
        "dump=alpha=1;beta=9;",
        "vals=1,9,",
        "get=1 has=1 miss=0",
        "keys=alpha,beta,",
    ] {
        assert!(
            stdout.contains(expected),
            "missing `{expected}` in output:\n{stdout}"
        );
    }
}
