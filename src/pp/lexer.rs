//! Preprocessor lexer.
//!
//! The lexer is deliberately light. Every token is a slice of the source text
//! and nothing is decoded: numbers stay as text, strings keep their quotes and
//! escapes. Concatenating the token texts reproduces the input exactly, so
//! positions can be recovered from slice pointers (see [`crate::source`]).
//!
//! The lexer never fails. Malformed input such as an unterminated string or
//! comment becomes an error token, and the preprocessor decides what to report.

/// One preprocessor token. The payload is always the exact source text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token<'a> {
    /// Spaces, tabs, vertical tabs and form feeds.
    Whitespace(&'a str),
    /// `\n`, `\r\n` or a lone `\r`.
    Newline(&'a str),
    /// A backslash immediately followed by a newline. Continues a `` `define `` body.
    LineContinuation(&'a str),
    /// `// ...` up to, not including, the newline. A trailing backslash-newline
    /// is left out too, so that a comment at the end of a define line still
    /// continues the define.
    LineComment(&'a str),
    /// `/* ... */`.
    BlockComment(&'a str),
    /// `` `name ``: a compiler directive or a macro use, including the backtick.
    Directive(&'a str),
    /// ``` `` ```: token paste in a macro body.
    Paste(&'a str),
    /// `` `" ``: starts or ends a stringification in a macro body.
    MacroQuote(&'a str),
    /// `` `\`" ``: a literal `\"` inside a stringification.
    MacroEscapedQuote(&'a str),
    /// A simple identifier or keyword: `[A-Za-z_][A-Za-z0-9_$]*`.
    Ident(&'a str),
    /// `\name`: the backslash and the name, not the terminating whitespace.
    /// It also stops before ``` `` ```, so a macro body can paste onto an
    /// escaped identifier.
    EscapedIdent(&'a str),
    /// `$name`: a system task or function name.
    SystemIdent(&'a str),
    /// A run of digits plus any letters, underscores and decimal points that
    /// follow, and an exponent sign: `12`, `1.5e-3`, `10ns`, `1step`. Based
    /// literals are split: `8'hFF` is `Number("8")`, `Punct("'")`, `Ident("hFF")`.
    Number(&'a str),
    /// `"..."`, including the quotes.
    Str(&'a str),
    /// `"""..."""` (IEEE 1800-2023), including the quotes.
    TripleStr(&'a str),
    /// Any other single character.
    Punct(&'a str),
    /// The raw body between `` `protected `` and `` `endprotected ``. It is
    /// encrypted or vendor-encoded text that may contain unbalanced quotes,
    /// so it is not lexed. Runs to end of file if `` `endprotected `` is missing.
    Protected(&'a str),
    /// A `"` string ended by a raw newline or end of file. The token stops
    /// before the newline.
    UnterminatedStr(&'a str),
    /// A `"""` string ended by end of file.
    UnterminatedTripleStr(&'a str),
    /// A `/*` comment ended by end of file.
    UnterminatedBlockComment(&'a str),
}

impl<'a> Token<'a> {
    /// The exact source text of the token.
    #[inline]
    pub fn text(self) -> &'a str {
        use Token::*;
        match self {
            Whitespace(s)
            | Newline(s)
            | LineContinuation(s)
            | LineComment(s)
            | BlockComment(s)
            | Directive(s)
            | Paste(s)
            | MacroQuote(s)
            | MacroEscapedQuote(s)
            | Ident(s)
            | EscapedIdent(s)
            | SystemIdent(s)
            | Number(s)
            | Str(s)
            | TripleStr(s)
            | Punct(s)
            | Protected(s)
            | UnterminatedStr(s)
            | UnterminatedTripleStr(s)
            | UnterminatedBlockComment(s) => s,
        }
    }

    /// True for the tokens that represent malformed input.
    #[inline]
    pub fn is_error(self) -> bool {
        matches!(
            self,
            Token::UnterminatedStr(_)
                | Token::UnterminatedTripleStr(_)
                | Token::UnterminatedBlockComment(_)
        )
    }

    /// True for whitespace and comments, which carry no meaning outside a define body.
    #[inline]
    pub fn is_trivia(self) -> bool {
        matches!(
            self,
            Token::Whitespace(_) | Token::LineComment(_) | Token::BlockComment(_)
        )
    }
}

/// Iterator over the tokens of a source string.
#[derive(Clone, Debug)]
pub struct Lexer<'a> {
    src: &'a str,
    pos: usize,
    /// The previous token was `` `protected ``.
    in_protected: bool,
}

/// Lex `src` into preprocessor tokens.
#[inline]
pub fn lex(src: &str) -> Lexer<'_> {
    Lexer {
        src,
        pos: 0,
        in_protected: false,
    }
}

#[inline]
fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

#[inline]
fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

#[inline]
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | 0x0b | 0x0c)
}

/// Length of a newline sequence starting at `i`, or 0.
#[inline]
fn newline_len(bytes: &[u8], i: usize) -> usize {
    match bytes.get(i) {
        Some(b'\n') => 1,
        Some(b'\r') if bytes.get(i + 1) == Some(&b'\n') => 2,
        Some(b'\r') => 1,
        _ => 0,
    }
}

/// Byte length of the UTF-8 character whose first byte is `b`.
#[inline]
fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

impl<'a> Lexer<'a> {
    /// The source being lexed.
    pub fn source(&self) -> &'a str {
        self.src
    }

    /// Scan forward from `i` while `f` holds.
    #[inline]
    fn end_while(&self, mut i: usize, f: impl Fn(u8) -> bool) -> usize {
        let bytes = self.src.as_bytes();
        while i < bytes.len() && f(bytes[i]) {
            i += 1;
        }
        i
    }

    /// End of a `"` string body starting just after the opening quote.
    /// Returns the end offset and whether a closing quote was found.
    fn string_end(&self, mut i: usize) -> (usize, bool) {
        let bytes = self.src.as_bytes();
        while i < bytes.len() {
            match bytes[i] {
                b'"' => return (i + 1, true),
                b'\\' => {
                    // An escaped newline continues the string onto the next line.
                    let nl = newline_len(bytes, i + 1);
                    i += 1 + if nl > 0 { nl } else { 1 };
                }
                b'\n' | b'\r' => return (i, false),
                _ => i += 1,
            }
        }
        (bytes.len(), false)
    }

    /// End of a `"""` string body starting just after the opening quotes.
    fn triple_string_end(&self, mut i: usize) -> (usize, bool) {
        let bytes = self.src.as_bytes();
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => i += 2,
                b'"' if bytes[i..].starts_with(b"\"\"\"") => return (i + 3, true),
                _ => i += 1,
            }
        }
        (bytes.len(), false)
    }

    /// End of a number starting with a digit at `i`.
    fn number_end(&self, mut i: usize) -> usize {
        let bytes = self.src.as_bytes();
        while i < bytes.len() {
            let b = bytes[i];
            if b.is_ascii_alphanumeric() || b == b'_' {
                i += 1;
            } else if bytes.get(i + 1).is_some_and(u8::is_ascii_digit)
                && (b == b'.' || (matches!(b, b'+' | b'-') && matches!(bytes[i - 1], b'e' | b'E')))
            {
                // A decimal point, or an exponent sign, followed by a digit.
                i += 2;
            } else {
                break;
            }
        }
        i
    }

    /// End of an escaped identifier whose backslash is at `i`.
    fn escaped_ident_end(&self, mut i: usize) -> usize {
        let bytes = self.src.as_bytes();
        i += 1;
        while i < bytes.len() {
            let b = bytes[i];
            if is_space(b) || b == b'\n' || b == b'\r' || bytes[i..].starts_with(b"``") {
                break;
            }
            i += 1;
        }
        i
    }
}

impl<'a> Iterator for Lexer<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Token<'a>> {
        let src = self.src;
        let bytes = src.as_bytes();
        let start = self.pos;
        let b = *bytes.get(start)?;

        if std::mem::take(&mut self.in_protected) {
            let end = src[start..]
                .find("`endprotected")
                .map_or(bytes.len(), |n| start + n);
            if end > start {
                self.pos = end;
                return Some(Token::Protected(&src[start..end]));
            }
        }
        let next = bytes.get(start + 1).copied();

        let (end, kind): (usize, fn(&'a str) -> Token<'a>) = match b {
            b'\n' | b'\r' => (start + newline_len(bytes, start), Token::Newline),
            _ if is_space(b) => (self.end_while(start, is_space), Token::Whitespace),

            b'/' if next == Some(b'/') => {
                let mut i = start + 2;
                while i < bytes.len() && newline_len(bytes, i) == 0 {
                    if bytes[i] == b'\\' && newline_len(bytes, i + 1) > 0 {
                        break;
                    }
                    i += 1;
                }
                (i, Token::LineComment)
            }
            b'/' if next == Some(b'*') => match src[start + 2..].find("*/") {
                Some(n) => (start + 2 + n + 2, Token::BlockComment),
                None => (bytes.len(), Token::UnterminatedBlockComment),
            },

            b'`' => match next {
                Some(b'`') => (start + 2, Token::Paste),
                Some(b'"') => (start + 2, Token::MacroQuote),
                Some(b'\\') if bytes[start..].starts_with(b"`\\`\"") => {
                    (start + 4, Token::MacroEscapedQuote)
                }
                Some(c) if is_ident_start(c) => {
                    (self.end_while(start + 1, is_ident_char), Token::Directive)
                }
                _ => (start + 1, Token::Punct),
            },

            b'"' if bytes[start..].starts_with(b"\"\"\"") => {
                match self.triple_string_end(start + 3) {
                    (end, true) => (end, Token::TripleStr),
                    (end, false) => (end, Token::UnterminatedTripleStr),
                }
            }
            b'"' => match self.string_end(start + 1) {
                (end, true) => (end, Token::Str),
                (end, false) => (end, Token::UnterminatedStr),
            },

            b'\\' => {
                let nl = newline_len(bytes, start + 1);
                if nl > 0 {
                    (start + 1 + nl, Token::LineContinuation)
                } else if next.is_some_and(|c| !(is_space(c) || c == b'`')) {
                    (self.escaped_ident_end(start), Token::EscapedIdent)
                } else {
                    (start + 1, Token::Punct)
                }
            }

            b'$' if next.is_some_and(is_ident_char) => {
                (self.end_while(start + 1, is_ident_char), Token::SystemIdent)
            }
            _ if is_ident_start(b) => (self.end_while(start, is_ident_char), Token::Ident),
            _ if b.is_ascii_digit() => (self.number_end(start), Token::Number),

            _ => (start + utf8_len(b), Token::Punct),
        };

        self.pos = end;
        let tok = kind(&src[start..end]);
        self.in_protected = tok == Token::Directive("`protected");
        Some(tok)
    }
}

#[cfg(test)]
mod tests {
    use super::Token::*;
    use super::*;

    fn toks(src: &str) -> Vec<Token<'_>> {
        lex(src).collect()
    }

    #[test]
    fn empty() {
        assert_eq!(toks(""), vec![]);
    }

    #[test]
    fn whitespace_and_newlines() {
        assert_eq!(
            toks(" \t\n\r\n\rx"),
            vec![
                Whitespace(" \t"),
                Newline("\n"),
                Newline("\r\n"),
                Newline("\r"),
                Ident("x")
            ]
        );
    }

    #[test]
    fn comments() {
        assert_eq!(
            toks("a // c\n/* b\n */"),
            vec![
                Ident("a"),
                Whitespace(" "),
                LineComment("// c"),
                Newline("\n"),
                BlockComment("/* b\n */"),
            ]
        );
        assert_eq!(toks("/* x"), vec![UnterminatedBlockComment("/* x")]);
        assert_eq!(toks("/**/"), vec![BlockComment("/**/")]);
        assert_eq!(toks("/*/ */"), vec![BlockComment("/*/ */")]);
    }

    #[test]
    fn line_comment_keeps_continuation() {
        assert_eq!(
            toks("// c \\\nx"),
            vec![LineComment("// c "), LineContinuation("\\\n"), Ident("x")]
        );
        // A backslash elsewhere in the comment is just text.
        assert_eq!(toks("// a\\b"), vec![LineComment("// a\\b")]);
    }

    #[test]
    fn directives_and_macro_specials() {
        assert_eq!(
            toks("`define `__FILE__ `` `\" `\\`\" `"),
            vec![
                Directive("`define"),
                Whitespace(" "),
                Directive("`__FILE__"),
                Whitespace(" "),
                Paste("``"),
                Whitespace(" "),
                MacroQuote("`\""),
                Whitespace(" "),
                MacroEscapedQuote("`\\`\""),
                Whitespace(" "),
                Punct("`"),
            ]
        );
    }

    #[test]
    fn triple_backtick_is_paste_then_directive() {
        assert_eq!(toks("```BAR"), vec![Paste("``"), Directive("`BAR")]);
        assert_eq!(toks("```\""), vec![Paste("``"), MacroQuote("`\"")]);
    }

    #[test]
    fn identifiers() {
        assert_eq!(
            toks("a_1$b $display $ \\esc+id x"),
            vec![
                Ident("a_1$b"),
                Whitespace(" "),
                SystemIdent("$display"),
                Whitespace(" "),
                Punct("$"),
                Whitespace(" "),
                EscapedIdent("\\esc+id"),
                Whitespace(" "),
                Ident("x"),
            ]
        );
        assert_eq!(toks("$test$plusargs"), vec![SystemIdent("$test$plusargs")]);
    }

    #[test]
    fn escaped_ident_stops_at_paste() {
        assert_eq!(
            toks("\\inv_``out "),
            vec![
                EscapedIdent("\\inv_"),
                Paste("``"),
                Ident("out"),
                Whitespace(" ")
            ]
        );
    }

    #[test]
    fn backslash_forms() {
        assert_eq!(toks("\\\r\n"), vec![LineContinuation("\\\r\n")]);
        assert_eq!(toks("\\ "), vec![Punct("\\"), Whitespace(" ")]);
        assert_eq!(toks("\\"), vec![Punct("\\")]);
    }

    #[test]
    fn numbers() {
        assert_eq!(
            toks("8'hFF 1.5e-3 10ns 1step 3'b1_0x 'z 1.x"),
            vec![
                Number("8"),
                Punct("'"),
                Ident("hFF"),
                Whitespace(" "),
                Number("1.5e-3"),
                Whitespace(" "),
                Number("10ns"),
                Whitespace(" "),
                Number("1step"),
                Whitespace(" "),
                Number("3"),
                Punct("'"),
                Ident("b1_0x"),
                Whitespace(" "),
                Punct("'"),
                Ident("z"),
                Whitespace(" "),
                Number("1"),
                Punct("."),
                Ident("x"),
            ]
        );
        assert_eq!(toks("a-1"), vec![Ident("a"), Punct("-"), Number("1")]);
    }

    #[test]
    fn strings() {
        assert_eq!(
            toks(r#""a\"b" "" "x\\""#),
            vec![
                Str(r#""a\"b""#),
                Whitespace(" "),
                Str(r#""""#),
                Whitespace(" "),
                Str(r#""x\\""#),
            ]
        );
        assert_eq!(toks("\"a\\\nb\""), vec![Str("\"a\\\nb\"")]);
        assert_eq!(
            toks("\"ab\nc"),
            vec![UnterminatedStr("\"ab"), Newline("\n"), Ident("c")]
        );
        assert_eq!(toks("\"ab"), vec![UnterminatedStr("\"ab")]);
        // A backtick in a string is plain text.
        assert_eq!(toks("\"`x\""), vec![Str("\"`x\"")]);
    }

    #[test]
    fn triple_strings() {
        assert_eq!(
            toks("\"\"\"a \"q\"\n'b'\"\"\" x"),
            vec![
                TripleStr("\"\"\"a \"q\"\n'b'\"\"\""),
                Whitespace(" "),
                Ident("x")
            ]
        );
        assert_eq!(toks("\"\"\"ab"), vec![UnterminatedTripleStr("\"\"\"ab")]);
        assert_eq!(
            toks("\"\"\"\\\"\"\"\""),
            vec![TripleStr("\"\"\"\\\"\"\"\"")]
        );
    }

    #[test]
    fn protected_body_is_raw() {
        assert_eq!(
            toks("`protected\n\"x'\n`endprotected\n"),
            vec![
                Directive("`protected"),
                Protected("\n\"x'\n"),
                Directive("`endprotected"),
                Newline("\n"),
            ]
        );
        assert_eq!(toks("`protected"), vec![Directive("`protected")]);
        assert_eq!(
            toks("`protected a\""),
            vec![Directive("`protected"), Protected(" a\"")]
        );
    }

    #[test]
    fn non_ascii_punct() {
        assert_eq!(toks("é·"), vec![Punct("é"), Punct("·")]);
    }

    #[test]
    fn round_trip_is_lossless() {
        let src =
            "`define X(a,b=1) a``b \\\n `\"a`\" // c\n\"s\" \"\"\"t\"\"\" \\e $f 8'h1 é\r\n/*";
        let joined: std::string::String = lex(src).map(Token::text).collect();
        assert_eq!(joined, src);
    }
}
