//! `@:include` path resolution.
//!
//! Includes are resolved in source-tree-relative space (relative to the `--src`
//! root). Because generated `.h`/`.cpp` mirror the source layout, a path computed
//! between two source directories is equally valid between the two generated
//! files, regardless of the output root.
//!
//! The two-step rule:
//!   1. treat the `@:include` string as relative to the directory of the file
//!      that *declares* it, producing a path relative to the source root;
//!   2. re-express that path as relative to the directory of the file being
//!      generated.
//!
//! Example: `modules/Foo.hx` imports `native/api/Native.hx`, which declares
//! `@:include("../../src/Native.h")`. Step 1: `native/api` + `../../src/Native.h`
//! → `src/Native.h`. Step 2: from `modules`, that is `../src/Native.h`.

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
/// leading `./` for same-directory targets.
fn relative(from: &[String], to: &[String]) -> String {
    let common = from
        .iter()
        .zip(to.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let ups = from.len() - common;
    let mut parts: Vec<String> = std::iter::repeat_n("..".to_string(), ups).collect();
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

/// Re-base an already-resolved `#include` path when the generated tree is written
/// somewhere other than in-place (the source root).
///
/// In-tree paths (between two generated files) and `<system>` headers are
/// output-location-independent and returned unchanged. A path that **escapes** the
/// generated tree (a leading `..` — an external engine or sibling-project header
/// Hatchet does not generate) is only re-pointed when it has to be: if the
/// source-relative path still resolves to a real file *from the output location*
/// (the whole tree was relocated together, deps and all — e.g. the compile gate's
/// junctioned mirror, or a same-level `--out`), it is left untouched. It is
/// re-based onto the dependency's real location only when the source-relative path
/// would dangle from the output and the real source location does exist — the
/// "files moved, deps stayed" case (e.g. `--out` nested inside the project).
///
/// * `include` — the source-relative include string (relative to `target_dir`).
/// * `target_dir` — source-root-relative directory of the file being generated.
/// * `src_root` / `out_dir` — absolute source root and absolute output directory.
pub fn rebase_if_escaping(
    include: &str,
    target_dir: &[String],
    src_root: &std::path::Path,
    out_dir: &std::path::Path,
) -> String {
    if include.trim_start().starts_with('<') {
        return include.to_string(); // system header — never path-resolved
    }
    // Reconstruct the source-root-relative path of the included header from the
    // generated file's directory plus the (target-relative) include string.
    let mut abs = target_dir.to_vec();
    abs.extend(split_path(include));
    let abs = normalize(abs);
    // In-tree (no leading `..`) → mirrors into the output tree unchanged.
    if abs.first().map(|c| c != "..").unwrap_or(true) {
        return include.to_string();
    }
    // The dep was relocated alongside the output (its mirrored copy exists where
    // the source-relative path points from the output tree) → keep it as written.
    if resolve_lexical(out_dir, &abs).exists() {
        return include.to_string();
    }
    // Otherwise re-point at the dep's real source location, if it exists there.
    let real = resolve_lexical(src_root, &abs);
    if real.exists() {
        return relative_fs(&push_all(out_dir.to_path_buf(), target_dir), &real);
    }
    // Can't locate it either way — leave the source-relative form unchanged.
    include.to_string()
}

/// Resolve a source-root-relative component list (possibly leading with `..`)
/// against an absolute base, lexically (no filesystem access).
fn resolve_lexical(base: &std::path::Path, rel: &[String]) -> std::path::PathBuf {
    let mut p = base.to_path_buf();
    for c in rel {
        if c == ".." {
            p.pop();
        } else {
            p.push(c);
        }
    }
    p
}

fn push_all(mut base: std::path::PathBuf, parts: &[String]) -> std::path::PathBuf {
    for p in parts {
        base.push(p);
    }
    base
}

/// A forward-slashed relative path from directory `from` to file `to`, both
/// absolute. Falls back to the absolute `to` when they share no common prefix
/// (e.g. different Windows drives), which is still a valid `#include` target.
fn relative_fs(from: &std::path::Path, to: &std::path::Path) -> String {
    let f: Vec<_> = from.components().collect();
    let t: Vec<_> = to.components().collect();
    let common = f.iter().zip(&t).take_while(|(a, b)| a == b).count();
    if common == 0 {
        return to.to_string_lossy().replace('\\', "/");
    }
    let ups = f.len() - common;
    let mut parts: Vec<String> = std::iter::repeat_n("..".to_string(), ups).collect();
    parts.extend(
        t[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(s: &str) -> Vec<String> {
        split_path(s)
    }

    #[test]
    fn two_step_inherited_include() {
        // native/api declares ../../src/Native.h ; target is modules/
        let got = resolve_include(
            "../../src/Native.h",
            &parts("native/api"),
            &parts("modules"),
        );
        assert_eq!(got, "../src/Native.h");
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
        let got = relative_header(&parts("src/Native.h"), &parts("game"));
        assert_eq!(got, "../src/Native.h");
    }

    #[test]
    fn system_header_is_emitted_verbatim() {
        // Angle-bracket system headers are never path-resolved, even across
        // directories, and surrounding whitespace is trimmed.
        assert_eq!(
            resolve_include("<string>", &parts("modules"), &parts("game")),
            "<string>"
        );
        assert_eq!(
            resolve_include("  <vector>  ", &parts("a/b"), &parts("a/b")),
            "<vector>"
        );
    }

    #[test]
    fn rebase_leaves_in_tree_and_system_includes_alone() {
        // In-tree and system includes are output-independent — never touched, and
        // the filesystem is never consulted (these paths need not exist).
        let src = std::path::Path::new("/proj/Modules");
        let out = std::path::Path::new("/proj/Modules/out");
        assert_eq!(
            rebase_if_escaping("Module.h", &parts("modules"), src, out),
            "Module.h"
        );
        assert_eq!(
            rebase_if_escaping("<string>", &parts("modules"), src, out),
            "<string>"
        );
        assert_eq!(
            rebase_if_escaping("../game/Scene.h", &parts("modules"), src, out),
            "../game/Scene.h"
        );
    }

    #[test]
    fn rebase_repoints_an_escaping_include_when_the_dep_stayed_put() {
        // Layout: <t>/Modules is the source root, engine at <t>/NativeEngine. Output
        // is nested at <t>/Modules/out, and the engine is NOT mirrored there, so the
        // escaping include must gain one `..` to keep resolving to <t>/NativeEngine.
        let t = std::env::temp_dir().join(format!("hatchet_rebase_a_{}", std::process::id()));
        let engine = t.join("NativeEngine/src");
        std::fs::create_dir_all(&engine).unwrap();
        std::fs::write(engine.join("Native.h"), "").unwrap();
        let src = t.join("Modules");
        let out = t.join("Modules/out");
        std::fs::create_dir_all(out.join("modules")).unwrap();

        let got = rebase_if_escaping(
            "../../NativeEngine/src/Native.h",
            &parts("modules"),
            &src,
            &out,
        );
        let _ = std::fs::remove_dir_all(&t);
        assert_eq!(got, "../../../NativeEngine/src/Native.h");
    }

    #[test]
    fn rebase_keeps_source_relative_when_the_dep_moved_alongside() {
        // The whole tree was relocated together (the compile-gate case): the dep is
        // mirrored under the output's parent, so the source-relative path still
        // resolves from the output and must be left untouched.
        let t = std::env::temp_dir().join(format!("hatchet_rebase_b_{}", std::process::id()));
        let mirrored = t.join("gate/NativeEngine/src");
        std::fs::create_dir_all(&mirrored).unwrap();
        std::fs::write(mirrored.join("Native.h"), "").unwrap();
        // src_root is the *real* source tree, which also exists, but the alongside copy
        // wins because the source-relative path resolves from the output.
        let src = t.join("real/Modules");
        std::fs::create_dir_all(src.join("../NativeEngine/src")).unwrap();
        std::fs::write(t.join("real/NativeEngine/src/Native.h"), "").unwrap();
        let out = t.join("gate/Modules");
        std::fs::create_dir_all(out.join("modules")).unwrap();

        let got = rebase_if_escaping(
            "../../NativeEngine/src/Native.h",
            &parts("modules"),
            &src,
            &out,
        );
        let _ = std::fs::remove_dir_all(&t);
        assert_eq!(got, "../../NativeEngine/src/Native.h");
    }
}
