//! Command-line interface and top-level driver.
//!
//! `--src` accepts any mix of single `.hx` files, glob patterns (`modules/*.hx`,
//! `src/**/*.hx`), and directories (crawled recursively for `.hx`). The resulting
//! set of files is also the entire resolution scope, so a file's dependencies
//! (superclasses, native `@:native` stubs) must be reachable in that set too.
//!
//! There is no `--root` flag and no namespace flag: a file's project root (used for
//! the output layout and relative includes) is inferred from its `package`
//! declaration — the file's directory minus its package path — and the C++
//! namespace is always that same `package` (empty/absent package → no namespace).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;

use crate::sema::{Module, Program};
use crate::{codegen, diag, discover, parser, sema, stdafx};

#[derive(Parser, Debug)]
#[command(
    name = "hatchet",
    version,
    about = "Transpile Haxe 4.x to C++98 / Visual C++ 6.0 compatible code"
)]
pub struct Args {
    /// Haxe `.hx` source(s) to transpile (prompted if omitted). Each entry may be
    /// a single file, a directory (crawled recursively for `.hx`), or a glob
    /// (`*`/`?`/`**`, e.g. `modules/*.hx` or `src/**/*.hx`) — and you may pass
    /// several, e.g. `--src modules mucus/Mucus.hx`. Globs are expanded by Hatchet
    /// itself, so quoting them to bypass shell expansion works too. The full
    /// expanded set is also the whole resolution scope, so a file's dependencies
    /// (superclasses, native `@:native` stubs) must be reachable in it. Each
    /// file's project root is inferred from its `package` declaration.
    #[arg(short, long, value_name = "PATH", num_args = 1..)]
    pub src: Vec<PathBuf>,

    /// Output directory for generated .h/.cpp (defaults to the inferred project
    /// root, so files are produced alongside their Haxe sources). Ignored with
    /// `--dry-run` / `--stdout`.
    #[arg(short, long)]
    pub out: Option<PathBuf>,

    /// Overwrite existing generated files without prompting. Ignored with
    /// `--dry-run` (nothing is written).
    #[arg(long)]
    pub force: bool,

    /// Transpile and report info, status, warnings and errors only — write
    /// nothing. Takes precedence over `--stdout`, `-o/--out` and `--force`, which
    /// are accepted but have no effect.
    #[arg(long)]
    pub dry_run: bool,

    /// Write generated C++ to stdout instead of to files (file output is the
    /// default). Status messages go to stderr so the stream stays pipeable.
    #[arg(long)]
    pub stdout: bool,

    /// Maximum expression-nesting depth at which a buried `Null<T>` call result is
    /// auto-extracted into a freed local instead of warned about.
    #[arg(long, default_value_t = 1)]
    pub depth: usize,

    /// Strip all `trace(...)` calls from the generated C++ (lowered to no-ops,
    /// arguments not evaluated), mirroring hxcpp's `-D no-traces`.
    #[arg(long)]
    pub no_traces: bool,

    /// Name (stem) of the prelude source/header. e.g. `--stdafx MyGame` to use `MyGame.hx` instead of `StdAfx.hx`.
    #[arg(long, default_value = "StdAfx")]
    pub stdafx: String,

    /// Prefix for the platform export/calling-convention macros wrapped around
    /// `extern inline` functions. e.g. `--export-macro MUCUS` emits `MUCUS_EXPORT`
    /// / `MUCUS_CALL` (defined in the prelude). Defaults to `HATCHET`.
    #[arg(long, default_value = "HATCHET")]
    pub export_macro: String,
}

/// Where generated C++ goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutputMode {
    /// Write `.h`/`.cpp` files (the default).
    Files,
    /// Print generated C++ to stdout; write nothing.
    Stdout,
    /// Write nothing and print nothing to stdout — only info/warnings/errors.
    DryRun,
}

