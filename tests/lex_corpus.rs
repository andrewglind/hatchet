//! Lex every `.hx` file across the split corpus (`../Modules` + `../Game`) and
//! assert there are no lexer errors. Skipped when both are absent (see
//! `HATCHET_CORPUS` / `HATCHET_GAME_CORPUS`).

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
fn lexes_entire_corpus() {
    let roots = corpus_roots();
    if roots.is_empty() {
        eprintln!("skipping: corpus not found (set HATCHET_CORPUS / HATCHET_GAME_CORPUS)");
        return;
    }

    let mut files = Vec::new();
    for root in &roots {
        files.extend(hatchet::discover::find_haxe_files(root).expect("scan corpus"));
    }
    assert!(!files.is_empty(), "no .hx files found under {:?}", roots);

    let mut failures = Vec::new();
    for f in &files {
        let src = std::fs::read_to_string(f).expect("read source");
        match hatchet::lexer::lex(&src) {
            Ok(toks) => assert!(toks.len() > 1, "{} produced no tokens", f.display()),
            Err(e) => failures.push(format!("{}: {e}", f.display())),
        }
    }
    assert!(failures.is_empty(), "lexer errors:\n{}", failures.join("\n"));
    eprintln!("lexed {} corpus files cleanly", files.len());
}
