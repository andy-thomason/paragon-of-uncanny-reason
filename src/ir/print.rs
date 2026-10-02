//! Text form of the IR, for tests and inspection (`docs/design/20-architecture.md` §2.3).

use super::*;
use std::fmt::{self, Display, Formatter, Write};

impl Design<'_> {
    /// The hierarchical name of a scope: `t.sub.g[1]`.
    pub fn scope_path(&self, s: ScopeId) -> String {
        let sc = &self.scopes[s.0 as usize];
        match sc.parent {
            Some(p) if !sc.name.is_empty() => format!("{}.{}", self.scope_path(p), sc.name),
            Some(p) => self.scope_path(p),
            None => match &self.root_name {
                Some(r) if sc.module.is_some() => format!("{r}.{}", sc.name),
                _ => sc.name.to_string(),
            },
        }
    }

    pub fn var_path(&self, v: VarId) -> String {
        let var = &self.vars[v.0 as usize];
        format!("{}.{}", self.scope_path(var.scope), var.name)
    }

    pub fn type_name(&self, t: TypeId) -> String {
        match &self.types[t.0 as usize] {
            Type::Bits {
                width,
                signed,
                four_state,
                ..
            } => {
                let base = if *four_state { "logic" } else { "bit" };
                let sign = if *signed { " signed" } else { "" };
                if *width == 1 {
                    format!("{base}{sign}")
                } else {
                    format!("{base}[{width}]{sign}")
                }
            }
            Type::Real => "real".into(),
            Type::String => "string".into(),
            Type::Event => "event".into(),
            Type::Unpacked { elem, left, right } => {
                format!("{} [{left}:{right}]", self.type_name(*elem))
            }
            Type::Dynamic { elem } => format!("{} []", self.type_name(*elem)),
            Type::Queue { elem, .. } => format!("{} [$]", self.type_name(*elem)),
            Type::Assoc { elem, .. } => format!("{} [*]", self.type_name(*elem)),
            Type::Struct { .. } => "struct".into(),
        }
    }
}

impl Display for Bits {
    /// `8'hab`, or binary with x/z digits when some bits are unknown.
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if self.has_unknown() {
            write!(f, "{}'b", self.width)?;
            for i in (0..self.width).rev() {
                f.write_char(match self.bit(i) {
                    (false, false) => '0',
                    (true, false) => '1',
                    (false, true) => 'x',
                    (true, true) => 'z',
                })?;
            }
            return Ok(());
        }
        write!(f, "{}'h", self.width)?;
        let mut started = false;
        for w in self.val.iter().rev() {
            if started {
                write!(f, "{w:016x}")?;
            } else if *w != 0 || std::ptr::eq(w, &self.val[0]) {
                write!(f, "{w:x}")?;
                started = true;
            }
        }
        Ok(())
    }
}

struct BodyFmt<'d, 'a> {
    design: &'d Design<'a>,
    body: &'d Body<'a>,
}

impl BodyFmt<'_, '_> {
    fn var(&self, v: VarId) -> String {
        self.design.var_path(v)
    }

