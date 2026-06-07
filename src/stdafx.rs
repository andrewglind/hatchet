//! `StdAfx.hx` handling.
//!
//! Per the rules, when a `StdAfx.hx` is found we create a `StdAfx.h` and inject
//! the code exactly as specified in the `@:headerCode(...)` metadata; no
//! `StdAfx.cpp` is generated. The include guard is derived from the package, e.g.
//! package `modules` → `STDAFX_MODULES_H` (this matches the committed goldens and
//! supersedes the older `_STDAFX_H_` text in SKILL.md — see milestone 8).

use std::path::{Path, PathBuf};

use crate::scan::package_parts;

/// A generated output file: where it goes and what it contains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedFile {
    pub path: PathBuf,
    pub content: String,
}

/// Extract the raw contents of the first `@:headerCode('...')` string literal.
/// Returns the text exactly as written between the quotes (verbatim, no
/// unescaping), or `None` if there is no such metadata.
pub fn extract_header_code(src: &str) -> Option<String> {
    let bytes = src.as_bytes();
    let needle = b"@:headerCode";
    let start = find_subslice(bytes, needle)?;
    let mut i = start + needle.len();
    let n = bytes.len();
    // skip whitespace then expect '('
    while i < n && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= n || bytes[i] != b'(' {
        return None;
    }
    i += 1;
    while i < n && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= n || (bytes[i] != b'\'' && bytes[i] != b'"') {
        return None;
    }
    let quote = bytes[i];
    i += 1;
    let content_start = i;
    while i < n {
        let c = bytes[i];
        if c == b'\\' && i + 1 < n {
            i += 2; // keep escape sequence verbatim, don't let it terminate
            continue;
        }
        if c == quote {
            // SAFETY: slice lies on UTF-8 boundaries since we only advanced past
            // whole bytes and the delimiters are ASCII.
            return Some(src[content_start..i].to_string());
        }
        i += 1;
    }
    None
}

/// Build the prelude header's include guard for `stem` (the StdAfx file's stem,
/// e.g. `StdAfx` or a custom `MyGame`) and a given package (dotted parts).
pub fn guard_for_package(stem: &str, package: &[String]) -> String {
    let s = stem.to_uppercase();
    if package.is_empty() {
        format!("{s}_H")
    } else {
        format!("{s}_{}_H", package.join("_").to_uppercase())
    }
}

/// Render a prelude header wrapping `header_code` verbatim in the package guard,
/// without merging the required includes. Kept for the byte-exact shape test;
/// production output goes through [`content_for`].
pub fn render(stem: &str, header_code: &str, package: &[String]) -> String {
    let guard = guard_for_package(stem, package);
    format!("#ifndef {guard}\n#define {guard}\n{header_code}\n#endif\n")
}

/// The standard headers Hatchet's generated C++ actually relies on, grouped (C,
/// then C++). The transpiler **owns** this list because it is fixed by the idioms
/// Hatchet emits: `NULL`/`rand`/`abs` (`<stdlib.h>`), `sprintf` (`<stdio.h>`),
/// `sqrt`/`sin`/`pow`/`fabs`/`floor`/`HUGE_VAL` (`<math.h>`), `clock`/`CLOCKS_PER_SEC`
/// (`<time.h>`), and the `std::string`/`std::vector`/`std::map` containers.
/// (`<float.h>` is deliberately absent — Hatchet never emits `FLT_MAX`/`DBL_MAX`;
/// a project whose hand-written C++ needs it can add it via `@:headerCode`.)
const REQUIRED_C: [&str; 4] = ["<stdlib.h>", "<stdio.h>", "<math.h>", "<time.h>"];
const REQUIRED_CPP: [&str; 3] = ["<string>", "<vector>", "<map>"];

/// The required `#include`s **not already present** in `existing`, formatted as a
/// block (C group, blank line, C++ group). Empty when `existing` already has them
/// all — which is what keeps the corpus `StdAfx.h` byte-exact, since its
/// `@:headerCode` lists every required header itself.
fn missing_includes(existing: &str) -> String {
    let mut groups: Vec<String> = Vec::new();
    for group in [&REQUIRED_C[..], &REQUIRED_CPP[..]] {
        let lines: Vec<String> = group
            .iter()
            .filter(|h| !existing.contains(**h))
            .map(|h| format!("#include {h}"))
            .collect();
        if !lines.is_empty() {
            groups.push(lines.join("\n"));
        }
    }
    groups.join("\n\n")
}

