# Changelog

All notable changes to Hatchet are documented here. Versions follow the project's milestones.

## v0.3.4 — String literals match the type they are given (2026-09-25)

A Haxe string literal is typed `String` but emitted as a plain C++ literal — a `const char*`, not a
`std::string`. That is deliberate (it keeps `s == "x"` and `f("x")` free of a pointless temporary),
but in two positions the value then did not behave as the `String` Hatchet had typed it, and the code
generated around it was generated for a `std::string` that was not there. Both now materialise the
value, as `std::string("…")`, exactly where it is needed.

### A `?:` with a literal in both arms

The conditional's own C++ type was `const char*`, so:

```cpp
(cond ? "A" : "B") + " x"     // invalid operands to binary + — did not compile
(cond ? "A" : "B") == "A"     // compiled, and compared addresses instead of text
(cond ? "A" : "BB").length    // member call on a pointer
```

all three now work off `std::string(cond ? "A" : "B")`. The silent one is the reason this is worth
more than a compile fix: the comparison built without error and answered by pointer identity.

Only when **both** arms are literals — if either already holds a `std::string`, C++ unifies the
conditional to `std::string` on its own and the output is unchanged. Non-string ternaries are
untouched.

### A literal receiver of a member

`"abc".length`, `"abc".charAt(1)`, `"abc".indexOf("c")` and `"abc".charCodeAt(0)` emitted member
calls on a `const char*`. The receiver is now materialised for the call. (`substr`, `split` and the
case conversions were already correct — they read the receiver through a temporary.)

## v0.3.3 — VC6-safe conversions at call sites (2026-09-22)

Conversion hazards at call sites, routed around in the generator rather than worked around in Haxe.

### Narrowing conversions are explicit casts

A value stored into a smaller scalar than it has now says so — `(uint16_t)(base + 1)`,
`(float)(a * 0.5)` — instead of relying on the implicit conversion. The conversion happens either
way (Haxe arithmetic on `Int` is `int` arithmetic and on `Float` is double arithmetic), so nothing
changes at runtime; it silences MSVC C4244, and it is the one lever the generator has against a VC6
`/O2` miscompile in which a `uint16_t` narrowed from a **loop-derived `int`** takes the first
iteration's value on every iteration — correct element count, repeated values. Whether the cast
suppresses that is **unconfirmed**: it needs a real VC6 Release build to tell.

Applied wherever the target type is known — a call argument, a local or field initialiser or
assignment, a `return`, a container element, a struct-literal field — and only for genuine
narrowings: a smaller destination of the same kind, or a floating value into an integer. Literals
are written in the target type already, so they are not cast. C++ **integral promotion** is
accounted for: `n + 1` where `n` is a `cpp.UInt16` is an `int` expression, so storing it back into a
`uint16_t` is a narrowing and is cast.

### Pushed values are bound to an element-typed local

`std::vector<T>::push_back` takes `const T&`, so an argument that is not already a `T` lvalue
materialises a temporary at the call site and binds the reference to that. `push`, `insert` and
`unshift` now bind such an argument to a named local of the element type first:

```cpp
uint16_t _elem2 = (uint16_t)(base + 1);
indices.push_back(_elem2);
```

Left inline: a literal, a plain local or `this` field **already of the element type**, a
struct/array/map literal (already hoisted into an element-typed temp), and a pointer element type
(nothing converts, and hoisting a `new` would disturb the ownership lowering). Alias typedefs are
looked through, so a local declared `Tileset` still pushes straight into an `Array<Tile>` with no
redundant copy.

This began as a suspected fix for the VC6 miscompile above; that diagnosis was **wrong** — the fault
survives it, and the narrowing is the isolated trigger. It is kept as hardening: binding the
reference to a named variable is the shape hand-written C++ would have, at no runtime cost.

### Floating literals and narrowings in `cpp.Float32` contexts

Every Haxe floating literal was emitted as a C++ `double` literal, whatever it landed in, so a
`cpp.Float32` (C++ `float`) target narrowed at the conversion — which MSVC reports as C4305 on each
line. A literal in a `float` context is now emitted with the `f` suffix, in every position where
the target type is known: an argument to a `cpp.Float32` parameter, a local or field initialiser or
assignment, a `return`, an `Array<cpp.Float32>` element, and a struct-literal field.

```cpp
camera->SetPerspective(70.0f, 0.1f, 100.0f);   // was 70.0, 0.1, 100.0 — C4305
```

An **expression** (rather than a literal) narrowing into a `float` context is the C4244 counterpart,
covered by the explicit casts above — `this->fx = (float)(someFloatValue);`.

Arithmetic keeps Haxe's semantics: `Float` arithmetic *is* double arithmetic whatever it is assigned
to, so an operand keeps its `double` literal and `return a * 0.5;` still computes in double,
narrowing once at the end (`return (float)(a * 0.5);`). Suffixing the operand would silently make
the computation single-precision — a behaviour change rather than a cosmetic one. Genuine `Float` /
`double` contexts are untouched.

## v0.3.2 — Member types resolve where they are declared (2026-09-17)

