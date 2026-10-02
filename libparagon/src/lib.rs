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
//! arrive as it runs. The pipeline exists only as far as the preprocessor:
//! a source that fails to preprocess returns [`Error::Diagnostics`], and one
//! that preprocesses cleanly returns [`Error::NotImplemented`] naming the next
//! stage. The signature will not change as later stages are added.

mod channel;
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
// Used by the simulator once it exists; exercised by the tests until then.
#[cfg_attr(not(test), allow(dead_code))]
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
