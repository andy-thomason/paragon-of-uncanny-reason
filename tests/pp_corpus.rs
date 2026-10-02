//! Run the preprocessor over every CC0 source in the adjacent Verilator
//! checkout, read in place. Skipped if the checkout is missing; override the
//! location with `VERILATOR_TESTS=/path/to/test_regress/t`.
//!
//! Each file must preprocess without panicking. Errors are allowed in files
//! whose name says they are meant to fail (`_bad`, `_unsup`). Elsewhere the
//! only errors allowed are `` `error `` checks and missing includes, which come
//! from flags and include paths the Verilator test driver would supply.

use paragon_of_uncanny_reason::pp::{FileSystem, Options, Preprocessor, Severity};
use paragon_of_uncanny_reason::source::SourceMap;
use std::path::PathBuf;

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
        let name = path.strip_prefix("t/")?;
        std::fs::read_to_string(self.0.join(name)).ok()
    }
}

#[test]
fn preprocess_cc0_corpus() {
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
    let (mut done, mut unexpected) = (0, Vec::new());
    for path in &files {
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        if !src.contains("SPDX-License-Identifier: CC0-1.0") {
            continue;
        }
        done += 1;
        let sm = SourceMap::new();
        let opts = Options {
            include_dirs: vec!["t".into(), ".".into()],
            defines: ["TEST_OBJ_DIR", "TEST_DUMPFILE"]
                .map(|d| (d.into(), "obj".into()))
                .into(),
            ..Options::default()
        };
        let mut pp = Preprocessor::new(&sm, &fs, opts);
        pp.process_file(&format!("t/{name}"));
        let expected_to_fail = name.contains("_bad") || name.contains("unsup");
        for d in pp.diagnostics() {
            if d.severity == Severity::Error && !expected_to_fail {
                let at = sm
                    .locate(d.at)
                    .map(|l| format!("{}:{}", l.line, l.col))
                    .unwrap_or_default();
                let first = d.message.lines().next().unwrap_or("");
                unexpected.push(format!("{name}:{at}: {first}"));
            }
        }
    }
    eprintln!(
        "preprocessed {done} CC0 files; {} unexpected errors",
        unexpected.len()
    );
    for u in &unexpected {
        eprintln!("  {u}");
    }
    assert!(
        done > 3000,
        "only {done} CC0 files found in {}",
        dir.display()
    );
    // Without the driver's per-test flags, a few files hit their own `error
    // checks or miss an include directory. Anything else is a bug.
    let other: Vec<_> = unexpected
        .iter()
        .filter(|u| !(u.contains(": `error \"") || u.contains(": Cannot find include file:")))
        .collect();
    assert!(other.is_empty(), "unexpected errors:\n{other:#?}");
}
