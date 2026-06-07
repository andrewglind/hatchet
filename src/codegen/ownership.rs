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

    // (1) Fields this class allocated with `new`, emitted in declaration order.
    let mut allocated = std::collections::HashSet::new();
    for f in c.ctor.iter().chain(c.methods.iter()).filter_map(|f| f.body.as_ref()) {
        collect_new_assigns(f, &mut allocated);
    }
    for f in &c.fields {
        if allocated.contains(&f.name) {
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
        emit_container_delete(&field, depth, &mut deletes);
    }

    deletes
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
    let mut allocated = std::collections::HashSet::new();
    for f in c.ctor.iter().chain(c.methods.iter()).filter_map(|f| f.body.as_ref()) {
        collect_new_assigns(f, &mut allocated);
    }
    allocated
}

/// Walk statements collecting field names assigned a `new` expression
/// (`this.field = new X(...)`).
fn collect_new_assigns(stmts: &[Stmt], out: &mut std::collections::HashSet<String>) {
    for st in stmts {
        walk_stmt(st, out);
    }
}

fn walk_stmt(st: &Stmt, out: &mut std::collections::HashSet<String>) {
    match st {
        Stmt::Expr(Expr::Assign { op: None, target, value }, _) => {
            if let (Expr::Field(recv, field), Expr::New(ty, _)) = (&**target, &**value) {
                // `new Array<T>()` / `new Map<K,V>()` lower to *value* containers,
                // not heap pointers, so they are not owned allocations.
                if matches!(**recv, Expr::This) && !is_container_type(ty) {
                    out.insert(field.clone());
                }
            }
        }
        Stmt::If { then, els, .. } => {
            walk_stmt(then, out);
            if let Some(e) = els {
                walk_stmt(e, out);
            }
        }
        Stmt::For { body, .. } | Stmt::While { body, .. } => walk_stmt(body, out),
        Stmt::Block(stmts) => collect_new_assigns(stmts, out),
        Stmt::Switch { cases, default, .. } => {
            for case in cases {
                collect_new_assigns(&case.body, out);
            }
            if let Some(d) = default {
                collect_new_assigns(d, out);
            }
        }
        _ => {}
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
