//! `--header-only` amalgamation: order the `--src` modules so any type needed
//! *complete* (a base class, a by-value field) has its defining module emitted
//! first, then emit one self-contained header. Split out of `mod.rs`.

use std::fmt::Write;

use crate::ast::*;
use crate::sema::Program;

use super::header::HeaderGen;
use super::*;

/// Amalgamate every module in `indices` (the `--src` set, in order) into one
/// self-contained header (`--header-only <stem>`): a single include guard, the
/// prelude inlined, native `@:include`s hoisted + de-duplicated, a global forward-
/// declaration block (so reference-type cross-references between modules resolve
/// regardless of section order), then each module's section — its `@:headerCode`
/// followed by its declarations with inline constructor/method bodies. No `.cpp`
/// and no separate prelude header are produced. `header_codes` maps a module index
/// to its verbatim `@:headerCode`.
pub fn generate_amalgamation(
    prog: &Program,
    stem: &str,
    indices: &[usize],
    prelude_body: &str,
    header_codes: &std::collections::BTreeMap<usize, String>,
) -> HeaderOutput {
    // Declarations only here; the inline bodies are emitted in a separate second
    // pass (see below), so `inline_bodies` stays off for the per-module sections.
    let opts = HeaderOpts::default();
    let guard = format!("{}_H", sanitize(&stem.to_uppercase()));
    let mut out = String::new();
    let mut warnings: Vec<(usize, String)> = Vec::new();
    let mut errors: Vec<(usize, String)> = Vec::new();

    // Order the modules so any type a module needs *complete* — a base class it
    // extends/implements, or a value (non-pointer) field/underlying — has its
    // defining module emitted first. Reference-type (pointer) cross-references do not
    // constrain the order (the forward-declaration block satisfies them).
    let (order, cycle) = amalgam_order(prog, indices);
    if let Some(msg) = cycle {
        errors.push((0, msg));
    }

    let _ = writeln!(out, "#ifndef {guard}");
    let _ = writeln!(out, "#define {guard}");
    out.push('\n');

    // The prelude (shim, standard includes, export macros) inlined once at the top.
    out.push_str(prelude_body.trim_end());
    out.push_str("\n\n");

    // Native `@:include`s from every module, de-duplicated, hoisted to the top.
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut inc_block = String::new();
    for &mi in &order {
        for inc in prog.native_includes(mi) {
            if !seen.insert(inc.clone()) {
                continue;
            }
            if inc.starts_with('<') {
                let _ = writeln!(inc_block, "#include {inc}");
            } else {
                let _ = writeln!(inc_block, "#include \"{inc}\"");
            }
        }
    }
    if !inc_block.is_empty() {
        out.push_str(&inc_block);
        out.push('\n');
    }

    out.push_str(&amalgam_forward_decls(prog, &order));

    // Pass 1 — every module's declarations (its `@:headerCode`, then enums /
    // typedefs / interfaces / class declarations), with no method bodies yet.
    for &mi in &order {
        let m = &prog.modules[mi];
        if let Some(hc) = header_codes.get(&mi) {
            out.push_str(hc.trim_matches('\n'));
            out.push_str("\n\n");
        }
        let gen = HeaderGen {
            prog,
            mi,
            ns: m.package.clone(),
            opts: &opts,
        };
        let (sect, _, _) = gen.section();
        out.push_str(&sect);
    }

    // Pass 2 — every module's inline constructor/method definitions, emitted after
    // *all* declarations so a body that uses a value or member of a class from
    // another module sees that class's complete definition, not just a forward
    // declaration. (Cross-module reference-type pointers were already satisfied by
    // the forward-declaration block; this covers by-value use and member access.)
    for &mi in &order {
        let m = &prog.modules[mi];
        let gen = HeaderGen {
            prog,
            mi,
            ns: m.package.clone(),
            opts: &opts,
        };
        let (defs, mut w, mut e) = gen.inline_defs_section();
        out.push_str(&defs);
        warnings.append(&mut w);
        errors.append(&mut e);
    }

    let _ = writeln!(out, "#endif");
    (out, warnings, errors)
}

/// The global forward-declaration block for an amalgamation: every emittable class /
/// interface / ADT-enum name, grouped under its package namespace. `@:native`-renamed
/// types are skipped — their real definition lives in a hoisted engine header.
fn amalgam_forward_decls(prog: &Program, indices: &[usize]) -> String {
    let mut by_ns: std::collections::BTreeMap<Vec<String>, Vec<String>> =
        std::collections::BTreeMap::new();
    for &mi in indices {
        let m = &prog.modules[mi];
        let names = by_ns.entry(m.package.clone()).or_default();
        for d in &m.file.decls {
            let name = match d {
                Decl::Class(c)
                    if !c.is_extern
                        && !has_meta(&c.meta, "proxy")
                        && !has_meta(&c.meta, "native") =>
                {
                    Some(&c.name)
                }
                Decl::Interface(i) if !i.is_extern && !has_meta(&i.meta, "native") => Some(&i.name),
                Decl::Enum(e) if e.is_adt() && !has_meta(&e.meta, "native") => Some(&e.name),
                _ => None,
            };
            if let Some(n) = name {
                if !names.contains(n) {
                    names.push(n.clone());
                }
            }
        }
    }
    let mut out = String::new();
    let mut any = false;
    for (ns, names) in &by_ns {
        if names.is_empty() {
            continue;
        }
        any = true;
        let base = ns.len();
        for part in ns {
            let _ = writeln!(out, "namespace {part} {{");
        }
        for n in names {
            let _ = writeln!(out, "{}class {n};", tabs(base));
        }
        for _ in ns.iter().rev() {
            let _ = writeln!(out, "}}");
        }
    }
    if any {
        out.push('\n');
    }
    out
}

