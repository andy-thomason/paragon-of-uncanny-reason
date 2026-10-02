//! Paragon of Uncanny Reason: a clean-room SystemVerilog simulator front end.
//!
//! DO NOT COPY CODE from Verilator. See `docs/design/README.md`.

pub mod ast;
pub mod bits;
pub mod diag;
pub mod elab;
pub mod eval;
pub mod ir;
pub mod keywords;
pub mod lex;
pub mod parse;
pub mod pp;
pub mod sim;
pub mod source;
