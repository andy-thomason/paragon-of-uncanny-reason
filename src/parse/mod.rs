//! Recursive-descent parser for SystemVerilog. See `docs/design/03-grammar.md`.
//!
//! The parser stops at the first syntax error, which is what the Verilator
//! tests expect to match (decision 1 in `docs/design/00-analysis-plan.md`).
//! Valid constructs Paragon does not handle yet are reported with the code
//! [`NOT_YET`](crate::diag::NOT_YET), so a test run can tell parser gaps
//! from real syntax errors.

mod expr;
mod items;
mod stmt;
mod types;

use crate::ast::SourceText;
use crate::diag::{Diag, NOT_YET};
use crate::lex::{Lexed, Token};
use std::collections::HashSet;

/// Parse lexed tokens into a syntax tree. Errors are returned as diagnostics.
pub fn parse<'a>(lexed: &Lexed<'a>) -> (SourceText<'a>, Vec<Diag<'a>>) {
    let mut p = Parser::new(&lexed.tokens, lexed.eof);
    let st = p.source_text();
    (st.unwrap_or_default(), p.diags)
}

/// The first error has been recorded in [`Parser::diags`]; stop parsing.
#[derive(Debug)]
pub(crate) struct Stop;

pub(crate) type PResult<T> = Result<T, Stop>;

pub(crate) struct Parser<'a> {
    toks: Vec<Token<'a>>,
    pos: usize,
    pub(crate) diags: Vec<Diag<'a>>,
    /// Names declared as types so far (`typedef`, type parameters).
    types: HashSet<&'a str>,
    /// An empty slice at the end of the input, for errors at end of file.
    eof: &'a str,
    /// Current nesting of statements and expressions (see [`MAX_DEPTH`]).
    depth: usize,
}

/// Nesting allowed before parsing stops with an error rather than risk the
/// stack. Callers parse on a large stack (see `libparagon`).
pub const MAX_DEPTH: usize = 20_000;

impl<'a> Parser<'a> {
    pub(crate) fn new(tokens: &[Token<'a>], eof: &'a str) -> Self {
        // Metacomments are not used by the parser yet.
        let toks: Vec<Token<'a>> = tokens
            .iter()
            .copied()
            .filter(|t| !matches!(t, Token::Meta(_)))
            .collect();
        Parser {
            toks,
            pos: 0,
            diags: Vec::new(),
            types: HashSet::new(),
            eof,
            depth: 0,
        }
    }

    // ------------------------------------------------------------ cursor

    pub(crate) fn peek(&self) -> Option<Token<'a>> {
        self.toks.get(self.pos).copied()
    }

