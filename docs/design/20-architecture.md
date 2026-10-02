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

There are two representations, a tree for syntax and a linear IR for
meaning, and data flows one way:

```
source text
  │  preprocessor (src/pp)              tokens: &str slices
  ▼
pp tokens ──► lexer (src/lex.rs) ──► parser (src/parse) ──► AST (src/ast.rs)
                                                              │  elaboration + lowering
                                                              ▼
                                                 linear IR: Design (src/ir.rs)
                                                   │                    │
                                      reference interpreter      code generator
                                         (step 3)            (Rust: build.rs / proc macro)
```

| Representation | Shape | Holds |
|---|---|---|
| **AST** (`src/ast.rs`) | Tree | What was written. Every name, literal, operator and keyword is a `&'a str` slice; nothing is resolved or typed |
| **IR** (`src/ir.rs`) | Control-flow graphs of basic blocks, with three-address instructions on SSA values | What it means: a flattened hierarchy, resolved storage, explicit widths, explicit resume points |

The AST is **immutable** once parsed. Elaboration resolves names, evaluates
parameters and generate constructs, and lowers each process and function
straight into blocks. It applies the width rules as it goes. There is no
intermediate typed tree. Parameter values are computed by lowering the
constant expression and running it on the interpreter, so constant folding
and simulation share one evaluator.

### 1.3 Why

1. **Suspension needs explicit resume points.** A process can suspend part way
   through nested statements (`#5`, `@(posedge clk)`, `wait`). In a linear IR
   every resume point is a block, which the interpreter resumes at and the
   code generator turns into a state of a `loop { match state { ... } }`
   coroutine. A tree would have to rediscover this in each back end.
2. **Scheduling and dataflow analysis want a CFG.** Read and write sets,
   combinational dependency order, NBA handling and dead-code removal are
   ordinary analyses over blocks and SSA values.
3. **One IR, two consumers.** The reference interpreter and the code generator
   both read the same IR. Running both on every T1 test checks the fast path
   against the simple one.
4. **Each stage can be tested and cached on its own.** That matters for a proc
   macro, which runs on every `cargo build`.
5. **Positions without spans.** Instructions keep an `at: &'a str` slice for
   run-time diagnostics, so errors trace back through macro expansions to the
   source line.
6. **Different from Verilator by construction:** a syntax tree, then a separate
   linear IR, rather than one tree transformed in place.

## 2. The IR (`src/ir.rs`)

### 2.1 Design level

A `Design` is a set of index-addressed tables:

| Table | Element | ID |
|---|---|---|
| `scopes` | Instance, generate block, named block or package; for names only | `ScopeId` |
| `vars` | Design state: a net or static variable, flattened across instances | `VarId` |
| `types` | Resolved type; every packed type is `Bits { width, signed, four_state }` | `TypeId` |
| `procs` | A process: `initial`, `always*`, `final`, continuous assignment | index |
| `funcs` | A function or task | `FuncId` |
| `formats` | A parsed `$display` format string | `FormatId` |

### 2.2 Bodies, blocks and values

Each process and function has a `Body`, which is a control-flow graph:

- A `Block` holds parameters (values passed in by jumps, instead of phi
  nodes), a list of `Inst`s, and one `Terminator`.
- Each `Inst` defines at most one SSA `Val`, and every value has a type.
- The terminators are `Jump`, `Branch`, `Switch` (for exact `case`), `Return`,
  `Suspend { wait, resume }`, `Fork { children, join, resume }`,
  `EndThread`, `Finish` and `Unreachable`.
- `Wait` is one of: `Delay`, `Inactive` (`#0`), `AnyChange(vars)`,
  `Edge(var, Pos|Neg|Both)`, `Event`, or `Children` (`wait fork`). An event
  control on an arbitrary expression lowers to `AnyChange` on what the
  expression reads, plus a compare loop.

There are three storage classes:

| Storage | Lives | Accessed by |
|---|---|---|
| `VarId`: design state | the whole simulation | `Load`, `Store`, `NbaStore`, `LoadElem`, `StoreElem`. The scheduler watches these |
| `SlotId`: frame slot (automatic locals, loop counters) | one activation | `LoadSlot`, `StoreSlot` |
| `Val`: SSA temporary | where it is defined, or passed on as a block parameter | instruction operands |

Locals start as slots so that lowering stays simple. A later pass can promote
them to SSA values, as `mem2reg` does.

Every process is a coroutine, and `ProcKind` is only a hint. Each body records
the design state it `reads` and `writes`. That lets the scheduler run
combinational processes in dependency order instead of as coroutines when it
can, without the IR depending on it.

### 2.3 Example

```systemverilog
integer cyc = 0;
always @(posedge clk) begin
  if (cyc == 9) $finish;
  cyc <= cyc + 1;
end
```

lowers to (`cyc` is 32-bit signed 4-state; `clk` is 1-bit):

```
bb0:                                   ; entry: wait for the clock
    suspend Edge[(clk, Pos)] -> bb1
bb1:
    %0 = load cyc                      : integer
    %1 = const 32'sd9                  : integer
    %2 = binary Eq %0, %1              : logic
    branch %2, bb2(), bb3()
bb2:
    finish Finish
bb3:
    %3 = load cyc                      : integer
    %4 = const 32'sd1                  : integer
    %5 = binary Add %3, %4             : integer
    nba_store cyc, %5
    jump bb0()
```

`always_comb` and `assign` lower to "compute, store, then
`suspend AnyChange(reads) -> entry`".

### 2.4 Decisions in the IR

- **Elaboration applies the width rules once.** IEEE 1800 §11.6–11.8 sizes an
  expression from its context. Lowering works out each operand's width and
  signedness and emits explicit `Resize { extend: Zero | Sign | Truncate }`, so
  each instruction's operands already have the right types.
- **One kind of select.** Every bit select, part select and packed member
  access becomes `Select { value, lsb, width }`, or `part` on a store, with the
  declared range offset already applied.
- **Typed operators.** `UnOp` and `BinOp` enums. Signedness comes from the
  operand types.
- **Short-circuit operators become control flow.** `&&`, `||` and `?:` with
  side effects lower to branches. A side-effect-free `?:` may use `Mux`, which
  also handles the LRM's X-merge rule.
- **Values have two planes.** `Bits` holds `val` and an optional `unknown`
  plane, so 2-state values cost nothing extra and 4-state values are exact.

### 2.5 Not decided yet

- **Flatten, or specialise and share?** The IR is flattened: one `vars` table
  for all instances, and one process per instance. The alternative is one body
  per unique parameter set, shared by instances and taking the instance's
  state as an argument. That gives smaller code and faster compiles, and suits
  batch simulation. Decide at step 4 with benchmarks.
- **Fork frames.** Children are blocks in the parent's body and share its
  slots, which matches the LRM's sharing of automatic variables. A
  `fork ... join_none` child that outlives its parent's frame needs that frame
  kept alive, which is still open.
- **Multiple drivers, strengths and tristates:** with `05-simulation-semantics.md`.
- **Classes, queues and strings at run time:** object handles and lifetimes.

## 3. Back ends (to be decided at step 4)

- **Reference interpreter (step 3):** walks the IR directly. It should be
  simple and obviously correct, not fast.
- **Code generator:** emits Rust from the IR, through `build.rs` or a proc
  macro (`#[verilog(file = "top.sv")]`; source can't be inline, see
  `00-analysis-plan.md` Phase 11). The performance ideas to benchmark:
  monomorphised `Bits<N>` for widths known at compile time, static scheduling
  from `Sensitivity`, and many stimuli per compiled design.
