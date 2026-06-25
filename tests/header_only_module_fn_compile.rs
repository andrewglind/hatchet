//! Compile + run gate for **module-level free functions in `--header-only` mode**.
//! A header-only amalgamation has no `.cpp`, so plain `function`s and the
//! `final NAME = lambda` free-function form are emitted `inline` into the single
//! header. Exercises (a) a public function used cross-module via an unqualified
//! `import` call, (b) a `private` helper defined *after* its use (so the inline
//! pass must forward-declare it), and (c) the `final NAME = lambda` free-fn form.
//!
//! Two further tests need no C++ compiler: a free-function **name clash** across
//! two modules in one package, and a `@cexport` `extern "C"` export — both must be
//! rejected (the amalgamation shares one namespace per package, and an exported
//! symbol still needs an object file).
//!
//! The compile/run test is skipped (passes vacuously) when no C++ compiler is
//! available.

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

// `makeVec`/`distance` are public (declared in the header, callable cross-module);
// `twice` is the `final NAME = lambda` free-fn form; `sq` is a `private` helper used
// by `distance` but defined *after* it, so the inline pass must forward-declare it.
const GEOM: &str = r#"package lib;

typedef Vec2 = { x:Float, y:Float };

function makeVec(x:Float, y:Float):Vec2 {
	return { x: x, y: y };
}

function distance(a:Vec2, b:Vec2):Float {
	return Math.sqrt(sq(a.x - b.x) + sq(a.y - b.y));
}

final twice = (x:Float) -> x * 2.0;

private function sq(x:Float):Float {
	return x * x;
}
"#;

// Calls the free functions unqualified after `import` (the supported call style).
const USER: &str = r#"package lib;

import lib.Geom;

class User {
	public function new() {}
	public function run():Float {
		var a = makeVec(0.0, 0.0);
		var b = makeVec(twice(3.0), 0.0);
		return distance(a, b);
	}
}
"#;

const MAIN_CPP: &str = r#"#include <stdio.h>
#include "Lib.h"
int main() {
	lib::User u;
	printf("d=%.0f\n", u.run());
	return 0;
}
"#;

fn has_ext(dir: &Path, ext: &str) -> bool {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten().any(|e| {
                let p = e.path();
                (p.is_file() && p.extension().and_then(|s| s.to_str()) == Some(ext))
                    || (p.is_dir() && has_ext(&p, ext))
            })
        })
        .unwrap_or(false)
}

#[test]
fn header_only_module_functions_compile_and_run() {
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++ compiler");
        return;
    };

    let root = std::env::temp_dir().join(format!("hatchet_honly_modfn_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let lib = root.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("Geom.hx"), GEOM).unwrap();
    std::fs::write(lib.join("User.hx"), USER).unwrap();
    let main_cpp = root.join("main.cpp");
    std::fs::write(&main_cpp, MAIN_CPP).unwrap();

    let out = root.join("out");
    let gen_ok = Command::new(env!("CARGO_BIN_EXE_hatchet"))
        .arg("--src")
        .arg(&lib)
        .arg("--header-only")
        .arg("Lib")
        .arg("--out")
        .arg(&out)
        .arg("--force")
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(gen_ok, "transpiling the header-only module-fn demo failed");

    let header = out.join("Lib.h");
    assert!(header.is_file(), "Lib.h was not generated");
    assert!(!has_ext(&out, "cpp"), "header-only must not emit any .cpp");
    let text = std::fs::read_to_string(&header).unwrap();
    // The definitions must be `inline` (ODR-safe), and the private helper used
    // before its definition must be forward-declared.
    assert!(
        text.contains("inline Vec2 makeVec(") && text.contains("inline double distance("),
        "expected inline free-function definitions:\n{text}"
    );
    assert!(
        text.contains("inline double sq(double x);"),
        "expected a forward declaration for the private helper `sq`:\n{text}"
    );

    let exe = out.join(if cfg!(windows) { "hm.exe" } else { "hm" });
    let compile = Command::new(&gxx)
        .args(["-std=c++98", "-pedantic", "-Wall"])
        .arg("-I")
        .arg(&out)
        .arg(&main_cpp)
        .arg("-o")
        .arg(&exe)
        .output()
        .expect("run g++");
    assert!(
        compile.status.success(),
        "the header-only module-fn demo did not compile under g++ -std=c++98:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&exe).output().expect("run the demo");
    let stdout = String::from_utf8_lossy(&run.stdout).to_string();
    let _ = std::fs::remove_dir_all(&root);
    // makeVec(twice(3)=6, 0) is distance 6 from the origin.
    assert!(stdout.contains("d=6"), "wrong result:\n{stdout}");
}

/// Two modules in the same package each define `clashing()`; in the amalgamation
/// they would land in the same C++ namespace, so Hatchet must reject the second.
#[test]
fn header_only_free_function_name_clash_is_rejected() {
    let root = std::env::temp_dir().join(format!("hatchet_honly_clash_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let lib = root.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(
        lib.join("A.hx"),
        "package lib;\n\nfunction clashing():Int { return 1; }\n",
    )
    .unwrap();
    std::fs::write(
        lib.join("B.hx"),
        "package lib;\n\nfunction clashing():Int { return 2; }\n",
    )
    .unwrap();

    let out = root.join("out");
    let result = Command::new(env!("CARGO_BIN_EXE_hatchet"))
        .arg("--src")
        .arg(&lib)
        .arg("--header-only")
        .arg("Lib")
        .arg("--out")
        .arg(&out)
        .arg("--force")
        .output()
        .expect("run hatchet");
    let stderr = String::from_utf8_lossy(&result.stderr).to_string();
    let _ = std::fs::remove_dir_all(&root);

    assert!(!result.status.success(), "a free-function clash must fail the run");
    assert!(
        stderr.contains("clashes with one already defined")
            && stderr.contains("clashing"),
        "expected a free-function clash diagnostic, got:\n{stderr}"
    );
    assert!(
        !out.join("Lib.h").is_file(),
        "no header should be written when a clash is detected"
    );
}

/// A `@cexport` `extern "C"` export needs its own object file, so it stays
/// unsupported in `--header-only` mode and must be rejected.
#[test]
fn header_only_cexport_export_is_rejected() {
    let root = std::env::temp_dir().join(format!("hatchet_honly_cexport_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let lib = root.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(
        lib.join("Abi.hx"),
        "package lib;\n\n@cexport\nfunction exported(x:Int):Int { return x + 1; }\n",
    )
    .unwrap();

    let out = root.join("out");
    let result = Command::new(env!("CARGO_BIN_EXE_hatchet"))
        .arg("--src")
        .arg(&lib)
        .arg("--header-only")
        .arg("Lib")
        .arg("--out")
        .arg(&out)
        .arg("--force")
        .output()
        .expect("run hatchet");
    let stderr = String::from_utf8_lossy(&result.stderr).to_string();
    let _ = std::fs::remove_dir_all(&root);

    assert!(
        !result.status.success(),
        "a @cexport export must fail the run"
    );
    assert!(
        stderr.contains("@cexport") && stderr.contains("--header-only"),
        "expected a @cexport rejection diagnostic, got:\n{stderr}"
    );
}