/// Order the amalgamated modules so that whenever one module needs a type from
/// another **complete** — a base class (`extends`/`implements`) or a value
/// (non-pointer) field / abstract underlying — the defining module comes first.
/// Reference-type (pointer) cross-references impose no constraint (the global
/// forward-declaration block covers them). Returns the ordered module indices and,
/// when a genuine cross-module cycle of complete-type dependencies remains, a
/// diagnostic message (the leftover modules are appended in source order so the
/// caller still produces output, but the run fails).
fn amalgam_order(prog: &Program, indices: &[usize]) -> (Vec<usize>, Option<String>) {
    use std::collections::{BTreeMap, BTreeSet};

    // Type name → the module that declares it (only types defined in the amalgamation).
    let mut defining: BTreeMap<String, usize> = BTreeMap::new();
    for &mi in indices {
        for d in &prog.modules[mi].file.decls {
            if let Some(name) = amalgam_type_name(d) {
                defining.entry(name).or_insert(mi);
            }
        }
    }

    // mi → the modules it must follow (its complete-type dependencies).
    let mut deps: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
    for &mi in indices {
        let m = &prog.modules[mi];
        for d in &m.file.decls {
            for name in decl_complete_deps(prog, mi, &m.package, d) {
                if let Some(&dm) = defining.get(&name) {
                    if dm != mi {
                        deps.entry(mi).or_default().insert(dm);
                    }
                }
            }
        }
    }

    // Stable topological order: repeatedly take the earliest still-unplaced module
    // whose dependencies are all placed. No such module (with some left) ⇒ a cycle.
    let mut remaining: Vec<usize> = indices.to_vec();
    let mut placed: BTreeSet<usize> = BTreeSet::new();
    let mut ordered: Vec<usize> = Vec::with_capacity(remaining.len());
    while !remaining.is_empty() {
        let pos = remaining.iter().position(|mi| {
            deps.get(mi)
                .is_none_or(|ds| ds.iter().all(|d| placed.contains(d)))
        });
        match pos {
            Some(p) => {
                let mi = remaining.remove(p);
                placed.insert(mi);
                ordered.push(mi);
            }
            None => {
                let names: Vec<String> = remaining
                    .iter()
                    .map(|&mi| {
                        prog.modules[mi]
                            .path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("?")
                            .to_string()
                    })
                    .collect();
                let msg = format!(
                    "circular cross-module dependency in --header-only between: {} — a base \
                     class or by-value field forms a cycle across modules; reorder so base / \
                     value types are defined first, or merge the modules",
                    names.join(", ")
                );
                ordered.append(&mut remaining);
                return (ordered, Some(msg));
            }
        }
    }
    (ordered, None)
}

/// The name a type declaration defines, if it is emitted into the amalgamation
/// (so it can be the *target* of a complete-type dependency). `extern` / `@proxy`
/// types live in hand-written C++ and are excluded.
fn amalgam_type_name(d: &Decl) -> Option<String> {
    match d {
        Decl::Class(c) if !c.is_extern && !has_meta(&c.meta, "proxy") => Some(c.name.clone()),
        Decl::Interface(i) if !i.is_extern => Some(i.name.clone()),
        Decl::Enum(e) if !e.is_extern => Some(e.name.clone()),
        Decl::Typedef(t) if !has_meta(&t.meta, "native") => Some(t.name.clone()),
        _ => None,
    }
}

/// The user-type names a declaration needs **complete** at its point of declaration:
/// base classes (always), and by-value fields / abstract underlying (only when stored
/// by value — a pointer or container needs just a forward declaration).
fn decl_complete_deps(prog: &Program, mi: usize, ns: &[String], d: &Decl) -> Vec<String> {
    let mut out = Vec::new();
    match d {
        Decl::Class(c) => {
            for b in c.extends.iter().chain(c.implements.iter()) {
                if let Some(n) = b.base_name() {
                    out.push(n.to_string());
                }
            }
            if let Some(u) = &c.abstract_underlying {
                if let Some(n) = value_dep_name(prog, mi, ns, u) {
                    out.push(n);
                }
            }
            for f in &c.fields {
                if let Some(ty) = &f.ty {
                    if let Some(n) = value_dep_name(prog, mi, ns, ty) {
                        out.push(n);
                    }
                }
            }
        }
        Decl::Interface(i) => {
            for b in &i.extends {
                if let Some(n) = b.base_name() {
                    out.push(n.to_string());
                }
            }
        }
        Decl::Typedef(t) => {
            if let TypedefTarget::Struct(fields) = &t.target {
                for sf in fields {
                    if let Some(n) = value_dep_name(prog, mi, ns, &sf.ty) {
                        out.push(n);
                    }
                }
            }
        }
        _ => {}
    }
    out
}

/// The user-type name `ty` needs complete when it is used **by value** — i.e. it
/// lowers to a non-pointer, non-container C++ type (a value class, `abstract`,
/// struct typedef, or enum). A reference type (pointer) or an `Array`/`Map` only
/// needs a forward declaration of its element, so it returns `None`.
fn value_dep_name(prog: &Program, mi: usize, ns: &[String], ty: &Type) -> Option<String> {
    let base = ty.base_name()?;
    let cpp = prog.map_type_use(ty, mi, ns);
    if cpp.ends_with('*') || cpp.starts_with("std::") {
        return None;
    }
    Some(base.to_string())
}
