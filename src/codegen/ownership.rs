//! Destructor ownership analysis.
//!
//! Determines which pointers a class must `delete` in its destructor, erring
//! toward leaking rather than risking a double-free on a borrowed pointer:
//!
//! 1. **Allocated here.** A pointer field whose value is produced by `new`
//!    within this class ("I allocated it, I free it"). Injected dependencies
//!    (constructor parameters such as `IEngine*`, forwarded to `super(...)`)
//!    are never `new`ed here, so they are never deleted.
//! 2. **Handed up to a base's opaque field.** When this class forwards a
//!    `Null<T>` constructor parameter into a base class's `void*`/`Dynamic`
//!    field, only this class knows the concrete type, so it deletes it with a
//!    cast (`delete (T*)this->data;`).

use crate::ast::*;
use crate::sema::Program;

/// The `delete` statements a class's destructor should run (empty when it owns
/// nothing). Returned as ready-to-emit C++ lines.
pub(crate) fn owned_deletes(prog: &Program, mi: usize, ns: &[String], c: &Class) -> Vec<String> {
    let mut deletes = Vec::new();
    // Fields already deleted, so the explicit `@owned` pass never double-emits a
    // delete the automatic rules already produced.
    let mut emitted = std::collections::HashSet::new();
    let field_names = own_field_names(c);

    // (1) Fields this class allocated with `new`, emitted in declaration order. A
    //     bare `field = new X()` counts the same as `this.field = new X()`.
    let mut allocated = std::collections::HashSet::new();
    for f in c.ctor.iter().chain(c.methods.iter()).filter_map(|f| f.body.as_ref()) {
        collect_new_assigns(f, &field_names, &mut allocated);
    }
    for f in &c.fields {
        if allocated.contains(&f.name) && emitted.insert(f.name.clone()) {
            deletes.push(format!("delete this->{};", f.name));
        }
    }

    // (2) A `Null<T>` parameter forwarded into a base's `void*`/`Dynamic` field.
    if let Some(forward) = base_void_forward(prog, mi, c) {
        let cpp = prog.map_type_base(&forward.inner, mi, ns);
        deletes.push(format!("delete ({cpp}*)this->{};", forward.base_field));
    }

    // (3) Container fields holding pointers this class `new`ed into them
    //     (`tileset.push(new Tile(...)); this.tilesets.push(tileset);`).
    for (field, depth) in owned_container_fields(prog, mi, ns, c) {
        if emitted.insert(field.clone()) {
            emit_container_delete(&field, depth, &mut deletes);
        }
    }

    // (4) Fields the developer explicitly marked `@owned`: the tie-breaker for
    //     injected pointers (a ctor parameter stored into a field) that the
    //     automatic rules above cannot tell apart from a borrow. Purely additive
    //     — a field already deleted above is skipped. A container is freed
    //     element-wise; a scalar pointer with a plain `delete`.
    for f in &c.fields {
        if !f.meta.iter().any(|m| m.name == "owned") {
            continue;
        }
        let Some(ty) = &f.ty else { continue };
        if let Some(depth) = container_depth_if_pointer_leaf(prog, mi, ns, ty) {
            if emitted.insert(f.name.clone()) {
                emit_container_delete(&f.name, depth, &mut deletes);
            }
        } else if prog.map_type_use(ty, mi, ns).ends_with('*') && emitted.insert(f.name.clone()) {
            deletes.push(format!("delete this->{};", f.name));
        }
    }

    deletes
}

/// The bare names of a class's own fields (so a bare `field = …` can be matched
/// against the same key as `this.field = …`).
fn own_field_names(c: &Class) -> std::collections::HashSet<String> {
    c.fields.iter().map(|f| f.name.clone()).collect()
}

/// The set of fields the **current heuristics** have the destructor free: those
/// allocated here with `new`, the `@owned`-marked ones, and the owned containers.
/// Exposed so the new escape analysis (`sema::escape`) can be diffed against it
/// during the M2→M5 transition. (Excludes the base-`void*` forward, which is not an
/// own field.)
// Used by the `sema::escape` diff tests now; becomes a real consumer at the M5
// cutover, so it is intentionally retained ahead of that.
#[allow(dead_code)]
pub(crate) fn owned_field_set(
    prog: &Program,
    mi: usize,
    ns: &[String],
    c: &Class,
) -> std::collections::HashSet<String> {
    let mut s = owned_pointer_fields(c);
    s.extend(owned_container_field_names(prog, mi, ns, c));
    for f in &c.fields {
        if f.meta.iter().any(|m| m.name == "owned") {
            s.insert(f.name.clone());
        }
    }
    s
}

