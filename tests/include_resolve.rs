//! `--include` inputs are resolve-only: parsed so the `--src` files' references
//! resolve, but never transpiled. This guards that a file reached via `--include`
//! produces no `.h`/`.cpp`, while the `--src` file beside it still does.

use std::process::Command;

const APP: &str = r#"package app;
class App {
	public function new() {}
	public function run():Int { return 42; }
}
"#;

const STUB: &str = r#"package app;
class Stub {
	public function new() {}
}
"#;

#[test]
fn include_inputs_are_not_transpiled() {
    let root = std::env::temp_dir().join(format!("hatchet_inc_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let app_dir = root.join("app");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::write(app_dir.join("App.hx"), APP).unwrap();
    std::fs::write(app_dir.join("Stub.hx"), STUB).unwrap();

    let out = root.join("out");
    let ok = Command::new(env!("CARGO_BIN_EXE_hatchet"))
        .arg("--src")
        .arg(app_dir.join("App.hx"))
        .arg("--include")
        .arg(app_dir.join("Stub.hx"))
        .arg("--out")
        .arg(&out)
        .arg("--force")
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "transpiling with --include failed");

    let app_out = out.join("app");
    // The `--src` file is emitted.
    assert!(app_out.join("App.h").is_file(), "App.h should be generated");
    assert!(app_out.join("App.cpp").is_file(), "App.cpp should be generated");
    // The `--include` file is resolve-only — never emitted.
    assert!(!app_out.join("Stub.h").is_file(), "Stub.h must not be generated from --include");
    assert!(!app_out.join("Stub.cpp").is_file(), "Stub.cpp must not be generated from --include");

    let _ = std::fs::remove_dir_all(&root);
}
