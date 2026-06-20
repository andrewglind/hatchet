//! Compile + run gate for **alias typedefs of containers** — the `Backdrop`
//! pattern `typedef Tileset = Array<Tile>; typedef Tilesets = Array<Tileset>;`.
//! Such an alias maps *as a name* to its emitted `typedef std::vector<…>`, so every
//! container operation must resolve through the alias to its `std::vector` head:
//! `new Tilesets()` value-constructs (not a heap pointer, never `delete`d), `.push`
//! → `push_back`, `.length` → `.size()`, `arr[i]` indexes, and `for (x in arr)` /
//! `[for (x in arr) …]` iterate as containers (not the custom-iterator path).
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

typedef Tile = Int;
typedef Tileset = Array<Tile>;
typedef Tilesets = Array<Tileset>;

class Demo {
	var tilesets:Tilesets;

	public function new() {
		this.tilesets = new Tilesets();
		var row:Tileset = new Tileset();
		row.push(1); row.push(2); row.push(3);
		this.tilesets.push(row);
		this.tilesets.push(row);
	}

	// nested iteration over alias-typedef'd containers
	public function total():Int {
		var sum = 0;
		for (tileset in this.tilesets) {
			for (tile in tileset) { sum += tile; }
		}
		return sum;
	}

	// `.length` on an aliased container, in a comprehension
	public function widths():Array<Int> {
		return [for (tileset in this.tilesets) tileset.length];
	}

	// indexing an aliased container
	public function firstOfFirst():Int {
		return this.tilesets[0][0];
	}
}
"#;

const MAIN_CPP: &str = r#"#include <stdio.h>
#include "Demo.h"
int main() {
	demo::Demo d;
	printf("total=%d\n", d.total());
	printf("first=%d\n", d.firstOfFirst());
	std::vector<int> w = d.widths();
	printf("widths=");
	for (size_t i = 0; i < w.size(); ++i) printf("%d,", w[i]);
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
fn alias_typedef_containers_compile_and_run() {
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++ compiler");
        return;
    };

    let root = std::env::temp_dir().join(format!("hatchet_tdcont_{}", std::process::id()));
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
    assert!(gen_ok, "transpiling the typedef-container demo failed");

    // A value container alias must not be heap-allocated or freed.
    let demo_cpp = std::fs::read_to_string(cpp_files(&out).first().expect("a .cpp")).unwrap();
    assert!(
        !demo_cpp.contains("new Tilesets") && !demo_cpp.contains("new Tileset"),
        "alias container `new` must value-construct, not heap-allocate:\n{demo_cpp}"
    );
    assert!(
        !demo_cpp.contains("delete "),
        "a value container alias must never be deleted:\n{demo_cpp}"
    );
    assert!(
        demo_cpp.contains("push_back") && demo_cpp.contains(".size()"),
        "alias container `.push`/`.length` must map to push_back/size():\n{demo_cpp}"
    );

    let exe = out.join(if cfg!(windows) { "tc.exe" } else { "tc" });
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
        "the typedef-container demo did not compile under g++ -std=c++98:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&exe).output().expect("run the demo");
    let stdout = String::from_utf8_lossy(&run.stdout).to_string();
    let _ = std::fs::remove_dir_all(&root);

    for expected in ["total=12", "first=1", "widths=3,3,"] {
        assert!(
            stdout.contains(expected),
            "missing `{expected}` in output:\n{stdout}"
        );
    }
}
