//! Pre-codegen validation: catch references Hatchet would otherwise *guess* about.
//!
//! The type mapper falls back to a bare, by-value spelling for any name it cannot
//! resolve (a typo, a missing `import`, or a type declared outside the `--src`
//! scope). That silent guess produces subtly wrong C++ (a class emitted by value
//! instead of as a pointer) or output that fails to compile far from the cause.
//! This pass walks every type a module references and turns each unresolved one
//! into a hard [`Diagnostic`], so the run fails with an actionable message rather
//! than quietly guessing.

use std::collections::BTreeSet;

use crate::ast::*;
use crate::diag::Diagnostic;

use super::types::{container_template, map_primitive};
use super::Program;

/// One named-type reference found in a module, with where it appeared.
struct TypeUse {
    path: Vec<String>,
    line: usize,
    ctx: String,
}

/// Walk every type a module references (in signatures and bodies) and collect the
/// non-built-in named uses — the raw material for both the unresolved-type check
/// and the referenced-module (include) computation.
fn collect_refs(prog: &Program, mi: usize) -> Vec<TypeUse> {
    let m = &prog.modules[mi];
    // Generic parameters declared on any type in this module are valid names even
    // though the symbol table has no entry for them.
    let mut type_params: BTreeSet<String> = BTreeSet::new();
    for d in &m.file.decls {
        match d {
            Decl::Class(c) => type_params.extend(c.type_params.iter().cloned()),
            Decl::Interface(i) => type_params.extend(i.type_params.iter().cloned()),
            _ => {}
        }
    }
    let mut c = Collector { type_params, refs: Vec::new() };
    for d in &m.file.decls {
        c.decl(d);
    }
    c.refs
}

/// Every "unresolved type" error for the declarations in module `mi`.
pub fn unresolved_type_errors(prog: &Program, mi: usize) -> Vec<Diagnostic> {
    let m = &prog.modules[mi];
    let file = m.path.file_name().and_then(|s| s.to_str()).unwrap_or("").to_string();
    let mut out = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for r in collect_refs(prog, mi) {
        if prog.resolve_type(&r.path, mi).is_none() {
            // Types Hatchet recognises but does not support (the macro `Expr`, the
            // regex `EReg`) are reported as `Unsupported` (with the contribution
            // invite) by `unsupported_construct_errors`, not as a plain unresolved
            // type — skip them here to avoid a double report.
            if unsupported_type_label(&r.path).is_some() {
                continue;
            }
            // De-duplicate identical (name, line, context) reports.
            let key = format!("{}|{}|{}", r.path.join("."), r.line, r.ctx);
            if seen.insert(key) {
                out.push(Diagnostic::error(
                    file.clone(),
                    r.line,
                    format!(
                        "unresolved type `{}` {} — is it declared and within the --src scope?",
                        r.path.join("."),
                        r.ctx
                    ),
                ));
            }
        }
    }
    out
}

