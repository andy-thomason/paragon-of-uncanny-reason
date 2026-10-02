//! SystemVerilog preprocessor (IEEE 1800-2023 clause 22). See `docs/design/03-grammar.md` §3.
//!
//! The preprocessor reads lexer tokens from a stack of input frames (files and
//! macro expansions) and produces an output token stream. Macro expansion is
//! textual: formals are replaced by the raw argument text, the result is added
//! to the [`SourceMap`] as a new buffer, and that buffer is pushed as a frame
//! and rescanned. Output tokens are therefore always slices of a buffer in the
//! map, and their positions can be traced back to the use site.

pub mod emit;
pub mod lexer;

pub use lexer::{Lexer, Token, lex};

use crate::source::{BufId, Origin, SourceMap};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

pub use crate::diag::{Diag, Severity};

/// Where the preprocessor reads files from.
pub trait FileSystem {
    fn read(&self, path: &str) -> Option<String>;
}

/// Reads files from the real file system.
pub struct StdFs;

impl FileSystem for StdFs {
    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    /// Directories searched for `` `include `` files, in order. `"."` is the
    /// current directory and produces names without a directory prefix.
    pub include_dirs: Vec<String>,
    /// Search the including file's directory first.
    pub relative_includes: bool,
    /// Suffixes tried for each include candidate.
    pub lib_exts: Vec<String>,
    /// Command-line defines (`-DNAME=VALUE`). An empty value defines the name with no text.
    pub defines: Vec<(String, String)>,
    /// Keep ordinary comments in the output instead of replacing them with a space.
    pub keep_comments: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            include_dirs: vec![".".into()],
            relative_includes: false,
            lib_exts: vec!["".into(), ".v".into(), ".sv".into()],
            defines: Vec::new(),
            keep_comments: false,
        }
    }
}

/// Directives the preprocessor implements itself.
const HANDLED: &[&str] = &[
    "define",
    "undef",
    "undefineall",
    "ifdef",
    "ifndef",
    "elsif",
    "else",
    "endif",
    "include",
    "line",
    "__FILE__",
    "__LINE__",
    "error",
];

/// Directives passed through to the parser unchanged.
const PASS_THROUGH: &[&str] = &[
    "timescale",
    "resetall",
    "default_nettype",
    "celldefine",
    "endcelldefine",
    "unconnected_drive",
    "nounconnected_drive",
    "pragma",
    "begin_keywords",
    "end_keywords",
    "protected",
    "endprotected",
    "protect",
    "endprotect",
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
    "systemc_header",
    "systemc_header_post",
    "systemc_interface",
    "systemc_imp_header",
    "systemc_implementation",
    "systemc_ctor",
    "systemc_dtor",
    "systemc_class_name",
    "verilog",
    "verilator_config",
];

fn is_builtin(name: &str) -> bool {
    HANDLED.contains(&name) || PASS_THROUGH.contains(&name)
}

/// Expansions allowed between two newlines of a file before we assume recursion.
const MAX_EXPANSIONS_PER_LINE: usize = 100_000;
/// Nested input frames allowed before we assume recursion.
const MAX_FRAMES: usize = 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MacroKind {
    Predefined,
    CommandLine,
    User,
}

struct Macro<'a> {
    formals: Option<Vec<Formal<'a>>>,
    body: Vec<Token<'a>>,
    at: &'a str,
    kind: MacroKind,
}

impl Macro<'_> {
    fn body_text(&self) -> String {
        self.body
            .iter()
            .map(|t| t.text())
            .collect::<String>()
            .trim()
            .to_string()
    }
}

struct Formal<'a> {
    name: &'a str,
    default: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FrameKind {
    File(BufId),
    Text,
}

struct Frame<'a> {
    lexer: Lexer<'a>,
    kind: FrameKind,
    id: u32,
}

struct Cond<'a> {
    /// The current branch is being taken.
    taking: bool,
    /// Some branch has already been taken.
    done: bool,
    /// The enclosing region is active.
    parent_active: bool,
    else_seen: bool,
    file_frame: u32,
    at: &'a str,
}

/// An open stringification, or a barrier collecting the output of a nested run.
struct Capture<'a> {
    text: String,
    /// The frame whose `` `" `` opened it, or `u32::MAX` for a barrier.
    frame: u32,
    at: &'a str,
}

/// A token read from a frame, with the frame it came from.
#[derive(Clone, Copy)]
struct Read<'a> {
    tok: Token<'a>,
    frame: u32,
    from_file: bool,
}

pub struct Preprocessor<'a, 'fs> {
    sm: &'a SourceMap,
    fs: &'fs dyn FileSystem,
    opts: Options,
    macros: HashMap<&'a str, Rc<Macro<'a>>>,
    frames: Vec<Frame<'a>>,
    next_frame_id: u32,
    /// Frames below this index belong to an outer run and are not read.
    floor: usize,
    pushback: Vec<Read<'a>>,
    last: Option<Read<'a>>,
    conds: Vec<Cond<'a>>,
    captures: Vec<Capture<'a>>,
    out: Vec<Token<'a>>,
    diags: Vec<Diag<'a>>,
    lint_off: HashSet<String>,
    lint_stack: Vec<HashSet<String>>,
    keywords_depth: usize,
    expansions_on_line: usize,
}

