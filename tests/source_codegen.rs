//! Body (`.cpp`) transpilation checks against small self-contained programs
//! built in a temp directory (no external corpus required): `super(...)` → init
//! list, `this->`, anonymous struct → named temp, ownership, the Std/Math/String
//! intrinsics, and the sugar lowerings.

use hatchet::codegen::{generate_source, generate_source_diagnostics};
use hatchet::sema::Program;

/// Transpile a single synthetic `.hx` source and return class `stem`'s generated `.cpp`.
fn gen_one(src: &str, stem: &str) -> String {
    let dir = std::env::temp_dir().join(format!("hatchet_t_{stem}_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{stem}.hx")), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some(stem))
        .unwrap();
    let out = generate_source(&prog, idx).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    out
}

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
        warnings.iter().any(|(line, w)| *line == 7 && w.contains("Null<T>")),
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
    assert!(out.contains("_init_TABLE() {"), "array final → builder helper:\n{out}");
    assert!(out.contains("v;") && out.contains("v.push_back(ORIGIN);") && out.contains("v.push_back(CORNER);"), "builder declares v and push_backs elements:\n{out}");
    assert!(out.contains("return v;"), "builder returns the vector:\n{out}");
    assert!(
        out.contains("TABLE = _init_TABLE();") && out.contains("static const std::vector<Coord"),
        "array final → const vector object:\n{out}"
    );
    // Call site is unchanged — it indexes the vector object directly.
    assert!(out.contains("return TABLE[i];"), "call site indexes the vector object:\n{out}");
    assert!(!out.contains("Coord TABLE["), "array final must not become a C array:\n{out}");
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
    assert!(out.contains(".pop_back()"), "and removes it (the fix):\n{out}");
}

#[test]
fn string_concat_with_int_operands() {
    // `x + "," + y` with Int operands is string concatenation, not `int + const char*`
    // pointer arithmetic. The ints are formatted and the chain is a `std::string`.
    let out = gen_one(
        "class S {\n  public function new() {}\n  public function label(x:Int, y:Int):String { return x + \",\" + y; }\n}\n",
        "S",
    );
    assert!(out.contains("sprintf("), "int operands are formatted to text:\n{out}");
    assert!(out.contains("std::string(") && out.contains(" + "), "result is a std::string concatenation:\n{out}");
    assert!(!out.contains("x + \",\""), "must not emit raw `int + const char*` pointer math:\n{out}");
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
    assert!(out.contains("uint8_t a"), "cpp.UInt8 param → uint8_t:\n{out}");
    assert!(out.contains("uint16_t b"), "cpp.UInt16 param → uint16_t:\n{out}");
    assert!(out.contains("std::vector<uint32_t"), "Array<cpp.UInt32> → std::vector<uint32_t>:\n{out}");
    assert!(out.contains("uint32_t W::f"), "cpp.UInt32 return → uint32_t:\n{out}");
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
    assert!(out.contains("std::string "), "builds a std::string accumulator:\n{out}");
    assert!(out.contains("+= s"), "the string operand is appended directly (unbounded-safe):\n{out}");
    // No single value-guessed buffer (the old `char buf[n*50+lit]` form).
    assert!(!out.contains("[50]") && !out.contains("* 50"), "no guessed buffer size:\n{out}");
    // The numeric operand still gets a type-bounded buffer.
    assert!(out.contains("char ") && out.contains("sprintf(") && out.contains("[24]"), "int → type-bounded buffer:\n{out}");
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
    assert!(names.len() >= 4, "expected four range loops, found {}:\n{out}", names.len());
    let mut sorted = names.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), names.len(), "range loop control variables must be unique: {names:?}\n{out}");
    // And the user's bare `i` must not survive as a raw for-init (it was renamed).
    assert!(!out.contains("for (int i ="), "the reused Haxe `i` must be renamed:\n{out}");
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
    assert!(out.contains("return Color_::Green;"), "bare enum variant must be scoped:\n{out}");
    assert!(!out.contains("return Green;"), "unqualified variant must not leak:\n{out}");
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

