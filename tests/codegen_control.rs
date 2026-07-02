//! Switch/if-expression, enum-abstract, numeric lowering, try/catch, and property accessors.
mod common;
use common::*;

#[test]
fn int_enum_abstract_members_qualify_in_bodies() {
    // An `Int`-backed `enum abstract` member is a C++ enumerator: a bare member in
    // a `switch` case or expression position qualifies to `X_::Member`, exactly
    // like a plain enum constant — the enum machinery is reused wholesale.
    let out = gen_one(
        "enum abstract Dir(Int) { var North; var South; }\nclass Nav {\n  public function new() {}\n  public function code(d:Dir):Int {\n    switch (d) {\n      case North: return 0;\n      case South: return 1;\n    }\n    return -1;\n  }\n  public function home():Dir { return South; }\n}\n",
        "Nav",
    );
    assert!(
        out.contains("case Dir_::North:"),
        "switch case qualifies the member:\n{out}"
    );
    assert!(
        out.contains("return Dir_::South;"),
        "bare member in return qualifies:\n{out}"
    );
}

#[test]
fn string_subject_switch_lowers_to_an_if_else_chain() {
    // A `switch` on a `String` cannot use a C++ `switch` (case labels must be
    // integral), so it lowers to an `if`/`else if`/`else` chain: the subject is
    // hoisted into one `std::string`, multi-pattern cases become OR-ed equality
    // tests, and `default` becomes the trailing `else`.
    let out = gen_one(
        "class Sw {\n  public function new() {}\n  public function f(s:String):Int {\n    switch (s) {\n      case \"one\": return 1;\n      case \"two\", \"deux\": return 2;\n      default: return -1;\n    }\n  }\n}\n",
        "Sw",
    );
    assert!(
        !out.contains("switch ("),
        "no C++ switch on a string:\n{out}"
    );
    assert!(
        out.contains("std::string") && out.contains(" = s;"),
        "subject hoisted once:\n{out}"
    );
    assert!(out.contains("== \"one\""), "first pattern compared:\n{out}");
    assert!(
        out.contains("== \"two\" || ") && out.contains("== \"deux\""),
        "multi-pattern case is OR-ed:\n{out}"
    );
    assert!(
        out.contains("} else {"),
        "default becomes the trailing else:\n{out}"
    );
}

#[test]
fn switch_expression_lowers_to_a_hoisted_temp() {
    // A `switch` in value position desugars to a temporary declared before a
    // statement `switch`, whose arms assign their trailing value to the temp; the
    // expression then evaluates to that temp.
    let out = gen_one(
        "class E {\n  public function new() {}\n  public function pick(n:Int):String {\n    var s = switch (n) {\n      case 0: \"zero\";\n      default: \"other\";\n    }\n    return s;\n  }\n}\n",
        "E",
    );
    // A hoisted std::string temp, assigned inside the switch, then bound to `s`.
    assert!(
        out.contains("std::string _swx"),
        "result temp is declared:\n{out}"
    );
    assert!(
        out.contains("switch ("),
        "desugars to a statement switch:\n{out}"
    );
    assert!(
        out.contains("= \"zero\";") && out.contains("= \"other\";"),
        "arms assign the temp:\n{out}"
    );
    assert!(
        out.contains("std::string s = _swx"),
        "expression evaluates to the temp:\n{out}"
    );
}

#[test]
fn array_filter_lowers_to_a_predicate_loop() {
    // `xs.filter(p)` → a fresh vector of the kept elements (same element type),
    // the predicate lambda inlined with its parameter bound to each element.
    let out = gen_one(
        "class Flt {\n  public function new() {}\n  public function f(xs:Array<Int>):Array<Int> {\n    return xs.filter(n -> n > 2);\n  }\n}\n",
        "Flt",
    );
    assert!(
        out.contains("std::vector<int >"),
        "result is a vector of the element type:\n{out}"
    );
    assert!(
        out.contains("int n = "),
        "predicate param bound to the element:\n{out}"
    );
    assert!(
        out.contains("if (n > 2)") && out.contains(".push_back(n)"),
        "predicate guards the push:\n{out}"
    );
}

