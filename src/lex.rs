//! Language lexer: turns preprocessor output into parser tokens.
//!
//! Like the preprocessor lexer, tokens are slices of source text and nothing
//! is decoded. This lexer:
//! - drops whitespace, newlines and ordinary comments, and keeps metacomments;
//! - turns identifiers into keywords for the current language version
//!   (`` `begin_keywords ``);
//! - merges based literals (`8'hFF`, `'b1x?`, `4 'd 10`) and multi-character
//!   operators (`<=`, `<<<=`, `'{`) into one token. Pieces that touch in
//!   memory are joined into one slice with no copy (see
//!   [`source::join`](crate::source::join)). Pieces that don't (a size from
//!   a macro, or spaces inside a literal) are copied into the source map;
//! - consumes compiler directives the preprocessor passed through, keeping
//!   each as one [`Token::Directive`];
//! - drops attribute instances `(* ... *)`, which Verilator ignores.

use crate::diag::{Diag, NOT_YET};
use crate::keywords::{Lang, is_keyword};
use crate::pp::Token as Pp;
use crate::source::{SourceMap, join};

/// A parser token. The payload is always the token's source text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token<'a> {
    Ident(&'a str),
    /// `\name`, including the backslash.
    EscapedIdent(&'a str),
    /// `$name`.
    SystemIdent(&'a str),
    Keyword(&'a str),
    /// Any numeric, real or time literal, undecoded: `12`, `8'hFF`, `'1`, `1.5e3`, `10ns`.
    Number(&'a str),
    /// A string literal including its quotes (`"..."` or `"""..."""`).
    Str(&'a str),
    /// An operator or punctuation, possibly several characters: `<=`, `'{`, `::`.
    Op(&'a str),
    /// A `/*verilator ...*/` metacomment.
    Meta(&'a str),
    /// A compiler directive passed through by the preprocessor, with its
    /// arguments, such as `` `timescale 1ns/1ps ``.
    Directive(&'a str),
}

impl<'a> Token<'a> {
    pub fn text(self) -> &'a str {
        match self {
            Token::Ident(s)
            | Token::EscapedIdent(s)
            | Token::SystemIdent(s)
            | Token::Keyword(s)
            | Token::Number(s)
            | Token::Str(s)
            | Token::Op(s)
            | Token::Meta(s)
            | Token::Directive(s) => s,
        }
    }
}

/// Multi-character operators, longest first.
const OPS: &[&str] = &[
    "<<<=", ">>>=", "&&&", "<->", "|->", "|=>", "#-#", "#=#", "->>", "<<=", ">>=", "===", "!==",
    "==?", "!=?", "<<<", ">>>", "+/-", "+%-", "**", "&&", "||", "==", "!=", "<=", ">=", "<<", ">>",
    "->", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "~&", "~|", "~^", "^~", "++", "--", "::",
    ":=", ":/", "+:", "-:", ".*", "##", "@@", "(*", "*)", "=>", "*>",
];

/// Directives that take the rest of the line as arguments.
const LINE_DIRECTIVES: &[&str] = &[
    "timescale",
    "default_nettype",
    "resetall",
    "celldefine",
    "endcelldefine",
    "unconnected_drive",
    "nounconnected_drive",
    "pragma",
    "uselib",
    "default_decay_time",
    "default_trireg_strength",
    "delay_mode_distributed",
    "delay_mode_path",
    "delay_mode_unit",
    "delay_mode_zero",
    "accelerate",
    "noaccelerate",
    "expand_vectornets",
    "noexpand_vectornets",
    "autoexpand_vectornets",
    "remove_gatename",
    "noremove_gatenames",
    "remove_netname",
    "noremove_netnames",
    "suppress_faults",
    "nosuppress_faults",
    "enable_portfaults",
    "disable_portfaults",
    "inline",
    "portcoerce",
    "noportcoerce",
    "protect",
    "endprotect",
    "protected",
    "endprotected",
];

/// Directives that start a raw region running to `` `verilog ``.
const REGION_DIRECTIVES: &[&str] = &[
    "systemc_header",
    "systemc_header_post",
    "systemc_interface",
    "systemc_imp_header",
    "systemc_implementation",
    "systemc_ctor",
    "systemc_dtor",
    "verilator_config",
];

pub struct Lexed<'a> {
    pub tokens: Vec<Token<'a>>,
    pub diags: Vec<Diag<'a>>,
    /// An empty slice at the end of the input, for errors at end of file.
    pub eof: &'a str,
}

/// Lex preprocessor output for the parser.
pub fn lex<'a>(sm: &'a SourceMap, input: &[Pp<'a>], lang: Lang) -> Lexed<'a> {
    let mut lx = Lexer {
        sm,
        input,
        pos: 0,
        langs: vec![lang],
        out: Vec::new(),
        diags: Vec::new(),
    };
    lx.run();
    let eof = input.last().map_or("", |t| &t.text()[t.text().len()..]);
    Lexed {
        tokens: lx.out,
        diags: lx.diags,
        eof,
    }
}

struct Lexer<'a, 'i> {
    sm: &'a SourceMap,
    input: &'i [Pp<'a>],
    pos: usize,
    langs: Vec<Lang>,
    out: Vec<Token<'a>>,
    diags: Vec<Diag<'a>>,
}

fn is_trivia(t: &Pp) -> bool {
    matches!(
        t,
        Pp::Whitespace(_) | Pp::Newline(_) | Pp::LineContinuation(_) | Pp::LineComment(_)
    ) || matches!(t, Pp::BlockComment(s) if !s.starts_with("/*verilator"))
}

/// `[sS]?[bBoOdDhH]` followed by the start of the digits, if any.
fn base_prefix(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let i = usize::from(matches!(b.first(), Some(b's' | b'S')));
    matches!(
        b.get(i),
        Some(b'b' | b'B' | b'o' | b'O' | b'd' | b'D' | b'h' | b'H')
    )
    .then_some(i + 1)
}

impl<'a, 'i> Lexer<'a, 'i> {
    fn lang(&self) -> Lang {
        *self.langs.last().unwrap()
    }

    fn peek(&self, k: usize) -> Option<Pp<'a>> {
        self.input.get(self.pos + k).copied()
    }

    /// Index of the next non-trivia token at or after `i`.
    fn significant(&self, mut i: usize) -> usize {
        while i < self.input.len() && is_trivia(&self.input[i]) {
            i += 1;
        }
        i
    }

    /// Join `parts` into one slice if they touch, otherwise copy them (without
    /// the gaps between them) into the source map.
    fn merge(&self, parts: &[&'a str]) -> &'a str {
        let mut whole = parts[0];
        for p in &parts[1..] {
            match join(whole, p) {
                Some(j) => whole = j,
                None => return self.sm.derive(parts.concat(), parts[0]),
            }
        }
        whole
    }

    fn run(&mut self) {
        while let Some(t) = self.peek(0) {
            self.pos += 1;
            match t {
                _ if is_trivia(&t) => {}
                Pp::BlockComment(s) => self.out.push(Token::Meta(s)),
                Pp::Ident(s) => {
                    // `global` is reserved only in `global clocking`; `reg global;` is legal.
                    let contextual = s == "global"
                        && !matches!(
                            self.input.get(self.significant(self.pos)),
                            Some(Pp::Ident("clocking"))
                        );
                    let tok = if is_keyword(s, self.lang()) && !contextual {
                        Token::Keyword(s)
                    } else {
                        Token::Ident(s)
                    };
                    self.out.push(tok);
                }
                Pp::EscapedIdent(s) => self.out.push(Token::EscapedIdent(s)),
                Pp::SystemIdent(s) => self.out.push(Token::SystemIdent(s)),
                Pp::Str(s) | Pp::TripleStr(s) => self.out.push(Token::Str(s)),
                Pp::Number(s) => self.number(s),
                Pp::Punct("'") => self.apostrophe(t.text()),
                Pp::Punct(s) => self.op(s),
                Pp::Directive(d) => self.directive(d),
                Pp::Protected(_) => {}
                Pp::Paste(s) | Pp::MacroQuote(s) | Pp::MacroEscapedQuote(s) => {
                    self.diags
                        .push(Diag::error(s, format!("syntax error, unexpected {s}")));
                }
                Pp::UnterminatedStr(s) | Pp::UnterminatedTripleStr(s) => {
                    self.diags.push(Diag::error(s, "Unterminated string"));
                }
                Pp::UnterminatedBlockComment(s) => {
                    self.diags
                        .push(Diag::error(s, "EOF in '/* ... */' block comment"));
                }
                Pp::Whitespace(_)
                | Pp::Newline(_)
                | Pp::LineContinuation(_)
                | Pp::LineComment(_) => {}
            }
        }
    }

    /// A decimal number, which may be the size of a based literal: `8'hFF`.
    /// The size must touch the apostrophe: Verilator lexes `# 100 'b0` as a
    /// delay of 100 and the literal `'b0` (`t_parse_delay`).
    fn number(&mut self, s: &'a str) {
        let simple = s.bytes().all(|b| b.is_ascii_digit() || b == b'_');
        // After `#` a number is a delay, never the size of a literal: `#100'b0`.
        let after_hash = self.out.last() == Some(&Token::Op("#"));
        if simple && !after_hash {
            let q = self.pos;
            if self.input.get(q) == Some(&Pp::Punct("'")) {
                let apos = self.input[q].text();
                if let Some(parts) = self.based_parts(q + 1) {
                    let mut all = vec![s, apos];
                    all.extend(parts.0);
                    self.pos = parts.1;
                    let m = self.merge(&all);
                    self.out.push(Token::Number(m));
                    return;
                }
            }
        }
        self.out.push(Token::Number(s));
    }

    /// The base and digits of a based literal starting at input index `i`
    /// (just after the apostrophe). Returns the parts and the index after them.
    fn based_parts(&self, i: usize) -> Option<(Vec<&'a str>, usize)> {
        let Some(Pp::Ident(b)) = self.input.get(i).copied() else {
            return None;
        };
        let digits_at = base_prefix(b)?;
        let mut parts = vec![b];
        let mut j = i + 1;
        if digits_at == b.len() {
            // Digits after optional whitespace: 'h FF
            let k = self.significant(j);
            match self.input.get(k) {
                Some(Pp::Number(d) | Pp::Ident(d)) => {
                    parts.push(d);
                    j = k + 1;
                }
                Some(Pp::Punct(d @ ("?" | "_"))) => {
                    parts.push(d);
                    j = k + 1;
                }
                _ => return None,
            }
        }
        // Further digit pieces that touch: 'b10?? or 'hx_z.
        while let Some(next) = self.input.get(j) {
            match next {
                Pp::Number(d) | Pp::Ident(d) | Pp::Punct(d @ "?")
                    if join(parts.last().unwrap(), d).is_some() =>
                {
                    parts.push(d);
                    j += 1;
                }
                _ => break,
            }
        }
        Some((parts, j))
    }

    fn apostrophe(&mut self, apos: &'a str) {
        // Unsized based literal: 'hFF, 'sb1
        if let Some((parts, j)) = self.based_parts(self.pos) {
            let mut all = vec![apos];
            all.extend(parts);
            self.pos = j;
            let m = self.merge(&all);
            self.out.push(Token::Number(m));
            return;
        }
        match self.peek(0) {
            // Unbased unsized: '0 '1 'x 'z
            Some(Pp::Number(d @ ("0" | "1")) | Pp::Ident(d @ ("x" | "X" | "z" | "Z")))
                if join(apos, d).is_some() =>
            {
                self.pos += 1;
                self.out.push(Token::Number(join(apos, d).unwrap()));
            }
            Some(Pp::Punct(p @ "{")) if join(apos, p).is_some() => {
                self.pos += 1;
                self.out.push(Token::Op(join(apos, p).unwrap()));
            }
            _ => self.out.push(Token::Op(apos)),
        }
    }

    fn op(&mut self, first: &'a str) {
        // Longest operator made of touching punctuation.
        let mut text = first;
        let mut used = 0;
        let mut j = self.pos;
        while let Some(Pp::Punct(p)) = self.input.get(j).copied() {
            let Some(longer) = join(text, p) else { break };
            if !OPS.iter().any(|o| o.starts_with(longer)) {
                break;
            }
            text = longer;
            j += 1;
            if OPS.contains(&text) {
                used = j - self.pos;
            }
        }
        let op = if used > 0 {
            join_n(first, &self.input[self.pos..self.pos + used])
        } else {
            first
        };
        // `@(*)` is an event control, not an attribute: keep `(` `*` `)` apart.
        if op == "(*"
            && matches!(
                self.input.get(self.significant(self.pos + used)),
                Some(Pp::Punct(")"))
            )
        {
            self.out.push(Token::Op(first));
            return;
        }
        if op == "*)" && self.out.last() == Some(&Token::Op("(")) {
            self.out.push(Token::Op(first));
            return;
        }
        self.pos += used;
        if op == "(*" {
            return self.skip_attribute(op);
        }
        self.out.push(Token::Op(op));
    }

    /// Drop an attribute instance `(* ... *)`.
    fn skip_attribute(&mut self, start: &'a str) {
        while let Some(t) = self.peek(0) {
            self.pos += 1;
            if let Pp::Punct("*") = t
                && let Some(Pp::Punct(")")) = self.peek(0)
                && join(t.text(), self.peek(0).unwrap().text()).is_some()
            {
                self.pos += 1;
                return;
            }
        }
        self.diags.push(Diag::error(start, "EOF in (*"));
    }

    fn rest_of_line(&mut self) -> Vec<&'a str> {
        let mut parts = Vec::new();
        while let Some(t) = self.peek(0) {
            if let Pp::Newline(_) = t {
                break;
            }
            self.pos += 1;
            if !is_trivia(&t) {
                parts.push(t.text());
            }
        }
        parts
    }

    fn directive(&mut self, d: &'a str) {
        let name = &d[1..];
        match name {
            "begin_keywords" => {
                let k = self.significant(self.pos);
                match self.input.get(k).copied() {
                    Some(Pp::Str(s)) => {
                        self.pos = k + 1;
                        match Lang::parse(&s[1..s.len() - 1]) {
                            Some(l) => self.langs.push(l),
                            None => {
                                self.diags.push(Diag::error(
                                    s,
                                    format!("Unknown language specified: {s}"),
                                ));
                                self.langs.push(self.lang());
                            }
                        }
                    }
                    _ => self.diags.push(Diag::error(
                        d,
                        "Expecting a version string after `begin_keywords",
                    )),
                }
            }
            "end_keywords" => {
                if self.langs.len() > 1 {
                    self.langs.pop();
                }
            }
            _ if LINE_DIRECTIVES.contains(&name) => {
                let mut parts = vec![d];
                parts.extend(self.rest_of_line());
                let begins_envelope = name == "pragma"
                    && parts.get(1) == Some(&"protect")
                    && parts.contains(&"begin_protected");
                let text = if parts.len() == 1 {
                    d
                } else {
                    self.merge_spaced(&parts)
                };
                self.out.push(Token::Directive(text));
                if begins_envelope {
                    self.skip_protected_envelope(d);
                }
            }
            _ if REGION_DIRECTIVES.contains(&name) => {
                while let Some(t) = self.peek(0) {
                    if matches!(t, Pp::Directive(e) if e == "`verilog" || REGION_DIRECTIVES.contains(&&e[1..]))
                    {
                        break;
                    }
                    self.pos += 1;
                }
                if self.peek(0) == Some(Pp::Directive("`verilog")) {
                    self.pos += 1;
                }
                self.diags.push(
                    Diag::error(d, format!("Not yet supported: `{name} regions"))
                        .with_code(NOT_YET),
                );
            }
            "verilog" => {}
            _ => self.diags.push(Diag::error(
                d,
                format!("Define or directive not defined: '{d}'"),
            )),
        }
    }

    /// Skip the encrypted body of a `` `pragma protect begin_protected ``
    /// envelope, up to and including `` `pragma protect end_protected ``.
    fn skip_protected_envelope(&mut self, start: &'a str) {
        while let Some(t) = self.peek(0) {
            self.pos += 1;
            if let Pp::Directive("`pragma") = t {
                let parts = self.rest_of_line();
                if parts.first() == Some(&"protect") && parts.contains(&"end_protected") {
                    return;
                }
            }
        }
        self.diags
            .push(Diag::error(start, "Missing `pragma protect end_protected"));
    }

    /// A directive and its arguments as one slice when they lie in one buffer,
    /// otherwise copied with single spaces between parts.
    fn merge_spaced(&self, parts: &[&'a str]) -> &'a str {
        if let (Some((b0, o0)), Some((b1, o1))) =
            (self.sm.find(parts[0]), self.sm.find(parts[parts.len() - 1]))
            && b0 == b1
        {
            let last = parts[parts.len() - 1];
            return &self.sm.text(b0)[o0..o1 + last.len()];
        }
        self.sm.derive(parts.join(" "), parts[0])
    }
}

/// Join `first` with the texts of `rest`, which are known to touch.
fn join_n<'a>(first: &'a str, rest: &[Pp<'a>]) -> &'a str {
    rest.iter().fold(first, |acc, t| {
        join(acc, t.text()).expect("touching punctuation")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pp;

    fn toks(src: &str) -> Vec<String> {
        let sm = SourceMap::new();
        let (_, text) = sm.add(src.to_string(), crate::source::Origin::CommandLine);
        let input: Vec<Pp> = pp::lex(text).collect();
        let l = lex(&sm, &input, Lang::Sv2023);
        assert!(l.diags.is_empty(), "{:?}", l.diags);
        l.tokens.iter().map(|t| format!("{t:?}")).collect()
    }

    fn one(src: &str) -> String {
        let t = toks(src);
        assert_eq!(t.len(), 1, "{t:?}");
        t[0].clone()
    }

    #[test]
    fn keywords_and_identifiers() {
        assert_eq!(toks("module m"), ["Keyword(\"module\")", "Ident(\"m\")"]);
    }

    #[test]
    fn based_numbers() {
        assert_eq!(one("8'hFF"), "Number(\"8'hFF\")");
        assert_eq!(one("'sb101"), "Number(\"'sb101\")");
        assert_eq!(one("4'b1x?z"), "Number(\"4'b1x?z\")");
        assert_eq!(one("'0"), "Number(\"'0\")");
        assert_eq!(one("'x"), "Number(\"'x\")");
        assert_eq!(one("16'h dead_beef"), "Number(\"16'hdead_beef\")");
        assert_eq!(toks("8 'd 255"), ["Number(\"8\")", "Number(\"'d255\")"]);
        assert_eq!(one("1.5e-3"), "Number(\"1.5e-3\")");
        assert_eq!(one("10ns"), "Number(\"10ns\")");
    }

    #[test]
    fn casts_and_patterns_are_not_numbers() {
        assert_eq!(
            toks("8'(x)"),
            [
                "Number(\"8\")",
                "Op(\"'\")",
                "Op(\"(\")",
                "Ident(\"x\")",
                "Op(\")\")"
            ]
        );
        assert_eq!(toks("'{1}"), ["Op(\"'{\")", "Number(\"1\")", "Op(\"}\")"]);
    }

    #[test]
    fn number_after_hash_is_a_delay() {
        assert_eq!(
            toks("#100'b0"),
            ["Op(\"#\")", "Number(\"100\")", "Number(\"'b0\")"]
        );
    }

    #[test]
    fn global_is_contextual() {
        assert_eq!(toks("reg global;")[1], "Ident(\"global\")");
        assert_eq!(toks("global clocking")[0], "Keyword(\"global\")");
    }

    #[test]
    fn operators() {
        assert_eq!(toks("a<=b"), ["Ident(\"a\")", "Op(\"<=\")", "Ident(\"b\")"]);
        assert_eq!(
            toks("a<<<=b"),
            ["Ident(\"a\")", "Op(\"<<<=\")", "Ident(\"b\")"]
        );
        assert_eq!(
            toks("a < = b"),
            ["Ident(\"a\")", "Op(\"<\")", "Op(\"=\")", "Ident(\"b\")"]
        );
        assert_eq!(
            toks("x[a+:4]"),
            [
                "Ident(\"x\")",
                "Op(\"[\")",
                "Ident(\"a\")",
                "Op(\"+:\")",
                "Number(\"4\")",
                "Op(\"]\")"
            ]
        );
        assert_eq!(toks("p::q"), ["Ident(\"p\")", "Op(\"::\")", "Ident(\"q\")"]);
    }

    #[test]
    fn event_star_is_not_an_attribute() {
        assert_eq!(
            toks("@(*)"),
            ["Op(\"@\")", "Op(\"(\")", "Op(\"*\")", "Op(\")\")"]
        );
        assert_eq!(toks("@*"), ["Op(\"@\")", "Op(\"*\")"]);
    }

    #[test]
    fn attributes_are_dropped() {
        assert_eq!(toks("(* full_case *) x"), ["Ident(\"x\")"]);
    }

    #[test]
    fn directives_keep_their_arguments() {
        assert_eq!(
            toks("`timescale 1ns / 1ps\nm"),
            ["Directive(\"`timescale 1ns / 1ps\")", "Ident(\"m\")"]
        );
    }

    #[test]
    fn begin_keywords_changes_reserved_words() {
        let t = toks("`begin_keywords \"1364-2005\"\nlogic\n`end_keywords\nlogic");
        assert_eq!(t, ["Ident(\"logic\")", "Keyword(\"logic\")"]);
    }

    #[test]
    fn metacomments_are_kept() {
        assert_eq!(
            toks("x /*verilator public*/"),
            ["Ident(\"x\")", "Meta(\"/*verilator public*/\")"]
        );
    }

    #[test]
    fn undefined_macro_is_an_error() {
        let sm = SourceMap::new();
        let (_, text) = sm.add("`nope".into(), crate::source::Origin::CommandLine);
        let input: Vec<Pp> = pp::lex(text).collect();
        let l = lex(&sm, &input, Lang::Sv2023);
        assert_eq!(
            l.diags[0].message,
            "Define or directive not defined: '`nope'"
        );
    }
}
