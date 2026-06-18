//! `.cpp` source generation: constructor and method bodies.
//!
//! This is where Haxe statements and expressions are transpiled to C++. The
//! generator carries a small amount of type information so it can choose between
//! `.` and `->` for member access, rewrite property-accessor access to
//! `GetX()`/`SetX()`, qualify enum constants, and desugar the Haxe constructs
//! described in `README.md`.
//!
//! The validation gate is *compilation*, not byte-equality with a reference, so
//! the output favours correct, compilable C++ over matching a specific layout.

use std::collections::HashMap;
use std::fmt::Write;

use crate::ast::*;
use crate::sema::{Program, TypeInfo, TypeKind};

mod builtins;
mod decls;
mod expr;
mod literals;
mod stmt;
mod types;

/// Generate the `.cpp` for a module, or `None` if it has no class to implement.
/// Uses the default buried-`Null<T>` extraction depth (1).
pub fn generate_source(prog: &Program, module_index: usize) -> Option<String> {
    generate_source_diagnostics(prog, module_index, 1, false).map(|(text, _, _)| text)
}

/// Generated `.cpp` text plus the `(warnings, errors)` collected during body
/// generation — each diagnostic paired with its source line.
pub type SourceOutput = (String, Vec<(usize, String)>, Vec<(usize, String)>);

/// Like [`generate_source`], but also returns `(warnings, errors)` for the driver
/// to surface. Warnings are lint-level (currently nullable `Null<T>` handling);
/// errors are hard "do not guess" cases detected during body generation (an
/// overloaded call matching no `@:overload` signature) and fail the run.
/// `extract_depth` is the maximum expression-nesting depth at which a buried
/// `Null<T>` call is auto-extracted into an owned local instead of warned about.
pub fn generate_source_diagnostics(
    prog: &Program,
    module_index: usize,
    extract_depth: usize,
    no_trace: bool,
) -> Option<SourceOutput> {
    let m = &prog.modules[module_index];
    if m.is_stdafx || !prog.generates_header(m) {
        return None;
    }
    let has_class = m
        .file
        .decls
        .iter()
        .any(|d| matches!(d, Decl::Class(c) if !c.is_extern && !has_meta(&c.meta, "proxy")));
    let free_fns: Vec<&GlobalVar> = m
        .file
        .decls
        .iter()
        .filter_map(|d| match d {
            Decl::Global(g) if lambda_parts(g).is_some() => Some(g),
            _ => None,
        })
        .collect();
    // `@:abi` functions → `extern "C"` exports defined at global scope.
    let extern_fns: Vec<&Function> = m
        .file
        .decls
        .iter()
        .filter_map(|d| match d {
            Decl::Function(f) if has_meta(&f.meta, "abi") => Some(f),
            _ => None,
        })
        .collect();
    // Plain module-level `function name(...) {...}` → namespace free functions
    // (unlike the lambda form, these have a real signature and body). A `@:abi`
    // function is *not* one of these — it is a global `extern "C"` export.
    let plain_fns: Vec<&Function> = m
        .file
        .decls
        .iter()
        .filter_map(|d| match d {
            Decl::Function(f)
                if !f.modifiers.is_macro && !has_meta(&f.meta, "abi") && f.body.is_some() =>
            {
                Some(f)
            }
            _ => None,
        })
        .collect();
    if !has_class && free_fns.is_empty() && extern_fns.is_empty() && plain_fns.is_empty() {
        return None; // headers-only (enums/typedefs/interfaces)
    }

    let stem = m
        .path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Module");
    let mut out = String::new();
    let _ = writeln!(out, "#include \"{stem}.h\"");
    out.push('\n');

    // The namespace wraps classes and `final`-lambda free functions. A file whose
    // only output is an `extern "C"` export has no namespace block at all (the
    // export lives at global scope, below).
    let has_ns_body = has_class || !free_fns.is_empty() || !plain_fns.is_empty();
    if has_ns_body {
        for part in &m.package {
            let _ = writeln!(out, "namespace {part} {{");
        }
        out.push('\n');
    }

    let mut warnings: Vec<(usize, String)> = Vec::new();
    let mut errors: Vec<(usize, String)> = Vec::new();

    // File-scoped (`private`) `final` constants → `static const` definitions inside
    // the namespace (file-local linkage), before the impls that reference them.
    // Scalar (integral/float/String) and struct constants are written directly;
    // a `std::vector`/`std::map` (which C++98 cannot brace-initialise) is built by
    // a one-off helper assigned to a `const` container object (the symbol stays a
    // vector/map, per the container rule). Both scalar and value finals use the one
    // `static const` mechanism — there is no `#define` form. Native finals come
    // from the C++ engine and are not emitted (references are namespace-qualified).
    if has_ns_body {
        let file_finals: Vec<&GlobalVar> = m
            .file
            .decls
            .iter()
            .filter_map(|d| match d {
                Decl::Global(g)
                    if g.is_final
                        && !g.is_extern
                        && g.access == Access::Private
                        && lambda_parts(g).is_none() =>
                {
                    Some(g)
                }
                _ => None,
            })
            .collect();
        if !file_finals.is_empty() {
            let empty = empty_class();
            let mut bg = BodyGen::new(prog, module_index, &empty, extract_depth, no_trace);
            for g in &file_finals {
                if let Some(text) = bg.file_scope_const(g) {
                    out.push_str(&text);
                }
            }
            warnings.append(&mut bg.warnings);
            errors.append(&mut bg.errors);
            out.push('\n');
        }
    }

    // Top-level `final NAME = function/lambda` become namespace free functions.
    // File-local (`private`) ones get forward declarations so they can call each
    // other regardless of definition order.
    if !free_fns.is_empty() {
        let empty = empty_class();
        let mut fwd = String::new();
        for g in &free_fns {
            if g.access == Access::Private {
                let mut bg = BodyGen::new(prog, module_index, &empty, extract_depth, no_trace);
                if let Some(sig) = bg.free_fn_signature(g, true) {
                    let _ = writeln!(fwd, "\tstatic {sig};");
                }
            }
        }
        if !fwd.is_empty() {
            out.push_str(&fwd);
            out.push('\n');
        }
        for g in &free_fns {
            let mut bg = BodyGen::new(prog, module_index, &empty, extract_depth, no_trace);
            out.push_str(&bg.free_fn_def(g));
            warnings.append(&mut bg.warnings);
            errors.append(&mut bg.errors);
            out.push('\n');
        }
    }

    // Plain module-level `function`s become namespace free functions. As with the
    // lambda form, file-local (`private`) ones get a forward declaration so they can
    // be called regardless of definition order; public ones are declared in the header.
    if !plain_fns.is_empty() {
        let empty = empty_class();
        let mut fwd = String::new();
        for f in &plain_fns {
            if f.access == Access::Private {
                let mut bg = BodyGen::new(prog, module_index, &empty, extract_depth, no_trace);
                if let Some(sig) = bg.plain_fn_signature(f, false) {
                    let _ = writeln!(fwd, "\tstatic {sig};");
                }
            }
        }
        if !fwd.is_empty() {
            out.push_str(&fwd);
            out.push('\n');
        }
        for f in &plain_fns {
            let mut bg = BodyGen::new(prog, module_index, &empty, extract_depth, no_trace);
            out.push_str(&bg.plain_fn_def(f));
            warnings.append(&mut bg.warnings);
            errors.append(&mut bg.errors);
            out.push('\n');
        }
    }

    for decl in &m.file.decls {
        if let Decl::Class(c) = decl {
            // `extern` classes live in hand-written C++; `@proxy` glue classes are
            // never emitted (a consume proxy transpiles *as* its native extern; a
            // produce proxy is a base the modules subclass). Neither has a `.cpp`.
            if c.is_extern || has_meta(&c.meta, "proxy") {
                continue;
            }
            let mut g = BodyGen::new(prog, module_index, c, extract_depth, no_trace);
            out.push_str(&g.class_impl());
            warnings.append(&mut g.warnings);
            errors.append(&mut g.errors);
            // Advisory diagnostics for `@owned`/`@delete` overrides that look unsound to
            // the escape analysis (the tags are still obeyed; this only flags a likely
            // double-free / use-after-free).
            warnings.extend(crate::sema::escape::advisory_warnings(
                prog,
                module_index,
                c,
            ));
        }
    }

    if has_ns_body {
        for _ in m.package.iter().rev() {
            let _ = writeln!(out, "}}");
        }
    }

    // `extern inline` exports are defined at global scope (outside any namespace),
    // with every referenced type fully qualified (the generator's namespace is
    // emptied for this).
    if !extern_fns.is_empty() {
        let empty = empty_class();
        for f in &extern_fns {
            if has_ns_body {
                out.push('\n');
            }
            let mut bg = BodyGen::new(prog, module_index, &empty, extract_depth, no_trace);
            bg.ns = Vec::new();
            out.push_str(&bg.extern_fn_def(f));
            warnings.append(&mut bg.warnings);
            errors.append(&mut bg.errors);
        }
    }

    Some((out, warnings, errors))
}

