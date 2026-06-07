//! Milestone-5 checks: the core `.cpp` body transpilations against the corpus —
//! `super(...)` → init list, `this->`, inherited-field pointer access, anonymous
//! struct → named temp (with pointer deref), and external accessor rewrites.
//! Skipped when the corpus is absent.

use std::path::PathBuf;

use hatchet::codegen::{generate_source, generate_source_diagnostics};
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

fn source(prog: &Program, stem: &str) -> String {
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some(stem))
        .unwrap_or_else(|| panic!("module {stem} not found"));
    generate_source(prog, idx).unwrap_or_else(|| panic!("no source for {stem}"))
}

#[test]
fn body_transpilation_core() {
    let Some(root) = modules_root() else {
        eprintln!("skipping: Modules corpus not found (set HATCHET_CORPUS)");
        return;
    };
    let prog = Program::from_src_dir(&root).expect("build program");

    let vertex = source(&prog, "Vertex");
    // super(engine) becomes a base initialiser list
    assert!(vertex.contains(": Module(engine)"), "super → init list");
    // this. → this->
    assert!(vertex.contains("this->x = x;"));
    // anonymous struct return → named temp of the native return type, with
    // value-typed struct fields copied directly.
    assert!(vertex.contains("mucus::Vertex _ret"));
    assert!(vertex.contains(".effects = this->effects;"), "value struct field copy");
    // inherited pointer field accessed with -> and chained calls
    assert!(vertex.contains("this->engine->GetRenderer()->PushVertex(vertex());"));

    // Anonymous struct passed as an argument is hoisted to a typed temporary.
    let line = source(&prog, "Line");
    assert!(line.contains("mucus::Line _anon1;"), "anon arg hoisted to native type");
    assert!(line.contains("PushLine(_anon1)"));

    // External property access uses generated getters/setters.
    let quad = source(&prog, "Quad");
    assert!(quad.contains("->SetX("), "external (default,set) write → SetX");
    assert!(quad.contains("->GetX()"), "external (default,set) read → GetX");
    // Enum value access is namespace + `_::` qualified.
    assert!(quad.contains("mucus::EffectType_::"), "enum constant qualified");
}

