//! Compile + run gate for **`return []` with a by-value container return type**.
//! An empty array literal returned from a function returning `Array<T>`
//! (`std::vector<...>` by value) used to lower to `return NULL;`, which does not
//! compile — you cannot return `NULL` from a function returning a vector by
//! value. It must default-construct an empty container instead.
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

class Bag {
	public function new() {}

	// `return []` for a by-value `Array<Int>` (`std::vector<int>`): must
	// default-construct, not `return NULL`.
	public function empty():Array<Int> {
		return [];
	}

	// A populated return, so the same path that builds a vector is exercised too.
	public function some():Array<Int> {
		return [1, 2, 3];
	}
}
"#;

const MAIN_CPP: &str = r#"#include <stdio.h>
#include "lib/Bag.h"
using namespace lib;
int main() {
	Bag b;
	std::vector<int> e = b.empty();
	std::vector<int> s = b.some();
	printf("empty=%d\n", (int)e.size());
	printf("some=%d,%d\n", (int)s.size(), s[2]);
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
fn empty_array_return_compiles_and_runs() {
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++ compiler");
        return;
    };

    let root = std::env::temp_dir().join(format!("hatchet_emptyarr_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let lib = root.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("Bag.hx"), SRC).unwrap();
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
    assert!(gen_ok, "transpiling the empty-array demo failed");

    let exe = out.join(if cfg!(windows) {
        "emptyarr.exe"
    } else {
        "emptyarr"
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
        "the empty-array demo did not compile under g++ -std=c++98:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&exe)
        .output()
        .expect("run the empty-array demo");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let _ = std::fs::remove_dir_all(&root);

    assert!(
        stdout.contains("empty=0"),
        "empty array return wrong:\n{stdout}"
    );
    assert!(
        stdout.contains("some=3,3"),
        "populated array return wrong:\n{stdout}"
    );
}
