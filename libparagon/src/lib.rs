//! Async entry points for the Paragon SystemVerilog simulator.
//!
//! DO NOT COPY CODE from Verilator. See `docs/design/README.md`.
//!
//! ```no_run
//! # use libparagon::{Event, block_on, simulate};
//! # block_on(async {
//! let mut sim = simulate(r#"
//!     module t;
//!       initial begin $display("hello"); $finish; end
//!     endmodule
//! "#).await?;
//! while let Some(event) = sim.next_event().await {
//!     if let Event::Display { text, .. } = event {
//!         print!("{text}");
//!     }
//! }
//! # Ok::<(), libparagon::Error>(()) });
//! ```
//!
//! The futures do not depend on any particular runtime, so they can be awaited
//! from tokio, async-std, smol or the small [`block_on`] provided here.
//!
//! # Status
//!
//! [`simulate`] compiles the source and returns a [`Simulation`] whose events
//! arrive as it runs, from the reference interpreter. A source with errors
//! returns [`Error::Diagnostics`]; one using a construct Paragon does not
//! support yet returns them with the code `NOTYET`. The signature will
//! not change as later stages are added.

mod channel;
mod executor;

/// Compiles and runs the Rust examples in the repository README.
#[doc = include_str!("../../README.md")]
#[cfg(doctest)]
pub struct ReadmeDoctests;

pub use executor::block_on;

use paragon_of_uncanny_reason::ir::ReportSeverity;
use paragon_of_uncanny_reason::keywords::Lang;
use paragon_of_uncanny_reason::pp::emit::{EmitOptions, write};
use paragon_of_uncanny_reason::pp::{self, FileSystem, Preprocessor};
use paragon_of_uncanny_reason::source::SourceMap;
use paragon_of_uncanny_reason::{elab, lex, parse, sim};
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
    /// The language version, as `--language` takes it (`"1800-2017"`,
    /// `"1364-2005"`). `None` means IEEE 1800-2023.
    pub language: Option<String>,
    /// More source files read from disk and compiled after the source text,
    /// sharing its defines, as with extra files on Verilator's command line.
    pub extra_files: Vec<String>,
    /// The top module (`--top-module`). By default, every module that no
    /// other module instantiates is a top.
    pub top: Option<String>,
    /// Library directories (`-y`): a module that no file defines is looked
    /// for as `<dir>/<module><ext>`.
    pub lib_dirs: Vec<String>,
    /// Extensions tried in library directories (`+libext+`).
    pub lib_exts: Vec<String>,
    /// Stop a simulation after this many IR instructions (a guard against
    /// infinite zero-delay loops). `None` uses the simulator's default.
    pub max_steps: Option<u64>,
    /// Top-level inputs to drive as clocks, toggling every time step, like
    /// Verilator's test bench does for `clk`. Other top-level inputs are 0.
    pub clocks: Vec<String>,
    /// A name for the test bench around the top modules, which prefixes
    /// hierarchical names: `Some("top")` makes `%m` print `top.t`.
    pub root_name: Option<String>,
    /// `-Gname=value` overrides for top-level parameters.
    pub params: Vec<(String, String)>,
    /// Stop a simulation that runs longer than this (wall-clock time); it
    /// then ends with [`Finish::Aborted`] and a diagnostic.
    pub time_limit: Option<std::time::Duration>,
    /// The top module's instance name (Verilator's `--l2-name`).
    pub top_instance: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            source_name: "input.sv".into(),
            include_dirs: vec![".".into()],
            defines: Vec::new(),
            read_includes_from_disk: true,
            line_markers: false,
            language: None,
            extra_files: Vec::new(),
            top: None,
            lib_dirs: Vec::new(),
            lib_exts: vec![".v".into(), ".sv".into()],
            max_steps: None,
            clocks: Vec::new(),
            root_name: None,
            params: Vec::new(),
            time_limit: None,
            top_instance: None,
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
    /// The simulation thread ended without saying why (for example, it panicked).
    Aborted,
}

