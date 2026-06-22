//! M1 of the ownership-analysis rewrite: a runtime safety net.
//!
//! Each test transpiles a small, self-contained Haxe ownership scenario (no engine
//! dependency), compiles the generated C++ together with an **instrumented global
//! `operator new`/`delete`** that tracks live heap allocations, runs it, and checks:
//!   * **double-free / use-after-free is a hard failure** (a `delete` of an
//!     untracked pointer), and
//!   * **leaks are reported** (live allocations at the end) — conservative-leak is
//!     an acceptable failure mode, so a leak is asserted absent only where the
//!     scenario is supposed to have a clean unique owner.
//!
//! This is the safety net the rest of the rewrite is validated against: every later
//! milestone must keep these green (no double-free) while shrinking the leak set.
//!
//! Skipped (passes vacuously) when no C++ compiler is available.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Locate a C++98 compiler: `$HATCHET_GXX`, else `g++`, else the MSYS2 default.
fn find_gxx() -> Option<String> {
    let candidates = [
        std::env::var("HATCHET_GXX").ok(),
        Some("g++".to_string()),
        Some(r"C:\msys64\mingw32\bin\g++.exe".to_string()),
    ];
    candidates.into_iter().flatten().find(|c| {
        Command::new(c)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

/// Instrumented global allocator: every `new`/`delete` (from any translation unit
/// linked in) routes through here once `mem_begin()` is called. A `delete` of an
/// untracked pointer (double-free / use-after-free) bumps `g_double`; allocations
/// still live at `mem_report()` are leaks. A reentrancy guard keeps the tracking
/// map's own allocations out of the counts.
const ALLOC: &str = r#"
#include <cstddef>
#include <cstdlib>
#include <cstdio>
#include <new>
#include <map>

static std::map<void*, int> g_live;
static bool g_track = false;
static bool g_in = false;
static int g_seq = 0;
static int g_double = 0;

void* operator new(std::size_t n) throw(std::bad_alloc) {
    void* p = std::malloc(n ? n : 1);
    if (!p) throw std::bad_alloc();
    if (g_track && !g_in) { g_in = true; g_live[p] = ++g_seq; g_in = false; }
    return p;
}
void operator delete(void* p) throw() {
    if (p && g_track && !g_in) {
        g_in = true;
        // Untracked pointer => double-free / use-after-free. Count it, but do NOT
        // pass it to std::free again: a real second free would let a hardened
        // libc/CRT abort() the process before mem_report() runs, so the harness
        // could never observe the double-free it is meant to detect.
        if (g_live.erase(p) == 0) { g_double++; g_in = false; return; }
        g_in = false;
    }
    std::free(p);
}
void* operator new[](std::size_t n) throw(std::bad_alloc) { return operator new(n); }
void operator delete[](void* p) throw() { operator delete(p); }

static void mem_begin() { g_track = true; }
static void mem_report() {
    g_track = false;
    std::printf("LIVE=%d DOUBLEFREE=%d\n", (int)g_live.size(), g_double);
}
"#;

struct MemResult {
    /// Heap objects still allocated when the scenario finished (leaks).
    live: i32,
    /// `delete`s of an untracked pointer — a double-free / use-after-free.
    double_free: i32,
}

/// Parse the driver's `LIVE=.. DOUBLEFREE=..` summary line.
fn parse_result(stdout: &str) -> MemResult {
    let get = |key: &str| {
        stdout
            .split_whitespace()
            .find_map(|t| t.strip_prefix(key))
            .and_then(|v| v.parse().ok())
            .unwrap_or(-1)
    };
    MemResult {
        live: get("LIVE="),
        double_free: get("DOUBLEFREE="),
    }
}

fn unique_dir(tag: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "hatchet_own_{}_{}_{}",
        tag,
        std::process::id(),
        stamp
    ))
}

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "scn.exe"
    } else {
        "scn"
    }
}

/// `.cpp` files directly inside `dir`.
fn cpp_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("cpp") {
                out.push(p);
            }
        }
    }
    out
}