/// The constructor and method **definitions** for one class, each marked `inline`
/// and qualified out-of-line (`inline Ret Class::method(...) { … }`), for placement
/// inside the class's namespace in a header-only amalgamation. The class
/// *declaration* (and its inline destructor) is emitted separately by the header
/// generator; this supplies only the bodies that would otherwise live in a `.cpp`.
/// Returns `(text, warnings, errors)` with each diagnostic paired to its source line.
pub fn inline_class_defs(prog: &Program, mi: usize, class: &Class) -> SourceOutput {
    let mut g = BodyGen::new(prog, mi, class, prog.extract_depth, prog.no_trace);
    g.inline_defs = true;
    let text = g.class_impl();
    (text, g.warnings, g.errors)
}

/// `true` if this module-level `final` binds a function/lambda (so it lowers to a
/// namespace free function defined in the `.cpp`). Used by the driver to reject
/// module-level free functions in `--header-only` mode, where there is no `.cpp` to
/// hold the definition.
pub fn is_free_fn_global(g: &GlobalVar) -> bool {
    lambda_parts(g).is_some()
}

/// A lightweight description of an expression's C++ type, enough to drive member
/// access and pointer handling.
#[derive(Clone, Default)]
struct Ty {
    /// C++ spelling without a trailing `*` (the base).
    base: String,
    /// Whether the C++ value is a pointer.
    is_ptr: bool,
    /// The resolved user/native type, for member lookup.
    info: Option<TypeInfo>,
    /// `true` when this type came from a `Null<T>` (a nullable value type lowered
    /// to a pointer); drives the nullable-handling lint warnings.
    nullable: bool,
    /// `true` when the C++ value is an unsigned size (`.length` → `.size()` /
    /// `.length()`, all `size_t`). A signed/unsigned comparison against such a
    /// value warns under MSVC (C4018); the comparison lowering makes the implicit
    /// `int → size_t` conversion explicit with a `(size_t)` cast to silence it.
    unsigned: bool,
    /// When set, this local aliases a `Map.get(k)` result, which is `Null<V>`.
    /// A value type `V` has no C++ null, so the local is bound to a map *iterator*:
    /// a null check lowers to the existence check (`it == map.end()`) and any
    /// value/member use to `it->second`. See [`IterAlias`].
    iter: Option<Box<IterAlias>>,
}

/// A local bound to a `Map.get(k)` result, represented as a map iterator.
#[derive(Clone)]
struct IterAlias {
    /// The C++ iterator local (`std::map<K,V>::iterator it = map.find(k);`).
    it_name: String,
    /// The already-generated map receiver code, for the `it == map.end()` check.
    map_code: String,
    /// The map's value type `V` (the type of `it->second`).
    value_ty: Ty,
}

