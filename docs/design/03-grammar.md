# 03: Grammar (SystemVerilog as accepted by Verilator)

> **⚠️ DO NOT COPY CODE.** See the [banner in README.md](README.md).
> This grammar is written **from IEEE 1800-2023** (Annex A *Formal syntax*, Annex B *Keywords*, Clause 22 *Compiler directives*).
> Verilator-specific behaviour comes **only** from the Verilator *user guide* and the *observable* behaviour of its test suite.
> `verilog.y`, `verilog.l` and `V3PreLex.l` were **not** opened while writing this document. Don't open them while working on it.

## 0. Scope, sources and conventions

### 0.1 What this document is

This is a formal grammar for the language our front end must accept so that it matches Verilator's accepted language.
It is:

- **IEEE 1800-2023**, restated in our own EBNF. Rules are reorganised and merged for a hand-written parser, and each section
  cites the Annex A clause it covers. We have deliberately *not* transcribed the BNF production by production. When a detail
  matters, the LRM is the authority, and the clause references show where to look.
- **plus Verilator specialities**, tagged `[VLT]`: metacomments, control files, `$c`, the `` `systemc_* `` blocks and extra
  predefines.
- **minus what Verilator rejects**, tagged `[UNSUP]`. A test that expects Verilator to fail on an unsupported construct passes
  for us if we also reject it. We may later choose to *support* such constructs, which is a strict improvement, but then
  those `_unsup` tests become waivers.
- **with constructs Verilator parses and then ignores** tagged `[IGN]`, such as specify blocks and timing checks.

### 0.2 Evidence base

| Evidence | What it gave us |
|---|---|
| IEEE 1800-2023 Annex A/B, Clause 22 | The base grammar, keywords and directives |
| `docs/guide/extensions.rst` | The list of `[VLT]` directives, metacomments and system tasks |
| `docs/guide/languages.rst` | `[UNSUP]`/`[IGN]` statements, language-mode list, keyword limitations |
| `docs/guide/control.rst` | The control-file (`.vlt`) command grammar |
| Survey of the 3,656 test sources in `test_regress/t` (a counting script of our own) | Frequency of every metacomment, directive and system task. This shows what tests actually exercise. |
| Survey of the 1,572 golden `.out` files | 75 syntax-error expectations; about 300 distinct `Unsupported:` diagnostics; preprocessor error expectations; the `--dump-defines` predefine list |

**Not yet done:** black-box probing of the `verilator` binary. It could not be built on this machine: there is no
Homebrew or autoconf, and the system `bison` 2.3 is too old. Points that need a probe are marked **(probe)**, and
[§17](#17-open-items-needing-black-box-probes) collects them.

### 0.3 Notation

```
rule      ::= alternative | alternative        -- choice
x?        -- optional          x*  -- zero or more        x+  -- one or more
( ... )   -- grouping          'kw' / ";"  -- terminal
list(X)   ::= X ( "," X )*                     -- comma-separated list (helper)
IDENT, NUMBER, STRING ...                      -- lexical tokens (§2)
[VLT] Verilator extension   [UNSUP] Verilator rejects   [IGN] parsed then ignored   [LRM a.b.c] Annex A clause
```

### 0.4 Layers

```
bytes ──▶ L1 preprocessor lexer ──▶ L2 preprocessor (macros, `ifdef, `include) ──▶ preprocessed text (+ `line markers)
      ──▶ L3 language lexer (keywords per mode, metacomment tokens) ──▶ L4 parser ──▶ L5 elaboration-time disambiguation
```

The layers are kept separate on purpose:

- `-E` output (19 tests) must be producible from L2 alone.
- Metacomments must survive L2 so that L3 can see them. Ordinary comments are dropped at L2 unless `--pp-comments` is given.
- `` `verilator_config `` and `` `systemc_* `` switch L3 into other sub-languages (§3.9).

---

## 1. Language modes and keyword sets

### 1.1 Selecting a mode

The mode decides which words are **reserved keywords**. Precedence, highest first:

1. `` `begin_keywords "<version>" `` … `` `end_keywords `` (a nesting stack; [LRM 22.14])
2. The per-file suffix map: `+<std>ext+<suffix>`, for example `+1364-2005ext+v`, `+systemverilogext+v`
3. `--language <version>` / `--default-language <version>`
4. The default: 1800-2023

Version strings seen in tests: `1364-1995`, `1364-2001`, `1364-2001-noconfig`, `1364-2005`, `1800-2005`, `1800-2009`,
`1800-2012`, `1800-2017`, `1800-2023`, `VAMS-2.3`, `1800+VAMS`. The value `--language 1-2-3-4` appears in a negative test,
so invalid versions must be diagnosed.

### 1.2 Keyword deltas

Each mode reserves every keyword of the modes before it, plus these additions (from Annex B and IEEE 1364):

| Mode | Adds |
|---|---|
| 1364-1995 | The Verilog-95 set: `always and assign begin buf bufif0 bufif1 case casex casez cmos deassign default defparam disable edge else end endcase endfunction endmodule endprimitive endspecify endtable endtask event for force forever fork function highz0 highz1 if ifnone initial inout input integer join large macromodule medium module nand negedge nmos nor not notif0 notif1 or output parameter pmos posedge primitive pull0 pull1 pulldown pullup rcmos real realtime reg release repeat rnmos rpmos rtran rtranif0 rtranif1 scalared small specify specparam strong0 strong1 supply0 supply1 table task time tran tranif0 tranif1 tri tri0 tri1 triand trior trireg vectored wait wand weak0 weak1 while wire wor xnor xor` |
| 1364-2001-noconfig | `automatic endgenerate generate genvar localparam noshowcancelled pulsestyle_ondetect pulsestyle_onevent showcancelled signed unsigned` |
| 1364-2001 | the above plus `cell config design endconfig incdir include instance liblist library use` |
| 1364-2005 | `uwire` |
| 1800-2005 | The SystemVerilog core: `alias always_comb always_ff always_latch assert assume before bind bins binsof bit break byte chandle class clocking const constraint context continue cover covergroup coverpoint cross dist do endclass endclocking endgroup endinterface endpackage endprogram endproperty endsequence enum expect export extends extern final first_match foreach forkjoin iff ignore_bins illegal_bins import inside int interface intersect join_any join_none local logic longint matches modport new null package packed priority program property protected pure rand randc randcase randsequence ref return sequence shortint shortreal solve static string struct super tagged this throughout timeprecision timeunit type typedef union unique var virtual void wait_order wildcard with within` |
| 1800-2009 | `accept_on checker endchecker eventually global implies let nexttime reject_on restrict s_always s_eventually s_nexttime s_until s_until_with strong sync_accept_on sync_reject_on unique0 until until_with untyped weak` |
| 1800-2012 | `implements interconnect nettype soft` |
| 1800-2017 | (none) |
| 1800-2023 | (none). 2023 changes are syntactic, for example triple-quoted strings. |
| VAMS / 1800+VAMS | The Verilog-AMS words. Verilator *implements* only `ceil exp floor ln log pow sqrt string wreal`, per the guide. The rest are reserved, and using one gives `Unsupported: AMS reserved word not implemented: '<w>'` `[UNSUP]`. Observed examples: `above abs absdelay abstol ac_stim access acos acosh aliasparam analog analysis branch connect connectmodule connectrules continuous ddt ddt_nature ddx discipline discrete domain driver_update endconnectrules enddiscipline endnature endparamset exclude final_step flicker_noise flow from ground idt idt_nature idtmod inf initial_step laplace_nd laplace_np laplace_zd laplace_zp last_crossing limexp max merged min nature net_resolution noise_table paramset potential resolveto slew split timer transition units white_noise zi_nd zi_np zi_zd zi_zp` |

**Rule:** a word that is not reserved in the current mode is an ordinary identifier. For example, `logic` is a legal net name
under `` `begin_keywords "1364-2005" ``. Tests exercise this.

---

## 2. Lexical grammar [LRM 5, A.8.7–A.8.8, A.9]

### 2.1 Whitespace and comments

```
WS            ::= ( " " | "\t" | "\n" | "\r" | "\f" )+
LINE_COMMENT  ::= "//" (any char except "\n")*
BLOCK_COMMENT ::= "/*" (any char)* "*/"          -- no nesting; EOF inside it is an error
```

Error observed in golden files: EOF inside a block comment is reported at the EOF position.

### 2.2 Metacomments [VLT]

Some comments are **tokens**, not whitespace:

```
META_COMMENT  ::= "/*" WS? "verilator" WS META_BODY WS? "*/"
                | "//" WS? "verilator" WS META_BODY ( "\n" | before-next "//" )
META_BODY     ::= META_KEYWORD ( WS META_ARG )*
```

- The `//` form ends at the newline **or at the next `//`**, so `// verilator lint_off WIDTH // reason` works (guide).
- Whitespace between `/*` and `verilator` is accepted. The corpus contains `/* verilator public_off*/`.
- A misspelt keyword (corpus: `lintt_off`) is a diagnostic, not a silent comment **(probe: which code)**.
- `` `coverage_block_off `` is a **predefined macro** that expands to `/*verilator coverage_block_off*/`. This is observed in
  the `--dump-defines` golden output, so the metacomment must survive macro expansion.

The full keyword list and where each may appear is in [§14](#14-metacomment-grammar-vlt).

### 2.3 Synthesis-pragma comments

```
PRAGMA_COMMENT ::= "//" WS? PRAGMA_PREFIX WS PRAGMA_WORD ...
PRAGMA_PREFIX  ::= "synopsys" | "synthesis" | "ambit synthesis" | "cadence" | "pragma"
PRAGMA_WORD    ::= "full_case" | "parallel_case" | "translate_off" | "translate_on" | ...
```

`full_case` and `parallel_case` are significant: they generate run-time assertions unless `--no-assert-case` is given.
The corpus also contains `synopsys parallel_full`, `ambit synthesis one_hot`, `cadence one_cold`, `synopsys enum` and
`pragma for`. These must be **accepted** **(probe: are they ignored or acted on?)**.
`translate_off`/`translate_on` appears in the corpus **(probe: does Verilator skip the enclosed text?)**.

### 2.4 Attribute instances [LRM 5.12]

```
attribute_instance ::= "(*" list(attr_spec) "*)"
attr_spec          ::= IDENT ( "=" constant_expression )?
```

These are accepted wherever the LRM allows them, and ignored `[IGN]`. The token `(*)` (as in `@(*)`) is **not** an
attribute start. The lexer must give `@ (*)` and `@*` priority.

### 2.5 Identifiers

```
SIMPLE_IDENT   ::= [a-zA-Z_] [a-zA-Z0-9_$]*        -- "$" allowed after the first char (corpus: a$b, normal$var)
ESCAPED_IDENT  ::= "\" (any printable non-WS)+ WS  -- terminating WS is not part of the name
SYSTEM_IDENT   ::= "$" [a-zA-Z0-9_$]+              -- $display, $test$plusargs, $c32
IDENT          ::= SIMPLE_IDENT | ESCAPED_IDENT
```

- `\foo ` and `foo` are the **same** identifier when the escaped form is a legal simple identifier [LRM 5.6.1].
- Identifiers are case-sensitive.

### 2.6 Numbers [LRM 5.7, A.8.7]

```
NUMBER          ::= integral_number | REAL_NUMBER
integral_number ::= DECIMAL_NUMBER | based_number | UNBASED_UNSIZED
DECIMAL_NUMBER  ::= UNSIGNED                               -- 32-bit signed
                  | SIZE? "'" [sS]? [dD] WS? ( UNSIGNED | [xXzZ?] "_"* )
based_number    ::= SIZE? "'" [sS]? [bB] WS? BIN_DIGITS
                  | SIZE? "'" [sS]? [oO] WS? OCT_DIGITS
                  | SIZE? "'" [sS]? [hH] WS? HEX_DIGITS
SIZE            ::= [1-9] [0-9_]*                          -- non-zero
UNSIGNED        ::= [0-9] [0-9_]*
BIN_DIGITS      ::= [01xXzZ?] [01xXzZ?_]*                  -- likewise OCT, HEX with their digit sets
UNBASED_UNSIZED ::= "'0" | "'1" | "'x" | "'X" | "'z" | "'Z"
REAL_NUMBER     ::= UNSIGNED "." UNSIGNED
                  | UNSIGNED ( "." UNSIGNED )? [eE] [+-]? UNSIGNED
TIME_LITERAL    ::= ( UNSIGNED | UNSIGNED "." UNSIGNED ) TIME_UNIT
TIME_UNIT       ::= "s" | "ms" | "us" | "ns" | "ps" | "fs"
```

- Whitespace is legal between size, base and digits (`8 'h FF`). It is **not** legal inside the `'` + base pair.
  A macro producing `8` followed by `'hFF` must lex as one number.
- Width limit: one golden reports `Unsupported: Width of number exceeds implementation limit`, which ties to
  `--max-num-width` **(probe: the default)**.
- Truncation and extension of over-long or short literals produce warnings (`WIDTH` family), not syntax errors.

### 2.6a Lexer decisions taken from Verilator's tests

- **Size and apostrophe must touch.** `8'hFF` is one literal, but after `#` a
  number is always a delay: Verilator documents `#100'b0` and `# 100 'b0` as
  `[#] [100] ['b0]` (`t_parse_delay`). Whitespace *after* the base is allowed
  (`'h FF`).
- **`global` is contextual.** It is reserved only before `clocking`, so
  `reg global;` is legal (`t_var_rsvd`).
- **Attribute instances are dropped** in the lexer. `(*)` and `(* )` inside
  `@(...)` are event controls, not attributes (`t_attr_parenstar`).
- **`` `pragma protect begin_protected `` envelopes are skipped** up to
  `` `pragma protect end_protected ``.
- **Mixed headers:** Verilator accepts a module header that switches from
  non-ANSI to ANSI style part way (`t_clk_concat2`). We report it as `NOTYET`
  for now.

### 2.7 Strings [LRM 5.9]

```
STRING        ::= '"' ( STR_CHAR | ESCAPE )* '"'        -- no raw newline
TRIPLE_STRING ::= '"""' ( any char, may include newline and '"' )* '"""'   -- 1800-2023
ESCAPE        ::= "\n" | "\t" | "\\" | '\"' | "\v" | "\f" | "\a" | "\" OCT{1,3} | "\x" HEX{1,2} | "\" NEWLINE
```

Errors seen in golden files: EOF inside a `"""` string. Line continuation (`\` + newline) inside a normal string is legal.

### 2.8 Operators and punctuation

Longest match wins. The full set:

```
+ - * / % ** ++ -- ! ~ & ~& | ~| ^ ~^ ^~ << >> <<< >>> < <= > >= == != === !== ==? !=?
&& || -> <-> ? : = += -= *= /= %= &= |= ^= <<= >>= <<<= >>>= := :/ :: ; , . .* ' '{
( ) [ ] { } @ @@ # ## #-# #=# |-> |=> -> ->> => *> &&& $ (* *)
```

Context-sensitive points:

- `'{` starts an assignment pattern. `'(` is a cast, as in `type'(expr)` or `8'(x)`.
- `<=` is NBA or less-or-equal, decided by the parser (§16).
- `##` is a cycle delay in sequences. In a macro body, ``` `` ``` means token paste. The preprocessor handles that, so the
  two never conflict.
- `#-#` and `#=#` are followed-by operators [LRM 16.12.9].
- `@@` appears only in the `(… @@ …)` cover-group sampling syntax **(probe)**.

---

## 3. Preprocessor grammar [LRM 22]

### 3.1 Directive token

```
DIRECTIVE  ::= "`" SIMPLE_IDENT       -- a known directive, or a macro usage `NAME
```

A backtick in a string literal is literal text. Inside a macro *body*, it is significant only as `` `" ``, `` `\`" ``
or ``` `` ```.

### 3.2 Directive inventory

The test-corpus counts show priority:

| Directive | Tests use | Behaviour |
|---|---|---|
| `` `define `` / `` `undef `` / `` `undefineall `` | 2,613 / 24 / 4 | Full support, including function-like macros with defaults (§3.3) |
| `` `ifdef `` `` `ifndef `` `` `elsif `` `` `else `` `` `endif `` | 925 / 452 / 82 / 295 / 1,380 | Full support, including **1800-2023 expressions** (§3.4) |
| `` `include `` | 45 | `"file"` and `<file>` forms. Search order: the current file's directory if `--relative-includes`, then `-I`/`+incdir+` |
| `` `line `` | 15 | `` `line N "file" level ``. Special form: `` `line `__LINE__ "file" `` changes the filename only |
| `` `__FILE__ `` / `` `__LINE__ `` | 941 / 1,317 | Expand to a string literal or decimal number |
| `` `timescale `` | 103 | `` `timescale <n><unit> / <n><unit> ``, n ∈ {1, 10, 100} |
| `` `default_nettype `` | 24 | `wire tri tri0 tri1 wand triand wor trior trireg uwire none` |
| `` `resetall `` | 5 | Resets timescale, default_nettype and friends. Illegal inside a design element: one golden shows a syntax error at `` `resetall `` |
| `` `celldefine `` / `` `endcelldefine `` | 2 | Accepted, effectively `[IGN]` |
| `` `unconnected_drive pull0/pull1 `` / `` `nounconnected_drive `` | 6 / 2 | Affects unconnected input ports **(probe)** |
| `` `begin_keywords `` / `` `end_keywords `` | 19 / 12 | §1 |
| `` `pragma `` | 66 | `` `pragma <name> <args> ``. Includes `` `pragma protect ... ``: only `encoding = (enctype = "BASE64")` is recognised, otherwise `[UNSUP]` |
| `` `error "msg" `` | 36 | [VLT] Error at preprocessing time. It must be followed by a string. Golden: ``Expecting `error string`` |
| `` `uselib `` | 4 | `[IGN]` up to end of line |
| `` `accelerate `` `` `noaccelerate `` `` `delay_mode_* `` `` `expand_vectornets `` `` `noexpand_vectornets `` `` `autoexpand_vectornets `` `` `noremove_gatenames `` `` `noremove_netnames `` `` `remove_gatename `` `` `remove_netname `` `` `suppress_faults `` `` `nosuppress_faults `` `` `enable_portfaults `` `` `disable_portfaults `` `` `default_decay_time `` `` `default_trireg_strength `` `` `inline `` `` `portcoerce `` `` `noportcoerce `` | 1–3 each | Legacy (LRM Annex E optional directives). Accepted and ignored, or warned: one golden shows `Unsupported: Verilog optional directive not implemented` **(probe: which ones warn)** |
| `` `systemc_header `` `` `systemc_header_post `` `` `systemc_interface `` `` `systemc_imp_header `` `` `systemc_implementation `` `` `systemc_ctor `` `` `systemc_dtor `` | 19 / 1 / 4 / 3 / 3 / 3 / 3 | [VLT] Raw-text mode switches (§3.9) |
| `` `systemc_class_name `` | 2 | [VLT] Inside raw text only |
| `` `verilog `` | 29 | [VLT] Return from raw-text mode |
| `` `verilator_config `` | 59 | [VLT] Switch into control-file language (§13) |
| `` `coverage_block_off `` | 1 | [VLT] Predefined macro → metacomment |

### 3.3 Macro definitions and uses [LRM 22.5]

```
define_dir     ::= "`define" WS MACRO_NAME formals? ( WS macro_body )? EOL
formals        ::= "(" list( IDENT ( "=" default_text )? ) ")"    -- "(" must immediately follow MACRO_NAME, no WS
macro_body     ::= ( body_text | "\" NEWLINE )*                    -- backslash-newline continues the body
body_text      ::= any token, plus the specials:
                   "``"     token paste (joins adjacent tokens, e.g. a``b)
                   "`\""    begin/end stringification: `" ... `"  (formals substituted inside)
                   "`\`\""  inside a stringification: emits a literal \" character
macro_use      ::= "`" MACRO_NAME actuals?
actuals        ::= "(" list( actual_text? ) ")"     -- an empty actual takes the default
actual_text    ::= balanced text; "," splits only at depth 0 of (), [], {} and outside strings
```

Behaviour that golden files pin down (`t_preproc*`, 51 tests):

- "Illegal text before '(' that starts define arguments" means a space before `(` in a *use* of a function-like macro is an error.
- Errors exist for too many arguments, a missing `(` for a function-like macro, EOF inside an argument list, and
  "Unterminated ( in define formal arguments".
- Arguments are macro-expanded *after* substitution, so recursive definitions are legal: `` `define quux(x) `qux(`"x`") ``.
- Inside a stringification `` `" … `" ``, formal names are substituted **and macro uses are expanded**. An undefined macro stays as literal text (`t_preproc_strify_join`, `t_preproc`). Full rules are in §3.10.
- Redefining a macro with a different body is a warning (`REDEFMACRO`).
- `` `undefineall `` removes every user macro but keeps the predefines **(probe)**.

### 3.4 Conditional compilation, including 1800-2023 expressions [LRM 22.6]

```
if_dir         ::= ( "`ifdef" | "`ifndef" ) WS cond_expr
elsif_dir      ::= "`elsif" WS cond_expr
cond_expr      ::= MACRO_NAME
                 | "(" cond_or ")"                       -- 1800-2023 form
cond_or        ::= cond_and ( "||" cond_and )*
cond_and       ::= cond_imp ( "&&" cond_imp )*
cond_imp       ::= cond_unary ( ( "->" | "<->" ) cond_unary )?
cond_unary     ::= "!" cond_unary | MACRO_NAME | "(" cond_or ")"
```

- An unmatched `` `elsif ``, `` `else `` or `` `endif `` is an error (golden: ``"`elsif with no matching `if"``).
- Operands are define names; their values don't matter. A define whose value is `0` draws a `PREPROCZERO` warning (`t_preproc_preproczero_bad`).
- **Precedence as observed:** `&&`, `||`, `->` and `<->` have equal precedence and associate left to right. For example, `( ONE || ZERO && ZERO )` is false. This differs from expression precedence in the LRM, but Verilator's golden output requires it.
- **Known deviation:** Verilator's golden omits the branch for `` `elsif ( ONE && !( ZERO && ONE ) ) `` (with `ONE` defined), which is true under any reading. We keep the true result. The test `ifexpr_exact_except_known_deviation` records this.
- Skipped regions are still lexed for **comments and strings**, so `` `endif `` inside a skipped `/* */` is not seen.

### 3.5 Include

```
include_dir ::= "`include" WS ( STRING | "<" path ">" | macro_use )
```

A macro that expands to a string is legal. Recursive include must be detected.

### 3.6 `` `line `` and `-E` output

Our `-E` output must reproduce the observable `` `line `` markers that `t_preproc*.out` compares:

```
`line <lineno> "<filename>" <level>      level: 0 = continuing, 1 = entering include, 2 = returning from include
```

The output begins with `` `line 1 "<file>" 1 ``. The other `-E`-related flags in tests are `-P` (no `` `line `` markers),
`--pp-comments` and `--dump-defines`. Byte-identical output is required for `files_identical` tests; see `01`/`08`.

### 3.7 `` `timescale `` / `timeunit`

```
timescale_dir ::= "`timescale" WS TIME_LITERAL WS? "/" WS? TIME_LITERAL
```

The precision must be ≤ the unit. Interaction with `timeunit`/`timeprecision` declarations is per [LRM 3.14.2.3].
Verilator options `--timescale` and `--timescale-override` affect the result **(see `10`)**.

### 3.8 Predefined macros (observed with `--dump-defines`)

```
`define SYSTEMVERILOG 1          `define VERILATOR 1        `define verilator 1        `define verilator3 1
`define coverage_block_off /*verilator coverage_block_off*/
`define SV_COV_START 0  SV_COV_STOP 1  SV_COV_RESET 2  SV_COV_CHECK 3  SV_COV_MODULE 10  SV_COV_HIER 11
        SV_COV_ASSERTION 20  SV_COV_FSM_STATE 21  SV_COV_STATEMENT 22  SV_COV_TOGGLE 23
        SV_COV_OVERFLOW -2  SV_COV_ERROR -1  SV_COV_NOCOV 0  SV_COV_OK 1  SV_COV_PARTIAL 2
`define VERILATOR_TIMING 1       -- only with --timing (guide)
```

Plus every `-D<name>[=<val>]` and `+define+<name>[=<val>]` from the command line. `--dump-defines` prints them sorted
by name, with the `SV_COV_*` values as listed in [LRM 40.3.2.1].

### 3.8a Comments in preprocessor output

- An ordinary comment becomes a single space. `integer/*x*/foo` gives `integer foo`.
- Metacomments are kept and normalised to `/*verilator …*/`. This applies to both the `/* verilator … */` and `// verilator …` forms. Runs of spaces and tabs collapse to one space, backslash-newline becomes a newline, and the ends are trimmed. A `//` metacomment ends at the next `//`.
- `/*verilator_…*/` and `/*synopsys_…*/` (an underscore instead of a space) give `BADVLTPRAGMA` and are dropped.

### 3.9 Raw-text regions [VLT]

```
systemc_region  ::= SYSTEMC_DIR raw_text*  ( "`verilog" | SYSTEMC_DIR | "`verilator_config" | EOF )
SYSTEMC_DIR     ::= "`systemc_header" | "`systemc_header_post" | "`systemc_interface" | "`systemc_imp_header"
                  | "`systemc_implementation" | "`systemc_ctor" | "`systemc_dtor"
config_region   ::= "`verilator_config" control_command* ( "`verilog" | EOF )   -- §13
```

The raw text is captured verbatim, with `` `systemc_class_name `` substituted. It is only legal as a module or class item.
It is C++ text, so our Rust back end must decide what to do with it. Proposal: accept it, ignore it with a warning, and
waive tests whose behaviour depends on it.

### 3.10 Macro expansion rules (from Verilator's golden output)

These rules come from `t_preproc`, `t_preproc_def09`, `t_preproc_strify_join` and `t_preproc_noline`. The implementation is `src/pp/mod.rs`.

1. **The body** runs to the first newline not preceded by `\`. A block comment that spans lines does not end it. Ordinary line comments are dropped. Trailing spaces are then trimmed, and only after that do block comments become one space. So `` `define A x // c `` gives `x`, but `` `define B x /* c */ `` gives `x  `. Leading spaces are stripped, and a backslash-newline in the body becomes a newline in the expansion.
2. **Formals:** a `(` *immediately* after the name starts the formals. With a space before it, the macro is object-like and the parentheses are body text. The formal list may span lines and contain comments. A default value keeps its trailing whitespace.
3. **Actuals** are split at commas at bracket depth 0, counting `()`, `[]` and `{}`; strings are atomic. Splitting happens on the **raw** text, so with `` `define a x,y ``, `` `B(`a,`a) `` has two arguments. Each actual is trimmed. An empty actual takes the default. `` `F() `` is legal for a macro with zero formals.
4. **Substitution is textual.** Formals are replaced by the raw actual text, but not inside string literals (IEEE 1800-2023). The result is then **rescanned**. Rescanning continues into the text after the use, so `` `CAT(`R_, 2)(d) `` builds `` `R_2 `` and then takes `(d)` as its arguments.
5. **Paste ``` `` ```** joins its neighbours and keeps any whitespace beside it (`` f`` y `` gives `a y`). If an operand is a use of a **defined** macro, it is expanded *before* joining. If the macro is undefined, the joined text is rescanned, so `` `QA``_b `` becomes `` `QA_b `` when `QA` is undefined.
6. **Escaped identifiers** in a body are split, so formals inside them are substituted (`\name``_x`). In the rescanned text they are atomic again, so `` \`FOO `` is not expanded.
7. **Define names can be pasted:** `` `define X_```SOME `` defines `X_some`.
8. **Directives inside expansions work**, including `` `define ``, `` `undef `` and `` `ifdef ``. An inner define ends at a newline in the expansion, which is why `` `define DEFINEIT(d) d \ `` needs its trailing continuation.
9. **`` `__LINE__ ``** inside an expansion gives the line of the outermost use. **`` `__FILE__ ``** gives the current name, after any `` `line ``, with `\` and `"` re-escaped.
10. **`` `line `__LINE__ "name" 0 ``** renames the file without changing the line numbering (user guide). With a literal number N, the next line is N.
11. **Undefined macros pass through** literally. The "Define or directive not defined" error comes from the parser, not the preprocessor.
12. **`` `error ``** takes one string argument and nothing else on the line.
13. **Known deviation:** Verilator's golden for `bug202` in `t_preproc.v` comes from a `` `define `` whose name follows a multi-line block comment ending in `\`. Verilator emits a stray `\` and leaves the formal unsubstituted. We follow the LRM. The test `preproc_words_except_known_deviation` records this.
14. **`-E` layout:** we match Verilator byte for byte for `-E -P`. For plain `-E`, we match the words and the line numbering but not Verilator's exact placement of line breaks and `` `line `` markers. For example, Verilator starts a new line before a multi-line comment that comes from an expansion. That is a later polish task.

---

## 4. Source text [LRM A.1]

```
source_text        ::= timeunits_decl? description*
description        ::= module_decl | udp_decl | interface_decl | program_decl | package_decl | checker_decl
                     | attribute_instance* ( package_item | bind_directive )
                     | config_decl
                     | META_COMMENT                                     -- [VLT] file-level metacomments (lint_off ...)

module_decl        ::= attribute_instance* module_keyword lifetime? IDENT
                       package_import_decl* parameter_port_list? ports? ";" META_COMMENT*
                       timeunits_decl? module_item* "endmodule" ( ":" IDENT )?
                     | "extern" module_keyword lifetime? IDENT parameter_port_list? ports? ";"
module_keyword     ::= "module" | "macromodule"
lifetime           ::= "static" | "automatic"
ports              ::= "(" list(ansi_port_decl) ")"                      -- ANSI style
                     | "(" list(port_expr_item) ")"                      -- non-ANSI; directions follow as items
                     | "(" ".*" ")"                                       -- wildcard header [LRM 23.2.2.4]
ansi_port_decl     ::= attribute_instance* ( port_direction? net_or_var_type_opt port_ident_dims
                                            | interface_port_header IDENT unpacked_dim*
                                            | "." IDENT "(" expression? ")" )                 -- explicit port
                       ( "=" constant_expression )? META_COMMENT*
port_direction     ::= "input" | "output" | "inout" | "ref" | "const" "ref"
interface_port_header ::= ( IDENT | "interface" ) ( "." IDENT )?         -- iface or iface.modport
port_expr_item     ::= port_reference | "{" list(port_reference) "}"     -- complex ports [UNSUP] in some forms
                     | "." IDENT "(" port_reference? ")"

interface_decl     ::= like module_decl with "interface"/"endinterface", items = interface_item
program_decl       ::= like module_decl with "program"/"endprogram"
checker_decl       ::= "checker" IDENT ( "(" checker_ports? ")" )? ";" checker_item* "endchecker" ( ":" IDENT )?
                                                         -- treated like a module; few checker restrictions enforced (guide)
package_decl       ::= attribute_instance* "package" lifetime? IDENT ";" timeunits_decl? package_item*
                       "endpackage" ( ":" IDENT )?
timeunits_decl     ::= "timeunit" TIME_LITERAL ( "/" TIME_LITERAL )? ";" ( "timeprecision" TIME_LITERAL ";" )?
                     | "timeprecision" TIME_LITERAL ";" ( "timeunit" TIME_LITERAL ";" )?

bind_directive     ::= "bind" bind_target ( ":" list(bind_inst_path) )? instantiation ";"
                       -- Verilator: bind target must be a MODULE NAME, not an instance path (guide)

config_decl        ::= "config" IDENT ";" local_param_decl* design_stmt config_rule* "endconfig" ( ":" IDENT )?
design_stmt        ::= "design" ( ( IDENT "." )? IDENT )* ";"
config_rule        ::= "default" liblist_clause ";"
                     | ( "instance" hier_path | "cell" ( IDENT "." )? IDENT ) ( liblist_clause | use_clause ) ";"
                       -- Verilator: config localparam decl [UNSUP]; hierarchical config rule [UNSUP]
```

**Non-ANSI header items**: `input/output/inout` declarations may appear later among the module items and must bind to
names in the port list. The "C-style declarations inside port lists" of Verilog-2001 are ANSI ports.

**Recursive module instantiation**: a module that instantiates a path leading back to itself is `[UNSUP]` unless a
generate condition terminates the recursion **(probe)**.

---

## 5. Declarations [LRM A.2]

### 5.1 Data types [LRM A.2.2]

```
data_type          ::= integer_vector_type signing? packed_dim*
                     | integer_atom_type signing?
                     | non_integer_type
                     | ( "struct" | "union" ( "soft" | "tagged" )? ) ( "packed" signing? )? "{" struct_member+ "}" packed_dim*
                     | "enum" enum_base_type? "{" list(enum_name_decl) "}" packed_dim*
                     | "string" | "chandle" | "event"
                     | "virtual" "interface"? IDENT parameter_value_assignment? ( "." IDENT )?
                     | type_reference
                     | class_or_typedef_type packed_dim*
                     | "type" "(" ( expression | data_type ) ")"
integer_vector_type ::= "bit" | "logic" | "reg"
integer_atom_type  ::= "byte" | "shortint" | "int" | "longint" | "integer" | "time"
non_integer_type   ::= "shortreal" | "real" | "realtime"            -- shortreal is treated as real (guide); warning when promoted
signing            ::= "signed" | "unsigned"
struct_member      ::= attribute_instance* random_qualifier? data_type_or_void list(var_decl_assign) ";" META_COMMENT*
enum_base_type     ::= integer_atom_type signing? | integer_vector_type signing? packed_dim? | IDENT packed_dim?
enum_name_decl     ::= IDENT ( "[" INTEGRAL "(" ":" INTEGRAL ")"? "]" )? ( "=" constant_expression )?
class_or_typedef_type ::= ( package_scope | class_scope )? IDENT parameter_value_assignment? ( "::" IDENT parameter_value_assignment? )*
package_scope      ::= ( IDENT | "$unit" ) "::"
packed_dim         ::= "[" constant_expression ":" constant_expression "]" | "[" "]"
unpacked_dim       ::= "[" constant_expression ( ":" constant_expression )? "]"   -- [N] means [0:N-1]
                     | "[" "]"                                   -- dynamic array
                     | "[" "*" "]" | "[" data_type "]"           -- associative (wildcard [*] partially [UNSUP])
                     | "[" "$" ( ":" constant_expression )? "]"  -- queue, optionally bounded
```

Verilator caveats:

- `union tagged`, tagged patterns, `case … matches`, the `matches` operator and `void` members of tagged unions are all `[UNSUP]` (about 100 golden occurrences).
- Structs and unions are "scheduled together". This is a semantics note for `05`, not grammar.
- `chandle` is accepted and treated as a 64-bit value (guide).

### 5.2 Nets and variables [LRM A.2.1.3, A.2.2.1]

```
net_decl           ::= net_type ( drive_strength | charge_strength )? ( "vectored" | "scalared" )?
                       data_type_or_implicit delay3? list(net_decl_assign) ";"
                     | IDENT delay_control? list(net_decl_assign) ";"                 -- user nettype
                     | "interconnect" implicit_dt ( "#" delay_value )? list(IDENT unpacked_dim*) ";"   -- [UNSUP]
net_type           ::= "supply0" | "supply1" | "tri" | "triand" | "trior" | "trireg" | "tri0" | "tri1"
                     | "uwire" | "wire" | "wand" | "wor"
                       -- trireg [UNSUP]; uwire treated as wire (guide)
net_decl_assign    ::= IDENT unpacked_dim* META_COMMENT* ( "=" expression )?
var_decl           ::= "const"? "var"? lifetime? data_type_or_implicit list(var_decl_assign) ";"
var_decl_assign    ::= IDENT variable_dim* META_COMMENT* ( "=" ( expression | class_new | dynamic_array_new ) )?
                     | IDENT META_COMMENT* ...
nettype_decl       ::= "nettype" data_type IDENT ( "with" ( package_scope | class_scope )? IDENT )? ";"   -- "with" resolution fn [UNSUP]
typedef_decl       ::= "typedef" data_type IDENT variable_dim* META_COMMENT* ";"
                     | "typedef" ( "enum" | "struct" | "union" | "class" | "interface" "class" )? IDENT ";"   -- forward
                     | "typedef" IDENT constant_bit_select? "." IDENT IDENT ";"              -- interface-based typedef
drive_strength     ::= "(" strength0 "," strength1 ")" | "(" strength1 "," strength0 ")" | "(" strength0 "," "highz1" ")" ...
                       -- highz strengths [UNSUP]; pullup/pulldown strengths [UNSUP]
delay3             ::= "#" delay_value | "#" "(" mintypmax ( "," mintypmax ( "," mintypmax )? )? ")"
                       -- rise/fall/turn-off: only one delay used (RISEFALLDLY); min:typ:max → typ (MINTYPMAXDLY)
```

**Metacomment attachment.** `META_COMMENT` after the declared name and before `=` or `;` attaches to that one
variable. Examples: `reg x /*verilator public*/;` and `logic [7:0] x [0:1] /*verilator split_var*/;`.
The corpus also has the comment after the whole declaration but before `;`, as in `reg splitme /* verilator isolate_assignments*/;`.
Both positions are legal.

### 5.3 Parameters [LRM A.2.1.1]

```
parameter_port_list ::= "#" "(" ( list(param_port_decl) )? ")"
param_port_decl    ::= ( "parameter" | "localparam" )? ( data_type_or_implicit | "type" type_restriction? ) list(param_assign)
                     | data_type list(param_assign)
param_decl         ::= ( "parameter" | "localparam" ) ( data_type_or_implicit list(param_assign)
                                                      | "type" type_restriction? list(type_assign) ) ";"
param_assign       ::= IDENT unpacked_dim* META_COMMENT* ( "=" constant_param_expression )?
type_assign        ::= IDENT ( "=" data_type )?
type_restriction   ::= "enum" | "struct" | "union" | "class" | "interface" "class"     -- 1800-2023
specparam_decl     ::= "specparam" packed_dim? list(IDENT "=" constant_mintypmax) ";"    -- supported (guide)
defparam_stmt      ::= "defparam" list(hier_ident "=" constant_mintypmax) ";"          -- "defparam with no dot" [UNSUP]
```

The `-G<name>=<value>` option overrides top-level parameters. This is semantic, see `10`.

### 5.4 Functions and tasks [LRM A.2.6, A.2.7]

```
function_decl      ::= "function" dynamic_override? lifetime? function_data_type_or_implicit
                       ( interface_ident "." | class_scope )? IDENT
                       ( "(" tf_port_list? ")" )? ";" tf_item_decl* function_stmt* "endfunction" ( ":" IDENT )?
                     | "function" lifetime? "new" ( "(" tf_port_list? ")" )? ";" ... "endfunction" ( ":" "new" )?
task_decl          ::= "task" dynamic_override? lifetime? ( interface_ident "." | class_scope )? IDENT
                       ( "(" tf_port_list? ")" )? ";" tf_item_decl* statement_or_null* "endtask" ( ":" IDENT )?
dynamic_override   ::= ":" ( "initial" | "extends" | "final" )                   -- 1800-2023 qualifiers
tf_port_list       ::= list( attribute_instance* tf_port_direction? "var"? data_type_or_implicit
                             IDENT variable_dim* ( "=" expression )? META_COMMENT* )
tf_port_direction  ::= port_direction
tf_item_decl       ::= block_item_decl | tf_port_decl | META_COMMENT        -- [VLT] public, no_inline_task, sformat…

dpi_import_export  ::= "import" DPI_SPEC dpi_import_property? ( IDENT "=" )? dpi_proto ";" META_COMMENT*
                     | "export" DPI_SPEC ( IDENT "=" )? ( "function" | "task" ) IDENT ";"
DPI_SPEC           ::= '"DPI-C"' | '"DPI"'
dpi_import_property ::= "context" | "pure"
dpi_proto          ::= "function" data_type_or_void IDENT ( "(" tf_port_list? ")" )?
                     | "task" IDENT ( "(" tf_port_list? ")" )?
```

`[VLT]` `/*verilator dpi_c_decl "<C prototype>"*/` may follow an import prototype and replaces the emitted C
declaration (guide). For us the string is a C ABI hint. Timing controls inside DPI-exported tasks are `[UNSUP]`.

### 5.5 `let`, sequences and properties [LRM A.2.10, A.2.12]

```
let_decl           ::= "let" IDENT ( "(" list(let_port)? ")" )? "=" expression ";"   -- typed let ports [UNSUP]
sequence_decl      ::= "sequence" IDENT ( "(" seq_port_list? ")" )? ";" assertion_var_decl* sequence_expr ";"?
                       "endsequence" ( ":" IDENT )?
property_decl      ::= "property" IDENT ( "(" prop_port_list? ")" )? ";" assertion_var_decl* property_spec ";"?
                       "endproperty" ( ":" IDENT )?
```

Assertion detail and the `[UNSUP]` subset are in §11.

### 5.6 Imports and exports

```
package_import_decl ::= "import" list( package_import_item ) ";"
package_import_item ::= IDENT "::" ( IDENT | "*" )
package_export_decl ::= "export" "*" "::" "*" ";" | "export" list(package_import_item) ";"
```

---

## 6. Module items, instantiation and generate [LRM A.1.4, A.4]

```
module_item        ::= port_decl ";"                                     -- non-ANSI
                     | attribute_instance* module_or_generate_item
                     | generate_region | specify_block | specparam_decl
                     | program_decl | module_decl | interface_decl        -- nested; "program decls within module" [UNSUP]
                     | timeunits_decl
                     | META_COMMENT                                       -- [VLT]
                     | systemc_region                                     -- [VLT]
                     | config_region                                      -- [VLT]
module_or_generate_item ::= parameter_override | gate_instantiation | udp_instantiation | module_instantiation
                     | net_decl | var_decl | typedef_decl | nettype_decl | param_decl | package_import_decl
                     | function_decl | task_decl | dpi_import_export | checker_decl
                     | class_decl | interface_class_decl | covergroup_decl | let_decl
                     | sequence_decl | property_decl | clocking_decl | default_clocking | default_disable
                     | genvar_decl | continuous_assign | net_alias | initial_construct | final_construct
                     | always_construct | assertion_item | bind_directive
                     | loop_generate | conditional_generate | elaboration_task
                     | ";"
elaboration_task   ::= ( "$fatal" | "$error" | "$warning" | "$info" ) ( "(" list(expression)? ")" )? ";"
net_alias          ::= "alias" net_lvalue ( "=" net_lvalue )+ ";"        -- operands must be plain variable refs

module_instantiation ::= IDENT parameter_value_assignment? list(hier_instance) ";"
parameter_value_assignment ::= "#" "(" list(ordered_param | named_param)? ")" | "#" delay_value   -- legacy #N
named_param        ::= "." IDENT "(" ( expression | data_type )? ")"
hier_instance      ::= IDENT unpacked_dim* "(" port_connections? ")"
port_connections   ::= list( attribute_instance* expression? )                -- ordered; empty = unconnected
                     | list( attribute_instance* ( "." IDENT ( "(" expression? ")" )? | ".*" ) )   -- named, .name, .*
gate_instantiation ::= gate_type drive_strength? delay3? list( IDENT? unpacked_dim* "(" list(expression) ")" ) ";"
gate_type          ::= "and" | "nand" | "or" | "nor" | "xor" | "xnor" | "buf" | "not"
                     | "bufif0" | "bufif1" | "notif0" | "notif1" | "pullup" | "pulldown"
                     | "nmos" | "pmos"                                            -- supported (guide)
                     | "cmos" | "rcmos" | "rnmos" | "rpmos" | "tran" | "tranif0" | "tranif1"
                     | "rtran" | "rtranif0" | "rtranif1"                          -- [UNSUP] (guide: other MOS/switch)
udp_instantiation  ::= IDENT drive_strength? delay2? list( IDENT? unpacked_dim* "(" list(expression) ")" ) ";"

genvar_decl        ::= "genvar" list(IDENT) ";"
generate_region    ::= "generate" module_item* "endgenerate"
loop_generate      ::= "for" "(" "genvar"? IDENT "=" constant_expression ";" constant_expression ";" genvar_iteration ")"
                       generate_block
genvar_iteration   ::= IDENT assignment_operator expression | inc_or_dec_operator IDENT | IDENT inc_or_dec_operator
conditional_generate ::= "if" "(" constant_expression ")" generate_block ( "else" generate_block )?
                     | "case" "(" constant_expression ")" case_generate_item+ "endcase"
generate_block     ::= module_or_generate_item
                     | ( IDENT ":" )? "begin" ( ":" IDENT )? module_item* "end" ( ":" IDENT )?
```

Verilator naming: unnamed generate blocks get the LRM names `genblk<N>` [LRM 27.6], and hierarchical paths use them.
Arrayed instances are referenced as `inst[3]`.

**Interface and modport** [LRM A.2.9]:

```
modport_decl       ::= "modport" list( IDENT "(" list(modport_ports) ")" ) ";"
modport_ports      ::= port_direction list( IDENT | "." IDENT "(" expression? ")" )    -- expr with part-select [UNSUP]
                     | "clocking" IDENT
                     | ( "import" | "export" ) list( IDENT | method_prototype )
```

The guide says generate blocks around modports are `[UNSUP]`, as are virtual interfaces and unnamed interfaces. But the
corpus has 456 interface tests, and there are "virtual interface trigger" `Unsupported:` goldens. That suggests virtual
interfaces are **largely supported now** and the guide is out of date **(probe)**. Treat the test corpus as authoritative.

---

## 7. User-defined primitives [LRM A.5]

```
udp_decl           ::= "primitive" IDENT "(" udp_ports ")" ";" udp_port_decl+ udp_body "endprimitive" ( ":" IDENT )?
                     | "primitive" IDENT "(" udp_ansi_ports ")" ";" udp_body "endprimitive" ( ":" IDENT )?
udp_body           ::= ( "initial" IDENT "=" init_val ";" )? "table" udp_entry+ "endtable"
udp_entry          ::= level_symbol+ ":" ( level_symbol ":" )? output_symbol ";"     -- comb if no current-state field
                     | ( level_symbol | edge_indicator )+ ":" level_symbol ":" next_state ";"
level_symbol       ::= "0" | "1" | "x" | "X" | "?" | "b" | "B"
edge_indicator     ::= "(" level_symbol level_symbol ")" | "r" | "R" | "f" | "F" | "p" | "P" | "n" | "N" | "*"
output_symbol      ::= "0" | "1" | "x" | "X"
next_state         ::= output_symbol | "-"
```

UDP tables are supported (guide; 51 `t_udp*` tests). Inside `table … endtable` the **lexer must switch modes**: `0`, `1`,
`x`, `?`, `(01)`, `r`, `*` and `-` are table symbols, not numbers or identifiers. Golden files include
`syntax error, unexpected UDP table field`.

---

## 8. Behavioural statements [LRM A.6]

### 8.1 Processes and continuous assignment

```
continuous_assign  ::= "assign" drive_strength? delay3? list(net_lvalue "=" expression) ";"
                     | "assign" delay_control? list(variable_lvalue "=" expression) ";"
initial_construct  ::= "initial" statement_or_null
final_construct    ::= "final" function_statement
always_construct   ::= ( "always" | "always_comb" | "always_latch" | "always_ff" ) statement
```

### 8.2 Statements

```
statement_or_null  ::= statement | attribute_instance* ";"
statement          ::= ( IDENT ":" )? attribute_instance* META_COMMENT* statement_item
statement_item     ::= blocking_assignment ";" | nonblocking_assignment ";"
                     | procedural_continuous ";" | case_stmt | conditional_stmt
                     | inc_or_dec_expr ";" | subroutine_call_stmt | disable_stmt | event_trigger
                     | loop_stmt | jump_stmt | par_block | procedural_timing_control_stmt
                     | seq_block | wait_stmt | procedural_assertion | clocking_drive ";"
                     | randsequence_stmt | randcase_stmt | expect_property_stmt
blocking_assignment ::= variable_lvalue "=" delay_or_event_control expression     -- intra-assignment delay
                     | nonrange_variable_lvalue "=" dynamic_array_new
                     | ( implicit_class_handle "." | class_scope | package_scope )? hier_ident select "=" class_new
                     | operator_assignment
operator_assignment ::= variable_lvalue assignment_operator expression
assignment_operator ::= "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "&=" | "|=" | "^=" | "<<=" | ">>=" | "<<<=" | ">>>="
nonblocking_assignment ::= variable_lvalue "<=" delay_or_event_control? expression
procedural_continuous ::= "assign" variable_lvalue "=" expression | "deassign" variable_lvalue
                     | "force" ( variable_lvalue | net_lvalue ) "=" expression | "release" ( variable_lvalue | net_lvalue )
                       -- force/release supported with documented deviations (guide); procedural assign/deassign
                       -- treated as procedural; complex-select force [UNSUP]
seq_block          ::= "begin" ( ":" IDENT )? block_item_decl* statement_or_null* "end" ( ":" IDENT )?
par_block          ::= "fork" ( ":" IDENT )? block_item_decl* statement_or_null* join_keyword ( ":" IDENT )?
join_keyword       ::= "join" | "join_any" | "join_none"
conditional_stmt   ::= unique_priority? "if" "(" cond_predicate ")" statement_or_null
                       ( "else" "if" "(" cond_predicate ")" statement_or_null )* ( "else" statement_or_null )?
unique_priority    ::= "unique" | "unique0" | "priority"              -- "priority if" ignored (guide)
cond_predicate     ::= expression_or_cond_pattern ( "&&&" expression_or_cond_pattern )*   -- "&&&" [UNSUP]
case_stmt          ::= unique_priority? case_keyword "(" expression ")" case_item+ "endcase"
                     | unique_priority? case_keyword "(" expression ")" "matches" case_pattern_item+ "endcase"   -- [UNSUP]
                     | unique_priority? "case" "(" expression ")" "inside" case_inside_item+ "endcase"
case_keyword       ::= "case" | "casez" | "casex"
case_item          ::= list(expression) ":" statement_or_null | "default" ":"? statement_or_null
case_inside_item   ::= list(open_range) ":" statement_or_null | "default" ":"? statement_or_null
randcase_stmt      ::= "randcase" ( expression ":" statement_or_null )+ "endcase"
loop_stmt          ::= "forever" statement_or_null
                     | "repeat" "(" expression ")" statement_or_null
                     | "while" "(" expression ")" statement_or_null
                     | "for" "(" for_init? ";" expression? ";" list(for_step)? ")" statement_or_null
                     | "do" statement_or_null "while" "(" expression ")" ";"
                     | "foreach" "(" ps_or_hier_ident "[" loop_variables "]" ")" statement
for_init           ::= list( data_type? IDENT "=" expression | variable_assignment )   -- 2017: mixed decl/assign
for_step           ::= operator_assignment | inc_or_dec_expr | function_subroutine_call
jump_stmt          ::= "return" expression? ";" | "break" ";" | "continue" ";"
disable_stmt       ::= "disable" hier_ident ";" | "disable" "fork" ";"
event_trigger      ::= "->" hier_ident ";" | "->>" delay_or_event_control? hier_ident ";"
wait_stmt          ::= "wait" "(" expression ")" statement_or_null | "wait" "fork" ";"
                     | "wait_order" "(" list(hier_ident) ")" action_block           -- [UNSUP]
subroutine_call_stmt ::= subroutine_call ";" | "void" "'" "(" function_subroutine_call ")" ";"
```

Note on the `case … inside` item: the guide says `case inside` is `[UNSUP]`, but the golden files only mention
`case matches`. Treat `case inside` as **supported** **(probe)**.

### 8.3 Timing controls [LRM A.6.5]

```
procedural_timing_control_stmt ::= ( delay_control | event_control | cycle_delay ) statement_or_null
delay_control      ::= "#" delay_value | "#" "(" mintypmax_expression ")"
delay_value        ::= UNSIGNED | REAL_NUMBER | TIME_LITERAL | "1step" | ps_ident
event_control      ::= "@" hier_ident | "@" "(" event_expression ")" | "@*" | "@" "(*)" | "@" ps_or_hier_sequence_ident
event_expression   ::= edge_identifier? expression ( "iff" expression )?
                     | sequence_instance ( "iff" expression )?
                     | event_expression ( "or" | "," ) event_expression
                     | "(" event_expression ")"
edge_identifier    ::= "posedge" | "negedge" | "edge"
cycle_delay        ::= "##" ( INTEGRAL | IDENT | "(" expression ")" )
delay_or_event_control ::= delay_control | event_control | "repeat" "(" expression ")" event_control
```

**Semantic gate (not grammar), recorded here because it decides whether a parse is rejected:**

| Mode | `#delay` statement | Intra-assignment delay | `@` not at top of process | `wait` | `fork` (≥2 stmts) |
|---|---|---|---|---|---|
| `--timing` | supported | supported | supported | supported | supported |
| `--no-timing` | ignored + `STMTDLY` | ignored + `ASSIGNDLY` | `NOTIMING` error | `NOTIMING` | `NOTIMING` |
| neither flag | `NEEDTIMINGOPT` | `NEEDTIMINGOPT` | `NEEDTIMINGOPT` | `NEEDTIMINGOPT` | `NEEDTIMINGOPT` |

Exceptions: an empty fork, `fork…join`/`join_any` with one statement, and `--bbox-unsup`. The `timing_off` metacomment or
control-file command forces `--no-timing` behaviour locally, and turns `fork` into `begin`.

### 8.4 Clocking blocks [LRM A.6.11]

```
clocking_decl      ::= "default"? "clocking" IDENT? clocking_event ";" clocking_item* "endclocking" ( ":" IDENT )?
                     | "global" "clocking" IDENT? clocking_event ";" "endclocking" ( ":" IDENT )?
clocking_item      ::= "default" default_skew ";" | clocking_direction list(clocking_decl_assign) ";"
                     | attribute_instance* assertion_item_decl
clocking_direction ::= ( "input" skew? ) ( "output" skew? )? | "output" skew? | "inout"
skew               ::= edge_identifier delay_control? | delay_control
clocking_drive     ::= clockvar_expression "<=" cycle_delay? expression
```

There are 70 `t_clocking*` tests. A "clocking event edge override" is `[UNSUP]`.

### 8.5 Randsequence [LRM A.6.12]

```
randsequence_stmt  ::= "randsequence" "(" IDENT? ")" rs_production+ "endsequence"
rs_production      ::= data_type_or_void? IDENT ( "(" tf_port_list ")" )? ":" list_sep("|", rs_rule) ";"
rs_rule            ::= rs_prod+ ( ":=" weight ( rs_code_block )? )?
rs_prod            ::= IDENT ( "(" list(expression) ")" )? | rs_code_block | rs_if_else | rs_repeat | rs_case
```

`randsequence` nested under `randsequence` is `[UNSUP]`.

### 8.6 Statement-position metacomments [VLT]

`/*verilator unroll_full*/` and `/*verilator unroll_disable*/` apply to the **next loop at the same level**.
`/*verilator coverage_block_off*/` applies to the enclosing `begin…end`.

---

## 9. Specify blocks [LRM A.7] `[IGN]`

```
specify_block      ::= "specify" specify_item* "endspecify"
specify_item       ::= specparam_decl | pulsestyle_decl | showcancelled_decl | path_decl | system_timing_check
system_timing_check ::= ( "$setup" | "$hold" | "$setuphold" | "$recovery" | "$removal" | "$recrem" | "$skew"
                       | "$timeskew" | "$fullskew" | "$period" | "$width" | "$nochange" ) "(" timing_check_args ")" ";"
path_decl          ::= ( simple_path | edge_sensitive_path | state_dependent_path ) "=" path_delay_value ";"
```

The whole block must be **parsed** (malformed specify text is still a syntax error) and is then **discarded**.
Only `specparam` survives. `$sdf_annotate` appears in 4 tests and is accepted and ignored **(probe)**.

---

## 10. Expressions [LRM A.8, Clause 11]

### 10.1 Precedence (highest first) [LRM Table 11-2]

| Level | Operators | Assoc |
|---|---|---|
| 1 | `()` `[]` `::` `.` | left |
| 2 | unary `+ - ! ~ & ~& \| ~\| ^ ~^ ^~`, `++ --` | — |
| 3 | `**` | left |
| 4 | `* / %` | left |
| 5 | binary `+ -` | left |
| 6 | `<< >> <<< >>>` | left |
| 7 | `< <= > >=` `inside` `dist` | left |
| 8 | `== != === !== ==? !=?` | left |
| 9 | `&` (binary) | left |
| 10 | `^ ~^ ^~` (binary) | left |
| 11 | `\|` (binary) | left |
| 12 | `&&` | left |
| 13 | `\|\|` | left |
| 14 | `?:` | right |
| 15 | `-> <->` | right |
| 16 | `= += -= …` (in expression context, parenthesised) | none |
| 17 | `{} {{}}` concatenation | — |

### 10.2 Expression grammar

```
expression         ::= primary
                     | unary_operator attribute_instance* primary
                     | inc_or_dec_expr
                     | "(" operator_assignment ")"
                     | expression binary_operator attribute_instance* expression
                     | expression "?" attribute_instance* expression ":" expression
                     | expression "inside" "{" list(open_range) "}"
                     | expression "matches" pattern                              -- [UNSUP]
                     | tagged_union_expr                                         -- [UNSUP]
inc_or_dec_expr    ::= ( "++" | "--" ) variable_lvalue | variable_lvalue ( "++" | "--" )
                       -- Verilator: inside &&, ||, ?: [UNSUP]; general expression use is limited (guide)
open_range         ::= expression | "[" expression ":" expression "]"
                     | "[" expression ( "+/-" | "+%-" ) expression "]"           -- 1800-2023; [UNSUP]
primary            ::= primary_literal
                     | ( class_qualifier | package_scope )? hier_ident select
                     | empty_queue                                               -- "{}"
                     | concatenation ( "[" range_expression "]" )?
                     | multiple_concatenation ( "[" range_expression "]" )?
                     | streaming_concatenation
                     | function_subroutine_call
                     | let_expression
                     | "(" mintypmax_expression ")"
                     | cast
                     | assignment_pattern_expression
                     | sequence_method_call
                     | "this" | "super" | "$" | "null"
                     | "$root" "." hier_ident | "$unit" "::" IDENT
primary_literal    ::= NUMBER | TIME_LITERAL | UNBASED_UNSIZED | STRING | TRIPLE_STRING
select             ::= ( "." IDENT bit_select )* bit_select ( "[" part_select_range "]" )?
bit_select         ::= ( "[" expression "]" )*
part_select_range  ::= constant_expression ":" constant_expression
                     | expression "+:" constant_expression | expression "-:" constant_expression
concatenation      ::= "{" list(expression) "}"
multiple_concatenation ::= "{" expression concatenation "}"            -- replication; non-constant count [UNSUP]
streaming_concatenation ::= "{" ( ">>" | "<<" ) slice_size? "{" list(stream_expr) "}" "}"
stream_expr        ::= expression ( "with" "[" array_range_expr "]" )?
cast               ::= casting_type "'" "(" expression ")"
casting_type       ::= simple_type | constant_primary | signing | "string" | "const"
                       -- guide: casts limited to simple scalar types (likely out of date; 49 $cast/cast uses in corpus)
assignment_pattern_expression ::= ( assignment_pattern_type )? assignment_pattern
assignment_pattern ::= "'{" list(expression) "}"
                     | "'{" list( pattern_key ":" expression ) "}"
                     | "'{" constant_expression "{" list(expression) "}" "}"
pattern_key        ::= constant_expression | IDENT | data_type | "default"
                       -- guide: data-type keys and computed constant keys [UNSUP]
function_subroutine_call ::= tf_call | system_tf_call | method_call | randomize_call
tf_call            ::= ps_or_hier_ident attribute_instance* ( "(" list_of_arguments ")" )?
system_tf_call     ::= SYSTEM_IDENT ( "(" list_of_arguments ")" )?
                     | SYSTEM_IDENT "(" data_type ( "," expression )? ")"         -- $bits(type), $typename(t)...
list_of_arguments  ::= list( expression? ) ( "," "." IDENT "(" expression? ")" )*   -- empty positional args legal
                     | list( "." IDENT "(" expression? ")" )
method_call        ::= primary "." IDENT attribute_instance* ( "(" list_of_arguments ")" )?
                       ( "with" "(" expression ")" )?                               -- array methods
                     | primary "." ( "and" | "or" | "xor" | "unique" ) ...           -- array reduction
randomize_call     ::= ( primary "." )? "randomize" attribute_instance* ( "(" ( list(IDENT) | "null" )? ")" )?
                       ( "with" ( "(" list(IDENT)? ")" )? constraint_block )?
                     | "std" "::" "randomize" "(" list(variable)? ")" ( "with" constraint_block )?
```

### 10.3 Inline C [VLT]

```
c_call             ::= ( "$c" | "$cpure" | "$c" WIDTH ) "(" list( STRING | expression ) ")"
WIDTH              ::= [1-9][0-9]*          -- $c8, $c32, $c64 seen; $c1, $c9, $c48, $c100 also in the corpus
```

Strings are pasted into C++ verbatim, and expressions are evaluated and inlined. It can be a statement or an expression
(≤ 32 bits unless a width is given). It is used 242 + 77 + … times in the corpus, mostly to call harness helpers or to
check optimisation. **For our Rust back end we propose:**

1. Recognise common idioms such as `$c("1")` and literal constants.
2. Diagnose anything else as `Unsupported: $c in Rust backend`.
3. Waive the affected tests, and record each one in the manifest as `inline-c`.

---

## 11. Assertions [LRM A.2.10, Clause 16]

```
procedural_assertion ::= immediate_assertion | concurrent_assertion
immediate_assertion ::= ( "assert" | "assume" | "cover" ) ( "#0" | "final" )? "(" expression ")" action_block
concurrent_assertion ::= ( IDENT ":" )? ( "assert" | "assume" ) "property" "(" property_spec ")" action_block
                     | ( IDENT ":" )? "cover" "property" "(" property_spec ")" statement_or_null
                     | ( IDENT ":" )? "cover" "sequence" "(" clocking_event? ( "disable" "iff" "(" expression ")" )? sequence_expr ")" statement_or_null
                     | ( IDENT ":" )? "restrict" "property" "(" property_spec ")" ";"
expect_property_stmt ::= "expect" "(" property_spec ")" action_block            -- [UNSUP]
action_block       ::= statement_or_null | statement? "else" statement_or_null
property_spec      ::= clocking_event? ( "disable" "iff" "(" expression_or_dist ")" )? property_expr
property_expr      ::= sequence_expr | "strong" "(" sequence_expr ")" | "weak" "(" sequence_expr ")"
                     | "(" property_expr ")" | "not" property_expr
                     | property_expr ( "or" | "and" | "until" | "s_until" | "until_with" | "s_until_with"
                                     | "implies" | "iff" ) property_expr
                     | sequence_expr ( "|->" | "|=>" | "#-#" | "#=#" ) property_expr
                     | "if" "(" expression_or_dist ")" property_expr ( "else" property_expr )?
                     | "case" "(" expression_or_dist ")" property_case_item+ "endcase"
                     | ( "nexttime" | "s_nexttime" ) ( "[" constant_expression "]" )? property_expr
                     | ( "always" | "s_always" ) ( "[" cycle_delay_const_range "]" )? property_expr
                     | ( "eventually" | "s_eventually" ) ( "[" cycle_delay_const_range "]" )? property_expr
                     | ( "accept_on" | "reject_on" | "sync_accept_on" | "sync_reject_on" ) "(" expression_or_dist ")" property_expr
                     | property_instance | clocking_event property_expr
sequence_expr      ::= cycle_delay_range sequence_expr ( cycle_delay_range sequence_expr )*
                     | sequence_expr cycle_delay_range sequence_expr ( cycle_delay_range sequence_expr )*
                     | expression_or_dist boolean_abbrev?
                     | sequence_instance sequence_abbrev?
                     | "(" sequence_expr ( "," sequence_match_item )* ")" sequence_abbrev?
                     | sequence_expr ( "and" | "intersect" | "or" | "within" ) sequence_expr
                     | "first_match" "(" sequence_expr ( "," sequence_match_item )* ")"
                     | expression_or_dist "throughout" sequence_expr
                     | clocking_event sequence_expr
cycle_delay_range  ::= "##" ( constant_primary | "[" cycle_delay_const_range "]" | "[*]" | "[+]" )
boolean_abbrev     ::= "[*" const_or_range "]" | "[*]" | "[+]" | "[=" const_or_range "]" | "[->" const_or_range "]"
```

**Assertion `[UNSUP]` subset** (each item has at least one golden):

- `nexttime`, `s_nexttime`, `eventually[]`, `s_eventually[]`, cycle delays in `s_eventually`
- `expect`; strong properties in `assert` and `assume`
- `[=M:N]` non-consecutive range; zero minimum count in goto and non-consecutive ranges; zero repetition count
- multi-cycle sequences inside consecutive repetition
- `first_match` with match items
- `intersect` of two ranged-length structured sequences
- `within` with a ranged-delay operand; bounded ranged cycle delay in a sequence prefix
- a sequence expression as the antecedent of `#-#`/`#=#`
- `if`/`case` in a temporal property with a pass action, negation, or `cover`
- recursive properties; property/sequence argument data types; property-variable default values
- property local variables used across non-constant delays or composite operators
- a clocking event both before a property call and inside its body; a non-edge clocking event on a sequence
- a procedural concurrent assertion with a clocking event inside `always`
- sequences referenced outside an assertion; a sequence as an event control with certain operands
- implementation limits: SVA cycle delay, repetition count, `$past` tick, `always` range bound

The syntax-error goldens show that, in **some language modes**, `accept_on`, `reject_on`, `s_always`, `s_eventually`,
`s_nexttime`, `nexttime`, `eventually`, `implies`, `iff`, `|->`, `|=>`, `strong` and `weak` are parse errors.
Those words are not keywords in older modes, which fits §1.

Assertion-control system tasks: `$assertcontrol` (115 uses; several `control_type` values `[UNSUP]`), `$asserton`,
`$assertoff`, `$assertkill`, `$assertpasson`, `$assertpassoff`, `$assertfailon`, `$assertfailoff`, `$assertvacuousoff`,
`$assertnonvacuouson`. Sampled-value functions: `$past` (expr2 and clock arguments `[UNSUP]`), `$sampled`, `$rose`,
`$fell`, `$stable`, `$changed`, and the `_gclk` / `$future_gclk` family with `$global_clock`.

---

## 12. Classes, constraints and covergroups [LRM A.1.9, A.1.10, A.2.11]

### 12.1 Classes

```
class_decl         ::= "virtual"? "class" ( ":" "final" )? lifetime? IDENT parameter_port_list?
                       ( "extends" class_type ( "(" ( list_of_arguments | "default" )? ")" )? )?
                       ( "implements" list(interface_class_type) )? ";"
                       class_item* "endclass" ( ":" IDENT )?
interface_class_decl ::= "interface" "class" IDENT parameter_port_list? ( "extends" list(interface_class_type) )? ";"
                       ( type_decl | method_prototype ";" | param_decl )* "endclass" ( ":" IDENT )?
class_item         ::= attribute_instance* ( class_property | class_method | class_constraint
                     | class_decl | interface_class_decl | covergroup_decl | local_param_decl ";" | param_decl ";" | ";" )
                     | systemc_region                                         -- [VLT] (class-level `systemc_*)
class_property     ::= property_qualifier* var_decl
                     | "const" class_item_qualifier* data_type IDENT ( "=" constant_expression )? ";"
property_qualifier ::= "rand" | "randc" | "static" | "protected" | "local"
class_method       ::= method_qualifier* ( task_decl | function_decl )
                     | "pure" "virtual" class_item_qualifier* method_prototype ";"
                     | "extern" method_qualifier* method_prototype ";"
                     | method_qualifier* class_constructor_decl
method_qualifier   ::= "virtual" | "static" | "protected" | "local"
class_new          ::= class_scope? "new" ( "(" list_of_arguments ")" )? | "new" expression    -- shallow copy
```

There are 528 class tests, making this the largest feature group. The guide's "limited" wording is out of date.
The goldens still show `[UNSUP]` for some `new` constructor forms, "member call on object" cases, and random members
whose type is the containing class.

### 12.2 Constraints

```
class_constraint   ::= constraint_prototype | constraint_decl
constraint_decl    ::= "static"? "constraint" dynamic_override? IDENT constraint_block
constraint_block   ::= "{" constraint_block_item* "}"
constraint_block_item ::= "solve" list(IDENT select) "before" list(IDENT select) ";" | constraint_expression
constraint_expression ::= "soft"? expression_or_dist ";"
                     | uniqueness_constraint ";"
                     | expression "->" constraint_set
                     | "if" "(" expression ")" constraint_set ( "else" constraint_set )?
                     | "foreach" "(" ps_or_hier_ident "[" loop_variables "]" ")" constraint_set
                     | "disable" "soft" hier_ident ";"
uniqueness_constraint ::= "unique" "{" list(range_list_item) "}"
constraint_set     ::= constraint_expression | "{" constraint_expression* "}"
expression_or_dist ::= expression ( "dist" "{" list(dist_item) "}" )?
dist_item          ::= value_range ( ( ":=" | ":/" ) expression )? | "default" ":/" expression
```

`[UNSUP]` (from goldens): `unique` on multidimensional, wildcard or large static arrays; `unique` inside inline
`randomize() with`; `real` values in constraints; `**` with a non-constant exponent or non-2 base; array-reduction
constraints on nested dynamic arrays; nested array access in global constraints; nested `randomize()` inside
inline constraints.

### 12.3 Covergroups

```
covergroup_decl    ::= "covergroup" IDENT ( "(" tf_port_list? ")" )? coverage_event? ";"
                       coverage_spec_or_option* "endgroup" ( ":" IDENT )?
                     | "covergroup" "extends" IDENT ";" coverage_spec_or_option* "endgroup" ( ":" IDENT )?
coverage_event     ::= clocking_event | "with" "function" "sample" "(" tf_port_list? ")" | "@@" "(" block_event_expr ")"
coverage_spec_or_option ::= coverage_option ";" | cover_point | cover_cross
cover_point        ::= ( data_type_or_implicit IDENT ":" )? "coverpoint" expression ( "iff" "(" expression ")" )?
                       bins_or_empty
bins_or_empty      ::= "{" attribute_instance* ( bins_or_options ";" )* "}" | ";"
bins_or_options    ::= coverage_option
                     | "wildcard"? bins_keyword IDENT ( "[" covergroup_expression? "]" )? "=" "{" list(covergroup_range) "}"
                       ( "with" "(" covergroup_expression ")" )? ( "iff" "(" expression ")" )?
                     | "wildcard"? bins_keyword IDENT ( "[" "]" )? "=" trans_list ( "iff" "(" expression ")" )?
                     | bins_keyword IDENT ( "[" covergroup_expression? "]" )? "=" "default" ( "sequence" )? ( "iff" "(" expression ")" )?
bins_keyword       ::= "bins" | "illegal_bins" | "ignore_bins"
cover_cross        ::= ( IDENT ":" )? "cross" list(IDENT) ( "iff" "(" expression ")" )? cross_body
```

There are 307 covergroup tests. `[UNSUP]` (from goldens): explicit cross bins; crosses with more than N tuples;
non-integral bin values; function calls in select expressions; covergroup value ranges in some forms; default values on
`ref` formals. The `$coverage_*` / `$get_coverage` / `$load_coverage_db` / `$set_coverage_db_name` system functions are
reserved but `[UNSUP]` (golden: *reserved word not implemented*).

---

## 13. Verilator control-file language [VLT]

This is reached through a `.vlt` file (read **before** the Verilog sources) or a `` `verilator_config `` region. Control
files are **preprocessed first**, so `` `ifdef ``, `` `define `` and comments all work. Grammar (from `control.rst`, plus
corpus usage):

```
control_file       ::= control_command*
control_command    ::= KEYWORD option*
option             ::= OPTNAME ( STRING | INTEGER | IDENT )?       -- see below
OPTNAME            ::= "-" NAME | "--" NAME                         -- both forms accepted (corpus: --file, --rule, --var)
line_range         ::= INTEGER ( "-" INTEGER )?

-- Lint, coverage, timing and tracing scoping
"lint_off"  ( "-rule" CODE )? ( "-file" STRING ( "-lines" line_range )? )? ( "-contents" STRING )? ( "-match" STRING )?
"lint_on"   ( "-rule" CODE )? ( "-file" STRING ( "-lines" line_range )? )?
"coverage_off" | "coverage_on"    ( "-file" STRING ( "-lines" line_range )? )?
"timing_off"   | "timing_on"      ( "-file" STRING ( "-lines" line_range )? )?
"tracing_off"  | "tracing_on"     ( "-file" STRING ( "-lines" line_range )? )?
"tracing_off"  | "tracing_on"     "-scope" STRING ( "-levels" INTEGER )?
"coverage_block_off" "-file" STRING "-line" INTEGER | "coverage_block_off" "-module" STRING "-block" STRING
"full_case" | "parallel_case"     "-file" STRING "-lines" INTEGER

-- Per-module / per-variable attributes
"public" | "public_flat" | "public_flat_rd" | "public_flat_rw"
            ( "-module" STRING )? ( ( "-task" | "-function" ) STRING )? ( ( "-var" | "-param" | "-port" ) STRING )?
            ( STRING )?                                   -- public_flat_rw only: legacy "@(edge)", ignored
"forceable"          "-module" STRING "-var" STRING
"split_var"          ( "-module" STRING )? ( ( "-function" | "-task" ) STRING )? "-var" STRING
"sformat"            ( "-module" STRING )? ( ( "-function" | "-task" ) STRING )? "-var" STRING
"sc_bv" | "sc_biguint" "-module" STRING "-var" STRING
"inline"             "-module" STRING
"no_inline"          ( "-module" STRING )? ( ( "-function" | "-task" ) STRING )?
"hier_block"         "-module" STRING
"hier_params"        "-module" STRING                     -- internal
"hier_workers"       ( "-module" STRING | "-hier-dpi" STRING ) "-workers" INTEGER
"fsm_register_wrapper" "-module" STRING "-d" STRING "-q" STRING "-clock" STRING
                     ( "-reset" STRING )? ( "-reset_value" STRING )?
"profile_data"       ( "-mtask" STRING | "-hier-dpi" STRING ) "-cost" INTEGER   -- internal
"verilator_lib"      "-module" STRING                     -- internal

-- Deprecated, accepted and ignored
"clock_enable" "-module" STRING "-var" STRING
"clocker" | "no_clocker" "-module" STRING ( ( "-function" | "-task" ) STRING )? "-var" STRING
"isolate_assignments" "-module" STRING ( ( "-function" | "-task" ) STRING )? ( "-var" STRING )?
```

- Wildcards: strings given to `-file`, `-var`, `-scope`, `-contents` and `-match` accept `*` and `?`.
- `-msg` was a deprecated alias of `-rule` until 5.000. Diagnose it.
- `CODE` is a warning code from `09-diagnostics.md`, or a group such as `UNUSED` **(probe: groups)**.
- **Precedence of overlapping lint rules (guide):** rules with no `-file` (or `-file "*"`) apply first in parse order;
  then rules with a specific `-file`, in parse order; then `-match` rules. Control-file and metacomment suppressions are
  independent. A warning is printed only if neither suppresses it.
- File and line rules apply only to files read **after** the control file.

Corpus frequency: `forceable` 34, `fsm_register_wrapper` 29, `lint_off` 34+, `public*` 22, `inline` 7, `hier_block` 7,
`hier_workers` 8, `tracing_*` 10, `sc_bv`/`sc_biguint` 10, `timing_*` 5, `coverage_*` 5, the rest 1–4.

---

## 14. Metacomment grammar [VLT]

Each keyword below appears with the positions where it is legal. The corpus count is the number of uses in test sources.

| Keyword | Args | Position | Count | Notes |
|---|---|---|---|---|
| `lint_off` / `lint_on` | `CODE ( "," CODE )*` | anywhere | 770 / 423 | Scope is from this point onward in the file |
| `lint_save` / `lint_restore` | — | anywhere | 3 / 4 | Push and pop of the lint state |
| `public` | — | after var, param, typedef enum; inside task/func decls; after module header | 111 | Module becomes public too |
| `public_flat` / `public_flat_rd` / `public_flat_rw` | `( "@(" edge_list ")" )?` (rw only, ignored) | after var | 4 / 34 / 120 | |
| `public_module` | — | after module header | 23 | |
| `public_on` / `public_flat_on` / `public_flat_rd_on` / `public_flat_rw_on` / `public_off` | `@(...)?` | item position (a region) | 4 / 2 / – / 5 / 9 | Cannot nest |
| `forceable` | — | after var | 46 | |
| `split_var` | — | after var or net | 70 | |
| `sformat` | — | after the final `input string` tf port | 3 | |
| `sc_bv` / `sc_biguint` | — | after port | 4 / 12 | SystemC only (deferred) |
| `inline_module` / `no_inline_module` | — | module item | 24 / 66 | |
| `no_inline_task` | — | tf declaration section | 137 | |
| `hier_block` | — | module item | 26 | Only active with `--hierarchical` |
| `coverage_off` / `coverage_on` | — | anywhere | 3 / 3 | |
| `coverage_block_off` | — | inside begin/end | 2 | |
| `tracing_off` / `tracing_on` | — | anywhere | 4 / 3 | |
| `timing_off` / `timing_on` | — | anywhere | 4 / 4 | |
| `unroll_full` / `unroll_disable` | — | statement before a loop | 7 / 3 | |
| `dpi_c_decl` | `STRING` | after a DPI import | 3 | |
| `tag` | `text…` | after var or struct member | 8 | Passed to JSON/XML output |
| `fargs` | `args…` | anywhere (read *without* preprocessing when the file is given via `-f`) | 4 | Developer only |
| `fsm_state` / `fsm_reset_arc` / `fsm_arc_include_cond` | **(probe)** | after a state var? | 50 / 27 / 6 | **Not in the user guide.** FSM coverage, new feature. Needs probing |
| `clocker` / `no_clocker` / `clock_enable` / `isolate_assignments` / `sc_clock` | — | after var | 22 / – / 1 / 5 / 1 | Deprecated, ignored |
| `parallel_case` / `full_case` | — | after `case (…)` | – | Same as the synopsys pragma **(probe)** |

**Negative cases in the corpus:** `lintt_off` (unknown keyword), and a misplaced `/*verilator clocker*/` producing a
**syntax error** with the metacomment named as the unexpected token. That second case shows metacomments are real
tokens, and that a metacomment in the wrong position is a syntax error, not something silently skipped.

---

## 15. System task and function inventory

This section lists names only, because system tasks are `system_tf_call` syntactically. Semantics go in `05`, and
formatting rules in `05`/`08`. Ranked by corpus use:

- **Control and output:** `$stop` 7,891 · `$write` 4,459 · `$display` 2,823 · `$finish` 2,698 · `$fwrite` 1,545 · `$sformatf` 439 ·
  `$error` 104 · `$fatal` 80 · `$info` 33 · `$warning` 11 · `$sformat` 27 · `$swrite[bho]` · `$fdisplay[bho]` ·
  `$display[bho]` · `$fwrite[bho]` · `$strobe[bho]` · `$fstrobe[bho]` · `$monitor[bho]` · `$fmonitor[bho]` ·
  `$monitoron` / `$monitoroff` · `$exit` (alias of `$finish`) · `$system` 11 · `$stacktrace`.
- **Time:** `$time` 1,155 · `$realtime` 68 · `$stime` · `$timeformat` 19 · `$printtimescale` 17 · `$timeunit` · `$timeprecision`.
- **Types and math:** `$bits` 643 · `$clog2` 100 · `$countbits` 62 · `$signed` / `$unsigned` · `$cast` 49 · `$typename` 28 ·
  `$size` · `$dimensions` · `$unpacked_dimensions` · `$left` · `$right` · `$low` · `$high` · `$increment` ·
  `$isunbounded` · `$countones` · `$onehot` · `$onehot0` · `$isunknown` · `$rtoi` · `$itor` · `$realtobits` ·
  `$bitstoreal` · `$shortrealtobits` · `$bitstoshortreal` · `$ln` · `$log10` · `$exp` · `$sqrt` · `$pow` · `$floor` ·
  `$ceil` · `$sin` · `$cos` · `$tan` · `$asin` · `$acos` · `$atan` · `$atan2` · `$hypot` · `$sinh` · `$cosh` · `$tanh` ·
  `$asinh` · `$acosh` · `$atanh`.
- **Random:** `$random` 137 · `$urandom` 75 · `$urandom_range` · `$dist_uniform` · `$dist_normal` · `$dist_exponential` ·
  `$dist_poisson` · `$dist_chi_square` · `$dist_t` · `$dist_erlang` · `$get_initial_random_seed` [VLT].
- **File I/O:** `$fopen` · `$fclose` · `$fgetc` · `$fgets` · `$fscanf` · `$sscanf` · `$fread` · `$feof` · `$ferror` ·
  `$fflush` · `$fseek` · `$ftell` · `$rewind` / `$frewind` · `$ungetc` · `$readmemh` · `$readmemb` · `$writememh` ·
  `$writememb`.
- **Plusargs:** `$test$plusargs` · `$value$plusargs`.
- **Waveforms:** `$dumpfile` · `$dumpvars` · `$dumpon` · `$dumpoff` · `$dumpall` · `$dumplimit` · `$dumpflush` ·
  `$dumpports*` (several are accepted and ignored, per the guide).
- **Assertion control and sampled values:** see §11.
- **Inline C [VLT]:** `$c` 242 · `$c32` 77 · `$c1`/`$c8`/`$c9`/`$c48`/`$c64`/`$c100` · `$cpure`.
- **Reserved, `[UNSUP]`:** `$q_initialize` `$q_add` `$q_remove` `$q_full` `$q_exam` (Verilog-95 stochastic queues),
  and the `$coverage_*` family.
- **Ignored `[IGN]`:** timing checks (§9), `$sdf_annotate` **(probe)**.
- **Unknown names:** tests use `$unknown_pli_task` and similar. An unknown system task is an error unless `--bbox-sys` is
  given, which black-boxes it **(probe)**.

---

## 16. Ambiguities and our resolution strategy

These are real ambiguities in the LRM grammar, and any parser has to resolve them. The resolution strategy is
**ours**. It is chosen to suit a hand-written recursive-descent parser in Rust.

| # | Ambiguity | Example | Our resolution |
|---|---|---|---|
| A1 | **Type name vs value name.** The grammar can't tell `t x;` (declaration of `x` with type `t`) or `t #(8) x;` from an instantiation, or `t::m` (type scope) from `p::m` (package scope), without knowing what `t` is | `foo_t #(8) bar;` vs `foo #(8) bar();` | Keep a **scoped symbol table during parsing**. Seed it with package typedefs, class names, forward typedefs, imported names and parameter `type`s. When an identifier is not yet known, parse it as `AmbiguousName` and resolve it during elaboration (L5). The golden syntax errors show `IDENTIFIER-for-type` and `IDENTIFIER-::` as distinct unexpected tokens, so a compatible front end must make this distinction **before** reporting a syntax error |
| A2 | `<=` as NBA or relational | `a <= b <= c;` | At statement level, after an lvalue: NBA. Inside an expression: relational |
| A3 | `#` as a parameter list or a delay | `foo #(1) u();` vs `#(1) x = y;` | Decided by the leading context (instantiation vs statement). `#N` on an instance is a legacy parameter override |
| A4 | `'` + `(` / `{` | `8'(x)`, `int'(y)`, `'{1,2}`, `4'b1` | The lexer emits `'{` as one token. `'(` after a size number, type keyword or type name is a cast |
| A5 | `{` concatenation vs `'{` pattern vs `{}` empty queue vs stream `{<<…}` | | Look ahead one token after `{` |
| A6 | `module_instantiation` vs `udp_instantiation` vs net declaration with user nettype | `foo bar(a, b);` | Parse as a generic instance. Resolve to module, UDP or interface at elaboration. A user nettype declaration lacks parentheses |
| A7 | `begin : name` vs a labelled statement `name : begin` | | Both are legal. Reject when two different names are given |
| A8 | `(*` attribute vs `@(*)` | | Lexer rule (§2.4) |
| A9 | `iff`, `dist` and `inside` precedence in event and constraint contexts | | Follow LRM tables; `dist` only in `expression_or_dist` positions |
| A10 | Sequence vs expression in `@(...)` and properties | `@(s)` where `s` is a sequence | Symbol-table lookup as in A1 |
| A11 | `->` as event trigger vs implication (`a -> b` in constraints and properties) | | Context: at statement start it is a trigger; inside a constraint or expression it is implication |
| A12 | Non-ANSI port list vs ANSI | `module m(a, b);` vs `module m(input a, b);` | The first token after `(` decides; mixing is an error |
| A13 | Table-mode lexing in UDPs | §7 | Lexer mode switch on `table` / `endtable` |
| A14 | Keyword vs identifier by language mode | `logic` in 1364 mode | The L3 lexer consults the `begin_keywords` stack per token |
| A15 | Metacomment position | §5.2, §14 | Grammar slots `META_COMMENT*` at the documented positions only. Anywhere else, `lint_*`, `coverage_*`, `tracing_*` and `timing_*` are accepted as "anywhere" tokens (the lexer filters them into a side channel). Attribute-type metacomments in the wrong place are syntax errors |

**Error recovery requirement** (from T2 goldens): we only need **one** syntax error per test, at the right
file:line:col, followed by `%Error: Exiting due to N error(s)`. We don't need Verilator's recovery behaviour. Many syntax
goldens list multiple errors, though, so we must decide whether to match only the first one. Per decision 1 in `00`, we
compare (severity, code, location) tuples: **propose matching the first syntax error only**, and record it as a tolerance
in `11-test-runner.md`.

---

## 17. Open items needing black-box probes

These need a working Verilator build. Phase 0 has to solve the toolchain problem, for example by installing bison ≥ 3
and autoconf, or by using a container.

1. Whether `// synthesis translate_off/on` skips text, and what the remaining pragma words do (§2.3).
2. Which Annex E legacy directives warn and which are silently ignored (§3.2).
3. `` `undefineall `` and predefines; macro expansion inside stringification (§3.3).
4. `` `unconnected_drive `` semantics (§3.2).
5. The `--max-num-width` default (§2.6).
6. Current status of virtual interfaces, `case inside`, generate-wrapped modports and casts. The guide appears out of date (§6, §8, §10).
7. The `fsm_state`, `fsm_reset_arc` and `fsm_arc_include_cond` metacomment grammar (§14).
8. Lint-code groups accepted by `lint_off -rule` (§13).
9. `$sdf_annotate`, `--bbox-sys`, and unknown-system-task behaviour (§15).
10. The diagnostic for an unknown metacomment keyword (§2.2).
11. Recursive module instantiation terminated by generate (§4).
12. Metacomment `parallel_case` / `full_case` placement (§14).

## 18. Validation plan

1. **Parse sweep.** Run our parser over all 3,583 `.v` and 2 `.sv` sources, plus the `.vh` includes and the UVM library.
   Use the per-test flags (`--language`, `+define+`, `-I`) from `tests/manifest.json`.
   - Tests that Verilator compiles must **parse**.
   - Tests with a golden syntax error must fail **at the same first location**.
   - Tests with an `Unsupported:` golden must reach the matching construct and report it, or be recorded as
     "we support more" waivers.
2. **Preprocessor sweep.** Diff our `-E` output against all `t_preproc*` / `t_pp_*` goldens (51 tests), byte for byte.
3. **Control-file sweep.** Parse every `.vlt` file and `` `verilator_config `` region (59 + 52 files).
4. **Keyword-mode sweep.** Compile each source under every `--language` in §1.2 and check that keyword reservations
   match Annex B. This is a self-consistency test and needs no Verilator.
5. **Coverage report.** Count each grammar rule hit by the corpus. Rules with zero hits are untested, and need tests of
   our own (original Verilog, not copied).

## Provenance

- **IEEE 1800-2023** (Annex A, Annex B, Clause 22, Tables 11-2 and 22-*), **IEEE 1364-2005**: base grammar, restated.
- **Verilator user guide** at commit `bdfb2e8`: `docs/guide/extensions.rst`, `languages.rst`, `control.rst`. Read in full
  and paraphrased.
- **Verilator test corpus** at `bdfb2e8`: frequency counts and diagnostic texts extracted by our own script `tools/corpus_survey.py` over `test_regress/t/*.{v,sv,vh,vlt,out,py}`. We read individual CC0 sources:
  `t_preproc_ifexpr.v`, `t_preproc_dump_defines.out`, `t_preproc.out` (first lines only).
- **Not read:** `src/**` (including `verilog.y`, `verilog.l`, `V3PreLex.l`), `include/**`, `docs/internals.rst`,
  `test_regress/driver.py`.
