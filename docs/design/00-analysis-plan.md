# 00: Analysis Plan

> **⚠️ DO NOT COPY CODE.** See the [banner in README.md](README.md). This plan collects *facts about behaviour*, never implementation.

## Goal

Gather every detail we need to build a clean-room simulator that passes the Verilator regression suite.
Verilator's methods stay out of the process. The deliverables are the `01`–`11` documents and a machine-readable test manifest.
Implementers will work from those deliverables alone.

## Baseline: what the test suite looks like

These counts come from a survey of `../verilator/test_regress/t` at the pinned commit:

| Measure | Count |
|---|---|
| Files in `t/` | 9,966 |
| Test drivers (`t_*.py`, one per test) | 4,478 |
| `.v` / `.sv` sources | 3,583 / 2 (plus `.vlt`, `.dat`, `.mem` support files) |
| Golden output files (`.out`) | 1,572 |
| C++ harness files (`.cpp`) | 182, used by about 250 tests |
| Drivers that **execute** a simulation | 2,835 |
| Drivers that **lint** only | 1,042 |
| Drivers expecting **failure** (`fails=True`) | 903 |
| Drivers comparing against a golden file (`expect_filename`) | 1,217 |
| Drivers that grep `--stats` output | 189 |
| Drivers that grep **generated C++** in `obj_dir` | ~135 |
| Drivers that use tree dumps or `--debug` | ~100 |
| Drivers that involve tracing (VCD/FST/SAIF) | ~162 |
| Drivers that use `--timing` | ~241 |

**Scenarios** (`test.scenarios(...)`): `simulator` 2,337 · `vlt` 888 · `linter` 619 · `vlt_all` 431 · `simulator_st` 68 · `dist` 56 · `vltmt` 25.
The `simulator` and `linter` scenarios are also run against other simulators (Icarus, Questa, Xcelium, VCS, xsim).
That makes them **simulator-agnostic by design**, so they are our primary target.

**Largest feature areas** (by test-name prefix): class 528, interface 456, trace 436, lint 409, covergroup 307, flag 300,
constraint 245, randomize 239, param 236, func 233, assert 203, math 194, cover 181, opt 180, dpi 177, var 170, vpi 163,
sys 159, timing 153, property 145, inst 145.

**Licensing of test inputs:** 3,322 CC0 · 240 LGPL · 18 Unlicense · a handful of others. The UVM library under `t/uvm/` is Apache-2.0.
All `.py` drivers are LGPL. We run the drivers in place and never copy them.

## What "pass all the tests" means

Not every test checks behaviour that a different implementation can reproduce. Phase 1 sorts each test into a tier:

| Tier | Description | Target |
|---|---|---|
| **T1: Behavioural simulation** | The test compiles, runs, and self-checks (`$stop` on mismatch, prints `*-* All Finished *-*`). Optionally it compares stdout with a `.out` file. | **Must pass.** This is the core of compatibility. |
| **T2: Diagnostics** | Lint or compile is expected to fail, and stderr is compared with a golden `.out` (warning code, file:line:col, message). | **Must pass** on outcome, code and location. Message text is not matched (see Decisions). |
| **T3: Verilator-specific features** | `/*verilator*/` metacomments, `.vlt` config, `$c`, `--public`, the C++ harness API, DPI/VPI, trace files, coverage files, `--protect-lib`, `--hierarchical`, `--json-only`, `-E`. | **Should pass** where the feature makes sense for a Rust back end. Exceptions are recorded per feature in `04`/`06`/`08`. |
| **T4: Implementation internals** | Greps of `--stats` counters, tree dumps, generated C++ file names or contents, and `dist` tests that lint Verilator's own source tree. | **Out of scope.** For each test we extract the behavioural part, if any, and record an "equivalent check". Otherwise we exclude it with a reason. |

One test can belong to more than one tier. For example, it may execute (T1) and also grep stats (T4).
The manifest records **every check** separately, so we can pass the T1 part and waive the T4 part.

