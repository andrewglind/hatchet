//! Statement, switch, and loop lowering for `BodyGen`. Split out of `source.rs`.

use super::*;

impl<'a> BodyGen<'a> {
    // ---- statements ----------------------------------------------------

    pub(super) fn gen_stmt(&mut self, st: &Stmt, ind: usize, out: &mut String) {
        let t = "\t".repeat(ind);
        self.prelude_ind = ind;
        // By default a statement's `new` arguments are owned by this scope; the
        // Var/Return/field-assignment arms below override this when the value
        // escapes (then the receiver owns the arguments).
        self.new_args_escape = false;
        match st {
            Stmt::Var {
                name,
                ty,
                init,
                is_final: _,
                delete,
                line,
            } => {
                self.current_line = *line;
                // A local that escapes (assigned to a field / returned later) is
                // owned elsewhere, so its `new` arguments are too.
                self.new_args_escape = self.escaping.contains(name);
                let declared = ty.as_ref().map(|t| self.ty_of(t));
                // var x = map.get(k)  →  bind x as a map-iterator alias. A null
                // check on x then lowers to `it == map.end()` and any value/member
                // use to `it->second` — faithful to the developer's intent (the
                // null check need not immediately follow, nor exit the scope).
                if let Some((map_expr, key)) = map_get_init(init.as_ref()) {
                    if self.try_bind_map_iter(name, declared.as_ref(), map_expr, key, ind, out) {
                        return;
                    }
                }
                // var x:T = { ... }  → build the struct directly into x
                if let Some(Expr::ObjectLit(fields)) = init {
                    match declared.clone() {
                        Some(decl_ty) if decl_ty.info.is_some() => {
                            self.expand_object_into_local(name, &decl_ty, fields, ind, out);
                            self.define_local(name, decl_ty);
                        }
                        _ => {
                            // No nominal type: emit a local anonymous struct.
                            let anon = self.expand_anon_struct_local(name, fields, ind, out);
                            self.define_local(name, anon);
                        }
                    }
                    return;
                }
                // var x:Array<T> = [a, b, ...]  → vector with push_backs
                if let Some(Expr::ArrayLit(elems)) = init {
                    if !elems.is_empty() {
                        let vec_ty = declared.clone().unwrap_or_else(|| self.infer_array(elems));
                        let elem = self
                            .elem_ast_ty(ty.as_ref())
                            .unwrap_or_else(|| self.element_ty(&vec_ty));
                        self.expand_array_into_local(name, &vec_ty, &elem, elems, ind, out);
                        self.define_local(name, vec_ty);
                        return;
                    }
                }
                // var x:Map<K,V> = ["k" => v, ...]  → map with inserts
                if let Some(Expr::MapLit(pairs)) = init {
                    if !pairs.is_empty() {
                        let map_ty = declared.clone().unwrap_or_default();
                        let (_, vty) = self.map_kv_ast_ty(ty.as_ref());
                        self.expand_map_into_local(name, &map_ty, &vty, pairs, ind, out);
                        self.define_local(name, map_ty);
                        return;
                    }
                }
                // var x:Array<T> = []  → just declare an empty container
                let empty = matches!(init, Some(Expr::ArrayLit(v)) if v.is_empty())
                    || matches!(init, Some(Expr::MapLit(v)) if v.is_empty());
                let (init_code, val_ty) = if empty {
                    (None, None)
                } else if let Some(e) = init {
                    // The declared type is the contextual hint for the initialiser
                    // (used by `Array.map` to type its result element).
                    self.expected = declared.clone();
                    let (c, ty) = self.gen_expr(e);
                    self.expected = None;
                    (Some(c), Some(ty))
                } else {
                    (None, None)
                };
                let nullable_init = val_ty.as_ref().map(|t| t.nullable).unwrap_or(false);
                // A nullable (`Null<T>`) initialiser requires a `Null<T>` local.
                if nullable_init && !is_null_type(ty) {
                    self.warn(format!(
                        "local '{name}' is assigned a Null<T> value but is not declared `Null<T>`; declare it `Null<T>` so the nullable result is tracked"
                    ));
                }
                let var_ty = declared.or(val_ty).unwrap_or_default();
                let cpp = self.decl_spelling(&var_ty);
                // Rename if this local would shadow a parameter/outer local (the
                // init was generated above, so it still sees the shadowed name).
                let emit = self.bind_local_name(name);
                self.flush(out);
                match init_code {
                    Some(code) => {
                        let _ = writeln!(out, "{t}{cpp} {emit} = {code};");
                    }
                    None => {
                        let _ = writeln!(out, "{t}{cpp} {emit};");
                    }
                }
                let is_ptr = var_ty.is_ptr;
                self.define_local(name, var_ty);
                // A non-escaping local holding a fresh `new` / nullable heap result
                // is owned by this scope and deleted when it closes. `@delete` is the
                // developer's explicit override: free this pointer at scope close
                // regardless of what the analysis would infer (e.g. a returned
                // pointer the scope would otherwise leak). Pointer-only — `delete`ing
                // a value local is meaningless.
                let heap_new = init
                    .as_ref()
                    .is_some_and(|e| matches!(e, Expr::New(ty, _) if !self.value_new(ty)));
                let owns_heap = heap_new || nullable_init;
                let forced = *delete && is_ptr;
                if forced || (owns_heap && !self.escaping.contains(name)) {
                    self.register_owned(&emit);
                }
            }
            Stmt::Expr(e, line) => {
                self.current_line = *line;
                // Pushing a `new` into a container field this class owns (frees in
                // its destructor) stores it long-term, so it escapes this scope —
                // emit it inline rather than hoisting it into a scope-owned local
                // that would be deleted (leaving a dangling pointer in the field).
                if let Some(value) = self.push_into_retaining_container(e) {
                    // a `new` value is emitted inline (the container keeps it)…
                    if matches!(value, Expr::New(ty, _) if !self.value_new(ty)) {
                        self.new_args_escape = true;
                    }
                    // …and an owned local handed to the container transfers out.
                    if let Expr::Ident(name) = value {
                        if self.lookup_local(name).is_some() {
                            self.transfer_owned(name);
                        }
                    }
                }
                // Assigning into a field stores the value long-term, so its `new`
                // arguments are owned by the receiver, not this scope. This holds
                // whether the field is written `this.field` or bare `field` (Haxe
                // lets you omit `this.`) — the bare form must not be treated as a
                // scope-local, or its `new` args would be wrongly freed at scope
                // close, leaving the field dangling.
                if let Expr::Assign {
                    op: None, target, ..
                } = e
                {
                    let own_field = self.assigned_own_field(target);
                    if matches!(&**target, Expr::Field(..)) || own_field.is_some() {
                        self.new_args_escape = true;
                    }
                    // Delete-before-overwrite: reassigning an owned pointer field
                    // (outside the constructor, where it is NULL-initialised) frees
                    // the prior value first. When the write routes through a custom
                    // `set_x`, the setter's own direct `x = v` is the single funnel
                    // for all writes — the delete is emitted *there*, so the routed
                    // caller must not also free (that would be a double delete).
                    if self.current_fn != "new" {
                        if let Some(field) = &own_field {
                            if self.owned_fields.contains(field)
                                && self.own_field_setter(target).is_none()
                            {
                                let _ = writeln!(out, "{t}delete this->{field};");
                            }
                        }
                    }
                }
                let (code, ety) = self.gen_assign_or_expr(e);
                self.flush(out);
                // A bare `Null<T>` call result is heap-allocated (the callee
                // `new`ed it). If the developer discards it, bind it to a fresh
                // `Null<T>` local so method-scope cleanup frees it instead of
                // leaking — protection the dev did not write explicitly.
                if matches!(e, Expr::Call(..)) && ety.nullable {
                    let tmp = self.fresh("null");
                    let spelling = self.decl_spelling(&ety);
                    let _ = writeln!(out, "{t}{spelling} {tmp} = {code};");
                    self.register_owned(&tmp);
                } else {
                    let _ = writeln!(out, "{t}{code};");
                }
            }
            Stmt::Return(None, _) => {
                // Free this scope's (and any enclosing scope's) owned heap locals
                // before exiting — an early `return` skips the closing-brace deletes.
                self.emit_all_owned_deletes(out, ind);
                let _ = writeln!(out, "{t}return;");
            }
            Stmt::Return(Some(e), line) => {
                self.current_line = *line;
                self.prelude_ind = ind;
                // A returned value's heap arguments are owned by the caller.
                self.new_args_escape = true;
                if matches!(e, Expr::Null) {
                    // `return null` on a value type returns a default instance.
                    let r = self.return_null_value();
                    self.finish_return(out, ind, r);
                } else if let Expr::ObjectLit(fields) = e {
                    // Build a value temp of the (de-pointered) return type, then
                    // heap-wrap it if the function returns a pointer.
                    let val_ty = self.return_value_ty();
                    let tmp = self.fresh("ret");
                    self.expand_object_into_local(&tmp, &val_ty, fields, ind, out);
                    let r = self.wrap_ret(tmp);
                    self.finish_return(out, ind, r);
                } else if let Expr::ArrayLit(elems) = e {
                    if elems.is_empty() {
                        let r = self.return_empty_container();
                        self.finish_return(out, ind, r);
                    } else {
                        let val_ty = self.return_value_ty();
                        let elem = self.element_ty(&val_ty);
                        let tmp = self.fresh("ret");
                        self.expand_array_into_local(&tmp, &val_ty, &elem, elems, ind, out);
                        let r = self.wrap_ret(tmp);
                        self.finish_return(out, ind, r);
                    }
                } else if matches!(e, Expr::MapLit(pairs) if pairs.is_empty()) {
                    // An empty map literal `return {}`/`return [...]` for a
                    // by-value `std::map<...>` return must default-construct the
                    // container, not `return NULL` (which won't compile).
                    let r = self.return_empty_container();
                    self.finish_return(out, ind, r);
                } else {
                    // The return type is the contextual (expected) type of the
                    // returned expression — so a value-position `switch`/etc. in
                    // `return` position unifies its arms to the function's return
                    // type (e.g. a base class) rather than its first arm's type.
                    let saved = self.expected.take();
                    self.expected = Some(self.current_ret.clone());
                    let (c, cty) = self.gen_expr(e);
                    self.expected = saved;
                    self.flush(out);
                    // A nullable (`Null<T>`) function returns a pointer; a value
                    // result is heap-allocated to match.
                    let r = if self.current_ret.is_ptr && !cty.is_ptr {
                        format!("new {}({c})", self.current_ret.base)
                    } else {
                        c
                    };
                    self.finish_return(out, ind, r);
                }
            }
            Stmt::If {
                cond,
                then,
                els,
                line,
            } => {
                self.current_line = *line;
                self.prelude_ind = ind;
                let (c, _) = self.gen_expr(cond);
                self.flush(out);
                let _ = writeln!(out, "{t}if ({c}) {{");
                self.gen_block(then, ind + 1, out);
                if let Some(e) = els {
                    let _ = writeln!(out, "{t}}} else {{");
                    self.gen_block(e, ind + 1, out);
                }
                let _ = writeln!(out, "{t}}}");
            }
            Stmt::While {
                cond,
                body,
                do_while,
                line,
            } => {
                self.current_line = *line;
                self.prelude_ind = ind;
                let saved = self.enter_loop();
                if *do_while {
                    let _ = writeln!(out, "{t}do {{");
                    self.gen_block(body, ind + 1, out);
                    let (c, _) = self.gen_expr(cond);
                    self.flush(out);
                    let _ = writeln!(out, "{t}}} while ({c});");
                } else {
                    let (c, _) = self.gen_expr(cond);
                    self.flush(out);
                    let _ = writeln!(out, "{t}while ({c}) {{");
                    self.gen_block(body, ind + 1, out);
                    let _ = writeln!(out, "{t}}}");
                }
                self.exit_loop(saved);
            }
            Stmt::For {
                var,
                value_var,
                iter,
                body,
                line,
            } => {
                self.current_line = *line;
                let saved = self.enter_loop();
                self.gen_for(var, value_var.as_deref(), iter, body, ind, out);
                self.exit_loop(saved);
            }
            Stmt::Switch {
                subject,
                cases,
                default,
                line,
            } => {
                self.current_line = *line;
                self.gen_switch(subject, cases, default.as_deref(), ind, out)
            }
            Stmt::Block(stmts) => {
                let _ = writeln!(out, "{t}{{");
                self.push_scope();
                for s in stmts {
                    self.gen_stmt(s, ind + 1, out);
                }
                self.emit_owned_deletes(out, ind + 1);
                self.pop_scope();
                let _ = writeln!(out, "{t}}}");
            }
            Stmt::Break => {
                // Inside a generated C++ `switch`, a bare `break` would exit the
                // switch; Haxe's `break` exits the enclosing loop. Route it
                // through the hoisted flag checked after the switch.
                if let Some(f) = self.switch_break_flag.clone() {
                    let _ = writeln!(out, "{t}{f} = true;");
                }
                let _ = writeln!(out, "{t}break;");
            }
            Stmt::Continue => {
                let _ = writeln!(out, "{t}continue;");
            }
            Stmt::Throw(e, line) => {
                self.current_line = *line;
                self.prelude_ind = ind;
                let (c, ty) = self.gen_expr(e);
                self.flush(out);
                // Coerce a thrown `String` to `std::string`, so it matches a
                // `catch (e:String)` (a bare literal would otherwise throw a
                // `const char*`, which that catch would not catch).
                if ty.base == "std::string" {
                    let _ = writeln!(out, "{t}throw std::string({c});");
                } else {
                    let _ = writeln!(out, "{t}throw {c};");
                }
            }
            Stmt::Verbatim { code, line } => {
                self.current_line = *line;
                // Emitted at column 0 (no indentation) so preprocessor directives
                // such as `#ifdef`/`#else`/`#endif` are valid; the text is written
                // exactly as the developer supplied it, save for line-ending
                // normalisation (sources are often CRLF; generated C++ is always LF).
                let code = code.replace("\r\n", "\n").replace('\r', "\n");
                let _ = writeln!(out, "{code}");
            }
            // `try { … } catch (e:T) { … }` → a C++ try/catch. Each block carries
            // its normal scope, so owned locals are freed at the *normal* close of
            // the block. On an exception the unwind skips those frees — a deliberate
            // **conservative leak** (never a double-free/UAF); the developer frees in
            // the catch if it matters. Haxe has no `finally`, so there is none to
            // emulate. Requires exceptions enabled on the target (VC6 `/GX`).
            Stmt::Try {
                body,
                catches,
                line,
            } => {
                self.current_line = *line;
                let _ = writeln!(out, "{t}try {{");
                self.push_scope();
                match &**body {
                    Stmt::Block(stmts) => {
                        for s in stmts {
                            self.gen_stmt(s, ind + 1, out);
                        }
                    }
                    other => self.gen_stmt(other, ind + 1, out),
                }
                self.emit_owned_deletes(out, ind + 1);
                self.pop_scope();
                for c in catches {
                    let (header, binds) = self.catch_header(c);
                    let _ = writeln!(out, "{t}}} {header} {{");
                    self.push_scope();
                    // Bind the caught value's name for a typed catch; an untyped /
                    // `Dynamic` catch is `catch (...)`, which binds nothing — mark the
                    // name so any use of it in the body is a hard error.
                    if binds {
                        if let Some(ht) = &c.ty {
                            let ty = self.ty_of(ht);
                            self.define_local(&c.name, ty);
                        }
                    } else {
                        self.nonbinding_catch_vars.push(c.name.clone());
                    }
                    for s in &c.body {
                        self.gen_stmt(s, ind + 1, out);
                    }
                    self.emit_owned_deletes(out, ind + 1);
                    self.pop_scope();
                    if !binds {
                        self.nonbinding_catch_vars.pop();
                    }
                }
                let _ = writeln!(out, "{t}}}");
            }
        }
    }

