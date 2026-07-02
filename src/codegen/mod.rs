//! C++ code generation.
//!
//! The header (`.h`) for every module that needs one — enums, struct typedefs,
//! alias typedefs, interfaces, and classes (constructor/method declarations,
//! inline getters/setters from property accessors, and access-grouped fields) —
//! is built by [`header`]. Method/constructor *bodies* and `.cpp` files are
//! produced by [`source`]. The `--header-only` amalgamation lives in [`amalgam`].
//!
//! This module is the shared core: the public re-exports below, plus the small
//! param / type / enum / literal helper functions used by both `header` and
//! `source` (kept here so both submodules reach them as descendants).

use crate::ast::*;
use crate::sema::{Program, TypeKind};

mod amalgam;
mod header;
pub mod holder;
pub mod ownership;
pub mod source;

pub use amalgam::generate_amalgamation;
pub use header::{generate_header, generate_header_with, HeaderOpts, HeaderOutput};
pub use source::{generate_source, generate_source_diagnostics};

// ---- reusable param / type helpers (shared with source.rs) -------------

/// A parameter declaration (with default value), per the pointer/reference rules:
/// reference types → `T*`; nullable value-struct → `T*`; non-optional
/// `String`/struct/container → `const T&`; primitives by value.
pub(crate) fn param_decl(prog: &Program, mi: usize, ns: &[String], p: &Param) -> String {
    let name = &p.name;
    let ty = p.ty.as_ref();

    if let Some(t) = ty {
        if let Type::Named { path, params, .. } = t {
            if params.is_empty() && prog.is_reference(path, mi) {
                let base = prog.map_type_base(t, mi, ns);
                return if p.optional {
                    format!("{base}* {name} = NULL")
                } else {
                    format!("{base}* {name}")
                };
            }
        }
    }

    let base_name = ty.and_then(|t| t.base_name());
    if let Some(t) = ty {
        if is_value_struct(prog, mi, t) {
            let base = prog.map_type_base(t, mi, ns);
            return if p.optional {
                format!("{base}* {name} = NULL")
            } else {
                format!("const {base}& {name}")
            };
        }
    }

    if base_name == Some("String") {
        return if p.optional {
            format!("std::string {name} = {}", param_default(prog, mi, ns, p))
        } else {
            format!("const std::string& {name}")
        };
    }
    if matches!(base_name, Some("Array") | Some("Map")) && !p.optional {
        let t = prog.map_type_use(ty.unwrap(), mi, ns);
        return format!("const {t}& {name}");
    }

    let t = ty
        .map(|t| prog.map_type_use(t, mi, ns))
        .unwrap_or_else(|| "int".to_string());
    if p.optional {
        format!("{t} {name} = {}", param_default(prog, mi, ns, p))
    } else {
        format!("{t} {name}")
    }
}

fn param_default(prog: &Program, mi: usize, ns: &[String], p: &Param) -> String {
    if let Some(expr) = &p.default {
        if let Some(lit) = render_scalar_literal(expr) {
            return lit;
        }
    }
    match p.ty.as_ref().and_then(|t| t.base_name()) {
        Some("Float") => "0.0".to_string(),
        Some("Bool") => "false".to_string(),
        Some("String") => "\"\"".to_string(),
        _ => {
            p.ty.as_ref()
                .and_then(|t| enum_default(prog, mi, ns, t))
                .unwrap_or_else(|| "0".to_string())
        }
    }
}

pub(crate) fn is_value_struct(prog: &Program, mi: usize, ty: &Type) -> bool {
    if let Type::Named { path, params, .. } = ty {
        if !params.is_empty() {
            return false;
        }
        let name = path.last().map(|s| s.as_str()).unwrap_or("");
        if crate::sema::types::map_primitive(name).is_some()
            || crate::sema::types::is_uint_shim(name)
        {
            return false;
        }
        return matches!(
            prog.kind_of(path, mi),
            Some(TypeKind::StructTypedef) | Some(TypeKind::AliasTypedef)
        );
    }
    false
}

/// A literal default value for a struct/enum field in a generated ctor.
fn default_value(prog: &Program, mi: usize, ns: &[String], ty: &Type) -> Option<String> {
    match ty.base_name() {
        Some("Int") | Some("UInt") | Some("UInt8") | Some("UInt16") | Some("UInt32") => {
            Some("0".to_string())
        }
        Some("Float") => Some("0.0".to_string()),
        Some("Bool") => Some("false".to_string()),
        _ => enum_default(prog, mi, ns, ty),
    }
}