/// Errors for Haxe constructs Hatchet recognises but does not yet transpile. These
/// are `Unsupported` (the fix is in Hatchet), so they carry the contribution invite.
///
/// Currently this flags a **lambda (arrow function) used outside a supported
/// position** — i.e. anywhere other than the initialiser of a top-level `final`
/// (which becomes a free function) or the argument of `Array.map(...)`. Hatchet has
/// no general first-class-function lowering, so such a lambda would otherwise emit
/// a placeholder that does not compile.
pub fn unsupported_construct_errors(prog: &Program, mi: usize) -> Vec<Diagnostic> {
    let m = &prog.modules[mi];
    let file = m.path.file_name().and_then(|s| s.to_str()).unwrap_or("").to_string();
    let mut out = Vec::new();
    {
        let mut w = UnsupportedWalker { file: file.clone(), line: 0, out: &mut out };
        for d in &m.file.decls {
            w.decl(d);
        }
    }
    // Haxe macros are compile-time metaprogramming with no C++ runtime lowering:
    // a `macro` function, or any use of the macro AST type `Expr`, is unsupported.
    for d in &m.file.decls {
        flag_macro_functions(d, &file, &mut out);
    }
    // `using` static extensions rewrite `a.f(b)` into `Module.f(a, b)` at the call
    // site, chosen by `a`'s type. Hatchet has no such call-site rewriting, so a
    // `using` would be silently ignored — flag it rather than drop it.
    for u in &m.file.usings {
        out.push(Diagnostic::unsupported(
            file.clone(),
            u.line,
            format!("the `using` static-extension declaration `{}`", u.path.join(".")),
        ));
    }
    // Top-level declarations Hatchet recognised but skipped at parse time (an
    // `abstract` type or `enum abstract`) — flag each with the label the parser set.
    for d in &m.file.decls {
        if let Decl::Unsupported { feature, line } = d {
            out.push(Diagnostic::unsupported(file.clone(), *line, feature.clone()));
        }
    }
    // Enum variants that take constructor parameters (`Move(dx:Int, dy:Int)`) need a
    // tagged-union lowering Hatchet does not implement — it emits plain C++ enums, so
    // the payload would be lost. Flag each parameterized variant.
    for d in &m.file.decls {
        if let Decl::Enum(e) = d {
            for v in &e.variants {
                if !v.params.is_empty() {
                    let line = v.params.iter().find_map(|p| p.ty.as_ref().map(type_line)).unwrap_or(0);
                    out.push(Diagnostic::unsupported(
                        file.clone(),
                        line,
                        format!("the parameterized enum variant `{}`", v.name),
                    ));
                }
            }
        }
    }
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for r in collect_refs(prog, mi) {
        if let Some(label) = unsupported_type_label(&r.path) {
            if seen.insert(format!("{}|{}", r.line, r.ctx)) {
                out.push(Diagnostic::unsupported(
                    file.clone(),
                    r.line,
                    format!("{label} `{}` {}", r.path.join("."), r.ctx),
                ));
            }
        }
    }
    out
}

/// If a type path names a Haxe construct Hatchet does not support, return a human
/// label for it (used in the diagnostic). Currently the macro AST type `Expr`
/// (`haxe.macro.Expr`) and the regular-expression type `EReg`. These are reported
/// as `Unsupported` rather than as plain unresolved types.
fn unsupported_type_label(path: &[String]) -> Option<&'static str> {
    match path.last().map(|s| s.as_str()) {
        Some("Expr") => Some("the Haxe macro type"),
        Some("EReg") => Some("the Haxe regular-expression type"),
        _ => None,
    }
}

/// The source line a type was written on (`0` if synthesized).
fn type_line(t: &Type) -> usize {
    match t {
        Type::Named { line, .. } => *line,
        Type::Anon(fields) => fields.first().map(|f| type_line(&f.ty)).unwrap_or(0),
        Type::Func { ret, .. } => type_line(ret),
    }
}

/// A representative source line for a function (from its signature types).
fn signature_line(f: &Function) -> usize {
    f.ret
        .as_ref()
        .map(type_line)
        .or_else(|| f.params.iter().find_map(|p| p.ty.as_ref().map(type_line)))
        .unwrap_or(0)
}

/// Flag every `macro` function declared in `d` as unsupported.
fn flag_macro_functions(d: &Decl, file: &str, out: &mut Vec<Diagnostic>) {
    fn flag(f: &Function, file: &str, out: &mut Vec<Diagnostic>) {
        if f.modifiers.is_macro {
            let who = f.name.clone().unwrap_or_else(|| "new".to_string());
            out.push(Diagnostic::unsupported(
                file.to_string(),
                signature_line(f),
                format!("the Haxe `macro` function `{who}`"),
            ));
        }
    }
    match d {
        Decl::Class(c) => {
            for m in &c.methods {
                flag(m, file, out);
            }
            if let Some(ct) = &c.ctor {
                flag(ct, file, out);
            }
        }
        Decl::Interface(i) => {
            for m in &i.methods {
                flag(m, file, out);
            }
        }
        Decl::Function(f) => flag(f, file, out),
        _ => {}
    }
}

/// Walks bodies flagging lambdas in unsupported positions. `line` tracks the
/// enclosing statement so a flagged lambda gets a source location.
struct UnsupportedWalker<'a> {
    file: String,
    line: usize,
    out: &'a mut Vec<Diagnostic>,
}

impl<'a> UnsupportedWalker<'a> {
    fn flag_lambda(&mut self) {
        self.out.push(Diagnostic::unsupported(
            self.file.clone(),
            self.line,
            "a lambda (arrow function) used outside a top-level `final` binding or an `Array.map`/`filter`/`sort(...)` call",
        ));
    }

