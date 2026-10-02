//! Compare preprocessor output with Verilator's golden `-E` output.
//!
//! The inputs and goldens are CC0 files from the Verilator suite (see
//! `tests/fixtures/verilator_cc0/README.md`). Verilator's tests run from
//! `test_regress/` and name files `t/<name>`, so we map that directory onto
//! the fixtures.

use paragon_of_uncanny_reason::pp::emit::{EmitOptions, write};
use paragon_of_uncanny_reason::pp::{Diag, FileSystem, Options, Preprocessor};
use paragon_of_uncanny_reason::source::SourceMap;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/verilator_cc0");

struct FixtureFs;

impl FileSystem for FixtureFs {
    fn read(&self, path: &str) -> Option<String> {
        let name = path.strip_prefix("t/")?;
        std::fs::read_to_string(format!("{FIXTURES}/{name}")).ok()
    }
}

fn golden(name: &str) -> String {
    std::fs::read_to_string(format!("{FIXTURES}/{name}.out")).unwrap()
}

/// Run the files through the preprocessor as Verilator's driver would.
fn preprocess(files: &[&str], defines: &[&str], line_markers: bool) -> (String, Vec<String>) {
    let sm = SourceMap::new();
    let opts = Options {
        include_dirs: vec!["t".into(), ".".into()],
        defines: defines
            .iter()
            .map(|d| (d.to_string(), String::new()))
            .collect(),
        ..Options::default()
    };
    let mut pp = Preprocessor::new(&sm, &FixtureFs, opts);
    for f in files {
        assert!(pp.process_file(&format!("t/{f}")), "missing fixture {f}");
    }
    let out = write(&sm, pp.output(), EmitOptions { line_markers });
    let diags = pp
        .diagnostics()
        .iter()
        .map(|d: &Diag| render(&sm, d))
        .collect();
    (out, diags)
}

fn render(sm: &SourceMap, d: &Diag) -> String {
    match sm.locate(d.at) {
        Some(l) => format!("{}:{}:{}: {}", l.name, l.line, l.col, d.message),
        None => d.message.clone(),
    }
}

/// The output as words: `` `line `` markers removed and all whitespace,
/// including line breaks, treated as a separator. This checks content but not
/// layout; line numbering is checked separately by `preproc_line_numbers`.
fn words(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| !l.starts_with("`line "))
        .flat_map(|l| l.split_whitespace())
        .map(String::from)
        .collect()
}

/// Panic showing the first difference between two sequences, with context.
fn assert_lines_eq(name: &str, ours: &[String], want: &[String]) {
    if ours == want {
        return;
    }
    let i = ours.iter().zip(want).take_while(|(a, b)| a == b).count();
    let ctx = |v: &[String]| v[i.saturating_sub(6)..(i + 10).min(v.len())].join(" ");
    panic!(
        "{name}: output differs from golden at item {i} ({} vs {} items)\n  ours: {}\n  want: {}",
        ours.len(),
        want.len(),
        ctx(ours),
        ctx(want)
    );
}

fn assert_exact(name: &str, ours: &str, want: &str) {
    let ours: Vec<String> = ours.lines().map(String::from).collect();
    let want: Vec<String> = want.lines().map(String::from).collect();
    assert_lines_eq(name, &ours, &want);
}

// ---------------------------------------------------------------- -E -P: exact

#[test]
fn strify_join_exact() {
    let (out, diags) = preprocess(&["t_preproc_strify_join.v"], &[], false);
    assert!(diags.is_empty(), "{diags:?}");
    assert_exact(
        "t_preproc_strify_join",
        &out,
        &golden("t_preproc_strify_join"),
    );
}

#[test]
fn noline_exact() {
    let (out, diags) = preprocess(&["t_preproc_noline.v"], &[], false);
    assert!(diags.is_empty(), "{diags:?}");
    assert_exact("t_preproc_noline", &out, &golden("t_preproc_noline"));
}

#[test]
fn ttempty_exact() {
    let (out, diags) = preprocess(&["t_preproc_ttempty.v"], &[], false);
    assert!(diags.is_empty(), "{diags:?}");
    assert_exact("t_preproc_ttempty", &out, &golden("t_preproc_ttempty"));
}

