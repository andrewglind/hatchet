# `shapes` — a self-contained Hatchet example

A small, **standalone** Haxe project (no native `@:native` stub) that
transpiles cleanly to C++98 and compiles under both `g++ -std=c++98` and
Visual C++ 6.0. It exists so Hatchet's output can be exercised end to end — and
checked for VC6 compatibility offline.

## What it exercises

| Feature | Where |
|---------|-------|
| Package → C++ `namespace` | every file is `package examples.shapes;` → `namespace examples { namespace shapes` |
| **`abstract class`** + **`abstract function`** (pure virtual) + **virtual dispatch** | `Shape` (abstract) ← `Circle`, `Rectangle` |
| Auto virtual destructor on an owned base | `Shape` (deleted through `Shape*`) |
| **Base-from-member "Holder" idiom** (work before `super`) | `Circle` → `CircleHolder` |
| Normal `super(...)` initializer list | `Rectangle` |
| `(default, null)` property accessors | `Shape.name`, `Circle.radius`, … |
| Optional param → C++ default argument | `Rectangle(width, height = 1.0)` |
| `typedef` anon struct → C++ `struct` (by value) | `Vec2` |
| Object literal → struct temporary | `{ x: 0.0, y: 0.0 }` in `World` |
| `enum` → pre-C++11 `struct E_ { enum … }` + `switch` | `ShapeKind`, `World.kindName` |
| **`enum abstract`** (`Int`) → enum with explicit member values | `Bucket` |
| **`switch` expression** (value position → hoisted temp) | `World.bucketName`, `World.code` |
| **`switch` on a `String`** → `if`/`else if` chain | `World.code` |
| **Owned container** freed element-by-element | `World.shapes : Array<Shape>` |
| `Array` → `std::vector` + array comprehension | `World.areas` |
| **`Array` methods** (`concat`/`slice`/`shift`/`unshift`/`lastIndexOf`/`join`) | `World.features` |
| `Map` → `std::map` (`exists`/`get`/`set`) | `World.tally` |
| `Math` / `Std` intrinsics (`sqrt`, `Std.int`) | `World.areaRms` |
| **`StringBuf`** accumulator + **`StringTools`** (`replace`) + **`substr`/`substring`** | `World.features` |
| String interpolation + `+` concatenation (overflow-safe) | `Shape.describe`, `World.report` |
| Conditional compilation (`#if` → `#ifdef`) | `World.new` (`VERBOSE`) |
| Auto-generated `StdAfx.h` prelude (no boilerplate) | produced on transpile |

## Build and run

From the repository root, transpile the project (the project root is inferred
from the `package examples.shapes;` declarations, so `--src` points at the
folder holding the `.hx` sources):

```bash
hatchet --src examples/shapes --out examples/shapes --force
```

Then compile the generated C++ together with the hand-written `main.cpp`
(Hatchet never transpiles an entry point — `Main.hx` is reserved for hxcpp):

```bash
cd examples/shapes
g++ -std=c++98 -pedantic -Wall -I out main.cpp examples/shapes/*.cpp -o shapes
./shapes
```

Expected output:

```text
World "shapes": 3 shapes, total area 31.634937
shape count: 3
area rms: 10
features: big|C|shapes=3|20,3,4|4
  circle x2
  rectangle x1
```

### On Windows 98 / Visual C++ 6.0

Copy the generated `out/examples/shapes/*.h` / `*.cpp` and `main.cpp` to the target, add
them to a VC6 project (or `cl`-compile them together with `main.cpp`), and build.
The generated code uses only standard headers pulled in by the generated
`StdAfx.h`, so no extra setup is required.

> The `out/` directory and any built binary are generated artifacts and are
> git-ignored; only the `.hx` sources, `main.cpp`, and this README are tracked.

## It is also valid hxcpp input

A core Hatchet rule is that every input must be real, compilable Haxe — not a
transpiler-only dialect. These sources are therefore kept buildable by Haxe's
own C++ backend: the repo-root [`build.hxml`](../../build.hxml) compiles
[`examples/Main.hx`](../Main.hx) (which imports every example package) with
hxcpp, so the example doubles as an hxcpp conformance check:

```bash
haxe build.hxml      # from the repository root
```