impl<'a, 'fs> Preprocessor<'a, 'fs> {
    pub fn new(sm: &'a SourceMap, fs: &'fs dyn FileSystem, opts: Options) -> Self {
        let mut pp = Preprocessor {
            sm,
            fs,
            opts,
            macros: HashMap::new(),
            frames: Vec::new(),
            next_frame_id: 0,
            floor: 0,
            pushback: Vec::new(),
            last: None,
            conds: Vec::new(),
            captures: Vec::new(),
            out: Vec::new(),
            diags: Vec::new(),
            lint_off: HashSet::new(),
            lint_stack: Vec::new(),
            keywords_depth: 0,
            expansions_on_line: 0,
        };
        let predefs: &[(&str, &str)] = &[
            ("VERILATOR", "1"),
            ("verilator", "1"),
            ("verilator3", "1"),
            ("SYSTEMVERILOG", "1"),
            ("coverage_block_off", "/*verilator coverage_block_off*/"),
            ("SV_COV_START", "0"),
            ("SV_COV_STOP", "1"),
            ("SV_COV_RESET", "2"),
            ("SV_COV_CHECK", "3"),
            ("SV_COV_MODULE", "10"),
            ("SV_COV_HIER", "11"),
            ("SV_COV_ASSERTION", "20"),
            ("SV_COV_FSM_STATE", "21"),
            ("SV_COV_STATEMENT", "22"),
            ("SV_COV_TOGGLE", "23"),
            ("SV_COV_OVERFLOW", "-2"),
            ("SV_COV_ERROR", "-1"),
            ("SV_COV_NOCOV", "0"),
            ("SV_COV_OK", "1"),
            ("SV_COV_PARTIAL", "2"),
        ];
        for (name, value) in predefs {
            pp.define_text(name, value, MacroKind::Predefined);
        }
        for (name, value) in pp.opts.defines.clone() {
            pp.define_text(&name, &value, MacroKind::CommandLine);
        }
        pp
    }

    fn define_text(&mut self, name: &str, value: &str, kind: MacroKind) {
        let (_, name) = self.sm.add(name.to_string(), Origin::CommandLine);
        let (_, value) = self.sm.add(value.to_string(), Origin::CommandLine);
        let m = Macro {
            formals: None,
            body: lex(value).collect(),
            at: name,
            kind,
        };
        self.macros.insert(name, Rc::new(m));
    }

    /// Preprocess a file, appending to the output. Defines persist between files.
    pub fn process_file(&mut self, path: &str) -> bool {
        let Some(text) = self.fs.read(path) else {
            let (_, at) = self.sm.add(path.to_string(), Origin::CommandLine);
            self.error(at, format!("Cannot find file: '{path}'"));
            return false;
        };
        let (buf, text) = self.sm.add(
            text,
            Origin::File {
                name: path.to_string(),
                parent: None,
            },
        );
        self.push_frame(text, FrameKind::File(buf));
        self.run(self.frames.len() - 1);
        true
    }

    /// The output tokens so far.
    pub fn output(&self) -> &[Token<'a>] {
        &self.out
    }

