//! `.cpp` source generation: constructor and method bodies.
//!
//! This is where Haxe statements and expressions are transpiled to C++. The
//! generator carries a small amount of type information so it can choose between
//! `.` and `->` for member access, rewrite property-accessor access to
//! `GetX()`/`SetX()`, qualify enum constants, and desugar the Haxe constructs
//! described in `SKILL.md`.
//!
//! The validation gate is *compilation*, not byte-equality with the goldens, so
//! the output favours correct, compilable C++ over matching a specific layout.

use std::collections::HashMap;
use std::fmt::Write;

use crate::ast::*;
use crate::sema::{Program, TypeInfo, TypeKind};

/// Generate the `.cpp` for a module, or `None` if it has no class to implement.
/// Uses the default buried-`Null<T>` extraction depth (1).
pub fn generate_source(prog: &Program, module_index: usize) -> Option<String> {
    generate_source_diagnostics(prog, module_index, 1).map(|(text, _, _)| text)
}

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
) -> Option<(String, Vec<(usize, String)>, Vec<(usize, String)>)> {
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
            let mut bg = BodyGen::new(prog, module_index, &empty, extract_depth);
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
                let mut bg = BodyGen::new(prog, module_index, &empty, extract_depth);
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
            let mut bg = BodyGen::new(prog, module_index, &empty, extract_depth);
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
            let mut g = BodyGen::new(prog, module_index, c, extract_depth);
            out.push_str(&g.class_impl());
            warnings.append(&mut g.warnings);
            errors.append(&mut g.errors);
        }
    }

    if has_ns_body {
        for part in m.package.iter().rev() {
            let _ = writeln!(out, "}} // namespace {part}");
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
            let mut bg = BodyGen::new(prog, module_index, &empty, extract_depth);
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
}

impl<'a> BodyGen<'a> {
    fn new(prog: &'a Program, mi: usize, class: &'a Class, max_extract_depth: usize) -> Self {
        let ns = prog.modules[mi].package.clone();
        let owned_containers =
            crate::codegen::ownership::escaping_new_receivers(prog, mi, &ns, class);
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
            owned_fields: crate::codegen::ownership::owned_pointer_fields(class),
            owned_containers,
            expected: None,
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
        collect_escaping(body, &mut self.escaping);
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
        self.push_scope();
        self.bind_params(params);
        let ret_ty = self.resolve_lambda_ret(ret, body, g.ty.as_ref());
        let ret_cpp = self.decl_spelling(&ret_ty);
        let plist = if with_defaults {
            params
                .iter()
                .map(|p| crate::codegen::param_decl(self.prog, self.mi, &self.ns, p))
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            self.header_params(params)
        };
        self.pop_scope();
        Some(format!("{ret_cpp} {}({plist})", g.name))
    }

    /// Full definition of a top-level free function (`static` when file-local).
    fn free_fn_def(&mut self, g: &GlobalVar) -> String {
        let Some((params, ret, body)) = lambda_parts(g) else { return String::new() };
        self.current_fn = g.name.clone();
        self.push_scope();
        self.bind_params(params);
        let ret_ty = self.resolve_lambda_ret(ret, body, g.ty.as_ref());
        self.current_ret = ret_ty.clone();
        let ret_cpp = self.decl_spelling(&ret_ty);
        let plist = self.header_params(params);
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
    /// gives the return type, (3) a `cast(expr, T)` arrow body. Per `SKILL.md`, a
    /// developer hints the return type via (2) or (3); absent any hint it falls back
    /// to `float` (the numeric helpers in the corpus). `decl_ty` is the binding's
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
            Stmt::Var { name, ty, init, is_final: _, line } => {
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
                        let (kty, vty) = self.map_kv_ast_ty(ty.as_ref());
                        self.expand_map_into_local(name, &map_ty, &kty, &vty, pairs, ind, out);
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
                self.define_local(name, var_ty);
                // A non-escaping local holding a fresh `new` / nullable heap result
                // is owned by this scope and deleted when it closes.
                let owns_heap = is_heap_new_init(init) || nullable_init;
                if owns_heap && !self.escaping.contains(name) {
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
                // arguments are owned by the receiver, not this scope.
                if let Expr::Assign { op: None, target, .. } = e {
                    if matches!(&**target, Expr::Field(..)) {
                        self.new_args_escape = true;
                    }
                    // Delete-before-overwrite: reassigning an owned pointer field
                    // (outside the constructor, where it is NULL-initialised) frees
                    // the prior value first.
                    if self.current_fn != "new" {
                        if let Expr::Field(recv, field) = &**target {
                            if matches!(**recv, Expr::This) && self.owned_fields.contains(field) {
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
            Stmt::For { var, iter, body, line } => {
                self.current_line = *line;
                self.gen_for(var, iter, body, ind, out)
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

    fn gen_for(&mut self, var: &str, iter: &Iterable, body: &Stmt, ind: usize, out: &mut String) {
        let t = "\t".repeat(ind);
        self.push_scope();
        match iter {
            Iterable::Range(start, end) => {
                let (s, _) = self.gen_expr(start);
                let (e, _) = self.gen_expr(end);
                self.define_local(var, Ty { base: "int".into(), ..Default::default() });
                let _ = writeln!(out, "{t}for (int {var} = {s}; {var} < {e}; ++{var}) {{");
                self.gen_block_inner(body, ind + 1, out);
                self.emit_owned_deletes(out, ind + 1);
                let _ = writeln!(out, "{t}}}");
            }
            Iterable::Coll(coll) => {
                // for (item in collection) → index loop with element binding
                let (c, cty) = self.gen_expr(coll);
                // A nullable container is a pointer — dereference it to iterate.
                let access = if cty.is_ptr { format!("(*{c})") } else { c };
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
                (
                    format!("{t}for (int {var} = {s}; {var} < {e}; ++{var}) {{\n"),
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

        let _ = write!(buf, "{t}{container} {tmp};\n");
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
            // Block-bodied map lambdas are not used by the corpus.
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
                    return format!("{}", self.enum_constant(info, name));
                }
            }
        }
        self.gen_expr(pat).0
    }

    // ---- expression entry points ---------------------------------------

    /// Generate a statement-level expression, handling assignment to property
    /// accessors (`a.x = v` → `a->SetX(v)`).
    fn gen_assign_or_expr(&mut self, e: &Expr) -> (String, Ty) {
        if let Expr::Assign { op: None, target, value } = e {
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
            Expr::Str { raw, interpolated } => self.gen_string(raw, *interpolated),
            Expr::This => (
                "this".into(),
                Ty {
                    base: self.class.name.clone(),
                    is_ptr: true,
                    info: self.prog.resolve_type(&[self.class.name.clone()], self.mi).cloned(),
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
                let a = self.gen_args_typed(args, &param_tys, false);
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
                    &tmp, &map_ty, &Ty::default(), &Ty::default(), pairs, self.prelude_ind, &mut buf,
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
            Expr::TypeCheck { expr, .. } => self.gen_expr(expr),
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
        // when it is used from a different namespace — e.g. `mucus::MAX_CHARACTERS`,
        // or `game::ALIENBEACH_SCENE_ID` inside a global-scope `extern "C"` export.
        if let Some(qref) = self.prog.global_final_ref(name, self.mi, &self.ns) {
            return (qref, Ty::default());
        }
        // free function / global / unknown — pass through
        (name.to_string(), Ty::default())
    }

    fn gen_field(&mut self, recv: &Expr, name: &str) -> (String, Ty) {
        // Enum constant: `EnumType.Variant`
        if let Expr::Ident(tname) = recv {
            if self.lookup_local(tname).is_none() && self.class_field(tname).is_none() {
                if let Some(info) = self.prog.resolve_type(&[tname.clone()], self.mi).cloned() {
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
        let a = self.gen_args_typed(args, &param_tys, false);
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
    /// typed by the callee's parameter (per SKILL: anon struct arg → temp var).
    fn gen_args_typed(&mut self, args: &[Expr], param_tys: &[Option<Ty>], coerce_str: bool) -> String {
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
                    // caller frees it) unless the receiver escapes.
                    Expr::New(nty, _) if !is_value_new(nty) => {
                        let (code, vty) = self.gen_expr(a);
                        self.place_new_arg(code, vty)
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
            "pop" => Some((format!("{rcode}.back()"), Ty::default())),
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
        if !interpolated || !raw.contains("${") {
            return (format!("\"{}\"", escape_str(raw)), str_ty);
        }
        // String interpolation → sprintf into a stack buffer, returned as a temp.
        let (literal, exprs) = split_interpolation(raw);
        if exprs.is_empty() {
            return (format!("\"{}\"", escape_str(raw)), str_ty);
        }
        let mut fmt = String::new();
        let mut args = Vec::new();
        let mut lit_chars = 0usize;
        for seg in literal {
            match seg {
                Seg::Lit(s) => {
                    lit_chars += s.len();
                    fmt.push_str(&printf_escape(&s));
                }
                Seg::Expr(src) => {
                    let (spec, arg) = match crate::parser::parse_expression(&src) {
                        Ok(e) => {
                            let (code, ty) = self.gen_expr(&e);
                            self.spec_for(&code, &ty)
                        }
                        Err(_) => ("%s".to_string(), format!("\"{}\"", escape_str(&src))),
                    };
                    fmt.push_str(&spec);
                    args.push(arg);
                }
            }
        }
        // Buffer sizing per SKILL: (n interpolations * 50) + literal characters.
        let size = exprs.len() * 50 + lit_chars + 1;
        let buf = self.fresh("buf");
        let t = "\t".repeat(self.prelude_ind);
        let mut pre = String::new();
        let _ = writeln!(pre, "{t}char {buf}[{size}];");
        let _ = writeln!(pre, "{t}sprintf({buf}, \"{fmt}\", {});", args.join(", "));
        self.prelude.push_str(&pre);
        (format!("std::string({buf})"), str_ty)
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
                    let (kty, vty) = self.map_kv_ty(&mt);
                    let tmp = self.fresh("f");
                    self.expand_map_into_local(&tmp, &mt, &kty, &vty, pairs, ind, out);
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
            // Nested struct values aren't needed by the corpus; bail (→ caller
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
        let builder = format!("_hatchet_init_{name}");
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
        _key: &Ty,
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

    // ---- type helpers --------------------------------------------------

    fn ty_of(&self, ht: &Type) -> Ty {
        self.ty_of_in(ht, self.mi)
    }

    /// Like `ty_of`, but resolve the type in the context of module `ctx` (used
    /// for a callee's parameter types, which must resolve where they were
    /// declared — e.g. `Line` inside `mucus.api` is the native `mucus::Line`).
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
    /// sites pass a pointer matching the signature (see SKILL: optional/nullable
    /// value types both lower to `T*`). Reference types are already pointers (no
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
        self.lookup_field(info, name).map(|f| has_accessor(f)).unwrap_or(false)
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
fn collect_escaping(stmts: &[Stmt], out: &mut std::collections::HashSet<String>) {
    for st in stmts {
        match st {
            Stmt::Expr(Expr::Assign { op: None, target, value }, _) => {
                if let (Expr::Field(recv, _), Expr::Ident(name)) = (&**target, &**value) {
                    if matches!(**recv, Expr::This) {
                        out.insert(name.clone());
                    }
                }
            }
            Stmt::Return(Some(Expr::Ident(name)), _) => {
                out.insert(name.clone());
            }
            Stmt::If { then, els, .. } => {
                collect_escaping(std::slice::from_ref(then), out);
                if let Some(e) = els {
                    collect_escaping(std::slice::from_ref(e), out);
                }
            }
            Stmt::For { body, .. } | Stmt::While { body, .. } => {
                collect_escaping(std::slice::from_ref(body), out);
            }
            Stmt::Block(stmts) => collect_escaping(stmts, out),
            Stmt::Switch { cases, default, .. } => {
                for c in cases {
                    collect_escaping(&c.body, out);
                }
                if let Some(d) = default {
                    collect_escaping(d, out);
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
                Some("Array") | Some("Map") | Some("String")
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
    let mut bg = BodyGen::new(prog, module_index, &empty, 1);
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
