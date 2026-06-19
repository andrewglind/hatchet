//! Whole-program escape / ownership analysis — the principled replacement for the
//! scattered heuristics in `codegen/ownership.rs`.
//!
//! Per function it tracks where each heap `new` allocation comes to rest — its single
//! owner — and classifies it; per class it derives the fields the destructor must
//! free. The goal is sound *conservative* ownership: every allocation gets exactly one
//! owner, or is left unowned (leaked) when ambiguous, so the failure mode is a leak
//! rather than a double-free. Constructor-argument ownership is resolved
//! interprocedurally (directly and transitively through nested owning constructors),
//! a call to a same-class method that returns a fresh allocation transfers ownership
//! to the caller, and a class field whose owned object is handed back out is downgraded
//! to a leak (the aliasing/soundness guard).
//!
//! **Deliberately unsupported (conservatively leaked, never double-freed):**
//! * *General method / free-function call summaries* — a `new` passed to a call other
//!   than a constructor or a same-class owned-returning method (e.g. `obj.foo(new X())`)
//!   is treated as escaping. Resolving it needs call-site receiver type inference, which
//!   lives in codegen, not here.
//! * *Borrowed-param-then-fresh-`new`* — the divergence where the old heuristic frees a
//!   fresh `new` at the caller's scope while this pass leaks it. Safe (a leak), left to
//!   reconcile at the M5 cutover.
//!
//! Codegen still runs on the old heuristics; this module is built and validated
//! alongside them until the M5 cutover.

use crate::ast::*;
use crate::sema::Program;
use std::collections::{BTreeMap, BTreeSet};

/// A `new T(...)` heap-allocation site within one function, numbered in walk order.
pub type AllocId = u32;