A correctness release fixing two lowering bugs that share one root cause. When a module imports
two types with the same leaf name — typically a `@proxy` handle and an unrelated native value
struct, such as a `ui.Vertex` proxy alongside a `gfx.Vertex` struct — Hatchet could pick the wrong
one while typing the *members* of the other module's types.

### Fixes

- **A member's type resolves in the module that declares it.** The field types of a `typedef`
  struct or class, a property's field type, and a method's return type were resolved in the scope
  of the module *using* them rather than the module *declaring* them.

- **Types recovered from a C++ spelling match the qualified name.** A vector's element type or a
  map's value type is recovered from its emitted spelling (`std::vector<gfx::Vertex>`). That lookup
  used only the leaf name (`Vertex`), so it too could land on a same-named type in another
  namespace; it now matches namespace + name exactly, falling back to the leaf name only when no
  qualified match exists.

Existing code whose type names don't collide generates byte-identical output.

## v0.3.1 — Static fields (2026-07-20)

Class `static` fields are now lowered as genuine class-scoped statics instead of being
mistakenly emitted as per-instance members. A **scalar / `String`** static with a **literal**
initializer (or none) is a plain class static — `static T NAME;` in the header, with an out-of-line
`T Class::NAME = <literal>;` definition — and reads as `Class::NAME`.

Otherwise the field is lowered as a **Meyers singleton**: a `static T& NAME()` accessor whose
function-local `static` holds the value and is initialised on the first call, read as
`Class::NAME()`. This is the case for **any struct / container / reference-typed** static (C++98
cannot constant-initialise one as a class-scope data member) and for a **scalar with a non-literal
initializer** (a call, `new`, arithmetic, …) — deferring it to first use rather than running it at
an unspecified point in the C++ static-initialisation order (the "static init order fiasco"). A
function-local `static` is initialised exactly once by the language, so no guard flag is needed;
when the initializer builds a temporary (e.g. an array), that setup is folded into a one-off
`_init_*` helper so it too runs once. A `final` field returns `const T&` (it is immutable); a `var`
stays writable through the returned reference. Reads resolve correctly whether written bare inside
the class or qualified (`Class.NAME`) from elsewhere.

Because the accessor-vs-data-member choice is driven by the field's **type**, a consuming
`extern` / `@proxy` binding — which carries no initializer — reads the field the same way the
producing class emits it: a struct-typed `static` bound through an extern is called
(`native::Class::NAME()`), matching the native Meyers accessor, while a scalar `static final` stays
a plain data-member read.

## v0.3.0 — `@sink` in more positions (2026-07-13)

`@sink` means what it always has — *ownership leaves here; this scope does not free the value* —
but until now it could only be written on a **parameter**. This release lets you write it where
the hand-off actually happens: on a **call argument** and on a **local declaration**.

### Metadata is allowed on any expression

Haxe permits `@meta expr` on any expression, but Hatchet's expression parser had no arm for a
leading metadata token. Expression-position metadata now parses and is transparent to
type and value — it changes nothing about how the wrapped expression lowers. Metadata other than
`@sink` is carried but inert, matching hxcpp, which ignores expression-position metadata at the
C++ target.

### `@sink` on a call argument and a local declaration

The same transfer semantics as a `@sink` parameter, now available at two more sites:

- A `@sink new X(...)` **call argument** is emitted **inline** — not hoisted into a scope-owned
  local that the caller deletes.
- A `@sink local` **call argument** (an already-owned local) has its scope-close `delete`
  **dropped**, transferring the object to the callee.
- A `@sink var x = new X(...)` **local declaration** suppresses the scope-close `delete` of `x`
  entirely — the inverse of `@delete var` — for when ownership is handed off with no single call
  to attach the marker to.

`@sink` is an assertion that ownership leaves the current scope, so it applies even
when the destination is opaque to Hatchet (an `@:native` class, or a hand-off the analysis cannot
follow) — it overrides the escape analysis. If nothing actually takes ownership the result is a
leak, never a double-free. `@sink` on a *value* local or argument (nothing to hand off) is a
no-op and warns.

## v0.2.9 — Type-resolution & arithmetic fixes (2026-07-11)

A correctness release fixing three lowering bugs. A `Module.func()` call that targets a
module-level function no longer emits a bogus class qualifier; a `typedef` alias is now fully
transparent — it takes on the shape of whatever it names, everywhere; and mixed Int/Float
arithmetic infers `Float`, so a `var` bound to it no longer truncates. The fixed-C-array
"fill" detection added during the raw-pointer work — undocumented since v0.2.8 — is removed.

### Fixes

- **`Module.func()` for a module-level function drops the class qualifier.** Haxe lets you
  call a *module-level* function through the module name — `Palette.mix(a, b)` where `mix` is
  declared at the top of `Palette.hx`, not a member of the primary class `Palette`. Such a
  function lowers to a namespace free function, so Hatchet no longer emits the non-existent
  `Palette::mix(...)`; it emits `mix(...)` (namespace-qualified as needed, e.g. `util::mix`).
  A genuine static member (`Registry.slot(...)`) still uses scope resolution,
  `Registry::slot(...)`.

