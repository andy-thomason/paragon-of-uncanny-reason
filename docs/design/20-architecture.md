# 20: Architecture

> **⚠️ DO NOT COPY CODE.** See the [banner in README.md](README.md).
> This design is our own, worked out from the IEEE 1800-2023 simulation
> semantics. It is not derived from Verilator's implementation.

## 1. Pipeline: separate representations, not one mutating tree

### 1.1 What Verilator does (public, high level only)

Verilator's own `AGENTS.md` describes its compiler as "an ordered sequence of
passes over one shared AST". One tree is parsed, linked, width-checked,
transformed, scheduled, and finally emitted as C++. Its file names also show
graph structures built on the side for particular jobs, such as ordering,
scheduling and a dataflow optimiser; results go back into the tree.
We deliberately go no further than this. `docs/internals.rst` and `src/` stay
off-limits under the clean-room rules.

### 1.2 What we do instead

Each stage has its own representation, and data flows one way:

```
source text
  │  preprocessor (src/pp)             tokens: &str slices
  ▼
pp tokens ──► lexer (src/lex.rs) ──► parser (src/parse) ──► AST (src/ast.rs)
                                                              │  elaboration
                                                              ▼
                                                     IR: Design (src/ir.rs)
                                                       │                │
                                    reference interpreter        code generator
                                       (step 3)               (Rust: build.rs / proc macro)
```

| Representation | Holds | Never holds |
|---|---|---|
| **AST** (`src/ast.rs`) | What was written. Every name, literal, operator and keyword is a `&'a str` slice | Types, resolved names, evaluated parameters |
| **IR** (`src/ir.rs`) | What it means: resolved IDs, typed expressions, an evaluated hierarchy, processes with sensitivity | Syntax that doesn't change behaviour |

The AST is **immutable** once parsed. Elaboration reads it and builds the IR.
It never rewrites the AST.

### 1.3 Why

1. **Each stage can be tested on its own.** Parser tests check the AST, and
   elaboration tests check the IR. That's how the parse sweep and the
   preprocessor goldens work today.
2. **One IR, two consumers.** The reference interpreter and the code generator
   both read the same IR. Running both on every T1 test checks the fast path
   against the slow, simple one.
3. **Caching and incremental builds.** An immutable AST per file, and an IR per
   design, are natural units to cache. That matters for a proc macro, which is
   re-run on every `cargo build`.
4. **Positions without spans.** Both trees keep `&'a str` slices for names and
   diagnostic sites, so any error can still be traced to its source line,
   through macro expansions.
5. **Different from Verilator by construction.** This is the architectural
   difference the project is built on.

## 2. The IR (`src/ir.rs`)

### 2.1 Shape

A `Design` is a set of index-addressed tables:

| Table | Element | ID |
|---|---|---|
| `scopes` | Instance, generate block, named block or package | `ScopeId` |
| `vars` | Variable, net, port or evaluated parameter | `VarId` |
| `types` | Resolved type | `TypeId` |
| `procs` | Process: `initial`, `always*`, `final`, continuous assignment | `ProcId` |
| `funcs` | Function or task | `FuncId` |

Using IDs instead of references keeps the IR easy to build up step by step, to
cache, and to hand to a code generator.

### 2.2 Decisions in the IR

- **Elaboration applies the width rules once.** IEEE 1800 §11.6–11.8 sizes
  expressions from their context. Elaboration works out every operand's width
  and signedness and inserts explicit `Resize { extend: Zero | Sign | Truncate }`
  nodes, so every IR expression is self-determined. Back ends never repeat
  this, which is the most error-prone part of the language.
- **Packed types are bit vectors.** `logic [7:0]`, `int`, packed structs and
  enums all become `Type::Bits { width, signed, four_state, .. }`. Field and
  enum-name tables are kept for `%p`, tracing and debugging. A member access
  on a packed struct becomes a `Select`.
- **One kind of select.** `a[i]`, `a[7:4]`, `a[i+:4]`, `a[i-:4]` and packed
  members all become `Select { base, lsb, width }`, with the declared range
  offset already applied.
- **Typed operators.** `UnOp` and `BinOp` enums replace the AST's operator
  strings. Signedness comes from the operand types, so there's one `Lt`.
- **Fewer statement forms.** `for` and `foreach` become a block containing a
  `While`. Compound assignments (`+=`) become plain ones. `unique`/`priority`
  become a `CaseCheck` flag.
- **Everything that runs is a `Process`.** That includes continuous
  assignments. Each process has a `Sensitivity`: `None` (it runs once, or
  waits inside its body), `Events` (explicit `@(...)`), or `Reads` (computed
  for `always_comb`, `@*` and `assign`). The scheduler sees one kind of thing.
- **Values have two planes.** `Bits` holds `val` and an optional `unknown`
  plane: 2-state values carry no X/Z cost, and 4-state values are exact.
  Verilator is mostly 2-state; we can keep 4-state where the design asks for
  it, and that may be a behavioural difference to manage.

### 2.3 Not decided yet

- **Flatten, or specialise and share?** The IR is a flattened design: one
  `vars` table for every instance. The alternative is one specialised module
  body per unique parameter set, with instances sharing it. That gives smaller
  code and faster compiles for large regular designs, and is a natural fit for
  batch simulation. Decide this at step 4 with benchmarks.
- **Multiple drivers and tristates.** `NetResolution` records the net kind.
  The resolution semantics (strengths, `z`) come with `05-simulation-semantics.md`.
- **Classes, queues and strings at run time.** The IR has the types; object
  handles and garbage collection are not designed yet.

## 3. Back ends (to be decided at step 4)

- **Reference interpreter (step 3):** walks the IR directly. It should be
  simple and obviously correct, not fast.
- **Code generator:** emits Rust from the IR, through `build.rs` or a proc
  macro (`#[verilog(file = "top.sv")]`; source can't be inline, see
  `00-analysis-plan.md` Phase 11). The performance ideas to benchmark:
  monomorphised `Bits<N>` for widths known at compile time, static scheduling
  from `Sensitivity`, and many stimuli per compiled design.
