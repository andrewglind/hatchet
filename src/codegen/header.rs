//! Header (`.h`) generation: the `HeaderGen` engine and the per-module /
//! `--header-only` entry points. Split out of `mod.rs`. The small param / type /
//! enum / literal helpers it leans on stay in `mod.rs` (shared with `source.rs`)
//! and are reachable here as a descendant module via `use super::*`.

use std::collections::BTreeSet;
use std::fmt::Write;

use crate::ast::*;
use crate::sema::Program;

use super::*;

/// Generate the header text for the module at `module_index`, or `None` if it
/// does not produce a header (pure `@:native` interop, `StdAfx`, or empty).
pub fn generate_header(prog: &Program, module_index: usize) -> Option<String> {
    generate_header_with(prog, module_index, &HeaderOpts::default()).map(|(s, _, _)| s)
}

/// Diagnostics raised while generating a header — `(warnings, errors)`, each paired
/// with its source line. Non-empty only when `inline_bodies` is set (the bodies are
/// generated here instead of in a `.cpp`); see [`HeaderOpts`].
pub type HeaderOutput = (String, Vec<(usize, String)>, Vec<(usize, String)>);

/// Knobs for header-only / amalgamated emission. All-default reproduces the normal
/// per-module header (declarations only, separate prelude `#include`, no inline
/// bodies), so existing callers are unaffected.
#[derive(Default, Clone)]
pub struct HeaderOpts {
    /// When set, the prelude body is inlined at the top of the header and the
    /// separate prelude `#include` (`StdAfx.h`) is omitted — a self-contained header.
    pub inline_prelude: Option<String>,
    /// Emit each class's constructor/method **bodies** inline (`inline Ret
    /// Class::m(){…}`) in the header, so no `.cpp` is needed. Diagnostics from body
    /// generation are returned via [`HeaderOutput`].
    pub inline_bodies: bool,
    /// Verbatim `@:headerCode` injected after the `#include`s and before the
    /// declarations (the per-module generalisation of the prelude's `@:headerCode`).
    pub header_code: Option<String>,
    /// Override the include-guard / naming stem (the amalgamation uses the chosen
    /// `--header-only` name rather than the module's own file stem).
    pub guard_stem: Option<String>,
}

pub fn generate_header_with(
    prog: &Program,
    module_index: usize,
    opts: &HeaderOpts,
) -> Option<HeaderOutput> {
    let m = &prog.modules[module_index];
    if m.is_stdafx || !prog.generates_header(m) {
        return None;
    }
    let gen = HeaderGen {
        prog,
        mi: module_index,
        ns: m.package.clone(),
        opts,
    };
    Some(gen.build())
}

pub(crate) struct HeaderGen<'a> {
    pub(crate) prog: &'a Program,
    pub(crate) mi: usize,
    pub(crate) ns: Vec<String>,
    pub(crate) opts: &'a HeaderOpts,
}

/// A C++ operator / conversion / converting-constructor forwarder built for an
/// `abstract` method, in three forms: the `inline` body (`Foo operator[](int k) {
/// return get(k); }`) used in-class when the forwarder's value types are all
/// complete there; and the `decl` + out-of-line `def` pair used when a by-value
/// return/parameter names a *later*-defined sibling class (incomplete in the
/// class body), so the definition must follow both class definitions — exactly
/// what a hand-written header does to break a `jobject`/`proxy` cycle.
struct Forwarder {
    inline: String,
    decl: String,
    def: String,
}

impl<'a> HeaderGen<'a> {
    fn build(&self) -> HeaderOutput {
        let m = &self.prog.modules[self.mi];
        let module_stem = m
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Module");
        let stem = self.opts.guard_stem.as_deref().unwrap_or(module_stem);
        let guard = format!("{}_H", sanitize(&stem.to_uppercase()));

        let mut out = String::new();
        let _ = writeln!(out, "#ifndef {guard}");
        let _ = writeln!(out, "#define {guard}");
        out.push('\n');
        // Header-only emission inlines the prelude (shim, std includes, export
        // macros) directly here and omits the separate prelude `#include`.
        if let Some(prelude) = &self.opts.inline_prelude {
            out.push_str(prelude.trim_end());
            out.push_str("\n\n");
        }
        let stdafx_inc = format!("{}.h", self.prog.stdafx_stem);
        for inc in self.prog.header_includes(self.mi) {
            // The inlined prelude replaces the `StdAfx.h` include.
            if self.opts.inline_prelude.is_some() && inc == stdafx_inc {
                continue;
            }
            // System headers (`<string>`) are emitted unquoted; project headers
            // are quoted.
            if inc.starts_with('<') {
                let _ = writeln!(out, "#include {inc}");
            } else {
                let _ = writeln!(out, "#include \"{inc}\"");
            }
        }
        out.push('\n');
        // Per-module `@:headerCode`, injected verbatim after the includes and before
        // the declarations (hxcpp semantics).
        if let Some(hc) = &self.opts.header_code {
            out.push_str(hc.trim_matches('\n'));
            out.push_str("\n\n");
        }

        let (section, warnings, errors) = self.section();
        out.push_str(&section);
        let _ = writeln!(out, "#endif");
        (out, warnings, errors)
    }

