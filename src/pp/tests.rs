use super::emit::{EmitOptions, write};
use super::*;

struct MemFs(Vec<(&'static str, &'static str)>);

impl FileSystem for MemFs {
    fn read(&self, path: &str) -> Option<String> {
        self.0
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, t)| t.to_string())
    }
}

/// Preprocess `src` as `top.v` and return the `-P` output and the diagnostics.
fn run_with(files: Vec<(&'static str, &'static str)>, opts: Options) -> (String, Vec<String>) {
    let sm = SourceMap::new();
    let fs = MemFs(files);
    let mut pp = Preprocessor::new(&sm, &fs, opts);
    pp.process_file("top.v");
    let out = write(
        &sm,
        pp.output(),
        EmitOptions {
            line_markers: false,
        },
    );
    let diags = pp
        .diagnostics()
        .iter()
        .map(|d| {
            let loc = sm
                .locate(d.at)
                .map(|l| format!("{}:{}", l.line, l.col))
                .unwrap_or_default();
            format!("{loc}: {}", d.message)
        })
        .collect();
    (out, diags)
}

fn run(src: &'static str) -> String {
    let (out, diags) = run_with(vec![("top.v", src)], Options::default());
    assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
    out
}

fn run_diags(src: &'static str) -> Vec<String> {
    run_with(vec![("top.v", src)], Options::default()).1
}

#[test]
fn body_trailing_space_trimmed_before_block_comments_become_spaces() {
    assert_eq!(run("`define A x \t// c\n[`A]\n"), "[x]\n");
    assert_eq!(run("`define B x /* c */\n[`B]\n"), "[x  ]\n");
}

#[test]
fn line_of_line_keeps_numbering() {
    assert_eq!(
        run("a\n`line `__LINE__ \"n.v\" 0\n`__FILE__ `__LINE__\n"),
        "a\n\"n.v\" 3\n"
    );
}

#[test]
fn object_macro() {
    assert_eq!(run("`define A  one /* c */ two\n`A\n"), "one   two\n");
}

#[test]
fn comments_become_spaces() {
    assert_eq!(run("a/*x*/b // y\n"), "a b  \n");
}

#[test]
fn metacomments_are_kept_and_normalised() {
    assert_eq!(run("/* verilator  public */\n"), "/*verilator public*/\n");
    assert_eq!(
        run("x // verilator lint_off WIDTH\n"),
        "x /*verilator lint_off WIDTH*/\n"
    );
}

#[test]
fn function_macro_args_are_trimmed() {
    assert_eq!(
        run("`define F(a, b) a b LL a b\n`F( x , y )\n"),
        "x y LL x y\n"
    );
}

#[test]
fn nested_calls_split_on_raw_text() {
    // Arguments are split before expansion, so `a's comma does not split.
    let src = "`define a x,y\n`define B(p, q) [p|q]\n`B(`a,`a)\n";
    assert_eq!(run(src), "[x,y|x,y]\n");
}

#[test]
fn commas_inside_brackets_and_strings() {
    let src = "`define F(l, m) f(l;m)\n`F(\"a,b\", {c, d[1,2]})\n";
    assert_eq!(run(src), "f(\"a,b\";{c, d[1,2]})\n");
}

#[test]
fn args_may_span_lines() {
    let src = "`define F(a, b) <a|b>\n`F(one\n  ,\n  two)\nz\n";
    assert_eq!(run(src), "<one|two>\nz\n");
}

#[test]
fn space_before_paren_makes_object_macro() {
    assert_eq!(run("`define N (a,b)\n`N(a,b)\n"), "(a,b)(a,b)\n");
}

#[test]
fn formals_not_substituted_in_strings() {
    assert_eq!(
        run("`define S(n) \"foo n\" n\n`S(bar)\n"),
        "\"foo n\" bar\n"
    );
}

#[test]
fn macros_not_expanded_in_strings() {
    assert_eq!(run("`define X y\n\"`X\"\n"), "\"`X\"\n");
}

