//! Validation diagnostics: Hatchet must fail loudly on a type it cannot resolve
//! rather than silently guess a by-value spelling. Synthetic inputs exercise the
//! errors.

use hatchet::diag::Severity;
use hatchet::sema::validate::{
    deprecated_meta_warnings, unresolved_type_errors, unsupported_construct_errors,
};
use hatchet::sema::Program;

/// Build a `Program` from a single synthetic `.hx` file in a fresh temp dir.
fn program_from(stem: &str, src: &str) -> (Program, usize) {
    let dir = std::env::temp_dir().join(format!("hatchet_diag_{}_{}", stem, std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{stem}.hx")), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some(stem))
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    (prog, idx)
}

#[test]
fn legacy_decl_and_abi_metas_warn_with_replacement() {
    // The legacy export metadata `@:decl` (class shared-library export) and `@:abi`
    // (`extern "C"` function export) were renamed to `@libexport` / `@cexport`. Using
    // the old tokens must emit a non-fatal deprecation warning naming the replacement
    // — the class hit carries its source line (1), the free function is file-level (0).
    let (prog, idx) = program_from(
        "Legacy",
        "@:decl class Widget {\n  public function new() {}\n}\n@:abi function pick(n:Int):Int { return n; }\n",
    );
    let warns = deprecated_meta_warnings(&prog, idx);

    let decl = warns
        .iter()
        .find(|(_, w)| w.contains("@:decl"))
        .unwrap_or_else(|| panic!("no @:decl warning in {warns:?}"));
    assert!(
        decl.1.contains("@libexport") && decl.1.contains("deprecated") && decl.0 == 1,
        "@:decl should point to @libexport on line 1: {decl:?}"
    );

    let abi = warns
        .iter()
        .find(|(_, w)| w.contains("@:abi"))
        .unwrap_or_else(|| panic!("no @:abi warning in {warns:?}"));
    assert!(
        abi.1.contains("@cexport") && abi.1.contains("deprecated"),
        "@:abi should point to @cexport: {abi:?}"
    );

    // The *new* spellings must NOT warn.
    let (prog2, idx2) = program_from(
        "Modern",
        "@libexport class Widget {\n  public function new() {}\n}\n@cexport function pick(n:Int):Int { return n; }\n",
    );
    assert!(
        deprecated_meta_warnings(&prog2, idx2).is_empty(),
        "@libexport / @cexport must not be flagged as deprecated"
    );
}

#[test]
fn missing_field_type_is_an_error() {
    // `Widget` is never declared anywhere, so the field type cannot resolve.
    // It is written on line 2 of the source, which the error must report.
    let (prog, idx) = program_from(
        "Panel",
        "class Panel {\n  var widget:Widget;\n  public function new() {}\n}\n",
    );
    let errs = unresolved_type_errors(&prog, idx);
    assert!(
        errs.iter().any(|d| d.message.contains("Widget")
            && d.message.contains("field `widget`")
            && d.line == 2),
        "expected an unresolved-type error for Widget on line 2, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn missing_type_in_new_expression_is_an_error() {
    // A type named only in a `new` inside a method body must still be caught.
    let (prog, idx) = program_from(
        "Factory",
        "class Factory {\n  public function new() {}\n  public function make():Void {\n    var w = new Gizmo();\n  }\n}\n",
    );
    let errs = unresolved_type_errors(&prog, idx);
    // `new Gizmo()` is on line 4 of the source.
    assert!(
        errs.iter()
            .any(|d| d.message.contains("Gizmo") && d.line == 4),
        "expected an unresolved-type error for Gizmo on line 4, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn lambda_outside_map_or_final_is_unsupported() {
    // A lambda passed to an ordinary call (not `Array.map`) is not yet supported;
    // it must raise an `Unsupported` diagnostic (carrying the PR invite), reported
    // on the statement's line (4).
    let (prog, idx) = program_from(
        "Cb",
        "class Cb {\n  public function new() {}\n  public function run():Void {\n    register((x) -> x + 1);\n  }\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        errs.iter().any(|d| d.severity == Severity::Unsupported
            && d.message.contains("lambda")
            && d.line == 4),
        "expected an Unsupported lambda diagnostic on line 4, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn lambda_in_map_or_top_level_final_is_supported() {
    // A lambda as an `Array.map` argument and a top-level `final = lambda` are both
    // supported — neither is flagged.
    let (prog, idx) = program_from(
        "Ok2",
        "package p;\n\
         final Sq:(Int)->Int = (a:Int) -> a * a;\n\
         \
         class Ok2 {\n\
           public function new() {}\n\
           public function run(xs:Array<Int>):Void {\n\
             var ys:Array<Int> = xs.map((x) -> x + 1);\n\
           }\n\
         }\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        errs.is_empty(),
        "no unsupported diagnostics expected, got: {:?}",
        errs.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn macro_function_is_unsupported() {
    // A `macro` function is compile-time metaprogramming with no C++ lowering; it
    // must raise an `Unsupported` diagnostic (which carries the PR invite).
    let (prog, idx) = program_from(
        "Builder",
        "class Builder {\n  public function new() {}\n  macro function build():Void {}\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        errs.iter().any(|d| d.severity == Severity::Unsupported
            && d.message.contains("macro")
            && d.message.contains("build")),
        "expected an Unsupported macro diagnostic, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn macro_function_with_reification_body_is_unsupported_not_a_parse_error() {
    // A real macro body contains reification syntax (`macro`, `$x`) that the
    // expression parser cannot handle. Hatchet must skip the body and still
    // report the function as `Unsupported` — not crash with a lex/parse error.
    let (prog, idx) = program_from(
        "Sq",
        "class Sq {\n  public function new() {}\n  \
         macro static function Square(x:Expr) { return macro $x * $x; }\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        errs.iter().any(|d| d.severity == Severity::Unsupported
            && d.message.contains("macro")
            && d.message.contains("Square")),
        "expected an Unsupported macro diagnostic for Square, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn expr_macro_type_is_unsupported_not_unresolved() {
    // The macro AST type `Expr` is unsupported — and reported as `Unsupported`
    // (with the PR invite), NOT as a plain "unresolved type" error.
    let (prog, idx) = program_from(
        "Reify",
        "class Reify {\n  var node:Expr;\n  public function new() {}\n}\n",
    );
    let unsup = unsupported_construct_errors(&prog, idx);
    assert!(
        unsup.iter().any(|d| d.severity == Severity::Unsupported
            && d.message.contains("Expr")
            && d.message.contains("field `node`")
            && d.line == 2),
        "expected an Unsupported Expr diagnostic on line 2, got: {:?}",
        unsup
            .iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
    // It must NOT also surface as an unresolved-type error.
    let unresolved = unresolved_type_errors(&prog, idx);
    assert!(
        !unresolved.iter().any(|d| d.message.contains("Expr")),
        "Expr should not be double-reported as unresolved, got: {:?}",
        unresolved.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn using_static_extension_is_unsupported() {
    // `using` rewrites call sites by type — Hatchet has no such lowering, so a
    // `using` must be flagged (it would otherwise be silently ignored). The
    // declaration is on line 2.
    let (prog, idx) = program_from(
        "Ext",
        "package p;\nusing StringTools;\nclass Ext {\n  public function new() {}\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        errs.iter().any(|d| d.severity == Severity::Unsupported
            && d.message.contains("using")
            && d.message.contains("StringTools")
            && d.line == 2),
        "expected an Unsupported `using` diagnostic on line 2, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn parameterized_enum_variant_is_supported() {
    // `Move(dx:Int, dy:Int)` lowers to the tagged value class — not flagged.
    let (prog, idx) = program_from(
        "Cmd",
        "enum Cmd {\n  Stop;\n  Move(dx:Int, dy:Int);\n}\nclass CmdUse {\n  public function new() {}\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        !errs.iter().any(|d| d.message.contains("Move")),
        "parameterized variants must not be flagged, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn regex_literal_is_unsupported() {
    // The `~/pattern/flags` regex literal is not transpiled; it must raise an
    // `Unsupported` diagnostic (which carries the PR invite) on its statement line.
    let (prog, idx) = program_from(
        "Matcher",
        "class Matcher {\n  public function new() {}\n  public function check():Void {\n    var r = ~/haxe/i;\n  }\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        errs.iter().any(|d| d.severity == Severity::Unsupported
            && d.message.contains("regular-expression literal")
            && d.line == 4),
        "expected an Unsupported regex-literal diagnostic on line 4, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn ereg_type_is_unsupported_not_unresolved() {
    // `EReg` (the std regex type) is unsupported — reported as `Unsupported` (with
    // the PR invite), not as a plain "unresolved type".
    let (prog, idx) = program_from(
        "Rx",
        "class Rx {\n  public function new() {}\n  public function go():Void {\n    var r = new EReg(\"haxe\", \"i\");\n  }\n}\n",
    );
    let unsup = unsupported_construct_errors(&prog, idx);
    assert!(
        unsup.iter().any(|d| d.severity == Severity::Unsupported
            && d.message.contains("regular-expression type")
            && d.message.contains("EReg")
            && d.line == 4),
        "expected an Unsupported EReg diagnostic on line 4, got: {:?}",
        unsup
            .iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
    let unresolved = unresolved_type_errors(&prog, idx);
    assert!(
        !unresolved.iter().any(|d| d.message.contains("EReg")),
        "EReg should not be double-reported as unresolved, got: {:?}",
        unresolved.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn try_catch_with_an_unresolved_catch_type_is_an_error() {
    // try/catch is transpiled (see `source_codegen` / the compile gate), but the
    // caught type is real C++ — an unresolved one must still be flagged.
    let (prog, idx) = program_from(
        "Guard",
        "class Guard {\n  public function new() {}\n  public function run():Void {\n    try {} catch (e:Nope) {}\n  }\n}\n",
    );
    let errs = unresolved_type_errors(&prog, idx);
    assert!(
        errs.iter()
            .any(|d| d.message.contains("Nope") && d.line == 4),
        "expected an unresolved-type error for the catch type `Nope` on line 4, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn is_operator_is_unsupported() {
    // The Haxe 4.2 `expr is Type` operator is recognised in operator position and
    // flagged (it would otherwise be an "unexpected token") on its statement line (4).
    let (prog, idx) = program_from(
        "Check",
        "class Check {\n  public function new() {}\n  public function test(x:Dynamic):Bool {\n    return x is Check;\n  }\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        errs.iter().any(|d| d.severity == Severity::Unsupported
            && d.message.contains("type-check operator")
            && d.line == 4),
        "expected an Unsupported `is` diagnostic on line 4, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn is_is_a_contextual_keyword_not_reserved() {
    // Haxe 4 treats `is` as a *contextual* keyword: only `expr is Type` is the
    // operator (flagged above). `is` otherwise remains an ordinary identifier — a
    // local variable or a field/method name — and must NOT be flagged. This pins the
    // contextual behavior so it can't regress into a reserved keyword by accident.
    let (prog, idx) = program_from(
        "Ctx",
        "class Ctx {\n  public function new() {}\n  \
         public function f():Int { var is = 5; return is; }\n  \
         public function g(o:Dynamic):Dynamic { return o.is; }\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        !errs
            .iter()
            .any(|d| d.message.contains("type-check operator")),
        "`is` as an identifier/field must not be flagged as the operator, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn abstract_newtype_is_supported() {
    // An `abstract Name(U)` newtype now lowers to a value class — no longer
    // flagged as unsupported.
    let (prog, idx) = program_from(
        "Meters",
        "package p;\nabstract Meters(Float) {\n  public inline function new(v:Float) { this = v; }\n  public function doubled():Float { return this * 2.0; }\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        !errs.iter().any(|d| d.message.contains("`abstract`")),
        "an abstract newtype must not be flagged unsupported, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn primitives_containers_and_declared_types_are_not_flagged() {
    // Int/String/Array/Map/Null and a locally-declared struct all resolve.
    let (prog, idx) = program_from(
        "Ok",
        "typedef Pt = { var x:Int; var y:Int; }\n\
         \
         class Ok {\n\
           var n:Int;\n\
           var name:String;\n\
           var pts:Array<Pt>;\n\
           var table:Map<String, Int>;\n\
           var maybe:Null<Pt>;\n\
           public function new() {}\n\
         }\n",
    );
    let errs = unresolved_type_errors(&prog, idx);
    assert!(
        errs.is_empty(),
        "no errors expected, got: {:?}",
        errs.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Generics — no C++98 template lowering, so every type-parameterized
// declaration must fail loudly instead of emitting `T` as a bare unknown type.
// ---------------------------------------------------------------------------

#[test]
fn generic_class_is_unsupported() {
    let (prog, idx) = program_from("Box", "class Box<T> {\n  public function new() {}\n}\n");
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        errs.iter().any(|d| d.severity == Severity::Unsupported
            && d.message.contains("generic class `Box<T>`")
            && d.line == 1),
        "expected an Unsupported generic-class diagnostic on line 1, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn generic_interface_is_unsupported() {
    let (prog, idx) = program_from(
        "Cmp",
        "interface Cmp<A, B> {\n  function compare(a:A, b:B):Int;\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        errs.iter().any(|d| d.severity == Severity::Unsupported
            && d.message.contains("generic interface `Cmp<A, B>`")
            && d.line == 1),
        "expected an Unsupported generic-interface diagnostic on line 1, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn generic_method_is_unsupported_and_t_is_not_double_reported() {
    // The method is flagged once; its `T` uses must not also surface as
    // unresolved types (that would be a misleading double report).
    let (prog, idx) = program_from(
        "Util",
        "class Util {\n  public function new() {}\n  public function first<T>(items:Array<T>):T {\n    return items[0];\n  }\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        errs.iter().any(|d| d.severity == Severity::Unsupported
            && d.message.contains("generic method `first<T>`")),
        "expected an Unsupported generic-method diagnostic, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
    let unresolved = unresolved_type_errors(&prog, idx);
    assert!(
        unresolved.is_empty(),
        "`T` must not double-report as unresolved, got: {:?}",
        unresolved.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn generic_typedef_is_unsupported() {
    let (prog, idx) = program_from(
        "Pairs",
        "typedef Pair<T> = { var a:T; var b:T; }\nclass Pairs {\n  public function new() {}\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        errs.iter().any(|d| d.severity == Severity::Unsupported
            && d.message.contains("generic typedef `Pair<T>`")
            && d.line == 1),
        "expected an Unsupported generic-typedef diagnostic on line 1, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Property accessors — only `(default, set)`, `(default, null)` and
// `(default, never)` have a lowering; custom accessor logic must fail loudly
// rather than be silently replaced by trivial generated accessors.
// ---------------------------------------------------------------------------

#[test]
fn get_set_property_pair_is_supported() {
    // Full `(get, set)`: both accessors are real methods; reads route through
    // `get_celsius()`, writes through `set_celsius()` — nothing is flagged.
    let (prog, idx) = program_from(
        "Temp",
        "class Temp {\n  public var celsius(get, set):Float;\n  function get_celsius():Float { return 0.0; }\n  function set_celsius(v:Float):Float { return v; }\n  public function new() {}\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        errs.is_empty(),
        "(get, set) with both accessors must not be flagged, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn user_defined_setter_body_is_supported() {
    // `(default, set)` with a user-written `set_x`: real Haxe semantics — every
    // write routes through it — so nothing is flagged. (Without a `set_x`,
    // Hatchet's dialect generates a trivial `SetX` instead.)
    let (prog, idx) = program_from(
        "Gauge",
        "class Gauge {\n  public var level(default, set):Int;\n  function set_level(v:Int):Int {\n    level = v > 0 ? v : 0;\n    return level;\n  }\n  public function new() {}\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        errs.is_empty(),
        "(default, set) with a custom set_x must not be flagged, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn supported_property_pairs_are_not_flagged() {
    // `(default, set)`, `(default, null)` and `(default, never)` (without custom
    // accessor bodies) all lower today — none may be flagged.
    let (prog, idx) = program_from(
        "Props",
        "class Props {\n  public var a(default, set):Int;\n  public var b(default, null):Int;\n  public var c(default, never):Int;\n  public function new() {}\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        !errs.iter().any(|d| d.message.contains("property")),
        "supported pairs must not be flagged, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Switch patterns — only constant patterns (literals, enum members) lower to
// C++ case labels; captures and destructuring must fail loudly.
// ---------------------------------------------------------------------------

#[test]
fn capture_pattern_is_unsupported() {
    let (prog, idx) = program_from(
        "Cap",
        "class Cap {\n  public function new() {}\n  public function run(n:Int):Int {\n    switch (n) {\n      case 0: return 0;\n      case x: return x + 1;\n    }\n  }\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        errs.iter().any(|d| d.severity == Severity::Unsupported
            && d.message.contains("capture pattern `case x:`")),
        "expected an Unsupported capture-pattern diagnostic, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn destructuring_enum_pattern_is_supported() {
    // Parameterized variants lower to the tagged value class; `case Add(a, b):`
    // binds the payload — nothing is flagged.
    let (prog, idx) = program_from(
        "Ev",
        "enum Op { Halt; Add(a:Int, b:Int); }\nclass Ev {\n  public function new() {}\n  public function eval(o:Op):Int {\n    switch (o) {\n      case Add(a, b): return a + b;\n      default: return 0;\n    }\n  }\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        errs.is_empty(),
        "destructuring a parameterized variant must not be flagged, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn literal_payload_subpattern_is_unsupported() {
    // Only plain captures and `_` are lowered in payload positions.
    let (prog, idx) = program_from(
        "Lit",
        "enum Op { Halt; Add(a:Int, b:Int); }\nclass Lit {\n  public function new() {}\n  public function eval(o:Op):Int {\n    switch (o) {\n      case Add(0, b): return b;\n      default: return 0;\n    }\n  }\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        errs.iter().any(|d| d.severity == Severity::Unsupported
            && d.message.contains("non-capture payload sub-pattern")),
        "expected a payload-sub-pattern diagnostic, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn recursive_enum_payload_is_unsupported() {
    // A by-value tagged class cannot contain itself (incomplete type in C++).
    let (prog, idx) = program_from(
        "Trees2",
        "enum Tree2 {\n  Leaf;\n  Node(value:Int, child:Tree2);\n}\nclass Trees2 {\n  public function new() {}\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        errs.iter().any(|d| d.severity == Severity::Unsupported
            && d.message
                .contains("recursive enum payload `Tree2` in variant `Node`")),
        "expected a recursive-payload diagnostic, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn constant_patterns_are_not_flagged() {
    // Literals (incl. negative), bare enum members, and qualified enum members
    // are all constant patterns with a lowering — none may be flagged.
    let (prog, idx) = program_from(
        "Konst",
        "enum Color { Red; Green; Blue; }\nclass Konst {\n  public function new() {}\n  public function hue(c:Color, n:Int, s:String):Int {\n    switch (c) {\n      case Red: return 0;\n      case Color.Green: return 120;\n      default: return 240;\n    }\n    switch (n) {\n      case -1: return 0;\n      case 0 | 1: return 1;\n      case _: return 2;\n    }\n    switch (s) {\n      case \"on\": return 1;\n      default: return 0;\n    }\n  }\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        !errs.iter().any(|d| d.message.contains("pattern")),
        "constant patterns must not be flagged, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn generic_enum_is_unsupported() {
    let (prog, idx) = program_from(
        "Trees",
        "enum Tree<T> {\n  Leaf;\n  Node(v:T);\n}\nclass Trees {\n  public function new() {}\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        errs.iter().any(|d| d.severity == Severity::Unsupported
            && d.message.contains("generic enum `Tree<T>`")
            && d.line == 1),
        "expected an Unsupported generic-enum diagnostic on line 1, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Property accessors, tiers 1 & 2: access-control pairs and computed
// `(get, null)` / `(get, never)` getters are supported; custom setters and
// `dynamic` access stay flagged; `get` without a `get_x` method is an error.
// ---------------------------------------------------------------------------

#[test]
fn access_control_property_pairs_are_not_flagged() {
    let (prog, idx) = program_from(
        "Acc",
        "class Acc {\n  public var a(null, default):Int;\n  var b(null, null):Int;\n  public var c(never, default):Int;\n  var d(never, null):Int;\n  public var e(null, set):Int;\n  public function new() {}\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        !errs.iter().any(|d| d.message.contains("property")),
        "access-control pairs must not be flagged, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn computed_getter_property_is_not_flagged() {
    let (prog, idx) = program_from(
        "Comp",
        "class Comp {\n  public var area(get, never):Float;\n  public var perim(get, null):Float;\n  function get_area():Float { return 1.0; }\n  function get_perim():Float { return 2.0; }\n  public function new() {}\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        errs.is_empty(),
        "computed (get, never)/(get, null) must not be flagged, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn get_default_pair_is_unsupported() {
    // `(get, default)` has no coherent lowering: a custom getter implies a
    // private backing field, but `default` write access needs it directly
    // writable from outside. It stays flagged.
    let (prog, idx) = program_from(
        "Gd",
        "class Gd {\n  public var x(get, default):Int;\n  function get_x():Int { return 0; }\n  public function new() {}\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        errs.iter().any(|d| d.severity == Severity::Unsupported
            && d.message
                .contains("`(get, default)` property accessor pair on `x`")),
        "expected (get, default) to stay flagged, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn get_access_without_get_method_is_an_error() {
    let (prog, idx) = program_from(
        "NoGet",
        "class NoGet {\n  public var area(get, never):Float;\n  public function new() {}\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        errs.iter().any(|d| d.severity == Severity::Error
            && d.message
                .contains("declares `get` access but no `get_area()` method")
            && d.line == 2),
        "expected a missing-get_area error on line 2, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Value classes (`@:stackOnly`) — shapes value semantics can't express, and the
// stack-residence (no-nesting) rule. `abstract` newtypes are the nestable value
// type and are exercised at the end of this block.
// ---------------------------------------------------------------------------

#[test]
fn value_class_inheritance_is_unsupported() {
    // Slicing: a value type cannot dispatch polymorphically through a base.
    let (prog, idx) = program_from(
        "Sh",
        "@:stackOnly class Base { public function new() {} }\n@:stackOnly class Sh extends Base { public function new() { super(); } }\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        errs.iter().any(|d| d.severity == Severity::Unsupported
            && d.message.contains("inheritance on the value class `Sh`")),
        "expected a value-class inheritance diagnostic, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn value_class_self_by_value_field_is_unsupported() {
    // A direct `self:Self` field is an incomplete-type member (a value type cannot
    // contain itself by value).
    let (prog, idx) = program_from(
        "Node",
        "@:stackOnly class Node {\n  public var self:Node;\n  public function new() {}\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        errs.iter().any(|d| d.severity == Severity::Unsupported
            && d.message.contains("by-value self-field `self`")),
        "expected a by-value self-field diagnostic, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn stack_only_nesting_is_unsupported_steering_to_abstract() {
    // `@:stackOnly` carries hxcpp's stack-residence rule: it may not be a field
    // (or container element) of anything — an `abstract` newtype is the nestable
    // value type. Both the direct field and the `Array<>` element are flagged.
    let (prog, idx) = program_from(
        "Use",
        "@:stackOnly class Vec2 { public var x:Float; public function new() { x = 0.0; } }\nclass Entity {\n  public var pos:Vec2;\n  public var trail:Array<Vec2>;\n  public function new() {}\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    let msgs: Vec<&String> = errs.iter().map(|d| &d.message).collect();
    assert!(
        msgs.iter().any(|m| m.contains("`@:stackOnly` type `Vec2`")
            && m.contains("field `pos`")
            && m.contains("use an `abstract`")),
        "expected a stack-only-as-field diagnostic steering to an abstract, got: {msgs:?}"
    );
    assert!(
        msgs.iter().any(|m| m.contains("field `trail`")),
        "the Array<Vec2> element should be flagged too, got: {msgs:?}"
    );
}

#[test]
fn abstract_value_type_may_be_nested() {
    // An `abstract Name(U)` is a value type that nests freely: as a field and as a
    // container element, with no diagnostic (unlike `@:stackOnly`).
    let (prog, idx) = program_from(
        "World",
        "typedef Vec2Data = { var x:Float; }\nabstract Vec2(Vec2Data) { public function new() { this = { x: 0.0 }; } }\nclass Entity {\n  public var pos:Vec2;\n  public var trail:Array<Vec2>;\n  public function new() {}\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        errs.is_empty(),
        "an `abstract` value type must nest freely, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn switch_case_final_constant_is_not_a_capture() {
    // A `final` constant used as a bare `case` pattern must NOT be flagged as a
    // capture variable (the regression where finals tripped the pattern check).
    let (prog, idx) = program_from(
        "Disp",
        "final A_ID:Int = 0;\nfinal B_ID:Int = 1;\nclass Disp {\n  public function new() {}\n  public function pick(id:Int):Int {\n    switch id { case A_ID: return 1; case B_ID: return 2; default: return 0; }\n  }\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        !errs.iter().any(|d| d.message.contains("capture pattern")),
        "final constants must not be treated as capture patterns, got: {:?}",
        errs.iter()
            .map(|d| (d.line, &d.message))
            .collect::<Vec<_>>()
    );
}