    /// The namespace-wrapped body of the header: forward declarations, public
    /// `final` constants, the type declarations (with inline constructor/method
    /// bodies when `inline_bodies` is set), the free-function declarations, then any
    /// global-scope `@cexport` `extern "C"` declarations. `build` wraps this in the
    /// guard + includes for a standalone per-module header; the header-only
    /// amalgamation concatenates one `section` per module under a single guard.
    pub(crate) fn section(&self) -> HeaderOutput {
        let mut warnings: Vec<(usize, String)> = Vec::new();
        let mut errors: Vec<(usize, String)> = Vec::new();
        let m = &self.prog.modules[self.mi];
        let base = self.ns.len();

        // The namespace body: public `final` constants → `static const` definitions
        // (file-local linkage per including TU), then public top-level
        // `final NAME = function/lambda` → free-function declarations (definitions
        // live in the `.cpp`), then the type declarations (enums, typedefs,
        // interfaces, classes). Public finals are constants inside the namespace —
        // there is no `#define` form; native (`@:native`) finals come from the C++
        // engine and are not emitted.
        let mut ns_body = String::new();

        // Forward declarations for classes/interfaces referenced *before* their
        // own definition in this module (mutually-recursive types). Hatchet
        // classes are reference types — every cross-class member/param/return is a
        // pointer, which only needs a forward declaration — so this resolves all
        // such cycles. Targeted: only the names actually referenced ahead of
        // their definition, and never `@:native` types (whose real definition
        // lives in the hand-written engine header). C++98 forbids forward-
        // declaring an enum, so only class-kinded types are declared here.
        let fwds = self.forward_decls();
        if !fwds.is_empty() {
            for name in &fwds {
                let _ = writeln!(ns_body, "{}class {name};", tabs(base));
            }
            ns_body.push('\n');
        }

        let mut emitted_const = false;
        for decl in &m.file.decls {
            if let Decl::Global(g) = decl {
                // `extern` finals are provided by hand-written C++ — not emitted.
                if g.is_final && !g.is_extern && g.access != Access::Private {
                    if let Some(text) =
                        crate::codegen::source::render_final_const(self.prog, self.mi, g)
                    {
                        ns_body.push_str(&text);
                        emitted_const = true;
                    }
                }
            }
        }
        if emitted_const {
            ns_body.push('\n');
        }
        let mut first = true;
        // Out-of-line `inline` forwarder definitions (a class returning a sibling
        // defined later) accumulate here and are emitted after every class, so
        // both ends of a cyclic value-type pair are complete by then.
        let mut deferred_defs = String::new();
        for decl in &m.file.decls {
            let chunk = match decl {
                Decl::Enum(e) if !e.is_extern => Some(self.emit_enum(e, base)),
                Decl::Typedef(t) if self.emit_typedef_wanted(t) => self.emit_typedef(t, base),
                Decl::Interface(i) if !i.is_extern => Some(self.emit_interface(i, base)),
                Decl::Class(c) if !c.is_extern && !has_meta(&c.meta, "proxy") => {
                    let (text, deferred) = self.emit_class(c, base);
                    deferred_defs.push_str(&deferred);
                    // Header-only: the constructor/method bodies are emitted inline
                    // here (after every class declaration) instead of in a `.cpp`.
                    if self.opts.inline_bodies {
                        let (defs, mut w, mut e) =
                            crate::codegen::source::inline_class_defs(self.prog, self.mi, c);
                        deferred_defs.push_str(&defs);
                        warnings.append(&mut w);
                        errors.append(&mut e);
                    }
                    Some(text)
                }
                _ => None,
            };
            if let Some(text) = chunk {
                if !first {
                    ns_body.push('\n');
                }
                first = false;
                ns_body.push_str(&text);
            }
        }
        if !deferred_defs.is_empty() {
            ns_body.push('\n');
            ns_body.push_str(&deferred_defs);
        }

        // Free-function declarations come **after** the type definitions above, since
        // their signatures may reference those types (`function makeVec():Vec2`).
        // Public functions only — private ones are `static` in the `.cpp`.
        let mut emitted_fn = false;
        for decl in &m.file.decls {
            if let Decl::Global(g) = decl {
                if g.access != Access::Private {
                    if let Some(sig) = self.free_fn_decl(g) {
                        if !emitted_fn && !first {
                            ns_body.push('\n');
                        }
                        let _ = writeln!(ns_body, "{}{sig};", tabs(base));
                        emitted_fn = true;
                    }
                }
            }
            // Plain module-level `function`s are declared in the header so other
            // translation units can call them.
            if let Decl::Function(f) = decl {
                if f.access != Access::Private {
                    if let Some(sig) = self.plain_fn_decl(f) {
                        if !emitted_fn && !first {
                            ns_body.push('\n');
                        }
                        let _ = writeln!(ns_body, "{}{sig};", tabs(base));
                        emitted_fn = true;
                    }
                }
            }
        }

        // `@cexport` functions become `extern "C"` exports at **global scope**
        // (an `extern "C"` symbol cannot be namespaced), declared with the portable
        // export/calling-convention macros.
        let mut extern_decls = String::new();
        for decl in &m.file.decls {
            if let Decl::Function(f) = decl {
                if let Some(sig) = self.extern_fn_decl(f) {
                    let _ = writeln!(extern_decls, "{sig};");
                }
            }
        }

        // Only wrap a namespace when there is something to put in it; a file whose
        // sole output is an `extern "C"` export has no namespace block at all.
        let mut sect = String::new();
        if !ns_body.trim().is_empty() {
            for part in &self.ns {
                let _ = writeln!(sect, "namespace {part} {{");
            }
            sect.push('\n');
            sect.push_str(&ns_body);
            sect.push('\n');
            for _ in self.ns.iter().rev() {
                let _ = writeln!(sect, "}}");
            }
            sect.push('\n');
        }
        if !extern_decls.is_empty() {
            sect.push_str(&extern_decls);
            sect.push('\n');
        }
        (sect, warnings, errors)
    }

    /// The inline constructor/method **definitions** for every class in this module,
    /// wrapped in the module's namespace. The header-only amalgamation emits this in
    /// a second pass — after every module's declarations — so a body that uses a
    /// class from another module sees its complete definition. Returns the text plus
    /// `(warnings, errors)` from body generation.
    pub(crate) fn inline_defs_section(&self) -> HeaderOutput {
        let mut warnings: Vec<(usize, String)> = Vec::new();
        let mut errors: Vec<(usize, String)> = Vec::new();
        let m = &self.prog.modules[self.mi];
        let mut body = String::new();
        for decl in &m.file.decls {
            if let Decl::Class(c) = decl {
                if c.is_extern || has_meta(&c.meta, "proxy") {
                    continue;
                }
                let (defs, mut w, mut e) =
                    crate::codegen::source::inline_class_defs(self.prog, self.mi, c);
                body.push_str(&defs);
                warnings.append(&mut w);
                errors.append(&mut e);
            }
        }
        // Module-level free functions (plain `function`s and `final NAME = lambda`)
        // are emitted `inline` here too — a header-only amalgamation has no `.cpp`
        // to define them in.
        let (fn_defs, mut fw, mut fe) =
            crate::codegen::source::inline_free_fn_defs(self.prog, self.mi);
        body.push_str(&fn_defs);
        warnings.append(&mut fw);
        errors.append(&mut fe);
        let mut out = String::new();
        if !body.trim().is_empty() {
            for part in &self.ns {
                let _ = writeln!(out, "namespace {part} {{");
            }
            out.push('\n');
            out.push_str(&body);
            out.push('\n');
            for _ in self.ns.iter().rev() {
                let _ = writeln!(out, "}}");
            }
            out.push('\n');
        }
        (out, warnings, errors)
    }

