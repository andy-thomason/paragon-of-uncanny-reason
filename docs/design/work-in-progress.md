# Work in progress

> **⚠️ DO NOT COPY CODE.** See the [banner in README.md](README.md).

This is the working roadmap: what is done, what comes next and why, and the
smaller items left open along the way. Update it as items land.

## Where we are (2026-10-02)

| Area | State | Where |
|---|---|---|
| Analysis plan, grammar | Draft | [`00`](00-analysis-plan.md), [`03`](03-grammar.md) |
| Lossless lexer (tokens are `&str` slices) | Done | `src/pp/lexer.rs` |
| Source map (positions without spans) | Done | `src/source.rs` |
| Preprocessor and `-E` writer | Done, with known gaps below | `src/pp/` |
| Async library `libparagon` and `paragon` CLI | Working: `simulate()` streams events from the interpreter | `libparagon/` |
| Test manifest and runner | Done ([`01`](01-test-suite-taxonomy.md)) | `tools/manifest.py`, `runner/` |
| Language lexer, parser, AST (`&str` slices) | Common subset done: 65% of T1 tests parse | `src/lex.rs`, `src/parse/`, `src/ast.rs` |
| Elaboration: AST → linear IR | Done for the common subset, plus strings, unpacked arrays, interfaces, instance arrays, output arguments and `disable` | `src/elab/`, `src/ir/`, `src/bits.rs`, `src/eval.rs` |
| Reference interpreter | Working: **801 of 2,139 T1 tests (37%) pass** | `src/sim/` |

## Progress (2026-10-02)

**801 of 2,139 T1 tests (37%) pass**: they simulate, print
`*-* All Finished *-*` or match their golden output, and end cleanly. Of the
rest, 1,158 stop at a named unsupported construct, 82 hit front-end errors and
76 simulate but fail. 46 T2 tests match Verilator's first diagnostic location.
Full table: [`01`](01-test-suite-taxonomy.md).

### Simulation (reference interpreter)

- **`src/sim`** runs the IR directly: each process is a thread with a stack of
  frames; the scheduler runs active, inactive, NBA and postponed (`$strobe`)
  regions, then advances time. Waiters carry an epoch so stale wake-ups are
  ignored. Fork/join, `disable` (blocks, tasks, `fork`) and `final` are done.
- **Time 0.** Combinational processes start first, in data-flow order (writers
  before readers), so `initial` blocks see settled values, as in Verilator.
  The LRM leaves this order open.
- **Test bench.** As Verilator's test bench does, the runner drives top-level
  `clk`/`fastclk` (low until 10, then toggling every 5), names the root `top`
  in `%m`, and holds other top inputs at 0. `--binary` tests run without it.
- **Pass rule.** A clean `$finish` (or running out of events) with no run-time
  errors, and output equal to the golden file where there is one. Run-time
  message text is not compared (decision 1).
- **Values.** Strings are stored one byte per `char`; unpacked arrays are a
  flat `Value::Array`, element 0 at the right-hand index; a subroutine with
  output arguments returns a tuple that the caller unpacks.

### Elaboration (done for the common subset)

- **Two phases.** First the whole scope tree is built: parameters, ports,
  implicit nets and generate expansion. Then the code of every scope is lowered.
  Hierarchical references, including upward ones, therefore reach any scope.
- **Declaration order.** Parameters and typedefs are resolved in dependency
  order, because Verilator accepts forward references (`t_param`). Functions
  are declared first and lowered on demand, so constant functions work in
  parameter expressions; `eval.rs` runs them.
- **Ports.** A port connected to a same-width variable becomes an alias of it.
  Anything else becomes a continuous-assignment process.
- **Library search.** A module that no file defines is looked for as
  `<dir>/<module>.v`/`.sv` (`-y`); the runner searches `t/` as the test driver does.
- **Width rules.** LRM §11.8 is applied in `elab/expr.rs` (self-determined
  types, then propagation), with explicit `Resize`.
- **Inspect the result:** `paragon --ir file.sv`.

Biggest gaps by T1 tests blocked: classes (459), concurrent assertions (62),
unpacked structs (43), virtual classes and interfaces (42), dynamic arrays and
queues (32), procedural `force` (27), clocking blocks (25), arrays of
interface ports (23).

Known differences kept on purpose: four-state run-time semantics where some
tests' `ifdef verilator` branches expect Verilator's two-state results (for
example `x / 0 == 0`); LRM left-to-right copying between arrays declared
`[0:7]` and `[7:0]`; `ref` arguments are copied in and out.

## Next steps

The aim is a thin slice through the whole pipeline, so the simplest Verilator
tests pass through `simulate()` and progress is a measured pass rate.

### Step 1: Test manifest and runner (Phase 1 of the plan): **done**

- An extractor that reads the 4,478 `t_*.py` drivers **as data**. It must not
  run them or import Verilator's harness. It writes `tests/manifest.json`
  with each test's scenario, flags, checks, tier (T1–T4 from the plan), and
  the licence of every input file.
- A runner that feeds each test through `libparagon`, evaluates the recorded
  checks (`*-* All Finished *-*`, golden output, expected failure), and
  reports pass / fail / waived by tier.
