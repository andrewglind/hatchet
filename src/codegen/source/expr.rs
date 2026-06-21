//! Expression, call, argument, and lvalue/assignment lowering for `BodyGen`. Split out of `source.rs`.

use super::*;

impl<'a> BodyGen<'a> {
    // ---- expression entry points ---------------------------------------

    /// Generate a statement-level expression, handling assignment to property
    /// accessors (`a.x = v` → `a->SetX(v)`).
    /// `arr[i] = v` where `arr` lowers to a `std::vector`. Haxe array writes
    /// auto-extend the array — a write past the end grows it, default-filling the
    /// gap — whereas C++ `operator[]` is out-of-bounds UB there. Emit a grow-guard
    /// that resizes first, evaluating the index exactly once. Returns `None` for a
    /// non-vector receiver (a map inserts on write; anything else uses the normal
    /// assignment path), having pushed nothing.
    pub(super) fn try_array_index_assign(
        &mut self,
        recv: &Expr,
        idx: &Expr,
        value: &Expr,
    ) -> Option<(String, Ty)> {
        let (rcode, rty) = self.gen_expr(recv);
        if !rty.base.starts_with("std::vector") {
            return None;
        }
        self.warn_if_param_container_mutated(recv, "[i] = …");
        let access = if rty.is_ptr && is_container_ty(&rty) {
            format!("(*{rcode})")
        } else {
            rcode
        };
        let (icode, _) = self.gen_expr(idx);
        let ix = self.fresh("ix");
        let t = "\t".repeat(self.prelude_ind);
        self.prelude
            .push_str(&format!("{t}size_t {ix} = (size_t)({icode});\n"));
        self.prelude.push_str(&format!(
            "{t}if ({ix} >= {access}.size()) {access}.resize({ix} + 1);\n"
        ));
        let ety = self.element_ty(&rty);
        // `arr[i] = []` clears the (now-present) element container.
        if matches!(value, Expr::ArrayLit(v) if v.is_empty()) {
            return Some((format!("{access}[{ix}].clear()"), Ty::default()));
        }
        // `arr[i] = { ... }` builds the struct into a temp, then assigns it.
        if let Expr::ObjectLit(fields) = value {
            if ety.info.is_some() && !ety.base.is_empty() {
                let tmp = self.hoist_object(fields, ety.clone());
                return Some((format!("{access}[{ix}] = {tmp}"), ety));
            }
        }
        self.expected = Some(ety.clone());
        let (vcode, _) = self.gen_expr(value);
        self.expected = None;
        Some((format!("{access}[{ix}] = {vcode}"), ety))
    }

    /// Generate an assignment target. A store into a custom `(get, …)` property's
    /// own field is a direct (physical) write — Haxe's `null`/`never` write
    /// access — so the read-side `get_x()` routing must not apply to it. Anything
    /// that is not an own-field of a custom-getter property generates exactly as
    /// an expression.
    pub(super) fn gen_lvalue(&mut self, target: &Expr) -> (String, Ty) {
        let own = match target {
            Expr::Ident(name) if self.lookup_local(name).is_none() => Some(name),
            Expr::Field(recv, name) if matches!(&**recv, Expr::This) => Some(name),
            _ => None,
        };
        if let Some(name) = own {
            if let Some(f) = self.class_field(name) {
                if f.get == PropAccess::Get {
                    return (format!("this->{name}"), self.field_ty(f));
                }
            }
        }
        // An lvalue is the storage location, never a value read — so a `Null<String>`
        // target stays the raw pointer (the assignment writes the pointer), not the
        // dereferenced value.
        let saved = self.no_nullable_deref;
        self.no_nullable_deref = true;
        let res = self.gen_expr(target);
        self.no_nullable_deref = saved;
        res
    }

    /// Resolve a mutation target (`x += v`, `x++`, …) that must route through a
    /// property setter. Returns the setter call prefix (e.g. `this->set_x` or
    /// `rcv->SetX`), the routed *read* of the current value, and the field type —
    /// the caller wraps them as `prefix(read op v)`, Haxe's own desugaring of a
    /// compound write to a property. A side-effecting external receiver is
    /// hoisted into a temp so it is evaluated exactly once. `None` means the
    /// target is not a setter-routed property (direct C++ mutation applies).
    pub(super) fn routed_compound_write(&mut self, target: &Expr) -> Option<(String, String, Ty)> {
        // own field with a custom user setter
        if let Some(name) = self.own_field_setter(target) {
            let (read, fty) = self.gen_expr(target); // routes get_x() / direct
            return Some((format!("this->set_{name}"), read, fty));
        }
        // external property with a routed (custom or generated) setter
        if let Expr::Field(recv, name) = target {
            if matches!(&**recv, Expr::This) {
                return None;
            }
            let (rc, rty) = self.gen_expr(recv);
            let info = rty.info.clone()?;
            let setter = self.field_setter(&info, name)?;
            let r = if matches!(&**recv, Expr::Ident(_)) {
                rc
            } else {
                let tmp = self.fresh("rcv");
                let t = "\t".repeat(self.prelude_ind);
                let spell = self.decl_spelling(&rty);
                self.prelude
                    .push_str(&format!("{t}{spell} {tmp} = {rc};\n"));
                tmp
            };
            let op = if rty.is_ptr { "->" } else { "." };
            let read = match self.field_getter(&info, name) {
                Some(g) => format!("{r}{op}{g}()"),
                None => format!("{r}{op}{name}"),
            };
            let fty = self.accessor_field_ty(&info, name);
            return Some((format!("{r}{op}{setter}"), read, fty));
        }
        None
    }

    pub(super) fn gen_assign_or_expr(&mut self, e: &Expr) -> (String, Ty) {
        if let Expr::Assign {
            op: None,
            target,
            value,
        } = e
        {
            // (Re)initialising an `@orderedMap` field — `m = new Map()` / `[]` clears
            // both vectors; a map literal clears then appends each pair in order.
            // Any other whole-map assignment is rejected (no single map value exists).
            if let Some(om) = self.ordered_map_ref(target) {
                return self.gen_ordered_map_assign(&om, value);
            }
            // `arr[i] = v` into an Array (→ std::vector): Haxe auto-extends the
            // array on an out-of-range write, so emit a grow-guard first (C++
            // `operator[]` past the end is undefined behaviour). Maps and other
            // receivers fall through to the normal assignment path.
            if let Expr::Index(recv, idx) = &**target {
                if let Some(result) = self.try_array_index_assign(recv, idx, value) {
                    return result;
                }
                // not a vector: `m[k] = v` on a map inserts — a mutation too
                self.warn_if_param_container_mutated(recv, "[k] = …");
            }
            // Own-field write through a user-written setter (`this.x = v` or bare
            // `x = v` where the property declares `set` and `set_x` exists):
            // `this->set_x(v)`, exactly as Haxe routes it. Checked before the
            // `x = []`/object-literal shortcuts — those must route too.
            if let Some(name) = self.own_field_setter(target) {
                let fty = self
                    .class_field(&name)
                    .map(|f| self.field_ty(f))
                    .unwrap_or_default();
                let vcode = if let Expr::ObjectLit(fields) = &**value {
                    if fty.info.is_some() && !fty.base.is_empty() {
                        self.hoist_object(fields, fty.clone())
                    } else {
                        self.gen_expr(value).0
                    }
                } else {
                    self.expected = Some(fty.clone());
                    let v = self.gen_expr(value).0;
                    self.expected = None;
                    v
                };
                return (format!("this->set_{name}({vcode})"), fty);
            }
            // x = []  → x.clear()
            if matches!(&**value, Expr::ArrayLit(v) if v.is_empty()) {
                let (t, _) = self.gen_lvalue(target);
                return (format!("{t}.clear()"), Ty::default());
            }
            // accessor setter: a.x = v → a->SetX(v)
            if let Expr::Field(recv, field) = &**target {
                if let Some(setter) = self.accessor_set(recv, field, value) {
                    return (setter, Ty::default());
                }
            }
            // x = { ... }  → hoist a temp of x's struct type, then assign it
            if let Expr::ObjectLit(fields) = &**value {
                let (tcode, tty) = self.gen_lvalue(target);
                if tty.info.is_some() && !tty.base.is_empty() {
                    let tmp = self.hoist_object(fields, tty);
                    return (format!("{tcode} = {tmp}"), Ty::default());
                }
            }
            // plain reassignment: warn when a nullable value lands in a
            // non-nullable target.
            let (tcode, tty) = self.gen_lvalue(target);
            // The target type is the contextual hint for the RHS (e.g. an
            // `Array.map` result whose element type comes from the LHS).
            self.expected = Some(tty.clone());
            let (vcode, vty) = self.gen_expr(value);
            self.expected = None;
            if vty.nullable && !tty.nullable {
                self.warn(format!(
                    "'{tcode}' is assigned a Null<T> value but is not a `Null<T>`; nullable values should be held in a `Null<T>`"
                ));
            }
            // Assigning a value into a `Null<T>` (a heap pointer for a value `T`) heap-
            // wraps it: `p = new T(v)`. `= null` stays `p = NULL`, and assigning an
            // existing pointer (another `Null<T>`) copies the pointer. The previous
            // value, if any, was freed by the delete-before-overwrite in the statement
            // path (a `Null<T>` field is owned).
            let rhs = if tty.is_ptr
                && tty.nullable
                && !vty.is_ptr
                && !matches!(value.as_ref(), Expr::Null)
            {
                format!("new {}({vcode})", tty.base)
            } else {
                vcode
            };
            return (format!("{tcode} = {rhs}"), tty);
        }
        self.gen_expr(e)
    }

