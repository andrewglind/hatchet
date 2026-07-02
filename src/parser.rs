//! Recursive-descent parser: tokens → `ast::File`.
//!
//! The grammar covers the Haxe subset Hatchet supports. Input is
//! assumed compile-time valid (it already passes `haxe`/`hxcpp`), so the parser
//! favours clear structure over exhaustive error recovery; on anything it does
//! not understand it stops with a `ParseError` carrying a line number.

use std::fmt;

use crate::ast::*;
use crate::lexer::{lex, Kw, Sym, TokKind, Token};

mod decls;
mod expr;
mod stmt;
mod types;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    pub line: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "parse error (line {}): {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

/// Parse a full Haxe source file.
pub fn parse(src: &str) -> Result<File, ParseError> {
    // Strip a leading UTF-8 BOM (U+FEFF) that some editors prepend — it is not source.
    // Done here (not in `lex`) so the lexer and the parser's source slice — used to
    // capture raw metadata-argument spans verbatim — share the same byte offsets.
    let src = src.strip_prefix('\u{FEFF}').unwrap_or(src);
    let tokens = lex(src).map_err(|e| ParseError {
        message: e.message,
        line: e.line,
    })?;
    let mut p = Parser {
        src: src.as_bytes(),
        toks: tokens,
        pos: 0,
    };
    p.parse_file()
}

/// Parse a single expression (used for `${...}` string-interpolation segments).
pub fn parse_expression(src: &str) -> Result<Expr, ParseError> {
    let tokens = lex(src).map_err(|e| ParseError {
        message: e.message,
        line: e.line,
    })?;
    let mut p = Parser {
        src: src.as_bytes(),
        toks: tokens,
        pos: 0,
    };
    p.parse_expr()
}

struct Parser<'a> {
    src: &'a [u8],
    toks: Vec<Token>,
    pos: usize,
}

type PResult<T> = Result<T, ParseError>;

impl<'a> Parser<'a> {
    // ---- cursor helpers -------------------------------------------------

    fn peek(&self) -> &TokKind {
        &self.toks[self.pos].kind
    }

    fn peek2(&self) -> &TokKind {
        self.toks
            .get(self.pos + 1)
            .map(|t| &t.kind)
            .unwrap_or(&TokKind::Eof)
    }

    fn line(&self) -> usize {
        self.toks[self.pos].line
    }

    fn bump(&mut self) -> TokKind {
        let k = self.toks[self.pos].kind.clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        k
    }

    fn at_sym(&self, s: Sym) -> bool {
        matches!(self.peek(), TokKind::Sym(x) if *x == s)
    }

    fn at_kw(&self, k: Kw) -> bool {
        matches!(self.peek(), TokKind::Kw(x) if *x == k)
    }