    pub(crate) fn peek_at(&self, k: usize) -> Option<Token<'a>> {
        self.toks.get(self.pos + k).copied()
    }

    pub(crate) fn bump(&mut self) -> Option<Token<'a>> {
        let t = self.peek();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    pub(crate) fn is_op(&self, op: &str) -> bool {
        matches!(self.peek(), Some(Token::Op(o)) if o == op)
    }

    pub(crate) fn is_op_at(&self, k: usize, op: &str) -> bool {
        matches!(self.peek_at(k), Some(Token::Op(o)) if o == op)
    }

    pub(crate) fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Token::Keyword(k)) if k == kw)
    }

    pub(crate) fn is_kw_at(&self, k: usize, kw: &str) -> bool {
        matches!(self.peek_at(k), Some(Token::Keyword(w)) if w == kw)
    }

    pub(crate) fn is_ident_at(&self, k: usize) -> bool {
        matches!(
            self.peek_at(k),
            Some(Token::Ident(_) | Token::EscapedIdent(_))
        )
    }

    pub(crate) fn eat_op(&mut self, op: &str) -> Option<&'a str> {
        if self.is_op(op) {
            self.bump().map(Token::text)
        } else {
            None
        }
    }

    pub(crate) fn eat_kw(&mut self, kw: &str) -> Option<&'a str> {
        if self.is_kw(kw) {
            self.bump().map(Token::text)
        } else {
            None
        }
    }

    /// Eat one of several keywords.
    pub(crate) fn eat_kw_of(&mut self, kws: &[&str]) -> Option<&'a str> {
        match self.peek() {
            Some(Token::Keyword(k)) if kws.contains(&k) => self.bump().map(Token::text),
            _ => None,
        }
    }

    pub(crate) fn expect_op(&mut self, op: &str) -> PResult<&'a str> {
        match self.eat_op(op) {
            Some(t) => Ok(t),
            None => Err(self.unexpected(&format!("'{op}'"))),
        }
    }

    pub(crate) fn expect_kw(&mut self, kw: &str) -> PResult<&'a str> {
        match self.eat_kw(kw) {
            Some(t) => Ok(t),
            None => Err(self.unexpected(&format!("'{kw}'"))),
        }
    }

    pub(crate) fn ident(&mut self) -> PResult<&'a str> {
        match self.peek() {
            Some(Token::Ident(s) | Token::EscapedIdent(s)) => {
                self.pos += 1;
                Ok(s)
            }
            _ => Err(self.unexpected("identifier")),
        }
    }

    /// An optional `: label` after an end keyword.
    pub(crate) fn end_label(&mut self) -> PResult<()> {
        if self.is_op(":") && !self.is_op_at(0, "::") {
            self.bump();
            if self.eat_kw("new").is_none() {
                self.ident()?;
            }
        }
        Ok(())
    }

    /// The current token's text, or the end of the input.
    pub(crate) fn here(&self) -> &'a str {
        self.peek().map_or(self.eof, Token::text)
    }

    /// Run `f` one nesting level deeper, failing cleanly past [`MAX_DEPTH`].
    pub(crate) fn nested<T>(&mut self, f: impl FnOnce(&mut Self) -> PResult<T>) -> PResult<T> {
        if self.depth >= MAX_DEPTH {
            self.diags
                .push(Diag::error(self.here(), "Nesting too deep"));
            return Err(Stop);
        }
        self.depth += 1;
        let r = f(self);
        self.depth -= 1;
        r
    }

    // ------------------------------------------------------------ errors

    pub(crate) fn unexpected(&mut self, expecting: &str) -> Stop {
        let msg = match self.peek() {
            None => format!("syntax error, unexpected end of file, expecting {expecting}"),
            Some(t) => format!(
                "syntax error, unexpected '{}', expecting {expecting}",
                t.text()
            ),
        };
        self.diags.push(Diag::error(self.here(), msg));
        Stop
    }

    /// Report a construct Paragon does not support yet.
    pub(crate) fn not_yet(&mut self, at: &'a str, what: &str) -> Stop {
        self.diags
            .push(Diag::error(at, format!("Not yet supported: {what}")).with_code(NOT_YET));
        Stop
    }

    /// Skip a balanced group starting at an opening bracket, returning the index after it.
    /// If the token at `i` is not an opening bracket, returns `i + 1`.
    pub(crate) fn skip_balanced_from(&self, mut i: usize) -> usize {
        if !matches!(self.toks.get(i), Some(Token::Op("(" | "[" | "{" | "'{"))) {
            return i + 1;
        }
        let mut depth = 0usize;
        while let Some(t) = self.toks.get(i) {
            match t {
                Token::Op("(" | "[" | "{" | "'{") => depth += 1,
                Token::Op(")" | "]" | "}") => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return i + 1;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        i
    }

    /// Skip tokens up to and including the keyword `end`, for constructs we ignore.
    pub(crate) fn skip_to_kw(&mut self, end: &str) -> PResult<&'a str> {
        let start = self.here();
        while let Some(t) = self.bump() {
            if t == Token::Keyword(end) {
                return Ok(t.text());
            }
        }
        self.diags
            .push(Diag::error(start, format!("syntax error, missing {end}")));
        Err(Stop)
    }
}

#[cfg(test)]
mod tests;