    /// If `recv.field = value` targets an external property accessor, produce the
    /// `recv->SetField(value)` (generated) or `recv->set_field(value)` (custom
    /// `set_x`) call.
    pub(super) fn accessor_set(
        &mut self,
        recv: &Expr,
        field: &str,
        value: &Expr,
    ) -> Option<String> {
        // own fields (`this.x`) are handled by the own-field routing (custom
        // setter) or assigned directly (generated trivial setter).
        if matches!(recv, Expr::This) {
            return None;
        }
        let (rcode, rty) = self.gen_expr(recv);
        let info = rty.info.clone()?;
        let setter = self.field_setter(&info, field)?;
        let (vcode, _) = self.gen_expr(value);
        let op = if rty.is_ptr { "->" } else { "." };
        Some(format!("{rcode}{op}{setter}({vcode})"))
    }

    // ---- expression generation -----------------------------------------

    /// Generate an expression, tracking nesting depth so a `Null<T>` call result
    /// can be classified as a *sink* (depth 1 — the whole value of a `var` init,
    /// assignment RHS, `return`, or a bare statement, all of which capture or
    /// auto-extract it) versus *buried* (depth > 1 — nested inside a larger
    /// expression, where its heap result has nowhere to be freed). Grouping-only
    /// wrappers are transparent so `(getEdge())` stays a sink.
    pub(super) fn gen_expr(&mut self, e: &Expr) -> (String, Ty) {
        let transparent = matches!(
            e,
            Expr::Paren(_) | Expr::Cast { .. } | Expr::TypeCheck { .. }
        );
        if !transparent {
            self.expr_depth += 1;
        }
        let r = self.gen_expr_inner(e);
        if !transparent {
            self.expr_depth -= 1;
        }
        r
    }

