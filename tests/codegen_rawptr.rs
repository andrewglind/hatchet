//! cpp pointer / RawPointer interop, fromStar/ofArray, and fixed-array fill diagnostics.
mod common;
use common::*;

#[test]
fn cpp_pointer_interop_types_resolve_in_validator() {
    // The validator (bypassed by `gen_one`) must accept the hxcpp pointer interop
    // types as resolved — otherwise real CLI runs fail with "unresolved type".
    use hatchet::sema::validate::unresolved_type_errors;
    let src = "\
class Probe {
  public function new() {}
  public function a(p:cpp.RawPointer<cpp.Float32>):Void {}
  public function b(p:cpp.Star<cpp.UInt8>):Void {}
  public function c(p:cpp.ConstStar<cpp.Void>):Void {}
  public function d(p:cpp.RawPointer<cpp.Void>):Void {}
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_ptrres_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Probe.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("Probe"))
        .unwrap();
    let errs = unresolved_type_errors(&prog, idx);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        errs.is_empty(),
        "cpp pointer interop types must resolve, got: {errs:?}"
    );
}

#[test]
fn cpp_raw_pointer_maps_to_c_pointer_and_indexes() {
    // hxcpp's `cpp.RawPointer<T>` (and `cpp.Star`/`cpp.ConstStar`) lower to a C
    // pointer `T*`, carrying the element type so indexing reads through it.
    let src = "\
class Probe {
  public function new() {}
  public function sum(data:cpp.RawPointer<cpp.Float32>):Float {
    return data[0] + data[1];
  }
}
";
    let out = gen_one(src, "Probe");
    assert!(
        out.contains("float* data"),
        "cpp.RawPointer<cpp.Float32> param → `float*`:\n{out}"
    );
    assert!(
        out.contains("return data[0] + data[1];"),
        "a raw pointer indexes directly:\n{out}"
    );
}

#[test]
fn dynamic_and_void_star_via_from_star() {
    // A `Dynamic` value (an opaque `void*`) flowing into a `cpp.RawPointer<cpp.Void>`
    // (also `void*`): `cpp.Pointer.fromStar(x).raw` of an already-pointer is identity.
    let src = "\
class Holder {
  public function new() {}
  public function wrap(payload:Dynamic):Void {
    var p:cpp.RawPointer<cpp.Void> = cpp.Pointer.fromStar(payload).raw;
  }
}
";
    let out = gen_one(src, "Holder");
    assert!(
        out.contains("void* payload"),
        "a `Dynamic` parameter erases to `void*`:\n{out}"
    );
    assert!(
        out.contains("void* p = payload;"),
        "`fromStar(payload).raw` of an already-pointer is the value itself:\n{out}"
    );
    assert!(
        !out.contains(".raw") && !out.contains("fromStar"),
        "the `cpp.Pointer.fromStar(...).raw` idiom must not survive into C++:\n{out}"
    );
}

#[test]
fn from_star_of_value_takes_address() {
    // `cpp.Pointer.fromStar(X).raw` where `X` is a value lvalue → `&(X)`.
    let src = "\
typedef Pt = { x:Int, y:Int }
class Holder {
  public function new() {}
  public function addr(p:Pt):cpp.RawPointer<cpp.Void> {
    return cpp.Pointer.fromStar(p).raw;
  }
}
";
    let out = gen_one(src, "Holder");
    assert!(
        out.contains("return &(p);"),
        "fromStar of a value lvalue takes its address:\n{out}"
    );
}

#[test]
fn of_array_raw_into_native_fixed_array_warns() {
    // `cpp.Pointer.ofArray(..).raw` does NOT fill a native fixed C-array field
    // (`uint8_t table[N]`, bound as `cpp.RawPointer<cpp.UInt8>`): a C array is
    // non-assignable and a pointer into the temporary vector would dangle. Hatchet no
    // longer copies behind the developer's back — it warns and points at the portable
    // explicit element loop. (The unsupported idiom is not hxcpp-portable either: hxcpp
    // would emit an illegal `table = ptr` array assignment.)
    let src = "\
@:native(\"ext::Ramp\")
typedef Ramp = {
  color:cpp.UInt32,
  table:cpp.RawPointer<cpp.UInt8>
}

class Builder {
  public function new() {}
  public function make():Ramp {
    return {
      color: 255,
      table: cpp.Pointer.ofArray(([0, 12, 24]:Array<cpp.UInt8>)).raw
    };
  }
}
";
    let (out, warnings) = gen_one_diag(src, "Builder");
    assert!(
        warnings
            .iter()
            .any(|(_, w)| w.contains("fixed C-array") && w.contains("explicit element loop")),
        "filling a fixed C-array field via ofArray(...).raw must warn: {warnings:?}"
    );
    // The magic copy loop is gone — no synthesised `for`-copy into `.table[...]`.
    assert!(
        !out.contains(".table[") || !out.contains("for ("),
        "the copy lowering must no longer synthesise a fill loop:\n{out}"
    );
}