    pub fn take_output(&mut self) -> Vec<Token<'a>> {
        std::mem::take(&mut self.out)
    }

    pub fn diagnostics(&self) -> &[Diag<'a>] {
        &self.diags
    }

    pub fn is_defined(&self, name: &str) -> bool {
        self.macros.contains_key(name)
    }

    // ---------------------------------------------------------------- input

    fn push_frame(&mut self, text: &'a str, kind: FrameKind) {
        let id = self.next_frame_id;
        self.next_frame_id += 1;
        self.frames.push(Frame {
            lexer: lex(text),
            kind,
            id,
        });
    }

    /// The next raw token, crossing into enclosing frames when one is used up.
    fn next_read(&mut self) -> Option<Read<'a>> {
        if let Some(r) = self.pushback.pop() {
            self.last = Some(r);
            return Some(r);
        }
        loop {
            if self.frames.len() <= self.floor {
                return None;
            }
            let f = self.frames.last_mut().unwrap();
            if let Some(tok) = f.lexer.next() {
                let r = Read {
                    tok,
                    frame: f.id,
                    from_file: matches!(f.kind, FrameKind::File(_)),
                };
                self.last = Some(r);
                return Some(r);
            }
            self.pop_frame();
        }
    }

    fn next_tok(&mut self) -> Option<Token<'a>> {
        self.next_read().map(|r| r.tok)
    }

    fn unread(&mut self, r: Read<'a>) {
        self.pushback.push(r);
    }

    /// Put back the token just read.
    fn unread_last(&mut self) {
        if let Some(r) = self.last {
            self.pushback.push(r);
        }
    }

    fn pop_frame(&mut self) {
        let f = self.frames.pop().unwrap();
        // An unterminated stringification ends with its frame.
        while self.captures.last().is_some_and(|c| c.frame == f.id) {
            let c = self.captures.pop().unwrap();
            self.error(c.at, "Unterminated `\" stringification".into());
            self.emit_string(c);
        }
        if let FrameKind::File(buf) = f.kind {
            let text = self.sm.text(buf);
            let eof = &text[text.len()..];
            while self.conds.last().is_some_and(|c| c.file_frame == f.id) {
                let c = self.conds.pop().unwrap();
                let _ = c.at;
                self.error(eof, "`ifdef not terminated at EOF".into());
            }
        }
    }

    /// Skip whitespace and comments (and newlines if `newlines`), returning the next token.
    fn next_significant(&mut self, newlines: bool) -> Option<Token<'a>> {
        loop {
            let t = self.next_tok()?;
            match t {
                Token::Whitespace(_) | Token::LineComment(_) | Token::BlockComment(_) => {}
                Token::LineContinuation(_) => {}
                Token::Newline(_) if newlines => {}
                _ => return Some(t),
            }
        }
    }

    /// The innermost file frame.
    fn current_file(&self) -> Option<(BufId, u32)> {
        self.frames.iter().rev().find_map(|f| match f.kind {
            FrameKind::File(b) => Some((b, f.id)),
            FrameKind::Text => None,
        })
    }

    // ---------------------------------------------------------------- output

    fn emit(&mut self, t: Token<'a>) {
        if let Some(c) = self.captures.last_mut() {
            match t {
                Token::Newline(_) if c.frame != u32::MAX => c.text.push(' '),
                _ => c.text.push_str(t.text()),
            }
        } else {
            self.out.push(t);
        }
    }

    fn emit_string(&mut self, c: Capture<'a>) {
        let s = self.sm.derive(format!("\"{}\"", c.text), c.at);
        self.emit(Token::Str(s));
    }

    fn error(&mut self, at: &'a str, message: String) {
        self.diags.push(Diag {
            severity: Severity::Error,
            code: None,
            at,
            message,
            notes: Vec::new(),
        });
    }

    fn warn(&mut self, code: &'static str, at: &'a str, message: String) {
        if !self.lint_off.contains(code) {
            self.diags.push(Diag {
                severity: Severity::Warning,
                code: Some(code),
                at,
                message,
                notes: Vec::new(),
            });
        }
    }

    fn active(&self) -> bool {
        self.conds
            .last()
            .is_none_or(|c| c.taking && c.parent_active)
    }

    // ---------------------------------------------------------------- main loop

    /// Process tokens until the frames above `floor` are used up.
    fn run(&mut self, floor: usize) {
        let saved_floor = std::mem::replace(&mut self.floor, floor);
        let saved_pushback = std::mem::take(&mut self.pushback);
        while let Some(r) = self.next_read() {
            self.handle(r);
        }
        self.floor = saved_floor;
        self.pushback = saved_pushback;
    }

    fn handle(&mut self, r: Read<'a>) {
        let t = r.tok;
        if let Token::Directive(d) = t {
            return self.directive(t, d);
        }
        if !self.active() {
            if let Token::Newline(_) = t {
                self.emit(t);
            }
            return;
        }
        match t {
            Token::Newline(_) => {
                if r.from_file {
                    self.expansions_on_line = 0;
                }
                self.emit(t);
            }
            Token::LineComment(s) | Token::BlockComment(s) => self.comment(t, s),
            Token::MacroQuote(s) => {
                if self.captures.last().is_some_and(|c| c.frame == r.frame) {
                    let c = self.captures.pop().unwrap();
                    self.emit_string(c);
                } else {
                    self.captures.push(Capture {
                        text: String::new(),
                        frame: r.frame,
                        at: s,
                    });
                }
            }
            Token::MacroEscapedQuote(s) => {
                let q = self.sm.derive("\\\"".into(), s);
                self.emit(Token::Punct(q));
            }
            _ => self.emit(t),
        }
    }

    fn comment(&mut self, t: Token<'a>, s: &'a str) {
        match meta_comment(s) {
            Meta::Comment(m) => {
                self.lint_control(&m);
                let m = self.sm.derive(m, s);
                self.emit(Token::BlockComment(m));
            }
            Meta::BadUnderscore(word) => {
                self.diags.push(Diag {
                    severity: Severity::Error,
                    code: Some("BADVLTPRAGMA"),
                    at: s,
                    message: format!(
                        "Extra underscore in meta-comment, ignoring comment; use /*{word} {{...}}*/ not /*{word}_{{...}}*/"
                    ),
                    notes: Vec::new(),
                });
                self.emit(Token::Whitespace(" "));
            }
            Meta::None if self.opts.keep_comments => self.emit(t),
            Meta::None => self.emit(Token::Whitespace(" ")),
        }
    }

    /// Track `lint_off`/`lint_on`/`lint_save`/`lint_restore` for the
    /// preprocessor's own warnings.
    fn lint_control(&mut self, meta: &str) {
        let body = meta
            .trim_start_matches("/*verilator")
            .trim_end_matches("*/")
            .trim();
        let mut words = body.splitn(2, char::is_whitespace);
        let cmd = words.next().unwrap_or("");
        let codes = || {
            words
                .clone()
                .next()
                .unwrap_or("")
                .split(',')
                .map(|c| c.trim().to_string())
        };
        match cmd {
            "lint_off" => self.lint_off.extend(codes()),
            "lint_on" => codes().for_each(|c| {
                self.lint_off.remove(&c);
            }),
            "lint_save" => self.lint_stack.push(self.lint_off.clone()),
            "lint_restore" => {
                if let Some(s) = self.lint_stack.pop() {
                    self.lint_off = s;
                }
            }
            _ => {}
        }
    }

    // ---------------------------------------------------------------- directives

    fn directive(&mut self, t: Token<'a>, d: &'a str) {
        let name = &d[1..];
        match name {
            "ifdef" | "ifndef" => {
                let file_frame = self.current_file().map_or(u32::MAX, |f| f.1);
                if self.active() {
                    let v = self.cond_operand(t) != (name == "ifndef");
                    self.conds.push(Cond {
                        taking: v,
                        done: v,
                        parent_active: true,
                        else_seen: false,
                        file_frame,
                        at: t.text(),
                    });
                } else {
                    self.conds.push(Cond {
                        taking: false,
                        done: true,
                        parent_active: false,
                        else_seen: false,
                        file_frame,
                        at: t.text(),
                    });
                }
            }
            "elsif" => match self.conds.last() {
                None => self.error(d, "`elsif with no matching `if".into()),
                Some(c) => {
                    let evaluate = c.parent_active && !c.done;
                    let v = evaluate && self.cond_operand(t);
                    let c = self.conds.last_mut().unwrap();
                    c.taking = v;
                    c.done |= v;
                }
            },
            "else" => match self.conds.last_mut() {
                None => self.error(d, "`else with no matching `if".into()),
                Some(c) => {
                    c.taking = !c.done;
                    c.done = true;
                    let twice = std::mem::replace(&mut c.else_seen, true);
                    if twice {
                        self.error(d, "Multiple `else for the same `ifdef".into());
                    }
                }
            },
            "endif" => {
                if self.conds.pop().is_none() {
                    self.error(d, "`endif with no matching `if".into());
                }
            }
            _ if !self.active() => {}
            "define" => self.define(d),
            "undef" => match self.next_significant(false) {
                Some(Token::Ident(n) | Token::EscapedIdent(n)) => {
                    self.macros.remove(n);
                }
                _ => {
                    self.unread_last();
                    self.error(d, "Expecting define name after `undef".into());
                }
            },
            "undefineall" => self.macros.retain(|_, m| m.kind != MacroKind::User),
            "include" => self.include(d),
            "line" => self.line_directive(d),
            "__FILE__" => {
                let name = self.sm.locate(d).map_or_else(String::new, |l| l.name);
                let name = name.replace('\\', "\\\\").replace('"', "\\\"");
                let s = self.sm.derive(format!("\"{name}\""), d);
                self.emit(Token::Str(s));
            }
            "__LINE__" => {
                let line = self.sm.locate(d).map_or(0, |l| l.line);
                let s = self.sm.derive(line.to_string(), d);
                self.emit(Token::Number(s));
            }
            "error" => match self.next_significant(false) {
                Some(Token::Str(s) | Token::TripleStr(s)) => self.error(d, format!("`error {s}")),
                other => {
                    self.unread_last();
                    let found = other.map_or("end of file", |t| t.text());
                    self.error(d, format!("Expecting `error string. Found: {found}"));
                }
            },
            "pragma" => {
                let next = self.next_significant(false);
                self.unread_last();
                if matches!(next, None | Some(Token::Newline(_))) {
                    self.diags.push(Diag {
                        severity: Severity::Error,
                        code: Some("BADSTDPRAGMA"),
                        at: d,
                        message: "`pragma is missing a pragma_expression.".into(),
                        notes: Vec::new(),
                    });
                }
                // Restore the whitespace we skipped so the line reads the same.
                self.emit(t);
                self.emit(Token::Whitespace(" "));
            }
            "begin_keywords" => {
                self.keywords_depth += 1;
                self.emit(t);
            }
            "end_keywords" => {
                if self.keywords_depth == 0 {
                    self.error(
                        d,
                        "`end_keywords when not inside `begin_keywords block".into(),
                    );
                } else {
                    self.keywords_depth -= 1;
                }
                self.emit(t);
            }
            _ if PASS_THROUGH.contains(&name) => self.emit(t),
            _ => match self.macros.get(name).cloned() {
                Some(m) => self.expand(d, name, &m),
                // Undefined macros pass through; the parser reports them.
                None => self.emit(t),
            },
        }
    }

    /// Raw text up to (not including) the end of the line.
    fn rest_of_line(&mut self) -> String {
        let mut s = String::new();
        while let Some(r) = self.next_read() {
            match r.tok {
                Token::Newline(_) => {
                    self.unread(r);
                    break;
                }
                Token::LineContinuation(_) => s.push('\n'),
                t => s.push_str(t.text()),
            }
        }
        s
    }

    fn define(&mut self, d: &'a str) {
        let name_tok = match self.next_significant(false) {
            Some(t @ (Token::Ident(_) | Token::EscapedIdent(_))) => t,
            _ => {
                self.unread_last();
                self.error(d, "Expecting define name after `define".into());
                return;
            }
        };
        let mut name = name_tok.text();
        if is_builtin(name) {
            self.error(
                name,
                format!(
                    "Attempting to define built-in directive: '`{name}' (IEEE 1800-2023 22.5.1)"
                ),
            );
            self.rest_of_line();
            return;
        }

        // A name may be pasted from parts: `define A_```B
        loop {
            match self.next_read() {
                Some(Read {
                    tok: Token::Paste(_),
                    ..
                }) => {}
                Some(r) => {
                    self.unread(r);
                    break;
                }
                None => break,
            }
            let part = match self.next_tok() {
                Some(Token::Directive(pd)) if self.macros.contains_key(&pd[1..]) => {
                    let call = self.raw_call(pd);
                    self.expand_fully(&call, pd)
                }
                Some(t) => t.text().to_string(),
                None => String::new(),
            };
            name = self.sm.derive(format!("{name}{part}"), name);
        }

        let formals = match self.next_read() {
            Some(Read {
                tok: Token::Punct("("),
                ..
            }) => match self.formals(d) {
                Some(f) => Some(f),
                None => return,
            },
            Some(r) => {
                self.unread(r);
                None
            }
            None => None,
        };

        let mut raw = Vec::new();
        while let Some(r) = self.next_read() {
            if let Token::Newline(_) = r.tok {
                self.unread(r);
                break;
            }
            raw.push(r.tok);
        }
        let body = self.clean_body(raw);
        let m = Macro {
            formals,
            body,
            at: name,
            kind: MacroKind::User,
        };

        if let Some(old) = self.macros.get(name) {
            let (old_text, new_text) = (old.body_text(), m.body_text());
            if old.kind == MacroKind::User && old_text != new_text {
                let old_at = old.at;
                self.warn(
                    "REDEFMACRO",
                    name,
                    format!(
                        "Redefining existing define: '{name}', with different value: '{new_text}'"
                    ),
                );
                if let Some(d) = self.diags.last_mut().filter(|d| d.at == name) {
                    d.notes.push((
                        old_at,
                        format!("... Location of previous definition, with value: '{old_text}'"),
                    ));
                }
            }
        }
        self.macros.insert(name, Rc::new(m));
    }

    /// Formal arguments after the opening parenthesis of a function-like define.
    fn formals(&mut self, d: &'a str) -> Option<Vec<Formal<'a>>> {
        let mut formals = Vec::new();
        loop {
            match self.next_significant(true) {
                Some(Token::Punct(")")) if formals.is_empty() => return Some(formals),
                Some(Token::Ident(n) | Token::EscapedIdent(n)) => {
                    let mut f = Formal {
                        name: n,
                        default: None,
                    };
                    match self.next_significant(true) {
                        Some(Token::Punct("=")) => {
                            let (text, end) = self.balanced_text(&[",", ")"]);
                            // Verilator keeps trailing whitespace in defaults.
                            f.default = Some(text.trim_start().to_string());
                            formals.push(f);
                            match end {
                                Some(")") => return Some(formals),
                                Some(_) => {}
                                None => break,
                            }
                        }
                        Some(Token::Punct(",")) => formals.push(f),
                        Some(Token::Punct(")")) => {
                            formals.push(f);
                            return Some(formals);
                        }
                        _ => break,
                    }
                }
                _ => break,
            }
        }
        self.error(d, "Unterminated ( in define formal arguments.".into());
        None
    }

    /// Raw text up to one of `stops` at bracket depth 0. Returns the text and the stop found.
    fn balanced_text(&mut self, stops: &[&'static str]) -> (String, Option<&'static str>) {
        let mut depth = 0usize;
        let mut s = String::new();
        while let Some(t) = self.next_tok() {
            match t {
                Token::Punct(p @ ("(" | "[" | "{")) => {
                    depth += 1;
                    s.push_str(p);
                }
                Token::Punct(p @ (")" | "]" | "}")) if depth > 0 => {
                    depth -= 1;
                    s.push_str(p);
                }
                Token::Punct(p) if depth == 0 && stops.contains(&p) => {
                    let stop = stops.iter().find(|s| **s == p).copied();
                    return (s, stop);
                }
                Token::LineComment(c) | Token::BlockComment(c) => match meta_comment(c) {
                    Meta::Comment(m) => s.push_str(&m),
                    _ => s.push(' '),
                },
                Token::LineContinuation(_) => s.push('\n'),
                _ => s.push_str(t.text()),
            }
        }
        (s, None)
    }

    /// Normalise a define body: drop ordinary comments, turn continuations
    /// into newlines, split escaped identifiers so formals inside them can be
    /// substituted, and strip leading and trailing spaces. Trailing spaces are
    /// trimmed after line comments are dropped but before block comments
    /// become spaces, which matches Verilator's golden output.
    fn clean_body(&self, mut raw: Vec<Token<'a>>) -> Vec<Token<'a>> {
        raw.retain(
            |t| !matches!(t, Token::LineComment(s) if !matches!(meta_comment(s), Meta::Comment(_))),
        );
        while matches!(raw.last(), Some(Token::Whitespace(_))) {
            raw.pop();
        }
        let mut body = Vec::with_capacity(raw.len());
        for t in raw {
            match t {
                Token::LineContinuation(_) => body.push(Token::Newline("\n")),
                Token::LineComment(s) | Token::BlockComment(s) => match meta_comment(s) {
                    Meta::Comment(m) => body.push(Token::BlockComment(self.sm.derive(m, s))),
                    _ if matches!(t, Token::BlockComment(_)) => body.push(Token::Whitespace(" ")),
                    _ => {}
                },
                Token::EscapedIdent(s) => {
                    body.push(Token::Punct(&s[..1]));
                    body.extend(lex(&s[1..]));
                }
                _ => body.push(t),
            }
        }
        let lead = body
            .iter()
            .take_while(|t| matches!(t, Token::Whitespace(_)))
            .count();
        body.drain(..lead);
        body
    }

    // ---------------------------------------------------------------- expansion

    fn expand(&mut self, d: &'a str, name: &'a str, m: &Macro<'a>) {
        if self.expansions_on_line > MAX_EXPANSIONS_PER_LINE || self.frames.len() > MAX_FRAMES {
            self.error(d, format!("Recursive `define substitution: `{name}"));
            while self.frames.len() > self.floor + 1
                && self
                    .frames
                    .last()
                    .is_some_and(|f| f.kind == FrameKind::Text)
            {
                self.frames.pop();
            }
            self.expansions_on_line = 0;
            return;
        }
        self.expansions_on_line += 1;

        let args = match &m.formals {
            None => Vec::new(),
            Some(formals) => {
                let Some(actuals) = self.args(d, name) else {
                    return;
                };
                let empty_call = formals.is_empty() && actuals.len() == 1 && actuals[0].is_empty();
                if actuals.len() > formals.len() && !empty_call {
                    self.error(d, format!("Define passed too many arguments: {name}"));
                    return;
                }
                let mut bound = Vec::with_capacity(formals.len());
                for (i, f) in formals.iter().enumerate() {
                    match (actuals.get(i), &f.default) {
                        (Some(a), Some(def)) if a.is_empty() => bound.push(def.clone()),
                        (Some(a), _) => bound.push(a.clone()),
                        (None, Some(def)) => bound.push(def.clone()),
                        (None, None) => {
                            self.error(
                                d,
                                format!("Define missing argument '{}' for: {name}", f.name),
                            );
                            return;
                        }
                    }
                }
                bound
            }
        };
        let text = self.substitute(m, &args, d);
        let text = self.sm.derive(text, d);
        self.push_frame(text, FrameKind::Text);
    }

    /// Actual arguments of a function-like macro use, trimmed.
    fn args(&mut self, d: &'a str, name: &str) -> Option<Vec<String>> {
        match self.next_significant(true) {
            Some(Token::Punct("(")) => {}
            Some(_) => {
                self.unread_last();
                self.error(
                    d,
                    format!("Expecting ( to begin argument list for define reference `{name}"),
                );
                return None;
            }
            None => {
                self.error(d, "EOF in define argument list".into());
                return None;
            }
        }
        let mut args = Vec::new();
        loop {
            let (text, stop) = self.balanced_text(&[",", ")"]);
            args.push(text.trim().to_string());
            match stop {
                Some(",") => {}
                Some(_) => return Some(args),
                None => {
                    self.error(d, "EOF in define argument list".into());
                    return None;
                }
            }
        }
    }

    /// Replace formals in the body with argument text. Operands of ``` `` ```
    /// that are macro uses are expanded before they are joined.
    fn substitute(&mut self, m: &Macro<'a>, args: &[String], site: &'a str) -> String {
        let body = &m.body;
        let formal = |s: &str| {
            m.formals
                .as_ref()
                .and_then(|f| f.iter().position(|f| f.name == s))
        };
        let is_paste = |i: usize| matches!(body.get(i), Some(Token::Paste(_)));
        let mut out = String::new();
        let mut i = 0;
        while i < body.len() {
            let t = body[i];
            let pasted_before = i > 0 && is_paste(i - 1);
            match t {
                Token::Paste(_) => i += 1,
                Token::Ident(s) if formal(s).is_some() => {
                    let arg = &args[formal(s).unwrap()];
                    if (pasted_before || is_paste(i + 1)) && self.starts_with_macro(arg) {
                        let e = self.expand_fully(arg, site);
                        out.push_str(&e);
                    } else {
                        out.push_str(arg);
                    }
                    i += 1;
                }
                Token::Directive(d)
                    if !is_builtin(&d[1..]) && self.macros.contains_key(&d[1..]) =>
                {
                    let end = self.call_end(body, i);
                    if pasted_before || is_paste(end) {
                        let call: String = body[i..end]
                            .iter()
                            .map(|t| match t {
                                Token::Ident(s) if formal(s).is_some() => {
                                    args[formal(s).unwrap()].as_str()
                                }
                                t => t.text(),
                            })
                            .collect();
                        let e = self.expand_fully(&call, site);
                        out.push_str(&e);
                        i = end;
                    } else {
                        out.push_str(d);
                        i += 1;
                    }
                }
                _ => {
                    out.push_str(t.text());
                    i += 1;
                }
            }
        }
        out
    }

    /// End index of a macro call starting at `body[i]`, including its argument list.
    fn call_end(&self, body: &[Token<'a>], i: usize) -> usize {
        let Token::Directive(d) = body[i] else {
            return i + 1;
        };
        let function_like = self
            .macros
            .get(&d[1..])
            .is_some_and(|m| m.formals.is_some());
        if !function_like {
            return i + 1;
        }
        let mut j = i + 1;
        while matches!(body.get(j), Some(Token::Whitespace(_))) {
            j += 1;
        }
        if body.get(j) != Some(&Token::Punct("(")) {
            return i + 1;
        }
        let mut depth = 0;
        while j < body.len() {
            match body[j] {
                Token::Punct("(" | "[" | "{") => depth += 1,
                Token::Punct(")" | "]" | "}") => {
                    depth -= 1;
                    if depth == 0 {
                        return j + 1;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        j
    }

    /// Raw text of a macro call read from the input: the directive and, if the
    /// macro is function-like, its parenthesised arguments.
    fn raw_call(&mut self, d: &'a str) -> String {
        let mut s = d.to_string();
        let function_like = self
            .macros
            .get(&d[1..])
            .is_some_and(|m| m.formals.is_some());
        if function_like && let Some(Token::Punct("(")) = self.next_significant(true) {
            let (text, _) = self.balanced_text(&[")"]);
            s.push('(');
            s.push_str(&text);
            s.push(')');
        } else if function_like {
            self.unread_last();
        }
        s
    }

    fn starts_with_macro(&self, text: &str) -> bool {
        match lex(text).find(|t| !t.is_trivia()) {
            Some(Token::Directive(d)) => !is_builtin(&d[1..]) && self.macros.contains_key(&d[1..]),
            _ => false,
        }
    }

    /// Preprocess `text` on its own and return the output text.
    fn expand_fully(&mut self, text: &str, site: &'a str) -> String {
        let text = self.sm.derive(text.to_string(), site);
        self.captures.push(Capture {
            text: String::new(),
            frame: u32::MAX,
            at: site,
        });
        let floor = self.frames.len();
        self.push_frame(text, FrameKind::Text);
        self.run(floor);
        self.captures.pop().map(|c| c.text).unwrap_or_default()
    }

    // ---------------------------------------------------------------- conditions

    /// Evaluate the operand of `` `ifdef ``, `` `ifndef `` or `` `elsif ``.
    fn cond_operand(&mut self, t: Token<'a>) -> bool {
        match self.next_significant(false) {
            Some(Token::Ident(n) | Token::EscapedIdent(n)) => self.is_defined(n),
            Some(Token::Directive(d)) if self.macros.contains_key(&d[1..]) => {
                let call = self.raw_call(d);
                let name = self.expand_fully(&call, d);
                self.is_defined(name.trim())
            }
            Some(Token::Punct("(")) => {
                let v = self.cond_expr(t);
                match self.next_significant(true) {
                    Some(Token::Punct(")")) => v,
                    _ => {
                        self.unread_last();
                        self.error(t.text(), "Expecting ')' in `ifdef expression".into());
                        v
                    }
                }
            }
            _ => {
                self.unread_last();
                self.error(
                    t.text(),
                    format!("Expecting define name after {}", t.text()),
                );
                false
            }
        }
    }

    /// `` `ifdef `` expression. Binary operators have equal precedence and
    /// associate left to right, which is what Verilator's golden output shows.
    fn cond_expr(&mut self, t: Token<'a>) -> bool {
        let mut v = self.cond_unary(t);
        loop {
            let op = match self.next_significant(true) {
                Some(Token::Punct("&")) => self.expect_punct(&["&"]).then_some("&&"),
                Some(Token::Punct("|")) => self.expect_punct(&["|"]).then_some("||"),
                Some(Token::Punct("-")) => self.expect_punct(&[">"]).then_some("->"),
                Some(Token::Punct("<")) => self.expect_punct(&["-", ">"]).then_some("<->"),
                _ => {
                    self.unread_last();
                    return v;
                }
            };
            let Some(op) = op else {
                self.error(t.text(), "Malformed `ifdef expression operator".into());
                return v;
            };
            let r = self.cond_unary(t);
            v = match op {
                "&&" => v && r,
                "||" => v || r,
                "->" => !v || r,
                _ => v == r,
            };
        }
    }

    fn cond_unary(&mut self, t: Token<'a>) -> bool {
        match self.next_significant(true) {
            Some(Token::Punct("!")) => !self.cond_unary(t),
            Some(Token::Punct("(")) => {
                let v = self.cond_expr(t);
                if self.next_significant(true) != Some(Token::Punct(")")) {
                    self.unread_last();
                    self.error(t.text(), "Expecting ')' in `ifdef expression".into());
                }
                v
            }
            Some(Token::Ident(n) | Token::EscapedIdent(n)) => {
                if let Some(m) = self.macros.get(n) {
                    if m.formals.is_none() && m.body_text() == "0" {
                        self.warn(
                            "PREPROCZERO",
                            n,
                            format!("Preprocessor expression evaluates define with 0: '{n}' with value '0'"),
                        );
                    }
                    true
                } else {
                    false
                }
            }
            _ => {
                self.unread_last();
                self.error(
                    t.text(),
                    "Expecting define name in `ifdef expression".into(),
                );
                false
            }
        }
    }

    /// Read the given punctuation sequence immediately (no whitespace between).
    fn expect_punct(&mut self, seq: &[&str]) -> bool {
        for p in seq {
            match self.next_tok() {
                Some(Token::Punct(q)) if q == *p => {}
                _ => {
                    self.unread_last();
                    return false;
                }
            }
        }
        true
    }

    // ---------------------------------------------------------------- include / line

    fn include(&mut self, d: &'a str) {
        let name = match self.next_significant(false) {
            Some(Token::Str(s)) => s[1..s.len() - 1].to_string(),
            Some(Token::Punct("<")) => {
                let (text, _) = self.balanced_text(&[">"]);
                text.trim().to_string()
            }
            Some(Token::Directive(md)) if self.macros.contains_key(&md[1..]) => {
                let call = self.raw_call(md);
                let text = self.expand_fully(&call, md);
                let t = text.trim();
                let t = t
                    .strip_prefix('"')
                    .and_then(|t| t.strip_suffix('"'))
                    .unwrap_or(t);
                let t = t
                    .strip_prefix('<')
                    .and_then(|t| t.strip_suffix('>'))
                    .unwrap_or(t);
                t.trim().to_string()
            }
            _ => {
                self.unread_last();
                self.error(d, "Expecting include filename".into());
                return;
            }
        };
        let parent = self.current_file();
        let mut dirs = Vec::new();
        if self.opts.relative_includes
            && let Some((buf, _)) = parent
            && let Origin::File { name, .. } = self.sm.origin(buf)
        {
            let dir = std::path::Path::new(&name)
                .parent()
                .map(|p| p.to_string_lossy().to_string());
            dirs.push(dir.filter(|d| !d.is_empty()).unwrap_or_else(|| ".".into()));
        }
        dirs.extend(self.opts.include_dirs.iter().cloned());
        let mut tried = Vec::new();
        for dir in &dirs {
            for ext in &self.opts.lib_exts {
                let path = if dir == "." {
                    format!("{name}{ext}")
                } else {
                    format!("{dir}/{name}{ext}")
                };
                if let Some(text) = self.fs.read(&path) {
                    let recursive = self.frames.iter().any(|f| match f.kind {
                        FrameKind::File(b) => {
                            matches!(self.sm.origin(b), Origin::File { name, .. } if name == path)
                        }
                        FrameKind::Text => false,
                    });
                    if recursive {
                        self.error(d, format!("Recursive inclusion of file: {path}"));
                        return;
                    }
                    let (buf, text) = self.sm.add(
                        text,
                        Origin::File {
                            name: path,
                            parent: parent.map(|p| p.0),
                        },
                    );
                    self.push_frame(text, FrameKind::File(buf));
                    return;
                }
                tried.push(path);
            }
        }
        self.error(
            d,
            format!(
                "Cannot find include file: '{name}'\n        ... Looked in:\n{}",
                tried
                    .iter()
                    .map(|p| format!("             {p}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        );
    }

    fn line_directive(&mut self, d: &'a str) {
        let raw = self.rest_of_line();
        // `line `__LINE__ "name" renames the file but keeps the line numbering.
        let keep_numbering =
            lex(&raw).find(|t| !t.is_trivia()) == Some(Token::Directive("`__LINE__"));
        let text = self.expand_fully(&raw, d);
        let mut parts = lex(&text).filter(|t| !t.is_trivia());
        let parsed = (|| {
            let Token::Number(line) = parts.next()? else {
                return None;
            };
            let Token::Str(name) = parts.next()? else {
                return None;
            };
            let Token::Number(level) = parts.next()? else {
                return None;
            };
            let level: u8 = level.parse().ok()?;
            let ok = level <= 2 && parts.next().is_none();
            ok.then(|| (line.parse().ok(), unescape(&name[1..name.len() - 1])))
        })();
        let Some((Some(line), name)) = parsed else {
            self.error(
                d,
                "`line was not properly formed with '`line number \"filename\" level'".into(),
            );
            return;
        };
        if let Some((buf, off)) = self.sm.find(d)
            && matches!(self.sm.origin(buf), Origin::File { .. })
        {
            let phys = self.sm.text(buf)[..off].matches('\n').count() + 1;
            let line = if keep_numbering { line + 1 } else { line };
            self.sm.remap_lines(buf, phys + 1, line, name);
        }
    }
}

/// Decode `\\` and `\"` in a string literal's contents.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match (c, chars.clone().next()) {
            ('\\', Some(n @ ('\\' | '"'))) => {
                out.push(n);
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out
}

enum Meta {
    None,
    Comment(String),
    BadUnderscore(&'static str),
}

/// Recognise `/* verilator ... */` and `// verilator ...` and normalise them
/// to `/*verilator ...*/`: spaces and tabs collapse to one space, a
/// backslash-newline becomes a newline, and the ends are trimmed.
fn meta_comment(text: &str) -> Meta {
    let inner = match text.strip_prefix("//") {
        // A `//` metacomment ends at the next `//`, so a reason can follow it.
        Some(rest) => rest.split("//").next().unwrap_or(rest),
        None => text
            .strip_prefix("/*")
            .and_then(|t| t.strip_suffix("*/"))
            .unwrap_or(text),
    };
    let t = inner.trim_start();
    if let Some(rest) = t.strip_prefix("verilator") {
        if rest.starts_with('_') {
            return Meta::BadUnderscore("verilator");
        }
        if !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
            return Meta::None;
        }
        let rest = rest.replace("\\\r\n", "\n").replace("\\\n", "\n");
        let mut out = String::from("/*verilator ");
        let mut space = false;
        for c in rest.trim().chars() {
            if c == ' ' || c == '\t' {
                space = true;
            } else {
                if space {
                    out.push(' ');
                }
                space = false;
                out.push(c);
            }
        }
        out.push_str("*/");
        return Meta::Comment(out);
    }
    if t.starts_with("synopsys_") {
        return Meta::BadUnderscore("synopsys");
    }
    Meta::None
}

#[cfg(test)]
mod tests;
