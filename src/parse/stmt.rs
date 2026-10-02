//! Procedural statements and timing controls.

use super::expr::is_assign_op;
use super::types::{TYPE_KWS, VAR_QUALIFIERS};
use super::{PResult, Parser};
use crate::ast::*;
use crate::lex::Token;

impl<'a> Parser<'a> {
    /// Does a declaration start here (inside a block or subroutine)?
    pub(crate) fn at_block_decl(&self) -> bool {
        match self.peek() {
            Some(Token::Keyword(k)) => {
                (TYPE_KWS.contains(&k) && !self.is_op_at(1, "'"))
                    || VAR_QUALIFIERS.contains(&k)
                    || matches!(
                        k,
                        "struct"
                            | "union"
                            | "enum"
                            | "typedef"
                            | "parameter"
                            | "localparam"
                            | "type"
                    )
            }
            Some(Token::Ident(_) | Token::EscapedIdent(_)) => {
                // `my_t x;`, `pkg::t x;`, `my_t [3:0] x;`, `cls #(int) x;`
                let mut i = self.pos + 1;
                if matches!(self.toks.get(i), Some(Token::Op("::"))) {
                    i += 2;
                }
                if matches!(self.toks.get(i), Some(Token::Op("#"))) {
                    i = self.skip_balanced_from(i + 1);
                }
                while matches!(self.toks.get(i), Some(Token::Op("["))) {
                    i = self.skip_balanced_from(i);
                }
                matches!(
                    self.toks.get(i),
                    Some(Token::Ident(_) | Token::EscapedIdent(_))
                ) && !matches!(self.toks.get(i + 1), Some(Token::Op("(")))
            }
            _ => false,
        }
    }

