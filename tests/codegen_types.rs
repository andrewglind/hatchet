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
fn alias_typedef_to_primitive_is_a_value_not_a_struct() {
    // `typedef Color = cpp.UInt32` aliases a primitive, so a `Color` parameter is
    // passed by value (an optional one gets a default), NOT by `const Color&` and
    // not as a `Color*` pointer — the alias must be resolved through before the
    // value-vs-reference decision. A `typedef` to a `{ … }` struct still lowers to
    // the value-struct shape (`const V&`, optional `V*`).
    let src = "\
typedef Color = cpp.UInt32;
typedef Pt = { var x:Int; var y:Int; };
class Painter {
  public function new() {}
  public function tint(c:Color):Color { return c; }
  public function opt(?c:Color):Color { return this.tint(0); }
  public function move(p:Pt):Void {}
  public function optPt(?p:Pt):Void {}
}
";
    let out = gen_one(src, "Painter");
    assert!(
        out.contains("Color Painter::tint(Color c)"),
        "primitive alias param is by value:\n{out}"
    );
    assert!(
        !out.contains("const Color&") && !out.contains("Color* c"),
        "primitive alias is neither const-ref nor pointer:\n{out}"
    );
    assert!(
        out.contains("Color Painter::opt(Color c"),
        "optional primitive alias is a defaulted value, not a pointer:\n{out}"
    );
    // The struct alias is unaffected — still a value struct.
    assert!(
        out.contains("void Painter::move(const Pt& p)"),
        "struct typedef stays a const-ref value struct:\n{out}"
    );
    assert!(
        out.contains("void Painter::optPt(Pt* p)"),
        "optional struct typedef stays a pointer:\n{out}"
    );
}

#[test]
fn alias_typedef_inherits_its_target_shape() {
    // A Haxe `typedef` is transparent: an alias takes on the value-vs-reference shape of
    // whatever it names, not "typedef, therefore value struct". Covers the shapes that
    // used to be mis-lowered — reference class (slicing), container (by-value copy),
    // string (lost const-ref), optional-string default, and `Null<pointer-alias>`
    // (double pointer) — plus member/method dispatch through the alias.
    let src = "\
typedef Name  = String;
typedef Ints  = Array<Int>;
typedef Ptr   = cpp.RawPointer<cpp.UInt8>;
typedef Pt    = { var x:Int; var y:Int; };
typedef Vertex = Pt;
class Widget {
  public var id:Int;
  public function new() { this.id = 0; }
  public function tag():Int { return this.id; }
}
typedef Panel = Widget;
class Uses {
  public function new() {}
  public function name(n:Name):Void {}
  public function ints(xs:Ints):Void {}
  public function ptr(p:Ptr):Void {}
  public function panel(w:Panel):Int { return w.tag(); }
  public function vertex(v:Vertex):Int { return v.x; }
  public function optName(?n:Name):Void {}
  public function nulPtr(p:Null<Ptr>):Void {}
}
";
    let out = gen_one(src, "Uses");
    // A reference-class alias is a pointer, not a sliced by-value copy — and dispatches `->`.
    assert!(
        out.contains("int Uses::panel(Panel* w)") && out.contains("return w->tag();"),
        "class alias is a pointer with `->` dispatch:\n{out}"
    );
    // A struct alias is a const-ref value struct whose fields resolve.
    assert!(
        out.contains("int Uses::vertex(const Vertex& v)") && out.contains("return v.x;"),
        "struct alias is a const-ref value struct with working field access:\n{out}"
    );
    // A container alias is passed by const-ref (Haxe reference semantics), not by value.
    assert!(
        out.contains("void Uses::ints(const Ints& xs)"),
        "container alias is a const-ref, not a by-value copy:\n{out}"
    );
    // A string alias keeps the const-ref optimization; an optional one defaults to "".
    assert!(
        out.contains("void Uses::name(const Name& n)"),
        "string alias keeps const-ref:\n{out}"
    );
    assert!(
        out.contains("void Uses::optName(Name n"),
        "optional string alias is a by-value `Name`, not a pointer:\n{out}"
    );
    // A pointer-interop alias passes by value, and `Null<Ptr>` stays a single pointer.
    assert!(
        out.contains("void Uses::ptr(Ptr p)") && out.contains("void Uses::nulPtr(Ptr p)"),
        "pointer alias passes by value; Null<Ptr> is not a double pointer:\n{out}"
    );
}

#[test]
fn module_function_called_via_class_name_drops_the_class_qualifier() {
    // Haxe allows `Module.func()` where `func` is a *module-level* function, not a
    // member of the primary class `Module` (the module name doubles as the class
    // name). Such a function lowers to a namespace free function, so the class
    // qualifier must be erased: `func(...)`, never `Module::func(...)`.
    let src = "\
class Palette {
  public var count:Int;
  public function new() { this.count = 0; }
}
function mix(a:Int, b:Int):Int { return a + b; }
class User {
  public function new() {}
  public function go():Int { return Palette.mix(2, 3); }
}
";
    let out = gen_one(src, "User");
    assert!(
        out.contains("return mix(2, 3);"),
        "module function called via the class name drops the qualifier:\n{out}"
    );
    assert!(
        !out.contains("Palette::mix"),
        "no class scope-resolution for a module-level free function:\n{out}"
    );
}