## Phases

### Phase 0: Setup and hygiene (½ day)

- [ ] Build Verilator from `../verilator` into a private prefix, or `brew install verilator` at a matching version.
  It is only ever *run*, never linked.
- [ ] Install reference simulators for cross-checking ambiguous semantics: Icarus Verilog, and optionally Verible for parse checking.
- [ ] Add a `CLAUDE.md` / `CONTRIBUTING.md` rule restating the clean-room policy, so humans and agents see it at the start of every session.
- [ ] Create `docs/design/provenance.md`: a log of who read which restricted source and when, so we can enforce the two-role split.

### Phase 1: Test-suite taxonomy → `01-test-suite-taxonomy.md` + `tests/manifest.json`

We write **our own** extractor (Rust or Python, original code). It parses each `t_*.py` as data. It does not execute the driver
or import `vltest_bootstrap`. For every test it records:

- [ ] Name, scenarios, the top source file (`t_foo.v` or an override), included and support files, and the licence of each input.
- [ ] Each phase called (`lint`, `compile`, `execute`, `passes`, `skip`), with its flags (`verilator_flags`, `verilator_flags2`,
  `v_flags2`, `threads`, `timing`, `make_main`, `make_top_shell`).
- [ ] Each assertion: `fails`, `expect_filename`, `file_grep` (and its target: stdout, a stats file, an obj_dir file, or a trace),
  `vcd_identical` / `fst_identical`, `files_identical`, `check_finished`.
- [ ] Whether a C++ harness is used and which API identifiers it references (a token count of `t_*.cpp`, not a copy).
- [ ] The computed tier(s), and the feature tag(s) from the name prefix plus the LRM constructs detected in the source.

Exit criterion: every one of the 4,478 tests is classified. Tests that can't be parsed automatically go on a list for
manual review. A summary table appears in `01`.

### Phase 2: Language feature inventory → `02-feature-inventory.md`

- [ ] Walk the IEEE 1800-2023 table of contents, clause by clause. For each construct, record whether Verilator supports it
  (from `docs/guide/languages.rst` and black-box probing), which tests exercise it (from the manifest), and its priority
  (weighted by T1 test count).
- [ ] Record the documented Verilator **limitations and deviations** in our own words: for example, 2-state default and X handling,
  unsupported constructs, and constructs accepted but ignored.
- [ ] Produce an ordered implementation roadmap: the smallest set of features that unlocks the most T1 tests.

### Phase 3: Formal grammar → `03-grammar.md`

This is written **from IEEE 1800-2023 Annex A (BNF) and Annex B (keywords)**. `verilog.y`, `verilog.l` and `V3PreLex.l` are
**not opened** for this phase.

- [ ] **Lexical grammar:** identifiers, escaped identifiers, number literals (sized/unsized, `'0 '1 'x 'z`), strings and escapes,
  time literals, operators, comments.
- [ ] **Preprocessor grammar:** `` `define `` with arguments and defaults, `` `ifdef/`elsif ``, `` `include ``, `` `line ``,
  `` `__FILE__/`__LINE__ ``, token pasting ``` `` ```, stringification `` `" ``, `` `default_nettype ``, `` `timescale ``,
  `` `begin_keywords ``, `` `pragma ``, `` `resetall ``, `` `celldefine ``. Golden `-E` outputs in `t_preproc*` define what is observable.
- [ ] **Language grammar** in EBNF, organised to match Annex A sections (source text, declarations, items, statements,
  expressions, assertions, classes, constraints, covergroups). Ambiguities are annotated with how the LRM resolves them.