    pub(super) fn gen_expr_inner(&mut self, e: &Expr) -> (String, Ty) {
        match e {
            Expr::Int(s) => (
                s.clone(),
                Ty {
                    base: "int".into(),
                    ..Default::default()
                },
            ),
            Expr::Float(s) => (float_lit(s), float_ty()),
            Expr::Bool(b) => (
                b.to_string(),
                Ty {
                    base: "bool".into(),
                    ..Default::default()
                },
            ),
            Expr::Null => ("NULL".into(), Ty::default()),
            // `untyped X` — emit X verbatim; its type is opaque to Hatchet.
            Expr::Verbatim(code) => (code.clone(), Ty::default()),
            // Regex literals are flagged `Unsupported` in validation, so a module
            // using one is never generated; this arm only keeps the match total.
            Expr::Regex { .. } => {
                self.err("regular-expression literals are not supported".to_string());
                ("/* regex unsupported */".into(), Ty::default())
            }
            // The `is` operator is flagged `Unsupported` in validation, so a module
            // using one is never generated; this arm only keeps the match total.
            Expr::Is { .. } => {
                self.err("the `is` type-check operator is not supported".to_string());
                (
                    "/* is unsupported */".into(),
                    Ty {
                        base: "bool".into(),
                        ..Default::default()
                    },
                )
            }
            Expr::Switch {
                subject,
                cases,
                default,
            } => self.gen_switch_expr(subject, cases, default.as_deref()),
            Expr::If { cond, then, els } => self.gen_if_expr(cond, then, els.as_deref()),
            Expr::Block(stmts) => self.gen_block_expr(stmts),
            Expr::Str { raw, interpolated } => self.gen_string(raw, *interpolated),
            // In an `abstract` newtype's method, `this` is the underlying value,
            // held in the synthetic `this->__this` member.
            Expr::This if self.abstract_this.is_some() => {
                ("this->__this".into(), self.abstract_this.clone().unwrap())
            }
            Expr::This => (
                "this".into(),
                Ty {
                    base: self.class.name.clone(),
                    is_ptr: true,
                    info: self
                        .prog
                        .resolve_type(std::slice::from_ref(&self.class.name), self.mi)
                        .cloned(),
                    ..Default::default()
                },
            ),
            Expr::Super => ("super".into(), Ty::default()),
            Expr::Ident(name) => self.gen_ident(name),
            Expr::Paren(inner) => {
                let (c, ty) = self.gen_expr(inner);
                (format!("({c})"), ty)
            }
            Expr::Field(recv, name) => self.gen_field(recv, name),
            Expr::Index(recv, idx) => {
                let (r, rty) = self.gen_expr(recv);
                let (i, _) = self.gen_expr(idx);
                // Resolve alias typedefs so an aliased container indexes/element-types
                // correctly (the receiver code is unchanged).
                let cty = self.deref_alias(&rty);
                // A nullable container (`Null<Array<T>>`) is a pointer; index the
                // pointee, not the pointer.
                let access = if rty.is_ptr && is_container_ty(&cty) {
                    format!("(*{r})")
                } else {
                    r
                };
                (format!("{access}[{i}]"), self.element_ty(&cty))
            }
            Expr::Call(target, args) => {
                let (code, ty) = self.gen_call(target, args);
                // A `Null<T>` result produced *inside* a larger expression (depth
                // > 1) has nowhere to be stored, so the heap object the callee
                // allocated would leak. Up to the configured extraction depth,
                // Hatchet hoists the call into an owned local (freed at scope close)
                // and uses that name in place; beyond it, the call is only flagged.
                // (A bare/sink call at depth 1 is auto-extracted by the statement.)
                if ty.nullable && self.expr_depth > 1 {
                    if self.expr_depth <= self.max_extract_depth {
                        let tmp = self.fresh("null");
                        let spell = self.decl_spelling(&ty);
                        let t = "\t".repeat(self.prelude_ind);
                        self.prelude
                            .push_str(&format!("{t}{spell} {tmp} = {code};\n"));
                        self.register_owned(&tmp);
                        return (tmp, ty);
                    }
                    self.warn(format!(
                        "a Null<T> function result is used inside a larger expression (nesting depth {}), so it cannot be stored in a `Null<T>` local and freed (extract the call to its own `Null<T>` local, or raise --depth to auto-extract)",
                        self.expr_depth
                    ));
                }
                (code, ty)
            }
            Expr::New(ty, args) => {
                let base = self.prog.map_type_base(ty, self.mi, &self.ns);
                // `new Array<T>()` / `new Map<K,V>()` → a value-constructed,
                // empty container (Haxe heap arrays are C++ value containers). An
                // alias typedef (`typedef Tilesets = Array<…>`) resolves to the same
                // container head, so `new Tilesets()` value-constructs `Tilesets()`
                // (a valid std::vector typedef) rather than heap-allocating.
                let nty = self.ty_of(ty);
                let resolved = self.deref_alias(&nty);
                if resolved.base.starts_with("std::vector") || resolved.base.starts_with("std::map")
                {
                    return (format!("{base}()"), nty);
                }
                // `new String(x)` → a string *value*, not a heap pointer.
                if base == "std::string" {
                    let a = self.gen_args(args);
                    return (
                        format!("std::string({a})"),
                        Ty {
                            base,
                            ..Default::default()
                        },
                    );
                }
                // `new` of a `@:stackOnly` class → a value temporary (`Foo(args)`),
                // not a heap `new Foo(args)`: value semantics, no ownership.
                if matches!(ty, Type::Named { path, .. } if self.prog.is_value_class(path, self.mi))
                {
                    let param_tys = self.ctor_param_types(ty);
                    let a = self.gen_args_typed(args, &param_tys, false);
                    return (
                        format!("{base}({a})"),
                        Ty {
                            base,
                            info: ty_named_info(self.prog, self.mi, ty),
                            ..Default::default()
                        },
                    );
                }
                let param_tys = self.ctor_param_types(ty);
                let owned = self.ctor_owned_params(ty);
                let a = self.gen_args_owned(args, &param_tys, &owned, false);
                (
                    format!("new {base}({a})"),
                    Ty {
                        base,
                        is_ptr: true,
                        info: ty_named_info(self.prog, self.mi, ty),
                        ..Default::default()
                    },
                )
            }
            Expr::Unary { op, expr, prefix } => {
                // `++`/`--` on a setter-routed property desugar to a setter call,
                // as in Haxe: `x++` → `set_x(read + 1)`. (The expression's value
                // is the setter's return — the *new* value — so a postfix use in
                // value position differs from Haxe's old-value result; in
                // statement position, the overwhelmingly common case, the value
                // is discarded and the semantics match.)
                if matches!(op, UnOp::Incr | UnOp::Decr) {
                    if let Some((w, read, fty)) = self.routed_compound_write(expr) {
                        let delta = if matches!(op, UnOp::Incr) {
                            "+ 1"
                        } else {
                            "- 1"
                        };
                        return (format!("{w}({read} {delta})"), fty);
                    }
                }
                // `++`/`--` mutate their operand — an lvalue, so a custom-getter
                // property's own field is the direct (physical) store.
                let (c, ty) = if matches!(op, UnOp::Incr | UnOp::Decr) {
                    self.gen_lvalue(expr)
                } else {
                    self.gen_expr(expr)
                };
                let o = unop(*op);
                if *prefix {
                    (format!("{o}{c}"), ty)
                } else {
                    (format!("{c}{o}"), ty)
                }
            }
            Expr::Binary { op, lhs, rhs } => {
                // A null check on a `Map.get(k)` result is the iterator existence
                // check (`it == map.end()` / `it != map.end()`) — handled before the
                // operands are generated, so the iterator is never dereferenced here.
                if let Some(res) = self.try_iter_null_check(*op, lhs, rhs) {
                    return res;
                }
                // In a `== null` / `!= null` comparison, read a `Null<String>` operand
                // as its raw pointer (so the check is `p != NULL`), not the value.
                let null_cmp = matches!(*op, BinOp::Eq | BinOp::Ne)
                    && (matches!(lhs.as_ref(), Expr::Null) || matches!(rhs.as_ref(), Expr::Null));
                let saved_deref = self.no_nullable_deref;
                self.no_nullable_deref = null_cmp;
                let (l, lty) = self.gen_expr(lhs);
                let (r, rty) = self.gen_expr(rhs);
                self.no_nullable_deref = saved_deref;
                // A Haxe `String` lowers to a value `std::string`, which has no null
                // state. A `Null<String>` (a pointer) compares against `NULL` as
                // usual and falls through here. A *value* `String` compared to `null`
                // has two faithful readings, kept distinct rather than guessed:
                //   * an optional `?s:String` param defaults to `""`, so a "was it
                //     passed?" check genuinely reads as `s.empty()`; and
                //   * any other value `String` is never null — the comparison is a
                //     category error, so fail loudly and steer to the real intent.
                if matches!(*op, BinOp::Eq | BinOp::Ne) {
                    let l_null = matches!(lhs.as_ref(), Expr::Null);
                    let r_null = matches!(rhs.as_ref(), Expr::Null);
                    if l_null ^ r_null {
                        let (s, sexpr, sty) = if l_null {
                            (&r, rhs.as_ref(), &rty)
                        } else {
                            (&l, lhs.as_ref(), &lty)
                        };
                        if sty.base == "std::string" && !sty.is_ptr {
                            let is_opt = matches!(sexpr, Expr::Ident(n)
                                if self.optional_string_params.contains(n));
                            if is_opt {
                                let neg = if matches!(*op, BinOp::Ne) { "!" } else { "" };
                                return (format!("{neg}{s}.empty()"), bool_ty());
                            }
                            self.err(
                                "a `String` compared to `null` is never null in Hatchet (it is a \
                                 value `std::string`, not a nullable reference). Use `!= \"\"` / \
                                 `== \"\"` to test for the empty string, or declare it \
                                 `Null<String>` for a genuinely nullable string (an optional \
                                 `?s:String` may instead take a default, e.g. `?s:String = \"…\"`)"
                                    .to_string(),
                            );
                            return (format!("{l} {} {r}", binop(*op)), bool_ty());
                        }
                    }
                }
                // String concatenation: in Haxe `+` with a `String` operand concatenates
                // (stringifying the other side). In C++ `int + "literal"` would be
                // pointer arithmetic and `std::string + int` does not compile, so build a
                // `std::string` concatenation, formatting any non-string operand.
                if matches!(*op, BinOp::Add)
                    && (lty.base == "std::string" || rty.base == "std::string")
                {
                    let lpart = self.concat_part(&l, &lty);
                    let rpart = self.concat_part(&r, &rty);
                    // `"a" + "b"` is `const char* + const char*` — anchor the left as a
                    // `std::string` so the chain is string concatenation, not pointer math.
                    let lpart = if matches!(lhs.as_ref(), Expr::Str { .. })
                        && matches!(rhs.as_ref(), Expr::Str { .. })
                    {
                        format!("std::string({lpart})")
                    } else {
                        lpart
                    };
                    return (
                        format!("{lpart} + {rpart}"),
                        Ty {
                            base: "std::string".into(),
                            ..Default::default()
                        },
                    );
                }
                // Haxe `/` always yields Float, even for Int operands; C++ `/`
                // would truncate. When both operand types are statically known
                // integers, force double division (Std.int(a / b) still truncates,
                // matching Haxe).
                if matches!(*op, BinOp::Div) && is_int_ty(&lty) && is_int_ty(&rty) {
                    return (format!("((double)({l}) / {r})"), float_ty());
                }
                // Haxe `>>>` is a 32-bit unsigned right shift; C++98 has no `>>>`,
                // so shift the value through `unsigned int` and come back to `int`.
                if matches!(*op, BinOp::UShr) {
                    return (format!("((int)((unsigned int)({l}) >> {r}))"), int_ty());
                }
                // Haxe `%` works on Floats; C++ `%` is integer-only. With a float
                // operand, lower to `fmod` (C89 <math.h>, portable to VC6).
                if matches!(*op, BinOp::Mod) && (is_float_base(&lty) || is_float_base(&rty)) {
                    return (format!("fmod({l}, {r})"), float_ty());
                }
                // A signed/unsigned comparison (e.g. a loop counter against
                // `arr.length`) warns under MSVC (C4018). Make the conversion C++
                // already performs explicit with a `(size_t)` cast on the signed side.
                let (mut l, mut r) = (l, r);
                if matches!(
                    *op,
                    BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge | BinOp::Eq | BinOp::Ne
                ) {
                    cast_signed_for_unsigned_cmp(&mut l, &lty, &mut r, &rty);
                }
                let ty = binop_result_ty(*op, lty);
                (format!("{l} {} {r}", binop(*op)), ty)
            }
            Expr::Ternary { cond, then, els } => {
                let (c, _) = self.gen_expr(cond);
                let (a, aty) = self.gen_expr(then);
                let (b, _) = self.gen_expr(els);
                (format!("{c} ? {a} : {b}"), aty)
            }
            Expr::Assign { op, target, value } => {
                // A compound write to a setter-routed property desugars as Haxe
                // does: `x op= v` → `set_x(read op v)`.
                if op.is_some() {
                    if let Some((w, read, fty)) = self.routed_compound_write(target) {
                        let (v, vty) = self.gen_expr(value);
                        let combined = match op.unwrap() {
                            BinOp::UShr => format!("(int)((unsigned int)({read}) >> {v})"),
                            BinOp::Mod if is_float_base(&fty) || is_float_base(&vty) => {
                                format!("fmod({read}, {v})")
                            }
                            o => format!("{read} {} {v}", binop(o)),
                        };
                        return (format!("{w}({combined})"), fty);
                    }
                }
                // A plain expression-position assignment to an own setter-routed
                // property routes like the statement form.
                if op.is_none() {
                    if let Some(name) = self.own_field_setter(target) {
                        let fty = self
                            .class_field(&name)
                            .map(|f| self.field_ty(f))
                            .unwrap_or_default();
                        let (v, _) = self.gen_expr(value);
                        return (format!("this->set_{name}({v})"), fty);
                    }
                }
                let (t, tty) = self.gen_lvalue(target);
                let (v, vty) = self.gen_expr(value);
                match op {
                    // `x >>>= y` — no C++ spelling; expand through the unsigned cast.
                    Some(BinOp::UShr) => (format!("{t} = (int)((unsigned int)({t}) >> {v})"), tty),
                    // `x %= y` with a float operand — C++ `%=` is integer-only.
                    Some(BinOp::Mod) if is_float_base(&tty) || is_float_base(&vty) => {
                        (format!("{t} = fmod({t}, {v})"), tty)
                    }
                    Some(o) => (format!("{t} {}= {v}", binop(*o)), tty),
                    None => (format!("{t} = {v}"), tty),
                }
            }
            Expr::NullCoalesce(a, b) => self.gen_null_coalesce(a, b),
            Expr::SafeField(recv, field) => self.gen_safe_field(recv, field),
            Expr::ArrayLit(elems) => {
                // Inline array literal → hoisted vector temporary.
                let vec_ty = self.infer_array(elems);
                let elem = self.element_ty(&vec_ty);
                let tmp = self.fresh("arr");
                let mut buf = String::new();
                self.expand_array_into_local(
                    &tmp,
                    &vec_ty,
                    &elem,
                    elems,
                    self.prelude_ind,
                    &mut buf,
                );
                self.prelude.push_str(&buf);
                (tmp, vec_ty)
            }
            Expr::MapLit(pairs) => {
                let map_ty = Ty::default();
                let tmp = self.fresh("map");
                let mut buf = String::new();
                self.expand_map_into_local(
                    &tmp,
                    &map_ty,
                    &Ty::default(),
                    pairs,
                    self.prelude_ind,
                    &mut buf,
                );
                self.prelude.push_str(&buf);
                (tmp, map_ty)
            }
            Expr::ObjectLit(fields) => {
                // Inline object literal with no contextual type → local anon struct.
                let tmp = self.fresh("obj");
                let mut buf = String::new();
                let ty = self.expand_anon_struct_local(&tmp, fields, self.prelude_ind, &mut buf);
                self.prelude.push_str(&buf);
                (tmp, ty)
            }
            Expr::Comprehension {
                var,
                value_var,
                iter,
                guard,
                body,
            } => self.gen_comprehension(var, value_var.as_deref(), iter, guard.as_deref(), body),
            Expr::Lambda { .. } => ("/* lambda */".into(), Ty::default()),
            Expr::Cast { expr, ty } => {
                let (c, cty) = self.gen_expr(expr);
                match ty {
                    Some(t) => {
                        let target = self.prog.map_type_use(t, self.mi, &self.ns);
                        (format!("(({target}) {c})"), self.ty_of(t))
                    }
                    None => (c, cty),
                }
            }
            // `(expr : Type)` is a compile-time type ascription with no runtime
            // effect — emit the inner expression unchanged, but honor the ascribed
            // type (it is exactly the hint for cases like `([] : Array<Int>)` or
            // `(null : Foo)`, where the inner expression's own type is uninformative).
            Expr::TypeCheck { expr, ty } => {
                let (c, _) = self.gen_expr(expr);
                (c, self.ty_of(ty))
            }
        }
    }

