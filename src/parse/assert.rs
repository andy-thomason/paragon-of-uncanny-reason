//! Concurrent assertions, properties and sequences (IEEE 1800-2023 §16): the
//! commonly used part of the grammar.

use super::{PResult, Parser};
use crate::ast::*;
use crate::lex::Token;

impl<'a> Parser<'a> {
    /// `assert|assume|cover|restrict property (spec) [pass] [else fail];`
    /// after an optional label; the keyword is next.
    pub(crate) fn concurrent_assertion(&mut self, label: Option<&'a str>) -> PResult<Assertion<'a>> {
        let kw = self.bump().unwrap().text();
        if self.eat_kw("property").is_none() {
            // `cover sequence (...)`
            self.expect_kw("sequence")?;
        }
        self.expect_op("(")?;
        let spec = self.prop_spec()?;
        self.expect_op(")")?;
        let (mut pass, mut fail) = (None, None);
        if self.eat_op(";").is_none() {
            if !self.is_kw("else") {
                pass = Some(Box::new(self.statement()?));
            }
            if self.eat_kw("else").is_some() {
                fail = Some(Box::new(self.statement()?));
            }
        }
        Ok(Assertion {
            kw,
            label,
            spec,
            pass,
            fail,
        })
    }

    /// `[@(clock)] [disable iff (cond)] property_expr`
    pub(crate) fn prop_spec(&mut self) -> PResult<PropSpec<'a>> {
        let clock = if self.is_op("@") {
            Some(self.timing()?)
        } else {
            None
        };
        let disable = if self.is_kw("disable") && self.is_kw_at(1, "iff") {
            self.bump();
            self.bump();
            Some(self.paren_expr()?)
        } else {
            None
        };
        let prop = self.prop_expr()?;
        Ok(PropSpec {
            clock,
            disable,
            prop,
        })
    }

