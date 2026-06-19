//! Type resolution, field/method lookups, scope management, and enum helpers for `BodyGen`. Split out of `source.rs`.

use super::*;

impl<'a> BodyGen<'a> {
    /// Constructor parameter types for `new T(...)`, resolved in T's module.
    pub(super) fn ctor_param_types(&self, ty: &Type) -> Vec<Option<Ty>> {
        let Some(info) = ty_named_info(self.prog, self.mi, ty) else {
            return Vec::new();
        };
        let cmi = info.module_index;
        let Some(Decl::Class(c)) = self.prog.type_decl(&info) else {
            return Vec::new();
        };
        match &c.ctor {
            Some(ctor) => ctor
                .params
                .iter()
                .map(|p| self.param_ty_in(p, cmi))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Per-position flags marking which constructor parameters of `ty`'s class take
    /// ownership of their argument (it is stored into a field the destructor frees).
    /// A `new` at such a position is freed by the constructed object, so it must be
    /// emitted inline rather than hoisted into a scope-owned local.
    pub(super) fn ctor_owned_params(&self, ty: &Type) -> Vec<bool> {
        let Some(info) = ty_named_info(self.prog, self.mi, ty) else {
            return Vec::new();
        };
        let Some(Decl::Class(c)) = self.prog.type_decl(&info) else {
            return Vec::new();
        };
        // Which constructor parameters take ownership of their argument comes from
        // the escape analysis, as a per-position predicate.
        let owned = crate::sema::escape::ctor_owned_params(self.prog, info.module_index, c);
        match &c.ctor {
            Some(ctor) => (0..ctor.params.len()).map(|i| owned.contains(&i)).collect(),
            None => Vec::new(),
        }
    }

    /// Element `Ty` (with `info`) from a declared `Array<T>` AST type.
    pub(super) fn elem_ast_ty(&self, ty: Option<&Type>) -> Option<Ty> {
        if let Some(Type::Named { path, params, .. }) = ty {
            if path.last().map(|s| s.as_str()) == Some("Array") && params.len() == 1 {
                return Some(self.ty_of(&params[0]));
            }
        }
        None
    }

    /// Key/value `Ty`s from a declared `Map<K,V>` AST type.
    pub(super) fn map_kv_ast_ty(&self, ty: Option<&Type>) -> (Ty, Ty) {
        if let Some(Type::Named { path, params, .. }) = ty {
            if path.last().map(|s| s.as_str()) == Some("Map") && params.len() == 2 {
                return (self.ty_of(&params[0]), self.ty_of(&params[1]));
            }
        }
        (Ty::default(), Ty::default())
    }

    /// Infer a `std::vector<T>` type from an array literal's first element.
    pub(super) fn infer_array(&mut self, elems: &[Expr]) -> Ty {
        let elem = elems
            .first()
            .map(|e| self.gen_expr(e).1)
            .unwrap_or_default();
        // discard any prelude produced while probing the element type
        self.prelude.clear();
        let inner = if elem.base.is_empty() {
            "int".to_string()
        } else {
            self.decl_spelling(&elem)
        };
        Ty {
            base: format!("std::vector<{inner} >"),
            ..Default::default()
        }
    }

    /// Key and value `Ty`s of a `std::map<K, V>` from its base spelling,
    /// recovering struct `info` for the value type where possible.
    pub(super) fn map_kv_ty(&self, map: &Ty) -> (Ty, Ty) {
        if let Some(inner) = map
            .base
            .strip_prefix("std::map<")
            .and_then(|s| s.strip_suffix(">"))
        {
            if let Some((k, v)) = split_top_comma(inner.trim()) {
                let key = Ty {
                    base: k.trim().to_string(),
                    ..Default::default()
                };
                let v = v.trim();
                let is_ptr = v.ends_with('*');
                let base = v.trim_end_matches('*').trim().to_string();
                let bare = base.rsplit("::").next().unwrap_or(&base);
                let info = self.prog.resolve_type_by_cpp(bare, self.mi).cloned();
                return (key, Ty { base, is_ptr, info, ..Default::default() });
            }
        }
        (Ty::default(), Ty::default())
    }

    /// Value `Ty` of a `std::map<K, V>` from its base spelling.
    pub(super) fn map_value_ty(&self, map: &Ty) -> Ty {
        if let Some(inner) = map
            .base
            .strip_prefix("std::map<")
            .and_then(|s| s.strip_suffix(">"))
        {
            if let Some((_, v)) = split_top_comma(inner.trim()) {
                let v = v.trim();
                let is_ptr = v.ends_with('*');
                return Ty {
                    base: v.trim_end_matches('*').trim().to_string(),
                    is_ptr,
                    ..Default::default()
                };
            }
        }
        Ty::default()
    }

    pub(super) fn map_key_ty(&self, map: &Ty) -> Ty {
        if let Some(inner) = map
            .base
            .strip_prefix("std::map<")
            .and_then(|s| s.strip_suffix(">"))
        {
            if let Some((k, _)) = split_top_comma(inner.trim()) {
                let k = k.trim();
                let is_ptr = k.ends_with('*');
                return Ty {
                    base: k.trim_end_matches('*').trim().to_string(),
                    is_ptr,
                    ..Default::default()
                };
            }
        }
        Ty::default()
    }

    // ---- scope / locals ------------------------------------------------

    pub(super) fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
        self.renames.push(HashMap::new());
        self.owned.push(Vec::new());
    }
    pub(super) fn pop_scope(&mut self) {
        self.scopes.pop();
        self.renames.pop();
        self.owned.pop();
    }
    pub(super) fn define_local(&mut self, name: &str, ty: Ty) {
        // A local shadowing a container parameter takes over the name — its
        // mutations are local copies, not parameter mutations.
        self.container_params.remove(name);
        self.scopes.last_mut().unwrap().insert(name.to_string(), ty);
    }
    pub(super) fn lookup_local(&self, name: &str) -> Option<Ty> {
        for s in self.scopes.iter().rev() {
            if let Some(t) = s.get(name) {
                return Some(t.clone());
            }
        }
        None
    }

    /// Choose the C++ identifier for a new local named `haxe`. Haxe permits a
    /// local to shadow a parameter or outer local; C++ does not at function
    /// scope, so a colliding name is renamed (`x` → `x_2`) and the mapping is
    /// recorded so later references resolve to the renamed identifier.
    pub(super) fn bind_local_name(&mut self, haxe: &str) -> String {
        if self.lookup_local(haxe).is_none() {
            return haxe.to_string();
        }
        let mut i = 2;
        let cpp = loop {
            let cand = format!("{haxe}_{i}");
            if self.lookup_local(&cand).is_none() {
                break cand;
            }
            i += 1;
        };
        self.renames
            .last_mut()
            .unwrap()
            .insert(haxe.to_string(), cpp.clone());
        cpp
    }

    /// Resolve a Haxe local name to its (possibly renamed) C++ identifier.
    pub(super) fn cpp_name(&self, haxe: &str) -> String {
        for r in self.renames.iter().rev() {
            if let Some(c) = r.get(haxe) {
                return c.clone();
            }
        }
        haxe.to_string()
    }
    pub(super) fn fresh(&mut self, hint: &str) -> String {
        self.tmp += 1;
        format!("_{hint}{}", self.tmp)
    }

    /// Allocate a unique C++ name for a counter-style loop control variable (a
    /// `for`-init `int`) and register the rename so the loop body resolves the
    /// Haxe name to it. VC6 uses the pre-standard `for` scope rule, where a
    /// `for (int i ...)` init variable leaks into the *enclosing* block; two
    /// loops (or comprehensions) reusing the same Haxe name in one function would
    /// then redeclare `i` (`error C2374`). A fresh name per loop sidesteps that
    /// with no change in behaviour. (Element/iterator bindings declared inside the
    /// loop braces are already block-scoped, so only the `for`-init needs this.)
    pub(super) fn loop_var(&mut self, haxe: &str) -> String {
        let cpp = self.fresh(haxe);
        self.renames
            .last_mut()
            .unwrap()
            .insert(haxe.to_string(), cpp.clone());
        cpp
    }

    // ---- type helpers --------------------------------------------------

    pub(super) fn ty_of(&self, ht: &Type) -> Ty {
        self.ty_of_in(ht, self.mi)
    }

    /// Like `ty_of`, but resolve the type in the context of module `ctx` (used
    /// for a callee's parameter types, which must resolve where they were
    /// declared — e.g. `Line` inside `native.api` is the native `native::Line`).
    /// The C++ spelling is still relative to the current namespace.
    pub(super) fn ty_of_in(&self, ht: &Type, ctx: usize) -> Ty {
        if let Type::Named { path, params, .. } = ht {
            if path.last().map(|s| s.as_str()) == Some("Null") && params.len() == 1 {
                // Nullable value type → pointer (keep the inner type's info for
                // member access; a reference type stays a single pointer). Only a
                // genuine value→pointer lowering is flagged `nullable` (a reference
                // type is already a pointer, so `Null<T>` needs no special care).
                let mut inner = self.ty_of_in(&params[0], ctx);
                let was_value = !inner.is_ptr;
                inner.is_ptr = true;
                inner.nullable = was_value;
                return inner;
            }
            // `cpp.Pointer<T>` → `T*`, carrying `T`'s info so member access on the
            // result resolves (method lookup, parameter types) and dispatches `->`.
            if path.last().map(|s| s.as_str()) == Some("Pointer") && params.len() == 1 {
                let mut inner = self.ty_of_in(&params[0], ctx);
                inner.is_ptr = true;
                return inner;
            }
        }
        let base_use = self.prog.map_type_use(ht, ctx, &self.ns);
        let is_ptr = base_use.ends_with('*');
        let base = base_use.trim_end_matches('*').to_string();
        let info = match ht {
            Type::Named { path, params, .. } if params.is_empty() => {
                self.prog.resolve_type(path, ctx).cloned()
            }
            _ => None,
        };
        Ty { base, is_ptr, info, nullable: false, unsigned: false, iter: None }
    }

    /// The `Ty` of a callee parameter, folding in optionality. `param_decl`
    /// lowers an *optional* value-struct (`?x:V`) to `V* x = NULL` — the same
    /// pointer shape as a *nullable* `Null<T>`. So optionality and nullability
    /// collapse to one C++ representation: mark such a param `nullable` so call
    /// sites pass a pointer matching the signature (optional and nullable value
    /// types both lower to `T*`). Reference types are already pointers (no
    /// change); `String`/primitive/`Array`/`Map` optionals stay by-value with a
    /// default, so they are left alone.
    pub(super) fn param_ty_in(&self, p: &Param, ctx: usize) -> Option<Ty> {
        let t = p.ty.as_ref()?;
        let mut ty = self.ty_of_in(t, ctx);
        if p.optional && !ty.is_ptr && crate::codegen::is_value_struct(self.prog, ctx, t) {
            ty.is_ptr = true;
            ty.nullable = true;
        }
        Some(ty)
    }

    pub(super) fn decl_spelling(&self, ty: &Ty) -> String {
        if ty.is_ptr {
            format!("{}*", ty.base)
        } else {
            ty.base.clone()
        }
    }

    /// The return type as a value (pointer stripped), for building the temporary
    /// that a struct/array `return` populates before any heap wrapping.
    pub(super) fn return_value_ty(&self) -> Ty {
        Ty {
            is_ptr: false,
            ..self.current_ret.clone()
        }
    }

    /// Heap-wrap a value temporary when the function returns a pointer
    /// (`Null<T>` → `T*`); otherwise return it unchanged.
    pub(super) fn wrap_ret(&self, code: String) -> String {
        if self.current_ret.is_ptr {
            format!("new {}({code})", self.current_ret.base)
        } else {
            code
        }
    }

    /// The C++ for `return null` given the method's return type: `NULL` for
    /// pointers/primitives, a default-constructed value for struct returns.
    pub(super) fn return_null_value(&self) -> String {
        if self.current_ret.is_ptr {
            "NULL".to_string()
        } else if self.current_ret.info.is_some() && !self.current_ret.base.is_empty() {
            format!("{}()", self.current_ret.base)
        } else {
            "NULL".to_string()
        }
    }

    /// The C++ for `return []`/`return {}` (an empty array/map literal) given the
    /// method's return type. A pointer return (`Null<Array<T>>` → `T*`) yields
    /// `NULL`; a by-value container (`std::vector<...>`/`std::map<...>`, which
    /// carry no `TypeInfo`) yields a default-constructed empty container — you
    /// cannot `return NULL` from a function returning a container by value.
    pub(super) fn return_empty_container(&self) -> String {
        if self.current_ret.is_ptr {
            "NULL".to_string()
        } else if !self.current_ret.base.is_empty() {
            format!("{}()", self.return_value_ty().base)
        } else {
            "NULL".to_string()
        }
    }

    pub(super) fn element_ty(&self, container: &Ty) -> Ty {
        // crude: strip one std::vector<...>/std::map<...> level
        let b = &container.base;
        if let Some(inner) = b
            .strip_prefix("std::vector<")
            .and_then(|s| s.strip_suffix(">"))
        {
            let inner = inner.trim();
            let is_ptr = inner.ends_with('*');
            let base = inner.trim_end_matches('*').trim().to_string();
            // Recover the user/native type so member access on the loop variable
            // still resolves (`for (tile in tiles) tile.GetExtents()`). Resolve via
            // the C++ leaf name so a `@:native`-renamed element type is found too.
            let bare = base.rsplit("::").next().unwrap_or(&base);
            let info = self.prog.resolve_type_by_cpp(bare, self.mi).cloned();
            return Ty { base, is_ptr, info, ..Default::default() };
        }
        Ty::default()
    }

    // ---- member/accessor lookup ----------------------------------------

    pub(super) fn class_field(&self, name: &str) -> Option<&'a Field> {
        self.find_field(self.class, name)
    }

