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

Hatchet is a working transpiler with a real lexer, recursive-descent parser, typed AST, semantic
model, and C++ code generator. A bundled [standalone example](examples/shapes) is transpiled, compiled
under `g++ -std=c++98`, run, and output-checked by the test suite, and the generated output has been
**built with Visual C++ 6.0 and run on Windows 98** — the primary target — closing the loop from Haxe
source to a running legacy binary. Hatchet has additionally been validated against a larger,
real-world C++ codebase.

Hatchet **fails loudly rather than guessing** — an unresolvable type or an unsupported idiom is a hard
error that skips that module and fails the run — and it **always generates the `StdAfx.h` prelude**, so
a standalone project compiles with no boilerplate.

## Quick start

```bash
cargo build --release      # optimized binary at target/release/hatchet

# Transpile a whole project — Hatchet crawls --src recursively for .hx
hatchet --src path/to/project --out path/to/output --force

# Preview on stdout, or validate without writing anything
hatchet --src src/Button.hx --stdout
hatchet --src . --dry-run
```

Requirements: **Rust** (stable, via [rustup](https://rustup.rs)) and a **C++98 toolchain** to build the
generated output (development gate uses `g++ -std=c++98`; the production target is Visual C++ 6.0 up).
See **[Building & Usage](https://github.com/andrewglind/hatchet/wiki/Building-and-Usage)** for the full
CLI and flag table.

## Documentation

Full documentation lives in the **[Hatchet Wiki](https://github.com/andrewglind/hatchet/wiki)**:

- **[Home](https://github.com/andrewglind/hatchet/wiki/Home)** — overview and the hxcpp compatibility principle
- **[Building & Usage](https://github.com/andrewglind/hatchet/wiki/Building-and-Usage)** — requirements, build commands, CLI flags

**Language support**

- **[Declarations](https://github.com/andrewglind/hatchet/wiki/Declarations)** — classes, interfaces, enums, `enum abstract`, typedefs, forward declarations
- **[Value Types & Abstracts](https://github.com/andrewglind/hatchet/wiki/Value-Types-and-Abstracts)** — value classes, `abstract Name(U)`, `@:op` / `@:to` / `@:from`
- **[Members & Access](https://github.com/andrewglind/hatchet/wiki/Members-and-Access)** — access mapping, property accessors, `@:overload`, abstract classes
- **[Statements & Expressions](https://github.com/andrewglind/hatchet/wiki/Statements-and-Expressions)** — control flow, `switch`, containers, strings, lambdas, exceptions
- **[Types & Nullability](https://github.com/andrewglind/hatchet/wiki/Types-and-Nullability)** — `Float`/`Single`, division semantics, shifts, `Null<T>`
- **[Conditional Compilation](https://github.com/andrewglind/hatchet/wiki/Conditional-Compilation)** — `#if`, `untyped`, `@:include`, `@:cppFileCode`
- **[Memory Ownership](https://github.com/andrewglind/hatchet/wiki/Memory-Ownership)** — escape analysis, `@owned` / `@sink` / `@delete`

**Semantics & interop**

- **[Container Semantics](https://github.com/andrewglind/hatchet/wiki/Container-Semantics)** — `Array` and `Map` as value types (the largest divergence from Haxe)
- **[Metadata](https://github.com/andrewglind/hatchet/wiki/Metadata)** — the `@:` and `@` metadata Hatchet honours, and `extern`
- **[Interop via `@proxy`](https://github.com/andrewglind/hatchet/wiki/Interop-via-proxy)** — binding to hand-written native C++

**Internals**

- **[Architecture](https://github.com/andrewglind/hatchet/wiki/Architecture)** — the pipeline and `src/` module map
- **[Diagnostics](https://github.com/andrewglind/hatchet/wiki/Diagnostics)** — fail-loud behaviour and currently-unsupported idioms
- **[The Prelude](https://github.com/andrewglind/hatchet/wiki/The-Prelude)** — standalone projects and the generated `StdAfx.h`
- **[Validation](https://github.com/andrewglind/hatchet/wiki/Validation)** — the test suite, anachrjsonistic, the native boundary contract

## License

This project is licensed under the MIT License — see the [LICENSE](LICENSE.md) file for details.

![Hatchy - the Hatchet mascot!](hatchy.png)

(c) 2026 Andrew Grant Lind
