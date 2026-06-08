//! Semantic model: the cross-file symbol table, type mapping, and include sets.
//!
//! After all files are parsed, [`Program::build`] indexes every declared type so
//! that references can be resolved (respecting Haxe scoping: local declarations,
//! then imported modules) and mapped to their C++ spelling with the correct
//! namespace. It also computes the set of `#include`s each generated header needs.

pub mod includes;
pub mod types;
pub mod validate;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::ast::*;
use crate::discover;
use crate::parser;

use types::{container_template, is_uint_shim, map_primitive};

/// What a declared name is, which determines value-vs-reference semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeKind {
    Class,
    Interface,
    Enum,
    /// `typedef X = { ... }` — a C++ value struct.
    StructTypedef,
    /// `typedef X = Y` — an alias.
    AliasTypedef,
}

impl TypeKind {
    /// Reference types are represented by pointers when stored/passed by value
    /// position (classes and interfaces). Enums and structs are value types.
    pub fn is_reference(self) -> bool {
        matches!(self, TypeKind::Class | TypeKind::Interface)
    }
}

#[derive(Debug, Clone)]
pub struct TypeInfo {
    pub name: String,
    pub package: Vec<String>,
    pub kind: TypeKind,
    pub is_native: bool,
    /// Explicit C++ namespace parts from `@:native("a::b::Name")`, if any.
    pub native_ns: Option<Vec<String>>,
    /// Explicit C++ name from `@:native("...::Name")`, else the Haxe name.
    pub native_name: Option<String>,
    pub module_index: usize,
}

impl TypeInfo {
    /// The C++ namespace this type lives in (component list).
    ///
    /// * regular types → their Haxe package (the rule "namespaces match packages");
    /// * `@:native("a::b::N")` → the explicit namespace `a::b`;
    /// * bare `@:native` → the first package component (the engine's root
    ///   namespace, e.g. package `mucus.api` → namespace `mucus`).
    pub fn cpp_namespace(&self) -> Vec<String> {
        if self.is_native {
            if let Some(ns) = &self.native_ns {
                return ns.clone();
            }
            return self.package.iter().take(1).cloned().collect();
        }
        self.package.clone()
    }

    pub fn cpp_name(&self) -> &str {
        self.native_name.as_deref().unwrap_or(&self.name)
    }
}

#[derive(Debug)]
pub struct Module {
    pub path: PathBuf,
    /// Source-root-relative directory components (mirrors output layout).
    pub dir: Vec<String>,
    pub package: Vec<String>,
    pub file: File,
    pub is_stdafx: bool,
    /// Module indices brought into scope by this module's `import`s.
    pub imports_resolved: Vec<usize>,
}

pub struct Program {
    pub src_root: PathBuf,
    pub modules: Vec<Module>,
    pub types: Vec<TypeInfo>,
    /// The stem of the prelude source/header (default `StdAfx`; configurable so a
    /// project can name it e.g. `MyGame`, producing `MyGame.h`).
    pub stdafx_stem: String,
    /// Prefix for the platform export/calling-convention macros emitted around
    /// `extern inline` functions (default `HATCHET` → `HATCHET_EXPORT`/`HATCHET_CALL`;
    /// configurable via `--export-macro`).
    pub export_macro: String,
}

impl Program {
    /// Discover, parse, and analyse every `.hx` file under `src_root`.
    pub fn from_src_dir(src_root: &Path) -> Result<Program, String> {
        let files = discover::find_haxe_files(src_root)
            .map_err(|e| format!("scanning {}: {e}", src_root.display()))?;
        let mut units = Vec::new();
        for f in files {
            let src = std::fs::read_to_string(&f).map_err(|e| format!("reading {}: {e}", f.display()))?;
            let parsed = parser::parse(&src).map_err(|e| format!("{}: {e}", f.display()))?;
            units.push((f, parsed));
        }
        Ok(Program::build(src_root, units))
    }

    /// Build the program model from already-parsed files, using the default
    /// prelude source name (`StdAfx`).
    pub fn build(src_root: &Path, units: Vec<(PathBuf, File)>) -> Program {
        Self::build_with(src_root, units, "StdAfx")
    }

