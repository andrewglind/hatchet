//! Class / constructor / method / free-function emitters for `BodyGen`. Split out of `source.rs`.

use super::*;

impl<'a> BodyGen<'a> {
    pub(super) fn class_impl(&mut self) -> String {
        let mut s = String::new();
        // The C++ name this class is emitted under — its `@:native` rename, if
        // any — used to qualify ctor/dtor/method definitions (`name::method`).
        let name = self
            .prog
            .resolve_type(std::slice::from_ref(&self.class.name), self.mi)
            .map(|t| t.cpp_name().to_string())
            .unwrap_or_else(|| self.class.name.clone());

        if let Some(ctor) = self.class.ctor.clone() {
            s.push_str(&self.ctor_impl(&name, &ctor));
            s.push('\n');
        }
        for m in self.class.methods.clone() {
            let Some(mname) = m.name.clone() else {
                continue;
            };
            if self.is_accessor_method(&mname) {
                continue;
            }
            if m.body.is_none() {
                continue;
            }
            // A custom accessor with an omitted return type returns the
            // property's type in Haxe — never void (mirrors the header side).
            let mut m = m;
            if m.ret.is_none() {
                if let Some(t) = crate::codegen::accessor_ret_type(self.class, &mname) {
                    m.ret = Some(t);
                }
            }
            s.push_str(&self.method_impl(&name, &m));
            s.push('\n');
        }
        s
    }

    pub(super) fn ctor_impl(&mut self, class_name: &str, ctor: &Function) -> String {
        // Base-from-member idiom when `super(...)` is not the first statement.
        self.current_fn = "new".to_string();
        self.set_abstract_this();
        if let Some(h) = crate::codegen::holder::analyze(self.prog, self.mi, &self.ns, self.class) {
            return self.holder_ctor_impl(class_name, ctor, &h);
        }

        self.push_scope();
        self.bind_params(&ctor.params);

        let params = self.header_params(&ctor.params);
        // Pull out a leading `super(...)` into the base initialiser list.
        let mut init = String::new();
        let body_stmts = ctor.body.clone().unwrap_or_default();
        let mut start = 0;
        if let Some(Stmt::Expr(Expr::Call(target, args), _)) = body_stmts.first() {
            if matches!(**target, Expr::Super) {
                if let Some(base) = &self.class.extends {
                    let base_name = self.prog.map_type_base(base, self.mi, &self.ns);
                    // Initialiser-list arguments are owned by the base, and run
                    // before the body — so `new` args stay inline, never hoisted.
                    self.new_args_escape = true;
                    let arglist = self.gen_args(args);
                    self.new_args_escape = false;
                    init = format!(" : {base_name}({arglist})");
                    start = 1;
                }
            }
        }
        // NULL-initialise owned pointer fields so `delete` (in the destructor and
        // before reassignment) is always safe.
        init = self.append_owned_null_inits(init);

        self.begin_fn_body(&body_stmts);
        let mut body = String::new();
        for st in &body_stmts[start..] {
            self.gen_stmt(st, 1, &mut body);
        }
        self.emit_owned_deletes(&mut body, 1);
        self.pop_scope();

        format!(
            "\t{inl}{class_name}::{class_name}({params}){init} {{\n{body}\t}}\n",
            inl = self.inline_kw()
        )
    }