    /// Global-scope declaration for a `@cexport` function:
    /// `<P>_EXPORT <ret> <P>_CALL name(params)` (no trailing `;`). Emitted outside
    /// any namespace, so every referenced type is fully qualified (empty namespace
    /// context). Returns `None` for non-`@cexport` functions.
    fn extern_fn_decl(&self, f: &Function) -> Option<String> {
        if !has_meta(&f.meta, "cexport") {
            return None;
        }
        let name = f.name.as_ref()?;
        let prefix = &self.prog.export_macro;
        let ret = match &f.ret {
            Some(t) => self.prog.map_type_use(t, self.mi, &[]),
            None => "void".to_string(),
        };
        let params = f
            .params
            .iter()
            .map(|p| param_decl(self.prog, self.mi, &[], p))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!(
            "{prefix}_EXPORT {ret} {prefix}_CALL {name}({params})"
        ))
    }

    // ---- forward declarations ------------------------------------------

    /// Class/interface (and ADT value-class) names this module must forward-
    /// declare: those referenced by an *earlier*-emitted type in the module's
    /// header, so a pointer/param/return to a not-yet-defined sibling resolves.
    /// Returned in definition order. Excludes `@:native` types.
    /// The type declarations this header actually emits, in emission order — the
    /// same set and order as the body emit loop (enums, struct/alias typedefs,
    /// interfaces, classes). Typedefs are included so a struct typedef referencing
    /// a later class is recognised as a referrer.
    fn emitted_type_decls(&self) -> Vec<&'a Decl> {
        self.prog.modules[self.mi]
            .file
            .decls
            .iter()
            .filter(|d| match d {
                Decl::Enum(e) => !e.is_extern,
                Decl::Typedef(t) => self.emit_typedef_wanted(t),
                Decl::Interface(i) => !i.is_extern,
                Decl::Class(c) => !c.is_extern && !has_meta(&c.meta, "proxy"),
                _ => false,
            })
            .collect()
    }

    /// Forward-declarable / definition-order targets: class/interface/ADT-enum
    /// (the only things both forward-declarable in C++98 and emitted by Hatchet
    /// itself), keyed to their emission index.
    fn type_def_order(&self) -> std::collections::HashMap<String, usize> {
        let mut def_order = std::collections::HashMap::new();
        for (i, d) in self.emitted_type_decls().iter().enumerate() {
            let name = match d {
                Decl::Class(c) => Some(&c.name),
                Decl::Interface(it) => Some(&it.name),
                // An ADT enum lowers to a value *class*, so it too can be a target.
                Decl::Enum(e) if e.is_adt() => Some(&e.name),
                _ => None,
            };
            if let Some(n) = name {
                def_order.insert(n.clone(), i);
            }
        }
        def_order
    }

    /// The C++ name a locally-declared type is **emitted** under: its `@:native`
    /// rename if present, else its Haxe name. (`@:native` now only renames; it no
    /// longer suppresses emission — that is `extern`.)
    fn cpp_def_name(&self, haxe_name: &str) -> String {
        self.prog
            .resolve_type(std::slice::from_ref(&haxe_name.to_string()), self.mi)
            .map(|t| t.cpp_name().to_string())
            .unwrap_or_else(|| haxe_name.to_string())
    }

    fn forward_decls(&self) -> Vec<String> {
        let emitted = self.emitted_type_decls();
        let def_order = self.type_def_order();

        // A target referenced by an earlier-emitted declaration needs a forward
        // decl. Hatchet classes are reference types (cross-class members are
        // pointers) and a recursive value tree reaches itself through a container
        // (`std::vector<Foo>`), so a forward declaration always suffices.
        let mut needed: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for (here, d) in emitted.iter().enumerate() {
            for r in header_type_refs(d) {
                if let Some(&def) = def_order.get(&r) {
                    if def > here {
                        needed.insert(r);
                    }
                }
            }
        }

        // Return in definition order (stable, readable) rather than alphabetical,
        // mapped to the C++ name each type is emitted under (its `@:native` rename,
        // if any), since that is what the forward declaration must name.
        let mut out: Vec<String> = needed.into_iter().collect();
        out.sort_by_key(|n| def_order.get(n).copied().unwrap_or(0));
        out.into_iter().map(|n| self.cpp_def_name(&n)).collect()
    }

    // ---- enums ---------------------------------------------------------

    fn emit_enum(&self, e: &Enum, ind: usize) -> String {
        // A non-integral `enum abstract` (String/Float backing) is a namespace of
        // typed `static const` constants, not a C++ enum.
        if let Some(u) = &e.underlying {
            if !crate::sema::types::is_integral_underlying(u) {
                return self.emit_enum_abstract(e, u, ind);
            }
        }
        // An algebraic enum (parameterized variants) lowers to a tagged value
        // class instead of a bare C++ enum.
        if e.is_adt() {
            return self.emit_enum_adt(e, ind);
        }
        let t = tabs(ind);
        let mut s = String::new();
        let _ = writeln!(s, "{t}struct {}_ {{", e.name);
        let _ = writeln!(s, "{t}\tenum Enum {{");
        // An `enum abstract` member carries an explicit value (`Red = 0`); a plain
        // Haxe enum variant has none and relies on C++'s auto-increment.
        let names: Vec<String> = e
            .variants
            .iter()
            .map(|v| match v.value.as_ref().and_then(enum_member_value) {
                Some(val) => format!("{t}\t\t{} = {val}", v.name),
                None => format!("{t}\t\t{}", v.name),
            })
            .collect();
        s.push_str(&names.join(",\n"));
        s.push('\n');
        let _ = writeln!(s, "{t}\t}};");
        let _ = writeln!(s, "{t}}};");
        let _ = writeln!(s, "{t}typedef {}_::Enum {};", e.name, e.name);
        s
    }

    /// An algebraic enum (`enum Op { Halt; Add(a:Int, b:Int); }`) → the C++98
    /// tagged-value idiom: the usual tag enum (`struct Op_ { enum Enum { … } };`,
    /// so `case` labels keep the `Op_::Add` spelling shared with plain enums)
    /// plus a copyable value class named after the enum, holding the tag and one
    /// set of payload fields per parameterized variant (`int Add_a;` — a plain
    /// struct, not a union, which C++98 would forbid for non-POD payloads like
    /// `std::string`), with an inline static factory per variant
    /// (`Op::Add(1, 2)`) so construction reads like the Haxe. Values are passed
    /// and stored **by value** (like every other Haxe enum here): payload
    /// pointers are borrowed, never owned. A *recursive* payload (`Node(next:Op)`)
    /// would need an indirection C++ cannot express by value — the C++ compiler
    /// rejects the incomplete-type member, which is the loud backstop.
    fn emit_enum_adt(&self, e: &Enum, ind: usize) -> String {
        let t = tabs(ind);
        let mut s = String::new();
        // tag (identical to the plain-enum emission, minus the typedef — the
        // value class below takes the enum's name)
        let _ = writeln!(s, "{t}struct {}_ {{", e.name);
        let _ = writeln!(s, "{t}\tenum Enum {{");
        let names: Vec<String> = e
            .variants
            .iter()
            .map(|v| format!("{t}\t\t{}", v.name))
            .collect();
        s.push_str(&names.join(",\n"));
        s.push('\n');
        let _ = writeln!(s, "{t}\t}};");
        let _ = writeln!(s, "{t}}};");
        // value class: tag + per-variant payload fields + inline static factories
        let _ = writeln!(s, "{t}class {} {{", e.name);
        let _ = writeln!(s, "{t}public:");
        let _ = writeln!(s, "{t}\t{}_::Enum kind;", e.name);
        for v in &e.variants {
            for p in &v.params {
                let cpp =
                    p.ty.as_ref()
                        .map(|ty| self.prog.map_type_use(ty, self.mi, &self.ns))
                        .unwrap_or_else(|| "int".to_string());
                let _ = writeln!(s, "{t}\t{cpp} {}_{};", v.name, p.name);
            }
        }
        for v in &e.variants {
            let params: Vec<String> = v
                .params
                .iter()
                .map(|p| {
                    let cpp =
                        p.ty.as_ref()
                            .map(|ty| self.prog.map_type_use(ty, self.mi, &self.ns))
                            .unwrap_or_else(|| "int".to_string());
                    format!("{cpp} {}", p.name)
                })
                .collect();
            let mut body = format!("{} e; e.kind = {}_::{};", e.name, e.name, v.name);
            for p in &v.params {
                body.push_str(&format!(" e.{}_{n} = {n};", v.name, n = p.name));
            }
            let _ = writeln!(
                s,
                "{t}\tstatic {} {}({}) {{ {body} return e; }}",
                e.name,
                v.name,
                params.join(", ")
            );
        }
        // Structural equality: same tag, equal payload (Haxe compares enum
        // values by constructor + arguments via `Type.enumEq`; with a by-value
        // lowering this is the only meaningful `==` — pointer payloads compare
        // by address, preserving the reference flavour where it matters). An
        // if-chain rather than a switch, so every path visibly returns (no
        // C4715-style warnings on old compilers).
        let _ = writeln!(s, "{t}\tbool operator==(const {}& o) const {{", e.name);
        let _ = writeln!(s, "{t}\t\tif (kind != o.kind) return false;");
        for v in &e.variants {
            if v.params.is_empty() {
                continue;
            }
            let cmp: Vec<String> = v
                .params
                .iter()
                .map(|p| format!("{v}_{p} == o.{v}_{p}", v = v.name, p = p.name))
                .collect();
            let _ = writeln!(
                s,
                "{t}\t\tif (kind == {}_::{}) return {};",
                e.name,
                v.name,
                cmp.join(" && ")
            );
        }
        let _ = writeln!(s, "{t}\t\treturn true;");
        let _ = writeln!(s, "{t}\t}}");
        let _ = writeln!(
            s,
            "{t}\tbool operator!=(const {}& o) const {{ return !(*this == o); }}",
            e.name
        );
        let _ = writeln!(s, "{t}}};");
        s
    }

    /// A `String`/`Float`-backed `enum abstract` → a namespace of typed
    /// `static const` constants: `namespace X_ { static const T A = v; … }`. The
    /// members are referenced as `X_::A` — the same spelling as the enum form — and
    /// the type `X` itself maps to the underlying C++ type `T` (see `map_type_base`).
    /// No `typedef` is emitted; `static const` at namespace scope keeps it
    /// header-only (each translation unit gets its own copy).
    fn emit_enum_abstract(&self, e: &Enum, underlying: &Type, ind: usize) -> String {
        let t = tabs(ind);
        let ucpp = self.prog.map_type_use(underlying, self.mi, &self.ns);
        let mut s = String::new();
        let _ = writeln!(s, "{t}namespace {}_ {{", e.name);
        for v in &e.variants {
            let val = v
                .value
                .as_ref()
                .and_then(enum_abstract_value)
                .unwrap_or_else(|| "0".to_string());
            let _ = writeln!(s, "{t}\tstatic const {ucpp} {} = {val};", v.name);
        }
        let _ = writeln!(s, "{t}}}");
        s
    }

    // ---- typedefs ------------------------------------------------------

    fn emit_typedef_wanted(&self, t: &Typedef) -> bool {
        // A `@:native` typedef names an existing engine struct (external) — not
        // emitted; nor are the `UInt*` shims.
        !has_meta(&t.meta, "native") && !crate::sema::types::is_uint_shim(&t.name)
    }

    fn emit_typedef(&self, t: &Typedef, ind: usize) -> Option<String> {
        let tab = tabs(ind);
        match &t.target {
            TypedefTarget::Alias(ty) => {
                let target = self.prog.map_type_base(ty, self.mi, &self.ns);
                Some(format!("{tab}typedef {target} {};\n", t.name))
            }
            TypedefTarget::Struct(fields) => Some(self.emit_struct(&t.name, fields, ind)),
        }
    }

    fn emit_struct(&self, name: &str, fields: &[StructField], ind: usize) -> String {
        let t = tabs(ind);
        let mut s = String::new();
        let _ = writeln!(s, "{t}struct {name} {{");
        for f in fields {
            let ty = self.prog.map_type_use(&f.ty, self.mi, &self.ns);
            let _ = writeln!(s, "{t}\t{ty} {};", f.name);
        }
        // Optional fields get a default constructor initialising them.
        let inits: Vec<String> = fields
            .iter()
            .filter(|f| f.optional)
            .filter_map(|f| {
                self.default_value(&f.ty)
                    .map(|d| format!("{}({d})", f.name))
            })
            .collect();
        if !inits.is_empty() {
            let _ = writeln!(s, "{t}\t{name}() : {} {{}}", inits.join(", "));
        }
        let _ = writeln!(s, "{t}}};");
        s
    }

    // ---- interfaces ----------------------------------------------------

    fn emit_interface(&self, i: &Interface, ind: usize) -> String {
        let t = tabs(ind);
        let mut s = String::new();
        let base = self.bases(&i.extends);
        let _ = writeln!(s, "{t}class {}{base} {{", i.name);
        let _ = writeln!(s, "{t}public:");
        let _ = writeln!(s, "{t}\tvirtual ~{}() {{}}", i.name);
        for m in &i.methods {
            let sig = self.method_signature(m, true);
            let _ = writeln!(s, "{t}\tvirtual {sig} = 0;");
        }
        let _ = writeln!(s, "{t}}};");
        s
    }

    // ---- classes -------------------------------------------------------

    /// Returns `(class_text, deferred_defs)` — the second holds any out-of-line
    /// `inline` forwarder definitions whose by-value types are incomplete in the
    /// class body (a sibling defined later); the caller emits them after all
    /// classes.
    fn emit_class(&self, c: &Class, ind: usize) -> (String, String) {
        let t = tabs(ind);
        let mut deferred = String::new();
        // The C++ name this class is emitted under (its `@:native` rename, if any).
        let cpp = self.cpp_def_name(&c.name);

        // Fields whose value can be null (matched by an optional constructor
        // parameter of the same name) are stored as pointers when struct-typed.
        let nullable: BTreeSet<String> = c
            .ctor
            .iter()
            .flat_map(|ctor| ctor.params.iter())
            .filter(|p| p.optional)
            .map(|p| p.name.clone())
            .collect();

        // Property-accessor methods replaced by *generated* getters/setters are
        // suppressed from the ordinary method list. A custom accessor — the
        // user's `get_x` for `(get, …)`, or a user-written `set_x` for a `set`
        // property — is the opposite: it IS the implementation, declared and
        // emitted like any other method.
        let mut accessor_methods: BTreeSet<String> = BTreeSet::new();
        for f in &c.fields {
            if has_accessor(f) {
                if f.get != PropAccess::Get {
                    accessor_methods.insert(format!("get_{}", f.name));
                }
                if !custom_setter(c, f) {
                    accessor_methods.insert(format!("set_{}", f.name));
                }
            }
        }

        // `@libexport` exports the class from the shared library. Like `extern
        // inline`, the platform-specific attribute is emitted via a prelude macro
        // (just the visibility attribute — no `extern "C"`/calling convention, which
        // would be invalid on a class): `__declspec(dllexport)` on MSVC,
        // `__attribute__((visibility("default")))` on GCC/Clang, nothing elsewhere —
        // so the output stays portable across Windows DLLs and Unix `.so`/`.dylib`.
        let decl_mod = if has_meta(&c.meta, "libexport") {
            format!("{}_CLASS ", self.prog.export_macro)
        } else {
            String::new()
        };
        // Base-from-member idiom: when `super(...)` is not the first ctor statement,
        // an intermediate `XHolder` base computes the pre-super values.
        let holder = holder::analyze(self.prog, self.mi, &self.ns, c);
        let base = match &holder {
            Some(h) => h.base_list.clone(),
            None => self.class_bases(c),
        };

        let mut public = String::new();
        // Hatchet never emits C++ `private`: Haxe `private` is accessible from
        // subclasses (and Haxe has no "private even from subclasses" concept), so
        // a hidden member maps to C++ `protected` — otherwise a subclass reaching
        // an inherited member would compile in Haxe but be rejected by C++.
        let mut protected = String::new();

        // constructor + (inline, empty) destructor
        if let Some(ctor) = &c.ctor {
            let params = self.params(&ctor.params);
            let _ = writeln!(public, "{t}\t{}({params});", cpp);
        }
        // Destructor: empty by default, or freeing the pointers this class owns.
        // A `@:stackOnly` value class is non-polymorphic and owns no heap, so its
        // destructor is **non-virtual** — a vtable pointer would bloat `sizeof`
        // and break the flat value layout that makes `std::vector<Foo>` and
        // recursive-by-value composition work.
        let is_value = has_meta(&c.meta, "stackOnly") || c.abstract_underlying.is_some();
        let virt_dtor = if is_value { "" } else { "virtual " };
        let deletes = ownership::owned_deletes(self.prog, self.mi, &self.ns, c);
        if deletes.is_empty() {
            let _ = writeln!(public, "{t}\t{virt_dtor}~{}() {{}}", cpp);
        } else {
            let _ = writeln!(public, "{t}\t{virt_dtor}~{}() {{", cpp);
            for d in &deletes {
                let _ = writeln!(public, "{t}\t\t{d}");
            }
            let _ = writeln!(public, "{t}\t}}");
        }

        // methods
        for m in &c.methods {
            let Some(name) = &m.name else { continue };
            if accessor_methods.contains(name) {
                continue;
            }
            // A custom accessor with an omitted return type returns the
            // property's type in Haxe — never void.
            let patched: Function;
            let m = if m.ret.is_none() && accessor_ret_type(c, name).is_some() {
                patched = Function {
                    ret: accessor_ret_type(c, name),
                    ..m.clone()
                };
                &patched
            } else {
                m
            };
            let sig = self.method_signature(m, false);
            // Haxe methods are virtual by default. Emit `virtual` when this method
            // either overrides a base (the derived side) or is itself overridden by
            // a subclass (the base side) — otherwise a call through a base pointer
            // would static-bind. Static methods are never virtual.
            let virt = if !m.modifiers.is_static
                && (m.modifiers.is_override
                    || self.prog.method_overrides_base(c, self.mi, name)
                    || self.prog.method_overridden_in_subclass(c, self.mi, name))
            {
                "virtual "
            } else {
                ""
            };
            let stat = if m.modifiers.is_static { "static " } else { "" };
            // An `abstract function` is a pure virtual method (`= 0`): always
            // virtual, never defined (its `.cpp` body is correctly absent). Concrete
            // methods keep the override-driven `virtual` decision above.
            let line = if m.modifiers.is_abstract {
                format!("{t}\tvirtual {sig} = 0;\n")
            } else {
                format!("{t}\t{virt}{stat}{sig};\n")
            };
            // Haxe's default (no modifier) access is private, so only an explicit
            // `public` (or a custom accessor backing a public property) lands in the
            // public block; everything else is hidden (C++ `protected`, mirroring
            // the field grouping below).
            match method_access(c, m) {
                Access::Public => public.push_str(&line),
                _ => protected.push_str(&line),
            }
            // `@:op(...)` on an abstract method → an additive C++ operator that
            // forwards to the named method (so the value reads as `a[k]` / `a + b`
            // *and* `a.method(...)`). The named method above is still emitted.
            // A forwarder whose by-value type is a later sibling is declared here
            // and defined out-of-line (see `forwarder_deferred`).
            if let Some(fwd) = self.op_forwarder(c, m) {
                if self.forwarder_deferred(c, m.ret.as_ref()) {
                    public.push_str(&format!("{t}\t{}\n", fwd.decl));
                    let _ = writeln!(deferred, "{t}{}", fwd.def);
                } else {
                    public.push_str(&format!("{t}\t{}\n", fwd.inline));
                }
            }
            // `@:to` → an implicit conversion operator; `@:from` → a converting
            // constructor. Both forward to the named (instance/static) method.
            if let Some(fwd) = self.to_forwarder(c, m) {
                if self.forwarder_deferred(c, m.ret.as_ref()) {
                    public.push_str(&format!("{t}\t{}\n", fwd.decl));
                    let _ = writeln!(deferred, "{t}{}", fwd.def);
                } else {
                    public.push_str(&format!("{t}\t{}\n", fwd.inline));
                }
            }
            if let Some(fwd) = self.converting_ctor(c, m) {
                let src = m.params.first().and_then(|p| p.ty.as_ref());
                if self.forwarder_deferred(c, src) {
                    public.push_str(&format!("{t}\t{}\n", fwd.decl));
                    let _ = writeln!(deferred, "{t}{}", fwd.def);
                } else {
                    public.push_str(&format!("{t}\t{}\n", fwd.inline));
                }
            }
        }

        // generated getters/setters (always public)
        for f in &c.fields {
            if generated_getter(f) || (f.set == PropAccess::Set && !custom_setter(c, f)) {
                public.push_str(&self.emit_accessors(c, f, &nullable, ind));
            }
        }

        // fields, grouped by access. A property's backing field is hidden
        // (`protected`) when its writes are restricted (`null`/`never`) or routed
        // (`set`) — the C++ compiler then enforces the Haxe access rule — or when
        // a custom getter backs it. A write-open property (`(null, default)`/
        // `(never, default)`) stays a directly-writable field in its declared
        // access group.
        for f in &c.fields {
            // Haxe physicality: a `(get, never)` property without `@:isVar` is
            // purely computed — it has no backing field at all (`(get, null)`
            // keeps one: `null` write access is a physical store within the class).
            if f.get == PropAccess::Get && f.set == PropAccess::Never && !has_meta(&f.meta, "isVar")
            {
                continue;
            }
            // `@orderedMap var m:Map<K,V>` is stored as two insertion-ordered
            // parallel vectors (`m_keys`/`m_vals`) — a VC6-safe ordered map that
            // sidesteps `std::map` (key-sorted, and fragile on VC6).
            if let Some((keys, vals)) = self.ordered_map_vector_decls(f) {
                let block = match f.access {
                    Access::Public => &mut public,
                    _ => &mut protected,
                };
                let _ = writeln!(block, "{t}\t{keys};\n{t}\t{vals};");
                continue;
            }
            let line = format!("{t}\t{} {};\n", self.field_type(c, f, &nullable), f.name);
            let hidden_backing =
                has_accessor(f) && (f.set != PropAccess::Default || f.get == PropAccess::Get);
            if hidden_backing {
                protected.push_str(&line); // backing field
            } else {
                match f.access {
                    Access::Public => public.push_str(&line),
                    _ => protected.push_str(&line),
                }
            }
        }

        let mut s = String::new();
        // Emit the XHolder struct (members + ctor declaration) ahead of the class.
        if let Some(h) = &holder {
            if let Some(ctor) = &c.ctor {
                let _ = writeln!(s, "{t}struct {} {{", h.name);
                for decl in &h.member_decls {
                    let _ = writeln!(s, "{t}\t{decl}");
                }
                let _ = writeln!(
                    s,
                    "{t}\t{}({});",
                    h.name,
                    self.params_no_default(&ctor.params)
                );
                let _ = writeln!(s, "{t}}};");
                s.push('\n');
            }
        }
        let _ = writeln!(s, "{t}class {decl_mod}{}{base} {{", cpp);
        let _ = writeln!(s, "{t}public:");
        s.push_str(&public);
        if !protected.is_empty() {
            let _ = writeln!(s, "{t}protected:");
            s.push_str(&protected);
        }
        let _ = writeln!(s, "{t}}};");
        (s, deferred)
    }

    fn emit_accessors(
        &self,
        _c: &Class,
        f: &Field,
        nullable: &BTreeSet<String>,
        ind: usize,
    ) -> String {
        let t = tabs(ind);
        let fty = self.field_type(_c, f, nullable);
        let is_ptr = fty.ends_with('*');
        let mut s = String::new();
        if generated_getter(f) {
            let getter = format!("Get{}", cap(&f.name));
            let is_container = fty.starts_with("std::vector") || fty.starts_with("std::map");
            // An `Array`/`Map` and a value struct (user typedef or `@:native`) are all
            // *reference* types in Haxe: reading the property and mutating the result
            // (`a.items[i] = v`, `a.items.push(v)`, `a.vtx.x = 1`) is allowed even when
            // the field's *write* (reassignment) access is restricted. Returning by const
            // value would copy — the mutation would be lost or fail to compile — so the
            // generated getter hands back a mutable reference to the member. A pointer
            // field is already mutable through the pointer; a `String`/scalar value (no
            // in-place mutation in Haxe) stays a read-only copy.
            let is_struct = f.ty.as_ref().is_some_and(|t| self.is_value_struct(t));
            let ret = if is_ptr {
                fty.clone()
            } else if is_container || is_struct {
                format!("{fty}&")
            } else {
                format!("const {fty}")
            };
            let _ = writeln!(s, "{t}\t{ret} {getter}() {{ return {}; }}", f.name);
        }
        if f.set == PropAccess::Set && !custom_setter(_c, f) {
            let setter = format!("Set{}", cap(&f.name));
            let _ = writeln!(
                s,
                "{t}\tvoid {setter}({fty} {n}) {{ this->{n} = {n}; }}",
                n = f.name
            );
        }
        s
    }

    /// Whether a forwarder whose by-value return / parameter type is `ty` must be
    /// defined out-of-line: `ty` names a sibling class defined *later* in this
    /// module, which is incomplete inside `c`'s body (a by-value return/param then
    /// needs the full definition, which a forward decl can't supply). The class's
    /// own type is fine — a member body is a complete-class context.
    fn forwarder_deferred(&self, c: &Class, ty: Option<&Type>) -> bool {
        let order = self.type_def_order();
        let Some(&here) = order.get(&c.name) else {
            return false;
        };
        let Some(t) = ty else { return false };
        let mut names = Vec::new();
        type_names_in(t, &mut names);
        names
            .iter()
            .any(|n| order.get(n).is_some_and(|&def| def > here))
    }

    /// If `m` carries `@:op(...)`, the C++ operator that forwards to it (e.g.
    /// `Proxy operator[](int k) { return get(k); }`), else `None`. The operator's
    /// return/parameter types mirror the method's; unsupported op forms (the 2-arg
    /// `[]` write, `a.b`, `a()`, postfix) yield `None` here and are flagged by the
    /// validation pass.
    fn op_forwarder(&self, c: &Class, m: &Function) -> Option<Forwarder> {
        let name = m.name.as_ref()?;
        let arg = m
            .meta
            .iter()
            .find(|me| me.name == "op")
            .and_then(|me| me.first_arg())?;
        let token = cpp_operator(arg, m.params.len())?;
        let ret = match &m.ret {
            Some(ty) => self.prog.map_type_use(ty, self.mi, &self.ns),
            None => "void".to_string(),
        };
        let params = self.params(&m.params);
        let arg_names = m
            .params
            .iter()
            .map(|p| p.name.clone())
            .collect::<Vec<_>>()
            .join(", ");
        let body = format!("return {name}({arg_names});");
        let cpp = self.cpp_def_name(&c.name);
        Some(Forwarder {
            inline: format!("{ret} operator{token}({params}) {{ {body} }}"),
            decl: format!("{ret} operator{token}({params});"),
            def: format!("inline {ret} {cpp}::operator{token}({params}) {{ {body} }}"),
        })
    }

    /// If `m` carries `@:to`, an implicit C++ conversion operator forwarding to
    /// it: `operator T() { return toX(); }` (non-`const`, so it may call the
    /// non-const named method). Expects a 0-parameter instance method.
    fn to_forwarder(&self, c: &Class, m: &Function) -> Option<Forwarder> {
        let name = m.name.as_ref()?;
        if !has_meta(&m.meta, "to") || m.modifiers.is_static || !m.params.is_empty() {
            return None;
        }
        let target = self.prog.map_type_use(m.ret.as_ref()?, self.mi, &self.ns);
        let cpp = self.cpp_def_name(&c.name);
        Some(Forwarder {
            inline: format!("operator {target}() {{ return {name}(); }}"),
            decl: format!("operator {target}();"),
            def: format!("inline {cpp}::operator {target}() {{ return {name}(); }}"),
        })
    }

    /// If `m` carries `@:from` (a static, single-parameter factory returning the
    /// abstract), a *converting constructor* forwarding to it, so the source type
    /// implicitly converts to the abstract: `Foo(Src s) { *this = fromX(s); }`.
    fn converting_ctor(&self, c: &Class, m: &Function) -> Option<Forwarder> {
        let name = m.name.as_ref()?;
        if !has_meta(&m.meta, "from") || !m.modifiers.is_static || m.params.len() != 1 {
            return None;
        }
        let p = &m.params[0];
        let decl = param_decl(self.prog, self.mi, &self.ns, p);
        // strip any ` = default` (a converting ctor parameter has none)
        let decl = decl.split(" = ").next().unwrap_or(&decl);
        let cn = self.cpp_def_name(&c.name);
        let body = format!("*this = {name}({});", p.name);
        Some(Forwarder {
            inline: format!("{cn}({decl}) {{ {body} }}"),
            decl: format!("{cn}({decl});"),
            def: format!("inline {cn}::{cn}({decl}) {{ {body} }}"),
        })
    }

    // ---- signatures & types --------------------------------------------

    fn method_signature(&self, m: &Function, _interface: bool) -> String {
        let ret = match &m.ret {
            Some(ty) => self.prog.map_type_use(ty, self.mi, &self.ns),
            None => "void".to_string(),
        };
        let name = m.name.clone().unwrap_or_else(|| "new".to_string());
        let params = self.params(&m.params);
        format!("{ret} {name}({params})")
    }

    /// Declaration (`ret name(params)`) for a public top-level free function.
    /// Header declaration for a plain module-level `function name(...) {...}`:
    /// `ret name(params)`. Skips a `@cexport` function (declared as a global
    /// `extern "C"` export instead) and the bodyless / `macro` forms. Defaults are
    /// kept on the declaration.
    fn plain_fn_decl(&self, f: &Function) -> Option<String> {
        if f.modifiers.is_macro || has_meta(&f.meta, "cexport") {
            return None;
        }
        f.body.as_ref()?;
        let name = f.name.as_ref()?;
        let ret = match &f.ret {
            Some(t) => self.prog.map_type_use(t, self.mi, &self.ns),
            None => "void".to_string(),
        };
        let params = f
            .params
            .iter()
            .map(|p| param_decl(self.prog, self.mi, &self.ns, p))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!("{ret} {name}({params})"))
    }

    fn free_fn_decl(&self, g: &GlobalVar) -> Option<String> {
        if !g.is_final {
            return None;
        }
        let (params, ret, body) = match &g.init {
            Some(Expr::Lambda { params, ret, body }) => (params, ret, body),
            _ => return None,
        };
        let ret_cpp = match ret {
            Some(t) => self.prog.map_type_use(t, self.mi, &self.ns),
            // A function-type annotation on the binding (`Sq:(Int,Int)->Int = …`)
            // supplies the return type; else a `cast(…, T)` body; else `double`
            // (Haxe `Float`).
            None if matches!(&g.ty, Some(Type::Func { .. })) => {
                let Some(Type::Func { ret, .. }) = &g.ty else {
                    unreachable!()
                };
                self.prog.map_type_use(ret, self.mi, &self.ns)
            }
            None => match &**body {
                LambdaBody::Expr(Expr::Cast { ty: Some(t), .. }) => {
                    self.prog.map_type_use(t, self.mi, &self.ns)
                }
                _ => "double".to_string(),
            },
        };
        Some(format!("{ret_cpp} {}({})", g.name, self.params(params)))
    }

    fn params(&self, params: &[Param]) -> String {
        params
            .iter()
            .map(|p| self.param(p))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Like [`params`], but without ` = default` suffixes (for the `XHolder`
    /// constructor, which is always called with explicit arguments).
    fn params_no_default(&self, params: &[Param]) -> String {
        params
            .iter()
            .map(|p| match self.param(p).split_once(" = ") {
                Some((head, _)) => head.to_string(),
                None => self.param(p),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn param(&self, p: &Param) -> String {
        param_decl(self.prog, self.mi, &self.ns, p)
    }

    /// The C++ type for a class field, applying the nullable-struct→pointer rule.
    /// The two `std::vector` member declarations (without trailing `;`) for an
    /// `@orderedMap` Map field — `std::vector<K> m_keys`, `std::vector<V> m_vals` —
    /// or `None` when `f` is not an `@orderedMap` Map field.
    fn ordered_map_vector_decls(&self, f: &Field) -> Option<(String, String)> {
        let (kty, vty) = ordered_map_kv(f)?;
        let k = self.prog.map_type_use(kty, self.mi, &self.ns);
        let v = self.prog.map_type_use(vty, self.mi, &self.ns);
        let kpad = if k.ends_with('>') { " " } else { "" };
        let vpad = if v.ends_with('>') { " " } else { "" };
        Some((
            format!("std::vector<{k}{kpad}> {}_keys", f.name),
            format!("std::vector<{v}{vpad}> {}_vals", f.name),
        ))
    }

    fn field_type(&self, _c: &Class, f: &Field, nullable: &BTreeSet<String>) -> String {
        let ty = match &f.ty {
            Some(t) => t,
            None => return "void*".to_string(),
        };
        let base_use = self.prog.map_type_use(ty, self.mi, &self.ns);
        if base_use.ends_with('*') {
            return base_use; // reference type
        }
        if nullable.contains(&f.name) && self.is_value_struct(ty) {
            return format!("{}*", self.prog.map_type_base(ty, self.mi, &self.ns));
        }
        base_use
    }

    fn class_bases(&self, c: &Class) -> String {
        let mut bases = Vec::new();
        if let Some(sup) = &c.extends {
            bases.push(format!(
                "public {}",
                self.prog.map_type_base(sup, self.mi, &self.ns)
            ));
        }
        for i in &c.implements {
            bases.push(format!(
                "public {}",
                self.prog.map_type_base(i, self.mi, &self.ns)
            ));
        }
        if bases.is_empty() {
            String::new()
        } else {
            format!(" : {}", bases.join(", "))
        }
    }

    fn bases(&self, list: &[Type]) -> String {
        if list.is_empty() {
            return String::new();
        }
        let parts: Vec<String> = list
            .iter()
            .map(|t| format!("public {}", self.prog.map_type_base(t, self.mi, &self.ns)))
            .collect();
        format!(" : {}", parts.join(", "))
    }

    // ---- type predicates / defaults ------------------------------------

    fn is_value_struct(&self, ty: &Type) -> bool {
        is_value_struct(self.prog, self.mi, ty)
    }

    fn default_value(&self, ty: &Type) -> Option<String> {
        default_value(self.prog, self.mi, &self.ns, ty)
    }
}