    /// The C++ `catch (...)` header for a Haxe catch clause, plus whether it binds a
    /// local. A typed catch maps via `param_decl` (`catch (Foo* e)` / `catch (const
    /// std::string& e)`); an untyped or `Dynamic`/`Any` catch is the non-binding
    /// catch-all `catch (...)` (so referencing its name in the body will not compile
    /// — by design, the developer should catch a concrete type to use the value).
    pub(super) fn catch_header(&self, c: &Catch) -> (String, bool) {
        let dynamic = match &c.ty {
            None => true,
            Some(Type::Named { path, .. }) => {
                matches!(
                    path.last().map(|s| s.as_str()),
                    Some("Dynamic") | Some("Any")
                )
            }
            Some(_) => false,
        };
        if dynamic {
            return ("catch (...)".to_string(), false);
        }
        let p = Param {
            name: c.name.clone(),
            ty: c.ty.clone(),
            optional: false,
            default: None,
            rest: false,
            meta: Vec::new(),
        };
        (
            format!(
                "catch ({})",
                crate::codegen::param_decl(self.prog, self.mi, &self.ns, &p)
            ),
            true,
        )
    }

    pub(super) fn gen_block(&mut self, st: &Stmt, ind: usize, out: &mut String) {
        self.push_scope();
        match st {
            Stmt::Block(stmts) => {
                for s in stmts {
                    self.gen_stmt(s, ind, out);
                }
            }
            other => self.gen_stmt(other, ind, out),
        }
        self.emit_owned_deletes(out, ind);
        self.pop_scope();
    }

