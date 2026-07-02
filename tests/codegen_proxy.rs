//! @proxy interop, @cexport, extern modules, and write-restricted accessors.
mod common;
use common::*;

#[test]
fn consume_proxy_abstract_is_not_emitted_and_lowers_to_its_native_extern() {
    // `@proxy("native")` on an `abstract Name(cpp.Pointer<T>)` is extern↔Haxe glue:
    // never emitted, and every reference transpiles *as* the named native extern —
    // so a field is `T*` and calls pass straight through (`->`) to the engine,
    // bypassing the proxy's stub bodies entirely.
    let src = "\
@:include(\"engine.h\") @:native(\"eng::IRenderer\") @:structAccess
extern class IRenderer { public function W():Int; }
@:include(\"engine.h\") @:native(\"eng::IEngine\") @:structAccess
extern class IEngine { public function GetRenderer():cpp.Pointer<IRenderer>; }

@proxy(\"eng::IEngine\")
abstract Engine(cpp.Pointer<IEngine>) {
  public function GetRenderer():Renderer { return null; }
}
@proxy(\"eng::IRenderer\")
abstract Renderer(cpp.Pointer<IRenderer>) {
  public function W():Int { return 0; }
}

class User {
  var engine:Engine;
  public function new(engine:Engine) { this.engine = engine; }
  public function go():Int { return engine.GetRenderer().W(); }
}
";
    let head = gen_header(src, "User");
    assert!(
        !head.contains("class Engine"),
        "proxy is not emitted:\n{head}"
    );
    assert!(
        !head.contains("class Renderer"),
        "proxy is not emitted:\n{head}"
    );
    assert!(
        head.contains("eng::IEngine* engine;"),
        "proxy field → native extern pointer:\n{head}"
    );

    let out = gen_one(src, "User");
    assert!(
        out.contains("engine->GetRenderer()->W()"),
        "calls pass straight through the extern with `->`:\n{out}"
    );
}

#[test]
fn produce_proxy_abstract_class_is_a_native_base_subclasses_derive_from() {
    // `@proxy("native")` on an `abstract class` is the produced-base form: the base
    // is never emitted, but a subclass `extends` it as the native base
    // (`: public eng::IScene`) and its `super(...)` routes to the native ctor.
    let src = "\
@:include(\"engine.h\") @:native(\"eng::IScene\") @:structAccess
extern class IScene {}

@proxy(\"eng::IScene\")
abstract class Scene {
  private var id:Int;
  public function new(id:Int) {}
  public abstract function OnLoad():Void;
}

class Title extends Scene {
  public function new() { super(7); }
  public function OnLoad():Void {}
}
";
    let head = gen_header(src, "Title");
    assert!(
        !head.contains("class Scene"),
        "produce-proxy base is not emitted:\n{head}"
    );
    assert!(
        !head.contains("class Scene"),
        "produce-proxy base is not emitted:\n{head}"
    );
    assert!(
        head.contains(": public eng::IScene"),
        "subclass derives from the native base:\n{head}"
    );

    let out = gen_one(src, "Title");
    assert!(
        out.contains("eng::IScene(7)"),
        "super(...) routes to the native base constructor:\n{out}"
    );
}

#[test]
fn proxy_without_argument_is_an_error() {
    let src = "\
@:native(\"eng::IScene\") extern class IScene {}
@proxy
abstract class Scene { public function new() {} }
";
    let errs = validation_errors(src, "Scene");
    assert!(
        errs.iter()
            .any(|e| e.contains("@proxy") && e.contains("requires the fully-qualified")),
        "a `@proxy` with no argument must error, got: {errs:?}"
    );
}

#[test]
fn proxy_on_non_abstract_declaration_is_an_error() {
    let src = "\
@:native(\"eng::IScene\") extern class IScene {}
@proxy(\"eng::IScene\")
class Scene { public function new() {} }
";
    let errs = validation_errors(src, "Scene");
    assert!(
        errs.iter()
            .any(|e| e.contains("@proxy") && e.contains("only valid on an `abstract`")),
        "a `@proxy` on a normal class must error, got: {errs:?}"
    );
}

#[test]
fn proxy_naming_an_undeclared_native_is_an_error() {
    let src = "\
@proxy(\"eng::IScene\")
abstract class Scene { public function new() {} }
";
    let errs = validation_errors(src, "Scene");
    assert!(
        errs.iter().any(|e| e.contains("no matching `extern`")),
        "a `@proxy` naming an undeclared native type must error, got: {errs:?}"
    );
}

#[test]
fn free_function_tail_return_frees_owned_local_once() {
    // A top-level free `function` whose body ends in a `return` must free its owned
    // heap local exactly once — before the `return` — not again at the closing
    // brace. The tail-return emitter already frees owned locals, so an unguarded
    // closing-brace delete is unreachable dead code AND a spurious second free.
    let src = "\
class Worker {
  public function new() {}
  public function run():Int { return 42; }
}

function go():Int {
  var w = new Worker();
  return w.run();
}
";
    let out = gen_one(src, "M");
    let deletes = out.matches("delete w;").count();
    assert_eq!(
        deletes, 1,
        "owned local freed exactly once (no dead double-delete):\n{out}"
    );
}

#[test]
fn extern_module_final_is_not_emitted_but_references_resolve() {
    // An `extern final` constant is provided by hand-written C++ — Hatchet emits
    // no `static const` definition for it (in the header or the cpp), but a
    // cross-namespace reference still resolves to its namespace-qualified name.
    let eng = "\
package eng;
@:include(\"engine.h\") @:native(\"LIMIT\")
extern inline final LIMIT:Int = 99;
";
    let game = "\
package game;
import eng.Engine;
class User {
  public function new() {}
  public function cap():Int { return LIMIT; }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_externfinal_{}", std::process::id()));
    std::fs::create_dir_all(dir.join("eng")).unwrap();
    std::fs::create_dir_all(dir.join("game")).unwrap();
    std::fs::write(dir.join("eng").join("Engine.hx"), eng).unwrap();
    std::fs::write(dir.join("game").join("Game.hx"), game).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("Game"))
        .unwrap();
    let out = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        !out.contains("99"),
        "no definition for the extern const is emitted:\n{out}"
    );
    assert!(
        out.contains("eng::LIMIT"),
        "cross-namespace reference is qualified:\n{out}"
    );
}

#[test]
fn cexport_meta_exports_a_c_abi_function_and_plain_does_not() {
    // `@cexport` is the C-ABI export (the export/calling-convention macros at
    // global scope); a plain `function` stays a namespace free function.
    let src = "\
@cexport function pick(n:Int):Int { return n; }
function helper(n:Int):Int { return n + 1; }
";
    let head = gen_header(src, "Api");
    assert!(
        head.contains("HATCHET_EXPORT int HATCHET_CALL pick(int n)"),
        "@cexport → a macro-wrapped global export:\n{head}"
    );
    assert!(
        head.contains("int helper(int n)"),
        "a plain function stays a free function:\n{head}"
    );
    // The export lives at global scope, the free function inside the namespace.
    assert!(
        !head.contains("HATCHET_EXPORT int HATCHET_CALL helper"),
        "plain fn is not exported:\n{head}"
    );
}

#[test]
fn real_haxe_decl_and_abi_metas_are_inert() {
    // Haxe's *real* `@:decl` and `@:abi` are inbound-only and do nothing here:
    // `@:abi` only sets the calling convention of a `cpp.Function<T,Abi>` callback
    // type (unsupported in C++98 — no `std::function`), and `@:decl` is vestigial
    // in the Haxe compiler (never read, filtered out of RTTI). Hatchet's outbound
    // shared-library / `extern "C"` export behaviours moved to `@libexport` / `@cexport`,
    // so these colon-forms must now be parsed-and-ignored: a plain free function and
    // a plain class, with no export decoration and no error.
    let src = "\
@:abi function pick(n:Int):Int { return n; }
@:decl class Widget { public function new() {} }
";
    let head = gen_header(src, "Inert");
    // `@:abi` → ordinary namespace free function, NOT a global `extern \"C\"` export.
    assert!(
        head.contains("int pick(int n)"),
        "@:abi fn stays a plain free function:\n{head}"
    );
    assert!(
        !head.contains("HATCHET_EXPORT") && !head.contains("HATCHET_CALL"),
        "@:abi must not emit the @cexport export macros:\n{head}"
    );
    // `@:decl` → ordinary class, NOT a DLL-exported one.
    assert!(
        head.contains("class Widget"),
        "@:decl class is still emitted as a plain class:\n{head}"
    );
    assert!(
        !head.contains("HATCHET_CLASS") && !head.contains("_CLASS Widget"),
        "@:decl must not emit the @libexport class macro:\n{head}"
    );
}

#[test]
fn empty_array_return_is_a_default_constructed_vector() {
    // `return []` for a by-value `Array<T>` (`std::vector<...>`) must
    // default-construct the container — you cannot `return NULL` from a function
    // returning a vector by value. A `Null<Array<T>>` (pointer) still returns NULL.
    let src = "\
class Thing { public function new() {} }
class Probe {
  public function new() {}
  public function ints():Array<Int> { return []; }
  public function things():Array<Thing> { return []; }
  public function maybe():Null<Array<Int>> { return []; }
}
";
    let out = gen_one(src, "Probe");
    assert!(
        out.contains("return std::vector<int>();"),
        "empty Array<Int> → default vector:\n{out}"
    );
    assert!(
        out.contains("return std::vector<Thing*>();"),
        "empty Array<Thing> → default vector:\n{out}"
    );
    assert!(
        out.contains("return NULL;"),
        "empty Null<Array<Int>> (pointer) still returns NULL:\n{out}"
    );
}

#[test]
fn write_restricted_container_getter_returns_mutable_reference() {
    // A `(default, null)` Array property is read-public / reassign-private, but Haxe
    // still allows mutating its contents (arrays are reference types). The generated
    // getter must therefore return a mutable reference, so `a.items[i] = v` and
    // `a.items.push(v)` compile and actually mutate the member (not a const copy).
    let src = "\
class Holder {
  public var items(default, null):Array<Int>;
  public function new() { this.items = []; }
}
";
    let out = gen_header(src, "Holder");
    assert!(
        out.contains("std::vector<int>& GetItems() { return items; }"),
        "a write-restricted container getter returns a mutable reference:\n{out}"
    );

    // ...and the read-only behaviour for a non-container value field is unchanged.
    let src2 = "\
class Named {
  public var name(default, null):String;
  public function new() { this.name = \"\"; }
}
";
    let out2 = gen_header(src2, "Named");
    assert!(
        out2.contains("const std::string GetName() { return name; }"),
        "a non-container value getter stays a read-only const copy:\n{out2}"
    );
}

#[test]
fn write_restricted_value_struct_getter_returns_mutable_reference() {
    // A value struct is a reference type in Haxe too, so `a.pos.x = v` mutates the
    // shared struct — the generated getter for a write-restricted struct field must
    // return a mutable reference, and the field write lowers through it.
    let src = "\
typedef Pt = { x:Int, y:Int }

class Holder {
  public var pos(default, null):Pt;
  public function new() { this.pos = { x: 0, y: 0 }; }
}
";
    let head = gen_header(src, "Holder");
    assert!(
        head.contains("Pt& GetPos() { return pos; }"),
        "a write-restricted value-struct getter returns a mutable reference:\n{head}"
    );

    let src2 = "\
typedef Pt = { x:Int, y:Int }

class Holder {
  public var pos(default, null):Pt;
  public function new() { this.pos = { x: 0, y: 0 }; }
}

class Probe {
  var a:Holder;
  public function new() { this.a = new Holder(); }
  public function move(nx:Int):Void {
    this.a.pos.x = nx;
  }
}
";
    let out = gen_one(src2, "Probe");
    assert!(
        out.contains("this->a->GetPos().x = nx;"),
        "a struct-field write lowers through the mutable getter:\n{out}"
    );
}

#[test]
fn container_index_assign_through_getter_compiles() {
    // The auto-extend index write reaches the array through the getter; with a
    // mutable-reference getter the `resize` / `[i] = v` operate on the real member.
    let src = "\
class Holder {
  public var items(default, null):Array<Int>;
  public function new() { this.items = []; }
}

class Probe {
  var a:Holder;
  public function new() { this.a = new Holder(); }
  public function put(key:Int, value:Int):Void {
    this.a.items[key] = value;
  }
}
";
    let out = gen_one(src, "Probe");
    assert!(
        out.contains("this->a->GetItems().resize(")
            && out.contains("this->a->GetItems()[")
            && out.contains("] = value;"),
        "index-assign auto-extend routes through the (now mutable) getter:\n{out}"
    );
}
