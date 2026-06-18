//! Compile + run gate for a value-position `switch` whose arms are different
//! subclasses (+ `null`). The hoisted temporary must be typed as the *expected*
//! type (here the function's return type, the common base `Scene`), not the
//! first arm's subclass — otherwise assigning a sibling subclass to it is
//! nonsense C++ that fails to compile. Guards the regression where the temp was
//! typed from the first arm.
//!
//! Skipped (passes vacuously) when no C++ compiler is available.

use std::path::{Path, PathBuf};
use std::process::Command;

fn find_gxx() -> Option<String> {
    let candidates = [
        std::env::var("HATCHET_GXX").ok(),
        Some("g++".to_string()),
        Some(r"C:\msys64\mingw32\bin\g++.exe".to_string()),
    ];
    candidates.into_iter().flatten().find(|c| {
        Command::new(c).arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
    })
}

const SRC: &str = r#"package lib;

final ALIENBEACH_SCENE_ID:Int = 0;
final POINTS_SCENE_ID:Int = 1;

class Scene {
	public var id:Int;
	public function new() { id = -1; }
	public function tag():Int { return id; }
}
class AlienBeach extends Scene { public function new() { super(); id = 100; } }
class Points extends Scene { public function new() { super(); id = 200; } }

class Factory {
	public function new() {}
	public function make(sceneId:Int):Scene {
		return switch sceneId {
			case ALIENBEACH_SCENE_ID: new AlienBeach();
			case POINTS_SCENE_ID: new Points();
			default: null;
		}
	}
}
"#;

const MAIN_CPP: &str = r#"#include <stdio.h>
#include "lib/Factory.h"
using namespace lib;
int main() {
	Factory f;
	Scene* a = f.make(0);
	Scene* p = f.make(1);
	Scene* n = f.make(9);
	printf("%d %d %d\n", a->tag(), p->tag(), n == 0 ? -1 : n->tag());
	return 0;
}
"#;

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
fn polymorphic_value_switch_compiles_and_runs() {
    let Some(gxx) = find_gxx() else {
        eprintln!("skipping: no C++ compiler");
        return;
    };

    let root = std::env::temp_dir().join(format!("hatchet_vswx_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let lib = root.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("Factory.hx"), SRC).unwrap();
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
    assert!(gen_ok, "transpiling the value-switch demo failed");

    // The temp is the base/return type, not the first arm's subclass.
    let body = std::fs::read_to_string(out.join("lib").join("Factory.cpp")).unwrap();
    assert!(body.contains("Scene* _swx"), "value-switch temp is the base type:\n{body}");

    let exe = out.join(if cfg!(windows) { "vswx.exe" } else { "vswx" });
    let mut cmd = Command::new(&gxx);
    cmd.args(["-std=c++98", "-pedantic", "-Wall"]).arg("-I").arg(&out).arg(&main_cpp);
    for f in cpp_files(&out) {
        cmd.arg(f);
    }
    cmd.arg("-o").arg(&exe);
    let compile = cmd.output().expect("run g++");
    assert!(
        compile.status.success(),
        "the value-switch demo did not compile under g++ -std=c++98:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = Command::new(&exe).output().expect("run the value-switch demo");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let _ = std::fs::remove_dir_all(&root);

    // AlienBeach.tag()=100, Points.tag()=200, null→-1 (dispatch through base ptr).
    assert!(stdout.contains("100 200 -1"), "polymorphic value-switch dispatch wrong:\n{stdout}");
}