#[test]
fn array_sort_lowers_to_an_inline_insertion_sort() {
    // `xs.sort(cmp)` → an in-place insertion sort with no `<algorithm>` dependency;
    // the comparator lambda's two params are bound to the compared elements.
    let out = gen_one(
        "class Srt {\n  public function new() {}\n  public function s():Void {\n    var xs = [3, 1, 2];\n    xs.sort((a, b) -> a - b);\n  }\n}\n",
        "Srt",
    );
    assert!(
        !out.contains("std::sort") && !out.contains("<algorithm>"),
        "no <algorithm>:\n{out}"
    );
    assert!(
        out.contains("while (") && out.contains("break;"),
        "insertion-sort shift loop:\n{out}"
    );
    assert!(
        out.contains("int _cmp") && out.contains("= a - b;"),
        "comparator inlined over a/b:\n{out}"
    );
}

#[test]
fn try_catch_lowers_to_cpp_exception_handling() {
    // `try { … } catch (e:T) { … }` → a C++ try/catch. A thrown String is coerced
    // to `std::string` (so a `catch (e:String)` matches it), a typed catch maps the
    // exception type via the parameter rules, and an untyped/`Dynamic` catch becomes
    // the non-binding `catch (...)`.
    let out = gen_one(
        "class T {\n  public function new() {}\n  public function f(b:Bool):String {\n    try {\n      if (b) throw \"x\";\n      return \"ok\";\n    } catch (e:String) {\n      return e;\n    } catch (e:Dynamic) {\n      return \"any\";\n    }\n  }\n}\n",
        "Tc",
    );
    assert!(out.contains("try {"), "emits a C++ try block:\n{out}");
    assert!(
        out.contains("throw std::string(\"x\")"),
        "String throw is coerced:\n{out}"
    );
    assert!(
        out.contains("catch (const std::string& e)"),
        "typed catch maps the exception type:\n{out}"
    );
    assert!(
        out.contains("catch (...)"),
        "Dynamic catch is the non-binding catch-all:\n{out}"
    );
}

#[test]
fn untyped_catch_using_its_value_is_an_error() {
    // An untyped/`Dynamic` catch lowers to the non-binding C++ `catch (...)`, which
    // cannot bind the exception. Referencing the caught name in the body is a hard
    // error rather than silently emitting an undeclared identifier.
    let src = "class Tcc {\n  public function new() {}\n  public function run():Void {\n    try {} catch (e) { trace(e); }\n  }\n}\n";
    let dir = std::env::temp_dir().join(format!("hatchet_catchval_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Tcc.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("Tcc"))
        .unwrap();
    let (_, _, errors) = generate_source_diagnostics(&prog, idx, 1, false).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        errors
            .iter()
            .any(|(_, e)| e.contains("untyped") && e.contains("catch")),
        "expected an error for using an untyped catch's value, got: {errors:?}"
    );
}

#[test]
fn untyped_catch_ignoring_its_value_is_a_plain_catch_all() {
    // The same untyped catch that does NOT reference the value is fine: it is the
    // non-binding `catch (...)`, with no error.
    let out = gen_one(
        "class Tci {\n  public function new() {}\n  public function run():Void {\n    try {} catch (e) { trace(\"oops\"); }\n  }\n}\n",
        "Tci",
    );
    assert!(
        out.contains("catch (...)"),
        "untyped catch ignoring the value → catch(...):\n{out}"
    );
}

#[test]
fn unsigned_shift_lowers_through_an_unsigned_cast() {
    // Haxe `>>>` (and `>>>=`) have no C++ spelling — both shift through
    // `unsigned int` and come back to `int`, matching Haxe's 32-bit semantics.
    let src = "\
class Bits {
  public function new() {}
  public function run(a:Int, n:Int):Int {
    var x:Int = a >>> n;
    x >>>= 1;
    return x >> 1;
  }
}
";
    let out = gen_one(src, "Bits");
    assert!(
        out.contains("((int)((unsigned int)(a) >> n))"),
        "`>>>` must shift through unsigned int:\n{out}"
    );
    assert!(
        out.contains("x = (int)((unsigned int)(x) >> 1)"),
        "`>>>=` must expand through unsigned int:\n{out}"
    );
    assert!(
        out.contains("x >> 1"),
        "plain `>>` stays a signed shift:\n{out}"
    );
}