#[test]
fn static_member_call_still_uses_scope_resolution_when_a_module_fn_shadows() {
    // The module-function rewrite must not swallow a *genuine* static member: when
    // the primary class declares the method, `Type.method()` stays `Type::method()`.
    let src = "\
class Registry {
  public function new() {}
  public static function slot(i:Int):Int { return i; }
}
class User {
  public function new() {}
  public function go():Int { return Registry.slot(7); }
}
";
    let out = gen_one(src, "User");
    assert!(
        out.contains("Registry::slot(7)"),
        "a real static member keeps scope resolution:\n{out}"
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

#[test]
fn float32_contexts_take_suffixed_float_literals() {
    // A bare C++ floating literal is a `double`, so one landing in a `cpp.Float32`
    // (C++ `float`) context narrows at the conversion — MSVC reports C4305 on every
    // such line, and a VC6 build is expected to compile clean. In a `float` context
    // the literal is emitted `0.1f`; a genuine `Float`/`double` context is untouched.
    let src = "\
@:include(\"t.h\") @:native(\"Target\") extern class Target {
  public function F32(a:cpp.Float32, b:cpp.Float32):Void;
  public function F64(a:Float, b:Float):Void;
}
class Use {
  var fx:cpp.Float32;
  var dx:Float;
  public function new() { this.fx = 0.1; this.dx = 0.1; }
  public function calls(t:Target):Void {
    t.F32(70.0, 0.1);
    t.F64(70.0, 0.1);
  }
  public function locals():Void {
    var f:cpp.Float32 = 0.1;
    var d:Float = 0.1;
    f = 0.25;
    this.fx = 0.3;
  }
  public function ret():cpp.Float32 { return 0.1; }
  public function arr():Array<cpp.Float32> {
    var xs:Array<cpp.Float32> = [0.1];
    xs.push(0.2);
    return xs;
  }
  public function arith(a:cpp.Float32):cpp.Float32 { return a * 0.5; }
}
";
    let out = gen_one(src, "Use");
    assert!(
        out.contains("t->F32(70.0f, 0.1f);"),
        "literal arguments to `cpp.Float32` parameters are suffixed:\n{out}"
    );
    assert!(
        out.contains("t->F64(70.0, 0.1);"),
        "a `Float` (double) parameter is left alone:\n{out}"
    );
    assert!(
        out.contains("float f = 0.1f;")
            && out.contains("double d = 0.1;")
            && out.contains("f = 0.25f;")
            && out.contains("this->fx = 0.3f;")
            && out.contains("this->fx = 0.1f;")
            && out.contains("this->dx = 0.1;"),
        "`cpp.Float32` locals and fields are suffixed, `Float` ones are not:\n{out}"
    );
    assert!(
        out.contains("return 0.1f;"),
        "a literal returned from a `cpp.Float32` function is suffixed:\n{out}"
    );
    assert!(
        out.contains("xs.push_back(0.1f);") && out.contains("xs.push_back(0.2f);"),
        "`Array<cpp.Float32>` elements are suffixed:\n{out}"
    );
    // Haxe arithmetic on `Float` is double arithmetic whatever it is assigned to,
    // so an operand keeps its `double` literal — suffixing it there would make the
    // computation single-precision, a behaviour change rather than a cosmetic one.
    // The narrowing back to `float` is instead made explicit with a cast, which is
    // what the conversion does anyway (and is what silences C4244).
    assert!(
        out.contains("return (float)(a * 0.5);"),
        "an arithmetic operand stays a double literal, narrowed by an explicit cast:\n{out}"
    );
}

#[test]
fn narrowing_conversions_are_explicit_casts() {
    // A value stored into a smaller scalar than it has narrows at the conversion.
    // Hatchet makes that explicit — `(uint16_t)(...)` — rather than relying on the
    // implicit conversion: it is what the conversion does anyway, it keeps MSVC
    // quiet (C4244), and VC6 `/O2` has been seen miscompiling an implicit
    // `int` → `uint16_t` narrowing of a loop-derived value.
    let src = "\
class Mesh {
  var small:cpp.UInt16;
  public function new() { this.small = 0; }
  public function indices(quads:Int):Array<cpp.UInt16> {
    var out:Array<cpp.UInt16> = [];
    var n:cpp.UInt16 = 0;
    for (q in 0...quads) {
      var base:Int = q * 4;
      out.push(base + 1);
      n = n + 1;
    }
    this.small = n;
    return out;
  }
  public function wide(a:Int, b:Int):Int { return a + b; }
}
";
    let out = gen_one(src, "Mesh");
    assert!(
        out.contains("uint16_t _elem2 = (uint16_t)(base + 1);"),
        "an `int` expression narrowed into a `cpp.UInt16` element is cast:\n{out}"
    );
    // `n + 1` promotes to `int` in C++ even though both sides are `UInt16`, so
    // storing it back into a `uint16_t` narrows and is cast too.
    assert!(
        out.contains("n = (uint16_t)(n + 1);"),
        "an arithmetic result promoted to `int` is cast back:\n{out}"
    );
    assert!(
        out.contains("this->small = n;"),
        "a same-typed value needs no cast:\n{out}"
    );
    assert!(
        out.contains("uint16_t n = 0;") && !out.contains("(uint16_t)(0)"),
        "a literal is written in the target type, uncast:\n{out}"
    );
    assert!(
        out.contains("return a + b;"),
        "a widening / same-width context is untouched:\n{out}"
    );
}
