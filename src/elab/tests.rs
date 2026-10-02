use super::*;
use crate::keywords::Lang;
use crate::pp::{FileSystem, Options, Preprocessor};

struct OneFile(&'static str);

impl FileSystem for OneFile {
    fn read(&self, path: &str) -> Option<String> {
        (path == "t.sv").then(|| self.0.to_string())
    }
}

/// Run `src` through the whole front end and elaboration; return the printed
/// IR, or the first error as `line:col: message`.
fn elab(src: &'static str) -> Result<String, String> {
    let sm: &'static SourceMap = Box::leak(Box::new(SourceMap::new()));
    let fs = OneFile(src);
    let mut pp = Preprocessor::new(sm, &fs, Options::default());
    pp.process_file("t.sv");
    let lexed = crate::lex::lex(sm, pp.output(), Lang::Sv2023);
    let (tree, mut diags) = crate::parse::parse(&lexed);
    diags.extend(lexed.diags.iter().cloned());
    let tree: &'static ast::SourceText<'static> = Box::leak(Box::new(tree));
    let (design, ed) = elaborate(sm, std::slice::from_ref(tree), &ElabOptions::default());
    diags.extend(ed);
    match diags.iter().find(|d| d.severity == Severity::Error) {
        Some(d) => {
            let l = sm
                .locate(d.at)
                .map(|l| format!("{}:{}", l.line, l.col))
                .unwrap_or_default();
            Err(format!("{l}: {}", d.message))
        }
        None => Ok(design.to_string()),
    }
}

fn ok(src: &'static str) -> String {
    match elab(src) {
        Ok(ir) => ir,
        Err(e) => panic!("elaboration failed: {e}"),
    }
}

#[test]
fn doc_example() {
    let ir = ok(
        "module t(input clk);\n  integer cyc = 0;\n  always @(posedge clk) begin\n    if (cyc == 9) $finish;\n    cyc <= cyc + 1;\n  end\nendmodule\n",
    );
    println!("{ir}");
    assert!(ir.contains("var t.cyc: logic[32] signed = 32'h0"), "{ir}");
    assert!(ir.contains("suspend Edge[(t.clk, Pos)] -> bb1"), "{ir}");
    assert!(ir.contains("finish Finish"), "{ir}");
    assert!(ir.contains("nba_store t.cyc ="), "{ir}");
    assert!(ir.contains("jump bb0()"), "{ir}");
}

#[test]
fn widths_follow_the_lrm() {
    // a + b is 8 bits in an 8-bit context but 9 bits in a 9-bit one.
    let ir = ok(
        "module t; logic [7:0] a, b; logic [8:0] s; logic [7:0] n;\n  always_comb begin s = a + b; n = a + b; end\nendmodule\n",
    );
    println!("{ir}");
    assert!(
        ir.contains("resize.Zero"),
        "operands extended to the 9-bit context:\n{ir}"
    );
    assert!(ir.contains(": logic[9]"), "{ir}");
    assert!(ir.contains("suspend AnyChange[t.a, t.b] -> bb0"), "{ir}");
}

#[test]
fn signed_extension_only_in_signed_context() {
    let ir = ok(
        "module t; logic signed [3:0] a; logic [3:0] u; logic signed [7:0] s; logic [7:0] m;\n  initial begin s = a; m = a + u; end\nendmodule\n",
    );
    println!("{ir}");
    // s = a sign-extends; a + u is unsigned, so a is zero-extended.
    assert!(ir.contains("resize.Sign"), "{ir}");
    assert!(ir.contains("resize.Zero"), "{ir}");
}

#[test]
fn selects_use_declared_ranges() {
    let ir = ok(
        "module t; logic [7:4] v; logic [0:3] w; logic x, y;\n  initial begin x = v[5]; y = w[1]; end\nendmodule\n",
    );
    println!("{ir}");
    // v[5] is bit 1 of v; w[1] is bit 2 of w (ascending range).
    assert!(ir.contains("const 32'h4"), "{ir}");
    assert!(ir.contains("const 32'h3"), "{ir}");
    assert!(ir.matches("select").count() >= 2, "{ir}");
}

#[test]
fn parameters_and_generate() {
    let ir = ok(r#"
        module sub #(parameter W = 4) (input [W-1:0] a, output [W-1:0] y);
          assign y = ~a;
        endmodule
        module t;
          localparam N = 3;
          logic [7:0] a, y;
          sub #(.W(8)) u (.a(a), .y(y));
          genvar i;
          for (i = 0; i < N; i++) begin : g
            logic [i:0] r;
          end
        endmodule
    "#);
    println!("{ir}");
    // The ports alias the parent's nets, so sub drives t.y directly.
    assert!(ir.contains("store t.y = "), "{ir}");
    assert!(ir.contains("var t.g[0].r: logic"), "{ir}");
    assert!(ir.contains("var t.g[2].r: logic[3]"), "{ir}");
}

#[test]
fn functions_and_loops() {
    let ir = ok(r#"
        module t;
          function automatic int add(int a, int b); return a + b; endfunction
          int total;
          initial begin
            for (int i = 0; i < 4; i++) total = add(total, i);
            $display("total=%0d", total);
            $finish;
          end
        endmodule
    "#);
    println!("{ir}");
    assert!(ir.contains("func t.add:"), "{ir}");
    assert!(ir.contains("call add("), "{ir}");
    assert!(ir.contains("Display fmt#0"), "{ir}");
    assert!(ir.contains("fmt#0 = \"total=%0d\""), "{ir}");
}

#[test]
fn case_and_delays() {
    let ir = ok(
        "`timescale 1ns/1ps\nmodule t; logic [1:0] s; logic y;\n  initial begin #5 s = 1; casez (s) 2'b1?: y = 1; default: y = 0; endcase end\nendmodule\n",
    );
    println!("{ir}");
    assert!(
        ir.contains("const 64'h1388"),
        "5ns at 1ps precision is 5000 ticks:\n{ir}"
    );
    assert!(ir.contains("CaseZEq"), "{ir}");
}

#[test]
fn errors() {
    assert_eq!(
        elab("module t; initial x = 1; endmodule").unwrap_err(),
        "1:19: Can't find definition of variable: 'x'"
    );
    assert_eq!(
        elab("module t; sub u(); endmodule").unwrap_err(),
        "1:11: Cannot find file containing module: 'sub'"
    );
    let e = elab("module t; logic a; localparam P = a; endmodule").unwrap_err();
    assert!(e.contains("Expecting expression to be constant"), "{e}");
}

#[test]
fn parameters_may_refer_forward() {
    let ir = ok("module t; localparam A = B - 1; localparam B = 4; logic [A:0] v; endmodule");
    assert!(ir.contains("var t.v: logic[4]"), "{ir}");
}

#[test]
fn constant_functions() {
    let ir = ok(r#"
        module t;
          localparam W = width(5);
          logic [W-1:0] v;
          function integer width(input integer n);
            integer i;
            width = 0;
            for (i = n; i > 0; i = i >> 1) width = width + 1;
          endfunction
        endmodule
    "#);
    assert!(ir.contains("var t.v: logic[3]"), "{ir}");
}

#[test]
fn hierarchical_references() {
    let ir = ok(r#"
        module leaf; logic x; endmodule
        module t;
          for (genvar i = 0; i < 2; i++) begin : g leaf u (); end
          logic a, b;
          initial begin
            a = g[1].u.x;
            begin : blk
              logic inner;
              inner = 1;
            end
            b = blk.inner;
          end
        endmodule
    "#);
    assert!(ir.contains("load t.g[1].u.x"), "{ir}");
    assert!(ir.contains("load t.blk.inner"), "{ir}");
}

#[test]
fn upward_references_reach_instances_declared_later() {
    let ir = ok(r#"
        module reader; logic y; initial y = holder.v; endmodule
        module keeper; logic v; endmodule
        module t; reader r (); keeper holder (); endmodule
    "#);
    assert!(ir.contains("load t.holder.v"), "{ir}");
}

#[test]
fn implicit_nets_on_gates_and_ports() {
    let ir = ok("module t; logic a, b; and g (y, a, b); not n (z, y); endmodule");
    assert!(ir.contains("var t.y: logic net.Wire"), "{ir}");
    assert!(ir.contains("var t.z: logic net.Wire"), "{ir}");
}

#[test]
fn default_arguments_and_zero_replication() {
    let ir = ok(r#"
        module t;
          function automatic int f(int a, int b = 7); return a + b; endfunction
          int r; logic [3:0] c;
          initial begin r = f(1); c = {4'h5, {0{1'b1}}}; end
        endmodule
    "#);
    assert!(ir.contains("call f("), "{ir}");
}