/// Something that happened during a simulation, in the order it happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// Text from `$display`, `$write`, `$strobe` or `$monitor`, exactly as
    /// written. `$display` text ends with a newline; `$write` text need not.
    Display { text: String, time: u64 },
    /// A run-time diagnostic, such as `$warning`, `$error` or a failed assertion.
    Diagnostic(Diagnostic),
    /// The simulation ended. This is always the last event.
    Finished { finish: Finish, time: u64 },
}

/// A running simulation. Events arrive while it runs.
///
/// Dropping the handle stops the simulation.
pub struct Simulation {
    events: channel::Receiver<Event>,
    done: bool,
}

/// Where a running simulation sends its events.
pub(crate) type EventSender = channel::Sender<Event>;

/// Start a simulation on its own thread. Compile-time `warnings` are sent
/// first, then `run` executes and returns how and when the simulation ended,
/// and that is sent as the final [`Event::Finished`]. If `run` panics, the
/// receiver sees [`Finish::Aborted`]. `run` should stop early once
/// [`channel::Sender::is_closed`] is true, which means the handle was dropped.
// The simulation thread is started by `simulate_with`; this simpler form is
// used by the tests of the event plumbing.
#[cfg(test)]
pub(crate) fn start<F>(warnings: Vec<Diagnostic>, run: F) -> Simulation
where
    F: FnOnce(&EventSender) -> (Finish, u64) + Send + 'static,
{
    let (tx, rx) = channel::channel();
    std::thread::spawn(move || {
        for w in warnings {
            if tx.send(Event::Diagnostic(w)).is_err() {
                return;
            }
        }
        let (finish, time) = run(&tx);
        let _ = tx.send(Event::Finished { finish, time });
    });
    Simulation {
        events: rx,
        done: false,
    }
}

impl Simulation {
    /// The next event, or `None` after [`Event::Finished`].
    pub async fn next_event(&mut self) -> Option<Event> {
        if self.done {
            return None;
        }
        let e = self.events.recv().await;
        self.track(e)
    }

    /// Like [`next_event`](Self::next_event), but blocks the current thread.
    pub fn next_event_blocking(&mut self) -> Option<Event> {
        if self.done {
            return None;
        }
        let e = self.events.recv_blocking();
        self.track(e)
    }

    /// Always end with `Finished`, even if the simulation thread vanished.
    fn track(&mut self, e: Option<Event>) -> Option<Event> {
        let e = e.unwrap_or(Event::Finished {
            finish: Finish::Aborted,
            time: 0,
        });
        self.done = matches!(e, Event::Finished { .. });
        Some(e)
    }

    /// Run to the end and gather the events into a [`SimResult`].
    pub async fn wait(mut self) -> SimResult {
        let mut r = SimResult {
            stdout: String::new(),
            finish: Finish::Aborted,
            time: 0,
            diagnostics: Vec::new(),
        };
        while let Some(e) = self.next_event().await {
            match e {
                Event::Display { text, .. } => r.stdout.push_str(&text),
                Event::Diagnostic(d) => r.diagnostics.push(d),
                Event::Finished { finish, time } => (r.finish, r.time) = (finish, time),
            }
        }
        r
    }
}

/// A finished simulation, gathered by [`Simulation::wait`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimResult {
    /// All [`Event::Display`] text, concatenated.
    pub stdout: String,
    pub finish: Finish,
    /// Simulation time when it ended, in the design's time precision.
    pub time: u64,
    /// Compile-time warnings followed by run-time diagnostics.
    pub diagnostics: Vec<Diagnostic>,
}

/// Compile SystemVerilog source text with default [`Options`] and start simulating it.
pub async fn simulate(source: &str) -> Result<Simulation, Error> {
    simulate_with(source, &Options::default()).await
}

