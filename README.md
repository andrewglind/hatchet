# Hatchet

**Hatchet** is a transpiler from [Haxe](https://haxe.org) 4.x to **C++98** — portable source that
compiles under **Visual C++ 6.0**, and therefore targets legacy platforms such as **Windows 9x** and
older Unix toolchains. It is a *transpiler*, not a compiler: it emits C++ source you then build on the
target, and it never produces a custom C++ runtime. Supported Haxe constructs map to an equivalent,
hand-writable C++ idiom. Hatchet implements a focused subset of Haxe 4.x; it is **not** a drop-in for hxcpp.

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
to a running legacy binary. Hatchet has additionally been validated against a larger real Haxe game
engine (the author's private offline corpus).

Supported today, end to end:

- **Declarations** — classes, interfaces (pure-virtual), enums (pre-C++11 `struct E_ { enum … }`),
  typedef structs and aliases, the fixed-width `UInt8/16/32` shims, and `@:native` interop wiring
  (which emits no code of its own, only the includes it contributes).
- **Members & access** — `(default,set)` / `(default,null)` property accessors (`GetX`/`SetX`, with
  the value-vs-pointer `const` rule), `@:protected`, `@:readOnly` (const-return), `@:decl` (DLL-export
  class), `@:overload(...)` (a call is resolved to the matching C++ overload by argument type, else a
  hard error), `extern inline` (`extern "C"` export via a portable macro), and the base-from-member
  `Holder` idiom for constructors whose `super(...)` is not the first statement.
- **Statements & expressions** — `super(...)` initializer lists; `.`-vs-`->` selection via type
  inference (including inherited fields); anonymous-struct-to-temporary expansion (typed, untyped,
  nested); array / map / object literals; `Array`→`std::vector` and `Map`→`std::map` with their
  container ops — Array `push`/`insert`/`pop`/`length`/`indexOf`/`contains`/`remove`/`reverse`/
  `copy`/`join` and Map `get`/`exists`/`set`/`remove`/`keys` (each an inline loop/expression, no
  `<algorithm>` dependency); Haxe's **auto-extending array writes** — `a[i] = v` past the end grows
  the vector first (an inline `resize`), matching Haxe rather than letting C++ `operator[]` run off
  the end; `for` over a range, an array, an anonymous array literal
  (`for (i in [1,2,3])`), or a map (`for (v in m)` over values, `for (k => v in m)` over key/value
  pairs, via a `std::map` iterator); array & map comprehensions; closures / `Array.map` lowered to
  free functions (an arrow param's type may be left off and taken from the binding's function-type
  annotation — `Cross:(Vec, Vec) -> Float = (a, b) -> …` types `a`/`b` as `Vec`); `String` methods
  (`charAt`/`charCodeAt`/`indexOf`/`lastIndexOf`/`toUpperCase`/
  `toLowerCase`/`split`) and `String.fromCharCode`, mapped to `std::string` expressions; string
  interpolation and `+` concatenation (built as a `std::string` — text appended directly and numeric
  operands formatted into type-bounded buffers, so no value-guessed buffer can overflow; interpolation
  also supports the `$ident` shorthand); the `??` null-coalesce and
  NULL-guarded `?.`; `cast` (C-style cast for `cast(expr, T)`, passthrough for `cast expr`); the
  `(expr : Type)` type ascription (a compile-time hint that drives inference, e.g. `([] : Array<Int>)`);
  `switch`/enum constants; `trace(...)` (with `--no-traces` to strip it); and the `Math` / `Std` /
  `Sys` intrinsics (`Std.int`/`Std.string`/`Std.parseInt`/`Std.parseFloat` → inline
  `(int)`/`sprintf`/`strtol`/`atof`).
- **Conditional compilation & escape hatches** — Haxe `#if FLAG` / `#else` / `#end` map to the C++
  preprocessor (`#ifdef`/`#else`/`#endif`); `untyped <expr>` passes an expression through to C++
  verbatim; a statement-level `@:include("…")` emits an `#include` at that point; and
  `@:cppFileCode('…')` injects verbatim C++ in a body.
- **Types & nullability** — `Null<T>` and optional value-structs lower uniformly to `T*` (with
  matching heap-allocation at call sites); `Map.get(k)` lowers to an iterator with an existence
  check; `final` constants lower to namespace-scoped `static const` (no `#define`), namespace-
  qualified across boundaries.
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
  frees it — rather than hoisted into a scope-owned local that would double-free it.

Hatchet **fails loudly rather than guessing** — an unresolvable type or an unsupported idiom is a
hard error that skips that module and fails the run (see *Diagnostics*) — and it **always generates
the `StdAfx.h` prelude**, so a standalone project compiles with no boilerplate.

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
hatchet --src modules/*.hx mucus/Mucus.hx --out path/to/output --force

# Transpile a single file — pass its dependencies too (superclasses, native stubs),
# since the listed sources are the entire resolution scope:
hatchet --src game/Scene.hx modules/Module.hx mucus/Mucus.hx --out out

# Preview on stdout, or validate without writing anything:
hatchet --src game/Scene.hx modules/Module.hx mucus/Mucus.hx --stdout
hatchet --src . --dry-run

# Run interactively (prompts for a source and a target dir) when --src is omitted:
hatchet
```

`--src` accepts any mix of **single `.hx` files, directories (crawled recursively for `.hx`), and
globs** (`*`, `?`, `**` — e.g. `modules/*.hx` or `src/**/*.hx`). Globs are expanded by Hatchet itself,
so quoting them to bypass shell expansion works. The full expanded set is also the **entire resolution
scope**, so a file's dependencies (superclasses, native `@:native` stubs) must be reachable in it —
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
| `--export-macro <PREFIX>` | Prefix for the portable DLL-export macros wrapped around `extern inline` functions (default `HATCHET` → `HATCHET_EXPORT`/`HATCHET_CALL`/`HATCHET_CLASS`; e.g. `MUCUS`) |
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
| `finals.rs` | Top-level `final` constant extraction |
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
extensions**; and **parameterized enum variants** (a variant with constructor parameters, e.g.
`Move(dx:Int, dy:Int)`, which would need a tagged-union lowering). Relatedly, a `for` loop over
anything other than a range, an `Array`, or a `Map` (a custom `Iterator`/`Iterable`) is a hard
error rather than a guess.

## Standalone projects and the prelude

Hatchet transpiles a **standalone** project — plain Haxe with no `@:native` API stub —
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

Hatchet was also developed against a larger private Haxe game-engine corpus whose output is built
on real Visual C++ 6.0 / Windows 98 hardware; that corpus is the author's offline validation harness
and is not part of the shipped test suite.

### The native boundary contract

For `@:native` types — those whose implementation is provided by hand-written C++ — Hatchet **stays
faithful to the Haxe names and never reads the C++ header**. It emits exactly what the Haxe code says
(`x.data = …` for a Haxe field `data`), and it does not rewrite names to match a presumed native
struct. If a Haxe `@:native` stub and its C++ definition disagree, the generated C++ simply fails to
compile and the developer reconciles the two. This is the intended division of labour, not a
transpiler limitation: the transpiler describes intent in Haxe terms, and the C++ compiler is the
backstop that enforces agreement with the native side.

## License

This project is licensed under the MIT License — see the [LICENSE](LICENSE.md) file for details.