#[test]
fn stringification_expands_macros() {
    let src = "`define A aa\n`define Q(x) `\"x: `\\`\"`A`\\`\"`\"\n`Q(left side)\n";
    assert_eq!(run(src), "\"left side: \\\"aa\\\"\"\n");
}

#[test]
fn stringification_keeps_undefined_macros() {
    assert_eq!(run("`define S(t) `\"t`\"\n`S(`NOPE)\n"), "\"`NOPE\"\n");
}

#[test]
fn standalone_stringification() {
    assert_eq!(run("`\"text`\"\n"), "\"text\"\n");
}

#[test]
fn paste_joins_and_keeps_following_space() {
    assert_eq!(run("`define F(f) f``_x f`` y\n`F(a)\n"), "a_x a y\n");
}

#[test]
fn paste_expands_macro_operands_first() {
    // The joined result is an identifier, so a macro of that name is not used.
    let src = "`define T s\n`define s_f other\n`define J `T``_f\n`J\n";
    assert_eq!(run(src), "s_f\n");
}

#[test]
fn paste_with_undefined_macro_is_rescanned() {
    let src = "`define QA_b zzz\n`define Q1 `QA``_b\n`Q1\n";
    assert_eq!(run(src), "zzz\n");
}

#[test]
fn rescan_picks_up_following_arguments() {
    let src = "`define R_2(d) d d\n`define CAT(a, b) a``b\n`define RC(n, d) `CAT(`R_, n)(d)\n`RC(2, hi)\n";
    assert_eq!(run(src), "hi hi\n");
}

#[test]
fn define_name_built_by_paste() {
    let src = "`define SOME some\n`define X_```SOME\n`ifdef X_some\nyes\n`endif\n";
    assert_eq!(run(src), "yes\n");
}

#[test]
fn multiline_body_keeps_newlines() {
    assert_eq!(run("`define M a \\\n  b\n`M c\n"), "a \n  b c\n");
}

#[test]
fn define_inside_expansion() {
    // The newline must survive in DEF's body to end the inner define.
    let src = "`define DEF(d) d \\\n\n`DEF(`define X 1)\n`X\n";
    assert_eq!(run(src), "1\n");
}

#[test]
fn nested_define_bodies_substitute_formals() {
    let src = "`define MK(n) \\\n `define d_``n is n \\\n\n`MK(foo)\n`d_foo\n";
    assert_eq!(run(src), "is foo\n");
}

#[test]
fn escaped_identifier_formals_substitute_but_do_not_expand() {
    let src = "`define FOO bar\n`define E(n) \\n \\\n\n`E(`FOO)\n";
    assert_eq!(run(src), "\\`FOO \n");
}

#[test]
fn escaped_identifier_outside_macros_is_literal() {
    assert_eq!(
        run("`define define_x 1\nNot a \\`define_x\n"),
        "Not a \\`define_x\n"
    );
}

#[test]
fn default_arguments() {
    let src = "`define D(a, b = 2, c = 3) a b c\n`D(1)\n`D(1, , 9)\n";
    assert_eq!(run(src), "1 2 3\n1 2 9\n");
}

#[test]
fn empty_call_of_zero_arg_macro() {
    assert_eq!(run("`define N() np\n`N()\n`N( )\n"), "np\nnp\n");
}

#[test]
fn file_and_line() {
    assert_eq!(run("\n`__FILE__ `__LINE__\n"), "\"top.v\" 2\n");
}

#[test]
fn line_inside_macro_uses_the_use_site() {
    assert_eq!(run("`define L `__LINE__\n\n\n`L\n"), "4\n");
}

#[test]
fn ifdef_chain() {
    let src = "`define B\n`ifdef A\na\n`elsif B\nb\n`else\nc\n`endif\n";
    assert_eq!(run(src), "b\n");
}

