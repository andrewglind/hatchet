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
        .any(|d| matches!(d, Decl::Class(c) if !has_meta(&c.meta, "native")));
    let free_fns: Vec<&GlobalVar> = m
        .file
        .decls
        .iter()
        .filter_map(|d| match d {
            Decl::Global(g) if lambda_parts(g).is_some() => Some(g),
            _ => None,
        })
        .collect();
    // `extern inline` functions → `extern "C"` exports defined at global scope.
    let extern_fns: Vec<&Function> = m
        .file
        .decls
        .iter()
        .filter_map(|d| match d {
            Decl::Function(f) if f.modifiers.is_extern && !has_meta(&f.meta, "native") => Some(f),
            _ => None,
        })
        .collect();
    if !has_class && free_fns.is_empty() && extern_fns.is_empty() {
        return None; // headers-only (enums/typedefs/interfaces)
    }

    let stem = m.path.file_stem().and_then(|s| s.to_str()).unwrap_or("Module");
    let mut out = String::new();
    let _ = writeln!(out, "#include \"{stem}.h\"");
    out.push('\n');

    // The namespace wraps classes and `final`-lambda free functions. A file whose
    // only output is an `extern "C"` export has no namespace block at all (the
    // export lives at global scope, below).
    let has_ns_body = has_class || !free_fns.is_empty();
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
                        && g.access == Access::Private
                        && !has_meta(&g.meta, "native")
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

    for decl in &m.file.decls {
        if let Decl::Class(c) = decl {
            if has_meta(&c.meta, "native") {
                continue;
            }
            let mut g = BodyGen::new(prog, module_index, c, extract_depth, no_trace);
            out.push_str(&g.class_impl());
            warnings.append(&mut g.warnings);
            errors.append(&mut g.errors);
            // Advisory diagnostics for `@owned`/`@delete` overrides that look unsound to
            // the escape analysis (the tags are still obeyed; this only flags a likely
            // double-free / use-after-free).
            warnings.extend(crate::sema::escape::advisory_warnings(prog, module_index, c));
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
            crate::sema::escape::escaping_push_receivers(prog, mi, class).into_iter().collect();
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
            .filter(|f| f.ty.as_ref().is_some_and(|t| prog.map_type_use(t, mi, &ns).ends_with('*')))
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
            owned_fields,
            owned_containers,
            expected: None,
            no_trace,
        }
    }

    /// Record a heap local the current scope owns (to be `delete`d at scope close).
    fn register_owned(&mut self, name: &str) {
        if let Some(top) = self.owned.last_mut() {
            top.push(name.to_string());
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
        self.warnings.push((self.current_line, format!("{ctx}{msg}")));
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

    fn class_impl(&mut self) -> String {
        let mut s = String::new();
        let name = self.class.name.clone();

        if let Some(ctor) = self.class.ctor.clone() {
            s.push_str(&self.ctor_impl(&name, &ctor));
            s.push('\n');
        }
        for m in self.class.methods.clone() {
            let Some(mname) = m.name.clone() else { continue };
            if self.is_accessor_method(&mname) {
                continue;
            }
            if m.body.is_none() {
                continue;
            }
            s.push_str(&self.method_impl(&name, &m));
            s.push('\n');
        }
        s
    }

    fn ctor_impl(&mut self, class_name: &str, ctor: &Function) -> String {
        // Base-from-member idiom when `super(...)` is not the first statement.
        self.current_fn = "new".to_string();
        if let Some(h) = crate::codegen::holder::analyze(self.prog, self.mi, &self.ns, self.class) {
            return self.holder_ctor_impl(class_name, ctor, &h);
        }

        self.push_scope();
        self.bind_params(&ctor.params);

        let params = self.header_params(&ctor.params);
        // Pull out a leading `super(...)` into the base initialiser list.
        let mut init = String::new();
        let body_stmts = ctor.body.clone().unwrap_or_default();
        let mut start = 0;
        if let Some(Stmt::Expr(Expr::Call(target, args), _)) = body_stmts.first() {
            if matches!(**target, Expr::Super) {
                if let Some(base) = &self.class.extends {
                    let base_name = self.prog.map_type_base(base, self.mi, &self.ns);
                    // Initialiser-list arguments are owned by the base, and run
                    // before the body — so `new` args stay inline, never hoisted.
                    self.new_args_escape = true;
                    let arglist = self.gen_args(args);
                    self.new_args_escape = false;
                    init = format!(" : {base_name}({arglist})");
                    start = 1;
                }
            }
        }
        // NULL-initialise owned pointer fields so `delete` (in the destructor and
        // before reassignment) is always safe.
        init = self.append_owned_null_inits(init);

        self.begin_fn_body(&body_stmts);
        let mut body = String::new();
        for st in &body_stmts[start..] {
            self.gen_stmt(st, 1, &mut body);
        }
        self.emit_owned_deletes(&mut body, 1);
        self.pop_scope();

        format!("\t{class_name}::{class_name}({params}){init} {{\n{body}\t}}\n")
    }

    /// Emit the `XHolder` constructor + the class constructor (with its
    /// `: XHolder(...), Base(...)` initialiser list) for the base-from-member idiom.
    fn holder_ctor_impl(
        &mut self,
        class_name: &str,
        ctor: &Function,
        h: &crate::codegen::holder::Holder,
    ) -> String {
        use crate::codegen::holder::SuperArg;
        let body_stmts = ctor.body.clone().unwrap_or_default();
        let params = self.header_params(&ctor.params);

        // 1. XHolder constructor: pre-super statements (lifted locals become member
        //    assignments) followed by the hoisted super arguments.
        self.push_scope();
        self.bind_params(&ctor.params);
        let mut hbody = String::new();
        for st in &body_stmts[..h.super_idx] {
            self.gen_holder_stmt(st, &h.lifted, 1, &mut hbody);
        }
        for (mname, expr) in &h.hoisted {
            self.prelude_ind = 1;
            let (code, _) = self.gen_expr(expr);
            self.flush(&mut hbody);
            let _ = writeln!(hbody, "\tthis->{mname} = {code};");
        }
        self.pop_scope();
        let holder_ctor =
            format!("\t{0}::{0}({params}) {{\n{hbody}\t}}\n", h.name);

        // 2. Class constructor: initialiser list calls XHolder with all params and
        //    the base with the mapped super arguments; body is the post-super code.
        self.push_scope();
        self.bind_params(&ctor.params);
        let base_name = self
            .class
            .extends
            .as_ref()
            .map(|b| self.prog.map_type_base(b, self.mi, &self.ns))
            .unwrap_or_default();
        let holder_args: Vec<String> = ctor.params.iter().map(|p| p.name.clone()).collect();
        self.new_args_escape = true;
        let super_args: Vec<String> = h
            .super_args
            .iter()
            .map(|a| match a {
                SuperArg::Member(n) => n.clone(),
                SuperArg::PassThrough(e) => self.gen_expr(e).0,
            })
            .collect();
        self.new_args_escape = false;
        let init = format!(
            " : {}({}), {base_name}({})",
            h.name,
            holder_args.join(", "),
            super_args.join(", ")
        );
        let init = self.append_owned_null_inits(init);
        self.begin_fn_body(&body_stmts);
        let mut body = String::new();
        for st in &body_stmts[h.super_idx + 1..] {
            self.gen_stmt(st, 1, &mut body);
        }
        self.emit_owned_deletes(&mut body, 1);
        self.pop_scope();
        let class_ctor = format!("\t{class_name}::{class_name}({params}){init} {{\n{body}\t}}\n");

        format!("{holder_ctor}\n{class_ctor}")
    }

    /// Append `field(NULL)` initialisers for owned pointer fields to a ctor's
    /// initialiser list (in field declaration order).
    fn append_owned_null_inits(&self, init: String) -> String {
        let nulls: Vec<String> = self
            .class
            .fields
            .iter()
            .filter(|f| self.owned_fields.contains(&f.name))
            .map(|f| format!("{}(NULL)", f.name))
            .collect();
        if nulls.is_empty() {
            init
        } else if init.is_empty() {
            format!(" : {}", nulls.join(", "))
        } else {
            format!("{init}, {}", nulls.join(", "))
        }
    }

    /// Generate a pre-super statement for an `XHolder` constructor: a `var` whose
    /// name is lifted becomes a member assignment (`this->name = ...`); everything
    /// else is an ordinary local statement.
    fn gen_holder_stmt(&mut self, st: &Stmt, lifted: &[String], ind: usize, out: &mut String) {
        if let Stmt::Var { name, ty, init: Some(init), .. } = st {
            if lifted.iter().any(|n| n == name) {
                let t = "\t".repeat(ind);
                self.prelude_ind = ind;
                let (code, _) = self.gen_expr(init);
                self.flush(out);
                let _ = writeln!(out, "{t}this->{name} = {code};");
                let decl_ty = ty.as_ref().map(|t| self.ty_of(t)).unwrap_or_default();
                self.define_local(name, decl_ty);
                return;
            }
        }
        self.gen_stmt(st, ind, out);
    }

    fn method_impl(&mut self, class_name: &str, m: &Function) -> String {
        self.push_scope();
        self.bind_params(&m.params);
        self.current_ret = m.ret.as_ref().map(|t| self.ty_of(t)).unwrap_or_default();
        let ret = self.return_type(m);
        let name = m.name.clone().unwrap();
        self.current_fn = name.clone();
        let params = self.header_params(&m.params);
        let stmts = m.body.as_ref().unwrap();
        self.begin_fn_body(stmts);
        let mut body = String::new();
        for st in stmts {
            self.gen_stmt(st, 1, &mut body);
        }
        // A tail `return` already freed the owned locals (and no path falls through
        // past it), so the closing-brace deletes would be unreachable dead code.
        if !matches!(stmts.last(), Some(Stmt::Return(..))) {
            self.emit_owned_deletes(&mut body, 1);
        }
        self.pop_scope();
        format!("\t{ret} {class_name}::{name}({params}) {{\n{body}\t}}\n")
    }

    /// Signature (`ret name(params)`) for a top-level free function; `with_defaults`
    /// keeps any ` = default` suffixes (for the declaration only).
    fn free_fn_signature(&mut self, g: &GlobalVar, with_defaults: bool) -> Option<String> {
        let (params, ret, body) = lambda_parts(g)?;
        let params = effective_lambda_params(params, g.ty.as_ref());
        self.push_scope();
        self.bind_params(&params);
        let ret_ty = self.resolve_lambda_ret(ret, body, g.ty.as_ref());
        let ret_cpp = self.decl_spelling(&ret_ty);
        let plist = if with_defaults {
            params
                .iter()
                .map(|p| crate::codegen::param_decl(self.prog, self.mi, &self.ns, p))
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            self.header_params(&params)
        };
        self.pop_scope();
        Some(format!("{ret_cpp} {}({plist})", g.name))
    }

    /// Full definition of a top-level free function (`static` when file-local).
    fn free_fn_def(&mut self, g: &GlobalVar) -> String {
        let Some((params, ret, body)) = lambda_parts(g) else { return String::new() };
        let params = effective_lambda_params(params, g.ty.as_ref());
        self.current_fn = g.name.clone();
        self.push_scope();
        self.bind_params(&params);
        let ret_ty = self.resolve_lambda_ret(ret, body, g.ty.as_ref());
        self.current_ret = ret_ty.clone();
        let ret_cpp = self.decl_spelling(&ret_ty);
        let plist = self.header_params(&params);
        let prefix = if g.access == Access::Private { "static " } else { "" };

        let mut body_buf = String::new();
        match body {
            LambdaBody::Expr(e) => {
                self.prelude_ind = 1;
                let (c, cty) = self.gen_expr(e);
                self.flush(&mut body_buf);
                let r = if ret_ty.is_ptr && !cty.is_ptr {
                    format!("new {}({c})", ret_ty.base)
                } else {
                    c
                };
                let _ = writeln!(body_buf, "\treturn {r};");
            }
            LambdaBody::Block(stmts) => {
                self.begin_fn_body(stmts);
                for st in stmts {
                    self.gen_stmt(st, 1, &mut body_buf);
                }
                self.emit_owned_deletes(&mut body_buf, 1);
            }
        }
        self.pop_scope();
        format!("\t{prefix}{ret_cpp} {}({plist}) {{\n{body_buf}\t}}\n", g.name)
    }

    /// Full definition of an `extern inline` function as an `extern "C"` export at
    /// **global scope**: `<P>_EXPORT <ret> <P>_CALL name(params) { body }`. The
    /// generator must already have an empty namespace so referenced types are fully
    /// qualified. The signature and braces sit at column 0; the body is indented one
    /// level (the function is not inside a namespace).
    fn extern_fn_def(&mut self, f: &Function) -> String {
        let Some(name) = f.name.clone() else { return String::new() };
        let prefix = self.prog.export_macro.clone();
        self.current_fn = name.clone();
        self.push_scope();
        self.bind_params(&f.params);
        let ret_ty = match &f.ret {
            Some(t) => self.ty_of(t),
            None => Ty { base: "void".to_string(), ..Default::default() },
        };
        self.current_ret = ret_ty.clone();
        let ret_cpp = self.decl_spelling(&ret_ty);
        let plist = self.header_params(&f.params);

        let mut body_buf = String::new();
        if let Some(stmts) = &f.body {
            self.begin_fn_body(stmts);
            for st in stmts {
                self.gen_stmt(st, 1, &mut body_buf);
            }
            self.emit_owned_deletes(&mut body_buf, 1);
        }
        self.pop_scope();
        format!("{prefix}_EXPORT {ret_cpp} {prefix}_CALL {name}({plist}) {{\n{body_buf}}}\n")
    }

    /// Resolve a lambda's C++ return type from, in priority order: (1) the lambda's
    /// own explicit `:T` return annotation, (2) the **function-type annotation on
    /// the binding** — `Square:(Int, Int) -> Int = (a, b) -> a * b` — whose `-> R`
    /// gives the return type, (3) a `cast(expr, T)` arrow body. A developer hints
    /// the return type via (2) or (3); absent any hint it falls back
    /// to `float` (the common case for numeric helpers). `decl_ty` is the binding's
    /// declared type, if any.
    fn resolve_lambda_ret(
        &self,
        ret: &Option<Type>,
        body: &LambdaBody,
        decl_ty: Option<&Type>,
    ) -> Ty {
        if let Some(t) = ret {
            return self.ty_of(t);
        }
        if let Some(Type::Func { ret, .. }) = decl_ty {
            return self.ty_of(ret);
        }
        if let LambdaBody::Expr(Expr::Cast { ty: Some(t), .. }) = body {
            return self.ty_of(t);
        }
        float_ty()
    }

    fn return_type(&self, m: &Function) -> String {
        let mut ret = match &m.ret {
            Some(t) => self.prog.map_type_use(t, self.mi, &self.ns),
            None => "void".to_string(),
        };
        if has_meta(&m.meta, "readOnly") {
            ret = format!("const {ret}");
        }
        ret
    }

    /// Parameters as they appear in the out-of-line definition (same spelling as
    /// the header, but without default values).
    fn header_params(&self, params: &[Param]) -> String {
        params
            .iter()
            .map(|p| {
                let decl = crate::codegen::param_decl(self.prog, self.mi, &self.ns, p);
                // strip any " = default"
                match decl.split_once(" = ") {
                    Some((head, _)) => head.to_string(),
                    None => decl,
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn bind_params(&mut self, params: &[Param]) {
        for p in params {
            let ty = match &p.ty {
                Some(t) => {
                    let mut ty = self.ty_of(t);
                    // optional value-struct parameters are pointers
                    if p.optional && !ty.is_ptr {
                        if let Some(info) = &ty.info {
                            if matches!(info.kind, TypeKind::StructTypedef | TypeKind::AliasTypedef) {
                                ty.is_ptr = true;
                            }
                        }
                    }
                    ty
                }
                None => Ty::default(),
            };
            self.define_local(&p.name, ty);
        }
    }

    // ---- statements ----------------------------------------------------

    fn gen_stmt(&mut self, st: &Stmt, ind: usize, out: &mut String) {
        let t = "\t".repeat(ind);
        self.prelude_ind = ind;
        // By default a statement's `new` arguments are owned by this scope; the
        // Var/Return/field-assignment arms below override this when the value
        // escapes (then the receiver owns the arguments).
        self.new_args_escape = false;
        match st {
            Stmt::Var { name, ty, init, is_final: _, delete, line } => {
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
                        let elem = self.elem_ast_ty(ty.as_ref()).unwrap_or_else(|| self.element_ty(&vec_ty));
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
                    Some(code) => { let _ = writeln!(out, "{t}{cpp} {emit} = {code};"); }
                    None => { let _ = writeln!(out, "{t}{cpp} {emit};"); }
                }
                let is_ptr = var_ty.is_ptr;
                self.define_local(name, var_ty);
                // A non-escaping local holding a fresh `new` / nullable heap result
                // is owned by this scope and deleted when it closes. `@delete` is the
                // developer's explicit override: free this pointer at scope close
                // regardless of what the analysis would infer (e.g. a returned
                // pointer the scope would otherwise leak). Pointer-only — `delete`ing
                // a value local is meaningless.
                let owns_heap = is_heap_new_init(init) || nullable_init;
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
                if self.pushes_new_into_owned_container(e) {
                    self.new_args_escape = true;
                }
                // Assigning into a field stores the value long-term, so its `new`
                // arguments are owned by the receiver, not this scope. This holds
                // whether the field is written `this.field` or bare `field` (Haxe
                // lets you omit `this.`) — the bare form must not be treated as a
                // scope-local, or its `new` args would be wrongly freed at scope
                // close, leaving the field dangling.
                if let Expr::Assign { op: None, target, .. } = e {
                    let own_field = self.assigned_own_field(target);
                    if matches!(&**target, Expr::Field(..)) || own_field.is_some() {
                        self.new_args_escape = true;
                    }
                    // Delete-before-overwrite: reassigning an owned pointer field
                    // (outside the constructor, where it is NULL-initialised) frees
                    // the prior value first.
                    if self.current_fn != "new" {
                        if let Some(field) = &own_field {
                            if self.owned_fields.contains(field) {
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
                        let r = self.return_null_value();
                        self.finish_return(out, ind, r);
                    } else {
                        let val_ty = self.return_value_ty();
                        let elem = self.element_ty(&val_ty);
                        let tmp = self.fresh("ret");
                        self.expand_array_into_local(&tmp, &val_ty, &elem, elems, ind, out);
                        let r = self.wrap_ret(tmp);
                        self.finish_return(out, ind, r);
                    }
                } else {
                    let (c, cty) = self.gen_expr(e);
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
            Stmt::If { cond, then, els, line } => {
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
            Stmt::While { cond, body, do_while, line } => {
                self.current_line = *line;
                self.prelude_ind = ind;
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
            }
            Stmt::For { var, value_var, iter, body, line } => {
                self.current_line = *line;
                self.gen_for(var, value_var.as_deref(), iter, body, ind, out)
            }
            Stmt::Switch { subject, cases, default, line } => {
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
                let _ = writeln!(out, "{t}break;");
            }
            Stmt::Continue => {
                let _ = writeln!(out, "{t}continue;");
            }
            Stmt::Throw(e, line) => {
                self.current_line = *line;
                self.prelude_ind = ind;
                let (c, _) = self.gen_expr(e);
                self.flush(out);
                let _ = writeln!(out, "{t}throw {c};");
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
            // Exception handling is not transpiled. `validate` flags any `try` as
            // unsupported and skips the whole module, so this is never reached in a
            // real run; emit a visible marker rather than silently nothing.
            Stmt::Try { line, .. } => {
                self.current_line = *line;
                let _ = writeln!(out, "{t}/* unsupported: try/catch */");
            }
        }
    }

    fn gen_block(&mut self, st: &Stmt, ind: usize, out: &mut String) {
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

    fn gen_for(
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
                let (e, _) = self.gen_expr(end);
                self.flush(out);
                self.define_local(var, Ty { base: "int".into(), ..Default::default() });
                let lv = self.loop_var(var);
                let _ = writeln!(out, "{t}for (int {lv} = {s}; {lv} < {e}; ++{lv}) {{");
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
                    let _ = writeln!(
                        out,
                        "{t}for ({}::iterator {it} = {access}.begin(); {it} != {access}.end(); ++{it}) {{",
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
                    if value_var.is_some() {
                        self.err("`for (key => value in ...)` is only valid over a Map".into());
                    }
                    let idx = self.fresh("i");
                    let elem_ty = self.element_ty(&cty);
                    let elem_spell = self.decl_spelling(&elem_ty);
                    self.define_local(var, elem_ty);
                    let _ = writeln!(
                        out,
                        "{t}for (size_t {idx} = 0; {idx} < {access}.size(); ++{idx}) {{"
                    );
                    let _ = writeln!(out, "{t}\t{elem_spell} {var} = {access}[{idx}];");
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
    fn gen_comprehension(
        &mut self,
        var: &str,
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
                let (e, _) = self.gen_expr(end);
                self.define_local(var, int_ty());
                let lv = self.loop_var(var);
                (
                    format!("{t}for (int {lv} = {s}; {lv} < {e}; ++{lv}) {{\n"),
                    format!("{t}}}\n"),
                )
            }
            Iterable::Coll(coll) => {
                let (c, cty) = self.gen_expr(coll);
                // A nullable container is a pointer — dereference it to iterate.
                let access = if cty.is_ptr { format!("(*{c})") } else { c };
                let idx = self.fresh("i");
                let elem = self.element_ty(&cty);
                let espell = self.decl_spelling(&elem);
                self.define_local(var, elem);
                (
                    format!(
                        "{t}for (size_t {idx} = 0; {idx} < {access}.size(); ++{idx}) {{\n{t}\t{espell} {var} = {access}[{idx}];\n"
                    ),
                    format!("{t}}}\n"),
                )
            }
        };

        // Generate the body (capturing any hoisted prelude so it lands in-loop).
        let saved = std::mem::take(&mut self.prelude);
        let (push_line, container) = match body {
            ComprBody::Value(e) => {
                let (bcode, bty) = self.gen_expr(e);
                let inner = if bty.base.is_empty() { "int".to_string() } else { self.decl_spelling(&bty) };
                (format!("{t}\t{tmp}.push_back({bcode});\n"), format!("std::vector<{inner} >"))
            }
            ComprBody::KeyValue(k, v) => {
                let (kcode, _) = self.gen_expr(k);
                let (vcode, vty) = self.gen_expr(v);
                let vspell = if vty.base.is_empty() { "void*".to_string() } else { self.decl_spelling(&vty) };
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
        (tmp, Ty { base: String::new(), ..Default::default() })
    }

    /// `coll.map(lambda)` → a hoisted `std::vector` filled by looping over `coll`
    /// and applying the lambda to each element — the **Map-comprehension + Lambda**
    /// composition. The result's element type is taken from the contextual hint
    /// (`self.expected`, the assignment/declaration target) when present, else
    /// inferred from the lambda body. When the body is an object literal it is
    /// expanded into a temporary of that element type (so `{ x:…, y:… }` becomes a
    /// nominal struct, not an anonymous one).
    fn gen_array_map(
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
        let access = if rty.is_ptr { format!("(*{rcode})") } else { rcode.to_string() };
        let in_elem = self.elem_member_ty(rty);
        let in_spell = self.decl_spelling(&in_elem);
        let var = params.first().map(|p| p.name.clone()).unwrap_or_else(|| "_x".to_string());
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
        let vec_ty = Ty { base: format!("std::vector<{out_spell} >"), ..Default::default() };

        let mut buf = String::new();
        let _ = writeln!(buf, "{t}{} {tmp};", self.decl_spelling(&vec_ty));
        let _ = writeln!(buf, "{t}for (size_t {idx} = 0; {idx} < {access}.size(); ++{idx}) {{");
        let _ = writeln!(buf, "{t}\t{in_spell} {var} = {access}[{idx}];");
        buf.push_str(&body_prelude);
        buf.push_str(&push_block);
        let _ = writeln!(buf, "{t}}}");
        self.prelude.push_str(&buf);

        (tmp, vec_ty)
    }

    fn gen_block_inner(&mut self, st: &Stmt, ind: usize, out: &mut String) {
        match st {
            Stmt::Block(stmts) => {
                for s in stmts {
                    self.gen_stmt(s, ind, out);
                }
            }
            other => self.gen_stmt(other, ind, out),
        }
    }

    fn gen_switch(
        &mut self,
        subject: &Expr,
        cases: &[Case],
        default: Option<&[Stmt]>,
        ind: usize,
        out: &mut String,
    ) {
        let t = "\t".repeat(ind);
        let (subj, sty) = self.gen_expr(subject);
        let _ = writeln!(out, "{t}switch ({subj}) {{");
        for case in cases {
            for pat in &case.patterns {
                // enum case labels need the enum-qualified constant
                let label = self.case_label(pat, &sty);
                let _ = writeln!(out, "{t}\tcase {label}:");
            }
            let _ = writeln!(out, "{t}\t{{");
            self.push_scope();
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
    }

    fn case_label(&mut self, pat: &Expr, subj_ty: &Ty) -> String {
        if let Expr::Ident(name) = pat {
            // bare enum variant → qualify with the subject's enum type
            if let Some(info) = &subj_ty.info {
                if info.kind == TypeKind::Enum {
                    return self.enum_constant(info, name).to_string();
                }
            }
        }
        self.gen_expr(pat).0
    }

    // ---- expression entry points ---------------------------------------

    /// Generate a statement-level expression, handling assignment to property
    /// accessors (`a.x = v` → `a->SetX(v)`).
    /// `arr[i] = v` where `arr` lowers to a `std::vector`. Haxe array writes
    /// auto-extend the array — a write past the end grows it, default-filling the
    /// gap — whereas C++ `operator[]` is out-of-bounds UB there. Emit a grow-guard
    /// that resizes first, evaluating the index exactly once. Returns `None` for a
    /// non-vector receiver (a map inserts on write; anything else uses the normal
    /// assignment path), having pushed nothing.
    fn try_array_index_assign(
        &mut self,
        recv: &Expr,
        idx: &Expr,
        value: &Expr,
    ) -> Option<(String, Ty)> {
        let (rcode, rty) = self.gen_expr(recv);
        if !rty.base.starts_with("std::vector") {
            return None;
        }
        let access = if rty.is_ptr && is_container_ty(&rty) {
            format!("(*{rcode})")
        } else {
            rcode
        };
        let (icode, _) = self.gen_expr(idx);
        let ix = self.fresh("ix");
        let t = "\t".repeat(self.prelude_ind);
        self.prelude.push_str(&format!("{t}size_t {ix} = (size_t)({icode});\n"));
        self.prelude
            .push_str(&format!("{t}if ({ix} >= {access}.size()) {access}.resize({ix} + 1);\n"));
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

    fn gen_assign_or_expr(&mut self, e: &Expr) -> (String, Ty) {
        if let Expr::Assign { op: None, target, value } = e {
            // `arr[i] = v` into an Array (→ std::vector): Haxe auto-extends the
            // array on an out-of-range write, so emit a grow-guard first (C++
            // `operator[]` past the end is undefined behaviour). Maps and other
            // receivers fall through to the normal assignment path.
            if let Expr::Index(recv, idx) = &**target {
                if let Some(result) = self.try_array_index_assign(recv, idx, value) {
                    return result;
                }
            }
            // x = []  → x.clear()
            if matches!(&**value, Expr::ArrayLit(v) if v.is_empty()) {
                let (t, _) = self.gen_expr(target);
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
                let (tcode, tty) = self.gen_expr(target);
                if tty.info.is_some() && !tty.base.is_empty() {
                    let tmp = self.hoist_object(fields, tty);
                    return (format!("{tcode} = {tmp}"), Ty::default());
                }
            }
            // plain reassignment: warn when a nullable value lands in a
            // non-nullable target.
            let (tcode, tty) = self.gen_expr(target);
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
            return (format!("{tcode} = {vcode}"), tty);
        }
        self.gen_expr(e)
    }

    /// If `recv.field = value` targets an external property accessor, produce the
    /// `recv->SetField(value)` call.
    fn accessor_set(&mut self, recv: &Expr, field: &str, value: &Expr) -> Option<String> {
        // own fields (`this.x`) are assigned directly, never via a setter
        if matches!(recv, Expr::This) {
            return None;
        }
        let (rcode, rty) = self.gen_expr(recv);
        let info = rty.info.clone()?;
        if !self.field_has_setter(&info, field) {
            return None;
        }
        let (vcode, _) = self.gen_expr(value);
        let op = if rty.is_ptr { "->" } else { "." };
        Some(format!("{rcode}{op}Set{}({vcode})", cap(field)))
    }

    // ---- expression generation -----------------------------------------

    /// Generate an expression, tracking nesting depth so a `Null<T>` call result
    /// can be classified as a *sink* (depth 1 — the whole value of a `var` init,
    /// assignment RHS, `return`, or a bare statement, all of which capture or
    /// auto-extract it) versus *buried* (depth > 1 — nested inside a larger
    /// expression, where its heap result has nowhere to be freed). Grouping-only
    /// wrappers are transparent so `(getEdge())` stays a sink.
    fn gen_expr(&mut self, e: &Expr) -> (String, Ty) {
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

    fn gen_expr_inner(&mut self, e: &Expr) -> (String, Ty) {
        match e {
            Expr::Int(s) => (s.clone(), Ty { base: "int".into(), ..Default::default() }),
            Expr::Float(s) => (float_lit(s), Ty { base: "float".into(), ..Default::default() }),
            Expr::Bool(b) => (b.to_string(), Ty { base: "bool".into(), ..Default::default() }),
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
                ("/* is unsupported */".into(), Ty { base: "bool".into(), ..Default::default() })
            }
            Expr::Str { raw, interpolated } => self.gen_string(raw, *interpolated),
            Expr::This => (
                "this".into(),
                Ty {
                    base: self.class.name.clone(),
                    is_ptr: true,
                    info: self.prog.resolve_type(std::slice::from_ref(&self.class.name), self.mi).cloned(),
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
                // A nullable container (`Null<Array<T>>`) is a pointer; index the
                // pointee, not the pointer.
                let access = if rty.is_ptr && is_container_ty(&rty) {
                    format!("(*{r})")
                } else {
                    r
                };
                (format!("{access}[{i}]"), self.element_ty(&rty))
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
                        self.prelude.push_str(&format!("{t}{spell} {tmp} = {code};\n"));
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
                // empty container (Haxe heap arrays are C++ value containers).
                if base.starts_with("std::vector") || base.starts_with("std::map") {
                    return (format!("{base}()"), Ty { base, ..Default::default() });
                }
                // `new String(x)` → a string *value*, not a heap pointer.
                if base == "std::string" {
                    let a = self.gen_args(args);
                    return (format!("std::string({a})"), Ty { base, ..Default::default() });
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
                let (c, ty) = self.gen_expr(expr);
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
                let (l, lty) = self.gen_expr(lhs);
                let (r, rty) = self.gen_expr(rhs);
                // A Haxe `String` lowers to `std::string`, a value type with no
                // null, so a null comparison cannot stay `s == NULL`. Optional
                // `String` params default to `""`, so "null" ≡ empty: lower
                // `s == null` → `s.empty()` and `s != null` → `!s.empty()`.
                if matches!(*op, BinOp::Eq | BinOp::Ne) {
                    let l_null = matches!(lhs.as_ref(), Expr::Null);
                    let r_null = matches!(rhs.as_ref(), Expr::Null);
                    if l_null ^ r_null {
                        let (s, sty) = if l_null { (&r, &rty) } else { (&l, &lty) };
                        if sty.base == "std::string" && !sty.is_ptr {
                            let neg = if matches!(*op, BinOp::Ne) { "!" } else { "" };
                            return (format!("{neg}{s}.empty()"), Ty { base: "bool".into(), ..Default::default() });
                        }
                    }
                }
                // String concatenation: in Haxe `+` with a `String` operand concatenates
                // (stringifying the other side). In C++ `int + "literal"` would be
                // pointer arithmetic and `std::string + int` does not compile, so build a
                // `std::string` concatenation, formatting any non-string operand.
                if matches!(*op, BinOp::Add) && (lty.base == "std::string" || rty.base == "std::string") {
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
                        Ty { base: "std::string".into(), ..Default::default() },
                    );
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
                let (t, tty) = self.gen_expr(target);
                let (v, _) = self.gen_expr(value);
                match op {
                    Some(o) => (format!("{t} {}= {v}", binop(*o)), tty),
                    None => (format!("{t} = {v}"), tty),
                }
            }
            Expr::NullCoalesce(a, b) => {
                let (ac, aty) = self.gen_expr(a);
                let (bc, _) = self.gen_expr(b);
                (format!("({ac} != NULL ? {ac} : {bc})"), aty)
            }
            Expr::SafeField(recv, field) => self.gen_safe_field(recv, field),
            Expr::ArrayLit(elems) => {
                // Inline array literal → hoisted vector temporary.
                let vec_ty = self.infer_array(elems);
                let elem = self.element_ty(&vec_ty);
                let tmp = self.fresh("arr");
                let mut buf = String::new();
                self.expand_array_into_local(&tmp, &vec_ty, &elem, elems, self.prelude_ind, &mut buf);
                self.prelude.push_str(&buf);
                (tmp, vec_ty)
            }
            Expr::MapLit(pairs) => {
                let map_ty = Ty::default();
                let tmp = self.fresh("map");
                let mut buf = String::new();
                self.expand_map_into_local(
                    &tmp, &map_ty, &Ty::default(), pairs, self.prelude_ind, &mut buf,
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
            Expr::Comprehension { var, iter, guard, body } => {
                self.gen_comprehension(var, iter, guard.as_deref(), body)
            }
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
    fn try_bind_map_iter(
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
        let _ = writeln!(out, "{t}{}::iterator {it} = {map_code}.find({key_code});", map_ty.base);
        let alias = IterAlias { it_name: it, map_code, value_ty: value_ty.clone() };
        let mut local_ty = value_ty;
        local_ty.iter = Some(Box::new(alias));
        self.define_local(name, local_ty);
        true
    }

    /// A null comparison against a `Map.get(k)` alias → the iterator existence
    /// check: `x == null` → `it == map.end()`, `x != null` → `it != map.end()`.
    /// `None` for any other comparison.
    fn try_iter_null_check(&self, op: BinOp, lhs: &Expr, rhs: &Expr) -> Option<(String, Ty)> {
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
            Ty { base: "bool".into(), ..Default::default() },
        ))
    }

    fn gen_ident(&mut self, name: &str) -> (String, Ty) {
        if let Some(ty) = self.lookup_local(name) {
            // A `Map.get(k)` alias: any value/member use is the dereferenced
            // iterator (`it->second`); a null check is handled in the `Binary` arm.
            if let Some(alias) = &ty.iter {
                return (format!("{}->second", alias.it_name), alias.value_ty.clone());
            }
            return (self.cpp_name(name), ty);
        }
        // implicit `this` field?
        if let Some(f) = self.class_field(name) {
            let ty = self.field_ty(f);
            return (format!("this->{name}"), ty);
        }
        // a type name (for static / enum access)?
        if let Some(info) = self.prog.resolve_type(&[name.to_string()], self.mi).cloned() {
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
        // free function / global / unknown — pass through
        (name.to_string(), Ty::default())
    }

    fn gen_field(&mut self, recv: &Expr, name: &str) -> (String, Ty) {
        // Enum constant: `EnumType.Variant`
        if let Expr::Ident(tname) = recv {
            if self.lookup_local(tname).is_none() && self.class_field(tname).is_none() {
                if let Some(info) = self.prog.resolve_type(std::slice::from_ref(tname), self.mi).cloned() {
                    if info.kind == TypeKind::Enum {
                        return (
                            self.enum_constant(&info, name),
                            Ty { base: info.cpp_name().to_string(), info: Some(info), ..Default::default() },
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
            Expr::New(ty, args) if !is_value_new(ty) => self.hoist_new_receiver(ty, args),
            _ => self.gen_expr(recv),
        };

        // Haxe `.length` → `.size()` (Array/Map) or `.length()` (String). A nullable
        // container is a pointer (`Null<Array<T>>`), so it must be dereferenced.
        if name == "length" {
            if is_container_ty(&rty) {
                if rty.is_ptr {
                    return (format!("(*{rcode}).size()"), int_ty());
                }
                return (format!("{rcode}.size()"), int_ty());
            }
            if rty.base == "std::string" {
                return (format!("{rcode}.length()"), int_ty());
            }
        }

        // Haxe `"A".code` → the first character's int value (usually a single-char
        // literal, but any string works — the first byte's code).
        if name == "code" && rty.base == "std::string" {
            return (format!("((int)(unsigned char)({rcode})[0])"), int_ty());
        }

        let op = if rty.is_ptr { "->" } else { "." };

        // External property accessor read: `obj.x` → `obj->GetX()`
        if !matches!(recv, Expr::This) {
            if let Some(info) = &rty.info {
                if self.field_has_getter(info, name) {
                    let fty = self.accessor_field_ty(info, name);
                    return (format!("{rcode}{op}Get{}()", cap(name)), fty);
                }
            }
        }

        // Plain field / member access
        let fty = rty
            .info
            .as_ref()
            .and_then(|info| self.member_field_ty(info, name))
            .unwrap_or_default();
        (format!("{rcode}{op}{name}"), fty)
    }

    /// Hoist `new T(...)` (used as the receiver of a field/property access) into a
    /// fresh local and return `(localName, T*)`. The local is **not** registered as
    /// owned — see the caller in [`gen_field`] for why it must not be freed.
    fn hoist_new_receiver(&mut self, ty: &Type, args: &[Expr]) -> (String, Ty) {
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
        self.prelude.push_str(&format!("{t}{}* {tmp} = new {base}({a});\n", base));
        (tmp, rty)
    }

    fn gen_safe_field(&mut self, recv: &Expr, field: &str) -> (String, Ty) {
        let (rcode, rty) = self.gen_expr(recv);
        // Value receivers cannot be null in C++ — access directly.
        if !rty.is_ptr {
            if let Some(info) = &rty.info {
                if self.field_has_getter(info, field) {
                    return (format!("{rcode}.Get{}()", cap(field)), self.accessor_field_ty(info, field));
                }
            }
            let fty = rty.info.as_ref().and_then(|i| self.member_field_ty(i, field)).unwrap_or_default();
            return (format!("{rcode}.{field}"), fty);
        }
        // Pointer receiver: guard against NULL.
        let access = match &rty.info {
            Some(info) if self.field_has_getter(info, field) => format!("{rcode}->Get{}()", cap(field)),
            _ => format!("{rcode}->{field}"),
        };
        (format!("({rcode} != NULL ? {access} : 0)"), Ty::default())
    }

    fn gen_call(&mut self, target: &Expr, args: &[Expr]) -> (String, Ty) {
        // `recv?.method(args)` → NULL-guarded call (comma operator keeps it usable
        // as a discardable expression even when the method returns void).
        if let Expr::SafeField(recv, method) = target {
            let (rcode, rty) = self.gen_expr(recv);
            let op = if rty.is_ptr { "->" } else { "." };
            let param_tys = self.callee_param_types(&rty, method);
            let overloaded = self.method_is_overloaded(&rty, method);
            if overloaded {
                if let Some(msg) = self.overload_mismatch(&rty, method, args) {
                    self.err(msg);
                }
            }
            let a = self.gen_args_typed(args, &param_tys, overloaded);
            let ret = self.method_return_ty(&rty, method, args);
            if !rty.is_ptr {
                return (format!("{rcode}{op}{method}({a})"), ret);
            }
            let call = format!("{rcode}->{method}({a})");
            return (format!("({rcode} != NULL ? ({call}, 0) : 0)"), Ty::default());
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
            let (rcode, rty) = self.gen_expr(recv);
            // Haxe container methods → std::vector / std::map equivalents.
            if is_container_ty(&rty) {
                if let Some(res) = self.container_call(&rcode, &rty, method, args) {
                    return res;
                }
            }
            // Haxe String methods → std::string expressions (Tier 1).
            if rty.base == "std::string" {
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
            let a = self.gen_args_typed(args, &param_tys, overloaded);
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
            let a = self.gen_args_typed(args, &param_tys, false);
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

    /// Return type of a top-level free function declared in this module.
    fn free_fn_return(&self, name: &str) -> Option<Ty> {
        for d in &self.prog.modules[self.mi].file.decls {
            if let Decl::Global(g) = d {
                if g.name == name {
                    let (_, ret, body) = lambda_parts(g)?;
                    return Some(self.resolve_lambda_ret(ret, body, g.ty.as_ref()));
                }
            }
        }
        None
    }

    /// Resolve an identifier that may be an import alias (`import a.b.Foo as Bar;`)
    /// to the real (last-component) name; returns the name unchanged otherwise.
    fn resolve_alias(&self, name: &str) -> String {
        for imp in &self.prog.modules[self.mi].file.imports {
            if imp.alias.as_deref() == Some(name) {
                if let Some(real) = imp.path.last() {
                    return real.clone();
                }
            }
        }
        name.to_string()
    }

    fn gen_args(&mut self, args: &[Expr]) -> String {
        args.iter().map(|a| self.gen_expr(a).0).collect::<Vec<_>>().join(", ")
    }

    /// Generate call arguments, hoisting anonymous struct literals to temporaries
    /// typed by the callee's parameter (an anon-struct argument → a named temp var).
    fn gen_args_typed(&mut self, args: &[Expr], param_tys: &[Option<Ty>], coerce_str: bool) -> String {
        self.gen_args_owned(args, param_tys, &[], coerce_str)
    }

    /// As [`gen_args_typed`], plus `owned`: per-position flags marking parameters
    /// the callee takes ownership of (constructor args stored into freed fields). A
    /// `new` at an owned position is emitted inline (the callee frees it) instead of
    /// being hoisted into a scope-owned local that would double-free it.
    fn gen_args_owned(
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
                            let value_ty = Ty { is_ptr: false, nullable: false, ..tgt.clone() };
                            let tmp = self.hoist_object(fields, value_ty);
                            let ptr_ty = Ty { is_ptr: true, ..tgt.clone() };
                            return self.place_new_arg(format!("new {}({tmp})", tgt.base), ptr_ty);
                        }
                        self.hoist_object(fields, tgt)
                    }
                    Expr::ArrayLit(elems) if !elems.is_empty() => {
                        let vec_ty = target.clone().unwrap_or_else(|| self.infer_array(elems));
                        let elem = self.element_ty(&vec_ty);
                        let tmp = self.fresh("arr");
                        let mut buf = String::new();
                        self.expand_array_into_local(&tmp, &vec_ty, &elem, elems, self.prelude_ind, &mut buf);
                        self.prelude.push_str(&buf);
                        tmp
                    }
                    // A `new X(...)` argument is hoisted to an owned local (the
                    // caller frees it) unless the receiver escapes — or the callee
                    // takes ownership of this parameter (an `@owned`/allocated field),
                    // in which case the constructed object frees it, so it is emitted
                    // inline to avoid a double-free.
                    Expr::New(nty, _) if !is_value_new(nty) => {
                        let (code, vty) = self.gen_expr(a);
                        if owned.get(i).copied().unwrap_or(false) {
                            code
                        } else {
                            self.place_new_arg(code, vty)
                        }
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
                                let ptr_ty = Ty { base: t.clone(), is_ptr: true, ..Default::default() };
                                return self.place_new_arg(format!("new {t}({code})"), ptr_ty);
                            }
                        }
                        // In an overloaded call, a bare string literal is a
                        // `const char*` and C++ prefers the `bool` overload over
                        // `std::string`; wrap it so the intended overload is chosen.
                        if coerce_str && matches!(a, Expr::Str { interpolated: false, .. }) {
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
    fn place_new_arg(&mut self, new_code: String, ty: Ty) -> String {
        if self.new_args_escape {
            return new_code;
        }
        let tmp = self.fresh("v");
        let spell = self.decl_spelling(&ty);
        let t = "\t".repeat(self.prelude_ind);
        self.prelude.push_str(&format!("{t}{spell} {tmp} = {new_code};\n"));
        self.register_owned(&tmp);
        tmp
    }

    /// Whether `e` is `container.push(new T(...))` / `container.insert(k, new T(...))`
    /// where `container` is one into which the `new` comes to rest in class-level
    /// storage (an owned class-field container, or a local that flows into one).
    /// Such a `new` escapes the current scope, so it must not be hoisted into a
    /// scope-owned local. Handles `field.push(...)`, `this.field.push(...)`, and
    /// `local.push(...)` (the receiver name is matched against the escape set).
    fn pushes_new_into_owned_container(&self, e: &Expr) -> bool {
        let Expr::Call(target, args) = e else { return false };
        let Expr::Field(recv, method) = &**target else { return false };
        let value = match (method.as_str(), args.len()) {
            ("push", 1) => &args[0],
            ("insert", 2) => &args[1],
            _ => return false,
        };
        if !matches!(value, Expr::New(ty, _) if !is_value_new(ty)) {
            return false;
        }
        match &**recv {
            Expr::Ident(n) => self.owned_containers.contains(n),
            Expr::Field(r, f) if matches!(**r, Expr::This) => self.owned_containers.contains(f),
            _ => false,
        }
    }

    /// Build an anonymous struct into a hoisted temporary; return the temp name.
    fn hoist_object(&mut self, fields: &[(String, Expr)], target: Ty) -> String {
        let tmp = self.fresh("anon");
        let mut buf = String::new();
        self.expand_object_into_local(&tmp, &target, fields, self.prelude_ind, &mut buf);
        self.prelude.push_str(&buf);
        tmp
    }

    fn container_call(
        &mut self,
        rcode: &str,
        _rty: &Ty,
        method: &str,
        args: &[Expr],
    ) -> Option<(String, Ty)> {
        match method {
            "push" => {
                // push([]) pushes a default-constructed element
                if matches!(args.first(), Some(Expr::ArrayLit(v)) if v.is_empty()) {
                    let spell = self.decl_spelling(&self.element_ty(_rty));
                    return Some((format!("{rcode}.push_back({spell}())"), Ty::default()));
                }
                // The pushed value is typed by the element type (so an object
                // literal becomes a temp of the element struct, not an anon one).
                let elem = self.elem_member_ty(_rty);
                let a = self.gen_args_typed(args, &[Some(elem)], false);
                Some((format!("{rcode}.push_back({a})"), Ty::default()))
            }
            "insert" => {
                let pos = self.gen_expr(&args[0]).0;
                let elem = self.elem_member_ty(_rty);
                let val = self.gen_args_typed(&args[1..2], &[Some(elem)], false);
                Some((format!("{rcode}.insert({rcode}.begin() + {pos}, {val})"), Ty::default()))
            }
            "pop" => {
                // Haxe `Array.pop()` removes AND returns the last element; C++
                // `back()` only *reads* it and `pop_back()` returns `void`, so capture
                // the value into a temp first, then shrink the vector.
                let elem = self.element_ty(_rty);
                let spell = self.decl_spelling(&elem);
                let tmp = self.fresh("pop");
                let t = "\t".repeat(self.prelude_ind);
                self.prelude.push_str(&format!("{t}{spell} {tmp} = {rcode}.back();\n"));
                self.prelude.push_str(&format!("{t}{rcode}.pop_back();\n"));
                Some((tmp, elem))
            }
            // Map.get(k) → m[k]; element type is the map's value type.
            "get" if rcode_is_map(_rty) => {
                let k = self.gen_expr(&args[0]).0;
                Some((format!("{rcode}[{k}]"), self.map_value_ty(_rty)))
            }
            "exists" if rcode_is_map(_rty) => {
                let k = self.gen_expr(&args[0]).0;
                Some((format!("({rcode}.find({k}) != {rcode}.end())"), Ty { base: "bool".into(), ..Default::default() }))
            }
            // Array.map(f) → a hoisted std::vector populated by a loop that applies
            // the lambda to each element (the Map-comprehension + Lambda composition).
            "map" if matches!(args.first(), Some(Expr::Lambda { .. })) => {
                if let Some(Expr::Lambda { params, body, .. }) = args.first() {
                    Some(self.gen_array_map(rcode, _rty, params, body))
                } else {
                    None
                }
            }

            // ---- Map (std::map) methods ------------------------------------
            // `Map.set(k, v)` → `m[k] = v`.
            "set" if rcode_is_map(_rty) => {
                let k = self.gen_expr(&args[0]).0;
                let v = self.gen_expr(&args[1]).0;
                Some((format!("{rcode}[{k}] = {v}"), Ty::default()))
            }
            // `Map.remove(k)` → `m.erase(k)`; Haxe returns Bool (was it present?).
            "remove" if rcode_is_map(_rty) => {
                let k = self.gen_expr(&args[0]).0;
                Some((format!("({rcode}.erase({k}) != 0)"), bool_ty()))
            }
            // `Map.keys()` → a hoisted std::vector<K> of the keys (iterable via the
            // ordinary collection `for`).
            "keys" if rcode_is_map(_rty) => {
                let kspell = self.decl_spelling(&self.map_key_ty(_rty));
                let it = self.fresh("it");
                let acc = self.fresh("keys");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}std::vector<{kspell} > {acc};");
                let _ = writeln!(
                    pre,
                    "{t}for ({}::iterator {it} = {rcode}.begin(); {it} != {rcode}.end(); ++{it}) {{ {acc}.push_back({it}->first); }}",
                    _rty.base
                );
                self.prelude.push_str(&pre);
                Some((acc, Ty { base: format!("std::vector<{kspell} >"), ..Default::default() }))
            }

            // ---- Array (std::vector) methods -------------------------------
            // `Array.contains(x)` → linear scan, no <algorithm> dependency.
            "contains" => {
                let x = self.gen_expr(&args[0]).0;
                let i = self.fresh("i");
                let has = self.fresh("has");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}bool {has} = false;");
                let _ = writeln!(
                    pre,
                    "{t}for (size_t {i} = 0; {i} < {rcode}.size(); ++{i}) {{ if ({rcode}[{i}] == {x}) {{ {has} = true; break; }} }}"
                );
                self.prelude.push_str(&pre);
                Some((has, bool_ty()))
            }
            // `Array.indexOf(x[, fromIndex])` → first matching index or -1.
            "indexOf" => {
                let x = self.gen_expr(&args[0]).0;
                let start = if args.len() > 1 { self.gen_expr(&args[1]).0 } else { "0".to_string() };
                let i = self.fresh("i");
                let idx = self.fresh("idx");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}int {idx} = -1;");
                let _ = writeln!(
                    pre,
                    "{t}for (size_t {i} = (size_t)({start}); {i} < {rcode}.size(); ++{i}) {{ if ({rcode}[{i}] == {x}) {{ {idx} = (int){i}; break; }} }}"
                );
                self.prelude.push_str(&pre);
                Some((idx, int_ty()))
            }
            // `Array.remove(x)` → erase first match; Haxe returns Bool.
            "remove" => {
                let x = self.gen_expr(&args[0]).0;
                let i = self.fresh("i");
                let rem = self.fresh("rem");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}bool {rem} = false;");
                let _ = writeln!(
                    pre,
                    "{t}for (size_t {i} = 0; {i} < {rcode}.size(); ++{i}) {{ if ({rcode}[{i}] == {x}) {{ {rcode}.erase({rcode}.begin() + {i}); {rem} = true; break; }} }}"
                );
                self.prelude.push_str(&pre);
                Some((rem, bool_ty()))
            }
            // `Array.reverse()` → in-place swap loop (Void).
            "reverse" => {
                let espell = self.decl_spelling(&self.element_ty(_rty));
                let i = self.fresh("i");
                let tmp = self.fresh("tmp");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(
                    pre,
                    "{t}for (size_t {i} = 0; {i} < {rcode}.size() / 2; ++{i}) {{ {espell} {tmp} = {rcode}[{i}]; {rcode}[{i}] = {rcode}[{rcode}.size() - 1 - {i}]; {rcode}[{rcode}.size() - 1 - {i}] = {tmp}; }}"
                );
                self.prelude.push_str(&pre);
                Some(("((void)0)".to_string(), Ty::default()))
            }
            // `Array.copy()` → a shallow copy via the vector copy constructor.
            "copy" => Some((format!("{}({rcode})", _rty.base), _rty.clone())),
            // `Array.join(sep)` → concatenate elements (stringified) with `sep`.
            "join" => {
                let sep = self.gen_expr(&args[0]).0;
                let elem = self.element_ty(_rty);
                let i = self.fresh("i");
                let acc = self.fresh("join");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}std::string {acc};");
                if elem.base == "std::string" {
                    let _ = writeln!(
                        pre,
                        "{t}for (size_t {i} = 0; {i} < {rcode}.size(); ++{i}) {{ if ({i}) {acc} += {sep}; {acc} += {rcode}[{i}]; }}"
                    );
                } else {
                    let (spec, _) = self.spec_for("", &elem);
                    let buf = self.fresh("buf");
                    let _ = writeln!(
                        pre,
                        "{t}for (size_t {i} = 0; {i} < {rcode}.size(); ++{i}) {{ if ({i}) {acc} += {sep}; char {buf}[64]; sprintf({buf}, \"{spec}\", {rcode}[{i}]); {acc} += {buf}; }}"
                    );
                }
                self.prelude.push_str(&pre);
                Some((acc, Ty { base: "std::string".into(), ..Default::default() }))
            }
            // `Array.concat(other)` → a new vector: a copy of this with `other`'s
            // elements appended (Haxe returns a fresh array, leaving both operands).
            "concat" => {
                let other = self.gen_expr(&args[0]).0;
                let i = self.fresh("i");
                let acc = self.fresh("cat");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}{} {acc} = {rcode};", _rty.base);
                let _ = writeln!(
                    pre,
                    "{t}for (size_t {i} = 0; {i} < ({other}).size(); ++{i}) {{ {acc}.push_back(({other})[{i}]); }}"
                );
                self.prelude.push_str(&pre);
                Some((acc, _rty.clone()))
            }
            // `Array.slice(pos, ?end)` → a new vector of `[pos, end)`; negative
            // indices count from the end, and the range is clamped to the array.
            "slice" => {
                let pos = self.gen_expr(&args[0]).0;
                let end = if args.len() > 1 { Some(self.gen_expr(&args[1]).0) } else { None };
                let acc = self.fresh("slc");
                let a = self.fresh("a");
                let b = self.fresh("b");
                let i = self.fresh("i");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}{} {acc};", _rty.base);
                let _ = writeln!(pre, "{t}int {a} = (int)({pos}); if ({a} < 0) {a} += (int){rcode}.size(); if ({a} < 0) {a} = 0; if ((size_t){a} > {rcode}.size()) {a} = (int){rcode}.size();");
                match end {
                    Some(end) => {
                        let _ = writeln!(pre, "{t}int {b} = (int)({end}); if ({b} < 0) {b} += (int){rcode}.size(); if ({b} < 0) {b} = 0; if ((size_t){b} > {rcode}.size()) {b} = (int){rcode}.size();");
                    }
                    None => {
                        let _ = writeln!(pre, "{t}int {b} = (int){rcode}.size();");
                    }
                }
                let _ = writeln!(pre, "{t}for (size_t {i} = (size_t){a}; {i} < (size_t){b}; ++{i}) {{ {acc}.push_back({rcode}[{i}]); }}");
                self.prelude.push_str(&pre);
                Some((acc, _rty.clone()))
            }
            // `Array.shift()` → remove and return the first element.
            "shift" => {
                let elem = self.element_ty(_rty);
                let spell = self.decl_spelling(&elem);
                let tmp = self.fresh("shift");
                let t = "\t".repeat(self.prelude_ind);
                self.prelude.push_str(&format!("{t}{spell} {tmp} = {rcode}.front();\n"));
                self.prelude.push_str(&format!("{t}{rcode}.erase({rcode}.begin());\n"));
                Some((tmp, elem))
            }
            // `Array.unshift(x)` → insert `x` at the front (Void).
            "unshift" => {
                let elem = self.elem_member_ty(_rty);
                let val = self.gen_args_typed(args, &[Some(elem)], false);
                Some((format!("{rcode}.insert({rcode}.begin(), {val})"), Ty::default()))
            }
            // `Array.lastIndexOf(x[, fromIndex])` → last matching index or -1,
            // searching backward from `fromIndex` (default: the last element).
            "lastIndexOf" => {
                let x = self.gen_expr(&args[0]).0;
                let i = self.fresh("i");
                let idx = self.fresh("idx");
                let t = "\t".repeat(self.prelude_ind);
                let start = if args.len() > 1 {
                    let from = self.gen_expr(&args[1]).0;
                    format!("(size_t)({from}) + 1")
                } else {
                    format!("{rcode}.size()")
                };
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}int {idx} = -1;");
                let _ = writeln!(
                    pre,
                    "{t}for (size_t {i} = {start}; {i}-- > 0; ) {{ if ({i} < {rcode}.size() && {rcode}[{i}] == {x}) {{ {idx} = (int){i}; break; }} }}"
                );
                self.prelude.push_str(&pre);
                Some((idx, int_ty()))
            }
            _ => None,
        }
    }

    /// Tier-1 Haxe `String` methods on a `std::string` receiver, each mapped to a
    /// single C++98 expression. Byte/ASCII semantics (VC6 narrow `char`); an
    /// out-of-range index makes `charAt`/`charCodeAt` *throw* via `.at()`/`substr`
    /// rather than returning `""`/`null` — an error-path divergence from Haxe.
    /// Returns `None` for methods not in Tier 1 (e.g. `split`, the `startIndex`
    /// form of `lastIndexOf`), which fall through to a later tier.
    fn string_call(&mut self, rcode: &str, method: &str, args: &[Expr]) -> Option<(String, Ty)> {
        let str_ty = Ty { base: "std::string".into(), ..Default::default() };
        match method {
            "toString" => Some((rcode.to_string(), str_ty)),
            "charAt" => {
                let i = self.gen_expr(&args[0]).0;
                Some((format!("{rcode}.substr({i}, 1)"), str_ty))
            }
            // `npos` (size_t(-1)) casts to int `-1` on 32- and 64-bit, matching
            // Haxe's "not found" sentinel.
            "indexOf" => {
                let needle = self.gen_expr(&args[0]).0;
                let call = if args.len() > 1 {
                    let start = self.gen_expr(&args[1]).0;
                    format!("{rcode}.find({needle}, {start})")
                } else {
                    format!("{rcode}.find({needle})")
                };
                Some((format!("((int){call})"), int_ty()))
            }
            // Tier 1 handles only the no-`startIndex` form; the search-window rule
            // for `lastIndexOf(str, startIndex)` is Tier 2.
            "lastIndexOf" if args.len() <= 1 => {
                let needle = self.gen_expr(&args[0]).0;
                Some((format!("((int){rcode}.rfind({needle}))"), int_ty()))
            }
            // Haxe returns `Null<Int>`; as an intrinsic this yields plain `int`
            // (the usual `var c:Int = s.charCodeAt(i)` form). `unsigned char` cast
            // is required for correct code values on MSVC.
            "charCodeAt" => {
                let i = self.gen_expr(&args[0]).0;
                Some((format!("((int)(unsigned char){rcode}.at({i}))"), int_ty()))
            }
            // Tier 2: ASCII case mapping done in-place on a copy (no <cctype>).
            "toUpperCase" | "toLowerCase" => {
                let upper = method == "toUpperCase";
                let acc = self.fresh("case");
                let i = self.fresh("i");
                let t = "\t".repeat(self.prelude_ind);
                let (lo, hi, delta) = if upper { ('a', 'z', "- 'a' + 'A'") } else { ('A', 'Z', "- 'A' + 'a'") };
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}std::string {acc} = {rcode};");
                let _ = writeln!(
                    pre,
                    "{t}for (size_t {i} = 0; {i} < {acc}.size(); ++{i}) {{ if ({acc}[{i}] >= '{lo}' && {acc}[{i}] <= '{hi}') {acc}[{i}] = (char)({acc}[{i}] {delta}); }}"
                );
                self.prelude.push_str(&pre);
                Some((acc, str_ty))
            }
            // Tier 2: `split(delim)` → a std::vector<std::string>. An empty delimiter
            // splits into individual characters (Haxe semantics).
            "split" => {
                let delim = self.gen_expr(&args[0]).0;
                let acc = self.fresh("spl");
                let s = self.fresh("s");
                let d = self.fresh("d");
                let i = self.fresh("i");
                let start = self.fresh("start");
                let found = self.fresh("found");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}std::vector<std::string > {acc};");
                let _ = writeln!(pre, "{t}std::string {s} = {rcode};");
                let _ = writeln!(pre, "{t}std::string {d} = {delim};");
                let _ = writeln!(pre, "{t}if ({d}.empty()) {{");
                let _ = writeln!(pre, "{t}\tfor (size_t {i} = 0; {i} < {s}.size(); ++{i}) {{ {acc}.push_back({s}.substr({i}, 1)); }}");
                let _ = writeln!(pre, "{t}}} else {{");
                let _ = writeln!(pre, "{t}\tsize_t {start} = 0;");
                let _ = writeln!(pre, "{t}\tsize_t {found};");
                let _ = writeln!(pre, "{t}\twhile (({found} = {s}.find({d}, {start})) != std::string::npos) {{");
                let _ = writeln!(pre, "{t}\t\t{acc}.push_back({s}.substr({start}, {found} - {start}));");
                let _ = writeln!(pre, "{t}\t\t{start} = {found} + {d}.size();");
                let _ = writeln!(pre, "{t}\t}}");
                let _ = writeln!(pre, "{t}\t{acc}.push_back({s}.substr({start}));");
                let _ = writeln!(pre, "{t}}}");
                self.prelude.push_str(&pre);
                Some((acc, Ty { base: "std::vector<std::string >".into(), ..Default::default() }))
            }
            // `substr(pos, ?len)` — `pos` may be negative (counted from the end);
            // `len` omitted means "to the end". Indices are clamped to the string.
            "substr" => {
                let pos = self.gen_expr(&args[0]).0;
                let len = if args.len() > 1 { Some(self.gen_expr(&args[1]).0) } else { None };
                let s = self.fresh("s");
                let p = self.fresh("p");
                let res = self.fresh("sub");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}std::string {s} = {rcode};");
                let _ = writeln!(pre, "{t}int {p} = (int)({pos});");
                let _ = writeln!(pre, "{t}if ({p} < 0) {p} += (int){s}.size();");
                let _ = writeln!(pre, "{t}if ({p} < 0) {p} = 0;");
                let _ = writeln!(pre, "{t}if ((size_t){p} > {s}.size()) {p} = (int){s}.size();");
                if let Some(len) = len {
                    let n = self.fresh("n");
                    let _ = writeln!(pre, "{t}int {n} = (int)({len}); if ({n} < 0) {n} = 0;");
                    let _ = writeln!(pre, "{t}std::string {res} = {s}.substr((size_t){p}, (size_t){n});");
                } else {
                    let _ = writeln!(pre, "{t}std::string {res} = {s}.substr((size_t){p});");
                }
                self.prelude.push_str(&pre);
                Some((res, str_ty))
            }
            // `substring(start, ?end)` — negative indices clamp to 0, and start/end
            // are swapped when start > end (Haxe semantics).
            "substring" => {
                let start = self.gen_expr(&args[0]).0;
                let end = if args.len() > 1 { Some(self.gen_expr(&args[1]).0) } else { None };
                let s = self.fresh("s");
                let a = self.fresh("a");
                let b = self.fresh("b");
                let res = self.fresh("sub");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}std::string {s} = {rcode};");
                let _ = writeln!(pre, "{t}int {a} = (int)({start}); if ({a} < 0) {a} = 0; if ((size_t){a} > {s}.size()) {a} = (int){s}.size();");
                match end {
                    Some(end) => {
                        let _ = writeln!(pre, "{t}int {b} = (int)({end}); if ({b} < 0) {b} = 0; if ((size_t){b} > {s}.size()) {b} = (int){s}.size();");
                    }
                    None => {
                        let _ = writeln!(pre, "{t}int {b} = (int){s}.size();");
                    }
                }
                let _ = writeln!(pre, "{t}if ({a} > {b}) {{ int t = {a}; {a} = {b}; {b} = t; }}");
                let _ = writeln!(pre, "{t}std::string {res} = {s}.substr((size_t){a}, (size_t)({b} - {a}));");
                self.prelude.push_str(&pre);
                Some((res, str_ty))
            }
            // `StringBuf.add(x)` → append `x` stringified (reuses the `Std.string`
            // lowering); `StringBuf` is a `std::string` accumulator. Returns Void.
            "add" => {
                let (sv, _) = self.gen_std_string(&args[0]);
                Some((format!("{rcode} += {sv}"), Ty::default()))
            }
            // `StringBuf.addChar(c)` → append a single byte.
            "addChar" => Some((format!("{rcode} += (char)({})", self.gen_expr(&args[0]).0), Ty::default())),
            _ => None,
        }
    }

    fn callee_param_types(&self, recv: &Ty, method: &str) -> Vec<Option<Ty>> {
        let Some(info) = &recv.info else { return Vec::new() };
        let cmi = info.module_index;
        let Some(decl) = self.prog.type_decl(info) else { return Vec::new() };
        let methods = match decl {
            Decl::Class(c) => &c.methods,
            Decl::Interface(i) => &i.methods,
            _ => return Vec::new(),
        };
        match methods.iter().find(|m| m.name.as_deref() == Some(method)) {
            // Parameter types resolve in the callee's declaring module.
            Some(m) => m.params.iter().map(|p| self.param_ty_in(p, cmi)).collect(),
            None => Vec::new(),
        }
    }

    fn own_method_param_types(&self, name: &str) -> Vec<Option<Ty>> {
        match self.class.methods.iter().find(|m| m.name.as_deref() == Some(name)) {
            Some(m) => m.params.iter().map(|p| self.param_ty_in(p, self.mi)).collect(),
            None => Vec::new(),
        }
    }

    // ---- intrinsics ----------------------------------------------------

    fn intrinsic_call(&mut self, obj: &str, method: &str, args: &[Expr]) -> Option<(String, Ty)> {
        let f = |this: &mut Self, i: usize| this.gen_expr(&args[i]).0;
        match (obj, method) {
            // Direct <math.h> functions (Float → Float).
            ("Math", "sqrt") => Some((format!("sqrt({})", f(self, 0)), float_ty())),
            ("Math", "sin") => Some((format!("sin({})", f(self, 0)), float_ty())),
            ("Math", "cos") => Some((format!("cos({})", f(self, 0)), float_ty())),
            ("Math", "tan") => Some((format!("tan({})", f(self, 0)), float_ty())),
            ("Math", "asin") => Some((format!("asin({})", f(self, 0)), float_ty())),
            ("Math", "acos") => Some((format!("acos({})", f(self, 0)), float_ty())),
            ("Math", "atan") => Some((format!("atan({})", f(self, 0)), float_ty())),
            ("Math", "exp") => Some((format!("exp({})", f(self, 0)), float_ty())),
            ("Math", "log") => Some((format!("log({})", f(self, 0)), float_ty())),
            ("Math", "atan2") => Some((format!("atan2({}, {})", f(self, 0), f(self, 1)), float_ty())),
            ("Math", "pow") => Some((format!("pow({}, {})", f(self, 0), f(self, 1)), float_ty())),
            // Float-returning rounding (ffloor/fceil/fround).
            ("Math", "ffloor") => Some((format!("floor({})", f(self, 0)), float_ty())),
            ("Math", "fceil") => Some((format!("ceil({})", f(self, 0)), float_ty())),
            ("Math", "fround") => Some((format!("floor(({}) + 0.5)", f(self, 0)), float_ty())),
            // Int-returning rounding (Haxe `floor`/`ceil`/`round` return Int).
            ("Math", "floor") => Some((format!("((int)floor({}))", f(self, 0)), int_ty())),
            ("Math", "ceil") => Some((format!("((int)ceil({}))", f(self, 0)), int_ty())),
            ("Math", "round") => Some((format!("((int)floor(({}) + 0.5))", f(self, 0)), int_ty())),
            ("Math", "abs") => {
                // abs for Int, fabs for Float — choose by inferred argument type
                let (c, ty) = self.gen_expr(&args[0]);
                let fname = if ty.base == "float" { "fabs" } else { "abs" };
                Some((format!("{fname}({c})"), ty))
            }
            ("Math", "min") => Some((self.min_max("<", args), float_ty())),
            ("Math", "max") => Some((self.min_max(">", args), float_ty())),
            // Math.random() ∈ [0, 1).
            ("Math", "random") => Some(("(rand() / (RAND_MAX + 1.0))".into(), float_ty())),
            // Predicates → bool (portable C++98, no <cmath> isnan/isfinite needed).
            ("Math", "isNaN") => {
                let a = f(self, 0);
                Some((format!("(({a}) != ({a}))"), bool_ty()))
            }
            ("Math", "isFinite") => Some((format!("((({}) * 0.0) == 0.0)", f(self, 0)), bool_ty())),
            ("Std", "int") => Some((format!("(int)({})", f(self, 0)), Ty { base: "int".into(), ..Default::default() })),
            ("Std", "string") => Some(self.gen_std_string(&args[0])),
            // `Std.parseInt` accepts decimal and `0x` hex (strtol base 0). Haxe
            // returns `Null<Int>`; the C++98 lowering yields a plain `int` (0 on a
            // fully unparseable string).
            ("Std", "parseInt") => {
                let s = self.cstr_arg(&args[0]);
                Some((format!("(int)strtol({s}, NULL, 0)"), int_ty()))
            }
            ("Std", "parseFloat") => {
                let s = self.cstr_arg(&args[0]);
                Some((format!("(float)atof({s})"), float_ty()))
            }
            // `Std.random(x)` → a non-negative int in `[0, x)` (0 when `x <= 0`, as in
            // Haxe). The argument is virtually always pure, so re-using it is safe.
            ("Std", "random") => {
                let n = f(self, 0);
                Some((format!("(((int)({n})) > 0 ? (rand() % (int)({n})) : 0)"), int_ty()))
            }
            // `StringTools.replace(s, sub, by)` → replace every occurrence of `sub`.
            ("StringTools", "replace") => {
                let s = self.gen_expr(&args[0]).0;
                let sub = self.gen_expr(&args[1]).0;
                let by = self.gen_expr(&args[2]).0;
                let acc = self.fresh("rep");
                let needle = self.fresh("sub");
                let repl = self.fresh("by");
                let pos = self.fresh("pos");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}std::string {acc} = {s};");
                let _ = writeln!(pre, "{t}std::string {needle} = {sub};");
                let _ = writeln!(pre, "{t}std::string {repl} = {by};");
                let _ = writeln!(pre, "{t}if (!{needle}.empty()) {{");
                let _ = writeln!(pre, "{t}\tsize_t {pos} = 0;");
                let _ = writeln!(pre, "{t}\twhile (({pos} = {acc}.find({needle}, {pos})) != std::string::npos) {{ {acc}.replace({pos}, {needle}.size(), {repl}); {pos} += {repl}.size(); }}");
                let _ = writeln!(pre, "{t}}}");
                self.prelude.push_str(&pre);
                Some((acc, str_ty()))
            }
            // `StringTools.trim(s)` → strip leading/trailing ASCII whitespace.
            ("StringTools", "trim") | ("StringTools", "ltrim") | ("StringTools", "rtrim") => {
                let s = self.gen_expr(&args[0]).0;
                let acc = self.fresh("trm");
                let a = self.fresh("a");
                let b = self.fresh("b");
                let res = self.fresh("res");
                let t = "\t".repeat(self.prelude_ind);
                let ws = "== ' ' || {C} == '\\t' || {C} == '\\n' || {C} == '\\r'";
                let lo = ws.replace("{C}", &format!("{acc}[{a}]"));
                let hi = ws.replace("{C}", &format!("{acc}[{b} - 1]"));
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}std::string {acc} = {s};");
                let _ = writeln!(pre, "{t}size_t {a} = 0; size_t {b} = {acc}.size();");
                if method != "rtrim" {
                    let _ = writeln!(pre, "{t}while ({a} < {b} && ({acc}[{a}] {lo})) ++{a};");
                }
                if method != "ltrim" {
                    let _ = writeln!(pre, "{t}while ({b} > {a} && ({acc}[{b} - 1] {hi})) --{b};");
                }
                let _ = writeln!(pre, "{t}std::string {res} = {acc}.substr({a}, {b} - {a});");
                self.prelude.push_str(&pre);
                Some((res, str_ty()))
            }
            // `StringTools.startsWith(s, start)` / `endsWith(s, end)` → a bool temp
            // (hoisted so the operands are evaluated exactly once).
            ("StringTools", "startsWith") | ("StringTools", "endsWith") => {
                let starts = method == "startsWith";
                let s = self.gen_expr(&args[0]).0;
                let sub = self.gen_expr(&args[1]).0;
                let sv = self.fresh("s");
                let ss = self.fresh("sub");
                let res = self.fresh("res");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}std::string {sv} = {s};");
                let _ = writeln!(pre, "{t}std::string {ss} = {sub};");
                let cmp = if starts {
                    format!("{sv}.compare(0, {ss}.size(), {ss}) == 0")
                } else {
                    format!("{sv}.compare({sv}.size() - {ss}.size(), {ss}.size(), {ss}) == 0")
                };
                let _ = writeln!(pre, "{t}bool {res} = ({sv}.size() >= {ss}.size() && {cmp});");
                self.prelude.push_str(&pre);
                Some((res, bool_ty()))
            }
            // `StringTools.hex(n, ?digits)` → uppercase hex, zero-padded to `digits`.
            ("StringTools", "hex") => {
                let n = self.gen_expr(&args[0]).0;
                let buf = self.fresh("hex");
                let res = self.fresh("res");
                let t = "\t".repeat(self.prelude_ind);
                let mut pre = String::new();
                let _ = writeln!(pre, "{t}char {buf}[32];");
                if args.len() > 1 {
                    let digits = self.gen_expr(&args[1]).0;
                    let _ = writeln!(pre, "{t}sprintf({buf}, \"%0*X\", (int)({digits}), (unsigned int)({n}));");
                } else {
                    let _ = writeln!(pre, "{t}sprintf({buf}, \"%X\", (unsigned int)({n}));");
                }
                let _ = writeln!(pre, "{t}std::string {res} = {buf};");
                self.prelude.push_str(&pre);
                Some((res, str_ty()))
            }
            ("Sys", "cpuTime") => Some((
                "((float) clock() / (float) CLOCKS_PER_SEC)".into(),
                float_ty(),
            )),
            // `String.fromCharCode(c)` → a one-char string (low byte only on VC6).
            ("String", "fromCharCode") => Some((
                format!("std::string(1, (char)(({}) & 0xFF))", f(self, 0)),
                Ty { base: "std::string".into(), ..Default::default() },
            )),
            _ => None,
        }
    }

    fn min_max(&mut self, cmp: &str, args: &[Expr]) -> String {
        let (a, _) = self.gen_expr(&args[0]);
        let (b, _) = self.gen_expr(&args[1]);
        // Inline ternary that propagates NaN exactly as Haxe does (NaN in either
        // operand → NaN result) — no `haxe_min`/`haxe_max` helper.
        format!("(({a}) {cmp} ({b}) ? ({a}) : (({a}) == ({a}) ? ({b}) : ({a})))")
    }

    fn gen_string(&mut self, raw: &str, interpolated: bool) -> (String, Ty) {
        let str_ty = Ty { base: "std::string".into(), ..Default::default() };
        if !interpolated || !has_interpolation(raw) {
            return (format!("\"{}\"", escape_str(raw)), str_ty);
        }
        let (segments, exprs) = split_interpolation(raw);
        if exprs.is_empty() {
            return (format!("\"{}\"", escape_str(raw)), str_ty);
        }
        // Build the result by appending each piece to a `std::string`: literal and string
        // segments append directly (`s += part`) — `std::string` grows itself, so an
        // arbitrarily long interpolated string is safe — while each numeric segment is
        // formatted into a type-bounded buffer. There is no single value-guessed buffer,
        // so this cannot overflow regardless of the runtime values.
        let acc = self.fresh("str");
        let t = "\t".repeat(self.prelude_ind);
        self.prelude.push_str(&format!("{t}std::string {acc};\n"));
        for seg in segments {
            let part = match seg {
                Seg::Lit(s) => format!("\"{}\"", escape_str(&s)),
                Seg::Expr(src) => match crate::parser::parse_expression(&src) {
                    Ok(e) => {
                        let (code, ty) = self.gen_expr(&e);
                        if ty.base == "std::string" {
                            code
                        } else {
                            self.format_scalar(&code, &ty)
                        }
                    }
                    Err(_) => format!("\"{}\"", escape_str(&src)),
                },
            };
            self.prelude.push_str(&format!("{t}{acc} += {part};\n"));
        }
        (acc, str_ty)
    }

    /// Lower a Haxe `trace(args...)` call. Like Haxe, the output is prefixed with
    /// the source `file:line` and the arguments follow, comma-separated. It reuses
    /// the string-interpolation plumbing (`spec_for`) to pick a printf conversion
    /// per argument, emitting a single `printf` to stdout. Under `--no-traces` the
    /// whole call (and its argument evaluation) is stripped to a no-op.
    fn gen_trace(&mut self, args: &[Expr]) -> (String, Ty) {
        if self.no_trace {
            return ("((void)0)".to_string(), Ty::default());
        }
        let file = self.prog.modules[self.mi]
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?");
        let mut fmt = printf_escape(&format!("{file}:{}: ", self.current_line));
        let mut printf_args: Vec<String> = Vec::new();
        for (i, a) in args.iter().enumerate() {
            if i > 0 {
                fmt.push_str(", ");
            }
            let (code, ty) = self.gen_expr(a);
            // A bare string literal is already a `const char*`; everything else
            // goes through the interpolation type→spec mapping (a `std::string`
            // value needs `.c_str()`, which `spec_for` supplies).
            let (spec, arg) = if matches!(a, Expr::Str { interpolated: false, .. }) {
                ("%s".to_string(), code)
            } else {
                self.spec_for(&code, &ty)
            };
            fmt.push_str(&spec);
            printf_args.push(arg);
        }
        fmt.push_str("\\n");
        let call = if printf_args.is_empty() {
            format!("printf(\"{fmt}\")")
        } else {
            format!("printf(\"{fmt}\", {})", printf_args.join(", "))
        };
        (call, Ty::default())
    }

    /// `Std.string(x)` → a `std::string` holding x's textual form. A value that is
    /// already a string passes through; a bool maps to `"true"`/`"false"`; a numeric
    /// value is formatted via `sprintf` into a stack buffer (reusing `spec_for`'s
    /// type→conversion mapping).
    fn gen_std_string(&mut self, arg: &Expr) -> (String, Ty) {
        let str_ty = Ty { base: "std::string".into(), ..Default::default() };
        // A bare string literal is emitted as a `const char*`; wrap it so the
        // result is a genuine `std::string` value.
        if matches!(arg, Expr::Str { interpolated: false, .. }) {
            let (code, _) = self.gen_expr(arg);
            return (format!("std::string({code})"), str_ty);
        }
        let (code, ty) = self.gen_expr(arg);
        if ty.base == "std::string" {
            return (code, str_ty);
        }
        if ty.base == "bool" {
            return (
                format!("(({code}) ? std::string(\"true\") : std::string(\"false\"))"),
                str_ty,
            );
        }
        let buf = self.format_scalar(&code, &ty);
        (format!("std::string({buf})"), str_ty)
    }

    /// Evaluate a string-typed argument as a C++ `const char*` expression: a
    /// `std::string` value gets `.c_str()`; a bare string literal is already one.
    fn cstr_arg(&mut self, arg: &Expr) -> String {
        let (code, ty) = self.gen_expr(arg);
        // A bare string literal is already a `const char*` — `.c_str()` on it is
        // invalid; everything else of string type is a `std::string` value.
        if matches!(arg, Expr::Str { interpolated: false, .. }) {
            code
        } else if ty.base == "std::string" {
            format!("{code}.c_str()")
        } else {
            code
        }
    }

    /// Choose a printf conversion and the matching argument expression for an
    /// interpolated value, based on its inferred C++ type.
    fn spec_for(&self, code: &str, ty: &Ty) -> (String, String) {
        if ty.base == "std::string" {
            ("%s".to_string(), format!("{code}.c_str()"))
        } else if ty.base == "float" || ty.base == "double" {
            ("%f".to_string(), code.to_string())
        } else {
            ("%d".to_string(), code.to_string())
        }
    }

    /// Format a non-string scalar (`code` of type `ty`) into a hoisted stack buffer and
    /// return the buffer name (a `const char*`). The buffer size is fixed by the *type*,
    /// never guessed from the runtime value, so it can never overflow: a 32-bit `int`
    /// prints ≤ 11 chars, a `float`/`double` via `%f` ≤ ~48 / ~316. This is the one place
    /// that turns a number into text, shared by interpolation, concatenation and
    /// `Std.string`. (Strings are never formatted through here — they are appended
    /// directly, which is unbounded-safe.)
    fn format_scalar(&mut self, code: &str, ty: &Ty) -> String {
        let (spec, arg) = self.spec_for(code, ty);
        let size = match ty.base.as_str() {
            "double" => 320, // %f of DBL_MAX ≈ 316 chars
            "float" => 64,   // %f of FLT_MAX ≈ 48 chars
            _ => 24,         // a 64-bit integer ≤ 20 chars
        };
        let buf = self.fresh("buf");
        let t = "\t".repeat(self.prelude_ind);
        self.prelude.push_str(&format!("{t}char {buf}[{size}]; sprintf({buf}, \"{spec}\", {arg});\n"));
        buf
    }

    /// One operand of a string concatenation, as a C++ expression that participates in
    /// `std::string` `operator+`. A `String` operand (variable or literal) is used as-is;
    /// a non-string (numeric) operand is formatted into a type-bounded buffer and wrapped
    /// as a `std::string` so it anchors the chain (`std::string(buf) + ","`).
    fn concat_part(&mut self, code: &str, ty: &Ty) -> String {
        if ty.base == "std::string" {
            return code.to_string();
        }
        let buf = self.format_scalar(code, ty);
        format!("std::string({buf})")
    }

    // ---- object-literal expansion --------------------------------------

    fn expand_object_into_local(
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
    fn elem_member_ty(&self, vec: &Ty) -> Ty {
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
    fn file_scope_const(&mut self, g: &GlobalVar) -> Option<String> {
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
    fn render_const_aggregate(&self, ty: &Ty, fields: &[(String, Expr)]) -> Option<String> {
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
    fn render_const_vector(&mut self, name: &str, vec_ty: &Ty, elems: &[Expr]) -> String {
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
    fn expand_array_into_local(
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
    fn expand_map_into_local(
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
    fn expand_anon_struct_local(
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

    /// Constructor parameter types for `new T(...)`, resolved in T's module.
    fn ctor_param_types(&self, ty: &Type) -> Vec<Option<Ty>> {
        let Some(info) = ty_named_info(self.prog, self.mi, ty) else { return Vec::new() };
        let cmi = info.module_index;
        let Some(Decl::Class(c)) = self.prog.type_decl(&info) else { return Vec::new() };
        match &c.ctor {
            Some(ctor) => ctor.params.iter().map(|p| self.param_ty_in(p, cmi)).collect(),
            None => Vec::new(),
        }
    }

    /// Per-position flags marking which constructor parameters of `ty`'s class take
    /// ownership of their argument (it is stored into a field the destructor frees).
    /// A `new` at such a position is freed by the constructed object, so it must be
    /// emitted inline rather than hoisted into a scope-owned local.
    fn ctor_owned_params(&self, ty: &Type) -> Vec<bool> {
        let Some(info) = ty_named_info(self.prog, self.mi, ty) else { return Vec::new() };
        let Some(Decl::Class(c)) = self.prog.type_decl(&info) else { return Vec::new() };
        // Which constructor parameters take ownership of their argument comes from
        // the escape analysis, as a per-position predicate.
        let owned = crate::sema::escape::ctor_owned_params(self.prog, info.module_index, c);
        match &c.ctor {
            Some(ctor) => (0..ctor.params.len()).map(|i| owned.contains(&i)).collect(),
            None => Vec::new(),
        }
    }

    /// Element `Ty` (with `info`) from a declared `Array<T>` AST type.
    fn elem_ast_ty(&self, ty: Option<&Type>) -> Option<Ty> {
        if let Some(Type::Named { path, params, .. }) = ty {
            if path.last().map(|s| s.as_str()) == Some("Array") && params.len() == 1 {
                return Some(self.ty_of(&params[0]));
            }
        }
        None
    }

    /// Key/value `Ty`s from a declared `Map<K,V>` AST type.
    fn map_kv_ast_ty(&self, ty: Option<&Type>) -> (Ty, Ty) {
        if let Some(Type::Named { path, params, .. }) = ty {
            if path.last().map(|s| s.as_str()) == Some("Map") && params.len() == 2 {
                return (self.ty_of(&params[0]), self.ty_of(&params[1]));
            }
        }
        (Ty::default(), Ty::default())
    }

    /// Infer a `std::vector<T>` type from an array literal's first element.
    fn infer_array(&mut self, elems: &[Expr]) -> Ty {
        let elem = elems.first().map(|e| self.gen_expr(e).1).unwrap_or_default();
        // discard any prelude produced while probing the element type
        self.prelude.clear();
        let inner = if elem.base.is_empty() { "int".to_string() } else { self.decl_spelling(&elem) };
        Ty { base: format!("std::vector<{inner} >"), ..Default::default() }
    }

    /// Key and value `Ty`s of a `std::map<K, V>` from its base spelling,
    /// recovering struct `info` for the value type where possible.
    fn map_kv_ty(&self, map: &Ty) -> (Ty, Ty) {
        if let Some(inner) = map.base.strip_prefix("std::map<").and_then(|s| s.strip_suffix(">")) {
            if let Some((k, v)) = split_top_comma(inner.trim()) {
                let key = Ty { base: k.trim().to_string(), ..Default::default() };
                let v = v.trim();
                let is_ptr = v.ends_with('*');
                let base = v.trim_end_matches('*').trim().to_string();
                let bare = base.rsplit("::").next().unwrap_or(&base).to_string();
                let info = self.prog.resolve_type(&[bare], self.mi).cloned();
                return (key, Ty { base, is_ptr, info, ..Default::default() });
            }
        }
        (Ty::default(), Ty::default())
    }

    /// Value `Ty` of a `std::map<K, V>` from its base spelling.
    fn map_value_ty(&self, map: &Ty) -> Ty {
        if let Some(inner) = map.base.strip_prefix("std::map<").and_then(|s| s.strip_suffix(">")) {
            if let Some((_, v)) = split_top_comma(inner.trim()) {
                let v = v.trim();
                let is_ptr = v.ends_with('*');
                return Ty { base: v.trim_end_matches('*').trim().to_string(), is_ptr, ..Default::default() };
            }
        }
        Ty::default()
    }

    fn map_key_ty(&self, map: &Ty) -> Ty {
        if let Some(inner) = map.base.strip_prefix("std::map<").and_then(|s| s.strip_suffix(">")) {
            if let Some((k, _)) = split_top_comma(inner.trim()) {
                let k = k.trim();
                let is_ptr = k.ends_with('*');
                return Ty { base: k.trim_end_matches('*').trim().to_string(), is_ptr, ..Default::default() };
            }
        }
        Ty::default()
    }

    // ---- scope / locals ------------------------------------------------

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
        self.renames.push(HashMap::new());
        self.owned.push(Vec::new());
    }
    fn pop_scope(&mut self) {
        self.scopes.pop();
        self.renames.pop();
        self.owned.pop();
    }
    fn define_local(&mut self, name: &str, ty: Ty) {
        self.scopes.last_mut().unwrap().insert(name.to_string(), ty);
    }
    fn lookup_local(&self, name: &str) -> Option<Ty> {
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
    fn bind_local_name(&mut self, haxe: &str) -> String {
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
        self.renames.last_mut().unwrap().insert(haxe.to_string(), cpp.clone());
        cpp
    }

    /// Resolve a Haxe local name to its (possibly renamed) C++ identifier.
    fn cpp_name(&self, haxe: &str) -> String {
        for r in self.renames.iter().rev() {
            if let Some(c) = r.get(haxe) {
                return c.clone();
            }
        }
        haxe.to_string()
    }
    fn fresh(&mut self, hint: &str) -> String {
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
    fn loop_var(&mut self, haxe: &str) -> String {
        let cpp = self.fresh(haxe);
        self.renames.last_mut().unwrap().insert(haxe.to_string(), cpp.clone());
        cpp
    }

    // ---- type helpers --------------------------------------------------

    fn ty_of(&self, ht: &Type) -> Ty {
        self.ty_of_in(ht, self.mi)
    }

    /// Like `ty_of`, but resolve the type in the context of module `ctx` (used
    /// for a callee's parameter types, which must resolve where they were
    /// declared — e.g. `Line` inside `native.api` is the native `native::Line`).
    /// The C++ spelling is still relative to the current namespace.
    fn ty_of_in(&self, ht: &Type, ctx: usize) -> Ty {
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
        Ty { base, is_ptr, info, nullable: false, iter: None }
    }

    /// The `Ty` of a callee parameter, folding in optionality. `param_decl`
    /// lowers an *optional* value-struct (`?x:V`) to `V* x = NULL` — the same
    /// pointer shape as a *nullable* `Null<T>`. So optionality and nullability
    /// collapse to one C++ representation: mark such a param `nullable` so call
    /// sites pass a pointer matching the signature (optional and nullable value
    /// types both lower to `T*`). Reference types are already pointers (no
    /// change); `String`/primitive/`Array`/`Map` optionals stay by-value with a
    /// default, so they are left alone.
    fn param_ty_in(&self, p: &Param, ctx: usize) -> Option<Ty> {
        let t = p.ty.as_ref()?;
        let mut ty = self.ty_of_in(t, ctx);
        if p.optional && !ty.is_ptr && crate::codegen::is_value_struct(self.prog, ctx, t) {
            ty.is_ptr = true;
            ty.nullable = true;
        }
        Some(ty)
    }

    fn decl_spelling(&self, ty: &Ty) -> String {
        if ty.is_ptr {
            format!("{}*", ty.base)
        } else {
            ty.base.clone()
        }
    }

    /// The return type as a value (pointer stripped), for building the temporary
    /// that a struct/array `return` populates before any heap wrapping.
    fn return_value_ty(&self) -> Ty {
        Ty { is_ptr: false, ..self.current_ret.clone() }
    }

    /// Heap-wrap a value temporary when the function returns a pointer
    /// (`Null<T>` → `T*`); otherwise return it unchanged.
    fn wrap_ret(&self, code: String) -> String {
        if self.current_ret.is_ptr {
            format!("new {}({code})", self.current_ret.base)
        } else {
            code
        }
    }

    /// The C++ for `return null` given the method's return type: `NULL` for
    /// pointers/primitives, a default-constructed value for struct returns.
    fn return_null_value(&self) -> String {
        if self.current_ret.is_ptr {
            "NULL".to_string()
        } else if self.current_ret.info.is_some() && !self.current_ret.base.is_empty() {
            format!("{}()", self.current_ret.base)
        } else {
            "NULL".to_string()
        }
    }

    fn element_ty(&self, container: &Ty) -> Ty {
        // crude: strip one std::vector<...>/std::map<...> level
        let b = &container.base;
        if let Some(inner) = b.strip_prefix("std::vector<").and_then(|s| s.strip_suffix(">")) {
            let inner = inner.trim();
            let is_ptr = inner.ends_with('*');
            let base = inner.trim_end_matches('*').trim().to_string();
            // Recover the user/native type so member access on the loop variable
            // still resolves (`for (tile in tiles) tile.GetExtents()`).
            let bare = base.rsplit("::").next().unwrap_or(&base).to_string();
            let info = self.prog.resolve_type(&[bare], self.mi).cloned();
            return Ty { base, is_ptr, info, ..Default::default() };
        }
        Ty::default()
    }

    // ---- member/accessor lookup ----------------------------------------

    fn class_field(&self, name: &str) -> Option<&'a Field> {
        self.find_field(self.class, name)
    }

    /// The class-field name an assignment target stores into when it is an
    /// **own-field** store: `this.field`, or a bare `field` that resolves to a
    /// class field rather than a local. `obj.field` on another object is not an
    /// own-field store and yields `None`. This is what lets `field = new X()`
    /// behave identically to `this.field = new X()` for ownership/escape.
    fn assigned_own_field(&self, target: &Expr) -> Option<String> {
        match target {
            Expr::Field(recv, field) if matches!(**recv, Expr::This) => Some(field.clone()),
            Expr::Ident(name) if self.lookup_local(name).is_none() && self.class_field(name).is_some() => {
                Some(name.clone())
            }
            _ => None,
        }
    }

    /// Find a field in `class` or any of its base classes.
    fn find_field(&self, class: &'a Class, name: &str) -> Option<&'a Field> {
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

    fn field_ty(&self, f: &Field) -> Ty {
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

    fn is_nullable_field(&self, name: &str) -> bool {
        self.class
            .ctor
            .as_ref()
            .map(|c| c.params.iter().any(|p| p.optional && p.name == name))
            .unwrap_or(false)
    }

    fn is_accessor_method(&self, name: &str) -> bool {
        self.class.fields.iter().any(|f| {
            (f.get != PropAccess::Default || f.set != PropAccess::Default)
                && (format!("get_{}", f.name) == name || format!("set_{}", f.name) == name)
        })
    }

    fn class_method_return(&self, name: &str) -> Option<Ty> {
        let m = self.class.methods.iter().find(|m| m.name.as_deref() == Some(name))?;
        m.ret.as_ref().map(|t| self.ty_of(t))
    }

    /// Find a field declaration in another type and whether it exposes a getter.
    fn field_has_getter(&self, info: &TypeInfo, name: &str) -> bool {
        self.lookup_field(info, name).map(has_accessor).unwrap_or(false)
    }
    fn field_has_setter(&self, info: &TypeInfo, name: &str) -> bool {
        self.lookup_field(info, name).map(|f| f.set == PropAccess::Set).unwrap_or(false)
    }

    fn lookup_field(&self, info: &TypeInfo, name: &str) -> Option<&'a Field> {
        if let Decl::Class(c) = self.prog.type_decl(info)? {
            return self.find_field(c, name);
        }
        None
    }

    fn accessor_field_ty(&self, info: &TypeInfo, name: &str) -> Ty {
        match self.lookup_field(info, name) {
            Some(f) => match &f.ty {
                Some(t) => self.ty_of(t),
                None => Ty::default(),
            },
            None => Ty::default(),
        }
    }

    fn member_field_ty(&self, info: &TypeInfo, name: &str) -> Option<Ty> {
        match self.prog.type_decl(info)? {
            Decl::Class(c) => self.find_field(c, name).map(|f| self.field_ty(f)),
            Decl::Typedef(Typedef { target: TypedefTarget::Struct(fields), .. }) => fields
                .iter()
                .find(|f| f.name == name)
                .map(|f| self.ty_of(&f.ty)),
            _ => None,
        }
    }

    fn method_return_ty(&self, recv: &Ty, method: &str, args: &[Expr]) -> Ty {
        let Some(info) = &recv.info else { return Ty::default() };
        let Some(decl) = self.prog.type_decl(info) else { return Ty::default() };
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
    fn method_is_overloaded(&self, recv: &Ty, method: &str) -> bool {
        let Some(info) = &recv.info else { return false };
        let Some(decl) = self.prog.type_decl(info) else { return false };
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
    fn resolve_overload_ret(&self, m: &Function, args: &[Expr]) -> Option<Ty> {
        let arg_tys: Vec<Ty> = args.iter().map(|a| self.arg_ty(a)).collect();
        for meta in m.meta.iter().filter(|x| x.name == "overload") {
            let Some(raw) = meta.first_arg() else { continue };
            let Some((params, ret)) = parse_overload_sig(raw) else { continue };
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
    fn overload_mismatch(&self, recv: &Ty, method: &str, args: &[Expr]) -> Option<String> {
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
                if t.base.is_empty() { "?".to_string() } else { t.base.clone() }
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
    fn arg_ty(&self, e: &Expr) -> Ty {
        match e {
            Expr::Int(_) => int_ty(),
            Expr::Float(_) => float_ty(),
            Expr::Bool(_) => bool_ty(),
            Expr::Str { .. } => Ty { base: "std::string".into(), ..Default::default() },
            Expr::Ident(n) => self.lookup_local(n).unwrap_or_default(),
            _ => Ty::default(),
        }
    }

    /// Resolve a bare enum-variant identifier (e.g. `CircleKind`) to its
    /// qualified C++ constant (`demo::ShapeKind_::CircleKind`) and enum type.
    /// Searches enums in scope by variant name, preferring the expected type when
    /// it is an enum (so a name shared by two enums resolves to the contextual
    /// one). Returns `None` when no enum declares the variant — the caller then
    /// treats the identifier as an ordinary unknown.
    fn enum_variant_ref(&self, name: &str) -> Option<(String, Ty)> {
        let mut order: Vec<&TypeInfo> = Vec::new();
        if let Some(info) = self.expected.as_ref().and_then(|t| t.info.as_ref()) {
            if info.kind == TypeKind::Enum {
                order.push(info);
            }
        }
        for info in &self.prog.types {
            if info.kind == TypeKind::Enum {
                order.push(info);
            }
        }
        for info in order {
            if self.enum_has_variant(info, name) {
                let ty = Ty { base: info.name.clone(), info: Some(info.clone()), ..Default::default() };
                return Some((self.enum_constant(info, name), ty));
            }
        }
        None
    }

    /// Whether the enum `info` declares a variant named `name`.
    fn enum_has_variant(&self, info: &TypeInfo, name: &str) -> bool {
        let Some(m) = self.prog.modules.get(info.module_index) else {
            return false;
        };
        m.file.decls.iter().any(|d| {
            matches!(d, Decl::Enum(e) if e.name == info.name && e.variants.iter().any(|v| v.name == name))
        })
    }

    fn enum_constant(&self, info: &TypeInfo, variant: &str) -> String {
        let ns = info.cpp_namespace();
        let prefix = if ns == self.ns || ns.is_empty() {
            String::new()
        } else {
            format!("{}::", ns.join("::"))
        };
        format!("{prefix}{}_::{variant}", info.cpp_name())
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

fn has_accessor(f: &Field) -> bool {
    f.get != PropAccess::Default || f.set != PropAccess::Default
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
            Stmt::Expr(Expr::Assign { op: None, target, value }, _) => {
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

/// Is a `var` initialiser a heap allocation the scope owns — a `new X(...)` that
/// is not a value type?
fn is_heap_new_init(init: &Option<Expr>) -> bool {
    matches!(init, Some(Expr::New(ty, _)) if !is_value_new(ty))
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
        .map(|(i, p)| match (&p.ty, func_params.and_then(|fps| fps.get(i))) {
            (None, Some(fp)) => Param { ty: Some(fp.clone()), ..p.clone() },
            _ => p.clone(),
        })
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
pub(crate) fn render_final_const(prog: &Program, module_index: usize, g: &GlobalVar) -> Option<String> {
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
        ("Math", "POSITIVE_INFINITY") => "((float) HUGE_VAL)",
        ("Math", "NEGATIVE_INFINITY") => "(-(float) HUGE_VAL)",
        // `M_PI` is not standard C++98 / portable to VC6 — use the literal.
        ("Math", "PI") => "((float) 3.141592653589793)",
        _ => return None,
    };
    Some((code.to_string(), float_ty()))
}

fn float_ty() -> Ty {
    Ty { base: "float".into(), ..Default::default() }
}

fn int_ty() -> Ty {
    Ty { base: "int".into(), ..Default::default() }
}

fn bool_ty() -> Ty {
    Ty { base: "bool".into(), ..Default::default() }
}

fn str_ty() -> Ty {
    Ty { base: "std::string".into(), ..Default::default() }
}

/// A bare `Type::Named` from a single type name (used to map a parsed overload
/// signature's Haxe type names back through the normal type machinery).
fn type_from_name(name: &str) -> Type {
    Type::Named { path: vec![name.to_string()], params: Vec::new(), optional: false, line: 0 }
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
        .map(|p| p.split_once(':').map(|(_, t)| t.trim()).unwrap_or("").to_string())
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
        Add => "+", Sub => "-", Mul => "*", Div => "/", Mod => "%",
        Eq => "==", Ne => "!=", Lt => "<", Gt => ">", Le => "<=", Ge => ">=",
        And => "&&", Or => "||",
        BitAnd => "&", BitOr => "|", BitXor => "^", Shl => "<<", Shr => ">>",
    }
}

fn binop_result_ty(op: BinOp, lhs: Ty) -> Ty {
    use BinOp::*;
    match op {
        Eq | Ne | Lt | Gt | Le | Ge | And | Or => Ty { base: "bool".into(), ..Default::default() },
        _ => lhs,
    }
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

fn float_lit(s: &str) -> String {
    if s.ends_with('f') || s.ends_with('F') || s.contains('x') {
        s.to_string()
    } else {
        format!("{s}f")
    }
}

fn escape_str(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
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
        } else if b[i] == b'$' && i + 1 < b.len() && (b[i + 1].is_ascii_alphabetic() || b[i + 1] == b'_') {
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
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%")
}

fn cap(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