#[test]
fn statement_and_expression_forms() {
    let (Some(mroot), Some(groot)) = (modules_root(), game_root()) else {
        eprintln!("skipping: Modules/Game corpora not found (set HATCHET_CORPUS / HATCHET_GAME_CORPUS)");
        return;
    };
    let prog = Program::from_src_dir(&mroot).expect("build Modules program");
    let game = Program::from_src_dir(&groot).expect("build Game program");

    // `new Array<T>()` → value-constructed container; `?.` method → guarded call.
    let backdrop = source(&prog, "Backdrop");
    assert!(
        backdrop.contains("std::vector<std::vector<Tile*> >()"),
        "new Array<Array<Tile>>() → value container"
    );
    assert!(
        backdrop.contains("!= NULL ? (") && backdrop.contains("->AddLighting"),
        "safe-navigation method call is NULL-guarded"
    );

    // Untyped object literal → local anonymous struct.
    let camera = source(&prog, "Camera");
    assert!(camera.contains("struct { int x; int y; }"), "untyped object → anon struct");

    // Non-empty array literal (here as a return) → vector + push_backs;
    // `Math.POSITIVE_INFINITY` → a portable large float.
    let graph = source(&prog, "Graph");
    assert!(graph.contains(".push_back(startIndex);"), "array literal → push_back");
    assert!(graph.contains("HUGE_VAL"), "Math.POSITIVE_INFINITY intrinsic");

    // Map `.get(k)` → an iterator alias: `find(k)`, a null check on the result is
    // the existence check (`it == map.end()`), and value/member use is `it->second`.
    let animation = source(&prog, "Animation");
    assert!(
        animation.contains("std::map<std::string, AnimationSequence>::iterator")
            && animation.contains(".sequences.find(name)"),
        "Map.get → iterator via find():\n{animation}"
    );
    assert!(
        animation.contains("== this->animationData.sequences.end()"),
        "null check on a Map.get result → iterator existence check:\n{animation}"
    );
    assert!(animation.contains("->second.frames"), "member use of the get result → it->second:\n{animation}");
    assert!(!animation.contains(".sequences[name]"), "Map.get must not lower to operator[]:\n{animation}");

    // String interpolation → sprintf into a stack buffer.
    let tile = source(&prog, "Tile");
    assert!(tile.contains("sprintf("), "string interpolation → sprintf");

    // Array comprehension → hoisted vector populated by a loop.
    let text = source(&prog, "Text");
    assert!(
        text.contains("std::vector<mucus::Quad > _compr"),
        "array comprehension → hoisted vector"
    );

    // Base-from-member idiom: the Holder ctor runs the pre-super logic and stores
    // the hoisted `new` arguments; the class ctor chains through both bases.
    let tile = source(&prog, "Tile");
    assert!(tile.contains("TileHolder::TileHolder("), "Holder ctor defined");
    assert!(tile.contains("this->_super1 = new Quad("), "hoisted super arg stored");
    assert!(
        tile.contains(": TileHolder(engine, tilesetId, x, y, textureName), TexturedQuad(engine, _super1, _super2)"),
        "class ctor chains Holder then base"
    );
    // A local that would shadow a parameter is renamed.
    assert!(tile.contains("textureName_2"), "shadowing local renamed");

    // Nullable value types (`Null<T>`) lower to pointers: the return type is a
    // pointer, a value result is heap-allocated, `null` becomes NULL, and
    // comparisons use NULL.
    let graph = source(&prog, "Graph");
    assert!(graph.contains("Edge* Graph::GetEdge("), "Null<Edge> return → Edge*");
    assert!(graph.contains("return new Edge("), "value result heap-allocated");
    assert!(graph.contains("== NULL"), "`== null` → `== NULL` on the pointer");
    assert!(graph.contains("std::vector<int>* Graph::FindPath("), "Null<Array<Int>> → vector*");
    // An aliased import (`Distance as Heuristic`) is called by its real name.
    assert!(graph.contains("Distance(this->nodes["), "import alias resolved to real name");

    // Top-level `final NAME = function/lambda` → namespace free functions
    // (public defined plainly, file-local ones `static`).
    let utilities = source(&prog, "Utilities");
    assert!(utilities.contains("float Distance(const Vector& a, const Vector& b) {"), "public free fn");
    assert!(utilities.contains("static float CrossProduct("), "private free fn is static");
    // Statement-level `@:cppFileCode('...')` injects verbatim C++ at column 0 so
    // preprocessor directives stay valid, interleaved with transpiled statements.
    assert!(utilities.contains("\n#ifdef DREAMCAST\n"), "@:cppFileCode injected at column 0");
    assert!(utilities.contains("\n#else\n") && utilities.contains("\n#endif\n"), "verbatim #else/#endif");
    assert!(utilities.contains("ret = sqrt(dx * dx + dy * dy);"), "#else branch is transpiled");

    // SceneFactory (game): `extern inline function` → an `extern "C"` export
    // DEFINED at global scope (no namespace), body fully qualified, `null` → `NULL`.
    let factory = source(&game, "SceneFactory");
    assert!(
        factory.contains("HATCHET_EXPORT mucus::IScene* HATCHET_CALL MCreateScene(mucus::IEngine* engine, int sceneId) {"),
        "extern \"C\" definition at global scope:\n{factory}"
    );
    assert!(factory.contains("new game::AlienBeach(engine)"), "body types fully qualified:\n{factory}");
    assert!(!factory.contains("namespace game"), "definition is not wrapped in a namespace:\n{factory}");

    // A value argument passed to a `Null<T>` parameter is heap-allocated so the
    // callee can own it (FogEffect takes `Null<FogEffectData>`). (game)
    let alien = source(&game, "AlienBeach");
    assert!(
        alien.contains("new modules::FogEffect(engine, new mucus::FogEffectData(fogEffectData))"),
        "value arg to Null<T> param → heap-allocated"
    );

    // Method-scope ownership: fresh `new`s bound to non-escaping locals (including
    // hoisted `new` arguments and a nullable result) are freed at scope close.
    let actor = source(&prog, "Actor");
    assert!(actor.contains("Vertex* _v"), "new arguments hoisted to owned locals");
    assert!(
        actor.contains("delete line;") && actor.contains("delete _v"),
        "scope-owned locals freed at the end of the loop"
    );
    assert!(actor.contains("delete path;"), "nullable result freed at scope close");

    // A `new` forwarded into a base initialiser list stays inline (the base owns
    // it) — it is never hoisted into the constructor body or scope-deleted.
    let sprite = source(&prog, "Sprite");
    assert!(
        sprite.contains(": TexturedQuad(engine, new Quad(engine, new Vertex("),
        "super-call `new` args stay inline in the initialiser list"
    );
    assert!(!sprite.contains("delete _v"), "base-owned args are not scope-deleted");

    // Owned pointer fields are NULL-initialised in the constructor and the prior
    // value is freed before reassignment (delete-before-overwrite).
    let dialog = source(&prog, "DialogBox");
    assert!(dialog.contains(": Module(engine), text(NULL)"), "owned field NULL-initialised");
    assert!(
        dialog.contains("delete this->text;\n\tthis->text = new ShadowText("),
        "delete-before-overwrite on field reassignment"
    );

    // Walkbox: `Array.map(lambda)` → a hoisted vector filled by a loop (the
    // Map-comprehension + Lambda composition).
    let walkbox = source(&prog, "Walkbox");
    // Case 1: object-literal body expands into the element struct via the
    // contextual (assignment-target) element type, not an anonymous struct.
    assert!(
        walkbox.contains("std::vector<Vector > _map")
            && walkbox.contains("for (size_t")
            && walkbox.contains(".push_back("),
        "map → hoisted vector + loop + push_back:\n{walkbox}"
    );
    assert!(
        walkbox.contains("Vector v = this->polygon.vertices[")
            && walkbox.contains(".x = (v.x * scaleFactor);"),
        "map lambda body (object literal) expanded into the element struct:\n{walkbox}"
    );
    // Case 2: the receiver is a `Null<Array<Int>>` pointer — iterate the pointee.
    assert!(
        walkbox.contains("for (size_t _i") && walkbox.contains("< (*indices).size();"),
        "map over a nullable container dereferences the pointer:\n{walkbox}"
    );
    // The nullable container's `.length` also dereferences (no `.size()` on a ptr).
    assert!(
        walkbox.contains("(*indices).size() > 0"),
        ".length on a Null<Array<T>> dereferences:\n{walkbox}"
    );
}