    /// Bind a `var x = map.get(k)` local as a map-iterator alias: emit
    /// `std::map<K,V>::iterator it = map.find(k);` and record `x → (it, map, V)`.
    /// Returns `false` (so the caller falls back to the generic var path) if the
    /// receiver does not actually resolve to a map.
    pub(super) fn try_bind_map_iter(
        &mut self,
        name: &str,
        declared: Option<&Ty>,
        map_expr: &Expr,
        key: &Expr,
        ind: usize,
        out: &mut String,
    ) -> bool {
        let (map_code, map_ty) = self.gen_expr(map_expr);
        if !rcode_is_map(&map_ty) {
            return false;
        }
        let key_code = self.gen_expr(key).0;
        // The value type `V` of `it->second`: prefer the declared local type (it
        // carries the resolved TypeInfo for member access), else the map's value.
        let value_ty = match declared {
            Some(t) if t.info.is_some() => t.clone(),
            _ => self.map_value_ty(&map_ty),
        };
        let it = self.fresh("it");
        let t = "\t".repeat(ind);
        self.flush(out);
        let _ = writeln!(
            out,
            "{t}{}::iterator {it} = {map_code}.find({key_code});",
            map_ty.base
        );
        let alias = IterAlias {
            it_name: it,
            map_code,
            value_ty: value_ty.clone(),
        };
        let mut local_ty = value_ty;
        local_ty.iter = Some(Box::new(alias));
        self.define_local(name, local_ty);
        true
    }

    /// A null comparison against a `Map.get(k)` alias → the iterator existence
    /// check: `x == null` → `it == map.end()`, `x != null` → `it != map.end()`.
    /// `None` for any other comparison.
    pub(super) fn try_iter_null_check(
        &self,
        op: BinOp,
        lhs: &Expr,
        rhs: &Expr,
    ) -> Option<(String, Ty)> {
        if !matches!(op, BinOp::Eq | BinOp::Ne) {
            return None;
        }
        let l_null = matches!(lhs, Expr::Null);
        let r_null = matches!(rhs, Expr::Null);
        if l_null == r_null {
            return None; // need exactly one side to be `null`
        }
        let other = if l_null { rhs } else { lhs };
        let Expr::Ident(n) = other else { return None };
        let ty = self.lookup_local(n)?;
        let alias = ty.iter.as_ref()?;
        let cmp = if matches!(op, BinOp::Ne) { "!=" } else { "==" };
        Some((
            format!("{} {cmp} {}.end()", alias.it_name, alias.map_code),
            Ty {
                base: "bool".into(),
                ..Default::default()
            },
        ))
    }

    pub(super) fn gen_ident(&mut self, name: &str) -> (String, Ty) {
        if let Some(ty) = self.lookup_local(name) {
            // A `Map.get(k)` alias: any value/member use is the dereferenced
            // iterator (`it->second`); a null check is handled in the `Binary` arm.
            if let Some(alias) = &ty.iter {
                return (format!("{}->second", alias.it_name), alias.value_ty.clone());
            }
            let code = self.cpp_name(name);
            return self.read_nullable_string(code, ty);
        }
        // implicit `this` field?
        if let Some(f) = self.class_field(name) {
            let ty = self.field_ty(f);
            // A custom `(get, …)` property reads through its accessor, exactly as
            // in Haxe — except inside that accessor itself, where the backing
            // field is accessed directly.
            if f.get == PropAccess::Get && self.current_fn != format!("get_{name}") {
                return (format!("this->get_{name}()"), ty);
            }
            return self.read_nullable_string(format!("this->{name}"), ty);
        }
        // a type name (for static / enum access)?
        if let Some(info) = self
            .prog
            .resolve_type(&[name.to_string()], self.mi)
            .cloned()
        {
            return (
                name.to_string(),
                Ty {
                    base: name.to_string(),
                    info: Some(info),
                    ..Default::default()
                },
            );
        }
        // A global `final` constant (`static const` inside its namespace, or a
        // `@:native` const from the C++ engine): namespace-qualify the reference
        // when it is used from a different namespace — e.g. `native::MAX_CHARACTERS`,
        // or `game::ALIENBEACH_SCENE_ID` inside a global-scope `extern "C"` export.
        if let Some(qref) = self.prog.global_final_ref(name, self.mi, &self.ns) {
            return (qref, Ty::default());
        }
        // A bare enum variant in expression position (`return CircleKind`,
        // `kind = RectKind`): qualify it with its enum's C++ type, mirroring the
        // `switch`-case path (`demo::ShapeKind_::CircleKind`). Without this the
        // raw `CircleKind` is undeclared in C++ (the constant lives inside the
        // enum's `struct E_`).
        if let Some((qref, ty)) = self.enum_variant_ref(name) {
            return (qref, ty);
        }
        // Reference to the value of a non-binding catch (`catch (...)` from an
        // untyped/`Dynamic` catch): C++ cannot bind the value, so this can't be
        // lowered. Fail loudly instead of emitting an undeclared identifier. (Only
        // reached when `name` resolved to nothing else — a shadowing local would
        // have been found above.)
        if self.nonbinding_catch_vars.iter().any(|n| n == name) {
            self.err(format!(
                "the value `{name}` of an untyped/`Dynamic` catch cannot be used: C++ `catch (...)` \
                 does not bind the exception — give the catch a concrete type (e.g. `catch ({name}:String)`)"
            ));
            return (name.to_string(), Ty::default());
        }
        // free function / global / unknown — pass through
        (name.to_string(), Ty::default())
    }