#[test]
fn returning_of_array_raw_warns_about_dangling() {
    // A helper that wraps a Haxe array and returns `cpp.Pointer.ofArray(...).raw`
    // hands back a pointer into an array that dies with the call — it dangles, and
    // bypasses the fixed-array copy the inline idiom gets. Returning it must warn.
    let src = "\
class Lib {
  public function new() {}
  public static function wrap(a:Array<cpp.UInt8>):cpp.RawPointer<cpp.UInt8> {
    return cpp.Pointer.ofArray(a).raw;
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_dangl_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Lib.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("Lib"))
        .unwrap();
    let (_, warnings, _) = generate_source_diagnostics(&prog, idx, 1, false).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        warnings
            .iter()
            .any(|(_, w)| w.contains("ofArray") && w.contains("dangles")),
        "returning ofArray(...).raw must warn about the dangling pointer: {warnings:?}"
    );
}

#[test]
fn inline_of_array_raw_field_fill_warns() {
    // Filling a fixed C-array field inline (`{ table: ofArray([...]).raw }`) is no longer
    // a supported copy — it warns and points at the optional-field + explicit-loop idiom.
    let src = "\
@:native(\"ext::Ramp\")
typedef Ramp = { table:cpp.RawPointer<cpp.UInt8> }

class Lib {
  public function new() {}
  public function make():Ramp {
    return { table: cpp.Pointer.ofArray([1, 2, 3]).raw };
  }
}
";
    let (_, warnings) = gen_one_diag(src, "Lib");
    assert!(
        warnings
            .iter()
            .any(|(_, w)| w.contains("fixed C-array") && w.contains("optional")),
        "inline ofArray(...).raw field-fill must warn and point at the optional-field idiom: {warnings:?}"
    );
}

#[test]
fn array_literal_cast_ascription_pins_element_type() {
    // `([…] : Array<cpp.UInt8>)` must build the literal with the ascribed element type
    // (`uint8_t`), not infer `int` from its members and then clash on assignment.
    let src = "\
class A {
  public function new() {}
  public function f():Void {
    var b = ([4, 5, 6] : Array<cpp.UInt8>);
  }
}
";
    let out = gen_one(src, "A");
    assert!(
        out.contains("std::vector<uint8_t>") && !out.contains("std::vector<int"),
        "the cast ascription pins the element type to uint8_t:\n{out}"
    );
}

#[test]
fn of_array_raw_into_bare_local_pointer_warns_not_copies() {
    // A bare `cpp.RawPointer<T>` local has no backing storage, so the fixed-array copy
    // must NOT fire (it would write through an uninitialised pointer). It stays a
    // pointer assignment, and — since a fresh array can only mean "fill fixed storage"
    // — Hatchet warns and points at inlining.
    let src = "\
class Lib {
  public function new() {}
  public function f():Void {
    var p:cpp.RawPointer<cpp.UInt8>;
    p = cpp.Pointer.ofArray([1, 2, 3]).raw;
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_barelocal_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Lib.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("Lib"))
        .unwrap();
    let (out, warnings, _) = generate_source_diagnostics(&prog, idx, 1, false).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        warnings
            .iter()
            .any(|(_, w)| w.contains("fixed C-array") && w.contains("explicit element loop")),
        "filling a bare RawPointer local from an array literal must warn: {warnings:?}"
    );
    // No element-copy is synthesised through the bare (uninitialised) pointer.
    assert!(
        !out.contains("p[0] ="),
        "no element-copy through the bare (uninitialised) pointer:\n{out}"
    );
}

#[test]
fn comprehension_still_materialises() {
    // An ordinary array comprehension builds its own vector (`push_back`). This held a
    // carve-out for the removed `ofArray(...).raw` fusion; with the copy lowering gone,
    // there is no longer any fused form to scope it against.
    let src = "\
class Builder {
  var src:Array<Int>;
  public function new() {}
  public function doubled():Array<Int> {
    return [for (e in this.src) e * 2];
  }
}
";
    let out = gen_one(src, "Builder");
    assert!(
        out.contains("push_back"),
        "a comprehension materialises a vector:\n{out}"
    );
}

