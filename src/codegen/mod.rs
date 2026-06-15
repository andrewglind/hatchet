//! C++ code generation.
//!
//! Milestone 4 emits the header (`.h`) for every module that needs one: enums,
//! struct typedefs, alias typedefs, interfaces, and classes (declarations of
//! constructors/methods, inline getters/setters from property accessors, and
//! fields grouped by access). Method/constructor *bodies* and `.cpp` files are
//! milestone 5.

use std::collections::BTreeSet;
use std::fmt::Write;

use crate::ast::*;
use crate::sema::{Program, TypeKind};
use crate::stdafx;

pub mod holder;
pub mod ownership;
pub mod source;
pub use source::{generate_source, generate_source_diagnostics};

/// Generate the header text for the module at `module_index`, or `None` if it
/// does not produce a header (pure `@:native` interop, `StdAfx`, or empty).
pub fn generate_header(prog: &Program, module_index: usize) -> Option<String> {
    let m = &prog.modules[module_index];
    if m.is_stdafx || !prog.generates_header(m) {
        return None;
    }
    let gen = HeaderGen {
        prog,
        mi: module_index,
        ns: m.package.clone(),
    };
    Some(gen.build())
}

struct HeaderGen<'a> {
    prog: &'a Program,
    mi: usize,
    ns: Vec<String>,
}