/// Render an `enum abstract` member's value expression as a C++ integral constant
/// expression for use inside an `enum { … }` body. Handles the forms a typed
/// constant uses — integer/char literals, a sibling member by name (`AB = A | B`),
/// and unary/binary/parenthesised combinations of those. Returns `None` for an
/// expression that is not a compile-time integral constant.
fn enum_member_value(e: &Expr) -> Option<String> {
    Some(match e {
        Expr::Int(s) => s.clone(),
        Expr::Bool(b) => (if *b { "1" } else { "0" }).to_string(),
        // A bare identifier is a sibling enumerator, valid inside the same `enum`.
        Expr::Ident(n) => n.clone(),
        Expr::Paren(inner) => format!("({})", enum_member_value(inner)?),
        Expr::Unary {
            op,
            expr,
            prefix: true,
        } => {
            let o = match op {
                UnOp::Neg => "-",
                UnOp::BitNot => "~",
                _ => return None,
            };
            format!("{o}{}", enum_member_value(expr)?)
        }
        Expr::Binary { op, lhs, rhs } => {
            let o = match op {
                BinOp::Add => "+",
                BinOp::Sub => "-",
                BinOp::Mul => "*",
                BinOp::Div => "/",
                BinOp::Mod => "%",
                BinOp::BitAnd => "&",
                BinOp::BitOr => "|",
                BinOp::BitXor => "^",
                BinOp::Shl => "<<",
                BinOp::Shr => ">>",
                _ => return None,
            };
            format!(
                "{} {o} {}",
                enum_member_value(lhs)?,
                enum_member_value(rhs)?
            )
        }
        _ => return None,
    })
}

/// Render a `String`/`Float`-backed `enum abstract` member's value as a C++
/// constant: a string literal (`"H"`), a float/int literal, a bool, a sibling
/// member by name, or a unary negation of those. Returns `None` for anything else.
fn enum_abstract_value(e: &Expr) -> Option<String> {
    use crate::codegen::source::{escape_str, float_lit};
    Some(match e {
        Expr::Str { raw, .. } => format!("\"{}\"", escape_str(raw)),
        Expr::Float(s) => float_lit(s),
        Expr::Int(s) => s.clone(),
        Expr::Bool(b) => (if *b { "true" } else { "false" }).to_string(),
        // A sibling member, valid as `X_::Other` from inside the same namespace.
        Expr::Ident(n) => n.clone(),
        Expr::Unary {
            op: UnOp::Neg,
            expr,
            prefix: true,
        } => format!("-{}", enum_abstract_value(expr)?),
        _ => return None,
    })
}

/// For an enum-typed value, `Name_::FirstVariant` (namespaced if needed).
fn enum_default(prog: &Program, mi: usize, ns: &[String], ty: &Type) -> Option<String> {
    let Type::Named { path, .. } = ty else {
        return None;
    };
    let ti = prog.resolve_type(path, mi)?;
    if ti.kind != TypeKind::Enum {
        return None;
    }
    let Decl::Enum(e) = prog.type_decl(ti)? else {
        return None;
    };
    let first = e.variants.first()?;
    let tns = ti.cpp_namespace();
    let prefix = if tns == ns || tns.is_empty() {
        String::new()
    } else {
        format!("{}::", tns.join("::"))
    };
    // An ADT value defaults to its first variant via the factory — only
    // meaningful when that variant carries no payload.
    if e.is_adt() {
        if !first.params.is_empty() {
            return None;
        }
        return Some(format!("{prefix}{}::{}()", ti.cpp_name(), first.name));
    }
    Some(format!("{prefix}{}_::{}", ti.cpp_name(), first.name))
}

// ---- free helpers ------------------------------------------------------

fn has_accessor(f: &Field) -> bool {
    f.get != PropAccess::Default || f.set != PropAccess::Default
}

/// The named types a class/interface/ADT-enum mentions in the parts that appear
/// in its **header** definition — base list, field types, and method signatures
/// (params + returns), plus enum-variant payloads. Bodies are excluded (they
/// live in the `.cpp`, which sees every full definition). Used to decide which
/// sibling types must be forward-declared. Returns the final path segment of
/// each named type (e.g. `Proxy` for `pack.Proxy`).
/// The final path segment of every named type mentioned by `t` (recursing into
/// type parameters, anonymous-struct fields, and function types). E.g. for
/// `Array<pack.Proxy>` → `["Array", "Proxy"]`.
fn type_names_in(t: &Type, out: &mut Vec<String>) {
    match t {
        Type::Named { path, params, .. } => {
            if let Some(n) = path.last() {
                out.push(n.clone());
            }
            for p in params {
                type_names_in(p, out);
            }
        }
        Type::Anon(fields) => {
            for f in fields {
                type_names_in(&f.ty, out);
            }
        }
        Type::Func { params, ret } => {
            for p in params {
                type_names_in(p, out);
            }
            type_names_in(ret, out);
        }
    }
}

