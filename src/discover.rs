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

/// Whether `s` looks like a glob pattern (contains a `*` or `?` wildcard).
pub fn is_glob(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

/// Expand a glob `pattern` into the `.hx` files it matches, sorted and
/// de-duplicated. Supports `*` and `?` within a path segment and `**` for
/// recursive directory descent (e.g. `modules/*.hx`, `src/**/*.hx`). Hidden
/// entries (names starting with `.`) are skipped unless the pattern segment
/// itself begins with `.`. Non-`.hx` matches are dropped. Returns empty when
/// nothing matches.
pub fn glob_hx(pattern: &str) -> Vec<PathBuf> {
    // Split the pattern into a literal base (the leading wildcard-free
    // components) and the remaining wildcard segments, so we only walk the
    // directories that can possibly match.
    let path = PathBuf::from(pattern);
    let mut base = PathBuf::new();
    let mut segs: Vec<String> = Vec::new();
    for comp in path.components() {
        let part = comp.as_os_str().to_string_lossy();
        if segs.is_empty() && !is_glob(&part) {
            base.push(comp.as_os_str());
        } else {
            segs.push(part.into_owned());
        }
    }
    if base.as_os_str().is_empty() {
        base = PathBuf::from(".");
    }
    let mut out = Vec::new();
    glob_collect(&base, &segs, &mut out);
    out.retain(|p| p.extension().and_then(|e| e.to_str()) == Some("hx"));
    out.sort();
    out.dedup();
    out
}

/// Walk `dir` matching the remaining wildcard `segs`, pushing matched files.
fn glob_collect(dir: &Path, segs: &[String], out: &mut Vec<PathBuf>) {
    let Some((seg, rest)) = segs.split_first() else {
        return;
    };
    // `**` matches zero or more directories: try `rest` here, then descend.
    if seg == "**" {
        glob_collect(dir, rest, out);
        if let Ok(rd) = std::fs::read_dir(dir) {
            for entry in rd.flatten() {
                let child = entry.path();
                if child.is_dir() && !name_is_hidden(&child) {
                    glob_collect(&child, segs, out);
                }
            }
        }
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') && !seg.starts_with('.') {
            continue;
        }
        if !segment_matches(seg, &name) {
            continue;
        }
        let child = entry.path();
        if rest.is_empty() {
            if child.is_file() {
                out.push(child);
            }
        } else if child.is_dir() {
            glob_collect(&child, rest, out);
        }
    }
}

fn name_is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with('.'))
        .unwrap_or(false)
}

/// Match a single path segment against one glob segment (`*` = any run, `?` =
/// any one char). Iterative two-pointer match with `*` backtracking.
fn segment_matches(pat: &str, name: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let s: Vec<char> = name.chars().collect();
    let (mut i, mut j) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while j < s.len() {
        if i < p.len() && (p[i] == '?' || p[i] == s[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
            star = Some(i);
            mark = j;
            i += 1;
        } else if let Some(st) = star {
            i = st + 1;
            mark += 1;
            j = mark;
        } else {
            return false;
        }
    }
    while i < p.len() && p[i] == '*' {
        i += 1;
    }
    i == p.len()
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
    fn segment_matching_handles_star_and_question() {
        assert!(segment_matches("*.hx", "Vertex.hx"));
        assert!(segment_matches("*.hx", ".hx"));
        assert!(!segment_matches("*.hx", "Vertex.cpp"));
        assert!(segment_matches("Vertex?.hx", "Vertex2.hx"));
        assert!(!segment_matches("Vertex?.hx", "Vertex.hx"));
        assert!(segment_matches("V*x.hx", "Vertex.hx"));
        assert!(segment_matches("*", "anything"));
        assert!(segment_matches("Vertex.hx", "Vertex.hx"));
    }

    #[test]
    fn glob_expands_a_single_level_and_recursive_pattern() {
        let dir = std::env::temp_dir().join(format!("hatchet_glob_{}", std::process::id()));
        let nested = dir.join("modules").join("sub");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.join("modules").join("Vertex.hx"), "").unwrap();
        std::fs::write(dir.join("modules").join("Edge.hx"), "").unwrap();
        std::fs::write(dir.join("modules").join("notes.txt"), "").unwrap();
        std::fs::write(nested.join("Deep.hx"), "").unwrap();

        // Single level: only the two .hx files directly under modules/.
        let one = glob_hx(&format!("{}/modules/*.hx", dir.display()));
        let stems: Vec<_> = one
            .iter()
            .filter_map(|p| p.file_name().and_then(|s| s.to_str()).map(String::from))
            .collect();
        assert_eq!(stems, vec!["Edge.hx".to_string(), "Vertex.hx".to_string()]);

        // Recursive: ** reaches the nested file too.
        let all = glob_hx(&format!("{}/**/*.hx", dir.display()));
        assert_eq!(all.len(), 3, "** should find all three .hx files: {all:?}");

        let _ = std::fs::remove_dir_all(&dir);
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
