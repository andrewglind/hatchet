//! Ownership and nullable handling: scope frees, owned containers, @delete, optional params.
mod common;
use common::*;

#[test]
fn early_return_frees_owned_locals() {
    // A scope-owned heap local (a `new` that does not escape) is freed at scope
    // close — but an early `return` skips that close. Hatchet must `delete` the owned
    // local before EVERY return, while never double-freeing: a tail return frees it
    // and emits no trailing (dead) delete.
    let src = "\
class Helper { public function new() {} }

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
    assert_eq!(
        run.matches("delete h;").count(),
        2,
        "h freed once per return path:\n{out}"
    );
    // Each delete immediately precedes a return (freed *before* exiting).
    assert!(
        run.contains("delete h;\n\t\treturn"),
        "early return frees before exiting:\n{out}"
    );
    assert!(
        run.contains("delete h;\n\treturn"),
        "tail return frees before exiting:\n{out}"
    );
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
    let (_, _, errors) = generate_source_diagnostics(&prog, idx, 1, false).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        errors
            .iter()
            .any(|(_, e)| e.contains("GetOr") && e.contains("matches no @:overload")),
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
class Tile {
  public function new(v:Int) {}
}

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
    assert!(
        body.contains("tiles.push_back(new Tile(1));"),
        "direct field push should be inline:\n{body}"
    );
    assert!(
        body.contains("row.push_back(new Tile(2));"),
        "local-container push should be inline:\n{body}"
    );
    assert!(
        !body.contains("delete "),
        "the constructor must not delete a pushed `new`:\n{body}"
    );
    // The destructor frees both containers (the owner of the heap objects).
    assert!(
        header.contains("delete this->tiles["),
        "dtor should free the tiles vector:\n{header}"
    );
    assert!(
        header.contains("delete this->rows["),
        "dtor should free the rows vector:\n{header}"
    );
}

#[test]
fn typedef_container_push_new_is_not_scope_deleted() {
    // Nested alias typedefs over `Array<…>` with pointer elements: a `new` pushed
    // into a local container that later flows into an owned class field must be
    // emitted inline — not hoisted into a scope-owned temporary and deleted.
    let src = "\
class Widget {
  public function new(id:Int, band:Int) {}
}

typedef Row = Array<Widget>;
typedef Matrix = Array<Row>;