    pub(super) fn gen_for(
        &mut self,
        var: &str,
        value_var: Option<&str>,
        iter: &Iterable,
        body: &Stmt,
        ind: usize,
        out: &mut String,
    ) {
        let t = "\t".repeat(ind);
        self.push_scope();
        // The iterable may hoist a prelude (e.g. an anonymous array literal
        // `for (i in [1,2,3])` builds a `std::vector` temporary); emit it just
        // before the loop so the temporary is in scope.
        self.prelude_ind = ind;
        match iter {
            Iterable::Range(start, end) => {
                let (s, _) = self.gen_expr(start);
                let (e, ety) = self.gen_expr(end);
                self.flush(out);
                self.define_local(
                    var,
                    Ty {
                        base: "int".into(),
                        ..Default::default()
                    },
                );
                let lv = self.loop_var(var);
                // `0...arr.length` compares an `int` counter against `size()` (size_t);
                // cast the counter to silence MSVC's C4018, as the body comparisons do.
                let lcmp = if ety.unsigned {
                    format!("(size_t){lv}")
                } else {
                    lv.clone()
                };
                let _ = writeln!(out, "{t}for (int {lv} = {s}; {lcmp} < {e}; ++{lv}) {{");
                self.gen_block_inner(body, ind + 1, out);
                self.emit_owned_deletes(out, ind + 1);
                let _ = writeln!(out, "{t}}}");
            }
            Iterable::Coll(coll) => {
                let (c, cty) = self.gen_expr(coll);
                self.flush(out);
                // A nullable container is a pointer — dereference it to iterate.
                let access = if cty.is_ptr { format!("(*{c})") } else { c };
                if rcode_is_map(&cty) {
                    // Map iteration via a std::map iterator. `for (k => v in m)`
                    // binds both; `for (v in m)` binds the value (Haxe iterates a
                    // map's values, not its keys).
                    let it = self.fresh("it");
                    let kty = self.map_key_ty(&cty);
                    let vty = self.map_value_ty(&cty);
                    let kspell = self.decl_spelling(&kty);
                    let vspell = self.decl_spelling(&vty);
                    // `const_iterator`: a Map parameter is passed by `const&`, so
                    // `begin()` yields a `const_iterator`; the bindings only read
                    // (copy) the key/value, so this is always sufficient (and
                    // converts cleanly from a non-const local map too).
                    let _ = writeln!(
                        out,
                        "{t}for ({}::const_iterator {it} = {access}.begin(); {it} != {access}.end(); ++{it}) {{",
                        cty.base
                    );
                    if let Some(vv) = value_var {
                        self.define_local(var, kty);
                        self.define_local(vv, vty);
                        let _ = writeln!(out, "{t}\t{kspell} {var} = {it}->first;");
                        let _ = writeln!(out, "{t}\t{vspell} {vv} = {it}->second;");
                    } else {
                        self.define_local(var, vty);
                        let _ = writeln!(out, "{t}\t{vspell} {var} = {it}->second;");
                    }
                    self.gen_block_inner(body, ind + 1, out);
                    self.emit_owned_deletes(out, ind + 1);
                    let _ = writeln!(out, "{t}}}");
                } else if is_container_ty(&cty) {
                    // for (item in array) → index loop with element binding.
                    // for (index => item in array) → the key is the Int index.
                    let idx = self.fresh("i");
                    let elem_ty = self.element_ty(&cty);
                    let elem_spell = self.decl_spelling(&elem_ty);
                    let _ = writeln!(
                        out,
                        "{t}for (size_t {idx} = 0; {idx} < {access}.size(); ++{idx}) {{"
                    );
                    if let Some(vv) = value_var {
                        self.define_local(var, int_ty());
                        self.define_local(vv, elem_ty);
                        let _ = writeln!(out, "{t}\tint {var} = (int){idx};");
                        let _ = writeln!(out, "{t}\t{elem_spell} {vv} = {access}[{idx}];");
                    } else {
                        self.define_local(var, elem_ty);
                        let _ = writeln!(out, "{t}\t{elem_spell} {var} = {access}[{idx}];");
                    }
                    self.gen_block_inner(body, ind + 1, out);
                    self.emit_owned_deletes(out, ind + 1);
                    let _ = writeln!(out, "{t}}}");
                } else {
                    // Not a range, vector, or map: Hatchet has no general iterator
                    // protocol, so this would otherwise emit invalid `.size()`/`[]`
                    // access. Fail loudly instead of guessing.
                    self.err(format!(
                        "cannot iterate `for ({var} in ...)` over `{}`: only ranges, Array, and Map are iterable (a custom Iterator/Iterable is unsupported)",
                        if cty.base.is_empty() { "an unknown type" } else { &cty.base }
                    ));
                }
            }
        }
        self.pop_scope();
    }