#[test]
fn ifdef_skips_nested_regions() {
    let src = "`ifdef NO\n`ifdef ALSO_NO\n`else\nx\n`endif\n`define Y\n`else\nz\n`endif\n`ifdef Y\nbad\n`endif\n";
    assert_eq!(run(src), "z\n");
}

#[test]
fn ifdef_expressions() {
    let src = "`define ONE\n\
               `ifdef ( ONE && !ZERO )\na\n`endif\n\
               `ifdef (ZERO || ONE)\nb\n`endif\n\
               `ifndef ( ONE -> ZERO )\nc\n`endif\n\
               `ifdef ( ZERO <-> ZERO )\nd\n`endif\n";
    assert_eq!(run(src), "a\nb\nc\nd\n");
}

#[test]
fn ifdef_operators_are_left_to_right() {
    // (ONE || ZERO) && ZERO is false; normal precedence would make it true.
    assert_eq!(
        run("`define ONE\n`ifdef ( ONE || ZERO && ZERO )\nbad\n`endif\n"),
        ""
    );
}

#[test]
fn ifdef_of_macro_call() {
    let src = "`define DEFD\n`define ID(x) x\n`ifdef `ID(DEFD)\nyes\n`endif\n";
    assert_eq!(run(src), "yes\n");
}

#[test]
fn undef_and_undefineall() {
    let src = "`define A\n`define B\n`undef A\n`ifdef A\nbad\n`endif\n`undefineall\n\
               `ifdef B\nbad\n`endif\n`ifdef VERILATOR\nkept\n`endif\n";
    assert_eq!(run(src), "kept\n");
}

#[test]
fn command_line_defines_survive_undefineall() {
    let opts = Options {
        defines: vec![("CMD".into(), "c".into())],
        ..Options::default()
    };
    let (out, _) = run_with(vec![("top.v", "`undefineall\n`CMD\n")], opts);
    assert_eq!(out, "c\n");
}

#[test]
fn include_with_dirs_and_macro_name() {
    let files = vec![
        (
            "top.v",
            "`define INC \"b.vh\"\n`include \"a.vh\"\n`include `INC\nend\n",
        ),
        ("inc/a.vh", "in a\n"),
        ("inc/b.vh", "in b\n"),
    ];
    let opts = Options {
        include_dirs: vec!["inc".into()],
        ..Options::default()
    };
    let (out, diags) = run_with(files, opts);
    assert!(diags.is_empty(), "{diags:?}");
    assert_eq!(out, "in a\nin b\nend\n");
}

#[test]
fn include_errors() {
    let (_, d) = run_with(
        vec![("top.v", "`include \"nope.vh\"\n")],
        Options::default(),
    );
    assert!(
        d[0].starts_with("1:1: Cannot find include file: 'nope.vh'"),
        "{d:?}"
    );
    let (_, d) = run_with(vec![("top.v", "`include \"top.v\"\n")], Options::default());
    assert_eq!(d, vec!["1:1: Recursive inclusion of file: top.v"]);
}

#[test]
fn undefined_macros_pass_through() {
    assert_eq!(
        run("`nope x\n`timescale 1ns/1ps\n"),
        "`nope x\n`timescale 1ns/1ps\n"
    );
}

#[test]
fn errors_for_bad_conditionals() {
    assert_eq!(
        run_diags("`ifdef A\n"),
        vec!["2:1: `ifdef not terminated at EOF"]
    );
    assert_eq!(
        run_diags("`endif\n"),
        vec!["1:1: `endif with no matching `if"]
    );
    assert_eq!(
        run_diags("`elsif X\n"),
        vec!["1:1: `elsif with no matching `if"]
    );
}

