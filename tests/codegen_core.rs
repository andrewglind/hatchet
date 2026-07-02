//! Core lowering: nullable lints, overloads, file finals, interpolation, trace, conditional compilation.
mod common;
use common::*;

#[test]
fn nullable_misuse_is_warned() {
    // A nullable (`Null<T>`) result assigned to a non-`Null<T>` local must warn.
    let src = "\
typedef Pt = { var x:Int; var y:Int; }

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
    let (_, warnings, _) = generate_source_diagnostics(&prog, idx, 1, false).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    // The misuse is the `var p:Pt = this.find();` on line 7 of `src`.
    assert!(
        warnings
            .iter()
            .any(|(line, w)| *line == 7 && w.contains("Null<T>")),
        "expected a nullable warning on line 7, got: {warnings:?}"
    );
}

#[test]
fn discarded_nullable_call_is_extracted_to_a_local() {
    // A bare `Null<T>` call result the developer discards is not warned about —
    // Hatchet binds it to a fresh local and frees it at scope close, so the heap
    // object the callee `new`ed does not leak.
    let src = "\
typedef Pt = { var x:Int; var y:Int; }

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
    let (source, warnings, _) = generate_source_diagnostics(&prog, idx, 1, false).unwrap();
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

    assert!(
        out.contains("std::string s = this->props->GetOr(std::string(\"a\"), std::string(\"x\"))"),
        "String overload → std::string + wrapped literals:\n{out}"
    );
    assert!(
        out.contains("int n = this->props->GetOr(std::string(\"b\"), 5)"),
        "Int overload → int:\n{out}"
    );
    assert!(
        out.contains("bool f = this->props->GetOr(std::string(\"c\"), false)"),
        "Bool overload → bool:\n{out}"
    );
    // The resolved calls use the concrete overload types; the `Dynamic` marker on the
    // (native, never-emitted) canonical signature must not surface as a `void*` here.
    assert!(
        !out.contains("void*"),
        "overload resolution yields concrete types, no void* leak:\n{out}"
    );
}