    /// The class-field name an assignment target stores into when it is an
    /// **own-field** store: `this.field`, or a bare `field` that resolves to a
    /// class field rather than a local. `obj.field` on another object is not an
    /// own-field store and yields `None`. This is what lets `field = new X()`
    /// behave identically to `this.field = new X()` for ownership/escape.
    pub(super) fn assigned_own_field(&self, target: &Expr) -> Option<String> {
        match target {
            Expr::Field(recv, field) if matches!(**recv, Expr::This) => Some(field.clone()),
            Expr::Ident(name)
                if self.lookup_local(name).is_none() && self.class_field(name).is_some() =>
            {
                Some(name.clone())
            }
            _ => None,
        }
    }

    /// Find a field in `class` or any of its base classes.
    pub(super) fn find_field(&self, class: &'a Class, name: &str) -> Option<&'a Field> {
        if let Some(f) = class.fields.iter().find(|f| f.name == name) {
            return Some(f);
        }
        if let Some(Type::Named { path, .. }) = &class.extends {
            if let Some(info) = self.prog.resolve_type(path, self.mi) {
                if let Some(Decl::Class(bc)) = self.prog.type_decl(info) {
                    return self.find_field(bc, name);
                }
            }
        }
        None
    }

    pub(super) fn field_ty(&self, f: &Field) -> Ty {
        match &f.ty {
            Some(t) => {
                let mut ty = self.ty_of(t);
                // A nullable value-struct field is stored as a pointer (primitives,
                // incl. the UInt aliases, stay values).
                if !ty.is_ptr
                    && self.is_nullable_field(&f.name)
                    && crate::codegen::is_value_struct(self.prog, self.mi, t)
                {
                    ty.is_ptr = true;
                }
                ty
            }
            None => Ty::default(),
        }
    }

    pub(super) fn is_nullable_field(&self, name: &str) -> bool {
        self.class
            .ctor
            .as_ref()
            .map(|c| c.params.iter().any(|p| p.optional && p.name == name))
            .unwrap_or(false)
    }

    /// Whether `name` is an accessor method *replaced* by a generated one (and so
    /// must not be emitted as an ordinary method). A custom accessor — `get_x`
    /// for a `(get, …)` property, or a user-written `set_x` for a `set` property —
    /// is the user's real implementation: it is emitted, so it is not matched here.
    pub(super) fn is_accessor_method(&self, name: &str) -> bool {
        self.class.fields.iter().any(|f| {
            if f.get == PropAccess::Default && f.set == PropAccess::Default {
                return false;
            }
            let suppressed_get = f.get != PropAccess::Get && format!("get_{}", f.name) == name;
            let suppressed_set = format!("set_{}", f.name) == name
                && !(f.set == PropAccess::Set
                    && self
                        .class
                        .methods
                        .iter()
                        .any(|m| m.name.as_deref() == Some(name)));
            suppressed_get || suppressed_set
        })
    }

    pub(super) fn class_method_return(&self, name: &str) -> Option<Ty> {
        let m = self
            .class
            .methods
            .iter()
            .find(|m| m.name.as_deref() == Some(name))?;
        m.ret.as_ref().map(|t| self.ty_of(t))
    }

    /// The method an external read of field `name` on `info` routes through, if
    /// any: the user's `get_x` for a custom `(get, …)` accessor, or the generated
    /// `GetX` when reads are open but the backing field is private (writes
    /// restricted or routed). `None` means direct field access.
    pub(super) fn field_getter(&self, info: &TypeInfo, name: &str) -> Option<String> {
        self.lookup_field(info, name).and_then(getter_method)
    }

    /// The method a write to field `name` on `info` routes through, if any: the
    /// user's `set_x` when one is defined (real Haxe `set` access), else the
    /// generated trivial `SetX`. `None` for non-`set` write access.
    pub(super) fn field_setter(&self, info: &TypeInfo, name: &str) -> Option<String> {
        let Some(Decl::Class(c)) = self.prog.type_decl(info) else {
            return None;
        };
        let f = self.find_field(c, name)?;
        if f.set != PropAccess::Set {
            return None;
        }
        let custom = format!("set_{name}");
        Some(if self.class_defines_method(c, &custom) {
            custom
        } else {
            format!("Set{}", cap(name))
        })
    }

    /// Does `class` (or a base) define a method named `name`?
    pub(super) fn class_defines_method(&self, class: &'a Class, name: &str) -> bool {
        if class
            .methods
            .iter()
            .any(|m| m.name.as_deref() == Some(name))
        {
            return true;
        }
        if let Some(Type::Named { path, .. }) = &class.extends {
            if let Some(info) = self.prog.resolve_type(path, self.mi) {
                if let Some(Decl::Class(bc)) = self.prog.type_decl(info) {
                    return self.class_defines_method(bc, name);
                }
            }
        }
        false
    }

    /// When an **own-field** assignment target must route through a user-written
    /// setter: the field declares `set` access, a custom `set_x` exists, and we
    /// are not already inside one of that property's accessors (where access is
    /// direct, as in Haxe). Fields with only the generated trivial `SetX` keep
    /// Hatchet's dialect behavior — internal writes stay direct.
    pub(super) fn own_field_setter(&self, target: &Expr) -> Option<String> {
        let name = match target {
            Expr::Ident(n) if self.lookup_local(n).is_none() => n,
            Expr::Field(recv, n) if matches!(&**recv, Expr::This) => n,
            _ => return None,
        };
        let f = self.class_field(name)?;
        if f.set != PropAccess::Set {
            return None;
        }
        let setter = format!("set_{name}");
        if !self.class_defines_method(self.class, &setter) {
            return None;
        }
        if self.current_fn == setter || self.current_fn == format!("get_{name}") {
            return None;
        }
        Some(name.clone())
    }

    pub(super) fn lookup_field(&self, info: &TypeInfo, name: &str) -> Option<&'a Field> {
        if let Decl::Class(c) = self.prog.type_decl(info)? {
            return self.find_field(c, name);
        }
        None
    }