    /// Build the program model, treating files whose stem is `stdafx_stem` as the
    /// prelude source (default `StdAfx`).
    pub fn build_with(src_root: &Path, units: Vec<(PathBuf, File)>, stdafx_stem: &str) -> Program {
        let mut modules = Vec::new();
        for (path, file) in units {
            let dir = dir_components(src_root, &path);
            let package = if file.package.is_empty() {
                dir.clone()
            } else {
                file.package.clone()
            };
            modules.push(Module {
                is_stdafx: discover::is_stdafx_named(&path, stdafx_stem),
                path,
                dir,
                package,
                file,
                imports_resolved: Vec::new(),
            });
        }

        let mut prog = Program {
            src_root: src_root.to_path_buf(),
            modules,
            types: Vec::new(),
            stdafx_stem: stdafx_stem.to_string(),
            export_macro: "HATCHET".to_string(),
        };
        prog.index_types();
        prog.resolve_imports();
        prog
    }

    fn index_types(&mut self) {
        for (mi, m) in self.modules.iter().enumerate() {
            for decl in &m.file.decls {
                let (name, kind, meta) = match decl {
                    Decl::Class(c) => (&c.name, TypeKind::Class, &c.meta),
                    Decl::Interface(i) => (&i.name, TypeKind::Interface, &i.meta),
                    Decl::Enum(e) => (&e.name, TypeKind::Enum, &e.meta),
                    Decl::Typedef(t) => {
                        let kind = match t.target {
                            TypedefTarget::Struct(_) => TypeKind::StructTypedef,
                            TypedefTarget::Alias(_) => TypeKind::AliasTypedef,
                        };
                        (&t.name, kind, &t.meta)
                    }
                    Decl::Global(_) | Decl::Function(_) => continue,
                };
                let is_native = has_meta(meta, "native");
                let (native_ns, native_name) = native_target(meta);
                self.types.push(TypeInfo {
                    name: name.clone(),
                    package: m.package.clone(),
                    kind,
                    is_native,
                    native_ns,
                    native_name,
                    module_index: mi,
                });
            }
        }
    }

