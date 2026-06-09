//! Milestone-4 acceptance: header generation against the real corpus. Module.h
//! and IModule.h are expected byte-for-byte; the richer headers are checked for
//! the rules that matter (namespacing, pointer-ness, accessors, C++98 `> >`).
//! Skipped when the corpus is absent.

use std::path::PathBuf;

use hatchet::codegen::generate_header;
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
        "package ui;\n@:decl @:expose class Widget {\n  public function new() {}\n}\n",
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
fn headers_match_and_follow_rules() {
    let (Some(mroot), Some(groot)) = (modules_root(), game_root()) else {
        eprintln!("skipping: Modules/Game corpora not found (set HATCHET_CORPUS / HATCHET_GAME_CORPUS)");
        return;
    };
    // Each repo is standalone (its own native binding stubs), so it transpiles as
    // its own `Program`: the engine modules from `Modules`, the scenes from `Game`.
    let prog = Program::from_src_dir(&mroot).expect("build Modules program");
    let game = Program::from_src_dir(&groot).expect("build Game program");

    // Byte-for-byte against the committed goldens, save for comments: the
    // transpiler intentionally emits comment-free output, so the golden's
    // `} // namespace modules` close is normalised to a bare `}` before
    // comparing (comments are cosmetic — goldens win on substance only).
    for stem in ["Module", "IModule"] {
        let gen = header(&prog, stem);
        let golden = std::fs::read_to_string(mroot.join("modules").join(format!("{stem}.h")))
            .unwrap()
            .replace("\r\n", "\n")
            .replace("} // namespace modules", "}");
        // Trailing-newline differences are cosmetic (and easily lost when a golden
        // is re-saved or restored from backup), so compare ignoring them.
        assert_eq!(gen.trim_end(), golden.trim_end(), "{stem}.h does not match golden");
    }

    // Vertex: accessor rules + optional-primitive defaults. `effects` is a
    // non-optional value-typed `(default,set)` property (stored by value).
    let vertex = header(&prog, "Vertex");
    assert!(vertex.contains("class Vertex : public Module {"));
    assert!(vertex.contains("uint32_t color = 0"), "optional UInt32 → value with 0 default");
    assert!(vertex.contains("mucus::Effects effects;"), "non-optional struct → value field");
    assert!(vertex.contains("const uint32_t GetColor() { return color; }"));
    assert!(vertex.contains("void SetColor(uint32_t color) { this->color = color; }"));
    assert!(
        vertex.contains("void SetEffects(mucus::Effects effects) { this->effects = effects; }"),
        "value (default,set) → by-value setter"
    );
    assert!(vertex.contains("private:"));

    // AlienBeach (game): module types resolve to modules::, native to mucus::.
    let alien = header(&game, "AlienBeach");
    assert!(alien.contains("class AlienBeach : public mucus::IScene {"));
    assert!(alien.contains("modules::Backdrop* backdrop;"), "module class → modules:: pointer");
    assert!(
        alien.contains("OnMouseClick(mucus::MouseButton button, int x, int y)"),
        "native enum param → mucus:: namespace"
    );
    // Public `final` → `static const` inside the namespace (not a `#define`).
    assert!(
        alien.contains("static const int ALIENBEACH_SCENE_ID = 1;"),
        "public final → static const inside the namespace"
    );
    assert!(!alien.contains("#define ALIENBEACH_SCENE_ID"), "no #define for finals");

    // Graph: nested template must use `> >` for C++98.
    let graph = header(&prog, "Graph");
    assert!(
        graph.contains("std::vector<std::vector<Edge> >"),
        "nested template needs a space before the outer '>'"
    );

    // Destructors free owned pointers. AlienBeach `new`s its module fields, so it
    // deletes them; it does not delete a borrowed/value member.
    assert!(
        alien.contains("delete this->fogEffect;") && alien.contains("delete this->backdrop;"),
        "scene destructor frees the modules it allocated"
    );

    // FogEffect forwards a `Null<FogEffectData>` into Effect's `void* data`, so it
    // deletes it with the concrete type; the base Effect owns nothing.
    let effect = header(&prog, "Effect");
    assert!(
        effect.contains("delete (mucus::FogEffectData*)this->data;"),
        "subclass frees the typed pointer it handed to the base's void* field"
    );
    assert!(effect.contains("virtual ~Effect() {}"), "base with no owned pointers has an empty dtor");

    // Backdrop's `new Array<...>()` is a value container, never `delete`d as a
    // whole — but it owns the `Tile*`s `new`ed into it, so the destructor walks
    // the nested vectors and frees each leaf.
    let backdrop = header(&prog, "Backdrop");
    assert!(!backdrop.contains("delete this->tilesets;"), "value containers are not deleted");
    assert!(
        backdrop.contains("delete this->tilesets[_i0][_i1];"),
        "owned container frees each pointer it allocated"
    );

    // Camera's `observers` container is borrowed (added via a parameter), so it
    // is never freed.
    let camera = header(&prog, "Camera");
    assert!(camera.contains("virtual ~Camera() {}"), "borrowed container is not freed");

    // Tile: base-from-member idiom — `super(...)` is not the first statement, so
    // an intermediate `TileHolder` struct precedes the class and is a private base.
    let tile = header(&prog, "Tile");
    assert!(tile.contains("struct TileHolder {"), "Holder struct emitted");
    assert!(
        tile.contains("class Tile : private TileHolder, public TexturedQuad {"),
        "class privately inherits its Holder"
    );
    assert!(
        tile.contains("TileHolder(mucus::IEngine* engine, int tilesetId, int x, int y, const std::string& textureName);"),
        "Holder ctor mirrors the class ctor params"
    );

    // Utilities: public top-level functions are declared (definitions in the .cpp);
    // file-local (`private`) ones are not exposed in the header.
    let utilities = header(&prog, "Utilities");
    assert!(
        utilities.contains("float Distance(const Vector& a, const Vector& b);"),
        "public free fn declared in header"
    );
    assert!(
        utilities.contains("bool PointInsidePolygon(const Point& point, const Polygon& polygon, float epsilon = 0.00001f);"),
        "free fn declaration keeps default arguments"
    );
    assert!(!utilities.contains("CrossProduct"), "private free fn not in header");

    // Enum form and typedef-struct default ctor.
    let modules = header(&prog, "Modules");
    assert!(modules.contains("struct Direction_ {"));
    assert!(modules.contains("typedef Direction_::Enum Direction;"));
    assert!(
        modules.contains("EffectOptions() :"),
        "struct with optional fields gets a default constructor"
    );

    // SceneFactory: an `extern inline function` becomes an `extern "C"` export at
    // GLOBAL scope (no namespace wrapper), declared with the portable export /
    // calling-convention macros and fully-qualified types.
    let factory = header(&game, "SceneFactory");
    assert!(
        factory.contains(
            "HATCHET_EXPORT mucus::IScene* HATCHET_CALL MCreateScene(mucus::IEngine* engine, int sceneId);"
        ),
        "extern inline → macro-wrapped extern \"C\" declaration:\n{factory}"
    );
    assert!(!factory.contains("namespace game"), "extern \"C\" export has no namespace block:\n{factory}");
}
