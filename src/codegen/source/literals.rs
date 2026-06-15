//! Object / array / map literal expansion and compile-time constant rendering for `BodyGen`. Split out of `source.rs`.

use super::*;

impl<'a> BodyGen<'a> {
    /// Build an anonymous struct into a hoisted temporary; return the temp name.
    pub(super) fn hoist_object(&mut self, fields: &[(String, Expr)], target: Ty) -> String {
        let tmp = self.fresh("anon");
        let mut buf = String::new();
        self.expand_object_into_local(&tmp, &target, fields, self.prelude_ind, &mut buf);
        self.prelude.push_str(&buf);
        tmp
    }

    // ---- object-literal expansion --------------------------------------

    pub(super) fn expand_object_into_local(
        &mut self,
        name: &str,
        target: &Ty,
        fields: &[(String, Expr)],
        ind: usize,
        out: &mut String,
    ) {
        // No nominal type to name → emit a local anonymous struct instead.
        if target.base.is_empty() {
            self.expand_anon_struct_local(name, fields, ind, out);
            return;
        }
        let t = "\t".repeat(ind);
        let spell = target.base.clone();
        let _ = writeln!(out, "{t}{spell} {name};");
        let info = target.info.clone();
        for (key, value) in fields {
            let member = info.as_ref().and_then(|i| self.member_field_ty(i, key));
            match value {
                // Nested struct literal → expand into a temp of the member type.
                Expr::ObjectLit(f) => {
                    let mt = member.clone().unwrap_or_default();
                    let tmp = self.fresh("f");
                    self.expand_object_into_local(&tmp, &mt, f, ind, out);
                    let _ = writeln!(out, "{t}{name}.{key} = {tmp};");
                }
                // Nested array literal → expand into a temp vector of the member
                // type (empty arrays included, so the element type comes from the
                // field rather than defaulting to `int`).
                Expr::ArrayLit(els) => {
                    let mt = member.clone().unwrap_or_default();
                    let elem = self.elem_member_ty(&mt);
                    let tmp = self.fresh("f");
                    self.expand_array_into_local(&tmp, &mt, &elem, els, ind, out);
                    let _ = writeln!(out, "{t}{name}.{key} = {tmp};");
                }
                // Nested map literal → expand into a temp map of the member type.
                Expr::MapLit(pairs) if !pairs.is_empty() => {
                    let mt = member.clone().unwrap_or_default();
                    let (_, vty) = self.map_kv_ty(&mt);
                    let tmp = self.fresh("f");
                    self.expand_map_into_local(&tmp, &mt, &vty, pairs, ind, out);
                    let _ = writeln!(out, "{t}{name}.{key} = {tmp};");
                }
                _ => {
                    let field_ptr = member.as_ref().map(|m| m.is_ptr).unwrap_or(false);
                    let (vcode, vty) = self.gen_expr(value);
                    self.flush(out);
                    if !field_ptr && vty.is_ptr {
                        // value is a pointer, target field is a value: null-guarded deref
                        let _ = writeln!(out, "{t}if ({vcode} != NULL) {{ {name}.{key} = *{vcode}; }}");
                    } else {
                        let _ = writeln!(out, "{t}{name}.{key} = {vcode};");
                    }
                }
            }
        }
    }

    /// Element `Ty` of a member whose declared `Ty` base is `std::vector<...>`,
    /// preserving the struct `info` when the element is a user type.
    pub(super) fn elem_member_ty(&self, vec: &Ty) -> Ty {
        let e = self.element_ty(vec);
        if e.info.is_none() && !e.base.is_empty() {
            // Recover info from the element's bare name for struct expansion.
            let bare = e.base.rsplit("::").next().unwrap_or(&e.base).to_string();
            if let Some(info) = self.prog.resolve_type(&[bare], self.mi).cloned() {
                return Ty { info: Some(info), ..e };
            }
        }
        e
    }

    /// Emit a file-scoped (`private`) struct/array/map `final` as a `static const`
    /// definition (placed inside the namespace, at one tab). A struct constant is
    /// aggregate-initialised; an `Array<T>` (which C++98 cannot brace-initialise) is
    /// built by a one-off helper assigned to a `const` vector object so the symbol
    /// stays a `std::vector`. Returns `None` if it cannot be lowered.
    pub(super) fn file_scope_const(&mut self, g: &GlobalVar) -> Option<String> {
        let init = g.init.as_ref()?;
        // An untyped scalar `final` infers its C++ type from the initialiser
        // (`final N = 1;` → `int`); a typed final uses its annotation.
        let decl_ty = match g.ty.as_ref() {
            Some(t) => self.ty_of(t),
            None => {
                let ty = self.gen_expr(init).1;
                self.prelude.clear();
                ty
            }
        };
        // `Array<T> = [..]` → builder helper + `const` vector object.
        if decl_ty.base.starts_with("std::vector") {
            if let Expr::ArrayLit(elems) = init {
                return Some(self.render_const_vector(&g.name, &decl_ty, elems));
            }
        }
        // `Struct = { .. }` → aggregate initialiser.
        if let Expr::ObjectLit(fields) = init {
            let agg = self.render_const_aggregate(&decl_ty, fields)?;
            return Some(format!("\tstatic const {} {} = {agg};\n", decl_ty.base, g.name));
        }
        // `Struct = OTHER` (alias) / any other scalar-ish init → copy-initialise.
        let v = crate::codegen::render_scalar_literal(init)?;
        Some(format!("\tstatic const {} {} = {v};\n", decl_ty.base, g.name))
    }