#[test]
fn of_array_raw_assignment_statement_warns() {
    // The warning also fires for a plain assignment statement into a fixed-array field,
    // not just an object-literal field initialiser — the copy lowering is gone there too.
    let src = "\
@:native(\"ext::Ramp\")
typedef Ramp = {
  table:cpp.RawPointer<cpp.UInt8>
}

class Builder {
  var r:Ramp;
  public function new() {}
  public function fill():Void {
    this.r.table = cpp.Pointer.ofArray(([1, 2, 3, 4]:Array<cpp.UInt8>)).raw;
  }
}
";
    let (out, warnings) = gen_one_diag(src, "Builder");
    assert!(
        warnings
            .iter()
            .any(|(_, w)| w.contains("fixed C-array") && w.contains("explicit element loop")),
        "assignment-form ofArray fixed-array fill must warn: {warnings:?}"
    );
    // No synthesised copy loop into the array storage.
    assert!(
        !out.contains("= {1, 2, 3, 4}"),
        "the magic copy must no longer materialise a local C array:\n{out}"
    );
}

#[test]
fn explicit_fill_loop_is_clean() {
    // The supported, hxcpp-portable idiom: make the fixed-array field optional so the
    // struct literal can omit it, then populate it with an explicit element loop. This
    // must emit plain indexed writes and raise no fixed-C-array warning.
    let src = "\
@:native(\"ext::Ramp\")
typedef Ramp = {
  color:cpp.UInt32,
  ?table:cpp.RawPointer<cpp.UInt8>
}

class Builder {
  public function new() {}
  public function make():Ramp {
    var r:Ramp = { color: 255 };
    var data:Array<cpp.UInt8> = [0, 12, 24];
    for (i in 0...data.length)
      r.table[i] = data[i];
    return r;
  }
}
";
    let (out, warnings) = gen_one_diag(src, "Builder");
    assert!(
        !warnings.iter().any(|(_, w)| w.contains("fixed C-array")),
        "the explicit fill loop is the supported idiom and must not warn: {warnings:?}"
    );
    assert!(
        out.contains("r.table[") && out.contains("] = ") && out.contains("for ("),
        "the explicit loop lowers to plain indexed writes:\n{out}"
    );
}

#[test]
fn delete_on_value_local_warns() {
    // `@delete` only frees a heap pointer. On a value local (here `std::vector<uint8_t>`)
    // it is a silent no-op — the vector is freed automatically at scope close — so the
    // tag must warn rather than quietly do nothing.
    let src = "\
class Builder {
  public function new() {}
  public function f():Void {
    @delete var data:Array<cpp.UInt8> = [0, 12, 24];
    var sum = 0;
    for (i in 0...data.length) sum += data[i];
  }
}
";
    let (_, warnings) = gen_one_diag(src, "Builder");
    assert!(
        warnings
            .iter()
            .any(|(_, w)| w.contains("@delete") && w.contains("no effect") && w.contains("data")),
        "@delete on a value local must warn that it has no effect: {warnings:?}"
    );
}

#[test]
fn mutating_value_struct_param_is_an_error() {
    // A value-struct (here `@:native`) parameter is passed `const T&`, so writing
    // through it cannot compile — Hatchet must reject it rather than emit broken C++.
    let src = "\
@:native(\"ext::Slot\")
typedef Slot = { data:cpp.RawPointer<cpp.Void> }

class Sink {
  public function new() {}
  public function store(s:Slot, payload:Dynamic):Void {
    s.data = cpp.Pointer.fromStar(payload).raw;
  }
}
";
    let dir = std::env::temp_dir().join(format!("hatchet_constparam_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Sink.hx"), src).unwrap();
    let prog = Program::from_src_dir(&dir).expect("build program");
    let idx = prog
        .modules
        .iter()
        .position(|m| m.path.file_stem().and_then(|s| s.to_str()) == Some("Sink"))
        .unwrap();
    let (_, _warnings, errors) = generate_source_diagnostics(&prog, idx, 1, false).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        errors
            .iter()
            .any(|(_, e)| e.contains("cannot assign to `s`") && e.contains("const&")),
        "mutating a value-struct param must error, got: {errors:?}"
    );
}
