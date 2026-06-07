//! Transpiler diagnostics.
//!
//! Hatchet aims to *fail loudly* rather than guess: when it cannot resolve a type
//! or meets a Haxe idiom it does not yet support, it records a [`Diagnostic`] and
//! the run fails (after generating whatever modules were clean). Two severities
//! are distinguished because they mean different things to the developer:
//!
//! * [`Severity::Error`] — the input is wrong or incomplete (e.g. a referenced
//!   type that is not declared / not in `--src` scope). The fix is in the Haxe.
//! * [`Severity::Unsupported`] — the input is valid Haxe, but Hatchet does not
//!   implement it yet. The fix is in Hatchet, so these carry an invitation to
//!   contribute upstream.

/// Where contributors are pointed for `Unsupported` features. Hatchet is an
/// open-source project; update this to the canonical repository URL.
pub const HATCHET_REPO: &str = "https://github.com/andrewglind/hatchet";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// The Haxe input is wrong/incomplete; the developer must fix it.
    Error,
    /// Valid Haxe that Hatchet does not yet transpile; Hatchet should grow to cover
    /// it. Triggers the contribution invite in the report.
    Unsupported,
}

/// A single problem found while transpiling, tied to a source file (and a line
/// when one is known; `0` means "location not pinned more precisely than file").
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub file: String,
    pub line: usize,
    pub severity: Severity,
    pub message: String,
}

impl Diagnostic {
    /// A developer-facing error (wrong/incomplete input).
    pub fn error(file: impl Into<String>, line: usize, message: impl Into<String>) -> Self {
        Diagnostic { file: file.into(), line, severity: Severity::Error, message: message.into() }
    }

    /// A not-yet-supported Haxe idiom. `feature` should read as a noun phrase, as
    /// it is rendered into "`<feature>` is not yet supported by Hatchet."
    pub fn unsupported(file: impl Into<String>, line: usize, feature: impl Into<String>) -> Self {
        Diagnostic {
            file: file.into(),
            line,
            severity: Severity::Unsupported,
            message: format!("{} is not yet supported by Hatchet", feature.into()),
        }
    }

    /// `file:line` when a line is known, else just `file`.
    fn location(&self) -> String {
        if self.line > 0 {
            format!("{}:{}", self.file, self.line)
        } else {
            self.file.clone()
        }
    }

    /// The single-line `error: <loc>: <message>` rendering (the contribution
    /// invite, for `Unsupported`, is printed once for the whole run by
    /// [`report`], not repeated per diagnostic).
    pub fn render(&self) -> String {
        format!("error: {}: {}", self.location(), self.message)
    }
}

/// Print every diagnostic to stderr, followed once by the contribution invite if
/// any of them are `Unsupported`. Returns the count (so the caller can fail).
pub fn report(diags: &[Diagnostic]) -> usize {
    for d in diags {
        eprintln!("{}", d.render());
    }
    if diags.iter().any(|d| d.severity == Severity::Unsupported) {
        eprintln!();
        eprintln!("Some of the above are features Hatchet does not implement yet.");
        eprintln!("Hatchet is open source and contributions are very welcome —");
        eprintln!("please consider raising a PR to add support: {HATCHET_REPO}");
    }
    diags.len()
}