#[test]
fn int_division_yields_float() {
    // Haxe `/` always yields Float, even for Int operands; C++ `/` would
    // truncate. Known-integer operands force a double division, and
    // `Std.int(a / b)` still truncates back, matching Haxe.
    let src = "\
class Ratio {
  public function new() {}
  public function half(a:Int, b:Int):Float {
    return a / b;
  }
  public function idiv(a:Int, b:Int):Int {
    return Std.int(a / b);
  }
  public function fdiv(a:Float, b:Float):Float {
    return a / b;
  }
}
";
    let out = gen_one(src, "Ratio");
    assert!(
        out.contains("((double)(a) / b)"),
        "Int / Int must divide as double:\n{out}"
    );
    assert!(
        out.contains("(int)(((double)(a) / b))"),
        "Std.int(a / b) must truncate the double division:\n{out}"
    );
    assert!(
        out.contains("return a / b;"),
        "Float / Float stays a plain division:\n{out}"
    );
}

#[test]
fn float_modulo_lowers_to_fmod() {
    // Haxe `%` works on Floats; C++ `%` is integer-only, so a float operand
    // lowers to `fmod` (C89 <math.h>, portable to VC6). Int % Int stays `%`.
    let src = "\
class Wrap {
  public function new() {}
  public function angle(a:Float, b:Float):Float {
    var r:Float = a % b;
    r %= 1.5;
    return r;
  }
  public function parity(a:Int, b:Int):Int {
    return a % b;
  }
}
";
    let out = gen_one(src, "Wrap");
    assert!(
        out.contains("fmod(a, b)"),
        "Float % Float must lower to fmod:\n{out}"
    );
    assert!(
        out.contains("r = fmod(r, 1.5)"),
        "`%=` with a float target must lower to fmod:\n{out}"
    );
    assert!(
        out.contains("return a % b;"),
        "Int % Int stays the plain operator:\n{out}"
    );
}

#[test]
fn switch_wildcard_and_or_patterns_lower() {
    // `case _:` is Haxe's wildcard — it lowers to C++ `default:` (never a literal
    // `case _:` label). In pattern position `|` is the or-pattern (patterns are
    // not evaluated), so `case 1 | 2:` yields two case labels, like `case 1, 2:`.
    let src = "\
class SwWild {
  public function new() {}
  public function describe(n:Int):String {
    switch (n) {
      case 0: return \"zero\";
      case 1 | 2: return \"few\";
      case _: return \"many\";
    }
  }
}
";
    let out = gen_one(src, "SwWild");
    assert!(
        !out.contains("case _"),
        "`case _:` must not leak as a C++ label:\n{out}"
    );
    assert!(
        out.contains("default:"),
        "`case _:` lowers to default:\n{out}"
    );
    assert!(
        out.contains("case 1:") && out.contains("case 2:") && !out.contains("case 1 | 2"),
        "`case 1 | 2:` is the or-pattern, two labels — not a bitwise OR:\n{out}"
    );
}

#[test]
fn math_nan_is_a_portable_double_nan() {
    let src = "\
class N {
  public function new() {}
  public function nan():Float { return Math.NaN; }
}
";
    let out = gen_one(src, "N");
    assert!(
        out.contains("(HUGE_VAL - HUGE_VAL)"),
        "Math.NaN → inf - inf (portable C++98 NaN):\n{out}"
    );
}