- **A `typedef` alias is fully transparent.** A Haxe `typedef X = Y` is the same type as `Y`
  everywhere, but Hatchet made shape decisions (pointer-vs-value, reference-vs-container,
  by-value-vs-`const&`, member dispatch) from the alias *name* rather than its target — so an
  alias only behaved correctly for a couple of shapes. Aliases are now resolved through
  *before* every such decision, while the alias name is kept in the emitted spelling. Fixes,
  across every alias shape:
    - `typedef Color = cpp.UInt32` (primitive) — passed **by value** (optional gets a default),
      not `const Color&` or a `Color*` pointer.
    - `typedef Panel = Widget` (a class) — a **`Panel*`** with `->` dispatch, not a sliced
      by-value `Panel`; methods/fields resolve through the alias (`panel.tag()` → `w->tag()`).
    - `typedef Ints = Array<Int>` (container) — passed **`const Ints&`** (Haxe's shared-reference
      semantics), not a silent by-value copy.
    - `typedef Vertex = Pt` (a `{ … }` struct) — `const Vertex&`, with working field access
      (`v.x`).
    - `typedef Name = String` — keeps the `const Name&` optimization; an optional `?n:Name`
      defaults to `""`.
    - `Null<Ptr>` where `typedef Ptr = cpp.RawPointer<T>` — a single pointer, no longer `Ptr*`
      (a double pointer). A custom iterator reached through an alias now iterates transparently.

- **Mixed Int/Float arithmetic infers `Float`.** An arithmetic operator's result type was
  taken from the left operand alone, so `intField / floatField` (and `+`, `-`, `*`, `%`) was
  inferred `Int`. A `var r = intField / floatField` was then declared `int` and truncated the
  `double` the expression actually computes. Arithmetic now promotes to `Float` when *either*
  operand is a `Float`, matching C++'s (and Haxe's) usual arithmetic conversions. Int-only
  arithmetic, shifts, and the existing `Int / Int → Float` double-division cast are unchanged.

### Removed: fixed-C-array "fill" detection

The raw-pointer work briefly special-cased `cpp.Pointer.ofArray(...).raw` written into fixed
C-array storage, warning that a bare `.raw` cannot fill a native `T[N]`. That handling was
de-documented in v0.2.8 and is now removed entirely: `cpp.Pointer.ofArray(a).raw` is simply
the address of the first element (`&(a)[0]`), with no position-dependent diagnostics. The
general `.raw` intrinsics (`cpp.Pointer.fromStar(x).raw`, `cpp.Pointer.ofArray(a).raw`) are
unchanged.

## v0.2.8 — C-style arrays & raw-pointer interop (2026-07-02)

A native-interop release: hxcpp's raw-pointer types now lower and the `.raw` pointer
intrinsics are recognised. `Dynamic` / `Any` become the faithful spelling for an opaque
`void*` and `{}` for that role is deprecated. A latent cast-ascription miscompile is fixed.
Internally the parser and code generator were split into focused modules with a reorganised
test suite. The `@:decl` / `@:abi` deprecation warnings added in v0.2.7 are removed (those
metadata are now silently parsed-and-ignored).

### hxcpp raw-pointer interop types lower to C pointers

The hxcpp pointer interop family now maps to native C++ pointers, joining `cpp.Pointer<T>`:

- `cpp.RawPointer<T>` and `cpp.Star<T>` → `T*`
- `cpp.ConstStar<T>` → `const T*`
- `cpp.Void` → `void`, so `cpp.RawPointer<cpp.Void>` / `cpp.Star<cpp.Void>` give `void*`

These resolve at every use site (field, parameter, local, return) and index as C pointers
(`p[i]`), so a Haxe binding to a native struct with a `T*` / `const T*` / `void*` member
transpiles faithfully.

### `Dynamic` / `Any` are the opaque `void*`; `{}` for that role is deprecated

`Dynamic` and `Any` in an *emitted* position now erase to `void*` — an opaque pointer that
carries anything (`Any` is `abstract Any(Dynamic)`, so both are Dynamic-backed at runtime).
`Dynamic` also keeps its existing `@:overload`-marker role.

Because `Dynamic` (opaque value) and `cpp.RawPointer<cpp.Void>` (opaque pointer) are now the
faithful spellings, using the empty structure **`{}` as a `void*` type is deprecated**. It
still lowers to `void*` so existing sources keep working, but now emits a non-fatal
deprecation warning naming the replacement; the `void*` lowering will be removed in a future
release. (In hxcpp `{}` is a structure object, not a raw pointer — this was a Hatchet-ism.)

### `.raw` pointer intrinsics

The `cpp.Pointer` `.raw` accessors used to satisfy hxcpp are recognised and lowered:

- **`cpp.Pointer.fromStar(x).raw`** — the developer's intent to view `x` as a raw pointer:
  emits `x` when `x` is already a pointer, else takes its address `&(x)`. This is the
  supported way to accept a `?data:Dynamic` argument and store it as `void*`
  (`this.data = cpp.Pointer.fromStar(data).raw;`).
