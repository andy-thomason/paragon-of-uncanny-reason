//! Source text, modules, ports, module items and subroutines.

use super::types::{NET_KWS, TYPE_KWS, VAR_QUALIFIERS};
use super::{PResult, Parser};
use crate::ast::*;
use crate::lex::Token;

const DIRECTIONS: &[&str] = &["input", "output", "inout", "ref"];

const GATES: &[&str] = &[
    "and", "nand", "or", "nor", "xor", "xnor", "buf", "not", "bufif0", "bufif1", "notif0",
    "notif1", "pullup", "pulldown", "nmos", "pmos", "cmos", "rcmos", "rnmos", "rpmos", "tran",
    "tranif0", "tranif1", "rtran", "rtranif0", "rtranif1",
];

const PROCESSES: &[&str] = &[
    "always",
    "always_comb",
    "always_ff",
    "always_latch",
    "initial",
    "final",
];

/// Constructs recognised but not yet implemented, with a description.
const NOT_YET_ITEMS: &[(&str, &str)] = &[
    ("covergroup", "covergroups"),
    ("property", "properties"),
    ("sequence", "sequences"),
    ("constraint", "constraints"),
    ("clocking", "clocking blocks"),
    ("checker", "checkers"),
    ("bind", "bind"),
    ("alias", "alias"),
    ("let", "let"),
    ("nettype", "nettype"),
    ("interconnect", "interconnect"),
    ("primitive", "user-defined primitives"),
    ("config", "configurations"),
    ("extern", "extern declarations"),
    ("restrict", "restrict"),
    ("global", "global clocking"),
];

