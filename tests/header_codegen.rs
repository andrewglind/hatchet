//! Header-generation rules, checked against small self-contained programs built
//! in a temp directory (no external dependencies).

use hatchet::codegen::generate_header;
use hatchet::sema::Program;

fn module_index(prog: &Program, stem: &str) -> usize {
    prog.modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some(stem))
        .unwrap_or_else(|| panic!("module {stem} not found"))
}

fn header(prog: &Program, stem: &str) -> String {
    generate_header(prog, module_index(prog, stem))
        .unwrap_or_else(|| panic!("no header generated for {stem}"))
}

#[test]
fn decl_class_uses_the_portable_export_macro() {
    // `@:decl class X {}` exports the class from the DLL via the portable
    // `<PREFIX>_CLASS` macro (default `HATCHET_CLASS`) — never the raw, MSVC-only
    // `__declspec(dllexport)` token. The prefix is configurable on `Program`.
    let dir = std::env::temp_dir().join(format!("hatchet_decl_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("Widget.hx"),
        "package ui;\n@:decl class Widget {\n  public function new() {}\n}\n",
    )
    .unwrap();
    let mut prog = Program::from_src_dir(&dir).expect("build program");
    prog.export_macro = "NATIVE".to_string();
    let idx = module_index(&prog, "Widget");
    let out = generate_header(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        out.contains("class NATIVE_CLASS Widget"),
        "@:decl → macro-decorated class:\n{out}"
    );
    assert!(
        !out.contains("__declspec"),
        "no raw MSVC token leaks into output:\n{out}"
    );
}

#[test]
fn base_method_overridden_by_a_subclass_is_virtual() {
    // Haxe methods are virtual by default. A base method that a subclass
    // overrides must be emitted `virtual` in the base, or a call through a base
    // pointer would static-bind to the base version. A base method that nobody
    // overrides stays non-virtual.
    let dir = std::env::temp_dir().join(format!("hatchet_virt_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("Base.hx"),
        "package demo;\nclass Base {\n  public function new() {}\n  \
         public function area():Float { return 0.0; }\n  \
         public function label():String { return \"base\"; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("Derived.hx"),
        "package demo;\nclass Derived extends Base {\n  public function new() { super(); }\n  \
         override public function area():Float { return 1.0; }\n}\n",
    )
    .unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let base = header(&prog, "Base");
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        base.contains("virtual double area();"),
        "overridden base method must be virtual:\n{base}"
    );
    assert!(
        base.contains("std::string label();") && !base.contains("virtual std::string label();"),
        "a method no subclass overrides stays non-virtual:\n{base}"
    );
}

#[test]
fn int_enum_abstract_lowers_to_a_cpp_enum_with_values() {
    // `enum abstract X(Int)` becomes the pre-C++11 `struct X_ { enum Enum { … } }`
    // idiom: members with an explicit value emit `Name = <expr>` (including
    // sibling-referencing bit-flag expressions), and a value-less member relies on
    // C++ auto-increment, exactly as a plain enum would.
    let dir = std::env::temp_dir().join(format!("hatchet_ea_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("Flag.hx"),
        "package p;\nenum abstract Flag(Int) {\n  var None;\n  var A = 1;\n  var B = 2;\n  var AB = A | B;\n  var Shift = 1 << 4;\n}\n",
    )
    .unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let out = header(&prog, "Flag");
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        out.contains("struct Flag_ {") && out.contains("enum Enum {"),
        "enum struct idiom:\n{out}"
    );
    assert!(
        out.contains("typedef Flag_::Enum Flag;"),
        "enum typedef:\n{out}"
    );
    assert!(
        out.contains("A = 1") && out.contains("B = 2"),
        "explicit values emitted:\n{out}"
    );
    assert!(
        out.contains("AB = A | B"),
        "sibling bit-flag expression emitted:\n{out}"
    );
    assert!(
        out.contains("Shift = 1 << 4"),
        "shift expression emitted:\n{out}"
    );
    // A value-less member is emitted bare (auto-increment), with no `= ` suffix.
    assert!(
        out.contains("None,") || out.contains("None\n"),
        "value-less member emitted bare:\n{out}"
    );
}

#[test]
fn abstract_function_is_a_pure_virtual_method() {
    // `abstract function f():T;` (in an `abstract class`) has no body, so it must be
    // emitted as a pure virtual `virtual T f() = 0;` — declared, never defined —
    // rather than a plain `virtual T f();` (which would be an undefined symbol).
    let dir = std::env::temp_dir().join(format!("hatchet_absfn_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("Shape.hx"),
        "package p;\nabstract class Shape {\n  public function new() {}\n  \
         public abstract function area():Float;\n  \
         public function describe():String { return \"shape\"; }\n}\n",
    )
    .unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let out = header(&prog, "Shape");
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        out.contains("virtual double area() = 0;"),
        "abstract method → pure virtual:\n{out}"
    );
    // A concrete method is not made pure virtual.
    assert!(
        out.contains("std::string describe();") && !out.contains("describe() = 0"),
        "concrete method stays defined:\n{out}"
    );
}

#[test]
fn string_enum_abstract_lowers_to_namespaced_static_consts() {
    // A `String`-backed `enum abstract` has no integral enum representation, so it
    // becomes a namespace of typed `static const` constants (header-only); the type
    // itself maps to `std::string`, and members are referenced as `Suit_::Member`.
    let dir = std::env::temp_dir().join(format!("hatchet_eas_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("Suit.hx"),
        "package p;\nenum abstract Suit(String) {\n  var Hearts = \"H\";\n  var Spades = \"S\";\n}\n\
         class Use {\n  public function new() {}\n  public function f():Suit { return Hearts; }\n}\n",
    )
    .unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let out = header(&prog, "Suit");
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        out.contains("namespace Suit_ {"),
        "members live in a `Suit_` namespace:\n{out}"
    );
    assert!(
        out.contains("static const std::string Hearts = \"H\";"),
        "string member → static const std::string:\n{out}"
    );
    assert!(
        !out.contains("enum Enum"),
        "no C++ enum for a non-integral backing:\n{out}"
    );
    assert!(
        !out.contains("typedef"),
        "no typedef — the type maps straight to std::string:\n{out}"
    );
    // The method returns `Suit`, which maps to the underlying `std::string`.
    assert!(
        out.contains("std::string f();"),
        "Suit maps to std::string in signatures:\n{out}"
    );
}

#[test]
fn plain_module_function_is_declared_after_the_types_it_uses() {
    // A plain module-level `function f(...)` becomes a namespace free function: a
    // PUBLIC one is declared in the header, AFTER the type definitions its signature
    // references (so `function makeVec():Vec2` sees `struct Vec2`); a `private` one
    // is `static` in the `.cpp` and must NOT appear in the header.
    let dir = std::env::temp_dir().join(format!("hatchet_modfn_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("Geom.hx"),
        "package p;\ntypedef Vec2 = { x:Float, y:Float };\n\
         function makeVec(x:Float, y:Float):Vec2 { return { x: x, y: y }; }\n\
         private function helper(v:Float):Float { return v * v; }\n",
    )
    .unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let out = header(&prog, "Geom");
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        out.contains("Vec2 makeVec(double x, double y);"),
        "public function declared in header:\n{out}"
    );
    assert!(
        !out.contains("helper"),
        "private function is static in the .cpp, not in the header:\n{out}"
    );
    // The `struct Vec2` definition must precede the function that returns it.
    let struct_at = out.find("struct Vec2").expect("Vec2 struct emitted");
    let fn_at = out.find("Vec2 makeVec").expect("makeVec declared");
    assert!(
        struct_at < fn_at,
        "the type must be defined before the function that uses it:\n{out}"
    );
}

#[test]
fn deeply_nested_generic_type_splits_the_unsigned_shift_token() {
    // `Array<Array<Array<Int>>>` ends in `>>>`, which the lexer greedily merges
    // into the unsigned-shift token — the type parser must split it back into
    // closing angle brackets (C++98 also requires a space between the `>`s).
    let dir = std::env::temp_dir().join(format!("hatchet_nested_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("Grid.hx"),
        "class Grid {\n  public var cells:Array<Array<Array<Int>>>;\n  public function new() {}\n}\n",
    )
    .unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let out = header(&prog, "Grid");
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        out.contains("std::vector<std::vector<std::vector<int> > > cells;"),
        "triple-nested array field must parse and map:\n{out}"
    );
}

#[test]
fn mutually_recursive_classes_get_targeted_forward_declarations() {
    // `A` references `B` before `B` is defined, so `B` is forward-declared; `A`
    // is defined first and is not forward-declared (targeted, not blanket).
    let dir = std::env::temp_dir().join(format!("hatchet_fwd_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("Pair.hx"),
        "class A {\n  public var b:B;\n  public function new() {}\n  public function make():B { return b; }\n}\nclass B {\n  public var a:A;\n  public function new() {}\n}\n",
    )
    .unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let out = header(&prog, "Pair");
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        out.contains("class B;"),
        "B is referenced before its definition → forward-declared:\n{out}"
    );
    assert!(
        !out.contains("class A;"),
        "A is defined first → no forward declaration (targeted):\n{out}"
    );
    // the forward declaration precedes the class definition that needs it
    let fwd = out.find("class B;").unwrap();
    let def = out.find("class A {").unwrap();
    assert!(
        fwd < def,
        "forward declaration must precede the referring class:\n{out}"
    );
}