struct BodyGen<'a> {
    prog: &'a Program,
    mi: usize,
    ns: Vec<String>,
    class: &'a Class,
    scopes: Vec<HashMap<String, Ty>>,
    /// Per-scope Haxe→C++ identifier renames, used when a local would otherwise
    /// shadow a name already in scope (illegal at C++ function scope).
    renames: Vec<HashMap<String, String>>,
    tmp: usize,
    /// When the class being generated is an `abstract` newtype, the C++ type of
    /// its underlying value. Inside its methods, Haxe `this` denotes that value,
    /// so `this` lowers to the synthetic `this->__this` member (of this type)
    /// rather than the C++ `this` pointer.
    abstract_this: Option<Ty>,
    current_ret: Ty,
    /// Lines that must be emitted before the current statement (temporaries for
    /// anonymous struct literals, comprehensions, etc.).
    prelude: String,
    prelude_ind: usize,
    /// Name of the function/constructor currently being generated (for warnings).
    current_fn: String,
    /// Source line (1-based) of the statement currently being generated, attached
    /// to any lint warning it raises. 0 when unknown.
    current_line: usize,
    /// Nesting depth of the expression currently being generated (1 = top-level /
    /// sink position). Drives buried-`Null<T>` detection.
    expr_depth: usize,
    /// Maximum expression depth at which a buried `Null<T>` call is auto-extracted
    /// into an owned local rather than warned about (the `--depth` flag; 1 = only
    /// sink-position calls are extracted, deeper ones warn).
    max_extract_depth: usize,
    /// Lint warnings about nullable (`Null<T>`) handling, paired with the source
    /// line that triggered them, surfaced by the driver.
    warnings: Vec<(usize, String)>,
    /// Hard errors raised during body generation (paired with the source line):
    /// the "do not guess" cases that have to wait until codegen because they need
    /// expression-type inference — currently an overloaded call whose argument
    /// types match no `@:overload` signature. These fail the run.
    errors: Vec<(usize, String)>,
    /// Per-scope heap locals the scope owns and must `delete` when it closes
    /// (fresh `new`s / nullable results that do not escape to a field or return).
    owned: Vec<Vec<String>>,
    /// Local names that escape the current function (assigned to a field or
    /// returned), so their heap value is owned elsewhere and must not be freed here.
    escaping: std::collections::HashSet<String>,
    /// While generating a statement whose value escapes (a field assignment or a
    /// `return`), `new` arguments are owned by the receiver, not this scope, so
    /// they are emitted inline rather than hoisted into owned locals.
    new_args_escape: bool,
    /// Container (`Array`/`Map`) parameters of the function currently being
    /// generated. Haxe containers are shared by reference — mutating one inside
    /// a function is visible to the caller — but Hatchet lowers container
    /// parameters to `const&` values, so a mutation through a parameter is
    /// flagged with a lint warning (ahead of the C++ const error). A local
    /// shadowing the name drops it from the set.
    container_params: std::collections::HashSet<String>,
    /// Loop nesting depth at the statement currently being generated. Drives the
    /// `break`-in-`switch` lowering (Haxe `switch` has no break semantics — a
    /// `break` in a case body exits the enclosing *loop*).
    loop_depth: usize,
    /// While generating the case bodies of a C++ `switch` that sits inside a
    /// loop and contains a loop-bound `break`: the hoisted flag a user `break`
    /// must set (`f = true; break;` exits the switch; `if (f) break;` after the
    /// switch exits the loop). Cleared inside nested loops, whose `break`s bind
    /// to themselves; chained for switches nested in other switches' cases.
    switch_break_flag: Option<String>,
    /// Pointer fields this class allocates with `new` (owned). They are
    /// NULL-initialised in the constructor and `delete`d before reassignment.
    owned_fields: std::collections::HashSet<String>,
    /// Container fields this class owns (frees in its destructor). A `new` pushed
    /// into one of these *escapes* the current scope, so it is emitted inline
    /// rather than hoisted into a scope-owned local that would be wrongly deleted.
    owned_containers: std::collections::HashSet<String>,
    /// The expected C++ type of the value expression currently being generated,
    /// set at sinks where the target type is known (a `var` initialiser or an
    /// assignment RHS). It supplies the *contextual* return-type inference for
    /// `Array.map` whose lambda body is an object literal (the element type comes
    /// from the assignment/declaration target). Consumed (taken) when used.
    expected: Option<Ty>,
    /// When set (`--no-traces`), `trace(...)` calls are stripped entirely (lowered
    /// to a no-op, arguments not evaluated), mirroring hxcpp's `-D no-traces`.
    no_trace: bool,
    /// Catch-variable names bound by an enclosing **non-binding** catch
    /// (`catch (...)`, from an untyped or `Dynamic` catch). C++ `catch (...)` cannot
    /// bind the value, so a reference to one of these names is a hard error (rather
    /// than silently emitting an undeclared identifier).
    nonbinding_catch_vars: Vec<String>,
    /// When set, each out-of-line constructor/method definition is prefixed with
    /// `inline` so it can live in a header included from several translation units
    /// (`--header-only` emission), instead of being defined once in a `.cpp`.
    inline_defs: bool,
}

impl<'a> BodyGen<'a> {
    fn new(
        prog: &'a Program,
        mi: usize,
        class: &'a Class,
        max_extract_depth: usize,
        no_trace: bool,
    ) -> Self {
        let ns = prog.modules[mi].package.clone();
        // M5 cutover (consumer #2): the container receiver names into which a pushed
        // `new` escapes to class-level storage (so it is emitted inline, not hoisted).
        let owned_containers: std::collections::HashSet<String> =
            crate::sema::escape::escaping_push_receivers(prog, mi, class)
                .into_iter()
                .collect();
        // M5 cutover (consumer #3): the scalar pointer fields this object owns —
        // NULL-initialised in the constructor and freed before reassignment. Sourced
        // from the escape analysis (destructor-owned set), then filtered to the same
        // shape the prior `owned_pointer_fields` heuristic produced: scalar pointers
        // only (containers are value vectors, not NULL-initialised), and excluding
        // `@owned` injected fields (always assigned, never NULL-initialised here).
        let class_owned = crate::sema::escape::analyze_class(prog, mi, class).owned_fields;
        let owned_fields: std::collections::HashSet<String> = class
            .fields
            .iter()
            .filter(|f| class_owned.contains(&f.name))
            .filter(|f| !f.meta.iter().any(|m| m.name == "owned"))
            .filter(|f| {
                f.ty.as_ref()
                    .is_some_and(|t| prog.map_type_use(t, mi, &ns).ends_with('*'))
            })
            .map(|f| f.name.clone())
            .collect();
        BodyGen {
            prog,
            mi,
            ns,
            class,
            scopes: vec![HashMap::new()],
            renames: vec![HashMap::new()],
            tmp: 0,
            // Set per-method (where `self.ty_of` is available) for abstract classes.
            abstract_this: None,
            current_ret: Ty::default(),
            prelude: String::new(),
            prelude_ind: 1,
            current_fn: String::new(),
            current_line: 0,
            expr_depth: 0,
            max_extract_depth: max_extract_depth.max(1),
            warnings: Vec::new(),
            errors: Vec::new(),
            owned: vec![Vec::new()],
            escaping: std::collections::HashSet::new(),
            new_args_escape: false,
            container_params: std::collections::HashSet::new(),
            loop_depth: 0,
            switch_break_flag: None,
            owned_fields,
            owned_containers,
            expected: None,
            no_trace,
            nonbinding_catch_vars: Vec::new(),
            inline_defs: false,
        }
    }

