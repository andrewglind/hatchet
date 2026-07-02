//! Std/Math/String intrinsics, Array/Map methods, and iteration (custom iterators, ordered maps).
mod common;
use common::*;

#[test]
fn string_tier2_case_and_split() {
    // toUpperCase/toLowerCase (ASCII, in-place on a copy) and split → vector, all
    // self-contained C++98 (no <cctype>/<algorithm>).
    let src = "\
class S {
  public function new() {}
  public function run():Void {
    var s:String = \"Hello\";
    var u:String = s.toUpperCase();
    var l:String = s.toLowerCase();
    var parts:Array<String> = \"a,b,c\".split(\",\");
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_str2_{}", std::process::id()));
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

    // Case mapping is an ASCII range check + offset (no toupper/tolower).
    assert!(
        out.contains(">= 'a' && ") && out.contains("- 'a' + 'A'"),
        "toUpperCase ASCII map:\n{out}"
    );
    assert!(
        out.contains(">= 'A' && ") && out.contains("- 'A' + 'a'"),
        "toLowerCase ASCII map:\n{out}"
    );
    assert!(
        !out.contains("toupper") && !out.contains("tolower"),
        "no <cctype> dependency:\n{out}"
    );
    // split builds a vector via find/substr (npos sentinel), no <algorithm>.
    assert!(
        out.contains("std::vector<std::string >"),
        "split returns a vector:\n{out}"
    );
    assert!(
        out.contains(".find(") && out.contains(".substr(") && out.contains("std::string::npos"),
        "split tokenizes with find/substr:\n{out}"
    );
    assert!(
        !out.contains("std::find"),
        "no <algorithm> dependency:\n{out}"
    );
}

#[test]
fn math_intrinsics_map_inline() {
    // Math API → inline C++98 expressions (no helper functions / shims).
    let src = "\
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
        "sin(1.0)",
        "pow(2.0, 8.0)",
        "atan2(1.0, 2.0)",
        "floor(2.7)",                  // Math.ffloor → Float (double) floor
        "((int)floor(2.7))",           // Math.floor → int
        "((int)floor((2.5) + 0.5))",   // Math.round → int
        "(rand() / (RAND_MAX + 1.0))", // Math.random ∈ [0,1)
        "3.141592653589793",           // Math.PI literal (no M_PI), double precision
    ] {
        assert!(out.contains(needle), "missing `{needle}` in:\n{out}");
    }
    // min is NaN-propagating inline (no helper), and no shim names leak in.
    assert!(
        out.contains("== (1.0) ? (2.0) : (1.0)"),
        "min NaN-aware inline:\n{out}"
    );
    assert!(
        !out.contains("haxe_min") && !out.contains("haxe_"),
        "no helper shims:\n{out}"
    );
}

