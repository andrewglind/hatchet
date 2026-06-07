//! Lightweight, comment-aware source scanning helpers.
//!
//! These are intentionally small and pragmatic. Milestone 2 introduces a real
//! lexer; until then a few features (StdAfx, `package`, top-level `final`) need
//! to read the source without tripping over comments or string literals.

/// Remove `//` line comments and `/* */` block comments while preserving the
/// contents of single- and double-quoted string literals. Comment bodies are
/// replaced so that byte ranges shift predictably is *not* guaranteed; callers
/// that need positions should not rely on this. Newlines inside block comments
/// are preserved to keep line numbers stable.
pub fn strip_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let n = bytes.len();
    let mut out = String::with_capacity(n);
    let mut i = 0;
    // string state
    let mut in_string = false;
    let mut string_quote = b'"';
    while i < n {
        let c = bytes[i];
        if in_string {
            out.push(c as char);
            if c == b'\\' && i + 1 < n {
                out.push(bytes[i + 1] as char);
                i += 2;
                continue;
            }
            if c == string_quote {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' | b'\'' => {
                in_string = true;
                string_quote = c;
                out.push(c as char);
                i += 1;
            }
            b'/' if i + 1 < n && bytes[i + 1] == b'/' => {
                i += 2;
                while i < n && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < n && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < n && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    if bytes[i] == b'\n' {
                        out.push('\n');
                    }
                    i += 1;
                }
                i += 2; // skip closing */
            }
            _ => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    out
}

/// Extract the `package a.b.c;` declaration, returning the dotted parts.
/// Returns an empty vec when there is no package declaration.
pub fn package_parts(src: &str) -> Vec<String> {
    let stripped = strip_comments(src);
    for line in stripped.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("package") {
            // must be the keyword, not an identifier like `packageThing`
            if rest.starts_with(|c: char| c.is_whitespace()) {
                let rest = rest.trim().trim_end_matches(';').trim();
                if rest.is_empty() {
                    return Vec::new();
                }
                return rest.split('.').map(|s| s.trim().to_string()).collect();
            }
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_line_and_block_comments() {
        let src = "a // comment\nb /* block */ c";
        let out = strip_comments(src);
        assert_eq!(out, "a \nb  c");
    }

    #[test]
    fn preserves_strings_with_comment_like_content() {
        let src = "var s = \"http://example.com\"; // real comment";
        let out = strip_comments(src);
        assert_eq!(out, "var s = \"http://example.com\"; ");
    }

    #[test]
    fn preserves_single_quoted_strings() {
        let src = "x('a // b')";
        assert_eq!(strip_comments(src), "x('a // b')");
    }

    #[test]
    fn reads_package() {
        assert_eq!(package_parts("package modules;\nclass X {}"), vec!["modules"]);
        assert_eq!(
            package_parts("// c\npackage mucus.api;\n"),
            vec!["mucus", "api"]
        );
        assert!(package_parts("class X {}").is_empty());
    }
}
