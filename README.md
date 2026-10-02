# Paragon of Uncanny Reason

A SystemVerilog simulator written in Rust. The aim is to pass the externally
observable behaviour checked by the [Verilator](https://verilator.org)
regression suite, but with a different architecture and better performance.
The plan includes a Rust procedural macro that compiles SystemVerilog to native
code.

This is a clean-room implementation. No Verilator code, algorithms or grammar
files are copied. See [`docs/design/README.md`](docs/design/README.md) for
the rules, and [`docs/design/`](docs/design/) for the design documents.

## Status

| Stage | State |
|---|---|
| Preprocessor (`` `define ``, `` `ifdef ``, `` `include ``, `-E`) | Working. Matches Verilator's golden output; see below |
| Lexer and parser (AST of `&str` slices) | Working for the common subset: 64% of CC0 test sources parse; the rest stop at a named unsupported construct, mostly classes |
| Test manifest and runner | Working: classifies all 4,447 Verilator tests and reports progress by tier |
| Elaboration to a linear IR | Working for the common subset plus strings, unpacked arrays, interfaces; `paragon --ir` prints the IR |
| Simulation | Reference interpreter working: 37% of self-checking tests pass |

`simulate()` compiles the source and returns a running `Simulation` that
streams events such as `$display` output from the reference interpreter.

The preprocessor is tested against Verilator's own CC0 golden files:

- byte-exact `-E -P` output for four tests, and a fifth apart from one documented deviation;
- word-for-word output and line numbering for `t_preproc`, the main preprocessor test;
- a smoke run over all 3,324 CC0 sources in the Verilator suite.

Two places where we follow IEEE 1800-2023 rather than copy a Verilator
quirk are documented in [`docs/design/03-grammar.md`](docs/design/03-grammar.md) §3.10.

## Command line

```console
$ cargo install --path libparagon
$ paragon [-E [-P] | --ir] [-DNAME[=VALUE]] [+incdir+DIR] [-IDIR] [--top-module NAME] FILE
```

| Option | Meaning |
|---|---|
| `-E` | Preprocess only, writing the result to stdout with `` `line `` markers |
| `--ir` | Compile and print the elaborated IR |
| `--top-module NAME` | Choose the top module |
| `-P` | With `-E`: no `` `line `` markers, and blank lines dropped |
| `-DNAME[=VALUE]` | Define a macro |
| `+incdir+DIR[+DIR...]`, `-IDIR` | Add include directories |

Given `counter.sv`:

```systemverilog
`include "widths.vh"
`define REG(name, w) logic [w-1:0] name

module counter;
  `REG(count, `WIDTH);
  initial $display("width=%0d at %s:%0d", `WIDTH, `__FILE__, `__LINE__);
endmodule
```

and `inc/widths.vh`:

```systemverilog
`ifndef WIDTH
  `define WIDTH 8
`endif
```

preprocess it:

```console
$ paragon -E -P +incdir+inc counter.sv
module counter;
  logic [8-1:0] count;
  initial $display("width=%0d at %s:%0d", 8, "counter.sv", 6);
endmodule

$ paragon -E -P +incdir+inc -DWIDTH=16 counter.sv
module counter;
  logic [16-1:0] count;
  initial $display("width=%0d at %s:%0d", 16, "counter.sv", 6);
endmodule
```

Without `-P`, the output carries `` `line `` markers, so later tools can
report positions in the original files:

```console
$ paragon -E +incdir+inc counter.sv
`line 1 "counter.sv" 1
`line 1 "inc/widths.vh" 1
...
```

Errors are reported in Verilator's format:

```console
$ paragon counter.sv
%Error: counter.sv:1:1: Cannot find include file: 'widths.vh'
        ... Looked in:
             widths.vh
             widths.vh.v
             widths.vh.sv
%Error: Exiting due to 1 error(s)
```

`--ir` compiles the design and prints the elaborated IR: basic blocks of
typed, three-address instructions, with every width change explicit. Given
`count.sv`:

```systemverilog
module count;
  logic clk = 0;
  logic [3:0] n = 0;
  always #5 clk = ~clk;
  always @(posedge clk) n <= n + 1;
endmodule
```

```console
$ paragon --ir count.sv
var count.clk: logic = 1'h0
var count.n: logic[4] = 4'h0
proc Always in count:
  bb0:
    %0 = const 64'h5                              : bit[64]
    suspend Delay(%0) -> bb1
  bb1:
    %1 = load count.clk                           : logic
    %2 = Not %1                                   : logic
    store count.clk = %2
    jump bb0()
proc Always in count:
  bb0:
    suspend Edge[(count.clk, Pos)] -> bb1
  bb1:
    %0 = load count.n                             : logic[4]
    %1 = resize.Zero %0                           : logic[32]
    %2 = const 32'h1                              : bit[32] signed
    %3 = resize.Zero %2                           : logic[32]
    %4 = Add %1, %3                               : logic[32]
    %5 = resize.Truncate %4                       : logic[4]
    nba_store count.n = %5
    jump bb0()
```

`n + 1` is computed at 32 bits, because the unsized `1` is 32 bits wide, and
then truncated to 4 bits, as IEEE 1800 §11.8 requires.

Running without `-E` or `--ir` simulates, printing `$display` output as it
happens:

```console
$ cat hello.sv
module t;
  logic [3:0] n = 0;
  initial repeat (3) #10 begin n++; $display("[%0t] n=%0d", $time, n); end
  initial #35 $finish;
endmodule
$ paragon hello.sv
[10] n=1
[20] n=2
[30] n=3
```

`--clock clk` drives a top-level input as a clock, as Verilator's test bench
does, and `-GNAME=VALUE` overrides a top-level parameter.

## Library

The `libparagon` crate has async entry points. They do not depend on any
particular runtime, and the futures are `Send`. For code without a runtime,
`libparagon::block_on` runs a future on the current thread.

```toml
[dependencies]
libparagon = { git = "https://github.com/andy-thomason/paragon-of-uncanny-reason" }
```

### Simulate source text

`simulate` compiles the source and returns a `Simulation`. Compile errors are
returned straight away. Once running, the simulation is on its own thread and
sends `Event`s: `$display` output as it happens, run-time diagnostics, and
always a final `Finished`. Dropping the `Simulation` stops it.

```rust
use libparagon::{Error, Event, block_on, simulate};

let source = r#"
    module t;
      initial begin
        $display("hello");
        $finish;
      end
    endmodule
"#;

block_on(async {
    let mut sim = match simulate(source).await {
        Ok(sim) => sim,
        Err(Error::Diagnostics(diags)) => return diags.iter().for_each(|d| eprintln!("{d}")),
        Err(e) => return eprintln!("{e}"),
    };
    while let Some(event) = sim.next_event().await {
        match event {
            Event::Display { text, time } => print!("[{time}] {text}"),
            Event::Diagnostic(d) => eprintln!("{d}"),
            Event::Finished { finish, time } => println!("{finish:?} at {time}"),
        }
    }
});
```

If you only want the end result, `Simulation::wait` gathers the events into a
`SimResult` with `stdout`, `finish`, `time` and `diagnostics`. Without an async
context, use `Simulation::next_event_blocking` instead of `next_event`.

### Preprocess with options

```rust
use libparagon::{Options, block_on, preprocess_with};

let opts = Options {
    defines: vec![("WIDTH".into(), "16".into())],
    read_includes_from_disk: false,
    ..Options::default()
};
let source = "`define REG(name, w) logic [w-1:0] name\n`REG(count, `WIDTH);\n";

let out = block_on(preprocess_with(source, &opts)).unwrap();
assert_eq!(out.text, "logic [16-1:0] count;\n");
```

### Diagnostics

Errors come back as `Error::Diagnostics`, with positions already resolved.
Warnings are returned alongside successful results.

```rust
use libparagon::{Error, Severity, block_on, preprocess};

let Err(Error::Diagnostics(diags)) = block_on(preprocess("\n`ifdef MISSING_ENDIF\n")) else {
    panic!("expected an error");
};
let d = &diags[0];
assert_eq!((d.severity, d.line, d.col), (Severity::Error, 3, 1));
assert_eq!(d.to_string(), "%Error: input.sv:3:1: `ifdef not terminated at EOF");

let out = block_on(preprocess("`define D a\n`define D b\n")).unwrap();
assert_eq!(out.warnings[0].code.as_deref(), Some("REDEFMACRO"));
```

### From an async runtime

```rust,ignore
#[tokio::main]
async fn main() -> Result<(), libparagon::Error> {
    let source = std::fs::read_to_string("top.sv").unwrap();
    let result = libparagon::simulate(&source).await?.wait().await;
    print!("{}", result.stdout);
    Ok(())
}
```

## Progress against the Verilator test suite

`paragon-runner` runs every Verilator regression test through `libparagon`
and reports how far each one gets. Today, 801 of the 2,139 self-checking
simulation tests (37%) pass. Most of the rest stop at classes, assertions or
other constructs not supported yet. See
[`docs/design/01-test-suite-taxonomy.md`](docs/design/01-test-suite-taxonomy.md).

```console
$ python3 tools/manifest.py                 # classify the tests (needs ../verilator)
$ cargo run --release -p paragon-runner     # summary by tier
$ cargo run --release -p paragon-runner -- --tier T1 --list not-yet
```

## Repository layout

| Path | Contents |
|---|---|
| `src/` | Core crate: preprocessor lexer (`pp/lexer.rs`), preprocessor (`pp/mod.rs`), `-E` writer (`pp/emit.rs`), language lexer (`lex.rs`), parser (`parse/`), syntax tree (`ast.rs`), source map (`source.rs`) |
| `runner/` | `paragon-runner`, which runs the Verilator tests and reports progress |
| `libparagon/` | Async library API and the `paragon` command-line tool |
| `docs/design/` | Design documents: analysis plan, grammar |
| `tests/` | Golden-output and corpus tests |
| `tests/fixtures/verilator_cc0/` | Test inputs and golden outputs copied from Verilator's CC0 test files |
| `tools/` | Test manifest extractor and corpus survey scripts |

## Testing

```console
$ cargo test --workspace
```

Some tests read the Verilator test suite in place. They expect a checkout next
to this repository and are skipped if it is missing:

```console
$ git clone --depth 1 https://github.com/verilator/verilator.git ../verilator
```

Set `VERILATOR_TESTS=/path/to/verilator/test_regress/t` to use another location.

## Licence

MIT; see [`LICENSE`](LICENSE). The files in `tests/fixtures/verilator_cc0/`
are copied from the Verilator test suite and are public domain (CC0-1.0), as
their headers state.