    /// Record a heap local the current scope owns (to be `delete`d at scope close).
    fn register_owned(&mut self, name: &str) {
        if let Some(top) = self.owned.last_mut() {
            top.push(name.to_string());
        }
    }

    /// Transfer ownership of a local out of the current function: drop it from
    /// every scope's owned set so it is not freed at scope close. Used when a
    /// scope-owned local is handed to a `@sink` parameter (the callee now owns
    /// it) — the call-site counterpart to emitting a `new` argument inline.
    fn transfer_owned(&mut self, name: &str) {
        for scope in self.owned.iter_mut() {
            scope.retain(|n| n != name);
        }
    }

    /// Emit a function body's scope-closing `delete`s — UNLESS the body ends in a
    /// tail `return`. A tail return already freed the owned locals (the `return`
    /// emitter does so before exiting) and nothing falls through past it, so the
    /// closing-brace deletes would be unreachable dead code AND a spurious second
    /// free of each owned local. Every function-body emitter funnels through here
    /// so the guard cannot drift between them.
    fn emit_body_close_deletes(&mut self, stmts: &[Stmt], out: &mut String, ind: usize) {
        if !matches!(stmts.last(), Some(Stmt::Return(..))) {
            self.emit_owned_deletes(out, ind);
        }
    }

    /// Emit `delete` statements for the current scope's owned locals, in reverse
    /// order of acquisition. Called just before the scope's closing brace.
    fn emit_owned_deletes(&mut self, out: &mut String, ind: usize) {
        let t = "\t".repeat(ind);
        if let Some(top) = self.owned.last() {
            for name in top.iter().rev() {
                let _ = writeln!(out, "{t}delete {name};");
            }
        }
    }

    /// Are there any owned heap locals live in any open scope?
    fn has_owned_locals(&self) -> bool {
        self.owned.iter().any(|s| !s.is_empty())
    }

    /// Emit `delete` statements for the owned locals of EVERY open scope (innermost
    /// scope first, reverse acquisition order within each). Used by an early
    /// `return`, which exits the whole function before the per-scope closing-brace
    /// deletes would run — without this the heap locals would leak.
    fn emit_all_owned_deletes(&self, out: &mut String, ind: usize) {
        let t = "\t".repeat(ind);
        for scope in self.owned.iter().rev() {
            for name in scope.iter().rev() {
                let _ = writeln!(out, "{t}delete {name};");
            }
        }
    }

    /// Emit a value `return`, freeing owned locals first. When the scope owns heap
    /// locals the value is captured into a temporary *before* the `delete`s, so a
    /// returned expression that reads an owned local is evaluated while it is still
    /// alive (and the freed pointers are not the one being returned — a returned
    /// bare local escapes and is never registered as owned).
    fn finish_return(&mut self, out: &mut String, ind: usize, value: String) {
        let t = "\t".repeat(ind);
        if self.has_owned_locals() {
            let spell = self.decl_spelling(&self.current_ret.clone());
            let tmp = self.fresh("ret");
            let _ = writeln!(out, "{t}{spell} {tmp} = {value};");
            self.emit_all_owned_deletes(out, ind);
            let _ = writeln!(out, "{t}return {tmp};");
        } else {
            let _ = writeln!(out, "{t}return {value};");
        }
    }

    /// Start a function body: reset the escape set for its statements.
    fn begin_fn_body(&mut self, body: &[Stmt]) {
        self.escaping.clear();
        let fields: std::collections::HashSet<String> =
            self.class.fields.iter().map(|f| f.name.clone()).collect();
        collect_escaping(body, &fields, &mut self.escaping);
    }

    fn warn(&mut self, msg: String) {
        let ctx = if self.current_fn.is_empty() {
            String::new()
        } else {
            format!("{}: ", self.current_fn)
        };
        self.warnings
            .push((self.current_line, format!("{ctx}{msg}")));
    }

    /// Lint a container mutation through a parameter. Haxe `Array`/`Map` are
    /// shared by reference — the caller sees a mutation made inside the
    /// function — but Hatchet lowers container parameters to `const&` values, so
    /// this neither compiles (const) nor would match Haxe if it did. Warn at
    /// the Haxe line, ahead of the C++ error. `what` names the mutation
    /// (`push`, `set`, `[i] = …`).
    fn warn_if_param_container_mutated(&mut self, recv: &Expr, what: &str) {
        let Expr::Ident(name) = recv else { return };
        if !self.container_params.contains(name) {
            return;
        }
        let kind = match self.lookup_local(name) {
            Some(t) if t.base.starts_with("std::vector") => "an Array",
            Some(t) if t.base.starts_with("std::map") => "a Map",
            _ => return,
        };
        self.warn(format!(
            "`{what}` mutates `{name}`, {kind} parameter — Haxe containers are shared by \
             reference (the caller would see this change), but Hatchet passes containers by \
             value (`const&`), so the mutation is lost and the generated C++ will not compile; \
             mutate a local copy and return it, or hold the container in a class"
        ));
    }

    fn err(&mut self, msg: String) {
        let ctx = if self.current_fn.is_empty() {
            String::new()
        } else {
            format!("{}: ", self.current_fn)
        };
        self.errors.push((self.current_line, format!("{ctx}{msg}")));
    }

