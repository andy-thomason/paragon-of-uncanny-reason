//! Data types, dimensions, declarations and parameters.

use super::{PResult, Parser};
use crate::ast::*;
use crate::lex::Token;

/// Built-in type keywords.
pub(crate) const TYPE_KWS: &[&str] = &[
    "logic",
    "bit",
    "reg",
    "int",
    "integer",
    "byte",
    "shortint",
    "longint",
    "real",
    "realtime",
    "shortreal",
    "time",
    "string",
    "event",
    "chandle",
    "void",
];

/// Net type keywords.
pub(crate) const NET_KWS: &[&str] = &[
    "wire", "tri", "tri0", "tri1", "triand", "trior", "trireg", "wand", "wor", "supply0",
    "supply1", "uwire",
];

/// Qualifiers that may start a variable declaration.
pub(crate) const VAR_QUALIFIERS: &[&str] =
    &["const", "var", "static", "automatic", "rand", "randc"];

impl<'a> Parser<'a> {
    /// True if the next tokens start an explicit data type.
    pub(crate) fn at_type(&self) -> bool {
        match self.peek() {
            Some(Token::Keyword(k)) => {
                TYPE_KWS.contains(&k)
                    || matches!(k, "struct" | "union" | "enum" | "type" | "virtual")
            }
            Some(Token::Ident(s) | Token::EscapedIdent(s)) => {
                self.types.contains(s)
                    // `cls x`, `cls #(...) x`
                    || self.is_ident_at(1)
                    || (self.is_op_at(1, "#") && self.ident_after(self.pos + 1))
                    || (self.is_op_at(1, "::") && self.is_ident_at(2) && self.ident_follows_type(2))
            }
            Some(Token::SystemIdent("$unit")) => self.is_op_at(1, "::"),
            _ => false,
        }
    }

    /// For `pkg::name` at offset `k`: is it a type? True if a declared name
    /// follows it, as in `pkg::t x;`.
    /// Is there an identifier after the bracketed group starting at index `i`?
    fn ident_after(&self, i: usize) -> bool {
        let j = self.skip_balanced_from(i + 1);
        matches!(
            self.toks.get(j),
            Some(Token::Ident(_) | Token::EscapedIdent(_))
        )
    }

    fn ident_follows_type(&self, k: usize) -> bool {
        let mut i = self.pos + k + 1;
        while matches!(self.toks.get(i), Some(Token::Op("["))) {
            i = self.skip_balanced_from(i);
        }
        matches!(
            self.toks.get(i),
            Some(Token::Ident(_) | Token::EscapedIdent(_))
        )
    }

    /// At `ifc.t` or `ifc[0].t`: an identifier followed by `.`, or by
    /// brackets and then `.`.
    fn at_iface_type(&self) -> bool {
        self.is_op_at(1, ".") && self.is_ident_at(2)
            || (self.is_op_at(1, "[") && {
                let j = self.skip_balanced_from(self.pos + 1);
                matches!(self.toks.get(j), Some(Token::Op(".")))
            })
    }

    /// At `[` : is the bracketed group followed by `.`?
    fn dot_after_brackets(&self) -> bool {
        let j = self.skip_balanced_from(self.pos);
        matches!(self.toks.get(j), Some(Token::Op(".")))
    }