/// Compile every `.cpp` in `dir` (with `-I dir`) into one executable, run it, and
/// return its parsed memory summary. Panics on a compile/run failure under `tag`.
fn compile_run(gxx: &str, tag: &str, dir: &Path) -> MemResult {
    let exe = dir.join(exe_name());
    let mut cmd = Command::new(gxx);
    cmd.args(["-std=c++98", "-g"]).arg("-I").arg(dir);
    for f in cpp_files(dir) {
        cmd.arg(f);
    }
    cmd.arg("-o").arg(&exe);
    let compile = cmd.output().expect("run g++");
    assert!(
        compile.status.success(),
        "[{tag}] g++ failed:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let run = Command::new(&exe).output().expect("run scenario exe");
    parse_result(&String::from_utf8_lossy(&run.stdout))
}

/// Transpile `haxe` (a self-contained module whose entry class is `Scenario` with
/// `new()` + `run()`), link it with the instrumented driver, run it, and return the
/// memory result. `None` only when no compiler is present.
fn run_scenario(tag: &str, haxe: &str) -> Option<MemResult> {
    let gxx = find_gxx()?;
    let root = unique_dir(tag);
    let src = root.join("src");
    let out = root.join("out");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("Scenario.hx"), haxe).unwrap();

    let ok = Command::new(env!("CARGO_BIN_EXE_hatchet"))
        .arg("--out")
        .arg(&out)
        .arg("--force")
        .arg("--src")
        .arg(src.join("Scenario.hx"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "[{tag}] transpilation failed");

    let driver = format!(
        "{ALLOC}\n#include \"Scenario.h\"\nint main() {{\n\
         \tmem_begin();\n\
         \t{{ Scenario* s = new Scenario(); s->run(); delete s; }}\n\
         \tmem_report();\n\treturn 0;\n}}\n"
    );
    std::fs::write(out.join("driver.cpp"), driver).unwrap();

    let result = compile_run(&gxx, tag, &out);
    let _ = std::fs::remove_dir_all(&root);
    Some(result)
}

/// The harness itself works and catches a double-free: a hand-written C++ program
/// (no Hatchet) that frees the same object twice must report `DOUBLEFREE>0`. This
/// validates the safety net so a clean run of the real scenarios actually means
/// something.
#[test]
fn harness_detects_a_double_free() {
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++ compiler");
        return;
    };
    let dir = unique_dir("selftest");
    std::fs::create_dir_all(&dir).unwrap();
    let main = format!(
        "{ALLOC}\nint main() {{\n\
         \tmem_begin();\n\
         \t{{ int* p = new int(1); delete p; delete p; }}\n\
         \tmem_report();\n\treturn 0;\n}}\n"
    );
    std::fs::write(dir.join("main.cpp"), main).unwrap();
    let r = compile_run(&gxx, "selftest", &dir);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        r.double_free > 0,
        "the harness must catch a deliberate double-free (got {})",
        r.double_free
    );
}

/// A `new` into an `@owned` constructor parameter is freed exactly once by the
/// owner's destructor — no double-free, no leak. (The Line/Actor bug, distilled.)
#[test]
fn owned_ctor_param_is_freed_once() {
    let src = "\
class Child {
  public function new() {}
}

class Owner {
  @owned var c:Child;
  public function new(c:Child) { this.c = c; }
}

class Scenario {
  public function new() {}
  public function run():Void {
    var o:Owner = new Owner(new Child());
  }
}
";
    let Some(r) = run_scenario("owned_ctor", src) else {
        eprintln!("skipping: no C++ compiler");
        return;
    };
    assert_eq!(r.double_free, 0, "owned ctor param must not double-free");
    assert_eq!(r.live, 0, "owner + child must both be freed (no leak)");
}

/// A scope-local `new` whose constructor arguments it owns frees them transitively,
/// exactly once. (The `new Node(new Leaf(), new Leaf())` shape.)
#[test]
fn scope_local_with_nested_owned_args_is_freed() {
    let src = "\
class Leaf {
  public function new() {}
}

class Node {
  @owned var a:Leaf;
  @owned var b:Leaf;
  public function new(a:Leaf, b:Leaf) { this.a = a; this.b = b; }
}

class Scenario {
  public function new() {}
  public function run():Void {
    var n:Node = new Node(new Leaf(), new Leaf());
  }
}
";
    let Some(r) = run_scenario("scope_nested", src) else {
        eprintln!("skipping: no C++ compiler");
        return;
    };
    assert_eq!(r.double_free, 0, "nested owned news must not double-free");
    assert_eq!(r.live, 0, "node + both leaves must be freed");
}

/// Aliasing: one object referenced by two fields. The transpiler must NOT free it
/// twice — leaking (LIVE>0) is the acceptable conservative outcome, but a
/// double-free is never allowed.
#[test]
fn aliased_object_is_not_double_freed() {
    let src = "\
class Thing {
  public function new() {}
}

class Holder {
  var a:Thing;
  var b:Thing;
  public function new() {
    var t:Thing = new Thing();
    this.a = t;
    this.b = t;
  }
}