impl<'a> Parser<'a> {
    pub(crate) fn source_text(&mut self) -> PResult<SourceText<'a>> {
        let mut items = Vec::new();
        while let Some(t) = self.peek() {
            match t {
                Token::Keyword("module" | "macromodule" | "interface" | "program")
                    if !(t == Token::Keyword("interface") && self.is_kw_at(1, "class")) =>
                {
                    items.push(Item::Module(self.module()?));
                }
                Token::Keyword("package") => items.push(Item::Package(self.package()?)),
                Token::Directive(d) => {
                    self.bump();
                    items.push(Item::Directive(d));
                }
                Token::Op(";") => {
                    self.bump();
                }
                _ => {
                    if let Some(item) = self.module_item()? {
                        items.push(Item::Decl(item));
                    }
                }
            }
        }
        Ok(SourceText { items })
    }

    fn package(&mut self) -> PResult<Package<'a>> {
        self.expect_kw("package")?;
        self.eat_kw_of(&["static", "automatic"]);
        let name = self.ident()?;
        self.expect_op(";")?;
        let mut items = Vec::new();
        let end = loop {
            if let Some(e) = self.eat_kw("endpackage") {
                break e;
            }
            if self.peek().is_none() {
                return Err(self.unexpected("'endpackage'"));
            }
            if let Some(item) = self.module_item()? {
                items.push(item);
            }
        };
        self.end_label()?;
        Ok(Package { name, items, end })
    }

    pub(crate) fn module(&mut self) -> PResult<Module<'a>> {
        let kind = self.bump().unwrap().text();
        let end_kw = match kind {
            "interface" => "endinterface",
            "program" => "endprogram",
            _ => "endmodule",
        };
        let lifetime = self.eat_kw_of(&["static", "automatic"]);
        if self.is_op(";") {
            return Err(self.not_yet(kind, "anonymous programs"));
        }
        let name = self.ident()?;
        let mut imports = Vec::new();
        while self.is_kw("import") {
            imports.extend(self.import()?);
        }
        let params = if self.is_op("#") {
            Some(self.param_port_list()?)
        } else {
            None
        };
        let ports = self.ports()?;
        self.expect_op(";")?;
        let mut items = Vec::new();
        let end = loop {
            if let Some(e) = self.eat_kw(end_kw) {
                break e;
            }
            if self.peek().is_none() {
                return Err(self.unexpected(&format!("'{end_kw}'")));
            }
            if let Some(item) = self.module_item()? {
                items.push(item);
            }
        };
        self.end_label()?;
        Ok(Module {
            kind,
            lifetime,
            name,
            imports,
            params,
            ports,
            items,
            end,
        })
    }

    /// `import a::b, c::*;`
    fn import(&mut self) -> PResult<Vec<Import<'a>>> {
        let kw = self.expect_kw("import")?;
        if matches!(self.peek(), Some(Token::Str(_))) {
            return Err(self.not_yet(kw, "DPI imports"));
        }
        let mut v = Vec::new();
        loop {
            let package = self.ident()?;
            self.expect_op("::")?;
            let name = match self.bump() {
                Some(Token::Op(s @ "*")) => s,
                Some(Token::Ident(s) | Token::EscapedIdent(s)) => s,
                _ => {
                    self.pos -= 1;
                    return Err(self.unexpected("a name or '*'"));
                }
            };
            v.push(Import { package, name });
            if self.eat_op(",").is_none() {
                break;
            }
        }
        self.expect_op(";")?;
        Ok(v)
    }

    fn ports(&mut self) -> PResult<Ports<'a>> {
        if self.eat_op("(").is_none() {
            return Ok(Ports::None);
        }
        if self.eat_op(")").is_some() {
            return Ok(Ports::None);
        }
        if self.is_op(".*") {
            self.bump();
            self.expect_op(")")?;
            return Ok(Ports::Wildcard);
        }
        let ansi = match self.peek() {
            Some(Token::Keyword(k)) => {
                DIRECTIONS.contains(&k)
                    || NET_KWS.contains(&k)
                    || TYPE_KWS.contains(&k)
                    || matches!(
                        k,
                        "var" | "interface" | "struct" | "union" | "enum" | "signed" | "unsigned"
                    )
            }
            Some(Token::Ident(s) | Token::EscapedIdent(s)) => {
                self.types.contains(s)
                    || self.is_ident_at(1)
                    || (self.is_op_at(1, ".") && self.is_ident_at(2) && self.is_ident_at(3))
                    || self.is_op_at(1, "::")
            }
            Some(Token::Op("[")) => true,
            _ => false,
        };
        let ports = if ansi {
            Ports::Ansi(self.ansi_ports()?)
        } else {
            Ports::NonAnsi(self.non_ansi_ports()?)
        };
        self.expect_op(")")?;
        Ok(ports)
    }

    fn non_ansi_ports(&mut self) -> PResult<Vec<PortRef<'a>>> {
        let mut v = Vec::new();
        loop {
            if matches!(self.peek(), Some(Token::Keyword(k)) if DIRECTIONS.contains(&k)) {
                // Verilator accepts a header that switches to ANSI style part way.
                return Err(self.not_yet(self.here(), "mixed ANSI and non-ANSI port lists"));
            }
            if self.eat_op(".").is_some() {
                let name = self.ident()?;
                self.expect_op("(")?;
                let expr = if self.is_op(")") {
                    None
                } else {
                    Some(self.expr()?)
                };
                self.expect_op(")")?;
                v.push(PortRef {
                    name: Some(name),
                    expr,
                });
            } else if self.is_op(",") || self.is_op(")") {
                v.push(PortRef {
                    name: None,
                    expr: None,
                });
            } else {
                v.push(PortRef {
                    name: None,
                    expr: Some(self.postfix()?),
                });
            }
            if self.eat_op(",").is_none() {
                return Ok(v);
            }
        }
    }

    fn ansi_ports(&mut self) -> PResult<Vec<AnsiPort<'a>>> {
        let mut v: Vec<AnsiPort<'a>> = Vec::new();
        loop {
            let dir = self.eat_kw_of(DIRECTIONS);
            let kind = self.eat_kw_of(NET_KWS).or_else(|| self.eat_kw("var"));
            let mut interface = None;
            let given = if self.eat_kw("interface").is_some() {
                let mp = if self.eat_op(".").is_some() {
                    Some(self.ident()?)
                } else {
                    None
                };
                interface = Some(("interface", mp));
                Some(DataType::Implicit {
                    signing: None,
                    packed: Vec::new(),
                })
            } else if self.is_ident_at(0)
                && self.is_op_at(1, ".")
                && self.is_ident_at(2)
                && self.is_ident_at(3)
            {
                let i = self.ident()?;
                self.bump();
                interface = Some((i, Some(self.ident()?)));
                Some(DataType::Implicit {
                    signing: None,
                    packed: Vec::new(),
                })
            } else if self.at_type()
                || (self.is_ident_at(0) && (self.is_ident_at(1) || self.is_op_at(1, "#")))
                || (self.is_ident_at(0)
                    && self.is_op_at(1, "[")
                    && self.types.contains(self.here()))
            {
                // A named type or interface followed by the port name.
                Some(self.data_type()?)
            } else if self.is_kw("signed") || self.is_kw("unsigned") || self.is_op("[") {
                Some(self.data_type_or_implicit()?)
            } else {
                None
            };
            // A port with no direction, kind or type takes all three from the previous port.
            let (dir, kind, ty, interface) = match (dir, kind, given, v.last()) {
                (None, None, None, Some(prev)) => {
                    (prev.dir, prev.kind, prev.ty.clone(), prev.interface)
                }
                (dir, kind, ty, _) => (
                    dir,
                    kind,
                    ty.unwrap_or(DataType::Implicit {
                        signing: None,
                        packed: Vec::new(),
                    }),
                    interface,
                ),
            };
            let name = self.ident()?;
            let dims = self.dims()?;
            let default = if self.eat_op("=").is_some() {
                Some(self.expr()?)
            } else {
                None
            };
            v.push(AnsiPort {
                dir,
                kind,
                ty,
                interface,
                name,
                dims,
                default,
            });
            if self.eat_op(",").is_none() {
                return Ok(v);
            }
        }
    }

    /// One module item. Returns `None` for items that produce nothing (`;`, ignored blocks).
    pub(crate) fn module_item(&mut self) -> PResult<Option<ModuleItem<'a>>> {
        let Some(t) = self.peek() else {
            return Err(self.unexpected("a module item"));
        };
        let item = match t {
            Token::Op(";") => {
                self.bump();
                return Ok(None);
            }
            Token::Directive(d) => {
                self.bump();
                ModuleItem::Directive(d)
            }
            Token::SystemIdent("$info" | "$warning" | "$error" | "$fatal") => {
                let e = self.postfix()?;
                self.expect_op(";")?;
                ModuleItem::ElabTask(e)
            }
            Token::Keyword(k) if DIRECTIONS.contains(&k) => {
                self.bump();
                let net = self.eat_kw_of(NET_KWS);
                let mut qualifiers = Vec::new();
                if let Some(v) = self.eat_kw("var") {
                    qualifiers.push(v);
                }
                let decl = self.port_var_decl(qualifiers, net)?;
                ModuleItem::Port(PortDecl { dir: k, decl })
            }
            Token::Keyword(k) if NET_KWS.contains(&k) => {
                self.bump();
                ModuleItem::Var(self.var_decl(Vec::new(), Some(k))?)
            }
            Token::Keyword("parameter" | "localparam") => {
                let d = self.param_decl()?;
                self.expect_op(";")?;
                ModuleItem::Param(d)
            }
            Token::Keyword("typedef") => self.typedef()?,
            Token::Keyword("import") => ModuleItem::Import(self.import()?),
            Token::Keyword(kw @ "export") => {
                if matches!(self.peek_at(1), Some(Token::Str(_))) {
                    return Err(self.not_yet(kw, "DPI exports"));
                }
                while self.bump().is_some_and(|t| t != Token::Op(";")) {}
                return Ok(None);
            }
            Token::Keyword(kw @ "assign") => {
                self.bump();
                self.strength()?;
                let delay = if self.is_op("#") {
                    Some(self.delay_value()?)
                } else {
                    None
                };
                let mut assigns = Vec::new();
                loop {
                    let lhs = self.postfix()?;
                    self.expect_op("=")?;
                    assigns.push((lhs, self.expr()?));
                    if self.eat_op(",").is_none() {
                        break;
                    }
                }
                self.expect_op(";")?;
                ModuleItem::Assign { kw, delay, assigns }
            }
            Token::Keyword(kw) if PROCESSES.contains(&kw) => {
                self.bump();
                ModuleItem::Process {
                    kw,
                    stmt: self.statement()?,
                }
            }
            Token::Keyword("generate") => {
                self.bump();
                let mut items = Vec::new();
                while self.eat_kw("endgenerate").is_none() {
                    if self.peek().is_none() {
                        return Err(self.unexpected("'endgenerate'"));
                    }
                    if let Some(i) = self.module_item()? {
                        items.push(i);
                    }
                }
                ModuleItem::Generate(items)
            }
            Token::Keyword("genvar") => {
                self.bump();
                let mut names = vec![self.ident()?];
                while self.eat_op(",").is_some() {
                    names.push(self.ident()?);
                }
                self.expect_op(";")?;
                ModuleItem::Genvar(names)
            }
            Token::Keyword("for") => self.gen_for()?,
            Token::Keyword(kw @ "if") => {
                self.bump();
                let cond = self.paren_expr()?;
                let then = Box::new(self.gen_block()?);
                let els = if self.eat_kw("else").is_some() {
                    Some(Box::new(self.gen_block()?))
                } else {
                    None
                };
                ModuleItem::GenIf {
                    kw,
                    cond,
                    then,
                    els,
                }
            }
            Token::Keyword(kw @ "case") => {
                self.bump();
                let expr = self.paren_expr()?;
                let mut items = Vec::new();
                while self.eat_kw("endcase").is_none() {
                    let mut labels = Vec::new();
                    if self.eat_kw("default").is_some() {
                        self.eat_op(":");
                    } else {
                        loop {
                            labels.push(self.expr()?);
                            if self.eat_op(",").is_none() {
                                break;
                            }
                        }
                        self.expect_op(":")?;
                    }
                    items.push((labels, self.gen_block()?));
                }
                ModuleItem::GenCase { kw, expr, items }
            }
            Token::Keyword("begin") => ModuleItem::GenBlock(self.gen_block()?),
            Token::Keyword("function" | "task") => self.subroutine()?,
            Token::Keyword("defparam") => {
                self.bump();
                let mut v = Vec::new();
                loop {
                    let lhs = self.postfix()?;
                    self.expect_op("=")?;
                    v.push((lhs, self.expr()?));
                    if self.eat_op(",").is_none() {
                        break;
                    }
                }
                self.expect_op(";")?;
                ModuleItem::Defparam(v)
            }
            Token::Keyword(kw @ ("timeunit" | "timeprecision")) => {
                self.bump();
                let mut values = Vec::new();
                while let Some(t) = self.bump() {
                    if t == Token::Op(";") {
                        break;
                    }
                    values.push(t.text());
                }
                ModuleItem::TimeUnits { kw, values }
            }
            Token::Keyword("modport") => self.modport()?,
            Token::Keyword("class") => ModuleItem::Class(self.class_decl(None)?),
            Token::Keyword(k @ ("virtual" | "interface")) if self.is_kw_at(1, "class") => {
                self.bump();
                ModuleItem::Class(self.class_decl(Some(k))?)
            }
            Token::Keyword(k @ "virtual") => {
                return Err(self.not_yet(k, "virtual interfaces"));
            }
            Token::Keyword("module" | "interface" | "program")
                if !(t == Token::Keyword("interface") && self.is_kw_at(1, "class")) =>
            {
                ModuleItem::Module(self.module()?)
            }
            Token::Keyword(k) if GATES.contains(&k) => self.gate(k)?,
            Token::Keyword("specify") => {
                // Specify blocks are parsed loosely and ignored, as Verilator does.
                self.skip_to_kw("endspecify")?;
                return Ok(None);
            }
            Token::Keyword("specparam") => {
                while self.bump().is_some_and(|t| t != Token::Op(";")) {}
                return Ok(None);
            }
            Token::Keyword(kw @ ("assert" | "assume" | "cover")) => {
                if self.is_kw_at(1, "property") || self.is_kw_at(1, "sequence") {
                    return Err(self.not_yet(kw, "concurrent assertions"));
                }
                return Err(self.not_yet(kw, "immediate assertions at module level"));
            }
            Token::Keyword("default")
                if self.is_kw_at(1, "clocking") || self.is_kw_at(1, "disable") =>
            {
                return Err(self.not_yet(t.text(), "default clocking"));
            }
            Token::Keyword(k) if NOT_YET_ITEMS.iter().any(|(w, _)| *w == k) => {
                let what = NOT_YET_ITEMS.iter().find(|(w, _)| *w == k).unwrap().1;
                return Err(self.not_yet(k, what));
            }
            Token::Keyword("interface") => return Err(self.not_yet(t.text(), "interface classes")),
            Token::Ident(_) | Token::EscapedIdent(_) if self.is_op_at(1, ":") => {
                // A labelled concurrent assertion or similar.
                return Err(self.not_yet(t.text(), "labelled module items (concurrent assertions)"));
            }
            Token::SystemIdent("$unit") if self.is_op_at(1, "::") => {
                ModuleItem::Var(self.var_decl(Vec::new(), None)?)
            }
            Token::Keyword(k)
                if TYPE_KWS.contains(&k)
                    || VAR_QUALIFIERS.contains(&k)
                    || matches!(
                        k,
                        "struct" | "union" | "enum" | "signed" | "unsigned" | "type"
                    ) =>
            {
                let mut qualifiers = Vec::new();
                while let Some(q) = self.eat_kw_of(VAR_QUALIFIERS) {
                    qualifiers.push(q);
                }
                ModuleItem::Var(self.var_decl(qualifiers, None)?)
            }
            Token::Ident(_) | Token::EscapedIdent(_) => {
                if self.at_instance() {
                    ModuleItem::Instance(self.instance()?)
                } else {
                    ModuleItem::Var(self.var_decl(Vec::new(), None)?)
                }
            }
            _ => return Err(self.unexpected("a module item")),
        };
        Ok(Some(item))
    }

    /// Port declarations allow an implicit type: `input [7:0] a;`, `output reg b;`.
    fn port_var_decl(
        &mut self,
        qualifiers: Vec<&'a str>,
        net: Option<&'a str>,
    ) -> PResult<VarDecl<'a>> {
        let ty = self.data_type_or_implicit()?;
        let vars = self.declarators()?;
        self.expect_op(";")?;
        Ok(VarDecl {
            qualifiers,
            net,
            ty,
            vars,
        })
    }

    /// At an identifier in module scope: is this an instantiation rather than
    /// a declaration of a variable with a user-defined type?
    fn at_instance(&self) -> bool {
        let mut i = self.pos + 1;
        if matches!(self.toks.get(i), Some(Token::Op("::"))) {
            return false;
        }
        if matches!(self.toks.get(i), Some(Token::Op("#"))) {
            // `sub #(..) u (...)` is an instance; `cls #(..) x;` declares a variable.
            i = self.skip_balanced_from(i + 1);
        }
        if matches!(self.toks.get(i), Some(Token::Op("("))) {
            // An unnamed instance: `my_udp (o, a, b);`
            return true;
        }
        if !matches!(
            self.toks.get(i),
            Some(Token::Ident(_) | Token::EscapedIdent(_))
        ) {
            return false;
        }
        i += 1;
        while matches!(self.toks.get(i), Some(Token::Op("["))) {
            i = self.skip_balanced_from(i);
        }
        matches!(self.toks.get(i), Some(Token::Op("(")))
    }

    fn instance(&mut self) -> PResult<Instance<'a>> {
        let module = self.ident()?;
        let params = if self.is_op("#") {
            Some(self.param_args()?)
        } else {
            None
        };
        let mut insts = Vec::new();
        loop {
            let (name, dims) = if self.is_op("(") {
                ("", Vec::new())
            } else {
                (self.ident()?, self.dims()?)
            };
            let conns = self.port_conns()?;
            insts.push(Inst { name, dims, conns });
            if self.eat_op(",").is_none() {
                break;
            }
        }
        self.expect_op(";")?;
        Ok(Instance {
            module,
            params,
            insts,
        })
    }

    fn port_conns(&mut self) -> PResult<Vec<PortConn<'a>>> {
        self.expect_op("(")?;
        let mut v = Vec::new();
        if self.eat_op(")").is_some() {
            return Ok(v);
        }
        loop {
            if self.eat_op(".*").is_some() {
                v.push(PortConn::Wildcard);
            } else if self.is_op(".") {
                self.bump();
                let name = self.ident()?;
                let e = if self.eat_op("(").is_some() {
                    let e = if self.is_op(")") {
                        None
                    } else {
                        Some(self.expr()?)
                    };
                    self.expect_op(")")?;
                    Some(e)
                } else {
                    None
                };
                v.push(PortConn::Named(name, e));
            } else if self.is_op(",") || self.is_op(")") {
                v.push(PortConn::Ordered(None));
            } else {
                v.push(PortConn::Ordered(Some(self.expr()?)));
            }
            if self.eat_op(",").is_none() {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(v)
    }

    fn gate(&mut self, kind: &'a str) -> PResult<ModuleItem<'a>> {
        self.bump();
        self.strength()?;
        let delay = if self.is_op("#") {
            Some(self.delay_value()?)
        } else {
            None
        };
        let mut insts = Vec::new();
        loop {
            let name = if self.is_ident_at(0) {
                let n = self.ident()?;
                self.dims()?;
                Some(n)
            } else {
                None
            };
            self.expect_op("(")?;
            let mut args = vec![self.expr()?];
            while self.eat_op(",").is_some() {
                args.push(self.expr()?);
            }
            self.expect_op(")")?;
            insts.push((name, args));
            if self.eat_op(",").is_none() {
                break;
            }
        }
        self.expect_op(";")?;
        Ok(ModuleItem::Gate(Gate { kind, delay, insts }))
    }

    fn gen_for(&mut self) -> PResult<ModuleItem<'a>> {
        let kw = self.expect_kw("for")?;
        self.expect_op("(")?;
        self.eat_kw("genvar");
        let var = self.ident()?;
        self.expect_op("=")?;
        let init = self.expr()?;
        self.expect_op(";")?;
        let cond = self.expr()?;
        self.expect_op(";")?;
        let step = Box::new(self.simple_stmt()?);
        self.expect_op(")")?;
        let body = Box::new(self.gen_block()?);
        Ok(ModuleItem::GenFor {
            kw,
            var,
            init,
            cond,
            step,
            body,
        })
    }

    /// A generate block: `begin [: name] items end`, or a single item.
    fn gen_block(&mut self) -> PResult<GenBlock<'a>> {
        let mut label = None;
        if self.is_ident_at(0) && self.is_op_at(1, ":") && self.is_kw_at(2, "begin") {
            label = Some(self.ident()?);
            self.bump();
        }
        if self.eat_kw("begin").is_none() {
            let items = self.module_item()?.into_iter().collect();
            return Ok(GenBlock { label, items });
        }
        if self.eat_op(":").is_some() {
            label = Some(self.ident()?);
        }
        let mut items = Vec::new();
        while self.eat_kw("end").is_none() {
            if self.peek().is_none() {
                return Err(self.unexpected("'end'"));
            }
            if let Some(i) = self.module_item()? {
                items.push(i);
            }
        }
        self.end_label()?;
        Ok(GenBlock { label, items })
    }

    fn modport(&mut self) -> PResult<ModuleItem<'a>> {
        self.expect_kw("modport")?;
        let mut v = Vec::new();
        loop {
            let name = self.ident()?;
            self.expect_op("(")?;
            let mut ports: Vec<(&'a str, Vec<&'a str>)> = Vec::new();
            loop {
                if let Some(d) =
                    self.eat_kw_of(&["input", "output", "inout", "ref", "import", "export"])
                {
                    ports.push((d, Vec::new()));
                } else if self.is_kw("clocking") {
                    return Err(self.not_yet(self.here(), "modport clocking"));
                }
                if self.is_op(".") {
                    return Err(self.not_yet(self.here(), "modport expressions"));
                }
                let n = match self.peek() {
                    Some(Token::Keyword(k @ ("task" | "function"))) => {
                        return Err(self.not_yet(k, "modport subroutine prototypes"));
                    }
                    _ => self.ident()?,
                };
                match ports.last_mut() {
                    Some((_, names)) => names.push(n),
                    None => return Err(self.unexpected("a port direction")),
                }
                if self.eat_op(",").is_none() {
                    break;
                }
            }
            self.expect_op(")")?;
            v.push(Modport { name, ports });
            if self.eat_op(",").is_none() {
                break;
            }
        }
        self.expect_op(";")?;
        Ok(ModuleItem::Modport(v))
    }

    fn subroutine(&mut self) -> PResult<ModuleItem<'a>> {
        self.subroutine_of(false)
    }

    /// A function or task; with `proto`, only its header (`extern`, `pure`).
    fn subroutine_of(&mut self, proto: bool) -> PResult<ModuleItem<'a>> {
        let kw = self.bump().unwrap().text();
        let is_fn = kw == "function";
        let end_kw = if is_fn { "endfunction" } else { "endtask" };
        let lifetime = self.eat_kw_of(&["static", "automatic"]);
        // `C::name` and `new` are names, not types.
        let named_next = |p: &Self, k: usize| {
            p.is_kw_at(k, "new")
                || (p.is_ident_at(k)
                    && (p.is_op_at(k + 1, "(")
                        || p.is_op_at(k + 1, ";")
                        || (p.is_op_at(k + 1, "::")
                            && (p.is_kw_at(k + 2, "new")
                                || (p.is_ident_at(k + 2)
                                    && (p.is_op_at(k + 3, "(") || p.is_op_at(k + 3, ";")))))))
        };
        let ret = if is_fn && !named_next(self, 0) {
            Some(self.data_type_or_implicit()?)
        } else {
            None
        };
        let class = if self.is_ident_at(0) && self.is_op_at(1, "::") {
            let c = self.ident()?;
            self.expect_op("::")?;
            Some(c)
        } else {
            None
        };
        let name = match self.eat_kw("new") {
            Some(n) => n,
            None => self.ident()?,
        };
        let ports = if self.is_op("(") {
            Some(self.tf_ports()?)
        } else {
            None
        };
        self.expect_op(";")?;
        if proto {
            let s = Subroutine {
                kw,
                class,
                proto: true,
                lifetime,
                ret,
                name,
                ports,
                decls: Vec::new(),
                stmts: Vec::new(),
                end: name,
            };
            return Ok(if is_fn {
                ModuleItem::Function(s)
            } else {
                ModuleItem::Task(s)
            });
        }
        let mut decls = Vec::new();
        loop {
            match self.peek() {
                Some(Token::Keyword(k)) if DIRECTIONS.contains(&k) => {
                    self.bump();
                    let mut qualifiers = Vec::new();
                    if let Some(v) = self.eat_kw("var") {
                        qualifiers.push(v);
                    }
                    let decl = self.port_var_decl(qualifiers, None)?;
                    decls.push(ModuleItem::Port(PortDecl { dir: k, decl }));
                }
                _ if self.at_block_decl() => decls.push(self.block_decl()?),
                _ => break,
            }
        }
        let mut stmts = Vec::new();
        let end = loop {
            if let Some(e) = self.eat_kw(end_kw) {
                break e;
            }
            if self.peek().is_none() {
                return Err(self.unexpected(&format!("'{end_kw}'")));
            }
            stmts.push(self.statement()?);
        };
        self.end_label()?;
        let s = Subroutine {
            kw,
            class,
            proto: false,
            lifetime,
            ret,
            name,
            ports,
            decls,
            stmts,
            end,
        };
        Ok(if is_fn {
            ModuleItem::Function(s)
        } else {
            ModuleItem::Task(s)
        })
    }

    /// A class declaration; `kind` is `virtual` or `interface` if given.
    fn class_decl(&mut self, kind: Option<&'a str>) -> PResult<ClassDecl<'a>> {
        let kw = self.expect_kw("class")?;
        self.eat_kw_of(&["static", "automatic"]);
        let name = self.ident()?;
        self.types.insert(name);
        let params = if self.is_op("#") {
            Some(self.param_port_list()?)
        } else {
            None
        };
        let extends = if self.eat_kw("extends").is_some() {
            let base = self.data_type()?;
            let args = if self.is_op("(") {
                self.call_args()?
            } else {
                Vec::new()
            };
            Some((base, args))
        } else {
            None
        };
        let mut implements = Vec::new();
        // `interface class C extends A, B;`
        while self.eat_op(",").is_some() {
            implements.push(self.data_type()?);
        }
        if self.eat_kw("implements").is_some() {
            loop {
                implements.push(self.data_type()?);
                if self.eat_op(",").is_none() {
                    break;
                }
            }
        }
        self.expect_op(";")?;
        let mut items = Vec::new();
        let end = loop {
            if let Some(e) = self.eat_kw("endclass") {
                break e;
            }
            if self.peek().is_none() {
                return Err(self.unexpected("'endclass'"));
            }
            if let Some(i) = self.class_item()? {
                items.push(i);
            }
        };
        self.end_label()?;
        Ok(ClassDecl {
            kw,
            kind,
            name,
            params,
            extends,
            implements,
            items,
            end,
        })
    }

    fn class_item(&mut self) -> PResult<Option<ClassItem<'a>>> {
        const QUALS: &[&str] = &[
            "static", "local", "protected", "rand", "randc", "virtual", "pure", "extern", "const",
            "automatic",
        ];
        if self.eat_op(";").is_some() {
            return Ok(None);
        }
        let mut quals = Vec::new();
        while let Some(q) = self.eat_kw_of(QUALS) {
            quals.push(q);
        }
        let item = match self.peek() {
            Some(Token::Keyword("function" | "task")) => {
                let proto = quals.iter().any(|q| matches!(*q, "pure" | "extern"));
                ClassMember::Item(Box::new(self.subroutine_of(proto)?))
            }
            Some(Token::Keyword("constraint")) => {
                self.bump();
                let name = self.ident()?;
                if self.is_op("{") {
                    ClassMember::Constraint(name, Some(self.constraint_block()?))
                } else {
                    self.expect_op(";")?;
                    ClassMember::Constraint(name, None)
                }
            }
            Some(Token::Keyword("covergroup")) => {
                self.bump();
                let name = self.ident()?;
                self.skip_to_kw("endgroup")?;
                self.end_label()?;
                ClassMember::Covergroup(name)
            }
            Some(Token::Keyword("class")) => ClassMember::Item(Box::new(ModuleItem::Class(self.class_decl(None)?))),
            Some(Token::Keyword("typedef" | "parameter" | "localparam" | "import")) => {
                match self.module_item()? {
                    Some(i) => ClassMember::Item(Box::new(i)),
                    None => return Ok(None),
                }
            }
            _ => {
                // A property; `const` and `rand` etc. were taken as qualifiers.
                let var_quals: Vec<&'a str> = quals
                    .iter()
                    .copied()
                    .filter(|q| VAR_QUALIFIERS.contains(q))
                    .collect();
                ClassMember::Item(Box::new(ModuleItem::Var(self.var_decl(var_quals, None)?)))
            }
        };
        Ok(Some(ClassItem { quals, item }))
    }

    /// `( [dir] [type] name [dims] [= default], ... )`
    fn tf_ports(&mut self) -> PResult<Vec<TfPort<'a>>> {
        self.expect_op("(")?;
        let mut v: Vec<TfPort<'a>> = Vec::new();
        if self.eat_op(")").is_some() {
            return Ok(v);
        }
        loop {
            let mut dir = self.eat_kw_of(DIRECTIONS);
            if dir == Some("ref") || self.is_kw("const") {
                self.eat_kw("const");
                if dir.is_none() {
                    dir = self.eat_kw("ref");
                }
            }
            self.eat_kw("var");
            self.eat_kw_of(&["static", "automatic"]);
            let explicit = self.at_type()
                || self.is_kw("signed")
                || self.is_kw("unsigned")
                || self.is_op("[")
                || (self.is_ident_at(0) && self.is_ident_at(1));
            let ty = if explicit {
                self.data_type_or_implicit()?
            } else {
                match (dir, v.last()) {
                    (None, Some(p)) => p.ty.clone(),
                    _ => DataType::Implicit {
                        signing: None,
                        packed: Vec::new(),
                    },
                }
            };
            let dir = dir.or_else(|| v.last().and_then(|p| p.dir));
            let name = self.ident()?;
            let dims = self.dims()?;
            let default = if self.eat_op("=").is_some() {
                Some(self.expr()?)
            } else {
                None
            };
            v.push(TfPort {
                dir,
                ty,
                name,
                dims,
                default,
            });
            if self.eat_op(",").is_none() {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(v)
    }
}
