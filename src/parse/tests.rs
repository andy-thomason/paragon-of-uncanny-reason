use super::*;
use crate::ast::*;
use crate::keywords::Lang;
use crate::pp::{FileSystem, Options, Preprocessor};
use crate::source::SourceMap;

struct OneFile(&'static str);

impl FileSystem for OneFile {
    fn read(&self, path: &str) -> Option<String> {
        (path == "t.sv").then(|| self.0.to_string())
    }
}

/// Preprocess, lex and parse `src`, returning the tree and rendered diagnostics.
fn parse_src<'a>(sm: &'a SourceMap, src: &'static str) -> (SourceText<'a>, Vec<String>) {
    let fs = OneFile(src);
    let mut pp = Preprocessor::new(sm, &fs, Options::default());
    pp.process_file("t.sv");
    let lexed = crate::lex::lex(sm, pp.output(), Lang::Sv2023);
    let (tree, diags) = parse(&lexed);
    let msgs = lexed
        .diags
        .iter()
        .chain(&diags)
        .map(|d| {
            let l = sm.locate(d.at).unwrap();
            format!("{}:{}: {}", l.line, l.col, d.message)
        })
        .collect();
    (tree, msgs)
}

fn ok(src: &'static str) -> SourceText<'static> {
    let sm = Box::leak(Box::new(SourceMap::new()));
    let (tree, msgs) = parse_src(sm, src);
    assert!(msgs.is_empty(), "{msgs:?}");
    tree
}

fn first_error(src: &'static str) -> String {
    let sm = Box::leak(Box::new(SourceMap::new()));
    let (_, msgs) = parse_src(sm, src);
    msgs.into_iter().next().expect("expected an error")
}

fn only_module(t: &SourceText<'static>) -> &'static Module<'static> {
    match &t.items[..] {
        [Item::Module(m)] => Box::leak(Box::new(m.clone())),
        other => panic!("expected one module, got {other:?}"),
    }
}

/// The classic shape of a Verilator self-checking test.
const COUNTER_TEST: &str = r#"
module t (/*AUTOARG*/
   // Inputs
   clk
   );
   input clk;
   integer cyc = 0;
   reg [63:0] crc;
   wire [31:0] sum = crc[31:0] ^ {crc[15:0], crc[31:16]};

   always @ (posedge clk) begin
      cyc <= cyc + 1;
      crc <= {crc[62:0], crc[63] ^ crc[2] ^ crc[0]};
      if (cyc == 0) begin
         crc <= 64'h5aef0c8d_d70a4497;
      end
      else if (cyc == 99) begin
         if (sum !== 32'hdeadbeef) $stop;
         $write("*-* All Finished *-*\n");
         $finish;
      end
   end
endmodule
"#;

#[test]
fn verilator_style_counter_test() {
    let t = ok(COUNTER_TEST);
    let m = only_module(&t);
    assert_eq!(m.name, "t");
    assert!(matches!(&m.ports, Ports::NonAnsi(p) if p.len() == 1));
    assert_eq!(m.items.len(), 5);
    let ModuleItem::Process {
        kw: "always",
        stmt: Stmt::Timed {
            timing: Timing::Event(Some(ev)),
            ..
        },
    } = &m.items[4]
    else {
        panic!("{:?}", m.items[4])
    };
    assert_eq!(ev[0].edge, Some("posedge"));
}

#[test]
fn ansi_ports_inherit_direction_and_type() {
    let t = ok("module m(input logic [7:0] a, b, output reg c, input d); endmodule");
    let Ports::Ansi(p) = &only_module(&t).ports else {
        panic!()
    };
    let summary: Vec<_> = p.iter().map(|p| (p.dir, p.name)).collect();
    assert_eq!(
        summary,
        [
            (Some("input"), "a"),
            (Some("input"), "b"),
            (Some("output"), "c"),
            (Some("input"), "d")
        ]
    );
    assert!(matches!(&p[1].ty, DataType::Builtin { kw: "logic", packed, .. } if packed.len() == 1));
}

#[test]
fn parameters_and_instances() {
    let t = ok(r#"
        module sub #(parameter int W = 8, parameter type T = logic) (input [W-1:0] a, output T y);
        endmodule
        module top;
          logic [3:0] a; logic y;
          sub #(.W(4), .T(logic)) u_sub (.a(a), .y);
          sub #(4) u2 (a, y), u3 (.*);
        endmodule
    "#);
    let Item::Module(top) = &t.items[1] else {
        panic!()
    };
    let ModuleItem::Instance(i) = &top.items[2] else {
        panic!("{:?}", top.items[2])
    };
    assert_eq!((i.module, i.insts[0].name), ("sub", "u_sub"));
    assert!(matches!(&i.insts[0].conns[1], PortConn::Named("y", None)));
    let ModuleItem::Instance(i) = &top.items[3] else {
        panic!()
    };
    assert_eq!(i.insts.len(), 2);
    assert!(matches!(&i.insts[1].conns[0], PortConn::Wildcard));
}

#[test]
fn user_type_declaration_vs_instance() {
    let t =
        ok("typedef logic [3:0] nib_t;\nmodule m; nib_t a, b; foo u (); nib_t c [2]; endmodule");
    let Item::Module(m) = &t.items[1] else {
        panic!()
    };
    assert!(matches!(&m.items[0], ModuleItem::Var(_)));
    assert!(matches!(&m.items[1], ModuleItem::Instance(_)));
    assert!(matches!(&m.items[2], ModuleItem::Var(_)));
}

#[test]
fn expression_precedence() {
    let t = ok("module m; initial x = a + b * c << 1 == d ? e : f || g; endmodule");
    let ModuleItem::Process {
        stmt: Stmt::Assign { rhs, .. },
        ..
    } = &only_module(&t).items[0]
    else {
        panic!()
    };
    // ?: binds loosest; == binds tighter than ?: ; << tighter than == ; + tighter than <<.
    let Expr::Cond { cond, els, .. } = rhs else {
        panic!("{rhs:?}")
    };
    assert!(matches!(&**cond, Expr::Binary { op: "==", .. }));
    assert!(matches!(&**els, Expr::Binary { op: "||", .. }));
    let Expr::Binary { lhs: shift, .. } = &**cond else {
        panic!()
    };
    let Expr::Binary {
        op: "<<", lhs: sum, ..
    } = &**shift
    else {
        panic!("{shift:?}")
    };
    assert!(
        matches!(&**sum, Expr::Binary { op: "+", rhs, .. } if matches!(&**rhs, Expr::Binary { op: "*", .. }))
    );
}

#[test]
fn nonblocking_vs_less_equal() {
    let t = ok("module m; always @* begin a <= b <= c; if (a <= b) d = 1; end endmodule");
    let ModuleItem::Process {
        stmt: Stmt::Timed { stmt, .. },
        ..
    } = &only_module(&t).items[0]
    else {
        panic!()
    };
    let Stmt::Block { stmts, .. } = &**stmt else {
        panic!()
    };
    assert!(matches!(
        &stmts[0],
        Stmt::Assign {
            op: "<=",
            rhs: Expr::Binary { op: "<=", .. },
            ..
        }
    ));
}

#[test]
fn statements() {
    ok(r#"
        module m;
          int q[$]; int arr[4]; string s; event ev;
          initial begin : blk
            int i;
            automatic int j = 2;
            for (int k = 0, l = 1; k < 4; k++, l += 2) arr[k] = k;
            foreach (arr[n]) s = {s, "x"};
            while (i < 3) i++;
            do i--; while (i > 0);
            repeat (2) #1;
            case (i) 0, 1: ; 2: i = 3; default i = 4; endcase
            casez (s) "a": ; endcase
            unique case (i) inside [0:3], 7: ; endcase
            if (i inside {1, [2:3]}) $display("in");
            fork #1 i = 1; join_none
            wait (i == 1);
            -> ev;
            @(ev);
            @(posedge i or negedge j iff i);
            i = #2 j;
            q.push_back(1);
            assert (i == 1) else $error("bad");
            void'($sformatf("%d", i));
            disable blk;
          end
        endmodule
    "#);
}

#[test]
fn generate_constructs() {
    ok(r#"
        module m #(parameter N = 4) (input [N-1:0] a, output [N-1:0] y);
          genvar i;
          generate
            for (i = 0; i < N; i = i + 1) begin : g
              assign y[i] = ~a[i];
            end
          endgenerate
          for (genvar j = 0; j < 2; j++) begin
            wire w;
          end
          if (N > 2) begin : big
            initial $display("big");
          end else begin
            initial $display("small");
          end
          case (N) 1: wire one; default: wire many; endcase
        endmodule
    "#);
}

#[test]
fn functions_and_tasks() {
    ok(r#"
        module m;
          function automatic int add(input int a, b = 1);
            return a + b;
          endfunction : add
          function [7:0] old_style;
            input [7:0] x;
            reg [7:0] t;
            begin t = x; old_style = t; end
          endfunction
          task t2(output logic o); #1 o = 1; endtask
          function void nothing(); endfunction
          initial begin int r; r = add(1, 2); t2(r); nothing(); end
        endmodule
    "#);
}

#[test]
fn types() {
    ok(r#"
        package p;
          typedef enum logic [1:0] { A, B = 2, C } e_t;
          typedef struct packed { logic [3:0] hi; logic [3:0] lo; } s_t;
          localparam int W = $bits(s_t);
        endpackage
        module m;
          import p::*;
          p::e_t e;
          s_t s;
          int dyn[]; int assoc[string]; int wild[*];
          logic [7:0] mem [0:15];
          initial begin
            s = '{hi: 4'h1, lo: 4'h2};
            s = s_t'(8'hAB);
            dyn = new[4];
            e = p::B;
            mem[0] = {4{2'b10}};
            mem[1] = {<<4{8'h12}};
          end
        endmodule
    "#);
}

#[test]
fn gates_and_directives() {
    ok(
        "`timescale 1ns/1ps\nmodule m(output y, input a, b); and g1 (y, a, b); not (z, a); endmodule",
    );
}

#[test]
fn syntax_errors_report_the_first_position() {
    assert_eq!(
        first_error("module m;\n  wire = 1;\nendmodule"),
        "2:8: syntax error, unexpected '=', expecting identifier"
    );
    assert_eq!(
        first_error("module m;\ninitial begin\n"),
        "3:1: syntax error, unexpected end of file, expecting 'end'"
    );
}

#[test]
fn unsupported_constructs_are_not_yet() {
    let sm = Box::leak(Box::new(SourceMap::new()));
    let fs = OneFile("module m; covergroup cg; endgroup endmodule");
    let mut pp = Preprocessor::new(sm, &fs, Options::default());
    pp.process_file("t.sv");
    let lexed = crate::lex::lex(sm, pp.output(), Lang::Sv2023);
    let (_, diags) = parse(&lexed);
    assert_eq!(diags[0].code, Some(crate::diag::NOT_YET));
    assert_eq!(diags[0].message, "Not yet supported: covergroups");
}

#[test]
fn pathological_nesting_fails_cleanly() {
    // Deeper than MAX_DEPTH, run on a big stack as libparagon does.
    let src: &'static str = Box::leak(
        format!(
            "module m; initial x = {}1{};\nendmodule",
            "(".repeat(30_000),
            ")".repeat(30_000)
        )
        .into_boxed_str(),
    );
    let msg = std::thread::Builder::new()
        .stack_size(1 << 30)
        .spawn(move || first_error(src))
        .unwrap()
        .join()
        .unwrap();
    assert!(msg.ends_with("Nesting too deep"), "{msg}");
}

#[test]
fn ast_slices_point_into_the_source() {
    let sm = Box::leak(Box::new(SourceMap::new()));
    let (t, msgs) = parse_src(sm, "module m;\n  initial x = 8'hFF;\nendmodule\n");
    assert!(msgs.is_empty());
    let Item::Module(m) = &t.items[0] else {
        panic!()
    };
    let ModuleItem::Process {
        stmt: Stmt::Assign { rhs, .. },
        ..
    } = &m.items[0]
    else {
        panic!()
    };
    let loc = sm.locate(rhs.at()).unwrap();
    assert_eq!((rhs.at(), loc.line, loc.col), ("8'hFF", 2, 15));
}