- [ ] **Verilator specialities**, each marked `[VLT]` in the grammar. They are derived from `docs/guide/extensions.rst`,
  `control.rst` and black-box probing:
  - metacomments `/*verilator <keyword> ...*/` and `// verilator <keyword>`. The full keyword list (public, public_flat_rw,
    lint_off/on, coverage_off/on, tracing_off/on, clock_enable, isolate_assignments, split_var, inline_module, no_inline_module,
    sc_bv, sformat, parallel_case, full_case, hier_block, …) is to be **enumerated from the user guide**.
  - `` `verilator_config `` blocks and `.vlt` configuration file syntax (`lint_off -rule … -file … -lines …`, `public`,
    `tracing_off`, `hier_block`, …)
  - `` `systemc_header/`systemc_interface/`systemc_imp/`systemc_ctor/`systemc_dtor `` and `` `verilog `` blocks
  - `$c`, `$cpure`, `$c8`…`$c64` inline-C system functions, and how a Rust back end should treat them
  - `` `coverage_block_off ``, `` `__VERILATOR__ `` / `` `VERILATOR `` predefines, `` `SYSTEMVERILOG ``
  - Verilator-specific system tasks and functions (`$stacktrace`, `$system`, …), enumerated from the guide
  - parse-mode switches: `--language`, `+1364-2005ext+`, `+1800-2023ext+`, `--relative-includes`
- [ ] **Validation.** Generate (or hand-write) a parser *test* from the grammar and run it over all 3,583 test sources.
  Every file that Verilator accepts must parse. Files that Verilator rejects with a syntax error (golden `.out`) must fail
  at the same location.
- [ ] Note any grammar features that are **hostile to a proc-macro token stream** (see Phase 8).

### Phase 4: Diagnostics → `09-diagnostics.md`

- [ ] Catalogue every warning and error code from `docs/guide/warnings.rst`: name, default severity, whether it can be
  suppressed, and its trigger described in our words.
- [ ] Reverse-engineer the **message format** from the golden `.out` files: `%Warning-CODE: file:line:col: text`, continuation
  lines, source-snippet lines with carets, the `... For warning description see …` trailer, `%Error: Exiting due to N error(s)`.
- [ ] Define the normalisation from golden `.out` to `(severity, code, file, line, col)` tuples, and list any T2 tests
  whose golden output carries no code (for example plain `%Error:` syntax errors). Those tests match on location only.

### Phase 5: Simulation semantics → `05-simulation-semantics.md`

Every item here is answered by **black-box experiments**. Each experiment is a tiny `.sv` file of our own, run through Verilator
and optionally Icarus. The doc records the observed output and the governing LRM clause.

- [ ] Event regions as observed: active, inactive, NBA, observed, reactive, postponed. Which orderings tests rely on.
- [ ] Initial-value rules: `--x-assign`, `--x-initial`, `--x-initial-edge`, and the 2-state representation of 4-state types.
- [ ] Combinational loops and convergence (`DIDNOTCONVERGE`), and evaluation of the `UNOPTFLAT` class.
- [ ] `--timing` vs `--no-timing`: `#delay`, `@event`, `wait`, `fork/join*`, `disable`, process control, `final`.
- [ ] Formatting: `$display`/`$write`/`$sformatf` widths and padding for every `%` code, `%t` and `$timeformat`, `$realtime`.
- [ ] Arithmetic: signedness and width rules (LRM 11.6–11.8), wide (>64-bit) values, real/shortreal, `**`, shifts, division by zero.
- [ ] Strings, dynamic arrays, queues, associative arrays: method semantics and out-of-bounds behaviour.
- [ ] Classes: virtual dispatch, parameterised classes, `static`, `this/super`, handles, garbage-collection observability.
- [ ] Randomisation and constraints. Expected outputs that depend on a specific RNG stream need special handling,
  because we **cannot** reproduce Verilator's RNG without copying it. Classify these as "seed-sensitive".
- [ ] Assertions (immediate and concurrent), `cover`, covergroups: observable outputs only.
- [ ] `$finish`/`$stop`/`$fatal` exit codes, `+verilator+` runtime plusargs, `$test$plusargs`/`$value$plusargs`.

### Phase 6: Harness API, DPI and VPI → `06-runtime-api.md`

