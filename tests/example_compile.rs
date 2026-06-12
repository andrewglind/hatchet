//! In-repo compile + run gate for the bundled `examples/shapes` project.
//!
//! This test needs nothing outside the repository: the
//! example ships in `examples/shapes`, so the only external requirement is a
//! C++98 compiler. It transpiles the example with the built `hatchet` binary,
//! compiles the generated C++ together with the hand-written `main.cpp` under
//! `g++ -std=c++98`, runs the result, and checks the output — so it validates
//! not just that the code compiles but that it *behaves* (virtual dispatch
//! through owned base pointers, the enum `switch`, ownership cleanup).
//!
//! Skipped (passes vacuously) when no C++ compiler is available.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Locate a C++98 compiler: `$HATCHET_GXX`, else `g++` on `PATH`, else the MSYS2
/// mingw32 default. Returns `None` if none can be run.
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

/// The bundled example's directory (`<crate>/examples/shapes`).
fn example_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples").join("shapes")
}

/// Every generated `.cpp` under `dir`, recursively.
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
fn shapes_example_transpiles_compiles_and_runs() {
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++ compiler");
        return;
    };
    let example = example_dir();
    let main_cpp = example.join("main.cpp");
    assert!(example.join("World.hx").is_file(), "example sources missing: {}", example.display());
    assert!(main_cpp.is_file(), "example driver missing: {}", main_cpp.display());

    // Transpile into a throwaway output dir with the built binary. The example's
    // `.hx` sources live directly in `examples/shapes` (package `examples.shapes`),
    // so `--src` points at that directory and Hatchet crawls it for `.hx` files.
    let out = std::env::temp_dir().join(format!("hatchet_example_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    let gen_ok = Command::new(env!("CARGO_BIN_EXE_hatchet"))
        .arg("--src")
        .arg(&example)
        .arg("--out")
        .arg(&out)
        .arg("--force")
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(gen_ok, "transpiling the example failed");

    // Compile the generated C++ together with the hand-written entry point.
    let exe = out.join(if cfg!(windows) { "shapes.exe" } else { "shapes" });
    let mut cmd = Command::new(&gxx);
    cmd.args(["-std=c++98", "-pedantic", "-Wall"]).arg("-I").arg(&out).arg(&main_cpp);
    for f in cpp_files(&out) {
        cmd.arg(f);
    }
    cmd.arg("-o").arg(&exe);
    let compile = cmd.output().expect("run g++");
    assert!(
        compile.status.success(),
        "the example did not compile under g++ -std=c++98:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    // Run it and check behaviour (not just that it built).
    let run = Command::new(&exe).output().expect("run the example");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let _ = std::fs::remove_dir_all(&out);

    assert!(stdout.contains("shape count: 3"), "unexpected output:\n{stdout}");
    // Virtual dispatch reached the overrides of the `abstract class Shape`'s pure
    // virtual methods: a non-zero total and the correct per-kind tally (2 circles +
    // 1 rectangle), not a base default.
    assert!(!stdout.contains("total area 0"), "virtual dispatch failed (base area used):\n{stdout}");
    assert!(stdout.contains("circle x2"), "enum switch / tally wrong:\n{stdout}");
    assert!(stdout.contains("rectangle x1"), "enum switch / tally wrong:\n{stdout}");
    assert!(stdout.contains("area rms: 10"), "Math.sqrt / Std.int wrong:\n{stdout}");
    // The feature probe exercises the newer lowerings, segment by segment:
    //   big       — `enum abstract` value + `switch` *expression* on an enum subject
    //   C         — `switch` *expression* on a String subject (→ if/else chain)
    //   shapes=3  — `StringBuf` + `StringTools.replace` + `substring`/`substr`
    //   20,3,4    — `Array` concat/slice/shift/unshift/join
    //   4         — `Array.lastIndexOf`
    assert!(
        stdout.contains("features: big|C|shapes=3|20,3,4|4"),
        "newer-feature lowerings wrong:\n{stdout}"
    );
}