#[test]
fn std_intrinsics_map_to_c_stdlib() {
    // Std.string/parseInt/parseFloat → inline C++98 (sprintf / strtol / atof), no
    // custom runtime helpers.
    let src = "\
class S {
  public function new() {}
  public function run():Void {
    var n:Int = 42;
    var s1:String = Std.string(n);
    var s2:String = Std.string(\"hi\");
    var s3:String = Std.string(true);
    var i:Int = Std.parseInt(\"0x1F\");
    var f:Float = Std.parseFloat(\"3.14\");
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_std_{}", std::process::id()));
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

    // Std.string(int) → sprintf %d into a buffer, returned as std::string.
    assert!(
        out.contains("sprintf(") && out.contains("\"%d\""),
        "Std.string(int) via sprintf:\n{out}"
    );
    assert!(
        out.contains("std::string("),
        "Std.string returns a std::string:\n{out}"
    );
    // Std.string of a literal and a bool.
    assert!(
        out.contains("std::string(\"hi\")"),
        "Std.string(literal):\n{out}"
    );
    assert!(
        out.contains("std::string(\"true\")") && out.contains("std::string(\"false\")"),
        "Std.string(bool) maps to \"true\"/\"false\":\n{out}"
    );
    // parseInt (hex-aware) and parseFloat — a bare string literal is a const char*,
    // so no invalid `.c_str()` is appended.
    assert!(
        out.contains("(int)strtol(\"0x1F\", NULL, 0)"),
        "Std.parseInt → strtol base 0:\n{out}"
    );
    assert!(
        out.contains("atof(\"3.14\")"),
        "Std.parseFloat → atof:\n{out}"
    );
    assert!(
        !out.contains("\".c_str()") && !out.contains("\"0x1F\".c_str()"),
        "no .c_str() on a string literal:\n{out}"
    );
    // No custom runtime helpers.
    assert!(!out.contains("haxe_"), "no helper shims:\n{out}");
}

#[test]
fn array_and_map_methods_lower_to_inline_cpp() {
    // Array indexOf/contains/remove/reverse/copy/join and Map set/remove/keys →
    // self-contained C++98 (explicit loops; no <algorithm>, no runtime helpers).
    let src = "\
class C {
  public function new() {}
  public function run():Void {
    var xs:Array<Int> = [1, 2, 3];
    var has:Bool = xs.contains(2);
    var i:Int = xs.indexOf(3);
    var r:Bool = xs.remove(1);
    xs.reverse();
    var ys:Array<Int> = xs.copy();
    var ji:String = xs.join(\"-\");
    var names:Array<String> = [\"a\", \"b\"];
    var j:String = names.join(\", \");
    var m:Map<String, Int> = new Map<String, Int>();
    m.set(\"k\", 7);
    var rm:Bool = m.remove(\"k\");
    var ks:Array<String> = m.keys();
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_containers_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("C.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("C"))
        .unwrap();
    let out = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    // Array methods.
    assert!(
        out.contains("== 2)") && out.contains("= true; break;"),
        "contains scan:\n{out}"
    );
    assert!(
        out.contains("int ") && out.contains("= -1;") && out.contains("== 3)"),
        "indexOf scan:\n{out}"
    );
    assert!(
        out.contains(".erase(") && out.contains(".begin() +"),
        "remove erases by index:\n{out}"
    );
    assert!(out.contains(".size() / 2"), "reverse swap loop:\n{out}");
    assert!(
        out.contains("std::vector<int>(xs)"),
        "copy via copy-constructor:\n{out}"
    );
    assert!(
        out.contains("sprintf(") && out.contains("\"%d\""),
        "numeric join via sprintf:\n{out}"
    );
    // Map methods.
    assert!(out.contains("[\"k\"] = 7"), "Map.set → m[k]=v:\n{out}");
    assert!(
        out.contains(".erase(\"k\") != 0"),
        "Map.remove → erase != 0:\n{out}"
    );
    assert!(
        out.contains("->first") && out.contains("std::vector<std::string >"),
        "Map.keys → vector of keys:\n{out}"
    );
    // No <algorithm>-only names or runtime helpers leak in.
    assert!(
        !out.contains("std::find") && !out.contains("std::reverse"),
        "no <algorithm> dependency:\n{out}"
    );
    assert!(!out.contains("haxe_"), "no helper shims:\n{out}");
}

#[test]
fn map_iteration_lowers_to_a_std_map_iterator() {
    // `for (v in map)` iterates values; `for (k => v in map)` binds key and value.
    // Both lower to a std::map iterator loop (not the index path).
    let src = "\
class M {
  public function new() {}
  public function run():Void {
    var m:Map<String, Int> = new Map<String, Int>();
    m.set(\"a\", 1);
    var total:Int = 0;
    for (v in m) { total += v; }
    for (k => val in m) { total += val; }
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_mapiter_{}", std::process::id()));
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

    assert!(
        out.contains("std::map<std::string, int>::const_iterator"),
        "map iterator type:\n{out}"
    );
    assert!(
        out.contains(".begin();") && out.contains(".end();"),
        "iterator bounds:\n{out}"
    );
    // `for (v in m)` binds the value only.
    assert!(
        out.contains("int v = ") && out.contains("->second;"),
        "value binding:\n{out}"
    );
    // `for (k => val in m)` binds key and value.
    assert!(
        out.contains("std::string k = ") && out.contains("->first;"),
        "key binding:\n{out}"
    );
    assert!(
        out.contains("int val = ") && out.contains("->second;"),
        "key/value value binding:\n{out}"
    );
    // The map must NOT be iterated with the index path.
    assert!(
        !out.contains("m.size()") && !out.contains("m[_i"),
        "map must not use index iteration:\n{out}"
    );
}

#[test]
fn custom_iterator_lowers_to_a_hasnext_next_while_loop() {
    // A value with `hasNext`/`next` is an Iterator; a value with `iterator()` is an
    // Iterable. Both lower to a `while (it.hasNext()) { T x = it.next(); … }` loop,
    // and an Iterable whose `iterator()` returns a heap (reference-type) iterator
    // makes the loop `delete` it once after the loop.
    let src = "\
class It {
  var n:Int;
  public function new(s:Int) { this.n = s; }
  public function hasNext():Bool { return this.n > 0; }
  public function next():Int { var v = this.n; this.n -= 1; return v; }
}
class Seq {
  public function new() {}
  public function iterator():It { return new It(3); }
}
class C {
  public function new() {}
  public function run():Void {
    var total:Int = 0;
    var it = new It(5);
    for (x in it) { total += x; }
    var s = new Seq();
    for (y in s) { total += y; }
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_customiter_unit_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("C.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("C"))
        .unwrap();
    let out = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    // The Iterator is aliased (not copied/freed); the Iterable's heap iterator is
    // allocated via iterator() and deleted exactly once.
    assert!(
        out.contains("->hasNext()") && out.contains("->next()"),
        "expected a hasNext/next while loop:\n{out}"
    );
    assert!(
        out.contains("->iterator()"),
        "expected an iterator() call for the Iterable:\n{out}"
    );
    assert!(
        out.contains("delete ") && out.contains("->iterator();"),
        "the heap iterator must be freed:\n{out}"
    );
    // No index path for a custom iterator.
    assert!(
        !out.contains(".size();"),
        "a custom iterator must not use the array index path:\n{out}"
    );
}

#[test]
fn custom_iterator_key_value_form_is_an_error() {
    // `key => value` over a value-only custom iterator is unsupported (it needs a
    // Map or Array); Hatchet must reject it rather than emit wrong code.
    let src = "\
class It {
  var n:Int;
  public function new(s:Int) { this.n = s; }
  public function hasNext():Bool { return this.n > 0; }
  public function next():Int { var v = this.n; this.n -= 1; return v; }
}
class C {
  public function new() {}
  public function run():Void {
    var it = new It(3);
    for (k => v in it) {}
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_customiter_kv_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("C.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("C"))
        .unwrap();
    let (_, _, errors) = generate_source_diagnostics(&prog, idx, 1, false).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        errors
            .iter()
            .any(|(_, e)| e.contains("cannot iterate") && e.contains("yields values only")),
        "expected a key=>value-over-custom-iterator error, got: {errors:?}"
    );
}

#[test]
fn iterator_only_via_alias_or_base_class_fails_loudly() {
    // The custom-iterator detection needs the protocol methods on the iterated type
    // itself. Reaching them only through a typedef alias, or by inheriting them from
    // a base class, is NOT detected — and must be a hard error (with a specific
    // message), in both a `for` statement and a comprehension, never silent wrong
    // code (a comprehension previously assumed `.size()`/`[]` for any unknown type).
    let src = "\
class It {
  var n:Int;
  public function new(s:Int) { this.n = s; }
  public function hasNext():Bool { return this.n > 0; }
  public function next():Int { var v = this.n; this.n -= 1; return v; }
}
typedef Alias = It;
class Sub extends It {
  public function new(s:Int) { super(s); }
}
class C {
  public function new() {}
  public function viaAliasFor():Void {
    var a:Alias = new It(3);
    for (x in a) {}
  }
  public function viaAliasCompr():Array<Int> {
    var a:Alias = new It(3);
    return [for (x in a) x];
  }
  public function viaBase():Void {
    var s = new Sub(3);
    for (x in s) {}
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_iter_indirect_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("C.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("C"))
        .unwrap();
    let (_, _, errors) = generate_source_diagnostics(&prog, idx, 1, false).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    // Two alias errors (the `for` and the comprehension) naming the typedef cause.
    let alias_errs = errors
        .iter()
        .filter(|(_, e)| e.contains("cannot iterate") && e.contains("typedef alias"))
        .count();
    assert!(
        alias_errs >= 2,
        "expected loud alias errors for both the for-loop and the comprehension, got: {errors:?}"
    );
    // The base-class case is also rejected, with its own hint.
    assert!(
        errors
            .iter()
            .any(|(_, e)| e.contains("cannot iterate") && e.contains("base class")),
        "expected a loud base-class iterator error, got: {errors:?}"
    );
}

#[test]
fn ordered_map_whole_value_use_is_an_error() {
    // An @orderedMap field has no single map object — using it as a whole value
    // (returning it, here) must be a hard error, not silently emit `this->object`.
    let src = "\
class B {
  @orderedMap public var object:Map<String, Int>;
  public function new() {}
  public function leak():Map<String, Int> { return this.object; }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_omwhole_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("B.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("B"))
        .unwrap();
    let (_, _, errors) = generate_source_diagnostics(&prog, idx, 1, false).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        errors
            .iter()
            .any(|(_, e)| e.contains("@orderedMap") && e.contains("whole value")),
        "expected a whole-map-use error, got: {errors:?}"
    );
}

#[test]
fn ordered_map_emits_parallel_vectors_and_set_lowering() {
    // `@orderedMap var m:Map<K,V>` → two vectors; `set` is a find-or-append scan.
    let src = "\
class B {
  @orderedMap public var m:Map<String, Int>;
  public function new() { this.m = new Map(); }
  public function put(k:String, v:Int):Void { this.m.set(k, v); }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_omcg_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("B.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("B"))
        .unwrap();
    let out = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        out.contains("m_keys") && out.contains("m_vals") && !out.contains("std::map<"),
        "expected parallel vectors, no std::map:\n{out}"
    );
    assert!(
        out.contains("m_keys.push_back") && out.contains("m_vals.push_back"),
        "expected a find-or-append set lowering:\n{out}"
    );
}

#[test]
fn anonymous_struct_container_element_is_an_error() {
    // `Array<{anon struct}>` would lower to a useless `std::vector<void*>`; it must
    // be rejected (a named typedef is the supported form), not silently miscompiled.
    let src = "\
class B {
  public var pairs:Array<{ key:String, val:Int }>;
  public function new() { this.pairs = []; }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_anonc_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("B.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("B"))
        .unwrap();
    let errs = unsupported_construct_errors(&prog, idx);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        errs.iter()
            .any(|d| d.message.contains("anonymous struct as a container element")),
        "expected an anonymous-struct-element error, got: {:?}",
        errs.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn array_key_value_iteration_binds_the_index() {
    // `for (index => value in array)` is valid Haxe — the key is the Int index.
    // Exercised in both statement and comprehension position.
    let src = "\
class A {
  public function new() {}
  public function run(xs:Array<String>):Void {
    for (i => s in xs) { trace(i); }
  }
  public function idx(xs:Array<String>):Array<Int> {
    return [for (i => s in xs) i];
  }
}
";
    let out = gen_one(src, "A");
    // Statement form: an index loop binding the Int key and the element value.
    assert!(
        out.contains("int i = (int)"),
        "Array key bound as Int index:\n{out}"
    );
    assert!(
        out.contains("std::string s = "),
        "Array element value bound:\n{out}"
    );
    // Comprehension form: same bindings, pushing the key.
    assert!(
        out.contains(".push_back(i)"),
        "comprehension yields the index key:\n{out}"
    );
    // It must use index iteration, never the map iterator path.
    assert!(
        !out.contains("::const_iterator"),
        "Array iteration is not a map iterator:\n{out}"
    );
}

#[test]
fn member_access_on_a_native_renamed_container_element_resolves() {
    // A container element whose type is `@:native`-renamed is spelled by its C++
    // name in the vector base (`std::vector<val*>`). Recovering the element's
    // `TypeInfo` to resolve `vals[i].s` must match that native name, not only the
    // Haxe name — otherwise `.s` is untyped and the map value degrades to `void*`.
    let src = "\
@:native(\"val\")
class Val {
  public var s:String;
  public var keys:Array<String>;
  public var vals:Array<Val>;
  public function new() { this.s = \"\"; this.keys = []; this.vals = []; }
}
abstract Holder(Val) {
  public function new() { this = new Val(); }
  public function toMap():Map<String,String> {
    return [for (i => k in this.keys) k => this.vals[i].s];
  }
}
";
    let out = gen_one(src, "Holder");
    // The field `s` resolves to `std::string`, so the map value type is concrete.
    assert!(
        out.contains("std::map<std::string, std::string >"),
        "native-renamed element field resolves (no void* fallback):\n{out}"
    );
    assert!(!out.contains("void*"), "no void* value type:\n{out}");
}

#[test]
fn for_over_a_non_container_is_an_error() {
    // Iterating a value that is neither a range, Array, or Map, nor implements the
    // Iterator/Iterable protocol (here a bare `Int`) must fail loudly rather than
    // emit invalid C++.
    let src = "\
class B {
  public function new() {}
  public function run():Void {
    var n:Int = 5;
    for (x in n) { }
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_baditer_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("B.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("B"))
        .unwrap();
    let (_, _, errors) = generate_source_diagnostics(&prog, idx, 1, false).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        errors.iter().any(|(_, e)| e.contains("cannot iterate")
            && e.contains("Iterator/Iterable protocol")),
        "expected a non-container iteration error, got: {errors:?}"
    );
}

#[test]
fn lambda_return_type_from_function_type_annotation() {
    // A lambda with no `cast`/`:T` hint takes its return type from the binding's
    // function-type annotation `(Int, Int) -> Int` (the second of the two hint
    // forms; see `resolve_lambda_ret`).
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
    assert!(
        out.contains("return a * b;"),
        "lambda body transpiled:\n{out}"
    );
}

#[test]
fn elseif_conditional_compilation_maps_to_elif_defined() {
    // `#elseif FLAG` lowers to `#elif defined(FLAG)` (C++98 has no `#elifdef`);
    // `#if`/`#else`/`#end` keep their established mappings.
    let out = gen_one(
        "class Cond {\n  public function new() {}\n  public function pick():Int {\n    #if WIN98\n    return 1;\n    #elseif DREAMCAST\n    return 2;\n    #else\n    return 3;\n    #end\n  }\n}\n",
        "Cond",
    );
    assert!(out.contains("#ifdef WIN98"), "#if → #ifdef:\n{out}");
    assert!(
        out.contains("#elif defined(DREAMCAST)"),
        "#elseif → #elif defined(...):\n{out}"
    );
    assert!(out.contains("#else"), "#else preserved:\n{out}");
    assert!(out.contains("#endif"), "#end → #endif:\n{out}");
}

#[test]
fn stringbuf_lowers_to_a_string_accumulator() {
    // `new StringBuf()` → `std::string()`; `add`/`addChar` → `+=`; `toString()` → it.
    let out = gen_one(
        "class B {\n  public function new() {}\n  public function build():String {\n    var b = new StringBuf();\n    b.add(\"n=\");\n    b.add(7);\n    b.addChar(33);\n    return b.toString();\n  }\n}\n",
        "B",
    );
    assert!(
        out.contains("std::string"),
        "StringBuf declared as std::string:\n{out}"
    );
    assert!(out.contains("+= "), "add/addChar append with +=:\n{out}");
    assert!(
        !out.contains("new StringBuf"),
        "StringBuf is not heap-allocated:\n{out}"
    );
}

#[test]
fn std_random_lowers_to_guarded_rand_modulo() {
    let out = gen_one(
        "class R {\n  public function new() {}\n  public function roll():Int {\n    return Std.random(6);\n  }\n}\n",
        "R",
    );
    assert!(out.contains("rand() %"), "Std.random → rand() %:\n{out}");
}

#[test]
fn stringtools_statics_lower_without_using() {
    // `StringTools.x(...)` is a plain static call — no `using` needed.
    let out = gen_one(
        "class Stools {\n  public function new() {}\n  public function clean(s:String):String {\n    return StringTools.replace(StringTools.trim(s), \"a\", \"b\");\n  }\n}\n",
        "Stools",
    );
    assert!(
        out.contains(".replace("),
        "StringTools.replace → std::string::replace loop:\n{out}"
    );
    assert!(
        out.contains(".substr("),
        "StringTools.trim → substr of the trimmed range:\n{out}"
    );
}