    fn resolve_imports(&mut self) {
        for mi in 0..self.modules.len() {
            let mut resolved = Vec::new();
            // import paths reference other source files (modules)
            let imports = self.modules[mi].file.imports.clone();
            for imp in &imports {
                if imp.wildcard {
                    // bring every module in the package into scope
                    for (j, m2) in self.modules.iter().enumerate() {
                        if m2.dir == imp.path {
                            resolved.push(j);
                        }
                    }
                } else if !imp.path.is_empty() {
                    // Find the longest prefix that names a module file. This also
                    // handles sub-type imports like `import pack.Module.Type;`,
                    // where the module is `pack.Module` and `Type` is a member.
                    'find: for module_idx in (0..imp.path.len()).rev() {
                        let pkg = &imp.path[..module_idx];
                        let module_name = &imp.path[module_idx];
                        for (j, m2) in self.modules.iter().enumerate() {
                            let stem = m2.path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                            if m2.dir == pkg && stem == module_name {
                                resolved.push(j);
                                break 'find;
                            }
                        }
                    }
                }
            }
            // De-duplicate while preserving import order (which feeds include order).
            let mut seen = std::collections::BTreeSet::new();
            resolved.retain(|&j| seen.insert(j));
            self.modules[mi].imports_resolved = resolved;
        }
    }

    // ---- type resolution & mapping -------------------------------------

    fn find_type(&self, package: &[String], name: &str) -> Option<&TypeInfo> {
        self.types
            .iter()
            .find(|t| t.name == name && t.package == package)
    }

    /// Resolve a (possibly dotted) type path as referenced from `ctx_module`,
    /// honouring local-then-imported scoping.
    pub fn resolve_type(&self, path: &[String], ctx_module: usize) -> Option<&TypeInfo> {
        let name = path.last()?;
        // Qualified reference (`pack.Module.Type`): use the leading lowercase
        // components as the package and match exactly.
        if path.len() > 1 {
            let pkg = leading_package(path);
            if let Some(t) = self.find_type(&pkg, name) {
                return Some(t);
            }
        }
        // Bare name: pick the best candidate. A local declaration always wins;
        // otherwise a generated (`@:expose`, non-native) type beats a native
        // shadow of the same name (the `mucus.modules.api` re-export declares
        // native aliases of the real `modules` classes, and references from
        // other packages should resolve to the real ones). Tie-break by scope:
        // local > imported > any.
        let m = &self.modules[ctx_module];
        let mut best: Option<(&TypeInfo, (u8, u8, u8))> = None;
        for t in &self.types {
            if &t.name != name {
                continue;
            }
            let scope = if t.package == m.package {
                0u8
            } else if m
                .imports_resolved
                .iter()
                .any(|&mi| self.modules[mi].package == t.package)
            {
                1
            } else {
                2
            };
            // local-ness dominates, then non-native, then nearest scope
            let key = ((scope != 0) as u8, t.is_native as u8, scope);
            if best.is_none_or(|(_, bk)| key < bk) {
                best = Some((t, key));
            }
        }
        best.map(|(t, _)| t)
    }

    /// If `name` refers to a global `final` **constant** in scope, return its
    /// namespace-qualified C++ reference. `final`s lower to `static const` inside
    /// the namespace (or, for `@:native`, come from the C++ engine in its native
    /// namespace), so a reference from a different namespace — notably a global-
    /// scope `extern "C"` export, e.g. `case game::ALIENBEACH_SCENE_ID:` — must be
    /// qualified, exactly like a type reference is. A `final` whose value is a
    /// function/lambda is a free function, not a constant, and is left alone.
    /// Searches the current module first, then its imports.
    pub fn global_final_ref(&self, name: &str, ctx_module: usize, current_ns: &[String]) -> Option<String> {
        let m = &self.modules[ctx_module];
        let scope = std::iter::once(ctx_module).chain(m.imports_resolved.iter().copied());
        for mi in scope {
            for decl in &self.modules[mi].file.decls {
                if let Decl::Global(g) = decl {
                    let is_lambda = matches!(g.init, Some(Expr::Lambda { .. }));
                    if g.name == name && g.is_final && !is_lambda {
                        let (ns, cpp_name) = if has_meta(&g.meta, "native") {
                            let (nns, nname) = native_target(&g.meta);
                            (
                                nns.unwrap_or_else(|| {
                                    self.modules[mi].package.iter().take(1).cloned().collect()
                                }),
                                nname.unwrap_or_else(|| g.name.clone()),
                            )
                        } else {
                            (self.modules[mi].package.clone(), g.name.clone())
                        };
                        return Some(if ns == current_ns || ns.is_empty() {
                            cpp_name
                        } else {
                            format!("{}::{}", ns.join("::"), cpp_name)
                        });
                    }
                }
            }
        }
        None
    }

    /// Is `path`, as seen from `ctx_module`, a reference (pointer) type?
    pub fn is_reference(&self, path: &[String], ctx_module: usize) -> bool {
        self.resolve_type(path, ctx_module)
            .map(|t| t.kind.is_reference())
            .unwrap_or(false)
    }

    /// The kind of a referenced type, if known.
    pub fn kind_of(&self, path: &[String], ctx_module: usize) -> Option<TypeKind> {
        self.resolve_type(path, ctx_module).map(|t| t.kind)
    }

    /// The declaration a `TypeInfo` was produced from.
    pub fn type_decl(&self, ti: &TypeInfo) -> Option<&Decl> {
        self.modules[ti.module_index]
            .file
            .decls
            .iter()
            .find(|d| decl_name(d) == Some(ti.name.as_str()))
    }

    /// Does `class` (or any transitive base / implemented interface, including
    /// native ones) declare a method named `name`? Used to decide `virtual`.
    pub fn method_overrides_base(&self, class: &Class, ctx_module: usize, name: &str) -> bool {
        let mut frontier: Vec<Type> = class
            .extends
            .iter()
            .cloned()
            .chain(class.implements.iter().cloned())
            .collect();
        let mut seen: BTreeSet<(Vec<String>, String)> = BTreeSet::new();
        while let Some(base) = frontier.pop() {
            let Type::Named { path, .. } = &base else { continue };
            let Some(ti) = self.resolve_type(path, ctx_module) else { continue };
            if !seen.insert((ti.package.clone(), ti.name.clone())) {
                continue;
            }
            let Some(decl) = self.type_decl(ti) else { continue };
            if decl_has_method(decl, name) {
                return true;
            }
            frontier.extend(decl_bases(decl));
        }
        false
    }

    /// Map a Haxe type to its C++ base spelling (no pointer), namespaced relative
    /// to `current_ns`. Used for base classes and as the leaf of richer mappings.
    pub fn map_type_base(&self, ty: &Type, ctx_module: usize, current_ns: &[String]) -> String {
        match ty {
            Type::Named { path, params, .. } => {
                let name = path.last().map(|s| s.as_str()).unwrap_or("Dynamic");
                // `Null<T>` is a nullability wrapper with no C++ representation of
                // its own — map straight through to `T`.
                if name == "Null" && params.len() == 1 {
                    return self.map_type_base(&params[0], ctx_module, current_ns);
                }
                if params.is_empty() {
                    if let Some(prim) = map_primitive(name) {
                        return prim.to_string();
                    }
                }
                if let Some(tmpl) = container_template(name) {
                    let inner = params
                        .iter()
                        .map(|p| self.map_type_use(p, ctx_module, current_ns))
                        .collect::<Vec<_>>()
                        .join(", ");
                    // C++98: a closing `>>` is the shift operator, so a nested
                    // template needs a space before the outer `>`.
                    let pad = if inner.ends_with('>') { " " } else { "" };
                    return format!("{tmpl}<{inner}{pad}>");
                }
                match self.resolve_type(path, ctx_module) {
                    Some(ti) => qualify(ti, current_ns),
                    // Unknown (e.g. a generic parameter) — emit the name verbatim.
                    None => name.to_string(),
                }
            }
            Type::Anon(fields) if fields.is_empty() => "void*".to_string(),
            // A non-empty anonymous structure used in type position is treated as
            // opaque (`void*`); named struct typedefs are the supported form.
            Type::Anon(_) => "void*".to_string(),
            Type::Func { .. } => "void*".to_string(),
        }
    }

    /// Map a Haxe type as used in a field/var/parameter/element position: the
    /// base spelling, plus a trailing `*` for reference (class/interface) types.
    /// Optionality-driven pointers (e.g. `?effects:Effects`) are applied by the
    /// code generator, not here.
    pub fn map_type_use(&self, ty: &Type, ctx_module: usize, current_ns: &[String]) -> String {
        // `Null<T>` makes a value type nullable; C++ value types can't be null, so
        // a nullable struct/container is represented as a pointer (a reference type
        // is already a pointer, so it is left unchanged — never `T**`).
        if let Type::Named { path, params, .. } = ty {
            if path.last().map(|s| s.as_str()) == Some("Null") && params.len() == 1 {
                let inner = self.map_type_use(&params[0], ctx_module, current_ns);
                return if inner.ends_with('*') { inner } else { format!("{inner}*") };
            }
        }
        let base = self.map_type_base(ty, ctx_module, current_ns);
        if let Type::Named { path, params, .. } = ty {
            if params.is_empty() && self.is_reference(path, ctx_module) {
                return format!("{base}*");
            }
        }
        base
    }

    // ---- includes ------------------------------------------------------

    /// Does this module emit a header of its own? Pure native-interop modules
    /// (only `@:native` decls, plus `UInt8/16/32` shims) do not — importers
    /// inherit their `@:include`s instead.
    pub fn generates_header(&self, m: &Module) -> bool {
        if m.is_stdafx {
            return false;
        }
        m.file.decls.iter().any(|d| self.is_emittable(d))
    }

    fn is_emittable(&self, decl: &Decl) -> bool {
        match decl {
            Decl::Class(c) => !has_meta(&c.meta, "native"),
            Decl::Interface(i) => !has_meta(&i.meta, "native"),
            Decl::Enum(e) => !has_meta(&e.meta, "native"),
            Decl::Typedef(t) => !has_meta(&t.meta, "native") && !is_uint_shim(&t.name),
            Decl::Global(g) => !has_meta(&g.meta, "native"),
            Decl::Function(f) => !has_meta(&f.meta, "native"),
        }
    }

    /// The ordered, de-duplicated `#include` list for the header generated from
    /// `module_index`: `StdAfx.h`, then inherited native `@:include`s, then the
    /// headers of imported modules that generate their own.
    pub fn header_includes(&self, module_index: usize) -> Vec<String> {
        let m = &self.modules[module_index];
        let target_dir = &m.dir;
        let mut out: Vec<String> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();

        let mut push = |inc: String, out: &mut Vec<String>| {
            if seen.insert(inc.clone()) {
                out.push(inc);
            }
        };

        // The per-package prelude header (`StdAfx.h`, or a configured name) always
        // comes first: Hatchet generates one for every output directory (merging any
        // prelude-source `@:headerCode` with the standard includes it requires), so
        // it is always available to include.
        push(format!("{}.h", self.stdafx_stem), &mut out);

        // This module's own @:include directives (resolved against itself).
        for raw in module_includes_raw(&m.file) {
            push(includes::resolve_include(&raw, &m.dir, target_dir), &mut out);
        }

        // Helper: pull in a module's header (or, for native-interop modules that
        // emit none, their @:include directives).
        let pull_module = |im: &Module, out: &mut Vec<String>, push: &mut dyn FnMut(String, &mut Vec<String>)| {
            if self.generates_header(im) {
                let mut header = im.dir.clone();
                let stem = im.path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                header.push(format!("{stem}.h"));
                push(includes::relative_header(&header, target_dir), out);
            } else {
                for raw in module_includes_raw(&im.file) {
                    push(includes::resolve_include(&raw, &im.dir, target_dir), out);
                }
            }
        };

        for &imp in &m.imports_resolved {
            pull_module(&self.modules[imp], &mut out, &mut push);
        }

        // Referenced-type headers: a type used without an explicit `import` (legal
        // in Haxe for same-package types) still needs its declaration included.
        // For the corpus this adds nothing (every reference is already imported);
        // it is what makes standalone, import-free projects compile.
        for dep in validate::referenced_modules(self, module_index) {
            pull_module(&self.modules[dep], &mut out, &mut push);
        }
        out
    }
}