#[test]
fn custom_getter_routing_breaks_recursion_and_bypasses_writes() {
    // Reads of a `(get, null)` property route through `get_x()` — except inside
    // `get_x` itself (else infinite recursion) — and assignment targets are
    // direct physical stores (`null` write access within the class).
    let src = "\
class Counter {
  public var count(get, null):Int;
  public function new() { count = 0; }
  function get_count():Int {
    return count;
  }
  public function bump():Void {
    count = count + 1;
  }
  public function peek(other:Counter):Int {
    return other.count;
  }
}
";
    let out = gen_one(src, "Counter");
    // inside get_count: direct backing-field read, no self-call
    assert!(
        out.contains("\treturn this->count;"),
        "get_count reads its backing field directly:\n{out}"
    );
    // bump: write target direct, read side routed through the getter
    assert!(
        out.contains("this->count = this->get_count() + 1;"),
        "write is a direct store, read routes through get_count():\n{out}"
    );
    // external read routes through the getter
    assert!(
        out.contains("other->get_count()"),
        "external read routes through get_count():\n{out}"
    );
}

#[test]
fn custom_setter_routes_all_writes() {
    // A `(default, set)` property with a user-written `set_x`: real Haxe
    // semantics — ctor writes, plain writes, compound writes and `++` all route
    // through `set_x`; inside `set_x` itself the store is direct.
    let src = "\
class Gauge {
  public var level(default, set):Int;
  public function new() { level = 50; }
  function set_level(v:Int):Int {
    level = v < 0 ? 0 : (v > 100 ? 100 : v);
    return level;
  }
  public function adjust():Void {
    level = 250;
    level += 10;
    level++;
  }
  public function tune(other:Gauge):Void {
    other.level = 1;
    other.level += 2;
  }
}
";
    let out = gen_one(src, "Gauge");
    assert!(
        out.contains("this->set_level(50)"),
        "ctor write routes:\n{out}"
    );
    assert!(
        out.contains("this->set_level(250)"),
        "internal write routes:\n{out}"
    );
    assert!(
        out.contains("this->set_level(this->level + 10)"),
        "compound write desugars through the setter:\n{out}"
    );
    assert!(
        out.contains("this->set_level(this->level + 1)"),
        "`++` desugars through the setter:\n{out}"
    );
    assert!(
        out.contains("this->level = v < 0 ? 0 : (v > 100 ? 100 : v);"),
        "inside set_level the store is direct:\n{out}"
    );
    assert!(
        out.contains("other->set_level(1)"),
        "external write routes:\n{out}"
    );
    assert!(
        out.contains("other->set_level(other->GetLevel() + 2)"),
        "external compound reads via the getter, writes via the setter:\n{out}"
    );
    assert!(
        !out.contains("SetLevel"),
        "no trivial setter generated when set_level exists:\n{out}"
    );
}

#[test]
fn custom_setter_fields_follow_the_conservative_ownership_bias() {
    // A Haxe setter returns the assigned value (`return buf;`), which the escape
    // analysis reads as the field being handed back out of the object — so a
    // custom-setter field leans *borrowed*: never freed on reassignment, never
    // NULL-deleted behind the caller's back (leak over double-free, the
    // documented bias; `@owned` opts the destructor in). What must hold: all
    // writes route through `set_buf`, and no `delete` is emitted anywhere a
    // routed caller could double-free.
    let src = "\
class Thing {
  public var id:Int;
  public function new(id:Int) { this.id = id; }
}