- **`cpp.Pointer.ofArray(a).raw`** — a pointer to the first element, `&(a)[0]`.

### Reference semantics: mutable-reference getters, const-ref parameter writes

- **Write-restricted getters return `T&`.** A generated getter for a container
  (`std::vector` / `std::map`) or value-struct field now returns a mutable reference (`T&`)
  rather than a `const T` copy, so Haxe's reference-type mutation through a getter works:
  `obj.items[k] = v;` and in-place struct mutation compile and take effect.
- **Assigning through a `const&` value-struct parameter is a hard error.** A value-struct
  parameter lowers to `const T&`; writing through it is now reported up front instead of
  emitting non-compiling C++.

### Fixes

- **Array-literal cast-ascription element type.** `([...] : Array<cpp.UInt8>)` built a
  `std::vector<int>` and then assigned it to a `std::vector<uint8_t>` — a type clash that
  would not compile. A container ascription now pins the element type through the literal, so
  the vector is built as `std::vector<uint8_t>` directly.

### `@:decl` / `@:abi` deprecation warnings removed

v0.2.7 renamed the export behaviours to `@libexport` / `@cexport` and made `@:decl` / `@:abi`
emit a deprecation warning. Those warnings are now **removed**: `@:decl` / `@:abi` are Haxe
inbound-only metadata and are purely parsed-and-ignored, with no diagnostic.

### Internal

- The monolithic `parser.rs` and `codegen/mod.rs` were split into focused submodules
  (`parser/{decls,expr,stmt,types}`, `codegen/{header,amalgam}`, `codegen/source/{control,loops}`),
  and the single `tests/source_codegen.rs` was reorganised into topic-scoped files
  (`codegen_core`, `codegen_control`, `codegen_intrinsics`, `codegen_ownership`, `codegen_proxy`,
  `codegen_rawptr`, `codegen_types`) sharing a `tests/common` helper module. No behavioural change.

## v0.2.7 — Export metadata `@libexport` / `@cexport` (2026-06-26)

A metadata-correctness release with **a breaking rename** (deprecation path is provided).

### Haxe-faithful `@:decl` / `@:abi`; export behaviours move to `@libexport` / `@cexport`

Hatchet repurposed `@:decl` and `@:abi` for *outbound* (producing) C++:
`@:decl` decorated a class for shared-library export (`<PREFIX>_CLASS`), and `@:abi`
turned a free function into a global `extern "C"` export. The Haxe compiler
source shows both metadata are **inbound-only** and unrelated to that, So the two 
Hatchet behaviours have been renamed to dedicated custom metadata,
and `@:decl` / `@:abi` are now **parsed and ignored**.

Using `@:decl` / `@:abi` tokens now emits a non-fatal **deprecation
warning** naming the replacement.

Bare `@:decl` / `@:abi` now have no effect beyond the warning, and will be removed in a future release.

#### Migration steps
- replace `@:decl` on an exported class with `@libexport`
- replace `@:abi` on an exported function with `@cexport`

## v0.2.6 — Scalar type ascriptions (2026-06-25)

A correctness release: a scalar-numeric type-check expression now emits a real C++ cast, so the ascribed
arithmetic type is honoured at the value level instead of being silently narrowed. No breaking changes.

### `(expr : Type)` pins the C++ arithmetic type

A type-check / ascription expression `(expr : Type)` was treated as a pure compile-time hint: the inner
expression was emitted unchanged and only its *internal* type was relabelled. For a scalar-numeric target
that lost the developer's intent — `(Std.parseFloat(s) : cpp.Float32)` still emitted a `double` (`atof`),
and `(0.0 : cpp.Float32)` a `double` literal, so a `cpp.Float32`-returning function silently narrowed its
result on `return`.

When the ascribed type is a built-in arithmetic scalar (`Int`, `Float`, `Single`/`cpp.Float32`, `Bool`,
the fixed-width integers, …), Hatchet now emits an explicit C cast to that type, matching what hxcpp
generates:

```haxe
function toFloat(s:String):cpp.Float32 {
  return (s != null) ? (Std.parseFloat(s) : cpp.Float32) : (0.0 : cpp.Float32);
}
```

now lowers both arms to `float` — `((float) atof(s.c_str()))` and `((float) 0.0)` — so the ternary type
matches the `float` return with no implicit narrowing.

Non-scalar ascriptions keep the existing no-op behaviour: a class-typed `null` (`(null : Widget)`), an
empty container literal (`([] : Array<Int>)`), or a `String` adopt the ascribed type without an unwanted
or unbuildable cast.

## v0.2.5 — Haxe-aligned `untyped` & `cpp.ConstCharStar` (2026-06-24)

A correctness release: `untyped` now matches Haxe's actual semantics, raw C++ injection moves to the
`__cpp__` intrinsic it belongs to, and the `cpp.ConstCharStar` interop type lowers. No breaking changes
for the supported `untyped` idiom — see below.

