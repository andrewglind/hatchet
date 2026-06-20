# Milestone 11 — Generics (type parameters → C++98 templates)

> Status: **design sketch** (not yet started). Targets the next major feature after
> M10 (abstract types). Lower the Haxe type-parameter system onto C++98 templates
> that compile under VC6.

## 1. Goal & scope

Let users declare and instantiate their own parameterized types — `class Box<T>`,
`interface Iterable<T>`, `abstract Holder<T>(...)`, and parameterized `typedef`/`enum`
— lowering each to an idiomatic C++98 template that compiles under VC6. Today these
are *parsed and rejected* (`parser.rs:629`, `parser.rs:754/839`; the `Unsupported`
flags on `Class.type_params` / `Interface.type_params` / `Function.type_params` in
`ast.rs:349/380/408`).

**In scope (Phase A):**
- Generic `class` / `interface` with one or more type params.
- Instantiation at use sites: `Box<Int>`, `Box<Shape>`, `Pair<Int, Shape>`,
  nesting (`Box<Array<Int>>`).
- Type-param fields, params, returns, and locals.

**Deferred (later phases / out of scope), failing loud as today:**
- Type **constraints/bounds** (`<T:Comparable>`) — Phase C, likely documentation-only
  (C++98 has no concepts).
- **Generic methods** on non-generic types (`function first<T>(...)`) — Phase B.
- `@:generic` forced monomorphization, variance, `@:multiType`.
- **Type-argument inference** at call sites — require explicit args (`new Box<Int>(...)`),
  or infer only from the `new T<...>` spelling.

The guiding principle holds: a human writing C++98 for VC6 writes
`template<class T> class Box`, not five hand-copied classes — so **templates, not
monomorphized copies**, is the idiomatic lowering. Monomorphization appears only in
the *analysis* (ownership), never in the emitted code.

## 2. The core insight: pointer-ness lives in the type argument

This is the design lynchpin, and it falls out of how the container path already works.

The reference-type model makes class/interface types pointers via `map_type_use`
(adds the trailing `*`). A type parameter `T` is unknown at template-definition time,
so it **cannot** be auto-pointered. The resolution: emit `T` *verbatim as a value*,
and let the **type argument** carry its own pointer-ness at the instantiation site.

This is exactly what `container_template` already does at `types.rs:36` and
`map_type_base` (`sema/mod.rs` ~585): `Array<Shape>` maps its param through
`map_type_use` → `std::vector<Shape*>`. Generalize that one branch from "is it
`Array`/`Map`?" to "is it any user generic?" and `Box<Shape>` → `Box<Shape*>`,
`Box<Int>` → `Box<int>` for free.

So:
- **Template definition** uses `T` as a value:
  `template<class T> class Box { T value; T get(); };`
- **Instantiation** `Box<Shape>` → `Box<Shape*>` (reference arg brings its own `*`);
  `Box<Int>` → `Box<int>`.

The C++98 `>>` hazard is already handled by the `pad` logic in `map_type_base`.

## 3. Changes by stage

### Parser (`src/parser.rs`)
- Stop discarding type params in `parse_abstract` (`parser.rs:629`) and the
  generic-enum/typedef rejections (`parser.rs:754`/`839`); thread them into the AST
  the way class/interface/function already do. `parse_type_params` itself is fine
  as-is.
- Add `type_params: Vec<String>` to `Enum`, `Typedef`, and the abstract-synthesized
  `Class` (the abstract already lowers to `Class`, which has the field — just
  populate it instead of dropping).

### Sema — type-param scoping (`src/sema/mod.rs`, `src/sema/validate.rs`)
This is the one genuinely new analysis concept. Today an unknown type name maps
verbatim (the `None => name.to_string()` arm, `sema/mod.rs` ~608) — good for
*emission*, but the validate pass's unresolved-type check (`validate.rs:78`) would
flag bare `T` as "unresolved type."

- Introduce a **type-param scope**: when resolving types inside a generic decl's
  members, the decl's `type_params` are in-scope names that resolve to "type
  parameter," suppressing the unresolved-type diagnostic.
- `resolve_type` / `map_type_base` consult that scope; an in-scope param emits
  verbatim (already the behavior), a genuinely-unknown name still fails loud.

