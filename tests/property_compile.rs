//! Compile + run gate for property-accessor lowering: the access-control pairs
//! (`(default, null)`, `(null, default)`, `(null, null)`) and the computed
//! `(get, never)` / `(get, null)` custom getters. Verifies the C++ shape (no
//! `GetArea` for a custom getter; a public directly-writable field for
//! `(null, default)`; no backing field for a non-`@:isVar` `(get, never)`) and
//! the *behavior*: external reads route through `get_x()`, internal reads route
//! through it too — except inside the accessor itself — and writes to the
//! backing field stay direct.
//!
//! Skipped (passes vacuously) when no C++ compiler is available.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Locate a C++98 compiler: `$HATCHET_GXX`, else `g++` on `PATH`, else the MSYS2
/// mingw32 default. Returns `None` if none can be run.
fn find_gxx() -> Option<String> {
    let candidates = [
        std::env::var("HATCHET_GXX").ok(),
        Some("g++".to_string()),
        Some(r"C:\msys64\mingw32\bin\g++.exe".to_string()),
    ];
    candidates.into_iter().flatten().find(|c| {
        Command::new(c)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

const RECT_HX: &str = r#"package lib;

class Rect {
	public var width(default, null):Float;
	public var height(default, null):Float;
	// computed property: no backing field (not @:isVar), reads call get_area()
	public var area(get, never):Float;
	// write-from-outside, read-within-class: a plain public field in C++
	public var scale(null, default):Float;
	// computed with class-internal physical writes allowed
	public var perimeter(get, null):Float;
	// class-internal only
	var perimCalls(null, null):Int;

	public function new(w:Float, h:Float) {
		width = w;
		height = h;
		scale = 1.0;
		perimCalls = 0;
	}

	function get_area():Float {
		return width * height * scale;
	}

	function get_perimeter():Float {
		perimCalls++;   // direct store: a (null, null) sibling field
		return 2.0 * (width + height);
	}

	public function describe():Float {
		// internal reads route through the accessors, as in Haxe
		return area + perimeter + perimeter;
	}

	public function calls():Int {
		return perimCalls;
	}
}
"#;

const MAIN_CPP: &str = r#"#include <stdio.h>
#include "lib/Rect.h"
int main() {
	lib::Rect r(3.0, 4.0);
	r.scale = 2.0;                          /* (null, default): external write */
	printf("area=%g\n", r.get_area());      /* 3*4*2 = 24 */
	printf("desc=%g\n", r.describe());      /* 24 + 14 + 14 = 52 */
	printf("calls=%d\n", r.calls());        /* get_perimeter ran exactly twice */
	printf("w=%g\n", r.GetWidth());         /* (default, null): generated getter */
	return 0;
}
"#;

/// Every generated `.cpp` under `dir`, recursively.
fn cpp_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(cpp_files(&p));
            } else if p.extension().and_then(|s| s.to_str()) == Some("cpp") {
                out.push(p);
            }
        }
    }
    out
}

