# Verilator CC0 test fixtures

These files were copied unchanged from the Verilator regression suite
(`test_regress/t/`, commit `bdfb2e8db16bf59dd6bc9b1f4009d30a11510888`).
Each one carries `SPDX-License-Identifier: CC0-1.0` in its header. They are
public domain, so we can include them here. Keep the headers intact.

Only add files whose header says CC0. Files licensed under LGPL (including every
`.py` test driver) must stay in `../verilator` and be read from there.

| File | Used for |
|---|---|
| `t_preproc.v` | The main preprocessor torture test (lossless lexing, macro specials) |
| `t_preproc_ifexpr.v` | 1800-2023 `` `ifdef `` expressions |
| `t_preproc_strify_join.v` | Stringification with token paste |
| `t_preproc_def09.v` | 1800-2009 define defaults |
| `t_preproc_stringend_bad.v`, `t_preproc_eof4_bad.v` | Unterminated `"` string |
| `t_preproc_cmtend_bad.v`, `t_preproc_eof1_bad.v` | Unterminated `/*` comment |
| `t_preproc_eof_qqq_bad.v` | Unterminated `"""` string |

## Where Verilator reports errors (from the golden `.out` files)

| Test | Lines in file | Error token starts | Verilator reports |
|---|---|---|---|
| `t_preproc_stringend_bad` | 7 | 7:1 | 8:1 |
| `t_preproc_eof4_bad` | 7 | 7:1 | 8:1 |
| `t_preproc_cmtend_bad` | 8 | 7:1 | 10:1 |
| `t_preproc_eof1_bad` | 7 | 7:1 | 9:1 |
| `t_preproc_eof_qqq_bad` | 7 | 7:1 | 10:1 |

Verilator reports these at or past end of file, not where the bad token starts.
For comments it is one line past EOF, and for `"""` it is two lines past.
Our diagnostics layer has to decide whether to reproduce this, because
decision 1 in `docs/design/00-analysis-plan.md` matches on location. This needs
a black-box probe with more lines after the bad token.

The reported position also depends on the mode. `t_parse_eof_qqq_bad.v` has the
same content as `t_preproc_eof_qqq_bad.v`, but it is compiled rather than run
with `-E`, and Verilator reports it at 7:1, the start of the token.
