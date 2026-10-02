//! `paragon`: command-line front end.
//!
//! ```text
//! paragon [-E [-P] | --ir] [-DNAME[=VALUE]] [-GNAME=VALUE] [+incdir+DIR] [-IDIR]
//!         [--top-module NAME] [--clock NAME] [--root NAME] FILE
//! ```

use libparagon::{
    Error, Event, Finish, Options, block_on, lower_with, preprocess_with, simulate_with,
};
use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut opts = Options::default();
    let (mut preprocess_only, mut no_markers, mut ir, mut file) = (false, false, false, None);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--ir" {
            ir = true;
        } else if arg == "--top-module" || arg == "--top" {
            opts.top = args.next();
        } else if arg == "--clock" {
            // Drive a top-level input as a clock, as Verilator's test bench does.
            opts.clocks.extend(args.next());
        } else if arg == "--root" {
            opts.root_name = args.next();
        } else if arg == "-E" {
            preprocess_only = true;
        } else if arg == "-P" {
            no_markers = true;
        } else if let Some(def) = arg.strip_prefix("-D") {
            let (name, value) = def.split_once('=').unwrap_or((def, ""));
            opts.defines.push((name.into(), value.into()));
        } else if let Some(g) = arg.strip_prefix("-G") {
            let (name, value) = g.split_once('=').unwrap_or((g, "1"));
            opts.params.push((name.into(), value.into()));
        } else if let Some(dirs) = arg.strip_prefix("+incdir+") {
            opts.include_dirs
                .extend(dirs.split('+').filter(|d| !d.is_empty()).map(String::from));
        } else if let Some(dir) = arg.strip_prefix("-I") {
            opts.include_dirs.push(dir.into());
        } else if arg.starts_with('-') || arg.starts_with('+') {
            eprintln!("%Error: Unknown option: {arg}");
            return ExitCode::FAILURE;
        } else {
            file = Some(arg);
        }
    }
    let Some(file) = file else {
        eprintln!(
            "usage: paragon [-E [-P] | --ir] [-DNAME[=VALUE]] [-GNAME=VALUE] [+incdir+DIR] [-IDIR] [--top-module NAME] [--clock NAME] [--root NAME] FILE"
        );
        return ExitCode::FAILURE;
    };
    let source = match std::fs::read_to_string(&file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("%Error: Cannot read {file}: {e}");
            return ExitCode::FAILURE;
        }
    };
    opts.source_name = file;
    opts.line_markers = !no_markers;

    if ir {
        return match block_on(lower_with(&source, &opts)) {
            Ok((text, warnings)) => {
                print!("{text}");
                warnings.iter().for_each(|w| eprintln!("{w}"));
                ExitCode::SUCCESS
            }
            Err(e) => report(e),
        };
    }
    if preprocess_only {
        return match block_on(preprocess_with(&source, &opts)) {
            Ok(p) => {
                print!("{}", p.text);
                p.warnings.iter().for_each(|w| eprintln!("{w}"));
                ExitCode::SUCCESS
            }
            Err(e) => report(e),
        };
    }
    let mut sim = match block_on(simulate_with(&source, &opts)) {
        Ok(sim) => sim,
        Err(e) => return report(e),
    };
    // Print output as it happens.
    while let Some(event) = sim.next_event_blocking() {
        match event {
            Event::Display { text, .. } => {
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
            Event::Diagnostic(d) => eprintln!("{d}"),
            Event::Finished { finish, .. } => {
                return match finish {
                    Finish::Finish | Finish::Quiescent => ExitCode::SUCCESS,
                    Finish::Stop | Finish::Fatal | Finish::Aborted => ExitCode::FAILURE,
                };
            }
        }
    }
    ExitCode::FAILURE
}

fn report(e: Error) -> ExitCode {
    match e {
        Error::NotImplemented { .. } => eprintln!("%Error: {e}"),
        Error::Diagnostics(_) => eprintln!("{e}"),
    }
    ExitCode::FAILURE
}
