//! Async entry points for the Paragon SystemVerilog simulator.
//!
//! DO NOT COPY CODE from Verilator. See `docs/design/README.md`.
//!
//! ```no_run
//! # use libparagon::{block_on, simulate};
//! let result = block_on(simulate(r#"
//!     module t;
//!       initial begin $display("hello"); $finish; end
//!     endmodule
//! "#));
//! ```
//!
//! The futures do not depend on any particular runtime, so they can be awaited
//! from tokio, async-std, smol or the small [`block_on`] provided here.
//!
//! # Status
//!
//! [`simulate`] runs the pipeline as far as it exists. Today that is the
//! preprocessor: a source that fails to preprocess returns
//! [`Error::Diagnostics`], and one that preprocesses cleanly returns
//! [`Error::NotImplemented`] naming the next stage. The signature will not
//! change as parsing, elaboration and simulation are added.

mod executor;

/// Compiles and runs the Rust examples in the repository README.
#[doc = include_str!("../../README.md")]
#[cfg(doctest)]
pub struct ReadmeDoctests;

pub use executor::block_on;

use paragon_of_uncanny_reason::pp::emit::{EmitOptions, write};
use paragon_of_uncanny_reason::pp::{self, FileSystem, Preprocessor};
use paragon_of_uncanny_reason::source::SourceMap;
use std::fmt;

/// Settings for a run. [`Options::default`] suits a single self-contained source.
#[derive(Clone, Debug)]
pub struct Options {
    /// The name the source text is given in diagnostics and `` `__FILE__ ``.
    pub source_name: String,
    /// Directories searched for `` `include `` files. `"."` is the current directory.
    pub include_dirs: Vec<String>,
    /// Defines, as from `-DNAME=VALUE`. An empty value defines the name with no text.
    pub defines: Vec<(String, String)>,
    /// Let `` `include `` read files from disk. When false, only the source text is visible.
    pub read_includes_from_disk: bool,
    /// Emit `` `line `` markers in [`Preprocessed::text`] (`-E` rather than `-E -P`).
    pub line_markers: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            source_name: "input.sv".into(),
            include_dirs: vec![".".into()],
            defines: Vec::new(),
            read_includes_from_disk: true,
            line_markers: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// A diagnostic with its position resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    /// Warning code such as `REDEFMACRO`, if the diagnostic has one.
    pub code: Option<String>,
    pub file: String,
    pub line: usize,
    pub col: usize,
    pub message: String,
    /// Follow-on lines, such as the location of a previous definition.
    pub notes: Vec<String>,
}

impl fmt::Display for Diagnostic {
    /// Formats as `%Error-CODE: file:line:col: message`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sev = match self.severity {
            Severity::Error => "Error",
            Severity::Warning => "Warning",
        };
        let code = self
            .code
            .as_deref()
            .map(|c| format!("-{c}"))
            .unwrap_or_default();
        write!(
            f,
            "%{sev}{code}: {}:{}:{}: {}",
            self.file, self.line, self.col, self.message
        )?;
        for n in &self.notes {
            write!(f, "\n    {n}")?;
        }
        Ok(())
    }
}

/// A pipeline stage, used to report how far a run got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Preprocess,
    Parse,
    Elaborate,
    Simulate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The source has errors. Warnings found along the way are included.
    Diagnostics(Vec<Diagnostic>),
    /// The pipeline does not yet implement this stage.
    NotImplemented { stage: Stage },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Diagnostics(d) => {
                for d in d {
                    writeln!(f, "{d}")?;
                }
                let errors = d.iter().filter(|d| d.severity == Severity::Error).count();
                write!(f, "%Error: Exiting due to {errors} error(s)")
            }
            Error::NotImplemented { stage } => write!(f, "{stage:?} is not implemented yet"),
        }
    }
}

impl std::error::Error for Error {}

/// Preprocessor output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preprocessed {
    /// The preprocessed text, as `-E` would print it.
    pub text: String,
    pub warnings: Vec<Diagnostic>,
}

/// How a simulation ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finish {
    /// `$finish` was called.
    Finish,
    /// `$stop` was called.
    Stop,
    /// `$fatal` was called, or an assertion failed fatally.
    Fatal,
    /// No more events were scheduled.
    Quiescent,
}

/// The result of a completed simulation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimResult {
    /// Text written by `$display`, `$write` and friends.
    pub stdout: String,
    pub finish: Finish,
    /// Simulation time when it ended, in the design's time precision.
    pub time: u64,
    pub warnings: Vec<Diagnostic>,
}

/// Simulate SystemVerilog source text with default [`Options`].
pub async fn simulate(source: &str) -> Result<SimResult, Error> {
    simulate_with(source, &Options::default()).await
}

/// Simulate SystemVerilog source text.
pub async fn simulate_with(source: &str, opts: &Options) -> Result<SimResult, Error> {
    let _pre = preprocess_with(source, opts).await?;
    Err(Error::NotImplemented {
        stage: Stage::Parse,
    })
}

/// Preprocess SystemVerilog source text with default [`Options`].
pub async fn preprocess(source: &str) -> Result<Preprocessed, Error> {
    preprocess_with(source, &Options::default()).await
}

/// Preprocess SystemVerilog source text, as `-E` does.
pub async fn preprocess_with(source: &str, opts: &Options) -> Result<Preprocessed, Error> {
    let fs = TextFs {
        name: &opts.source_name,
        text: source,
        disk: opts.read_includes_from_disk,
    };
    let sm = SourceMap::new();
    let pp_opts = pp::Options {
        include_dirs: opts.include_dirs.clone(),
        defines: opts.defines.clone(),
        ..pp::Options::default()
    };
    let mut pp = Preprocessor::new(&sm, &fs, pp_opts);
    pp.process_file(&opts.source_name);
    let diags: Vec<Diagnostic> = pp.diagnostics().iter().map(|d| resolve(&sm, d)).collect();
    if diags.iter().any(|d| d.severity == Severity::Error) {
        return Err(Error::Diagnostics(diags));
    }
    let text = write(
        &sm,
        pp.output(),
        EmitOptions {
            line_markers: opts.line_markers,
        },
    );
    Ok(Preprocessed {
        text,
        warnings: diags,
    })
}

/// The source text under its name, plus the disk for includes if allowed.
struct TextFs<'s> {
    name: &'s str,
    text: &'s str,
    disk: bool,
}

impl FileSystem for TextFs<'_> {
    fn read(&self, path: &str) -> Option<String> {
        if path == self.name {
            Some(self.text.to_string())
        } else if self.disk {
            std::fs::read_to_string(path).ok()
        } else {
            None
        }
    }
}

fn resolve(sm: &SourceMap, d: &pp::Diag) -> Diagnostic {
    let at = |s: &str| {
        sm.locate(s)
            .map(|l| (l.name, l.line, l.col))
            .unwrap_or_default()
    };
    let (file, line, col) = at(d.at);
    Diagnostic {
        severity: match d.severity {
            pp::Severity::Error => Severity::Error,
            pp::Severity::Warning => Severity::Warning,
        },
        code: d.code.map(String::from),
        file,
        line,
        col,
        message: d.message.clone(),
        notes: d
            .notes
            .iter()
            .map(|(s, msg)| {
                let (file, line, col) = at(s);
                format!("{file}:{line}:{col}: {msg}")
            })
            .collect(),
    }
}