- [ ] From the Phase 1 token counts, list every Verilator C++ API identifier the 182 harnesses use, ranked by frequency
  (for example `VerilatedContext`, `eval`, `timeInc`, `gotFinish`, `final`, `rootp` signal access, `Verilated::traceEverOn`).
  Describe the semantics in our words, from `docs/guide/connecting.rst` and experiments.
- [ ] DPI: `svdpi.h` is defined by IEEE 1800 Annex H, so we implement it from the standard. Inventory the import/export forms
  that the tests use.
- [ ] VPI: `vpi_user.h` is defined by IEEE 1800 Clause 36–38, implemented from the standard. Inventory the VPI calls that tests use.
- [ ] For each C++-harness test (waived for now, see Decisions), write a short behavioural spec in our own words: what it
  drives, what it observes, and what it asserts. These specs are the input for future Rust harness ports, and they define
  the capabilities our Rust model API must expose.

### Phase 7: Output formats → `08-output-formats.md`

- [ ] VCD (IEEE 1364 §18) and the exact conventions that `vcd_identical` tests compare: scope naming, ID codes, timescale,
  header, and the ordering that matters.
- [ ] FST (the GTKWave format specification), SAIF (an IEEE 1801 annex), coverage `.dat` format (from `exe_verilator_coverage.rst`
  plus experiments), `--json-only` schema (from samples), and `-E` preprocessed-output conventions.
- [ ] For each format, decide whether we must be *byte-identical* or *semantically identical*. Also decide whether to use the
  test-runner tools' own comparison semantics (for example `vcddiff`-style).

### Phase 8: Transpile examples → `07-transpile-examples.md`

Black-box only. We write small **original** examples, run `verilator --cc` (and `--binary`), and describe what comes out.
Generated code is quoted only in short excerpts, to illustrate behaviour.

- [ ] Example set: (1) combinational `assign` adder, (2) clocked counter with reset, (3) FSM with `case`, (4) parameterised
  module hierarchy, (5) memory with `$readmemh`, (6) `$display` testbench with `--binary`, (7) wide (>64-bit) arithmetic,
  (8) `--timing` delays and `fork`, (9) a simple class, (10) DPI import.
- [ ] For each: input `.sv`; the generated file list and model class shape (top class, internal state, eval entry points);
  the observable evaluation contract (what a harness must call, in what order); and **a sketch of our Rust output** for the
  same input. The Rust sketch is designed independently, from the semantics, not from the C++.
- [ ] Use the examples to set performance baselines: compile time and simulated cycles per second, with flags recorded.

### Phase 9: CLI options → `10-cli-options.md`

- [ ] Enumerate every option from `docs/guide/exe_verilator.rst` and every option the manifest finds in `verilator_flags*`.
- [ ] Classify each one: **semantic** (changes simulation results: `-G`, `-D`, `--top`, `--x-*`, `--timing`, `-Wno-*`, `+define+`,
  `-f`), **packaging** (`--cc/--binary/--exe/--build/--main/-o/--Mdir`), **optimisation-only** (`-O*`, `-f*`, `--threads`:
  we must accept and may ignore them), or **unsupported** (`--sc`, `--protect-lib`, …, with a decision for each).

### Phase 10: Test runner → `11-test-runner.md`

- [ ] Design **our own** runner. It reads `tests/manifest.json`, compiles each test's sources with our toolchain, and evaluates
  the recorded checks. It does not use `driver.py`.
- [ ] Map each assertion kind to our implementation: stdout/stderr golden compare, `All Finished` detection, exit codes,
  trace compare.
- [ ] Track pass/waive/fail per tier, and run continuously in CI to report progress.

### Phase 11: Architecture → `20-architecture.md`

This phase draws on the findings above but is designed independently. Key questions:

- [ ] **Proc macro vs build script.** SystemVerilog can't be written inline as Rust tokens. `4'b1010` lexes as a Rust lifetime,
  `` ` `` is not a Rust token, comments (and so `/*verilator*/` metacomments) are dropped, and `'0` is an unterminated char literal.
  So the macro must take a *path* (for example `#[verilog(file = "top.sv", top = "top")]`) and run its own lexer.
  Track file dependencies for rebuilds (`include_bytes!` trick vs `proc_macro::tracked_path`). Measure the macro's
  compile-time cost on large designs, and consider caching the elaborated IR across invocations.
- [ ] **Value representation.** Const-generic bit vectors (`Bits<N>`), 2-state vs 4-state planes, and SIMD for wide values.
- [ ] **Scheduling strategy.** Our own approach, derived from the LRM semantics in `05`.
- [ ] **Performance opportunities a different architecture enables:** monomorphised Rust plus LLVM, batch/multi-instance
  simulation (many stimuli per model), and incremental compilation of the design hierarchy.
- [ ] **Benchmark suite** and success metrics against the Phase 8 baselines.

## Ordering and dependencies

```
Phase 0 ─▶ Phase 1 (manifest) ─┬─▶ Phase 2 (features) ─▶ Phase 3 (grammar)
                               ├─▶ Phase 4 (diagnostics)
                               ├─▶ Phase 5 (semantics) ◀── Phase 8 (examples)
                               ├─▶ Phase 6 (API) ─▶ Phase 7 (formats)
                               └─▶ Phase 9 (options) ─▶ Phase 10 (runner)
                                                     all ─▶ Phase 11 (architecture)
```

Phase 1 comes first because every later doc uses the manifest to rank work by how many tests it unlocks.

## Decisions

Decided 2026-10-02:

1. **Diagnostic text: not matched.** T2 tests pass when the outcome (fail or succeed), the warning or error *code*, and the
   source location agree. We write our own message prose. The runner compares golden `.out` files only after normalising
   them to `(severity, code, file, line, col)` tuples.
2. **C++ harness tests (about 250): waived for now.** They are tagged `harness-cpp` in the manifest. Later we will write
   **Rust equivalents** of each harness against our own model API. Phase 6 therefore records *what each harness does*
   (the stimulus it drives, what it checks, which API capabilities it needs), so the Rust ports can be written from that alone.
   A C-ABI shim is not planned.
3. **Tests that depend on an exact RNG stream: waived.** These are tests whose expected output depends on Verilator's
   specific random sequence. Phase 1 tags them `seed-sensitive`. Self-checking randomisation tests (ones that only check the
   constraints hold) stay in scope.
4. **SystemC (`--sc`): deferred.** Tests are tagged `systemc` and waived.

5. **UVM (`t/uvm/`): a long-term milestone.** UVM tests are tagged `uvm` in the manifest and are not on the critical path.
   The checkpoints, in order, are:
   1. The non-DPI UVM 2017-1.0 library preprocesses and parses.
   2. It elaborates.
   3. A UVM hello-world runs (`run_test()`, phasing, `uvm_info`).
   4. The UVM tests in `test_regress` pass.

   These depend on the class, parameterisation, randomize and `--timing` work being done first.

   **Alternative to explore: a Rust-native verification library.** We may also offer a UVM-*like* methodology written in
   Rust. Testbenches would be Rust code driving the generated model directly, using Rust traits for components, async tasks
   for processes and timing, and Rust crates for constrained randomisation. It is an option to explore, not a replacement for
   SystemVerilog UVM support. It also overlaps with the planned Rust ports of the C++ harnesses (decision 2), so the two
   should share one model API. Design notes will go in `21-rust-verification.md`. That doc must be designed from the IEEE
   1800.2 concepts, not by porting UVM source.

## Provenance

The survey counts came from `ls`/`grep` over `../verilator/test_regress/t` and `../verilator/LICENSES`, plus reading
`REUSE.toml`, `docs/guide/copyright.rst`, the root `AGENTS.md`, and one sample test driver (`t_case_huge.py`).
No compiler or runtime source was read.