    /// `"inline "` when emitting header-only definitions, else empty. A
    /// constructor/method defined out-of-line inside a header needs `inline` so
    /// including it from several translation units does not violate the ODR.
    pub(super) fn inline_kw(&self) -> &'static str {
        if self.inline_defs {
            "inline "
        } else {
            ""
        }
    }

    /// Emit the `XHolder` constructor + the class constructor (with its
    /// `: XHolder(...), Base(...)` initialiser list) for the base-from-member idiom.
    pub(super) fn holder_ctor_impl(
        &mut self,
        class_name: &str,
        ctor: &Function,
        h: &crate::codegen::holder::Holder,
    ) -> String {
        use crate::codegen::holder::SuperArg;
        let body_stmts = ctor.body.clone().unwrap_or_default();
        let params = self.header_params(&ctor.params);

        // 1. XHolder constructor: pre-super statements (lifted locals become member
        //    assignments) followed by the hoisted super arguments.
        self.push_scope();
        self.bind_params(&ctor.params);
        let mut hbody = String::new();
        for st in &body_stmts[..h.super_idx] {
            self.gen_holder_stmt(st, &h.lifted, 1, &mut hbody);
        }
        for (mname, expr) in &h.hoisted {
            self.prelude_ind = 1;
            let (code, _) = self.gen_expr(expr);
            self.flush(&mut hbody);
            let _ = writeln!(hbody, "\tthis->{mname} = {code};");
        }
        self.pop_scope();
        let holder_ctor = format!(
            "\t{inl}{0}::{0}({params}) {{\n{hbody}\t}}\n",
            h.name,
            inl = self.inline_kw()
        );

        // 2. Class constructor: initialiser list calls XHolder with all params and
        //    the base with the mapped super arguments; body is the post-super code.
        self.push_scope();
        self.bind_params(&ctor.params);
        let base_name = self
            .class
            .extends
            .as_ref()
            .map(|b| self.prog.map_type_base(b, self.mi, &self.ns))
            .unwrap_or_default();
        let holder_args: Vec<String> = ctor.params.iter().map(|p| p.name.clone()).collect();
        self.new_args_escape = true;
        let super_args: Vec<String> = h
            .super_args
            .iter()
            .map(|a| match a {
                SuperArg::Member(n) => n.clone(),
                SuperArg::PassThrough(e) => self.gen_expr(e).0,
            })
            .collect();
        self.new_args_escape = false;
        let init = format!(
            " : {}({}), {base_name}({})",
            h.name,
            holder_args.join(", "),
            super_args.join(", ")
        );
        let init = self.append_owned_null_inits(init);
        self.begin_fn_body(&body_stmts);
        let mut body = String::new();
        for st in &body_stmts[h.super_idx + 1..] {
            self.gen_stmt(st, 1, &mut body);
        }
        self.emit_owned_deletes(&mut body, 1);
        self.pop_scope();
        let class_ctor = format!(
            "\t{inl}{class_name}::{class_name}({params}){init} {{\n{body}\t}}\n",
            inl = self.inline_kw()
        );

        format!("{holder_ctor}\n{class_ctor}")
    }

    /// Append `field(NULL)` initialisers for owned pointer fields to a ctor's
    /// initialiser list (in field declaration order).
    pub(super) fn append_owned_null_inits(&self, init: String) -> String {
        let nulls: Vec<String> = self
            .class
            .fields
            .iter()
            .filter(|f| self.owned_fields.contains(&f.name))
            .map(|f| format!("{}(NULL)", f.name))
            .collect();
        if nulls.is_empty() {
            init
        } else if init.is_empty() {
            format!(" : {}", nulls.join(", "))
        } else {
            format!("{init}, {}", nulls.join(", "))
        }
    }

    /// Generate a pre-super statement for an `XHolder` constructor: a `var` whose
    /// name is lifted becomes a member assignment (`this->name = ...`); everything
    /// else is an ordinary local statement.
    pub(super) fn gen_holder_stmt(
        &mut self,
        st: &Stmt,
        lifted: &[String],
        ind: usize,
        out: &mut String,
    ) {
        if let Stmt::Var {
            name,
            ty,
            init: Some(init),
            ..
        } = st
        {
            if lifted.iter().any(|n| n == name) {
                let t = "\t".repeat(ind);
                self.prelude_ind = ind;
                let (code, _) = self.gen_expr(init);
                self.flush(out);
                let _ = writeln!(out, "{t}this->{name} = {code};");
                let decl_ty = ty.as_ref().map(|t| self.ty_of(t)).unwrap_or_default();
                self.define_local(name, decl_ty);
                return;
            }
        }
        self.gen_stmt(st, ind, out);
    }

    /// For an `abstract` newtype's class, record the underlying value's C++ type
    /// so `this` (the underlying) lowers to the synthetic `this->__this` member
    /// inside method/ctor bodies. A no-op for an ordinary class.
    pub(super) fn set_abstract_this(&mut self) {
        self.abstract_this = self
            .class
            .abstract_underlying
            .clone()
            .map(|u| self.ty_of(&u));
    }

    pub(super) fn method_impl(&mut self, class_name: &str, m: &Function) -> String {
        self.push_scope();
        self.set_abstract_this();
        self.bind_params(&m.params);
        self.current_ret = m.ret.as_ref().map(|t| self.ty_of(t)).unwrap_or_default();
        let ret = self.return_type(m);
        let name = m.name.clone().unwrap();
        self.current_fn = name.clone();
        let params = self.header_params(&m.params);
        let stmts = m.body.as_ref().unwrap();
        self.begin_fn_body(stmts);
        let mut body = String::new();
        for st in stmts {
            self.gen_stmt(st, 1, &mut body);
        }
        self.emit_body_close_deletes(stmts, &mut body, 1);
        self.pop_scope();
        format!(
            "\t{inl}{ret} {class_name}::{name}({params}) {{\n{body}\t}}\n",
            inl = self.inline_kw()
        )
    }

    /// Signature (`ret name(params)`) for a top-level free function; `with_defaults`
    /// keeps any ` = default` suffixes (for the declaration only).
    pub(super) fn free_fn_signature(
        &mut self,
        g: &GlobalVar,
        with_defaults: bool,
    ) -> Option<String> {
        let (params, ret, body) = lambda_parts(g)?;
        let params = effective_lambda_params(params, g.ty.as_ref());
        self.push_scope();
        self.bind_params(&params);
        let ret_ty = self.resolve_lambda_ret(ret, body, g.ty.as_ref());
        let ret_cpp = self.decl_spelling(&ret_ty);
        let plist = if with_defaults {
            params
                .iter()
                .map(|p| crate::codegen::param_decl(self.prog, self.mi, &self.ns, p))
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            self.header_params(&params)
        };
        self.pop_scope();
        Some(format!("{ret_cpp} {}({plist})", g.name))
    }

    /// Full definition of a top-level free function (`static` when file-local).
    pub(super) fn free_fn_def(&mut self, g: &GlobalVar) -> String {
        let Some((params, ret, body)) = lambda_parts(g) else {
            return String::new();
        };
        let params = effective_lambda_params(params, g.ty.as_ref());
        self.current_fn = g.name.clone();
        self.push_scope();
        self.bind_params(&params);
        let ret_ty = self.resolve_lambda_ret(ret, body, g.ty.as_ref());
        self.current_ret = ret_ty.clone();
        let ret_cpp = self.decl_spelling(&ret_ty);
        let plist = self.header_params(&params);
        // See `plain_fn_def`: a header-only amalgamation has no `.cpp`, so the
        // definition is emitted `inline` in the header; otherwise a file-local
        // (`private`) function is `static`.
        let prefix = if self.inline_defs {
            "inline "
        } else if g.access == Access::Private {
            "static "
        } else {
            ""
        };

        let mut body_buf = String::new();
        match body {
            LambdaBody::Expr(e) => {
                self.prelude_ind = 1;
                let (c, cty) = self.gen_expr(e);
                self.flush(&mut body_buf);
                let r = if ret_ty.is_ptr && !cty.is_ptr {
                    format!("new {}({c})", ret_ty.base)
                } else {
                    c
                };
                let _ = writeln!(body_buf, "\treturn {r};");
            }
            LambdaBody::Block(stmts) => {
                self.begin_fn_body(stmts);
                for st in stmts {
                    self.gen_stmt(st, 1, &mut body_buf);
                }
                self.emit_body_close_deletes(stmts, &mut body_buf, 1);
            }
        }
        self.pop_scope();
        format!(
            "\t{prefix}{ret_cpp} {}({plist}) {{\n{body_buf}\t}}\n",
            g.name
        )
    }

    /// Full definition of an `extern inline` function as an `extern "C"` export at
    /// **global scope**: `<P>_EXPORT <ret> <P>_CALL name(params) { body }`. The
    /// generator must already have an empty namespace so referenced types are fully
    /// qualified. The signature and braces sit at column 0; the body is indented one
    /// level (the function is not inside a namespace).
    pub(super) fn extern_fn_def(&mut self, f: &Function) -> String {
        let Some(name) = f.name.clone() else {
            return String::new();
        };
        let prefix = self.prog.export_macro.clone();
        self.current_fn = name.clone();
        self.push_scope();
        self.bind_params(&f.params);
        let ret_ty = match &f.ret {
            Some(t) => self.ty_of(t),
            None => Ty {
                base: "void".to_string(),
                ..Default::default()
            },
        };
        self.current_ret = ret_ty.clone();
        let ret_cpp = self.decl_spelling(&ret_ty);
        let plist = self.header_params(&f.params);

        let mut body_buf = String::new();
        if let Some(stmts) = &f.body {
            self.begin_fn_body(stmts);
            for st in stmts {
                self.gen_stmt(st, 1, &mut body_buf);
            }
            self.emit_body_close_deletes(stmts, &mut body_buf, 1);
        }
        self.pop_scope();
        format!("{prefix}_EXPORT {ret_cpp} {prefix}_CALL {name}({plist}) {{\n{body_buf}}}\n")
    }

    /// Signature (`ret name(params)`) for a plain module-level free function;
    /// `with_defaults` keeps any ` = default` suffixes (the header declaration).
    pub(super) fn plain_fn_signature(
        &mut self,
        f: &Function,
        with_defaults: bool,
    ) -> Option<String> {
        let name = f.name.clone()?;
        self.push_scope();
        self.bind_params(&f.params);
        let ret_ty = match &f.ret {
            Some(t) => self.ty_of(t),
            None => Ty {
                base: "void".to_string(),
                ..Default::default()
            },
        };
        let ret_cpp = self.decl_spelling(&ret_ty);
        let plist = if with_defaults {
            f.params
                .iter()
                .map(|p| crate::codegen::param_decl(self.prog, self.mi, &self.ns, p))
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            self.header_params(&f.params)
        };
        self.pop_scope();
        Some(format!("{ret_cpp} {name}({plist})"))
    }

    /// Full definition of a plain module-level `function name(...) {...}` as a
    /// namespace free function (`static` when file-local). Unlike the lambda form,
    /// this has a real signature and statement body.
    pub(super) fn plain_fn_def(&mut self, f: &Function) -> String {
        let Some(name) = f.name.clone() else {
            return String::new();
        };
        self.current_fn = name.clone();
        self.push_scope();
        self.bind_params(&f.params);
        let ret_ty = match &f.ret {
            Some(t) => self.ty_of(t),
            None => Ty {
                base: "void".to_string(),
                ..Default::default()
            },
        };
        self.current_ret = ret_ty.clone();
        let ret_cpp = self.decl_spelling(&ret_ty);
        let plist = self.header_params(&f.params);
        // In a header-only amalgamation there is no `.cpp`, so the definition lives
        // in the header and must be `inline` (ODR-safe across the translation units
        // that include it). Otherwise a file-local (`private`) function is `static`.
        let prefix = if self.inline_defs {
            "inline "
        } else if f.access == Access::Private {
            "static "
        } else {
            ""
        };

        let mut body_buf = String::new();
        if let Some(stmts) = &f.body {
            self.begin_fn_body(stmts);
            for st in stmts {
                self.gen_stmt(st, 1, &mut body_buf);
            }
            self.emit_body_close_deletes(stmts, &mut body_buf, 1);
        }
        self.pop_scope();
        format!("\t{prefix}{ret_cpp} {name}({plist}) {{\n{body_buf}\t}}\n")
    }

    /// Resolve a lambda's C++ return type from, in priority order: (1) the lambda's
    /// own explicit `:T` return annotation, (2) the **function-type annotation on
    /// the binding** — `Square:(Int, Int) -> Int = (a, b) -> a * b` — whose `-> R`
    /// gives the return type, (3) a `cast(expr, T)` arrow body. A developer hints
    /// the return type via (2) or (3); absent any hint it falls back
    /// to `float` (the common case for numeric helpers). `decl_ty` is the binding's
    /// declared type, if any.
    pub(super) fn resolve_lambda_ret(
        &self,
        ret: &Option<Type>,
        body: &LambdaBody,
        decl_ty: Option<&Type>,
    ) -> Ty {
        if let Some(t) = ret {
            return self.ty_of(t);
        }
        if let Some(Type::Func { ret, .. }) = decl_ty {
            return self.ty_of(ret);
        }
        if let LambdaBody::Expr(Expr::Cast { ty: Some(t), .. }) = body {
            return self.ty_of(t);
        }
        float_ty()
    }

    pub(super) fn return_type(&self, m: &Function) -> String {
        match &m.ret {
            Some(t) => self.prog.map_type_use(t, self.mi, &self.ns),
            None => "void".to_string(),
        }
    }

    /// Parameters as they appear in the out-of-line definition (same spelling as
    /// the header, but without default values).
    pub(super) fn header_params(&self, params: &[Param]) -> String {
        params
            .iter()
            .map(|p| {
                let decl = crate::codegen::param_decl(self.prog, self.mi, &self.ns, p);
                // strip any " = default"
                match decl.split_once(" = ") {
                    Some((head, _)) => head.to_string(),
                    None => decl,
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub(super) fn bind_params(&mut self, params: &[Param]) {
        self.container_params.clear();
        self.value_struct_params.clear();
        self.optional_string_params.clear();
        for p in params {
            let ty = match &p.ty {
                Some(t) => {
                    let mut ty = self.ty_of(t);
                    // optional value-struct parameters are pointers
                    if p.optional && !ty.is_ptr {
                        if let Some(info) = &ty.info {
                            if matches!(info.kind, TypeKind::StructTypedef | TypeKind::AliasTypedef)
                            {
                                ty.is_ptr = true;
                            }
                        }
                    }
                    ty
                }
                None => Ty::default(),
            };
            let is_container = !ty.is_ptr && is_container_ty(&ty);
            // `@sink` only means something for a heap pointer (a class, or a
            // `Null<T>` value lowered to a pointer) — the caller transfers
            // ownership of the pointed-to object. On a by-value parameter
            // (primitive, String, value-struct, container) there is nothing to
            // consume, so the tag is a no-op; flag it so it can't silently mislead.
            if has_meta(&p.meta, "sink") && !ty.is_ptr {
                self.warn(format!(
                    "`@sink` on parameter `{}` has no effect — it is passed by value, not as an owned pointer",
                    p.name
                ));
            }
            let is_optional_string = p.optional && !ty.is_ptr && ty.base == "std::string";
            // A non-optional value-struct parameter is passed `const T&` (an optional
            // one becomes a mutable `T*`), so it cannot be assigned through.
            let is_value_struct_param = !p.optional
                && p.ty
                    .as_ref()
                    .is_some_and(|t| crate::codegen::is_value_struct(self.prog, self.mi, t));
            self.define_local(&p.name, ty);
            // after define_local, which clears any earlier shadow bookkeeping
            if is_container {
                self.container_params.insert(p.name.clone());
            }
            if is_value_struct_param {
                self.value_struct_params.insert(p.name.clone());
            }
            if is_optional_string {
                self.optional_string_params.insert(p.name.clone());
            }
        }
    }
}