    fn vals(vs: &[Val]) -> String {
        vs.iter()
            .map(|v| format!("%{}", v.0))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn target(&self, (b, args): &(BlockId, Vec<Val>)) -> String {
        format!("bb{}({})", b.0, Self::vals(args))
    }

    fn part(p: &Option<Part>) -> String {
        p.map_or(String::new(), |p| format!("[%{} +: {}]", p.lsb.0, p.width))
    }

    fn op(&self, op: &Op) -> String {
        let d = self.design;
        match op {
            Op::Const(b) => format!("const {b}"),
            Op::ConstReal(r) => format!("const {r}"),
            Op::ConstStr(s) => format!("const {s:?}"),
            Op::Load(v) => format!("load {}", self.var(*v)),
            Op::Store { var, part, value } => format!(
                "store {}{} = %{}",
                self.var(*var),
                Self::part(part),
                value.0
            ),
            Op::NbaStore { var, part, value } => format!(
                "nba_store {}{} = %{}",
                self.var(*var),
                Self::part(part),
                value.0
            ),
            Op::LoadElem { var, index } => format!("load {}[%{}]", self.var(*var), index.0),
            Op::StoreElem {
                var,
                index,
                part,
                value,
                nba,
            } => format!(
                "{} {}[%{}]{} = %{}",
                if *nba { "nba_store" } else { "store" },
                self.var(*var),
                index.0,
                Self::part(part),
                value.0
            ),
            Op::LoadSlot(s) => format!("load $s{}", s.0),
            Op::StoreSlot { slot, part, value } => {
                format!("store $s{}{} = %{}", slot.0, Self::part(part), value.0)
            }
            Op::Unary(u, a) => format!("{u:?} %{}", a.0),
            Op::Binary(b, x, y) => format!("{b:?} %{}, %{}", x.0, y.0),
            Op::Select { value, lsb, width } => {
                format!("select %{}[%{} +: {width}]", value.0, lsb.0)
            }
            Op::Concat(v) => format!("concat {{{}}}", Self::vals(v)),
            Op::Repl { value, count } => format!("repl {count}{{%{}}}", value.0),
            Op::Resize { value, extend } => format!("resize.{extend:?} %{}", value.0),
            Op::Mux { cond, then, els } => format!("mux %{} ? %{} : %{}", cond.0, then.0, els.0),
            Op::Convert(v) => format!("convert %{}", v.0),
            Op::Call { func, args } => format!(
                "call {}({})",
                d.funcs[func.0 as usize].name,
                Self::vals(args)
            ),
            Op::Display { kind, format, args } => {
                format!("{kind:?} fmt#{}({})", format.0, Self::vals(args))
            }
            Op::SysFunc { func, args } => format!("{func:?}({})", Self::vals(args)),
            Op::TriggerEvent(v) => format!("trigger {}", self.var(*v)),
            Op::Report { severity, args, .. } => {
                format!("report.{severity:?}({})", Self::vals(args))
            }
            Op::LoadRange { var, start, len } => {
                format!("load {}[%{} +: {len}]", self.var(*var), start.0)
            }
            Op::ArrayElem { value, index } => format!("elem %{}[%{}]", value.0, index.0),
            Op::ArraySlice { value, start, len } => {
                format!("slice %{}[%{} +: {len}]", value.0, start.0)
            }
            Op::StrFunc { func, args } => format!("str.{func:?}({})", Self::vals(args)),
            Op::Sformat { format, args } => {
                format!("sformat fmt#{}({})", format.0, Self::vals(args))
            }
        }
    }

    fn term(&self, t: &Terminator) -> String {
        match t {
            Terminator::Jump(b, args) => format!("jump {}", self.target(&(*b, args.clone()))),
            Terminator::Branch { cond, then, els } => {
                format!(
                    "branch %{}, {}, {}",
                    cond.0,
                    self.target(then),
                    self.target(els)
                )
            }
            Terminator::Switch {
                value,
                cases,
                default,
            } => {
                let c: Vec<String> = cases
                    .iter()
                    .map(|(b, t)| format!("{b} => bb{}", t.0))
                    .collect();
                format!(
                    "switch %{} [{}], default bb{}",
                    value.0,
                    c.join(", "),
                    default.0
                )
            }
            Terminator::Return(None) => "return".into(),
            Terminator::Return(Some(v)) => format!("return %{}", v.0),
            Terminator::Suspend { wait, resume } => {
                format!("suspend {} -> bb{}", self.wait(wait), resume.0)
            }
            Terminator::Fork {
                children,
                join,
                resume,
            } => {
                let c: Vec<String> = children.iter().map(|b| format!("bb{}", b.0)).collect();
                format!("fork [{}] join.{join:?} -> bb{}", c.join(", "), resume.0)
            }
            Terminator::EndThread => "end_thread".into(),
            Terminator::Finish(k) => format!("finish {k:?}"),
            Terminator::Unreachable => "unreachable".into(),
        }
    }

    fn wait(&self, w: &Wait) -> String {
        match w {
            Wait::Delay(v) => format!("Delay(%{})", v.0),
            Wait::Inactive => "Inactive".into(),
            Wait::AnyChange(vs) => {
                format!(
                    "AnyChange[{}]",
                    vs.iter()
                        .map(|v| self.var(*v))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            Wait::Edge(es) => format!(
                "Edge[{}]",
                es.iter()
                    .map(|(v, e)| format!("({}, {e:?})", self.var(*v)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Wait::Event(v) => format!("Event({})", self.var(*v)),
            Wait::Children => "Children".into(),
        }
    }
}

impl Display for BodyFmt<'_, '_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for (i, s) in self.body.slots.iter().enumerate() {
            writeln!(f, "    $s{i}: {}", self.design.type_name(*s))?;
        }
        for (i, b) in self.body.blocks.iter().enumerate() {
            write!(f, "  bb{i}")?;
            if !b.params.is_empty() {
                write!(f, "({})", Self::vals(&b.params))?;
            }
            writeln!(f, ":")?;
            for inst in &b.insts {
                let op = self.op(&inst.op);
                match inst.dst {
                    Some(d) => {
                        let ty = self.design.type_name(self.body.vals[d.0 as usize]);
                        writeln!(f, "    %{} = {op:<40} : {ty}", d.0)?
                    }
                    None => writeln!(f, "    {op}")?,
                }
            }
            writeln!(f, "    {}", self.term(&b.term))?;
        }
        Ok(())
    }
}

impl Display for Design<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for (i, v) in self.vars.iter().enumerate() {
            write!(
                f,
                "var {}: {}",
                self.var_path(VarId(i as u32)),
                self.type_name(v.ty)
            )?;
            if let VarKind::Net(r) = v.kind {
                write!(f, " net.{r:?}")?;
            }
            if let Some(init) = &v.init {
                write!(f, " = {init}")?;
            }
            writeln!(f)?;
        }
        for (i, fmt_) in self.formats.iter().enumerate() {
            write!(f, "fmt#{i} = \"")?;
            for p in &fmt_.pieces {
                match p {
                    FormatPiece::Text(t) => write!(f, "{}", t.escape_debug())?,
                    FormatPiece::Conv {
                        spec,
                        width,
                        zero_pad,
                        left,
                    } => {
                        write!(f, "%")?;
                        if *left {
                            write!(f, "-")?;
                        }
                        if *zero_pad {
                            write!(f, "0")?;
                        }
                        if let Some(w) = width {
                            write!(f, "{w}")?;
                        }
                        write!(f, "{spec}")?;
                    }
                }
            }
            writeln!(f, "\"")?;
        }
        for func in &self.funcs {
            writeln!(f, "func {}.{}:", self.scope_path(func.scope), func.name)?;
            write!(
                f,
                "{}",
                BodyFmt {
                    design: self,
                    body: &func.body
                }
            )?;
        }
        for p in &self.procs {
            writeln!(f, "proc {:?} in {}:", p.kind, self.scope_path(p.scope))?;
            write!(
                f,
                "{}",
                BodyFmt {
                    design: self,
                    body: &p.body
                }
            )?;
        }
        Ok(())
    }
}