/// Where an allocation comes to rest — its owner, or why it has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// Stored into a field — freed by the enclosing object's destructor.
    Field(String),
    /// Never escapes — freed at the end of its function's scope.
    Scope,
    /// Returned — ownership transfers to the caller.
    Return,
    /// Passed to a constructor parameter the callee owns — freed by *that* object's
    /// destructor (so it must be emitted inline, not freed here).
    Transferred,
    /// No single owner; left unfreed (leaked) for safety, with a reason.
    Leak(LeakReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeakReason {
    /// Passed to a call this pass cannot summarize (an unresolved or borrowing
    /// callee, a method/free function — interprocedural coverage beyond
    /// constructor arguments is future work).
    Escapes,
    /// Reaches more than one distinct sink (aliased) — unsafe to free once.
    Aliased,
}

/// Per-function ownership facts.
#[derive(Debug, Default)]
pub struct FnEscape {
    /// Classified owner of each allocation site (by walk-order id).
    pub allocs: BTreeMap<AllocId, Owner>,
    /// The local an allocation is directly bound to (`var x = new …`), if any.
    pub alloc_local: BTreeMap<AllocId, String>,
    /// Local names freed at scope close (allocations classified `Scope` bound to a
    /// named local).
    pub scope_owned: BTreeSet<String>,
}

/// Per-class ownership facts.
#[derive(Debug, Default)]
pub struct ClassEscape {
    /// Fields the destructor must free.
    pub owned_fields: BTreeSet<String>,
}

/// Analyse a class: the fields its destructor must free are the `@owned`-marked ones
/// plus every field an allocation *inferred* to come to rest in, **minus** any inferred
/// field whose pointer is handed back out of the object (the M4 soundness guard).
///
/// `@owned` is an explicit developer override and is never downgraded — the developer
/// asserts ownership, so even a handed-out `@owned` field is still freed (the
/// advisory-warning side of that override lands at M5).
pub fn analyze_class(prog: &Program, mi: usize, class: &Class) -> ClassEscape {
    let fields: BTreeSet<String> = class.fields.iter().map(|f| f.name.clone()).collect();
    let tagged = tagged_owned(class);
    let mut inferred = inferred_owned(class, &fields);
    // M4 aliasing/soundness guard: an inferred-owned field whose pointer is handed back
    // out of the object — returned, stored elsewhere, or passed to a call/constructor that
    // owns that argument — may be freed again or used after the destructor frees it, so
    // refuse to free it (leak instead).
    let handed_out = fields_handed_out(prog, mi, class, &fields);
    inferred.retain(|f| !handed_out.contains(f));
    let owned: BTreeSet<String> = tagged.union(&inferred).cloned().collect();
    ClassEscape {
        owned_fields: owned,
    }
}

/// The `@owned`-tagged field names (the explicit override set).
fn tagged_owned(class: &Class) -> BTreeSet<String> {
    class
        .fields
        .iter()
        .filter(|f| f.meta.iter().any(|m| m.name == "owned"))
        .map(|f| f.name.clone())
        .collect()
}

/// The fields an allocation is inferred to come to rest in (a `new` stored into the
/// field directly or via a local, or pushed into the field container) — *before* the M4
/// hand-out guard.
fn inferred_owned(class: &Class, fields: &BTreeSet<String>) -> BTreeSet<String> {
    let mut inferred = BTreeSet::new();
    for body in class
        .ctor
        .iter()
        .chain(class.methods.iter())
        .filter_map(|f| f.body.as_ref())
    {
        // `None` ctx: owned-field detection never depends on argument ownership, so
        // it skips the interprocedural resolution (and the recursion it could spawn);
        // return-consumption likewise never adds an owned field, so the summary is empty.
        let fe = analyze_body(fields, body, None, BTreeSet::new());
        for owner in fe.allocs.values() {
            if let Owner::Field(f) = owner {
                inferred.insert(f.clone());
            }
        }
    }
    inferred
}

/// The owned fields *without* the M4 hand-out guard (`@owned` plus inferred). Used by
/// constructor-ownership resolution, which must not recurse back into the guard (the
/// guard itself asks about constructor ownership) — and is safe to leave unguarded
/// there, since over-claiming ownership only ever produces a leak, never a double-free.
fn owned_fields_unguarded(class: &Class) -> BTreeSet<String> {
    let fields: BTreeSet<String> = class.fields.iter().map(|f| f.name.clone()).collect();
    tagged_owned(class)
        .union(&inferred_owned(class, &fields))
        .cloned()
        .collect()
}

/// Fields whose pointer value is handed back out of the object somewhere in the class —
/// read in an *escaping* position (a `return`/`throw` value, an assignment right-hand
/// side, a `var` initialiser, or a call/constructor argument) rather than merely
/// *borrowed* (used as a method or field receiver, or in a comparison). Such a field can
/// be aliased and then freed or used outside the object's lifetime, so the soundness
/// guard will not free it in the destructor.
fn fields_handed_out(
    prog: &Program,
    mi: usize,
    class: &Class,
    fields: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for f in class.ctor.iter().chain(class.methods.iter()) {
        let Some(body) = f.body.as_ref() else {
            continue;
        };
        // A bare `name` that is a parameter or a local `var` refers to that binding, not
        // a same-named field (Haxe shadowing) — so a local `texture` passed to a call is
        // not the field `texture` being handed out. Collect those names up front and
        // exclude them from bare-field detection (`this.field` stays unambiguous).
        let mut locals: BTreeSet<String> = f.params.iter().map(|p| p.name.clone()).collect();
        for st in body {
            collect_locals_stmt(st, &mut locals);
        }
        for st in body {
            scan_stmt_escapes(prog, mi, st, fields, &locals, &mut out);
        }
    }
    out
}

/// Collect the names bound by parameters' siblings — `var` declarations and `for`
/// loop variables — anywhere in a statement (over-approximating scope, which only ever
/// makes the hand-out guard *more* conservative about calling a name a field).
fn collect_locals_stmt(st: &Stmt, out: &mut BTreeSet<String>) {
    match st {
        Stmt::Var { name, .. } => {
            out.insert(name.clone());
        }
        Stmt::If { then, els, .. } => {
            collect_locals_stmt(then, out);
            if let Some(e) = els {
                collect_locals_stmt(e, out);
            }
        }
        Stmt::While { body, .. } => collect_locals_stmt(body, out),
        Stmt::For {
            var,
            value_var,
            body,
            ..
        } => {
            out.insert(var.clone());
            if let Some(v) = value_var {
                out.insert(v.clone());
            }
            collect_locals_stmt(body, out);
        }
        Stmt::Block(ss) => {
            for s in ss {
                collect_locals_stmt(s, out);
            }
        }
        Stmt::Switch { cases, default, .. } => {
            for c in cases {
                for s in &c.body {
                    collect_locals_stmt(s, out);
                }
            }
            if let Some(d) = default {
                for s in d {
                    collect_locals_stmt(s, out);
                }
            }
        }
        _ => {}
    }
}

fn scan_stmt_escapes(
    prog: &Program,
    mi: usize,
    st: &Stmt,
    fields: &BTreeSet<String>,
    locals: &BTreeSet<String>,
    out: &mut BTreeSet<String>,
) {
    match st {
        Stmt::Return(Some(e), _) | Stmt::Throw(e, _) => {
            scan_expr_escapes(prog, mi, e, true, fields, locals, out)
        }
        Stmt::Var { init: Some(e), .. } => {
            scan_expr_escapes(prog, mi, e, true, fields, locals, out)
        }
        Stmt::Expr(Expr::Assign { target, value, .. }, _) => {
            scan_expr_escapes(prog, mi, value, true, fields, locals, out);
            scan_expr_escapes(prog, mi, target, false, fields, locals, out);
        }
        Stmt::Expr(e, _) => scan_expr_escapes(prog, mi, e, false, fields, locals, out),
        Stmt::If {
            cond, then, els, ..
        } => {
            scan_expr_escapes(prog, mi, cond, false, fields, locals, out);
            scan_stmt_escapes(prog, mi, then, fields, locals, out);
            if let Some(e) = els {
                scan_stmt_escapes(prog, mi, e, fields, locals, out);
            }
        }
        Stmt::While { cond, body, .. } => {
            scan_expr_escapes(prog, mi, cond, false, fields, locals, out);
            scan_stmt_escapes(prog, mi, body, fields, locals, out);
        }
        Stmt::For { iter, body, .. } => {
            match iter {
                Iterable::Range(a, b) => {
                    scan_expr_escapes(prog, mi, a, false, fields, locals, out);
                    scan_expr_escapes(prog, mi, b, false, fields, locals, out);
                }
                Iterable::Coll(e) => scan_expr_escapes(prog, mi, e, false, fields, locals, out),
            }
            scan_stmt_escapes(prog, mi, body, fields, locals, out);
        }
        Stmt::Switch {
            subject,
            cases,
            default,
            ..
        } => {
            scan_expr_escapes(prog, mi, subject, false, fields, locals, out);
            for c in cases {
                for s in &c.body {
                    scan_stmt_escapes(prog, mi, s, fields, locals, out);
                }
            }
            if let Some(d) = default {
                for s in d {
                    scan_stmt_escapes(prog, mi, s, fields, locals, out);
                }
            }
        }
        Stmt::Block(ss) => {
            for s in ss {
                scan_stmt_escapes(prog, mi, s, fields, locals, out);
            }
        }
        _ => {}
    }
}

/// Record an own field (`this.f` or a bare `f` that is not shadowed by a local) read in
/// an `escaping` position. Receivers (`this.f.method()`, `this.f[i]`) and comparison
/// operands are borrows, so they recurse with `escaping = false`; call/constructor
/// arguments and aggregate-literal elements carry the escape inward.
fn scan_expr_escapes(
    prog: &Program,
    mi: usize,
    e: &Expr,
    escaping: bool,
    fields: &BTreeSet<String>,
    locals: &BTreeSet<String>,
    out: &mut BTreeSet<String>,
) {
    match e {
        Expr::Field(recv, name) | Expr::SafeField(recv, name) => {
            if escaping && matches!(**recv, Expr::This) && fields.contains(name) {
                out.insert(name.clone());
            }
            scan_expr_escapes(prog, mi, recv, false, fields, locals, out);
        }
        Expr::Ident(name) => {
            if escaping && fields.contains(name) && !locals.contains(name) {
                out.insert(name.clone());
            }
        }
        Expr::Index(recv, idx) => {
            scan_expr_escapes(prog, mi, recv, false, fields, locals, out);
            scan_expr_escapes(prog, mi, idx, false, fields, locals, out);
        }
        Expr::Call(target, args) => {
            scan_expr_escapes(prog, mi, target, false, fields, locals, out);
            // A method/free-function call may retain its argument, and this pass cannot
            // summarise it — so an argument conservatively escapes (the field would be
            // left owned only if the callee borrows, which we cannot prove here).
            for a in args {
                scan_expr_escapes(prog, mi, a, true, fields, locals, out);
            }
        }
        Expr::New(ty, args) => {
            // A constructor argument is handed out only if the constructed object *owns*
            // that position (it frees it). A borrowed position (`new Actor(walkbox)` where
            // Actor only stashes the pointer) leaves the field owned by this object.
            let owned_idx = resolve_class(prog, mi, ty)
                .map(|(c, cmi)| ctor_owned_indices(prog, cmi, c, &mut BTreeSet::new()))
                .unwrap_or_default();
            for (i, a) in args.iter().enumerate() {
                scan_expr_escapes(prog, mi, a, owned_idx.contains(&i), fields, locals, out);
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            scan_expr_escapes(prog, mi, lhs, false, fields, locals, out);
            scan_expr_escapes(prog, mi, rhs, false, fields, locals, out);
        }
        Expr::Unary { expr, .. } => scan_expr_escapes(prog, mi, expr, false, fields, locals, out),
        Expr::Ternary { cond, then, els } => {
            scan_expr_escapes(prog, mi, cond, false, fields, locals, out);
            scan_expr_escapes(prog, mi, then, escaping, fields, locals, out);
            scan_expr_escapes(prog, mi, els, escaping, fields, locals, out);
        }
        Expr::Assign { target, value, .. } => {
            scan_expr_escapes(prog, mi, value, true, fields, locals, out);
            scan_expr_escapes(prog, mi, target, false, fields, locals, out);
        }
        Expr::NullCoalesce(a, b) => {
            scan_expr_escapes(prog, mi, a, escaping, fields, locals, out);
            scan_expr_escapes(prog, mi, b, escaping, fields, locals, out);
        }
        Expr::ArrayLit(es) => {
            for e in es {
                scan_expr_escapes(prog, mi, e, escaping, fields, locals, out);
            }
        }
        Expr::MapLit(kvs) => {
            for (k, v) in kvs {
                scan_expr_escapes(prog, mi, k, escaping, fields, locals, out);
                scan_expr_escapes(prog, mi, v, escaping, fields, locals, out);
            }
        }
        Expr::ObjectLit(fs) => {
            for (_, v) in fs {
                scan_expr_escapes(prog, mi, v, escaping, fields, locals, out);
            }
        }
        Expr::Paren(inner) => scan_expr_escapes(prog, mi, inner, escaping, fields, locals, out),
        Expr::Cast { expr, .. } | Expr::TypeCheck { expr, .. } => {
            scan_expr_escapes(prog, mi, expr, escaping, fields, locals, out)
        }
        _ => {}
    }
}

/// The own-field an assignment target names: `this.field` or a bare `field` (an own
/// field). `obj.field` on another object yields `None`.
fn target_own_field(target: &Expr, fields: &BTreeSet<String>) -> Option<String> {
    match target {
        Expr::Field(r, f) if matches!(**r, Expr::This) => Some(f.clone()),
        Expr::Ident(n) if fields.contains(n) => Some(n.clone()),
        _ => None,
    }
}

/// Resolve a `new X(...)` type to its `Class` declaration and the module index it was
/// declared in (so types named inside *its* body resolve in their own scope).
fn resolve_class<'a>(prog: &'a Program, mi: usize, ty: &Type) -> Option<(&'a Class, usize)> {
    let Type::Named { path, .. } = ty else {
        return None;
    };
    let info = prog.resolve_type(path, mi)?;
    let cmi = info.module_index;
    match prog.type_decl(info) {
        Some(Decl::Class(c)) => Some((c, cmi)),
        _ => None,
    }
}

/// The constructor parameter indices `class` (declared in module `mi`) takes ownership
/// of — the public entry point to the cycle-guarded transitive computation, for codegen
/// to decide which `new` arguments to emit inline.
pub fn ctor_owned_params(prog: &Program, mi: usize, class: &Class) -> BTreeSet<usize> {
    ctor_owned_indices(prog, mi, class, &mut BTreeSet::new())
}

/// The constructor parameter *indices* `class`'s constructor takes ownership of —
/// the values its object (or one of its owned members) is responsible for freeing.
/// Drives the inline-vs-leak decision for `new` arguments at call sites.
///
/// A parameter is owned when it is either
/// * assigned straight into a field the destructor frees (`this.f = param`), or
/// * passed into *another* owning constructor whose result rests in an owned field
///   (`this.w = new Wrapper(param)` where `Wrapper` owns that position) — resolved
///   transitively, with `visiting` breaking constructor cycles (a cycle is treated
///   conservatively as borrowed, i.e. not owned → a safe leak, never a double-free).
fn ctor_owned_indices(
    prog: &Program,
    mi: usize,
    class: &Class,
    visiting: &mut BTreeSet<(usize, String)>,
) -> BTreeSet<usize> {
    let mut out = BTreeSet::new();
    if !visiting.insert((mi, class.name.clone())) {
        return out; // constructor cycle: conservatively borrowed.
    }
    if let Some(ctor) = class.ctor.as_ref() {
        if let Some(body) = ctor.body.as_ref() {
            let owned = owned_fields_unguarded(class);
            let fields: BTreeSet<String> = class.fields.iter().map(|f| f.name.clone()).collect();
            for (i, p) in ctor.params.iter().enumerate() {
                let direct =
                    param_dest_field(body, &p.name, &fields).is_some_and(|f| owned.contains(&f));
                if direct
                    || param_into_owning_new(prog, mi, body, &p.name, &owned, &fields, visiting)
                {
                    out.insert(i);
                }
            }
        }
    }
    visiting.remove(&(mi, class.name.clone()));
    out
}

/// The own-field a parameter is assigned straight into (`this.field = param` or the
/// bare `field = param`), searched across the (flat) statement list.
fn param_dest_field(body: &[Stmt], param: &str, fields: &BTreeSet<String>) -> Option<String> {
    for st in body {
        if let Stmt::Expr(
            Expr::Assign {
                op: None,
                target,
                value,
            },
            _,
        ) = st
        {
            if let Expr::Ident(p) = &**value {
                if p == param {
                    if let Some(f) = target_own_field(target, fields) {
                        return Some(f);
                    }
                }
            }
        }
    }
    None
}

/// Whether `param` is passed into an owning position of a `new X(...)` that comes to
/// rest in one of `class`'s owned fields (`this.owned = new X(param)`), making the
/// param transitively owned by this object through `X`. Recurses into `X`'s own
/// constructor ownership (cycle-guarded by `visiting`).
fn param_into_owning_new(
    prog: &Program,
    mi: usize,
    body: &[Stmt],
    param: &str,
    owned: &BTreeSet<String>,
    fields: &BTreeSet<String>,
    visiting: &mut BTreeSet<(usize, String)>,
) -> bool {
    for st in body {
        let Stmt::Expr(
            Expr::Assign {
                op: None,
                target,
                value,
            },
            _,
        ) = st
        else {
            continue;
        };
        let Some(field) = target_own_field(target, fields) else {
            continue;
        };
        if !owned.contains(&field) {
            continue;
        }
        let Expr::New(ty, args) = &**value else {
            continue;
        };
        let Some((xc, xmi)) = resolve_class(prog, mi, ty) else {
            continue;
        };
        let owned_idx = ctor_owned_indices(prog, xmi, xc, visiting);
        for (j, a) in args.iter().enumerate() {
            if owned_idx.contains(&j) {
                if let Expr::Ident(n) = a {
                    if n == param {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Analyse one function body. `class` is the enclosing class (its own fields let a
/// bare `field = …`/`field.push(…)` be recognised the same as `this.field`); the
/// `prog`/`mi` pair lets a `new X(arg)` resolve `X`'s constructor to decide whether
/// the argument is owned by the constructed object (interprocedural).
pub fn analyze_fn(prog: &Program, mi: usize, class: &Class, f: &Function) -> FnEscape {
    let fields: BTreeSet<String> = class.fields.iter().map(|f| f.name.clone()).collect();
    match &f.body {
        Some(body) => {
            let owned_returning = owned_returning_methods(prog, mi, class, &fields);
            analyze_body(&fields, body, Some((prog, mi)), owned_returning)
        }
        None => FnEscape::default(),
    }
}

/// The names of `class`'s methods that return a freshly-allocated value the caller
/// owns (their body has an allocation classified `Owner::Return`). Each method is
/// analysed *without* return-consumption (an empty summary) so this is a single
/// non-recursive pass — it sees a direct `return new T(...)`, which is what a
/// factory method (e.g. `Graph.GetEdge`) does. A method that returns the result of
/// *another* owned-returning call would need a fixpoint; left for later.
fn owned_returning_methods(
    prog: &Program,
    mi: usize,
    class: &Class,
    fields: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for m in &class.methods {
        let (Some(name), Some(body)) = (m.name.as_ref(), m.body.as_ref()) else {
            continue;
        };
        let fe = analyze_body(fields, body, Some((prog, mi)), BTreeSet::new());
        if fe.allocs.values().any(|o| *o == Owner::Return) {
            out.insert(name.clone());
        }
    }
    out
}

/// What feeds a sink or a local: a fresh allocation, an existing local, or nothing
/// trackable.
#[derive(Clone)]
enum Source {
    New(AllocId),
    Local(String),
    None,
}

/// A place an allocation comes to rest.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Sink {
    Field(String),
    Return,
    /// An argument to a constructor parameter the callee owns (freed by that object).
    Owned,
    /// An argument to a call this pass cannot summarise (an unresolved/borrowing
    /// callee, or a method/free function).
    Call,
}

struct Walk<'a> {
    fields: &'a BTreeSet<String>,
    /// Program + this module's index, for resolving `new X(...)` constructors.
    /// `None` disables interprocedural resolution (used when computing owned fields,
    /// which never depends on argument ownership and so avoids any recursion).
    ctx: Option<(&'a Program, usize)>,
    /// Same-class methods that return a freshly-allocated value the caller owns. A
    /// call to one of them (`GetEdge(x)`, `this.make()`) mints an allocation, so the
    /// caller can consume that ownership (`var t = make()` → scope-owned).
    owned_returning: BTreeSet<String>,
    next_id: AllocId,
    all: Vec<AllocId>,
    alloc_local: BTreeMap<AllocId, String>,
    /// `local <- source` edges (for points-to).
    assigns: Vec<(String, Source)>,
    /// `sink <- source` edges.
    sinks: Vec<(Sink, Source)>,
}

fn analyze_body(
    fields: &BTreeSet<String>,
    body: &[Stmt],
    ctx: Option<(&Program, usize)>,
    owned_returning: BTreeSet<String>,
) -> FnEscape {
    let mut w = Walk {
        fields,
        ctx,
        owned_returning,
        next_id: 0,
        all: Vec::new(),
        alloc_local: BTreeMap::new(),
        assigns: Vec::new(),
        sinks: Vec::new(),
    };
    for s in body {
        w.stmt(s);
    }
    w.finish()
}

impl<'a> Walk<'a> {
    fn fresh(&mut self) -> AllocId {
        let id = self.next_id;
        self.next_id += 1;
        self.all.push(id);
        id
    }

    /// A field an assignment/push target names, written `this.field` or bare
    /// `field` (an own field). `obj.field` on another object yields `None`.
    fn own_field(&self, recv: &Expr) -> Option<String> {
        match recv {
            Expr::Field(r, f) if matches!(**r, Expr::This) => Some(f.clone()),
            Expr::Ident(n) if self.fields.contains(n) => Some(n.clone()),
            _ => None,
        }
    }

    /// The argument positions a `new T(...)` constructor takes ownership of — empty
    /// when interprocedural resolution is disabled or the type can't be resolved (a
    /// conservative "the constructor borrows / is unknown", so the args escape).
    fn ctor_owned_args(&self, ty: &Type) -> BTreeSet<usize> {
        let Some((prog, mi)) = self.ctx else {
            return BTreeSet::new();
        };
        match resolve_class(prog, mi, ty) {
            Some((c, cmi)) => {
                let mut visiting = BTreeSet::new();
                ctor_owned_indices(prog, cmi, c, &mut visiting)
            }
            None => BTreeSet::new(),
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        match s {
            Stmt::Var {
                name,
                init: Some(e),
                ..
            } => {
                let src = self.value(e, Some(name));
                self.assigns.push((name.clone(), src));
            }
            Stmt::Expr(e, _) => self.expr_stmt(e),
            Stmt::Return(Some(e), _) => {
                let src = self.value(e, None);
                self.sinks.push((Sink::Return, src));
            }
            Stmt::If {
                cond, then, els, ..
            } => {
                let _ = self.value(cond, None);
                self.stmt(then);
                if let Some(e) = els {
                    self.stmt(e);
                }
            }
            Stmt::While { cond, body, .. } => {
                let _ = self.value(cond, None);
                self.stmt(body);
            }
            Stmt::For { iter, body, .. } => {
                match iter {
                    Iterable::Range(a, b) => {
                        let _ = self.value(a, None);
                        let _ = self.value(b, None);
                    }
                    Iterable::Coll(e) => {
                        let _ = self.value(e, None);
                    }
                }
                self.stmt(body);
            }
            Stmt::Block(ss) => {
                for s in ss {
                    self.stmt(s);
                }
            }
            Stmt::Switch {
                subject,
                cases,
                default,
                ..
            } => {
                let _ = self.value(subject, None);
                for c in cases {
                    for s in &c.body {
                        self.stmt(s);
                    }
                }
                if let Some(d) = default {
                    for s in d {
                        self.stmt(s);
                    }
                }
            }
            _ => {}
        }
    }

    fn expr_stmt(&mut self, e: &Expr) {
        match e {
            Expr::Assign {
                op: None,
                target,
                value,
            } => {
                if let Some(field) = self.own_field(target) {
                    let src = self.value(value, None);
                    self.sinks.push((Sink::Field(field), src));
                } else if let Expr::Ident(x) = &**target {
                    let src = self.value(value, None);
                    self.assigns.push((x.clone(), src));
                } else {
                    // e.g. `arr[i] = …` / `obj.f = …`: the value's allocations
                    // escape into a place we don't model — let `value` record any
                    // nested `new`s as Call sinks.
                    let src = self.value(value, None);
                    self.sinks.push((Sink::Call, src));
                }
            }
            Expr::Call(target, args) => {
                // Statement-position call: any owned value it produces is discarded.
                let _ = self.call(target, args);
            }
            _ => {
                let _ = self.value(e, None);
            }
        }
    }

    /// A method/function call. Recognises `container.push/insert(new …)`:
    /// - into an **own field** → the value comes to rest in that field (a `Field` sink);
    /// - into a **local container** → the value flows *into that local* (an assign edge),
    ///   so when the local later escapes to a field (`this.rows.push(row)`) the points-to
    ///   fixpoint carries the allocation transitively to that field.
    ///
    /// All other arguments are opaque sinks (`Call`) until interprocedural summaries land.
    ///
    /// Returns the allocation the call *produces* when it resolves to a same-class method
    /// that hands back a freshly-allocated value (`var t = make()`); `None` otherwise.
    fn call(&mut self, target: &Expr, args: &[Expr]) -> Option<AllocId> {
        if let Expr::Field(recv, method) = target {
            let pushed = match (method.as_str(), args.len()) {
                ("push", 1) => Some(&args[0]),
                ("insert", 2) => Some(&args[1]),
                _ => None,
            };
            if let Some(val) = pushed {
                if let Some(field) = self.own_field(recv) {
                    let src = self.value(val, None);
                    self.sinks.push((Sink::Field(field), src));
                    return None;
                }
                // Push into a bare local container: model it as the value flowing into
                // the local (transitive container flow). The local carries the alloc; if
                // it never escapes the alloc resolves to `Scope` (a safe leak, since
                // scope-local container *elements* aren't freed), and if it escapes to a
                // field container the alloc reaches that field.
                if let Expr::Ident(local) = &**recv {
                    let src = self.value(val, None);
                    self.assigns.push((local.clone(), src));
                    return None;
                }
            }
            let _ = self.value(recv, None);
        } else {
            let _ = self.value(target, None);
        }
        for a in args {
            let src = self.value(a, None);
            self.sinks.push((Sink::Call, src));
        }
        // A call to a same-class owned-returning method yields a fresh allocation the
        // caller now owns.
        if self.call_returns_owned(target) {
            Some(self.fresh())
        } else {
            None
        }
    }

    /// Whether a call target resolves to a same-class method known to return an owned
    /// allocation — an unqualified `make(...)` or a `this.make(...)`. A call through
    /// another object (`obj.make()`) needs receiver-type inference and is not resolved
    /// here (its result is conservatively untracked).
    fn call_returns_owned(&self, target: &Expr) -> bool {
        let name = match target {
            Expr::Ident(n) => Some(n.as_str()),
            Expr::Field(recv, m) if matches!(**recv, Expr::This) => Some(m.as_str()),
            _ => None,
        };
        name.map(|n| self.owned_returning.contains(n))
            .unwrap_or(false)
    }

    /// Evaluate an expression in a *result* position, returning what it produces.
    /// Nested `new`s in argument positions are recorded as escaping (`Call`); if
    /// `bound` is set, a top-level `new` is recorded as bound to that local.
    fn value(&mut self, e: &Expr, bound: Option<&str>) -> Source {
        match e {
            Expr::New(ty, args) if !is_value_new(ty) => {
                let id = self.fresh();
                if let Some(b) = bound {
                    self.alloc_local.insert(id, b.to_string());
                }
                // An argument the constructor owns is freed by the new object; one it
                // borrows (or that goes to an unresolved callee) escapes.
                let owned = self.ctor_owned_args(ty);
                for (i, a) in args.iter().enumerate() {
                    let src = self.value(a, None);
                    let sink = if owned.contains(&i) {
                        Sink::Owned
                    } else {
                        Sink::Call
                    };
                    self.sinks.push((sink, src));
                }
                Source::New(id)
            }
            Expr::New(_, args) => {
                // value `new` (Array/Map/String) — its arguments are plain values.
                for a in args {
                    let _ = self.value(a, None);
                }
                Source::None
            }
            Expr::Ident(x) => Source::Local(x.clone()),
            Expr::Paren(inner) => self.value(inner, bound),
            Expr::Cast { expr, .. } => self.value(expr, bound),
            Expr::Call(target, args) => match self.call(target, args) {
                Some(id) => {
                    if let Some(b) = bound {
                        self.alloc_local.insert(id, b.to_string());
                    }
                    Source::New(id)
                }
                None => Source::None,
            },
            Expr::Field(recv, _) | Expr::SafeField(recv, _) => {
                let _ = self.value(recv, None);
                Source::None
            }
            Expr::Index(recv, idx) => {
                let _ = self.value(recv, None);
                let _ = self.value(idx, None);
                Source::None
            }
            Expr::Binary { lhs, rhs, .. } => {
                // A binary operator yields a fresh value (bool/number) and never carries
                // a pointer operand forward, so a `new`/owned-call result used only as an
                // operand (`GetEdge(x) == null`) is a discardable temporary — `Scope`,
                // freeable after the statement — not an escape. Walk the operands to mint
                // any nested allocations, but do not sink them.
                let _ = self.value(lhs, None);
                let _ = self.value(rhs, None);
                Source::None
            }
            Expr::Unary { expr, .. } => {
                let s = self.value(expr, None);
                self.sink_if_new(s);
                Source::None
            }
            Expr::ObjectLit(fields) => {
                for (_, v) in fields {
                    let s = self.value(v, None);
                    self.sink_if_new(s);
                }
                Source::None
            }
            _ => Source::None,
        }
    }

    /// An allocation produced in a position with no resting place escapes.
    fn sink_if_new(&mut self, src: Source) {
        if let Source::New(_) = src {
            self.sinks.push((Sink::Call, src));
        }
    }

    fn finish(self) -> FnEscape {
        // 1. Points-to: resolve `local <- source` to a fixpoint.
        let mut pt: BTreeMap<String, BTreeSet<AllocId>> = BTreeMap::new();
        loop {
            let mut changed = false;
            for (local, src) in &self.assigns {
                let add = resolve(src, &pt);
                let e = pt.entry(local.clone()).or_default();
                for id in add {
                    if e.insert(id) {
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }

        // 2. Each allocation's set of distinct sinks.
        let mut per_alloc: BTreeMap<AllocId, BTreeSet<Sink>> = BTreeMap::new();
        for (sink, src) in &self.sinks {
            for id in resolve(src, &pt) {
                per_alloc.entry(id).or_default().insert(sink.clone());
            }
        }

        // 3. Classify.
        let mut allocs = BTreeMap::new();
        let mut scope_owned = BTreeSet::new();
        for id in &self.all {
            let owner = match per_alloc.get(id) {
                None => Owner::Scope,
                Some(s) if s.len() == 1 => match s.iter().next().unwrap() {
                    Sink::Field(f) => Owner::Field(f.clone()),
                    Sink::Return => Owner::Return,
                    Sink::Owned => Owner::Transferred,
                    Sink::Call => Owner::Leak(LeakReason::Escapes),
                },
                Some(_) => Owner::Leak(LeakReason::Aliased),
            };
            if owner == Owner::Scope {
                if let Some(name) = self.alloc_local.get(id) {
                    scope_owned.insert(name.clone());
                }
            }
            allocs.insert(*id, owner);
        }

        FnEscape {
            allocs,
            alloc_local: self.alloc_local,
            scope_owned,
        }
    }
}

fn resolve(src: &Source, pt: &BTreeMap<String, BTreeSet<AllocId>>) -> BTreeSet<AllocId> {
    match src {
        Source::New(id) => std::iter::once(*id).collect(),
        Source::Local(y) => pt.get(y).cloned().unwrap_or_default(),
        Source::None => BTreeSet::new(),
    }
}

/// A `new Array<T>()` / `new Map<K,V>()` / `new String(x)` lowers to a value
/// container or string — not a heap pointer the analysis tracks.
fn is_value_new(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Named { path, .. }
            if matches!(
                path.last().map(|s| s.as_str()),
                Some("Array") | Some("Map") | Some("String")
            )
    )
}

// ---- container escape: which push receivers a `new` comes to rest in ------------
//
// Decides, per `recv.push(new T())` / `recv.insert(k, new T())`, whether the pushed
// allocation escapes the current scope into class-level storage — so codegen emits it
// inline rather than hoisting it into a scope-owned local that would be freed at scope
// close, leaving a dangling pointer in the container. Returns the receiver NAMES that
// escape: owned class-field containers (keyed bare, `this.` stripped) plus any local
// that transitively flows into one (`tile.push(new …); this.tiles.push(tile)`).

/// What flows into a container via `push`/`insert`: a fresh `new`, or another name.
#[derive(Clone)]
enum PushFlow {
    New,
    Name(String),
}

/// The container receiver names into which a pushed `new` comes to rest in class-level
/// storage. (Was `codegen::ownership::escaping_new_receivers`; moved here at the M5
/// cutover so every ownership decision lives in this module.)
pub fn escaping_push_receivers(prog: &Program, mi: usize, class: &Class) -> BTreeSet<String> {
    let ns = prog.modules[mi].package.clone();
    let fields: BTreeSet<String> = class.fields.iter().map(|f| f.name.clone()).collect();
    let mut edges: Vec<(String, PushFlow)> = Vec::new();
    for body in class
        .ctor
        .iter()
        .chain(class.methods.iter())
        .filter_map(|f| f.body.as_ref())
    {
        collect_push_edges(body, &fields, &mut edges);
    }
    // Seed with the owned class-field containers (keyed `this.field`), then
    // backward-propagate through `recv.push(local)` flows: if a receiver escapes, the
    // local it pulls from escapes too.
    let mut keys: BTreeSet<String> = owned_container_fields(prog, mi, &ns, class)
        .into_iter()
        .map(|(f, _)| format!("this.{f}"))
        .collect();
    loop {
        let mut changed = false;
        for (recv, flow) in &edges {
            if let PushFlow::Name(n) = flow {
                if keys.contains(recv) && keys.insert(n.clone()) {
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    keys.into_iter()
        .map(|k| k.strip_prefix("this.").map(String::from).unwrap_or(k))
        .collect()
}

/// Container fields whose leaf elements are pointers this class allocates with `new`
/// (so it owns them): `(field, nesting depth)`.
fn owned_container_fields(
    prog: &Program,
    mi: usize,
    ns: &[String],
    class: &Class,
) -> Vec<(String, usize)> {
    let bearing = new_bearing_names(class);
    let mut out = Vec::new();
    for f in &class.fields {
        if !bearing.contains(&format!("this.{}", f.name)) {
            continue;
        }
        if let Some(ty) = &f.ty {
            if let Some(depth) = container_depth_if_pointer_leaf(prog, mi, ns, ty) {
                out.push((f.name.clone(), depth));
            }
        }
    }
    out
}

/// If `ty` is a nested `Array<...>` whose leaf element maps to a pointer, the nesting
/// depth; otherwise `None`. (Also used by the destructor emission in `codegen`.)
pub fn container_depth_if_pointer_leaf(
    prog: &Program,
    mi: usize,
    ns: &[String],
    ty: &Type,
) -> Option<usize> {
    let mut depth = 0;
    let mut cur = ty;
    while let Type::Named { path, params, .. } = cur {
        if path.last().map(|s| s.as_str()) == Some("Array") && params.len() == 1 {
            depth += 1;
            cur = &params[0];
        } else {
            break;
        }
    }
    if depth == 0 {
        return None;
    }
    prog.map_type_use(cur, mi, ns)
        .ends_with('*')
        .then_some(depth)
}

/// Names (locals as `n`, fields as `this.f`) that receive `new`-allocated elements
/// through `push`/`insert`, directly or transitively (forward propagation).
fn new_bearing_names(class: &Class) -> BTreeSet<String> {
    let fields: BTreeSet<String> = class.fields.iter().map(|f| f.name.clone()).collect();
    let mut edges: Vec<(String, PushFlow)> = Vec::new();
    for body in class
        .ctor
        .iter()
        .chain(class.methods.iter())
        .filter_map(|f| f.body.as_ref())
    {
        collect_push_edges(body, &fields, &mut edges);
    }
    let mut bearing = BTreeSet::new();
    loop {
        let mut changed = false;
        for (recv, flow) in &edges {
            let owns = match flow {
                PushFlow::New => true,
                PushFlow::Name(n) => bearing.contains(n),
            };
            if owns && bearing.insert(recv.clone()) {
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    bearing
}

fn collect_push_edges(
    stmts: &[Stmt],
    fields: &BTreeSet<String>,
    out: &mut Vec<(String, PushFlow)>,
) {
    for st in stmts {
        match st {
            Stmt::Expr(e, _) => push_edge_from_expr(e, fields, out),
            Stmt::If { then, els, .. } => {
                collect_push_edges(std::slice::from_ref(then), fields, out);
                if let Some(e) = els {
                    collect_push_edges(std::slice::from_ref(e), fields, out);
                }
            }
            Stmt::For { body, .. } | Stmt::While { body, .. } => {
                collect_push_edges(std::slice::from_ref(body), fields, out)
            }
            Stmt::Block(s) => collect_push_edges(s, fields, out),
            Stmt::Switch { cases, default, .. } => {
                for c in cases {
                    collect_push_edges(&c.body, fields, out);
                }
                if let Some(d) = default {
                    collect_push_edges(d, fields, out);
                }
            }
            _ => {}
        }
    }
}

fn push_edge_from_expr(e: &Expr, fields: &BTreeSet<String>, out: &mut Vec<(String, PushFlow)>) {
    if let Expr::Call(target, args) = e {
        if let Expr::Field(recv, method) = &**target {
            let value = match (method.as_str(), args.len()) {
                ("push", 1) => Some(&args[0]),
                ("insert", 2) => Some(&args[1]),
                _ => None,
            };
            if let (Some(key), Some(value)) = (push_receiver_key(recv, fields), value) {
                let flow = match value {
                    Expr::New(ty, _) if !is_value_new(ty) => Some(PushFlow::New),
                    Expr::Ident(n) => Some(PushFlow::Name(n.clone())),
                    _ => None,
                };
                if let Some(flow) = flow {
                    out.push((key, flow));
                }
            }
        }
    }
}

/// A stable key for a `push`/`insert` receiver: `this.field` for a field (whether
/// written `this.field` or bare `field`), otherwise the plain local name.
fn push_receiver_key(recv: &Expr, fields: &BTreeSet<String>) -> Option<String> {
    match recv {
        Expr::Ident(n) if fields.contains(n) => Some(format!("this.{n}")),
        Expr::Ident(n) => Some(n.clone()),
        Expr::Field(r, f) if matches!(**r, Expr::This) => Some(format!("this.{f}")),
        _ => None,
    }
}

// ---- override-layer advisory diagnostics ---------------------------------------
//
// The `@owned` (field) and `@delete` (local) tags are UNSAFE overrides: the analysis
// always obeys them, but if one asserts ownership the flow contradicts, it reintroduces
// the double-free / use-after-free the analysis otherwise prevents. These advisories
// run the analysis alongside the tag and warn when the marked object looks unsound —
// the tag still decides the *action*, this only flags a likely mistake.

/// Advisory `(line, message)` diagnostics for `@owned`/`@delete` overrides that look
/// unsound to the analysis. Empty when every override is consistent with the inferred
/// flow (the expected state for sound code).
pub fn advisory_warnings(prog: &Program, mi: usize, class: &Class) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let fields: BTreeSet<String> = class.fields.iter().map(|f| f.name.clone()).collect();

    // An `@owned` field whose pointer is also handed back out of the object: freeing it
    // in the destructor may double-free or use-after-free the external alias.
    let handed = fields_handed_out(prog, mi, class, &fields);
    for f in &class.fields {
        if f.meta.iter().any(|m| m.name == "owned") && handed.contains(&f.name) {
            out.push((
                0,
                format!(
                    "@owned field `{}` is also handed out of the object (returned, stored elsewhere, or passed to an owning callee); freeing it in the destructor risks a double-free or use-after-free",
                    f.name
                ),
            ));
        }
    }

    // An `@delete` local whose value the analysis says does not merely live and die in
    // this scope — it escapes to a field, is returned, or is aliased.
    for func in class.ctor.iter().chain(class.methods.iter()) {
        let Some(body) = func.body.as_ref() else {
            continue;
        };
        let mut marked: Vec<(String, usize)> = Vec::new();
        collect_delete_vars(body, &mut marked);
        if marked.is_empty() {
            continue;
        }
        let fe = analyze_fn(prog, mi, class, func);
        for (name, line) in marked {
            let owner = fe
                .alloc_local
                .iter()
                .find_map(|(id, n)| (n == &name).then(|| fe.allocs.get(id)))
                .flatten();
            let reason = match owner {
                Some(Owner::Field(field)) => Some(format!(
                    "escapes to field `{field}` (did you mean `@owned` on `{field}`?); freeing it at scope close will dangle the field"
                )),
                Some(Owner::Return) => {
                    Some("is returned; freeing it at scope close is a use-after-free in the caller".to_string())
                }
                Some(Owner::Leak(LeakReason::Aliased)) => {
                    Some("is aliased (reaches more than one place); freeing it may double-free".to_string())
                }
                _ => None,
            };
            if let Some(reason) = reason {
                out.push((line, format!("@delete local `{name}` {reason}")));
            }
        }
    }
    out
}

/// Collect `(name, line)` of every `@delete var` in a body, descending into blocks.
fn collect_delete_vars(body: &[Stmt], out: &mut Vec<(String, usize)>) {
    for st in body {
        match st {
            Stmt::Var {
                name,
                delete: true,
                line,
                ..
            } => out.push((name.clone(), *line)),
            Stmt::If { then, els, .. } => {
                collect_delete_vars(std::slice::from_ref(then), out);
                if let Some(e) = els {
                    collect_delete_vars(std::slice::from_ref(e), out);
                }
            }
            Stmt::While { body, .. } | Stmt::For { body, .. } => {
                collect_delete_vars(std::slice::from_ref(body), out)
            }
            Stmt::Block(ss) => collect_delete_vars(ss, out),
            Stmt::Switch { cases, default, .. } => {
                for c in cases {
                    collect_delete_vars(&c.body, out);
                }
                if let Some(d) = default {
                    collect_delete_vars(d, out);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    fn method<'a>(c: &'a Class, name: &str) -> &'a Function {
        c.methods
            .iter()
            .find(|m| m.name.as_deref() == Some(name))
            .expect("method")
    }

    /// Build a one-file `Program` so `analyze_fn` can resolve `new X` constructors.
    fn prog_of(src: &str) -> Program {
        let file = parser::parse(src).expect("parse");
        Program::build(
            std::path::Path::new("."),
            vec![(std::path::PathBuf::from("C.hx"), file)],
        )
    }

    fn class_c(prog: &Program) -> &Class {
        prog.modules[0]
            .file
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Class(c) if c.name == "C" => Some(c),
                _ => None,
            })
            .expect("a class named C")
    }

    /// Owned fields of class `C` in `src` (builds the one-file program so the
    /// constructor-aware soundness guard can resolve `new X(...)` types).
    fn owned_of(src: &str) -> BTreeSet<String> {
        let prog = prog_of(src);
        let c = class_c(&prog);
        analyze_class(&prog, 0, c).owned_fields
    }

    #[test]
    fn field_assigned_new_is_owned() {
        assert!(
            owned_of("class C { var f:T; public function new() { this.f = new T(); } }")
                .contains("f")
        );
    }

    #[test]
    fn local_flowing_into_a_field_is_owned() {
        // `var x = new T(); this.f = x;` — the field owns it even though the write
        // is via a local (a case the old `collect_new_assigns` heuristic missed).
        let prog = prog_of("class T {} class C { var f:T; public function new() { var x = new T(); this.f = x; } }");
        let c = class_c(&prog);
        assert!(
            analyze_class(&prog, 0, c).owned_fields.contains("f"),
            "field owns the allocation"
        );
        // The local must NOT also be scope-owned (that would double-free).
        let fe = analyze_fn(&prog, 0, c, c.ctor.as_ref().unwrap());
        assert!(
            !fe.scope_owned.contains("x"),
            "x escapes to the field, not the scope"
        );
    }

    #[test]
    fn non_escaping_local_is_scope_owned() {
        let prog = prog_of("class T {} class C { public function run() { var x = new T(); } }");
        let c = class_c(&prog);
        let fe = analyze_fn(&prog, 0, c, method(c, "run"));
        assert!(fe.scope_owned.contains("x"));
        assert!(analyze_class(&prog, 0, c).owned_fields.is_empty());
    }

    #[test]
    fn returned_new_transfers_out() {
        let prog = prog_of("class T {} class C { public function make():T { return new T(); } }");
        let c = class_c(&prog);
        let fe = analyze_fn(&prog, 0, c, method(c, "make"));
        assert!(fe.allocs.values().any(|o| *o == Owner::Return));
        assert!(
            fe.scope_owned.is_empty(),
            "a returned new is not scope-freed"
        );
    }

    #[test]
    fn ctor_argument_to_an_owning_param_is_transferred() {
        // M3: `var line = new Line(new Vertex())` where Line `@owns` its parameter.
        // The vertex is owned by Line (freed by its destructor) — classified
        // `Transferred`, not a leak — and `line` is scope-owned.
        let prog = prog_of(
            "class Vertex {} class Line { @owned var a:Vertex; public function new(a:Vertex) { this.a = a; } } class C { public function run() { var line = new Line(new Vertex()); } }",
        );
        let c = class_c(&prog);
        let fe = analyze_fn(&prog, 0, c, method(c, "run"));
        assert!(fe.scope_owned.contains("line"));
        assert!(
            fe.allocs.values().any(|o| *o == Owner::Transferred),
            "vertex is owned by Line"
        );
        assert!(
            !fe.allocs.values().any(|o| matches!(o, Owner::Leak(_))),
            "no leak: {:?}",
            fe.allocs
        );
    }

    #[test]
    fn ctor_param_transitively_owned_through_a_nested_owning_ctor() {
        // C's ctor stows `new Wrapper(t)` (which owns `t`) in its owned field `w`, so
        // C transitively owns its own `t`. `new C(new Thing())` therefore transfers the
        // Thing (freed via C → Wrapper), not leaks it.
        let prog = prog_of(
            "class Thing {} \
             class Wrapper { @owned var inner:Thing; public function new(t:Thing) { this.inner = t; } } \
             class C { var w:Wrapper; public function new(t:Thing) { this.w = new Wrapper(t); } public function run():Void { var c = new C(new Thing()); } }",
        );
        let c = class_c(&prog);
        let fe = analyze_fn(&prog, 0, c, method(c, "run"));
        assert!(
            fe.allocs.values().any(|o| *o == Owner::Transferred),
            "Thing is transitively owned by C->Wrapper: {:?}",
            fe.allocs
        );
        assert!(
            !fe.allocs.values().any(|o| matches!(o, Owner::Leak(_))),
            "no leak: {:?}",
            fe.allocs
        );
    }

    #[test]
    fn mutually_constructing_ctors_terminate_and_borrow() {
        // A owns a B it news from its param, B owns an A it news from its param — a
        // constructor cycle. The cycle guard must terminate; neither param is provably
        // owned (the recursion bottoms out borrowed), so both conservatively leak
        // rather than spin or double-free.
        let prog = prog_of(
            "class A { var b:B; public function new(x:B) { this.b = new B(x); } } \
             class B { var a:A; public function new(y:A) { this.a = new A(y); } } \
             class C { public function run(b:B):Void { var a = new A(b); } }",
        );
        let c = class_c(&prog);
        let fe = analyze_fn(&prog, 0, c, method(c, "run"));
        // Terminates (no stack overflow) and classifies the `new A` allocation; the
        // borrowed `b` arg is not owned, so nothing is transferred.
        assert!(
            fe.allocs.values().any(|o| *o == Owner::Scope),
            "the new A is a scope local: {:?}",
            fe.allocs
        );
        assert!(
            !fe.allocs.values().any(|o| *o == Owner::Transferred),
            "cycle bottoms out borrowed: {:?}",
            fe.allocs
        );
    }

    #[test]
    fn ctor_argument_to_an_unresolved_callee_escapes() {
        // The constructor's class isn't in scope (a native/borrowing callee), so its
        // parameter ownership is unknown — the argument conservatively escapes (a
        // safe leak, never a double-free).
        let prog =
            prog_of("class C { public function run() { var line = new Line(new Vertex()); } }");
        let c = class_c(&prog);
        let fe = analyze_fn(&prog, 0, c, method(c, "run"));
        assert!(fe.scope_owned.contains("line"));
        assert!(fe
            .allocs
            .values()
            .any(|o| *o == Owner::Leak(LeakReason::Escapes)));
    }

    #[test]
    fn a_method_returning_new_is_owned_returning() {
        // The summary that drives return-consumption: a method whose body does
        // `return new T()` is owned-returning; one that allocates nothing is not.
        let prog = prog_of(
            "class T {} class C { public function make():T { return new T(); } public function noop():Void {} }",
        );
        let c = class_c(&prog);
        let fields: BTreeSet<String> = c.fields.iter().map(|f| f.name.clone()).collect();
        let s = owned_returning_methods(&prog, 0, c, &fields);
        assert!(
            s.contains("make") && !s.contains("noop"),
            "owned-returning set: {s:?}"
        );
    }

    #[test]
    fn consumed_owned_return_is_scope_owned() {
        // `var t = make()` consumes the ownership `make` transfers out, so `t` is
        // freed at scope close — return-ownership consumption (the plan's `f.make()`).
        let prog = prog_of(
            "class T {} class C { public function make():T { return new T(); } public function run():Void { var t = make(); } }",
        );
        let c = class_c(&prog);
        let fe = analyze_fn(&prog, 0, c, method(c, "run"));
        assert!(
            fe.scope_owned.contains("t"),
            "consumed owned return is scope-owned: {:?}",
            fe.scope_owned
        );
        assert!(
            !fe.allocs.values().any(|o| matches!(o, Owner::Leak(_))),
            "no leak: {:?}",
            fe.allocs
        );
    }

    #[test]
    fn fresh_alloc_as_a_comparison_operand_is_not_leaked() {
        // A `new`/owned-call result used only as a comparison operand and discarded is
        // a freeable temporary (`Scope`), not an escaping leak — the binary operator
        // never carries the pointer forward.
        let prog = prog_of(
            "class T {} class C { public function run():Void { if (new T() == null) {} } }",
        );
        let c = class_c(&prog);
        let fe = analyze_fn(&prog, 0, c, method(c, "run"));
        assert!(!fe.allocs.is_empty(), "the operand allocation is tracked");
        assert!(
            fe.allocs.values().all(|o| *o == Owner::Scope),
            "operand temp is scope, not leak: {:?}",
            fe.allocs
        );
    }

    #[test]
    fn aliased_allocation_is_left_unowned() {
        // One object stored into two fields → no single owner → neither field is
        // freed (a leak, not a double-free).
        let prog = prog_of(
            "class T {} class C { var a:T; var b:T; public function new() { var x = new T(); this.a = x; this.b = x; } }",
        );
        let c = class_c(&prog);
        let owned = analyze_class(&prog, 0, c).owned_fields;
        assert!(
            !owned.contains("a") && !owned.contains("b"),
            "aliased object is not double-owned: {owned:?}"
        );
        let fe = analyze_fn(&prog, 0, c, c.ctor.as_ref().unwrap());
        assert!(fe
            .allocs
            .values()
            .any(|o| *o == Owner::Leak(LeakReason::Aliased)));
    }

    #[test]
    fn new_pushed_into_a_field_container_is_owned() {
        assert!(owned_of(
            "class C { var items:Array<Item>; public function new() { this.items = []; this.items.push(new Item()); } }",
        )
        .contains("items"));
    }

    #[test]
    fn owned_tag_marks_a_field_with_no_new() {
        // An injected pointer (param stored into a field, never `new`ed here) is
        // owned only because of the `@owned` tag.
        assert!(owned_of(
            "class C { @owned var c:Child; public function new(c:Child) { this.c = c; } }"
        )
        .contains("c"));
    }

    // ---- M4 aliasing / soundness guard ------------------------------------
    // A field whose owned pointer is handed back out of the object must NOT be freed
    // by the destructor (it may be freed again or used afterwards). These pin that every
    // hand-out channel downgrades to a leak, that a borrow does not, and that the
    // `@owned` override is exempt.

    #[test]
    fn owned_field_returned_is_downgraded_to_leak() {
        let prog = prog_of(
            "class T {} class C { var f:T; public function new() { this.f = new T(); } public function get():T { return this.f; } }",
        );
        let c = class_c(&prog);
        let owned = analyze_class(&prog, 0, c).owned_fields;
        assert!(
            !owned.contains("f"),
            "a returned field is handed out, so not freed (leak): {owned:?}"
        );
    }

    #[test]
    fn owned_field_passed_to_a_call_is_downgraded_to_leak() {
        let prog = prog_of(
            "class T {} class C { var f:T; public function new() { this.f = new T(); } public function send():Void { use(this.f); } }",
        );
        let c = class_c(&prog);
        let owned = analyze_class(&prog, 0, c).owned_fields;
        assert!(
            !owned.contains("f"),
            "a field passed to a call may be retained, so not freed: {owned:?}"
        );
    }

    #[test]
    fn owned_field_used_only_as_a_receiver_stays_owned() {
        // `this.f.doStuff()` borrows the field (receiver), never hands it out — so the
        // guard leaves it owned. (Guards against the over-leak of treating borrows as
        // escapes.)
        let prog = prog_of(
            "class T { public function doStuff():Void {} } class C { var f:T; public function new() { this.f = new T(); } public function use():Void { this.f.doStuff(); } }",
        );
        let c = class_c(&prog);
        let owned = analyze_class(&prog, 0, c).owned_fields;
        assert!(
            owned.contains("f"),
            "a borrowed-only field is still freed: {owned:?}"
        );
    }

    #[test]
    fn owned_tag_is_not_downgraded_when_handed_out() {
        // `@owned` is an explicit override: even though `get` hands the field out, the
        // developer's assertion of ownership wins (the soundness guard does not touch it).
        let prog = prog_of(
            "class T {} class C { @owned var f:T; public function new(f:T) { this.f = f; } public function get():T { return this.f; } }",
        );
        let c = class_c(&prog);
        let owned = analyze_class(&prog, 0, c).owned_fields;
        assert!(
            owned.contains("f"),
            "@owned override is exempt from the guard: {owned:?}"
        );
    }

    // ---- field-ownership flow cases ---------------------------------------
    // Pin the analysis's owned-field result across the common flow shapes. (These
    // were originally diffed against the now-deleted `codegen::ownership` heuristics;
    // after confirming equivalence they are now direct assertions on the analysis.)

    #[test]
    fn owns_the_direct_cases() {
        // Direct `new` into a field, a container push, and an `@owned` injected field.
        assert!(owned_of(
            "class T {} class C { var f:T; public function new() { this.f = new T(); } }"
        )
        .contains("f"));
        assert!(owned_of(
            "class I {} class C { var items:Array<I>; public function new() { this.items = []; this.items.push(new I()); } }",
        )
        .contains("items"));
        assert!(owned_of("class Child {} class C { @owned var c:Child; public function new(c:Child) { this.c = c; } }").contains("c"));
    }

    #[test]
    fn owns_a_local_to_field_flow() {
        // `var x = new T(); this.f = x;` — the field owns the allocation even though
        // the write is via a local (the old `new`-into-field heuristic missed this).
        assert!(owned_of("class T {} class C { var f:T; public function new() { var x = new T(); this.f = x; } }")
            .contains("f"));
    }

    #[test]
    fn owns_a_transitive_container_flow() {
        // `var row = []; row.push(new Tile()); this.rows.push(row);` — the alloc is
        // pushed into a *local* container that then escapes into the field container,
        // and is carried transitively to the field.
        assert!(owned_of(
            "class Tile {} class C { var rows:Array<Array<Tile>>; public function new() { this.rows = []; var row:Array<Tile> = []; row.push(new Tile()); this.rows.push(row); } }",
        )
        .contains("rows"));
    }

    // ---- override-layer advisory warnings ---------------------------------

    /// Just the advisory messages for class `C` in `src`.
    fn advisories_of(src: &str) -> Vec<String> {
        let prog = prog_of(src);
        let c = class_c(&prog);
        advisory_warnings(&prog, 0, c)
            .into_iter()
            .map(|(_, m)| m)
            .collect()
    }

    #[test]
    fn sound_owned_and_delete_produce_no_advisory() {
        // The common shapes: `@owned` fields used only as receivers, and a scope-local
        // `@delete`. Nothing looks unsound, so no advisory fires.
        assert!(advisories_of(
            "class V { public function use():Void {} } class C { @owned var a:V; public function new(a:V) { this.a = a; } public function draw():Void { this.a.use(); } }",
        )
        .is_empty());
        assert!(advisories_of(
            "class T { public function use():Void {} } class C { public function run():Void { @delete var t:T = new T(); t.use(); } }",
        )
        .is_empty());
    }

    #[test]
    fn owned_field_handed_out_warns() {
        let msgs = advisories_of(
            "class V {} class C { @owned var a:V; public function new(a:V) { this.a = a; } public function get():V { return this.a; } }",
        );
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert!(
            msgs[0].contains("@owned field `a`") && msgs[0].contains("handed out"),
            "{msgs:?}"
        );
    }

    #[test]
    fn delete_local_escaping_to_a_field_warns() {
        // `@delete var x = new V(); this.f = x;` — the field owns it, so the scope free
        // is a double-free; the advisory points at `@owned`.
        let msgs = advisories_of(
            "class V {} class C { var f:V; public function set():Void { @delete var x:V = new V(); this.f = x; } }",
        );
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert!(
            msgs[0].contains("@delete local `x`") && msgs[0].contains("field `f`"),
            "{msgs:?}"
        );
    }

    #[test]
    fn delete_local_returned_warns() {
        let msgs = advisories_of(
            "class V {} class C { public function make():V { @delete var x:V = new V(); return x; } }",
        );
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert!(
            msgs[0].contains("@delete local `x`") && msgs[0].contains("returned"),
            "{msgs:?}"
        );
    }
}