#[test]
fn persist_exact() {
    let (out, diags) = preprocess(&["t_preproc_persist.v", "t_preproc_persist2.v"], &[], false);
    assert!(diags.is_empty(), "{diags:?}");
    assert_exact("t_preproc_persist", &out, &golden("t_preproc_persist"));
}

/// Known deviation: Verilator's golden omits the branch for
/// `` `elsif ( ONE && !( ZERO && ONE ) ) ``, which is true under IEEE 1800-2023
/// 22.6. We keep the standard result and remove that one line before comparing.
/// See `docs/design/03-grammar.md` §3.4.
#[test]
fn ifexpr_exact_except_known_deviation() {
    let (out, diags) = preprocess(&["t_preproc_ifexpr.v"], &[], false);
    assert!(diags.is_empty(), "{diags:?}");
    let deviation = "  \"ok  ( ONE && !( ZERO && ONE ) )\"\n";
    assert!(out.contains(deviation));
    assert_exact(
        "t_preproc_ifexpr",
        &out.replacen(deviation, "", 1),
        &golden("t_preproc_ifexpr"),
    );
}

// ---------------------------------------------------------------- -E: same words, any layout

#[test]
fn def09_words() {
    let (out, diags) = preprocess(&["t_preproc_def09.v"], &[], true);
    assert!(diags.is_empty(), "{diags:?}");
    assert_lines_eq(
        "t_preproc_def09",
        &words(&out),
        &words(&golden("t_preproc_def09")),
    );
}

/// Known deviation (`bug202` in `t_preproc.v`): a `` `define `` whose name
/// follows a multi-line block comment ending in a backslash. Verilator emits a
/// stray `\` and does not substitute the formal (`def i`). We define the macro
/// as the LRM reads, giving `def foo`. Patch that one spot before comparing.
#[test]
fn preproc_words_except_known_deviation() {
    let (out, diags) = preprocess(&["t_preproc.v"], &["DEF_A0", "PREDEF_COMMAND_LINE"], true);
    assert!(diags.is_empty(), "{diags:?}");
    let mut ours = words(&out);
    let at = ours
        .windows(2)
        .position(|w| w == ["def", "foo"])
        .expect("bug202 expansion");
    ours.splice(at..at + 2, ["\\", "def", "i"].map(String::from));
    assert_lines_eq("t_preproc", &ours, &words(&golden("t_preproc")));
}

/// The check `t_preproc` makes: every `Line_Preproc_Check N` in the output
/// must sit on source line N according to our own `` `line `` markers, and
/// `__LINE__` must have expanded to N.
#[test]
fn preproc_line_numbers() {
    let src = std::fs::read_to_string(format!("{FIXTURES}/t_preproc.v")).unwrap();
    let mut expected: Vec<usize> = src
        .lines()
        .enumerate()
        .filter(|(_, l)| l.starts_with("Line_Preproc_Check"))
        .map(|(i, _)| i + 1)
        .collect();
    expected.reverse();

    let (out, _) = preprocess(&["t_preproc.v"], &["DEF_A0", "PREDEF_COMMAND_LINE"], true);
    let mut lineno = 0usize;
    let mut file = String::new();
    for line in out.lines() {
        lineno += 1;
        if let Some(rest) = line.strip_prefix("`line ") {
            let mut parts = rest.split_whitespace();
            lineno = parts.next().unwrap().parse::<usize>().unwrap() - 1;
            file = parts.next().unwrap().to_string();
            continue;
        }
        if let Some(n) = line.strip_prefix("Line_Preproc_Check ") {
            assert_eq!(file, "\"t/t_preproc.v\"");
            let want = expected.pop().expect("extra Line_Preproc_Check");
            assert_eq!(n.trim().parse::<usize>().unwrap(), want, "__LINE__ value");
            assert_eq!(lineno, want, "`line tracking at Line_Preproc_Check {want}");
        }
    }
    assert!(
        expected.is_empty(),
        "missing Line_Preproc_Check lines: {expected:?}"
    );
}
