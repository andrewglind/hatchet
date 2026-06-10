//! Destructor delete *emission*.
//!
//! Ownership itself — *which* fields a class must free — is decided by the escape
//! analysis (`sema::escape`); this module only turns that decision into ready-to-emit
//! C++ `delete` lines, choosing a plain `delete` for a scalar pointer or nested
//! element-wise loops for a pointer container, and handling the one case the escape
//! analysis cannot see: a `Null<T>` constructor parameter forwarded into a *base*
//! class's `void*`/`Dynamic` field, which only this class knows the concrete type of.

use crate::ast::*;
use crate::sema::Program;

/// The `delete` statements a class's destructor should run (empty when it owns
/// nothing). Returned as ready-to-emit C++ lines.
pub(crate) fn owned_deletes(prog: &Program, mi: usize, ns: &[String], c: &Class) -> Vec<String> {
    let mut deletes = Vec::new();
    let mut emitted = std::collections::HashSet::new();

    // *Which* fields the destructor frees comes from the escape analysis
    // (`sema::escape`). *How* to free each — a plain `delete` for a scalar pointer or
    // nested element-wise loops for a pointer container — is decided here from the
    // field's C++ type. Emitted in field-declaration order.
    let owned = crate::sema::escape::analyze_class(prog, mi, c).owned_fields;
    for f in &c.fields {
        if !owned.contains(&f.name) || !emitted.insert(f.name.clone()) {
            continue;
        }
        let Some(ty) = &f.ty else { continue };
        if let Some(depth) = crate::sema::escape::container_depth_if_pointer_leaf(prog, mi, ns, ty) {
            emit_container_delete(&f.name, depth, &mut deletes);
        } else if prog.map_type_use(ty, mi, ns).ends_with('*') {
            deletes.push(format!("delete this->{};", f.name));
        }
    }

    // A `Null<T>` parameter forwarded into a *base* class's `void*`/`Dynamic` field is
    // not one of this class's own fields, so the escape analysis (which only sees `c`'s
    // fields) does not cover it — only this class knows the concrete type to cast and
    // delete, so handle it explicitly.
    if let Some(forward) = base_void_forward(prog, mi, c) {
        let cpp = prog.map_type_base(&forward.inner, mi, ns);
        deletes.push(format!("delete ({cpp}*)this->{};", forward.base_field));
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
