//! Whole-corpus C++98 compile gate (Milestone 7), run as a `cargo test`.
//!
//! Transpiles the `Modules` and `Game` corpora with the built `hatchet` binary into
//! a temporary mirror, then compiles every generated `.cpp` with `g++ -std=c++98
//! -fsyntax-only`. A file passes when it compiles. The split-repo output uses
//! sibling-relative includes (`../../MucusEngine/src/Mucus.h`), so the mirror is
//! positioned with a directory junction `<gate>/MucusEngine` → the real engine,
//! and Modules/Game are generated beside it so those includes resolve.
//!
//! The test SKIPS (passes vacuously) when the corpus repos or a C++98 compiler are
//! not available, so it is a no-op on machines without the engine checkout.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A sibling corpus repo: `$env` if set, else `../<name>` next to this crate.
fn repo_root(env: &str, name: &str) -> Option<PathBuf> {
    if let Ok(p) = std::env::var(env) {
        let p = PathBuf::from(p);
        return p.is_dir().then_some(p);
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let sibling = manifest.parent()?.join(name);
    sibling.is_dir().then_some(sibling)
}

/// Locate a C++98 compiler: `$HATCHET_GXX`, else `g++` on `PATH`, else the MSYS2
/// mingw32 default. Returns `None` if none can be run.
fn find_gxx() -> Option<String> {
    let candidates = [
        std::env::var("HATCHET_GXX").ok(),
        Some("g++".to_string()),
        Some(r"C:\msys64\mingw32\bin\g++.exe".to_string()),
    ];
    for c in candidates.into_iter().flatten() {
        if Command::new(&c).arg("--version").output().map(|o| o.status.success()).unwrap_or(false) {
            return Some(c);
        }
    }
    None
}

/// Absolute paths of every `.hx` file directly inside `dir` (non-recursive).
fn hx_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("hx") {
                out.push(p);
            }
        }
    }
    out
}

/// Generate a corpus into `<out>` with the built binary. `srcs` is the full
/// resolution scope (every `.hx` the corpus needs).
fn generate(out: &Path, srcs: &[PathBuf]) -> bool {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hatchet"));
    cmd.arg("--out").arg(out).arg("--force").arg("--src");
    for s in srcs {
        cmd.arg(s);
    }
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

/// Create a directory junction `link` → `target` (Windows; no admin needed).
#[cfg(windows)]
fn link_dir(link: &Path, target: &Path) -> bool {
    Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn link_dir(link: &Path, target: &Path) -> bool {
    std::os::unix::fs::symlink(target, link).is_ok()
}

/// Every generated `.cpp` under `dir`, recursively.
fn cpp_files(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                cpp_files(&p, out);
            } else if p.extension().and_then(|s| s.to_str()) == Some("cpp") {
                out.push(p);
            }
        }
    }
}

#[test]
fn whole_corpus_compiles_under_cpp98() {
    let (Some(modules), Some(game)) = (repo_root("HATCHET_CORPUS", "Modules"), repo_root("HATCHET_GAME_CORPUS", "Game"))
    else {
        eprintln!("skipping: Modules/Game corpus not found");
        return;
    };
    let Some(engine) = repo_root("HATCHET_ENGINE", "MucusEngine") else {
        eprintln!("skipping: MucusEngine not found");
        return;
    };
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++98 compiler found");
        return;
    };

    // Unique gate dir so no pre-cleanup is needed.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let gate = std::env::temp_dir().join(format!("hatchet_gate_{}_{}", std::process::id(), stamp));
    std::fs::create_dir_all(&gate).unwrap();
    let junction = gate.join("MucusEngine");
    assert!(link_dir(&junction, &engine), "could not create MucusEngine junction");

    // Generate Modules and Game into the mirror (file lists, not globs).
    let mut mod_srcs = hx_files(&modules.join("modules"));
    mod_srcs.extend(hx_files(&modules.join("mucus")));
    let mut game_srcs = hx_files(&game.join("game"));
    game_srcs.extend(hx_files(&game.join("modules")));
    game_srcs.extend(hx_files(&game.join("mucus")));
    let gen_ok = generate(&gate.join("Modules"), &mod_srcs) && generate(&gate.join("Game"), &game_srcs);

    // Compile every generated `.cpp`.
    let inc_src = junction.join("src");
    let inc_inc = junction.join("include");
    let mut cpps = Vec::new();
    cpp_files(&gate.join("Modules"), &mut cpps);
    cpp_files(&gate.join("Game"), &mut cpps);

    let mut failures: Vec<String> = Vec::new();
    if gen_ok {
        for f in &cpps {
            let out = Command::new(&gxx)
                .args(["-std=c++98", "-fsyntax-only"])
                .arg("-I").arg(&inc_src)
                .arg("-I").arg(&inc_inc)
                .arg("-I").arg(f.parent().unwrap())
                .arg(f)
                .output()
                .expect("run g++");
            if !out.status.success() {
                let name = f.file_name().and_then(|s| s.to_str()).unwrap_or("?").to_string();
                let errs: String = String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .filter(|l| l.contains("error:"))
                    .take(4)
                    .collect::<Vec<_>>()
                    .join("\n");
                failures.push(format!("{name}:\n{errs}"));
            }
        }
    }

    // Remove the junction BEFORE the recursive delete so it is never followed into
    // the real engine; then drop the mirror.
    let _ = std::fs::remove_dir(&junction);
    let _ = std::fs::remove_dir_all(&gate);

    assert!(gen_ok, "transpilation failed before the compile gate");
    assert!(!cpps.is_empty(), "no .cpp files were generated");
    assert!(
        failures.is_empty(),
        "{} of {} generated .cpp failed to compile under g++ -std=c++98:\n{}",
        failures.len(),
        cpps.len(),
        failures.join("\n---\n")
    );
}