/// Compile SystemVerilog source text and start simulating it.
///
/// Compile errors are returned here. Once compiled, the simulation runs on its
/// own thread and reports through the returned [`Simulation`]. Compile-time
/// warnings are its first events.
pub async fn simulate_with(source: &str, opts: &Options) -> Result<Simulation, Error> {
    let (compiled_tx, mut compiled_rx) = channel::channel::<Result<(), Error>>();
    let (events_tx, events_rx) = channel::channel::<Event>();
    let (source, opts) = (source.to_string(), opts.clone());
    // One thread owns the source map and the design: it compiles, says how
    // that went, then simulates and streams events.
    std::thread::Builder::new()
        .stack_size(FRONT_END_STACK)
        .spawn(move || compile_and_run(&source, &opts, &compiled_tx, &events_tx))
        .expect("spawn simulation thread");
    match compiled_rx.recv().await {
        Some(Ok(())) => Ok(Simulation {
            events: events_rx,
            done: false,
        }),
        Some(Err(e)) => Err(e),
        None => Err(Error::Diagnostics(vec![Diagnostic {
            severity: Severity::Error,
            code: None,
            file: String::new(),
            line: 0,
            col: 0,
            message: "Internal error: the compiler thread stopped".into(),
            notes: Vec::new(),
        }])),
    }
}

/// The body of the simulation thread.
fn compile_and_run(
    source: &str,
    opts: &Options,
    compiled: &channel::Sender<Result<(), Error>>,
    events: &EventSender,
) {
    let sm = SourceMap::new();
    let (design, warnings) = match front_end(&sm, source, opts, Goal::Elaborate) {
        Ok((Output::Design(d), w)) => (d, w),
        Ok((Output::Text(_), _)) => unreachable!("elaboration produces a design"),
        Err(e) => {
            let _ = compiled.send(Err(e));
            return;
        }
    };
    let _ = compiled.send(Ok(()));
    for w in warnings {
        if events.send(Event::Diagnostic(w)).is_err() {
            return;
        }
    }
    let mut sim = sim::Simulator::new(&design);
    if let Some(n) = opts.max_steps {
        sim.max_steps = n;
    }
    for &var in &design.top_inputs {
        let v = &design.vars[var.0 as usize];
        if opts.clocks.iter().any(|c| c == v.name) {
            // Like Verilator's test bench: low until 10, then toggling every 5.
            sim.add_clock(var, 10, 5);
        } else if let Some((w, _, _)) =
            paragon_of_uncanny_reason::eval::bits_info(&design.types[v.ty.0 as usize])
        {
            sim.set_input(var, paragon_of_uncanny_reason::ir::Bits::zero(w));
        }
    }
    let deadline = opts.time_limit.map(|d| std::time::Instant::now() + d);
    let mut sink = EventSink {
        sm: &sm,
        events,
        deadline,
        checks: std::cell::Cell::new(0),
    };
    let end = sim.run(&mut sink);
    let finish = match end {
        sim::End::Finish => Finish::Finish,
        sim::End::Stop => Finish::Stop,
        sim::End::Fatal => Finish::Fatal,
        sim::End::Quiescent => Finish::Quiescent,
        sim::End::Hung => {
            let _ = events.send(Event::Diagnostic(Diagnostic {
                severity: Severity::Error,
                code: None,
                file: String::new(),
                line: 0,
                col: 0,
                message: "Simulation step limit reached (an infinite zero-delay loop?)".into(),
                notes: Vec::new(),
            }));
            Finish::Aborted
        }
        sim::End::Cancelled => {
            if sink.timed_out() {
                let _ = events.send(Event::Diagnostic(Diagnostic {
                    severity: Severity::Error,
                    code: None,
                    file: String::new(),
                    line: 0,
                    col: 0,
                    message: "Simulation time limit reached".into(),
                    notes: Vec::new(),
                }));
            }
            Finish::Aborted
        }
    };
    let _ = events.send(Event::Finished {
        finish,
        time: sim.time,
    });
}

