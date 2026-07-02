//! Core statement lowering for `BodyGen` (`gen_stmt`, blocks, `try`/`catch`, loop
//! flags). Loop/comprehension/`Array`-op lowering lives in `loops.rs` and
//! `switch`/`if`-expression lowering in `control.rs`. Split out of `source.rs`.

use super::*;

impl<'a> BodyGen<'a> {
    // ---- statements ----------------------------------------------------

    /// Warn that an `@delete` tag on a value local (struct / array / map / primitive)
    /// is a silent no-op: value locals are freed automatically when their scope closes,
    /// so there is nothing for `@delete` (a heap-pointer free) to do. Mirrors the
    /// `@sink`-on-a-value-parameter warning in `decls.rs`.
    fn warn_delete_noop_on_value(&mut self, name: &str) {
        self.warn(format!(
            "`@delete` on `{name}` has no effect — it is a value local (freed \
             automatically at scope close), not an owned pointer"
        ));
    }

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
                // The struct/array/map literal initialisers handled below early-return,
                // and all bind a *value* local — so an `@delete` on them is a no-op. Warn
                // here before those returns; the general path warns for other value inits.
                if *delete
                    && (matches!(init, Some(Expr::ObjectLit(_)))
                        || matches!(init, Some(Expr::ArrayLit(v)) if !v.is_empty())
                        || matches!(init, Some(Expr::MapLit(v)) if !v.is_empty()))
                {
                    self.warn_delete_noop_on_value(name);
                }
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
                // `@delete` only frees a heap pointer; on a value local it is a no-op (the
                // value-literal forms above are already covered by their own early-return
                // guard, so this catches the remaining non-literal value initialisers).
                if *delete && !is_ptr {
                    self.warn_delete_noop_on_value(name);
                }
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
                } else if !code.trim().is_empty() {
                    let _ = writeln!(out, "{t}{code};");
                }
                // else: a void op fully expressed through its hoisted prelude (e.g.
                // an `@orderedMap` `set`) — the flushed prelude *is* the statement.
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
                // Returning `cpp.Pointer.ofArray(...).raw` hands back a raw pointer into
                // a Haxe array that does not outlive the call — it dangles (and a
                // `const`-ref array parameter won't even compile). Such a "wrap an array
                // and return its `.raw`" helper also bypasses the fixed-array copy that
                // the *inline* idiom gets. Warn and point at inlining at the use site.
                if super::expr::as_of_array_raw(super::expr::unwrap_ascription(e)).is_some() {
                    self.warn(
                        "returning `cpp.Pointer.ofArray(...).raw` yields a pointer into a Haxe \
                         array that does not outlive this function — it dangles. Inline \
                         `cpp.Pointer.ofArray(...).raw` at the use site instead (so the array's \
                         lifetime covers the pointer, and a fixed C-array field gets a copy)"
                            .to_string(),
                    );
                }
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
                    // result is heap-allocated to match. (A `Null<String>` read in
                    // value position is already dereferenced at the read site.)
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
}