    pub(super) fn accessor_field_ty(&self, info: &TypeInfo, name: &str) -> Ty {
        match self.lookup_field(info, name) {
            Some(f) => match &f.ty {
                Some(t) => self.ty_of(t),
                None => Ty::default(),
            },
            None => Ty::default(),
        }
    }

    pub(super) fn member_field_ty(&self, info: &TypeInfo, name: &str) -> Option<Ty> {
        match self.prog.type_decl(info)? {
            Decl::Class(c) => self.find_field(c, name).map(|f| self.field_ty(f)),
            Decl::Typedef(Typedef {
                target: TypedefTarget::Struct(fields),
                ..
            }) => fields
                .iter()
                .find(|f| f.name == name)
                .map(|f| self.ty_of(&f.ty)),
            _ => None,
        }
    }

    pub(super) fn method_return_ty(&self, recv: &Ty, method: &str, args: &[Expr]) -> Ty {
        let Some(info) = &recv.info else {
            return Ty::default();
        };
        let Some(decl) = self.prog.type_decl(info) else {
            return Ty::default();
        };
        let methods = match decl {
            Decl::Class(c) => &c.methods,
            Decl::Interface(i) => &i.methods,
            _ => return Ty::default(),
        };
        match methods.iter().find(|m| m.name.as_deref() == Some(method)) {
            Some(m) => {
                // An `@:overload`'d method (the canonical signature is often
                // `Dynamic`) resolves its return type from the overload that
                // matches the argument types — the C++ method is genuinely
                // overloaded, so the emitted call is unchanged.
                if let Some(t) = self.resolve_overload_ret(m, args) {
                    return t;
                }
                match &m.ret {
                    Some(t) => self.ty_of(t),
                    None => Ty::default(),
                }
            }
            None => Ty::default(),
        }
    }