    /// A declaration inside a block: variable, parameter or typedef.
    pub(crate) fn block_decl(&mut self) -> PResult<ModuleItem<'a>> {
        match self.peek() {
            Some(Token::Keyword("typedef")) => self.typedef(),
            Some(Token::Keyword("parameter" | "localparam")) => {
                let d = self.param_decl()?;
                self.expect_op(";")?;
                Ok(ModuleItem::Param(d))
            }
            _ => {
                let mut qualifiers = Vec::new();
                while let Some(q) = self.eat_kw_of(VAR_QUALIFIERS) {
                    qualifiers.push(q);
                }
                Ok(ModuleItem::Var(self.var_decl(qualifiers, None)?))
            }
        }
    }

    pub(crate) fn statement(&mut self) -> PResult<Stmt<'a>> {
        self.nested(Self::statement_inner)
    }

    fn statement_inner(&mut self) -> PResult<Stmt<'a>> {
        let Some(t) = self.peek() else {
            return Err(self.unexpected("a statement"));
        };
        match t {
            Token::Op(s @ ";") => {
                self.bump();
                Ok(Stmt::Null(s))
            }
            Token::Ident(label) | Token::EscapedIdent(label) if self.is_op_at(1, ":") => {
                self.bump();
                self.bump();
                let stmt = self.statement()?;
                Ok(Stmt::Labeled {
                    label,
                    stmt: Box::new(stmt),
                })
            }
            Token::Keyword("begin") | Token::Keyword("fork") => self.block(),
            Token::Keyword(u @ ("unique" | "unique0" | "priority")) => {
                self.bump();
                match self.peek() {
                    Some(Token::Keyword("if")) => self.if_stmt(Some(u)),
                    Some(Token::Keyword("case" | "casez" | "casex")) => self.case_stmt(Some(u)),
                    _ => Err(self.unexpected("'if' or 'case'")),
                }
            }
            Token::Keyword("if") => self.if_stmt(None),
            Token::Keyword("case" | "casez" | "casex") => self.case_stmt(None),
            Token::Keyword(kw @ "for") => self.for_stmt(kw),
            Token::Keyword(kw @ "foreach") => self.foreach_stmt(kw),
            Token::Keyword(kw @ "while") => {
                self.bump();
                let cond = self.paren_expr()?;
                let body = self.statement()?;
                Ok(Stmt::While {
                    kw,
                    cond,
                    body: Box::new(body),
                })
            }
            Token::Keyword(kw @ "do") => {
                self.bump();
                let body = self.statement()?;
                self.expect_kw("while")?;
                let cond = self.paren_expr()?;
                self.expect_op(";")?;
                Ok(Stmt::DoWhile {
                    kw,
                    body: Box::new(body),
                    cond,
                })
            }
            Token::Keyword(kw @ "repeat") => {
                self.bump();
                let count = self.paren_expr()?;
                let body = self.statement()?;
                Ok(Stmt::Repeat {
                    kw,
                    count,
                    body: Box::new(body),
                })
            }
            Token::Keyword(kw @ "forever") => {
                self.bump();
                let body = self.statement()?;
                Ok(Stmt::Forever {
                    kw,
                    body: Box::new(body),
                })
            }
            Token::Op("#" | "@" | "##") => {
                let timing = self.timing()?;
                let stmt = self.statement()?;
                Ok(Stmt::Timed {
                    timing,
                    stmt: Box::new(stmt),
                })
            }
            Token::Keyword(kw @ "wait") => {
                self.bump();
                if self.eat_kw("fork").is_some() {
                    self.expect_op(";")?;
                    return Ok(Stmt::Wait {
                        kw,
                        cond: None,
                        stmt: Box::new(Stmt::Null(kw)),
                    });
                }
                let cond = self.paren_expr()?;
                let stmt = self.statement()?;
                Ok(Stmt::Wait {
                    kw,
                    cond: Some(cond),
                    stmt: Box::new(stmt),
                })
            }
            Token::Op(op @ ("->" | "->>")) => {
                self.bump();
                if op == "->>" && (self.is_op("#") || self.is_op("@")) {
                    self.timing()?;
                }
                let target = self.postfix()?;
                self.expect_op(";")?;
                Ok(Stmt::Trigger { op, target })
            }
            Token::Keyword(kw @ "disable") => {
                self.bump();
                let target = if self.eat_kw("fork").is_some() {
                    None
                } else {
                    Some(self.postfix()?)
                };
                self.expect_op(";")?;
                Ok(Stmt::Disable { kw, target })
            }
            Token::Keyword(kw @ "return") => {
                self.bump();
                let value = if self.is_op(";") {
                    None
                } else {
                    Some(self.expr()?)
                };
                self.expect_op(";")?;
                Ok(Stmt::Return { kw, value })
            }
            Token::Keyword(kw @ "break") => {
                self.bump();
                self.expect_op(";")?;
                Ok(Stmt::Break(kw))
            }
            Token::Keyword(kw @ "continue") => {
                self.bump();
                self.expect_op(";")?;
                Ok(Stmt::Continue(kw))
            }
            Token::Keyword(kw @ ("assign" | "force")) => {
                self.bump();
                let lhs = self.postfix()?;
                self.expect_op("=")?;
                let rhs = self.expr()?;
                self.expect_op(";")?;
                Ok(Stmt::ProcAssign {
                    kw,
                    lhs,
                    rhs: Some(rhs),
                })
            }
            Token::Keyword(kw @ ("deassign" | "release")) => {
                self.bump();
                let lhs = self.postfix()?;
                self.expect_op(";")?;
                Ok(Stmt::ProcAssign { kw, lhs, rhs: None })
            }
            Token::Keyword(kw @ ("assert" | "assume" | "cover")) => self.immediate_assert(kw),
            Token::Keyword(k @ ("randcase" | "randsequence" | "wait_order" | "expect")) => {
                Err(self.not_yet(k, k))
            }
            Token::Keyword("void") if self.is_op_at(1, "'") => {
                let e = self.expr()?;
                self.expect_op(";")?;
                Ok(Stmt::Expr(e))
            }
            _ if self.at_block_decl() => match self.block_decl()? {
                ModuleItem::Var(d) => Ok(Stmt::Decl(d)),
                _ => Err(self.not_yet(t.text(), "declarations after statements")),
            },
            _ => self.assign_or_expr_stmt(),
        }
    }

    /// `x = y;`, `x <= #1 y;`, `x += 1;`, `x++;`, `f(a);`, `$display(...);`
    fn assign_or_expr_stmt(&mut self) -> PResult<Stmt<'a>> {
        let s = self.simple_stmt()?;
        self.expect_op(";")?;
        Ok(s)
    }

    /// An assignment, increment or call without its `;` (also used in `for` headers).
    pub(crate) fn simple_stmt(&mut self) -> PResult<Stmt<'a>> {
        if let Some(Token::Op(op @ ("++" | "--"))) = self.peek() {
            self.bump();
            let arg = self.postfix()?;
            return Ok(Stmt::Expr(Expr::IncDec {
                op,
                prefix: true,
                arg: Box::new(arg),
            }));
        }
        let lhs = self.postfix()?;
        match self.peek() {
            Some(Token::Op(op)) if is_assign_op(op) || op == "<=" => {
                self.bump();
                let timing = if self.is_op("#") || self.is_op("@") || self.is_kw("repeat") {
                    Some(self.intra_timing()?)
                } else {
                    None
                };
                let rhs = self.expr()?;
                Ok(Stmt::Assign {
                    lhs,
                    op,
                    timing,
                    rhs,
                })
            }
            _ => Ok(Stmt::Expr(lhs)),
        }
    }

    fn intra_timing(&mut self) -> PResult<Timing<'a>> {
        if self.eat_kw("repeat").is_some() {
            let n = self.paren_expr()?;
            let ev = self.timing()?;
            return Ok(Timing::Repeat(n, Box::new(ev)));
        }
        self.timing()
    }

    pub(crate) fn paren_expr(&mut self) -> PResult<Expr<'a>> {
        self.expect_op("(")?;
        let e = self.expr()?;
        self.expect_op(")")?;
        Ok(e)
    }

    /// `#delay`, `@event` or `##cycles`.
    pub(crate) fn timing(&mut self) -> PResult<Timing<'a>> {
        if self.is_op("#") {
            return Ok(Timing::Delay(self.delay_value()?));
        }
        if self.eat_op("##").is_some() {
            let e = if self.is_op("(") {
                self.paren_expr()?
            } else {
                self.postfix()?
            };
            return Ok(Timing::Cycle(e));
        }
        self.expect_op("@")?;
        if self.eat_op("*").is_some() {
            return Ok(Timing::Event(None));
        }
        if self.eat_op("(").is_none() {
            // @name or @pkg::name
            let e = self.postfix()?;
            return Ok(Timing::Event(Some(vec![EventExpr {
                edge: None,
                expr: e,
                iff: None,
            }])));
        }
        if self.is_op("*") && self.is_op_at(1, ")") {
            self.bump();
            self.bump();
            return Ok(Timing::Event(None));
        }
        let mut events = Vec::new();
        loop {
            let edge = self.eat_kw_of(&["posedge", "negedge", "edge"]);
            let expr = self.expr()?;
            let iff = if self.eat_kw("iff").is_some() {
                Some(self.expr()?)
            } else {
                None
            };
            events.push(EventExpr { edge, expr, iff });
            if self.eat_kw("or").is_none() && self.eat_op(",").is_none() {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(Timing::Event(Some(events)))
    }

    fn block(&mut self) -> PResult<Stmt<'a>> {
        let kw = self.bump().unwrap().text();
        let label = if self.eat_op(":").is_some() {
            Some(self.ident()?)
        } else {
            None
        };
        let mut decls = Vec::new();
        while self.at_block_decl() {
            decls.push(self.block_decl()?);
        }
        let ends: &[&str] = if kw == "begin" {
            &["end"]
        } else {
            &["join", "join_any", "join_none"]
        };
        let mut stmts = Vec::new();
        let end = loop {
            if let Some(e) = self.eat_kw_of(ends) {
                break e;
            }
            if self.peek().is_none() {
                return Err(self.unexpected(&format!("'{}'", ends[0])));
            }
            stmts.push(self.statement()?);
        };
        self.end_label()?;
        Ok(Stmt::Block {
            kw,
            label,
            decls,
            stmts,
            end,
        })
    }

    fn if_stmt(&mut self, unique: Option<&'a str>) -> PResult<Stmt<'a>> {
        let kw = self.expect_kw("if")?;
        let cond = self.paren_expr()?;
        let then = self.statement()?;
        let els = if self.eat_kw("else").is_some() {
            Some(Box::new(self.statement()?))
        } else {
            None
        };
        Ok(Stmt::If {
            unique,
            kw,
            cond,
            then: Box::new(then),
            els,
        })
    }

    fn case_stmt(&mut self, unique: Option<&'a str>) -> PResult<Stmt<'a>> {
        let kw = self.bump().unwrap().text();
        let expr = self.paren_expr()?;
        if self.is_kw("matches") {
            return Err(self.not_yet(self.here(), "case matches"));
        }
        let inside = self.eat_kw("inside").is_some();
        let mut items = Vec::new();
        while self.eat_kw("endcase").is_none() {
            if self.eat_kw("default").is_some() {
                self.eat_op(":");
                items.push(CaseItem {
                    labels: Vec::new(),
                    stmt: self.statement()?,
                });
                continue;
            }
            let mut labels = Vec::new();
            loop {
                labels.push(if inside {
                    self.open_range()?
                } else {
                    self.expr()?
                });
                if self.eat_op(",").is_none() {
                    break;
                }
            }
            self.expect_op(":")?;
            items.push(CaseItem {
                labels,
                stmt: self.statement()?,
            });
        }
        Ok(Stmt::Case {
            unique,
            kw,
            expr,
            inside,
            items,
        })
    }

    fn for_stmt(&mut self, kw: &'a str) -> PResult<Stmt<'a>> {
        self.bump();
        self.expect_op("(")?;
        let mut init = Vec::new();
        if !self.is_op(";") {
            if self.at_block_decl() {
                // `int i = 0, j = 0` is one declaration with two declarators;
                // `int i = 0, int j = 0` is two declarations.
                loop {
                    let mut qualifiers = Vec::new();
                    while let Some(q) = self.eat_kw_of(VAR_QUALIFIERS) {
                        qualifiers.push(q);
                    }
                    let ty = self.data_type()?;
                    let mut vars = vec![self.for_init_var()?];
                    let mut another_decl = false;
                    while self.eat_op(",").is_some() {
                        if self.at_block_decl() {
                            another_decl = true;
                            break;
                        }
                        vars.push(self.for_init_var()?);
                    }
                    init.push(Stmt::Decl(VarDecl {
                        qualifiers,
                        net: None,
                        ty,
                        vars,
                    }));
                    if !another_decl {
                        break;
                    }
                }
            } else {
                loop {
                    init.push(self.simple_stmt()?);
                    if self.eat_op(",").is_none() {
                        break;
                    }
                }
            }
        }
        self.expect_op(";")?;
        let cond = if self.is_op(";") {
            None
        } else {
            Some(self.expr()?)
        };
        self.expect_op(";")?;
        let mut step = Vec::new();
        if !self.is_op(")") {
            loop {
                step.push(self.simple_stmt()?);
                if self.eat_op(",").is_none() {
                    break;
                }
            }
        }
        self.expect_op(")")?;
        let body = self.statement()?;
        Ok(Stmt::For {
            kw,
            init,
            cond,
            step,
            body: Box::new(body),
        })
    }

    /// `name [= init]` in a `for` header.
    fn for_init_var(&mut self) -> PResult<Declarator<'a>> {
        let name = self.ident()?;
        let dims = self.dims()?;
        let init = if self.eat_op("=").is_some() {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Declarator { name, dims, init })
    }

    fn foreach_stmt(&mut self, kw: &'a str) -> PResult<Stmt<'a>> {
        self.bump();
        let (array, vars) = self.foreach_header()?;
        let body = self.statement()?;
        Ok(Stmt::Foreach {
            kw,
            array,
            vars,
            body: Box::new(body),
        })
    }

    /// `( array[i, j] )` after `foreach`.
    pub(crate) fn foreach_header(&mut self) -> PResult<(Expr<'a>, Vec<Option<&'a str>>)> {
        self.expect_op("(")?;
        // The loop variables are in the last brackets before `)`; everything
        // before them is the array expression: foreach (a[i].b[j]) uses j.
        let close = self.skip_balanced_from(self.pos - 1) - 1;
        let mut lb = close;
        let mut depth = 0usize;
        while lb > self.pos {
            lb -= 1;
            match self.toks[lb] {
                Token::Op("]") => depth += 1,
                Token::Op("[") => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
        if !matches!(self.toks.get(lb), Some(Token::Op("["))) || lb == self.pos {
            return Err(self.unexpected("'[' loop variables"));
        }
        let mut sub = Parser {
            toks: self.toks[self.pos..lb].to_vec(),
            pos: 0,
            diags: Vec::new(),
            types: self.types.clone(),
            eof: self.toks[lb].text(),
            depth: self.depth,
        };
        let array = sub.postfix();
        let leftover = sub.pos < sub.toks.len();
        self.diags.append(&mut sub.diags);
        let array = array?;
        if leftover {
            self.pos += sub.pos;
            return Err(self.unexpected("'['"));
        }
        self.pos = lb;
        let mut vars = Vec::new();
        if self.eat_op("[").is_some() {
            loop {
                if self.is_op(",") || self.is_op("]") {
                    vars.push(None);
                } else {
                    vars.push(Some(self.ident()?));
                }
                if self.eat_op(",").is_none() {
                    break;
                }
            }
            self.expect_op("]")?;
        }
        self.expect_op(")")?;
        Ok((array, vars))
    }

    fn immediate_assert(&mut self, kw: &'a str) -> PResult<Stmt<'a>> {
        self.bump();
        if self.is_kw("property") || self.is_kw("sequence") {
            return Err(self.not_yet(kw, "concurrent assertions"));
        }
        if self.is_op("#") {
            self.bump();
            self.expect_op_number("0")?;
        } else {
            self.eat_kw("final");
        }
        let cond = self.paren_expr()?;
        let pass = if self.is_kw("else") {
            None
        } else {
            let s = self.statement()?;
            if matches!(s, Stmt::Null(_)) {
                None
            } else {
                Some(Box::new(s))
            }
        };
        let fail = if self.eat_kw("else").is_some() {
            Some(Box::new(self.statement()?))
        } else {
            None
        };
        Ok(Stmt::Assert {
            kw,
            cond,
            pass,
            fail,
        })
    }

    fn expect_op_number(&mut self, n: &str) -> PResult<()> {
        match self.peek() {
            Some(Token::Number(x)) if x == n => {
                self.bump();
                Ok(())
            }
            _ => Err(self.unexpected(n)),
        }
    }
}
