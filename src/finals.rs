//! Top-level `final` constants → `#define`.
//!
//! Rules:
//!   `final NAME:Type = V;`          → `#define NAME V`, exposed via the header
//!   `private final NAME:Type = V;`  → `#define NAME V`, scoped to the source file
//!   `@:native final NAME:Type = V;` → not emitted (the C++ side provides it)
//!
//! Milestone 1 ships a focused scanner so the rule is exercised and unit-tested;
//! the full parser (milestone 2) will supersede this extraction, and milestone 4
//! places the resulting defines into the correct header/source.

use crate::scan::strip_comments;

/// A top-level final constant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Final {
    pub name: String,
    /// C++ value text (Haxe `null` already mapped to `NULL`).
    pub value: String,
    pub is_private: bool,
    /// `@:native` — provided by C++, must not be emitted.
    pub is_native: bool,
}

impl Final {
    /// The `#define` line for this constant (without trailing newline).
    pub fn to_define(&self) -> String {
        format!("#define {} {}", self.name, self.value)
    }
}

/// Extract top-level (brace-depth 0) `final` constants from Haxe source.
pub fn extract_finals(src: &str) -> Vec<Final> {
    let stripped = strip_comments(src);
    let bytes = stripped.as_bytes();
    let n = bytes.len();
    let mut out = Vec::new();

    let mut depth: i32 = 0;
    let mut pending_meta: Vec<String> = Vec::new();
    let mut pending_private = false;
    let mut i = 0;

    while i < n {
        let c = bytes[i];
        match c {
            b'{' | b'(' | b'[' => {
                depth += 1;
                i += 1;
            }
            b'}' | b')' | b']' => {
                depth -= 1;
                i += 1;
            }
            b'@' if i + 1 < n && bytes[i + 1] == b':' && depth == 0 => {
                // metadata: @:name  (optionally with (...) which we skip)
                let (name, next) = read_meta_name(bytes, i + 2);
                pending_meta.push(name);
                i = next;
            }
            _ if is_ident_start(c) => {
                let (word, next) = read_ident(bytes, i);
                if depth == 0 {
                    match word.as_str() {
                        "private" => pending_private = true,
                        "public" => {}
                        "final" => {
                            if let Some((fin, after)) =
                                try_parse_final(&stripped, bytes, next, pending_private, &pending_meta)
                            {
                                out.push(fin);
                                i = after;
                                pending_meta.clear();
                                pending_private = false;
                                continue;
                            }
                            // not a constant final (e.g. `final class`): reset modifiers
                            pending_meta.clear();
                            pending_private = false;
                        }
                        // any other top-level declaration keyword resets pending modifiers
                        "class" | "interface" | "enum" | "typedef" | "abstract" | "var"
                        | "function" => {
                            pending_meta.clear();
                            pending_private = false;
                        }
                        _ => {}
                    }
                }
                i = next;
            }
            b';' if depth == 0 => {
                pending_meta.clear();
                pending_private = false;
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    out
}

/// Given position just after the `final` keyword, try to parse
/// `NAME : Type = VALUE ;`. Returns the Final and the index after `;`.
fn try_parse_final(
    _src: &str,
    bytes: &[u8],
    mut i: usize,
    is_private: bool,
    meta: &[String],
) -> Option<(Final, usize)> {
    let n = bytes.len();
    i = skip_ws(bytes, i);
    if i >= n || !is_ident_start(bytes[i]) {
        return None;
    }
    let (name, next) = read_ident(bytes, i);
    i = skip_ws(bytes, next);
    if i >= n || bytes[i] != b':' {
        return None; // not a typed constant
    }
    i += 1;
    // skip the type up to '='
    while i < n && bytes[i] != b'=' && bytes[i] != b';' {
        i += 1;
    }
    if i >= n || bytes[i] != b'=' {
        return None;
    }
    i += 1;
    let value_start = i;
    while i < n && bytes[i] != b';' {
        i += 1;
    }
    let raw_value = std::str::from_utf8(&bytes[value_start..i]).ok()?.trim();
    if i < n {
        i += 1; // consume ';'
    }
    let value = map_value(raw_value);
    let is_native = meta.iter().any(|m| m == "native");
    Some((
        Final {
            name,
            value,
            is_private,
            is_native,
        },
        i,
    ))
}

fn map_value(raw: &str) -> String {
    if raw == "null" {
        "NULL".to_string()
    } else {
        raw.to_string()
    }
}

fn read_meta_name(bytes: &[u8], start: usize) -> (String, usize) {
    let (name, mut i) = read_ident(bytes, start);
    // skip an optional (...) argument list
    let n = bytes.len();
    let j = skip_ws(bytes, i);
    if j < n && bytes[j] == b'(' {
        let mut depth = 0;
        i = j;
        while i < n {
            match bytes[i] {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    i += 1;
                    if depth == 0 {
                        break;
                    }
                    continue;
                }
                _ => {}
            }
            i += 1;
        }
    }
    (name, i)
}

fn read_ident(bytes: &[u8], start: usize) -> (String, usize) {
    let n = bytes.len();
    let mut i = start;
    while i < n && is_ident_continue(bytes[i]) {
        i += 1;
    }
    (
        String::from_utf8_lossy(&bytes[start..i]).into_owned(),
        i,
    )
}

fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    let n = bytes.len();
    while i < n && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

fn is_ident_start(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphabetic()
}

fn is_ident_continue(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_final() {
        let f = extract_finals("package game;\nfinal ALIENBEACH_SCENE_ID:Int = 1;\n");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].name, "ALIENBEACH_SCENE_ID");
        assert_eq!(f[0].value, "1");
        assert!(!f[0].is_private);
        assert!(!f[0].is_native);
        assert_eq!(f[0].to_define(), "#define ALIENBEACH_SCENE_ID 1");
    }

    #[test]
    fn private_final_scoped() {
        let f = extract_finals("private final SECRET:Int = 7;");
        assert_eq!(f.len(), 1);
        assert!(f[0].is_private);
    }

    #[test]
    fn native_final_flagged() {
        let f = extract_finals("@:native\nfinal FOG_TABLE_SIZE:Int = 64;");
        assert_eq!(f.len(), 1);
        assert!(f[0].is_native);
        assert_eq!(f[0].value, "64");
    }

    #[test]
    fn ignores_final_inside_class_body() {
        let src = "class X {\n  final inner:Int = 5;\n}";
        assert!(extract_finals(src).is_empty());
    }

    #[test]
    fn ignores_final_class_keyword() {
        assert!(extract_finals("final class Foo {}").is_empty());
    }
}
