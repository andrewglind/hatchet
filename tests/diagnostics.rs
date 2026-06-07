//! Validation diagnostics: Hatchet must fail loudly on a type it cannot resolve
//! rather than silently guess a by-value spelling. The real corpus must stay
//! clean (every referenced type resolves); synthetic inputs exercise the errors.

use std::path::PathBuf;

use hatchet::diag::Severity;
use hatchet::sema::validate::{unresolved_type_errors, unsupported_construct_errors};
use hatchet::sema::Program;

/// A sibling corpus repo: `$env` if set, else `../<name>` next to this crate.
fn repo_root(env: &str, name: &str) -> Option<PathBuf> {
    if let Ok(p) = std::env::var(env) {
        let p = PathBuf::from(p);
        return p.is_dir().then_some(p);
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let sibling = manifest.parent()?.join(name);
    sibling.is_dir().then_some(sibling)
}

/// The `Modules` corpus (`modules/*.hx` + `mucus/Mucus.hx`).
fn modules_root() -> Option<PathBuf> {
    repo_root("HATCHET_CORPUS", "Modules")
}

/// The `Game` corpus (`game/*.hx` + native binding stubs).
fn game_root() -> Option<PathBuf> {
    repo_root("HATCHET_GAME_CORPUS", "Game")
}

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
fn corpus_has_no_unresolved_types() {
    let (Some(mroot), Some(groot)) = (modules_root(), game_root()) else {
        eprintln!("skipping: Modules/Game corpora not found (set HATCHET_CORPUS / HATCHET_GAME_CORPUS)");
        return;
    };
    // Both standalone repos must resolve every referenced type.
    let mut all = Vec::new();
    for root in [&mroot, &groot] {
        let prog = Program::from_src_dir(root).expect("build program");
        for i in 0..prog.modules.len() {
            all.extend(unresolved_type_errors(&prog, i));
        }
    }
    let rendered: Vec<String> = all.iter().map(|d| d.render()).collect();
    assert!(all.is_empty(), "corpus should have no unresolved types, got:\n{}", rendered.join("\n"));
}

#[test]
fn missing_field_type_is_an_error() {
    // `Widget` is never declared anywhere, so the field type cannot resolve.
    // It is written on line 3 of the source, which the error must report.
    let (prog, idx) = program_from(
        "Panel",
        "@:expose\nclass Panel {\n  var widget:Widget;\n  public function new() {}\n}\n",
    );
    let errs = unresolved_type_errors(&prog, idx);
    assert!(
        errs.iter().any(|d| d.message.contains("Widget")
            && d.message.contains("field `widget`")
            && d.line == 3),
        "expected an unresolved-type error for Widget on line 3, got: {:?}",
        errs.iter().map(|d| (d.line, &d.message)).collect::<Vec<_>>()
    );
}

#[test]
fn missing_type_in_new_expression_is_an_error() {
    // A type named only in a `new` inside a method body must still be caught.
    let (prog, idx) = program_from(
        "Factory",
        "@:expose\nclass Factory {\n  public function new() {}\n  public function make():Void {\n    var w = new Gizmo();\n  }\n}\n",
    );
    let errs = unresolved_type_errors(&prog, idx);
    // `new Gizmo()` is on line 5 of the source.
    assert!(
        errs.iter().any(|d| d.message.contains("Gizmo") && d.line == 5),
        "expected an unresolved-type error for Gizmo on line 5, got: {:?}",
        errs.iter().map(|d| (d.line, &d.message)).collect::<Vec<_>>()
    );
}

#[test]
fn lambda_outside_map_or_final_is_unsupported() {
    // A lambda passed to an ordinary call (not `Array.map`) is not yet supported;
    // it must raise an `Unsupported` diagnostic (carrying the PR invite), reported
    // on the statement's line (5).
    let (prog, idx) = program_from(
        "Cb",
        "@:expose\nclass Cb {\n  public function new() {}\n  public function run():Void {\n    register((x) -> x + 1);\n  }\n}\n",
    );
    let errs = unsupported_construct_errors(&prog, idx);
    assert!(
        errs.iter().any(|d| d.severity == Severity::Unsupported
            && d.message.contains("lambda")
            && d.line == 5),
        "expected an Unsupported lambda diagnostic on line 5, got: {:?}",
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
         @:expose\n\
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
fn primitives_containers_and_declared_types_are_not_flagged() {
    // Int/String/Array/Map/Null and a locally-declared struct all resolve.
    let (prog, idx) = program_from(
        "Ok",
        "typedef Pt = { var x:Int; var y:Int; }\n\
         @:expose\n\
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