    /// Emit (and clear) any hoisted prelude lines accumulated while generating
    /// the current statement's expressions.
    fn flush(&mut self, out: &mut String) {
        if !self.prelude.is_empty() {
            out.push_str(&self.prelude);
            self.prelude.clear();
        }
    }
}

// ---- free helpers ------------------------------------------------------

fn ty_named_info(prog: &Program, mi: usize, ht: &Type) -> Option<TypeInfo> {
    if let Type::Named { path, params, .. } = ht {
        if params.is_empty() {
            return prog.resolve_type(path, mi).cloned();
        }
    }
    None
}

/// Whether `stmts` contain a `break` that binds to the loop *enclosing* the
/// switch being generated — i.e. one not nested inside an inner loop (whose
/// `break` is its own). Recurses into nested switches: their breaks are still
/// loop-bound in Haxe.
fn stmts_contain_loop_break(stmts: &[Stmt]) -> bool {
    stmts.iter().any(stmt_contains_loop_break)
}

fn stmt_contains_loop_break(st: &Stmt) -> bool {
    match st {
        Stmt::Break => true,
        Stmt::If { then, els, .. } => {
            stmt_contains_loop_break(then) || els.as_deref().is_some_and(stmt_contains_loop_break)
        }
        Stmt::Block(b) => stmts_contain_loop_break(b),
        Stmt::Switch { cases, default, .. } => {
            cases.iter().any(|c| stmts_contain_loop_break(&c.body))
                || default
                    .as_ref()
                    .is_some_and(|d| stmts_contain_loop_break(d))
        }
        Stmt::Try { body, catches, .. } => {
            stmt_contains_loop_break(body)
                || catches.iter().any(|c| stmts_contain_loop_break(&c.body))
        }
        // a break inside a nested loop binds to that loop, not ours
        Stmt::For { .. } | Stmt::While { .. } => false,
        _ => false,
    }
}

/// The variant name a destructuring `case` pattern's callee names (`Add(a, b)`
/// → `Add`; `Op.Add(a, b)` → `Add`).
fn call_pattern_variant(callee: &Expr) -> String {
    match callee {
        Expr::Ident(n) => n.clone(),
        Expr::Field(_, n) => n.clone(),
        _ => String::new(),
    }
}

/// The method an external read of property `f` routes through (see
/// [`BodyGen::field_getter`]). Mirrors the header side's `generated_getter`.
fn getter_method(f: &Field) -> Option<String> {
    if f.get == PropAccess::Get {
        return Some(format!("get_{}", f.name));
    }
    if f.get == PropAccess::Default && f.set != PropAccess::Default {
        return Some(format!("Get{}", cap(&f.name)));
    }
    None
}

/// Collect local names that escape a function body — assigned to a field
/// (`this.f = x`) or returned (`return x`) — so their heap value is owned
/// elsewhere and must not be freed at scope close.
fn collect_escaping(
    stmts: &[Stmt],
    fields: &std::collections::HashSet<String>,
    out: &mut std::collections::HashSet<String>,
) {
    for st in stmts {
        match st {
            Stmt::Expr(
                Expr::Assign {
                    op: None,
                    target,
                    value,
                },
                _,
            ) => {
                // A local stored into a field — `this.field = local` or the bare
                // `field = local` — escapes this scope (the field now owns it).
                let into_field = match &**target {
                    Expr::Field(recv, _) => matches!(**recv, Expr::This),
                    Expr::Ident(name) => fields.contains(name),
                    _ => false,
                };
                if into_field {
                    if let Expr::Ident(name) = &**value {
                        out.insert(name.clone());
                    }
                }
            }
            Stmt::Return(Some(Expr::Ident(name)), _) => {
                out.insert(name.clone());
            }
            Stmt::If { then, els, .. } => {
                collect_escaping(std::slice::from_ref(then), fields, out);
                if let Some(e) = els {
                    collect_escaping(std::slice::from_ref(e), fields, out);
                }
            }
            Stmt::For { body, .. } | Stmt::While { body, .. } => {
                collect_escaping(std::slice::from_ref(body), fields, out);
            }
            Stmt::Block(stmts) => collect_escaping(stmts, fields, out),
            Stmt::Switch { cases, default, .. } => {
                for c in cases {
                    collect_escaping(&c.body, fields, out);
                }
                if let Some(d) = default {
                    collect_escaping(d, fields, out);
                }
            }
            _ => {}
        }
    }
}

/// Is this a `new Array<T>()` / `new Map<K,V>()` (a value container, not a heap
/// pointer)?
/// A `new T(...)` that lowers to a C++ *value*, not a heap pointer: the
/// containers (`Array`/`Map` → `std::vector`/`std::map`) and `String`
/// (→ `std::string`). These are never owned/deleted.
/// If `init` is a `map.get(k)` call, return `(map_receiver, key)` for binding it
/// as an iterator alias. Only the `.get(k)` form is recognised (the explicit
/// nullable map accessor); a plain `map[k]` read would extend here the same way.
fn map_get_init(init: Option<&Expr>) -> Option<(&Expr, &Expr)> {
    if let Some(Expr::Call(target, args)) = init {
        if let Expr::Field(map_expr, method) = &**target {
            if method == "get" && args.len() == 1 {
                return Some((map_expr, &args[0]));
            }
        }
    }
    None
}

/// The trailing value expression of a `switch`-expression arm: the last statement
/// if it is a bare expression statement (the value the arm evaluates to). `None`
/// for an arm that ends in control flow (`return`/`throw`) or is empty.
fn case_value_expr(body: &[Stmt]) -> Option<&Expr> {
    match body.last() {
        Some(Stmt::Expr(e, _)) => Some(e),
        _ => None,
    }
}

/// A copy of a `switch`-expression arm body with its trailing value expression
/// rewritten to assign to `tmp` (`… expr` → `… tmp = expr`). An arm that ends in
/// control flow is left unchanged (it yields no value through fall-through).
fn assign_last_to(body: &[Stmt], tmp: &str) -> Vec<Stmt> {
    let mut out = body.to_vec();
    if let Some(Stmt::Expr(e, line)) = out.last() {
        let assign = Stmt::Expr(
            Expr::Assign {
                op: None,
                target: Box::new(Expr::Ident(tmp.to_string())),
                value: Box::new(e.clone()),
            },
            *line,
        );
        *out.last_mut().unwrap() = assign;
    }
    out
}