    fn flag_regex(&mut self) {
        self.out.push(Diagnostic::unsupported(
            self.file.clone(),
            self.line,
            "a regular-expression literal (`~/.../`)",
        ));
    }

    fn flag_is(&mut self) {
        self.out.push(Diagnostic::unsupported(
            self.file.clone(),
            self.line,
            "the `is` runtime type-check operator (Haxe 4.2)",
        ));
    }

    fn decl(&mut self, d: &Decl) {
        match d {
            Decl::Class(c) => {
                for m in &c.methods {
                    self.func(m);
                }
                if let Some(ct) = &c.ctor {
                    self.func(ct);
                }
            }
            Decl::Interface(i) => {
                for m in &i.methods {
                    self.func(m);
                }
            }
            Decl::Function(f) => self.func(f),
            // A top-level `final = <lambda>` is a supported free function: its body
            // is walked, but the binding lambda itself is allowed.
            Decl::Global(g) => match &g.init {
                Some(Expr::Lambda { body, .. }) => self.lambda_body(body),
                Some(e) => self.expr(e),
                None => {}
            },
            // Flagged directly in `unsupported_construct_errors` from its label.
            Decl::Unsupported { .. } => {}
            Decl::Enum(_) | Decl::Typedef(_) => {}
        }
    }

    fn func(&mut self, f: &Function) {
        if let Some(body) = &f.body {
            for s in body {
                self.stmt(s);
            }
        }
    }

    fn lambda_body(&mut self, body: &LambdaBody) {
        match body {
            LambdaBody::Expr(e) => self.expr(e),
            LambdaBody::Block(stmts) => {
                for s in stmts {
                    self.stmt(s);
                }
            }
        }
    }

    fn iterable(&mut self, it: &Iterable) {
        match it {
            Iterable::Range(a, b) => {
                self.expr(a);
                self.expr(b);
            }
            Iterable::Coll(e) => self.expr(e),
        }
    }

    fn stmt(&mut self, st: &Stmt) {
        match st {
            Stmt::Var { init, line, .. } => {
                self.line = *line;
                if let Some(e) = init {
                    self.expr(e);
                }
            }
            Stmt::Expr(e, line) => {
                self.line = *line;
                self.expr(e);
            }
            Stmt::If { cond, then, els, line } => {
                self.line = *line;
                self.expr(cond);
                self.stmt(then);
                if let Some(e) = els {
                    self.stmt(e);
                }
            }
            Stmt::For { iter, body, line, .. } => {
                self.line = *line;
                self.iterable(iter);
                self.stmt(body);
            }
            Stmt::While { cond, body, line, .. } => {
                self.line = *line;
                self.expr(cond);
                self.stmt(body);
            }
            Stmt::Switch { subject, cases, default, line } => {
                self.line = *line;
                self.expr(subject);
                for c in cases {
                    for p in &c.patterns {
                        self.expr(p);
                    }
                    for s in &c.body {
                        self.stmt(s);
                    }
                }
                if let Some(b) = default {
                    for s in b {
                        self.stmt(s);
                    }
                }
            }
            Stmt::Return(Some(e), line) => {
                self.line = *line;
                self.expr(e);
            }
            Stmt::Throw(e, line) => {
                self.line = *line;
                self.expr(e);
            }
            Stmt::Try { body, catches, .. } => {
                self.stmt(body);
                for c in catches {
                    for s in &c.body {
                        self.stmt(s);
                    }
                }
            }
            Stmt::Block(stmts) => {
                for s in stmts {
                    self.stmt(s);
                }
            }
            Stmt::Return(None, _) | Stmt::Break | Stmt::Continue | Stmt::Verbatim { .. } => {}
        }
    }

