//! Compile + run gate for two ownership/decl features working together:
//!   * targeted forward declarations let mutually-referential classes
//!     (`Slot` ↔ `Proxy`-style) live in one module without manual ordering, and
//!   * `@sink` parameters transfer ownership across a retaining method, so a
//!     `new` (or owned local) handed to a setter that stores it is freed exactly
//!     once — by the container's destructor — never by the caller (the
//!     use-after-free an un-annotated retaining method would cause).
//!
//! The program builds a small tree through a `@sink` setter and reads it back;
//! correct values prove the stored pointers are alive (not freed under the
//! caller's feet). Skipped (passes vacuously) when no C++ compiler is available.

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

// `Bag` references `Item` before `Item` is defined (forward decl), and `add`
// takes its element `@sink` — both a literal `new` and an owned local are
// handed to it and must survive in the bag.
const SRC: &str = r#"package lib;

class Bag {
	public var items:Array<Item>;
	public function new() { items = []; }
	public function add(@sink it:Item):Void {
		items.push(it);
	}
	public function total():Int {
		var sum = 0;
		for (i in items) sum += i.value;
		return sum;
	}
}

class Item {
	public var value:Int;
	public function new(v:Int) { value = v; }
}
"#;

const MAIN_CPP: &str = r#"#include <stdio.h>
#include "lib/Bag.h"
using namespace lib;
int main() {
	Bag* b = new Bag();
	b->add(new Item(10));        /* new at @sink position -> inline, not freed */
	Item* extra = new Item(32);
	b->add(extra);               /* owned local -> ownership transferred */
	printf("count=%d total=%d\n", (int)b->items.size(), b->total());
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
fn owned_params_and_forward_decls_compile_and_run() {
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++ compiler");
        return;
    };

    let root = std::env::temp_dir().join(format!("hatchet_ownfwd_{}", std::process::id()));
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
    assert!(gen_ok, "transpiling the owned/forward demo failed");

    // The forward declaration is present, and the `@sink` `new` is inline with
    // no caller-side delete.
    let header = std::fs::read_to_string(out.join("lib").join("Bag.h")).unwrap();
    assert!(
        header.contains("class Item;"),
        "Item forward-declared:\n{header}"
    );
    let body = std::fs::read_to_string(out.join("lib").join("Bag.cpp")).unwrap();
    assert!(
        body.contains("this->items.push_back(it)"),
        "add stores the element:\n{body}"
    );

    let exe = out.join(if cfg!(windows) {
        "ownfwd.exe"
    } else {
        "ownfwd"
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
        "the owned/forward demo did not compile under g++ -std=c++98:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&exe)
        .output()
        .expect("run the owned/forward demo");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let _ = std::fs::remove_dir_all(&root);

    // Both items survive in the bag (no use-after-free): 10 + 32 = 42.
    assert!(
        stdout.contains("count=2 total=42"),
        "stored owned pointers must stay alive:\n{stdout}"
    );
}