/// The trailing value expression of a value-`if` branch: the last expression of a
/// `{ … }` block, the first branch's value of a nested `if`, or a bare value
/// expression. `None` for a branch that ends in control flow or is empty.
fn branch_value_expr(e: &Expr) -> Option<&Expr> {
    match e {
        Expr::Block(stmts) => case_value_expr(stmts),
        Expr::If { then, .. } => branch_value_expr(then),
        other => Some(other),
    }
}

/// Rewrite a value-`if` branch into a statement whose trailing value assigns to
/// `tmp`. A block keeps its statements (the last assigning to `tmp`); a nested
/// `if` recurses into its own branches; a bare value becomes `tmp = value`.
fn branch_assign_to(e: &Expr, tmp: &str) -> Stmt {
    match e {
        Expr::Block(stmts) => Stmt::Block(assign_last_to(stmts, tmp)),
        Expr::If { cond, then, els } => Stmt::If {
            cond: (**cond).clone(),
            then: Box::new(branch_assign_to(then, tmp)),
            els: els.as_ref().map(|e| Box::new(branch_assign_to(e, tmp))),
            line: 0,
        },
        other => Stmt::Expr(
            Expr::Assign {
                op: None,
                target: Box::new(Expr::Ident(tmp.to_string())),
                value: Box::new(other.clone()),
            },
            0,
        ),
    }
}

fn is_value_new(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Named { path, .. }
            if matches!(
                path.last().map(|s| s.as_str()),
                Some("Array") | Some("Map") | Some("String") | Some("StringBuf")
            )
    )
}

/// Is this declared type an explicit `Null<T>`?
fn is_null_type(ty: &Option<Type>) -> bool {
    matches!(
        ty,
        Some(Type::Named { path, params, .. })
            if path.last().map(|s| s.as_str()) == Some("Null") && params.len() == 1
    )
}

/// A top-level `final NAME = function/lambda` — the source of a free function.
/// Lambda params with any missing type filled from a function-type binding
/// annotation: `Cross:(Vector, Vector) -> Float = (a, b) -> a.x * b.y` leaves
/// `a`/`b` unannotated on the arrow, but the binding's `(Vector, Vector)` types
/// them — without this they would default to `int` and `a.x` would be invalid.
/// An arrow param that *is* annotated wins over the binding's type.
fn effective_lambda_params(params: &[Param], decl_ty: Option<&Type>) -> Vec<Param> {
    let func_params = match decl_ty {
        Some(Type::Func { params, .. }) => Some(params),
        _ => None,
    };
    params
        .iter()
        .enumerate()
        .map(
            |(i, p)| match (&p.ty, func_params.and_then(|fps| fps.get(i))) {
                (None, Some(fp)) => Param {
                    ty: Some(fp.clone()),
                    ..p.clone()
                },
                _ => p.clone(),
            },
        )
        .collect()
}

fn lambda_parts(g: &GlobalVar) -> Option<(&Vec<Param>, &Option<Type>, &LambdaBody)> {
    if !g.is_final {
        return None;
    }
    match &g.init {
        Some(Expr::Lambda { params, ret, body }) => Some((params, ret, body)),
        _ => None,
    }
}

/// A throwaway empty class, used to seed a body generator for free functions
/// (which have no enclosing class / `this`).
fn empty_class() -> Class {
    Class {
        name: String::new(),
        type_params: Vec::new(),
        extends: None,
        implements: Vec::new(),
        is_extern: false,
        is_final: false,
        is_abstract: false,
        abstract_underlying: None,
        line: 0,
        meta: Vec::new(),
        fields: Vec::new(),
        methods: Vec::new(),
        ctor: None,
    }
}

/// Render one file-scoped `final` constant as a `static const` definition (inside
/// the namespace, one tab). Shared by the source generator (private finals → `.cpp`)
/// and the header generator (public finals → `.h`). Returns `None` for finals that
/// are not constants (function/lambda finals) or cannot be lowered.
pub(crate) fn render_final_const(
    prog: &Program,
    module_index: usize,
    g: &GlobalVar,
) -> Option<String> {
    if lambda_parts(g).is_some() {
        return None;
    }
    let empty = empty_class();
    let mut bg = BodyGen::new(prog, module_index, &empty, 1, false);
    bg.file_scope_const(g)
}

/// Intrinsic constant fields (`Math.POSITIVE_INFINITY`, `Math.PI`, ...).
/// `HUGE_VAL`/`M_PI` come from `<math.h>`, which the target already provides.
fn intrinsic_field(obj: &str, name: &str) -> Option<(String, Ty)> {
    let code = match (obj, name) {
        // `HUGE_VAL` is already a double.
        ("Math", "POSITIVE_INFINITY") => "HUGE_VAL",
        ("Math", "NEGATIVE_INFINITY") => "(-HUGE_VAL)",
        // No portable C++98 NaN constant — inf − inf is NaN per IEEE 754, and
        // old MSVC's `HUGE_VAL` is an extern double (`_HUGE`), so this is runtime
        // arithmetic, not a compile-time-folded constant.
        ("Math", "NaN") => "(HUGE_VAL - HUGE_VAL)",
        // `M_PI` is not standard C++98 / portable to VC6 — use the literal
        // (a plain double literal, full IEEE-754 double precision).
        ("Math", "PI") => "3.141592653589793",
        _ => return None,
    };
    Some((code.to_string(), float_ty()))
}

fn float_ty() -> Ty {
    // Haxe `Float` lowers to C++ `double` (64-bit, matching official targets).
    Ty {
        base: "double".into(),
        ..Default::default()
    }
}

fn int_ty() -> Ty {
    Ty {
        base: "int".into(),
        ..Default::default()
    }
}