/// Emit the nested `for` loops that `delete` every leaf pointer in an owned
/// container field, as individual relative-indented lines.
fn emit_container_delete(field: &str, depth: usize, out: &mut Vec<String>) {
    let mut idx = String::new();
    for d in 0..depth {
        let pad = "\t".repeat(d);
        let var = format!("_i{d}");
        out.push(format!(
            "{pad}for (size_t {var} = 0; {var} < this->{field}{idx}.size(); ++{var}) {{"
        ));
        idx.push_str(&format!("[{var}]"));
    }
    out.push(format!("{}delete this->{field}{idx};", "\t".repeat(depth)));
    for d in (0..depth).rev() {
        out.push(format!("{}}}", "\t".repeat(d)));
    }
}

/// The names of container fields this class owns (frees in its destructor).
fn owned_container_field_names(
    prog: &Program,
    mi: usize,
    ns: &[String],
    c: &Class,
) -> std::collections::HashSet<String> {
    owned_container_fields(prog, mi, ns, c)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// Bare names of the containers into which a pushed `new` comes to rest in
/// class-level storage — so the `new` *escapes* the current scope and must be
/// emitted inline rather than hoisted into a scope-owned local that would be
/// deleted at scope close (leaving a dangling pointer the destructor double-frees).
///
/// This is the owned class-field containers **plus** any local container that
/// transitively flows into one (`tileset.push(new Tile()); this.tilesets.push(tileset)`).
/// Returned as bare names (`this.` stripped) so codegen can match a `push`
/// receiver written either `field.push(...)` or `local.push(...)`.
pub(crate) fn escaping_new_receivers(
    prog: &Program,
    mi: usize,
    ns: &[String],
    c: &Class,
) -> std::collections::HashSet<String> {
    let fields: std::collections::HashSet<String> = c.fields.iter().map(|f| f.name.clone()).collect();
    let mut edges: Vec<(String, Flow)> = Vec::new();
    for f in c.ctor.iter().chain(c.methods.iter()).filter_map(|f| f.body.as_ref()) {
        collect_push_edges(f, &fields, &mut edges);
    }
    // Seed with the owned class-field containers (keyed `this.field`), then
    // backward-propagate through `recv.push(local)` flows: if a receiver escapes,
    // the local it pulls from escapes too.
    let mut keys: std::collections::HashSet<String> = owned_container_field_names(prog, mi, ns, c)
        .into_iter()
        .map(|f| format!("this.{f}"))
        .collect();
    loop {
        let mut changed = false;
        for (recv, flow) in &edges {
            if let Flow::Name(n) = flow {
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

/// Container fields whose leaf elements are pointers this class allocates with
/// `new` (so it owns them): returns `(field, nesting depth)`.
fn owned_container_fields(prog: &Program, mi: usize, ns: &[String], c: &Class) -> Vec<(String, usize)> {
    let bearing = new_bearing_names(c);
    let mut out = Vec::new();
    for f in &c.fields {
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

/// If `ty` is a nested `Array<...>` whose leaf element maps to a pointer, return
/// the nesting depth; otherwise `None`.
fn container_depth_if_pointer_leaf(prog: &Program, mi: usize, ns: &[String], ty: &Type) -> Option<usize> {
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
    prog.map_type_use(cur, mi, ns).ends_with('*').then_some(depth)
}

/// What flows into a container via `push`/`insert`.
enum Flow {
    New,
    Name(String),
}

/// Names (locals as `n`, fields as `this.f`) that receive `new`-allocated
/// elements through `push`/`insert`, directly or transitively.
fn new_bearing_names(c: &Class) -> std::collections::HashSet<String> {
    // Field names, so a bare `field.push(...)` (Haxe lets you omit `this.`) keys
    // the same as the `this.field.push(...)` form.
    let fields: std::collections::HashSet<String> = c.fields.iter().map(|f| f.name.clone()).collect();
    let mut edges: Vec<(String, Flow)> = Vec::new();
    for f in c.ctor.iter().chain(c.methods.iter()).filter_map(|f| f.body.as_ref()) {
        collect_push_edges(f, &fields, &mut edges);
    }
    let mut bearing = std::collections::HashSet::new();
    loop {
        let mut changed = false;
        for (recv, flow) in &edges {
            let owns = match flow {
                Flow::New => true,
                Flow::Name(n) => bearing.contains(n),
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
    fields: &std::collections::HashSet<String>,
    out: &mut Vec<(String, Flow)>,
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

fn push_edge_from_expr(
    e: &Expr,
    fields: &std::collections::HashSet<String>,
    out: &mut Vec<(String, Flow)>,
) {
    if let Expr::Call(target, args) = e {
        if let Expr::Field(recv, method) = &**target {
            let value = match (method.as_str(), args.len()) {
                ("push", 1) => Some(&args[0]),
                ("insert", 2) => Some(&args[1]),
                _ => None,
            };
            if let (Some(key), Some(value)) = (receiver_key(recv, fields), value) {
                let flow = match value {
                    Expr::New(ty, _) if !is_container_type(ty) => Some(Flow::New),
                    Expr::Ident(n) => Some(Flow::Name(n.clone())),
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
fn receiver_key(recv: &Expr, fields: &std::collections::HashSet<String>) -> Option<String> {
    match recv {
        Expr::Ident(n) if fields.contains(n) => Some(format!("this.{n}")),
        Expr::Ident(n) => Some(n.clone()),
        Expr::Field(r, f) if matches!(**r, Expr::This) => Some(format!("this.{f}")),
        _ => None,
    }
}

/// The set of pointer fields this class allocates with `new` (so it owns them):
/// used both for destructor deletes and for delete-before-overwrite on reassignment.
pub(crate) fn owned_pointer_fields(c: &Class) -> std::collections::HashSet<String> {
    let fields = own_field_names(c);
    let mut allocated = std::collections::HashSet::new();
    for f in c.ctor.iter().chain(c.methods.iter()).filter_map(|f| f.body.as_ref()) {
        collect_new_assigns(f, &fields, &mut allocated);
    }
    allocated
}

/// Constructor parameter names whose value the class takes ownership of: the
/// parameter is stored straight into a field the destructor frees (an `@owned`
/// field, a `new`-allocated field, or an owned container). A `new` passed to such
/// a parameter is owned by the constructed object — not the caller's scope — so it
/// must be emitted inline, never hoisted into a scope-owned local (which would
/// double-free once the object's own destructor runs).
pub(crate) fn owned_ctor_params(
    prog: &Program,
    mi: usize,
    ns: &[String],
    c: &Class,
) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    let Some(ctor) = c.ctor.as_ref() else { return out };
    // Every field the destructor frees.
    let mut owned = owned_pointer_fields(c);
    owned.extend(owned_container_field_names(prog, mi, ns, c));
    for f in &c.fields {
        if f.meta.iter().any(|m| m.name == "owned") {
            owned.insert(f.name.clone());
        }
    }
    // Parameters assigned straight into one of those fields (`this.field = param`).
    for p in &ctor.params {
        if let Some(field) = base_ctor_field_for(ctor, &p.name) {
            if owned.contains(&field) {
                out.insert(p.name.clone());
            }
        }
    }
    out
}

/// Walk statements collecting field names assigned a `new` expression — both the
/// qualified `this.field = new X(...)` and the bare `field = new X(...)` (Haxe
/// lets you omit `this.`; `fields` is the class's own field names).
fn collect_new_assigns(
    stmts: &[Stmt],
    fields: &std::collections::HashSet<String>,
    out: &mut std::collections::HashSet<String>,
) {
    for st in stmts {
        walk_stmt(st, fields, out);
    }
}

fn walk_stmt(
    st: &Stmt,
    fields: &std::collections::HashSet<String>,
    out: &mut std::collections::HashSet<String>,
) {
    match st {
        Stmt::Expr(Expr::Assign { op: None, target, value }, _) => {
            if let Expr::New(ty, _) = &**value {
                // `new Array<T>()` / `new Map<K,V>()` lower to *value* containers,
                // not heap pointers, so they are not owned allocations.
                if let (Some(field), false) = (assigned_field(target, fields), is_container_type(ty)) {
                    out.insert(field);
                }
            }
        }
        Stmt::If { then, els, .. } => {
            walk_stmt(then, fields, out);
            if let Some(e) = els {
                walk_stmt(e, fields, out);
            }
        }
        Stmt::For { body, .. } | Stmt::While { body, .. } => walk_stmt(body, fields, out),
        Stmt::Block(stmts) => collect_new_assigns(stmts, fields, out),
        Stmt::Switch { cases, default, .. } => {
            for case in cases {
                collect_new_assigns(&case.body, fields, out);
            }
            if let Some(d) = default {
                collect_new_assigns(d, fields, out);
            }
        }
        _ => {}
    }
}

/// The class field an assignment target stores into, written either `this.field`
/// or bare `field` (an unqualified own-field name). `obj.field` on another object
/// is not an own-field store and yields `None`.
fn assigned_field(target: &Expr, fields: &std::collections::HashSet<String>) -> Option<String> {
    match target {
        Expr::Field(recv, field) if matches!(**recv, Expr::This) => Some(field.clone()),
        Expr::Ident(name) if fields.contains(name) => Some(name.clone()),
        _ => None,
    }
}

/// Details of a `Null<T>` parameter forwarded to a base class's `void*` field.
struct Forward {
    /// Inner type `T` of the `Null<T>` parameter (for the delete cast).
    inner: Type,
    /// The base class field the pointer comes to rest in.
    base_field: String,
}

/// Detect the FogEffect-style hand-off: a `Null<T>` constructor parameter passed
/// through `super(...)` into a base class field that is `void*`/`Dynamic`.
fn base_void_forward(prog: &Program, mi: usize, c: &Class) -> Option<Forward> {
    let ctor = c.ctor.as_ref()?;
    let body = ctor.body.as_ref()?;
    let super_args = body.iter().find_map(|st| match st {
        Stmt::Expr(Expr::Call(target, args), _) if matches!(**target, Expr::Super) => Some(args),
        _ => None,
    })?;

    let base_path = match c.extends.as_ref()? {
        Type::Named { path, .. } => path,
        _ => return None,
    };
    let base_info = prog.resolve_type(base_path, mi)?;
    let base_class = match prog.type_decl(base_info)? {
        Decl::Class(bc) => bc,
        _ => return None,
    };
    let base_ctor = base_class.ctor.as_ref()?;

    for (i, arg) in super_args.iter().enumerate() {
        let Expr::Ident(pname) = arg else { continue };
        // Subclass parameter of type `Null<T>`?
        let Some(param) = ctor.params.iter().find(|p| &p.name == pname) else { continue };
        let Some(inner) = null_inner(&param.ty) else { continue };
        // Base constructor parameter at this position → the field it stores into.
        let Some(base_param) = base_ctor.params.get(i) else { continue };
        let Some(field) = base_ctor_field_for(base_ctor, &base_param.name) else { continue };
        if base_field_is_void(base_class, &field) {
            return Some(Forward { inner, base_field: field });
        }
    }
    None
}

/// Is this a Haxe container type (`Array`/`Map`), which lowers to a value
/// `std::vector`/`std::map` rather than a heap pointer?
fn is_container_type(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Named { path, .. }
            if matches!(path.last().map(|s| s.as_str()), Some("Array") | Some("Map"))
    )
}

/// The inner type `T` of a `Null<T>` type annotation.
fn null_inner(ty: &Option<Type>) -> Option<Type> {
    match ty {
        Some(Type::Named { path, params, .. })
            if path.last().map(|s| s.as_str()) == Some("Null") && params.len() == 1 =>
        {
            Some(params[0].clone())
        }
        _ => None,
    }
}

/// The field a base constructor assigns a given parameter to (`this.field = param`).
fn base_ctor_field_for(ctor: &Function, param: &str) -> Option<String> {
    let body = ctor.body.as_ref()?;
    body.iter().find_map(|st| match st {
        Stmt::Expr(Expr::Assign { op: None, target, value }, _) => {
            if let (Expr::Field(recv, field), Expr::Ident(p)) = (&**target, &**value) {
                if matches!(**recv, Expr::This) && p == param {
                    return Some(field.clone());
                }
            }
            None
        }
        _ => None,
    })
}

/// Is the named field of a class typed `{}` / `Dynamic` (i.e. `void*`)?
fn base_field_is_void(c: &Class, field: &str) -> bool {
    c.fields.iter().any(|f| {
        f.name == field
            && match &f.ty {
                Some(Type::Anon(fields)) => fields.is_empty(),
                Some(Type::Named { path, .. }) => {
                    matches!(path.last().map(|s| s.as_str()), Some("Dynamic") | Some("Any"))
                }
                _ => false,
            }
    })
}