/// Process-level entry point.
pub fn main() -> ExitCode {
    let args = Args::parse();
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hatchet: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Resolved configuration after flags + interactive prompts.
pub struct Config {
    /// Inferred project root (the files' directory minus their package path). Used
    /// as the base for the output layout and relative includes.
    pub root_dir: PathBuf,
    /// The explicit `.hx` files to transpile — also the entire resolution scope.
    pub files: Vec<PathBuf>,
    /// Output directory (used in `Files` mode only).
    pub out_dir: PathBuf,
    pub force: bool,
    /// Buried-`Null<T>` auto-extraction depth (see `Args::depth`).
    pub extract_depth: usize,
    /// Strip all `trace(...)` calls from the generated C++ (see `Args::no_trace`).
    pub no_traces: bool,
    pub mode: OutputMode,
    /// Stem of the prelude source/header (default `StdAfx`).
    pub stdafx_stem: String,
    /// Prefix for the platform export/calling-convention macros (default `HATCHET`).
    pub export_macro: String,
}

impl Config {
    /// Status/info text goes to stderr in `Stdout` mode (so stdout carries only
    /// generated code), and to stdout otherwise.
    fn info(&self, msg: &str) {
        if self.mode == OutputMode::Stdout {
            eprintln!("{msg}");
        } else {
            println!("{msg}");
        }
    }
}

fn run(args: Args) -> Result<(), String> {
    let cfg = resolve_config(args)?;

    // The resolution scope is exactly the expanded file set (files + crawled
    // directories + globs). A file's dependencies (superclasses, native stubs)
    // must therefore be reachable in that set, or its references will not resolve.
    cfg.info(&format!("Transpiling {} Haxe file(s).", cfg.files.len()));

    // Parse everything, keeping the raw source alongside (StdAfx needs it).
    let mut units = Vec::new();
    let mut sources = Vec::new();
    for hx in &cfg.files {
        let src = std::fs::read_to_string(hx).map_err(|e| format!("reading {}: {e}", hx.display()))?;
        let file = parser::parse(&src).map_err(|e| format!("{}: {e}", hx.display()))?;
        units.push((hx.clone(), file));
        sources.push(src);
    }

    let mut prog = Program::build_with(&cfg.root_dir, units, &cfg.stdafx_stem);
    prog.export_macro = cfg.export_macro.clone();
    // When writing files somewhere other than in-place, re-base includes that
    // escape the generated tree (the external engine / sibling projects) onto the
    // real output location, so `--out <anywhere>` resolves rather than only an
    // output dir that sits where the source tree does. In-place stays byte-for-byte.
    if cfg.mode == OutputMode::Files {
        let root_abs = canonical(&cfg.root_dir)?;
        if root_abs != cfg.out_dir {
            prog.include_rebase = Some((root_abs, cfg.out_dir.clone()));
        }
    }

    // Hatchet generates a `StdAfx.h` for every output directory (the StdAfx pass
    // below), so a developer-provided `StdAfx.hx` is optional. Map each directory
    // to the `StdAfx.hx` that supplies its `@:headerCode`, if one exists.
    let stdafx_src: std::collections::BTreeMap<Vec<String>, usize> = prog
        .modules
        .iter()
        .enumerate()
        .filter(|(_, m)| m.is_stdafx)
        .map(|(i, m)| (m.dir.clone(), i))
        .collect();

    let mut emitted = 0usize;
    let mut errors: Vec<diag::Diagnostic> = Vec::new();
    // Output directories that received a header → their package (for the StdAfx
    // guard); a StdAfx.h is emitted into each one afterwards.
    let mut header_dirs: std::collections::BTreeMap<Vec<String>, Vec<String>> =
        std::collections::BTreeMap::new();
    for (i, m) in prog.modules.iter().enumerate() {
        // `Main.hx` is the hxcpp entry point and is never transpiled; `StdAfx.hx`
        // is never emitted directly (the StdAfx pass produces the prelude header
        // for every directory that gets one).
        if discover::is_main(&m.path) || m.is_stdafx {
            continue;
        }

        // Fail loudly rather than guess: a module that references a type Hatchet
        // cannot resolve — or uses a construct Hatchet does not yet support — is not
        // generated (its output would be silently wrong or non-compiling). Clean
        // modules are still emitted; the run fails at the end.
        let mut module_errors = sema::validate::unresolved_type_errors(&prog, i);
        module_errors.extend(sema::validate::unsupported_construct_errors(&prog, i));
        if !module_errors.is_empty() {
            errors.extend(module_errors);
            continue;
        }

        // Generate the body first: a "do not guess" error found during body
        // generation (an overloaded call matching no @:overload signature) needs
        // expression-type inference, so it surfaces here rather than in sema.
        // Treat it like a sema error — collect it and skip the whole module
        // (header included) rather than emit a half-written pair.
        let source = codegen::generate_source_diagnostics(&prog, i, cfg.extract_depth, cfg.no_traces);
        if let Some((_, _, body_errors)) = &source {
            if !body_errors.is_empty() {
                let rel = m.path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                for (line, e) in body_errors {
                    errors.push(diag::Diagnostic::error(rel, *line, e.clone()));
                }
                continue;
            }
        }

        if let Some(header) = codegen::generate_header(&prog, i) {
            emit_artifact(&cfg, &out_module_path(&cfg, m, "h"), &module_rel(m, "h"), &header, &mut emitted)?;
            header_dirs.insert(m.dir.clone(), m.package.clone());
        }
        if let Some((source, warnings, _)) = source {
            emit_artifact(&cfg, &out_module_path(&cfg, m, "cpp"), &module_rel(m, "cpp"), &source, &mut emitted)?;
            let rel = m.path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            for (line, w) in warnings {
                if line > 0 {
                    eprintln!("warning: {rel}:{line}: {w}");
                } else {
                    eprintln!("warning: {rel}: {w}");
                }
            }
        }
    }

    // StdAfx pass: one prelude header per directory that received a header,
    // merging that directory's prelude-source `@:headerCode` (if any) with Hatchet's
    // required includes. The file name follows the configured stem.
    let stdafx_file = format!("{}.h", cfg.stdafx_stem);
    for (dir, package) in &header_dirs {
        let header_code = stdafx_src.get(dir).and_then(|&i| {
            stdafx::extract_header_code(&sources[i]).map(|h| h.replace("\r\n", "\n").replace('\r', "\n"))
        });
        let content =
            stdafx::content_for(&cfg.stdafx_stem, header_code.as_deref(), package, &cfg.export_macro);
        let mut out_path = cfg.out_dir.clone();
        for part in dir {
            out_path.push(part);
        }
        out_path.push(&stdafx_file);
        let label = if dir.is_empty() {
            stdafx_file.clone()
        } else {
            format!("{}/{stdafx_file}", dir.join("/"))
        };
        emit_artifact(&cfg, &out_path, &label, &content, &mut emitted)?;
    }

    let verb = match cfg.mode {
        OutputMode::Files => "Generated",
        OutputMode::Stdout => "Emitted",
        OutputMode::DryRun => "Would generate",
    };
    if !errors.is_empty() {
        eprintln!();
        let n = diag::report(&errors);
        let skipped = errors_module_count(&errors);
        cfg.info(&format!("\n{verb} {emitted} file(s); {skipped} module(s) skipped due to errors."));
        return Err(format!("{n} error(s); {skipped} module(s) were not generated"));
    }

    cfg.info(&format!("\n{verb} {emitted} file(s)."));
    Ok(())
}

/// Emit one generated artifact according to the output mode: write a file, print
/// to stdout (with a banner so multiple files stay distinguishable), or — in a
/// dry run — produce nothing but count it.
fn emit_artifact(
    cfg: &Config,
    out_path: &Path,
    label: &str,
    content: &str,
    emitted: &mut usize,
) -> Result<(), String> {
    match cfg.mode {
        OutputMode::Files => write_file(out_path, content, cfg.force)?,
        OutputMode::Stdout => {
            println!("// ===== {label} =====");
            print!("{content}");
            if !content.ends_with('\n') {
                println!();
            }
        }
        OutputMode::DryRun => {}
    }
    *emitted += 1;
    Ok(())
}

/// A module's generated file path relative to the source tree, e.g.
/// `modules/Vertex.h` — used as the stdout banner / dry-run label.
fn module_rel(m: &Module, ext: &str) -> String {
    let stem = m.path.file_stem().and_then(|s| s.to_str()).unwrap_or("Module");
    let file = format!("{stem}.{ext}");
    if m.dir.is_empty() {
        file
    } else {
        format!("{}/{}", m.dir.join("/"), file)
    }
}

/// Expand one `--src` entry into the concrete `.hx` files it denotes:
/// - a glob (`*`/`?`/`**`) → its matching `.hx` files;
/// - a directory → every `.hx` under it (recursively);
/// - a plain file → itself (which must exist and be a `.hx`).
///
/// Globs and directories pre-filter to `.hx`; only a directly-named file is
/// rejected for a wrong extension (a clear user mistake rather than a sweep).
fn expand_input(arg: &Path) -> Result<Vec<PathBuf>, String> {
    let s = arg.to_string_lossy();
    if discover::is_glob(&s) {
        let matches = discover::glob_hx(&s);
        if matches.is_empty() {
            return Err(format!("no .hx files match pattern: {s}"));
        }
        return Ok(matches);
    }
    let p = canonical(arg)?;
    if p.is_dir() {
        let files =
            discover::find_haxe_files(&p).map_err(|e| format!("crawling {}: {e}", p.display()))?;
        if files.is_empty() {
            return Err(format!("no .hx files under directory: {}", p.display()));
        }
        return Ok(files);
    }
    if !p.is_file() {
        return Err(format!("source not found: {}", p.display()));
    }
    if p.extension().and_then(|e| e.to_str()) != Some("hx") {
        return Err(format!("not a Haxe (.hx) file: {}", p.display()));
    }
    Ok(vec![p])
}

/// Infer a file's project root: its directory with its `package` path stripped
/// from the end. e.g. `<root>/modules/Vertex.hx` declaring `package modules;`
/// → `<root>`. Falls back to the file's own directory when the layout does not
/// match the package (or there is no package).
fn infer_root(file: &Path, package: &[String]) -> PathBuf {
    let parent = file.parent().unwrap_or_else(|| Path::new("."));
    if package.is_empty() {
        return parent.to_path_buf();
    }
    let comps: Vec<_> = parent.components().collect();
    if comps.len() >= package.len() {
        let tail = &comps[comps.len() - package.len()..];
        let matches = tail
            .iter()
            .zip(package)
            .all(|(c, p)| c.as_os_str().to_str() == Some(p.as_str()));
        if matches {
            return comps[..comps.len() - package.len()].iter().collect();
        }
    }
    parent.to_path_buf()
}

/// Number of distinct source files that contributed at least one error.
fn errors_module_count(errors: &[diag::Diagnostic]) -> usize {
    let mut files: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for e in errors {
        files.insert(e.file.as_str());
    }
    files.len()
}

fn resolve_config(args: Args) -> Result<Config, String> {
    // --dry-run takes precedence over --stdout, which takes precedence over the
    // default (write files). The superseded flags are accepted but ignored.
    let mode = if args.dry_run {
        OutputMode::DryRun
    } else if args.stdout {
        OutputMode::Stdout
    } else {
        OutputMode::Files
    };
    // Status routing mirrors `Config::info` (stderr in Stdout mode, else stdout)
    // so prompts/notes never pollute a piped stdout stream.
    let info = |msg: &str| {
        if mode == OutputMode::Stdout {
            eprintln!("{msg}");
        } else {
            println!("{msg}");
        }
    };

    // Gather the inputs (prompt for one when none was given), then expand each
    // entry — file, directory, or glob — into concrete `.hx` files. The expanded
    // set is the whole resolution scope.
    let raw = if args.src.is_empty() {
        vec![PathBuf::from(prompt(
            "Enter a Haxe (.hx) file, directory, or glob",
            Some("src"),
        ))]
    } else {
        args.src.clone()
    };

    let mut files = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for arg in raw {
        for p in expand_input(&arg)? {
            let p = canonical(&p)?;
            // Main.hx is the hxcpp entry point and is never transpiled; a crawl or
            // glob may sweep it in, so drop it rather than erroring.
            if discover::is_main(&p) {
                info(&format!("Skipping {} (Main.hx is never transpiled).", p.display()));
                continue;
            }
            if seen.insert(p.clone()) {
                files.push(p);
            }
        }
    }
    if files.is_empty() {
        return Err("no transpilable .hx files given".to_string());
    }

    // The project root is inferred from each file's `package` declaration — its
    // directory minus its package path. All files must agree on one root (a single
    // project per invocation), which anchors the output layout and relative includes.
    let mut root_dir: Option<PathBuf> = None;
    for f in &files {
        let src = std::fs::read_to_string(f).map_err(|e| format!("reading {}: {e}", f.display()))?;
        let r = infer_root(f, &crate::scan::package_parts(&src));
        match &root_dir {
            None => root_dir = Some(r),
            Some(existing) if existing != &r => {
                return Err(format!(
                    "files resolve to different project roots ({} vs {}); \
                     transpile one project at a time",
                    existing.display(),
                    r.display()
                ));
            }
            _ => {}
        }
    }
    
    let root_dir = root_dir.expect("at least one file present");
    
    // The output directory is only meaningful when writing files; it defaults to
    // the inferred project root, so generated files land beside their sources.
    let out_dir = if mode == OutputMode::Files {
        let out = match args.out {
            Some(p) => p,
            None => {
                let def = root_dir.display().to_string();
                PathBuf::from(prompt("Enter the target directory for generated files", Some(&def)))
            }
        };
        std::fs::create_dir_all(&out).map_err(|e| format!("creating {}: {e}", out.display()))?;
        canonical(&out)?
    } else {
        PathBuf::new()
    };

    let stdafx_stem = {
        let s = args.stdafx.trim();
        if s.is_empty() { "StdAfx".to_string() } else { s.to_string() }
    };
    let export_macro = {
        let s = args.export_macro.trim();
        if s.is_empty() { "HATCHET".to_string() } else { s.to_string() }
    };

    Ok(Config {
        root_dir,
        files,
        out_dir,
        force: args.force,
        extract_depth: args.depth.max(1),
        no_traces: args.no_traces,
        mode,
        stdafx_stem,
        export_macro,
    })
}

/// Map the in-place output path (next to the source) into the configured output
/// root, mirroring the source tree layout.
/// Output path for a module's generated file (`ext` = "h" or "cpp"), mirroring
/// the source tree.
fn out_module_path(cfg: &Config, m: &Module, ext: &str) -> PathBuf {
    let stem = m.path.file_stem().and_then(|s| s.to_str()).unwrap_or("Module");
    let mut p = cfg.out_dir.clone();
    for part in &m.dir {
        p.push(part);
    }
    p.push(format!("{stem}.{ext}"));
    p
}

fn write_file(path: &Path, content: &str, force: bool) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    if path.exists() && !force {
        return Err(format!(
            "{} already exists (use --force to overwrite)",
            path.display()
        ));
    }
    // Generated C++ is always LF, regardless of the host or source line endings.
    let normalized = content.replace("\r\n", "\n");
    std::fs::write(path, normalized).map_err(|e| format!("writing {}: {e}", path.display()))
}

fn canonical(p: &Path) -> Result<PathBuf, String> {
    // Fall back to the un-canonicalised path when it doesn't yet exist.
    std::fs::canonicalize(p).or_else(|_| Ok(p.to_path_buf()))
}

fn prompt(message: &str, default: Option<&str>) -> String {
    match default {
        Some(d) => print!("{message} [{d}]: "),
        None => print!("{message}: "),
    }
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return default.unwrap_or("").to_string();
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        default.unwrap_or("").to_string()
    } else {
        trimmed.to_string()
    }
}
