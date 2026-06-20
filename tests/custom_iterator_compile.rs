//! Compile + run gate for **custom `Iterator`/`Iterable` iteration**. Beyond
//! ranges, `Array`, and `Map`, Hatchet now lowers a `for (x in e)` where `e`
//! implements the Haxe iteration protocol:
//!
//! * an **Iterator** — `e` itself has `hasNext():Bool` and `next():T`;
//! * an **Iterable** — `e` has `iterator():Iterator<T>`.
//!
//! Both become a `while (it.hasNext()) { T x = it.next(); … }` loop. When the
//! iterator is a heap (reference-type) object produced by `iterator()`, the loop
//! owns it and `delete`s it — including on an early `return` out of the body. The
//! same lowering drives array/map comprehensions (`[for (x in e) …]`).
//!
//! Skipped (passes vacuously) when no C++ compiler is available.

use std::path::Path;
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

// `Countdown` is an Iterator (hasNext/next); `Range3` is an Iterable whose
// `iterator()` allocates a fresh (reference-type) `Countdown` the loop must free.
const DEMO: &str = r#"package;

class Countdown {
	var n:Int;
	public function new(start:Int) { this.n = start; }
	public function hasNext():Bool { return this.n > 0; }
	public function next():Int { var v = this.n; this.n = this.n - 1; return v; }
}

class Range3 {
	public function new() {}
	public function iterator():Countdown { return new Countdown(3); }
}

class Demo {
	public function new() {}

	// Iterate a custom Iterator directly.
	public function sumIter():Int {
		var total = 0;
		var it = new Countdown(5);
		for (x in it) total = total + x;     // 5+4+3+2+1 = 15
		return total;
	}

	// Iterate a custom Iterable (allocates a heap iterator, freed after the loop).
	public function sumIterable():Int {
		var total = 0;
		var r = new Range3();
		for (x in r) total = total + x;      // 3+2+1 = 6
		return total;
	}

	// Early return out of an Iterable loop must still free the heap iterator.
	public function firstEven():Int {
		var r = new Range3();
		for (x in r) {
			if (x % 2 == 0) return x;        // returns 2
		}
		return -1;
	}

	// Comprehension over a custom Iterator.
	public function collectIter():Array<Int> {
		var it = new Countdown(4);
		return [for (x in it) x * 10];       // [40,30,20,10]
	}

	// Comprehension over a custom Iterable, with a guard.
	public function collectIterable():Array<Int> {
		var r = new Range3();
		return [for (x in r) if (x != 2) x]; // [3,1]
	}
}
"#;

const MAIN_CPP: &str = r#"#include <stdio.h>
#include "Demo.h"
int main() {
	Demo d;
	printf("sumIter=%d\n", d.sumIter());
	printf("sumIterable=%d\n", d.sumIterable());
	printf("firstEven=%d\n", d.firstEven());
	std::vector<int> a = d.collectIter();
	printf("collectIter=");
	for (size_t i = 0; i < a.size(); ++i) printf("%d,", a[i]);
	printf("\n");
	std::vector<int> b = d.collectIterable();
	printf("collectIterable=");
	for (size_t i = 0; i < b.size(); ++i) printf("%d,", b[i]);
	printf("\n");
	return 0;
}
"#;

fn cpp_files(dir: &Path) -> Vec<std::path::PathBuf> {
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
fn custom_iterator_and_iterable_compile_and_run() {
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++ compiler");
        return;
    };

    let root = std::env::temp_dir().join(format!("hatchet_customiter_{}", std::process::id()));
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
    assert!(gen_ok, "transpiling the custom-iterator demo failed");

    // The generated body must drive the protocol and free the heap iterator once.
    let demo_cpp = std::fs::read_to_string(out.join("Demo.cpp")).unwrap();
    assert!(
        demo_cpp.contains("->hasNext()") && demo_cpp.contains("->next()"),
        "expected a hasNext/next while loop:\n{demo_cpp}"
    );
    assert!(
        demo_cpp.contains("->iterator()"),
        "expected an iterator() call for the Iterable:\n{demo_cpp}"
    );

    let exe = out.join(if cfg!(windows) { "ci.exe" } else { "ci" });
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
        "the custom-iterator demo did not compile under g++ -std=c++98:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&exe).output().expect("run the demo");
    let stdout = String::from_utf8_lossy(&run.stdout).to_string();
    let _ = std::fs::remove_dir_all(&root);

    for expected in [
        "sumIter=15",
        "sumIterable=6",
        "firstEven=2",
        "collectIter=40,30,20,10,",
        "collectIterable=3,1,",
    ] {
        assert!(
            stdout.contains(expected),
            "missing `{expected}` in output:\n{stdout}"
        );
    }
}