#[test]
fn corpus_nullable_warnings_are_only_the_known_buried_calls() {
    // The corpus is free of nullable *misuse* (a `Null<T>` value flowing into a
    // non-`Null<T>` `var`/assignment) and of discarded nullable results (those are
    // auto-extracted). The only remaining warnings are two *buried* nullable
    // calls in `Graph.AddEdge` — `if (GetEdge(edge) == null)` / `if (GetEdge(test)
    // == null)` — where the heap `Edge` the call returns has nowhere to be freed.
    // They are flagged, not auto-fixed: the fix belongs in the Haxe (extract the
    // call to a `Null<Edge>` local). This test pins that known set so a new buried
    // nullable elsewhere is caught as a regression.
    let (Some(mroot), Some(groot)) = (modules_root(), game_root()) else {
        eprintln!("skipping: Modules/Game corpora not found (set HATCHET_CORPUS / HATCHET_GAME_CORPUS)");
        return;
    };
    // Sweep both standalone repos: the whole corpus must be free of nullable misuse
    // bar the two known buried calls in `Modules`' Graph.AddEdge.
    let mut all = Vec::new();
    for root in [&mroot, &groot] {
        let prog = Program::from_src_dir(root).expect("build program");
        for i in 0..prog.modules.len() {
            if let Some((_, w, _)) = generate_source_diagnostics(&prog, i, 1) {
                all.extend(w);
            }
        }
    }
    // Every warning is a buried-nullable one from Graph.AddEdge at lines 34 / 39.
    let buried = "used inside a larger expression";
    assert!(
        all.iter().all(|(line, w)| {
            w.starts_with("AddEdge:") && w.contains(buried) && (*line == 34 || *line == 39)
        }),
        "unexpected nullable warnings (only the two known Graph.AddEdge buried calls are allowed): {all:?}"
    );
    assert_eq!(all.len(), 2, "expected exactly the two known buried calls, got: {all:?}");
}

