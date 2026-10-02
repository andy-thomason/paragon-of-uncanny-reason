//! Preprocessor lexer tests using CC0 fixtures from the Verilator test suite.

use paragon_of_uncanny_reason::pp::{Token, Token::*, lex};
use paragon_of_uncanny_reason::source::{line_col, line_col_end, offset_of};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/verilator_cc0");

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("{FIXTURES}/{name}")).unwrap()
}

/// Tokens must tile the source exactly: each starts where the last ended.
fn assert_lossless(src: &str) -> Vec<Token<'_>> {
    let toks: Vec<_> = lex(src).collect();
    let mut pos = 0;
    for t in &toks {
        assert!(!t.text().is_empty(), "empty token {t:?}");
        assert_eq!(offset_of(src, t.text()), pos, "gap or overlap before {t:?}");
        pos += t.text().len();
    }
    assert_eq!(pos, src.len());
    toks
}

/// Non-trivia tokens on a 1-based line.
fn line_tokens<'a>(src: &'a str, toks: &[Token<'a>], line: usize) -> Vec<Token<'a>> {
    toks.iter()
        .copied()
        .filter(|t| !t.is_trivia() && line_col(src, t.text()).0 == line)
        .collect()
}

#[test]
fn good_fixtures_have_no_error_tokens() {
    for name in [
        "t_preproc.v",
        "t_preproc_ifexpr.v",
        "t_preproc_strify_join.v",
        "t_preproc_def09.v",
    ] {
        let src = fixture(name);
        let toks = assert_lossless(&src);
        let bad: Vec<_> = toks.iter().filter(|t| t.is_error()).collect();
        assert!(bad.is_empty(), "{name}: {bad:?}");
    }
}

#[test]
fn ifdef_expression() {
    let src = fixture("t_preproc_ifexpr.v");
    let toks = assert_lossless(&src);
    assert_eq!(
        line_tokens(&src, &toks, 43),
        vec![
            Directive("`ifdef"),
            Punct("("),
            Ident("ONE"),
            Punct("&"),
            Punct("&"),
            Ident("ONE"),
            Punct("&"),
            Punct("&"),
            Ident("ONE"),
            Punct(")"),
            Newline("\n"),
        ]
    );
    assert_eq!(
        line_tokens(&src, &toks, 44),
        vec![Str("\"ok  ( ONE && ONE && ONE )\""), Newline("\n")]
    );
}

#[test]
fn stringify_with_paste() {
    // `define STRIFY `"`FOO``-```BAR``-```QUX```"
    let src = fixture("t_preproc_strify_join.v");
    let toks = assert_lossless(&src);
    assert_eq!(
        line_tokens(&src, &toks, 10),
        vec![
            Directive("`define"),
            Ident("STRIFY"),
            MacroQuote("`\""),
            Directive("`FOO"),
            Paste("``"),
            Punct("-"),
            Paste("``"),
            Directive("`BAR"),
            Paste("``"),
            Punct("-"),
            Paste("``"),
            Directive("`QUX"),
            Paste("``"),
            MacroQuote("`\""),
            Newline("\n"),
        ]
    );
}

#[test]
fn escaped_identifier_pasted_in_macro_body() {
    // cell \inv_``out <$typeof(out)> (.a(<in>), .o(<out>));	\
    let src = fixture("t_preproc.v");
    let toks = assert_lossless(&src);
    let line = line_tokens(&src, &toks, 331);
    assert_eq!(
        &line[..6],
        &[
            Ident("cell"),
            EscapedIdent("\\inv_"),
            Paste("``"),
            Ident("out"),
            Punct("<"),
            SystemIdent("$typeof"),
        ]
    );
    assert_eq!(line.last(), Some(&LineContinuation("\\\n")));
}

#[test]
fn line_comment_continues_define() {
    // `define CMT5 // CMT NOT \
    //   also in  // BUT TEXT IS \
    //   also3  // CMT NOT
    let src = fixture("t_preproc.v");
    let toks = assert_lossless(&src);
    assert_eq!(
        line_tokens(&src, &toks, 362),
        vec![
            Directive("`define"),
            Ident("CMT5"),
            LineContinuation("\\\n")
        ]
    );
    assert_eq!(
        line_tokens(&src, &toks, 363),
        vec![Ident("also"), Ident("in"), LineContinuation("\\\n")]
    );
}

/// The single error token in a `_bad` fixture, with its start and end positions.
fn only_error(name: &str) -> (String, (usize, usize), (usize, usize)) {
    let src = fixture(name);
    let toks = assert_lossless(&src);
    let errs: Vec<_> = toks.iter().filter(|t| t.is_error()).collect();
    assert_eq!(errs.len(), 1, "{name}: {errs:?}");
    let t = *errs[0];
    (
        format!("{t:?}").split('(').next().unwrap().to_string(),
        line_col(&src, t.text()),
        line_col_end(&src, t.text()),
    )
}

#[test]
fn unterminated_string() {
    for name in ["t_preproc_stringend_bad.v", "t_preproc_eof4_bad.v"] {
        let (kind, start, end) = only_error(name);
        assert_eq!(kind, "UnterminatedStr", "{name}");
        assert_eq!(start, (7, 1), "{name}");
        assert_eq!(end, (7, 6), "{name}: stops before the newline");
    }
}

#[test]
fn unterminated_block_comment() {
    let (kind, start, end) = only_error("t_preproc_cmtend_bad.v");
    assert_eq!(
        (kind.as_str(), start, end),
        ("UnterminatedBlockComment", (7, 1), (9, 1))
    );
    let (kind, start, end) = only_error("t_preproc_eof1_bad.v");
    assert_eq!(
        (kind.as_str(), start, end),
        ("UnterminatedBlockComment", (7, 1), (8, 1))
    );
}

#[test]
fn unterminated_triple_string() {
    let (kind, start, end) = only_error("t_preproc_eof_qqq_bad.v");
    assert_eq!(
        (kind.as_str(), start, end),
        ("UnterminatedTripleStr", (7, 1), (8, 1))
    );
}