    /// `[for (v in coll) body]` / `[for (v in coll) k => val]` → a hoisted
    /// `std::vector`/`std::map` temporary populated by an explicit loop.
    pub(super) fn gen_comprehension(
        &mut self,
        var: &str,
        value_var: Option<&str>,
        iter: &Iterable,
        guard: Option<&Expr>,
        body: &ComprBody,
    ) -> (String, Ty) {
        let ind = self.prelude_ind;
        let t = "\t".repeat(ind);
        let tmp = self.fresh("compr");
        let mut buf = String::new();

        self.push_scope();
        // Loop scaffolding + bind the loop variable.
        let (header, close): (String, String) = match iter {
            Iterable::Range(start, end) => {
                let (s, _) = self.gen_expr(start);
                let (e, ety) = self.gen_expr(end);
                self.define_local(var, int_ty());
                let lv = self.loop_var(var);
                let lcmp = if ety.unsigned {
                    format!("(size_t){lv}")
                } else {
                    lv.clone()
                };
                (
                    format!("{t}for (int {lv} = {s}; {lcmp} < {e}; ++{lv}) {{\n"),
                    format!("{t}}}\n"),
                )
            }
            Iterable::Coll(coll) => {
                let (c, cty) = self.gen_expr(coll);
                // A nullable container is a pointer — dereference it to iterate.
                let access = if cty.is_ptr { format!("(*{c})") } else { c };
                if rcode_is_map(&cty) {
                    // Map iteration via a std::map iterator. `for (k => v in m)`
                    // binds both; `for (v in m)` binds the value (Haxe iterates a
                    // map's values, not its keys).
                    let it = self.fresh("it");
                    let (kty, vty) = self.map_kv_ty(&cty);
                    let kspell = self.decl_spelling(&kty);
                    let vspell = self.decl_spelling(&vty);
                    let mut hdr = format!(
                        "{t}for ({}::const_iterator {it} = {access}.begin(); {it} != {access}.end(); ++{it}) {{\n",
                        cty.base
                    );
                    if let Some(vv) = value_var {
                        self.define_local(var, kty);
                        self.define_local(vv, vty);
                        let _ = write!(hdr, "{t}\t{kspell} {var} = {it}->first;\n{t}\t{vspell} {vv} = {it}->second;\n");
                    } else {
                        self.define_local(var, vty);
                        let _ = writeln!(hdr, "{t}\t{vspell} {var} = {it}->second;");
                    }
                    (hdr, format!("{t}}}\n"))
                } else {
                    let idx = self.fresh("i");
                    let elem = self.element_ty(&cty);
                    let espell = self.decl_spelling(&elem);
                    let mut hdr =
                        format!("{t}for (size_t {idx} = 0; {idx} < {access}.size(); ++{idx}) {{\n");
                    if let Some(vv) = value_var {
                        // `for (index => value in array)`: the key is the Int index.
                        self.define_local(var, int_ty());
                        self.define_local(vv, elem);
                        let _ = write!(
                            hdr,
                            "{t}\tint {var} = (int){idx};\n{t}\t{espell} {vv} = {access}[{idx}];\n"
                        );
                    } else {
                        self.define_local(var, elem);
                        let _ = writeln!(hdr, "{t}\t{espell} {var} = {access}[{idx}];");
                    }
                    (hdr, format!("{t}}}\n"))
                }
            }
        };

        // The contextual sink (`return`/`var x:Array<T> = …`) is the *whole*
        // container; each produced element's expected type is the element type, so
        // narrow it for the body (an `if`/`switch`-expression body unifies its
        // branches to this, rather than mistaking the array type for the element's).
        let elem_hint = self.expected.take().map(|v| self.elem_member_ty(&v));

        // Generate the body (capturing any hoisted prelude so it lands in-loop).
        let saved = std::mem::take(&mut self.prelude);
        let (push_line, container) = match body {
            ComprBody::Value(e) => {
                self.expected = elem_hint.clone();
                let (bcode, bty) = self.gen_expr(e);
                self.expected = None;
                let inner = if bty.base.is_empty() {
                    "int".to_string()
                } else {
                    self.decl_spelling(&bty)
                };
                (
                    format!("{t}\t{tmp}.push_back({bcode});\n"),
                    format!("std::vector<{inner} >"),
                )
            }
            ComprBody::KeyValue(k, v) => {
                let (kcode, _) = self.gen_expr(k);
                let (vcode, vty) = self.gen_expr(v);
                let vspell = if vty.base.is_empty() {
                    "void*".to_string()
                } else {
                    self.decl_spelling(&vty)
                };
                (
                    format!("{t}\t{tmp}[{kcode}] = {vcode};\n"),
                    format!("std::map<std::string, {vspell} >"),
                )
            }
        };
        let body_prelude = std::mem::replace(&mut self.prelude, saved);

        let _ = writeln!(buf, "{t}{container} {tmp};");
        buf.push_str(&header);
        buf.push_str(&body_prelude);
        if let Some(g) = guard {
            let (gc, _) = self.gen_expr(g);
            let _ = write!(buf, "{t}\tif ({gc}) {{\n{t}\t{push_line}{t}\t}}\n");
        } else {
            buf.push_str(&push_line);
        }
        buf.push_str(&close);
        self.pop_scope();

        self.prelude.push_str(&buf);
        (
            tmp,
            Ty {
                base: String::new(),
                ..Default::default()
            },
        )
    }