class Pool {
  public var buf(default, set):Thing;
  public function new() { buf = new Thing(0); }
  function set_buf(v:Thing):Thing {
    buf = new Thing(v.id + 1);
    return buf;
  }
  public function bump():Void {
    buf = new Thing(5);
  }
}
";
    let out = gen_one(src, "Pool");
    assert!(
        out.contains("this->set_buf(new Thing(0))"),
        "ctor write routes:\n{out}"
    );
    assert!(
        out.contains("this->set_buf(new Thing(5))"),
        "bump write routes:\n{out}"
    );
    assert!(
        out.contains("this->buf = new Thing(v->id + 1);"),
        "inside set_buf the store is direct:\n{out}"
    );
    assert!(
        !out.contains("delete this->buf"),
        "no caller-side delete may race the setter funnel:\n{out}"
    );
}
#[test]
fn float32_lowers_to_c_float_and_setter_return_type_is_inferred() {
    // `cpp.Float32` / `Single` target genuine C++ `float` (Haxe `Float` is
    // `double`), and a custom accessor whose signature omits its return type
    // (`function set_x(x:Float) { return this.x = x; }`) returns the property's
    // type — defaulting to void would emit a value `return` from a void function.
    let src = "\
class Particle {
  public var x(default, set):Float;
  public var vx:cpp.Float32;
  public var mass:Single;
  public function new() {
    x = 0.0;
    vx = 1.5;
    mass = 1.0;
  }
  public function set_x(x:Float) {
    return this.x = x;
  }
  public function step(dt:cpp.Float32):Float {
    x += vx * dt;
    return x;
  }
}
";
    let out = gen_one(src, "Particle");
    assert!(
        out.contains("double Particle::set_x(double x)"),
        "omitted accessor return type is the property's type, not void:\n{out}"
    );
    assert!(
        out.contains("return this->x = x;"),
        "the `return this.x = x` setter shape lowers as a value return:\n{out}"
    );
    assert!(
        out.contains("double Particle::step(float dt)"),
        "cpp.Float32 parameter lowers to C++ float:\n{out}"
    );
    assert!(
        out.contains("this->set_x(this->x + this->vx * dt)"),
        "compound write still routes through the setter:\n{out}"
    );
}

