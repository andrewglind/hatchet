//! Whole-program escape / ownership analysis — the principled replacement for the
//! scattered heuristics in `codegen/ownership.rs`.
//!
//! **M2: the intraprocedural core.** Per function it tracks where each heap `new`
//! allocation comes to rest — its single owner — and classifies it; per class it
//! derives the fields the destructor must free. The goal is sound *conservative*
//! ownership: every allocation gets exactly one owner, or is left unowned (leaked)
//! when ambiguous, so the failure mode is a leak rather than a double-free.
//!
//! What is NOT here yet: interprocedural call/return summaries (M3) — so a `new`
//! passed to a call is conservatively treated as escaping/leaked for now — and the
//! transitive local-container flow. Codegen still runs on the old heuristics; this
//! module is built and validated alongside them until the M5 cutover.

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

/// Analyse a class: the fields its destructor must free are the `@owned`-marked
/// ones plus every field an allocation comes to rest in across its constructor and
/// methods.
pub fn analyze_class(class: &Class) -> ClassEscape {
    let fields: BTreeSet<String> = class.fields.iter().map(|f| f.name.clone()).collect();
    let mut owned: BTreeSet<String> = class
        .fields
        .iter()
        .filter(|f| f.meta.iter().any(|m| m.name == "owned"))
        .map(|f| f.name.clone())
        .collect();
    for body in class.ctor.iter().chain(class.methods.iter()).filter_map(|f| f.body.as_ref()) {
        // `None` ctx: owned-field detection never depends on argument ownership, so
        // it skips the interprocedural resolution (and the recursion it could spawn).
        let fe = analyze_body(&fields, body, None);
        for owner in fe.allocs.values() {
            if let Owner::Field(f) = owner {
                owned.insert(f.clone());
            }
        }
    }
    ClassEscape { owned_fields: owned }
}

/// The constructor parameter *indices* of `class` the constructor takes ownership
/// of: a parameter assigned straight into a field the destructor frees. Drives the
/// inline-vs-leak decision for `new` arguments at call sites.
fn ctor_owned_indices(class: &Class) -> BTreeSet<usize> {
    let mut out = BTreeSet::new();
    let Some(ctor) = class.ctor.as_ref() else { return out };
    let owned = analyze_class(class).owned_fields;
    let fields: BTreeSet<String> = class.fields.iter().map(|f| f.name.clone()).collect();
    for (i, p) in ctor.params.iter().enumerate() {
        if let Some(body) = ctor.body.as_ref() {
            if let Some(field) = param_dest_field(body, &p.name, &fields) {
                if owned.contains(&field) {
                    out.insert(i);
                }
            }
        }
    }
    out
}

/// The own-field a parameter is assigned straight into (`this.field = param` or the
/// bare `field = param`), searched across the (flat) statement list.
fn param_dest_field(body: &[Stmt], param: &str, fields: &BTreeSet<String>) -> Option<String> {
    fn target_field(target: &Expr, fields: &BTreeSet<String>) -> Option<String> {
        match target {
            Expr::Field(r, f) if matches!(**r, Expr::This) => Some(f.clone()),
            Expr::Ident(n) if fields.contains(n) => Some(n.clone()),
            _ => None,
        }
    }
    for st in body {
        if let Stmt::Expr(Expr::Assign { op: None, target, value }, _) = st {
            if let Expr::Ident(p) = &**value {
                if p == param {
                    if let Some(f) = target_field(target, fields) {
                        return Some(f);
                    }
                }
            }
        }
    }
    None
}

