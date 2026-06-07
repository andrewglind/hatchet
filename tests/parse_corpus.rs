//! Parse every `.hx` file across the split corpus (`../Modules` + `../Game`) and
//! assert there are no parser errors. This is the milestone-2 acceptance gate.
//! Skipped when both are absent (see `HATCHET_CORPUS` / `HATCHET_GAME_CORPUS`).

use std::path::PathBuf;

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

/// Every corpus root present (`Modules`, then `Game`).
fn corpus_roots() -> Vec<PathBuf> {
    [("HATCHET_CORPUS", "Modules"), ("HATCHET_GAME_CORPUS", "Game")]
        .iter()
        .filter_map(|(e, n)| repo_root(e, n))
        .collect()
}

#[test]
fn parses_entire_corpus() {
    let roots = corpus_roots();
    if roots.is_empty() {
        eprintln!("skipping: corpus not found (set HATCHET_CORPUS / HATCHET_GAME_CORPUS)");
        return;
    }

    let mut files = Vec::new();
    for root in &roots {
        files.extend(hatchet::discover::find_haxe_files(root).expect("scan corpus"));
    }
    assert!(!files.is_empty());

    let mut failures = Vec::new();
    let mut total_decls = 0usize;
    for f in &files {
        let src = std::fs::read_to_string(f).expect("read source");
        match hatchet::parser::parse(&src) {
            Ok(file) => total_decls += file.decls.len(),
            Err(e) => failures.push(format!("{}: {e}", f.display())),
        }
    }
    assert!(
        failures.is_empty(),
        "parse errors ({} of {} files):\n{}",
        failures.len(),
        files.len(),
        failures.join("\n")
    );
    eprintln!(
        "parsed {} corpus files cleanly ({} top-level decls)",
        files.len(),
        total_decls
    );
}
