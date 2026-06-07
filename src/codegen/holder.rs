//! Base-from-member ("Holder") idiom analysis.
//!
//! When a constructor's `super(...)` call is **not** the first statement — because
//! locals must be computed first, or because the super arguments themselves are
//! non-trivial creation expressions — C++ cannot express it as a plain base
//! initialiser list. The fix (per `SKILL.md`) is an intermediate base `XHolder`:
//! the class derives `class X : private XHolder, public Base`, the `XHolder`
//! constructor runs the pre-`super` logic and stores whatever the `super` call
//! needs, and `Base(...)` then reads those members.
//!
//! This module performs the shared analysis so the header generator can emit the
//! `XHolder` struct + adjusted base list, and the source generator can emit the
//! `XHolder` constructor body + the class initialiser list.

use crate::ast::*;
use crate::sema::Program;

/// How a single `super(...)` argument is supplied to the base initialiser list.
pub(crate) enum SuperArg<'a> {
    /// Reference an `XHolder` member by name (a lifted local or a hoisted arg).
    Member(String),
    /// Emit the expression directly (a constructor parameter or a literal).
    PassThrough(&'a Expr),
}

/// The plan for emitting an `XHolder` for a class constructor.
pub(crate) struct Holder<'a> {
    pub name: String,
    /// Adjusted class base list, e.g. ` : private ActorHolder, public Sprite`.
    pub base_list: String,
    /// Index of the `super(...)` statement within the constructor body.
    pub super_idx: usize,
    /// Member declarations for the struct, e.g. `["float scaledX;", ...]`.
    pub member_decls: Vec<String>,
    /// Names of pre-`super` locals that are stored as members (assigned via
    /// `this->name = ...` instead of being declared as locals).
    pub lifted: Vec<String>,
    /// Non-trivial `super` arguments hoisted into members, assigned in the
    /// `XHolder` constructor body after the pre-`super` statements.
    pub hoisted: Vec<(String, &'a Expr)>,
    /// Each `super(...)` argument mapped for the `Base(...)` initialiser.
    pub super_args: Vec<SuperArg<'a>>,
}

/// Analyse a class constructor; returns a [`Holder`] plan when the base-from-member
/// idiom is required, or `None` when a normal initialiser list suffices.
pub(crate) fn analyze<'a>(
    prog: &Program,
    mi: usize,
    ns: &[String],
    c: &'a Class,
) -> Option<Holder<'a>> {
    let ctor = c.ctor.as_ref()?;
    let body = ctor.body.as_ref()?;
    let super_idx = body.iter().position(is_super_call)?;
    if super_idx == 0 {
        return None; // super is first → plain initialiser list handles it
    }
    let super_args = match &body[super_idx] {
        Stmt::Expr(Expr::Call(_, args), _) => args,
        _ => return None,
    };

    // Pre-super local declarations (name → declared type) and constructor params.
    let mut local_ty: Vec<(&str, &Option<Type>)> = Vec::new();
    for st in &body[..super_idx] {
        if let Stmt::Var { name, ty, .. } = st {
            local_ty.push((name.as_str(), ty));
        }
    }
    let is_local = |n: &str| local_ty.iter().any(|(ln, _)| *ln == n);
    let is_param = |n: &str| ctor.params.iter().any(|p| p.name == n);

    let mut member_decls = Vec::new();
    let mut lifted = Vec::new();
    let mut hoisted = Vec::new();
    let mut super_plan = Vec::new();
    let mut k = 0usize;

    for arg in super_args {
        match arg {
            // A pre-super local passed straight to super → lift it to a member.
            Expr::Ident(n) if is_local(n) => {
                let ty = local_ty.iter().find(|(ln, _)| ln == n).and_then(|(_, t)| t.as_ref());
                let cpp = match ty {
                    Some(t) => prog.map_type_use(t, mi, ns),
                    None => return None, // can't type the member → bail to default path
                };
                if !lifted.iter().any(|x| x == n) {
                    member_decls.push(format!("{cpp} {n};"));
                    lifted.push(n.clone());
                }
                super_plan.push(SuperArg::Member(n.clone()));
            }
            // A constructor parameter or literal → pass straight through.
            Expr::Ident(n) if is_param(n) => super_plan.push(SuperArg::PassThrough(arg)),
            Expr::Int(_) | Expr::Float(_) | Expr::Bool(_) | Expr::Str { .. } | Expr::Null => {
                super_plan.push(SuperArg::PassThrough(arg))
            }
            // A non-trivial creation expression → hoist into a member.
            Expr::New(ty, _) => {
                k += 1;
                let name = format!("_super{k}");
                let cpp = format!("{}*", prog.map_type_base(ty, mi, ns));
                member_decls.push(format!("{cpp} {name};"));
                hoisted.push((name.clone(), arg));
                super_plan.push(SuperArg::Member(name));
            }
            // Anything else is outside the shapes we can lower safely.
            _ => return None,
        }
    }

    // No members means a normal initialiser list would have worked.
    if member_decls.is_empty() {
        return None;
    }

    let name = format!("{}Holder", c.name);
    let mut bases = vec![format!("private {name}")];
    if let Some(sup) = &c.extends {
        bases.push(format!("public {}", prog.map_type_base(sup, mi, ns)));
    }
    for i in &c.implements {
        bases.push(format!("public {}", prog.map_type_base(i, mi, ns)));
    }
    let base_list = format!(" : {}", bases.join(", "));

    Some(Holder {
        name,
        base_list,
        super_idx,
        member_decls,
        lifted,
        hoisted,
        super_args: super_plan,
    })
}

fn is_super_call(st: &Stmt) -> bool {
    matches!(st, Stmt::Expr(Expr::Call(target, _), _) if matches!(**target, Expr::Super))
}