    /// Render a struct constant's value as a C++98 aggregate initialiser
    /// (`{ v0, v1 }`) in the struct's declared field order. `None` if the type is
    /// not a struct typedef or a field value is missing/unsupported.
    pub(super) fn render_const_aggregate(&self, ty: &Ty, fields: &[(String, Expr)]) -> Option<String> {
        let info = ty.info.as_ref()?;
        let Decl::Typedef(td) = self.prog.type_decl(info)? else { return None };
        let TypedefTarget::Struct(sfields) = &td.target else { return None };
        let mut parts = Vec::new();
        for sf in sfields {
            let val = fields.iter().find(|(k, _)| k == &sf.name).map(|(_, v)| v)?;
            // Nested struct values aren't supported here; bail (→ caller
            // returns None) rather than emit something wrong.
            if matches!(val, Expr::ObjectLit(_)) {
                return None;
            }
            parts.push(crate::codegen::render_scalar_literal(val)?);
        }
        Some(format!("{{ {} }}", parts.join(", ")))
    }

    /// Render a file-scoped `final Array<T> = [..]` as a builder helper plus a
    /// `const` vector object initialised from it (C++98 cannot brace-initialise a
    /// `std::vector`). Keeps the symbol a `std::vector`, so call sites are unchanged.
    pub(super) fn render_const_vector(&mut self, name: &str, vec_ty: &Ty, elems: &[Expr]) -> String {
        let builder = format!("_init_{name}");
        let spell = self.decl_spelling(vec_ty);
        let elem = self.elem_member_ty(vec_ty);
        let mut body = String::new();
        self.expand_array_into_local("v", vec_ty, &elem, elems, 2, &mut body);
        let mut s = String::new();
        let _ = writeln!(s, "\tstatic {spell} {builder}() {{");
        s.push_str(&body);
        let _ = writeln!(s, "\t\treturn v;");
        let _ = writeln!(s, "\t}}");
        let _ = writeln!(s, "\tstatic const {spell} {name} = {builder}();");
        s
    }

    /// Declare a `std::vector` local and `push_back` each element. Object-literal
    /// elements are expanded into a temporary of the element type first.
    pub(super) fn expand_array_into_local(
        &mut self,
        name: &str,
        vec_ty: &Ty,
        elem: &Ty,
        elems: &[Expr],
        ind: usize,
        out: &mut String,
    ) {
        let t = "\t".repeat(ind);
        // Fall back to inferring the container/element type from the elements when
        // the declared type is unknown (e.g. an untyped or @:native struct field).
        let (vec_ty, elem) = if vec_ty.base.is_empty() {
            let inferred = self.infer_array(elems);
            let e = self.elem_member_ty(&inferred);
            (inferred, e)
        } else {
            (vec_ty.clone(), elem.clone())
        };
        let _ = writeln!(out, "{t}{} {name};", self.decl_spelling(&vec_ty));
        for el in elems {
            if let Expr::ObjectLit(fields) = el {
                let tmp = self.fresh("elem");
                self.expand_object_into_local(&tmp, &elem, fields, ind, out);
                let _ = writeln!(out, "{t}{name}.push_back({tmp});");
            } else {
                let (c, _) = self.gen_expr(el);
                self.flush(out);
                let _ = writeln!(out, "{t}{name}.push_back({c});");
            }
        }
    }

    /// Declare a `std::map` local and assign each `key => value` pair.
    pub(super) fn expand_map_into_local(
        &mut self,
        name: &str,
        map_ty: &Ty,
        val: &Ty,
        pairs: &[(Expr, Expr)],
        ind: usize,
        out: &mut String,
    ) {
        let t = "\t".repeat(ind);
        let spell = if map_ty.base.is_empty() { "std::map<std::string, void*>".to_string() } else { self.decl_spelling(map_ty) };
        let _ = writeln!(out, "{t}{spell} {name};");
        for (k, v) in pairs {
            let (kc, _) = self.gen_expr(k);
            self.flush(out);
            if let Expr::ObjectLit(fields) = v {
                let tmp = self.fresh("val");
                self.expand_object_into_local(&tmp, val, fields, ind, out);
                let _ = writeln!(out, "{t}{name}[{kc}] = {tmp};");
            } else {
                let (vc, _) = self.gen_expr(v);
                self.flush(out);
                let _ = writeln!(out, "{t}{name}[{kc}] = {vc};");
            }
        }
    }

    /// Emit a local anonymous `struct { ... } name;` for an object literal that
    /// has no nominal target type, inferring each field's type from its value.
    pub(super) fn expand_anon_struct_local(
        &mut self,
        name: &str,
        fields: &[(String, Expr)],
        ind: usize,
        out: &mut String,
    ) -> Ty {
        let t = "\t".repeat(ind);
        // Generate the field values first (so any prelude is captured) and record
        // their types for the struct declaration.
        let mut decls = Vec::new();
        let mut assigns = Vec::new();
        for (key, value) in fields {
            let (vcode, vty) = self.gen_expr(value);
            self.flush(out);
            let spell = if vty.base.is_empty() { "int".to_string() } else { self.decl_spelling(&vty) };
            decls.push(format!("{spell} {key};"));
            assigns.push((key.clone(), vcode));
        }
        let _ = writeln!(out, "{t}struct {{ {} }} {name};", decls.join(" "));
        for (key, vcode) in assigns {
            let _ = writeln!(out, "{t}{name}.{key} = {vcode};");
        }
        Ty::default()
    }
}
