//! Abstract/value types, native @:native renaming, extern, and cyclic forwarders.
mod common;
use common::*;

#[test]
fn abstract_newtype_lowers_to_a_value_class() {
    // `abstract Name(U)` → a value class wrapping U in a synthetic `__this`
    // field; `this` inside methods is the underlying value (`this->__this`);
    // `new` is value construction (no heap); non-virtual destructor.
    let src = "\
abstract Meters(Float) {
  public function new(v:Float) { this = v; }
  public function doubled():Float { return this * 2.0; }
}
class Use {
  public function new() {}
  public function go():Float { var m = new Meters(3.5); return m.doubled(); }
}
";
    let head = {
        let dir = std::env::temp_dir().join(format!("hatchet_abs_h_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Use.hx"), src).unwrap();
        let prog = Program::from_src_dir(&dir).expect("build program");
        let idx = prog
            .modules
            .iter()
            .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("Use"))
            .unwrap();
        let h = hatchet::codegen::generate_header(&prog, idx).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        h
    };
    assert!(
        head.contains("double __this;"),
        "underlying wrapped in __this:\n{head}"
    );
    assert!(
        head.contains("\t~Meters() {}"),
        "value class: non-virtual destructor:\n{head}"
    );

    let out = gen_one(src, "Use");
    assert!(
        out.contains("this->__this = v;"),
        "`this = v` writes the underlying:\n{out}"
    );
    assert!(
        out.contains("return this->__this * 2.0;"),
        "`this` reads the underlying:\n{out}"
    );
    assert!(
        out.contains("Meters m = Meters(3.5)"),
        "`new` is value construction:\n{out}"
    );
}

#[test]
fn static_abstract_method_call_uses_scope_resolution() {
    // `Type.staticMethod(args)` on a user value class → `Type::staticMethod(args)`
    // (scope resolution), not member access (`.`).
    let src = "\
typedef Cents = { var n:Int; }
abstract Money(Cents) {
  public function new(n:Int) { this = { n: n }; }
  public static function zero():Money { return new Money(0); }
}
class Use {
  public function new() {}
  public function go():Money { return Money.zero(); }
}
";
    let out = gen_one(src, "Use");
    assert!(
        out.contains("Money::zero()"),
        "static call uses scope resolution:\n{out}"
    );
    assert!(
        !out.contains("Money.zero()"),
        "no member-access dot for a static call:\n{out}"
    );
}

#[test]
fn cyclic_value_types_define_forwarders_out_of_line() {
    // A `@:op([])` forwarder that returns a *later*-defined sibling value class
    // (the sibling is incomplete in the class body) must be declared in-class and
    // defined out-of-line (`inline`) after both classes — how a hand-written
    // header breaks a `jobject`/`proxy` cycle. The self-returning operator stays
    // inline (a member body is a complete-class context).
    let src = "\
typedef ViewData = { var n:Int; }
typedef BagData = { var v:View; }
abstract Bag(BagData) {
  public function new(v:View) { this = { v: v }; }
  @:op([]) public function at(i:Int):View { return this.v; }
}
abstract View(ViewData) {
  public function new(n:Int) { this = { n: n }; }
  @:to public function toInt():Int { return this.n; }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_cycle_h_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Cyc.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("Cyc"))
        .unwrap();
    let head = hatchet::codegen::generate_header(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        head.contains("class View;"),
        "later sibling is forward-declared:\n{head}"
    );
    assert!(
        head.contains("View operator[](int i);"),
        "the cyclic forwarder is declared in-class:\n{head}"
    );
    assert!(
        head.contains("inline View Bag::operator[](int i) { return at(i); }"),
        "...and defined out-of-line after both classes:\n{head}"
    );
    // `View`'s own `@:to` (return type Int) is complete in-class → stays inline.
    assert!(
        head.contains("operator int() { return toInt(); }"),
        "a non-deferred conversion stays inline:\n{head}"
    );
}

