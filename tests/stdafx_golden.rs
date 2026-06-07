//! `StdAfx.hx` → `StdAfx.h`: Hatchet owns the set of standard headers its generated
//! C++ requires and merges them with the developer's `@:headerCode` (de-duping so
//! a header the developer already listed is not repeated). This supersedes the
//! older byte-exact golden, which captured each package's hand-curated include
//! list (e.g. `game/StdAfx.h` lacked `<math.h>`); the transpiler now guarantees
//! the full set everywhere. Each package's `StdAfx.hx` lives in its own repo since
//! the corpus split (`modules` in `../Modules`, `game` in `../Game`); located via
//! `HATCHET_CORPUS` / `HATCHET_GAME_CORPUS`, skipped when absent.

use std::path::PathBuf;

const REQUIRED: [&str; 7] = [
    "<stdlib.h>", "<stdio.h>", "<math.h>", "<time.h>", "<string>", "<vector>", "<map>",
];

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

#[test]
fn stdafx_merges_headercode_with_required_includes() {
    // (package, repo): the `modules` prelude is in `../Modules`, `game` in `../Game`.
    let repos = [
        ("modules", repo_root("HATCHET_CORPUS", "Modules")),
        ("game", repo_root("HATCHET_GAME_CORPUS", "Game")),
    ];
    if repos.iter().all(|(_, r)| r.is_none()) {
        eprintln!("skipping: corpus not found (set HATCHET_CORPUS / HATCHET_GAME_CORPUS)");
        return;
    }

    for (pkg, root) in &repos {
        let Some(root) = root else { continue };
        let hx_path = root.join(pkg).join("StdAfx.hx");
        let src = std::fs::read_to_string(&hx_path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", hx_path.display()));
        let gen = hatchet::stdafx::generate("StdAfx", &hx_path, &src, "HATCHET")
            .unwrap_or_else(|| panic!("no StdAfx generated for {}", hx_path.display()));
        let out = gen.content;

        // Package guard.
        let guard = format!("STDAFX_{}_H", pkg.to_uppercase());
        assert!(out.starts_with(&format!("#ifndef {guard}\n")), "{pkg}: guard\n{out}");

        // The developer's @:headerCode is preserved (the MSVC pragma block).
        assert!(out.contains("#pragma warning(disable: 4068)"), "{pkg}: keeps @:headerCode\n{out}");

        // Every required header is present exactly once (merge + de-dupe).
        for h in REQUIRED {
            let needle = format!("#include {h}");
            assert_eq!(out.matches(&needle).count(), 1, "{pkg}: {h} present exactly once\n{out}");
        }
    }
}
