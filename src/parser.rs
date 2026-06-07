//! Recursive-descent parser: tokens → `ast::File`.
//!
//! The grammar covers the Haxe subset Hatchet supports. Input is
//! assumed compile-time valid (it already passes `haxe`/`hxcpp`), so the parser
//! favours clear structure over exhaustive error recovery; on anything it does
//! not understand it stops with a `ParseError` carrying a line number.

use std::fmt;

use crate::ast::*;
use crate::lexer::{lex, Kw, Sym, TokKind, Token};

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

    // ---- file -----------------------------------------------------------

    fn parse_file(&mut self) -> PResult<File> {
        let mut file = File {
            package: Vec::new(),
            imports: Vec::new(),
            usings: Vec::new(),
            decls: Vec::new(),
            meta: Vec::new(),
        };

        if self.eat_kw(Kw::Package) {
            file.package = self.parse_dotted()?;
            self.expect_sym(Sym::Semi)?;
        }

        loop {
            // imports / usings may be interleaved with metadata-free positions
            if self.at_kw(Kw::Import) {
                file.imports.push(self.parse_import()?);
                continue;
            }
            if self.at_kw(Kw::Using) {
                self.bump();
                let path = self.parse_dotted()?;
                self.expect_sym(Sym::Semi)?;
                file.usings.push(path);
                continue;
            }
            if self.at_eof() {
                break;
            }
            // A declaration may be preceded by metadata. Parse that metadata
            // first: if nothing but EOF follows, this is a class-less file
            // (e.g. `StdAfx.hx` carrying only `@:headerCode`) and the metadata
            // belongs to the file, not a declaration.
            let meta = self.parse_meta_list()?;
            if self.at_eof() {
                file.meta = meta;
                break;
            }
            let decl = self.parse_decl_body(meta)?;
            file.decls.push(decl);
        }

        Ok(file)
    }

    fn parse_dotted(&mut self) -> PResult<Vec<String>> {
        let mut parts = vec![self.expect_ident()?];
        while self.eat_sym(Sym::Dot) {
            parts.push(self.expect_ident()?);
        }
        Ok(parts)
    }

    fn parse_import(&mut self) -> PResult<Import> {
        self.expect_sym_kw(Kw::Import)?;
        let mut path = vec![self.expect_ident()?];
        let mut wildcard = false;
        while self.eat_sym(Sym::Dot) {
            if self.eat_sym(Sym::Star) {
                wildcard = true;
                break;
            }
            path.push(self.expect_ident()?);
        }
        let mut alias = None;
        // `import a.b.C as D;` or `... in D;`
        if let TokKind::Ident(kw) = self.peek().clone() {
            if kw == "as" {
                self.bump();
                alias = Some(self.expect_ident()?);
            }
        }
        if self.at_kw(Kw::In) {
            self.bump();
            alias = Some(self.expect_ident()?);
        }
        self.expect_sym(Sym::Semi)?;
        Ok(Import { path, wildcard, alias })
    }

    fn expect_sym_kw(&mut self, k: Kw) -> PResult<()> {
        if self.eat_kw(k) {
            Ok(())
        } else {
            Err(self.err(&format!("expected keyword {:?}", k)))
        }
    }

    // ---- declarations ---------------------------------------------------

    /// Parse a declaration whose leading metadata has already been consumed (so
    /// `parse_file` can decide, after seeing the metadata, whether it precedes a
    /// declaration or stands alone as file-level metadata).
    fn parse_decl_body(&mut self, meta: Vec<Meta>) -> PResult<Decl> {
        // declaration-level modifiers — shared by classes (extern/final/abstract),
        // top-level functions (extern/inline/static/...), and globals (private).
        let mut modifiers = FnModifiers::default();
        let mut is_final_class = false;
        let mut is_abstract_class = false;
        let mut access = Access::Default;
        loop {
            match self.peek() {
                TokKind::Kw(Kw::Extern) => modifiers.is_extern = true,
                TokKind::Kw(Kw::Inline) => modifiers.is_inline = true,
                TokKind::Kw(Kw::Static) => modifiers.is_static = true,
                TokKind::Kw(Kw::Override) => modifiers.is_override = true,
                TokKind::Kw(Kw::Dynamic) => modifiers.is_dynamic = true,
                TokKind::Kw(Kw::Macro) => modifiers.is_macro = true,
                TokKind::Kw(Kw::Public) => access = set_access(access, Access::Public),
                TokKind::Kw(Kw::Private) => access = set_access(access, Access::Private),
                TokKind::Kw(Kw::Final) if matches!(self.peek2(), TokKind::Kw(Kw::Class)) => {
                    is_final_class = true;
                }
                TokKind::Kw(Kw::Abstract) if matches!(self.peek2(), TokKind::Kw(Kw::Class)) => {
                    is_abstract_class = true;
                }
                _ => break,
            }
            self.bump();
        }

        match self.peek().clone() {
            TokKind::Kw(Kw::Class) => Ok(Decl::Class(self.parse_class(
                meta,
                modifiers.is_extern,
                is_final_class,
                is_abstract_class,
            )?)),
            TokKind::Kw(Kw::Interface) => Ok(Decl::Interface(self.parse_interface(meta)?)),
            TokKind::Kw(Kw::Enum) => Ok(Decl::Enum(self.parse_enum(meta)?)),
            TokKind::Kw(Kw::Typedef) => Ok(Decl::Typedef(self.parse_typedef(meta)?)),
            TokKind::Kw(Kw::Function) => {
                // top-level function, e.g. `extern inline function f(...) {}`
                let func = self.parse_function(meta, access, modifiers)?;
                Ok(Decl::Function(func))
            }
            TokKind::Kw(Kw::Var) | TokKind::Kw(Kw::Final) => {
                let is_final_kw = self.at_kw(Kw::Final);
                self.bump();
                let g = self.parse_global_var(meta, access, is_final_kw)?;
                Ok(Decl::Global(g))
            }
            other => Err(self.err(&format!("unexpected top-level token {:?}", other))),
        }
    }

    fn parse_type_params(&mut self) -> PResult<Vec<String>> {
        let mut params = Vec::new();
        if self.eat_sym(Sym::Lt) {
            loop {
                params.push(self.expect_ident()?);
                if self.eat_sym(Sym::Comma) {
                    continue;
                }
                break;
            }
            self.expect_type_gt()?;
        }
        Ok(params)
    }

    fn parse_class(
        &mut self,
        meta: Vec<Meta>,
        is_extern: bool,
        is_final: bool,
        is_abstract: bool,
    ) -> PResult<Class> {
        self.expect_sym_kw(Kw::Class)?;
        let name = self.expect_ident()?;
        let type_params = self.parse_type_params()?;
        let mut extends = None;
        let mut implements = Vec::new();
        loop {
            if self.eat_kw(Kw::Extends) {
                extends = Some(self.parse_type()?);
            } else if self.eat_kw(Kw::Implements) {
                implements.push(self.parse_type()?);
            } else {
                break;
            }
        }
        self.expect_sym(Sym::LBrace)?;
        let mut class = Class {
            name,
            type_params,
            extends,
            implements,
            is_extern,
            is_final,
            is_abstract,
            meta,
            fields: Vec::new(),
            methods: Vec::new(),
            ctor: None,
        };
        while !self.at_sym(Sym::RBrace) && !self.at_eof() {
            if self.eat_sym(Sym::Semi) {
                continue; // stray semicolons between members
            }
            self.parse_member_into(&mut class)?;
        }
        self.expect_sym(Sym::RBrace)?;
        Ok(class)
    }

    fn parse_interface(&mut self, meta: Vec<Meta>) -> PResult<Interface> {
        self.expect_sym_kw(Kw::Interface)?;
        let name = self.expect_ident()?;
        let type_params = self.parse_type_params()?;
        let mut extends = Vec::new();
        while self.eat_kw(Kw::Extends) {
            extends.push(self.parse_type()?);
            while self.eat_sym(Sym::Comma) {
                extends.push(self.parse_type()?);
            }
        }
        self.expect_sym(Sym::LBrace)?;
        let mut iface = Interface {
            name,
            type_params,
            extends,
            meta,
            methods: Vec::new(),
            fields: Vec::new(),
        };
        while !self.at_sym(Sym::RBrace) && !self.at_eof() {
            // interface members are method signatures (and rarely fields)
            let mut tmp = Class {
                name: String::new(),
                type_params: vec![],
                extends: None,
                implements: vec![],
                is_extern: false,
                is_final: false,
                is_abstract: false,
                meta: vec![],
                fields: vec![],
                methods: vec![],
                ctor: None,
            };
            self.parse_member_into(&mut tmp)?;
            iface.methods.extend(tmp.methods);
            iface.fields.extend(tmp.fields);
            if let Some(c) = tmp.ctor {
                iface.methods.push(c);
            }
        }
        self.expect_sym(Sym::RBrace)?;
        Ok(iface)
    }

    fn parse_enum(&mut self, meta: Vec<Meta>) -> PResult<Enum> {
        self.expect_sym_kw(Kw::Enum)?;
        let name = self.expect_ident()?;
        self.expect_sym(Sym::LBrace)?;
        let mut variants = Vec::new();
        while !self.at_sym(Sym::RBrace) && !self.at_eof() {
            let vname = self.expect_ident()?;
            let mut params = Vec::new();
            if self.at_sym(Sym::LParen) {
                params = self.parse_params()?;
            }
            self.eat_sym(Sym::Semi);
            variants.push(EnumVariant { name: vname, params });
        }
        self.expect_sym(Sym::RBrace)?;
        Ok(Enum { name, meta, variants })
    }

    fn parse_typedef(&mut self, meta: Vec<Meta>) -> PResult<Typedef> {
        self.expect_sym_kw(Kw::Typedef)?;
        let name = self.expect_ident()?;
        self.parse_type_params()?; // discard (corpus typedefs are not generic)
        self.expect_sym(Sym::Assign)?;
        let target = if self.at_sym(Sym::LBrace) {
            TypedefTarget::Struct(self.parse_struct_fields()?)
        } else {
            TypedefTarget::Alias(self.parse_type()?)
        };
        self.eat_sym(Sym::Semi);
        Ok(Typedef { name, meta, target })
    }

    fn parse_global_var(
        &mut self,
        meta: Vec<Meta>,
        access: Access,
        is_final: bool,
    ) -> PResult<GlobalVar> {
        let name = self.expect_ident()?;
        let ty = if self.eat_sym(Sym::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let init = if self.eat_sym(Sym::Assign) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        self.eat_sym(Sym::Semi);
        Ok(GlobalVar {
            name,
            ty,
            init,
            is_final,
            access,
            meta,
        })
    }

    // ---- members --------------------------------------------------------

    fn parse_member_into(&mut self, class: &mut Class) -> PResult<()> {
        let meta = self.parse_meta_list()?;

        let mut access = Access::Default;
        let mut is_static = false;
        let mut modifiers = FnModifiers::default();
        if has_meta(&meta, "protected") {
            access = Access::Protected;
        }
        loop {
            match self.peek() {
                TokKind::Kw(Kw::Public) => access = set_access(access, Access::Public),
                TokKind::Kw(Kw::Private) => access = set_access(access, Access::Private),
                TokKind::Kw(Kw::Static) => is_static = true,
                TokKind::Kw(Kw::Inline) => modifiers.is_inline = true,
                TokKind::Kw(Kw::Extern) => modifiers.is_extern = true,
                TokKind::Kw(Kw::Override) => modifiers.is_override = true,
                TokKind::Kw(Kw::Dynamic) => modifiers.is_dynamic = true,
                TokKind::Kw(Kw::Abstract) => modifiers.is_abstract = true,
                TokKind::Kw(Kw::Macro) => modifiers.is_macro = true,
                // `final function` (rare) — function modifier; `final var` is a field
                TokKind::Kw(Kw::Final) if matches!(self.peek2(), TokKind::Kw(Kw::Function)) => {
                    modifiers.is_final = true;
                }
                _ => break,
            }
            self.bump();
        }

        match self.peek().clone() {
            TokKind::Kw(Kw::Var) | TokKind::Kw(Kw::Final) => {
                let is_final = self.at_kw(Kw::Final);
                self.bump();
                let field = self.parse_field(meta, access, is_static, is_final)?;
                class.fields.push(field);
            }
            TokKind::Kw(Kw::Function) => {
                modifiers.is_static = is_static;
                let func = self.parse_function(meta, access, modifiers)?;
                if func.name.is_none() {
                    class.ctor = Some(func);
                } else {
                    class.methods.push(func);
                }
            }
            other => return Err(self.err(&format!("unexpected class member {:?}", other))),
        }
        Ok(())
    }

    fn parse_field(
        &mut self,
        meta: Vec<Meta>,
        access: Access,
        is_static: bool,
        is_final: bool,
    ) -> PResult<Field> {
        let name = self.expect_ident()?;
        let (mut get, mut set) = (PropAccess::Default, PropAccess::Default);
        if self.eat_sym(Sym::LParen) {
            get = self.parse_prop_access()?;
            self.expect_sym(Sym::Comma)?;
            set = self.parse_prop_access()?;
            self.expect_sym(Sym::RParen)?;
        }
        let ty = if self.eat_sym(Sym::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let init = if self.eat_sym(Sym::Assign) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        self.eat_sym(Sym::Semi);
        Ok(Field {
            name,
            ty,
            init,
            access,
            is_static,
            is_final,
            get,
            set,
            meta,
        })
    }

    fn parse_prop_access(&mut self) -> PResult<PropAccess> {
        let word = match self.peek().clone() {
            TokKind::Ident(s) => s,
            TokKind::Kw(Kw::Default) => "default".to_string(),
            TokKind::Kw(Kw::Null) => "null".to_string(),
            TokKind::Kw(Kw::Dynamic) => "dynamic".to_string(),
            other => return Err(self.err(&format!("expected property access, found {:?}", other))),
        };
        self.bump();
        Ok(match word.as_str() {
            "default" => PropAccess::Default,
            "null" => PropAccess::Null,
            "get" => PropAccess::Get,
            "set" => PropAccess::Set,
            "never" => PropAccess::Never,
            "dynamic" => PropAccess::Dynamic,
            _ => return Err(self.err(&format!("unknown property access '{word}'"))),
        })
    }

    fn parse_function(
        &mut self,
        meta: Vec<Meta>,
        access: Access,
        modifiers: FnModifiers,
    ) -> PResult<Function> {
        self.expect_sym_kw(Kw::Function)?;
        let name = if self.eat_kw(Kw::New) {
            None
        } else {
            Some(self.expect_ident()?)
        };
        self.parse_type_params()?; // generic methods: discard params
        let params = self.parse_params()?;
        let ret = if self.eat_sym(Sym::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let body = if self.at_sym(Sym::LBrace) {
            Some(self.parse_block()?)
        } else {
            self.eat_sym(Sym::Semi);
            None
        };
        Ok(Function {
            name,
            params,
            ret,
            body,
            access,
            modifiers,
            meta,
        })
    }

    fn parse_params(&mut self) -> PResult<Vec<Param>> {
        self.expect_sym(Sym::LParen)?;
        let mut params = Vec::new();
        if self.eat_sym(Sym::RParen) {
            return Ok(params);
        }
        loop {
            let optional = self.eat_sym(Sym::Question);
            let name = self.expect_ident()?;
            let ty = if self.eat_sym(Sym::Colon) {
                Some(self.parse_type()?)
            } else {
                None
            };
            let default = if self.eat_sym(Sym::Assign) {
                Some(self.parse_expr()?)
            } else {
                None
            };
            params.push(Param {
                name,
                ty,
                optional: optional || default.is_some(),
                default,
            });
            if self.eat_sym(Sym::Comma) {
                continue;
            }
            break;
        }
        self.expect_sym(Sym::RParen)?;
        Ok(params)
    }

    // ---- types ----------------------------------------------------------

    fn parse_type(&mut self) -> PResult<Type> {
        // Parenthesized function type `(A, B) -> R` (modern Haxe). Parameters may
        // carry a `name:` label (`(a:Int, b:Int) -> Int`), which is ignored — only
        // the types matter for the C++ signature.
        if self.at_sym(Sym::LParen) {
            self.bump();
            let mut params = Vec::new();
            if !self.at_sym(Sym::RParen) {
                loop {
                    if matches!(self.peek(), TokKind::Ident(_))
                        && matches!(self.peek2(), TokKind::Sym(Sym::Colon))
                    {
                        self.bump(); // label
                        self.bump(); // ':'
                    }
                    params.push(self.parse_type()?);
                    if self.eat_sym(Sym::Comma) {
                        continue;
                    }
                    break;
                }
            }
            self.expect_sym(Sym::RParen)?;
            self.expect_sym(Sym::Arrow)?;
            let ret = self.parse_type()?;
            return Ok(Type::Func { params, ret: Box::new(ret) });
        }
        let first = self.parse_type_atom()?;
        // function type `A -> B`
        if self.at_sym(Sym::Arrow) {
            let mut params = vec![first];
            while self.eat_sym(Sym::Arrow) {
                params.push(self.parse_type_atom()?);
            }
            let ret = params.pop().unwrap();
            return Ok(Type::Func {
                params,
                ret: Box::new(ret),
            });
        }
        Ok(first)
    }

    fn parse_type_atom(&mut self) -> PResult<Type> {
        let optional = self.eat_sym(Sym::Question);
        let line = self.line();
        if self.at_sym(Sym::LBrace) {
            let fields = self.parse_struct_fields()?;
            return Ok(Type::Anon(fields));
        }
        let mut path = vec![self.expect_ident()?];
        while self.at_sym(Sym::Dot) && matches!(self.peek2(), TokKind::Ident(_)) {
            self.bump();
            path.push(self.expect_ident()?);
        }
        let mut params = Vec::new();
        if self.eat_sym(Sym::Lt) {
            loop {
                params.push(self.parse_type()?);
                if self.eat_sym(Sym::Comma) {
                    continue;
                }
                break;
            }
            self.expect_type_gt()?;
        }
        Ok(Type::Named {
            path,
            params,
            optional,
            line,
        })
    }

    fn parse_struct_fields(&mut self) -> PResult<Vec<StructField>> {
        self.expect_sym(Sym::LBrace)?;
        let mut fields = Vec::new();
        while !self.at_sym(Sym::RBrace) && !self.at_eof() {
            // Both short notation (`name:Type,`) and class notation
            // (`var name:Type;`, `var ?name:Type;`) are accepted.
            self.eat_kw(Kw::Var);
            let optional = self.eat_sym(Sym::Question);
            let name = self.field_key()?;
            self.expect_sym(Sym::Colon)?;
            let ty = self.parse_type()?;
            fields.push(StructField { name, optional, ty });
            if !self.eat_sym(Sym::Comma) {
                self.eat_sym(Sym::Semi);
            }
        }
        self.expect_sym(Sym::RBrace)?;
        Ok(fields)
    }

    fn field_key(&mut self) -> PResult<String> {
        match self.peek().clone() {
            TokKind::Ident(s) => {
                self.bump();
                Ok(s)
            }
            TokKind::Str { raw, .. } => {
                self.bump();
                Ok(raw)
            }
            other => Err(self.err(&format!("expected field name, found {:?}", other))),
        }
    }

    /// Consume a `>` that closes a type parameter list, splitting `>>`/`>=`/`>>=`
    /// as needed (the lexer greedily merges them).
    fn expect_type_gt(&mut self) -> PResult<()> {
        match self.peek() {
            TokKind::Sym(Sym::Gt) => {
                self.bump();
                Ok(())
            }
            TokKind::Sym(Sym::Shr) => {
                self.toks[self.pos].kind = TokKind::Sym(Sym::Gt);
                Ok(())
            }
            TokKind::Sym(Sym::Ge) => {
                self.toks[self.pos].kind = TokKind::Sym(Sym::Assign);
                Ok(())
            }
            TokKind::Sym(Sym::ShrEq) => {
                self.toks[self.pos].kind = TokKind::Sym(Sym::Ge);
                Ok(())
            }
            other => Err(self.err(&format!("expected '>', found {:?}", other))),
        }
    }

    // ---- statements -----------------------------------------------------

    fn parse_block(&mut self) -> PResult<Vec<Stmt>> {
        self.expect_sym(Sym::LBrace)?;
        let mut stmts = Vec::new();
        while !self.at_sym(Sym::RBrace) && !self.at_eof() {
            if self.eat_sym(Sym::Semi) {
                continue;
            }
            stmts.push(self.parse_stmt()?);
        }
        self.expect_sym(Sym::RBrace)?;
        Ok(stmts)
    }

    fn parse_stmt(&mut self) -> PResult<Stmt> {
        let line = self.line();
        match self.peek().clone() {
            TokKind::Kw(Kw::Var) | TokKind::Kw(Kw::Final) => {
                let is_final = self.at_kw(Kw::Final);
                self.bump();
                let name = self.expect_ident()?;
                let ty = if self.eat_sym(Sym::Colon) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                let init = if self.eat_sym(Sym::Assign) {
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                self.eat_sym(Sym::Semi);
                Ok(Stmt::Var {
                    name,
                    ty,
                    init,
                    is_final,
                    line,
                })
            }
            TokKind::Kw(Kw::If) => self.parse_if(),
            TokKind::Kw(Kw::For) => self.parse_for(),
            TokKind::Kw(Kw::While) => {
                self.bump();
                self.expect_sym(Sym::LParen)?;
                let cond = self.parse_expr()?;
                self.expect_sym(Sym::RParen)?;
                let body = Box::new(self.parse_stmt()?);
                Ok(Stmt::While {
                    cond,
                    body,
                    do_while: false,
                    line,
                })
            }
            TokKind::Kw(Kw::Do) => {
                self.bump();
                let body = Box::new(self.parse_stmt()?);
                self.expect_sym_kw(Kw::While)?;
                self.expect_sym(Sym::LParen)?;
                let cond = self.parse_expr()?;
                self.expect_sym(Sym::RParen)?;
                self.eat_sym(Sym::Semi);
                Ok(Stmt::While {
                    cond,
                    body,
                    do_while: true,
                    line,
                })
            }
            TokKind::Kw(Kw::Switch) => self.parse_switch(),
            TokKind::Kw(Kw::Return) => {
                self.bump();
                let e = if self.at_sym(Sym::Semi) || self.at_sym(Sym::RBrace) {
                    None
                } else {
                    Some(self.parse_expr()?)
                };
                self.eat_sym(Sym::Semi);
                Ok(Stmt::Return(e, line))
            }
            TokKind::Kw(Kw::Break) => {
                self.bump();
                self.eat_sym(Sym::Semi);
                Ok(Stmt::Break)
            }
            TokKind::Kw(Kw::Continue) => {
                self.bump();
                self.eat_sym(Sym::Semi);
                Ok(Stmt::Continue)
            }
            TokKind::Kw(Kw::Throw) => {
                self.bump();
                let e = self.parse_expr()?;
                self.eat_sym(Sym::Semi);
                Ok(Stmt::Throw(e, line))
            }
            TokKind::Sym(Sym::LBrace) => Ok(Stmt::Block(self.parse_block()?)),
            // Statement-level `@:cppFileCode('...')` injects verbatim C++ here.
            TokKind::Meta(name) if name == "cppFileCode" => {
                self.bump();
                let args = if self.at_sym(Sym::LParen) {
                    self.parse_meta_args()?
                } else {
                    Vec::new()
                };
                let code = args.into_iter().next().unwrap_or_default();
                self.eat_sym(Sym::Semi);
                Ok(Stmt::Verbatim { code, line })
            }
            _ => {
                let e = self.parse_expr()?;
                self.eat_sym(Sym::Semi);
                Ok(Stmt::Expr(e, line))
            }
        }
    }

    fn parse_if(&mut self) -> PResult<Stmt> {
        let line = self.line();
        self.expect_sym_kw(Kw::If)?;
        self.expect_sym(Sym::LParen)?;
        let cond = self.parse_expr()?;
        self.expect_sym(Sym::RParen)?;
        let then = Box::new(self.parse_stmt()?);
        self.eat_sym(Sym::Semi);
        let els = if self.eat_kw(Kw::Else) {
            Some(Box::new(self.parse_stmt()?))
        } else {
            None
        };
        Ok(Stmt::If { cond, then, els, line })
    }

    fn parse_for(&mut self) -> PResult<Stmt> {
        let line = self.line();
        self.expect_sym_kw(Kw::For)?;
        self.expect_sym(Sym::LParen)?;
        let var = self.expect_ident()?;
        self.expect_sym_kw(Kw::In)?;
        let iter = self.parse_iterable()?;
        self.expect_sym(Sym::RParen)?;
        let body = Box::new(self.parse_stmt()?);
        Ok(Stmt::For { var, iter, body, line })
    }

    fn parse_iterable(&mut self) -> PResult<Iterable> {
        let start = self.parse_expr()?;
        if self.eat_sym(Sym::DotDotDot) {
            let end = self.parse_expr()?;
            Ok(Iterable::Range(start, end))
        } else {
            Ok(Iterable::Coll(start))
        }
    }

    fn parse_switch(&mut self) -> PResult<Stmt> {
        let line = self.line();
        self.expect_sym_kw(Kw::Switch)?;
        let paren = self.eat_sym(Sym::LParen);
        let subject = self.parse_expr()?;
        if paren {
            self.expect_sym(Sym::RParen)?;
        }
        self.expect_sym(Sym::LBrace)?;
        let mut cases = Vec::new();
        let mut default = None;
        while !self.at_sym(Sym::RBrace) && !self.at_eof() {
            if self.eat_kw(Kw::Case) {
                let mut patterns = vec![self.parse_expr()?];
                while self.eat_sym(Sym::Comma) {
                    patterns.push(self.parse_expr()?);
                }
                self.expect_sym(Sym::Colon)?;
                let body = self.parse_case_body()?;
                cases.push(Case { patterns, body });
            } else if self.eat_kw(Kw::Default) {
                self.expect_sym(Sym::Colon)?;
                default = Some(self.parse_case_body()?);
            } else {
                return Err(self.err("expected 'case' or 'default' in switch"));
            }
        }
        self.expect_sym(Sym::RBrace)?;
        Ok(Stmt::Switch {
            subject,
            cases,
            default,
            line,
        })
    }

    /// Case bodies run until the next `case`/`default`/`}`. A single brace-block
    /// body (as in the corpus) is unwrapped into its statements.
    fn parse_case_body(&mut self) -> PResult<Vec<Stmt>> {
        if self.at_sym(Sym::LBrace) {
            return self.parse_block();
        }
        let mut body = Vec::new();
        while !self.at_kw(Kw::Case)
            && !self.at_kw(Kw::Default)
            && !self.at_sym(Sym::RBrace)
            && !self.at_eof()
        {
            if self.eat_sym(Sym::Semi) {
                continue;
            }
            body.push(self.parse_stmt()?);
        }
        Ok(body)
    }

    // ---- expressions ----------------------------------------------------

    fn parse_expr(&mut self) -> PResult<Expr> {
        self.parse_assign()
    }

    fn parse_assign(&mut self) -> PResult<Expr> {
        let lhs = self.parse_ternary()?;
        let op = match self.peek() {
            TokKind::Sym(Sym::Assign) => Some(None),
            TokKind::Sym(Sym::PlusEq) => Some(Some(BinOp::Add)),
            TokKind::Sym(Sym::MinusEq) => Some(Some(BinOp::Sub)),
            TokKind::Sym(Sym::StarEq) => Some(Some(BinOp::Mul)),
            TokKind::Sym(Sym::SlashEq) => Some(Some(BinOp::Div)),
            TokKind::Sym(Sym::PercentEq) => Some(Some(BinOp::Mod)),
            TokKind::Sym(Sym::AmpEq) => Some(Some(BinOp::BitAnd)),
            TokKind::Sym(Sym::PipeEq) => Some(Some(BinOp::BitOr)),
            TokKind::Sym(Sym::CaretEq) => Some(Some(BinOp::BitXor)),
            TokKind::Sym(Sym::ShlEq) => Some(Some(BinOp::Shl)),
            TokKind::Sym(Sym::ShrEq) => Some(Some(BinOp::Shr)),
            TokKind::Sym(Sym::QuestionQuestionEq) => {
                // x ??= y  → desugars later; model as assign of a coalesce
                self.bump();
                let value = self.parse_assign()?;
                return Ok(Expr::Assign {
                    op: None,
                    target: Box::new(lhs.clone()),
                    value: Box::new(Expr::NullCoalesce(
                        Box::new(lhs),
                        Box::new(value),
                    )),
                });
            }
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let value = self.parse_assign()?;
            Ok(Expr::Assign {
                op,
                target: Box::new(lhs),
                value: Box::new(value),
            })
        } else {
            Ok(lhs)
        }
    }

    fn parse_ternary(&mut self) -> PResult<Expr> {
        let cond = self.parse_coalesce()?;
        if self.eat_sym(Sym::Question) {
            let then = self.parse_assign()?;
            self.expect_sym(Sym::Colon)?;
            let els = self.parse_assign()?;
            Ok(Expr::Ternary {
                cond: Box::new(cond),
                then: Box::new(then),
                els: Box::new(els),
            })
        } else {
            Ok(cond)
        }
    }

    fn parse_coalesce(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_binary(0)?;
        while self.eat_sym(Sym::QuestionQuestion) {
            let rhs = self.parse_binary(0)?;
            lhs = Expr::NullCoalesce(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_binary(&mut self, min_prec: u8) -> PResult<Expr> {
        let mut lhs = self.parse_unary()?;
        while let Some((op, prec)) = self.peek_binop() {
            if prec < min_prec {
                break;
            }
            self.bump();
            let rhs = self.parse_binary(prec + 1)?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn peek_binop(&self) -> Option<(BinOp, u8)> {
        let s = match self.peek() {
            TokKind::Sym(s) => *s,
            _ => return None,
        };
        Some(match s {
            Sym::PipePipe => (BinOp::Or, 1),
            Sym::AmpAmp => (BinOp::And, 2),
            Sym::Pipe => (BinOp::BitOr, 3),
            Sym::Caret => (BinOp::BitXor, 4),
            Sym::Amp => (BinOp::BitAnd, 5),
            Sym::Eq => (BinOp::Eq, 6),
            Sym::Ne => (BinOp::Ne, 6),
            Sym::Lt => (BinOp::Lt, 7),
            Sym::Gt => (BinOp::Gt, 7),
            Sym::Le => (BinOp::Le, 7),
            Sym::Ge => (BinOp::Ge, 7),
            Sym::Shl => (BinOp::Shl, 8),
            Sym::Shr => (BinOp::Shr, 8),
            Sym::Plus => (BinOp::Add, 9),
            Sym::Minus => (BinOp::Sub, 9),
            Sym::Star => (BinOp::Mul, 10),
            Sym::Slash => (BinOp::Div, 10),
            Sym::Percent => (BinOp::Mod, 10),
            _ => return None,
        })
    }

    fn parse_unary(&mut self) -> PResult<Expr> {
        if self.eat_kw(Kw::Cast) {
            // cast(expr, Type) | cast expr
            if self.at_sym(Sym::LParen) {
                // Could be cast(expr, Type) or cast (grouped expr). Peek for a
                // top-level comma to decide.
                if self.paren_has_top_level_comma() {
                    self.expect_sym(Sym::LParen)?;
                    let expr = self.parse_expr()?;
                    self.expect_sym(Sym::Comma)?;
                    let ty = self.parse_type()?;
                    self.expect_sym(Sym::RParen)?;
                    return Ok(Expr::Cast {
                        expr: Box::new(expr),
                        ty: Some(ty),
                    });
                }
            }
            let expr = self.parse_unary()?;
            return Ok(Expr::Cast {
                expr: Box::new(expr),
                ty: None,
            });
        }
        let op = match self.peek() {
            TokKind::Sym(Sym::Minus) => Some(UnOp::Neg),
            TokKind::Sym(Sym::Bang) => Some(UnOp::Not),
            TokKind::Sym(Sym::Tilde) => Some(UnOp::BitNot),
            TokKind::Sym(Sym::PlusPlus) => Some(UnOp::Incr),
            TokKind::Sym(Sym::MinusMinus) => Some(UnOp::Decr),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let expr = self.parse_unary()?;
            return Ok(Expr::Unary {
                op,
                expr: Box::new(expr),
                prefix: true,
            });
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> PResult<Expr> {
        let mut e = self.parse_primary()?;
        loop {
            match self.peek() {
                TokKind::Sym(Sym::Dot) => {
                    self.bump();
                    let name = self.member_name()?;
                    e = Expr::Field(Box::new(e), name);
                }
                TokKind::Sym(Sym::QuestionDot) => {
                    self.bump();
                    let name = self.member_name()?;
                    e = Expr::SafeField(Box::new(e), name);
                }
                TokKind::Sym(Sym::LParen) => {
                    let args = self.parse_call_args()?;
                    e = Expr::Call(Box::new(e), args);
                }
                TokKind::Sym(Sym::LBracket) => {
                    self.bump();
                    let idx = self.parse_expr()?;
                    self.expect_sym(Sym::RBracket)?;
                    e = Expr::Index(Box::new(e), Box::new(idx));
                }
                TokKind::Sym(Sym::PlusPlus) => {
                    self.bump();
                    e = Expr::Unary {
                        op: UnOp::Incr,
                        expr: Box::new(e),
                        prefix: false,
                    };
                }
                TokKind::Sym(Sym::MinusMinus) => {
                    self.bump();
                    e = Expr::Unary {
                        op: UnOp::Decr,
                        expr: Box::new(e),
                        prefix: false,
                    };
                }
                _ => break,
            }
        }
        Ok(e)
    }

    /// A member name after `.` may be an identifier or a contextual keyword
    /// (e.g. `new` in `Type.new`, or other reserved words used as field names).
    fn member_name(&mut self) -> PResult<String> {
        match self.peek().clone() {
            TokKind::Ident(s) => {
                self.bump();
                Ok(s)
            }
            TokKind::Kw(Kw::New) => {
                self.bump();
                Ok("new".to_string())
            }
            other => Err(self.err(&format!("expected member name, found {:?}", other))),
        }
    }

    fn parse_call_args(&mut self) -> PResult<Vec<Expr>> {
        self.expect_sym(Sym::LParen)?;
        let mut args = Vec::new();
        if self.eat_sym(Sym::RParen) {
            return Ok(args);
        }
        loop {
            args.push(self.parse_expr()?);
            if self.eat_sym(Sym::Comma) {
                continue;
            }
            break;
        }
        self.expect_sym(Sym::RParen)?;
        Ok(args)
    }

    fn parse_primary(&mut self) -> PResult<Expr> {
        match self.peek().clone() {
            TokKind::Int(s) => {
                self.bump();
                Ok(Expr::Int(s))
            }
            TokKind::Float(s) => {
                self.bump();
                Ok(Expr::Float(s))
            }
            TokKind::Str { raw, interpolated } => {
                self.bump();
                Ok(Expr::Str { raw, interpolated })
            }
            TokKind::Kw(Kw::True) => {
                self.bump();
                Ok(Expr::Bool(true))
            }
            TokKind::Kw(Kw::False) => {
                self.bump();
                Ok(Expr::Bool(false))
            }
            TokKind::Kw(Kw::Null) => {
                self.bump();
                Ok(Expr::Null)
            }
            TokKind::Kw(Kw::This) => {
                self.bump();
                Ok(Expr::This)
            }
            TokKind::Kw(Kw::Super) => {
                self.bump();
                Ok(Expr::Super)
            }
            TokKind::Kw(Kw::New) => self.parse_new(),
            TokKind::Kw(Kw::Function) => self.parse_anon_function(),
            TokKind::Kw(Kw::Switch) => {
                // switch as an expression: parse as statement form, wrap.
                // (Not used by the corpus; supported for completeness.)
                Err(self.err("switch expressions are not yet supported"))
            }
            TokKind::Ident(name) => {
                // single-parameter lambda `x -> expr`
                if matches!(self.peek2(), TokKind::Sym(Sym::Arrow)) {
                    self.bump();
                    self.bump();
                    let body = self.parse_lambda_body()?;
                    return Ok(Expr::Lambda {
                        params: vec![Param {
                            name,
                            ty: None,
                            optional: false,
                            default: None,
                        }],
                        ret: None,
                        body: Box::new(body),
                    });
                }
                self.bump();
                Ok(Expr::Ident(name))
            }
            TokKind::Sym(Sym::LParen) => self.parse_paren_or_lambda(),
            TokKind::Sym(Sym::LBracket) => self.parse_array_or_comprehension(),
            TokKind::Sym(Sym::LBrace) => self.parse_object_literal(),
            other => Err(self.err(&format!("unexpected token in expression: {:?}", other))),
        }
    }

    fn parse_new(&mut self) -> PResult<Expr> {
        self.expect_sym_kw(Kw::New)?;
        let ty = self.parse_type()?;
        let args = self.parse_call_args()?;
        Ok(Expr::New(ty, args))
    }

    /// Anonymous function expression: `function (params) [:Ret] { body }`.
    /// Modelled as a `Lambda` with an explicit return type and block body.
    fn parse_anon_function(&mut self) -> PResult<Expr> {
        self.expect_sym_kw(Kw::Function)?;
        // an optional local name is allowed in Haxe; ignore it
        if let TokKind::Ident(_) = self.peek() {
            self.bump();
        }
        let params = self.parse_params()?;
        let ret = if self.eat_sym(Sym::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let body = self.parse_lambda_body()?;
        Ok(Expr::Lambda {
            params,
            ret,
            body: Box::new(body),
        })
    }

    fn parse_lambda_body(&mut self) -> PResult<LambdaBody> {
        // `-> { x: ... }` is an object literal; `-> { stmt; ... }` is a block.
        if self.at_sym(Sym::LBrace) && !self.looks_like_object_literal() {
            Ok(LambdaBody::Block(self.parse_block()?))
        } else {
            Ok(LambdaBody::Expr(self.parse_expr()?))
        }
    }

    /// With the cursor on `{`, decide whether it begins an object literal:
    /// `{}`, or `{ key : ... }` where `key` is an identifier or string.
    fn looks_like_object_literal(&self) -> bool {
        if !self.at_sym(Sym::LBrace) {
            return false;
        }
        match self.toks.get(self.pos + 1).map(|t| &t.kind) {
            Some(TokKind::Sym(Sym::RBrace)) => true,
            Some(TokKind::Ident(_) | TokKind::Str { .. }) => {
                matches!(
                    self.toks.get(self.pos + 2).map(|t| &t.kind),
                    Some(TokKind::Sym(Sym::Colon))
                )
            }
            _ => false,
        }
    }

    /// `(expr)`, `(expr : Type)`, or a lambda `(params) -> body`.
    fn parse_paren_or_lambda(&mut self) -> PResult<Expr> {
        if self.parens_introduce_lambda() {
            let params = self.parse_params()?;
            self.expect_sym(Sym::Arrow)?;
            let body = self.parse_lambda_body()?;
            return Ok(Expr::Lambda {
                params,
                ret: None,
                body: Box::new(body),
            });
        }
        self.expect_sym(Sym::LParen)?;
        let e = self.parse_expr()?;
        if self.eat_sym(Sym::Colon) {
            let ty = self.parse_type()?;
            self.expect_sym(Sym::RParen)?;
            return Ok(Expr::TypeCheck {
                expr: Box::new(e),
                ty,
            });
        }
        self.expect_sym(Sym::RParen)?;
        Ok(Expr::Paren(Box::new(e)))
    }

    fn parse_array_or_comprehension(&mut self) -> PResult<Expr> {
        self.expect_sym(Sym::LBracket)?;
        if self.at_kw(Kw::For) {
            return self.parse_comprehension_tail();
        }
        if self.eat_sym(Sym::RBracket) {
            return Ok(Expr::ArrayLit(Vec::new()));
        }
        let first = self.parse_expr()?;
        if self.eat_sym(Sym::FatArrow) {
            // map literal
            let v = self.parse_expr()?;
            let mut entries = vec![(first, v)];
            while self.eat_sym(Sym::Comma) {
                if self.at_sym(Sym::RBracket) {
                    break;
                }
                let k = self.parse_expr()?;
                self.expect_sym(Sym::FatArrow)?;
                let val = self.parse_expr()?;
                entries.push((k, val));
            }
            self.expect_sym(Sym::RBracket)?;
            return Ok(Expr::MapLit(entries));
        }
        let mut elems = vec![first];
        while self.eat_sym(Sym::Comma) {
            if self.at_sym(Sym::RBracket) {
                break;
            }
            elems.push(self.parse_expr()?);
        }
        self.expect_sym(Sym::RBracket)?;
        Ok(Expr::ArrayLit(elems))
    }

    /// After consuming `[`, parse `for (v in iter) [if (g)] body]`.
    fn parse_comprehension_tail(&mut self) -> PResult<Expr> {
        self.expect_sym_kw(Kw::For)?;
        self.expect_sym(Sym::LParen)?;
        let var = self.expect_ident()?;
        self.expect_sym_kw(Kw::In)?;
        let iter = self.parse_iterable()?;
        self.expect_sym(Sym::RParen)?;
        let guard = if self.eat_kw(Kw::If) {
            self.expect_sym(Sym::LParen)?;
            let g = self.parse_expr()?;
            self.expect_sym(Sym::RParen)?;
            Some(Box::new(g))
        } else {
            None
        };
        let key_or_val = self.parse_expr()?;
        let body = if self.eat_sym(Sym::FatArrow) {
            let v = self.parse_expr()?;
            ComprBody::KeyValue(Box::new(key_or_val), Box::new(v))
        } else {
            ComprBody::Value(Box::new(key_or_val))
        };
        self.expect_sym(Sym::RBracket)?;
        Ok(Expr::Comprehension {
            var,
            iter: Box::new(iter),
            guard,
            body,
        })
    }

    fn parse_object_literal(&mut self) -> PResult<Expr> {
        self.expect_sym(Sym::LBrace)?;
        let mut fields = Vec::new();
        if self.eat_sym(Sym::RBrace) {
            return Ok(Expr::ObjectLit(fields));
        }
        loop {
            let key = self.field_key()?;
            self.expect_sym(Sym::Colon)?;
            let value = self.parse_expr()?;
            fields.push((key, value));
            if self.eat_sym(Sym::Comma) {
                if self.at_sym(Sym::RBrace) {
                    break;
                }
                continue;
            }
            break;
        }
        self.expect_sym(Sym::RBrace)?;
        Ok(Expr::ObjectLit(fields))
    }

    // ---- lookahead helpers ---------------------------------------------

    /// Does the `(` at the cursor enclose a top-level `,` before its matching `)`?
    fn paren_has_top_level_comma(&self) -> bool {
        let mut depth = 0i32;
        let mut i = self.pos;
        // expects cursor at '('
        while i < self.toks.len() {
            match &self.toks[i].kind {
                TokKind::Sym(Sym::LParen) => depth += 1,
                TokKind::Sym(Sym::RParen) => {
                    depth -= 1;
                    if depth == 0 {
                        return false;
                    }
                }
                TokKind::Sym(Sym::Comma) if depth == 1 => return true,
                TokKind::Eof => return false,
                _ => {}
            }
            i += 1;
        }
        false
    }

    /// Is the `(` at the cursor the start of a lambda parameter list, i.e. its
    /// matching `)` is immediately followed by `->`?
    fn parens_introduce_lambda(&self) -> bool {
        let mut depth = 0i32;
        let mut i = self.pos;
        while i < self.toks.len() {
            match &self.toks[i].kind {
                TokKind::Sym(Sym::LParen) => depth += 1,
                TokKind::Sym(Sym::RParen) => {
                    depth -= 1;
                    if depth == 0 {
                        return matches!(
                            self.toks.get(i + 1).map(|t| &t.kind),
                            Some(TokKind::Sym(Sym::Arrow))
                        );
                    }
                }
                TokKind::Eof => return false,
                _ => {}
            }
            i += 1;
        }
        false
    }
}

fn set_access(current: Access, new: Access) -> Access {
    // explicit public/private win over default/protected metadata
    match current {
        Access::Default => new,
        other => other,
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
        let f = file("package modules;\nimport mucus.api.Mucus;\nimport modules.Module;\nclass X {}");
        assert_eq!(f.package, vec!["modules"]);
        assert_eq!(f.imports.len(), 2);
        assert_eq!(f.imports[0].path, vec!["mucus", "api", "Mucus"]);
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
             @:expose class Vertex extends Module {\n\
               public var x(default, set):Float;\n\
               public function new(engine:IEngine, x:Float) { this.x = x; }\n\
               public function set_x(x:Float) { return this.x = x; }\n\
             }",
        );
        let Decl::Class(c) = &f.decls[0] else { panic!() };
        assert_eq!(c.name, "Vertex");
        assert!(has_meta(&c.meta, "expose"));
        assert_eq!(c.extends.as_ref().unwrap().base_name(), Some("Module"));
        assert_eq!(c.fields[0].name, "x");
        assert_eq!(c.fields[0].set, PropAccess::Set);
        assert!(c.ctor.is_some());
        assert_eq!(c.methods.len(), 1);
    }

    #[test]
    fn parses_enum_and_typedef_and_native_meta() {
        let f = file(
            "package mucus.api;\n\
             @:include(\"../../src/Mucus.h\")\n\
             @:native enum EffectType { Unknown; Fog; }\n\
             @:native typedef Vertex = { x:Float, ?color:UInt32 };\n\
             @:native typedef Cursor = TexturedQuad;",
        );
        let Decl::Enum(e) = &f.decls[0] else { panic!("{:?}", f.decls[0]) };
        assert_eq!(e.name, "EffectType");
        assert_eq!(e.variants.len(), 2);
        assert!(has_meta(&e.meta, "native"));
        assert_eq!(e.meta.iter().find(|m| m.name == "include").unwrap().first_arg(), Some("../../src/Mucus.h"));

        let Decl::Typedef(t) = &f.decls[1] else { panic!() };
        let TypedefTarget::Struct(fields) = &t.target else { panic!() };
        assert_eq!(fields[1].name, "color");
        assert!(fields[1].optional);

        let Decl::Typedef(t2) = &f.decls[2] else { panic!() };
        assert!(matches!(t2.target, TypedefTarget::Alias(_)));
    }

    #[test]
    fn parses_interface_with_overloads() {
        let f = file(
            "package mucus.api;\n\
             @:native interface ISceneManager {\n\
               @:overload(function(s:Int):Void {})\n\
               public function SetScene(s:Dynamic):Void;\n\
             }",
        );
        let Decl::Interface(i) = &f.decls[0] else { panic!() };
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
        let Decl::Class(c) = &f.decls[0] else { panic!() };
        let body = c.methods[0].body.as_ref().unwrap();
        assert!(matches!(body[0], Stmt::Switch { .. }));
        if let Stmt::Var { init: Some(Expr::NullCoalesce(..)), .. } = &body[1] {
        } else {
            panic!("expected null-coalesce var, got {:?}", body[1]);
        }
        assert!(matches!(body[2], Stmt::Var { init: Some(Expr::Comprehension { .. }), .. }));
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
        let Decl::Class(c) = &f.decls[0] else { panic!() };
        let body = c.methods[0].body.as_ref().unwrap();
        assert!(matches!(&body[0], Stmt::Var { init: Some(Expr::ObjectLit(_)), .. }));
        assert!(matches!(&body[1], Stmt::Var { init: Some(Expr::MapLit(_)), .. }));
    }
}