    fn eat_sym(&mut self, s: Sym) -> bool {
        if self.at_sym(s) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn eat_kw(&mut self, k: Kw) -> bool {
        if self.at_kw(k) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect_sym(&mut self, s: Sym) -> PResult<()> {
        if self.eat_sym(s) {
            Ok(())
        } else {
            Err(self.err(&format!("expected {:?}, found {:?}", s, self.peek())))
        }
    }

    fn expect_ident(&mut self) -> PResult<String> {
        match self.peek().clone() {
            TokKind::Ident(s) => {
                self.bump();
                Ok(s)
            }
            // a handful of contextual keywords are valid identifiers in places
            // (e.g. property access `get`/`set`, the `default` accessor)
            other => Err(self.err(&format!("expected identifier, found {:?}", other))),
        }
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek(), TokKind::Eof)
    }

    fn err(&self, msg: &str) -> ParseError {
        ParseError {
            message: msg.to_string(),
            line: self.line(),
        }
    }

    /// Validate that a `#if` condition is a single bare flag (an identifier).
    /// Boolean conditions (`!X`, `A && B`, parentheses) are rejected: they map
    /// to `#ifdef`, which only takes one macro name.
    fn bare_flag(&self, cond: &str) -> PResult<String> {
        let ok = !cond.is_empty()
            && cond.bytes().enumerate().all(|(i, b)| {
                if i == 0 {
                    b == b'_' || b.is_ascii_alphabetic()
                } else {
                    b == b'_' || b.is_ascii_alphanumeric()
                }
            });
        if ok {
            Ok(cond.to_string())
        } else {
            Err(self.err(&format!(
                "only a bare conditional-compilation flag is supported \
                 (e.g. `#if DREAMCAST`); `{cond}` is not a single flag"
            )))
        }
    }

    /// Lower a boolean `#if` condition over flags to its C++ preprocessor
    /// spelling: each flag becomes `defined(FLAG)`, and `!`, `&&`, `||` and
    /// parentheses pass through (`(A && !B)` → `(defined(A) && !defined(B))`).
    /// Anything else — a version comparison, a `-D` value, an arithmetic
    /// expression — is an error: it has no `defined(…)` mapping.
    fn pp_condition(&self, cond: &str) -> PResult<String> {
        let bytes = cond.as_bytes();
        let mut out = String::new();
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            match b {
                b' ' | b'\t' => {
                    out.push(b as char);
                    i += 1;
                }
                b'(' | b')' => {
                    out.push(b as char);
                    i += 1;
                }
                b'!' if i + 1 < bytes.len() && bytes[i + 1] != b'=' => {
                    out.push('!');
                    i += 1;
                }
                b'&' if i + 1 < bytes.len() && bytes[i + 1] == b'&' => {
                    out.push_str("&&");
                    i += 2;
                }
                b'|' if i + 1 < bytes.len() && bytes[i + 1] == b'|' => {
                    out.push_str("||");
                    i += 2;
                }
                b'_' | b'a'..=b'z' | b'A'..=b'Z' => {
                    let start = i;
                    while i < bytes.len() && (bytes[i] == b'_' || bytes[i].is_ascii_alphanumeric())
                    {
                        i += 1;
                    }
                    out.push_str(&format!("defined({})", &cond[start..i]));
                }
                _ => {
                    return Err(self.err(&format!(
                        "only flags combined with `!`, `&&`, `||` and parentheses are \
                         supported in a `#if` condition (e.g. `#if (DREAMCAST && !DEBUG)`); \
                         `{cond}` is not"
                    )));
                }
            }
        }
        Ok(out)
    }

    /// Skip a declaration body: advance to the next `{`, then consume the balanced
    /// `{ … }` group. Used for declarations Hatchet recognises but does not yet
    /// transpile (`abstract` / `enum abstract`), so the rest of the file still parses.
    fn skip_braced_body(&mut self) -> PResult<()> {
        while !self.at_sym(Sym::LBrace) {
            if self.at_eof() {
                return Err(self.err("expected `{` to begin the declaration body"));
            }
            self.bump();
        }
        let mut depth = 0usize;
        loop {
            if self.at_eof() {
                return Err(self.err("unterminated declaration body"));
            }
            match self.peek() {
                TokKind::Sym(Sym::LBrace) => depth += 1,
                TokKind::Sym(Sym::RBrace) => depth -= 1,
                _ => {}
            }
            self.bump();
            if depth == 0 {
                return Ok(());
            }
        }
    }

    // ---- metadata -------------------------------------------------------

    fn parse_meta_list(&mut self) -> PResult<Vec<Meta>> {
        let mut metas = Vec::new();
        while let TokKind::Meta(name) = self.peek().clone() {
            self.bump();
            let mut args = Vec::new();
            if self.at_sym(Sym::LParen) {
                args = self.parse_meta_args()?;
            }
            metas.push(Meta { name, args });
        }
        Ok(metas)
    }

    /// Parse `( arg, arg, ... )` capturing each top-level argument's raw source.
    /// A lone string argument is captured as its inner text (so `@:native("x")`
    /// yields `x`); anything else is captured as the verbatim source slice.
    fn parse_meta_args(&mut self) -> PResult<Vec<String>> {
        self.expect_sym(Sym::LParen)?;
        let mut args = Vec::new();
        if self.at_sym(Sym::RParen) {
            self.bump();
            return Ok(args);
        }
        loop {
            let start_tok = self.pos;
            self.skip_balanced_until_arg_end()?;
            let end_tok = self.pos; // one past the last token of this arg
            args.push(self.capture_arg(start_tok, end_tok));
            if self.eat_sym(Sym::Comma) {
                continue;
            }
            break;
        }
        self.expect_sym(Sym::RParen)?;
        Ok(args)
    }

    /// Advance over one argument, stopping before a top-level `,` or `)`.
    /// Consume a balanced `{ ... }` block without parsing its contents. Used to
    /// skip the body of constructs Hatchet does not transpile (e.g. `macro`
    /// functions), which may contain syntax the expression parser rejects.
    fn skip_braced_block(&mut self) -> PResult<()> {
        self.expect_sym(Sym::LBrace)?;
        let mut depth = 1i32;
        loop {
            match self.peek() {
                TokKind::Eof => return Err(self.err("unterminated block")),
                TokKind::Sym(Sym::LBrace) => depth += 1,
                TokKind::Sym(Sym::RBrace) => {
                    depth -= 1;
                    self.bump();
                    if depth == 0 {
                        return Ok(());
                    }
                    continue;
                }
                _ => {}
            }
            self.bump();
        }
    }

