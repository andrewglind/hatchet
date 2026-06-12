//! Validation diagnostics: Hatchet must fail loudly on a type it cannot resolve
//! rather than silently guess a by-value spelling. Synthetic inputs exercise the
//! errors.

use hatchet::diag::Severity;
use hatchet::sema::validate::{unresolved_type_errors, unsupported_construct_errors};
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
        errs.iter().map(|d| (d.line, &d.message)).collect::<Vec<_>>()
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
        errs.iter().any(|d| d.message.contains("Gizmo") && d.line == 4),
        "expected an unresolved-type error for Gizmo on line 4, got: {:?}",
        errs.iter().map(|d| (d.line, &d.message)).collect::<Vec<_>>()
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
        errs.iter().map(|d| (d.line, &d.message)).collect::<Vec<_>>()
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
    assert!(errs.is_empty(), "no unsupported diagnostics expected, got: {:?}", errs.iter().map(|d| &d.message).collect::<Vec<_>>());
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
        errs.iter().map(|d| (d.line, &d.message)).collect::<Vec<_>>()
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
        errs.iter().map(|d| (d.line, &d.message)).collect::<Vec<_>>()
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
        unsup.iter().map(|d| (d.line, &d.message)).collect::<Vec<_>>()
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
        errs.iter().map(|d| (d.line, &d.message)).collect::<Vec<_>>()
    );
}

#[test]
fn parameterized_enum_variant_is_unsupported() {
    // A variant with constructor parameters needs a tagged union; Hatchet emits a
    // plain C++ enum, so the payload would be lost. The parameterized variant
    // `Move` is on line 3; the bare variant `Stop` must NOT be flagged.
    let (prog, idx) = program_from(
        "Cmd",
        "package p;\nenum Cmd {\n  Move(dx:Int, dy:Int);\n  Stop;\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        errs.iter().any(|d| d.severity == Severity::Unsupported
            && d.message.contains("parameterized enum variant")
            && d.message.contains("Move")
            && d.line == 3),
        "expected an Unsupported parameterized-variant diagnostic for Move on line 3, got: {:?}",
        errs.iter().map(|d| (d.line, &d.message)).collect::<Vec<_>>()
    );
    assert!(
        !errs.iter().any(|d| d.message.contains("Stop")),
        "the bare variant Stop must not be flagged, got: {:?}",
        errs.iter().map(|d| &d.message).collect::<Vec<_>>()
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
        errs.iter().map(|d| (d.line, &d.message)).collect::<Vec<_>>()
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
        unsup.iter().map(|d| (d.line, &d.message)).collect::<Vec<_>>()
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
        errs.iter().any(|d| d.message.contains("Nope") && d.line == 4),
        "expected an unresolved-type error for the catch type `Nope` on line 4, got: {:?}",
        errs.iter().map(|d| (d.line, &d.message)).collect::<Vec<_>>()
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
        errs.iter().map(|d| (d.line, &d.message)).collect::<Vec<_>>()
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
        !errs.iter().any(|d| d.message.contains("type-check operator")),
        "`is` as an identifier/field must not be flagged as the operator, got: {:?}",
        errs.iter().map(|d| (d.line, &d.message)).collect::<Vec<_>>()
    );
}

#[test]
fn abstract_type_is_unsupported() {
    // A bare `abstract` type is parsed-and-skipped and flagged on its line (2).
    let (prog, idx) = program_from(
        "Meters",
        "package p;\nabstract Meters(Float) {\n  public inline function new(v:Float) { this = v; }\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        errs.iter().any(|d| d.severity == Severity::Unsupported
            && d.message.contains("`abstract` type")
            && d.message.contains("Meters")
            && d.line == 2),
        "expected an Unsupported `abstract` diagnostic on line 2, got: {:?}",
        errs.iter().map(|d| (d.line, &d.message)).collect::<Vec<_>>()
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
    assert!(errs.is_empty(), "no errors expected, got: {:?}", errs.iter().map(|d| &d.message).collect::<Vec<_>>());
}
