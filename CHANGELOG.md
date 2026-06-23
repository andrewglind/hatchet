# Changelog

All notable changes to Hatchet are documented here. Versions follow the
project's milestones.

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