### Codegen — type mapping (`src/codegen/source/types.rs`, `src/sema/mod.rs`)
- Generalize the `container_template` branch to **any decl with non-empty
  `type_params`**: map each argument through `map_type_use`, join, emit `Name<args>`
  with the existing `>>` padding.
- A bare type-param `T` in field/param/return position emits as `T` (value),
  **not** `T*` — pointer-ness is already in the argument (§2).

### Codegen — header-only emission (`src/codegen/source.rs`, `src/codegen/source/decls.rs`)
The biggest structural change. **VC6 requires template definitions visible at every
instantiation** (no `export`, separate-`.cpp` templates don't link). So a generic
type must emit *declaration + all method bodies inline in the header*, never split
to `.cpp`.

- `generate_source` (`source.rs:27`) currently returns `None` for header-only
  modules (`source.rs:94`). Extend the header generator to emit generic types'
  method bodies inline (like the existing value-class/abstract inline path), and
  have the `.cpp` generator **skip** generic decls.
- Add `template<class T, class U>` prefixes to the class and to each
  out-of-line-but-still-in-header method.
- Mutually-recursive/cyclic generics reuse the existing forward-declaration
  machinery — but a forward decl of a template is `template<class T> class Box;`.

### Sema — ownership across instantiations (`src/sema/escape.rs`)
The hard design question, and §2 makes it tractable. `analyze_class`/`inferred_owned`
(`escape.rs:85/112`) decide owned-ness from concrete types; a `T` field has no
concrete type at definition.

**Rule (Phase A, sound and simple):** a field of bare type-param type is **borrowed
by default — never inferred-owned**. To own it, the developer writes `@owned`, which
emits `delete value;` in the template destructor.

**The soundness guard (reuses the whole-program analysis):** `@owned` on a `T` field
only type-checks for *reference-type* arguments (`delete` on `Box<int>`'s `int`
value is nonsense). Since the analysis is already whole-program, collect the set of
concrete arguments each generic is instantiated with, and **fail loud** if an
`@owned`-T generic is ever instantiated with a value-type argument. This keeps the
fail-loud contract and avoids per-instantiation destructor specialization (which VC6
can't do anyway).

This means **no monomorphized codegen** — only a monomorphized *check*.

## 4. Phasing

- **Phase A** — generic `class`/`interface`, value-or-reference `T`, header-only
  emission, borrowed-`T` ownership. Ships the 80%: `Box<T>`, `Pair<K,V>`,
  `Stack<T>`-over-vector.
- **Phase B** — `@owned T` with the instantiation-consistency check; generic
  `enum`/`typedef`; generic methods on non-generic types.
- **Phase C** — constraints/bounds (documentation + maybe a `// requires` comment;
  no `static_assert` in C++98).

## 5. Test plan (mirror existing `tests/*_compile.rs` gates)
- `generics_compile.rs`: `Box<Int>` (value arg) and `Box<Shape>` (reference arg)
  round-trip; `Pair<Int, Shape>`; nested `Box<Array<Int>>`.
- Compile-and-run gate under `g++ -std=c++98` (existing harness), plus at least one
  VC6-flavored smoke check given the header-only constraint.
- `@owned T` instantiated with a reference arg frees once; instantiated with a value
  arg → **diagnostic**, asserted in `tests/diagnostics.rs`.
- A generic type in a mutually-recursive module to exercise template forward
  declarations.

## 6. Risks / VC6 footguns
- **No template separation** → the header-only emission change is mandatory, not
  optional.
- **VC6 template name lookup** is pre-two-phase and buggy with nested/dependent
  names; keep generated template bodies syntactically plain (no clever
  dependent-name tricks).
- **Static members of templates** and default template args are weak in VC6 —
  exclude from Phase A.
- `>>` closing already handled; default *function* args inside templates should be
  verified against VC6.

## 7. Net assessment
Phase A is meaningfully smaller than it looks because the
type-argument-carries-pointer-ness model (§2) reuses the container path, and
`T`-verbatim emission already exists. The two real new pieces are **header-only
template emission** and the **type-param scope** in sema; the elegant part is that
ownership needs a monomorphized *check*, not monomorphized *output*.
