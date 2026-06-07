# Hatchet

**Hatchet** is a transpiler from [Haxe](https://haxe.org) 4.x to **C++98** — portable source that
compiles under **Visual C++ 6.0**, and therefore targets legacy platforms such as **Windows 9x** and
older Unix toolchains. It is a *transpiler*, not a compiler: it emits C++ source you then build on the
target, and it never produces a custom C++ runtime — every Haxe construct maps to an equivalent,
hand-writable C++ idiom.

The name is a small play on Haxe — a *hatchet* is a small axe (Hatchet implements a focused subset of
Haxe 4.x) — and on "hatch it": develop on a modern machine, then hatch your ideas onto legacy targets.

The transpilation rules are specified in [`SKILL.md`](SKILL.md). Hatchet began life as a "skill" for
coding agents (e.g. [Claude Code](https://claude.com/product/claude-code)); this repository is a
concrete, deterministic Rust implementation of that specification.

## Motivation

hxcpp (Haxe's official C++ backend) cannot target C++ revisions older than C++11, which has
traditionally put Haxe 4.x out of reach for retro and embedded platforms — Windows 98 + VC6, early
Linux, and similar. Hatchet bridges that gap: develop in Haxe on a modern machine, transpile to
C++98, then copy the generated `.h`/`.cpp` to the target and build them with the old toolchain.

## Status

Hatchet is a working transpiler with a real lexer, recursive-descent parser, typed AST, semantic
model, and C++ code generator. It supersedes an earlier regex-driven Python prototype.

It is validated against a real Haxe game engine corpus (see *Validation*): **every generated `.cpp`
in the corpus (24 of 24) compiles under `g++ -std=c++98`**, and the header goldens that have a
byte-for-byte reference (`Module.h`, `IModule.h`) match exactly. The whole-corpus compile gate runs
as a `cargo test`.

Supported today, end to end:

- **Declarations** — classes, interfaces (pure-virtual), enums (pre-C++11 `struct E_ { enum … }`),
  typedef structs and aliases, the fixed-width `UInt8/16/32` shims, and `@:native` interop wiring
  (which emits no code of its own, only the includes it contributes).
- **Members & access** — `(default,set)` / `(default,null)` property accessors (`GetX`/`SetX`, with
  the value-vs-pointer `const` rule), `@:protected`, `@:readOnly` (const-return), `@:decl` (DLL-export
  class), `extern inline` (`extern "C"` export via a portable macro), and the base-from-member
  `Holder` idiom for constructors whose `super(...)` is not the first statement.
- **Statements & expressions** — `super(...)` initializer lists; `.`-vs-`->` selection via type
  inference (including inherited fields); anonymous-struct-to-temporary expansion (typed, untyped,
  nested); array / map / object literals; `Array`→`std::vector`, `Map`→`std::map`; container ops
  (`push`/`insert`/`pop`/`get`/`exists`/`length`); array & map comprehensions; closures /
  `Array.map` lowered to free functions; string interpolation (`sprintf`); `??`, NULL-guarded `?.`;
  `switch`/enum constants; and the `Math` / `Std` / `Sys` intrinsics.
- **Types & nullability** — `Null<T>` and optional value-structs lower uniformly to `T*` (with
  matching heap-allocation at call sites); `Map.get(k)` lowers to an iterator with an existence
  check; `final` constants lower to namespace-scoped `static const` (no `#define`), namespace-
  qualified across boundaries.
- **Memory ownership** — destructors free what a class `new`ed (and the typed pointer handed to a
  base's `void*` field); short-lived heap locals are freed at scope close, and **before every early
  `return`**; owned pointer fields are NULL-initialized, freed before reassignment, and freed in the
  destructor; owned containers are walked and freed element-by-element. Borrowed dependencies, value
  containers, and objects owned by a receiver are left alone.

Hatchet **fails loudly rather than guessing** — an unresolvable type or an unsupported idiom is a
hard error that skips that module and fails the run (see *Diagnostics*) — and it **always generates
the `StdAfx.h` prelude**, so a standalone project compiles with no boilerplate.

## Requirements

- **Rust** (stable; install via [rustup](https://rustup.rs)). On Windows this links via the MSVC
  linker from a Visual Studio / Build Tools install. **Build from PowerShell, cmd, or a VS developer
  prompt** — not Git Bash, whose `/usr/bin/link.exe` shadows the MSVC linker.
- A **C++98 toolchain** to build the generated output. The development validation gate uses
  `g++ -std=c++98`; the production target is Visual C++ 6.0.

## Building

```powershell
cargo build            # debug binary at target/debug/hatchet
cargo build --release  # optimized binary at target/release/hatchet
cargo test             # unit tests + corpus lex/parse + goldens + the C++98 compile gate
```

## Usage

```bash
# Transpile a whole project — pass the files (a shell glob), not a directory. The C++
# namespace of each file follows its Haxe `package`, and the project root is inferred
# from that package.
hatchet --src modules/*.hx mucus/*.hx --out path/to/output --force

# Transpile a single file — pass its dependencies too (superclasses, native stubs),
# since the listed files are the entire resolution scope:
hatchet --src game/Scene.hx modules/Module.hx mucus/Mucus.hx --out out

# Preview on stdout, or validate without writing anything:
hatchet --src game/Scene.hx modules/Module.hx mucus/Mucus.hx --stdout
hatchet --src modules/*.hx mucus/*.hx --dry-run

# Run interactively (prompts for files and a target dir) when --src is omitted:
hatchet
```

`--src` accepts **one or more `.hx` files** — Hatchet does not crawl directories (passing a directory
is an error). The listed files are also the **entire resolution scope**, so a file's dependencies
(superclasses, native `@:native` stubs) must be in the list too; to transpile a whole project, glob
it. Each file's **project root** — the base for the output layout and relative includes — is inferred
from its `package` declaration (the file's directory minus its package path).

| Flag | Description |
|------|-------------|
| `--src, -s <FILE>...` | Haxe `.hx` file(s) to transpile — one or many, e.g. a shell glob (prompted if omitted). Not a directory. Also the full resolution scope |
| `--out, -o <DIR>` | Output directory (defaults to the inferred project root; ignored with `--dry-run`/`--stdout`) |
| `--force` | Overwrite existing generated files (ignored with `--dry-run`) |
| `--dry-run` | Transpile and report info/warnings/errors only — write nothing. Takes precedence over `--stdout`/`-o`/`--force` |
| `--stdout` | Write generated C++ to stdout instead of files (status goes to stderr) |
| `--stdafx <NAME>` | Stem of the prelude source/header (default `StdAfx` → `StdAfx.h`; e.g. `MyGame` → `MyGame.h`) |
| `--export-macro <PREFIX>` | Prefix for the portable DLL-export macros wrapped around `extern inline` functions (default `HATCHET` → `HATCHET_EXPORT`/`HATCHET_CALL`/`HATCHET_CLASS`; e.g. `MUCUS`) |
| `--depth <N>` | Max expression-nesting depth at which a buried `Null<T>` call is auto-extracted into a freed local instead of warned about (default `1`; e.g. `2` auto-extracts `if (GetEdge(e) == null)`) |

Per the rules in `SKILL.md`, `Main.hx` is never transpiled (it is the hxcpp entry point), and
`StdAfx.hx` produces only a `StdAfx.h` (from its `@:headerCode` metadata).

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
| `sema/` | Symbol table, Haxe→C++ type & namespace mapping (`types.rs`), `@:include` resolution (`includes.rs`), pre-codegen validation (`validate.rs`) |
| `codegen/` | C++ generation: `mod.rs` (headers), `source.rs` (`.cpp` bodies), `holder.rs` (base-from-member idiom), `ownership.rs` (destructor / scope ownership analysis) |
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
PR).

## Standalone projects and the prelude

Hatchet transpiles a **standalone** project — pure `@:expose` Haxe with no `@:native` API stub —
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

Hatchet is validated against the **MucusEngine** project — a real Haxe game engine whose committed
C++ output (the `.h`/`.cpp` files beside each `.hx`) has been compiled on Visual C++ 6.0 and serves
as the golden reference. The corpus is split into three sibling repositories — `MucusEngine` (the
native C++ engine), `Modules` (engine modules in Haxe), and `Game` (game scenes in Haxe) — which
live outside this repository. Tests locate them via environment variables, falling back to siblings
of this crate, and skip when absent.

```bash
# Point the test harness at the corpus explicitly if needed:
HATCHET_CORPUS=/path/to/Modules \
HATCHET_GAME_CORPUS=/path/to/Game \
HATCHET_ENGINE=/path/to/MucusEngine \
cargo test
```

The whole-corpus **compile gate** (`tests/compile_gate.rs`) transpiles both Haxe repos into a
temporary mirror and compiles every generated `.cpp` with `g++ -std=c++98 -fsyntax-only`; it locates
a compiler via `HATCHET_GXX`, else `g++` on `PATH`, else a default MSYS2 install, and skips if none
is found. When the goldens and `SKILL.md` disagree, the **goldens win** and `SKILL.md` is updated to
match (it is the compiled, ground-truth output).

### The native boundary contract

For `@:native` types — those whose implementation is provided by hand-written C++ — Hatchet **stays
faithful to the Haxe names and never reads the C++ header**. It emits exactly what the Haxe code says
(`x.data = …` for a Haxe field `data`), and it does not rewrite names to match a presumed native
struct. If a Haxe `@:native` stub and its C++ definition disagree, the generated C++ simply fails to
compile and the developer reconciles the two. This is the intended division of labour, not a
transpiler limitation: the transpiler describes intent in Haxe terms, and the C++ compiler is the
backstop that enforces agreement with the native side.

## License

MIT.
