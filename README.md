# Hatchet

**Hatchet** is a transpiler from [Haxe](https://haxe.org) 4.x to **C++98** — portable source that
compiles under **Visual C++ 6.0**, and therefore targets legacy platforms such as **Windows 9x** and
older Unix toolchains. It is a *transpiler*, not a compiler: it emits C++ source you then build on the
target, and it never produces a custom C++ runtime. Supported Haxe constructs map to an equivalent,
hand-writable C++ idiom. Hatchet implements a focused subset of Haxe 4.x; it is **not** a drop-in for hxcpp.

> **hxcpp compatibility is compile-time only.** A guiding principle of Hatchet is that the Haxe you
> write always *compiles* under hxcpp (Haxe's official C++ backend), so the source stays valid,
> portable Haxe you can keep editing and type-checking with normal Haxe tooling. Hatchet makes **no
> guarantee that the hxcpp build runs** or behaves identically — the supported, authoritative runtime
> is the **C++98 that Hatchet emits**. The two targets can diverge at runtime (most notably value vs.
> reference semantics: a Hatchet value class / `abstract` is a flat value, while under hxcpp the same
> type may be a heap object). Validate behaviour on the transpiled C++98, never on an hxcpp build.

## Motivation

hxcpp (Haxe's official C++ backend) cannot target C++ revisions older than C++11, which has
traditionally put Haxe 4.x out of reach for retro and embedded platforms — Windows 98 + VC6, early
Linux, and similar. Hatchet bridges that gap: develop in Haxe on a modern machine, transpile to
C++98, then copy the generated `.h`/`.cpp` to the target and build them with the old toolchain.

## Status

Hatchet is a working transpiler with a real lexer, recursive-descent parser, typed AST, semantic model, and C++ code generator.

A bundled [standalone example](examples/shapes) is transpiled, compiled under `g++ -std=c++98`, run,
and output-checked by the test suite (see *Validation*), and the generated output has been **built
with Visual C++ 6.0 and run on Windows 98** — the primary target — closing the loop from Haxe source
to a running legacy binary. Hatchet has additionally been validated against a larger, real C++ game engine.

Supported today, end to end:

- **Declarations** — classes, interfaces (pure-virtual), enums (pre-C++11 `struct E_ { enum … }`;
  a **parameterized enum** — `Add(a:Int, b:Int)` — lowers to the tagged-value idiom: the same tag
  enum plus a copyable value class with per-variant payload fields and inline static factories,
  `Op::Add(1, 2)`, so ADT values construct, pass, and store by value with no heap or union, plus
  **structural `==`/`!=`** — same tag, equal payload, with pointer payloads compared by address
  (Haxe compares enum values by constructor + arguments via `Type.enumEq`); a
  *recursive* payload is flagged — a by-value class cannot contain itself),
  `enum abstract` (an `Int` backing reuses the enum idiom with explicit member values, including
  sibling-referencing bit-flag expressions like `AB = A | B`; a `String`/`Float` backing becomes a
  namespace of typed `static const` constants, `namespace X_ { static const std::string A = "…"; }`,
  with the type mapping straight to its underlying C++ type), typedef structs and aliases, the
  fixed-width `UInt8/16/32` shims, and `extern` interop wiring (which emits no code of its own,
  only the includes it contributes). **Mutually-recursive classes** in one module need no manual
  ordering: Hatchet emits a **targeted forward declaration** (`class B;`) for any class/interface/ADT
  it defines that is referenced before its own definition — and only those, never `extern` types.
  (A forward declaration uses the type's emitted name, so a `@:native`-renamed type is declared under
  its renamed name.)
  Since Hatchet classes are reference types (every cross-class member/param/return is a pointer), a
  forward declaration is always sufficient, so cyclic type graphs "just work".
- **Value classes** — a class can be emitted as a C++ **value type** rather than a reference type:
  `new Foo(a)` becomes a stack value `Foo(a)` (no heap), a field/`Array<Foo>` is held by value
  (`Foo`/`std::vector<Foo>`), member access is `.`, the destructor is non-virtual (no vtable — a flat
  value layout), and the ownership analysis never touches it (nothing to free). This gives a **value
  object with methods** — the hand-written-C++ idiom of a small struct-with-behaviour — letting
  value-heavy code (vectors, JSON-style trees) transliterate by value instead of being reimplemented
  on the heap. A value type can't do C++ polymorphism, so inheritance is rejected (slicing), as is a
  field holding the class itself by value (an incomplete type — use an `Array<Self>`); `Null<Foo>`
  still boxes to `Foo*`, as for any value type. Two tags select a value class: the real hxcpp
  **`@:stackOnly`** metadata makes one that also obeys hxcpp's stack-residence rule — it may *not* be
  nested as a field or element (Hatchet flags that, steering to an `abstract`) — suiting genuine
  stack-only locals while staying portable to hxcpp; and an **`abstract Name(U)`** newtype (next
  bullet) is a value type that **nests freely**, the portable way to get a value object with methods,
  operators, and conversions. See the **Metadata** section for the full `@:` vs `@` split.
- **Abstract types (`abstract Name(U)`)** — Haxe's newtype: a **value type with methods** over an
  underlying type `U`, lowered to a value class that wraps `U` in a synthetic `__this` field. Inside
  its methods Haxe `this` *is* the underlying value (so `this = v` and `this.field` operate on `U`),
  and `new Name(...)` is value construction. This is the portable, idiomatic way to get a value
  object with operators and conversions, and the nestable value type (a field, an `Array<Name>`, or a
  recursive-by-value tree through a container). Supported member metadata:
  - **`@:op(...)`** operator overloading, emitted as a C++ operator that forwards to the named
    method (so the value reads as `v[k]` / `a + b` *and* `v.method(...)`, with C++ doing the
    operator resolution): `@:op([])` (one-arg read) → `operator[]`, `@:op(A op B)` → the binary
    `operator op`, and prefix unary `@:op(-A)` / `!A` / `~A`. Forms with no C++98 mapping — the
    two-arg `@:op([])` write, `@:op(a.b)`, `@:op(a())`, postfix — are flagged as unsupported.
  - **`@:to`** → an implicit C++ conversion operator (`@:to function toStr():String` →
    `operator std::string()`), and **`@:from`** (a static factory) → a converting constructor, so the
    source type implicitly converts to the abstract.

  Generic abstracts and `@:multiType` are not supported; an `abstract`'s value-class nature means the
  usual value-class caveats (no polymorphism; nests freely; `Null<Name>` boxes to `Name*`).
- **Members & access** — a Haxe `private` member maps to C++ **`protected`**, not `private`: Haxe
  `private` is accessible from subclasses (and Haxe has no "private even from subclasses" concept),
  so emitting C++ `private` would reject an inherited-member access that Haxe accepts. Hatchet
  therefore never emits C++ `private` — hidden members are `protected` (still closed to outside code,
  open to subclasses, matching Haxe). Property accessors: every pair of `default`/`null`/`never`
  (pure access control — lowered to direct field access, the backing field hidden as `protected`
  behind a generated `GetX` for `(default,null)`); **custom accessors with
  real Haxe routing** — a user-written `get_x`/`set_x` is emitted as a real method and **every
  access routes through it**, external and internal alike, except inside the property's own
  accessors (Haxe's recursion rule): reads become `get_x()`, writes become `set_x(v)` —
  including constructor writes, and compound writes/`++`/`--`, which desugar exactly as Haxe does
  (`x += v` → `set_x(read + v)`, with a side-effecting receiver hoisted so it evaluates once); an
  accessor whose signature omits its return type gets the property's type, as Haxe infers it
  (so `function set_x(x:Float) { return this.x = x; }` is a `double`-returning function, not a
  value `return` from void);
  per Haxe physicality, a non-`@:isVar` `(get,never)` emits no backing field at all. A `set`
  property *without* a `set_x` keeps the Hatchet dialect: an auto-generated trivial `SetX` (with
  the value-vs-pointer `const` rule) and direct internal writes. For owned pointer fields behind a
  custom setter, the setter's direct store is the single delete-before-overwrite site (routed
  callers never also free), and a setter that `return`s the field reads to the escape analysis as
  the value being handed out — the field then leans borrowed (leak over double-free, the standard
  bias; `@owned` opts the destructor in). `(get, default)` and `dynamic` access remain flagged as
  unsupported. Also `@:decl` (DLL-export
  class), `@:overload(...)` (a call is resolved to the matching C++ overload by argument type, else a
  hard error), `@:abi` (`extern "C"` export via a portable macro), `abstract class` and
  `abstract function` (an abstract method becomes a pure virtual `virtual T f() = 0;`, declared and
  never defined), and the base-from-member `Holder` idiom for constructors whose `super(...)` is not
  the first statement.
- **Statements & expressions** — `super(...)` initializer lists; `.`-vs-`->` selection via type
  inference (including inherited fields); anonymous-struct-to-temporary expansion (typed, untyped,
  nested); array / map / object literals; `Array`→`std::vector` and `Map`→`std::map` with their
  container ops — Array `push`/`insert`/`pop`/`shift`/`unshift`/`length`/`indexOf`/`lastIndexOf`/
  `contains`/`remove`/`reverse`/`concat`/`slice`/`copy`/`join`/`filter`/`sort` (the last two take a
  lambda: `filter` builds a kept-elements vector, `sort` is an in-place insertion sort driven by the
  comparator) and Map `get`/`exists`/`set`/`remove`/`keys` (each an inline loop/expression, no
  `<algorithm>` dependency); containers are **value types** in the generated C++ — Haxe's are
  shared by reference, and mutating an `Array`/`Map` *parameter* is linted at the Haxe line (see
  **Container semantics** below for the full divergence and the idiomatic patterns); Haxe's
  **auto-extending array
  writes** — `a[i] = v` past the end grows
  the vector first (an inline `resize`), matching Haxe rather than letting C++ `operator[]` run off
  the end; `for` over a range, an array, an anonymous array literal
  (`for (i in [1,2,3])`), or a map (`for (v in m)` over values, `for (k => v in m)` over key/value
  pairs, via a `std::map` iterator); array & map comprehensions; **module-level functions**
  (`function f(...) {...}` → a namespace free function, public ones declared in the header, `private`
  ones `static` in the `.cpp`); closures inlined into a loop for
  `Array.map`/`filter`/`sort`, or lowered to free functions for a top-level `final` binding (an arrow
  param's type may be left off and taken from the binding's function-type annotation —
  `Cross:(Vec, Vec) -> Float = (a, b) -> …` types `a`/`b` as `Vec`); `String` methods
  (`charAt`/`charCodeAt`/`indexOf`/`lastIndexOf`/`substr`/`substring`/`toUpperCase`/
  `toLowerCase`/`split`), `String.fromCharCode`, `StringBuf` (an `add`/`addChar`/`toString`
  accumulator over `std::string`), and the `StringTools` statics
  (`replace`/`trim`/`startsWith`/`endsWith`/`hex`) — all mapped to `std::string` expressions; string
  interpolation and `+` concatenation (built as a `std::string` — text appended directly and numeric
  operands formatted into type-bounded buffers, so no value-guessed buffer can overflow; interpolation
  also supports the `$ident` shorthand); the `??` null-coalesce and
  NULL-guarded `?.`; `cast` (C-style cast for `cast(expr, T)`, passthrough for `cast expr`); the
  `(expr : Type)` type ascription (a compile-time hint that drives inference, e.g. `([] : Array<Int>)`);
  `switch` (on an integer/enum subject → a C++ `switch`; on a `String` subject → an `if`/`else if`
  chain, since C++ case labels must be integral) with **constant patterns** — literals, negated
  numeric literals, and enum constants (bare or `EnumType.Member`-qualified), with the wildcard
  `case _:` lowering to `default:`, comma alternatives (`case A, B:`), and the or-pattern
  (`case 1 | 2:` — in pattern position `|` means *or*, exactly as in Haxe, and lowers to two case
  labels), and **destructuring of parameterized enums** — `case Add(a, b):` switches on the tag and
  binds one typed local per non-`_` capture from the variant's payload fields, with a
  side-effecting subject hoisted so it evaluates once (a destructuring pattern must be its case's
  only alternative, payload positions take only plain captures or `_`, and a bare capture pattern
  `case x:` stays flagged rather than emitted as a broken label). **Haxe's `break` semantics are
  preserved**: Haxe `switch` has no break of its own, so a `break` in a case body exits the
  enclosing *loop* — inside a generated C++ `switch` it routes through a hoisted flag checked
  after the switch (`f = true; break;` … `if (f) break;`), chaining through nested switches, while
  a `break` in a loop nested *within* a case body stays bound to that inner loop (`continue`
  needs no help — C++ gets it right natively) — including a `switch` used
  in **value position** (`var x = switch (e) { … }`), which desugars to a hidden temporary assigned
  inside a statement `switch`; `trace(...)` (with `--no-traces` to strip it); `throw` / `try` / `catch`
  exception handling (a thrown `String` is coerced to `std::string`; a typed `catch (e:T)` maps the
  exception type; an untyped/`Dynamic` catch becomes the non-binding `catch (...)` — so it may catch but
  cannot *use* the value: referencing the caught name there is a hard error, since C++ `catch (...)`
  binds nothing; see the ownership note below for unwind behaviour); and the `Math` / `Std` /
  `Sys` intrinsics (`Std.int`/`Std.string`/`Std.parseInt`/`Std.parseFloat`/`Std.random` → inline
  `(int)`/`sprintf`/`strtol`/`atof`/`rand()`).
- **Conditional compilation & escape hatches** — Haxe `#if FLAG` / `#elseif FLAG` / `#else` / `#end`
  map to the C++ preprocessor (`#ifdef` / `#elif defined(FLAG)` / `#else` / `#endif`), and **boolean
  conditions over flags** map each flag through `defined(…)` (`#if (DREAMCAST && !DEBUG)` →
  `#if (defined(DREAMCAST) && !defined(DEBUG))`; version comparisons like `haxe_ver >= 4` have no
  `defined(…)` mapping and stay a hard error); `untyped
  <expr>` passes an expression through to C++
  verbatim; a statement-level `@:include("…")` emits an `#include` at that point; and
  `@:cppFileCode('…')` injects verbatim C++ in a body.
- **Types & nullability** — `Float` lowers to C++ `double` (64-bit, matching Haxe's `Float` on every
  official target — never `float`, which would silently halve the precision); genuine
  single-precision is available as `Single` or hxcpp's `cpp.Float32` → C++ `float` (and
  `cpp.Float64` → `double`), and Haxe's division
  semantics are preserved: `/` always yields `Float`, so two statically-known-integer operands divide
  as `double` (`a / b` → `((double)(a) / b)`; `Std.int(a / b)` truncates back, as in Haxe); `%` with
  a float operand lowers to `fmod` (C89 `<math.h>`, portable to VC6 — C++ `%` is integer-only); the
  full shift set including the unsigned `>>>` / `>>>=` (no C++ spelling — lowered through an
  `unsigned int` cast, `(int)((unsigned int)a >> b)`); `Null<T>` and optional value-structs lower
  uniformly to `T*` (with matching heap-allocation at call sites); `Map.get(k)` lowers to an iterator
  with an existence check; `final` constants lower to namespace-scoped `static const` (no `#define`),
  namespace-qualified across boundaries.
- **Memory ownership** — a whole-program **escape/ownership analysis** decides what each class frees,
  erring toward a leak (safe) over a double-free: destructors free what a class `new`ed (and the typed
  pointer handed to a base's `void*` field); short-lived heap locals are freed at scope close, and
  **before every early `return`**; owned pointer fields are NULL-initialized, freed before reassignment,
  and freed in the destructor; owned containers are walked and freed element-by-element. Borrowed
  dependencies, value containers, objects owned by a receiver, and fields handed back out of the object
  are left alone. A field reference is recognized whether written `this.field` or bare `field` (Haxe
  lets you omit `this.`), so ownership does not hinge on the qualifier. For an **injected** pointer the
  class stores but did not `new` — where own-vs-borrow is not statically decidable — mark the field
  **`@owned`** to have the destructor free it (a scalar with `delete`, a container element-by-element);
  the local-scope counterpart **`@delete var x = …`** frees a marked local at scope close. Unmarked
  injected pointers stay borrowed. These overrides are obeyed but **advisory-checked** — the analysis
  warns when a tag looks unsound (e.g. an `@owned` field that is also handed out); auto-inferring an
  injected pointer's ownership would need interprocedural call-site analysis and is a future improvement.
  A `new` passed to a constructor parameter the class owns is emitted **inline** — the constructed object
  frees it — rather than hoisted into a scope-owned local that would double-free it. A **method/function
  parameter** can be marked **`@sink`** — **`function setKey(key, @sink val:JValue)`** — a *consuming*
  parameter that takes ownership across the call (distinct from `@owned`, which says a *field* frees its
  member when the object dies; `@sink` says ownership *transfers in* at this call): a `new` handed to a
  `@sink` position is emitted inline, and an owned local handed there has its scope-close free dropped,
  so the value is freed once (by the receiver) rather than dangled by the caller. This is the explicit
  answer to a retaining method (`store(x)` that keeps `x`), which an intraprocedural analysis cannot
  otherwise see; `@sink` on a by-value parameter (where there is nothing to consume) is flagged as a
  no-op. A `new` pushed into a container that **escapes** the scope — a class-owned field container, or
  a local container that is returned/stored — likewise comes to rest there and is not freed locally. **On exception
  unwind** (a `throw` inside a `try`), the scope-close frees do not run, so owned objects created in
  the `try` before the throw **leak** — a deliberate extension of the conservative bias (never a
  double-free/use-after-free); free them in the `catch` if it matters. (Exceptions must be enabled on
  the target — VC6 `/GX`; g++ enables them by default.)

Hatchet **fails loudly rather than guessing** — an unresolvable type or an unsupported idiom is a
hard error that skips that module and fails the run (see *Diagnostics*) — and it **always generates
the `StdAfx.h` prelude**, so a standalone project compiles with no boilerplate.

## Container semantics: `Array` and `Map` are value types

This is Hatchet's **largest deliberate divergence from Haxe**, so it gets its own section. In Haxe,
`Array` and `Map` are objects — every binding is a *reference* to one shared container, so a
mutation made through any of them is visible through all of them. Hatchet lowers them to
`std::vector` / `std::map` **by value**: assignment and parameter passing **copy** the container.
There is no shared-container runtime to lean on (that is the point of targeting bare C++98), so the
divergence shows up in four places:

1. **Parameters.** Containers are passed `const&`. Mutating a parameter (`items.push(x)`,
   `items[i] = v`, `tags.set(k, v)`, `tags.remove(k)`, in-place `sort`/`reverse`/…) is the
   classic Haxe idiom for filling a caller's list — and it is exactly what the value lowering
   cannot express. Hatchet **lints every such mutation** at the Haxe line, ahead of the C++
   `const` error:

   ```text
   warning: World.hx:31: fill: `push` mutates `items`, an Array parameter — Haxe containers
   are shared by reference (the caller would see this change), but Hatchet passes containers
   by value (`const&`), so the mutation is lost and the generated C++ will not compile; …
   ```

2. **Local aliases.** `var b = a; b.push(x);` copies in Hatchet — `a` is unchanged, where Haxe
   would mutate the one shared array. This is *not* linted (a local working copy is usually the
   intent in retro-target code, and Hatchet's `copy()`-free spelling of it is idiomatic here);
   write `var b = a.copy();` if you want the copy to be visible in the Haxe semantics too.

3. **Fields.** `this.items = items;` stores a copy into the field, where Haxe would store a
   reference to the caller's container. Later mutations of the field are the class's own; later
   mutations of the original do not reach the field.

4. **Returns.** Returning a container returns a copy. Mutating a returned container does not
   affect the one the function read from.

The idiomatic Hatchet patterns, in preference order:

- **Hold the container in a class and pass the object.** Classes *are* reference types (objects
  are pointers), so a `Roster` class owning an `Array<Unit>` gives you Haxe-style shared mutation
  through the object — `squad.add(u)` works from anywhere, one container, no copies:

  ```haxe
  class Roster {
      public var units(default, null):Array<Unit>;
      public function new() { units = []; }
      public function add(u:Unit):Void { units.push(u); }
  }
  ```

- **Return the result** instead of mutating an argument: `function doubled(xs:Array<Int>):Array<Int>`.
- **Mutate a local copy** when a working copy is genuinely what you want.

Conversely, when value semantics are what you *want* — small objects copied freely, composed by
value, no heap — an **`abstract Name(U)`** (see *Declarations* above) is the deliberate tool:
it carries methods like any class but behaves like a `struct`, matching the value-object idiom of
hand-written C++.

A future evolution may pass mutated container parameters by non-const reference (restoring Haxe
semantics at the parameter boundary); until then, the lint is the contract.

## Metadata

Hatchet reacts to two kinds of metadata, kept deliberately separate. (Internally Hatchet matches
metadata by **name** and ignores the `@:`/`@` prefix, but the prefix matters to *other* Haxe
targets, so write each tag in the form shown.)

**Haxe / hxcpp compiler metadata Hatchet honours (`@:…`)** — real metadata whose meaning Hatchet
matches, so the same source stays valid and keeps compiling under hxcpp:

| Tag | Effect in Hatchet |
|-----|-------------------|
| `@:native("a::b::Name")` | Rename the emitted C++ symbol (and namespace) — the type/function is still Hatchet's to emit, just under that name. Orthogonal to `extern` |
| `@:include("p.h")` / `@:include("<h>")` | Emit an `#include` (quoted for a project header, angle-bracketed verbatim for a system one) |
| `@:cppFileCode('…')` | Inject verbatim C++ at that point in a body (also real hxcpp) |
| `@:overload(function(...){})` | Resolve a call to the matching C++ overload by argument type, else a hard error |
| `@:isVar` | Force a physical backing field for a property (so `(get,never)` keeps storage) |
| `@:decl` | Export a class from a DLL via the portable export macro |
| `@:abi` | (on a module-level function) export it across a C ABI: emitted as an `extern "C"` global at file scope with the portable export/calling-convention macros (`HATCHET_EXPORT`/`HATCHET_CALL`) |
| `@:op(...)` | (on an `abstract` method) operator overloading → a C++ operator forwarding to the method: `@:op([])` read → `operator[]`, `@:op(A op B)` → binary `operator op`, prefix unary `@:op(-A)` |
| `@:to` / `@:from` | (on an `abstract` method) `@:to` → an implicit conversion operator; `@:from` (static) → a converting constructor |
| `@:stackOnly` | A **value class** that also obeys hxcpp's stack-residence rule — may **not** be nested as a field/element (flagged, steering to an `abstract`). Portable; use for genuine stack-only value types |

**Interop — the `extern` keyword.** A type whose implementation lives in hand-written C++ is declared
**`extern`** (`extern class` / `extern interface` / `extern enum`, all valid Haxe): Hatchet emits no
definition for it, only type-checks and references it, and pulls its `@:include` into anything that
uses it. This is separate from `@:native`, which *only renames* — combine them to bind to a specific
C++ name (`extern @:native("jobject") class JObject`). A typedef cannot be `extern` in Haxe, so a
`@:native typedef` naming an engine struct is the way to declare an external *value* struct (the one
case where `@:native` still implies "external").

**Hatchet directives (user metadata, `@…`)** — Hatchet's own. The guiding rule: a user-metadata tag
exists **only for a C++ reality Haxe genuinely cannot express** — now that value-types-with-methods
are `abstract` newtypes, that is just **manual memory ownership**. Anything Haxe *can* say — operators,
casts, value types via `abstract`, external types via `extern`, the C-ABI export via `@:abi`, access
levels — is expressed in real Haxe, not invented here. Because these are plain user metadata, every
other Haxe target ignores them, so the source stays portable.

| Tag | Effect |
|-----|--------|
| `@owned` | A field the owning object frees in its destructor (a scalar via `delete`, a container element-by-element) |
| `@sink` | A *consuming* parameter — the callee takes ownership of what is passed (caller does not free); the call-site counterpart to an owning field |
| `@delete` | Free a marked local (`@delete var x = …`) at scope close |

**Portability note.** Both paths *compile* under hxcpp and Hatchet (the compile-time guarantee); only
their **runtime** representation can differ, and Hatchet's emitted C++98 is the authoritative runtime
(see the note at the top of this README). The reference-semantic path — plain `class` plus
`@owned`/`@sink`/`@delete` — even behaves the same under both (hxcpp's GC ignores the ownership tags;
Hatchet's analysis uses them). The value-semantic path — `abstract Name(U)` and `@:stackOnly` — is a
flat value type under Hatchet, but under hxcpp its representation depends on the underlying (an
anonymous-structure underlying is a heap object there), so copy-vs-share behaviour can diverge.
Validate such behaviour on the transpiled C++98.

## Requirements

- **Rust** (stable; install via [rustup](https://rustup.rs))
- A **C++98 toolchain** to build the generated output. The development validation gate uses
  `g++ -std=c++98`; the production target is Visual C++ 6.0 up.

## Building

```powershell
cargo build            # debug binary at target/debug/hatchet
cargo build --release  # optimized binary at target/release/hatchet
cargo test             # unit tests, header/body codegen checks, and the bundled-example compile+run gate
```

## Usage

```bash
# Transpile a whole project — point --src at its root directory; Hatchet crawls it
# recursively for .hx. The C++ namespace of each file follows its Haxe `package`, and
# the project root is inferred from that package.
hatchet --src path/to/project --out path/to/output --force

# A glob works too (expanded by Hatchet itself, so quote it on shells that would
# otherwise expand it). Mix files, dirs, and globs freely:
hatchet --src modules/*.hx --out path/to/output --force

# Transpile a single file — pass its dependencies too (superclasses, native stubs),
# since the listed sources are the entire resolution scope:
hatchet --src game/Scene.hx modules/Module.hx --out out

# Preview on stdout, or validate without writing anything:
hatchet --src game/Scene.hx modules/Module.hx --stdout
hatchet --src . --dry-run

# Run interactively (prompts for a source and a target dir) when --src is omitted:
hatchet
```

`--src` accepts any mix of **single `.hx` files, directories (crawled recursively for `.hx`), and
globs** (`*`, `?`, `**` — e.g. `modules/*.hx` or `src/**/*.hx`). Globs are expanded by Hatchet itself,
so quoting them to bypass shell expansion works. The full expanded set is also the **entire resolution
scope**, so a file's dependencies (superclasses, `extern` stubs) must be reachable in it —
crawl the project root to pull everything in. Each file's **project root** — the base for the output
layout and relative includes — is inferred from its `package` declaration (the file's directory minus
its package path).

| Flag | Description |
|------|-------------|
| `--src, -s <PATH>...` | Haxe sources to transpile — any mix of `.hx` files, directories (crawled recursively), and globs (`*`/`?`/`**`); prompted if omitted. Also the full resolution scope |
| `--out, -o <DIR>` | Output directory (defaults to the inferred project root; ignored with `--dry-run`/`--stdout`). Generated files mirror the source package layout; includes that point at external dependencies (a native engine, a sibling project) are re-pointed at the dependency's real location when needed, so `--out` resolves from any directory |
| `--force` | Overwrite existing generated files (ignored with `--dry-run`) |
| `--dry-run` | Transpile and report info/warnings/errors only — write nothing. Takes precedence over `--stdout`/`-o`/`--force` |
| `--stdout` | Write generated C++ to stdout instead of files (status goes to stderr) |
| `--stdafx <NAME>` | Stem of the prelude source/header (default `StdAfx` → `StdAfx.h`; e.g. `MyGame` → `MyGame.h`) |
| `--export-macro <PREFIX>` | Prefix for the portable DLL-export macros wrapped around `@:abi` functions (default `HATCHET` → `HATCHET_EXPORT`/`HATCHET_CALL`/`HATCHET_CLASS`) |
| `--depth <N>` | Max expression-nesting depth at which a buried `Null<T>` call is auto-extracted into a freed local instead of warned about (default `1`; e.g. `2` auto-extracts `if (GetEdge(e) == null)`) |
| `--no-traces` | Strip all `trace(...)` calls from the generated C++ (lowered to no-ops, arguments not evaluated), mirroring hxcpp's `-D no-traces` |

A `Main.hx` is never transpiled — it is treated as the hxcpp entry point only.

## Architecture

```
discover → lex → parse → semantic analysis → code generation
```

Source layout (`src/`):

| Module | Responsibility |
|--------|----------------|
| `main.rs` / `cli.rs` | CLI parsing, interactive prompts, the top-level driver |
| `discover.rs` | Find `.hx` files; package/path helpers |
| `lexer.rs` | Haxe tokenizer (`'${...}'` interpolation, `1...6` ranges, `@:meta`, etc.) |
| `ast.rs` | Typed AST for the supported Haxe subset |
| `parser.rs` | Recursive-descent + precedence-climbing parser |
| `sema/` | Symbol table, Haxe→C++ type & namespace mapping (`types.rs`), `@:include` resolution (`includes.rs`), pre-codegen validation (`validate.rs`), and the whole-program escape / ownership analysis (`escape.rs`) |
| `codegen/` | C++ generation: `mod.rs` (headers), `source.rs` (`.cpp` bodies), `holder.rs` (base-from-member idiom), `ownership.rs` (destructor delete emission, driven by `sema/escape.rs`) |
| `stdafx.rs` | `StdAfx.hx` → `StdAfx.h`, and the generated standard-library prelude |
| `scan.rs` | Small comment-aware scanning helpers |
| `diag.rs` | Diagnostics (`error:` / unsupported-feature reporting) |

## Diagnostics

Hatchet **fails loudly rather than guessing.** When it cannot resolve a type — a typo, a missing
`import`, or a type declared outside the `--src` scope — it reports an error and does not generate
that module, instead of silently emitting wrong C++ (e.g. a class rendered by value instead of as a
pointer). Errors are collected across all files and reported together; modules that transpiled
cleanly are still written, and the run exits non-zero:

```text
error: Scene.hx:14: unresolved type `IEngine` in parameter `engine` of `new` — is it declared and within the --src scope?

Generated 6 file(s); 1 module(s) skipped due to errors.
hatchet: 1 error(s); 1 module(s) were not generated
```

The same discipline applies to **unsupported Haxe idioms**: valid Haxe that Hatchet does not yet
transpile fails with an invitation to contribute upstream (the repository URL is in `src/diag.rs`).
This distinguishes "your input is wrong" (fix the Haxe) from "Hatchet doesn't do this yet" (raise a
PR). Currently flagged as unsupported: a **lambda** used outside a top-level `final` binding or an
`Array.map(...)` argument; **Haxe macros** (a `macro` function, or the macro AST type `Expr`);
**regular expressions** (both the `~/pattern/flags` literal and the `EReg` type); **`using` static
extensions**; **function types as values** (`var cb:Int->Int` in a field, parameter, return, or
typedef — first-class function values have no C++98 lowering; the one lowered position is a
top-level `final` lambda binding, which becomes a free function); **rest parameters**
(`...vals:Int` and the `haxe.Rest` type, Haxe 4.2 varargs); **recursive enum payloads**
(`Node(child:Tree)` — a by-value tagged class cannot contain itself); the **`is`** runtime type-check
operator (Haxe 4.2); **generics** (a type-parameterized `class Box<T>`, `interface I<T>`,
`enum Tree<T>`, generic method `first<T>(…)`, or `typedef Pair<T>` — type parameters have no C++98
template lowering, so each is flagged rather than emitted with `T` as a bare unknown type);
**non-constant `switch` patterns** (a capture `case x:`, a literal or nested payload sub-pattern in
`case Add(0, b):`, or a destructuring pattern combined with other alternatives in one case);
**`(get, default)` properties and `dynamic`
access** (every other accessor pair is lowered — see *Members & access* above — and a stray
`get_x`/`set_x` whose field does not declare the matching access kind is flagged rather than
silently dropped); **generic / `@:multiType` abstracts** (a plain `abstract X(T)` newtype *is*
supported — see *Declarations* — but a type-parameterized or multitype one is not); and **`@:op`
forms with no C++98 operator** (the two-arg `@:op([])` write, `@:op(a.b)`, `@:op(a())`, postfix).
These are parsed but not transpiled, so they are reported with a clean diagnostic rather than a
parse error.
Relatedly, a `for` loop over anything other than a range,
an `Array`, or a `Map` (a custom `Iterator`/`Iterable`) is a hard error rather than a guess.

## Standalone projects and the prelude

Hatchet transpiles a **standalone** project — plain Haxe with no `extern` interop layer —
with no special setup: cross-file types resolve, a type used without an explicit `import` (legal for
same-package Haxe) still has its header pulled in, and the **standard-library prelude is generated
automatically**. Hatchet owns that prelude — it knows which headers its supported idioms need
(`NULL`/`<stdlib.h>`, `sprintf`/`<stdio.h>`, `<math.h>`, `std::string`/`std::vector`/`std::map`, …) —
so it **always emits a prelude header** into each output directory and includes it from every
generated header. A standalone project therefore compiles out of the box. The prelude header is
named `StdAfx.h` by default; `--stdafx MyGame` renames the source/header pair.

If you provide a prelude source (`StdAfx.hx`, or the configured name), its `@:headerCode` is
**merged** with the required headers (de-duplicated), so your custom pragmas/includes are kept and
nothing is doubled. `@:include` is still available on any file — `@:native` or not — for headers
beyond the prelude; a system header in angle brackets is emitted verbatim (`#include <string>`), a
project header stays relative and quoted.

## Validation

The test suite is self-contained — it needs nothing outside this repository. Alongside the unit
tests and the header/body codegen checks (which build small synthetic programs in a temp directory),
the **bundled-example compile gate** (`tests/example_compile.rs`) transpiles the standalone
[`examples/shapes`](examples/shapes) project, compiles the generated C++ together with its
hand-written `main.cpp` under `g++ -std=c++98 -pedantic -Wall`, runs it, and checks the output — so
it validates not just that the code compiles but that it *behaves* (virtual dispatch through owned
base pointers, the enum `switch`, ownership cleanup). It locates a compiler via `HATCHET_GXX`, else
`g++` on `PATH`, else a default MSYS2 install, and skips only if none is found.

```bash
cargo test                 # whole suite; the compile gate is skipped if no C++ compiler is present
HATCHET_GXX=/path/to/g++ cargo test   # point it at a specific compiler
```

Hatchet was also developed against a larger private Haxe game engine whose output is built on real
Visual C++ 6.0 / Windows 98 hardware; that project is the author's offline validation harness and is
not part of the shipped test suite.

### The native boundary contract

For `extern` types — those whose implementation is provided by hand-written C++ — Hatchet **stays
faithful to the Haxe names (as renamed by any `@:native`) and never reads the C++ header**. It emits
exactly what the Haxe code says (`x.data = …` for a Haxe field `data`), and it does not rewrite names
to match a presumed native struct. If an `extern` stub and its C++ definition disagree, the generated
C++ simply fails to compile and the developer reconciles the two. This is the intended division of
labour, not a transpiler limitation: the transpiler describes intent in Haxe terms, and the C++
compiler is the backstop that enforces agreement with the native side.

## License

This project is licensed under the MIT License — see the [LICENSE](LICENSE.md) file for details.

![Hatchy - the Hatchet mascot!](/images/hatchy-small.png)

(c) 2026 Andrew Grant Lind