    /// If `e` accesses an `@orderedMap` field, resolve it to its two parallel-vector
    /// accessors and key/value types. Handles `this.field`, a bare own `field`, and
    /// `recv.field` on another object. Generating the receiver code can hoist a
    /// prelude; when this turns out not to be an ordered map, that is rolled back so
    /// the normal path regenerates the receiver cleanly.
    pub(super) fn ordered_map_ref(&mut self, e: &Expr) -> Option<OrderedMapRef> {
        let saved = self.prelude.len();
        let (prefix, info, name): (String, Option<TypeInfo>, String) = match e {
            Expr::Field(recv, name) => {
                let (rcode, rty) = self.gen_expr(recv);
                let op = if rty.is_ptr { "->" } else { "." };
                (format!("{rcode}{op}"), rty.info, name.clone())
            }
            Expr::Ident(name) if self.lookup_local(name).is_none() => {
                if let Some(at) = self.abstract_this.clone() {
                    ("this->__this->".to_string(), at.info, name.clone())
                } else {
                    let info = self
                        .prog
                        .resolve_type(std::slice::from_ref(&self.class.name), self.mi)
                        .cloned();
                    ("this->".to_string(), info, name.clone())
                }
            }
            _ => return None,
        };
        let kv = info
            .as_ref()
            .and_then(|i| self.lookup_field(i, &name))
            .and_then(|f| crate::codegen::ordered_map_kv(f).map(|(k, v)| (k.clone(), v.clone())));
        let Some((kty, vty)) = kv else {
            self.prelude.truncate(saved);
            return None;
        };
        Some(OrderedMapRef {
            keys: format!("{prefix}{name}_keys"),
            vals: format!("{prefix}{name}_vals"),
            key_ty: self.ty_of(&kty),
            val_ty: self.ty_of(&vty),
        })
    }

    /// Lower an assignment to an `@orderedMap` field. An empty initialiser
    /// (`new Map()` / `[]` / `[ ]` map literal) clears both vectors; a non-empty
    /// map literal clears then appends each `k => v` pair in order. Assigning any
    /// other whole-map value is a hard error (the field has no single map object).
    fn gen_ordered_map_assign(&mut self, om: &OrderedMapRef, value: &Expr) -> (String, Ty) {
        let is_empty_init = matches!(value, Expr::ArrayLit(v) if v.is_empty())
            || matches!(value, Expr::MapLit(v) if v.is_empty())
            || matches!(value, Expr::New(Type::Named { path, .. }, _)
                if path.last().map(|s| s.as_str()) == Some("Map"));
        if is_empty_init {
            return (
                format!("{}.clear(), {}.clear()", om.keys, om.vals),
                Ty::default(),
            );
        }
        if let Expr::MapLit(pairs) = value {
            let t = "\t".repeat(self.prelude_ind);
            let mut pre = String::new();
            let _ = writeln!(pre, "{t}{}.clear();", om.keys);
            let _ = writeln!(pre, "{t}{}.clear();", om.vals);
            for (k, v) in pairs {
                let kc = self.gen_expr(k).0;
                let vc = self.gen_expr(v).0;
                let _ = writeln!(pre, "{t}{}.push_back({kc});", om.keys);
                let _ = writeln!(pre, "{t}{}.push_back({vc});", om.vals);
            }
            self.prelude.push_str(&pre);
            return (String::new(), Ty::default());
        }
        self.err(
            "an @orderedMap field can only be assigned an empty map (`new Map()` / `[]`) or a \
             map literal; a whole map value cannot be assigned (the field is stored as two \
             parallel vectors, not a single map)"
                .to_string(),
        );
        (String::new(), Ty::default())
    }

    /// A `Null<T>` value (a heap pointer for a value `T`) read in *value* context:
    /// deref it, treating `NULL` as a default-constructed `T`. Member access on a
    /// nullable struct uses `->` directly, so in practice this is whole-value use of
    /// a `Null<String>` (`return name`, concatenation, comparison to a string). `code`
    /// must be side-effect-free (a field/local accessor) — it is evaluated twice.
    pub(super) fn deref_nullable(&self, code: &str, ty: &Ty) -> String {
        if ty.is_ptr && ty.nullable {
            format!("({code} != NULL ? *({code}) : {}())", ty.base)
        } else {
            code.to_string()
        }
    }

    pub(super) fn gen_field(&mut self, recv: &Expr, name: &str) -> (String, Ty) {
        // Enum constant: `EnumType.Variant` (a plain/Int enum, or a non-integral
        // `enum abstract` whose member is the underlying type).
        if let Expr::Ident(tname) = recv {
            if self.lookup_local(tname).is_none() && self.class_field(tname).is_none() {
                if let Some(info) = self
                    .prog
                    .resolve_type(std::slice::from_ref(tname), self.mi)
                    .cloned()
                {
                    if matches!(info.kind, TypeKind::Enum | TypeKind::EnumAbstract) {
                        // Qualified ADT variant (`Op.Halt` in value position) →
                        // the factory call; parameterized variants are handled
                        // by `gen_call` (and `case` labels by `case_label`).
                        if let Some(e) = self.adt_enum(&info) {
                            let paramless = e
                                .variants
                                .iter()
                                .any(|v| v.name == name && v.params.is_empty());
                            let ctor = self.enum_value_ctor(&info, name);
                            let code = if paramless { format!("{ctor}()") } else { ctor };
                            return (
                                code,
                                Ty {
                                    base: info.name.clone(),
                                    info: Some(info),
                                    ..Default::default()
                                },
                            );
                        }
                        let base = if info.kind == TypeKind::EnumAbstract {
                            self.prog
                                .enum_abstract_underlying(&info)
                                .map(|u| self.prog.map_type_base(&u, self.mi, &self.ns))
                                .unwrap_or_else(|| info.cpp_name().to_string())
                        } else {
                            info.cpp_name().to_string()
                        };
                        return (
                            self.enum_constant(&info, name),
                            Ty {
                                base,
                                info: Some(info),
                                ..Default::default()
                            },
                        );
                    }
                }
            }
        }

        // Intrinsic constants: `Math.POSITIVE_INFINITY`, etc.
        if let Expr::Ident(obj) = recv {
            if self.lookup_local(obj).is_none() && self.class_field(obj).is_none() {
                if let Some(res) = intrinsic_field(obj, name) {
                    return res;
                }
            }
        }

        // A field/property access on a freshly-constructed object —
        // `new T(...).field`. The new-expression binds looser than postfix `->`, so
        // `new T(...)->GetField()` is a parse error; and the temporary's members
        // must be reachable. Hoist the construction to a local and access the field
        // on it. The temporary is intentionally NOT freed: only the field value
        // escapes (e.g. pushed into a container), and the object's destructor would
        // free that value — so freeing the wrapper would be a use-after-free. This
        // mirrors the Haxe (GC) semantics where the wrapper is collected but the
        // referenced value lives on.
        let (rcode, rty) = match recv {
            Expr::New(ty, args) if !self.value_new(ty) => self.hoist_new_receiver(ty, args),
            _ => self.gen_expr(recv),
        };

        // An `@orderedMap` field reached here is being used as a *whole value* — every
        // supported use (get/set/exists/remove/keys, iteration, init) is intercepted
        // earlier. There is no single map object to read, so fail loudly.
        if let Some(info) = &rty.info {
            if self
                .lookup_field(info, name)
                .is_some_and(|f| crate::codegen::ordered_map_kv(f).is_some())
            {
                self.err(format!(
                    "`{name}` is an @orderedMap field and has no single map value: use it via \
                     `get`/`set`/`exists`/`remove`/`keys` or a `for` loop, not as a whole value \
                     (it cannot be passed, returned, or assigned as a map)"
                ));
                return (format!("{rcode}->{name}"), Ty::default());
            }
        }

        // Haxe `.length` → `.size()` (Array/Map) or `.length()` (String). A nullable
        // container is a pointer (`Null<Array<T>>`), so it must be dereferenced.
        if name == "length" {
            let cty = self.deref_alias(&rty);
            if is_container_ty(&cty) {
                if rty.is_ptr {
                    return (format!("(*{rcode}).size()"), size_ty());
                }
                return (format!("{rcode}.size()"), size_ty());
            }
            if cty.base == "std::string" {
                return (format!("{rcode}.length()"), size_ty());
            }
        }

        // Haxe `"A".code` → the first character's int value (usually a single-char
        // literal, but any string works — the first byte's code).
        if name == "code" && rty.base == "std::string" {
            return (format!("((int)(unsigned char)({rcode})[0])"), int_ty());
        }

        let op = if rty.is_ptr { "->" } else { "." };

        // External property accessor read: `obj.x` → `obj->GetX()` (generated)
        // or `obj->get_x()` (custom `(get, …)` accessor).
        if !matches!(recv, Expr::This) {
            if let Some(info) = &rty.info {
                if let Some(g) = self.field_getter(info, name) {
                    let fty = self.accessor_field_ty(info, name);
                    return (format!("{rcode}{op}{g}()"), fty);
                }
            }
        }

        // Internal read of a custom `(get, …)` property: `this.x` routes through
        // the accessor, exactly as in Haxe — except inside that accessor itself,
        // where the backing field is accessed directly.
        if matches!(recv, Expr::This) {
            if let Some(f) = self.class_field(name) {
                if f.get == PropAccess::Get && self.current_fn != format!("get_{name}") {
                    return (format!("this->get_{name}()"), self.field_ty(f));
                }
            }
        }

        // Plain field / member access
        let fty = rty
            .info
            .as_ref()
            .and_then(|info| self.member_field_ty(info, name))
            .unwrap_or_default();
        let access = format!("{rcode}{op}{name}");
        self.read_nullable_string(access, fty)
    }