/// Turns simulator output into [`Event`]s.
struct EventSink<'s> {
    sm: &'s SourceMap,
    events: &'s EventSender,
    deadline: Option<std::time::Instant>,
    /// Calls to `cancelled`, so the clock is read only now and then.
    checks: std::cell::Cell<u32>,
}

impl EventSink<'_> {
    fn timed_out(&self) -> bool {
        self.deadline
            .is_some_and(|d| std::time::Instant::now() >= d)
    }
}

impl sim::Sink for EventSink<'_> {
    fn display(&mut self, text: &str, time: u64) {
        let _ = self.events.send(Event::Display {
            text: text.to_string(),
            time,
        });
    }

    fn report(&mut self, severity: ReportSeverity, message: &str, at: &str, _time: u64) {
        let (file, line, col) = self
            .sm
            .locate(at)
            .map(|l| (l.name, l.line, l.col))
            .unwrap_or_default();
        let severity = match severity {
            ReportSeverity::Error => Severity::Error,
            _ => Severity::Warning,
        };
        let _ = self.events.send(Event::Diagnostic(Diagnostic {
            severity,
            code: None,
            file,
            line,
            col,
            message: message.to_string(),
            notes: Vec::new(),
        }));
    }

    fn cancelled(&self) -> bool {
        let n = self.checks.get().wrapping_add(1);
        self.checks.set(n);
        self.events.is_closed() || (n.is_multiple_of(1024) && self.timed_out())
    }
}

/// Compile SystemVerilog source text and return the elaborated IR as text,
/// with any warnings, for inspection (`paragon --ir`).
pub async fn lower_with(source: &str, opts: &Options) -> Result<(String, Vec<Diagnostic>), Error> {
    on_big_stack(|| {
        let sm = SourceMap::new();
        match front_end(&sm, source, opts, Goal::Elaborate)? {
            (Output::Design(d), w) => Ok((d.to_string(), w)),
            (Output::Text(t), w) => Ok((t, w)),
        }
    })
}

/// Preprocess SystemVerilog source text with default [`Options`].
pub async fn preprocess(source: &str) -> Result<Preprocessed, Error> {
    preprocess_with(source, &Options::default()).await
}

/// Preprocess SystemVerilog source text, as `-E` does.
pub async fn preprocess_with(source: &str, opts: &Options) -> Result<Preprocessed, Error> {
    let (text, warnings) = on_big_stack(|| {
        let sm = SourceMap::new();
        match front_end(&sm, source, opts, Goal::Preprocess)? {
            (Output::Text(t), w) => Ok((t, w)),
            (Output::Design(d), w) => Ok((d.to_string(), w)),
        }
    })?;
    Ok(Preprocessed { text, warnings })
}

/// Stack for the compiler front end. Real designs (and some Verilator tests)
/// nest deeply, and the parser and later passes recurse. This reserves
/// address space; memory is only committed as the stack is used.
const FRONT_END_STACK: usize = 256 << 20;

/// Run `f` on a thread with a [`FRONT_END_STACK`] stack and wait for it.
fn on_big_stack<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(FRONT_END_STACK)
            .spawn_scoped(s, f)
            .expect("spawn front-end thread")
            .join()
            .unwrap_or_else(|e| std::panic::resume_unwind(e))
    })
}

/// What [`front_end`] produces.
enum Output<'a> {
    /// Preprocessed text.
    Text(String),
    Design(paragon_of_uncanny_reason::ir::Design<'a>),
}

/// How far [`front_end`] goes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Goal {
    Preprocess,
    Elaborate,
}