    fn skip_balanced_until_arg_end(&mut self) -> PResult<()> {
        let mut depth = 0i32;
        loop {
            match self.peek() {
                TokKind::Eof => return Err(self.err("unterminated metadata arguments")),
                TokKind::Sym(Sym::LParen | Sym::LBracket | Sym::LBrace) => depth += 1,
                TokKind::Sym(Sym::RParen | Sym::RBracket | Sym::RBrace) => {
                    if depth == 0 {
                        return Ok(());
                    }
                    depth -= 1;
                }
                TokKind::Sym(Sym::Comma) if depth == 0 => return Ok(()),
                _ => {}
            }
            self.bump();
        }
    }

    fn capture_arg(&self, start_tok: usize, end_tok: usize) -> String {
        // Single string literal → its inner content.
        if end_tok == start_tok + 1 {
            if let TokKind::Str { raw, .. } = &self.toks[start_tok].kind {
                return raw.clone();
            }
        }
        let start = self.toks[start_tok].start;
        let end = self.toks[end_tok.saturating_sub(1)].end;
        String::from_utf8_lossy(&self.src[start..end])
            .trim()
            .to_string()
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(src: &str) -> File {
        parse(src).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn parses_package_and_imports() {
        let f =
            file("package modules;\nimport native.api.Native;\nimport modules.Module;\nclass X {}");
        assert_eq!(f.package, vec!["modules"]);
        assert_eq!(f.imports.len(), 2);
        assert_eq!(f.imports[0].path, vec!["native", "api", "Native"]);
    }

    #[test]
    fn parses_empty_package_as_root() {
        // `package;` is valid Haxe and equivalent to omitting the declaration:
        // the file lives in the root package.
        let f = file("package;\nclass X {}");
        assert!(f.package.is_empty(), "empty package should be the root");
        assert_eq!(f.decls.len(), 1);
    }

    #[test]
    fn parses_class_less_file_with_file_level_metadata() {
        // A `StdAfx.hx` style file: package + `@:headerCode`, no declaration.
        let f = file("package modules;\n@:headerCode('#include <vector>')");
        assert_eq!(f.package, vec!["modules"]);
        assert!(f.decls.is_empty(), "no declarations expected");
        assert_eq!(f.meta.len(), 1);
        assert_eq!(f.meta[0].name, "headerCode");
        assert_eq!(f.meta[0].first_arg(), Some("#include <vector>"));
    }

    #[test]
    fn parses_statement_level_cpp_file_code_as_verbatim() {
        let f = file(
            "package p;\nclass X {\n\
               function f():Void {\n\
                 @:cppFileCode('#ifdef DREAMCAST')\n\
                 var a:Int = 1;\n\
               }\n\
             }",
        );
        let cls = match &f.decls[0] {
            Decl::Class(c) => c,
            _ => panic!("expected class"),
        };
        let body = cls.methods[0].body.as_ref().expect("body");
        match &body[0] {
            Stmt::Verbatim { code, .. } => assert_eq!(code, "#ifdef DREAMCAST"),
            other => panic!("expected verbatim statement, got {other:?}"),
        }
    }

    #[test]
    fn parses_conditional_compilation_into_verbatim_statements() {
        // `#if FLAG`/`#else`/`#end` and a statement-level `@:include` lower to
        // `Stmt::Verbatim` carrying the C++ preprocessor lines; `untyped` wraps its
        // operand (a real, transpiled expression) in `Expr::Untyped`.
        let f = file(
            "package p;\nclass X {\n\
               function f(d:Float):Float {\n\
             #if DREAMCAST\n\
                 @:include('<dc/fmath.h>');\n\
                 return untyped fsqrtf(d * d);\n\
             #else\n\
                 return d;\n\
             #end\n\
               }\n\
             }",
        );
        let cls = match &f.decls[0] {
            Decl::Class(c) => c,
            _ => panic!("expected class"),
        };
        let body = cls.methods[0].body.as_ref().expect("body");
        let verbatims: Vec<&str> = body
            .iter()
            .filter_map(|s| match s {
                Stmt::Verbatim { code, .. } => Some(code.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            verbatims,
            vec![
                "#ifdef DREAMCAST",
                "#include <dc/fmath.h>",
                "#else",
                "#endif"
            ]
        );
        // The DREAMCAST `return` carries the `untyped` operand as a real call expr.
        match &body[2] {
            Stmt::Return(Some(Expr::Untyped(inner)), _) => match &**inner {
                Expr::Call(target, args) => {
                    assert_eq!(**target, Expr::Ident("fsqrtf".into()));
                    assert_eq!(args.len(), 1);
                }
                other => panic!("expected untyped call, got {other:?}"),
            },
            other => panic!("expected untyped return, got {other:?}"),
        }
    }

    #[test]
    fn strips_a_leading_utf8_bom_keeping_offsets_aligned() {
        // A leading UTF-8 BOM (U+FEFF) some editors prepend is stripped before lexing.
        // Because it is stripped in `parse` (not `lex`), the parser's source slice stays
        // byte-aligned with the token offsets, so raw metadata-argument spans come out
        // exact, not shifted by the 3 BOM bytes.
        let f = file(
            "\u{FEFF}package p;\nclass X {\n  function f(d:Float):Float { return untyped fsqrtf(d * d); }\n}",
        );
        assert_eq!(f.package, vec!["p"]);
        let cls = match &f.decls[0] {
            Decl::Class(c) => c,
            _ => panic!("expected class"),
        };
        let body = cls.methods[0].body.as_ref().expect("body");
        match &body[0] {
            Stmt::Return(Some(Expr::Untyped(inner)), _) => {
                assert!(matches!(&**inner, Expr::Call(..)), "got {inner:?}");
            }
            other => panic!("expected untyped return, got {other:?}"),
        }
    }

    #[test]
    fn conditional_compilation_flags_and_boolean_conditions() {
        // Boolean conditions over flags map each flag through `defined(…)`.
        assert!(
            parse("package p;\nclass X {\n function f():Void {\n#if !DEBUG\n#end\n }\n}").is_ok()
        );
        assert!(
            parse("package p;\nclass X {\n function f():Void {\n#if (A && B)\n#end\n }\n}").is_ok()
        );
        // A bare `#elseif FLAG` is supported (→ `#elif defined(FLAG)`)...
        assert!(parse(
            "package p;\nclass X {\n function f():Void {\n#if A\n#elseif B\n#end\n }\n}"
        )
        .is_ok());
        // ...as is a boolean `#elseif` condition (→ `#elif defined(B) && …`).
        assert!(parse(
            "package p;\nclass X {\n function f():Void {\n#if A\n#elseif (B && C)\n#end\n }\n}"
        )
        .is_ok());
        // Version comparisons / values have no `defined(…)` mapping — rejected.
        assert!(parse(
            "package p;\nclass X {\n function f():Void {\n#if (haxe_ver >= 4)\n#end\n }\n}"
        )
        .is_err());
    }

    #[test]
    fn parses_multiline_cpp_file_code_verbatim() {
        // A single `@:cppFileCode` whose string literal spans several lines is
        // captured verbatim (internal newlines preserved).
        let f = file(
            "package p;\nclass X {\n\
               function f():Void {\n\
                 @:cppFileCode('#ifdef DC\n#include <dc/fmath.h>\n#else')\n\
               }\n\
             }",
        );
        let cls = match &f.decls[0] {
            Decl::Class(c) => c,
            _ => panic!("expected class"),
        };
        let body = cls.methods[0].body.as_ref().expect("body");
        match &body[0] {
            Stmt::Verbatim { code, .. } => {
                assert_eq!(code, "#ifdef DC\n#include <dc/fmath.h>\n#else");
            }
            other => panic!("expected verbatim statement, got {other:?}"),
        }
    }

    #[test]
    fn parses_parenthesized_function_type_annotation() {
        // `Square:(Int, Int) -> Int = (a, b) -> a * b;` — the function-type
        // annotation on the binding parses to `Type::Func`.
        let f = file("package p;\nfinal Square:(Int, Int) -> Int = (a, b) -> a * b;");
        let g = match &f.decls[0] {
            Decl::Global(g) => g,
            other => panic!("expected global, got {other:?}"),
        };
        match &g.ty {
            Some(Type::Func { params, ret }) => {
                assert_eq!(params.len(), 2, "two parameter types");
                assert_eq!(ret.base_name(), Some("Int"), "return type is Int");
            }
            other => panic!("expected Type::Func, got {other:?}"),
        }
    }

    #[test]
    fn parses_class_with_accessors_and_ctor() {
        let f = file(
            "package modules;\n\
             class Vertex extends Module {\n\
               public var x(default, set):Float;\n\
               public function new(engine:IEngine, x:Float) { this.x = x; }\n\
               public function set_x(x:Float) { return this.x = x; }\n\
             }",
        );
        let Decl::Class(c) = &f.decls[0] else {
            panic!()
        };
        assert_eq!(c.name, "Vertex");
        assert_eq!(c.extends.as_ref().unwrap().base_name(), Some("Module"));
        assert_eq!(c.fields[0].name, "x");
        assert_eq!(c.fields[0].set, PropAccess::Set);
        assert!(c.ctor.is_some());
        assert_eq!(c.methods.len(), 1);
    }

    #[test]
    fn parses_enum_and_typedef_and_native_meta() {
        let f = file(
            "package native.api;\n\
             @:include(\"../../src/Native.h\")\n\
             @:native enum EffectType { Unknown; Fog; }\n\
             @:native typedef Vertex = { x:Float, ?color:UInt32 };\n\
             @:native typedef Cursor = TexturedQuad;",
        );
        let Decl::Enum(e) = &f.decls[0] else {
            panic!("{:?}", f.decls[0])
        };
        assert_eq!(e.name, "EffectType");
        assert_eq!(e.variants.len(), 2);
        assert!(has_meta(&e.meta, "native"));
        assert_eq!(
            e.meta
                .iter()
                .find(|m| m.name == "include")
                .unwrap()
                .first_arg(),
            Some("../../src/Native.h")
        );

        let Decl::Typedef(t) = &f.decls[1] else {
            panic!()
        };
        let TypedefTarget::Struct(fields) = &t.target else {
            panic!()
        };
        assert_eq!(fields[1].name, "color");
        assert!(fields[1].optional);

        let Decl::Typedef(t2) = &f.decls[2] else {
            panic!()
        };
        assert!(matches!(t2.target, TypedefTarget::Alias(_)));
    }

    #[test]
    fn parses_interface_with_overloads() {
        let f = file(
            "package native.api;\n\
             @:native interface ISceneManager {\n\
               @:overload(function(s:Int):Void {})\n\
               public function SetScene(s:Dynamic):Void;\n\
             }",
        );
        let Decl::Interface(i) = &f.decls[0] else {
            panic!()
        };
        assert_eq!(i.methods.len(), 1);
        assert!(has_meta(&i.methods[0].meta, "overload"));
    }

    #[test]
    fn parses_switch_and_sugar() {
        let f = file(
            "package game;\nclass S {\n\
               public function f(button:Int):Void {\n\
                 switch (button) {\n\
                   case Left: { this.x = 1; }\n\
                   case Right: { trace(\"r\"); }\n\
                   default: {}\n\
                 }\n\
                 var w = width ?? 128;\n\
                 var list = [for (i in 0...6) i * 2];\n\
                 this.coords.SetText(x + \",\" + y);\n\
               }\n\
             }",
        );
        let Decl::Class(c) = &f.decls[0] else {
            panic!()
        };
        let body = c.methods[0].body.as_ref().unwrap();
        assert!(matches!(body[0], Stmt::Switch { .. }));
        if let Stmt::Var {
            init: Some(Expr::NullCoalesce(..)),
            ..
        } = &body[1]
        {
        } else {
            panic!("expected null-coalesce var, got {:?}", body[1]);
        }
        assert!(matches!(
            body[2],
            Stmt::Var {
                init: Some(Expr::Comprehension { .. }),
                ..
            }
        ));
    }

    #[test]
    fn parses_anon_struct_and_map_literal() {
        let f = file(
            "package game;\nclass S {\n\
               public function f():Void {\n\
                 var s = { walk: { targetIndex: 0, walking: false } };\n\
                 var m = [ \"idle\" => { loop:true }, \"walk\" => { loop:false } ];\n\
               }\n\
             }",
        );
        let Decl::Class(c) = &f.decls[0] else {
            panic!()
        };
        let body = c.methods[0].body.as_ref().unwrap();
        assert!(matches!(
            &body[0],
            Stmt::Var {
                init: Some(Expr::ObjectLit(_)),
                ..
            }
        ));
        assert!(matches!(
            &body[1],
            Stmt::Var {
                init: Some(Expr::MapLit(_)),
                ..
            }
        ));
    }
}