    /// A `Null<String>` read in value position is dereferenced to a value
    /// `std::string` (`NULL` → `""`), so every downstream use (return, concat,
    /// comparison, ternary) just works — *unless* `no_nullable_deref` is set, which
    /// is the case while generating a `== null` operand, where the raw pointer is
    /// wanted. Other `Null<T>` reads keep the pointer (struct member access uses `->`).
    pub(super) fn read_nullable_string(&self, code: String, ty: Ty) -> (String, Ty) {
        if !self.no_nullable_deref && ty.is_ptr && ty.nullable && ty.base == "std::string" {
            let derefed = self.deref_nullable(&code, &ty);
            let value = Ty {
                is_ptr: false,
                nullable: false,
                ..ty
            };
            return (derefed, value);
        }
        (code, ty)
    }

    /// Hoist `new T(...)` (used as the receiver of a field/property access) into a
    /// fresh local and return `(localName, T*)`. The local is **not** registered as
    /// owned — see the caller in [`gen_field`] for why it must not be freed.
    pub(super) fn hoist_new_receiver(&mut self, ty: &Type, args: &[Expr]) -> (String, Ty) {
        let base = self.prog.map_type_base(ty, self.mi, &self.ns);
        let param_tys = self.ctor_param_types(ty);
        let owned = self.ctor_owned_params(ty);
        let a = self.gen_args_owned(args, &param_tys, &owned, false);
        let rty = Ty {
            base: base.clone(),
            is_ptr: true,
            info: ty_named_info(self.prog, self.mi, ty),
            ..Default::default()
        };
        let tmp = self.fresh("tmp");
        let t = "\t".repeat(self.prelude_ind);
        self.prelude
            .push_str(&format!("{t}{}* {tmp} = new {base}({a});\n", base));
        (tmp, rty)
    }

    pub(super) fn gen_safe_field(&mut self, recv: &Expr, field: &str) -> (String, Ty) {
        let (rcode, is_ptr, access, fty) = self.gen_safe_field_parts(recv, field);
        // Pointer receiver: guard against NULL. A discarded result keeps the `0` else
        // branch (the value form, when wanted, is built by `gen_null_coalesce`).
        if is_ptr {
            return (format!("({rcode} != NULL ? {access} : 0)"), Ty::default());
        }
        (access, fty)
    }

    /// The parts of a `recv?.field` access: the receiver code, whether it is a
    /// pointer (so a `NULL` guard is needed), the non-null access expression, and
    /// the field's type. A value receiver cannot be null in C++, so it is accessed
    /// directly (`is_ptr == false`).
    fn gen_safe_field_parts(&mut self, recv: &Expr, field: &str) -> (String, bool, String, Ty) {
        let (rcode, rty) = self.gen_expr(recv);
        if !rty.is_ptr {
            if let Some(info) = &rty.info {
                if let Some(g) = self.field_getter(info, field) {
                    let fty = self.accessor_field_ty(info, field);
                    return (rcode.clone(), false, format!("{rcode}.{g}()"), fty);
                }
            }
            let fty = rty
                .info
                .as_ref()
                .and_then(|i| self.member_field_ty(i, field))
                .unwrap_or_default();
            let access = format!("{rcode}.{field}");
            return (rcode, false, access, fty);
        }
        let (access, fty) = match rty.info.as_ref().and_then(|info| self.field_getter(info, field)) {
            Some(g) => {
                let fty = self.accessor_field_ty(rty.info.as_ref().unwrap(), field);
                (format!("{rcode}->{g}()"), fty)
            }
            None => {
                let fty = rty
                    .info
                    .as_ref()
                    .and_then(|i| self.member_field_ty(i, field))
                    .unwrap_or_default();
                (format!("{rcode}->{field}"), fty)
            }
        };
        (rcode, true, access, fty)
    }

    /// The parts of a `recv?.method(args)` safe call: the receiver code, whether it
    /// is a pointer (so a `NULL` guard is needed), the non-null call expression, and
    /// the call's return type.
    fn gen_safe_call_parts(
        &mut self,
        recv: &Expr,
        method: &str,
        args: &[Expr],
    ) -> (String, bool, String, Ty) {
        let (rcode, rty) = self.gen_expr(recv);
        let op = if rty.is_ptr { "->" } else { "." };
        let param_tys = self.callee_param_types(&rty, method);
        let overloaded = self.method_is_overloaded(&rty, method);
        if overloaded {
            if let Some(msg) = self.overload_mismatch(&rty, method, args) {
                self.err(msg);
            }
        }
        let sink = self.callee_sink_params(&rty, method);
        let a = self.gen_args_owned(args, &param_tys, &sink, overloaded);
        let ret = self.method_return_ty(&rty, method, args);
        let call = format!("{rcode}{op}{method}({a})");
        (rcode, rty.is_ptr, call, ret)
    }

    /// Lower `a ?? b`. When `a` is a null-safe navigation — `recv?.method(args)` or
    /// `recv?.field` — the safe-nav and the coalesce collapse into a single
    /// NULL-guarded select that yields the navigated **value**:
    /// `(recv != NULL ? recv->X : b)`. That is what the pattern means, and it keeps
    /// the discardable `(…, 0)` statement form (which always yields `0`) from leaking
    /// into a value position — the bug where `recv?.isObject() ?? false` always read
    /// `false`. Any other `a` keeps the plain pointer form `(a != NULL ? a : b)`.
    fn gen_null_coalesce(&mut self, a: &Expr, b: &Expr) -> (String, Ty) {
        let mut lhs = a;
        while let Expr::Paren(inner) = lhs {
            lhs = inner;
        }
        // `recv?.method(args) ?? b`
        if let Expr::Call(target, cargs) = lhs {
            if let Expr::SafeField(recv, method) = &**target {
                let (rcode, is_ptr, call, ret) = self.gen_safe_call_parts(recv, method, cargs);
                if !is_ptr {
                    return (call, ret); // a value receiver can never be null
                }
                let (bc, _) = self.gen_expr(b);
                return (format!("({rcode} != NULL ? {call} : {bc})"), ret);
            }
        }
        // `recv?.field ?? b`
        if let Expr::SafeField(recv, field) = lhs {
            let (rcode, is_ptr, access, fty) = self.gen_safe_field_parts(recv, field);
            if !is_ptr {
                return (access, fty);
            }
            let (bc, _) = self.gen_expr(b);
            return (format!("({rcode} != NULL ? {access} : {bc})"), fty);
        }
        let (ac, aty) = self.gen_expr(a);
        let (bc, _) = self.gen_expr(b);
        (format!("({ac} != NULL ? {ac} : {bc})"), aty)
    }