    fn prop_expr(&mut self) -> PResult<Prop<'a>> {
        let mut p = self.prop_unary()?;
        loop {
            if let Some(op) = self.eat_kw_of(&["and", "or"]) {
                let rhs = self.prop_unary()?;
                p = if op == "and" {
                    Prop::And(Box::new(p), Box::new(rhs))
                } else {
                    Prop::Or(Box::new(p), Box::new(rhs))
                };
                continue;
            }
            if let Some(k) =
                self.eat_kw_of(&["until", "s_until", "until_with", "s_until_with", "implies", "iff"])
            {
                self.prop_unary()?;
                p = Prop::Unsupported(k);
                continue;
            }
            break;
        }
        Ok(p)
    }

    fn prop_unary(&mut self) -> PResult<Prop<'a>> {
        if self.eat_kw("not").is_some() {
            return Ok(Prop::Not(Box::new(self.prop_unary()?)));
        }
        if self.eat_kw("if").is_some() {
            let cond = self.paren_expr()?;
            let then = Box::new(self.prop_expr()?);
            let els = if self.eat_kw("else").is_some() {
                Some(Box::new(self.prop_expr()?))
            } else {
                None
            };
            return Ok(Prop::If { cond, then, els });
        }
        if let Some(k) = self.eat_kw_of(&[
            "s_eventually",
            "eventually",
            "nexttime",
            "s_nexttime",
            "always",
            "s_always",
            "accept_on",
            "reject_on",
            "sync_accept_on",
            "sync_reject_on",
            "strong",
            "weak",
            "case",
        ]) {
            // Skip the operand.
            if self.is_op("[") {
                self.bump();
                while self.bump().is_some_and(|t| t != Token::Op("]")) {}
            }
            if self.is_op("(") {
                let end = self.skip_balanced_from(self.pos);
                self.pos = end;
            } else {
                self.prop_unary()?;
            }
            return Ok(Prop::Unsupported(k));
        }
        // A parenthesised property, if the parentheses hold one.
        if self.is_op("(") && self.parens_hold_property() {
            self.bump();
            let p = self.prop_expr()?;
            self.expect_op(")")?;
            return Ok(p);
        }
        let ante = self.seq_expr()?;
        if let Some(op) = self.eat_op_of(&["|->", "|=>", "#-#", "#=#"]) {
            let cons = self.prop_expr()?;
            return Ok(Prop::Implies {
                ante,
                overlap: matches!(op, "|->" | "#-#"),
                cons: Box::new(cons),
            });
        }
        Ok(Prop::Seq(ante))
    }

    /// Do the parentheses starting here contain a property operator at their top level?
    fn parens_hold_property(&self) -> bool {
        let end = self.skip_balanced_from(self.pos);
        let mut depth = 0i32;
        for t in &self.toks[self.pos + 1..end.saturating_sub(1)] {
            match t {
                Token::Op("(" | "[" | "{") => depth += 1,
                Token::Op(")" | "]" | "}") => depth -= 1,
                Token::Op("|->" | "|=>") if depth == 0 => return true,
                Token::Keyword("not" | "until" | "s_eventually" | "nexttime" | "always")
                    if depth == 0 =>
                {
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    /// Do the parentheses starting here contain a sequence operator at their top level?
    fn parens_hold_sequence(&self) -> bool {
        let end = self.skip_balanced_from(self.pos);
        let mut depth = 0i32;
        let toks = &self.toks[self.pos + 1..end.saturating_sub(1)];
        for (i, t) in toks.iter().enumerate() {
            match t {
                Token::Op("(" | "{") => depth += 1,
                Token::Op(")" | "}") => depth -= 1,
                Token::Op("[") => {
                    if depth == 0 && matches!(toks.get(i + 1), Some(Token::Op("*" | "->" | "="))) {
                        return true;
                    }
                    depth += 1;
                }
                Token::Op("]") => depth -= 1,
                Token::Op("##" | "|->" | "|=>") if depth == 0 => return true,
                Token::Keyword("and" | "or" | "intersect" | "throughout" | "within" | "not")
                    if depth == 0 =>
                {
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    pub(crate) fn seq_expr(&mut self) -> PResult<Seq<'a>> {
        let mut s = self.seq_delays()?;
        loop {
            let Some(op) = self.eat_kw_of(&["and", "or", "intersect", "throughout", "within"])
            else {
                break;
            };
            // `a and b` between properties is handled by the caller.
            if matches!(op, "and" | "or") && self.peek().is_none() {
                break;
            }
            let rhs = self.seq_delays()?;
            s = Seq::Binary {
                op,
                lhs: Box::new(s),
                rhs: Box::new(rhs),
            };
        }
        Ok(s)
    }

    /// Elements joined by `##` delays.
    fn seq_delays(&mut self) -> PResult<Seq<'a>> {
        let mut s = if self.is_op("##") {
            None
        } else {
            Some(self.seq_repeat()?)
        };
        while self.eat_op("##").is_some() {
            let (min, max) = self.cycle_range()?;
            let rhs = self.seq_repeat()?;
            s = Some(Seq::Delay {
                lhs: s.map(Box::new),
                min,
                max,
                rhs: Box::new(rhs),
            });
        }
        Ok(s.unwrap())
    }

    /// `n`, `(expr)`, `[m:n]`, `[m:$]`, `[*]`, `[+]` after `##`.
    fn cycle_range(&mut self) -> PResult<(Expr<'a>, Option<Option<Expr<'a>>>)> {
        if self.eat_op("[").is_some() {
            if self.eat_op("*").is_some() {
                self.expect_op("]")?;
                return Ok((Expr::Number("0"), Some(None)));
            }
            if self.eat_op("+").is_some() {
                self.expect_op("]")?;
                return Ok((Expr::Number("1"), Some(None)));
            }
            let min = self.expr()?;
            self.expect_op(":")?;
            let max = if self.eat_op("$").is_some() {
                None
            } else {
                Some(self.expr()?)
            };
            self.expect_op("]")?;
            return Ok((min, Some(max)));
        }
        if self.is_op("(") {
            return Ok((self.paren_expr()?, None));
        }
        Ok((self.primary_for_delay()?, None))
    }

    fn primary_for_delay(&mut self) -> PResult<Expr<'a>> {
        match self.bump() {
            Some(Token::Number(n)) => Ok(Expr::Number(n)),
            Some(Token::Ident(n) | Token::EscapedIdent(n)) => Ok(Expr::Ident(n)),
            _ => {
                self.pos -= 1;
                Err(self.unexpected("a cycle delay"))
            }
        }
    }

    /// An element with optional repetition.
    fn seq_repeat(&mut self) -> PResult<Seq<'a>> {
        let mut s = if self.is_kw("first_match") {
            self.bump();
            self.expect_op("(")?;
            let s = self.seq_expr()?;
            self.expect_op(")")?;
            s
        } else if self.is_op("(") && self.parens_hold_sequence() {
            self.bump();
            let s = self.seq_expr()?;
            self.expect_op(")")?;
            s
        } else {
            Seq::Expr(self.expr()?)
        };
        while self.is_op("[") && matches!(self.peek_at(1), Some(Token::Op("*" | "->" | "="))) {
            self.bump();
            let kind = self.bump().unwrap().text();
            let (min, max) = if self.eat_op("]").is_some() {
                // `[*]`: zero or more.
                (Expr::Number("0"), Some(None))
            } else {
                let min = self.expr()?;
                let max = if self.eat_op(":").is_some() {
                    if self.eat_op("$").is_some() {
                        Some(None)
                    } else {
                        Some(Some(self.expr()?))
                    }
                } else {
                    None
                };
                self.expect_op("]")?;
                (min, max)
            };
            s = Seq::Repeat {
                seq: Box::new(s),
                kind,
                min,
                max,
            };
        }
        Ok(s)
    }

    /// `property name [(ports)]; [decls] spec; endproperty` or the same for
    /// `sequence`.
    pub(crate) fn property_decl(&mut self) -> PResult<ModuleItem<'a>> {
        let kw = self.bump().unwrap().text();
        let name = self.ident()?;
        let mut ports = Vec::new();
        if self.eat_op("(").is_some() && self.eat_op(")").is_none() {
            loop {
                // `[local] [input] [type] name [= default]`
                self.eat_kw("local");
                self.eat_kw_of(&["input", "output", "inout"]);
                let mut last = self.ident_or_type_name()?;
                while self.is_ident_at(0) {
                    last = self.ident()?;
                }
                if self.eat_op("=").is_some() {
                    self.expr()?;
                }
                ports.push(last);
                if self.eat_op(",").is_none() {
                    break;
                }
            }
            self.expect_op(")")?;
        }
        self.expect_op(";")?;
        let end_kw = if kw == "property" {
            "endproperty"
        } else {
            "endsequence"
        };
        let item = if kw == "property" {
            let spec = self.prop_spec()?;
            ModuleItem::PropertyDecl { name, ports, spec }
        } else {
            let seq = self.seq_expr()?;
            ModuleItem::SequenceDecl { name, ports, seq }
        };
        self.eat_op(";");
        self.expect_kw(end_kw)?;
        self.end_label()?;
        Ok(item)
    }

    /// A port name in a property or sequence header, which may follow a type
    /// keyword (`logic a`, `int n`, `untyped x`).
    fn ident_or_type_name(&mut self) -> PResult<&'a str> {
        match self.bump() {
            Some(Token::Ident(n) | Token::EscapedIdent(n) | Token::Keyword(n)) => Ok(n),
            _ => {
                self.pos -= 1;
                Err(self.unexpected("a port"))
            }
        }
    }

    /// `default clocking [name] @(...); endclocking`, or a named clocking
    /// block (not supported).
    pub(crate) fn default_clocking(&mut self) -> PResult<ModuleItem<'a>> {
        self.expect_kw("default")?;
        if self.eat_kw("disable").is_some() {
            self.expect_kw("iff")?;
            let e = self.expr()?;
            self.expect_op(";")?;
            return Ok(ModuleItem::DefaultDisable(e));
        }
        let kw = self.expect_kw("clocking")?;
        if self.is_ident_at(0) && self.is_op_at(1, ";") {
            // `default clocking name;` refers to a clocking block.
            return Err(self.not_yet(kw, "clocking blocks"));
        }
        if self.is_ident_at(0) {
            self.bump();
        }
        let t = self.timing()?;
        self.expect_op(";")?;
        if self.eat_kw("endclocking").is_none() {
            return Err(self.not_yet(kw, "clocking blocks"));
        }
        self.end_label()?;
        Ok(ModuleItem::DefaultClocking(t))
    }
}