    /// Whether the named method on `recv` carries `@:overload` signatures (so its
    /// call arguments need coercion to select the intended C++ overload).
    pub(super) fn method_is_overloaded(&self, recv: &Ty, method: &str) -> bool {
        let Some(info) = &recv.info else { return false };
        let Some(decl) = self.prog.type_decl(info) else {
            return false;
        };
        let methods = match decl {
            Decl::Class(c) => &c.methods,
            Decl::Interface(i) => &i.methods,
            _ => return false,
        };
        methods
            .iter()
            .find(|m| m.name.as_deref() == Some(method))
            .map(|m| m.meta.iter().any(|x| x.name == "overload"))
            .unwrap_or(false)
    }

    /// If `m` carries `@:overload(function(p:T,…):R {})` signatures, resolve the
    /// call's return type by matching the argument types against each overload's
    /// parameters. Returns `None` when there are no overloads or none match.
    pub(super) fn resolve_overload_ret(&self, m: &Function, args: &[Expr]) -> Option<Ty> {
        let arg_tys: Vec<Ty> = args.iter().map(|a| self.arg_ty(a)).collect();
        for meta in m.meta.iter().filter(|x| x.name == "overload") {
            let Some(raw) = meta.first_arg() else {
                continue;
            };
            let Some((params, ret)) = parse_overload_sig(raw) else {
                continue;
            };
            if params.len() != arg_tys.len() {
                continue;
            }
            let matches = params.iter().zip(&arg_tys).all(|(p, a)| {
                // An unknown argument type (e.g. a complex expression) is treated
                // as a wildcard; a known type must agree on the C++ base spelling.
                a.base.is_empty() || a.base == self.ty_of(&type_from_name(p)).base
            });
            if matches {
                return Some(self.ty_of(&type_from_name(&ret)));
            }
        }
        None
    }

