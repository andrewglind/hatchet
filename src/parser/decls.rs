//! Declaration parsing: package, imports, classes, interfaces, enums, typedefs, abstracts, fields, functions. Part of the `Parser` impl, split out of `parser.rs`.

use crate::ast::*;
use crate::lexer::*;

use super::{Parser, PResult};

impl<'a> Parser<'a> {
    // ---- file -----------------------------------------------------------

    pub(super) fn parse_file(&mut self) -> PResult<File> {
        let mut file = File {
            package: Vec::new(),
            imports: Vec::new(),
            usings: Vec::new(),
            decls: Vec::new(),
            meta: Vec::new(),
        };

        if self.eat_kw(Kw::Package) {
            // `package;` (empty package) is valid Haxe and means the root
            // package — identical to omitting the declaration. Only parse a
            // dotted path when a name actually follows the keyword.
            if !self.at_sym(Sym::Semi) {
                file.package = self.parse_dotted()?;
            }
            self.expect_sym(Sym::Semi)?;
        }

        loop {
            // imports / usings may be interleaved with metadata-free positions
            if self.at_kw(Kw::Import) {
                file.imports.push(self.parse_import()?);
                continue;
            }
            if self.at_kw(Kw::Using) {
                let line = self.line();
                self.bump();
                let path = self.parse_dotted()?;
                self.expect_sym(Sym::Semi)?;
                file.usings.push(Using { path, line });
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

    pub(super) fn parse_dotted(&mut self) -> PResult<Vec<String>> {
        let mut parts = vec![self.expect_ident()?];
        while self.eat_sym(Sym::Dot) {
            parts.push(self.expect_ident()?);
        }
        Ok(parts)
    }

    pub(super) fn parse_import(&mut self) -> PResult<Import> {
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
        Ok(Import {
            path,
            wildcard,
            alias,
        })
    }

    pub(super) fn expect_sym_kw(&mut self, k: Kw) -> PResult<()> {
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
    pub(super) fn parse_decl_body(&mut self, meta: Vec<Meta>) -> PResult<Decl> {
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

        let line = self.line();
        match self.peek().clone() {
            TokKind::Kw(Kw::Class) => Ok(Decl::Class(Box::new(self.parse_class(
                meta,
                modifiers.is_extern,
                is_final_class,
                is_abstract_class,
            )?))),
            TokKind::Kw(Kw::Interface) => Ok(Decl::Interface(
                self.parse_interface(meta, modifiers.is_extern)?,
            )),
            // `enum abstract X(T) { … }` — Haxe's typed-constant idiom. An integral
            // backing lowers to a plain C++ `enum` (with explicit member values); a
            // `String`/`Float` backing lowers to a namespace of `static const`s.
            TokKind::Kw(Kw::Enum) if matches!(self.peek2(), TokKind::Kw(Kw::Abstract)) => {
                self.bump(); // enum
                self.bump(); // abstract
                Ok(Decl::Enum(
                    self.parse_enum_abstract(meta, modifiers.is_extern)?,
                ))
            }
            TokKind::Kw(Kw::Enum) => self.parse_enum(meta, modifiers.is_extern),
            // A bare `abstract X(T) { … }` newtype (the `abstract class` form is
            // handled by the modifier loop above). Lowered to a value class.
            TokKind::Kw(Kw::Abstract) => {
                self.bump(); // abstract
                self.parse_abstract(meta, line)
            }
            TokKind::Kw(Kw::Typedef) => self.parse_typedef(meta),
            TokKind::Kw(Kw::Function) => {
                // top-level function, e.g. `extern inline function f(...) {}`
                let func = self.parse_function(meta, access, modifiers)?;
                Ok(Decl::Function(func))
            }
            TokKind::Kw(Kw::Var) | TokKind::Kw(Kw::Final) => {
                let is_final_kw = self.at_kw(Kw::Final);
                self.bump();
                let g = self.parse_global_var(meta, access, is_final_kw, modifiers.is_extern)?;
                Ok(Decl::Global(g))
            }
            other => Err(self.err(&format!("unexpected top-level token {:?}", other))),
        }
    }

    pub(super) fn parse_type_params(&mut self) -> PResult<Vec<String>> {
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

    pub(super) fn parse_class(
        &mut self,
        meta: Vec<Meta>,
        is_extern: bool,
        is_final: bool,
        is_abstract: bool,
    ) -> PResult<Class> {
        let line = self.line();
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
            line,
            type_params,
            extends,
            implements,
            is_extern,
            is_final,
            is_abstract,
            abstract_underlying: None,
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

    /// Parse `abstract Name(Underlying) [from/to …] { members }` — a Haxe
    /// newtype. Lowered to a value `Class` that wraps the underlying in a
    /// synthetic `__this` field; the abstract's methods become the class's
    /// methods, and `this` inside them denotes the underlying value. The leading
    /// `abstract` keyword is already consumed.
    pub(super) fn parse_abstract(&mut self, meta: Vec<Meta>, line: usize) -> PResult<Decl> {
        let name = self.expect_ident()?;
        self.parse_type_params()?; // generic abstracts not supported; discard
                                   // The underlying type `(T)`.
        self.expect_sym(Sym::LParen)?;
        let underlying = self.parse_type()?;
        self.expect_sym(Sym::RParen)?;
        // Skip any `from`/`to`/`to`-cast header clauses up to the body — the
        // implicit-cast-to-underlying forms are not modelled; `@:to`/`@:from`
        // methods inside the body are.
        while !self.at_sym(Sym::LBrace) && !self.at_eof() {
            self.bump();
        }
        self.expect_sym(Sym::LBrace)?;

        // The synthetic field that holds the underlying value. The abstract's
        // `this` rewrites to it during codegen.
        let this_field = Field {
            name: "__this".to_string(),
            ty: Some(underlying.clone()),
            init: None,
            access: Access::Private,
            is_static: false,
            is_final: false,
            get: PropAccess::Default,
            set: PropAccess::Default,
            meta: Vec::new(),
        };
        let mut class = Class {
            name,
            line,
            type_params: vec![],
            extends: None,
            implements: vec![],
            is_extern: false,
            is_final: false,
            is_abstract: false,
            abstract_underlying: Some(underlying),
            meta,
            fields: vec![this_field],
            methods: Vec::new(),
            ctor: None,
        };
        while !self.at_sym(Sym::RBrace) && !self.at_eof() {
            if self.eat_sym(Sym::Semi) {
                continue;
            }
            self.parse_member_into(&mut class)?;
        }
        self.expect_sym(Sym::RBrace)?;
        Ok(Decl::Class(Box::new(class)))
    }

    pub(super) fn parse_interface(&mut self, meta: Vec<Meta>, is_extern: bool) -> PResult<Interface> {
        let line = self.line();
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
            line,
            is_extern,
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
                line: 0,
                type_params: vec![],
                extends: None,
                implements: vec![],
                is_extern: false,
                is_final: false,
                is_abstract: false,
                abstract_underlying: None,
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

    pub(super) fn parse_enum(&mut self, meta: Vec<Meta>, is_extern: bool) -> PResult<Decl> {
        let line = self.line();
        self.expect_sym_kw(Kw::Enum)?;
        let name = self.expect_ident()?;
        let type_params = self.parse_type_params()?;
        self.expect_sym(Sym::LBrace)?;
        let mut variants = Vec::new();
        while !self.at_sym(Sym::RBrace) && !self.at_eof() {
            let vname = self.expect_ident()?;
            let mut params = Vec::new();
            if self.at_sym(Sym::LParen) {
                params = self.parse_params()?;
            }
            self.eat_sym(Sym::Semi);
            variants.push(EnumVariant {
                name: vname,
                params,
                value: None,
            });
        }
        self.expect_sym(Sym::RBrace)?;
        // Generic enums have no C++98 template lowering (and their variants almost
        // always carry `T` payloads, which need the tagged-union lowering too). The
        // body is consumed and discarded so the validation pass reports a clean
        // `Unsupported` instead of a parse error or unresolved-`T` noise.
        if !type_params.is_empty() {
            return Ok(Decl::Unsupported {
                feature: format!("the generic enum `{name}<{}>`", type_params.join(", ")),
                line,
            });
        }
        Ok(Decl::Enum(Enum {
            name,
            is_extern,
            meta,
            variants,
            underlying: None,
        }))
    }

    /// Parse `enum abstract X(T) { var A [= expr]; ... }`. The leading
    /// `enum abstract` keywords are already consumed. The returned `Enum` carries
    /// the underlying type `T` and each member's explicit value; codegen lowers an
    /// integral backing to a C++ `enum` and a `String`/`Float` backing to a
    /// namespace of typed `static const` constants.
    pub(super) fn parse_enum_abstract(&mut self, meta: Vec<Meta>, is_extern: bool) -> PResult<Enum> {
        let name = self.expect_ident()?;
        // The underlying type `(T)`.
        self.expect_sym(Sym::LParen)?;
        let underlying = self.parse_type()?;
        self.expect_sym(Sym::RParen)?;
        // Skip any `from`/`to` cast clauses up to the body.
        while !self.at_sym(Sym::LBrace) && !self.at_eof() {
            self.bump();
        }
        self.expect_sym(Sym::LBrace)?;
        let mut variants = Vec::new();
        while !self.at_sym(Sym::RBrace) && !self.at_eof() {
            // Member-level metadata/modifiers (`public`, `inline`, `final`, …) carry
            // no meaning for the C++ enum — skip them.
            while matches!(self.peek(), TokKind::Meta(_)) {
                self.bump();
            }
            while matches!(
                self.peek(),
                TokKind::Kw(Kw::Public)
                    | TokKind::Kw(Kw::Private)
                    | TokKind::Kw(Kw::Inline)
                    | TokKind::Kw(Kw::Static)
                    | TokKind::Kw(Kw::Final)
            ) {
                self.bump();
            }
            // A method inside an `enum abstract` (`function …`) is not transpiled —
            // skip its signature and body and move on.
            if self.at_kw(Kw::Function) {
                while !self.at_sym(Sym::LBrace) && !self.at_eof() {
                    self.bump();
                }
                self.skip_braced_body()?;
                self.eat_sym(Sym::Semi);
                continue;
            }
            if !self.eat_kw(Kw::Var) {
                // Anything unexpected: stop trying to extract members cleanly.
                break;
            }
            let vname = self.expect_ident()?;
            // Optional `: Type` annotation on the member (ignored).
            if self.eat_sym(Sym::Colon) {
                let _ = self.parse_type()?;
            }
            let value = if self.eat_sym(Sym::Assign) {
                Some(self.parse_expr()?)
            } else {
                None
            };
            self.eat_sym(Sym::Semi);
            variants.push(EnumVariant {
                name: vname,
                params: Vec::new(),
                value,
            });
        }
        self.expect_sym(Sym::RBrace)?;
        Ok(Enum {
            name,
            is_extern,
            meta,
            variants,
            underlying: Some(underlying),
        })
    }

    pub(super) fn parse_typedef(&mut self, meta: Vec<Meta>) -> PResult<Decl> {
        let line = self.line();
        self.expect_sym_kw(Kw::Typedef)?;
        let name = self.expect_ident()?;
        let type_params = self.parse_type_params()?;
        self.expect_sym(Sym::Assign)?;
        let target = if self.at_sym(Sym::LBrace) {
            TypedefTarget::Struct(self.parse_struct_fields()?)
        } else {
            TypedefTarget::Alias(self.parse_type()?)
        };
        self.eat_sym(Sym::Semi);
        // Generic typedefs have no C++98 template lowering. The body is parsed (to
        // consume it) and discarded, so the validation pass reports the typedef as
        // `Unsupported` instead of flooding it with unresolved-`T` errors.
        if !type_params.is_empty() {
            return Ok(Decl::Unsupported {
                feature: format!("the generic typedef `{name}<{}>`", type_params.join(", ")),
                line,
            });
        }
        Ok(Decl::Typedef(Typedef { name, meta, target }))
    }

    pub(super) fn parse_global_var(
        &mut self,
        meta: Vec<Meta>,
        access: Access,
        is_final: bool,
        is_extern: bool,
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
            is_extern,
            access,
            meta,
        })
    }

    // ---- members --------------------------------------------------------

    pub(super) fn parse_member_into(&mut self, class: &mut Class) -> PResult<()> {
        let meta = self.parse_meta_list()?;

        let mut access = Access::Default;
        let mut is_static = false;
        let mut modifiers = FnModifiers::default();
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

    pub(super) fn parse_field(
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

    pub(super) fn parse_prop_access(&mut self) -> PResult<PropAccess> {
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

    pub(super) fn parse_function(
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
        // Generic methods have no template lowering; the params are kept so the
        // validation pass can flag the method as `Unsupported` (and so `T` uses in
        // its body are not double-reported as unresolved types).
        let type_params = self.parse_type_params()?;
        let params = self.parse_params()?;
        let ret = if self.eat_sym(Sym::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let body = if self.at_sym(Sym::LBrace) {
            if modifiers.is_macro {
                // A `macro` function's body is Haxe macro-reification syntax
                // (`macro`, `$x`, ...) that Hatchet does not transpile. Skip it
                // verbatim so the reification doesn't trip up the expression
                // parser; sema reports the function itself as unsupported.
                self.skip_braced_block()?;
                None
            } else {
                Some(self.parse_block()?)
            }
        } else {
            self.eat_sym(Sym::Semi);
            None
        };
        Ok(Function {
            name,
            type_params,
            params,
            ret,
            body,
            access,
            modifiers,
            meta,
        })
    }

    pub(super) fn parse_params(&mut self) -> PResult<Vec<Param>> {
        self.expect_sym(Sym::LParen)?;
        let mut params = Vec::new();
        if self.eat_sym(Sym::RParen) {
            return Ok(params);
        }
        loop {
            // Leading parameter metadata (`@sink val:T`).
            let meta = self.parse_meta_list()?;
            // Haxe 4.2 rest parameter (`...vals:Int`): parsed and marked so the
            // validation pass reports it cleanly instead of a parse error.
            let rest = self.eat_sym(Sym::DotDotDot);
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
                rest,
                meta,
            });
            if self.eat_sym(Sym::Comma) {
                continue;
            }
            break;
        }
        self.expect_sym(Sym::RParen)?;
        Ok(params)
    }

}

fn set_access(current: Access, new: Access) -> Access {
    // explicit public/private win over the default
    match current {
        Access::Default => new,
        other => other,
    }
}