### `untyped` and `__cpp__` aligned with Haxe

Previously `untyped <expr>` was a *lexical* construct: it sliced the raw source to the end of the
statement and emitted it verbatim, bypassing all transpilation. That conflated two separate Haxe
mechanisms and meant the canonical escape hatch `untyped __cpp__("…")` was broken (it emitted the literal
text `__cpp__("…")`).

The two concepts are now distinct, matching Haxe:

- **`untyped <expr>`** is the typer escape hatch. The operand is parsed and transpiled like any other
  expression; the only effect is that its static type is treated as opaque (`Dynamic`), so type-driven
  checks relax. It binds as a normal unary prefix rather than swallowing to the next `;`.
- **`__cpp__("…", a, b)`** (Haxe's `cpp.Syntax.code`) is the raw-injection intrinsic. The format string
  is emitted verbatim, with `{0}`, `{1}`, … placeholders replaced by the **transpiled** arguments — so
  real Hatchet expressions can be spliced into hand-written C++ (`__cpp__("::fmaxf({0}, {1})", v, lo)` →
  `::fmaxf(v, lo)`). Because Hatchet only ever targets C++, the call is recognised **with or without** the
  `untyped` wrapper Haxe needs to silence its unknown-identifier error.

The common existing idiom `untyped someCName(args)` is unaffected: the call is now transpiled normally and
produces identical output.

### `cpp.ConstCharStar` lowers to `const char*`

hxcpp's `cpp.ConstCharStar` interop type now maps to `const char*` (joining `cpp.StdString` → `std::string`
and the rest of the primitive mappings).

## v0.2.4 — Typedef-alias containers & method visibility (2026-06-23)

A correctness release: escape analysis now sees through typedef aliases to the containers they name,
and class methods land in the right C++ visibility section. No breaking changes.

### Escape analysis sees through typedef alias containers

Escape analysis now peels typedef aliases (`typedef Matrix = Array<Row>`) before counting `Array` nesting,
so aliased containers are tracked at their true depth. A local container of owned (`new`-d) elements that
flows into an owned alias-typed field is recognised as escaping and is no longer freed at end of scope.

A nullable alias container (`Null<Indicies>` where `typedef Indicies = Array<Int>`) also keeps its
pointer-ness through alias resolution, so container methods on it dereference correctly — `indices.map(…)`
lowers to `(*indices).size()` / `(*indices)[i]`.

### Class methods lower to the correct C++ visibility

A class method lands in the C++ `public` section only when it is explicitly `public`; everything else —
including Haxe's default (no modifier) access, which is private — lowers to `protected`, mirroring how
fields are grouped.

Custom property accessors are the exception: a `get_x` / `set_x` backing a `public` property is promoted to
`public` even when the accessor itself is private, keeping it at least as visible as the property it serves.

## v0.2.3 — Ordered maps (`@orderedMap`) (2026-06-21)

A minor release on top of Milestone 10: a `@orderedMap` field metadata that stores a `Map` as two
insertion-ordered parallel vectors — a VC6-safe ordered map — plus a fail-loud fix for anonymous-struct
container elements. No breaking changes.

### `@orderedMap` — an insertion-ordered map without `std::map`

`@orderedMap` on a class field of type `Map<K,V>` stores it as two parallel `std::vector`s — `m_keys`
and `m_vals` — instead of a `std::map`:

```haxe
@orderedMap public var object:Map<String, JValue>;
```

This buys two things `std::map` cannot. First, **insertion order**: a `std::map` is key-sorted, and Haxe
`Map` iteration order is unspecified under hxcpp, whereas the parallel vectors preserve first-seen order
(both only ever grow at the end) — exactly what JSON-object round-tripping needs. Second, **VC6 safety**:
the vectors hold a single key/value type each (no `std::map`, which is fragile on VC6, and no incomplete
recursive container — a reference-typed `V` is a pointer, forward-declarable).

Every operation lowers to a scan over the vectors, no `std::map` anywhere: `get`/`exists` (linear find),
`set` (find-or-append, replacing in place to keep position), `remove` (paired erase), `keys()` (the keys
vector), and `for (k => v in m)` / `for (v in m)` / `[for (k => v in m) …]` (a paired index loop).
Construction is `new Map()` / `[]` (clears) or a map literal (clears, then appends each pair in order).
Lookups are O(n) linear scans rather than `std::map`'s O(log n) — faster for the small maps this targets,
but not for large ones.

Because an `@orderedMap` field has no single map object, using it **as a whole value** — returning it,
passing it, assigning a map into it — is a hard error naming the supported operations, rather than
emitting code that will not compile. The hxcpp build still sees an ordinary `Map<K,V>` (so the source
stays valid, type-checkable Haxe); the parallel-vector representation and its insertion-order guarantee
are Hatchet's, matching the "runtime authority lives in the emitted C++98" contract.

### String nullability: errors over guesses, and a working `Null<String>`

A plain `String` lowers to a value `std::string`, which has no null state. Previously `s == null` /
`s != null` on a value String was silently lowered to `s.empty()` / `!s.empty()` — a different predicate
that conflates `null` with the empty string `""` (it would, for instance, drop a JSON key of `""`). That
silent guess is gone:

