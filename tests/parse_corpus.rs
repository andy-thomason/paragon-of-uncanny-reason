//! Parse sweep (`docs/design/03-grammar.md` §18): preprocess, lex and parse
//! every CC0 source in the adjacent Verilator checkout, read in place.
//! Skipped if the checkout is missing; override the location with
//! `VERILATOR_TESTS=/path/to/test_regress/t`.
//!
//! Each file lands in one bucket: parsed, not yet supported (by construct), or
//! a syntax error. The test fails if the parsed count drops below a floor that
//! we raise as the parser grows. Run with `--nocapture` to see the report.

use paragon_of_uncanny_reason::diag::{NOT_YET, Severity};
use paragon_of_uncanny_reason::keywords::Lang;
use paragon_of_uncanny_reason::pp::{FileSystem, Options, Preprocessor};
use paragon_of_uncanny_reason::source::SourceMap;
use paragon_of_uncanny_reason::{lex, parse};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Raise this as the parser improves.
const MIN_PARSED: usize = 1600;

fn corpus_dir() -> PathBuf {
    std::env::var_os("VERILATOR_TESTS")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../verilator/test_regress/t")
        })
}

struct CorpusFs(PathBuf);

impl FileSystem for CorpusFs {
    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.0.join(path.strip_prefix("t/")?)).ok()
    }
}

#[test]
fn parse_cc0_corpus() {
    // Some tests nest hundreds of statements deep (t_if_deep.v); parse on a
    // big stack, as libparagon does.
    std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(sweep)
        .unwrap()
        .join()
        .unwrap();
}

fn sweep() {
    let dir = corpus_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!("skipping: no Verilator test corpus at {}", dir.display());
        return;
    };
    let mut files: Vec<_> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("v" | "sv")))
        .collect();
    files.sort();

    let fs = CorpusFs(dir.clone());
    let (mut total, mut parsed, mut pp_fail) = (0, 0, 0);
    let mut not_yet: BTreeMap<String, usize> = BTreeMap::new();
    let mut syntax: Vec<String> = Vec::new();
    for path in &files {
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        // Preprocessor-only tests (run with -E) are not meant to parse.
        if name.contains("_bad")
            || name.contains("unsup")
            || name.starts_with("t_preproc")
            || name.starts_with("t_pp_")
        {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        if !src.contains("SPDX-License-Identifier: CC0-1.0") {
            continue;
        }
        total += 1;
        if std::env::var_os("PARSE_TRACE").is_some() {
            eprintln!("parsing {name}");
        }
        let sm = SourceMap::new();
        let opts = Options {
            include_dirs: vec!["t".into(), ".".into()],
            ..Options::default()
        };
        let mut pp = Preprocessor::new(&sm, &fs, opts);
        pp.process_file(&format!("t/{name}"));
        if pp
            .diagnostics()
            .iter()
            .any(|d| d.severity == Severity::Error)
        {
            pp_fail += 1;
            continue;
        }
        let lexed = lex::lex(&sm, pp.output(), Lang::Sv2023);
        let (_, pdiags) = parse::parse(&lexed);
        let first = lexed
            .diags
            .iter()
            .chain(&pdiags)
            .find(|d| d.severity == Severity::Error);
        match first {
            None => parsed += 1,
            Some(d) if d.code == Some(NOT_YET) => {
                *not_yet
                    .entry(
                        d.message
                            .trim_start_matches("Not yet supported: ")
                            .to_string(),
                    )
                    .or_default() += 1;
            }
            Some(d) => {
                let at = sm
                    .locate(d.at)
                    .map(|l| format!("{}:{}", l.line, l.col))
                    .unwrap_or_default();
                syntax.push(format!("{name}:{at}: {}", d.message));
            }
        }
    }

    let gaps: usize = not_yet.values().sum();
    eprintln!(
        "parse sweep: {total} CC0 files; {parsed} parsed, {gaps} not yet supported, {} syntax errors, {pp_fail} preprocessor errors",
        syntax.len()
    );
    let mut by_count: Vec<_> = not_yet.iter().collect();
    by_count.sort_by(|a, b| b.1.cmp(a.1));
    eprintln!("not yet supported:");
    for (what, n) in by_count {
        eprintln!("  {n:5}  {what}");
    }
    eprintln!("syntax errors (first 60):");
    for s in syntax.iter().take(60) {
        eprintln!("  {s}");
    }
    if total > 0 {
        assert!(
            parsed >= MIN_PARSED,
            "only {parsed} files parsed (floor {MIN_PARSED})"
        );
    }
}