class Shelf {
  private var matrix:Matrix;
  public function new() {
    this.matrix = new Matrix();
    for (rack in 0...3) {
      var row:Row = new Row();
      for (slot in 0...5) {
        for (tier in 0...2) {
          row.push(new Widget(slot, tier));
        }
      }
      this.matrix.push(row);
    }
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_typpush_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("S.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("S"))
        .unwrap();
    let body = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        body.contains("row.push_back(new Widget("),
        "nested-loop push should be inline:\n{body}"
    );
    assert!(
        !body.contains("delete "),
        "the constructor must not delete a pushed `new`:\n{body}"
    );
}

#[test]
fn nullable_alias_container_map_dereferences_the_pointer() {
    // A `Null<Indicies>` where `typedef Indicies = Array<Int>` is a *pointer* to the
    // resolved `std::vector<int>`. Resolving the alias for container-method dispatch
    // (`.map`) must preserve the use-site pointer-ness, so the generated loop
    // dereferences the receiver (`(*indices).size()` / `(*indices)[i]`) rather than
    // calling `.size()` / `[]` on the bare pointer.
    let out = gen_one(
        "typedef Indicies = Array<Int>;\nclass M {\n  public var xs:Array<Int>;\n  public function new() { xs = []; }\n  public function f(indices:Null<Indicies>):Array<Int> {\n    return indices.map((i) -> this.xs[i]);\n  }\n}\n",
        "M",
    );
    assert!(
        out.contains("(*indices).size()"),
        "the map loop bound dereferences the nullable alias pointer:\n{out}"
    );
    assert!(
        out.contains("(*indices)["),
        "the map loop indexes the dereferenced container:\n{out}"
    );
    assert!(
        !out.contains("indices.size()"),
        "must not call .size() on the bare pointer:\n{out}"
    );
}

#[test]
fn bare_field_assigned_new_behaves_like_qualified() {
    // `field = new X(new Y())` with `this.` omitted must be treated the same as
    // `this.field = new X(...)`: the field owns the allocation, so the nested
    // `new` is emitted inline (NOT hoisted into a scope-owned local that the
    // constructor frees, which would leave `field` dangling), the field is
    // NULL-initialised, and the destructor deletes it.
    let src = "\
class Leaf {
  public function new(n:Int) {}
}

class Holder {
  public function new(a:Leaf) {}
}

class Owner {
  var h:Holder;
  public function new() {
    h = new Holder(new Leaf(1));
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_barefield_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("O.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("O"))
        .unwrap();
    let body = generate_source(&prog, idx).unwrap();
    let header = hatchet::codegen::generate_header(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        body.contains("this->h = new Holder(new Leaf(1));"),
        "bare field assign must qualify and keep the nested new inline:\n{body}"
    );
    assert!(
        body.contains(": h(NULL)") || body.contains(", h(NULL)") || body.contains("h(NULL)"),
        "the owned field must be NULL-initialised:\n{body}"
    );
    assert!(
        !body.contains("delete "),
        "the constructor must not free the escaped nested new:\n{body}"
    );
    assert!(
        header.contains("delete this->h;"),
        "destructor must free the owned field:\n{header}"
    );
}

#[test]
fn untyped_lambda_params_typed_from_function_annotation() {
    // `Cross:(Vec, Vec) -> Float = (a, b) -> …` — the arrow params are
    // unannotated; their types come from the binding's function-type annotation.
    // Without propagating them they default to `int` and `a.x` is invalid C++.
    let src = "\
typedef Vec = {
  var x:Float;
  var y:Float;
}

final Cross:(Vec, Vec) -> Float = (a, b) -> a.x * b.y - a.y * b.x;

class M {}
";
    let dir = std::env::temp_dir().join(format!("hatchet_lambdaty_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("M.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("M"))
        .unwrap();
    let body = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        body.contains("double Cross(const Vec& a, const Vec& b)"),
        "untyped arrow params take their type from the function annotation:\n{body}"
    );
    assert!(
        !body.contains("int a"),
        "params must not default to int:\n{body}"
    );
    assert!(
        body.contains("return a.x * b.y - a.y * b.x;"),
        "body uses the typed params:\n{body}"
    );
}

#[test]
fn new_passed_to_an_owning_ctor_param_is_inline_not_double_freed() {
    // `var o = new Owner(new Child())` where Owner `@owned`s the child: the child
    // is freed by `~Owner`, so it must be emitted inline — NOT hoisted into a
    // scope-owned local that the scope also deletes (a double-free once `o`'s
    // destructor runs). A *borrowing* ctor param keeps the hoist (the scope must
    // free the fresh `new`, since the borrower never will).
    let src = "\
class Child {
  public function new() {}
}

class Owner {
  @owned var c:Child;
  public function new(c:Child) { this.c = c; }
}

class Borrower {
  var c:Child;
  public function new(c:Child) { this.c = c; }
}

class User {
  public function new() {}
  public function owns():Void {
    var o:Owner = new Owner(new Child());
  }
  public function borrows():Void {
    var b:Borrower = new Borrower(new Child());
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_ownarg_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("U.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("U"))
        .unwrap();
    let body = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    // Owned param: the child is inline, and the only delete is `delete o`.
    assert!(
        body.contains("new Owner(new Child())"),
        "a `new` into an @owned ctor param must be inline:\n{body}"
    );
    let owns = &body[body.find("User::owns").unwrap()..body.find("User::borrows").unwrap()];
    assert!(
        owns.contains("delete o;"),
        "the scope frees the owner:\n{owns}"
    );
    assert!(
        !owns.contains("delete _v"),
        "the owned child must not be separately scope-deleted (double-free):\n{owns}"
    );
    // Borrowed param: the fresh `new` is hoisted and freed by the scope.
    let borrows = &body[body.find("User::borrows").unwrap()..];
    assert!(
        borrows.contains("delete _v") || borrows.contains("Child* _v"),
        "a `new` into a borrowing param is hoisted to a scope-owned local:\n{borrows}"
    );
}

#[test]
fn delete_tag_forces_a_scope_free() {
    // `@delete var t = make()` frees `t` at scope close even though the analysis
    // would leak a returned pointer. `@delete` is the local-scope override.
    let src = "\
class Thing {
  public function new() {}
}

class C {
  public function new() {}
  public function make():Thing { return new Thing(); }
  public function run():Void {
    @delete var t:Thing = make();
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_deltag_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("D.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("D"))
        .unwrap();
    let body = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        body.contains("delete t;"),
        "@delete forces a scope-close free of the local:\n{body}"
    );
}

#[test]
fn array_index_write_grows_the_vector() {
    // Haxe auto-extends an array on an out-of-range write (`a[i] = v` past the end
    // grows it); C++ `std::vector::operator[]` would be out-of-bounds UB. The write
    // must be preceded by a grow-guard. A map write (`std::map::operator[]` inserts)
    // must NOT be guarded.
    let src = "\
class G {
  public function new() {}
  public function fill(n:Int):Array<Int> {
    var a:Array<Int> = [];
    for (i in 0...n) {
      a[i] = -1;
    }
    return a;
  }
  public function put(m:Map<String,Int>):Void {
    m[\"x\"] = 1;
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_arrgrow_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("G.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("G"))
        .unwrap();
    let body = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        body.contains(".resize("),
        "array index write must grow the vector first:\n{body}"
    );
    assert!(
        body.contains(">= a.size()) a.resize("),
        "the grow-guard resizes when the index is past the end:\n{body}"
    );
    // The map write inserts on its own — it must not be wrapped in a resize-guard.
    assert!(
        body.contains("m[\"x\"] = 1") && !body.contains("m.resize("),
        "map index writes are not guarded:\n{body}"
    );
}

#[test]
fn owned_marker_deletes_injected_pointers() {
    // `@owned` is the tie-breaker for injected pointers the automatic rules can't
    // tell from a borrow: a ctor parameter stored into a field. A scalar pointer
    // field gets a plain `delete`; a container field is freed element-wise (never
    // a flat `delete` on the std::vector).
    let src = "\
class Dep {
  public function new() {}
}

class Widget {
  @owned var dep:Dep;
  @owned var kids:Array<Dep>;
  public function new(dep:Dep, kids:Array<Dep>) {
    this.dep = dep;
    this.kids = kids;
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_owned_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("W.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("W"))
        .unwrap();
    let header = hatchet::codegen::generate_header(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        header.contains("delete this->dep;"),
        "@owned scalar pointer → plain delete:\n{header}"
    );
    assert!(
        header.contains("delete this->kids[_i0];"),
        "@owned container → element-wise delete loop:\n{header}"
    );
    assert!(
        !header.contains("delete this->kids;"),
        "@owned container must not flat-delete the std::vector:\n{header}"
    );
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

class Quad {
  public function new() {}
  public function set(a:Coords, ?b:Coords):Void {}
}

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
class Cell {
  public var value(default, null):Int;
  public function new(v:Int) { this.value = v; }
}

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
    assert!(
        !out.contains("new Cell(7)->"),
        "no field access directly on a new-expression:\n{out}"
    );
    assert!(
        !out.contains("delete "),
        "the hoisted wrapper must not be freed:\n{out}"
    );
}

#[test]
fn optional_string_param_null_check_lowers_to_empty() {
    // An *optional* `?s:String` param defaults to `""`, so a "was it passed?" check
    // genuinely reads as empty: `s == null` → `s.empty()`, `s != null` → `!s.empty()`.
    // (A non-optional value `String` compared to null is a hard error instead — see
    // `string_vs_null_is_an_error` — since a value string is never null.)
    let src = "\
class S {
  public function new() {}
  public function run(?palette:String):Void {
    if (palette == null) { palette = \"default\"; }
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

    assert!(
        out.contains("if (palette.empty()) {"),
        "`== null` on an optional String param → `.empty()`:\n{out}"
    );
    assert!(
        !out.contains("== NULL") && !out.contains("!= NULL"),
        "no NULL comparison on a string:\n{out}"
    );
}

#[test]
fn string_vs_null_is_an_error() {
    // A non-optional value `String` is never null, so comparing it to `null` is a
    // category error — Hatchet must reject it (steering to `!= ""` or `Null<String>`)
    // rather than silently guess `!s.empty()`.
    let src = "\
class S {
  var name:String;
  public function new() { this.name = \"\"; }
  public function run():Bool { return this.name != null; }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_strnullerr_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("S.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("S"))
        .unwrap();
    let (_, _, errors) = generate_source_diagnostics(&prog, idx, 1, false).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        errors
            .iter()
            .any(|(_, e)| e.contains("never null") && e.contains("Null<String>")),
        "expected a String-vs-null error, got: {errors:?}"
    );
}

#[test]
fn string_tier1_methods_map_to_std_string() {
    // Tier-1 Haxe `String` API → single C++98 `std::string` expressions.
    let src = "\
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
    assert!(
        !out.contains("delete copy"),
        "string value must not be deleted:\n{out}"
    );
}

#[test]
fn type_ascription_honors_the_ascribed_type() {
    // `(expr : Type)` is a compile-time hint with no runtime effect; the ascribed
    // type drives inference where the inner expression's own type is uninformative
    // (a class-typed `null`, an empty array literal).
    let src = "\
class Widget { public function new() {} }

class A {
  public function new() {}
  public function run():Void {
    var w = (null : Widget);
    var xs = ([] : Array<Int>);
  }
  // A scalar ascription pins the C++ arithmetic type with a real cast, so the
  // value does not keep its own (here `double`) type and silently narrow.
  public function asFloat(s:String):cpp.Float32 {
    return (Std.parseFloat(s) : cpp.Float32);
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_ascr_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("A.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("A"))
        .unwrap();
    let out = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    // The ascription gives `null` a pointer type and `[]` an element type.
    assert!(
        out.contains("Widget* w = NULL;"),
        "class-typed null follows the ascription:\n{out}"
    );
    assert!(
        out.contains("std::vector<int> xs"),
        "empty array literal follows the ascription:\n{out}"
    );
    // A scalar ascription emits a real C cast to the ascribed arithmetic type, so a
    // `double` (here `atof`) is pinned to `float` rather than narrowed implicitly.
    assert!(
        out.contains("(float) atof("),
        "scalar ascription casts to the ascribed arithmetic type:\n{out}"
    );
}