    /// If `target(args)` constructs a parameterized enum variant — a bare
    /// `Add(1, 2)` or a qualified `Op.Add(1, 2)` — emit the static factory call
    /// (`Op::Add(1, 2)`). `None` for anything that is not an ADT constructor.
    pub(super) fn try_enum_ctor_call(
        &mut self,
        target: &Expr,
        args: &[Expr],
    ) -> Option<(String, Ty)> {
        let (info, vname) = match target {
            Expr::Ident(n) if self.lookup_local(n).is_none() && self.class_field(n).is_none() => {
                // Find the ADT declaring this variant (the expected type wins).
                let mut found: Option<TypeInfo> = None;
                if let Some(i) = self.expected.as_ref().and_then(|t| t.info.as_ref()) {
                    if self.adt_enum(i).is_some() && self.enum_has_variant(i, n) {
                        found = Some(i.clone());
                    }
                }
                if found.is_none() {
                    for i in &self.prog.types {
                        if self.adt_enum(i).is_some() && self.enum_has_variant(i, n) {
                            found = Some(i.clone());
                            break;
                        }
                    }
                }
                (found?, n.clone())
            }
            Expr::Field(recv, n) => {
                let Expr::Ident(tname) = &**recv else {
                    return None;
                };
                if self.lookup_local(tname).is_some() || self.class_field(tname).is_some() {
                    return None;
                }
                let info = self
                    .prog
                    .resolve_type(std::slice::from_ref(tname), self.mi)?
                    .clone();
                self.adt_enum(&info)?;
                (info, n.clone())
            }
            _ => return None,
        };
        let e = self.adt_enum(&info)?;
        let v = e.variants.iter().find(|v| v.name == vname)?;
        if v.params.is_empty() {
            // a paramless variant is referenced bare, never called, in Haxe
            return None;
        }
        let a = self.gen_args(args);
        let ty = Ty {
            base: info.name.clone(),
            info: Some(info.clone()),
            ..Default::default()
        };
        Some((format!("{}({a})", self.enum_value_ctor(&info, &vname)), ty))
    }

    pub(super) fn gen_call(&mut self, target: &Expr, args: &[Expr]) -> (String, Ty) {
        // `recv?.method(args)` → NULL-guarded call (comma operator keeps it usable
        // as a discardable expression even when the method returns void).
        if let Expr::SafeField(recv, method) = target {
            let (rcode, is_ptr, call, ret) = self.gen_safe_call_parts(recv, method, args);
            if !is_ptr {
                return (call, ret);
            }
            // Discardable form: the navigated value is thrown away (the comma
            // operator keeps it usable even when the method returns void). When the
            // value is actually wanted — `recv?.m() ?? default` — `gen_null_coalesce`
            // builds the value form instead.
            return (format!("({rcode} != NULL ? ({call}, 0) : 0)"), Ty::default());
        }
        // Parameterized-enum constructor call (`Add(1, 2)` or `Op.Add(1, 2)`) →
        // the generated static factory (`Op::Add(1, 2)`), a tagged value.
        if let Some(res) = self.try_enum_ctor_call(target, args) {
            return res;
        }
        if let Expr::Field(recv, method) = target {
            // Intrinsics on Math / Std / Sys (only when not shadowed by a local).
            if let Expr::Ident(obj) = &**recv {
                if self.lookup_local(obj).is_none() && self.class_field(obj).is_none() {
                    if let Some(res) = self.intrinsic_call(obj, method, args) {
                        return res;
                    }
                }
            }
            // super.method(...) → Base::method(...)
            if matches!(**recv, Expr::Super) {
                let base = self
                    .class
                    .extends
                    .as_ref()
                    .map(|b| self.prog.map_type_base(b, self.mi, &self.ns))
                    .unwrap_or_default();
                let a = self.gen_args(args);
                return (format!("{base}::{method}({a})"), Ty::default());
            }
            // Static method call on a user class/abstract: `Type.method(args)` →
            // `NS::Type::method(args)` (scope resolution, not member access). The
            // receiver is a bare type name (not a local, field, or enum — those are
            // handled above).
            if let Expr::Ident(tname) = &**recv {
                if self.lookup_local(tname).is_none() && self.class_field(tname).is_none() {
                    if let Some(info) = self
                        .prog
                        .resolve_type(std::slice::from_ref(tname), self.mi)
                        .cloned()
                    {
                        if info.kind == TypeKind::Class {
                            let recv_ty = Ty {
                                base: info.name.clone(),
                                info: Some(info.clone()),
                                ..Default::default()
                            };
                            let param_tys = self.callee_param_types(&recv_ty, method);
                            let overloaded = self.method_is_overloaded(&recv_ty, method);
                            if overloaded {
                                if let Some(msg) = self.overload_mismatch(&recv_ty, method, args) {
                                    self.err(msg);
                                }
                            }
                            let sink = self.callee_sink_params(&recv_ty, method);
                            let a = self.gen_args_owned(args, &param_tys, &sink, overloaded);
                            let ret = self.method_return_ty(&recv_ty, method, args);
                            let ns = info.cpp_namespace();
                            let prefix = if ns == self.ns || ns.is_empty() {
                                String::new()
                            } else {
                                format!("{}::", ns.join("::"))
                            };
                            return (format!("{prefix}{}::{method}({a})", info.cpp_name()), ret);
                        }
                    }
                }
            }
            // `@orderedMap` fields carry no `std::map` value to call methods on —
            // intercept `get`/`set`/`exists`/`remove`/`keys` and lower them to scans
            // over the parallel key/value vectors before the normal dispatch.
            if let Some(om) = self.ordered_map_ref(recv) {
                if let Some(res) = self.ordered_map_call(&om, method, args) {
                    return res;
                }
            }
            let (rcode, rty) = self.gen_expr(recv);
            // Resolve through alias typedefs (`typedef Tilesets = Array<…>`) so a
            // method on an aliased container/string still dispatches to the
            // std::vector / std::map / std::string lowering. The receiver code is
            // unchanged (the C++ value already *is* that container via its typedef).
            let cty = self.deref_alias(&rty);
            // Haxe container methods → std::vector / std::map equivalents.
            if is_container_ty(&cty) {
                if is_mutating_container_method(method) {
                    self.warn_if_param_container_mutated(recv, method);
                }
                if let Some(res) = self.container_call(&rcode, &cty, method, args) {
                    return res;
                }
            }
            // Haxe String methods → std::string expressions (Tier 1).
            if cty.base == "std::string" {
                if let Some(res) = self.string_call(&rcode, method, args) {
                    return res;
                }
            }
            let op = if rty.is_ptr { "->" } else { "." };
            let param_tys = self.callee_param_types(&rty, method);
            let overloaded = self.method_is_overloaded(&rty, method);
            if overloaded {
                if let Some(msg) = self.overload_mismatch(&rty, method, args) {
                    self.err(msg);
                }
            }
            let sink = self.callee_sink_params(&rty, method);
            let a = self.gen_args_owned(args, &param_tys, &sink, overloaded);
            let ret = self.method_return_ty(&rty, method, args);
            return (format!("{rcode}{op}{method}({a})"), ret);
        }
        // Bare call: free function or own method.
        if let Expr::Ident(fname) = target {
            // `trace(...)` is the Haxe top-level trace (unless shadowed locally).
            if fname == "trace"
                && self.lookup_local(fname).is_none()
                && self.class_field(fname).is_none()
            {
                return self.gen_trace(args);
            }
            // An aliased import (`import a.b.Foo as Bar;`) calls the real name.
            let callee = self.resolve_alias(fname);
            let param_tys = self.own_method_param_types(fname);
            let sink = self.bare_sink_params(fname);
            let a = self.gen_args_owned(args, &param_tys, &sink, false);
            let ret = self
                .class_method_return(fname)
                .or_else(|| self.free_fn_return(&callee))
                .unwrap_or_default();
            return (format!("{callee}({a})"), ret);
        }
        let (tc, _) = self.gen_expr(target);
        let a = self.gen_args(args);
        (format!("{tc}({a})"), Ty::default())
    }