// ---- free helpers ------------------------------------------------------

fn dir_components(src_root: &Path, file: &Path) -> Vec<String> {
    let parent = file.parent().unwrap_or(Path::new(""));
    match parent.strip_prefix(src_root) {
        Ok(rel) => rel
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// The leading lowercase-initial components of a dotted type path form its
/// package (e.g. `mucus.api.Mucus.Vertex` → `[mucus, api]`).
fn leading_package(path: &[String]) -> Vec<String> {
    path.iter()
        .take(path.len() - 1) // never include the type name itself
        .take_while(|c| c.chars().next().map(|ch| ch.is_lowercase()).unwrap_or(false))
        .cloned()
        .collect()
}

/// Parse `@:native("a::b::Name")` into `(Some([a,b]), Some("Name"))`. A bare
/// `@:native` yields `(None, None)`.
fn native_target(meta: &[Meta]) -> (Option<Vec<String>>, Option<String>) {
    let Some(m) = meta.iter().find(|m| m.name == "native") else {
        return (None, None);
    };
    let Some(arg) = m.first_arg() else {
        return (None, None);
    };
    let mut parts: Vec<String> = arg.split("::").map(|s| s.to_string()).collect();
    let name = parts.pop();
    let ns = if parts.is_empty() { None } else { Some(parts) };
    (ns, name)
}

/// Every `@:include` argument declared anywhere in a file.
fn module_includes_raw(file: &File) -> Vec<String> {
    let mut out = Vec::new();
    for decl in &file.decls {
        for m in decl_meta(decl) {
            if m.name == "include" {
                out.extend(m.args.iter().cloned());
            }
        }
    }
    out
}

fn decl_meta(decl: &Decl) -> &[Meta] {
    match decl {
        Decl::Class(c) => &c.meta,
        Decl::Interface(i) => &i.meta,
        Decl::Enum(e) => &e.meta,
        Decl::Typedef(t) => &t.meta,
        Decl::Global(g) => &g.meta,
        Decl::Function(f) => &f.meta,
    }
}

fn decl_name(decl: &Decl) -> Option<&str> {
    Some(match decl {
        Decl::Class(c) => &c.name,
        Decl::Interface(i) => &i.name,
        Decl::Enum(e) => &e.name,
        Decl::Typedef(t) => &t.name,
        Decl::Global(g) => &g.name,
        Decl::Function(f) => return f.name.as_deref(),
    })
}

fn decl_has_method(decl: &Decl, name: &str) -> bool {
    match decl {
        Decl::Class(c) => c.methods.iter().any(|m| m.name.as_deref() == Some(name)),
        Decl::Interface(i) => i.methods.iter().any(|m| m.name.as_deref() == Some(name)),
        _ => false,
    }
}

fn decl_bases(decl: &Decl) -> Vec<Type> {
    match decl {
        Decl::Class(c) => c
            .extends
            .iter()
            .cloned()
            .chain(c.implements.iter().cloned())
            .collect(),
        Decl::Interface(i) => i.extends.clone(),
        _ => Vec::new(),
    }
}

fn qualify(ti: &TypeInfo, current_ns: &[String]) -> String {
    let ns = ti.cpp_namespace();
    let name = ti.cpp_name();
    if ns == current_ns || ns.is_empty() {
        name.to_string()
    } else {
        format!("{}::{}", ns.join("::"), name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prog(units: &[(&str, &str)]) -> Program {
        let parsed: Vec<(PathBuf, File)> = units
            .iter()
            .map(|(path, src)| (PathBuf::from(path), parser::parse(src).unwrap()))
            .collect();
        Program::build(Path::new("/src"), parsed)
    }

    fn named(parts: &[&str]) -> Type {
        Type::Named {
            path: parts.iter().map(|s| s.to_string()).collect(),
            params: vec![],
            optional: false,
            line: 0,
        }
    }

    #[test]
    fn maps_primitives_and_containers() {
        let p = prog(&[("/src/modules/X.hx", "package modules; class X {}")]);
        let ns = vec!["modules".to_string()];
        let array_float = Type::Named {
            path: vec!["Array".into()],
            params: vec![named(&["Float"])],
            optional: false,
            line: 0,
        };
        assert_eq!(p.map_type_use(&array_float, 0, &ns), "std::vector<float>");
        assert_eq!(p.map_type_use(&named(&["UInt32"]), 0, &ns), "uint32_t");
        assert_eq!(p.map_type_use(&named(&["String"]), 0, &ns), "std::string");
        // Only the empty structure `{}` erases to `void*`; `Dynamic`/`Any` no longer
        // do (they are the overload marker, resolved at the call site, not spelled).
        assert_eq!(p.map_type_use(&Type::Anon(vec![]), 0, &ns), "void*");
        assert_eq!(p.map_type_use(&named(&["Dynamic"]), 0, &ns), "Dynamic");
    }

    #[test]
    fn native_types_get_engine_namespace_and_pointer() {
        let mucus = (
            "/src/mucus/api/Mucus.hx",
            "package mucus.api;\n\
             @:include(\"../../src/Mucus.h\")\n\
             @:native interface IEngine {}\n\
             @:native typedef Effects = { values:Array<Float> };\n\
             @:native typedef Vertex = { x:Float };",
        );
        let vertex = (
            "/src/modules/Vertex.hx",
            "package modules;\n\
             import mucus.api.Mucus;\n\
             import modules.Module;\n\
             @:expose class Vertex extends Module {}",
        );
        let module = (
            "/src/modules/Module.hx",
            "package modules; @:expose class Module {}",
        );
        let p = prog(&[mucus, vertex, module]);
        let vidx = p.modules.iter().position(|m| m.path.ends_with("Vertex.hx")).unwrap();
        let ns = vec!["modules".to_string()];

        // native interface → mucus::IEngine*, pointer because it's a reference type
        assert_eq!(p.map_type_use(&named(&["IEngine"]), vidx, &ns), "mucus::IEngine*");
        // native struct typedef → value, namespaced
        assert_eq!(p.map_type_use(&named(&["Effects"]), vidx, &ns), "mucus::Effects");
        // qualified native type disambiguates from the local `modules.Vertex` class
        let qualified = named(&["mucus", "api", "Mucus", "Vertex"]);
        assert_eq!(p.map_type_base(&qualified, vidx, &ns), "mucus::Vertex");
        // local user class in the same namespace → unqualified, pointer
        assert_eq!(p.map_type_use(&named(&["Module"]), vidx, &ns), "Module*");
    }

    #[test]
    fn header_includes_inherit_and_reference() {
        let mucus = (
            "/src/mucus/api/Mucus.hx",
            "package mucus.api;\n\
             @:include(\"../../src/Mucus.h\")\n\
             typedef UInt8 = UInt;\n\
             @:native interface IEngine {}",
        );
        let stdafx = ("/src/modules/StdAfx.hx", "package modules;");
        let module = ("/src/modules/Module.hx", "package modules; @:expose class Module {}");
        let vertex = (
            "/src/modules/Vertex.hx",
            "package modules;\n\
             import mucus.api.Mucus;\n\
             import modules.Module;\n\
             @:expose class Vertex extends Module {}",
        );
        let p = prog(&[mucus, stdafx, module, vertex]);
        let vidx = p.modules.iter().position(|m| m.path.ends_with("Vertex.hx")).unwrap();
        let incs = p.header_includes(vidx);
        // StdAfx (present in this package) first, the inherited native include,
        // then the user-class header.
        assert_eq!(incs, vec!["StdAfx.h", "../src/Mucus.h", "Module.h"]);
    }

    #[test]
    fn header_includes_are_reference_driven() {
        // A standalone, import-free project: B uses A (same package) with no
        // `import`. The header must pull in A.h (driven by the reference). StdAfx.h
        // is always present (Hatchet generates one for every output directory).
        let a = ("/src/A.hx", "@:expose class A { public function new() {} }");
        let b = (
            "/src/B.hx",
            "@:expose class B { var a:A; public function new() { this.a = new A(); } }",
        );
        let p = prog(&[a, b]);
        let bidx = p.modules.iter().position(|m| m.path.ends_with("B.hx")).unwrap();
        let incs = p.header_includes(bidx);
        assert_eq!(incs, vec!["StdAfx.h", "A.h"], "StdAfx.h always, plus A.h by reference");
    }

    #[test]
    fn at_include_works_on_any_file_and_keeps_system_headers() {
        // `@:include` is not exclusive to @:native API stubs: a pure @:expose
        // class can pull in C/C++ headers, and a system header keeps its `<...>`.
        let w = (
            "/src/W.hx",
            "@:include(\"<string>\")\n@:expose class W { public function new() {} }",
        );
        let p = prog(&[w]);
        let idx = p.modules.iter().position(|m| m.path.ends_with("W.hx")).unwrap();
        let incs = p.header_includes(idx);
        assert!(incs.contains(&"<string>".to_string()), "system @:include kept verbatim: {incs:?}");
        // StdAfx.h is always included (Hatchet generates one for every directory).
        assert_eq!(incs.first().map(String::as_str), Some("StdAfx.h"), "StdAfx.h first: {incs:?}");
    }

    #[test]
    fn native_only_module_emits_no_header() {
        let mucus = (
            "/src/mucus/api/Mucus.hx",
            "package mucus.api;\n@:include(\"../../src/Mucus.h\")\ntypedef UInt8 = UInt;\n@:native interface IEngine {}",
        );
        let p = prog(&[mucus]);
        assert!(!p.generates_header(&p.modules[0]));
    }
}
