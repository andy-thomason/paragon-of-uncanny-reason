//! `paragon-runner`: run the Verilator regression tests through `libparagon`.
//!
//! Reads `tests/manifest.json` (made by `tools/manifest.py`), turns each test's
//! driver flags into `libparagon::Options`, runs it, and reports how far each
//! test got, by tier. This is our own runner. It does not use Verilator's
//! `driver.py`.
//!
//! ```text
//! paragon-runner [--manifest FILE] [--tier T1] [--filter TEXT] [--json OUT] [--list OUTCOME]
//! ```

use libparagon::{Diagnostic, Error, Options, Stage, block_on, preprocess_with, simulate_with};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// What happened to one test.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Outcome {
    /// Simulated and printed `*-* All Finished *-*`.
    Pass,
    /// Expected to fail; our first diagnostic is at the same place as the golden's.
    PassDiagnostic,
    /// Expected to fail and did, but there is no golden file to compare with.
    FailedAsExpected,
    /// Expected to fail; we failed at a different place.
    WrongDiagnostic { ours: String, want: String },
    /// Ran (or preprocessed) but the output differs from the golden file.
    WrongOutput(String),
    /// The test uses a construct Paragon doesn't support yet.
    NotYet(String),
    /// Our front end reported an error on a test that should compile.
    FrontEndError(String),
    /// Compiled as far as the pipeline goes; stopped at this unimplemented stage.
    Reached(Stage),
    /// Out of scope by decision (see docs/design/00-analysis-plan.md).
    Waived(String),
    /// The test's inputs could not be read or its flags understood.
    Setup(String),
}

impl Outcome {
    fn class(&self) -> &'static str {
        match self {
            Outcome::Pass => "pass",
            Outcome::PassDiagnostic => "pass-diagnostic",
            Outcome::FailedAsExpected => "failed-as-expected",
            Outcome::WrongDiagnostic { .. } => "wrong-diagnostic",
            Outcome::WrongOutput(_) => "wrong-output",
            Outcome::NotYet(_) => "not-yet",
            Outcome::FrontEndError(_) => "front-end-error",
            Outcome::Reached(Stage::Elaborate) => "reached-elaborate",
            Outcome::Reached(_) => "reached-other",
            Outcome::Waived(_) => "waived",
            Outcome::Setup(_) => "setup",
        }
    }

    fn detail(&self) -> String {
        match self {
            Outcome::WrongDiagnostic { ours, want } => format!("ours {ours} / want {want}"),
            Outcome::NotYet(s)
            | Outcome::FrontEndError(s)
            | Outcome::Waived(s)
            | Outcome::Setup(s)
            | Outcome::WrongOutput(s) => s.clone(),
            Outcome::Reached(s) => format!("{s:?}"),
            _ => String::new(),
        }
    }
}

struct Args {
    manifest: PathBuf,
    tier: Option<String>,
    filter: Option<String>,
    json: Option<PathBuf>,
    list: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        manifest: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tests/manifest.json"),
        tier: None,
        filter: None,
        json: None,
        list: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--manifest" => a.manifest = value()?.into(),
            "--tier" => a.tier = Some(value()?),
            "--filter" => a.filter = Some(value()?),
            "--json" => a.json = Some(value()?.into()),
            "--list" => a.list = Some(value()?),
            _ => return Err(format!("unknown argument {arg}")),
        }
    }
    Ok(a)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("paragon-runner: {e}");
            return ExitCode::FAILURE;
        }
    };
    let manifest: Value = match std::fs::read_to_string(&args.manifest) {
        Ok(text) => serde_json::from_str(&text).expect("manifest is valid JSON"),
        Err(e) => {
            eprintln!(
                "paragon-runner: cannot read {}: {e}\nGenerate it with: python3 tools/manifest.py",
                args.manifest.display()
            );
            return ExitCode::FAILURE;
        }
    };
    // Tests name files relative to test_regress/, so run from there.
    let t_dir = PathBuf::from(manifest["source"].as_str().unwrap());
    let root = t_dir.parent().unwrap().to_path_buf();
    std::env::set_current_dir(&root).expect("cd to test_regress");

    let mut results: Vec<(String, String, Outcome)> = Vec::new();
    for test in manifest["tests"].as_array().unwrap() {
        let name = test["name"].as_str().unwrap().to_string();
        let tier = test["tier"].as_str().unwrap().to_string();
        if args.tier.as_ref().is_some_and(|t| *t != tier)
            || args
                .filter
                .as_ref()
                .is_some_and(|f| !name.contains(f.as_str()))
        {
            continue;
        }
        let outcome = run_test(test, &root);
        results.push((name, tier, outcome));
    }

    report(&results, args.list.as_deref());
    if let Some(path) = &args.json {
        let rows: Vec<Value> = results
            .iter()
            .map(|(n, t, o)| serde_json::json!({ "name": n, "tier": t, "outcome": o.class(), "detail": o.detail() }))
            .collect();
        std::fs::write(path, serde_json::to_string_pretty(&rows).unwrap()).expect("write results");
    }
    ExitCode::SUCCESS
}