    /// `Some(diagnostic)` when `method` on `recv` is `@:overload`'d but the call's
    /// argument types match **no** declared overload signature. Hatchet will not
    /// guess which C++ overload was intended, so the call site is a hard error.
    /// `None` when the method is not overloaded or at least one overload matches.
    pub(super) fn overload_mismatch(
        &self,
        recv: &Ty,
        method: &str,
        args: &[Expr],
    ) -> Option<String> {
        let info = recv.info.as_ref()?;
        let decl = self.prog.type_decl(info)?;
        let methods = match decl {
            Decl::Class(c) => &c.methods,
            Decl::Interface(i) => &i.methods,
            _ => return None,
        };
        let m = methods.iter().find(|m| m.name.as_deref() == Some(method))?;
        if !m.meta.iter().any(|x| x.name == "overload") {
            return None;
        }
        if self.resolve_overload_ret(m, args).is_some() {
            return None;
        }
        let arg_desc = args
            .iter()
            .map(|a| {
                let t = self.arg_ty(a);
                if t.base.is_empty() {
                    "?".to_string()
                } else {
                    t.base.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!(
            "call to overloaded method `{method}` with argument type(s) ({arg_desc}) \
             matches no @:overload signature; Hatchet will not guess the intended C++ overload \
             (add a matching @:overload or correct the call arguments)"
        ))
    }

    /// A pure (non-emitting) type of a call argument, used only for overload
    /// matching — covers the literal/identifier cases that distinguish overloads.
    pub(super) fn arg_ty(&self, e: &Expr) -> Ty {
        match e {
            Expr::Int(_) => int_ty(),
            Expr::Float(_) => float_ty(),
            Expr::Bool(_) => bool_ty(),
            Expr::Str { .. } => Ty {
                base: "std::string".into(),
                ..Default::default()
            },
            Expr::Ident(n) => self.lookup_local(n).unwrap_or_default(),
            // A *typed* cast (`cast(x, cpp.Float32)`) carries exactly the type the
            // call means to select an overload on (e.g. C++ `float` vs `double`) —
            // honour it. An untyped `cast(x)` stays a wildcard.
            Expr::Cast { ty: Some(t), .. } => self.ty_of(t),
            _ => Ty::default(),
        }
    }

    /// Resolve a bare enum-variant identifier (e.g. `CircleKind`) to its
    /// qualified C++ constant (`demo::ShapeKind_::CircleKind`) and enum type.
    /// Searches enums in scope by variant name, preferring the expected type when
    /// it is an enum (so a name shared by two enums resolves to the contextual
    /// one). Returns `None` when no enum declares the variant — the caller then
    /// treats the identifier as an ordinary unknown.
    pub(super) fn enum_variant_ref(&self, name: &str) -> Option<(String, Ty)> {
        let is_enumish = |k: TypeKind| matches!(k, TypeKind::Enum | TypeKind::EnumAbstract);
        let mut order: Vec<&TypeInfo> = Vec::new();
        if let Some(info) = self.expected.as_ref().and_then(|t| t.info.as_ref()) {
            if is_enumish(info.kind) {
                order.push(info);
            }
        }
        for info in &self.prog.types {
            if is_enumish(info.kind) {
                order.push(info);
            }
        }
        for info in order {
            if self.enum_has_variant(info, name) {
                // ADT: a bare paramless variant in value position constructs the
                // tagged value through its factory (`Op::Halt()`), not the bare
                // tag (parameterized variants are constructed via `gen_call`).
                if let Some(e) = self.adt_enum(info) {
                    let paramless = e
                        .variants
                        .iter()
                        .any(|v| v.name == name && v.params.is_empty());
                    let ty = Ty {
                        base: info.name.clone(),
                        info: Some(info.clone()),
                        ..Default::default()
                    };
                    let ctor = self.enum_value_ctor(info, name);
                    let code = if paramless { format!("{ctor}()") } else { ctor };
                    return Some((code, ty));
                }
                // A plain/Int enum member has the enum type; a non-integral
                // `enum abstract` member has the underlying type (String/Float).
                let base = if info.kind == TypeKind::EnumAbstract {
                    self.prog
                        .enum_abstract_underlying(info)
                        .map(|u| self.prog.map_type_base(&u, self.mi, &self.ns))
                        .unwrap_or_else(|| info.name.clone())
                } else {
                    info.name.clone()
                };
                let ty = Ty {
                    base,
                    info: Some(info.clone()),
                    ..Default::default()
                };
                return Some((self.enum_constant(info, name), ty));
            }
        }
        None
    }

    /// Whether the enum `info` declares a variant named `name`.
    pub(super) fn enum_has_variant(&self, info: &TypeInfo, name: &str) -> bool {
        self.enum_decl(info)
            .map(|e| e.variants.iter().any(|v| v.name == name))
            .unwrap_or(false)
    }

    /// The `Enum` declaration behind a resolved enum `TypeInfo`.
    pub(super) fn enum_decl(&self, info: &TypeInfo) -> Option<&'a Enum> {
        let m = self.prog.modules.get(info.module_index)?;
        m.file.decls.iter().find_map(|d| match d {
            Decl::Enum(e) if e.name == info.name => Some(e),
            _ => None,
        })
    }

    /// The algebraic enum behind `info`, if it is one (a plain enum with at
    /// least one parameterized variant — lowered to the tagged value class).
    pub(super) fn adt_enum(&self, info: &TypeInfo) -> Option<&'a Enum> {
        if info.kind != TypeKind::Enum {
            return None;
        }
        self.enum_decl(info).filter(|e| e.is_adt())
    }

    /// The namespaced factory spelling for an ADT variant (`demo::Op::Add` —
    /// the value class, not the `Op_::Add` tag).
    pub(super) fn enum_value_ctor(&self, info: &TypeInfo, variant: &str) -> String {
        let ns = info.cpp_namespace();
        let prefix = if ns == self.ns || ns.is_empty() {
            String::new()
        } else {
            format!("{}::", ns.join("::"))
        };
        format!("{prefix}{}::{variant}", info.cpp_name())
    }

    pub(super) fn enum_constant(&self, info: &TypeInfo, variant: &str) -> String {
        let ns = info.cpp_namespace();
        let prefix = if ns == self.ns || ns.is_empty() {
            String::new()
        } else {
            format!("{}::", ns.join("::"))
        };
        format!("{prefix}{}_::{variant}", info.cpp_name())
    }
}