/// The `StdAfx.h` body: the developer's `@:headerCode` (if any) merged with the
/// required includes it does not already contain.
fn merged_body(header_code: Option<&str>) -> String {
    let existing = header_code.unwrap_or("");
    let block = missing_includes(existing);
    if block.is_empty() {
        existing.to_string()
    } else if existing.trim().is_empty() {
        format!("\n{block}\n")
    } else {
        let sep = if existing.ends_with('\n') { "\n" } else { "\n\n" };
        format!("{existing}{sep}{block}\n")
    }
}

/// The platform export / calling-convention macro block, parameterised by the
/// `export_macro` prefix (default `HATCHET` → `HATCHET_EXPORT`/`HATCHET_CALL`). On MSVC
/// these expand to exactly the Visual C++ `__declspec(dllexport)`/`__cdecl` tokens
/// the corpus goldens use, so the VC6 output is unchanged after preprocessing;
/// elsewhere they degrade to the GCC/Clang visibility attribute (or bare
/// `extern "C"`), keeping Hatchet's output portable across Linux, macOS and embedded
/// (e.g. Dreamcast SH4) toolchains.
pub fn export_macros(prefix: &str) -> String {
    format!(
        "#if defined(_MSC_VER)\n\
         \t#define {p}_EXPORT extern \"C\" __declspec(dllexport)\n\
         \t#define {p}_CALL   __cdecl\n\
         \t#define {p}_CLASS  __declspec(dllexport)\n\
         #elif defined(__GNUC__)\n\
         \t#define {p}_EXPORT extern \"C\" __attribute__((visibility(\"default\")))\n\
         \t#define {p}_CALL\n\
         \t#define {p}_CLASS  __attribute__((visibility(\"default\")))\n\
         #else\n\
         \t#define {p}_EXPORT extern \"C\"\n\
         \t#define {p}_CALL\n\
         \t#define {p}_CLASS\n\
         #endif\n",
        p = prefix
    )
}

/// The fixed-width unsigned integer shim. Hatchet emits `uint8_t`/`uint16_t`/
/// `uint32_t` (from Haxe `UInt8`/`UInt16`/`UInt32`), but C++98 / Visual C++ 6.0 has
/// no `<cstdint>`; this defines them portably — via MSVC's `__intN` builtins, and
/// plain unsigned types elsewhere — and falls through to `<cstdint>` on C++11 and
/// later. It is emitted **first** in the prelude so the types are available to any
/// developer `@:headerCode` and to every engine header the modules include.
pub fn stdint_shim() -> &'static str {
    "#if __cplusplus < 201103L\n\
     \t#if defined(_MSC_VER)\n\
     \t\ttypedef unsigned __int8 uint8_t;\n\
     \t\ttypedef unsigned __int16 uint16_t;\n\
     \t\ttypedef unsigned __int32 uint32_t;\n\
     \t#else\n\
     \t\ttypedef unsigned char uint8_t;\n\
     \t\ttypedef unsigned short uint16_t;\n\
     \t\ttypedef unsigned int uint32_t;\n\
     \t#endif\n\
     #else\n\
     \t#include <cstdint>\n\
     #endif\n"
}

/// Render the full `StdAfx.h`: inside the package guard, the `uint*_t` shim, then
/// the developer's `@:headerCode` (when present) merged with Hatchet's required
/// standard includes, then the platform export macros. `header_code` is `None` for
/// a synthesized `StdAfx.h` (a directory with no `StdAfx.hx`), which then contains
/// just the shim, the required includes, and the export macros. Sections are
/// separated by a blank line.
pub fn content_for(
    stem: &str,
    header_code: Option<&str>,
    package: &[String],
    export_macro: &str,
) -> String {
    let guard = guard_for_package(stem, package);
    let body = merged_body(header_code);
    let macros = export_macros(export_macro);
    let sections = [stdint_shim().trim(), body.trim(), macros.trim()];
    let inner = sections
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("\n\n");
    format!("#ifndef {guard}\n#define {guard}\n\n{inner}\n\n#endif\n")
}