- **A value `String` compared to `null` is now a hard error**, steering to `!= ""` (emptiness) or
  `Null<String>` (nullability). The one exception is an **optional `?s:String`** parameter, which
  defaults to `""`, so its "was it passed?" check still reads as `s.empty()`.
- **`Null<String>` now works end-to-end.** A `Null<T>` over a value `T` is an owned heap pointer:
  `null` → `NULL`, assigning a value heap-wraps (`new std::string(v)`) and frees any prior value, a
  value-position read dereferences (`NULL` → `""`), `!= null` is a real pointer check, and the
  destructor frees it. (This also fixes the same assignment gap for other `Null<value-type>` fields,
  which previously emitted non-compiling `T* = T`.)

### Anonymous struct as a container element fails loudly

`Array<{ key:String, val:T }>` (an inline anonymous struct as a container element) previously lowered to
a useless `std::vector<void*>`. It is now a hard error pointing at the fix — give the struct a `typedef`
and use that named type as the element — rather than a silent miscompile. (A named struct typedef as a
container element already works.)

## v0.2.2 — Custom iterators, container-typedef resolution, header-only functions (2026-06-21)

A minor release on top of Milestone 10: a general **`Iterator`/`Iterable` protocol** for `for` loops and
comprehensions, **alias-typedef containers** resolved at every use site, **module-level functions in
`--header-only`** mode, and a **provenance banner** on every generated file. No breaking changes — existing
output is unchanged except for the new banner comment at the top of each file.

### Highlights

- **Custom `Iterator` / `Iterable` iteration** — `for (x in e)` and comprehensions now iterate any value
  exposing `hasNext`/`next` or `iterator()`, not just ranges, `Array`, and `Map`.
- **Alias typedefs of containers** — `typedef Tilesets = Array<…>` now behaves as a container everywhere
  (iteration, `new`, `.push`, `.length`, indexing), not only in declarations.
- **`--header-only` module-level functions** — plain `function`s and `final NAME = lambda` free functions
  are emitted `inline` into the amalgamation; only `@:abi` exports remain unsupported there.
- **Provenance banner** — every generated file opens with `// Generated by Hatchet (<repo>) v<version>`.

### Custom `Iterator` / `Iterable` iteration

`for (x in e)` now iterates any value that implements the Haxe iteration protocol, not just ranges,
`Array`, and `Map`:

- an **Iterator** — `e` itself exposes `hasNext():Bool` and `next():T`;
- an **Iterable** — `e` exposes `iterator():Iterator<T>`.

Both lower to a `while (it.hasNext()) { T x = it.next(); … }` loop, with `.`/`->` access chosen by whether
the iterator is a value or a reference type. When `iterator()` hands back a heap (reference-type) iterator,
the loop **owns and `delete`s it** — including on an early `return` out of the loop body (freed via the
all-scopes delete) with no double-free on normal completion. The same lowering drives array/map
comprehensions (`[for (x in e) …]`). A value that implements neither protocol (nor is a range/Array/Map)
is still a hard error, as is `key => value` over a value-only custom iterator (that needs a `Map` or
`Array`). The protocol methods must be declared on the iterated type **itself**: a custom iterator reached
only through a typedef alias, or inherited from a base class, is deliberately not detected — but it now
**fails loudly** with a message naming that cause, in both `for` statements and comprehensions (a
comprehension over an undetected type previously emitted invalid `.size()`/`[]` access instead of erroring).

### Alias typedefs of containers resolve at use sites

A container alias — `typedef Tileset = Array<Tile>; typedef Tilesets = Array<Tileset>;` — maps *as a name*
to its emitted `typedef std::vector<…>`. Container operations now resolve through such aliases to the real
`std::vector`/`std::map`/`std::string` head, so they work on aliased values exactly as on the underlying
container: **iteration** and comprehensions, `new Tilesets()` (value-constructed, never treated as an
owned heap pointer to `delete`), `.push`→`push_back`, `.length`→`.size()`, and `arr[i]` indexing.
Previously these saw only the alias name and either failed to transpile (iteration) or emitted invalid C++.

### Alias typedefs of containers resolve at use sites

A container alias — `typedef Tileset = Array<Tile>; typedef Tilesets = Array<Tileset>;` — maps *as a name*
to its emitted `typedef std::vector<…>`. Container operations now resolve through such aliases to the real
`std::vector`/`std::map`/`std::string` head, so they work on aliased values exactly as on the underlying
container: **iteration** and comprehensions, `new Tilesets()` (value-constructed, never treated as an
owned heap pointer to `delete`), `.push`→`push_back`, `.length`→`.size()`, and `arr[i]` indexing.
Previously these saw only the alias name and either failed to transpile (iteration) or emitted invalid C++.

### Header-only: module-level free functions

