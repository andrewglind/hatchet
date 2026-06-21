//! Compile + run gate for **`Null<String>`** — a genuinely nullable string. Because
//! a plain `String` is a value `std::string` (never null), nullability is expressed
//! with `Null<String>`, which lowers to an owned `std::string*`: `null` is `NULL`,
//! assigning a value heap-allocates (`new std::string(v)`, freeing the prior value),
//! reads in value position dereference (`NULL` → `""`), `!= null` is a real pointer
//! check, and the pointer is freed in the destructor. ASan confirms no leak/double-free.
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

class Box {
	var name:Null<String>;
	public function new() { this.name = null; }

	public function setName(s:String):Void { this.name = s; }  // heap-wrap + free prior
	public function clear():Void { this.name = null; }
	public function has():Bool { return this.name != null; }   // raw pointer check

	// reads in value position dereference (NULL → "")
	public function label():String { return this.name != null ? this.name : "(none)"; }
	public function greeting():String { return "Hi " + this.name; }
}
"#;

const MAIN_CPP: &str = r#"#include <stdio.h>
#include "Demo.h"
int main() {
	demo::Box b;
	printf("a has=%d label=%s\n", (int)b.has(), b.label().c_str());
	b.setName("Ada");
	printf("b has=%d label=%s greet=%s\n", (int)b.has(), b.label().c_str(), b.greeting().c_str());
	b.setName("Bea");   // overwrite frees the prior "Ada"
	printf("c label=%s\n", b.label().c_str());
	b.clear();
	printf("d has=%d label=%s\n", (int)b.has(), b.label().c_str());
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
fn nullable_string_compiles_and_runs() {
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++ compiler");
        return;
    };

    let root = std::env::temp_dir().join(format!("hatchet_nullstr_{}", std::process::id()));
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
    assert!(gen_ok, "transpiling the Null<String> demo failed");

    // Owned heap string: heap-wrap on assign, deref on read, freed in the destructor.
    let cpp = std::fs::read_to_string(cpp_files(&out).first().expect("a .cpp")).unwrap();
    assert!(
        cpp.contains("new std::string("),
        "expected a heap-wrapped assignment:\n{cpp}"
    );
    assert!(
        cpp.contains("!= NULL ? *(") || cpp.contains("!= NULL ? *this->name"),
        "expected a value-position dereference:\n{cpp}"
    );

    let exe = out.join(if cfg!(windows) { "ns.exe" } else { "ns" });
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
        "the Null<String> demo did not compile under g++ -std=c++98:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&exe).output().expect("run the demo");
    let stdout = String::from_utf8_lossy(&run.stdout).to_string();
    let _ = std::fs::remove_dir_all(&root);

    for expected in [
        "a has=0 label=(none)",
        "b has=1 label=Ada greet=Hi Ada",
        "c label=Bea",
        "d has=0 label=(none)",
    ] {
        assert!(
            stdout.contains(expected),
            "missing `{expected}` in output:\n{stdout}"
        );
    }
}
