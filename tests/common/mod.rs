//! Shared helpers for the codegen integration tests (split from the former
//! monolithic `source_codegen.rs`). Each `tests/codegen_*.rs` binary includes
//! this via `mod common;` — `allow(dead_code)` because no single binary uses
//! every helper.
#![allow(dead_code, unused_imports)]

pub use hatchet::codegen::{generate_source, generate_source_diagnostics};
pub use hatchet::sema::validate::unsupported_construct_errors;
pub use hatchet::sema::Program;

/// Transpile a single synthetic `.hx` source and return class `stem`'s generated `.cpp`.
pub fn gen_one(src: &str, stem: &str) -> String {
    // Unique per call: two tests may share a `stem`, and tests run in parallel, so
    // keying the scratch dir on the stem alone lets them race in one directory.
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let uniq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("hatchet_t_{stem}_{}_{uniq}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{stem}.hx")), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some(stem))
        .unwrap();
    let out = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    out
}

/// Transpile a single synthetic `.hx` source and return module `stem`'s `.h`.
pub fn gen_header(src: &str, stem: &str) -> String {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let uniq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("hatchet_h_{stem}_{}_{uniq}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{stem}.hx")), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some(stem))
        .unwrap();
    let head = hatchet::codegen::generate_header(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    head
}

/// Transpile a single synthetic `.hx` source, returning class `stem`'s `.cpp` body
/// together with its codegen warnings (`(line, message)`).
pub fn gen_one_diag(src: &str, stem: &str) -> (String, Vec<(usize, String)>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let uniq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("hatchet_d_{stem}_{}_{uniq}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{stem}.hx")), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some(stem))
        .unwrap();
    let (out, warnings, _) = generate_source_diagnostics(&prog, idx, 1, false).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    (out, warnings)
}

/// Run the pre-codegen validation pass over a single synthetic source and return
/// the error messages (the `@proxy` misuse checks live here, not in codegen).
pub fn validation_errors(src: &str, stem: &str) -> Vec<String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let uniq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("hatchet_v_{stem}_{}_{uniq}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{stem}.hx")), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some(stem))
        .unwrap();
    let errs = hatchet::sema::validate::unsupported_construct_errors(&prog, idx);
    let _ = std::fs::remove_dir_all(&dir);
    errs.into_iter().map(|d| d.message).collect()
}