`--header-only` now supports **module-level free functions** — both the plain `function name(...) {...}`
form and the `final NAME = (...) -> ...` lambda form. They are emitted `inline` into the amalgamated
header (ODR-safe across the translation units that include it), alongside the inline class bodies; a
file-local (`private`) helper used before its definition is forward-declared. Only `@:abi` `extern "C"`
exports remain unsupported in this mode — an exported symbol still needs an object file. Because every
module in a package shares one C++ namespace in the amalgamation, two free functions with the **same name
in the same package** are now a hard error rather than non-compiling output.

### Generated-file provenance banner

Every generated file — per-module `.h`/`.cpp`, the `StdAfx` prelude, and a `--header-only` amalgamation —
now opens with a comment naming the repository and the transpiler version, e.g.
`// Generated by Hatchet (https://github.com/andrewglind/hatchet) v0.2.2`, using the same version string
`--version` reports. It is a `//` comment ahead of the include guard, so it never affects compilation.

## v0.2.1 — Header-only output, resolve-only includes, null-safe fix (2026-06-19)

A minor release on top of Milestone 10: a **single-header amalgamation** mode, an explicit
**resolve-only input** flag, the generalisation of `@:headerCode` to any module, and a correctness fix
for null-safe navigation. No breaking changes — the new flags are opt-in and default `.h`/`.cpp` output
is unchanged.

### Highlights

- **`--header-only <NAME>`** — amalgamate an entire `--src` set into one self-contained `<NAME>.h`: the
  prelude inlined, every class emitted with inline bodies, native `@:include`s hoisted, no `.cpp` and no
  separate `StdAfx.h`. A drop-in single-header library.
- **`--include <PATH>...`** — resolve-only inputs: `extern`/`@:native` stub files parsed for resolution
  and `@:include` propagation, but never transpiled (the Haxe equivalent of a C/C++ header).
- **`@:headerCode` on any module** — previously honoured only on the prelude source, now injected
  verbatim into any emitted module's header (matching hxcpp).
- **Fix** — null-safe navigation combined with null-coalescing (`recv?.m() ?? default`).

### Header-only output

`--header-only <NAME>` (a trailing `.h` is stripped) amalgamates every `--src` module into a single
`<NAME>.h`:

- the prelude (the `uint*_t` shim, the standard includes, the export macros, and any `StdAfx.hx`
  `@:headerCode`) is inlined at the top instead of emitted as a separate `StdAfx.h`;
- every class is emitted with its constructor/method bodies **inline** (`inline T C::m() { … }`), so no
  `.cpp` is produced;
- the native `@:include`s of all modules are hoisted to the top and de-duplicated;
- declarations and bodies are emitted in **two passes** (all declarations, then all bodies) behind a
  global forward-declaration block, so cross-module references resolve.

Because the single header has no `#include`s to settle the order, the modules are **topologically
sorted**: a module that needs another's type *complete* — a base class (`extends`/`implements`) or a
value (non-pointer) field — is emitted after its dependency (pointer cross-references impose no order,
the forward-declaration block covers them). A genuine cross-module dependency **cycle** is a hard error
rather than non-compiling output. Module-level free functions and `@:abi` exports are rejected in this
mode (there is no `.cpp` to define them).

### Resolve-only inputs (`--include`)

`--src` and `--include` now separate the two roles the input list used to conflate. `--src` files are
transpiled; `--include` files (files, directories, or globs, like `--src`) are added to the resolution
scope so the `--src` files' native references resolve and their `@:include` headers propagate, but are
**never emitted**. This makes native-stub boundaries explicit and keeps them out of a `--header-only`
amalgamation. Backward compatible: `extern` stubs passed via `--src` are still not emitted.

### Fixes

- **Null-safe navigation with null-coalescing.** `recv?.method() ?? default` (and `recv?.field ??
  default`) on a pointer receiver was lowered to a discardable comma form that evaluated the call but
  yielded `0`, throwing the navigated value away — so every such read returned the default. It now
  lowers to the value form `(recv != NULL ? recv->method() : default)`. Surfaced by the anachrjsonistic
  `Proxy` accessors, whose `(this?.isObject() ?? false)` guards now read back correctly.

### Validation

