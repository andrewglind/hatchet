//! Header-generation rules, checked against small self-contained programs built
//! in a temp directory (no external corpus required).

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
    prog.export_macro = "MUCUS".to_string();
    let idx = module_index(&prog, "Widget");
    let out = generate_header(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(out.contains("class MUCUS_CLASS Widget"), "@:decl → macro-decorated class:\n{out}");
    assert!(!out.contains("__declspec"), "no raw MSVC token leaks into output:\n{out}");
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

    assert!(base.contains("virtual float area();"), "overridden base method must be virtual:\n{base}");
    assert!(
        base.contains("std::string label();") && !base.contains("virtual std::string label();"),
        "a method no subclass overrides stays non-virtual:\n{base}"
    );
}
