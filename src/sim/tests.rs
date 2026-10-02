use super::*;
use crate::ast;
use crate::elab::{ElabOptions, elaborate};
use crate::keywords::Lang;
use crate::pp::{FileSystem, Options, Preprocessor};
use crate::source::SourceMap;

struct OneFile(&'static str);

impl FileSystem for OneFile {
    fn read(&self, path: &str) -> Option<String> {
        (path == "t.sv").then(|| self.0.to_string())
    }
}

#[derive(Default)]
struct Collect {
    out: String,
    reports: Vec<String>,
}

impl Sink for Collect {
    fn display(&mut self, text: &str, _time: u64) {
        self.out.push_str(text);
    }
    fn report(&mut self, _s: ReportSeverity, message: &str, _at: &str, _time: u64) {
        self.reports.push(message.to_string());
    }
}

/// Compile and simulate `src`; return stdout and how it ended.
fn sim(src: &'static str) -> (String, End) {
    let sm: &'static SourceMap = Box::leak(Box::new(SourceMap::new()));
    let fs = OneFile(src);
    let mut pp = Preprocessor::new(sm, &fs, Options::default());
    pp.process_file("t.sv");
    let lexed = crate::lex::lex(sm, pp.output(), Lang::Sv2023);
    let (tree, diags) = crate::parse::parse(&lexed);
    assert!(diags.is_empty() && lexed.diags.is_empty(), "{diags:?}");
    let tree: &'static ast::SourceText<'static> = Box::leak(Box::new(tree));
    let (design, ed) = elaborate(sm, std::slice::from_ref(tree), &ElabOptions::default());
    let errs: Vec<_> = ed
        .iter()
        .filter(|d| d.severity == crate::diag::Severity::Error)
        .map(|d| d.message.clone())
        .collect();
    assert!(errs.is_empty(), "{errs:?}");
    let mut s = Simulator::new(&design);
    s.max_steps = 10_000_000;
    let mut c = Collect::default();
    let end = s.run(&mut c);
    (c.out, end)
}

#[test]
fn hello() {
    let (out, end) =
        sim("module t; initial begin $display(\"hello %0d\", 42); $finish; end endmodule");
    assert_eq!(out, "hello 42\n");
    assert_eq!(end, End::Finish);
}

#[test]
fn counter_with_clock_and_nba() {
    let (out, end) = sim(r#"
        module t;
          logic clk = 0;
          integer cyc = 0;
          always #5 clk = ~clk;
          always @(posedge clk) begin
            cyc <= cyc + 1;
            if (cyc == 3) begin
              $write("*-* All Finished *-* at %0t\n", $time);
              $finish;
            end
          end
        endmodule
    "#);
    // Edges at 5, 15, 25, 35 ps; cyc is 3 at the fourth.
    assert_eq!(out, "*-* All Finished *-* at 35\n");
    assert_eq!(end, End::Finish);
}

#[test]
fn nonblocking_swap() {
    let (out, _) = sim(r#"
        module t;
          logic [3:0] a = 1, b = 2;
          initial begin
            a <= b; b <= a;
            #1 $display("%0d %0d", a, b);
          end
        endmodule
    "#);
    assert_eq!(out, "2 1\n");
}

#[test]
fn combinational_and_continuous() {
    let (out, _) = sim(r#"
        module t;
          logic [7:0] a, b, s;
          wire [7:0] w = a ^ b;
          always_comb s = a + b;
          initial begin
            a = 3; b = 5;
            #1 $display("%0d %0d", s, w);
            a = 10;
            #1 $display("%0d %0d", s, w);
          end
        endmodule
    "#);
    assert_eq!(out, "8 6\n15 15\n");
}

#[test]
fn hierarchy_functions_and_loops() {
    let (out, _) = sim(r#"
        module add #(parameter W = 8) (input [W-1:0] a, b, output [W-1:0] y);
          assign y = a + b;
        endmodule
        module t;
          logic [7:0] x, y, z;
          add #(8) u (.a(x), .b(y), .y(z));
          function automatic int sq(int v); return v * v; endfunction
          initial begin
            int total = 0;
            for (int i = 0; i < 4; i++) total += sq(i);
            x = 20; y = 22;
            #1 $display("%0d %0d", total, z);
            $finish;
          end
        endmodule
    "#);
    assert_eq!(out, "14 42\n");
}

#[test]
fn case_and_formats() {
    let (out, _) = sim(r#"
        module t;
          logic [3:0] v = 4'b1010;
          logic [7:0] x;
          initial begin
            casez (v) 4'b1?1?: $display("match"); default: $display("no"); endcase
            $display("%b %h %o %d", v, v, v, v);
            $display("[%5d] [%-3d]", 7, 7);
            $display("%0d", x);
            $finish;
          end
        endmodule
    "#);
    assert_eq!(out, "match\n1010 a 12 10\n[    7] [7  ]\nx\n");
}

#[test]
fn fork_join_and_events() {
    let (out, _) = sim(r#"
        module t;
          event go;
          initial begin
            fork
              begin #2 $display("b"); end
              begin #1 $display("a"); end
            join
            $display("joined");
            -> go;
          end
          initial begin @(go); $display("got go"); $finish; end
        endmodule
    "#);
    assert_eq!(out, "a\nb\njoined\ngot go\n");
}

#[test]
fn stop_and_final() {
    let (out, end) = sim("module t; initial $stop; final $display(\"final\"); endmodule");
    assert_eq!((out.as_str(), end), ("final\n", End::Stop));
}
