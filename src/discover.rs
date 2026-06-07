//! Source discovery and path/package helpers.

use std::path::{Path, PathBuf};

/// Recursively collect `.hx` files under `root`, excluding `Main.hx` (the hxcpp
/// entry point, which is never transpiled). Results are sorted for determinism.
pub fn find_haxe_files(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect(root, &mut out)?;
    out.retain(|p| !is_main(p));
    out.sort();
    Ok(out)
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            // Skip hidden directories (e.g. .git, .venv). Otherwise stay
            // unopinionated about directory names so that any Haxe package —
            // whatever it happens to be called — is discovered.
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with('.') {
                    continue;
                }
            }
            collect(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("hx") {
            out.push(path);
        }
    }
    Ok(())
}

/// `true` if this is `Main.hx`.
pub fn is_main(path: &Path) -> bool {
    file_name_is(path, "Main.hx")
}

/// `true` if this is `StdAfx.hx` (the default prelude source name).
pub fn is_stdafx(path: &Path) -> bool {
    is_stdafx_named(path, "StdAfx")
}

/// `true` if this file's stem is `stem` (the configurable prelude source name,
/// e.g. `StdAfx` or a project's custom `MyGame`).
pub fn is_stdafx_named(path: &Path, stem: &str) -> bool {
    path.file_stem().and_then(|s| s.to_str()) == Some(stem)
}

fn file_name_is(path: &Path, name: &str) -> bool {
    path.file_name().and_then(|n| n.to_str()) == Some(name)
}

/// Infer the package (dotted parts) from a file's directory relative to the
/// source root. e.g. `<root>/mucus/api/Mucus.hx` → `["mucus", "api"]`.
pub fn package_from_path(src_root: &Path, hx_path: &Path) -> Vec<String> {
    let parent = match hx_path.parent() {
        Some(p) => p,
        None => return Vec::new(),
    };
    match parent.strip_prefix(src_root) {
        Ok(rel) => rel
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_special_files() {
        assert!(is_main(Path::new("game/Main.hx")));
        assert!(is_stdafx(Path::new("modules/StdAfx.hx")));
        assert!(!is_main(Path::new("game/AlienBeach.hx")));
    }

    #[test]
    fn package_from_dirs() {
        let root = Path::new("/src");
        assert_eq!(
            package_from_path(root, Path::new("/src/mucus/api/Mucus.hx")),
            vec!["mucus", "api"]
        );
        assert_eq!(
            package_from_path(root, Path::new("/src/modules/Vertex.hx")),
            vec!["modules"]
        );
        assert!(package_from_path(root, Path::new("/src/Root.hx")).is_empty());
    }
}