fn header_type_refs(d: &Decl) -> Vec<String> {
    let mut out = Vec::new();
    fn ty(t: &Type, out: &mut Vec<String>) {
        match t {
            Type::Named { path, params, .. } => {
                if let Some(n) = path.last() {
                    out.push(n.clone());
                }
                for p in params {
                    ty(p, out);
                }
            }
            Type::Anon(fields) => {
                for f in fields {
                    ty(&f.ty, out);
                }
            }
            Type::Func { params, ret } => {
                for p in params {
                    ty(p, out);
                }
                ty(ret, out);
            }
        }
    }
    fn sig(f: &Function, out: &mut Vec<String>) {
        for p in &f.params {
            if let Some(t) = &p.ty {
                ty(t, out);
            }
        }
        if let Some(t) = &f.ret {
            ty(t, out);
        }
    }
    match d {
        Decl::Class(c) => {
            if let Some(b) = &c.extends {
                ty(b, &mut out);
            }
            for i in &c.implements {
                ty(i, &mut out);
            }
            for f in &c.fields {
                if let Some(t) = &f.ty {
                    ty(t, &mut out);
                }
            }
            for m in c.methods.iter().chain(c.ctor.iter()) {
                sig(m, &mut out);
            }
        }
        Decl::Interface(i) => {
            for b in &i.extends {
                ty(b, &mut out);
            }
            for f in &i.fields {
                if let Some(t) = &f.ty {
                    ty(t, &mut out);
                }
            }
            for m in &i.methods {
                sig(m, &mut out);
            }
        }
        Decl::Enum(e) => {
            for v in &e.variants {
                for p in &v.params {
                    if let Some(t) = &p.ty {
                        ty(t, &mut out);
                    }
                }
            }
        }
        // A struct/alias typedef references types too — e.g. an `abstract`'s
        // underlying record (`typedef FooData = { … Array<Foo> … }`) names the
        // very class it backs, which is emitted *after* it.
        Decl::Typedef(td) => match &td.target {
            TypedefTarget::Alias(t) => ty(t, &mut out),
            TypedefTarget::Struct(fields) => {
                for f in fields {
                    ty(&f.ty, &mut out);
                }
            }
        },
        _ => {}
    }
    out
}

/// Whether codegen synthesizes a trivial `GetX` for this property: reads are
/// open (`default`) but the backing field is private because writes are
/// restricted (`null`/`never`) or routed (`set`). A custom `(get, …)` accessor
/// uses the user's `get_x` method instead; a read-restricted property
/// (`(null, …)`/`(never, …)`) has no external reads to serve.
fn generated_getter(f: &Field) -> bool {
    f.get == PropAccess::Default && f.set != PropAccess::Default
}

/// Map a `@:op(...)` argument to the C++ operator token to emit (so the caller
/// writes `operator<token>`), given the method's parameter count. Supports:
/// `[]` read (1 param) → subscript; binary `A op B` (1 param); prefix unary
/// `op A` (0 params). Returns `None` for forms with no C++98 operator mapping —
/// the 2-arg `[]` write, postfix, `a.b` field access, `a()` call — which the
/// validation pass flags instead.
pub(crate) fn cpp_operator(arg: &str, n_params: usize) -> Option<String> {
    let a = arg.trim();
    // Array read: `@:op([])` with one parameter (the index). The two-parameter
    // write form has no C++ `operator[]` equivalent (it must return a reference).
    if a == "[]" {
        return (n_params == 1).then(|| "[]".to_string());
    }
    // The operator symbol is whatever remains after dropping the `A`/`B` operand
    // placeholders and whitespace (`A << B` → `<<`, `-A` → `-`).
    let sym: String = a
        .chars()
        .filter(|c| !c.is_alphanumeric() && *c != '_' && !c.is_whitespace())
        .collect();
    const BINARY: &[&str] = &[
        "+", "-", "*", "/", "%", "==", "!=", "<", ">", "<=", ">=", "&", "|", "^", "<<", ">>", "&&",
        "||",
    ];
    const UNARY: &[&str] = &["-", "!", "~"];
    let ok = (n_params == 1 && BINARY.contains(&sym.as_str()))
        || (n_params == 0 && UNARY.contains(&sym.as_str()));
    ok.then_some(sym)
}