#[test]
fn native_meta_renames_an_emitted_class() {
    // `@:native("name")` only renames the emitted C++ symbol — the type is still
    // emitted (definition, ctor/dtor, method qualifiers, and uses all use `name`).
    let src = "\
typedef PtData = { var x:Int; }
@:native(\"pt\") abstract Point(PtData) {
  public function new(x:Int) { this = { x: x }; }
  public function gx():Int { return this.x; }
}
class Use {
  public var p:Point;
  public function new() { p = new Point(1); }
}
";
    let head = gen_header(src, "Use");
    assert!(
        head.contains("class pt {"),
        "renamed class definition:\n{head}"
    );
    assert!(head.contains("pt(int x);"), "renamed constructor:\n{head}");
    assert!(
        head.contains("pt p;"),
        "uses of the type are renamed:\n{head}"
    );
    assert!(
        !head.contains("Point"),
        "the Haxe name must not leak:\n{head}"
    );

    let out = gen_one(src, "Use");
    assert!(
        out.contains("pt::pt(int x)"),
        "ctor definition qualifier renamed:\n{out}"
    );
    assert!(
        out.contains("int pt::gx()"),
        "method definition qualifier renamed:\n{out}"
    );
}

#[test]
fn extern_type_is_not_emitted_but_its_include_is_pulled() {
    // `extern class` — implementation in hand-written C++; Hatchet emits no
    // definition, but a module using it still pulls the `@:include`.
    let src = "\
@:include(\"engine.h\")
extern class Engine {
  public function ping():Int;
}
class User {
  public var e:Engine;
  public function new() {}
  public function go():Int { return e.ping(); }
}
";
    let head = gen_header(src, "User");
    assert!(
        !head.contains("class Engine"),
        "extern class must not be emitted:\n{head}"
    );
    assert!(
        head.contains("#include \"engine.h\""),
        "its @:include is pulled:\n{head}"
    );
    assert!(
        head.contains("class User"),
        "the non-extern class is emitted:\n{head}"
    );
    assert!(
        head.contains("Engine* e;"),
        "the extern type is referenced (by pointer):\n{head}"
    );
}

#[test]
fn cpp_pointer_and_stdstring_lower_to_pointer_and_std_string() {
    // hxcpp interop shims used to bind external engine handles: `cpp.Pointer<T>`
    // lowers to `T*` (the inner type's spelling, incl. any `@:native` rename),
    // and `cpp.StdString` to `std::string`.
    let src = "\
@:include(\"engine.h\") @:native(\"eng::IEngine\") @:structAccess
extern class IEngine {
  public function go():Int;
}
class User {
  public var engine:cpp.Pointer<IEngine>;
  public var name:cpp.StdString;
  public function new() {}
  public function rename(n:cpp.StdString):Void { this.name = n; }
}
";
    let head = gen_header(src, "User");
    assert!(
        head.contains("eng::IEngine* engine;"),
        "cpp.Pointer<IEngine> → eng::IEngine* (inner @:native rename applied):\n{head}"
    );
    assert!(
        head.contains("std::string name;"),
        "cpp.StdString → std::string:\n{head}"
    );

    let out = gen_one(src, "User");
    assert!(
        out.contains("std::string& n") || out.contains("std::string n"),
        "cpp.StdString parameter lowers to std::string:\n{out}"
    );
}

#[test]
fn cpp_pointer_return_carries_inner_info_for_chained_calls() {
    // Regression: a `cpp.Pointer<T>` result must carry `T`'s `TypeInfo` so a
    // *chained* call resolves (`GetRenderer()->Push(...)`), and an anon literal
    // argument lowers to the callee's struct parameter type — not `void`.
    let src = "\
@:include(\"e.h\") @:native(\"e::Effect\") @:structAccess
typedef Effect = { kind:Int };
@:include(\"e.h\") @:native(\"e::IRenderer\") @:structAccess
extern class IRenderer { public function Push(eff:Effect):Void; }
@:include(\"e.h\") @:native(\"e::IEngine\") @:structAccess
extern class IEngine { public function GetRenderer():cpp.Pointer<IRenderer>; }
class User {
  var engine:cpp.Pointer<IEngine>;
  public function new() {}
  public function go():Void { engine.GetRenderer().Push({ kind: 1 }); }
}
";
    let out = gen_one(src, "User");
    assert!(
        !out.contains("void _anon"),
        "anon arg must not fall back to void:\n{out}"
    );
    assert!(
        out.contains("e::Effect _anon"),
        "anon arg lowers to the struct param type:\n{out}"
    );
    assert!(
        out.contains("engine->GetRenderer()->Push("),
        "chained call resolves and dispatches via `->`:\n{out}"
    );
}
