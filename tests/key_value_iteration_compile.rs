//! Compile + run gate for **key-value iteration** (`for (key => value in expr)`,
//! https://haxe.org/manual/expression-for.html#key-value-iteration). Covers all
//! four shapes: statement and comprehension forms, each over an Array (the key is
//! the `Int` index) and a Map (the key is the map's key type). The Map cases also
//! exercise iterating a `const&` Map *parameter* — which must use a
//! `const_iterator` — then compiles the generated C++ under `g++ -std=c++98`.
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

class KV {
	public function new() {}

	// Statement, Array key-value: the key is the Int index.
	public function sumIndexed(xs:Array<Int>):Int {
		var total = 0;
		for (i => x in xs) total += i * x;
		return total;
	}

	// Statement, Map key-value over a `const&` Map parameter.
	public function sumValues(m:Map<String, Int>):Int {
		var total = 0;
		for (k => v in m) total += v;
		return total;
	}

	// Comprehension, Array key-value: collect the indices.
	public function indicesOf(xs:Array<String>):Array<Int> {
		return [for (i => x in xs) i];
	}

	// Comprehension, Map key-value over a `const&` Map parameter → a new Map.
	public function incremented(m:Map<String, Int>):Map<String, Int> {
		return [for (k => v in m) k => v + 1];
	}
}
"#;

const MAIN_CPP: &str = r#"#include <stdio.h>
#include "lib/KV.h"
using namespace lib;
int main() {
	KV kv;
	std::vector<int> xs;
	xs.push_back(10); xs.push_back(20); xs.push_back(30);
	printf("sumIndexed=%d\n", kv.sumIndexed(xs)); // 0*10 + 1*20 + 2*30

	std::map<std::string, int> m;
	m["a"] = 1; m["b"] = 2;
	printf("sumValues=%d\n", kv.sumValues(m)); // 3

	std::vector<std::string> ss;
	ss.push_back("x"); ss.push_back("y");
	std::vector<int> idx = kv.indicesOf(ss);
	printf("indices=%d,%d,%d\n", (int)idx.size(), idx[0], idx[1]);

	std::map<std::string, int> inc = kv.incremented(m);
	printf("incremented=%d,%d\n", inc["a"], inc["b"]); // 2,3
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
fn key_value_iteration_compiles_and_runs() {
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++ compiler");
        return;
    };

    let root = std::env::temp_dir().join(format!("hatchet_kviter_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let lib = root.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("KV.hx"), SRC).unwrap();
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
    assert!(gen_ok, "transpiling the key-value demo failed");

    let exe = out.join(if cfg!(windows) {
        "kviter.exe"
    } else {
        "kviter"
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
        "the key-value demo did not compile under g++ -std=c++98:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&exe).output().expect("run the key-value demo");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let _ = std::fs::remove_dir_all(&root);

    assert!(
        stdout.contains("sumIndexed=80"),
        "statement Array key-value wrong:\n{stdout}"
    );
    assert!(
        stdout.contains("sumValues=3"),
        "statement Map key-value wrong:\n{stdout}"
    );
    assert!(
        stdout.contains("indices=2,0,1"),
        "comprehension Array key-value wrong:\n{stdout}"
    );
    assert!(
        stdout.contains("incremented=2,3"),
        "comprehension Map key-value wrong:\n{stdout}"
    );
}