/// Analyse one function body. `class` is the enclosing class (its own fields let a
/// bare `field = …`/`field.push(…)` be recognised the same as `this.field`); the
/// `prog`/`mi` pair lets a `new X(arg)` resolve `X`'s constructor to decide whether
/// the argument is owned by the constructed object (interprocedural).
pub fn analyze_fn(prog: &Program, mi: usize, class: &Class, f: &Function) -> FnEscape {
    let fields: BTreeSet<String> = class.fields.iter().map(|f| f.name.clone()).collect();
    match &f.body {
        Some(body) => analyze_body(&fields, body, Some((prog, mi))),
        None => FnEscape::default(),
    }
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
) -> FnEscape {
    let mut w = Walk {
        fields,
        ctx,
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
        let Some((prog, mi)) = self.ctx else { return BTreeSet::new() };
        let Type::Named { path, .. } = ty else { return BTreeSet::new() };
        let Some(info) = prog.resolve_type(path, mi) else { return BTreeSet::new() };
        match prog.type_decl(info) {
            Some(Decl::Class(c)) => ctor_owned_indices(c),
            _ => BTreeSet::new(),
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        match s {
            Stmt::Var { name, init: Some(e), .. } => {
                let src = self.value(e, Some(name));
                self.assigns.push((name.clone(), src));
            }
            Stmt::Expr(e, _) => self.expr_stmt(e),
            Stmt::Return(Some(e), _) => {
                let src = self.value(e, None);
                self.sinks.push((Sink::Return, src));
            }
            Stmt::If { then, els, .. } => {
                self.stmt(then);
                if let Some(e) = els {
                    self.stmt(e);
                }
            }
            Stmt::For { body, .. } | Stmt::While { body, .. } => self.stmt(body),
            Stmt::Block(ss) => {
                for s in ss {
                    self.stmt(s);
                }
            }
            Stmt::Switch { cases, default, .. } => {
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
            Expr::Assign { op: None, target, value } => {
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
            Expr::Call(target, args) => self.call(target, args),
            _ => {
                let _ = self.value(e, None);
            }
        }
    }

    /// A method/function call. Recognises `container.push/insert(new …)` into an own
    /// field (the pushed value comes to rest in that field); all other arguments are
    /// opaque sinks (`Call`) until interprocedural summaries land.
    fn call(&mut self, target: &Expr, args: &[Expr]) {
        if let Expr::Field(recv, method) = target {
            let pushed = match (method.as_str(), args.len()) {
                ("push", 1) => Some(&args[0]),
                ("insert", 2) => Some(&args[1]),
                _ => None,
            };
            if let (Some(val), Some(field)) = (pushed, self.own_field(recv)) {
                let src = self.value(val, None);
                self.sinks.push((Sink::Field(field), src));
                return;
            }
            let _ = self.value(recv, None);
        } else {
            let _ = self.value(target, None);
        }
        for a in args {
            let src = self.value(a, None);
            self.sinks.push((Sink::Call, src));
        }
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
                    let sink = if owned.contains(&i) { Sink::Owned } else { Sink::Call };
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
            Expr::Call(target, args) => {
                self.call(target, args);
                Source::None
            }
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
                let l = self.value(lhs, None);
                let r = self.value(rhs, None);
                // a `new` buried in an operand has no resting place — it escapes.
                self.sink_if_new(l);
                self.sink_if_new(r);
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

        FnEscape { allocs, alloc_local: self.alloc_local, scope_owned }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    fn class(src: &str) -> Class {
        let file = parser::parse(src).expect("parse");
        file.decls
            .into_iter()
            .find_map(|d| match d {
                Decl::Class(c) => Some(c),
                _ => None,
            })
            .expect("a class")
    }

    fn method<'a>(c: &'a Class, name: &str) -> &'a Function {
        c.methods.iter().find(|m| m.name.as_deref() == Some(name)).expect("method")
    }

    /// Build a one-file `Program` so `analyze_fn` can resolve `new X` constructors.
    fn prog_of(src: &str) -> Program {
        let file = parser::parse(src).expect("parse");
        Program::build(std::path::Path::new("."), vec![(std::path::PathBuf::from("C.hx"), file)])
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

    #[test]
    fn field_assigned_new_is_owned() {
        let c = class("class C { var f:T; public function new() { this.f = new T(); } }");
        assert!(analyze_class(&c).owned_fields.contains("f"));
    }

    #[test]
    fn local_flowing_into_a_field_is_owned() {
        // `var x = new T(); this.f = x;` — the field owns it even though the write
        // is via a local (a case the old `collect_new_assigns` heuristic missed).
        let prog = prog_of("class T {} class C { var f:T; public function new() { var x = new T(); this.f = x; } }");
        let c = class_c(&prog);
        assert!(analyze_class(c).owned_fields.contains("f"), "field owns the allocation");
        // The local must NOT also be scope-owned (that would double-free).
        let fe = analyze_fn(&prog, 0, c, c.ctor.as_ref().unwrap());
        assert!(!fe.scope_owned.contains("x"), "x escapes to the field, not the scope");
    }

    #[test]
    fn non_escaping_local_is_scope_owned() {
        let prog = prog_of("class T {} class C { public function run() { var x = new T(); } }");
        let c = class_c(&prog);
        let fe = analyze_fn(&prog, 0, c, method(c, "run"));
        assert!(fe.scope_owned.contains("x"));
        assert!(analyze_class(c).owned_fields.is_empty());
    }

    #[test]
    fn returned_new_transfers_out() {
        let prog = prog_of("class T {} class C { public function make():T { return new T(); } }");
        let c = class_c(&prog);
        let fe = analyze_fn(&prog, 0, c, method(c, "make"));
        assert!(fe.allocs.values().any(|o| *o == Owner::Return));
        assert!(fe.scope_owned.is_empty(), "a returned new is not scope-freed");
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
        assert!(fe.allocs.values().any(|o| *o == Owner::Transferred), "vertex is owned by Line");
        assert!(!fe.allocs.values().any(|o| matches!(o, Owner::Leak(_))), "no leak: {:?}", fe.allocs);
    }

    #[test]
    fn ctor_argument_to_an_unresolved_callee_escapes() {
        // The constructor's class isn't in scope (a native/borrowing callee), so its
        // parameter ownership is unknown — the argument conservatively escapes (a
        // safe leak, never a double-free).
        let prog = prog_of("class C { public function run() { var line = new Line(new Vertex()); } }");
        let c = class_c(&prog);
        let fe = analyze_fn(&prog, 0, c, method(c, "run"));
        assert!(fe.scope_owned.contains("line"));
        assert!(fe.allocs.values().any(|o| *o == Owner::Leak(LeakReason::Escapes)));
    }

    #[test]
    fn aliased_allocation_is_left_unowned() {
        // One object stored into two fields → no single owner → neither field is
        // freed (a leak, not a double-free).
        let prog = prog_of(
            "class T {} class C { var a:T; var b:T; public function new() { var x = new T(); this.a = x; this.b = x; } }",
        );
        let c = class_c(&prog);
        let owned = analyze_class(c).owned_fields;
        assert!(!owned.contains("a") && !owned.contains("b"), "aliased object is not double-owned: {owned:?}");
        let fe = analyze_fn(&prog, 0, c, c.ctor.as_ref().unwrap());
        assert!(fe.allocs.values().any(|o| *o == Owner::Leak(LeakReason::Aliased)));
    }

    #[test]
    fn new_pushed_into_a_field_container_is_owned() {
        let c = class(
            "class C { var items:Array<Item>; public function new() { this.items = []; this.items.push(new Item()); } }",
        );
        assert!(analyze_class(&c).owned_fields.contains("items"));
    }

    #[test]
    fn owned_tag_marks_a_field_with_no_new() {
        // An injected pointer (param stored into a field, never `new`ed here) is
        // owned only because of the `@owned` tag.
        let c = class("class C { @owned var c:Child; public function new(c:Child) { this.c = c; } }");
        assert!(analyze_class(&c).owned_fields.contains("c"));
    }

    // ---- diff against the current heuristics (M2 validation) --------------
    // The new analysis is built alongside `codegen::ownership`; these cases pin
    // exactly where it agrees, improves, and still has a gap, so the M5 cutover is
    // a known quantity. (Codegen still runs on the heuristics until then.)

    use crate::sema::Program;
    use std::path::PathBuf;

    /// Owned-field sets from both the new escape analysis and the current
    /// heuristics, for class `C` in `src` (other classes are element types that
    /// must be declared so the heuristics' pointer/container detection resolves).
    fn both(src: &str) -> (BTreeSet<String>, BTreeSet<String>) {
        let file = parser::parse(src).expect("parse");
        let prog = Program::build(std::path::Path::new("."), vec![(PathBuf::from("C.hx"), file)]);
        let class = prog.modules[0]
            .file
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Class(c) if c.name == "C" => Some(c),
                _ => None,
            })
            .expect("a class named C");
        let escape = analyze_class(class).owned_fields;
        let current: BTreeSet<String> =
            crate::codegen::ownership::owned_field_set(&prog, 0, &[], class).into_iter().collect();
        (escape, current)
    }

    #[test]
    fn diff_agrees_on_the_direct_cases() {
        // Direct `new` into a field, container push, and `@owned`: the analysis and
        // the heuristics agree exactly.
        for src in [
            "class T {} class C { var f:T; public function new() { this.f = new T(); } }",
            "class I {} class C { var items:Array<I>; public function new() { this.items = []; this.items.push(new I()); } }",
            "class Child {} class C { @owned var c:Child; public function new(c:Child) { this.c = c; } }",
        ] {
            let (escape, current) = both(src);
            assert_eq!(escape, current, "escape vs heuristics disagree on:\n{src}");
        }
    }

    #[test]
    fn diff_analysis_improves_on_local_to_field_flow() {
        // `var x = new T(); this.f = x;` — the heuristics miss it (leak); the
        // analysis owns the field. A strict improvement (the leaked object is now
        // freed; the M1 harness confirms it is freed exactly once).
        let (escape, current) =
            both("class T {} class C { var f:T; public function new() { var x = new T(); this.f = x; } }");
        assert!(escape.contains("f"), "analysis owns the field");
        assert!(!current.contains("f"), "the heuristics miss the local-to-field flow");
    }

    #[test]
    fn diff_analysis_still_misses_transitive_container_flow() {
        // `var row = []; row.push(new Tile()); this.rows.push(row);` — the
        // heuristics free `rows` (transitive container flow); the M2 analysis does
        // not yet. A documented gap for M3 (interprocedural + container flow); the
        // analysis under-frees here, which is a safe leak, never a double-free.
        let (escape, current) = both(
            "class Tile {} class C { var rows:Array<Array<Tile>>; public function new() { this.rows = []; var row:Array<Tile> = []; row.push(new Tile()); this.rows.push(row); } }",
        );
        assert!(current.contains("rows"), "the heuristics own the nested container");
        assert!(!escape.contains("rows"), "M2 does not model transitive container flow yet (M3)");
    }
}
