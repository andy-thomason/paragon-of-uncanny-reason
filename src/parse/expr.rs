//! Expressions, by precedence climbing (IEEE 1800-2023 Table 11-2).

use super::types::TYPE_KWS;
use super::{PResult, Parser};
use crate::ast::*;
use crate::lex::Token;

/// Binding power of a binary operator; higher binds tighter.
fn binary_prec(op: &str) -> Option<u8> {
    Some(match op {
        "||" => 1,
        "&&" => 2,
        "|" => 3,
        "^" | "~^" | "^~" => 4,
        "&" => 5,
        "==" | "!=" | "===" | "!==" | "==?" | "!=?" => 6,
        "<" | "<=" | ">" | ">=" => 7,
        "<<" | ">>" | "<<<" | ">>>" => 8,
        "+" | "-" => 9,
        "*" | "/" | "%" => 10,
        "**" => 11,
        _ => return None,
    })
}

const INSIDE_PREC: u8 = 7;

const UNARY_OPS: &[&str] = &["+", "-", "!", "~", "&", "~&", "|", "~|", "^", "~^", "^~"];

const ASSIGN_OPS: &[&str] = &[
    "=", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "<<=", ">>=", "<<<=", ">>>=",
];

pub(crate) fn is_assign_op(op: &str) -> bool {
    ASSIGN_OPS.contains(&op)
}