- **Why first:** every later step shows up as a number ("T1 passing:
  0 → 37"), and the manifest ranks features by how many tests they unlock.
- **Done when:** every test is classified (any left over are listed for manual
  review), and the runner produces a per-tier summary in CI.

### Step 2: Language lexer and parser: **common subset done**

Status: about 64% of CC0 sources parse, and the rest stop at a named
`NOTYET` construct. With no driver flags there are no remaining genuine syntax
errors in non-`_bad` CC0 sources; the 12 left need `-D` defines or
`--language`. The parser runs on a 256 MB stack (`libparagon`), with a
nesting limit, because `t_if_deep.v` nests several hundred deep.

Next parser work, by tests unlocked: classes (684), concurrent assertions and
properties (128), virtual interfaces (76), covergroups (60), clocking blocks
(60), user-defined primitives (26). Also needed: `.vlt` control files and `-f`
option files (about 40 setup failures).

Original plan:

- A light lexer on top of the preprocessor's token stream, keeping the `&str`
  approach: keywords per language mode (`03-grammar.md` §1), numbers split
  only as far as the parser needs.
- A hand-written recursive-descent parser. Start with the subset most
  self-checking tests use: modules and ports; `logic`/`reg`/`wire`/`integer`;
  parameters; `assign`; `always`/`always_ff`/`always_comb`/`initial`;
  `if`/`case`/loops; full expression precedence (`03` §10);
  `$display`/`$write`/`$finish`/`$stop`.
- Type-versus-value names are resolved with a scoped symbol table (ambiguity
  A1 in `03` §16).
- **Done when:** the parse sweep in `03` §18 runs over the corpus in CI and
  reports coverage. Every source in the chosen subset parses, and the expected
  syntax errors fail at the right first location.

### Step 3: A minimal simulator for that subset: **next**

- An interpreter: 2-state values of any width, the active and NBA
  scheduling regions (`05-simulation-semantics.md` when written),
  `$display` formatting, `$finish`/`$stop`.
- It is a **reference**, not the fast path. It is the oracle that the
  generated-code back end will be checked against later.
- It plugs into `libparagon::start` and reports through the event channel (see
  below).
- **Done when:** the first T1 tests pass end to end through `simulate()` and the
  runner counts them.

### Step 4: Architecture decision for the fast path

- With a working pipeline and a benchmark, decide the back end
  (`20-architecture.md`): proc macro vs `build.rs` code generation, value
  representation (`Bits<N>`, 2- vs 4-state), scheduling strategy, and
  batch/multi-instance simulation.
- **Done when:** there is an ADR with benchmark numbers against the interpreter
  and against Verilator.

## `libparagon` API notes

- `simulate(&str) -> Result<Simulation, Error>`. Compile errors come back
  straight away. The `Simulation` handle receives `Event`s while the
  simulation runs on its own thread:
  - `Event::Display { text, time }` for `$display`, `$write`, `$strobe` and
    `$monitor`, with the text exactly as written;
  - `Event::Diagnostic(..)` for compile-time warnings (sent first) and
    run-time `$warning`/`$error`/assertion failures;
  - `Event::Finished { finish, time }`, always last. `Finish::Aborted` means
    the simulation thread died, for example by panicking.
- `Simulation::next_event` (async), `next_event_blocking`, and `wait`, which
  gathers everything into a `SimResult`. Dropping the handle closes the channel,
  and the simulator must check `is_closed()` and stop.
- The channel is our own runtime-agnostic, unbounded one (`libparagon/src/channel.rs`).
- **Open:**
  - **Backpressure:** a bounded channel, so a chatty simulation can't use
    unbounded memory when the consumer is slow.
  - **Control messages:** a channel *into* the simulation (pause, run until time T,
    poke or peek signals) for interactive use and for the Rust harness ports
    (decision 2 in `00`).
  - **Event granularity:** whether `$monitor`/`$strobe` need their own variants,
    and whether waveform data (VCD/FST) goes through events or a separate sink.
  - **Many simulations:** an API for batch runs (many stimuli for one compiled
    design), which is one of the performance bets.

## Smaller open items

### Preprocessor

- [ ] Byte-exact plain `-E` layout: Verilator's placement of line breaks and
  `` `line `` markers. We already match the words and the line numbers. This
  affects about 4 `files_identical` tests.
- [ ] `--preproc-comments` and `--preproc-defines` output modes.
- [ ] `` `systemc_* `` raw-text regions are processed as ordinary text, not
  captured verbatim.
- [ ] `// synopsys` / `// synthesis` pragma comments (`full_case`,
  `parallel_case`, `translate_off/on`) are dropped. They should reach the
  parser.
- [ ] The diagnostic position for "Illegal text before '('" differs from
  Verilator's (we report at the macro use).
- [ ] Two documented deviations from Verilator, both kept on purpose
  (`03` §3.4 and §3.10): the `` `elsif ( ONE && !( ZERO && ONE ) ) `` case and
  `bug202`.

### Black-box probes

The grammar doc §17 lists about a dozen questions that need a working Verilator
binary. It can't be built on this machine yet (no Homebrew or autoconf, and the
system bison is too old). Options: install Homebrew, or run Verilator in Docker.

### Diagnostics

- [ ] Decide how to reproduce Verilator's end-of-file error positions, which
  differ by mode (see `tests/fixtures/verilator_cc0/README.md`).
- [ ] The 37 T2 tests where our first diagnostic is at a different place from
  the golden's (`paragon-runner --list wrong-diagnostic`). Verilator points at
  operands (an include filename, the end of an `` `ifdef `` expression), past
  end of file, or at the start of a construct.
- [ ] Column numbering with tabs: we count a tab as one column (unconfirmed).