/// Generate the prelude header (`<stem>.h`) that sits next to the given prelude
/// source (`<stem>.hx`), merging its `@:headerCode` with the required includes.
pub fn generate(stem: &str, hx_path: &Path, src: &str, export_macro: &str) -> Option<GeneratedFile> {
    // Sources are commonly CRLF; generated C++ is always LF to match the goldens.
    let header_code = extract_header_code(src).map(|h| h.replace("\r\n", "\n").replace('\r', "\n"));
    let package = package_parts(src);
    let content = content_for(stem, header_code.as_deref(), &package, export_macro);
    let out_path = hx_path.with_file_name(format!("{stem}.h"));
    Some(GeneratedFile {
        path: out_path,
        content,
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_verbatim_header_code() {
        let src = "@:headerCode('\n#include <vector>\n')\nclass StdAfx {}";
        assert_eq!(
            extract_header_code(src).unwrap(),
            "\n#include <vector>\n"
        );
    }

    #[test]
    fn guard_naming() {
        assert_eq!(guard_for_package("StdAfx", &["modules".into()]), "STDAFX_MODULES_H");
        assert_eq!(guard_for_package("StdAfx", &["game".into()]), "STDAFX_GAME_H");
        assert_eq!(
            guard_for_package("StdAfx", &["mucus".into(), "api".into()]),
            "STDAFX_MUCUS_API_H"
        );
        assert_eq!(guard_for_package("StdAfx", &[]), "STDAFX_H");
        // A custom prelude name flows into the guard.
        assert_eq!(guard_for_package("MyGame", &["game".into()]), "MYGAME_GAME_H");
    }

    #[test]
    fn render_matches_expected_shape() {
        let out = render("StdAfx", "\n#include <map>\n", &["game".into()]);
        let expected = "#ifndef STDAFX_GAME_H\n#define STDAFX_GAME_H\n\n#include <map>\n\n#endif\n";
        assert_eq!(out, expected);
    }

    const REQUIRED: [&str; 7] = [
        "<stdlib.h>", "<stdio.h>", "<math.h>", "<time.h>", "<string>", "<vector>", "<map>",
    ];

    #[test]
    fn content_for_synthesizes_full_prelude_when_no_header_code() {
        let out = content_for("StdAfx", None, &[], "HATCHET");
        assert!(out.starts_with("#ifndef STDAFX_H\n#define STDAFX_H\n"), "{out}");
        assert!(out.ends_with("#endif\n"), "{out}");
        for h in REQUIRED {
            assert_eq!(out.matches(&format!("#include {h}")).count(), 1, "{h}\n{out}");
        }
        assert!(!out.contains("<float.h>"), "float.h is not part of the required set:\n{out}");
        // The uint*_t shim is present and precedes the includes (types first).
        assert!(out.contains("typedef unsigned __int32 uint32_t;"), "MSVC uint shim:\n{out}");
        assert!(out.contains("#include <cstdint>"), "C++11 fallthrough:\n{out}");
        assert!(
            out.find("uint32_t").unwrap() < out.find("#include <stdlib.h>").unwrap(),
            "the shim comes before the required includes:\n{out}"
        );
    }

    #[test]
    fn content_for_merges_and_dedupes() {
        // @:headerCode already lists some headers (and a custom pragma); the rest
        // are added, nothing is duplicated, and the custom content is kept.
        let hc = "\n#pragma once\n#include <string>\n#include <vector>\n#include <map>\n";
        let out = content_for("StdAfx", Some(hc), &["modules".into()], "HATCHET");
        assert!(out.contains("#pragma once"), "keeps custom header code: {out}");
        for h in REQUIRED {
            assert_eq!(out.matches(&format!("#include {h}")).count(), 1, "{h}\n{out}");
        }
    }

    #[test]
    fn content_for_emits_export_macros_with_prefix() {
        // The export macros land in the prelude, parameterised by the prefix, and
        // expand to the MSVC tokens under `_MSC_VER`.
        let out = content_for("StdAfx", None, &["game".into()], "MUCUS");
        assert!(out.contains("#if defined(_MSC_VER)"), "platform guard present:\n{out}");
        assert!(
            out.contains("#define MUCUS_EXPORT extern \"C\" __declspec(dllexport)"),
            "MSVC export macro:\n{out}"
        );
        assert!(out.contains("#define MUCUS_CALL   __cdecl"), "MSVC call macro:\n{out}");
        assert!(
            out.contains("#define MUCUS_CLASS  __declspec(dllexport)"),
            "MSVC class-export macro (no extern \"C\"):\n{out}"
        );
        assert!(
            out.contains("__attribute__((visibility(\"default\")))"),
            "GCC fallback present:\n{out}"
        );
        // Macros sit inside the include guard.
        assert!(out.trim_end().ends_with("#endif"), "guard closes last:\n{out}");
    }
}