/// Preprocess the source text and any extra files, then (for
/// [`Goal::Elaborate`]) lex, parse and elaborate them. Returns the `-E` text
/// or the IR text, and the warnings; or every diagnostic if there was an error.
fn front_end<'a>(
    sm: &'a SourceMap,
    source: &str,
    opts: &Options,
    goal: Goal,
) -> Result<(Output<'a>, Vec<Diagnostic>), Error> {
    let parse = goal == Goal::Elaborate;
    let lang = match opts.language.as_deref() {
        None => Lang::Sv2023,
        Some(l) => Lang::parse(l).ok_or_else(|| {
            Error::Diagnostics(vec![Diagnostic {
                severity: Severity::Error,
                code: None,
                file: String::new(),
                line: 0,
                col: 0,
                message: format!("Unknown language specified: {l}"),
                notes: Vec::new(),
            }])
        })?,
    };
    let fs = TextFs {
        name: &opts.source_name,
        text: source,
        disk: opts.read_includes_from_disk,
        extra: &opts.extra_files,
    };
    let pp_opts = pp::Options {
        include_dirs: opts.include_dirs.clone(),
        defines: opts.defines.clone(),
        ..pp::Options::default()
    };
    let mut pp = Preprocessor::new(sm, &fs, pp_opts);
    let mut text = String::new();
    let mut diags = Vec::new();
    let mut seen = 0;
    let mut trees = Vec::new();
    // Files to compile: the source, the extra files, then library files found
    // for modules nobody defines (as Verilator's -y does).
    let mut queue: Vec<String> = std::iter::once(&opts.source_name)
        .chain(&opts.extra_files)
        .cloned()
        .collect();
    let mut next = 0;
    loop {
        if next == queue.len() {
            if !parse || !opts.read_includes_from_disk {
                break;
            }
            let found = library_files(&trees, opts, &queue);
            if found.is_empty() {
                break;
            }
            queue.extend(found);
        }
        let file = queue[next].clone();
        next += 1;
        pp.process_file(&file);
        let tokens = pp.take_output();
        let new = &pp.diagnostics()[seen..];
        seen = pp.diagnostics().len();
        diags.extend(new.iter().map(|d| resolve(sm, d)));
        if !parse {
            text.push_str(&write(
                sm,
                &tokens,
                EmitOptions {
                    line_markers: opts.line_markers,
                },
            ));
            continue;
        }
        if new.iter().any(|d| d.severity == pp::Severity::Error) {
            continue;
        }
        let lexed = lex::lex(sm, &tokens, lang);
        let (tree, parse_diags) = parse::parse(&lexed);
        diags.extend(
            lexed
                .diags
                .iter()
                .chain(&parse_diags)
                .map(|d| resolve(sm, d)),
        );
        trees.push(tree);
    }
    if diags.iter().any(|d| d.severity == Severity::Error) {
        return Err(Error::Diagnostics(diags));
    }
    if parse {
        let eopts = elab::ElabOptions {
            top: opts.top.clone(),
            root_name: opts.root_name.clone(),
            params: opts.params.clone(),
            top_instance: opts.top_instance.clone(),
        };
        let (design, elab_diags) = elab::elaborate(sm, &trees, &eopts);
        diags.extend(elab_diags.iter().map(|d| resolve(sm, d)));
        if diags.iter().any(|d| d.severity == Severity::Error) {
            return Err(Error::Diagnostics(diags));
        }
        return Ok((Output::Design(design), diags));
    }
    Ok((Output::Text(text), diags))
}

/// Library files (`<dir>/<module><ext>`) for modules instantiated but not defined.
fn library_files(
    trees: &[paragon_of_uncanny_reason::ast::SourceText<'_>],
    opts: &Options,
    have: &[String],
) -> Vec<String> {
    let mut defined = std::collections::HashSet::new();
    let mut used = Vec::new();
    for t in trees {
        let (d, u) = elab::module_refs(t);
        defined.extend(d);
        used.extend(u);
    }
    let mut found = Vec::new();
    for m in used {
        if defined.contains(m) {
            continue;
        }
        'search: for dir in &opts.lib_dirs {
            for ext in &opts.lib_exts {
                let path = if dir == "." {
                    format!("{m}{ext}")
                } else {
                    format!("{dir}/{m}{ext}")
                };
                if !have.contains(&path)
                    && !found.contains(&path)
                    && std::path::Path::new(&path).is_file()
                {
                    found.push(path);
                    break 'search;
                }
            }
        }
    }
    found
}