fn tags(test: &Value) -> Vec<&str> {
    test["tags"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn run_test(test: &Value, root: &Path) -> Outcome {
    let name = test["name"].as_str().unwrap();
    let tags = tags(test);
    let tier = test["tier"].as_str().unwrap();
    for (tag, why) in [
        ("harness-cpp", "C++ harness (decision 2)"),
        ("systemc", "SystemC (decision 4)"),
        ("dist", "checks Verilator's own sources"),
        ("skipped", "skipped by the driver"),
    ] {
        if tags.contains(&tag) {
            return Outcome::Waived(why.into());
        }
    }
    if tier == "T4" {
        return Outcome::Waived("checks Verilator internals".into());
    }
    // Drivers sometimes name the top file without its directory.
    let top = test["top"].as_str().unwrap();
    let top = if top.contains('/') {
        top.to_string()
    } else {
        format!("t/{top}")
    };
    let top = top.as_str();
    let Ok(source) = std::fs::read_to_string(root.join(top)) else {
        return Outcome::Setup(format!("cannot read {top}"));
    };
    let opts = match options(test, top, name) {
        Ok(o) => o,
        Err(e) => return Outcome::Setup(e),
    };
    let expect_fail = tags.contains(&"expect-fail");
    let flags = flags(test);
    if flags.contains(&"-E") {
        return preprocess_only(test, root, &source, opts, &flags, expect_fail);
    }
    let result = block_on(simulate_with(&source, &opts));
    let diags = match result {
        Ok(sim) => {
            let r = block_on(sim.wait());
            let finished = r.stdout.contains("*-* All Finished *-*");
            return if finished && !expect_fail {
                Outcome::Pass
            } else {
                Outcome::FrontEndError("simulation did not finish".into())
            };
        }
        Err(Error::NotImplemented { stage }) => return Outcome::Reached(stage),
        Err(Error::Diagnostics(d)) => d,
    };
    let first = diags
        .iter()
        .find(|d| d.severity == libparagon::Severity::Error)
        .unwrap_or(&diags[0]);
    if first.code.as_deref() == Some("NOTYET") {
        return Outcome::NotYet(
            first
                .message
                .trim_start_matches("Not yet supported: ")
                .to_string(),
        );
    }
    if !expect_fail {
        return Outcome::FrontEndError(format!("{}:{}: {}", first.line, first.col, first.message));
    }
    match test["golden"]
        .as_str()
        .and_then(|g| std::fs::read_to_string(root.join(g)).ok())
    {
        Some(golden) => match first_golden_location(&golden) {
            Some(want) => {
                let ours = location(first);
                if ours == want {
                    Outcome::PassDiagnostic
                } else {
                    Outcome::WrongDiagnostic { ours, want }
                }
            }
            None => Outcome::FailedAsExpected,
        },
        None => Outcome::FailedAsExpected,
    }
}

/// A `-E` test: preprocess and compare the text with the golden output.
fn preprocess_only(
    test: &Value,
    root: &Path,
    source: &str,
    mut opts: Options,
    flags: &[&str],
    expect_fail: bool,
) -> Outcome {
    for f in [
        "--dump-defines",
        "--preproc-comments",
        "--preproc-defines",
        "--preproc-resolve",
        "--pipe-filter",
    ] {
        if flags.contains(&f) {
            return Outcome::NotYet(f.to_string());
        }
    }
    opts.line_markers = !flags.contains(&"-P");
    match block_on(preprocess_with(source, &opts)) {
        Ok(p) => {
            let golden = test["golden"]
                .as_str()
                .and_then(|g| std::fs::read_to_string(root.join(g)).ok());
            match golden {
                Some(g) if g == p.text => Outcome::Pass,
                Some(_) if opts.line_markers => Outcome::WrongOutput(
                    "-E layout differs (known gap: plain -E line placement)".into(),
                ),
                Some(_) => Outcome::WrongOutput("-E -P output differs".into()),
                None if expect_fail => {
                    Outcome::FrontEndError("expected a preprocessor error".into())
                }
                None => Outcome::Reached(Stage::Parse),
            }
        }
        Err(Error::Diagnostics(d)) if expect_fail => {
            match test["golden"]
                .as_str()
                .and_then(|g| std::fs::read_to_string(root.join(g)).ok())
                .and_then(|g| first_golden_location(&g))
            {
                Some(want) if want == location(&d[0]) => Outcome::PassDiagnostic,
                Some(want) => Outcome::WrongDiagnostic {
                    ours: location(&d[0]),
                    want,
                },
                None => Outcome::FailedAsExpected,
            }
        }
        Err(Error::Diagnostics(d)) => {
            Outcome::FrontEndError(format!("{}:{}: {}", d[0].line, d[0].col, d[0].message))
        }
        Err(e) => Outcome::FrontEndError(e.to_string()),
    }
}

fn flags(test: &Value) -> Vec<&str> {
    test["flags"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// Verilator's `+<language>ext+<suffix>` spellings.
fn ext_language(lang: &str) -> Option<&'static str> {
    Some(match lang {
        "1364-1995" | "verilog1995" => "1364-1995",
        "1364-2001" | "verilog2001" => "1364-2001",
        "1364-2005" => "1364-2005",
        "1800-2005" => "1800-2005",
        "1800-2009" => "1800-2009",
        "1800-2012" => "1800-2012",
        "1800-2017" => "1800-2017",
        "1800-2023" | "systemverilog" => "1800-2023",
        _ => return None,
    })
}

fn location(d: &Diagnostic) -> String {
    format!("{}:{}:{}", d.file, d.line, d.col)
}

/// The `file:line:col` of the first `%Error` or `%Warning` in a golden file.
fn first_golden_location(golden: &str) -> Option<String> {
    golden.lines().find_map(|l| {
        let rest = l
            .strip_prefix("%Error")
            .or_else(|| l.strip_prefix("%Warning"))?;
        let rest = rest.split_once(": ")?.1;
        let mut parts = rest.splitn(4, ':');
        let (file, line, col) = (parts.next()?, parts.next()?, parts.next()?);
        (line.parse::<usize>().is_ok() && col.trim().parse::<usize>().is_ok())
            .then(|| format!("{file}:{line}:{}", col.trim()))
    })
}

/// Build options from the driver's flags, as Verilator would interpret them.
fn options(test: &Value, top: &str, name: &str) -> Result<Options, String> {
    let mut o = Options {
        source_name: top.to_string(),
        include_dirs: vec!["t".into(), ".".into()],
        defines: vec![
            ("TEST_OBJ_DIR".into(), format!("obj_vlt/{name}")),
            ("TEST_DUMPFILE".into(), format!("obj_vlt/{name}/simx.vcd")),
        ],
        ..Options::default()
    };
    let flags = flags(test);
    let mut it = flags.iter().copied().peekable();
    while let Some(f) = it.next() {
        if let Some(d) = f.strip_prefix("-D") {
            let (k, v) = d.split_once('=').unwrap_or((d, ""));
            o.defines.push((k.into(), v.into()));
        } else if let Some(defs) = f.strip_prefix("+define+") {
            for d in defs.split('+').filter(|d| !d.is_empty()) {
                let (k, v) = d.split_once('=').unwrap_or((d, ""));
                o.defines.push((k.into(), v.into()));
            }
        } else if let Some(dirs) = f.strip_prefix("+incdir+") {
            o.include_dirs
                .extend(dirs.split('+').filter(|d| !d.is_empty()).map(String::from));
        } else if let Some(dir) = f.strip_prefix("-I") {
            o.include_dirs.push(dir.into());
        } else if f == "--language" || f == "--default-language" || f == "-language" {
            o.language = it.next().map(String::from);
        } else if let Some((l, _suffix)) = f
            .strip_prefix('+')
            .filter(|f| !f.starts_with("libext"))
            .and_then(|f| f.split_once("ext+"))
        {
            match ext_language(l) {
                Some(l) => o.language = Some(l.into()),
                None => return Err(format!("unknown language in {f}")),
            }
        } else if f.ends_with(".vlt") {
            return Err("control files (.vlt) are not supported yet".into());
        } else if !f.starts_with(['-', '+']) && (f.ends_with(".v") || f.ends_with(".sv")) {
            o.extra_files.push(f.to_string());
        } else if f == "-f" || f == "-F" {
            return Err("-f option files are not supported yet".into());
        }
    }
    Ok(o)
}

fn report(results: &[(String, String, Outcome)], list: Option<&str>) {
    let mut table: BTreeMap<&str, BTreeMap<&str, usize>> = BTreeMap::new();
    let mut classes: Vec<&str> = Vec::new();
    for (_, tier, o) in results {
        *table
            .entry(tier.as_str())
            .or_default()
            .entry(o.class())
            .or_default() += 1;
        if !classes.contains(&o.class()) {
            classes.push(o.class());
        }
    }
    classes.sort_by_key(|c| ORDER.iter().position(|o| o == c).unwrap_or(usize::MAX));
    print!("{:<12}", "tier");
    for c in &classes {
        print!("{:>20}", c);
    }
    println!("{:>8}", "total");
    for (tier, row) in &table {
        print!("{tier:<12}");
        for c in &classes {
            print!("{:>20}", row.get(c).copied().unwrap_or(0));
        }
        println!("{:>8}", row.values().sum::<usize>());
    }
    println!();

    let mut not_yet: BTreeMap<String, usize> = BTreeMap::new();
    let mut errors: BTreeMap<String, usize> = BTreeMap::new();
    for (_, _, o) in results {
        match o {
            Outcome::NotYet(w) => *not_yet.entry(w.clone()).or_default() += 1,
            Outcome::FrontEndError(e) | Outcome::Setup(e) => {
                let msg = e.split_once(": ").map_or(e.as_str(), |x| x.1);
                *errors.entry(msg.to_string()).or_default() += 1;
            }
            _ => {}
        }
    }
    for (title, map) in [
        ("Most common unsupported constructs", &not_yet),
        ("Most common front-end errors", &errors),
    ] {
        let mut v: Vec<_> = map.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1));
        println!("{title}:");
        for (what, n) in v.iter().take(15) {
            println!("  {n:5}  {what}");
        }
        println!();
    }
    if let Some(class) = list {
        println!("Tests with outcome {class}:");
        for (name, tier, o) in results.iter().filter(|r| r.2.class() == class) {
            println!("  {name:40} {tier:4} {}", o.detail());
        }
    }
}

const ORDER: &[&str] = &[
    "pass",
    "pass-diagnostic",
    "failed-as-expected",
    "reached-elaborate",
    "reached-other",
    "wrong-output",
    "wrong-diagnostic",
    "front-end-error",
    "not-yet",
    "setup",
    "waived",
];