    /// A data type, or an implicit one (`[7:0]`, `signed`, or nothing).
    pub(crate) fn data_type_or_implicit(&mut self) -> PResult<DataType<'a>> {
        if self.at_type() {
            return self.data_type();
        }
        let signing = self.eat_kw_of(&["signed", "unsigned"]);
        let packed = self.dims()?;
        Ok(DataType::Implicit { signing, packed })
    }

    pub(crate) fn data_type(&mut self) -> PResult<DataType<'a>> {
        match self.peek() {
            Some(Token::Keyword(k)) if TYPE_KWS.contains(&k) => {
                self.bump();
                let signing = self.eat_kw_of(&["signed", "unsigned"]);
                let packed = self.dims()?;
                Ok(DataType::Builtin {
                    kw: k,
                    signing,
                    packed,
                })
            }
            Some(Token::Keyword("enum")) => self.enum_type(),
            Some(Token::Keyword(k @ ("struct" | "union"))) => {
                self.bump();
                if self.is_kw("tagged") {
                    return Err(self.not_yet(k, "tagged unions"));
                }
                self.eat_kw("soft");
                let packed = self.eat_kw("packed").is_some();
                let signing = self.eat_kw_of(&["signed", "unsigned"]);
                self.expect_op("{")?;
                let mut members = Vec::new();
                while self.eat_op("}").is_none() {
                    let mut qualifiers = Vec::new();
                    while let Some(q) = self.eat_kw_of(&["rand", "randc"]) {
                        qualifiers.push(q);
                    }
                    let ty = self.data_type_or_implicit()?;
                    let vars = self.declarators()?;
                    self.expect_op(";")?;
                    members.push(VarDecl {
                        qualifiers,
                        net: None,
                        ty,
                        vars,
                    });
                }
                let dims = self.dims()?;
                Ok(DataType::Struct {
                    kw: k,
                    packed,
                    signing,
                    members,
                    dims,
                })
            }
            Some(Token::Keyword("type")) => {
                self.bump();
                self.expect_op("(")?;
                let e = self.expr_or_type()?;
                self.expect_op(")")?;
                Ok(DataType::TypeOf(Box::new(e)))
            }
            Some(Token::Keyword(k @ "virtual")) => Err(self.not_yet(k, "virtual interfaces")),
            Some(Token::Ident(_) | Token::EscapedIdent(_)) if self.at_iface_type() => {
                // ifc.t, ifc[0].t, ifc.sub.t: the last name is the type.
                let mut path = Expr::Ident(self.ident()?);
                loop {
                    if self.is_op("[") {
                        self.bump();
                        let index = self.expr()?;
                        self.expect_op("]")?;
                        path = Expr::Index {
                            base: Box::new(path),
                            index: Box::new(index),
                        };
                    } else {
                        self.expect_op(".")?;
                        let n = self.ident()?;
                        // The path ends at the last name: no `.` follows, directly or after brackets.
                        let more_path =
                            self.is_op(".") || (self.is_op("[") && self.dot_after_brackets());
                        if !more_path {
                            let packed = self.dims()?;
                            return Ok(DataType::IfaceType {
                                iface: Box::new(path),
                                name: n,
                                packed,
                            });
                        }
                        path = Expr::Member {
                            base: Box::new(path),
                            name: n,
                        };
                    }
                }
            }
            Some(Token::SystemIdent(u @ "$unit")) if self.is_op_at(1, "::") => {
                self.bump();
                self.bump();
                let name = self.ident()?;
                let packed = self.dims()?;
                Ok(DataType::Named {
                    scope: Some(u),
                    name,
                    params: None,
                    packed,
                })
            }
            Some(Token::Ident(_) | Token::EscapedIdent(_)) => {
                let first = self.ident()?;
                let (scope, name) = if self.eat_op("::").is_some() {
                    (Some(first), self.ident()?)
                } else {
                    (None, first)
                };
                let params = if self.is_op("#") {
                    Some(self.param_args()?)
                } else {
                    None
                };
                let packed = self.dims()?;
                Ok(DataType::Named {
                    scope,
                    name,
                    params,
                    packed,
                })
            }
            _ => Err(self.unexpected("a data type")),
        }
    }

    fn enum_type(&mut self) -> PResult<DataType<'a>> {
        let kw = self.expect_kw("enum")?;
        let base = if self.is_op("{") {
            None
        } else if self.is_ident_at(0) {
            // A named base type: enum my_t {...}, enum pkg::t {...}
            Some(Box::new(self.data_type()?))
        } else {
            Some(Box::new(self.data_type_or_implicit()?))
        };
        self.expect_op("{")?;
        let mut items = Vec::new();
        loop {
            let name = self.ident()?;
            let range = if self.eat_op("[").is_some() {
                let a = self.expr()?;
                let b = if self.eat_op(":").is_some() {
                    Some(self.expr()?)
                } else {
                    None
                };
                self.expect_op("]")?;
                Some((a, b))
            } else {
                None
            };
            let value = if self.eat_op("=").is_some() {
                Some(self.expr()?)
            } else {
                None
            };
            items.push(EnumItem { name, range, value });
            if self.eat_op(",").is_none() {
                break;
            }
        }
        self.expect_op("}")?;
        let packed = self.dims()?;
        Ok(DataType::Enum {
            kw,
            base,
            items,
            packed,
        })
    }

    /// Zero or more `[...]` dimensions.
    pub(crate) fn dims(&mut self) -> PResult<Vec<Dim<'a>>> {
        let mut dims = Vec::new();
        while self.is_op("[") {
            dims.push(self.dim()?);
        }
        Ok(dims)
    }

    fn dim(&mut self) -> PResult<Dim<'a>> {
        self.expect_op("[")?;
        let d = if self.eat_op("]").is_some() {
            return Ok(Dim::Dynamic);
        } else if self.is_op("$") {
            self.bump();
            let max = if self.eat_op(":").is_some() {
                Some(self.expr()?)
            } else {
                None
            };
            Dim::Queue(max)
        } else if self.is_op("*") && self.is_op_at(1, "]") {
            self.bump();
            Dim::Assoc(None)
        } else if self.at_type() && (!self.is_ident_at(0) || self.is_op_at(1, "]")) {
            // [string], [int], or [my_t] for a known type name.
            Dim::Assoc(Some(Box::new(self.data_type()?)))
        } else {
            let a = self.expr()?;
            if self.eat_op(":").is_some() {
                Dim::Range(a, self.expr()?)
            } else {
                Dim::Size(a)
            }
        };
        self.expect_op("]")?;
        Ok(d)
    }

    /// `name [dims] [= init]` separated by commas.
    pub(crate) fn declarators(&mut self) -> PResult<Vec<Declarator<'a>>> {
        let mut vars = Vec::new();
        loop {
            let name = self.ident()?;
            let dims = self.dims()?;
            let init = if self.eat_op("=").is_some() {
                Some(self.expr()?)
            } else {
                None
            };
            vars.push(Declarator { name, dims, init });
            if self.eat_op(",").is_none() {
                return Ok(vars);
            }
        }
    }

    /// The rest of a net or variable declaration after any qualifiers and net
    /// keyword: type, declarators and `;`.
    pub(crate) fn var_decl(
        &mut self,
        qualifiers: Vec<&'a str>,
        net: Option<&'a str>,
    ) -> PResult<VarDecl<'a>> {
        if net.is_some() {
            self.eat_kw_of(&["vectored", "scalared"]);
            self.strength()?;
        }
        let ty = if net.is_some() || !qualifiers.is_empty() {
            self.data_type_or_implicit()?
        } else {
            self.data_type()?
        };
        if net.is_some() && self.is_op("#") {
            self.delay_value()?;
        }
        let vars = self.declarators()?;
        self.expect_op(";")?;
        Ok(VarDecl {
            qualifiers,
            net,
            ty,
            vars,
        })
    }

    /// Skip a drive or charge strength such as `(strong0, weak1)`.
    pub(crate) fn strength(&mut self) -> PResult<()> {
        const STRENGTHS: &[&str] = &[
            "supply0", "strong0", "pull0", "weak0", "highz0", "supply1", "strong1", "pull1",
            "weak1", "highz1", "small", "medium", "large",
        ];
        if self.is_op("(")
            && matches!(self.peek_at(1), Some(Token::Keyword(k)) if STRENGTHS.contains(&k))
        {
            let end = self.skip_balanced_from(self.pos);
            self.pos = end;
        }
        Ok(())
    }

    /// `#value` or `#(a, b, c)`: a delay. Returns the first value.
    pub(crate) fn delay_value(&mut self) -> PResult<Expr<'a>> {
        self.expect_op("#")?;
        if self.eat_op("(").is_some() {
            let e = self.mintypmax()?;
            while self.eat_op(",").is_some() {
                self.mintypmax()?;
            }
            self.expect_op(")")?;
            return Ok(e);
        }
        match self.peek() {
            Some(Token::Number(n)) => {
                self.bump();
                Ok(Expr::Number(n))
            }
            // A parameter, possibly hierarchical or package-scoped: #W, #sub.delay, #p::D
            Some(Token::Ident(_) | Token::EscapedIdent(_)) => {
                let first = self.ident()?;
                let mut e = if self.eat_op("::").is_some() {
                    Expr::Scoped {
                        scope: Box::new(Expr::Ident(first)),
                        name: self.ident()?,
                    }
                } else {
                    Expr::Ident(first)
                };
                while self.is_op(".") && self.is_ident_at(1) {
                    self.bump();
                    e = Expr::Member {
                        base: Box::new(e),
                        name: self.ident()?,
                    };
                }
                Ok(e)
            }
            _ => Err(self.unexpected("a delay value")),
        }
    }

    /// `parameter`/`localparam` declaration up to, but not including, the `;`.
    pub(crate) fn param_decl(&mut self) -> PResult<ParamDecl<'a>> {
        let kw = self.eat_kw_of(&["parameter", "localparam"]);
        self.param_decl_body(kw)
    }

    fn param_decl_body(&mut self, kw: Option<&'a str>) -> PResult<ParamDecl<'a>> {
        if self.eat_kw("type").is_some() {
            let mut assigns = Vec::new();
            loop {
                let name = self.ident()?;
                self.types.insert(name);
                let value = if self.eat_op("=").is_some() {
                    Some(Expr::Type(Box::new(self.data_type()?)))
                } else {
                    None
                };
                assigns.push(ParamAssign {
                    name,
                    dims: Vec::new(),
                    value,
                });
                if !(self.is_op(",") && self.is_ident_at(1) && !self.next_starts_param(1)) {
                    break;
                }
                self.bump();
            }
            return Ok(ParamDecl {
                kw,
                ty: None,
                assigns,
            });
        }
        // A type is present unless the name comes straight away. `name [1:0] = ...`
        // is a name with dimensions; `my_t [1:0] name` is a type.
        let name_first = self.is_ident_at(0)
            && (self.is_op_at(1, "=")
                || self.is_op_at(1, ",")
                || self.is_op_at(1, ")")
                || self.is_op_at(1, ";")
                || (self.is_op_at(1, "[") && !self.ident_after_dims(self.pos + 1)));
        let ty = if name_first {
            None
        } else {
            Some(self.data_type_or_implicit()?)
        };
        let mut assigns = Vec::new();
        loop {
            let name = self.ident()?;
            let dims = self.dims()?;
            let value = if self.eat_op("=").is_some() {
                Some(self.expr()?)
            } else {
                None
            };
            assigns.push(ParamAssign { name, dims, value });
            // In a parameter port list a comma may start a new declaration.
            if !(self.is_op(",") && self.is_ident_at(1) && !self.next_starts_param(1)) {
                break;
            }
            self.bump();
        }
        Ok(ParamDecl { kw, ty, assigns })
    }

    /// Is there an identifier after the dimensions starting at index `i`?
    fn ident_after_dims(&self, mut i: usize) -> bool {
        while matches!(self.toks.get(i), Some(Token::Op("["))) {
            i = self.skip_balanced_from(i);
        }
        matches!(
            self.toks.get(i),
            Some(Token::Ident(_) | Token::EscapedIdent(_))
        )
    }

    /// Does the token at `k` start a new parameter declaration (a type
    /// followed by a name) rather than another name in the same one?
    fn next_starts_param(&self, k: usize) -> bool {
        self.is_ident_at(k) && self.is_ident_at(k + 1)
    }

    /// `#( ... )` parameter port list of a module or interface.
    pub(crate) fn param_port_list(&mut self) -> PResult<Vec<ParamDecl<'a>>> {
        self.expect_op("#")?;
        self.expect_op("(")?;
        let mut decls = Vec::new();
        if self.eat_op(")").is_some() {
            return Ok(decls);
        }
        let mut kw = None;
        loop {
            if let Some(k) = self.eat_kw_of(&["parameter", "localparam"]) {
                kw = Some(k);
            }
            decls.push(self.param_decl_body(kw)?);
            if self.eat_op(",").is_none() {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(decls)
    }

    /// `#( ... )` or `#value` parameter overrides.
    pub(crate) fn param_args(&mut self) -> PResult<Vec<ParamArg<'a>>> {
        self.expect_op("#")?;
        if !self.is_op("(") {
            // Legacy `#5` or `#W` override.
            let e = match self.bump() {
                Some(Token::Number(n)) => Expr::Number(n),
                Some(Token::Ident(n) | Token::EscapedIdent(n)) => Expr::Ident(n),
                _ => return Err(self.unexpected("'('")),
            };
            return Ok(vec![ParamArg::Ordered(e)]);
        }
        self.expect_op("(")?;
        let mut args = Vec::new();
        if self.eat_op(")").is_some() {
            return Ok(args);
        }
        loop {
            if self.eat_op(".").is_some() {
                let name = self.ident()?;
                self.expect_op("(")?;
                let value = if self.is_op(")") {
                    None
                } else {
                    Some(self.expr_or_type()?)
                };
                self.expect_op(")")?;
                args.push(ParamArg::Named(name, value));
            } else {
                args.push(ParamArg::Ordered(self.expr_or_type()?));
            }
            if self.eat_op(",").is_none() {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(args)
    }

    /// `typedef ...;`
    pub(crate) fn typedef(&mut self) -> PResult<ModuleItem<'a>> {
        self.expect_kw("typedef")?;
        // Forward declarations: typedef name; typedef enum name; typedef class name;
        if self.is_kw("interface")
            && self.is_kw_at(1, "class")
            && self.is_ident_at(2)
            && self.is_op_at(3, ";")
        {
            self.bump();
        }
        let kind = ["enum", "struct", "union", "class"]
            .iter()
            .any(|k| self.is_kw(k));
        if kind && self.is_ident_at(1) && self.is_op_at(2, ";") {
            self.bump();
        }
        if self.is_ident_at(0) && self.is_op_at(1, ";") {
            let name = self.ident()?;
            self.types.insert(name);
            self.expect_op(";")?;
            return Ok(ModuleItem::ForwardTypedef(name));
        }
        if self.is_kw("class") {
            return Err(self.not_yet(self.here(), "typedef class"));
        }
        let ty = self.data_type()?;
        let name = self.ident()?;
        let dims = self.dims()?;
        self.expect_op(";")?;
        self.types.insert(name);
        Ok(ModuleItem::Typedef(Typedef { ty, name, dims }))
    }
}