#[test]
fn mutating_a_container_parameter_is_warned() {
    // Haxe Arrays/Maps are shared by reference — mutating one inside a function
    // is visible to the caller. Hatchet passes containers by value (`const&`),
    // so a mutation through a parameter is linted at the Haxe line (ahead of
    // the C++ const error). Reads, copies, fields, locals, and shadowing
    // locals must stay silent.
    let src = "\
class MutWarn {
  var roster:Array<Int>;
  public function new() { roster = []; }
  public function fill(items:Array<Int>, tags:Map<String,Int>):Void {
    items.push(42);
    items[0] = 7;
    tags.set(\"a\", 1);
    tags[\"b\"] = 2;
  }
  public function fine(items:Array<Int>):Int {
    var copy = items.copy();
    copy.push(42);
    var n = items.indexOf(3);
    roster.push(n);
    for (i in items) n += i;
    return n + copy.length;
  }
  public function shadowed(items:Array<Int>):Void {
    var items = [9];
    items.push(1);
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_mutwarn_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("MutWarn.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("MutWarn"))
        .unwrap();
    let (_, warnings, _) = generate_source_diagnostics(&prog, idx, 1, false).unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    let expect = [
        (5, "`push` mutates `items`, an Array parameter"),
        (6, "`[i] = …` mutates `items`, an Array parameter"),
        (7, "`set` mutates `tags`, a Map parameter"),
        (8, "`[k] = …` mutates `tags`, a Map parameter"),
    ];
    for (line, needle) in expect {
        assert!(
            warnings
                .iter()
                .any(|(l, w)| *l == line && w.contains(needle)),
            "expected `{needle}` on line {line}, got: {warnings:?}"
        );
    }
    assert_eq!(
        warnings.len(),
        expect.len(),
        "reads/copies/fields/locals/shadowing must not warn: {warnings:?}"
    );
}

#[test]
fn string_escapes_translate_to_cpp_not_double_escaped() {
    // The lexer keeps escape sequences uninterpreted (Haxe `\n` is stored as
    // backslash + 'n'); codegen must re-emit them as the matching C++ escape,
    // not double the backslash (which made `\n` a literal backslash-n and
    // `"\"".code` the backslash's code instead of the quote's). Octal-normalises
    // numeric byte escapes so they can never absorb a following digit.
    let src = "\
class Esc {
  public function new() {}
  public function nl():String { return \"a\\nb\"; }
  public function quote():Int { return \"\\\"\".code; }
  public function tab():Int { return \"\\t\".code; }
  public function backslash():Int { return \"\\\\\".code; }
  public function hex():String { return \"\\x41\\x42\"; }
}
";
    let out = gen_one(src, "Esc");
    assert!(
        out.contains("return \"a\\nb\";"),
        "`\\n` stays a single C++ escape:\n{out}"
    );
    assert!(
        out.contains("((int)(unsigned char)(\"\\\"\")[0])"),
        "`\"\\\"\".code` compares against the quote (34), not the backslash:\n{out}"
    );
    assert!(
        out.contains("((int)(unsigned char)(\"\\t\")[0])"),
        "`\\t.code` is the tab escape:\n{out}"
    );
    assert!(
        out.contains("((int)(unsigned char)(\"\\\\\")[0])"),
        "`\\\\.code` is a single backslash:\n{out}"
    );
    // \x41\x42 → octal \101\102 ("AB"), byte-exact and non-greedy.
    assert!(
        out.contains("\\101\\102"),
        "hex byte escapes normalise to octal:\n{out}"
    );
}

#[test]
fn sink_parameter_transfers_ownership_no_double_free() {
    // `@sink` on a parameter: a `new` passed there is emitted inline (the
    // callee consumes it), and an owned local handed there transfers out — so
    // the caller never frees what the callee retained (the use-after-free that
    // an un-annotated retaining method would otherwise cause).
    let src = "\
class Node {
  public var kids:Array<Node>;
  public function new() { kids = []; }
  public function adopt(@sink child:Node):Void {
    kids.push(child);
  }
}
class Tree {
  public var root:Node;
  public function new() { root = new Node(); }
  public function grow():Void {
    root.adopt(new Node());          // new at @sink position -> inline, no free
    var extra = new Node();
    root.adopt(extra);               // owned local -> ownership transferred
  }
}
";
    let out = gen_one(src, "Tree");
    assert!(
        out.contains("root->adopt(new Node())"),
        "a `new` at a `@sink` position is emitted inline (no hoist/free):\n{out}"
    );
    assert!(
        !out.contains("delete _v") && !out.contains("delete extra"),
        "neither the inline new nor the transferred local is freed by the caller:\n{out}"
    );
}

#[test]
fn sink_on_value_parameter_is_warned() {
    // `@sink` is meaningless on a by-value parameter — flag it as a no-op.
    let src = "\
class W {
  public function new() {}
  public function take(@sink n:Int):Void {}
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_sinkwarn_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("W.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("W"))
        .unwrap();
    let (_, warnings, _) = generate_source_diagnostics(&prog, idx, 1, false).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        warnings
            .iter()
            .any(|(_, w)| w.contains("`@sink` on parameter `n` has no effect")),
        "expected a no-op `@sink` warning, got: {warnings:?}"
    );
}

#[test]
fn abstract_value_type_nests_as_field_and_vector() {
    // An `abstract Name(U)` is a value type that — unlike `@:stackOnly` — nests
    // freely: `new` is value construction (no heap), a field and an `Array<T>` are
    // by value, member access is `.`, and nothing is ever freed (no ownership).
    let src = "\
typedef Vec2Data = { var x:Float; var y:Float; }
abstract Vec2(Vec2Data) {
  public function new(x:Float, y:Float) { this = { x: x, y: y }; }
  public function lenSq():Float { return this.x * this.x + this.y * this.y; }
}
class Use {
  public var here:Vec2;
  public function new() { here = new Vec2(0.0, 0.0); }
  public function run():Float {
    var v = new Vec2(3.0, 4.0);
    var pts:Array<Vec2> = [];
    pts.push(new Vec2(1.0, 2.0));
    return v.lenSq() + pts[0].lenSq();
  }
}
";
    let head = {
        let dir = std::env::temp_dir().join(format!("hatchet_so_h_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Use.hx"), src).unwrap();
        let prog = Program::from_src_dir(&dir).expect("build program");
        let idx = prog
            .modules
            .iter()
            .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("Use"))
            .unwrap();
        let h = hatchet::codegen::generate_header(&prog, idx).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        h
    };
    // value field, and a non-virtual destructor (no vtable → flat value layout)
    assert!(
        head.contains("Vec2 here;"),
        "value field is by value, not a pointer:\n{head}"
    );
    assert!(
        head.contains("\t~Vec2() {}"),
        "value class destructor is non-virtual:\n{head}"
    );
    assert!(
        !head.contains("virtual ~Vec2"),
        "no virtual destructor on a value class:\n{head}"
    );

    let out = gen_one(src, "Use");
    assert!(
        out.contains("Vec2 v = Vec2(3.0, 4.0)"),
        "`new` is value construction, not heap:\n{out}"
    );
    assert!(
        out.contains("std::vector<Vec2>"),
        "Array<Vec2> is a value vector:\n{out}"
    );
    assert!(
        out.contains("pts.push_back(Vec2(1.0, 2.0))"),
        "pushed value, no heap:\n{out}"
    );
    assert!(
        out.contains("this->here = Vec2(0.0, 0.0)"),
        "value field init, no `new`:\n{out}"
    );
    assert!(
        !out.contains("new Vec2") && !out.contains("delete"),
        "no heap or frees for a value class:\n{out}"
    );
}

#[test]
fn switch_case_on_final_constants_is_supported() {
    // A `case` whose pattern is a `final` constant (not a literal or enum member)
    // is a constant pattern — it lowers to the constant as a C++ case label, not
    // a Haxe capture variable. Regression guard for the switch-pattern validator.
    let src = "\
final MENU_SCENE_ID:Int = 0;
final POINTS_SCENE_ID:Int = 1;
class Factory {
  public function new() {}
  public function make(sceneId:Int):Int {
    switch sceneId {
      case MENU_SCENE_ID: return 10;
      case POINTS_SCENE_ID: return 20;
      default: return -1;
    }
  }
}
";
    let out = gen_one(src, "Factory");
    assert!(
        out.contains("case MENU_SCENE_ID:"),
        "final constant is a valid case label:\n{out}"
    );
    assert!(
        out.contains("case POINTS_SCENE_ID:"),
        "final constant is a valid case label:\n{out}"
    );
}

#[test]
fn value_position_switch_uses_the_expected_type_not_the_first_arm() {
    // `return switch …` whose arms are different subclasses (+ null) must hoist
    // the temporary as the *return type* (the common base), not the first arm's
    // subclass — otherwise assigning a sibling subclass to it is nonsense C++.
    let src = "\
class Scene { public function new() {} }
class MenuScene extends Scene { public function new() { super(); } }
class Points extends Scene { public function new() { super(); } }
class Factory {
  public function new() {}
  public function make(id:Int):Scene {
    return switch id {
      case 0: new MenuScene();
      case 1: new Points();
      default: null;
    }
  }
}
";
    let out = gen_one(src, "Factory");
    assert!(
        out.contains("Scene* _swx"),
        "temp is the base/return type, not the first arm:\n{out}"
    );
    assert!(
        !out.contains("AlienBeach* _swx"),
        "temp must not be typed as the first arm's subclass:\n{out}"
    );
    assert!(
        out.contains("_swx1 = new Points()"),
        "a sibling subclass assigns to the base temp:\n{out}"
    );
}

