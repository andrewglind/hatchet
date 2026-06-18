# Changelog

All notable changes to Hatchet are documented here. Versions follow the
project's milestones.

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