impl<'a> HeaderGen<'a> {
    fn build(&self) -> String {
        let m = &self.prog.modules[self.mi];
        let stem = m.path.file_stem().and_then(|s| s.to_str()).unwrap_or("Module");
        let guard = format!("{}_H", sanitize(&stem.to_uppercase()));

        let mut out = String::new();
        let _ = writeln!(out, "#ifndef {guard}");
        let _ = writeln!(out, "#define {guard}");
        out.push('\n');
        for inc in self.prog.header_includes(self.mi) {
            // System headers (`<string>`) are emitted unquoted; project headers
            // are quoted.
            if inc.starts_with('<') {
                let _ = writeln!(out, "#include {inc}");
            } else {
                let _ = writeln!(out, "#include \"{inc}\"");
            }
        }
        out.push('\n');

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
                if g.is_final && g.access != Access::Private && !has_meta(&g.meta, "native") {
                    if let Some(text) = crate::codegen::source::render_final_const(self.prog, self.mi, g) {
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
        for decl in &m.file.decls {
            let chunk = match decl {
                Decl::Enum(e) if !has_meta(&e.meta, "native") => Some(self.emit_enum(e, base)),
                Decl::Typedef(t) if self.emit_typedef_wanted(t) => self.emit_typedef(t, base),
                Decl::Interface(i) if !has_meta(&i.meta, "native") => Some(self.emit_interface(i, base)),
                Decl::Class(c) if !has_meta(&c.meta, "native") => Some(self.emit_class(c, base)),
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

        // Free-function declarations come **after** the type definitions above, since
        // their signatures may reference those types (`function makeVec():Vec2`).
        // Public functions only — private ones are `static` in the `.cpp`.
        let mut emitted_fn = false;
        for decl in &m.file.decls {
            if let Decl::Global(g) = decl {
                if g.access != Access::Private && !has_meta(&g.meta, "native") {
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

        // `extern inline` functions become `extern "C"` exports at **global scope**
        // (an `extern "C"` symbol cannot be namespaced), declared with the portable
        // export/calling-convention macros.
        let mut extern_decls = String::new();
        for decl in &m.file.decls {
            if let Decl::Function(f) = decl {
                if !has_meta(&f.meta, "native") {
                    if let Some(sig) = self.extern_fn_decl(f) {
                        let _ = writeln!(extern_decls, "{sig};");
                    }
                }
            }
        }

        // Only wrap a namespace when there is something to put in it; a file whose
        // sole output is an `extern "C"` export has no namespace block at all.
        if !ns_body.trim().is_empty() {
            for part in &self.ns {
                let _ = writeln!(out, "namespace {part} {{");
            }
            out.push('\n');
            out.push_str(&ns_body);
            out.push('\n');
            for _ in self.ns.iter().rev() {
                let _ = writeln!(out, "}}");
            }
            out.push('\n');
        }
        if !extern_decls.is_empty() {
            out.push_str(&extern_decls);
            out.push('\n');
        }
        let _ = writeln!(out, "#endif");
        out
    }

    /// Global-scope declaration for an `extern inline` function:
    /// `<P>_EXPORT <ret> <P>_CALL name(params)` (no trailing `;`). Emitted outside
    /// any namespace, so every referenced type is fully qualified (empty namespace
    /// context). Returns `None` for non-`extern` functions.
    fn extern_fn_decl(&self, f: &Function) -> Option<String> {
        if !f.modifiers.is_extern {
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
        Some(format!("{prefix}_EXPORT {ret} {prefix}_CALL {name}({params})"))
    }

    // ---- forward declarations ------------------------------------------

    /// Class/interface (and ADT value-class) names this module must forward-
    /// declare: those referenced by an *earlier*-emitted type in the module's
    /// header, so a pointer/param/return to a not-yet-defined sibling resolves.
    /// Returned in definition order. Excludes `@:native` types.
    fn forward_decls(&self) -> Vec<String> {
        let decls = &self.prog.modules[self.mi].file.decls;

        // The local class-kinded types (the only things both forward-declarable
        // in C++98 and emitted by Hatchet itself), with their definition order.
        let mut def_order: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut emitted: Vec<&str> = Vec::new();
        for d in decls {
            let name = match d {
                Decl::Class(c) if !has_meta(&c.meta, "native") => &c.name,
                Decl::Interface(i) if !has_meta(&i.meta, "native") => &i.name,
                // An ADT enum lowers to a value *class*, so it too can be a
                // forward-declared target.
                Decl::Enum(e) if !has_meta(&e.meta, "native") && e.is_adt() => &e.name,
                _ => continue,
            };
            def_order.insert(name.clone(), emitted.len());
            emitted.push(name);
        }

        let mut needed: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut pos = 0usize;
        for d in decls {
            let is_emitted_type = matches!(d,
                Decl::Class(c) if !has_meta(&c.meta, "native"))
                || matches!(d, Decl::Interface(i) if !has_meta(&i.meta, "native"))
                || matches!(d, Decl::Enum(e) if !has_meta(&e.meta, "native") && e.is_adt());
            if !is_emitted_type {
                continue;
            }
            let here = pos;
            pos += 1;
            for r in header_type_refs(d) {
                if let Some(&def) = def_order.get(&r) {
                    // referenced before it is defined → needs a forward decl
                    if def > here {
                        needed.insert(r);
                    }
                }
            }
        }

        // Return in definition order (stable, readable) rather than alphabetical.
        let mut out: Vec<String> = needed.into_iter().collect();
        out.sort_by_key(|n| def_order.get(n).copied().unwrap_or(0));
        out
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
        let names: Vec<String> =
            e.variants.iter().map(|v| format!("{t}\t\t{}", v.name)).collect();
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
                let cpp = p
                    .ty
                    .as_ref()
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
                    let cpp = p
                        .ty
                        .as_ref()
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
            .filter_map(|f| self.default_value(&f.ty).map(|d| format!("{}({d})", f.name)))
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

    fn emit_class(&self, c: &Class, ind: usize) -> String {
        let t = tabs(ind);

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

        // `@:decl` exports the class from the DLL. Like `extern inline`, the
        // platform-specific attribute is emitted via a prelude macro (just the
        // visibility attribute — no `extern "C"`/calling convention, which would be
        // invalid on a class) so the output stays portable across compilers.
        let decl_mod = if has_meta(&c.meta, "decl") {
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
            let _ = writeln!(public, "{t}\t{}({params});", c.name);
        }
        // Destructor: empty by default, or freeing the pointers this class owns.
        // A `@:stackOnly` value class is non-polymorphic and owns no heap, so its
        // destructor is **non-virtual** — a vtable pointer would bloat `sizeof`
        // and break the flat value layout that makes `std::vector<Foo>` and
        // recursive-by-value composition work.
        let is_value = has_meta(&c.meta, "stackOnly")
            || has_meta(&c.meta, "value")
            || c.abstract_underlying.is_some();
        let virt_dtor = if is_value { "" } else { "virtual " };
        let deletes = ownership::owned_deletes(self.prog, self.mi, &self.ns, c);
        if deletes.is_empty() {
            let _ = writeln!(public, "{t}\t{virt_dtor}~{}() {{}}", c.name);
        } else {
            let _ = writeln!(public, "{t}\t{virt_dtor}~{}() {{", c.name);
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
                patched = Function { ret: accessor_ret_type(c, name), ..m.clone() };
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
            match m.access {
                Access::Private => protected.push_str(&line),
                _ => public.push_str(&line),
            }
            // `@:op(...)` on an abstract method → an additive C++ operator that
            // forwards to the named method (so the value reads as `a[k]` / `a + b`
            // *and* `a.method(...)`). The named method above is still emitted.
            if let Some(fwd) = self.op_forwarder(m) {
                public.push_str(&format!("{t}\t{fwd}\n"));
            }
            // `@:to` → an implicit conversion operator; `@:from` → a converting
            // constructor. Both forward to the named (instance/static) method.
            if let Some(fwd) = self.to_forwarder(m) {
                public.push_str(&format!("{t}\t{fwd}\n"));
            }
            if let Some(ctor) = self.converting_ctor(c, m) {
                public.push_str(&format!("{t}\t{ctor}\n"));
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
            if f.get == PropAccess::Get
                && f.set == PropAccess::Never
                && !has_meta(&f.meta, "isVar")
            {
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
                let _ = writeln!(s, "{t}\t{}({});", h.name, self.params_no_default(&ctor.params));
                let _ = writeln!(s, "{t}}};");
                s.push('\n');
            }
        }
        let _ = writeln!(s, "{t}class {decl_mod}{}{base} {{", c.name);
        let _ = writeln!(s, "{t}public:");
        s.push_str(&public);
        if !protected.is_empty() {
            let _ = writeln!(s, "{t}protected:");
            s.push_str(&protected);
        }
        let _ = writeln!(s, "{t}}};");
        s
    }

    fn emit_accessors(&self, _c: &Class, f: &Field, nullable: &BTreeSet<String>, ind: usize) -> String {
        let t = tabs(ind);
        let fty = self.field_type(_c, f, nullable);
        let is_ptr = fty.ends_with('*');
        let mut s = String::new();
        if generated_getter(f) {
            let getter = format!("Get{}", cap(&f.name));
            let constness = if is_ptr { "" } else { "const " };
            let _ = writeln!(s, "{t}\t{constness}{fty} {getter}() {{ return {}; }}", f.name);
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

    /// If `m` carries `@:op(...)`, the inline C++ operator that forwards to it
    /// (e.g. `Proxy operator[](int k) { return get(k); }`), else `None`. The
    /// operator's return/parameter types mirror the method's; unsupported op
    /// forms (the 2-arg `[]` write, `a.b`, `a()`, postfix) yield `None` here and
    /// are flagged by the validation pass.
    fn op_forwarder(&self, m: &Function) -> Option<String> {
        let name = m.name.as_ref()?;
        let arg = m.meta.iter().find(|me| me.name == "op").and_then(|me| me.first_arg())?;
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
        Some(format!("{ret} operator{token}({params}) {{ return {name}({arg_names}); }}"))
    }

    /// If `m` carries `@:to`, an implicit C++ conversion operator forwarding to
    /// it: `operator T() { return toX(); }` (non-`const`, so it may call the
    /// non-const named method). Expects a 0-parameter instance method.
    fn to_forwarder(&self, m: &Function) -> Option<String> {
        let name = m.name.as_ref()?;
        if !has_meta(&m.meta, "to") || m.modifiers.is_static || !m.params.is_empty() {
            return None;
        }
        let target = self.prog.map_type_use(m.ret.as_ref()?, self.mi, &self.ns);
        Some(format!("operator {target}() {{ return {name}(); }}"))
    }

    /// If `m` carries `@:from` (a static, single-parameter factory returning the
    /// abstract), a *converting constructor* forwarding to it, so the source type
    /// implicitly converts to the abstract: `Foo(Src s) { *this = fromX(s); }`.
    fn converting_ctor(&self, c: &Class, m: &Function) -> Option<String> {
        let name = m.name.as_ref()?;
        if !has_meta(&m.meta, "from") || !m.modifiers.is_static || m.params.len() != 1 {
            return None;
        }
        let p = &m.params[0];
        let decl = param_decl(self.prog, self.mi, &self.ns, p);
        // strip any ` = default` (a converting ctor parameter has none)
        let decl = decl.split(" = ").next().unwrap_or(&decl);
        Some(format!("{}({decl}) {{ *this = {name}({}); }}", c.name, p.name))
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
    /// `ret name(params)`. Skips `extern`/`@:native` (handled elsewhere) and the
    /// bodyless / `macro` forms. Defaults are kept on the declaration.
    fn plain_fn_decl(&self, f: &Function) -> Option<String> {
        if f.modifiers.is_extern || f.modifiers.is_macro || has_meta(&f.meta, "native") {
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
                let Some(Type::Func { ret, .. }) = &g.ty else { unreachable!() };
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
        params.iter().map(|p| self.param(p)).collect::<Vec<_>>().join(", ")
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
            bases.push(format!("public {}", self.prog.map_type_base(sup, self.mi, &self.ns)));
        }
        for i in &c.implements {
            bases.push(format!("public {}", self.prog.map_type_base(i, self.mi, &self.ns)));
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
        _ => p
            .ty
            .as_ref()
            .and_then(|t| enum_default(prog, mi, ns, t))
            .unwrap_or_else(|| "0".to_string()),
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
        Expr::Unary { op, expr, prefix: true } => {
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
            format!("{} {o} {}", enum_member_value(lhs)?, enum_member_value(rhs)?)
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
        Expr::Unary { op: UnOp::Neg, expr, prefix: true } => format!("-{}", enum_abstract_value(expr)?),
        _ => return None,
    })
}

/// For an enum-typed value, `Name_::FirstVariant` (namespaced if needed).
fn enum_default(prog: &Program, mi: usize, ns: &[String], ty: &Type) -> Option<String> {
    let Type::Named { path, .. } = ty else { return None };
    let ti = prog.resolve_type(path, mi)?;
    if ti.kind != TypeKind::Enum {
        return None;
    }
    let Decl::Enum(e) = prog.type_decl(ti)? else { return None };
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
        "+", "-", "*", "/", "%", "==", "!=", "<", ">", "<=", ">=", "&", "|", "^", "<<", ">>",
        "&&", "||",
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
        && c.methods.iter().any(|m| m.name.as_deref() == Some(&format!("set_{}", f.name)))
}

/// The return type Haxe infers for a property accessor whose signature omits
/// it: `get_x` and `set_x` both return the property's type (the common shape
/// `function set_x(v:T) { return this.x = v; }` relies on this — defaulting to
/// `void` would emit a value `return` from a void C++ function). `None` when
/// `method` is not an accessor of a matching declared property.
pub(crate) fn accessor_ret_type(c: &Class, method: &str) -> Option<Type> {
    let field = method.strip_prefix("get_").or_else(|| method.strip_prefix("set_"))?;
    let f = c.fields.iter().find(|f| f.name == field)?;
    let matches_kind = (method.starts_with("get_") && f.get == PropAccess::Get)
        || (method.starts_with("set_") && f.set == PropAccess::Set);
    if !matches_kind {
        return None;
    }
    f.ty.clone()
}

fn tabs(n: usize) -> String {
    "\t".repeat(n)
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
        Expr::Unary { op: UnOp::Neg, expr, prefix: true } => {
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
pub use stdafx::generate as generate_stdafx;