#[test]
fn errors_for_bad_defines() {
    assert_eq!(
        run_diags("`define define 1\n"),
        vec!["1:9: Attempting to define built-in directive: '`define' (IEEE 1800-2023 22.5.1)"]
    );
    assert_eq!(
        run_diags("`define A(x)\n`A(1,2)\n"),
        vec!["2:1: Define passed too many arguments: A"]
    );
    assert_eq!(
        run_diags("`define A(x)\n`A\n"),
        vec!["2:1: EOF in define argument list"]
    );
    assert_eq!(
        run_diags("`define F(a,\n"),
        vec!["1:1: Unterminated ( in define formal arguments."]
    );
}

#[test]
fn recursion_is_reported() {
    let d = run_diags("`define R `R\n`R\n");
    assert_eq!(d, vec!["2:1: Recursive `define substitution: `R"]);
    let d = run_diags("`define E fun `E\n`E\n");
    assert_eq!(d, vec!["2:1: Recursive `define substitution: `E"]);
}

#[test]
fn redefinition_warns_unless_lint_off() {
    let d = run_diags(
        "`define D a\n`define D a\n`define D b\n// verilator lint_off REDEFMACRO\n`define D c\n",
    );
    assert_eq!(
        d,
        vec!["3:9: Redefining existing define: 'D', with different value: 'b'"]
    );
}

#[test]
fn preproczero_warning() {
    let d = run_diags("`define Z 0\n`ifdef ( Z )\n`endif\n");
    assert_eq!(
        d,
        vec!["2:10: Preprocessor expression evaluates define with 0: 'Z' with value '0'"]
    );
}

#[test]
fn line_directive_renumbers() {
    let sm = SourceMap::new();
    let fs = MemFs(vec![(
        "top.v",
        "`line 100 \"other.v\" 0\n`__FILE__ `__LINE__\n",
    )]);
    let mut pp = Preprocessor::new(&sm, &fs, Options::default());
    pp.process_file("top.v");
    let out = write(
        &sm,
        pp.output(),
        EmitOptions {
            line_markers: false,
        },
    );
    assert_eq!(out, "\"other.v\" 100\n");
}

#[test]
fn line_directive_names_with_spaces_and_escapes() {
    let src = "`line 7 \"a b.v\" 0\n`__FILE__\n`line 9 \"C:\\\\x.v\" 0\n`__FILE__ `__LINE__\n";
    assert_eq!(run(src), "\"a b.v\"\n\"C:\\\\x.v\" 9\n");
}

#[test]
fn error_takes_one_string() {
    let src = "`ifndef X `error \"no X\" `endif\nafter\n";
    let (out, d) = run_with(vec![("top.v", src)], Options::default());
    assert_eq!(d, vec!["1:11: `error \"no X\""]);
    assert_eq!(out, "after\n");
}

#[test]
fn line_markers_start_with_top_file_when_output_begins_in_include() {
    let sm = SourceMap::new();
    let fs = MemFs(vec![("top.v", "`include \"i.vh\"\nb\n"), ("i.vh", "inc\n")]);
    let mut pp = Preprocessor::new(&sm, &fs, Options::default());
    pp.process_file("top.v");
    let out = write(&sm, pp.output(), EmitOptions { line_markers: true });
    assert_eq!(
        out,
        "`line 1 \"top.v\" 1\n`line 1 \"i.vh\" 1\ninc\n`line 1 \"top.v\" 2\n\nb\n"
    );
}

#[test]
fn line_markers() {
    let sm = SourceMap::new();
    let fs = MemFs(vec![
        (
            "top.v",
            "a\n`define M x \\\n  y\n`include \"i.vh\"\nb `M\nc\n",
        ),
        ("i.vh", "inc\n"),
    ]);
    let mut pp = Preprocessor::new(&sm, &fs, Options::default());
    pp.process_file("top.v");
    let out = write(&sm, pp.output(), EmitOptions { line_markers: true });
    assert_eq!(
        out,
        "`line 1 \"top.v\" 1\na\n`line 3 \"top.v\" 0\n\n\
         `line 1 \"i.vh\" 1\ninc\n\
         `line 4 \"top.v\" 2\n\nb x \n`line 5 \"top.v\" 0\n  y\nc\n"
    );
}