#[test]
fn properties_compile_and_run() {
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++ compiler");
        return;
    };

    let root = std::env::temp_dir().join(format!("hatchet_prop_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let lib = root.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("Rect.hx"), RECT_HX).unwrap();
    let main_cpp = root.join("main.cpp");
    std::fs::write(&main_cpp, MAIN_CPP).unwrap();

    let out = root.join("out");
    let gen_ok = Command::new(env!("CARGO_BIN_EXE_hatchet"))
        .arg("--src")
        .arg(&lib)
        .arg("--out")
        .arg(&out)
        .arg("--force")
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(gen_ok, "transpiling the property demo failed");

    // Header shape: the custom getter is declared under its Haxe name, no
    // generated `GetArea` shadows it, and the non-@:isVar `(get, never)`
    // property has no backing field.
    let header = std::fs::read_to_string(out.join("lib").join("Rect.h")).unwrap();
    assert!(
        header.contains("double get_area();"),
        "custom getter declared:\n{header}"
    );
    assert!(
        !header.contains("GetArea"),
        "no generated getter for a custom `(get, …)`:\n{header}"
    );
    assert!(
        !header.contains("double area;"),
        "(get, never) without @:isVar has no backing field:\n{header}"
    );
    assert!(
        header.contains("double perimeter;"),
        "(get, null) keeps its (physical) backing field:\n{header}"
    );

    let exe = out.join(if cfg!(windows) { "prop.exe" } else { "prop" });
    let mut cmd = Command::new(&gxx);
    cmd.args(["-std=c++98", "-pedantic", "-Wall"])
        .arg("-I")
        .arg(&out)
        .arg(&main_cpp);
    for f in cpp_files(&out) {
        cmd.arg(f);
    }
    cmd.arg("-o").arg(&exe);
    let compile = cmd.output().expect("run g++");
    assert!(
        compile.status.success(),
        "the property demo did not compile under g++ -std=c++98:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&exe).output().expect("run the property demo");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let _ = std::fs::remove_dir_all(&root);

    assert!(
        stdout.contains("area=24"),
        "external read through get_area():\n{stdout}"
    );
    assert!(
        stdout.contains("desc=52"),
        "internal reads route through accessors:\n{stdout}"
    );
    assert!(
        stdout.contains("calls=2"),
        "get_perimeter invoked exactly twice:\n{stdout}"
    );
    assert!(
        stdout.contains("w=3"),
        "(default, null) generated getter still works:\n{stdout}"
    );
}

const GAUGE_HX: &str = r#"package lib;

class Gauge {
	public var level(default, set):Int;

	public function new(start:Int) {
		level = start;   // ctor write routes through the setter too
	}

	function set_level(v:Int):Int {
		level = v < 0 ? 0 : (v > 100 ? 100 : v);
		return level;
	}

	public function adjust():Void {
		level += 200;    // compound: set_level(level + 200) -> clamped to 100
		level -= 250;    // -> clamped to 0
		level++;         // -> 1
	}
}

class Tuner {
	public function new() {}

	public function blast(g:Gauge):Void {
		g.level = 999;        // external write routes -> 100
		g.level += 1;         // external compound: set_level(GetLevel() + 1) -> stays 100
	}
}
"#;

const GAUGE_MAIN_CPP: &str = r#"#include <stdio.h>
#include "lib/Gauge.h"
int main() {
	lib::Gauge g(150);                       /* ctor clamps 150 -> 100 */
	printf("start=%d\n", g.GetLevel());
	g.adjust();                              /* 100 -> 0 -> 1 */
	printf("adjusted=%d\n", g.GetLevel());
	lib::Tuner t;
	t.blast(&g);                             /* 999 -> 100, +1 stays 100 */
	printf("blasted=%d\n", g.GetLevel());
	return 0;
}
"#;

#[test]
fn custom_setters_compile_and_run() {
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++ compiler");
        return;
    };

    let root = std::env::temp_dir().join(format!("hatchet_setter_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let lib = root.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("Gauge.hx"), GAUGE_HX).unwrap();
    let main_cpp = root.join("main.cpp");
    std::fs::write(&main_cpp, GAUGE_MAIN_CPP).unwrap();

    let out = root.join("out");
    let gen_ok = Command::new(env!("CARGO_BIN_EXE_hatchet"))
        .arg("--src")
        .arg(&lib)
        .arg("--out")
        .arg(&out)
        .arg("--force")
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(gen_ok, "transpiling the custom-setter demo failed");

    // Header shape: the custom setter is the declared method, no trivial
    // `SetLevel` shadows it, and external reads still have their `GetLevel`.
    let header = std::fs::read_to_string(out.join("lib").join("Gauge.h")).unwrap();
    assert!(
        header.contains("int set_level(int v);"),
        "custom setter declared:\n{header}"
    );
    assert!(
        !header.contains("SetLevel"),
        "no trivial setter when set_level exists:\n{header}"
    );
    assert!(
        header.contains("GetLevel()"),
        "generated getter for external reads:\n{header}"
    );

    let exe = out.join(if cfg!(windows) {
        "setter.exe"
    } else {
        "setter"
    });
    let mut cmd = Command::new(&gxx);
    cmd.args(["-std=c++98", "-pedantic", "-Wall"])
        .arg("-I")
        .arg(&out)
        .arg(&main_cpp);
    for f in cpp_files(&out) {
        cmd.arg(f);
    }
    cmd.arg("-o").arg(&exe);
    let compile = cmd.output().expect("run g++");
    assert!(
        compile.status.success(),
        "the custom-setter demo did not compile under g++ -std=c++98:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&exe)
        .output()
        .expect("run the custom-setter demo");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let _ = std::fs::remove_dir_all(&root);

    assert!(
        stdout.contains("start=100"),
        "ctor write clamps via set_level:\n{stdout}"
    );
    assert!(
        stdout.contains("adjusted=1"),
        "internal compound writes clamp via set_level:\n{stdout}"
    );
    assert!(
        stdout.contains("blasted=100"),
        "external writes clamp via set_level:\n{stdout}"
    );
}

const PARTICLE_HX: &str = r#"package lib;

class Particle {
	public var x(default, set):Float;
	public var vx:cpp.Float32;     // genuine single-precision C++ float
	public var mass:Single;        // the std single-precision alias

	public function new() {
		x = 0.0;
		vx = 1.5;
		mass = 1.0;
	}

	// return type omitted: Haxe infers the property's type (never void)
	public function set_x(x:Float) {
		return this.x = x;
	}

	public function step(dt:cpp.Float32):Float {
		x += vx * dt;
		return x;
	}
}
"#;

const PARTICLE_MAIN_CPP: &str = r#"#include <stdio.h>
#include "lib/Particle.h"
int main() {
	lib::Particle p;
	printf("vx_bytes=%d\n", (int)sizeof(p.vx));     /* genuine float: 4 */
	printf("step=%g\n", p.step(2.0f));              /* 0 + 1.5*2 = 3 */
	printf("ret=%g\n", p.set_x(7.25));              /* setter returns the value */
	return 0;
}
"#;

#[test]
fn float32_and_inferred_setter_return_compile_and_run() {
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++ compiler");
        return;
    };

    let root = std::env::temp_dir().join(format!("hatchet_f32_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let lib = root.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("Particle.hx"), PARTICLE_HX).unwrap();
    let main_cpp = root.join("main.cpp");
    std::fs::write(&main_cpp, PARTICLE_MAIN_CPP).unwrap();

    let out = root.join("out");
    let gen_ok = Command::new(env!("CARGO_BIN_EXE_hatchet"))
        .arg("--src")
        .arg(&lib)
        .arg("--out")
        .arg(&out)
        .arg("--force")
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(gen_ok, "transpiling the Float32 demo failed");

    let header = std::fs::read_to_string(out.join("lib").join("Particle.h")).unwrap();
    assert!(
        header.contains("float vx;"),
        "cpp.Float32 field is a C++ float:\n{header}"
    );
    assert!(
        header.contains("float mass;"),
        "Single field is a C++ float:\n{header}"
    );
    assert!(
        header.contains("double set_x(double x);"),
        "omitted accessor return type is the property's type:\n{header}"
    );

    let exe = out.join(if cfg!(windows) { "f32.exe" } else { "f32" });
    let mut cmd = Command::new(&gxx);
    cmd.args(["-std=c++98", "-pedantic", "-Wall"])
        .arg("-I")
        .arg(&out)
        .arg(&main_cpp);
    for f in cpp_files(&out) {
        cmd.arg(f);
    }
    cmd.arg("-o").arg(&exe);
    let compile = cmd.output().expect("run g++");
    assert!(
        compile.status.success(),
        "the Float32 demo did not compile under g++ -std=c++98:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&exe).output().expect("run the Float32 demo");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let _ = std::fs::remove_dir_all(&root);

    assert!(
        stdout.contains("vx_bytes=4"),
        "Float32 is genuinely 4 bytes:\n{stdout}"
    );
    assert!(
        stdout.contains("step=3"),
        "Float32 arithmetic flows into Float:\n{stdout}"
    );
    assert!(
        stdout.contains("ret=7.25"),
        "the value-returning setter works:\n{stdout}"
    );
}