class Scenario {
  public function new() {}
  public function run():Void {
    var h:Holder = new Holder();
  }
}
";
    let Some(r) = run_scenario("aliased", src) else {
        eprintln!("skipping: no C++ compiler");
        return;
    };
    assert_eq!(
        r.double_free, 0,
        "an aliased object must never be double-freed (leak is OK)"
    );
}

/// Haxe's auto-extending array writes run correctly at runtime (the Graph crash):
/// writing past the end of an empty vector must grow it, not corrupt the heap.
#[test]
fn array_auto_grow_runs_clean() {
    let src = "\
class Scenario {
  public function new() {}
  public function run():Void {
    var a:Array<Int> = [];
    for (i in 0...8) {
      a[i] = i * i;
    }
  }
}
";
    let Some(r) = run_scenario("array_grow", src) else {
        eprintln!("skipping: no C++ compiler");
        return;
    };
    assert_eq!(r.double_free, 0, "array grow must not corrupt the heap");
    assert_eq!(r.live, 0, "the value vector is freed at scope close");
}

/// An `@owned` container of pointers: `new`s pushed in are freed element-by-element
/// by the destructor, exactly once. (The owned-collection-of-children shape.)
#[test]
fn owned_container_of_pointers_freed_once() {
    let src = "\
class Item {
  public function new() {}
}

class Bag {
  @owned var items:Array<Item>;
  public function new() {
    this.items = [];
    this.items.push(new Item());
    this.items.push(new Item());
    this.items.push(new Item());
  }
}

class Scenario {
  public function new() {}
  public function run():Void {
    var b:Bag = new Bag();
  }
}
";
    let Some(r) = run_scenario("owned_container", src) else {
        eprintln!("skipping: no C++ compiler");
        return;
    };
    assert_eq!(
        r.double_free, 0,
        "owned container elements must not double-free"
    );
    assert_eq!(r.live, 0, "every pushed item must be freed");
}

/// A nested owned container (`Array<Array<T>>`) is walked and freed at every level,
/// exactly once. (The nested `tilesets` shape.)
#[test]
fn nested_owned_container_freed_once() {
    let src = "\
class Tile {
  public function new() {}
}

class Grid {
  @owned var rows:Array<Array<Tile>>;
  public function new() {
    this.rows = [];
    var row:Array<Tile> = [];
    row.push(new Tile());
    row.push(new Tile());
    this.rows.push(row);
  }
}

class Scenario {
  public function new() {}
  public function run():Void {
    var g:Grid = new Grid();
  }
}
";
    let Some(r) = run_scenario("nested_container", src) else {
        eprintln!("skipping: no C++ compiler");
        return;
    };
    assert_eq!(r.double_free, 0, "nested owned tiles must not double-free");
    assert_eq!(r.live, 0, "every tile must be freed");
}

/// Reassigning an owned pointer field frees the prior value first
/// (delete-before-overwrite): no leak of the old object, no double-free.
#[test]
fn delete_before_overwrite_is_clean() {
    let src = "\
class Thing {
  public function new() {}
}

class Box {
  var t:Thing;
  public function new() { this.t = new Thing(); }
  public function swap():Void { this.t = new Thing(); }
}

class Scenario {
  public function new() {}
  public function run():Void {
    var b:Box = new Box();
    b.swap();
    b.swap();
  }
}
";
    let Some(r) = run_scenario("delete_overwrite", src) else {
        eprintln!("skipping: no C++ compiler");
        return;
    };
    assert_eq!(
        r.double_free, 0,
        "reassigned owned field must not double-free"
    );
    assert_eq!(
        r.live, 0,
        "the prior value is freed before overwrite (no leak)"
    );
}

/// The bare-field escape bug, end to end: `o = new Outer(new Inner())` (with `this.`
/// omitted) where Outer `@owns` the inner. Both are freed once via `delete o`.
#[test]
fn bare_field_assign_with_nested_owned_is_clean() {
    let src = "\
class Inner {
  public function new() {}
}

class Outer {
  @owned var i:Inner;
  public function new(i:Inner) { this.i = i; }
}

class Holder {
  var o:Outer;
  public function new() {
    o = new Outer(new Inner());
  }
}

class Scenario {
  public function new() {}
  public function run():Void {
    var h:Holder = new Holder();
  }
}
";
    let Some(r) = run_scenario("bare_field", src) else {
        eprintln!("skipping: no C++ compiler");
        return;
    };
    assert_eq!(
        r.double_free, 0,
        "bare-field nested owned new must not double-free"
    );
    assert_eq!(r.live, 0, "outer + inner must both be freed");
}

/// A `new` returned from a method and bound to a caller local is NOT double-freed.
/// Today this *leaks* (the caller doesn't know the return transfers ownership) — an
/// acceptable conservative outcome that M3's interprocedural return summaries will
/// later tighten. The hard guarantee here is just: no double-free.
#[test]
fn returned_new_is_not_double_freed() {
    let src = "\
class Thing {
  public function new() {}
}

