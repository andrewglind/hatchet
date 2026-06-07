//! `@:include` path resolution.
//!
//! Includes are resolved in source-tree-relative space (relative to the `--src`
//! root). Because generated `.h`/`.cpp` mirror the source layout, a path computed
//! between two source directories is equally valid between the two generated
//! files, regardless of the output root.
//!
//! The two-step rule (see `SKILL.md`):
//!   1. treat the `@:include` string as relative to the directory of the file
//!      that *declares* it, producing a path relative to the source root;
//!   2. re-express that path as relative to the directory of the file being
//!      generated.
//!
//! Example: `modules/Foo.hx` imports `mucus/api/Mucus.hx`, which declares
//! `@:include("../../src/Mucus.h")`. Step 1: `mucus/api` + `../../src/Mucus.h`
//! → `src/Mucus.h`. Step 2: from `modules`, that is `../src/Mucus.h`.

/// Split a path string (using `/` or `\`) into non-empty components.
fn split_path(s: &str) -> Vec<String> {
    s.split(['/', '\\'])
        .filter(|c| !c.is_empty() && *c != ".")
        .map(|c| c.to_string())
        .collect()
}

/// Collapse `.`/`..` in a component list. Leading `..` are preserved (the path
/// escapes the base), which can happen for includes outside the source tree.
fn normalize(parts: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in parts {
        if p == ".." {
            if matches!(out.last(), Some(last) if last != "..") {
                out.pop();
            } else {
                out.push(p);
            }
        } else {
            out.push(p);
        }
    }
    out
}

/// Compute a relative path string from directory `from` to file `to`
/// (both source-root-relative component lists). Uses `/` separators and no
/// leading `./` for same-directory targets (matching the goldens).
fn relative(from: &[String], to: &[String]) -> String {
    let common = from
        .iter()
        .zip(to.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let ups = from.len() - common;
    let mut parts: Vec<String> = std::iter::repeat("..".to_string()).take(ups).collect();
    parts.extend(to[common..].iter().cloned());
    parts.join("/")
}

/// Resolve an `@:include` string to the path that should appear in an
/// `#include "..."` of a file generated in `target_dir`.
///
/// * `raw` — the `@:include` argument.
/// * `decl_dir` — source-root-relative directory of the file that declared it.
/// * `target_dir` — source-root-relative directory of the file being generated.
pub fn resolve_include(raw: &str, decl_dir: &[String], target_dir: &[String]) -> String {
    // A system header in angle brackets (`@:include("<string>")`) is emitted
    // verbatim — it is not a path within the source tree, so it is never made
    // relative. The emitter renders it as `#include <string>` (unquoted).
    let trimmed = raw.trim();
    if trimmed.starts_with('<') && trimmed.ends_with('>') {
        return trimmed.to_string();
    }
    // Step 1: declarer-relative → source-root-relative absolute.
    let mut abs = decl_dir.to_vec();
    abs.extend(split_path(raw));
    let abs = normalize(abs);
    // Step 2: re-express relative to the target directory.
    relative(target_dir, &abs)
}

/// The `#include` form of a header that lives at `header` (source-root-relative
/// component list) as seen from a file generated in `target_dir`.
pub fn relative_header(header: &[String], target_dir: &[String]) -> String {
    relative(target_dir, header)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(s: &str) -> Vec<String> {
        split_path(s)
    }

    #[test]
    fn two_step_inherited_include() {
        // mucus/api declares ../../src/Mucus.h ; target is modules/
        let got = resolve_include("../../src/Mucus.h", &parts("mucus/api"), &parts("modules"));
        assert_eq!(got, "../src/Mucus.h");
    }

    #[test]
    fn same_directory_header_has_no_dot_slash() {
        let got = relative_header(&parts("modules/Module.h"), &parts("modules"));
        assert_eq!(got, "Module.h");
    }

    #[test]
    fn nested_targets() {
        // declarer and target in the same deep dir
        let got = resolve_include("Other.h", &parts("a/b/c"), &parts("a/b/c"));
        assert_eq!(got, "Other.h");
        // going down into a sibling subtree
        let got = resolve_include("../d/Other.h", &parts("a/b/c"), &parts("a/b"));
        assert_eq!(got, "d/Other.h");
    }

    #[test]
    fn header_up_and_over() {
        let got = relative_header(&parts("src/Mucus.h"), &parts("game"));
        assert_eq!(got, "../src/Mucus.h");
    }

    #[test]
    fn system_header_is_emitted_verbatim() {
        // Angle-bracket system headers are never path-resolved, even across
        // directories, and surrounding whitespace is trimmed.
        assert_eq!(resolve_include("<string>", &parts("modules"), &parts("game")), "<string>");
        assert_eq!(resolve_include("  <vector>  ", &parts("a/b"), &parts("a/b")), "<vector>");
    }
}