/// A Haxe `Int`-typed value backed by an unsigned C++ `size_t` (`.length` →
/// `.size()`/`.length()`). Modelled as `int` for member lookup and arithmetic, but
/// flagged `unsigned` so a comparison against a signed `int` gets the explicit
/// `(size_t)` cast that silences MSVC's C4018.
fn size_ty() -> Ty {
    Ty {
        base: "int".into(),
        unsigned: true,
        ..Default::default()
    }
}

fn bool_ty() -> Ty {
    Ty {
        base: "bool".into(),
        ..Default::default()
    }
}

fn str_ty() -> Ty {
    Ty {
        base: "std::string".into(),
        ..Default::default()
    }
}

/// A bare `Type::Named` from a type name (used to map a parsed overload
/// signature's Haxe type names back through the normal type machinery). A dotted
/// name (`cpp.StdString`, `cpp.Float32`) is split into path segments so the leaf
/// is matched by the primitive/`cpp.*` mapper, exactly as a normally-parsed type.
fn type_from_name(name: &str) -> Type {
    Type::Named {
        path: name.split('.').map(|s| s.to_string()).collect(),
        params: Vec::new(),
        optional: false,
        line: 0,
    }
}

/// Parse one `@:overload(function(p:T, …):R {})` signature into its parameter Haxe
/// type names and the return Haxe type name. Returns `None` if it does not look
/// like a function signature.
fn parse_overload_sig(s: &str) -> Option<(Vec<String>, String)> {
    let s = s.trim();
    let open = s.find('(')?;
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut close = None;
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close?;
    let params = split_top_commas(&s[open + 1..close])
        .into_iter()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        // `name:Type` (or `?name:Type`) → the Type after the first `:`.
        .map(|p| {
            p.split_once(':')
                .map(|(_, t)| t.trim())
                .unwrap_or("")
                .to_string()
        })
        .collect();
    // Return type: after the close paren, `: R` up to the `{` body (or end).
    let ret = s[close + 1..]
        .split_once(':')
        .map(|(_, r)| r.split('{').next().unwrap_or(r).trim().to_string())
        .unwrap_or_default();
    Some((params, ret))
}

/// Split on top-level commas, respecting `<…>`, `(…)` and `[…]` nesting (so a
/// generic parameter like `Map<K, V>` is not split on its inner comma).
fn split_top_commas(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, &b) in s.as_bytes().iter().enumerate() {
        match b {
            b'<' | b'(' | b'[' => depth += 1,
            b'>' | b')' | b']' => depth -= 1,
            b',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

fn is_container_ty(ty: &Ty) -> bool {
    ty.base.starts_with("std::vector") || ty.base.starts_with("std::map")
}

/// Per-position `@sink` flags for a parameter list — positions whose callee
/// consumes (takes ownership of) what is passed, so the caller does not free it.
fn param_sink_flags(params: &[Param]) -> Vec<bool> {
    params
        .iter()
        .map(|p| p.meta.iter().any(|m| m.name == "sink"))
        .collect()
}

/// Container methods that mutate their receiver (the rest — `indexOf`, `copy`,
/// `filter`, `get`, `keys`, … — read it). Drives the mutated-parameter lint.
fn is_mutating_container_method(method: &str) -> bool {
    matches!(
        method,
        "push" | "insert" | "pop" | "shift" | "unshift" | "remove" | "reverse" | "sort" | "set"
    )
}

fn rcode_is_map(ty: &Ty) -> bool {
    ty.base.starts_with("std::map")
}

/// Split a generic-argument list on the top-level comma (depth-0), so
/// `std::string, std::vector<int>` → (`std::string`, `std::vector<int>`).
fn split_top_comma(s: &str) -> Option<(&str, &str)> {
    let b = s.as_bytes();
    let mut depth = 0i32;
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'<' => depth += 1,
            b'>' => depth -= 1,
            b',' if depth == 0 => return Some((&s[..i], &s[i + 1..])),
            _ => {}
        }
    }
    None
}

fn binop(op: BinOp) -> &'static str {
    use BinOp::*;
    match op {
        Add => "+",
        Sub => "-",
        Mul => "*",
        Div => "/",
        Mod => "%",
        Eq => "==",
        Ne => "!=",
        Lt => "<",
        Gt => ">",
        Le => "<=",
        Ge => ">=",
        And => "&&",
        Or => "||",
        BitAnd => "&",
        BitOr => "|",
        BitXor => "^",
        Shl => "<<",
        Shr => ">>",
        // `>>>` is lowered through an unsigned cast before this table is consulted
        // (see `gen_expr` for `Expr::Binary` / `Expr::Assign`); never emitted bare.
        UShr => ">>",
    }
}

fn binop_result_ty(op: BinOp, lhs: Ty) -> Ty {
    use BinOp::*;
    match op {
        Eq | Ne | Lt | Gt | Le | Ge | And | Or => Ty {
            base: "bool".into(),
            ..Default::default()
        },
        UShr => int_ty(),
        _ => lhs,
    }
}

/// Whether an operand is a plain signed C++ `int` — the side of a mixed
/// signed/unsigned comparison that MSVC implicitly converts to `size_t` (C4018).
fn is_signed_int(ty: &Ty) -> bool {
    !ty.is_ptr && !ty.unsigned && ty.base == "int"
}

/// Make a mixed signed/unsigned comparison explicit. C++ already converts the
/// signed operand to the unsigned type before comparing, so wrapping that operand
/// in `(size_t)` is behaviour-preserving — it just silences MSVC's C4018. Casts
/// whichever side is a signed `int` when the other is an unsigned size
/// (`.length`/`.size()`); a no-op when neither or both sides are unsigned.
fn cast_signed_for_unsigned_cmp(l: &mut String, lty: &Ty, r: &mut String, rty: &Ty) {
    if lty.unsigned && is_signed_int(rty) {
        *r = format!("(size_t)({r})");
    } else if rty.unsigned && is_signed_int(lty) {
        *l = format!("(size_t)({l})");
    }
}

/// Whether an inferred C++ type is a statically known integer scalar — the
/// precondition for the Haxe `Int / Int → Float` division lowering. Unknown
/// (empty) bases stay on the plain C++ operator, conservatively.
fn is_int_ty(ty: &Ty) -> bool {
    !ty.is_ptr
        && matches!(
            ty.base.as_str(),
            "int"
                | "unsigned int"
                | "uint8_t"
                | "uint16_t"
                | "uint32_t"
                | "char"
                | "short"
                | "long"
        )
}

