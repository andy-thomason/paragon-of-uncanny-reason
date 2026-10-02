//! SystemVerilog preprocessor (IEEE 1800-2023 clause 22). See `docs/design/03-grammar.md` §3.

pub mod lexer;

pub use lexer::{Lexer, Token, lex};
