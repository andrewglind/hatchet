# `shapes` — a self-contained Hatchet example

A small, **standalone** Haxe project (no native `@:native` engine stub) that
transpiles cleanly to C++98 and compiles under both `g++ -std=c++98` and
Visual C++ 6.0. It exists so Hatchet's output can be exercised end to end — and
checked for VC6 compatibility offline — without the private game corpus.

## What it exercises

| Feature | Where |
|---------|-------|
| Package → C++ `namespace` | every file is `package demo;` → `namespace demo` |
| Class inheritance + **virtual dispatch** | `Shape` ← `Circle`, `Rectangle` |
| Auto virtual destructor on an owned base | `Shape` (deleted through `Shape*`) |
| **Base-from-member "Holder" idiom** (work before `super`) | `Circle` → `CircleHolder` |
| Normal `super(...)` initializer list | `Rectangle` |
| `(default, null)` property accessors | `Shape.name`, `Circle.radius`, … |
| Optional param → C++ default argument | `Rectangle(width, height = 1.0)` |
| `typedef` anon struct → C++ `struct` (by value) | `Vec2` |
| Object literal → struct temporary | `{ x: 0.0, y: 0.0 }` in `World` |
| `enum` → pre-C++11 `struct E_ { enum … }` + `switch` | `ShapeKind`, `World.kindName` |
| **Owned container** freed element-by-element | `World.shapes : Array<Shape>` |
| `Array` → `std::vector` + array comprehension | `World.areas` |
| `Map` → `std::map` (`exists`/`get`/`set`) | `World.tally` |
| `Math` / `Std` intrinsics (`sqrt`, `Std.int`) | `World.areaRms` |
| String interpolation + `+` concatenation (overflow-safe) | `Shape.describe`, `World.report` |
| Conditional compilation (`#if` → `#ifdef`) | `World.new` (`VERBOSE`) |
| Auto-generated `StdAfx.h` prelude (no boilerplate) | produced on transpile |

## Build and run

From the repository root, transpile the project (the project root is inferred
from the `package demo;` declarations, so `--src` points at the package folder):

```bash
hatchet --src examples/shapes/demo --out examples/shapes/out --force
```

Then compile the generated C++ together with the hand-written `main.cpp`
(Hatchet never transpiles an entry point — `Main.hx` is reserved for hxcpp):

```bash
cd examples/shapes
g++ -std=c++98 -pedantic -Wall -I out main.cpp out/demo/*.cpp -o shapes
./shapes
```

Expected output:

```text
World "demo": 3 shapes, total area 31.634937
shape count: 3
area rms: 10
  circle x2
  rectangle x1
```

### On Windows 98 / Visual C++ 6.0

Copy the generated `out/demo/*.h` / `*.cpp` and `main.cpp` to the target, add
them to a VC6 project (or `cl`-compile them together with `main.cpp`), and build.
The generated code uses only standard headers pulled in by the generated
`StdAfx.h`, so no extra setup is required.

> The `out/` directory and any built binary are generated artifacts and are
> git-ignored; only the `.hx` sources, `main.cpp`, and this README are tracked.