/// Whether an inferred C++ type is a floating scalar (Haxe `Float`) — the
/// trigger for the `%` → `fmod` lowering (C++ `%` is integer-only).
fn is_float_base(ty: &Ty) -> bool {
    !ty.is_ptr && matches!(ty.base.as_str(), "float" | "double")
}

fn unop(op: UnOp) -> &'static str {
    match op {
        UnOp::Neg => "-",
        UnOp::Not => "!",
        UnOp::BitNot => "~",
        UnOp::Incr => "++",
        UnOp::Decr => "--",
    }
}

/// A Haxe `Float` literal as C++. Haxe `Float` lowers to `double`, and an
/// unsuffixed C++ floating literal *is* a `double` — emit it unchanged (no `f`
/// suffix, which would truncate it to single precision).
pub(crate) fn float_lit(s: &str) -> String {
    s.to_string()
}

/// Translate the raw source text of a Haxe string literal into the body of a C++
/// string literal (the part between the quotes). The lexer keeps escape
/// sequences *uninterpreted* (Haxe `\n` is stored as backslash + `n`), so this
/// must re-emit each Haxe escape as the matching C++ escape — not blindly double
/// the backslash, which would turn `\n` into a literal backslash-n.
///
/// Most escapes are byte-identical between the two languages and pass straight
/// through. Numeric byte escapes (`\0`, `\xHH`) are normalised to 3-digit octal,
/// which is byte-exact and — unlike C++ `\x` (greedy) or short octal — can never
/// absorb a following digit. Unicode escapes (`\u…`) have no single-byte C++98
/// form on Hatchet's byte-oriented strings; they are passed through verbatim
/// (the backslash preserved) and remain a documented limitation.
pub(crate) fn escape_str(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'\\' && i + 1 < b.len() {
            let n = b[i + 1];
            match n {
                // byte-identical, non-greedy escapes — emit as-is
                b'n' | b't' | b'r' | b'\\' | b'"' => {
                    out.push(b'\\');
                    out.push(n);
                    i += 2;
                }
                // Haxe's escaped single quote needs no escaping inside C++ "..."
                b'\'' => {
                    out.push(b'\'');
                    i += 2;
                }
                // numeric byte escapes → 3-digit octal (byte-exact, non-greedy)
                b'0' => {
                    out.extend_from_slice(b"\\000");
                    i += 2;
                }
                b'x' if i + 3 < b.len()
                    && (b[i + 2] as char).is_ascii_hexdigit()
                    && (b[i + 3] as char).is_ascii_hexdigit() =>
                {
                    let v = u8::from_str_radix(&s[i + 2..i + 4], 16).unwrap();
                    out.extend_from_slice(format!("\\{:03o}", v).as_bytes());
                    i += 4;
                }
                // unknown escape (notably `\u` unicode, unrepresentable as one
                // byte): keep the bytes verbatim, escaping the backslash so the
                // emitted C++ is well-formed rather than silently mistranslated.
                _ => {
                    out.extend_from_slice(b"\\\\");
                    i += 1;
                }
            }
        } else {
            // A bare character. Escape the C++-significant ones, and normalise any
            // raw control bytes (from a multi-line literal) to their escapes.
            match c {
                b'"' => out.extend_from_slice(b"\\\""),
                b'\\' => out.extend_from_slice(b"\\\\"),
                b'\n' => out.extend_from_slice(b"\\n"),
                b'\t' => out.extend_from_slice(b"\\t"),
                b'\r' => out.extend_from_slice(b"\\r"),
                other => out.push(other),
            }
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A segment of an interpolated string: a literal run or a `${...}` expression.
enum Seg {
    Lit(String),
    Expr(String),
}

/// Whether a single-quoted string's raw text carries any interpolation: either
/// the `${expr}` form or the `$ident` shorthand. A `$` followed by anything else
/// (a digit, punctuation, end of string) is a literal dollar sign.
fn has_interpolation(raw: &str) -> bool {
    let b = raw.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'$' && i + 1 < b.len() {
            let n = b[i + 1];
            if n == b'{' || n.is_ascii_alphabetic() || n == b'_' {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Split `'a${b}c'` raw text into ordered segments plus the list of expression
/// sources (for buffer sizing).
fn split_interpolation(raw: &str) -> (Vec<Seg>, Vec<String>) {
    let mut segs = Vec::new();
    let mut exprs = Vec::new();
    let b = raw.as_bytes();
    let mut i = 0;
    let mut lit = String::new();
    while i < b.len() {
        if b[i] == b'$' && i + 1 < b.len() && b[i + 1] == b'{' {
            if !lit.is_empty() {
                segs.push(Seg::Lit(std::mem::take(&mut lit)));
            }
            // find matching close brace
            let mut depth = 1;
            let mut j = i + 2;
            while j < b.len() && depth > 0 {
                match b[j] {
                    b'{' => depth += 1,
                    b'}' => depth -= 1,
                    _ => {}
                }
                if depth == 0 {
                    break;
                }
                j += 1;
            }
            let inner = raw[i + 2..j].to_string();
            exprs.push(inner.clone());
            segs.push(Seg::Expr(inner));
            i = j + 1;
        } else if b[i] == b'$'
            && i + 1 < b.len()
            && (b[i + 1].is_ascii_alphabetic() || b[i + 1] == b'_')
        {
            // `$ident` shorthand
            if !lit.is_empty() {
                segs.push(Seg::Lit(std::mem::take(&mut lit)));
            }
            let mut j = i + 1;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            let inner = raw[i + 1..j].to_string();
            exprs.push(inner.clone());
            segs.push(Seg::Expr(inner));
            i = j;
        } else {
            lit.push(b[i] as char);
            i += 1;
        }
    }
    if !lit.is_empty() {
        segs.push(Seg::Lit(lit));
    }
    (segs, exprs)
}

/// Escape a literal run for use inside a printf/sprintf format string.
fn printf_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
}

fn cap(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