- New self-contained compile-and-run gates: the `--header-only` amalgamation (including cross-module
  ordering and the cycle diagnostic), `--include` resolve-only emission, and the null-safe/coalesce
  lowering. The standalone [anachrjsonistic](https://github.com/andrewglind/anachrjsonistic) library was
  re-transpiled as a single header and verified to parse and read values correctly under
  `g++ -std=c++98`.
- The in-repo `examples/shapes` demo was removed; anachrjsonistic is the end-to-end showcase, and the
  test suite's own temp-dir compile-and-run gates remain the in-repo C++98 validation.

## v0.2.0 — Milestone 10: Abstract types (2026-06-18)

This release makes Haxe **`abstract` types** a first-class lowering target: zero-overhead value
types that carry methods, operators, and conversions, and lower to idiomatic C++98 with no heap, no
vtable, and no runtime wrapper. On top of that foundation it adds **`@proxy`**, a single construct for
binding native C++ classes — both the ones you *call into* and the ones you *subclass*. Together these
let a real, hand-written C++ library be re-expressed in Haxe and transpiled back to equivalent C++
(see *Validation*).

### Highlights

- **`abstract Name(U)` newtypes** — value types with methods over an underlying value, emitted as a
  flat C++ value class.
- **Operator overloading** via `@:op` — subscript `@:op([])`, binary `@:op(A op B)`, and prefix-unary
  operators.
- **Implicit conversions** — `@:to` lowers to C++ conversion operators, `@:from` to converting
  constructors.
- **Value-type composition** — recursive-by-value trees and mutually-referential (cyclic) types in one
  module, with automatic forward declarations and out-of-line definitions.
- **`@proxy("native::Name")`** — one metadata for native interop, covering both consumed handles and
  produced (subclassed) native bases.
- **`cpp.Pointer<T>` / `cpp.StdString`** interop types.

### Abstract types

A Haxe `abstract Name(U) { … }` now lowers to a C++ value class that wraps the underlying `U` and
forwards its methods, with `this` denoting the underlying value. There is no allocation, no pointer,
and no vtable — an abstract is a true zero-cost newtype.

- **`@:op` operator overloading** (on abstract methods) emits the corresponding C++ `operator`:
  - `@:op([])` → `operator[]` (overloadable by argument type, e.g. a string-keyed and an int-indexed
    subscript on the same type);
  - `@:op(A + B)` and the other binary operators → `operator+`, etc.;
  - prefix unary `@:op(-A)` / `@:op(!A)` / `@:op(~A)`.
- **`@:to`** (on an abstract method) → an implicit C++ conversion operator (`operator int()`,
  `operator std::string()`, `operator SomeClass()`, `operator std::vector<T>()`, …).
- **`@:from`** (on a static abstract method) → a converting constructor.

This retires the experimental `@value` tag — value-types-with-methods are now expressed in plain Haxe
as `abstract` newtypes, with no Hatchet-specific metadata.

### Value-type composition: recursion and cycles

Abstracts and classes can now be composed by value in the shapes real libraries need:

- **Recursive-by-value trees** — a value type that holds a container of itself (`Array<Self>`) composes
  and is queried entirely by value, with no `new`/`delete` and no vtable pointer.
- **Mutually-referential (cyclic) types** — two types that each return the other by value (the classic
  `jobject`/`proxy` cycle) can live in a single module; Hatchet emits a forward declaration and moves
  the offending inline definition out-of-line after both types are complete — exactly what a
  hand-written header does to break the cycle.

These compose with the ownership model: `@sink` parameters transfer ownership across a retaining
setter, so a value handed to a method that stores it is freed exactly once, by the owner.

### Native interop: `@proxy`

A new `@proxy("native::Name")` metadata binds a Haxe glue type to a native C++ class it is **never
emitted for**. The fully-qualified native name is mandatory and must match a declared `extern`. Two
forms, by declaration shape:

- **Consume** — `@proxy(...) abstract Name(cpp.Pointer<T>)`: a transparent handle. Every reference
  transpiles *as* the native type and calls pass straight through (`h.Method()` → `h->Method()`).
- **Produce** — `@proxy(...) abstract class Name`: a base your code subclasses. `extends Name` emits
  `: public native::Name` and `super(...)` routes to the native constructor — the supported way to
  subclass a native C++ base (which hxcpp itself cannot do).

Supporting interop types: **`cpp.Pointer<T>` → `T*`** and **`cpp.StdString` → `std::string`**. Misuse
is caught up front: a missing argument, an unmatched native name, or `@proxy` on anything but an
`abstract` / `abstract class` is a hard error.

(`@proxy` supersedes the short-lived experimental `@facade` / `@router` names, which were never part of
a release.)

### Diagnostics

The fail-loud validation pass gained coverage for the new surface — `@proxy` misuse and unsupported
`abstract` / operator forms are reported with actionable messages rather than emitting subtly wrong
C++.

### Fixes

- **Free-function double-delete.** A top-level free function whose body ended in a tail `return` emitted
  its owned-local `delete`s twice (once before the return, once as dead code after it). The
  closing-brace cleanup is now skipped after a tail return, consistent with methods.

### Validation

- The standalone [anachrjsonistic](https://github.com/andrewglind/anachrjsonistic) JSON library was
  re-implemented in Haxe and transpiled with Hatchet — the end-to-end exercise for abstract types. The
  same source compiles under both hxcpp and `g++ -std=c++98`, and the transpiled library parses and
  reads values identically to the original C++.
- New compile-and-run gates in the test suite: abstract operators/conversions, recursive value trees,
  value-position `switch`, and owned / forward-declared cyclic types.

### Upgrade notes

- **No breaking changes to released APIs.** `@value` is retired in favour of `abstract` newtypes; the
  experimental `@facade` (never released) is now `@proxy` with a mandatory native-class argument.

## v0.1.0

Initial release: the Haxe 4.x → C++98 transpiler core — lexer, recursive-descent parser, typed AST,
semantic model, and C++98 code generator — with the bundled `examples/shapes` compile-and-run gate.