impl<'a> Parser<'a> {
    /// A full expression, including `?:` and the implication operators.
    pub(crate) fn expr(&mut self) -> PResult<Expr<'a>> {
        let e = self.cond_expr()?;
        if let Some(Token::Op(op @ ("->" | "<->"))) = self.peek() {
            self.bump();
            let rhs = self.expr()?;
            return Ok(Expr::Binary {
                op,
                lhs: Box::new(e),
                rhs: Box::new(rhs),
            });
        }
        Ok(e)
    }

    fn cond_expr(&mut self) -> PResult<Expr<'a>> {
        let c = self.binary(1)?;
        if let Some(op) = self.eat_op("?") {
            let then = self.expr()?;
            self.expect_op(":")?;
            let els = self.cond_expr()?;
            return Ok(Expr::Cond {
                op,
                cond: Box::new(c),
                then: Box::new(then),
                els: Box::new(els),
            });
        }
        Ok(c)
    }

    fn binary(&mut self, min: u8) -> PResult<Expr<'a>> {
        let mut lhs = self.unary()?;
        loop {
            if self.is_kw("inside") && INSIDE_PREC >= min {
                self.bump();
                let set = self.open_range_list()?;
                lhs = Expr::Inside {
                    expr: Box::new(lhs),
                    set,
                };
                continue;
            }
            if self.is_kw("dist") {
                return Err(self.not_yet(self.here(), "dist"));
            }
            let Some(Token::Op(op)) = self.peek() else {
                break;
            };
            let Some(prec) = binary_prec(op) else { break };
            if prec < min {
                break;
            }
            self.bump();
            let rhs = self.binary(prec + 1)?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    /// `{ a, [lo:hi], ... }` after `inside`.
    pub(crate) fn open_range_list(&mut self) -> PResult<Vec<Expr<'a>>> {
        self.expect_op("{")?;
        let mut set = Vec::new();
        loop {
            set.push(self.open_range()?);
            if self.eat_op(",").is_none() {
                break;
            }
        }
        self.expect_op("}")?;
        Ok(set)
    }

    /// An expression or `[lo:hi]`.
    pub(crate) fn open_range(&mut self) -> PResult<Expr<'a>> {
        if self.eat_op("[").is_some() {
            let lo = self.expr()?;
            if let Some(Token::Op(op @ ("+/-" | "+%-"))) = self.peek() {
                return Err(self.not_yet(op, "+/- ranges"));
            }
            self.expect_op(":")?;
            let hi = self.expr()?;
            self.expect_op("]")?;
            return Ok(Expr::Range {
                lo: Box::new(lo),
                hi: Box::new(hi),
            });
        }
        self.expr()
    }

    fn unary(&mut self) -> PResult<Expr<'a>> {
        self.nested(Self::unary_inner)
    }

    fn unary_inner(&mut self) -> PResult<Expr<'a>> {
        match self.peek() {
            Some(Token::Op(op)) if UNARY_OPS.contains(&op) => {
                self.bump();
                let arg = self.unary()?;
                Ok(Expr::Unary {
                    op,
                    arg: Box::new(arg),
                })
            }
            Some(Token::Op(op @ ("++" | "--"))) => {
                self.bump();
                let arg = self.unary()?;
                Ok(Expr::IncDec {
                    op,
                    prefix: true,
                    arg: Box::new(arg),
                })
            }
            _ => self.postfix(),
        }
    }

    /// A primary followed by selects, member access, calls and casts.
    pub(crate) fn postfix(&mut self) -> PResult<Expr<'a>> {
        let mut e = self.primary()?;
        loop {
            match self.peek() {
                Some(Token::Op("[")) => {
                    self.bump();
                    let left = self.expr()?;
                    e = match self.peek() {
                        Some(Token::Op(op @ (":" | "+:" | "-:"))) => {
                            self.bump();
                            let right = self.expr()?;
                            Expr::Slice {
                                base: Box::new(e),
                                op,
                                left: Box::new(left),
                                right: Box::new(right),
                            }
                        }
                        _ => Expr::Index {
                            base: Box::new(e),
                            index: Box::new(left),
                        },
                    };
                    self.expect_op("]")?;
                }
                Some(Token::Op(".")) => {
                    self.bump();
                    let name = match self.bump() {
                        Some(Token::Ident(n) | Token::EscapedIdent(n) | Token::Keyword(n)) => n,
                        _ => {
                            self.pos -= 1;
                            return Err(self.unexpected("a member name"));
                        }
                    };
                    e = Expr::Member {
                        base: Box::new(e),
                        name,
                    };
                }
                Some(Token::Op("("))
                    if matches!(
                        e,
                        Expr::Ident(_) | Expr::Member { .. } | Expr::Scoped { .. }
                    ) =>
                {
                    let args = self.call_args()?;
                    e = Expr::Call {
                        func: Box::new(e),
                        args,
                    };
                }
                Some(Token::Op(op @ ("++" | "--"))) => {
                    self.bump();
                    e = Expr::IncDec {
                        op,
                        prefix: false,
                        arg: Box::new(e),
                    };
                }
                Some(Token::Keyword(w @ "with")) if self.is_op_at(1, "{") => {
                    return Err(self.not_yet(w, "inline constraints"));
                }
                Some(Token::Keyword("with")) if self.is_op_at(1, "(") => {
                    self.bump();
                    self.bump();
                    let w = self.expr()?;
                    self.expect_op(")")?;
                    e = Expr::With {
                        base: Box::new(e),
                        expr: Box::new(w),
                    };
                }
                // A cast: my_t'(x), 8'(x), (W)'(x)
                Some(Token::Op("'")) if self.is_op_at(1, "(") => {
                    self.bump();
                    self.bump();
                    let x = self.expr()?;
                    self.expect_op(")")?;
                    e = Expr::Cast {
                        ty: Box::new(e),
                        expr: Box::new(x),
                    };
                }
                // A typed assignment pattern: my_t'{...}
                Some(Token::Op("'{")) if matches!(e, Expr::Ident(_) | Expr::Scoped { .. }) => {
                    let Expr::Pattern { items, .. } = self.pattern()? else {
                        unreachable!()
                    };
                    e = Expr::Pattern {
                        ty: Some(Box::new(e)),
                        items,
                    };
                }
                _ => return Ok(e),
            }
        }
    }

    /// `( args )` for a call. Arguments may be empty or named.
    pub(crate) fn call_args(&mut self) -> PResult<Vec<Arg<'a>>> {
        self.expect_op("(")?;
        let mut args = Vec::new();
        if self.eat_op(")").is_some() {
            return Ok(args);
        }
        loop {
            if self.is_op(".") && self.is_ident_at(1) {
                self.bump();
                let name = self.ident()?;
                self.expect_op("(")?;
                let v = if self.is_op(")") {
                    None
                } else {
                    Some(self.expr_or_type()?)
                };
                self.expect_op(")")?;
                args.push(Arg::Named(name, v));
            } else if self.is_op(",") || self.is_op(")") {
                args.push(Arg::Ordered(None));
            } else {
                args.push(Arg::Ordered(Some(self.expr_or_type()?)));
            }
            if self.eat_op(",").is_none() {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(args)
    }

    /// An expression, or a data type where one is allowed (`$bits(int)`, `#(.T(logic))`).
    pub(crate) fn expr_or_type(&mut self) -> PResult<Expr<'a>> {
        let type_kw = matches!(self.peek(), Some(Token::Keyword(k)) if TYPE_KWS.contains(&k) || matches!(k, "struct" | "union" | "enum"));
        if type_kw && !self.is_op_at(1, "'") {
            return Ok(Expr::Type(Box::new(self.data_type()?)));
        }
        // A parameterised class type: `C#(int)`, `pkg::C#(8)`, but not `C#(8)::x`.
        let class_at = if self.is_ident_at(0) && self.is_op_at(1, "#") {
            Some(1)
        } else if self.is_ident_at(0) && self.is_op_at(1, "::") && self.is_ident_at(2) && self.is_op_at(3, "#") {
            Some(3)
        } else {
            None
        };
        if let Some(k) = class_at
            && self.is_op_at(k + 1, "(")
        {
            let end = self.skip_balanced_from(self.pos + k + 1);
            if !matches!(self.toks.get(end), Some(Token::Op("::"))) {
                return Ok(Expr::Type(Box::new(self.data_type()?)));
            }
        }
        self.expr()
    }

    /// `(a : b : c)` or just an expression.
    pub(crate) fn mintypmax(&mut self) -> PResult<Expr<'a>> {
        let a = self.expr()?;
        if self.eat_op(":").is_some() {
            let b = self.expr()?;
            self.expect_op(":")?;
            let c = self.expr()?;
            return Ok(Expr::MinTypMax(Box::new([a, b, c])));
        }
        Ok(a)
    }

    fn primary(&mut self) -> PResult<Expr<'a>> {
        let Some(t) = self.peek() else {
            return Err(self.unexpected("an expression"));
        };
        match t {
            Token::Number(n) => {
                self.bump();
                Ok(Expr::Number(n))
            }
            Token::Str(s) => {
                self.bump();
                Ok(Expr::Str(s))
            }
            Token::Ident(n) | Token::EscapedIdent(n) => {
                self.bump();
                if self.is_op("::") {
                    let mut e = Expr::Ident(n);
                    while self.eat_op("::").is_some() {
                        let name = match self.bump() {
                            Some(Token::Ident(s) | Token::EscapedIdent(s) | Token::Keyword(s)) => s,
                            _ => {
                                self.pos -= 1;
                                return Err(self.unexpected("identifier"));
                            }
                        };
                        e = Expr::Scoped {
                            scope: Box::new(e),
                            name,
                        };
                    }
                    return Ok(e);
                }
                if self.is_op("#") && self.is_op_at(1, "(") && self.class_scope_follows() {
                    // `C#(8)::name`: a member of a parameterised class.
                    let params = Some(self.param_args()?);
                    let mut e = Expr::Type(Box::new(DataType::Named {
                        scope: None,
                        name: n,
                        params,
                        packed: Vec::new(),
                    }));
                    while self.eat_op("::").is_some() {
                        let name = match self.bump() {
                            Some(Token::Ident(s) | Token::EscapedIdent(s) | Token::Keyword(s)) => s,
                            _ => {
                                self.pos -= 1;
                                return Err(self.unexpected("identifier"));
                            }
                        };
                        e = Expr::Scoped {
                            scope: Box::new(e),
                            name,
                        };
                    }
                    return Ok(e);
                }
                Ok(Expr::Ident(n))
            }
            Token::SystemIdent(name) => {
                self.bump();
                if matches!(name, "$unit" | "$root") {
                    if self.eat_op("::").is_some() {
                        return Ok(Expr::Scoped {
                            scope: Box::new(Expr::Ident(name)),
                            name: self.ident()?,
                        });
                    }
                    return Ok(Expr::Ident(name));
                }
                let args = if self.is_op("(") {
                    self.call_args()?
                } else {
                    Vec::new()
                };
                Ok(Expr::SysCall { name, args })
            }
            Token::Op("(") => {
                self.bump();
                let a = self.expr()?;
                if let Some(Token::Op(op)) = self.peek()
                    && is_assign_op(op)
                {
                    self.bump();
                    let rhs = self.expr()?;
                    self.expect_op(")")?;
                    return Ok(Expr::Assign {
                        lhs: Box::new(a),
                        op,
                        rhs: Box::new(rhs),
                    });
                }
                if self.eat_op(":").is_some() {
                    let b = self.expr()?;
                    self.expect_op(":")?;
                    let c = self.expr()?;
                    self.expect_op(")")?;
                    return Ok(Expr::MinTypMax(Box::new([a, b, c])));
                }
                self.expect_op(")")?;
                Ok(a)
            }
            Token::Op("{") => self.concat(),
            Token::Op("'{") => self.pattern(),
            Token::Op(op @ "$") => {
                self.bump();
                Ok(Expr::Keyword(op))
            }
            Token::Keyword(k @ ("null" | "this" | "super")) => {
                self.bump();
                Ok(Expr::Keyword(k))
            }
            Token::Keyword("default") => {
                // Only valid as an assignment pattern key.
                self.bump();
                Ok(Expr::Keyword("default"))
            }
            Token::Keyword(kw @ "new") => {
                self.bump();
                let size = if self.eat_op("[").is_some() {
                    let s = self.expr()?;
                    self.expect_op("]")?;
                    Some(Box::new(s))
                } else {
                    None
                };
                let args = if self.is_op("(") {
                    self.call_args()?
                } else {
                    Vec::new()
                };
                // `new obj` copies an object.
                let copy = if size.is_none()
                    && args.is_empty()
                    && (self.is_ident_at(0) || self.is_kw("this"))
                {
                    Some(Box::new(self.postfix()?))
                } else {
                    None
                };
                Ok(Expr::New {
                    kw,
                    args,
                    size,
                    copy,
                })
            }
            Token::Keyword(k)
                if TYPE_KWS.contains(&k) || matches!(k, "signed" | "unsigned" | "const") =>
            {
                if self.is_op_at(1, "'") {
                    self.bump();
                    let ty = match k {
                        "signed" | "unsigned" | "const" => Expr::Keyword(k),
                        _ => Expr::Type(Box::new(DataType::Builtin {
                            kw: k,
                            signing: None,
                            packed: Vec::new(),
                        })),
                    };
                    self.expect_op("'")?;
                    self.expect_op("(")?;
                    let x = self.expr()?;
                    self.expect_op(")")?;
                    return Ok(Expr::Cast {
                        ty: Box::new(ty),
                        expr: Box::new(x),
                    });
                }
                Ok(Expr::Type(Box::new(self.data_type()?)))
            }
            Token::Keyword("type") => Ok(Expr::Type(Box::new(self.data_type()?))),
            Token::Keyword(k @ ("tagged" | "randomize")) => Err(self.not_yet(k, k)),
            _ => Err(self.unexpected("an expression")),
        }
    }

    /// After `name #(`: does a `::` follow the parameter list?
    fn class_scope_follows(&self) -> bool {
        let end = self.skip_balanced_from(self.pos + 1);
        matches!(self.toks.get(end), Some(Token::Op("::")))
    }

    /// `{...}`: concatenation, replication, streaming, or the empty queue `{}`.
    fn concat(&mut self) -> PResult<Expr<'a>> {
        self.expect_op("{")?;
        if self.eat_op("}").is_some() {
            return Ok(Expr::Concat(Vec::new()));
        }
        if let Some(Token::Op(op @ ("<<" | ">>"))) = self.peek() {
            self.bump();
            let slice = if self.is_op("{") {
                None
            } else {
                Some(Box::new(self.expr_or_type()?))
            };
            self.expect_op("{")?;
            let items = self.expr_list("}")?;
            self.expect_op("}")?;
            self.expect_op("}")?;
            return Ok(Expr::Stream { op, slice, items });
        }
        let first = self.expr()?;
        if self.is_op("{") {
            self.bump();
            let items = self.expr_list("}")?;
            self.expect_op("}")?;
            self.expect_op("}")?;
            return Ok(Expr::Repl {
                count: Box::new(first),
                items,
            });
        }
        let mut items = vec![first];
        while self.eat_op(",").is_some() {
            items.push(self.expr()?);
        }
        self.expect_op("}")?;
        Ok(Expr::Concat(items))
    }

    /// Comma-separated expressions up to (not including) `close`.
    fn expr_list(&mut self, close: &str) -> PResult<Vec<Expr<'a>>> {
        let mut items = Vec::new();
        if self.is_op(close) {
            return Ok(items);
        }
        loop {
            items.push(self.expr()?);
            if self.eat_op(",").is_none() {
                return Ok(items);
            }
        }
    }

    /// `'{ ... }` assignment pattern.
    fn pattern(&mut self) -> PResult<Expr<'a>> {
        self.expect_op("'{")?;
        let mut items = Vec::new();
        if self.eat_op("}").is_some() {
            return Ok(Expr::Pattern { ty: None, items });
        }
        loop {
            let first = self.expr_or_type()?;
            if self.eat_op(":").is_some() {
                items.push(PatItem::Keyed(first, self.expr()?));
            } else if self.is_op("{") {
                self.bump();
                let inner = self.expr_list("}")?;
                self.expect_op("}")?;
                items.push(PatItem::Repeat(first, inner));
            } else {
                items.push(PatItem::Value(first));
            }
            if self.eat_op(",").is_none() {
                break;
            }
        }
        self.expect_op("}")?;
        Ok(Expr::Pattern { ty: None, items })
    }
}