/// The source text under its name, plus the disk for includes if allowed.
struct TextFs<'s> {
    name: &'s str,
    text: &'s str,
    disk: bool,
    /// Extra source files, always read from disk.
    extra: &'s [String],
}

impl FileSystem for TextFs<'_> {
    fn read(&self, path: &str) -> Option<String> {
        if path == self.name {
            Some(self.text.to_string())
        } else if self.disk || self.extra.iter().any(|e| e == path) {
            // Library files are found by the caller and read from disk too.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A simulation that sends `events` and then finishes as `end`.
    fn sim_with(events: Vec<Event>, end: (Finish, u64)) -> Simulation {
        start(Vec::new(), move |tx| {
            for e in events {
                if tx.send(e).is_err() {
                    break;
                }
            }
            end
        })
    }

    fn display(text: &str, time: u64) -> Event {
        Event::Display {
            text: text.into(),
            time,
        }
    }

    #[test]
    fn events_stream_then_stop_after_finished() {
        let fin = Event::Finished {
            finish: Finish::Finish,
            time: 20,
        };
        let mut sim = sim_with(
            vec![display("a\n", 0), display("b", 10)],
            (Finish::Finish, 20),
        );
        assert_eq!(block_on(sim.next_event()), Some(display("a\n", 0)));
        assert_eq!(sim.next_event_blocking(), Some(display("b", 10)));
        assert_eq!(block_on(sim.next_event()), Some(fin));
        assert_eq!(block_on(sim.next_event()), None);
    }

    #[test]
    fn wait_gathers_a_result() {
        let warn = Diagnostic {
            severity: Severity::Warning,
            code: None,
            file: "t.sv".into(),
            line: 3,
            col: 1,
            message: "w".into(),
            notes: vec![],
        };
        let runtime = Diagnostic {
            message: "r".into(),
            ..warn.clone()
        };
        let events = vec![
            display("hello\n", 0),
            Event::Diagnostic(runtime.clone()),
            display("bye\n", 5),
        ];
        let sim = start(vec![warn.clone()], move |tx| {
            events.into_iter().for_each(|e| tx.send(e).unwrap());
            (Finish::Stop, 5)
        });
        let r = block_on(sim.wait());
        assert_eq!(r.stdout, "hello\nbye\n");
        assert_eq!((r.finish, r.time), (Finish::Stop, 5));
        assert_eq!(r.diagnostics, vec![warn, runtime]);
    }

    #[test]
    fn a_panicking_simulation_ends_as_aborted() {
        let mut sim = start(Vec::new(), |tx| {
            tx.send(display("x", 0)).unwrap();
            panic!("simulator bug");
        });
        assert_eq!(sim.next_event_blocking(), Some(display("x", 0)));
        assert_eq!(
            sim.next_event_blocking(),
            Some(Event::Finished {
                finish: Finish::Aborted,
                time: 0
            })
        );
        assert_eq!(sim.next_event_blocking(), None);
    }

    #[test]
    fn dropping_the_handle_stops_the_simulation() {
        let (stopped_tx, stopped_rx) = std::sync::mpsc::channel();
        let mut sim = start(Vec::new(), move |tx| {
            let mut t = 0;
            while !tx.is_closed() {
                let _ = tx.send(display(".", t));
                t += 1;
                std::thread::yield_now();
            }
            stopped_tx.send(()).unwrap();
            (Finish::Quiescent, t)
        });
        assert!(matches!(
            sim.next_event_blocking(),
            Some(Event::Display { .. })
        ));
        drop(sim);
        stopped_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("simulation did not stop");
    }

    #[test]
    fn simulation_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Simulation>();
    }
}
