//! Semantic-layer checks against the real `Modules` corpus: the milestone-3
//! acceptance gate (Vertex's include set + `mucus::` namespacing + the
//! local-vs-native `Vertex` name collision). Skipped when the corpus is absent.
//!
//! Since the corpus split into standalone sibling repos, the Haxe modules live in
//! `../Modules` (the engine's native API stubs live in `../MucusEngine/src`, which
//! Mucus.hx's `@:include` now points at via a sibling-relative path).

use std::path::PathBuf;

use hatchet::ast::Type;
use hatchet::sema::Program;

/// The `Modules` corpus root: `$HATCHET_CORPUS` if set, else the sibling `../Modules`.
fn corpus_root() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("HATCHET_CORPUS") {
        let p = PathBuf::from(p);
        return p.is_dir().then_some(p);
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let sibling = manifest.parent()?.join("Modules");
    sibling.is_dir().then_some(sibling)
}

fn named(parts: &[&str]) -> Type {
    Type::Named {
        path: parts.iter().map(|s| s.to_string()).collect(),
        params: vec![],
        optional: false,
        line: 0,
    }
}

#[test]
fn vertex_includes_and_namespacing() {
    let Some(root) = corpus_root() else {
        eprintln!("skipping: Modules corpus not found (set HATCHET_CORPUS)");
        return;
    };
    let prog = Program::from_src_dir(&root).expect("build program");

    let vidx = prog
        .modules
        .iter()
        .position(|m| m.path.ends_with("Vertex.hx"))
        .expect("Vertex.hx present");
    let ns = vec!["modules".to_string()];

    // Two-step inherited include + superclass header, StdAfx first. Mucus.hx's
    // `@:include` reaches the native API in the sibling engine repo — from
    // `Modules/modules/` that is `../../MucusEngine/src/Mucus.h`.
    let incs = prog.header_includes(vidx);
    assert_eq!(incs, vec!["StdAfx.h", "../../MucusEngine/src/Mucus.h", "Module.h"]);

    // Native engine types are namespaced to `mucus` and are pointers.
    assert_eq!(prog.map_type_use(&named(&["IEngine"]), vidx, &ns), "mucus::IEngine*");
    assert_eq!(prog.map_type_use(&named(&["Effects"]), vidx, &ns), "mucus::Effects");

    // The bare `Vertex` is the local class; the qualified one is the native struct.
    let local = prog.resolve_type(&["Vertex".into()], vidx).expect("local Vertex");
    assert!(!local.is_native, "bare Vertex should resolve to the modules class");
    assert_eq!(local.package, vec!["modules"]);

    let native = prog
        .resolve_type(
            &["mucus".into(), "Mucus".into(), "Vertex".into()],
            vidx,
        )
        .expect("native Vertex");
    assert!(native.is_native, "qualified Vertex should resolve to the native typedef");
    assert_eq!(native.cpp_namespace(), vec!["mucus"]);
}

#[test]
fn native_interop_modules_emit_no_header() {
    let Some(root) = corpus_root() else { return };
    let prog = Program::from_src_dir(&root).expect("build program");

    // The pure `@:native` interop module (`mucus/Mucus.hx`) contributes includes,
    // not a header. (Unlike `modules/Modules.hx`, which holds enums/typedefs and
    // *does* generate a header.)
    let mucus = prog
        .modules
        .iter()
        .find(|m| m.path.ends_with("Mucus.hx"))
        .expect("Mucus.hx present");
    assert!(
        !prog.generates_header(mucus),
        "{} should not generate a header",
        mucus.path.display()
    );
}