#[test]
fn overload_matching_accepts_dotted_cpp_param_types() {
    // Regression: an `@:overload` parameter typed `cpp.StdString` / `cpp.Float32`
    // (a dotted name) must match a `std::string` / `float` argument — the dotted
    // name has to be split into path segments so the leaf maps through the
    // primitive/`cpp.*` mapper (otherwise the whole `"cpp.StdString"` is the base
    // and never equals `"std::string"`, so no overload matches).
    let src = "\
@:native(\"Props\") @:include(\"props.h\") @:structAccess
extern class IProps {
  @:overload(function(k:cpp.StdString, d:Int):Int {})
  @:overload(function(k:cpp.StdString, d:cpp.Float32):cpp.Float32 {})
  public function GetOr(k:cpp.StdString, d:Dynamic):Dynamic;
}
class C {
  var props:cpp.Pointer<IProps>;
  public function new() {}
  public function run():Void {
    var n = this.props.GetOr(\"a\", 5);
    var f = this.props.GetOr(\"b\", cast(1.5, cpp.Float32));
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_overload_dotted_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("P.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("P"))
        .unwrap();
    let (out, _warnings, errors) = generate_source_diagnostics(&prog, idx, 1, false).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        !errors.iter().any(|(_, msg)| msg.contains("overload")),
        "cpp.StdString param must match a std::string arg (no overload mismatch): {:?}",
        errors
    );
    assert!(
        out.contains("int n = this->props->GetOr("),
        "Int overload return type resolves:\n{out}"
    );
    // A typed `cast(_, cpp.Float32)` arg selects the float overload (not the first).
    assert!(
        out.contains("float f = this->props->GetOr("),
        "typed cast selects the cpp.Float32 overload:\n{out}"
    );
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
    assert!(
        out.contains("static const double SCALE = 2.0;"),
        "scalar final → static const:\n{out}"
    );
    assert!(
        !out.contains("#define SCALE"),
        "scalar final must not be a #define:\n{out}"
    );
    assert!(
        out.contains("static const Coord ORIGIN = { 0.0, 0.0 };"),
        "struct final → aggregate const:\n{out}"
    );
    assert!(
        out.contains("static const Coord CORNER = { 16.0, 32.0 };"),
        "struct final aggregate in field order:\n{out}"
    );
    // Array final → builder helper + const vector object (stays a vector).
    assert!(
        out.contains("_init_TABLE() {"),
        "array final → builder helper:\n{out}"
    );
    assert!(
        out.contains("v;")
            && out.contains("v.push_back(ORIGIN);")
            && out.contains("v.push_back(CORNER);"),
        "builder declares v and push_backs elements:\n{out}"
    );
    assert!(
        out.contains("return v;"),
        "builder returns the vector:\n{out}"
    );
    assert!(
        out.contains("TABLE = _init_TABLE();") && out.contains("static const std::vector<Coord"),
        "array final → const vector object:\n{out}"
    );
    // Call site is unchanged — it indexes the vector object directly.
    assert!(
        out.contains("return TABLE[i];"),
        "call site indexes the vector object:\n{out}"
    );
    assert!(
        !out.contains("Coord TABLE["),
        "array final must not become a C array:\n{out}"
    );
}

#[test]
fn array_pop_removes_and_returns_the_last_element() {
    // `Array.pop()` must both read the last element AND shrink the vector — a bare
    // `back()` (the prior lowering) never removed it.
    let out = gen_one(
        "class Q {\n  public var items(default, null):Array<Int>;\n  public function new() { this.items = []; }\n  public function take():Int { return this.items.pop(); }\n}\n",
        "Q",
    );
    assert!(out.contains(".back()"), "reads the last element:\n{out}");
    assert!(
        out.contains(".pop_back()"),
        "and removes it (the fix):\n{out}"
    );
}

#[test]
fn string_concat_with_int_operands() {
    // `x + "," + y` with Int operands is string concatenation, not `int + const char*`
    // pointer arithmetic. The ints are formatted and the chain is a `std::string`.
    let out = gen_one(
        "class S {\n  public function new() {}\n  public function label(x:Int, y:Int):String { return x + \",\" + y; }\n}\n",
        "S",
    );
    assert!(
        out.contains("sprintf("),
        "int operands are formatted to text:\n{out}"
    );
    assert!(
        out.contains("std::string(") && out.contains(" + "),
        "result is a std::string concatenation:\n{out}"
    );
    assert!(
        !out.contains("x + \",\""),
        "must not emit raw `int + const char*` pointer math:\n{out}"
    );
}

#[test]
fn cpp_qualified_fixed_width_uints_map_to_uint_aliases() {
    // hxcpp's built-in `cpp.UInt8/16/32` (qualified) map to the fixed-width C++ aliases
    // by their last path segment — in params, return types, and Array elements — so a
    // project can use the idiomatic `cpp.*` types instead of a homegrown `UInt` shim.
    let out = gen_one(
        "package demo;\nclass W {\n  public function new() {}\n  public function f(a:cpp.UInt8, b:cpp.UInt16, t:Array<cpp.UInt32>):cpp.UInt32 { return b; }\n}\n",
        "W",
    );
    assert!(
        out.contains("uint8_t a"),
        "cpp.UInt8 param → uint8_t:\n{out}"
    );
    assert!(
        out.contains("uint16_t b"),
        "cpp.UInt16 param → uint16_t:\n{out}"
    );
    assert!(
        out.contains("std::vector<uint32_t"),
        "Array<cpp.UInt32> → std::vector<uint32_t>:\n{out}"
    );
    assert!(
        out.contains("uint32_t W::f"),
        "cpp.UInt32 return → uint32_t:\n{out}"
    );
}

#[test]
fn interpolation_builds_incrementally_without_a_guessed_buffer() {
    // A string operand is appended directly (`s += part`), so an arbitrarily long value
    // can never overflow a fixed buffer; a numeric operand is formatted into a buffer
    // sized by its TYPE (not guessed from the value).
    let out = gen_one(
        "class G {\n  public function new() {}\n  public function f(s:String, n:Int):String { return 'a${s}b${n}c'; }\n}\n",
        "G",
    );
    assert!(
        out.contains("std::string "),
        "builds a std::string accumulator:\n{out}"
    );
    assert!(
        out.contains("+= s"),
        "the string operand is appended directly (unbounded-safe):\n{out}"
    );
    // No single value-guessed buffer (the old `char buf[n*50+lit]` form).
    assert!(
        !out.contains("[50]") && !out.contains("* 50"),
        "no guessed buffer size:\n{out}"
    );
    // The numeric operand still gets a type-bounded buffer.
    assert!(
        out.contains("char ") && out.contains("sprintf(") && out.contains("[24]"),
        "int → type-bounded buffer:\n{out}"
    );
}

#[test]
fn range_loop_variables_are_unique_per_loop() {
    // VC6 uses the pre-standard `for` scope: a `for (int i ...)` init variable
    // leaks into the enclosing block, so reusing the same Haxe loop name across
    // several range loops/comprehensions in one function would redeclare it
    // (error C2374). Each generated `for`-init must get a distinct name even when
    // the Haxe source reuses `i`.
    let out = gen_one(
        "\
         class G {\n\
         \tpublic function new() {}\n\
         \tpublic function f(n:Int):Int {\n\
         \t\tvar a:Array<Int> = [for (i in 0...n) -1];\n\
         \t\tvar b:Array<Int> = [for (i in 0...n) 0];\n\
         \t\tvar total:Int = 0;\n\
         \t\tfor (i in 0...n) total += i;\n\
         \t\tfor (i in 0...n) total += i;\n\
         \t\treturn total + a.length + b.length;\n\
         \t}\n\
         }\n",
        "G",
    );
    // No two `for`-inits may share a control-variable name. Collect every
    // `for (int <name> = ` declaration and assert they are all distinct.
    let mut names = Vec::new();
    for piece in out.split("for (int ").skip(1) {
        let name: String = piece.chars().take_while(|c| *c != ' ').collect();
        names.push(name);
    }
    assert!(
        names.len() >= 4,
        "expected four range loops, found {}:\n{out}",
        names.len()
    );
    let mut sorted = names.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        names.len(),
        "range loop control variables must be unique: {names:?}\n{out}"
    );
    // And the user's bare `i` must not survive as a raw for-init (it was renamed).
    assert!(
        !out.contains("for (int i ="),
        "the reused Haxe `i` must be renamed:\n{out}"
    );
}

#[test]
fn bare_enum_constant_is_qualified_in_expression_position() {
    // Returning (or assigning) a bare enum variant must qualify it with the
    // enum's `struct E_` scope — the raw `Red` is undeclared in C++.
    let out = gen_one(
        "\
         enum Color { Red; Green; }\n\
         \
         class Paint {\n\
         \tpublic function new() {}\n\
         \tpublic function pick():Color { return Green; }\n\
         }\n",
        "Paint",
    );
    assert!(
        out.contains("return Color_::Green;"),
        "bare enum variant must be scoped:\n{out}"
    );
    assert!(
        !out.contains("return Green;"),
        "unqualified variant must not leak:\n{out}"
    );
}

#[test]
fn final_constant_references_are_namespace_qualified() {
    // A public `final` is a `static const` inside its module's namespace, so a
    // reference from another namespace is qualified (`native::MAX_CHARS`), and a
    // global-scope `@cexport` export qualifies a same-module ref too
    // (`game::SCENE_ID`), while a reference from within the same namespace stays
    // bare.
    let native = "\
package native;
final MAX_CHARS:Int = 100;
";
    let scenes = "\
package game;
import native.Native;
final SCENE_ID:Int = 7;

class Scene {
  public function new() {}
  public function cap():Int { return MAX_CHARS; }        // other ns → native::
  public function id():Int { return SCENE_ID; }          // same ns → bare
}

@cexport function Pick(n:Int):Int {
  switch (n) {
    case SCENE_ID: return 1;                              // global scope → game::
    default: return 0;
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_finalref_{}", std::process::id()));
    std::fs::create_dir_all(dir.join("native")).unwrap();
    std::fs::create_dir_all(dir.join("game")).unwrap();
    std::fs::write(dir.join("native").join("Native.hx"), native).unwrap();
    std::fs::write(dir.join("game").join("Game.hx"), scenes).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("Game"))
        .unwrap();
    let out = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        out.contains("return native::MAX_CHARS;"),
        "native const → native namespace:\n{out}"
    );
    assert!(
        out.contains("return SCENE_ID;"),
        "same-namespace ref stays bare:\n{out}"
    );
    assert!(
        out.contains("case game::SCENE_ID:"),
        "global-scope ref → namespace-qualified:\n{out}"
    );
}

#[test]
fn map_get_lowers_to_iterator_with_existence_check() {
    // `Map.get(k)` is `Null<V>`; for a value `V` there is no C++ null, so the local
    // is bound to a map iterator. A null check on it is the existence check
    // (`it == map.end()` / `!= map.end()`), value/member use is `it->second`. The
    // shape is not enforced: the null check need not be next, nor exit the scope.
    let src = "\
typedef Entry = { var count:Int; }

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
        out.contains("std::map<std::string, Entry>::iterator")
            && out.contains("this->entries.find(key)"),
        "get → find() iterator:\n{out}"
    );
    assert!(
        out.contains("!= this->entries.end()"),
        "`!= null` → `!= map.end()`:\n{out}"
    );
    assert!(
        out.contains("->second.count"),
        "member use → it->second.count:\n{out}"
    );
    assert!(
        !out.contains("== NULL") && !out.contains("[key]"),
        "no NULL compare / operator[]:\n{out}"
    );
}

#[test]
fn for_over_anonymous_array_literal_hoists_the_vector_before_the_loop() {
    // `for (i in [1,2,3])` builds a `std::vector` temporary, emitted *before* the
    // loop (in scope), then iterates it by index.
    let src = "\
class Summer {
  public function new() {}
  public function total():Int {
    var sum:Int = 0;
    for (i in [1, 2, 3]) {
      sum += i;
    }
    return sum;
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_anoniter_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Summer.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("Summer"))
        .unwrap();
    let out = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    // The vector temp and its push_backs are emitted before the for header.
    assert!(
        out.contains("std::vector<int "),
        "array temp is a std::vector<int>:\n{out}"
    );
    assert!(
        out.contains(".push_back(1)") && out.contains(".push_back(3)"),
        "literal elements pushed:\n{out}"
    );
    let push_pos = out
        .find(".push_back(1)")
        .unwrap_or_else(|| panic!("element push expected:\n{out}"));
    let for_pos = out
        .find("for (size_t")
        .unwrap_or_else(|| panic!("index loop expected:\n{out}"));
    assert!(
        push_pos < for_pos,
        "the vector must be built before the loop:\n{out}"
    );
    assert!(
        out.contains("int i = "),
        "loop binds element by value:\n{out}"
    );
    assert!(
        out.contains(".size(); ++"),
        "index loop over .size():\n{out}"
    );
}

#[test]
fn signed_unsigned_comparisons_cast_to_size_t() {
    // `for (i in 0...arr.length)` compares an `int` counter against `.size()`
    // (size_t); MSVC warns (C4018) on the mixed signed/unsigned comparison. C++
    // already converts the signed side to size_t, so the lowering makes that cast
    // explicit — in the loop header and in any body comparison against `.length` —
    // while leaving signed/signed comparisons (`i < n`) untouched.
    let src = "\
class Keys {
  public var objectKeys:Array<String>;
  public function new() { objectKeys = []; }
  public function scan():Void {
    for (i in 0...this.objectKeys.length) {
      if (i < this.objectKeys.length) {
        trace(this.objectKeys[i]);
      }
      var n:Int = 5;
      if (i < n) trace(\"low\");
    }
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_c4018_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Keys.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("Keys"))
        .unwrap();
    let out = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    // The loop header casts the counter against `.size()`.
    assert!(
        out.contains("(size_t)") && out.contains("< this->objectKeys.size(); ++"),
        "loop header compares the counter as size_t:\n{out}"
    );
    // A body comparison against `.length` is also cast.
    assert!(
        out.contains(") < this->objectKeys.size()"),
        "body comparison against .length is cast to size_t:\n{out}"
    );
    // A signed/signed comparison (`i < n`) keeps its plain operands — no spurious cast.
    assert!(
        out.contains(" < n)"),
        "the `i < n` comparison is not cast:\n{out}"
    );
}

#[test]
fn if_expression_comprehension_body_lowers_to_a_hoisted_temp() {
    // An array comprehension whose body is an `if`/`else if`/`else` — each branch a
    // block ending in a value. The `if` is a *value* expression (it has an `else`),
    // so it desugars like a value `switch`: a hoisted temp the branches assign, then
    // the temp is pushed. The element type is the branch value's type (`int`), not
    // the surrounding array type.
    let out = gen_one(
        "class M {\n  public function new() {}\n  public function f(nums:Array<Int>):Array<Int> {\n    return [\n      for (n in nums)\n        if (n < 0) { -1; }\n        else if (n == 0) { var z = 0; z; }\n        else { var p = n * 2; p; }\n    ];\n  }\n}\n",
        "M",
    );
    // Container is a vector of the branch value type, not the array type.
    assert!(
        out.contains("std::vector<int >"),
        "element type is the branch value type:\n{out}"
    );
    assert!(
        !out.contains("std::vector<std::vector"),
        "the array type must not become the element type:\n{out}"
    );
    // The if-expression is hoisted to a temp the branches assign, then pushed.
    assert!(out.contains("int _ifx"), "value `if` hoists a temp:\n{out}");
    assert!(
        out.contains(".push_back(_ifx"),
        "the temp is the produced element:\n{out}"
    );
}

#[test]
fn comprehension_if_without_else_stays_a_filter() {
    // A leading `if` with no `else` is a filter: the element is produced only when
    // the condition holds (it is *not* a value `if`).
    let out = gen_one(
        "class M {\n  public function new() {}\n  public function f(nums:Array<Int>):Array<Int> {\n    return [for (n in nums) if (n > 0) n];\n  }\n}\n",
        "M",
    );
    assert!(
        out.contains("if (n > 0) {"),
        "the filter guards the push:\n{out}"
    );
    assert!(
        out.contains(".push_back(n)"),
        "only the kept element is pushed:\n{out}"
    );
    assert!(
        !out.contains("_ifx"),
        "a no-else filter is not a value if-expression:\n{out}"
    );
}

#[test]
fn value_if_expression_in_assignment_position() {
    // `var s = if (c) a else b` is a value `if`, desugared to a hoisted temp.
    let out = gen_one(
        "class M {\n  public function new() {}\n  public function f(flag:Bool):String {\n    var s = if (flag) \"yes\" else \"no\";\n    return s;\n  }\n}\n",
        "M",
    );
    assert!(
        out.contains("std::string _ifx"),
        "the value `if` hoists a typed temp:\n{out}"
    );
    assert!(
        out.contains("if (flag) {") && out.contains("} else {"),
        "desugars to a statement if:\n{out}"
    );
    assert!(
        out.contains("std::string s = _ifx"),
        "the assignment reads the temp:\n{out}"
    );
}

#[test]
fn trace_lowers_to_printf_with_file_and_line_and_no_trace_strips_it() {
    // `trace(...)` prints `file:line: ` followed by the comma-separated args via a
    // single printf (reusing the interpolation type→spec mapping). `--no-trace`
    // strips the call (and its argument evaluation) to a no-op.
    let src = "\
class Tracer {
  public function new() {}
  public function go(count:Int):Void {
    trace(\"hello\");
    trace(\"count\", count);
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_trace_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Tracer.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("Tracer"))
        .unwrap();

    // Default: trace → printf with the file:line prefix.
    let out = generate_source_diagnostics(&prog, idx, 1, false).unwrap().0;
    assert!(
        out.contains(r#"printf("Tracer.hx:4: %s\n", "hello")"#),
        "string-literal trace → printf with file:line, no .c_str():\n{out}"
    );
    assert!(
        out.contains(r#"printf("Tracer.hx:5: %s, %d\n", "count", count)"#),
        "multi-arg trace → comma-separated specs:\n{out}"
    );

    // --no-traces: the calls are stripped to no-ops.
    let stripped = generate_source_diagnostics(&prog, idx, 1, true).unwrap().0;
    assert!(
        !stripped.contains("printf("),
        "no-traces must emit no printf:\n{stripped}"
    );
    assert!(
        stripped.contains("((void)0);"),
        "no-traces lowers trace to a no-op:\n{stripped}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn conditional_compilation_and_untyped_lower_to_preprocessor_and_verbatim() {
    // `#if FLAG`/`#else`/`#end` become `#ifdef`/`#else`/`#endif`; a statement-level
    // `@:include` becomes an `#include` at that point; `untyped` hands the rest of
    // the statement to C++ verbatim (here `fsqrtf`, which Haxe cannot see).
    let src = "\
class Maths {
  public function new() {}
  public function Dist(dx:Float, dy:Float):Float {
#if DREAMCAST
    @:include(\"<dc/fmath.h>\");
    return untyped fsqrtf(dx * dx + dy * dy);
#else
    return Math.sqrt(dx * dx + dy * dy);
#end
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_ppcond_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Maths.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("Maths"))
        .unwrap();
    let out = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        out.contains("#ifdef DREAMCAST"),
        "`#if FLAG` → `#ifdef FLAG`:\n{out}"
    );
    assert!(out.contains("#else"), "`#else` preserved:\n{out}");
    assert!(out.contains("#endif"), "`#end` → `#endif`:\n{out}");
    assert!(
        out.contains("#include <dc/fmath.h>"),
        "stmt `@:include` → `#include`:\n{out}"
    );
    assert!(
        out.contains("return fsqrtf(dx * dx + dy * dy);"),
        "`untyped` operand emitted verbatim:\n{out}"
    );
    assert!(
        !out.contains("untyped"),
        "the `untyped` keyword must not survive:\n{out}"
    );
    // Preprocessor directives sit at column 0 so they are valid.
    assert!(
        out.contains("\n#ifdef DREAMCAST"),
        "directive at column 0:\n{out}"
    );
    // The #else branch is transpiled normally.
    assert!(
        out.contains("sqrt("),
        "Math.sqrt → sqrt in the else branch:\n{out}"
    );
}

#[test]
fn cpp_code_intrinsic_injects_verbatim_with_transpiled_arg_substitution() {
    // Haxe's `__cpp__("fmt", a, b)` raw-injection intrinsic: the format string is
    // emitted verbatim, `{N}` placeholders are replaced by the *transpiled* args, and
    // it is recognised both bare and under the (C++-redundant) `untyped` wrapper.
    let src = "\
class Native {
  public function new() {}
  public function Clamp(v:Float, lo:Float):Float {
    return untyped __cpp__(\"::fmaxf({0}, {1})\", v, lo);
  }
  public function Now():Float {
    return __cpp__(\"(double)::clock() / CLOCKS_PER_SEC\");
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_cppcode_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Native.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("Native"))
        .unwrap();
    let out = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    // `{0}`/`{1}` are replaced by the transpiled argument expressions (the params
    // `v`/`lo`), and the surrounding C++ is emitted verbatim.
    assert!(
        out.contains("return ::fmaxf(v, lo);"),
        "`__cpp__` substitutes transpiled args into the verbatim string:\n{out}"
    );
    // The bare (un-`untyped`) form is recognised too.
    assert!(
        out.contains("return (double)::clock() / CLOCKS_PER_SEC;"),
        "bare `__cpp__` is recognised without an `untyped` wrapper:\n{out}"
    );
    // Neither the intrinsic name nor the `untyped` keyword survives.
    assert!(
        !out.contains("__cpp__") && !out.contains("untyped"),
        "the `__cpp__`/`untyped` spelling must not survive into C++:\n{out}"
    );
    // Placeholders are fully resolved.
    assert!(
        !out.contains("{0}") && !out.contains("{1}"),
        "all `{{N}}` placeholders must be substituted:\n{out}"
    );
}

#[test]
fn plain_dollar_interpolation_is_supported() {
    // `'$name'` shorthand interpolates the identifier, exactly like `'${name}'`.
    // A `$` not followed by an identifier (here `$5`) stays a literal dollar.
    let src = "\
class Greeter {
  public function new() {}
  public function Greet(name:String):String {
    return 'hi $name costs $5';
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_dollar_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Greeter.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("Greeter"))
        .unwrap();
    let out = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    // Built incrementally: a `std::string` accumulator with the string operand appended
    // directly (no fixed-size buffer — unbounded-safe), the `$5` staying literal.
    assert!(
        out.contains("std::string"),
        "interpolation builds a std::string accumulator:\n{out}"
    );
    assert!(
        out.contains("+= name"),
        "the string operand is appended directly:\n{out}"
    );
    assert!(
        !out.contains("sprintf"),
        "a string interpolation needs no fixed-size buffer:\n{out}"
    );
    assert!(
        out.contains("$5"),
        "a `$` not before an identifier stays a literal:\n{out}"
    );
}