    fn expr(&mut self, e: &Expr) {
        match e {
            // A lambda reaching any ordinary expression position is unsupported.
            Expr::Lambda { .. } => self.flag_lambda(),
            Expr::Regex { .. } => self.flag_regex(),
            Expr::Switch { subject, cases, default } => {
                self.expr(subject);
                for c in cases {
                    for p in &c.patterns {
                        self.expr(p);
                    }
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
            Expr::Call(target, args) => {
                // `recv.map/filter/sort(lambda)` — the first-argument lambda is
                // supported; walk the receiver and the lambda's body, but do not
                // flag the lambda itself.
                if let Expr::Field(recv, method) = &**target {
                    if matches!(method.as_str(), "map" | "filter" | "sort")
                        && matches!(args.first(), Some(Expr::Lambda { .. }))
                    {
                        self.expr(recv);
                        if let Some(Expr::Lambda { body, .. }) = args.first() {
                            self.lambda_body(body);
                        }
                        for a in &args[1..] {
                            self.expr(a);
                        }
                        return;
                    }
                }
                self.expr(target);
                for a in args {
                    self.expr(a);
                }
            }
            Expr::New(_, args) => {
                for a in args {
                    self.expr(a);
                }
            }
            Expr::Is { expr, .. } => {
                self.flag_is();
                self.expr(expr);
            }
            Expr::Cast { expr, .. }
            | Expr::TypeCheck { expr, .. }
            | Expr::Unary { expr, .. }
            | Expr::Paren(expr) => self.expr(expr),
            Expr::Field(r, _) | Expr::SafeField(r, _) => self.expr(r),
            Expr::Index(a, b) => {
                self.expr(a);
                self.expr(b);
            }
            Expr::Binary { lhs, rhs, .. } | Expr::NullCoalesce(lhs, rhs) => {
                self.expr(lhs);
                self.expr(rhs);
            }
            Expr::Ternary { cond, then, els } => {
                self.expr(cond);
                self.expr(then);
                self.expr(els);
            }
            Expr::Assign { target, value, .. } => {
                self.expr(target);
                self.expr(value);
            }
            Expr::ArrayLit(items) => {
                for i in items {
                    self.expr(i);
                }
            }
            Expr::MapLit(pairs) => {
                for (k, v) in pairs {
                    self.expr(k);
                    self.expr(v);
                }
            }
            Expr::ObjectLit(fields) => {
                for (_, v) in fields {
                    self.expr(v);
                }
            }
            Expr::Comprehension { iter, guard, body, .. } => {
                self.iterable(iter);
                if let Some(g) = guard {
                    self.expr(g);
                }
                match body {
                    ComprBody::Value(e) => self.expr(e),
                    ComprBody::KeyValue(k, v) => {
                        self.expr(k);
                        self.expr(v);
                    }
                }
            }
            Expr::Int(_)
            | Expr::Float(_)
            | Expr::Str { .. }
            | Expr::Bool(_)
            | Expr::Null
            | Expr::This
            | Expr::Super
            | Expr::Verbatim(_)
            | Expr::Ident(_) => {}
        }
    }
}

/// The distinct modules whose declared types module `mi` references (so their
/// headers can be `#include`d). Excludes `mi` itself.
pub fn referenced_modules(prog: &Program, mi: usize) -> BTreeSet<usize> {
    let mut out = BTreeSet::new();
    for r in collect_refs(prog, mi) {
        if let Some(ti) = prog.resolve_type(&r.path, mi) {
            if ti.module_index != mi {
                out.insert(ti.module_index);
            }
        }
    }
    out
}

struct Collector {
    type_params: BTreeSet<String>,
    refs: Vec<TypeUse>,
}

impl Collector {
    /// Record one named-type *use* (unless it is a built-in / generic param), then
    /// recurse into its parameters. `ctx` labels where the type appeared.
    fn check(&mut self, ty: &Type, ctx: &str) {
        match ty {
            Type::Named { path, params, line, .. } => {
                let name = path.last().map(|s| s.as_str()).unwrap_or("");
                // `Dynamic`/`Any` are valid Haxe types with no concrete C++ spelling
                // (the overload marker); they resolve, even if never emitted directly.
                let known = map_primitive(name).is_some()
                    || container_template(name).is_some()
                    || name == "Null"
                    || matches!(name, "Dynamic" | "Any")
                    || self.type_params.contains(name);
                if !known {
                    self.refs.push(TypeUse {
                        path: path.clone(),
                        line: *line,
                        ctx: ctx.to_string(),
                    });
                }
                for p in params {
                    self.check(p, ctx);
                }
            }
            Type::Anon(fields) => {
                for f in fields {
                    self.check(&f.ty, ctx);
                }
            }
            Type::Func { params, ret } => {
                for p in params {
                    self.check(p, ctx);
                }
                self.check(ret, ctx);
            }
        }
    }

    fn opt(&mut self, ty: &Option<Type>, ctx: &str) {
        if let Some(t) = ty {
            self.check(t, ctx);
        }
    }

    fn decl(&mut self, d: &Decl) {
        match d {
            Decl::Class(c) => {
                if let Some(b) = &c.extends {
                    self.check(b, &format!("as a base class of `{}`", c.name));
                }
                for i in &c.implements {
                    self.check(i, &format!("as an interface of `{}`", c.name));
                }
                for f in &c.fields {
                    self.opt(&f.ty, &format!("in field `{}`", f.name));
                }
                for m in &c.methods {
                    self.func(m);
                }
                if let Some(ct) = &c.ctor {
                    self.func(ct);
                }
            }
            Decl::Interface(i) => {
                for b in &i.extends {
                    self.check(b, &format!("as a base interface of `{}`", i.name));
                }
                for f in &i.fields {
                    self.opt(&f.ty, &format!("in field `{}`", f.name));
                }
                for m in &i.methods {
                    self.func(m);
                }
            }
            Decl::Typedef(t) => match &t.target {
                TypedefTarget::Alias(ty) => self.check(ty, &format!("in typedef `{}`", t.name)),
                TypedefTarget::Struct(fields) => {
                    for f in fields {
                        self.check(&f.ty, &format!("in field `{}` of `{}`", f.name, t.name));
                    }
                }
            },
            Decl::Enum(e) => {
                for variant in &e.variants {
                    for p in &variant.params {
                        self.opt(&p.ty, &format!("in enum variant `{}`", variant.name));
                    }
                }
            }
            Decl::Global(g) => {
                self.opt(&g.ty, &format!("in `{}`", g.name));
                if let Some(init) = &g.init {
                    self.expr(init, &format!("in `{}`", g.name));
                }
            }
            Decl::Function(f) => self.func(f),
            // Skipped at parse time (body discarded) and flagged elsewhere — nothing
            // to collect type references from.
            Decl::Unsupported { .. } => {}
        }
    }

    fn func(&mut self, f: &Function) {
        let who = f.name.clone().unwrap_or_else(|| "new".to_string());
        for p in &f.params {
            self.opt(&p.ty, &format!("in parameter `{}` of `{}`", p.name, who));
        }
        self.opt(&f.ret, &format!("in the return type of `{}`", who));
        if let Some(body) = &f.body {
            let ctx = format!("in the body of `{}`", who);
            for st in body {
                self.stmt(st, &ctx);
            }
        }
    }

    /// Walk a statement for explicit type annotations (`var x:T`) and nested
    /// expressions that name a type (`new T()`, `cast(e, T)`, `(e : T)`).
    fn stmt(&mut self, st: &Stmt, ctx: &str) {
        match st {
            Stmt::Var { ty, init, .. } => {
                self.opt(ty, ctx);
                if let Some(e) = init {
                    self.expr(e, ctx);
                }
            }
            Stmt::Expr(e, _) => self.expr(e, ctx),
            Stmt::If { cond, then, els, .. } => {
                self.expr(cond, ctx);
                self.stmt(then, ctx);
                if let Some(e) = els {
                    self.stmt(e, ctx);
                }
            }
            Stmt::For { iter, body, .. } => {
                self.iterable(iter, ctx);
                self.stmt(body, ctx);
            }
            Stmt::While { cond, body, .. } => {
                self.expr(cond, ctx);
                self.stmt(body, ctx);
            }
            Stmt::Switch { subject, cases, default, .. } => {
                self.expr(subject, ctx);
                for c in cases {
                    for p in &c.patterns {
                        self.expr(p, ctx);
                    }
                    for s in &c.body {
                        self.stmt(s, ctx);
                    }
                }
                if let Some(body) = default {
                    for s in body {
                        self.stmt(s, ctx);
                    }
                }
            }
            Stmt::Return(Some(e), _) => self.expr(e, ctx),
            Stmt::Throw(e, _) => self.expr(e, ctx),
            // try/catch is transpiled: walk the body, and validate each (typed)
            // catch's exception type — an unresolved one would not compile.
            Stmt::Try { body, catches, .. } => {
                self.stmt(body, ctx);
                for c in catches {
                    self.opt(&c.ty, ctx);
                    for s in &c.body {
                        self.stmt(s, ctx);
                    }
                }
            }
            Stmt::Block(stmts) => {
                for s in stmts {
                    self.stmt(s, ctx);
                }
            }
            Stmt::Return(None, _) | Stmt::Break | Stmt::Continue | Stmt::Verbatim { .. } => {}
        }
    }

    fn iterable(&mut self, it: &Iterable, ctx: &str) {
        match it {
            Iterable::Range(a, b) => {
                self.expr(a, ctx);
                self.expr(b, ctx);
            }
            Iterable::Coll(e) => self.expr(e, ctx),
        }
    }

    /// Walk an expression for the type names it mentions, recursing through every
    /// sub-expression so `new`/`cast`/type-checks anywhere are validated.
    fn expr(&mut self, e: &Expr, ctx: &str) {
        match e {
            Expr::Switch { subject, cases, default } => {
                self.expr(subject, ctx);
                for c in cases {
                    for p in &c.patterns {
                        self.expr(p, ctx);
                    }
                    for s in &c.body {
                        self.stmt(s, ctx);
                    }
                }
                if let Some(d) = default {
                    for s in d {
                        self.stmt(s, ctx);
                    }
                }
            }
            Expr::New(ty, args) => {
                self.check(ty, ctx);
                for a in args {
                    self.expr(a, ctx);
                }
            }
            Expr::Cast { expr, ty } => {
                self.opt(ty, ctx);
                self.expr(expr, ctx);
            }
            Expr::TypeCheck { expr, ty } => {
                self.check(ty, ctx);
                self.expr(expr, ctx);
            }
            // Unsupported (flagged elsewhere). Walk the operand for nested refs, but
            // skip the checked type — it is the construct we do not support, so a
            // separate "unresolved type" report on it would be misleading.
            Expr::Is { expr, .. } => self.expr(expr, ctx),
            Expr::Lambda { params, ret, body } => {
                for p in params {
                    self.opt(&p.ty, ctx);
                }
                self.opt(ret, ctx);
                match &**body {
                    LambdaBody::Expr(e) => self.expr(e, ctx),
                    LambdaBody::Block(stmts) => {
                        for s in stmts {
                            self.stmt(s, ctx);
                        }
                    }
                }
            }
            Expr::Field(r, _) | Expr::SafeField(r, _) => self.expr(r, ctx),
            Expr::Index(a, b) => {
                self.expr(a, ctx);
                self.expr(b, ctx);
            }
            Expr::Call(t, args) => {
                self.expr(t, ctx);
                for a in args {
                    self.expr(a, ctx);
                }
            }
            Expr::Unary { expr, .. } | Expr::Paren(expr) => self.expr(expr, ctx),
            Expr::Binary { lhs, rhs, .. } | Expr::NullCoalesce(lhs, rhs) => {
                self.expr(lhs, ctx);
                self.expr(rhs, ctx);
            }
            Expr::Ternary { cond, then, els } => {
                self.expr(cond, ctx);
                self.expr(then, ctx);
                self.expr(els, ctx);
            }
            Expr::Assign { target, value, .. } => {
                self.expr(target, ctx);
                self.expr(value, ctx);
            }
            Expr::ArrayLit(items) => {
                for i in items {
                    self.expr(i, ctx);
                }
            }
            Expr::MapLit(pairs) => {
                for (k, val) in pairs {
                    self.expr(k, ctx);
                    self.expr(val, ctx);
                }
            }
            Expr::ObjectLit(fields) => {
                for (_, val) in fields {
                    self.expr(val, ctx);
                }
            }
            Expr::Comprehension { iter, guard, body, .. } => {
                self.iterable(iter, ctx);
                if let Some(g) = guard {
                    self.expr(g, ctx);
                }
                match body {
                    ComprBody::Value(e) => self.expr(e, ctx),
                    ComprBody::KeyValue(k, val) => {
                        self.expr(k, ctx);
                        self.expr(val, ctx);
                    }
                }
            }
            // Leaves with no nested expressions or types.
            Expr::Int(_)
            | Expr::Float(_)
            | Expr::Str { .. }
            | Expr::Bool(_)
            | Expr::Null
            | Expr::This
            | Expr::Super
            | Expr::Verbatim(_)
            | Expr::Regex { .. }
            | Expr::Ident(_) => {}
        }
    }
}
