//! Loop, comprehension, and `Array` map/filter/sort lowering for `BodyGen`.
//! Split out of `stmt.rs`.

use super::*;

/// A lowering plan for a custom `Iterator`/`Iterable` `for` loop (see
/// [`BodyGen::iter_plan`]).
struct IterPlan {
    /// C++ expression that initialises the iterator local.
    init: String,
    /// Type of the iterator local — its spelling and whether access is `.`/`->`.
    iter_ty: Ty,
    /// Element type bound to the loop variable (`next()`'s return type).
    elem: Ty,
    /// Whether the loop owns the iterator and must `delete` it when done (true only
    /// when an `iterator()` call handed us a fresh heap pointer).
    owns_iter: bool,
}

impl<'a> BodyGen<'a> {
    /// How to drive a custom `Iterator`/`Iterable` loop: the C++ expression that
    /// initialises the iterator local, that local's type (for spelling and the
    /// `.`/`->` access operator), the element type bound to the loop variable, and
    /// whether the loop must `delete` the iterator when it finishes (true only when
    /// we allocated it — an `iterator()` call returning a heap pointer).
    fn iter_plan(&self, coll_code: &str, cty: &Ty) -> Option<IterPlan> {
        // The Haxe `Iterator` protocol: the value itself has `hasNext()`/`next()`.
        if self.has_method(cty, "hasNext") && self.has_method(cty, "next") {
            let elem = self.method_return_ty(cty, "next", &[]);
            if elem.base.is_empty() {
                return None;
            }
            return Some(IterPlan {
                init: coll_code.to_string(),
                iter_ty: cty.clone(),
                elem,
                owns_iter: false,
            });
        }
        // The `Iterable` protocol: the value has `iterator()` returning an Iterator.
        if self.has_method(cty, "iterator") {
            let iter_ty = self.method_return_ty(cty, "iterator", &[]);
            if iter_ty.base.is_empty() || !self.has_method(&iter_ty, "next") {
                return None;
            }
            let elem = self.method_return_ty(&iter_ty, "next", &[]);
            if elem.base.is_empty() {
                return None;
            }
            let recv = if cty.is_ptr { "->" } else { "." };
            let owns_iter = iter_ty.is_ptr;
            return Some(IterPlan {
                init: format!("{coll_code}{recv}iterator()"),
                iter_ty,
                elem,
                owns_iter,
            });
        }
        None
    }

    /// Whether `ty`'s class/interface *directly* declares the Iterator protocol
    /// (`hasNext`+`next`) or the Iterable protocol (`iterator`).
    fn has_iter_protocol(&self, ty: &Ty) -> bool {
        (self.has_method(ty, "hasNext") && self.has_method(ty, "next"))
            || self.has_method(ty, "iterator")
    }

    /// Whether a *base class* of `ty` declares the iteration protocol. Hatchet does
    /// not consult inherited methods for iteration, so this only sharpens the error
    /// message — it never makes such a type iterable.
    fn base_has_iter_protocol(&self, ty: &Ty) -> bool {
        let mut info = match &ty.info {
            Some(i) => i.clone(),
            None => return false,
        };
        for _ in 0..16 {
            let extends = match self.prog.type_decl(&info) {
                Some(Decl::Class(c)) => c.extends.clone(),
                _ => return false,
            };
            let Some(Type::Named { path, .. }) = extends else {
                return false;
            };
            let Some(binfo) = self.prog.resolve_type(&path, info.module_index).cloned() else {
                return false;
            };
            let bty = Ty {
                info: Some(binfo.clone()),
                ..Default::default()
            };
            if self.has_iter_protocol(&bty) {
                return true;
            }
            info = binfo;
        }
        false
    }

