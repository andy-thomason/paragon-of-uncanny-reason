//! Lex every CC0 source in the adjacent Verilator checkout.
//!
//! The files are read in place from `../verilator/test_regress/t` and are never
//! copied. The test is skipped if the checkout is missing. Override the location
//! with `VERILATOR_TESTS=/path/to/test_regress/t`.

use paragon_of_uncanny_reason::pp::{Token, lex};
use paragon_of_uncanny_reason::source::{line_col, offset_of};
use std::path::PathBuf;

fn corpus_dir() -> PathBuf {
    std::env::var_os("VERILATOR_TESTS")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../verilator/test_regress/t")
        })
}

/// Files that are expected to contain malformed tokens.
const EXPECT_ERRORS: &[&str] = &[
    "t_preproc_stringend_bad.v",
    "t_preproc_eof4_bad.v",
    "t_preproc_cmtend_bad.v",
    "t_preproc_eof1_bad.v",
    "t_preproc_eof_qqq_bad.v",
    "t_parse_eof_qqq_bad.v",
    "t_parse_eof_str_bad.v",
    "t_fuzz_eof_bad.v",
    "t_pp_line_bad.v",
];

#[test]
fn lex_cc0_corpus() {
    let dir = corpus_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!("skipping: no Verilator test corpus at {}", dir.display());
        return;
    };
    let mut files: Vec<_> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("v" | "sv" | "vh" | "svh" | "vlt")
            )
        })
        .collect();
    files.sort();

    let (mut lexed, mut skipped_licence, mut skipped_utf8, mut tokens) = (0, 0, 0, 0usize);
    let mut failures = Vec::new();
    for path in &files {
        let name = path.file_name().unwrap().to_str().unwrap();
        let Ok(src) = std::fs::read_to_string(path) else {
            skipped_utf8 += 1;
            continue;
        };
        if !src.contains("SPDX-License-Identifier: CC0-1.0") {
            skipped_licence += 1;
            continue;
        }
        lexed += 1;

        let mut pos = 0;
        let mut errors = Vec::<Token>::new();
        for t in lex(&src) {
            assert_eq!(offset_of(&src, t.text()), pos, "{name}: tokens do not tile");
            pos += t.text().len();
            tokens += 1;
            if t.is_error() {
                errors.push(t);
            }
        }
        assert_eq!(pos, src.len(), "{name}: tokens do not cover the file");

        let expected = EXPECT_ERRORS.contains(&name);
        if errors.is_empty() == expected {
            let at: Vec<_> = errors
                .iter()
                .map(|t| (line_col(&src, t.text()), *t))
                .collect();
            failures.push(format!("{name}: expected errors = {expected}, got {at:?}"));
        }
    }

    eprintln!(
        "lexed {lexed} CC0 files ({tokens} tokens); skipped {skipped_licence} non-CC0, {skipped_utf8} non-UTF-8"
    );
    assert!(
        lexed > 3000,
        "only {lexed} CC0 files found in {}",
        dir.display()
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