#[test]
fn depth_two_auto_extracts_the_graph_buried_calls() {
    // With `--depth 2`, the two depth-2 buried `GetEdge(...)` calls in
    // `Graph.AddEdge` (the `if (GetEdge(e) == null)` checks) are no longer warned
    // about: each is hoisted into an owned `Edge*` local that is freed at scope
    // close, so the warning set is empty and the locals appear in the output.
    let Some(root) = modules_root() else {
        eprintln!("skipping: Modules corpus not found (set HATCHET_CORPUS)");
        return;
    };
    let prog = Program::from_src_dir(&root).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("Graph"))
        .unwrap();
    let (source, warnings, _) = generate_source_diagnostics(&prog, idx, 2).unwrap();
    assert!(
        warnings.is_empty(),
        "depth 2 should auto-extract the buried Graph calls, leaving no warnings: {warnings:?}"
    );
    // Each buried `GetEdge(...)` is hoisted into an owned local that is freed at
    // scope close — two of them, so two `= GetEdge(` hoists and two deletes.
    // (Indices are not pinned: the `tmp` counter is shared across the class's
    // methods.)
    assert_eq!(
        source.matches("= GetEdge(").count(),
        2,
        "both buried calls should be hoisted into locals, got:\n{source}"
    );
    assert_eq!(
        source.matches("delete _null").count(),
        2,
        "both hoisted locals should be freed, got:\n{source}"
    );
    // The conditions now compare a hoisted local against NULL rather than calling
    // GetEdge inline inside the `if`.
    assert!(
        source.contains("_null") && source.contains("== NULL"),
        "hoisted locals should be compared against NULL, got:\n{source}"
    );
}