class Factory {
  public function new() {}
  public function make():Thing { return new Thing(); }
}

class Scenario {
  public function new() {}
  public function run():Void {
    var f:Factory = new Factory();
    var t:Thing = f.make();
  }
}
";
    let Some(r) = run_scenario("return_transfer", src) else {
        eprintln!("skipping: no C++ compiler");
        return;
    };
    assert_eq!(
        r.double_free, 0,
        "a transferred-out new must never be double-freed"
    );
}

/// `@delete` on a local frees it at scope close — even a returned pointer the
/// analysis would otherwise leak. The developer's explicit override turns the
/// `returned_new` leak into a clean free, with no double-free.
#[test]
fn delete_tag_frees_a_returned_pointer() {
    let src = "\
class Thing {
  public function new() {}
}

class Factory {
  public function new() {}
  public function make():Thing { return new Thing(); }
}

class Scenario {
  public function new() {}
  public function run():Void {
    var f:Factory = new Factory();
    @delete var t:Thing = f.make();
  }
}
";
    let Some(r) = run_scenario("delete_tag", src) else {
        eprintln!("skipping: no C++ compiler");
        return;
    };
    assert_eq!(r.double_free, 0, "@delete must not double-free");
    assert_eq!(
        r.live, 0,
        "@delete frees the returned pointer the analysis would leak"
    );
}

// ---- real-world structural features (engine-free) -------------------------
// These mirror the shapes real Haxe code uses around ownership — property
// accessors, inheritance + `super` forwarding, and the base-from-member holder
// idiom — without needing the engine, so the codegen paths real projects
// exercise are checked at runtime too.

/// `@owned` on a property field (`var a(default, null):Child`) — the Quad/Line
/// shape: the accessor is generated and the field is still freed exactly once.
#[test]
fn owned_property_field_is_freed_once() {
    let src = "\
class Child {
  public function new() {}
}

class Owner {
  @owned public var a(default, null):Child;
  public function new(a:Child) { this.a = a; }
}

class Scenario {
  public function new() {}
  public function run():Void {
    var o:Owner = new Owner(new Child());
  }
}
";
    let Some(r) = run_scenario("owned_property", src) else {
        eprintln!("skipping: no C++ compiler");
        return;
    };
    assert_eq!(
        r.double_free, 0,
        "owned property field must not double-free"
    );
    assert_eq!(r.live, 0, "owner + child must both be freed");
}

/// Inheritance with a borrowed `super(...)` parameter and an `@owned` own-field
/// (the Quad-extends-Module shape): the base-forwarded value is not freed, the
/// owned field is freed once.
#[test]
fn inherited_super_forward_plus_owned_field_is_clean() {
    let src = "\
class Base {
  var tag:Int;
  public function new(tag:Int) { this.tag = tag; }
}

class Leaf {
  public function new() {}
}

class Derived extends Base {
  @owned var c:Leaf;
  public function new(tag:Int, c:Leaf) {
    super(tag);
    this.c = c;
  }
}

class Scenario {
  public function new() {}
  public function run():Void {
    var d:Derived = new Derived(7, new Leaf());
  }
}
";
    let Some(r) = run_scenario("inherit_owned", src) else {
        eprintln!("skipping: no C++ compiler");
        return;
    };
    assert_eq!(r.double_free, 0, "inherited owner must not double-free");
    assert_eq!(
        r.live, 0,
        "the owned leaf must be freed; the value tag has nothing to free"
    );
}

/// Base-from-member holder idiom (computed locals before `super`, as in Actor) with
/// an `@owned` field: the holder construction must not disturb ownership — the field
/// is still freed exactly once.
#[test]
fn holder_idiom_with_owned_field_is_clean() {
    let src = "\
class Base {
  var v:Int;
  public function new(v:Int) { this.v = v; }
}

class Leaf {
  public function new() {}
}

class Sub extends Base {
  @owned var c:Leaf;
  public function new(c:Leaf) {
    var computed:Int = 40 + 2;
    super(computed);
    this.c = c;
  }
}

class Scenario {
  public function new() {}
  public function run():Void {
    var s:Sub = new Sub(new Leaf());
  }
}
";
    let Some(r) = run_scenario("holder_owned", src) else {
        eprintln!("skipping: no C++ compiler");
        return;
    };
    assert_eq!(r.double_free, 0, "holder-idiom owner must not double-free");
    assert_eq!(r.live, 0, "the owned leaf must be freed");
}