class Scene {
  public function new() {}
  public function cap():Int { return MAX_CHARS; }        // native const → mucus::
  public function id():Int { return SCENE_ID; }          // same ns → bare
}

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
    assert!(out.contains("std::vector<int "), "array temp is a std::vector<int>:\n{out}");
    assert!(out.contains(".push_back(1)") && out.contains(".push_back(3)"), "literal elements pushed:\n{out}");
    let push_pos = out.find(".push_back(1)").unwrap_or_else(|| panic!("element push expected:\n{out}"));
    let for_pos = out.find("for (size_t").unwrap_or_else(|| panic!("index loop expected:\n{out}"));
    assert!(push_pos < for_pos, "the vector must be built before the loop:\n{out}");
    assert!(out.contains("int i = "), "loop binds element by value:\n{out}");
    assert!(out.contains(".size(); ++"), "index loop over .size():\n{out}");
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
    assert!(!stripped.contains("printf("), "no-traces must emit no printf:\n{stripped}");
    assert!(stripped.contains("((void)0);"), "no-traces lowers trace to a no-op:\n{stripped}");

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

    assert!(out.contains("#ifdef DREAMCAST"), "`#if FLAG` → `#ifdef FLAG`:\n{out}");
    assert!(out.contains("#else"), "`#else` preserved:\n{out}");
    assert!(out.contains("#endif"), "`#end` → `#endif`:\n{out}");
    assert!(out.contains("#include <dc/fmath.h>"), "stmt `@:include` → `#include`:\n{out}");
    assert!(
        out.contains("return fsqrtf(dx * dx + dy * dy);"),
        "`untyped` operand emitted verbatim:\n{out}"
    );
    assert!(!out.contains("untyped"), "the `untyped` keyword must not survive:\n{out}");
    // Preprocessor directives sit at column 0 so they are valid.
    assert!(out.contains("\n#ifdef DREAMCAST"), "directive at column 0:\n{out}");
    // The #else branch is transpiled normally.
    assert!(out.contains("sqrt("), "Math.sqrt → sqrt in the else branch:\n{out}");
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
    assert!(out.contains("std::string"), "interpolation builds a std::string accumulator:\n{out}");
    assert!(out.contains("+= name"), "the string operand is appended directly:\n{out}");
    assert!(!out.contains("sprintf"), "a string interpolation needs no fixed-size buffer:\n{out}");
    assert!(out.contains("$5"), "a `$` not before an identifier stays a literal:\n{out}");
}

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
    assert!(body.contains("tiles.push_back(new Tile(1));"), "direct field push should be inline:\n{body}");
    assert!(body.contains("row.push_back(new Tile(2));"), "local-container push should be inline:\n{body}");
    assert!(!body.contains("delete "), "the constructor must not delete a pushed `new`:\n{body}");
    // The destructor frees both containers (the owner of the heap objects).
    assert!(header.contains("delete this->tiles[") , "dtor should free the tiles vector:\n{header}");
    assert!(header.contains("delete this->rows["), "dtor should free the rows vector:\n{header}");
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
    assert!(header.contains("delete this->h;"), "destructor must free the owned field:\n{header}");
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
        body.contains("float Cross(const Vec& a, const Vec& b)"),
        "untyped arrow params take their type from the function annotation:\n{body}"
    );
    assert!(!body.contains("int a"), "params must not default to int:\n{body}");
    assert!(body.contains("return a.x * b.y - a.y * b.x;"), "body uses the typed params:\n{body}");
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
    assert!(owns.contains("delete o;"), "the scope frees the owner:\n{owns}");
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

    assert!(body.contains("delete t;"), "@delete forces a scope-close free of the local:\n{body}");
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

    assert!(body.contains(".resize("), "array index write must grow the vector first:\n{body}");
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

    assert!(header.contains("delete this->dep;"), "@owned scalar pointer → plain delete:\n{header}");
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
    assert!(!out.contains("new Cell(7)->"), "no field access directly on a new-expression:\n{out}");
    assert!(!out.contains("delete "), "the hoisted wrapper must not be freed:\n{out}");
}