/// Whether a `set`-access property has a user-written `set_x` (real Haxe
/// semantics: every write routes through it). Without one, Hatchet's dialect
/// generates the trivial `SetX` instead.
fn custom_setter(c: &Class, f: &Field) -> bool {
    f.set == PropAccess::Set
        && c.methods
            .iter()
            .any(|m| m.name.as_deref() == Some(&format!("set_{}", f.name)))
}

/// The property field a `get_x`/`set_x` method is the custom accessor for, if
/// `method` matches a declared `(get, …)`/`(…, set)` property. `None` when
/// `method` is an ordinary method (no matching property of the right kind).
fn accessor_field<'a>(c: &'a Class, method: &str) -> Option<&'a Field> {
    let field = method
        .strip_prefix("get_")
        .or_else(|| method.strip_prefix("set_"))?;
    let f = c.fields.iter().find(|f| f.name == field)?;
    let matches_kind = (method.starts_with("get_") && f.get == PropAccess::Get)
        || (method.starts_with("set_") && f.set == PropAccess::Set);
    matches_kind.then_some(f)
}

/// The return type Haxe infers for a property accessor whose signature omits
/// it: `get_x` and `set_x` both return the property's type (the common shape
/// `function set_x(v:T) { return this.x = v; }` relies on this — defaulting to
/// `void` would emit a value `return` from a void C++ function). `None` when
/// `method` is not an accessor of a matching declared property.
pub(crate) fn accessor_ret_type(c: &Class, method: &str) -> Option<Type> {
    accessor_field(c, method).and_then(|f| f.ty.clone())
}

/// The C++ visibility group a method belongs in. Haxe's default (no modifier)
/// access is private, which maps to C++ `protected` (see the field grouping and
/// the note where `protected` is declared). A custom property accessor is the
/// exception: Haxe governs property access by the *property's* visibility, not
/// the accessor's, and Hatchet lowers external reads/writes to direct
/// `get_x()`/`set_x()` calls — so the accessor must be at least as visible as
/// the property it backs.
fn method_access(c: &Class, m: &Function) -> Access {
    let own = m.access;
    match m.name.as_deref().and_then(|n| accessor_field(c, n)) {
        Some(f) if f.access == Access::Public || own == Access::Public => Access::Public,
        Some(_) => own,
        None => own,
    }
}

fn tabs(n: usize) -> String {
    "\t".repeat(n)
}

/// The key and value AST types of an `@orderedMap` field — `Map<K,V>` → `(K, V)` —
/// or `None` when the field is not tagged `@orderedMap` or is not a `Map<K,V>`.
/// `@orderedMap` stores a `Map` as two insertion-ordered parallel vectors.
pub(crate) fn ordered_map_kv(f: &Field) -> Option<(&Type, &Type)> {
    if !has_meta(&f.meta, "orderedMap") {
        return None;
    }
    if let Some(Type::Named { path, params, .. }) = &f.ty {
        if path.last().map(|s| s.as_str()) == Some("Map") && params.len() == 2 {
            return Some((&params[0], &params[1]));
        }
    }
    None
}

fn cap(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Render a scalar literal expression to C++ (`null`→`NULL`, floats get an `f`
/// suffix). Returns `None` for non-scalar expressions (objects, arrays, lambdas).
pub(crate) fn render_scalar_literal(e: &Expr) -> Option<String> {
    Some(match e {
        Expr::Int(s) => s.clone(),
        Expr::Float(s) => float_lit(s),
        Expr::Bool(b) => b.to_string(),
        Expr::Null => "NULL".to_string(),
        Expr::Str { raw, .. } => format!("\"{raw}\""),
        Expr::Ident(name) => name.clone(),
        Expr::Unary {
            op: UnOp::Neg,
            expr,
            prefix: true,
        } => {
            format!("-{}", render_scalar_literal(expr)?)
        }
        Expr::Paren(inner) => render_scalar_literal(inner)?,
        _ => return None,
    })
}

/// A Haxe `Float` literal as C++. `Float` lowers to `double`, and an unsuffixed
/// C++ floating literal *is* a `double` — emit it unchanged (an `f` suffix would
/// truncate it to single precision).
fn float_lit(s: &str) -> String {
    s.to_string()
}

// Re-export for the driver: produce StdAfx output too.
pub use crate::stdafx::generate as generate_stdafx;
