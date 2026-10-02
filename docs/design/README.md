# Paragon of Uncanny Reason: Design Documents

> # ⚠️ DO NOT COPY CODE ⚠️
>
> **Do not copy code, algorithms, data structures, grammar rules, or comments from Verilator into this project.**
>
> Verilator is © Wilson Snyder and contributors. It is licensed under LGPL-3.0-only OR Artistic-2.0.
> This project is a **clean-room rewrite** with a **different architecture**. That means:
>
> - **No** Verilator source (`src/`, `include/`, `bin/`, `test_regress/driver.py`, `verilog.y`, `verilog.l`, `V3PreLex.l`)
>   may be pasted, transliterated, ported or "translated to Rust", in whole or in part.
> - **No** design document may describe *how* a Verilator pass is implemented internally.
>   We record **what** Verilator does when observed from outside (inputs, outputs, messages, options), never **how** its code does it.
> - Our formal grammar is derived from **IEEE 1800-2023 Annex A**. It is **not** derived from `verilog.y`.
> - Our runtime does **not** link against or reproduce `include/verilated*`.
> - If you have read a piece of Verilator implementation code, you do not write the matching part of our implementation.
>   Hand it to someone who has not read it (see [Clean-room rules](#clean-room-rules)).
>
> If in doubt, **stop and ask**.

## Purpose

We are building a SystemVerilog-to-native simulator in Rust. It uses a procedural macro, possibly alongside a `build.rs` front end,
to turn SystemVerilog into Rust code at compile time. The aims are:

1. **Behavioural compatibility.** It should pass the externally observable behaviour checked by the Verilator regression suite
   (see [the analysis plan](00-analysis-plan.md) for exactly what "pass" means).
2. **Performance.** It should beat Verilator by using a different architecture, not by doing the same thing faster.
3. **Licence hygiene.** It must be independently written and independently licensable.

The Verilator checkout lives next to this repo at `../verilator`. It is pinned to commit
`bdfb2e8db16bf59dd6bc9b1f4009d30a11510888` (2026-10-01). We use it only as a **black box** and as a **test corpus**.

## Clean-room rules

| Source | Status | How we may use it |
|---|---|---|
| IEEE 1800-2023 LRM (SystemVerilog), IEEE 1364 (Verilog), IEEE 1800.2 (UVM) | ✅ Primary authority | Read freely. Cite clause numbers. |
| Verilator **user** guide (`docs/guide/*.rst`) | ✅ Allowed | Gather facts: which features, options, warnings and extensions exist. Paraphrase; never paste prose. |
| Running the `verilator` binary and inspecting its output | ✅ Allowed (black box) | Run experiments and record observed behaviour. Show generated C++ only as short illustrative excerpts in analysis docs, never as a template. |
| Test Verilog marked `SPDX-License-Identifier: CC0-1.0` (3,322 files) | ✅ Public domain | May be used as test inputs. Keep the provenance header. |
| Test Verilog marked `Unlicense`, `BSD-3-Clause`, `ISC`, `Apache-2.0` (UVM) | ⚠️ Permissive | May be used, but keep the licence notices. |
| Test Verilog marked LGPL (240 files) and **all** `.py` test drivers | ⚠️ Read-only spec | Run them **in place** from `../verilator`. Never vendor them into this repo. Reading a driver to learn *what it checks* is fine. |
| `docs/internals.rst` | 🚫 Restricted | Describes Verilator's internal algorithms. Implementers must not read it. Analysts must not summarise its methods. |
| `src/**`, `include/**`, `test_regress/driver.py`, `bin/**` | 🚫 Forbidden for design | Do not read these for algorithms. The only allowed use is mechanical fact extraction (for example, counting option names) by an analyst, recorded as a plain list. |

**Two roles.** An *analyst* gathers facts from the allowed sources and writes them in their own words in `docs/design/`.
An *implementer* writes code in this repo and reads **only** `docs/design/`, the IEEE standards, and test inputs and expected outputs.
Each design doc ends with a **Provenance** section that lists the sources used.

## Document index

| # | Document | Status |
|---|---|---|
| WIP | [Work in progress](work-in-progress.md): current status, next steps and open items | Living |
| 00 | [Analysis plan](00-analysis-plan.md): how we gather everything needed to pass the test suite | Draft |
| 01 | [Test-suite taxonomy](01-test-suite-taxonomy.md): classification of every regression test, and the runner | Draft |
| 02 | `02-feature-inventory.md`: language features mapped to LRM clauses and tests | Planned |
| 03 | [Grammar](03-grammar.md): formal grammar (preprocessor and language) with Verilator specialities | Draft (probes pending) |
| 04 | `04-verilator-extensions.md`: metacomments, config files, `$c`, Verilator-only system tasks | Planned |
| 05 | `05-simulation-semantics.md`: observable scheduling, 2-state/X rules, timing and display formatting | Planned |
| 06 | `06-runtime-api.md`: the C++ harness API surface the tests use, plus DPI/VPI | Planned |
| 07 | `07-transpile-examples.md`: black-box examples of Verilog → Verilator C++, with our Rust equivalent | Planned |
| 08 | `08-output-formats.md`: VCD/FST/SAIF traces, coverage data, JSON, `-E` output | Planned |
| 09 | `09-diagnostics.md`: warning and error catalogue and message format | Planned |
| 10 | `10-cli-options.md`: command-line options classified by effect | Planned |
| 11 | `11-test-runner.md`: our clean-room test runner design | Planned |
| 20 | [Architecture](20-architecture.md): pipeline, AST vs IR, IR design, back ends | Draft (§1–2) |
| 21 | `21-rust-verification.md`: exploratory Rust-native, UVM-like verification library | Exploratory |