#[test]
fn string_null_check_lowers_to_empty() {
    // A Haxe `String` is a value-typed `std::string` with no null, so a null
    // comparison cannot stay `s == NULL`. Optional `String` params default to `""`,
    // so "null" ≡ empty: `s == null` → `s.empty()`, `s != null` → `!s.empty()`.
    let src = "\
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
    assert!(out.contains("Widget* w = NULL;"), "class-typed null follows the ascription:\n{out}");
    assert!(out.contains("std::vector<int> xs"), "empty array literal follows the ascription:\n{out}");
}

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
    assert!(out.contains(">= 'a' && ") && out.contains("- 'a' + 'A'"), "toUpperCase ASCII map:\n{out}");
    assert!(out.contains(">= 'A' && ") && out.contains("- 'A' + 'a'"), "toLowerCase ASCII map:\n{out}");
    assert!(!out.contains("toupper") && !out.contains("tolower"), "no <cctype> dependency:\n{out}");
    // split builds a vector via find/substr (npos sentinel), no <algorithm>.
    assert!(out.contains("std::vector<std::string >"), "split returns a vector:\n{out}");
    assert!(out.contains(".find(") && out.contains(".substr(") && out.contains("std::string::npos"),
        "split tokenizes with find/substr:\n{out}");
    assert!(!out.contains("std::find"), "no <algorithm> dependency:\n{out}");
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
    assert!(out.contains("sprintf(") && out.contains("\"%d\""), "Std.string(int) via sprintf:\n{out}");
    assert!(out.contains("std::string("), "Std.string returns a std::string:\n{out}");
    // Std.string of a literal and a bool.
    assert!(out.contains("std::string(\"hi\")"), "Std.string(literal):\n{out}");
    assert!(out.contains("std::string(\"true\")") && out.contains("std::string(\"false\")"),
        "Std.string(bool) maps to \"true\"/\"false\":\n{out}");
    // parseInt (hex-aware) and parseFloat — a bare string literal is a const char*,
    // so no invalid `.c_str()` is appended.
    assert!(out.contains("(int)strtol(\"0x1F\", NULL, 0)"), "Std.parseInt → strtol base 0:\n{out}");
    assert!(out.contains("(float)atof(\"3.14\")"), "Std.parseFloat → atof:\n{out}");
    assert!(!out.contains("\".c_str()") && !out.contains("\"0x1F\".c_str()"),
        "no .c_str() on a string literal:\n{out}");
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
    assert!(out.contains("== 2)") && out.contains("= true; break;"), "contains scan:\n{out}");
    assert!(out.contains("int ") && out.contains("= -1;") && out.contains("== 3)"), "indexOf scan:\n{out}");
    assert!(out.contains(".erase(") && out.contains(".begin() +"), "remove erases by index:\n{out}");
    assert!(out.contains(".size() / 2"), "reverse swap loop:\n{out}");
    assert!(out.contains("std::vector<int>(xs)"), "copy via copy-constructor:\n{out}");
    assert!(out.contains("sprintf(") && out.contains("\"%d\""), "numeric join via sprintf:\n{out}");
    // Map methods.
    assert!(out.contains("[\"k\"] = 7"), "Map.set → m[k]=v:\n{out}");
    assert!(out.contains(".erase(\"k\") != 0"), "Map.remove → erase != 0:\n{out}");
    assert!(out.contains("->first") && out.contains("std::vector<std::string >"), "Map.keys → vector of keys:\n{out}");
    // No <algorithm>-only names or runtime helpers leak in.
    assert!(!out.contains("std::find") && !out.contains("std::reverse"), "no <algorithm> dependency:\n{out}");
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

    assert!(out.contains("std::map<std::string, int>::iterator"), "map iterator type:\n{out}");
    assert!(out.contains(".begin();") && out.contains(".end();"), "iterator bounds:\n{out}");
    // `for (v in m)` binds the value only.
    assert!(out.contains("int v = ") && out.contains("->second;"), "value binding:\n{out}");
    // `for (k => val in m)` binds key and value.
    assert!(out.contains("std::string k = ") && out.contains("->first;"), "key binding:\n{out}");
    assert!(out.contains("int val = ") && out.contains("->second;"), "key/value value binding:\n{out}");
    // The map must NOT be iterated with the index path.
    assert!(!out.contains("m.size()") && !out.contains("m[_i"), "map must not use index iteration:\n{out}");
}

#[test]
fn for_over_a_non_container_is_an_error() {
    // Hatchet has no general Iterator/Iterable protocol; iterating something that is
    // not a range, Array, or Map must fail loudly rather than emit invalid C++.
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
        errors.iter().any(|(_, e)| e.contains("cannot iterate") && e.contains("only ranges, Array, and Map")),
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
    assert!(out.contains("return a * b;"), "lambda body transpiled:\n{out}");
}