#[test]
fn nullable_misuse_is_warned() {
    // A nullable (`Null<T>`) result assigned to a non-`Null<T>` local must warn.
    let src = "\
typedef Pt = { var x:Int; var y:Int; }

@:expose
class Probe {
  public function new() {}
  public function find():Null<Pt> { return null; }
  public function use():Void {
    var p:Pt = this.find();
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_lint_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Probe.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("Probe"))
        .unwrap();
    let (_, warnings, _) = generate_source_diagnostics(&prog, idx, 1).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    // The misuse is the `var p:Pt = this.find();` on line 8 of `src`.
    assert!(
        warnings.iter().any(|(line, w)| *line == 8 && w.contains("Null<T>")),
        "expected a nullable warning on line 8, got: {warnings:?}"
    );
}

#[test]
fn discarded_nullable_call_is_extracted_to_a_local() {
    // A bare `Null<T>` call result the developer discards is not warned about —
    // Hatchet binds it to a fresh local and frees it at scope close, so the heap
    // object the callee `new`ed does not leak.
    let src = "\
typedef Pt = { var x:Int; var y:Int; }

@:expose
class Probe {
  public function new() {}
  public function find():Null<Pt> { return null; }
  public function run():Void {
    this.find();
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_extract_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Probe.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("Probe"))
        .unwrap();
    let (source, warnings, _) = generate_source_diagnostics(&prog, idx, 1).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    // The discarded result is bound to a Pt* local and deleted before the
    // method returns — and it is an auto-fix, not a warning.
    assert!(
        source.contains("Pt* _null1 = this->find();"),
        "discarded Null<T> call should bind to a local, got:\n{source}"
    );
    assert!(
        source.contains("delete _null1;"),
        "the extracted local should be freed at scope close, got:\n{source}"
    );
    assert!(
        !warnings.iter().any(|(_, w)| w.contains("discarded")),
        "auto-extract replaces the discard warning, got: {warnings:?}"
    );
}

#[test]
fn overloaded_method_call_resolves_return_type_by_args() {
    // An `@:overload`'d method whose canonical signature is `Dynamic` resolves its
    // return type from the overload matching the argument types (the C++ method is
    // genuinely overloaded, so the call text is unchanged). String-literal args are
    // wrapped in `std::string(...)` so C++ selects the `std::string` overload rather
    // than the `bool` one (a bare `const char*` prefers pointer→bool).
    let src = "\
@:native(\"Props\") @:include(\"props.h\")
interface IProps {
  @:overload(function(k:String, d:String):String {})
  @:overload(function(k:String, d:Int):Int {})
  @:overload(function(k:String, d:Bool):Bool {})
  public function GetOr(k:String, d:Dynamic):Dynamic;
}

@:expose
class C {
  var props:IProps;
  public function new() {}
  public function run():Void {
    var s = this.props.GetOr(\"a\", \"x\");
    var n = this.props.GetOr(\"b\", 5);
    var f = this.props.GetOr(\"c\", false);
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_overload_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("P.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("P"))
        .unwrap();
    let out = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(out.contains("std::string s = this->props->GetOr(std::string(\"a\"), std::string(\"x\"))"), "String overload → std::string + wrapped literals:\n{out}");
    assert!(out.contains("int n = this->props->GetOr(std::string(\"b\"), 5)"), "Int overload → int:\n{out}");
    assert!(out.contains("bool f = this->props->GetOr(std::string(\"c\"), false)"), "Bool overload → bool:\n{out}");
    assert!(!out.contains("void*"), "Dynamic no longer erases to void*:\n{out}");
}

#[test]
fn file_finals_become_static_const_definitions() {
    // Every `final` becomes a `static const` inside the namespace (no `#define`):
    // a scalar is written directly, a struct is aggregate-initialised, and an
    // `Array<T>` (which C++98 cannot brace-initialise) is built by a one-off helper
    // assigned to a `const std::vector` object — so it stays a vector and call
    // sites are unchanged.
    let src = "\
typedef Coord = { var u:Float; var v:Float; }

private final SCALE:Float = 2.0;
private final ORIGIN:Coord = { u: 0.0, v: 0.0 };
private final CORNER:Coord = { u: 16.0, v: 32.0 };
private final TABLE:Array<Coord> = [ ORIGIN, CORNER ];

@:expose
class Atlas {
  public function new() {}
  public function At(i:Int):Coord { return TABLE[i]; }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_finals_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Atlas.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("Atlas"))
        .unwrap();
    let out = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    // Scalar final → static const (not a #define); struct finals → aggregate const.
    assert!(out.contains("static const float SCALE = 2.0f;"), "scalar final → static const:\n{out}");
    assert!(!out.contains("#define SCALE"), "scalar final must not be a #define:\n{out}");
    assert!(out.contains("static const Coord ORIGIN = { 0.0f, 0.0f };"), "struct final → aggregate const:\n{out}");
    assert!(out.contains("static const Coord CORNER = { 16.0f, 32.0f };"), "struct final aggregate in field order:\n{out}");
    // Array final → builder helper + const vector object (stays a vector).
    assert!(out.contains("_hatchet_init_TABLE() {"), "array final → builder helper:\n{out}");
    assert!(out.contains("v;") && out.contains("v.push_back(ORIGIN);") && out.contains("v.push_back(CORNER);"), "builder declares v and push_backs elements:\n{out}");
    assert!(out.contains("return v;"), "builder returns the vector:\n{out}");
    assert!(
        out.contains("TABLE = _hatchet_init_TABLE();") && out.contains("static const std::vector<Coord"),
        "array final → const vector object:\n{out}"
    );
    // Call site is unchanged — it indexes the vector object directly.
    assert!(out.contains("return TABLE[i];"), "call site indexes the vector object:\n{out}");
    assert!(!out.contains("Coord TABLE["), "array final must not become a C array:\n{out}");
}

#[test]
fn final_constant_references_are_namespace_qualified() {
    // A `@:native final` (provided by the C++ engine, not emitted) is referenced
    // with its native namespace (`mucus::MAX_CHARS`). A public `final` is a
    // `static const` inside its namespace, so a reference from a *different*
    // namespace — here a global-scope `extern "C"` export — is qualified too
    // (`game::SCENE_ID`), while a reference from within the same namespace stays
    // bare.
    let native = "\
package mucus;
@:native @:include(\"engine.h\")
final MAX_CHARS:Int = 100;
";
    let scenes = "\
package game;
import mucus.Mucus;
final SCENE_ID:Int = 7;

@:expose
class Scene {
  public function new() {}
  public function cap():Int { return MAX_CHARS; }        // native const → mucus::
  public function id():Int { return SCENE_ID; }          // same ns → bare
}

@:expose
extern inline function Pick(n:Int):Int {
  switch (n) {
    case SCENE_ID: return 1;                              // global scope → game::
    default: return 0;
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_finalref_{}", std::process::id()));
    std::fs::create_dir_all(dir.join("mucus")).unwrap();
    std::fs::create_dir_all(dir.join("game")).unwrap();
    std::fs::write(dir.join("mucus").join("Mucus.hx"), native).unwrap();
    std::fs::write(dir.join("game").join("Game.hx"), scenes).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("Game"))
        .unwrap();
    let out = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(out.contains("return mucus::MAX_CHARS;"), "native const → native namespace:\n{out}");
    assert!(out.contains("return SCENE_ID;"), "same-namespace ref stays bare:\n{out}");
    assert!(out.contains("case game::SCENE_ID:"), "global-scope ref → namespace-qualified:\n{out}");
}

#[test]
fn map_get_lowers_to_iterator_with_existence_check() {
    // `Map.get(k)` is `Null<V>`; for a value `V` there is no C++ null, so the local
    // is bound to a map iterator. A null check on it is the existence check
    // (`it == map.end()` / `!= map.end()`), value/member use is `it->second`. The
    // shape is not enforced: the null check need not be next, nor exit the scope.
    let src = "\
typedef Entry = { var count:Int; }

@:expose
class Store {
  private var entries:Map<String, Entry>;
  public function new() { this.entries = []; }
  public function CountOf(key:String):Int {
    var e:Entry = this.entries.get(key);
    var total:Int = 0;
    if (e != null) {
      total = e.count;
    }
    return total;
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_mapget_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Store.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("Store"))
        .unwrap();
    let out = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        out.contains("std::map<std::string, Entry>::iterator") && out.contains("this->entries.find(key)"),
        "get → find() iterator:\n{out}"
    );
    assert!(out.contains("!= this->entries.end()"), "`!= null` → `!= map.end()`:\n{out}");
    assert!(out.contains("->second.count"), "member use → it->second.count:\n{out}");
    assert!(!out.contains("== NULL") && !out.contains("[key]"), "no NULL compare / operator[]:\n{out}");
}

#[test]
fn early_return_frees_owned_locals() {
    // A scope-owned heap local (a `new` that does not escape) is freed at scope
    // close — but an early `return` skips that close. Hatchet must `delete` the owned
    // local before EVERY return, while never double-freeing: a tail return frees it
    // and emits no trailing (dead) delete.
    let src = "\
@:expose
class Helper { public function new() {} }

@:expose
class Runner {
  public function new() {}
  public function run(early:Bool):Null<Helper> {
    var h = new Helper();        // owned local (does not escape)
    if (early) {
      return null;               // early return → must free h first
    }
    return null;                 // tail return → frees h, no dead delete after
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_earlyret_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("R.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("R"))
        .unwrap();
    let out = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    // Isolate the `run` method body.
    let run = out
        .split("Runner::run(")
        .nth(1)
        .expect("run method present");
    // The owned local is freed exactly twice — once per return path — and never a
    // third (dead) time after the tail return.
    assert_eq!(run.matches("delete h;").count(), 2, "h freed once per return path:\n{out}");
    // Each delete immediately precedes a return (freed *before* exiting).
    assert!(run.contains("delete h;\n\t\treturn"), "early return frees before exiting:\n{out}");
    assert!(run.contains("delete h;\n\treturn"), "tail return frees before exiting:\n{out}");
}

#[test]
fn overloaded_call_matching_no_signature_is_an_error() {
    // Hatchet resolves an overloaded call from the argument types; if none of the
    // `@:overload` signatures match, it must NOT guess (the canonical `Dynamic`
    // return would compile to garbage). Here the overloads cover String/Int/Bool
    // but the call passes a Float second argument — no match → a hard error.
    let src = "\
@:native(\"Props\") @:include(\"props.h\")
interface IProps {
  @:overload(function(k:String, d:String):String {})
  @:overload(function(k:String, d:Int):Int {})
  @:overload(function(k:String, d:Bool):Bool {})
  public function GetOr(k:String, d:Dynamic):Dynamic;
}

@:expose
class C {
  var props:IProps;
  public function new() {}
  public function run():Void {
    var f = this.props.GetOr(\"c\", 1.5);
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_overload_err_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("P.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("P"))
        .unwrap();
    let (_, _, errors) = generate_source_diagnostics(&prog, idx, 1).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        errors.iter().any(|(_, e)| e.contains("GetOr") && e.contains("matches no @:overload")),
        "expected an overload-mismatch error for the Float call, got: {errors:?}"
    );
}

#[test]
fn new_pushed_into_owned_container_is_not_scope_deleted() {
    // A `new` pushed into a container the class owns escapes the current scope —
    // it must be emitted inline (not hoisted into a scope-owned local that gets
    // deleted at scope close, which would leave a dangling pointer the destructor
    // double-frees). Covers a direct field push AND a local that flows into a
    // field (the bare-`field` form, with `this.` omitted, must be recognised too).
    let src = "\
@:expose
class Tile {
  public function new(v:Int) {}
}

@:expose
class Grid {
  private var tiles:Array<Tile>;
  private var rows:Array<Array<Tile>>;
  public function new() {
    this.tiles = [];
    this.rows = [];
    tiles.push(new Tile(1));                 // direct push into owned field (bare)
    var row:Array<Tile> = new Array<Tile>(); // local that flows into rows
    row.push(new Tile(2));
    this.rows.push(row);
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_ownpush_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("G.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("G"))
        .unwrap();
    let body = generate_source(&prog, idx).unwrap();
    let header = hatchet::codegen::generate_header(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    // Both pushes are inline; the constructor never deletes the just-pushed object.
    assert!(body.contains("tiles.push_back(new Tile(1));"), "direct field push should be inline:\n{body}");
    assert!(body.contains("row.push_back(new Tile(2));"), "local-container push should be inline:\n{body}");
    assert!(!body.contains("delete "), "the constructor must not delete a pushed `new`:\n{body}");
    // The destructor frees both containers (the owner of the heap objects).
    assert!(header.contains("delete this->tiles[") , "dtor should free the tiles vector:\n{header}");
    assert!(header.contains("delete this->rows["), "dtor should free the rows vector:\n{header}");
}

#[test]
fn optional_value_struct_param_is_a_pointer_consistently() {
    // Option 1 (optional/nullable unification): an *optional* value-struct param
    // `?b:Coords` lowers to `Coords* b = NULL` — the same pointer shape as a
    // nullable `Null<Coords>`. The signature side (`param_decl`) and the call side
    // (`param_ty_in`) must agree, or a value would be passed into a pointer slot.
    // A *required* value-struct stays `const Coords&` and is passed by value.
    let src = "\
typedef Coords = {
  var u:Float;
  var v:Float;
}

@:expose
class Quad {
  public function new() {}
  public function set(a:Coords, ?b:Coords):Void {}
}

@:expose
class User {
  var q:Quad;
  public function new() {}
  public function run():Void {
    var a:Coords = { u: 0.0, v: 0.0 };
    var b:Coords = { u: 1.0, v: 1.0 };
    this.q.set(a, b);
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_optstruct_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Q.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("Q"))
        .unwrap();
    let body = generate_source(&prog, idx).unwrap();
    let header = hatchet::codegen::generate_header(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    // Signature: required value-struct → `const Coords&`; optional → `Coords*`
    // with a NULL default in the header declaration.
    assert!(
        header.contains("set(const Coords& a, Coords* b = NULL)"),
        "optional value-struct param should be `Coords* = NULL` in the header:\n{header}"
    );
    assert!(
        body.contains("void Quad::set(const Coords& a, Coords* b)"),
        "out-of-line definition keeps the pointer param (no default):\n{body}"
    );
    // Call site: required `a` passed by value; optional `b` heap-allocated into an
    // owned temp so a `Coords*` matches the signature, then freed after the call.
    assert!(
        body.contains("new Coords(b)"),
        "optional value-struct argument must be heap-allocated to match `Coords*`:\n{body}"
    );
    assert!(
        body.contains("->set(a, _"),
        "required arg passed by value, optional arg as the pointer temp:\n{body}"
    );
    assert!(
        body.contains("delete _"),
        "the owned optional-arg temp must be freed (no leak):\n{body}"
    );
}

#[test]
fn property_access_on_new_hoists_to_a_local() {
    // `new T(...).prop` cannot be written as `new T(...)->GetProp()` (the
    // new-expression binds looser than `->`). Hatchet hoists the construction to a
    // local and reads the property off it. The temporary is not freed (only the
    // property value escapes; the object's dtor would free it).
    let src = "\
@:expose
class Cell {
  public var value(default, null):Int;
  public function new(v:Int) { this.value = v; }
}

@:expose
class Grid {
  private var values:Array<Int>;
  public function new() {
    this.values = [];
    this.values.push(new Cell(7).value);
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_propnew_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("G.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("G"))
        .unwrap();
    let out = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    // The construction is hoisted to a `Cell*` local, then the accessor is read
    // off it — never `new Cell(7)->...` (a parse error) and never a `delete`.
    assert!(
        out.contains("Cell* ") && out.contains(" = new Cell(7);"),
        "construction should be hoisted to a Cell* local:\n{out}"
    );
    assert!(
        out.contains("->GetValue());"),
        "the property should be read off the local via the accessor:\n{out}"
    );
    assert!(!out.contains("new Cell(7)->"), "no field access directly on a new-expression:\n{out}");
    assert!(!out.contains("delete "), "the hoisted wrapper must not be freed:\n{out}");
}

#[test]
fn string_null_check_lowers_to_empty() {
    // A Haxe `String` is a value-typed `std::string` with no null, so a null
    // comparison cannot stay `s == NULL`. Optional `String` params default to `""`,
    // so "null" ≡ empty: `s == null` → `s.empty()`, `s != null` → `!s.empty()`.
    let src = "\
@:expose
class S {
  public function new() {}
  public function run(?palette:String):Void {
    if (palette == null) { palette = \"default\"; }
    var s:String = \"x\";
    if (s != null) { s = \"y\"; }
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_strnull_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("S.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("S"))
        .unwrap();
    let out = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(out.contains("if (palette.empty()) {"), "`== null` → `.empty()`:\n{out}");
    assert!(out.contains("if (!s.empty()) {"), "`!= null` → `!...empty()`:\n{out}");
    assert!(!out.contains("== NULL") && !out.contains("!= NULL"), "no NULL comparison on a string:\n{out}");
}

#[test]
fn string_tier1_methods_map_to_std_string() {
    // Tier-1 Haxe `String` API → single C++98 `std::string` expressions.
    let src = "\
@:expose
class S {
  public function new() {}
  public function run():Void {
    var s:String = \"hello\";
    var len:Int = s.length;
    var c:Int = s.charCodeAt(0);
    var ch:String = s.charAt(1);
    var i:Int = s.indexOf(\"l\");
    var j:Int = s.indexOf(\"l\", 3);
    var k:Int = s.lastIndexOf(\"l\");
    var t:String = s.toString();
    var f:String = String.fromCharCode(65);
    var copy:String = new String(\"world\");
    var code:Int = \"A\".code;
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_str_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("S.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("S"))
        .unwrap();
    let out = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    for needle in [
        "int len = s.length();",
        "int c = ((int)(unsigned char)s.at(0));",
        "std::string ch = s.substr(1, 1);",
        "int i = ((int)s.find(\"l\"));",
        "int j = ((int)s.find(\"l\", 3));",
        "int k = ((int)s.rfind(\"l\"));",
        "std::string t = s;",
        "std::string f = std::string(1, (char)((65) & 0xFF));",
        "std::string copy = std::string(\"world\");",
        "int code = ((int)(unsigned char)(\"A\")[0]);",
    ] {
        assert!(out.contains(needle), "missing `{needle}` in:\n{out}");
    }
    // A `new String(...)` is a value, not a heap pointer — it is never deleted.
    assert!(!out.contains("delete copy"), "string value must not be deleted:\n{out}");
}

#[test]
fn math_intrinsics_map_inline() {
    // Math API → inline C++98 expressions (no helper functions / shims).
    let src = "\
@:expose
class M {
  public function new() {}
  public function run():Void {
    var a:Float = Math.sin(1.0) + Math.pow(2.0, 8.0) + Math.atan2(1.0, 2.0);
    var b:Float = Math.ffloor(2.7) + Math.fround(2.5);
    var e:Int = Math.floor(2.7) + Math.round(2.5);
    var g:Float = Math.min(1.0, 2.0) + Math.random();
    var ok:Bool = Math.isNaN(0.0) || Math.isFinite(1.0);
    var pi:Float = Math.PI;
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_math_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("M.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("M"))
        .unwrap();
    let out = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    for needle in [
        "sin(1.0f)",
        "pow(2.0f, 8.0f)",
        "atan2(1.0f, 2.0f)",
        "floor(2.7f)",                       // Math.ffloor → float floor
        "((int)floor(2.7f))",                // Math.floor → int
        "((int)floor((2.5f) + 0.5))",        // Math.round → int
        "(rand() / (RAND_MAX + 1.0))",       // Math.random ∈ [0,1)
        "((float) 3.141592653589793)",       // Math.PI literal (no M_PI)
    ] {
        assert!(out.contains(needle), "missing `{needle}` in:\n{out}");
    }
    // min is NaN-propagating inline (no helper), and no shim names leak in.
    assert!(out.contains("== (1.0f) ? (2.0f) : (1.0f)"), "min NaN-aware inline:\n{out}");
    assert!(!out.contains("haxe_min") && !out.contains("haxe_"), "no helper shims:\n{out}");
}

#[test]
fn lambda_return_type_from_function_type_annotation() {
    // A lambda with no `cast`/`:T` hint takes its return type from the binding's
    // function-type annotation `(Int, Int) -> Int` (the second of the two hint
    // forms in SKILL.md).
    let src = "\
package p;
final Square:(Int, Int) -> Int = (a:Int, b:Int) -> a * b;
";
    let dir = std::env::temp_dir().join(format!("hatchet_lam_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("P.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("P"))
        .unwrap();
    let out = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        out.contains("int Square(int a, int b)"),
        "return type taken from the (Int,Int)->Int annotation:\n{out}"
    );
    assert!(out.contains("return a * b;"), "lambda body transpiled:\n{out}");
}