    /// Return type of a top-level free function — a lambda-form `final NAME = …` or
    /// a plain `function NAME(...)`. Searched across all modules (a free function may
    /// be imported from another file), so a `var x = f()` gets a typed declaration.
    pub(super) fn free_fn_return(&self, name: &str) -> Option<Ty> {
        for m in &self.prog.modules {
            for d in &m.file.decls {
                match d {
                    Decl::Global(g) if g.name == name => {
                        if let Some((_, ret, body)) = lambda_parts(g) {
                            return Some(self.resolve_lambda_ret(ret, body, g.ty.as_ref()));
                        }
                    }
                    Decl::Function(f) if f.name.as_deref() == Some(name) && f.body.is_some() => {
                        return Some(match &f.ret {
                            Some(t) => self.ty_of(t),
                            None => Ty {
                                base: "void".into(),
                                ..Default::default()
                            },
                        });
                    }
                    _ => {}
                }
            }
        }
        None
    }

    /// Resolve an identifier that may be an import alias (`import a.b.Foo as Bar;`)
    /// to the real (last-component) name; returns the name unchanged otherwise.
    pub(super) fn resolve_alias(&self, name: &str) -> String {
        for imp in &self.prog.modules[self.mi].file.imports {
            if imp.alias.as_deref() == Some(name) {
                if let Some(real) = imp.path.last() {
                    return real.clone();
                }
            }
        }
        name.to_string()
    }

    pub(super) fn gen_args(&mut self, args: &[Expr]) -> String {
        args.iter()
            .map(|a| self.gen_expr(a).0)
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Generate call arguments, hoisting anonymous struct literals to temporaries
    /// typed by the callee's parameter (an anon-struct argument → a named temp var).
    pub(super) fn gen_args_typed(
        &mut self,
        args: &[Expr],
        param_tys: &[Option<Ty>],
        coerce_str: bool,
    ) -> String {
        self.gen_args_owned(args, param_tys, &[], coerce_str)
    }

    /// As [`gen_args_typed`], plus `owned`: per-position flags marking parameters
    /// the callee takes ownership of (constructor args stored into freed fields). A
    /// `new` at an owned position is emitted inline (the callee frees it) instead of
    /// being hoisted into a scope-owned local that would double-free it.
    pub(super) fn gen_args_owned(
        &mut self,
        args: &[Expr],
        param_tys: &[Option<Ty>],
        owned: &[bool],
        coerce_str: bool,
    ) -> String {
        args.iter()
            .enumerate()
            .map(|(i, a)| {
                let target = param_tys.get(i).and_then(|t| t.clone());
                // A `Null<T>`/`Dynamic`/`{}` parameter is a pointer/`void*`; a value
                // argument is heap-allocated so the callee can own (and free) it.
                let heap = target
                    .as_ref()
                    .map(|t| t.nullable || t.base == "void*")
                    .unwrap_or(false);
                match a {
                    Expr::ObjectLit(fields) => {
                        let tgt = target.clone().unwrap_or_else(|| self.current_ret.clone());
                        if heap {
                            let value_ty = Ty {
                                is_ptr: false,
                                nullable: false,
                                ..tgt.clone()
                            };
                            let tmp = self.hoist_object(fields, value_ty);
                            let ptr_ty = Ty {
                                is_ptr: true,
                                ..tgt.clone()
                            };
                            return self.place_new_arg(format!("new {}({tmp})", tgt.base), ptr_ty);
                        }
                        self.hoist_object(fields, tgt)
                    }
                    Expr::ArrayLit(elems) if !elems.is_empty() => {
                        let vec_ty = target.clone().unwrap_or_else(|| self.infer_array(elems));
                        let elem = self.element_ty(&vec_ty);
                        let tmp = self.fresh("arr");
                        let mut buf = String::new();
                        self.expand_array_into_local(
                            &tmp,
                            &vec_ty,
                            &elem,
                            elems,
                            self.prelude_ind,
                            &mut buf,
                        );
                        self.prelude.push_str(&buf);
                        tmp
                    }
                    // A `new X(...)` argument is hoisted to an owned local (the
                    // caller frees it) unless the receiver escapes — or the callee
                    // takes ownership of this position (a `@sink` parameter, or a
                    // constructor arg stored into an owned field), in which case
                    // the receiver frees it, so it is emitted inline to avoid a
                    // double-free.
                    Expr::New(nty, _) if !self.value_new(nty) => {
                        let (code, vty) = self.gen_expr(a);
                        if owned.get(i).copied().unwrap_or(false) {
                            code
                        } else {
                            self.place_new_arg(code, vty)
                        }
                    }
                    // A scope-owned local handed to a `@sink` parameter: the
                    // callee takes ownership, so transfer it (drop the scope-close
                    // delete) rather than freeing here and dangling the callee's copy.
                    Expr::Ident(name)
                        if owned.get(i).copied().unwrap_or(false)
                            && self.lookup_local(name).is_some() =>
                    {
                        let code = self.gen_expr(a).0;
                        self.transfer_owned(name);
                        code
                    }
                    _ => {
                        let (code, vty) = self.gen_expr(a);
                        if heap && !vty.is_ptr {
                            // Null<T> → allocate T; void*/Dynamic → allocate the
                            // argument's own type (it converts to void* implicitly).
                            let t = match &target {
                                Some(t) if t.nullable => t.base.clone(),
                                _ => vty.base.clone(),
                            };
                            if !t.is_empty() {
                                let ptr_ty = Ty {
                                    base: t.clone(),
                                    is_ptr: true,
                                    ..Default::default()
                                };
                                return self.place_new_arg(format!("new {t}({code})"), ptr_ty);
                            }
                        }
                        // In an overloaded call, a bare string literal is a
                        // `const char*` and C++ prefers the `bool` overload over
                        // `std::string`; wrap it so the intended overload is chosen.
                        if coerce_str
                            && matches!(
                                a,
                                Expr::Str {
                                    interpolated: false,
                                    ..
                                }
                            )
                        {
                            return format!("std::string({code})");
                        }
                        code
                    }
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Place a freshly-allocated (`new …`) argument: hoist it into an owned local
    /// the current scope frees, unless the receiver escapes (then emit it inline,
    /// since the receiver takes ownership).
    pub(super) fn place_new_arg(&mut self, new_code: String, ty: Ty) -> String {
        if self.new_args_escape {
            return new_code;
        }
        let tmp = self.fresh("v");
        let spell = self.decl_spelling(&ty);
        let t = "\t".repeat(self.prelude_ind);
        self.prelude
            .push_str(&format!("{t}{spell} {tmp} = {new_code};\n"));
        self.register_owned(&tmp);
        tmp
    }

    /// Whether `e` is `container.push(new T(...))` / `container.insert(k, new T(...))`
    /// where `container` is one into which the `new` comes to rest in class-level
    /// storage (an owned class-field container, or a local that flows into one).
    /// Such a `new` escapes the current scope, so it must not be hoisted into a
    /// scope-owned local. Handles `field.push(...)`, `this.field.push(...)`, and
    /// `local.push(...)` (the receiver name is matched against the escape set).
    /// If `e` is `container.push(v)` / `container.insert(k, v)` where the
    /// container *retains* what is pushed beyond this scope — a class-owned
    /// container field, or a local container that escapes (returned, or stored
    /// into a field) — return the value expression `v`. Such a `v` comes to rest
    /// in the container, so it must not be freed here: a `new` is emitted inline,
    /// and an owned local has its ownership transferred to the container.
    /// Like the free [`is_value_new`], but also treats a `new` of a
    /// `@:stackOnly` class as a value construction (no heap, no ownership) — so
    /// such a `new` is never hoisted into an owned local or freed.
    pub(super) fn value_new(&self, ty: &Type) -> bool {
        // Resolve alias typedefs (`typedef Tilesets = Array<…>`) so `new Tilesets()`
        // is recognised as a value container — value-constructed, never owned/freed.
        let resolved = self.prog.resolve_alias_type(ty, self.mi);
        is_value_new(&resolved)
            || matches!(&resolved, Type::Named { path, .. } if self.prog.is_value_class(path, self.mi))
    }

    pub(super) fn push_into_retaining_container(&self, e: &'a Expr) -> Option<&'a Expr> {
        let Expr::Call(target, args) = e else {
            return None;
        };
        let Expr::Field(recv, method) = &**target else {
            return None;
        };
        let value = match (method.as_str(), args.len()) {
            ("push", 1) => &args[0],
            ("insert", 2) => &args[1],
            _ => return None,
        };
        let retains = match &**recv {
            Expr::Ident(n) => self.owned_containers.contains(n) || self.escaping.contains(n),
            Expr::Field(r, f) if matches!(**r, Expr::This) => self.owned_containers.contains(f),
            _ => false,
        };
        retains.then_some(value)
    }
}
