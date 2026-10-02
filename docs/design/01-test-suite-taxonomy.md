# 01: Test-suite taxonomy

> **⚠️ DO NOT COPY CODE.** See the [banner in README.md](README.md).

This document describes how we classify Verilator's regression tests, and how
our own runner uses that classification to measure progress. It is Phase 1 of
the [analysis plan](00-analysis-plan.md).

## The manifest

`tools/manifest.py` writes `tests/manifest.json`. The file is git-ignored,
because it is generated from the Verilator checkout.

- **How it reads drivers:** each `t_*.py` driver is parsed with Python's `ast`
  module. It is **never executed or imported**, and Verilator's `driver.py`
  and `vltest_bootstrap` are not used. The extractor records each
  `test.<call>(...)` with its literal arguments, plus assignments such as
  `test.top_filename = ...`.
- **Helper modules:** calls into helpers in `t/`, such as
  `coverage_covergroup_common.run(test)`, are followed into the helper's
  function, which is also parsed as data. 179 drivers work this way.
- **What it records for each test:**
  - scenarios;
  - top file;
  - golden file;
  - every harness call and its arguments;
  - flattened command-line flags;
  - the top file's SPDX licence;
  - tags;
  - a tier.

```console
$ python3 tools/manifest.py        # reads ../verilator/test_regress/t
```

### Tiers

| Tier | Meaning | Target |
|---|---|---|
| **T1** | Runs a simulation and checks itself (`*-* All Finished *-*`) | Must pass |
| **T1-compile** | Compiles or lints, is expected to succeed, and has no golden file | Must pass |
| **T2** | Expected diagnostics: fails, or lints against a golden `.out` | Must match (code and location, decision 1) |
| **T3** | Verilator features: `--public`, tracing, coverage, `-E`, C++ harnesses, SystemC, UVM | Case by case |
| **T4** | Checks Verilator internals: `--stats`, tree dumps, generated C++, `dist` | Waived |
| **other** | No compile, lint or execute call found | Reviewed by hand |

Tier rules, applied in order:
1. **T4:** `dist`, `internal-stats`, `internal-objdir`, `internal-dump`.
2. **T3:** `harness-cpp`, `systemc`, `uvm`, `vlt-feature`, `trace`.
3. **T2:** `expect-fail`, or lint with a golden file.
4. **T1:** `execute`.

Long options with one dash (`-trace`, `-sc`) are normalised before matching.

### Snapshot (Verilator `bdfb2e8`, 2026-10-02)

| | Count |
|---|---|
| Tests | 4,447 (all drivers parsed) |
| T1 / T1-compile / T2 / T3 / T4 / other | 2,139 / 321 / 923 / 501 / 473 / 90 |
| Top-file licence | CC0 3,917 · LGPL 264 · none found 210 · Unlicense 28 · other 28 |
| Tags | execute 2,925 · golden 1,213 · lint 1,041 · expect-fail 895 · vlt-feature 357 · harness-cpp 277 · timing 217 · systemc 25 · uvm 15 |

## The runner

`paragon-runner` (`runner/`) reads the manifest and runs each test through
`libparagon` from `test_regress/`, so paths like `t/t_foo.v` resolve.

- **Inputs from the driver's flags:**
  - `-D` / `+define+` become defines;
  - `+incdir+` / `-I` become include directories;
  - `--language` and `+<lang>ext+` set the language;
  - extra `.v`/`.sv` files are compiled after the top file.
- **Defines the driver normally passes:** `TEST_OBJ_DIR` and `TEST_DUMPFILE`.
- **`-E` tests:** preprocessed, and the text is compared with the golden file.
- **Not handled yet:** `.vlt` control files and `-f` option files are reported
  as `setup`.

```console
$ cargo run --release -p paragon-runner                       # summary by tier
$ cargo run --release -p paragon-runner -- --tier T1 --list not-yet
$ cargo run --release -p paragon-runner -- --json results.json
```

### Outcomes

| Outcome | Meaning |
|---|---|
| `pass` | Simulated and finished, or `-E` output matches the golden byte for byte |
| `pass-diagnostic` | Expected to fail, and our first diagnostic is at the golden's first location |
| `failed-as-expected` | Expected to fail and did, but there is no golden to compare with |
| `reached-elaborate` | Preprocessed and parsed cleanly; stopped at the first stage not yet built |
| `wrong-output` | `-E` output differs from the golden |
| `wrong-diagnostic` | Expected to fail; our first diagnostic is somewhere else |
| `front-end-error` | We reported an error on a test that should compile: a bug, or a flag we don't model |
| `not-yet` | Stopped at a construct Paragon doesn't support yet (`NOTYET`), named in the report |
| `setup` | Inputs or flags we can't handle yet (`.vlt`, `-f`, missing file) |
| `waived` | Out of scope by decision: T4, C++ harnesses, SystemC, `dist` |

### Snapshot (2026-10-02: preprocessor, lexer and parser)

| Tier | pass | pass-diag | as-expected | reached-elaborate | wrong-output | wrong-diag | front-end-error | not-yet | setup | waived | total |
|---|---|---|---|---|---|---|---|---|---|---|---|
| T1 | 0 | 0 | 0 | **1,399** | 0 | 0 | 3 | 722 | 14 | 1 | 2,139 |
| T1-compile | 0 | 0 | 0 | 242 | 0 | 0 | 5 | 63 | 11 | 0 | 321 |
| T2 | 0 | 13 | 1 | 500 | 0 | 37 | 3 | 339 | 30 | 0 | 923 |
| T3 | 2 | 2 | 0 | 157 | 6 | 0 | 3 | 77 | 6 | 248 | 501 |
| T4 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 473 | 473 |

- **Ready for later stages:** 65% of T1 tests get through the whole front end,
  waiting for elaboration and simulation.
- **Not-yet constructs:** the main ones by test count are classes (684),
  concurrent assertions (102), virtual classes and interfaces (65),
  covergroups (60), clocking blocks (35), user-defined primitives (26) and
  properties (26).
- **The 37 wrong diagnostics:** mostly *where* errors are reported.
  - Verilator points at an operand: an `` `include `` filename (column 10), or
    the end of an `` `ifdef `` expression.
  - It points past end of file for unterminated comments and strings.
  - It points at the start of a construct for some parser errors.
  - We point at the directive or the token where parsing failed.

  This is in the work-in-progress list.