    /// Raise the hard error for a `for (var in ...)` over a non-iterable `cty`,
    /// distinguishing the cases Hatchet deliberately does *not* detect — a custom
    /// Iterator/Iterable reached only through a typedef alias, or inherited from a
    /// base class — from a genuinely non-iterable type. `cty` is alias-resolved.
    fn not_iterable_error(&mut self, var: &str, cty: &Ty) {
        let base = if cty.base.is_empty() {
            "an unknown type"
        } else {
            &cty.base
        };
        let detail = if self.has_iter_protocol(cty) {
            // The resolved type has the protocol, yet we reached the error — so the
            // iterated value named it through a typedef alias, which is not detected.
            "it implements the Iterator/Iterable protocol only through a typedef alias; \
             iterate the alias's underlying type directly (the protocol methods must be \
             on the iterated type itself)"
                .to_string()
        } else if self.base_has_iter_protocol(cty) {
            "it inherits the Iterator/Iterable protocol from a base class, which Hatchet \
             does not consult; declare `hasNext`/`next` (or `iterator()`) on the type itself"
                .to_string()
        } else {
            "only ranges, Array, Map, and types implementing the Iterator/Iterable protocol \
             (`hasNext`/`next`, or `iterator()`) are iterable"
                .to_string()
        };
        self.err(format!(
            "cannot iterate `for ({var} in ...)` over `{base}`: {detail}"
        ));
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
                // `@orderedMap` field: iterate the parallel key/value vectors. The
                // field carries no `std::map` value, so this must precede any
                // `gen_expr(coll)` (which would reject the whole-map use).
                if let Some(om) = self.ordered_map_ref(coll) {
                    self.flush(out);
                    let idx = self.fresh("i");
                    let vspell = self.decl_spelling(&om.val_ty);
                    let _ = writeln!(
                        out,
                        "{t}for (size_t {idx} = 0; {idx} < {}.size(); ++{idx}) {{",
                        om.keys
                    );
                    if let Some(vv) = value_var {
                        // `for (k => v in m)` binds key and value.
                        let kspell = self.decl_spelling(&om.key_ty);
                        self.define_local(var, om.key_ty.clone());
                        self.define_local(vv, om.val_ty.clone());
                        let _ = writeln!(out, "{t}\t{kspell} {var} = {}[{idx}];", om.keys);
                        let _ = writeln!(out, "{t}\t{vspell} {vv} = {}[{idx}];", om.vals);
                    } else {
                        // `for (v in m)` binds the value (Haxe iterates a map's values).
                        self.define_local(var, om.val_ty.clone());
                        let _ = writeln!(out, "{t}\t{vspell} {var} = {}[{idx}];", om.vals);
                    }
                    self.gen_block_inner(body, ind + 1, out);
                    self.emit_owned_deletes(out, ind + 1);
                    let _ = writeln!(out, "{t}}}");
                    self.pop_scope();
                    return;
                }
                let (c_raw, raw_ty) = self.gen_expr(coll);
                self.flush(out);
                // Resolve through alias typedefs (`typedef Tilesets = Array<…>`) so
                // the container/map/iterator checks see the real C++ head, not the
                // alias name; access still uses the original pointer-ness.
                let cty = self.deref_alias(&raw_ty);
                // A nullable container is a pointer — dereference it to iterate.
                let access = if raw_ty.is_ptr {
                    format!("(*{c_raw})")
                } else {
                    c_raw.clone()
                };
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
                } else if let Some(plan) = value_var
                    .is_none()
                    .then(|| self.iter_plan(&c_raw, &raw_ty))
                    .flatten()
                {
                    // A custom `Iterator` (has `hasNext`/`next`) or `Iterable` (has
                    // `iterator()`) → a `while (it.hasNext()) { v = it.next(); … }`
                    // loop. The iterator local lives in its own block; when we
                    // allocated it (`iterator()` returning a heap pointer) it is
                    // registered owned so an early `return` frees it, and deleted
                    // again on normal completion (mutually exclusive paths).
                    let it = self.fresh("it");
                    let it_spell = self.decl_spelling(&plan.iter_ty);
                    let acc = if plan.iter_ty.is_ptr { "->" } else { "." };
                    let elem_spell = self.decl_spelling(&plan.elem);
                    let _ = writeln!(out, "{t}{{");
                    // Outer scope: the iterator local, freed once *after* the loop.
                    self.push_scope();
                    let _ = writeln!(out, "{t}\t{it_spell} {it} = {};", plan.init);
                    if plan.owns_iter {
                        self.register_owned(&it);
                    }
                    let _ = writeln!(out, "{t}\twhile ({it}{acc}hasNext()) {{");
                    // Inner scope: the loop variable and any heap locals the body
                    // allocates, freed *inside* the loop on each iteration.
                    self.push_scope();
                    self.define_local(var, plan.elem.clone());
                    let _ = writeln!(out, "{t}\t\t{elem_spell} {var} = {it}{acc}next();");
                    self.gen_block_inner(body, ind + 2, out);
                    self.emit_owned_deletes(out, ind + 2);
                    self.pop_scope();
                    let _ = writeln!(out, "{t}\t}}");
                    // Free the iterator on normal loop exit (no-op unless owned). An
                    // early `return` in the body already freed it via the
                    // all-scopes delete, and never reaches here.
                    self.emit_owned_deletes(out, ind + 1);
                    self.pop_scope();
                    let _ = writeln!(out, "{t}}}");
                } else if value_var.is_some() && self.iter_plan(&c_raw, &raw_ty).is_some() {
                    // The type implements the (value-only) Iterator/Iterable
                    // protocol, but `key => value` needs the key-value protocol,
                    // which Hatchet supports only for Map and Array (index keys).
                    self.err(format!(
                        "cannot iterate `for ({var} => {} in ...)` over `{}`: a custom Iterator/Iterable yields values only (key=>value iteration needs a Map or Array)",
                        value_var.unwrap_or(""),
                        cty.base
                    ));
                } else {
                    // Not a range, vector, map, or a type exposing the iterator
                    // protocol — Hatchet would otherwise emit invalid `.size()`/`[]`
                    // access. Fail loudly instead of guessing.
                    self.not_iterable_error(var, &cty);
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
            Iterable::Coll(coll) => 'coll: {
                // `@orderedMap` field: a paired index loop over the parallel key/value
                // vectors, before any `gen_expr(coll)` (which rejects whole-map use).
                if let Some(om) = self.ordered_map_ref(coll) {
                    let idx = self.fresh("i");
                    let vspell = self.decl_spelling(&om.val_ty);
                    let mut hdr = format!(
                        "{t}for (size_t {idx} = 0; {idx} < {}.size(); ++{idx}) {{\n",
                        om.keys
                    );
                    if let Some(vv) = value_var {
                        let kspell = self.decl_spelling(&om.key_ty);
                        self.define_local(var, om.key_ty.clone());
                        self.define_local(vv, om.val_ty.clone());
                        let _ = write!(
                            hdr,
                            "{t}\t{kspell} {var} = {}[{idx}];\n{t}\t{vspell} {vv} = {}[{idx}];\n",
                            om.keys, om.vals
                        );
                    } else {
                        self.define_local(var, om.val_ty.clone());
                        let _ = writeln!(hdr, "{t}\t{vspell} {var} = {}[{idx}];", om.vals);
                    }
                    break 'coll (hdr, format!("{t}}}\n"));
                }
                let (c_raw, raw_ty) = self.gen_expr(coll);
                // Resolve through alias typedefs so the container/map/iterator checks
                // see the real C++ head (see `gen_for`); access keeps the original
                // pointer-ness.
                let cty = self.deref_alias(&raw_ty);
                // A nullable container is a pointer — dereference it to iterate.
                let access = if raw_ty.is_ptr {
                    format!("(*{c_raw})")
                } else {
                    c_raw.clone()
                };
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
                } else if let Some(plan) = value_var
                    .is_none()
                    .then(|| self.iter_plan(&c_raw, &raw_ty))
                    .flatten()
                {
                    // A custom `Iterator`/`Iterable` (see `gen_for`). A comprehension
                    // body is a pure expression — no `break`/`return` — so the heap
                    // iterator (if any) is freed by a literal `delete` baked into the
                    // loop's close rather than the owned-scope machinery.
                    let it = self.fresh("it");
                    let it_spell = self.decl_spelling(&plan.iter_ty);
                    let acc = if plan.iter_ty.is_ptr { "->" } else { "." };
                    let espell = self.decl_spelling(&plan.elem);
                    self.define_local(var, plan.elem.clone());
                    let hdr = format!(
                        "{t}{{\n{t}\t{it_spell} {it} = {init};\n{t}\twhile ({it}{acc}hasNext()) {{\n{t}\t\t{espell} {var} = {it}{acc}next();\n",
                        init = plan.init
                    );
                    let close = if plan.owns_iter {
                        format!("{t}\t}}\n{t}\tdelete {it};\n{t}}}\n")
                    } else {
                        format!("{t}\t}}\n{t}}}\n")
                    };
                    (hdr, close)
                } else if is_container_ty(&cty) {
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
                } else {
                    // Neither a map, an Array, nor a detected Iterator/Iterable —
                    // fail loudly rather than emit invalid `.size()`/`[]` access (the
                    // same `for`-statement diagnostic, including the alias / base-class
                    // hints). A `var` binding keeps the body codegen from tripping over
                    // an unresolved loop variable while the run is already failing.
                    self.define_local(var, Ty::default());
                    self.not_iterable_error(var, &cty);
                    (String::new(), String::new())
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
}