    /// `coll.map(lambda)` → a hoisted `std::vector` filled by looping over `coll`
    /// and applying the lambda to each element — the **Map-comprehension + Lambda**
    /// composition. The result's element type is taken from the contextual hint
    /// (`self.expected`, the assignment/declaration target) when present, else
    /// inferred from the lambda body. When the body is an object literal it is
    /// expanded into a temporary of that element type (so `{ x:…, y:… }` becomes a
    /// nominal struct, not an anonymous one).
    pub(super) fn gen_array_map(
        &mut self,
        rcode: &str,
        rty: &Ty,
        params: &[Param],
        body: &LambdaBody,
    ) -> (String, Ty) {
        let ind = self.prelude_ind;
        let t = "\t".repeat(ind);
        let tmp = self.fresh("map");
        let idx = self.fresh("i");
        // A nullable container is a pointer (`Null<Array<T>>`) — dereference it.
        let access = if rty.is_ptr {
            format!("(*{rcode})")
        } else {
            rcode.to_string()
        };
        let in_elem = self.elem_member_ty(rty);
        let in_spell = self.decl_spelling(&in_elem);
        let var = params
            .first()
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "_x".to_string());
        // Contextual element type from the surrounding sink (assignment/var target).
        let hint_elem = self.expected.take().map(|v| self.elem_member_ty(&v));

        self.push_scope();
        self.define_local(&var, in_elem.clone());

        // Generate the per-element push, capturing any prelude the body hoists so it
        // lands inside the loop (mirrors `gen_comprehension`).
        let saved = std::mem::take(&mut self.prelude);
        let mut push_block = String::new();
        let body_elem: Ty = match body {
            LambdaBody::Expr(Expr::ObjectLit(fields)) => {
                let elem = hint_elem.clone().unwrap_or_default();
                let etmp = self.fresh("elem");
                self.expand_object_into_local(&etmp, &elem, fields, ind + 1, &mut push_block);
                let _ = writeln!(push_block, "{t}\t{tmp}.push_back({etmp});");
                elem
            }
            LambdaBody::Expr(e) => {
                let (bcode, bty) = self.gen_expr(e);
                let _ = writeln!(push_block, "{t}\t{tmp}.push_back({bcode});");
                bty
            }
            // Block-bodied map lambdas are not supported.
            LambdaBody::Block(_) => Ty::default(),
        };
        let body_prelude = std::mem::replace(&mut self.prelude, saved);
        self.pop_scope();

        // Prefer the contextual element type (the target is authoritative), else the
        // body's inferred type.
        let out_elem = match &hint_elem {
            Some(h) if !h.base.is_empty() => h.clone(),
            _ => body_elem,
        };
        let out_spell = if out_elem.base.is_empty() {
            "void*".to_string()
        } else {
            self.decl_spelling(&out_elem)
        };
        let vec_ty = Ty {
            base: format!("std::vector<{out_spell} >"),
            ..Default::default()
        };

        let mut buf = String::new();
        let _ = writeln!(buf, "{t}{} {tmp};", self.decl_spelling(&vec_ty));
        let _ = writeln!(
            buf,
            "{t}for (size_t {idx} = 0; {idx} < {access}.size(); ++{idx}) {{"
        );
        let _ = writeln!(buf, "{t}\t{in_spell} {var} = {access}[{idx}];");
        buf.push_str(&body_prelude);
        buf.push_str(&push_block);
        let _ = writeln!(buf, "{t}}}");
        self.prelude.push_str(&buf);

        (tmp, vec_ty)
    }

    /// `Array.filter(p)` → a hoisted `std::vector<T>` (same element type as the
    /// receiver) holding the elements for which the predicate is true. Mirrors
    /// `gen_array_map`'s lambda inlining, but the body is a `bool` guard.
    pub(super) fn gen_array_filter(
        &mut self,
        rcode: &str,
        rty: &Ty,
        params: &[Param],
        body: &LambdaBody,
    ) -> (String, Ty) {
        let ind = self.prelude_ind;
        let t = "\t".repeat(ind);
        let tmp = self.fresh("flt");
        let idx = self.fresh("i");
        let access = if rty.is_ptr {
            format!("(*{rcode})")
        } else {
            rcode.to_string()
        };
        let elem = self.elem_member_ty(rty);
        let spell = self.decl_spelling(&elem);
        let var = params
            .first()
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "_x".to_string());

        self.push_scope();
        self.define_local(&var, elem.clone());
        // Capture any prelude the predicate hoists so it lands inside the loop.
        let saved = std::mem::take(&mut self.prelude);
        let pred = match body {
            LambdaBody::Expr(e) => self.gen_expr(e).0,
            // A block-bodied predicate is not supported; bail to keep the match total.
            LambdaBody::Block(_) => {
                self.prelude = saved;
                self.pop_scope();
                return (tmp, Ty::default());
            }
        };
        let pred_prelude = std::mem::replace(&mut self.prelude, saved);
        self.pop_scope();

        let vec_ty = Ty {
            base: format!("std::vector<{spell} >"),
            ..Default::default()
        };
        let mut buf = String::new();
        let _ = writeln!(buf, "{t}{} {tmp};", self.decl_spelling(&vec_ty));
        let _ = writeln!(
            buf,
            "{t}for (size_t {idx} = 0; {idx} < {access}.size(); ++{idx}) {{"
        );
        let _ = writeln!(buf, "{t}\t{spell} {var} = {access}[{idx}];");
        buf.push_str(&pred_prelude);
        let _ = writeln!(buf, "{t}\tif ({pred}) {{ {tmp}.push_back({var}); }}");
        let _ = writeln!(buf, "{t}}}");
        self.prelude.push_str(&buf);

        (tmp, vec_ty)
    }

    /// `Array.sort(cmp)` → an in-place insertion sort (no `<algorithm>`). The
    /// comparator lambda takes two elements and returns an `Int` (`< 0` / `0` /
    /// `> 0`, like Haxe). Mutates the receiver; the expression's value is Void.
    pub(super) fn gen_array_sort(
        &mut self,
        rcode: &str,
        rty: &Ty,
        params: &[Param],
        body: &LambdaBody,
    ) -> (String, Ty) {
        let ind = self.prelude_ind;
        let t = "\t".repeat(ind);
        let access = if rty.is_ptr {
            format!("(*{rcode})")
        } else {
            rcode.to_string()
        };
        let elem = self.elem_member_ty(rty);
        let spell = self.decl_spelling(&elem);
        let i = self.fresh("i");
        let j = self.fresh("j");
        let key = self.fresh("key");
        let cmp = self.fresh("cmp");
        // The comparator's two parameters (defaulted if the lambda omits them).
        let a = params
            .first()
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "_a".to_string());
        let b = params
            .get(1)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "_b".to_string());

        self.push_scope();
        self.define_local(&a, elem.clone());
        self.define_local(&b, elem.clone());
        // Comparator body, capturing any prelude it hoists.
        let saved = std::mem::take(&mut self.prelude);
        let ccode = match body {
            LambdaBody::Expr(e) => self.gen_expr(e).0,
            LambdaBody::Block(_) => {
                self.prelude = saved;
                self.pop_scope();
                return ("((void)0)".to_string(), Ty::default());
            }
        };
        let cmp_prelude = std::mem::replace(&mut self.prelude, saved);
        self.pop_scope();

        // Insertion sort: shift elements while the comparator says they belong after
        // `key`. `a` is bound to the element under test, `b` to `key`, so the
        // comparator's parameter references resolve inside the loop body.
        let mut buf = String::new();
        let _ = writeln!(
            buf,
            "{t}for (size_t {i} = 1; {i} < {access}.size(); ++{i}) {{"
        );
        let _ = writeln!(buf, "{t}\t{spell} {key} = {access}[{i}];");
        let _ = writeln!(buf, "{t}\tsize_t {j} = {i};");
        let _ = writeln!(buf, "{t}\twhile ({j} > 0) {{");
        let _ = writeln!(buf, "{t}\t\t{spell} {a} = {access}[{j} - 1];");
        let _ = writeln!(buf, "{t}\t\t{spell} {b} = {key};");
        buf.push_str(&cmp_prelude);
        let _ = writeln!(buf, "{t}\t\tint {cmp} = {ccode};");
        let _ = writeln!(buf, "{t}\t\tif ({cmp} <= 0) break;");
        let _ = writeln!(buf, "{t}\t\t{access}[{j}] = {a};");
        let _ = writeln!(buf, "{t}\t\t--{j};");
        let _ = writeln!(buf, "{t}\t}}");
        let _ = writeln!(buf, "{t}\t{access}[{j}] = {key};");
        let _ = writeln!(buf, "{t}}}");
        self.prelude.push_str(&buf);

        ("((void)0)".to_string(), Ty::default())
    }

    pub(super) fn gen_block_inner(&mut self, st: &Stmt, ind: usize, out: &mut String) {
        match st {
            Stmt::Block(stmts) => {
                for s in stmts {
                    self.gen_stmt(s, ind, out);
                }
            }
            other => self.gen_stmt(other, ind, out),
        }
    }

    /// Enter a loop body: bump the depth and suspend any enclosing switch's
    /// break flag — a `break` inside the nested loop binds to that loop.
    pub(super) fn enter_loop(&mut self) -> Option<String> {
        self.loop_depth += 1;
        self.switch_break_flag.take()
    }
    pub(super) fn exit_loop(&mut self, saved: Option<String>) {
        self.loop_depth -= 1;
        self.switch_break_flag = saved;
    }

    pub(super) fn gen_switch(
        &mut self,
        subject: &Expr,
        cases: &[Case],
        default: Option<&[Stmt]>,
        ind: usize,
        out: &mut String,
    ) {
        let t = "\t".repeat(ind);
        let (subj, sty) = self.gen_expr(subject);
        // The subject may hoist statements (a call, a comprehension); emit them
        // before the switch rather than leaving them for the next statement.
        self.flush(out);
        // A subject that cannot be a C++ `case` label — a `String`, or a
        // floating-point value such as a non-integral `enum abstract` — lowers to an
        // `if`/`else if` chain comparing the subject (hoisted once) against each
        // case's pattern(s) with `==`.
        if matches!(sty.base.as_str(), "std::string" | "float" | "double") {
            let spell = self.decl_spelling(&sty);
            self.gen_equality_switch(&subj, &spell, cases, default, ind, out);
            return;
        }
        // An ADT subject (a tagged value class) dispatches on its `kind`; a
        // destructuring case (`case Add(a, b):`) binds payload fields to locals.
        // The subject is hoisted into a local unless it is already a bare name,
        // so payload reads never re-evaluate a side-effecting expression.
        let adt = sty
            .info
            .as_ref()
            .and_then(|i| self.adt_enum(i).map(|e| (i.clone(), e)));
        let (switch_subj, sv, acc) = match &adt {
            Some(_) => {
                let acc = if sty.is_ptr { "->" } else { "." };
                let sv = if matches!(subject, Expr::Ident(_)) {
                    subj.clone()
                } else {
                    let tmp = self.fresh("subj");
                    let spell = self.decl_spelling(&sty);
                    let _ = writeln!(out, "{t}{spell} {tmp} = {subj};");
                    tmp
                };
                (format!("{sv}{acc}kind"), sv, acc)
            }
            None => (subj.clone(), subj.clone(), "."),
        };
        // Haxe `switch` has no break semantics: a `break` in a case body exits
        // the enclosing *loop*. A bare C++ `break` inside the generated switch
        // would exit only the switch — so when a case body contains a loop-bound
        // break, hoist a flag, set it at the break, and re-break after the
        // switch. (Chained when this switch sits in an outer switch's case: the
        // post-check then sets the outer flag instead of breaking bare.)
        let needs_break_flag = self.loop_depth > 0
            && (cases.iter().any(|c| stmts_contain_loop_break(&c.body))
                || default.is_some_and(stmts_contain_loop_break));
        let break_flag = if needs_break_flag {
            let f = self.fresh("brk");
            let _ = writeln!(out, "{t}bool {f} = false;");
            Some(f)
        } else {
            None
        };
        let outer_flag = std::mem::replace(&mut self.switch_break_flag, break_flag.clone());
        let _ = writeln!(out, "{t}switch ({switch_subj}) {{");
        for case in cases {
            for pat in &case.patterns {
                // enum case labels need the enum-qualified constant; a
                // destructuring pattern labels with its variant's tag
                let label = match (&adt, pat) {
                    (Some((info, _)), Expr::Call(callee, _)) => {
                        self.enum_constant(info, &call_pattern_variant(callee))
                    }
                    _ => self.case_label(pat, &sty),
                };
                let _ = writeln!(out, "{t}\tcase {label}:");
            }
            let _ = writeln!(out, "{t}\t{{");
            self.push_scope();
            // Destructuring bindings: `case Add(a, b):` declares one typed local
            // per non-`_` capture, read from the variant's payload fields.
            // (Validation guarantees a destructuring pattern is its case's only
            // pattern, so the bindings are unambiguous.)
            if let Some((_, e)) = &adt {
                if let (1, Some(Expr::Call(callee, pargs))) =
                    (case.patterns.len(), case.patterns.first())
                {
                    let vname = call_pattern_variant(callee);
                    if let Some(v) = e.variants.iter().find(|v| v.name == vname) {
                        for (i, parg) in pargs.iter().enumerate() {
                            let Expr::Ident(bind) = parg else { continue };
                            if bind == "_" {
                                continue;
                            }
                            let Some(p) = v.params.get(i) else { continue };
                            let pty = p.ty.as_ref().map(|t| self.ty_of(t)).unwrap_or_default();
                            let spell = self.decl_spelling(&pty);
                            let _ = writeln!(
                                out,
                                "{t}\t\t{spell} {bind} = {sv}{acc}{vname}_{};",
                                p.name
                            );
                            self.define_local(bind, pty);
                        }
                    }
                }
            }
            for s in &case.body {
                self.gen_stmt(s, ind + 2, out);
            }
            self.pop_scope();
            let _ = writeln!(out, "{t}\t}}");
            let _ = writeln!(out, "{t}\tbreak;");
        }
        if let Some(d) = default {
            let _ = writeln!(out, "{t}\tdefault:");
            let _ = writeln!(out, "{t}\t{{");
            self.push_scope();
            for s in d {
                self.gen_stmt(s, ind + 2, out);
            }
            self.pop_scope();
            let _ = writeln!(out, "{t}\t}}");
            let _ = writeln!(out, "{t}\tbreak;");
        }
        let _ = writeln!(out, "{t}}}");
        // Re-raise a routed loop break: bare when the loop is the next enclosing
        // construct; through the outer switch's flag when this switch sits inside
        // another switch's case body (whose own post-check then breaks the loop).
        self.switch_break_flag = outer_flag;
        if let Some(f) = &break_flag {
            match &self.switch_break_flag {
                Some(of) => {
                    let _ = writeln!(out, "{t}if ({f}) {{ {of} = true; break; }}");
                }
                None => {
                    let _ = writeln!(out, "{t}if ({f}) break;");
                }
            }
        }
    }

    /// Lower a `switch` on a non-integral subject (a `String`, or a float-backed
    /// `enum abstract`) to an `if`/`else if`/`else` chain. The subject is hoisted
    /// into one local of type `spell` (so a side-effecting subject runs once), and
    /// each case's patterns become an OR-ed equality test. Haxe cases do not fall
    /// through, which the chain matches naturally.
    pub(super) fn gen_equality_switch(
        &mut self,
        subj: &str,
        spell: &str,
        cases: &[Case],
        default: Option<&[Stmt]>,
        ind: usize,
        out: &mut String,
    ) {
        let t = "\t".repeat(ind);
        let sw = self.fresh("sw");
        let _ = writeln!(out, "{t}{spell} {sw} = {subj};");
        let mut started = false;
        for case in cases {
            // String case patterns are constants (literals), so they hoist nothing.
            let cond = case
                .patterns
                .iter()
                .map(|p| format!("{sw} == {}", self.gen_expr(p).0))
                .collect::<Vec<_>>()
                .join(" || ");
            let kw = if started { "} else if" } else { "if" };
            let _ = writeln!(out, "{t}{kw} ({cond}) {{");
            self.push_scope();
            for s in &case.body {
                self.gen_stmt(s, ind + 1, out);
            }
            self.pop_scope();
            started = true;
        }
        match default {
            Some(d) if started => {
                let _ = writeln!(out, "{t}}} else {{");
                self.push_scope();
                for s in d {
                    self.gen_stmt(s, ind + 1, out);
                }
                self.pop_scope();
                let _ = writeln!(out, "{t}}}");
            }
            // A `default` with no preceding `case` is just an unconditional block.
            Some(d) => {
                let _ = writeln!(out, "{t}{{");
                self.push_scope();
                for s in d {
                    self.gen_stmt(s, ind + 1, out);
                }
                self.pop_scope();
                let _ = writeln!(out, "{t}}}");
            }
            None if started => {
                let _ = writeln!(out, "{t}}}");
            }
            None => {}
        }
    }

    /// Desugar a value-position `switch` into a hoisted temporary plus a statement
    /// `switch` (reusing the integer/string/enum lowering): each arm assigns its
    /// trailing value expression to the temp, and the whole thing evaluates to it.
    pub(super) fn gen_switch_expr(
        &mut self,
        subject: &Expr,
        cases: &[Case],
        default: Option<&[Stmt]>,
    ) -> (String, Ty) {
        let tmp = self.fresh("swx");
        // The temporary's type is the switch's *expected* type when the context
        // supplies one (a typed `return`, `var x:T = …`, or assignment) — that is
        // the common type the arms unify to (e.g. a base class when the arms are
        // different subclasses). Only when there is no contextual type do we fall
        // back to inferring from the first arm, which would otherwise mistype a
        // polymorphic switch as its first subclass.
        let ty = match &self.expected {
            Some(t) if !t.base.is_empty() => t.clone(),
            _ => self.switch_expr_ty(cases, default),
        };
        let spell = self.decl_spelling(&ty);
        // Rewrite each arm so its trailing value expression assigns to the temp.
        let cases2: Vec<Case> = cases
            .iter()
            .map(|c| Case {
                patterns: c.patterns.clone(),
                body: assign_last_to(&c.body, &tmp),
            })
            .collect();
        let default2: Option<Vec<Stmt>> = default.map(|d| assign_last_to(d, &tmp));

        // Build the statement switch in an isolated prelude context so its internal
        // flushing does not move unrelated, already-pending prelude into the middle.
        let saved = std::mem::take(&mut self.prelude);
        let ind = self.prelude_ind;
        let mut buf = String::new();
        self.gen_switch(subject, &cases2, default2.as_deref(), ind, &mut buf);
        // `gen_switch` flushes its own prelude into `buf`; nothing should remain.
        let leftover = std::mem::replace(&mut self.prelude, saved);
        let t = "\t".repeat(ind);
        self.prelude.push_str(&format!("{t}{spell} {tmp};\n"));
        self.prelude.push_str(&leftover);
        self.prelude.push_str(&buf);
        (tmp, ty)
    }

    /// A value-position `if`/`else`, desugared like a value `switch`: a hoisted
    /// temporary, then a statement `if` whose branches assign their trailing value
    /// to it. `else if` chains and `{ … }` blocks nest naturally.
    pub(super) fn gen_if_expr(
        &mut self,
        cond: &Expr,
        then: &Expr,
        els: Option<&Expr>,
    ) -> (String, Ty) {
        let tmp = self.fresh("ifx");
        // The temp's type is the contextual expected type when present (the common
        // type the branches unify to), else inferred from the first branch's value.
        let ty = match &self.expected {
            Some(t) if !t.base.is_empty() => t.clone(),
            _ => self.if_expr_ty(then, els),
        };
        let spell = self.decl_spelling(&ty);
        let stmt = Stmt::If {
            cond: cond.clone(),
            then: Box::new(branch_assign_to(then, &tmp)),
            els: els.map(|e| Box::new(branch_assign_to(e, &tmp))),
            line: self.current_line,
        };
        // Build the statement `if` in an isolated prelude context (mirrors
        // `gen_switch_expr`) so its flushing does not reorder pending prelude.
        let saved = std::mem::take(&mut self.prelude);
        let ind = self.prelude_ind;
        let mut buf = String::new();
        self.gen_stmt(&stmt, ind, &mut buf);
        let leftover = std::mem::replace(&mut self.prelude, saved);
        let t = "\t".repeat(ind);
        self.prelude.push_str(&format!("{t}{spell} {tmp};\n"));
        self.prelude.push_str(&leftover);
        self.prelude.push_str(&buf);
        (tmp, ty)
    }

    /// The result type of a value-position `if`: the type of the first branch's
    /// trailing value expression, inferred without emitting.
    fn if_expr_ty(&mut self, then: &Expr, els: Option<&Expr>) -> Ty {
        let value = branch_value_expr(then)
            .or_else(|| els.and_then(branch_value_expr))
            .cloned();
        match value {
            Some(e) => self.dry_ty(&e),
            None => Ty::default(),
        }
    }

    /// A value-position `{ … }` block: hoist its statements, the trailing value
    /// assigned to a temporary that the expression evaluates to.
    pub(super) fn gen_block_expr(&mut self, stmts: &[Stmt]) -> (String, Ty) {
        let tmp = self.fresh("blk");
        let ty = match &self.expected {
            Some(t) if !t.base.is_empty() => t.clone(),
            _ => match case_value_expr(stmts) {
                Some(e) => self.dry_ty(&e.clone()),
                None => Ty::default(),
            },
        };
        let spell = self.decl_spelling(&ty);
        let body = Stmt::Block(assign_last_to(stmts, &tmp));
        let saved = std::mem::take(&mut self.prelude);
        let ind = self.prelude_ind;
        let mut buf = String::new();
        self.gen_stmt(&body, ind, &mut buf);
        let leftover = std::mem::replace(&mut self.prelude, saved);
        let t = "\t".repeat(ind);
        self.prelude.push_str(&format!("{t}{spell} {tmp};\n"));
        self.prelude.push_str(&leftover);
        self.prelude.push_str(&buf);
        (tmp, ty)
    }

    /// The result type of a value-position `switch`: the type of the first arm's
    /// (or the default's) trailing value expression, inferred without emitting.
    pub(super) fn switch_expr_ty(&mut self, cases: &[Case], default: Option<&[Stmt]>) -> Ty {
        let value = cases
            .iter()
            .find_map(|c| case_value_expr(&c.body))
            .or_else(|| default.and_then(case_value_expr))
            .cloned();
        match value {
            Some(e) => self.dry_ty(&e),
            None => Ty::default(),
        }
    }

    /// Infer an expression's type without emitting any code: the throwaway
    /// generation's prelude is discarded (a value expression has no side effects we
    /// need to keep). The fresh-name counter may advance, which is harmless.
    pub(super) fn dry_ty(&mut self, e: &Expr) -> Ty {
        let saved = std::mem::take(&mut self.prelude);
        let (_, ty) = self.gen_expr(e);
        self.prelude = saved;
        ty
    }

    pub(super) fn case_label(&mut self, pat: &Expr, subj_ty: &Ty) -> String {
        if let Expr::Ident(name) = pat {
            // bare enum variant → qualify with the subject's enum type
            if let Some(info) = &subj_ty.info {
                if info.kind == TypeKind::Enum {
                    return self.enum_constant(info, name).to_string();
                }
            }
        }
        // Qualified enum variant (`EnumType.Variant`): a `case` label is always
        // the tag — even for an ADT, whose *value*-position spelling is the
        // factory call (`Op::Halt()`), which cannot label a case.
        if let Expr::Field(recv, name) = pat {
            if let Expr::Ident(tname) = &**recv {
                if let Some(info) = self
                    .prog
                    .resolve_type(std::slice::from_ref(tname), self.mi)
                    .cloned()
                {
                    if info.kind == TypeKind::Enum {
                        return self.enum_constant(&info, name);
                    }
                }
            }
        }
        self.gen_expr(pat).0
    }
}
