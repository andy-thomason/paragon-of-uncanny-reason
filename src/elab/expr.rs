//! Expressions: types, the width rules, selects and assignments.
//!
//! IEEE 1800-2023 §11.6–11.8 size expressions in two steps, and so does this:
//!
//! 1. [`Elab::self_type`] works out an expression's self-determined width
//!    and signedness, bottom up.
//! 2. [`Elab::lower`] lowers it with the *propagated* width and signedness
//!    ([`Want`]) pushed down to every context-determined operand. Each leaf
//!    is resized to it, sign-extended only if the propagated type is signed.
//!    Self-determined operands (shift amounts, concatenation parts,
//!    comparison operands, conditions) are lowered with their own types.

use super::build::Builder;
use super::types::{Base, Ty, UDim, range_len};
use super::{EResult, Elab, Stop, Sym};
use crate::ast::{self, Arg, Expr};
use crate::bits::{Literal, parse_literal};
use crate::eval::{Value, eval_const};
use crate::ir::*;
use std::collections::HashMap;

/// A body being lowered, with its local names.
pub(crate) struct Cx<'a> {
    pub(crate) b: Builder<'a>,
    pub(crate) locals: Vec<HashMap<&'a str, Sym<'a>>>,
    /// The return-value slot of the function being lowered.
    pub(crate) ret: Option<(SlotId, Ty<'a>)>,
    pub(crate) is_func: bool,
    /// Lowering a constant expression: reading a variable is an error.
    pub(crate) const_mode: bool,
    /// The innermost named block, which owns static variables declared in it.
    pub(crate) block_scope: Option<ScopeId>,
    /// For a subroutine with output arguments: the block every return goes
    /// to, which collects the results.
    pub(crate) exit: Option<BlockId>,
    /// While lowering a queue index: the value of `$` (its last index).
    pub(crate) dollar: Option<Val>,
}

impl<'a> Cx<'a> {
    pub(crate) fn new(const_mode: bool) -> Self {
        Cx {
            b: Builder::new(),
            locals: vec![HashMap::new()],
            ret: None,
            is_func: false,
            const_mode,
            block_scope: None,
            exit: None,
            dollar: None,
        }
    }
}

/// A self-determined type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum STy {
    Bits { w: u32, s: bool, f: bool },
    Real,
    Str,
    /// A class handle; `None` for `null`.
    Class(Option<ClassId>),
}

impl STy {
    pub(crate) fn ty<'a>(&self) -> Ty<'a> {
        match *self {
            STy::Bits { w, s, f } => Ty::bits(w, s, f),
            STy::Real => Ty::scalar(Base::Real),
            STy::Str => Ty::scalar(Base::Str),
            STy::Class(Some(c)) => Ty::scalar(Base::Class(c)),
            STy::Class(None) => Ty::scalar(Base::Void),
        }
    }

    fn width(&self) -> u32 {
        match self {
            STy::Bits { w, .. } => *w,
            _ => 64,
        }
    }

    fn signed(&self) -> bool {
        match self {
            STy::Bits { s, .. } => *s,
            STy::Real => true,
            STy::Str | STy::Class(_) => false,
        }
    }

    fn four(&self) -> bool {
        matches!(self, STy::Bits { f: true, .. })
    }
}

pub(crate) fn sty_of(t: &Ty) -> STy {
    match &t.base {
        _ if !t.unpacked.is_empty() => STy::Bits {
            w: t.width(),
            s: false,
            f: t.four_state(),
        },
        Base::Real => STy::Real,
        Base::Str => STy::Str,
        Base::Class(c) => STy::Class(Some(*c)),
        _ => STy::Bits {
            w: t.width().max(1),
            s: t.signed,
            f: t.four_state(),
        },
    }
}

/// The width and signedness pushed down to context-determined operands.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Want {
    pub w: u32,
    pub s: bool,
    pub f: bool,
}

/// Where a selected value lives.
#[derive(Clone, Debug)]
pub(crate) enum Root {
    Var(VarId),
    Slot(SlotId),
    Const(Value),
    /// Property `index` (of type `ty`) of the object `obj` refers to.
    Field { obj: Val, index: u32, ty: TypeId },
}

/// A (possibly selected) reference to storage: `x`, `mem[i]`, `v[7:4]`, `s.f`.
#[derive(Clone, Debug)]
pub(crate) struct Path<'a> {
    pub root: Root,
    /// The type of what is loaded: the whole variable, or one array element.
    pub base: Ty<'a>,
    /// The linear element index, for an unpacked array.
    pub elem: Option<Val>,
    /// The type of the selected part.
    pub ty: Ty<'a>,
    /// Bit offset of the selected part within `base`, if it is a part.
    pub lsb: Option<Val>,
    pub width: u32,
    pub at: &'a str,
}

const BINARY: &[(&str, BinOp)] = &[
    ("+", BinOp::Add),
    ("-", BinOp::Sub),
    ("*", BinOp::Mul),
    ("/", BinOp::Div),
    ("%", BinOp::Mod),
    ("&", BinOp::And),
    ("|", BinOp::Or),
    ("^", BinOp::Xor),
    ("~^", BinOp::Xnor),
    ("^~", BinOp::Xnor),
];

const COMPARE: &[(&str, BinOp)] = &[
    ("==", BinOp::Eq),
    ("!=", BinOp::Ne),
    ("===", BinOp::CaseEq),
    ("!==", BinOp::CaseNe),
    ("==?", BinOp::WildEq),
    ("!=?", BinOp::WildNe),
    ("<", BinOp::Lt),
    ("<=", BinOp::Le),
    (">", BinOp::Gt),
    (">=", BinOp::Ge),
];

fn lookup_op(table: &[(&str, BinOp)], op: &str) -> Option<BinOp> {
    table.iter().find(|(o, _)| *o == op).map(|(_, b)| *b)
}

/// The number of elements of a fixed-size unpacked array type; `None` if a
/// dimension is dynamic.
pub(crate) fn fixed_count(t: &Ty) -> Option<u32> {
    t.unpacked
        .iter()
        .map(|d| match d {
            UDim::Fixed(l, r) => Some(range_len((*l, *r))),
            _ => None,
        })
        .product()
}

/// The type of an array method's value; `None` for one without a value.
fn array_method_type(t: &Ty, name: &str) -> Option<STy> {
    let mut elem = t.clone();
    elem.unpacked.remove(0);
    Some(match name {
        "size" | "num" => STy::Bits {
            w: 32,
            s: true,
            f: false,
        },
        "sum" | "product" | "and" | "or" | "xor" | "pop_front" | "pop_back" => sty_of(&elem),
        "min" | "max" => {
            let mut q = elem;
            q.unpacked.insert(0, UDim::Queue);
            sty_of(&q)
        }
        _ => return None,
    })
}

/// The type of a string method's value; `None` for one that has none.
fn str_method_type(name: &str) -> Option<STy> {
    let int = STy::Bits {
        w: 32,
        s: true,
        f: false,
    };
    Some(match name {
        "len" | "compare" | "icompare" | "atoi" | "atohex" | "atooct" | "atobin" => int,
        "getc" => STy::Bits {
            w: 8,
            s: false,
            f: false,
        },
        "toupper" | "tolower" | "substr" => STy::Str,
        "atoreal" => STy::Real,
        _ => return None,
    })
}

fn literal_sty(l: &Literal) -> STy {
    match l {
        Literal::Bits { bits, signed, .. } => STy::Bits {
            w: bits.width,
            s: *signed,
            f: bits.has_unknown(),
        },
        Literal::Fill(b) => STy::Bits {
            w: 1,
            s: false,
            f: b.has_unknown(),
        },
        Literal::Real(_) | Literal::Time(..) => STy::Real,
    }
}

/// Does lowering this expression have side effects, or could it fail (a
/// member of a null handle), so it can't be speculated?
fn has_effects(e: &Expr) -> bool {
    match e {
        Expr::Member { .. } => true,
        Expr::Call { .. } | Expr::IncDec { .. } | Expr::Assign { .. } | Expr::New { .. } => true,
        Expr::SysCall { name, .. } => matches!(
            *name,
            "$random" | "$urandom" | "$urandom_range" | "$fgetc" | "$fopen"
        ),
        Expr::Unary { arg, .. } => has_effects(arg),
        Expr::Binary { lhs, rhs, .. } => has_effects(lhs) || has_effects(rhs),
        Expr::Cond {
            cond, then, els, ..
        } => has_effects(cond) || has_effects(then) || has_effects(els),
        Expr::Concat(v) => v.iter().any(has_effects),
        Expr::Repl { items, .. } => items.iter().any(has_effects),
        Expr::Index { base, index } => has_effects(base) || has_effects(index),
        _ => false,
    }
}

impl<'a, 't> Elab<'a, 't> {
    pub(crate) fn lookup_cx(&self, cx: Option<&Cx<'a>>, name: &str) -> Option<Sym<'a>> {
        if let Some(cx) = cx {
            for scope in cx.locals.iter().rev() {
                if let Some(s) = scope.get(name) {
                    return Some(s.clone());
                }
            }
        }
        self.lookup(name)
    }

    fn bt(&mut self, w: u32, s: bool, f: bool) -> TypeId {
        self.bits_type(w.max(1), s, f)
    }

    fn want_type(&mut self, want: Want) -> TypeId {
        self.bt(want.w, want.s, want.f)
    }

    // ------------------------------------------------------------ constants

    /// Evaluate a constant expression, converted to `ty` if given.
    pub(crate) fn const_value(&mut self, e: &Expr<'a>, ty: Option<&Ty<'a>>) -> EResult<Value> {
        self.last_at = e.at();
        let mut cx = Cx::new(true);
        self.nonconst = None;
        let r = match ty {
            Some(t) => self.lower_to(&mut cx, e, t),
            None => self.lower_self(&mut cx, e).map(|(v, _)| v),
        };
        let v = match r {
            Ok(v) => v,
            Err(Stop) => {
                if let Some(name) = self.nonconst.take() {
                    return Err(self.error(
                        name,
                        format!("Expecting expression to be constant, but variable isn't const: '{name}'"),
                    ));
                }
                return Err(Stop);
            }
        };
        cx.b.terminate(Terminator::Return(Some(v)));
        let (body, _) = cx.b.finish(Terminator::Unreachable);
        match eval_const(&body, &self.d) {
            Ok(v) => Ok(v),
            Err(crate::eval::NotConst::Impure(_)) => Err(self.error(
                e.at(),
                "Expecting expression to be constant, but it reads a variable (perhaps inside a function)",
            )),
            Err(_) => Err(self.error(e.at(), "Expecting expression to be constant")),
        }
    }

    /// Like [`const_value`](Self::const_value), but quietly `None` if the
    /// expression is not constant.
    pub(crate) fn try_const(&mut self, e: &Expr<'a>, ty: &Ty<'a>) -> Option<Value> {
        let n = self.diags.len();
        let r = self.const_value(e, Some(ty)).ok();
        self.diags.truncate(n);
        r
    }

    pub(crate) fn const_int(&mut self, e: &Expr<'a>) -> EResult<i64> {
        let st = self.self_type(e)?;
        let v = self.const_value(e, None)?;
        match v {
            Value::Bits(b) => match b.to_i64(st.signed()) {
                Some(i) => Ok(i),
                None => Err(self.error(e.at(), "Expecting a known integer constant")),
            },
            Value::Real(r) => Ok(r.round() as i64),
            Value::Str(_) | Value::Array(_) | Value::Obj(_) => {
                Err(self.error(e.at(), "Expecting an integer constant"))
            }
        }
    }

    pub(crate) fn const_bits(&mut self, e: &Expr<'a>, ty: &Ty<'a>) -> EResult<Bits> {
        match self.const_value(e, Some(ty))? {
            Value::Bits(b) => Ok(b),
            _ => Err(self.error(e.at(), "Expecting an integral constant")),
        }
    }

    // ------------------------------------------------------------ self-determined types

    pub(crate) fn self_type(&mut self, e: &Expr<'a>) -> EResult<STy> {
        self.self_type_cx(None, e)
    }

    pub(crate) fn self_type_cx(&mut self, cx: Option<&Cx<'a>>, e: &Expr<'a>) -> EResult<STy> {
        let bit = |f: bool| STy::Bits { w: 1, s: false, f };
        Ok(match e {
            Expr::Number(n) => match parse_literal(n) {
                Ok(l) => literal_sty(&l),
                Err(m) => return Err(self.error(n, m)),
            },
            Expr::Str(s) => STy::Bits {
                w: (super::decode_string_bytes(super::str_body(s)).len() as u32 * 8).max(8),
                s: false,
                f: false,
            },
            Expr::Member { base, name } if self.array_type_opt(cx, base).is_some() => {
                let t = self.array_type_opt(cx, base).unwrap();
                match array_method_type(&t, name) {
                    Some(st) => st,
                    None => return Err(self.not_yet(name, &format!("array method {name}"))),
                }
            }
            Expr::Call { func, .. }
                if matches!(&**func, Expr::Member { base, .. } if self.array_type_opt(cx, base).is_some()) =>
            {
                let Expr::Member { base, name } = &**func else {
                    unreachable!()
                };
                let t = self.array_type_opt(cx, base).unwrap();
                match array_method_type(&t, name) {
                    Some(st) => st,
                    None => {
                        return Err(self.error(name, format!("Array method '{name}' has no value")));
                    }
                }
            }
            Expr::Member { base, name } if self.is_str(cx, base) => match str_method_type(name) {
                Some(t) => t,
                None => return Err(self.not_yet(name, &format!("string method {name}"))),
            },
            Expr::Call { func, .. } if matches!(&**func, Expr::Member { base, .. } if self.is_str(cx, base)) =>
            {
                let Expr::Member { name, .. } = &**func else {
                    unreachable!()
                };
                match str_method_type(name) {
                    Some(t) => t,
                    None => {
                        return Err(
                            self.error(name, format!("String method '{name}' has no value"))
                        );
                    }
                }
            }
            Expr::Ident(_)
            | Expr::Member { .. }
            | Expr::Index { .. }
            | Expr::Slice { .. }
            | Expr::Scoped { .. } => match self.path_type(cx, e)? {
                Some((ty, selected)) => {
                    let st = sty_of(&ty);
                    match st {
                        STy::Bits { w, f, .. } if selected => STy::Bits { w, s: false, f },
                        st => st,
                    }
                }
                None => return Err(self.not_yet(e.at(), "this kind of name")),
            },
            Expr::Unary { op, arg } => {
                let a = self.self_type_cx(cx, arg)?;
                match *op {
                    "+" | "-" | "~" => a,
                    _ => bit(a.four()),
                }
            }
            Expr::IncDec { arg, .. } => self.self_type_cx(cx, arg)?,
            Expr::Binary { op, lhs, rhs } => {
                let (a, b) = (self.self_type_cx(cx, lhs)?, self.self_type_cx(cx, rhs)?);
                match *op {
                    "<<" | ">>" | "<<<" | ">>>" | "**" => a,
                    "&&" | "||" | "->" | "<->" => bit(a.four() || b.four()),
                    "===" | "!==" => bit(false),
                    _ if lookup_op(COMPARE, op).is_some() => bit(a.four() || b.four()),
                    _ if a == STy::Real || b == STy::Real => STy::Real,
                    _ => STy::Bits {
                        w: a.width().max(b.width()),
                        s: a.signed() && b.signed(),
                        f: a.four() || b.four(),
                    },
                }
            }
            Expr::Cond { then, els, .. } => {
                let (a, b) = (self.self_type_cx(cx, then)?, self.self_type_cx(cx, els)?);
                if a == STy::Str || b == STy::Str {
                    STy::Str
                } else if a == STy::Real || b == STy::Real {
                    STy::Real
                } else {
                    STy::Bits {
                        w: a.width().max(b.width()),
                        s: a.signed() && b.signed(),
                        f: true,
                    }
                }
            }
            Expr::Inside { .. } => bit(true),
            Expr::Concat(items) => {
                let mut w = 0;
                let mut f = false;
                for i in items {
                    if self.is_zero_repl(i) {
                        continue;
                    }
                    let t = self.self_type_cx(cx, i)?;
                    if t == STy::Str {
                        return Ok(STy::Str);
                    }
                    w += t.width();
                    f |= t.four();
                }
                STy::Bits { w, s: false, f }
            }
            Expr::Repl { count, items } => {
                let inner = self.self_type_cx(cx, &Expr::Concat(items.clone()))?;
                if inner == STy::Str {
                    return Ok(STy::Str);
                }
                let n = self.const_int(count)?;
                STy::Bits {
                    w: inner.width() * n.max(0) as u32,
                    s: false,
                    f: inner.four(),
                }
            }
            Expr::MinTypMax(v) => self.self_type_cx(cx, &v[1])?,
            Expr::Call { func, .. } if self.process_call(cx, func).is_some() => {
                match self.process_call(cx, func).unwrap() {
                    "status" => STy::Bits {
                        w: 32,
                        s: true,
                        f: false,
                    },
                    "get_randstate" => STy::Str,
                    "self" => STy::Class(self.d.process_class),
                    n => return Err(self.error(n, format!("process::{n} has no value"))),
                }
            }
            Expr::Call { func, .. } if self.method_of(cx, func).is_some() => {
                let f = self.method_of(cx, func).unwrap()?;
                match self.sig(f)?.ret {
                    Some(t) => sty_of(&t),
                    None => {
                        return Err(self.error(func.at(), "Void function used in an expression"));
                    }
                }
            }
            Expr::Call { func, .. } => {
                let f = self.resolve_func(cx, func)?;
                match self.sig(f)?.ret {
                    Some(t) => sty_of(&t),
                    None => {
                        return Err(self.error(func.at(), "Void function used in an expression"));
                    }
                }
            }
            Expr::SysCall { name, args } => self.sys_type(cx, name, args)?,
            Expr::Cast { ty, expr } => {
                let inner = self.self_type_cx(cx, expr)?;
                match &**ty {
                    Expr::Keyword("signed") => STy::Bits {
                        w: inner.width(),
                        s: true,
                        f: inner.four(),
                    },
                    Expr::Keyword("unsigned") => STy::Bits {
                        w: inner.width(),
                        s: false,
                        f: inner.four(),
                    },
                    Expr::Keyword(k) => return Err(self.not_yet(k, "this cast")),
                    Expr::Type(t) => sty_of(&self.resolve_type(t)?),
                    Expr::Ident(n) if matches!(self.lookup_cx(cx, n), Some(Sym::Type(_))) => {
                        let Some(Sym::Type(t)) = self.lookup_cx(cx, n) else {
                            unreachable!()
                        };
                        sty_of(&t)
                    }
                    size => {
                        let w = self.const_int(size)?;
                        STy::Bits {
                            w: w.max(1) as u32,
                            s: inner.signed(),
                            f: inner.four(),
                        }
                    }
                }
            }
            Expr::Keyword("null") => STy::Class(None),
            Expr::New { .. } => STy::Class(None),
            Expr::Keyword("$") if cx.is_some_and(|c| c.dollar.is_some()) => STy::Bits {
                w: 32,
                s: true,
                f: false,
            },
            Expr::Keyword("$") => return Err(self.not_yet(e.at(), "$ outside a queue or range")),
            Expr::Pattern { .. } => {
                return Err(self.not_yet(e.at(), "assignment patterns without a target type"));
            }
            other => return Err(self.not_yet(other.at(), "this expression")),
        })
    }

    fn sys_type(&mut self, cx: Option<&Cx<'a>>, name: &'a str, args: &[Arg<'a>]) -> EResult<STy> {
        let int = STy::Bits {
            w: 32,
            s: true,
            f: false,
        };
        Ok(match name {
            "$time" => STy::Bits {
                w: 64,
                s: false,
                f: true,
            },
            "$stime" => STy::Bits {
                w: 32,
                s: false,
                f: true,
            },
            "$realtime" | "$itor" | "$bitstoreal" | "$sqrt" | "$ln" | "$log10" | "$exp"
            | "$pow" | "$floor" | "$ceil" => STy::Real,
            "$random" | "$clog2" | "$bits" | "$size" | "$countones" | "$rtoi" | "$left"
            | "$right" | "$low" | "$high" | "$dimensions" | "$increment" | "$test$plusargs" => int,
            "$urandom" | "$urandom_range" => STy::Bits {
                w: 32,
                s: false,
                f: false,
            },
            "$sformatf" | "$psprintf" => STy::Str,
            "$cast" => STy::Bits {
                w: 1,
                s: false,
                f: false,
            },
            "$realtobits" => STy::Bits {
                w: 64,
                s: false,
                f: false,
            },
            "$isunknown" | "$onehot" | "$onehot0" => STy::Bits {
                w: 1,
                s: false,
                f: false,
            },
            "$signed" | "$unsigned" => {
                let Some(Arg::Ordered(Some(a))) = args.first() else {
                    return Err(self.error(name, format!("{name} needs one argument")));
                };
                let t = self.self_type_cx(cx, a)?;
                STy::Bits {
                    w: t.width(),
                    s: name == "$signed",
                    f: t.four(),
                }
            }
            _ => return Err(self.not_yet(name, &format!("system function {name}"))),
        })
    }

    pub(crate) fn resolve_func(&mut self, cx: Option<&Cx<'a>>, func: &Expr<'a>) -> EResult<FuncId> {
        let sym = match func {
            Expr::Ident(n) => self.lookup_cx(cx, n),
            Expr::Scoped { scope, name } => match &**scope {
                Expr::Ident(p) => self.lookup_scoped(p, name),
                _ => None,
            },
            // A task or function of an instance or interface: `ifc.f()`.
            Expr::Member { base, name } => match self.hier_scope(cx, base) {
                Some(s) => self.lookup_child(s, name),
                None => return Err(self.not_yet(func.at(), "method calls")),
            },
            _ => return Err(self.not_yet(func.at(), "method calls")),
        };
        match sym {
            Some(Sym::Func(f)) => Ok(f),
            _ => Err(self.error(
                func.at(),
                format!("Can't find definition of task/function: '{}'", func.at()),
            )),
        }
    }

    // ------------------------------------------------------------ paths

    /// The type of a name or select, without lowering it. `None` if `e` is
    /// not a name or select. The flag is true when the value is a part select.
    fn path_type(&mut self, cx: Option<&Cx<'a>>, e: &Expr<'a>) -> EResult<Option<(Ty<'a>, bool)>> {
        Ok(match e {
            Expr::Keyword("this") => match cx.and_then(|c| self.this_class(c)) {
                Some(c) => Some((Self::class_ty(c), false)),
                None => return Err(self.error(e.at(), "'this' used outside a non-static method")),
            },
            Expr::Ident(n) => match self.lookup_cx(cx, n) {
                Some(Sym::Var(_, t) | Sym::Slot(_, t) | Sym::Param(_, t) | Sym::Field(_, t)) => {
                    Some((t, false))
                }
                Some(Sym::Scope(_)) => {
                    return Err(
                        self.error(n, format!("'{n}' is an instance or block, not a value"))
                    );
                }
                Some(_) => return Err(self.error(n, format!("'{n}' is not a value"))),
                None => {
                    if cx.is_some_and(|c| c.const_mode) || cx.is_none() {
                        // Could be a variable declared later; report it once at lowering.
                    }
                    return Err(self.error(n, format!("Can't find definition of variable: '{n}'")));
                }
            },
            Expr::Scoped { scope, name } if self.scope_class(scope).is_some() => {
                let c = self.scope_class(scope).unwrap()?;
                match self.class_member(c, name)? {
                    Some(Sym::Var(_, t) | Sym::Param(_, t) | Sym::Field(_, t)) => Some((t, false)),
                    _ => return Err(self.error(name, format!("Can't find definition of '{name}' in class"))),
                }
            }
            Expr::Scoped { scope, name } => match &**scope {
                Expr::Ident(p) | Expr::Keyword(p) => match self.scoped_sym(p, name) {
                    Some(Sym::Var(_, t) | Sym::Param(_, t)) => Some((t, false)),
                    _ => {
                        return Err(
                            self.error(name, format!("Can't find definition of '{p}::{name}'"))
                        );
                    }
                },
                _ => return Err(self.not_yet(e.at(), "nested scopes")),
            },
            Expr::Member { base, name } if self.handle_class(cx, base).is_some() => {
                let c = self.handle_class(cx, base).unwrap();
                match self.class_member(c, name)? {
                    Some(Sym::Var(_, t) | Sym::Param(_, t) | Sym::Field(_, t)) => Some((t, false)),
                    _ => return Ok(None),
                }
            }
            Expr::Member { base, name } => {
                if let Some(s) = self.hier_scope(cx, base) {
                    return match self.lookup_child(s, name) {
                        Some(Sym::Var(_, t) | Sym::Param(_, t)) => Ok(Some((t, false))),
                        _ => Err(self.error(
                            name,
                            format!("Can't find definition of '{name}' in dotted reference"),
                        )),
                    };
                }
                let Some((bt, _)) = self.path_type(cx, base)? else {
                    return Ok(None);
                };
                if !matches!(bt.base, Base::Struct { .. }) || !bt.packed.is_empty() {
                    return Err(self.not_yet(name, "built-in methods and properties"));
                }
                let (ft, _) = self.member(&bt, name)?;
                Some((ft, false))
            }
            Expr::Index { base, .. } => {
                let Some((bt, _)) = self.path_type(cx, base)? else {
                    return Ok(None);
                };
                if bt.base == Base::Str && bt.unpacked.is_empty() {
                    // `s[i]`: a byte of the string.
                    return Ok(Some((Ty::bits(8, false, false), true)));
                }
                if !bt.unpacked.is_empty() {
                    let mut t = bt.clone();
                    t.unpacked.remove(0);
                    Some((t, false))
                } else if !bt.packed.is_empty() {
                    Some((bt.packed_element(), true))
                } else if bt.width() > 1 {
                    Some((Ty::bits(1, false, bt.four_state()), true))
                } else {
                    return Err(self.error(e.at(), "Illegal bit select of a scalar"));
                }
            }
            Expr::Slice {
                base,
                op,
                left,
                right,
            } => {
                let Some((bt, _)) = self.path_type(cx, base)? else {
                    return Ok(None);
                };
                if !bt.unpacked.is_empty() {
                    return Err(self.not_yet(e.at(), "slices of unpacked arrays"));
                }
                let elem = if bt.packed.is_empty() {
                    Ty::bits(1, false, bt.four_state())
                } else {
                    bt.packed_element()
                };
                let n = match *op {
                    ":" => {
                        (self.const_int(left)? - self.const_int(right)?).unsigned_abs() as u32 + 1
                    }
                    _ => self.const_int(right)?.max(1) as u32,
                };
                let mut t = elem;
                t.packed.insert(0, (n as i64 - 1, 0));
                t.signed = false;
                Some((t, true))
            }
            _ => None,
        })
    }

    /// The class named by the scope of `C::x` or `C#(8)::x`, if a class.
    pub(crate) fn scope_class(&mut self, scope: &Expr<'a>) -> Option<EResult<ClassId>> {
        match scope {
            Expr::Ident(p) => match self.lookup(p) {
                Some(Sym::ClassDef(d)) => Some(self.specialise(d, None, p)),
                Some(Sym::Type(t)) => Self::class_of(&t).map(Ok),
                None if *p == "process" => Some(Ok(self.process_class())),
                _ => None,
            },
            Expr::Type(t) => {
                let n = self.diags.len();
                match self.resolve_type(t) {
                    Ok(t) => Self::class_of(&t).map(Ok),
                    Err(_) => {
                        self.diags.truncate(n);
                        None
                    }
                }
            }
            Expr::Scoped { scope: s, name } => {
                // `pkg::C::x`
                if let Expr::Ident(p) = &**s
                    && let Some(Sym::ClassDef(d)) = self.lookup_scoped(p, name)
                {
                    return Some(self.specialise(d, None, name));
                }
                None
            }
            _ => None,
        }
    }

    /// The method name of a call on the built-in `process` class.
    fn process_call(&mut self, cx: Option<&Cx<'a>>, func: &Expr<'a>) -> Option<&'a str> {
        let (c, name) = match func {
            Expr::Member { base, name } => (self.handle_class(cx, base)?, *name),
            Expr::Scoped { scope, name } => (self.scope_class(scope)?.ok()?, *name),
            _ => return None,
        };
        (Some(c) == self.d.process_class).then_some(name)
    }

    /// The method a call names, if it is a method of a class handle or
    /// class scope: `obj.f`, `super.f`, `C::f`.
    pub(crate) fn method_of(&mut self, cx: Option<&Cx<'a>>, func: &Expr<'a>) -> Option<EResult<FuncId>> {
        let (c, name) = match func {
            Expr::Member { base, name } => (self.handle_class(cx, base)?, *name),
            Expr::Scoped { scope, name } => match self.scope_class(scope)? {
                Ok(c) => (c, *name),
                Err(e) => return Some(Err(e)),
            },
            _ => return None,
        };
        if Some(c) == self.d.process_class {
            return None;
        }
        if let Err(e) = self.ensure_class(c) {
            return Some(Err(e));
        }
        let info = &self.classes[c.0 as usize];
        let f = if name == "new" {
            info.ctor
        } else {
            info.methods.get(name).map(|m| m.func)
        };
        match f {
            Some(f) => Some(Ok(f)),
            None if matches!(name, "randomize" | "srandom") => Some(Err(self.not_yet(name, "randomization"))),
            None => Some(Err(self.error(name, format!("Class method '{name}' not found")))),
        }
    }

    /// The class of a handle-valued `e` (`super` and `this` included).
    pub(crate) fn handle_class(&mut self, cx: Option<&Cx<'a>>, e: &Expr<'a>) -> Option<ClassId> {
        match e {
            Expr::Keyword("super") => {
                let c = self.this_class(cx?)?;
                self.classes[c.0 as usize].base
            }
            Expr::Keyword("this") => self.this_class(cx?),
            _ => {
                let n = self.diags.len();
                let r = self.self_type_cx(cx, e).ok();
                self.diags.truncate(n);
                match r {
                    Some(STy::Class(Some(c))) => Some(c),
                    _ => None,
                }
            }
        }
    }

    /// `{0{x}}`, a zero replication.
    fn is_zero_repl(&mut self, e: &Expr<'a>) -> bool {
        let Expr::Repl { count, .. } = e else {
            return false;
        };
        let n = self.diags.len();
        let z = self.const_int(count).is_ok_and(|c| c == 0);
        self.diags.truncate(n);
        z
    }

    /// `pkg::name` or `$unit::name`.
    fn scoped_sym(&self, p: &str, name: &str) -> Option<Sym<'a>> {
        if p == "$unit" {
            return self.lookup_child(self.unit_scope, name);
        }
        self.lookup_scoped(p, name)
    }

    /// If `e` names an instance or generate block, that scope.
    pub(crate) fn hier_scope(&mut self, cx: Option<&Cx<'a>>, e: &Expr<'a>) -> Option<ScopeId> {
        match e {
            Expr::Ident(n) => match self.lookup_cx(cx, n) {
                Some(Sym::Scope(s)) => Some(s),
                Some(_) => None,
                None => self.lookup_upward_scope(n),
            },
            Expr::Member { base, name } => {
                let s = self.hier_scope(cx, base)?;
                match self.lookup_child(s, name)? {
                    Sym::Scope(s) => Some(s),
                    _ => None,
                }
            }
            // g[2] for a block of a generate loop: its scope is named `g[2]`.
            Expr::Index { base, index } => {
                let (parent, base_name) = match &**base {
                    Expr::Ident(n) => (None, *n),
                    Expr::Member { base, name } => (Some(self.hier_scope(cx, base)?), *name),
                    _ => return None,
                };
                let n = self.diags.len();
                let i = self.const_int(index);
                self.diags.truncate(n);
                let name = format!("{base_name}[{}]", i.ok()?);
                let sym = match parent {
                    Some(p) => self.lookup_child(p, &name),
                    None => self.lookup_cx(cx, &name),
                };
                match sym? {
                    Sym::Scope(s) => Some(s),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// The first part of a hierarchical name not found locally: a child
    /// scope of an enclosing instance, an enclosing instance, or a top.
    fn lookup_upward_scope(&self, n: &str) -> Option<ScopeId> {
        let mut s = self.d.scopes[self.cur.0 as usize].parent;
        while let Some(id) = s {
            if let Some(Sym::Scope(c)) = self.lookup_child(id, n) {
                return Some(c);
            }
            s = self.d.scopes[id.0 as usize].parent;
        }
        self.lookup_upward(n)
    }

    /// A member of a packed struct: its type and bit offset.
    fn member(&mut self, t: &Ty<'a>, name: &'a str) -> EResult<(Ty<'a>, u32)> {
        let Base::Struct { fields, union, .. } = &t.base else {
            return Err(self.error(
                name,
                format!("Member '{name}' of a value that is not a struct"),
            ));
        };
        if !t.packed.is_empty() || !t.unpacked.is_empty() {
            return Err(self.error(name, format!("Member '{name}' of an array; index it first")));
        }
        let mut off = if *union { 0 } else { t.base_width() };
        for (n, ft) in fields.iter() {
            if !*union {
                off -= ft.width();
            }
            if *n == name {
                return Ok((ft.clone(), off));
            }
        }
        Err(self.error(name, format!("Member '{name}' not found in struct")))
    }

    /// `index` normalised to an offset from the right end of range `(l, r)`.
    fn normalise(&mut self, cx: &mut Cx<'a>, index: Val, (l, r): (i64, i64), at: &'a str) -> Val {
        let it = self.bt(32, true, true);
        let k = cx.b.emit(Op::Const(Bits::from_i64(32, r)), it, at);
        if l >= r {
            if r == 0 {
                return index;
            }
            cx.b.emit(Op::Binary(BinOp::Sub, index, k), it, at)
        } else {
            cx.b.emit(Op::Binary(BinOp::Sub, k, index), it, at)
        }
    }

    fn scale_add(
        &mut self,
        cx: &mut Cx<'a>,
        v: Val,
        scale: u32,
        add: Option<Val>,
        at: &'a str,
    ) -> Val {
        let it = self.bt(32, true, true);
        let v = if scale == 1 {
            v
        } else {
            let k =
                cx.b.emit(Op::Const(Bits::from_u64(32, scale as u64)), it, at);
            cx.b.emit(Op::Binary(BinOp::Mul, v, k), it, at)
        };
        match add {
            Some(a) => cx.b.emit(Op::Binary(BinOp::Add, a, v), it, at),
            None => v,
        }
    }

    /// An index expression as a 32-bit signed value.
    fn index_val(&mut self, cx: &mut Cx<'a>, e: &Expr<'a>) -> EResult<Val> {
        let (v, st) = self.lower_self(cx, e)?;
        Ok(self.resize(
            cx,
            v,
            st,
            Want {
                w: 32,
                s: true,
                f: true,
            },
            e.at(),
        ))
    }

    /// Lower a name or select to a [`Path`]; `None` if `e` is not one.
    pub(crate) fn path(&mut self, cx: &mut Cx<'a>, e: &Expr<'a>) -> EResult<Option<Path<'a>>> {
        let at = e.at();
        Ok(Some(match e {
            Expr::Ident(n) => {
                let sym = self.lookup_cx(Some(cx), n);
                self.root_path(cx, sym, n)?
            }
            Expr::Keyword("this") => {
                let Some(c) = self.this_class(cx) else {
                    return Err(self.error(at, "'this' used outside a non-static method"));
                };
                let sym = cx.locals.iter().rev().find_map(|l| l.get("this").cloned());
                let _ = c;
                self.root_path(cx, sym, at)?
            }
            Expr::Scoped { scope, name } if self.scope_class(scope).is_some() => {
                let c = self.scope_class(scope).unwrap()?;
                let sym = self.class_member(c, name)?;
                self.root_path(cx, sym, name)?
            }
            Expr::Scoped { scope, name } => match &**scope {
                Expr::Ident(p) | Expr::Keyword(p) => {
                    let sym = self.scoped_sym(p, name);
                    self.root_path(cx, sym, name)?
                }
                _ => return Err(self.not_yet(at, "nested scopes")),
            },
            Expr::Member { base, name } if self.handle_class(Some(cx), base).is_some() => {
                let c = self.handle_class(Some(cx), base).unwrap();
                // `super.x` is this object's `x` as the base class sees it.
                let obj = match &**base {
                    Expr::Keyword("super") => self.this_handle(cx, at)?,
                    b => self.lower_handle(cx, b, None)?,
                };
                match self.class_member(c, name)? {
                    Some(Sym::Field(index, ty)) => {
                        let fty = self.ir_type(&ty);
                        let width = ty.width();
                        Path {
                            root: Root::Field {
                                obj,
                                index,
                                ty: fty,
                            },
                            base: ty.clone(),
                            elem: None,
                            ty,
                            lsb: None,
                            width,
                            at: name,
                        }
                    }
                    sym => self.root_path(cx, sym, name)?,
                }
            }
            Expr::Member { base, name } => {
                if let Some(s) = self.hier_scope(Some(cx), base) {
                    let sym = self.lookup_child(s, name);
                    return Ok(Some(self.root_path(cx, sym, name)?));
                }
                let Some(mut p) = self.path(cx, base)? else {
                    return Ok(None);
                };
                if !matches!(p.ty.base, Base::Struct { .. }) || !p.ty.packed.is_empty() {
                    return Err(self.not_yet(name, "built-in methods and properties"));
                }
                let (ft, off) = self.member(&p.ty, name)?;
                let it = self.bt(32, true, true);
                let k = cx.b.emit(Op::Const(Bits::from_u64(32, off as u64)), it, at);
                p.lsb = Some(match p.lsb {
                    Some(l) => cx.b.emit(Op::Binary(BinOp::Add, l, k), it, at),
                    None => k,
                });
                p.width = ft.width();
                p.ty = ft;
                p
            }
            Expr::Index { base, index } => {
                let Some(mut p) = self.path(cx, base)? else {
                    return Ok(None);
                };
                if matches!(p.ty.unpacked.first(), Some(UDim::Dynamic | UDim::Queue)) {
                    if p.elem.is_some() || p.ty.unpacked.len() > 1 {
                        return Err(self.not_yet(at, "arrays of arrays with dynamic dimensions"));
                    }
                    let i = self.queue_index(cx, &p, index)?;
                    p.elem = Some(i);
                    p.ty.unpacked.remove(0);
                    p.base = p.ty.clone();
                    p.width = p.ty.width();
                    return Ok(Some(p));
                }
                let i = self.index_val(cx, index)?;
                if !p.ty.unpacked.is_empty() {
                    let UDim::Fixed(l, r) = p.ty.unpacked[0] else {
                        return Err(
                            self.not_yet(at, "dynamic arrays, queues and associative arrays")
                        );
                    };
                    let stride: u32 = p.ty.unpacked[1..]
                        .iter()
                        .map(|d| match d {
                            UDim::Fixed(l, r) => range_len((*l, *r)),
                            _ => 1,
                        })
                        .product();
                    let n = self.normalise(cx, i, (l, r), at);
                    p.elem = Some(self.scale_add(cx, n, stride, p.elem, at));
                    p.ty.unpacked.remove(0);
                    if p.ty.unpacked.is_empty() {
                        p.base = p.ty.clone();
                        p.width = p.ty.width();
                    }
                    p
                } else {
                    let (range, elem) = if !p.ty.packed.is_empty() {
                        (p.ty.packed[0], p.ty.packed_element())
                    } else if p.ty.width() > 1 {
                        (
                            (p.ty.width() as i64 - 1, 0),
                            Ty::bits(1, false, p.ty.four_state()),
                        )
                    } else {
                        return Err(self.error(at, "Illegal bit select of a scalar"));
                    };
                    let ew = elem.width();
                    let n = self.normalise(cx, i, range, at);
                    p.lsb = Some(self.scale_add(cx, n, ew, p.lsb, at));
                    p.width = ew;
                    p.ty = elem;
                    p
                }
            }
            Expr::Slice {
                base,
                op,
                left,
                right,
            } => {
                let Some(mut p) = self.path(cx, base)? else {
                    return Ok(None);
                };
                if !p.ty.unpacked.is_empty() {
                    return Err(self.not_yet(at, "slices of unpacked arrays"));
                }
                let (range, elem) = if !p.ty.packed.is_empty() {
                    (p.ty.packed[0], p.ty.packed_element())
                } else {
                    (
                        (p.ty.width() as i64 - 1, 0),
                        Ty::bits(1, false, p.ty.four_state()),
                    )
                };
                let ew = elem.width();
                let descending = range.0 >= range.1;
                let it = self.bt(32, true, true);
                let (low_index, n) = match *op {
                    ":" => {
                        let (a, b) = (self.const_int(left)?, self.const_int(right)?);
                        // The index nearer the range's right end is the least significant.
                        let lo = if descending { a.min(b) } else { a.max(b) };
                        (
                            cx.b.emit(Op::Const(Bits::from_i64(32, lo)), it, at),
                            (a - b).unsigned_abs() as u32 + 1,
                        )
                    }
                    "+:" | "-:" => {
                        let w = self.const_int(right)?.max(1);
                        let i = self.index_val(cx, left)?;
                        let up = *op == "+:";
                        // +: covers i .. i+w-1, -: covers i-w+1 .. i.
                        let shift = match (up, descending) {
                            (true, true) | (false, false) => 0,
                            _ => w - 1,
                        };
                        let shift = if up { shift } else { -shift };
                        let low = if shift == 0 {
                            i
                        } else {
                            let k = cx.b.emit(Op::Const(Bits::from_i64(32, shift)), it, at);
                            cx.b.emit(Op::Binary(BinOp::Add, i, k), it, at)
                        };
                        (low, w as u32)
                    }
                    _ => return Err(self.error(op, "Unknown part select")),
                };
                let nrm = self.normalise(cx, low_index, range, at);
                p.lsb = Some(self.scale_add(cx, nrm, ew, p.lsb, at));
                p.width = ew * n;
                let mut t = elem;
                t.packed.insert(0, (n as i64 - 1, 0));
                t.signed = false;
                p.ty = t;
                p
            }
            _ => return Ok(None),
        }))
    }

    fn root_path(
        &mut self,
        cx: &mut Cx<'a>,
        sym: Option<Sym<'a>>,
        n: &'a str,
    ) -> EResult<Path<'a>> {
        let (root, ty) = match sym {
            Some(Sym::Var(v, t)) => {
                if cx.const_mode {
                    self.nonconst = Some(n);
                    return Err(Stop);
                }
                (Root::Var(v), t)
            }
            Some(Sym::Slot(s, t)) => {
                if cx.const_mode {
                    self.nonconst = Some(n);
                    return Err(Stop);
                }
                (Root::Slot(s), t)
            }
            Some(Sym::Param(v, t)) => (Root::Const(v), t),
            Some(Sym::Field(index, t)) => {
                if cx.const_mode {
                    self.nonconst = Some(n);
                    return Err(Stop);
                }
                let obj = self.this_handle(cx, n)?;
                let fty = self.ir_type(&t);
                (
                    Root::Field {
                        obj,
                        index,
                        ty: fty,
                    },
                    t,
                )
            }
            Some(Sym::Scope(_)) => {
                return Err(self.error(n, format!("'{n}' is an instance or block, not a value")));
            }
            Some(Sym::Genvar) => {
                return Err(self.error(n, format!("Genvar '{n}' used outside its generate loop")));
            }
            Some(_) => return Err(self.error(n, format!("'{n}' is not a value"))),
            None => return Err(self.error(n, format!("Can't find definition of variable: '{n}'"))),
        };
        let width = ty.width();
        Ok(Path {
            root,
            base: ty.clone(),
            elem: None,
            ty,
            lsb: None,
            width,
            at: n,
        })
    }

    /// Load a path's value.
    pub(crate) fn load_path(&mut self, cx: &mut Cx<'a>, p: &Path<'a>) -> EResult<(Val, STy)> {
        if let Root::Field { obj, index, ty } = &p.root {
            // A property: the whole value, then any element and bit select.
            let whole = cx.b.emit(
                Op::LoadField {
                    obj: *obj,
                    field: *index,
                },
                *ty,
                p.at,
            );
            if !p.ty.unpacked.is_empty() {
                return Ok(match (p.elem, fixed_count(&p.ty)) {
                    (None, _) => (whole, sty_of(&p.ty)),
                    (Some(start), Some(len)) => {
                        let aty = self.ir_type(&p.ty);
                        (
                            cx.b.emit(
                                Op::ArraySlice {
                                    value: whole,
                                    start,
                                    len,
                                },
                                aty,
                                p.at,
                            ),
                            sty_of(&p.ty),
                        )
                    }
                    (Some(_), None) => return Err(self.not_yet(p.at, "this array property")),
                });
            }
            let base = match p.elem {
                Some(index) => {
                    let bty = self.ir_type(&p.base);
                    cx.b.emit(Op::ArrayElem { value: whole, index }, bty, p.at)
                }
                None => whole,
            };
            return Ok(self.select_part(cx, p, base));
        }
        if matches!(p.ty.unpacked.first(), Some(UDim::Dynamic | UDim::Queue)) {
            let aty = self.ir_type(&p.ty);
            let v = match (&p.root, p.elem) {
                (Root::Var(v), None) => cx.b.emit(Op::Load(*v), aty, p.at),
                (Root::Slot(s), None) => cx.b.emit(Op::LoadSlot(*s), aty, p.at),
                _ => return Err(self.not_yet(p.at, "this dynamic array or queue")),
            };
            return Ok((v, sty_of(&p.ty)));
        }
        if !p.ty.unpacked.is_empty() {
            // A whole array, or the sub-array `mem[i]` of a multi-dimensional one.
            let Some(len) = fixed_count(&p.ty) else {
                return Err(self.not_yet(p.at, "dynamic arrays, queues and associative arrays"));
            };
            let aty = self.ir_type(&p.ty);
            let v = match (&p.root, p.elem) {
                (Root::Var(v), None) => cx.b.emit(Op::Load(*v), aty, p.at),
                (Root::Var(v), Some(start)) => cx.b.emit(
                    Op::LoadRange {
                        var: *v,
                        start,
                        len,
                    },
                    aty,
                    p.at,
                ),
                (Root::Const(c), elem) => {
                    let whole = self.emit_value(cx, c, &p.ty, p.at);
                    match elem {
                        None => whole,
                        Some(start) => cx.b.emit(
                            Op::ArraySlice {
                                value: whole,
                                start,
                                len,
                            },
                            aty,
                            p.at,
                        ),
                    }
                }
                (Root::Slot(s), elem) => {
                    let st = cx.b.slot_type(*s);
                    let whole = cx.b.emit(Op::LoadSlot(*s), st, p.at);
                    match elem {
                        None => whole,
                        Some(start) => cx.b.emit(
                            Op::ArraySlice {
                                value: whole,
                                start,
                                len,
                            },
                            aty,
                            p.at,
                        ),
                    }
                }
                (Root::Field { .. }, _) => unreachable!("handled above"),
            };
            return Ok((v, sty_of(&p.ty)));
        }
        if let (Root::Const(c @ Value::Array(_)), Some(index)) = (&p.root, p.elem) {
            // An element of a parameter array.
            let whole = self.emit_value(cx, c, &p.base, p.at);
            let bty = self.ir_type(&p.base);
            let v = cx.b.emit(Op::ArrayElem { value: whole, index }, bty, p.at);
            return Ok(self.select_part(cx, p, v));
        }
        let bty = self.ir_type(&p.base);
        let base = match (&p.root, p.elem) {
            (Root::Var(v), None) => cx.b.emit(Op::Load(*v), bty, p.at),
            (Root::Var(v), Some(i)) => cx.b.emit(Op::LoadElem { var: *v, index: i }, bty, p.at),
            (Root::Slot(s), None) => cx.b.emit(Op::LoadSlot(*s), bty, p.at),
            (Root::Slot(s), Some(index)) => {
                let st = cx.b.slot_type(*s);
                let whole = cx.b.emit(Op::LoadSlot(*s), st, p.at);
                cx.b.emit(Op::ArrayElem { value: whole, index }, bty, p.at)
            }
            (Root::Const(Value::Bits(b)), None) => cx.b.emit(Op::Const(b.clone()), bty, p.at),
            (Root::Const(Value::Real(r)), None) => cx.b.emit(Op::ConstReal(*r), bty, p.at),
            (Root::Const(Value::Str(s)), None) => cx.b.emit(Op::ConstStr(s.clone()), bty, p.at),
            (Root::Const(_), _) => {
                return Err(self.not_yet(p.at, "indexing parameter arrays"));
            }
            (Root::Field { .. }, _) => unreachable!("handled above"),
        };
        Ok(self.select_part(cx, p, base))
    }

    /// The selected part of `base`, the loaded value of `p.base`.
    fn select_part(&mut self, cx: &mut Cx<'a>, p: &Path<'a>, base: Val) -> (Val, STy) {
        match p.lsb {
            None => (base, sty_of(&p.ty)),
            Some(lsb) => {
                let f = p.base.four_state();
                let t = self.bt(p.width, false, f);
                let v = cx.b.emit(
                    Op::Select {
                        value: base,
                        lsb,
                        width: p.width,
                    },
                    t,
                    p.at,
                );
                (
                    v,
                    STy::Bits {
                        w: p.width,
                        s: false,
                        f,
                    },
                )
            }
        }
    }

    /// Emit a constant value of type `ty` (an array's element type, for an array).
    pub(crate) fn emit_value(&mut self, cx: &mut Cx<'a>, v: &Value, ty: &Ty<'a>, at: &'a str) -> Val {
        let mut elem = ty.clone();
        elem.unpacked.clear();
        match v {
            Value::Array(a) => {
                let et = self.ir_type(&elem);
                let parts: Vec<Val> = a.iter().rev().map(|x| self.emit_value(cx, x, &elem, at)).collect();
                let aty = self.add_type(Type::Unpacked {
                    elem: et,
                    left: 0,
                    right: a.len() as i64 - 1,
                });
                cx.b.emit(Op::Concat(parts), aty, at)
            }
            Value::Bits(b) => {
                let t = self.ir_type(&elem);
                cx.b.emit(Op::Const(b.clone()), t, at)
            }
            Value::Real(r) => {
                let t = self.add_type(Type::Real);
                cx.b.emit(Op::ConstReal(*r), t, at)
            }
            Value::Str(s) => {
                let t = self.add_type(Type::String);
                cx.b.emit(Op::ConstStr(s.clone()), t, at)
            }
            Value::Obj(_) => {
                let t = self.add_type(Type::Null);
                cx.b.emit(Op::Null, t, at)
            }
        }
    }

    /// Store `v` (already `p.width` bits) into a path.
    pub(crate) fn store_path(
        &mut self,
        cx: &mut Cx<'a>,
        p: &Path<'a>,
        value: Val,
        nba: bool,
    ) -> EResult<()> {
        let part = p.lsb.map(|lsb| Part {
            lsb,
            width: p.width,
        });
        let op = match (&p.root, p.elem) {
            (Root::Var(var), None) if nba => Op::NbaStore {
                var: *var,
                part,
                value,
            },
            (Root::Var(var), None) => Op::Store {
                var: *var,
                part,
                value,
            },
            (Root::Var(var), Some(index)) => Op::StoreElem {
                var: *var,
                index,
                part,
                value,
                nba,
            },
            (Root::Slot(slot), None) => Op::StoreSlot {
                slot: *slot,
                part,
                value,
            },
            (Root::Slot(slot), Some(index)) => Op::StoreSlotElem {
                slot: *slot,
                index,
                part,
                value,
            },
            (Root::Field { obj, index, .. }, None) => Op::StoreField {
                obj: *obj,
                field: *index,
                part,
                value,
            },
            (Root::Field { obj, index, .. }, Some(elem)) => Op::StoreFieldElem {
                obj: *obj,
                field: *index,
                index: elem,
                part,
                value,
            },
            (Root::Const(_), _) => {
                return Err(self.error(p.at, format!("Cannot assign to a parameter: '{}'", p.at)));
            }
        };
        cx.b.effect(op, p.at);
        Ok(())
    }

    // ------------------------------------------------------------ lowering

    /// Convert `v` of type `from` to the wanted width and signedness.
    pub(crate) fn resize(
        &mut self,
        cx: &mut Cx<'a>,
        v: Val,
        from: STy,
        to: Want,
        at: &'a str,
    ) -> Val {
        let t = self.want_type(to);
        if cx.b.val_type(v) == t {
            return v;
        }
        let extend = match from {
            STy::Bits { w, .. } if to.w < w => Extend::Truncate,
            // Sign-extend only when the propagated type is signed (and so is the operand).
            STy::Bits { s, .. } if to.s && s => Extend::Sign,
            STy::Bits { .. } => Extend::Zero,
            _ => Extend::Sign,
        };
        cx.b.emit(Op::Resize { value: v, extend }, t, at)
    }

    /// Lower with the expression's own self-determined type.
    pub(crate) fn lower_self(&mut self, cx: &mut Cx<'a>, e: &Expr<'a>) -> EResult<(Val, STy)> {
        let st = self.self_type_cx(Some(cx), e)?;
        match st {
            STy::Bits { w, s, f } => Ok((self.lower(cx, e, Want { w, s, f })?, st)),
            STy::Real => Ok((self.lower_real(cx, e)?, st)),
            STy::Str => Ok((self.lower_str(cx, e)?, st)),
            STy::Class(c) => Ok((self.lower_handle(cx, e, c)?, st)),
        }
    }

    /// Lower an expression as the right-hand side of an assignment to `ty`.
    pub(crate) fn lower_to(&mut self, cx: &mut Cx<'a>, e: &Expr<'a>, ty: &Ty<'a>) -> EResult<Val> {
        if !ty.unpacked.is_empty() {
            return self.lower_array(cx, e, ty);
        }
        if let Base::Class(c) = ty.base {
            return self.lower_handle(cx, e, Some(c));
        }
        if ty.base == Base::Str {
            return self.lower_str(cx, e);
        }
        if ty.is_real() {
            return self.lower_real(cx, e);
        }
        if !ty.is_integral() {
            return Err(self.not_yet(e.at(), "assignments of this type"));
        }
        if let Expr::Pattern { items, .. } = e {
            return self.lower_pattern(cx, items, ty, e.at());
        }
        let target = Want {
            w: ty.width(),
            s: ty.signed,
            f: ty.four_state(),
        };
        let st = self.self_type_cx(Some(cx), e)?;
        if st == STy::Real {
            let r = self.lower_real(cx, e)?;
            let t = self.want_type(target);
            return Ok(cx.b.emit(
                Op::Resize {
                    value: r,
                    extend: Extend::Sign,
                },
                t,
                e.at(),
            ));
        }
        let w = st.width().max(target.w);
        let v = self.lower(
            cx,
            e,
            Want {
                w,
                s: st.signed(),
                f: st.four(),
            },
        )?;
        Ok(self.resize(
            cx,
            v,
            STy::Bits {
                w,
                s: st.signed(),
                f: st.four(),
            },
            target,
            e.at(),
        ))
    }

    /// A value for a fixed-size unpacked array type: a pattern, another
    /// array, or a conditional choice between them.
    pub(crate) fn lower_array(&mut self, cx: &mut Cx<'a>, e: &Expr<'a>, ty: &Ty<'a>) -> EResult<Val> {
        let at = e.at();
        if matches!(ty.unpacked[0], UDim::Dynamic | UDim::Queue)
            && ty.unpacked[1..].iter().all(|d| matches!(d, UDim::Fixed(..)))
        {
            return self.lower_dynamic(cx, e, ty);
        }
        if fixed_count(ty).is_none() {
            return Err(self.not_yet(at, "dynamic arrays, queues and associative arrays"));
        }
        let UDim::Fixed(l, r) = ty.unpacked[0] else {
            unreachable!()
        };
        let n = range_len((l, r)) as usize;
        let mut sub = ty.clone();
        sub.unpacked.remove(0);
        let aty = self.ir_type(ty);
        match e {
            Expr::Pattern { items, .. } => {
                let exprs = self.pattern_items(items, n, |this, k| {
                    // A key is an index into the dimension.
                    let i = this.const_int(k)?;
                    let pos = if l >= r { l - i } else { i - l };
                    if pos < 0 || pos as usize >= n {
                        return Err(this.error(k.at(), format!("Assignment pattern index {i} is out of range")));
                    }
                    Ok(pos as usize)
                }, at)?;
                let mut parts = Vec::new();
                for x in &exprs {
                    parts.push(self.lower_to(cx, x, &sub)?);
                }
                Ok(cx.b.emit(Op::Concat(parts), aty, at))
            }
            Expr::Concat(items) => {
                // `{a, b}` of arrays and elements, in unpacked context.
                let mut parts = Vec::new();
                for i in items {
                    match self.array_type(cx, i) {
                        Some(t) => parts.push(self.lower_array(cx, i, &t)?),
                        None => parts.push(self.lower_to(cx, i, &sub)?),
                    }
                }
                Ok(cx.b.emit(Op::Concat(parts), aty, at))
            }
            Expr::Cond {
                cond, then, els, ..
            } => {
                let c = self.truth(cx, cond)?;
                let a = self.lower_array(cx, then, ty)?;
                let b = self.lower_array(cx, els, ty)?;
                Ok(cx.b.emit(
                    Op::Mux {
                        cond: c,
                        then: a,
                        els: b,
                    },
                    aty,
                    at,
                ))
            }
            Expr::Call { func, args } => Ok(self.lower_call(cx, func, args)?.0),
            Expr::Cast { expr, .. } => self.lower_array(cx, expr, ty),
            Expr::Ident(_) | Expr::Member { .. } | Expr::Index { .. } | Expr::Scoped { .. } => {
                let Some(p) = self.path(cx, e)? else {
                    return Err(self.not_yet(at, "this kind of name"));
                };
                if p.ty.unpacked.is_empty() {
                    return Err(self.error(at, "Assigning a non-array value to an unpacked array"));
                }
                if matches!(p.ty.unpacked[0], UDim::Dynamic | UDim::Queue) {
                    // A queue's element 0 is its leftmost; a fixed array's is its rightmost.
                    let (v, _) = self.load_path(cx, &p)?;
                    return Ok(self.arr_op(cx, ArrFunc::Reverse, vec![v], ty, at));
                }
                if fixed_count(&p.ty) != fixed_count(ty) {
                    return Err(self.error(at, "Unpacked array sizes differ in assignment"));
                }
                Ok(self.load_path(cx, &p)?.0)
            }
            _ => Err(self.not_yet(at, "this unpacked array expression")),
        }
    }

    /// [`array_type`](Self::array_type) without a lowering context.
    pub(crate) fn array_type_opt(&mut self, cx: Option<&Cx<'a>>, e: &Expr<'a>) -> Option<Ty<'a>> {
        let n = self.diags.len();
        let r = self.path_type(cx, e).ok().flatten();
        self.diags.truncate(n);
        r.map(|(t, _)| t).filter(|t| !t.unpacked.is_empty())
    }

    /// Emit an array operation giving a value of type `ty`.
    pub(crate) fn arr_op(
        &mut self,
        cx: &mut Cx<'a>,
        func: ArrFunc,
        args: Vec<Val>,
        ty: &Ty<'a>,
        at: &'a str,
    ) -> Val {
        let t = self.ir_type(ty);
        cx.b.emit(Op::ArrFunc { func, args }, t, at)
    }

    /// The index of a queue or dynamic array element, with `$` standing for
    /// the last index.
    fn queue_index(&mut self, cx: &mut Cx<'a>, p: &Path<'a>, index: &Expr<'a>) -> EResult<Val> {
        let whole = self.load_path(cx, p)?.0;
        let saved = cx.dollar;
        let int = Ty::bits(32, true, false);
        let size = self.arr_op(cx, ArrFunc::Size, vec![whole], &int, index.at());
        let it = self.ir_type(&int);
        let one = cx.b.emit(Op::Const(Bits::from_i64(32, 1)), it, index.at());
        cx.dollar = Some(cx.b.emit(Op::Binary(BinOp::Sub, size, one), it, index.at()));
        let r = self.index_val(cx, index);
        cx.dollar = saved;
        r
    }

    /// A value for a dynamic array or queue type.
    fn lower_dynamic(&mut self, cx: &mut Cx<'a>, e: &Expr<'a>, ty: &Ty<'a>) -> EResult<Val> {
        let at = e.at();
        let mut elem = ty.clone();
        elem.unpacked.remove(0);
        let dt = self.ir_type(ty);
        match e {
            Expr::Pattern { items, .. } => {
                if items.iter().any(|i| matches!(i, ast::PatItem::Keyed(..))) {
                    return Err(self.not_yet(at, "keyed patterns for dynamic arrays and queues"));
                }
                let mut parts = Vec::new();
                for i in items {
                    match i {
                        ast::PatItem::Value(v) => parts.push(self.lower_to(cx, v, &elem)?),
                        ast::PatItem::Repeat(count, vs) => {
                            let c = self.const_int(count)?;
                            for _ in 0..c.max(0) {
                                for v in vs {
                                    parts.push(self.lower_to(cx, v, &elem)?);
                                }
                            }
                        }
                        ast::PatItem::Keyed(..) => unreachable!(),
                    }
                }
                Ok(cx.b.emit(Op::Concat(parts), dt, at))
            }
            Expr::Concat(items) => {
                let mut parts = Vec::new();
                for i in items {
                    match self.array_type(cx, i) {
                        Some(t) if elem.unpacked.is_empty() => {
                            // Splice the elements in, as a queue.
                            let mut q = t.clone();
                            q.unpacked[0] = UDim::Queue;
                            parts.push(self.lower_array(cx, i, &q)?);
                        }
                        _ => parts.push(self.lower_to(cx, i, &elem)?),
                    }
                }
                Ok(cx.b.emit(Op::Concat(parts), dt, at))
            }
            Expr::New {
                size: Some(n),
                args,
                ..
            } => {
                let n = self.lower_to(cx, n, &Ty::bits(32, true, false))?;
                let et = self.ir_type(&elem);
                let d = crate::eval::default_for(&self.d.types, &self.d.types[et.0 as usize]);
                let dv = self.emit_value(cx, &d, &elem, at);
                let mut vals = vec![n, dv];
                if let Some(Arg::Ordered(Some(old))) = args.first() {
                    vals.push(self.lower_array(cx, old, ty)?);
                }
                Ok(self.arr_op(cx, ArrFunc::New, vals, ty, at))
            }
            Expr::Slice {
                base,
                op: ":",
                left,
                right,
            } if self.array_type(cx, base).is_some() => {
                let Some(p) = self.path(cx, base)? else {
                    return Err(self.not_yet(at, "this slice"));
                };
                let lo = self.queue_index(cx, &p, left)?;
                let hi = self.queue_index(cx, &p, right)?;
                let (whole, _) = self.load_path(cx, &p)?;
                Ok(self.arr_op(cx, ArrFunc::Slice, vec![whole, lo, hi], ty, at))
            }
            Expr::Cond {
                cond, then, els, ..
            } => {
                let c = self.truth(cx, cond)?;
                let a = self.lower_dynamic(cx, then, ty)?;
                let b = self.lower_dynamic(cx, els, ty)?;
                Ok(cx.b.emit(
                    Op::Mux {
                        cond: c,
                        then: a,
                        els: b,
                    },
                    dt,
                    at,
                ))
            }
            Expr::Call { func, args } => Ok(self.lower_call(cx, func, args)?.0),
            Expr::Cast { expr, .. } => self.lower_dynamic(cx, expr, ty),
            Expr::Ident(_) | Expr::Member { .. } | Expr::Index { .. } | Expr::Scoped { .. } => {
                let Some(p) = self.path(cx, e)? else {
                    return Err(self.not_yet(at, "this kind of name"));
                };
                match p.ty.unpacked.first() {
                    Some(UDim::Dynamic | UDim::Queue) => Ok(self.load_path(cx, &p)?.0),
                    Some(UDim::Fixed(..)) => {
                        let (v, _) = self.load_path(cx, &p)?;
                        Ok(self.arr_op(cx, ArrFunc::Reverse, vec![v], ty, at))
                    }
                    _ => Err(self.error(at, "Assigning a non-array value to a dynamic array")),
                }
            }
            _ => Err(self.not_yet(at, "this dynamic array expression")),
        }
    }

    /// An array method with a value: `q.size()`, `a.sum()`, `q.pop_front()`...
    pub(crate) fn array_method(
        &mut self,
        cx: &mut Cx<'a>,
        base: &Expr<'a>,
        name: &'a str,
        args: &[Arg<'a>],
    ) -> EResult<(Val, STy)> {
        let Some(t) = self.array_type(cx, base) else {
            return Err(self.error(base.at(), "Not an array"));
        };
        if matches!(name, "pop_front" | "pop_back") {
            let v = self.array_pop(cx, base, name)?;
            let mut elem = t.clone();
            elem.unpacked.remove(0);
            return Ok((v, sty_of(&elem)));
        }
        let Some(st) = array_method_type(&t, name) else {
            return Err(self.not_yet(name, &format!("array method {name}")));
        };
        let _ = args;
        let Some(p) = self.path(cx, base)? else {
            return Err(self.error(base.at(), "Not an array"));
        };
        let (whole, _) = self.load_path(cx, &p)?;
        let mut elem = t.clone();
        elem.unpacked.remove(0);
        let func = match name {
            "size" | "num" => ArrFunc::Size,
            "sum" => ArrFunc::Sum,
            "product" => ArrFunc::Product,
            "and" => ArrFunc::And,
            "or" => ArrFunc::Or,
            "xor" => ArrFunc::Xor,
            "min" => ArrFunc::Min,
            "max" => ArrFunc::Max,
            _ => return Err(self.not_yet(name, &format!("array method {name}"))),
        };
        let rty = if matches!(func, ArrFunc::Min | ArrFunc::Max) {
            let mut q = elem.clone();
            q.unpacked.insert(0, UDim::Queue);
            q
        } else {
            st.ty()
        };
        Ok((self.arr_op(cx, func, vec![whole], &rty, name), st))
    }

    /// `q.pop_front()` / `q.pop_back()`: the element, and the queue without it.
    fn array_pop(&mut self, cx: &mut Cx<'a>, base: &Expr<'a>, name: &'a str) -> EResult<Val> {
        let Some(t) = self.array_type(cx, base) else {
            return Err(self.error(base.at(), "Not an array"));
        };
        let Some(p) = self.path(cx, base)? else {
            return Err(self.error(base.at(), "Not an array"));
        };
        let mut elem = t.clone();
        elem.unpacked.remove(0);
        let (whole, _) = self.load_path(cx, &p)?;
        let (take, drop) = if name == "pop_front" {
            (ArrFunc::First, ArrFunc::DropFront)
        } else {
            (ArrFunc::Last, ArrFunc::DropBack)
        };
        let x = self.arr_op(cx, take, vec![whole], &elem, name);
        let rest = self.arr_op(cx, drop, vec![whole], &t, name);
        self.store_path(cx, &p, rest, false)?;
        Ok(x)
    }

    /// An array method that changes the array. False if `name` is not one.
    pub(crate) fn array_mutate(
        &mut self,
        cx: &mut Cx<'a>,
        base: &Expr<'a>,
        name: &'a str,
        args: &[Arg<'a>],
    ) -> EResult<bool> {
        if matches!(name, "pop_front" | "pop_back") {
            self.array_pop(cx, base, name)?;
            return Ok(true);
        }
        if !matches!(
            name,
            "push_back" | "push_front" | "insert" | "delete" | "reverse" | "sort" | "rsort"
        ) {
            return Ok(false);
        }
        let Some(t) = self.array_type(cx, base) else {
            return Ok(false);
        };
        let Some(p) = self.path(cx, base)? else {
            return Err(self.error(base.at(), "Not an array"));
        };
        let mut elem = t.clone();
        elem.unpacked.remove(0);
        let arg = |i: usize| match args.get(i) {
            Some(Arg::Ordered(Some(e))) => Some(e.clone()),
            _ => None,
        };
        let int = Ty::bits(32, true, false);
        let (whole, _) = self.load_path(cx, &p)?;
        let need = |this: &mut Self, i: usize| {
            arg(i).ok_or_else(|| this.error(name, format!("Too few arguments to '{name}'")))
        };
        let new = match name {
            "push_back" | "push_front" => {
                let x = need(self, 0)?;
                let x = self.lower_to(cx, &x, &elem)?;
                let f = if name == "push_back" {
                    ArrFunc::PushBack
                } else {
                    ArrFunc::PushFront
                };
                self.arr_op(cx, f, vec![whole, x], &t, name)
            }
            "insert" => {
                let i = need(self, 0)?;
                let i = self.lower_to(cx, &i, &int)?;
                let x = need(self, 1)?;
                let x = self.lower_to(cx, &x, &elem)?;
                self.arr_op(cx, ArrFunc::Insert, vec![whole, i, x], &t, name)
            }
            "delete" => match arg(0) {
                Some(i) => {
                    let i = self.lower_to(cx, &i, &int)?;
                    self.arr_op(cx, ArrFunc::DeleteAt, vec![whole, i], &t, name)
                }
                None => self.arr_op(cx, ArrFunc::Clear, vec![whole], &t, name),
            },
            "reverse" => self.arr_op(cx, ArrFunc::Reverse, vec![whole], &t, name),
            _ => {
                // A fixed array's elements are stored right to left.
                let fixed = matches!(t.unpacked[0], UDim::Fixed(..));
                let f = match (name, fixed) {
                    ("sort", false) | ("rsort", true) => ArrFunc::Sort,
                    _ => ArrFunc::Rsort,
                };
                self.arr_op(cx, f, vec![whole], &t, name)
            }
        };
        self.store_path(cx, &p, new, false)?;
        Ok(true)
    }

    /// The type of `e` if it names an unpacked array (or a sub-array).
    pub(crate) fn array_type(&mut self, cx: &Cx<'a>, e: &Expr<'a>) -> Option<Ty<'a>> {
        let n = self.diags.len();
        let r = self.path_type(Some(cx), e).ok().flatten();
        self.diags.truncate(n);
        r.map(|(t, _)| t).filter(|t| !t.unpacked.is_empty())
    }

    /// The expressions of an assignment pattern for `n` positions, left to
    /// right: positional items, `n{...}` replications, and `key: value`
    /// items with `default:`. `key` maps an index key to a position.
    pub(crate) fn pattern_items(
        &mut self,
        items: &[ast::PatItem<'a>],
        n: usize,
        mut key: impl FnMut(&mut Self, &Expr<'a>) -> EResult<usize>,
        at: &'a str,
    ) -> EResult<Vec<Expr<'a>>> {
        let keyed = items.iter().any(|i| matches!(i, ast::PatItem::Keyed(..)));
        if !keyed {
            let mut v = Vec::new();
            for i in items {
                match i {
                    ast::PatItem::Value(e) => v.push(e.clone()),
                    ast::PatItem::Repeat(count, es) => {
                        let c = self.const_int(count)?;
                        for _ in 0..c.max(0) {
                            v.extend(es.iter().cloned());
                        }
                    }
                    ast::PatItem::Keyed(..) => unreachable!(),
                }
            }
            if v.len() != n {
                return Err(self.error(
                    at,
                    format!("Assignment pattern has {} items for {n} positions", v.len()),
                ));
            }
            return Ok(v);
        }
        let mut slots: Vec<Option<Expr<'a>>> = vec![None; n];
        let mut default = None;
        for i in items {
            match i {
                ast::PatItem::Keyed(Expr::Keyword("default"), v) => default = Some(v.clone()),
                ast::PatItem::Keyed(k, v) => {
                    let pos = key(self, k)?;
                    slots[pos] = Some(v.clone());
                }
                _ => return Err(self.error(at, "Mixed positional and keyed assignment pattern")),
            }
        }
        slots
            .into_iter()
            .map(|s| match s.or_else(|| default.clone()) {
                Some(e) => Ok(e),
                None => Err(self.error(at, "Assignment pattern is missing values and has no default")),
            })
            .collect()
    }

    /// A positional assignment pattern for a packed type: `'{a, b, c}`.
    fn lower_pattern(
        &mut self,
        cx: &mut Cx<'a>,
        items: &[ast::PatItem<'a>],
        ty: &Ty<'a>,
        at: &'a str,
    ) -> EResult<Val> {
        let (parts, names): (Vec<Ty<'a>>, Vec<&'a str>) = match &ty.base {
            Base::Struct {
                fields,
                union: false,
                ..
            } if ty.packed.is_empty() => (
                fields.iter().map(|(_, t)| t.clone()).collect(),
                fields.iter().map(|(n, _)| *n).collect(),
            ),
            _ if !ty.packed.is_empty() => (
                vec![ty.packed_element(); range_len(ty.packed[0]) as usize],
                Vec::new(),
            ),
            _ => return Err(self.not_yet(at, "assignment patterns for this type")),
        };
        let range = ty.packed.first().copied();
        let n = parts.len();
        let exprs = self.pattern_items(
            items,
            n,
            |this, k| match (k, range) {
                (Expr::Ident(name), None) => match names.iter().position(|f| f == name) {
                    Some(p) => Ok(p),
                    None => Err(this.error(name, format!("Unknown member '{name}' in assignment pattern"))),
                },
                (_, Some((l, r))) => {
                    let i = this.const_int(k)?;
                    let pos = if l >= r { l - i } else { i - l };
                    if pos < 0 || pos as usize >= n {
                        return Err(this.error(k.at(), format!("Assignment pattern index {i} is out of range")));
                    }
                    Ok(pos as usize)
                }
                _ => Err(this.not_yet(k.at(), "type keys in assignment patterns")),
            },
            at,
        )?;
        let mut vals = Vec::new();
        for (v, t) in exprs.iter().zip(&parts) {
            vals.push(self.lower_to(cx, v, t)?);
        }
        let t = self.ir_type(ty);
        Ok(cx.b.emit(Op::Concat(vals), t, at))
    }

    /// A 1-bit truth value for a condition.
    pub(crate) fn truth(&mut self, cx: &mut Cx<'a>, e: &Expr<'a>) -> EResult<Val> {
        let (v, st) = self.lower_self(cx, e)?;
        Ok(self.truth_of(cx, v, st, e.at()))
    }

    fn truth_of(&mut self, cx: &mut Cx<'a>, v: Val, st: STy, at: &'a str) -> Val {
        match st {
            STy::Bits { w: 1, s: false, .. } => v,
            STy::Bits { f, .. } => {
                let t = self.bt(1, false, f);
                cx.b.emit(Op::Unary(UnOp::RedOr, v), t, at)
            }
            STy::Class(_) => {
                // A handle is true when it is not null.
                let t = self.bt(1, false, false);
                let nt = self.add_type(Type::Null);
                let null = cx.b.emit(Op::Null, nt, at);
                cx.b.emit(Op::Binary(BinOp::Ne, v, null), t, at)
            }
            _ => {
                let t = self.bt(1, false, false);
                let rt = self.add_type(Type::Real);
                let zero = cx.b.emit(Op::ConstReal(0.0), rt, at);
                cx.b.emit(Op::Binary(BinOp::Ne, v, zero), t, at)
            }
        }
    }

    /// Lower an integral expression to exactly `want` (context-determined).
    pub(crate) fn lower(&mut self, cx: &mut Cx<'a>, e: &Expr<'a>, want: Want) -> EResult<Val> {
        let at = e.at();
        self.last_at = at;
        let t = self.want_type(want);
        // A string in an integral context: its bytes (LRM 6.16).
        if matches!(
            e,
            Expr::Ident(_)
                | Expr::Member { .. }
                | Expr::Scoped { .. }
                | Expr::Concat(_)
                | Expr::Repl { .. }
                | Expr::Cond { .. }
                | Expr::Call { .. }
                | Expr::SysCall { .. }
        ) && self.is_str(Some(cx), e)
        {
            let v = self.lower_str(cx, e)?;
            return Ok(cx.b.emit(Op::Convert(v), t, at));
        }
        match e {
            Expr::Member { base, name } if self.is_str(Some(cx), base) => {
                let (v, st) = self.str_method(cx, base, name, &[])?;
                return Ok(self.resize(cx, v, st, want, at));
            }
            Expr::Member { base, name } if self.array_type(cx, base).is_some() => {
                let (v, st) = self.array_method(cx, base, name, &[])?;
                return Ok(self.resize(cx, v, st, want, at));
            }
            Expr::Keyword("$") if cx.dollar.is_some() => {
                let d = cx.dollar.unwrap();
                let st = STy::Bits {
                    w: 32,
                    s: true,
                    f: false,
                };
                return Ok(self.resize(cx, d, st, want, at));
            }
            Expr::Index { base, index } if self.is_str(Some(cx), base) => {
                let args = [Arg::Ordered(Some((**index).clone()))];
                let (v, st) = self.str_method(cx, base, "getc", &args)?;
                return Ok(self.resize(cx, v, st, want, at));
            }
            _ => {}
        }
        match e {
            Expr::Number(n) => match parse_literal(n) {
                Ok(Literal::Bits {
                    mut bits,
                    signed,
                    sized,
                }) => {
                    // An unsized literal whose leftmost digit is X or Z
                    // extends with X or Z to the context's width (LRM 5.7.1).
                    let (mv, mu) = bits.msb();
                    if !sized && mu && want.w > bits.width {
                        let natural = bits.width;
                        bits = bits.resize(want.w, false);
                        for i in natural..want.w {
                            bits.set_bit(i, mv, true);
                        }
                    }
                    let c = cx.b.emit(
                        Op::Const(bits.clone()),
                        self.bt(bits.width, signed, bits.has_unknown()),
                        at,
                    );
                    Ok(self.resize(
                        cx,
                        c,
                        STy::Bits {
                            w: bits.width,
                            s: signed,
                            f: bits.has_unknown(),
                        },
                        want,
                        at,
                    ))
                }
                Ok(Literal::Fill(b)) => {
                    let (v, u) = b.bit(0);
                    let fill = match (v, u) {
                        (false, false) => Bits::zero(want.w),
                        (true, false) => Bits::ones(want.w),
                        (false, true) => Bits::all_x(want.w),
                        (true, true) => Bits::all_z(want.w),
                    };
                    Ok(cx.b.emit(Op::Const(fill), t, at))
                }
                Ok(Literal::Real(_) | Literal::Time(..)) => {
                    let r = self.lower_real(cx, e)?;
                    Ok(cx.b.emit(
                        Op::Resize {
                            value: r,
                            extend: Extend::Sign,
                        },
                        t,
                        at,
                    ))
                }
                Err(m) => Err(self.error(n, m)),
            },
            Expr::Str(s) => {
                let text = super::decode_string_bytes(super::str_body(s));
                let w = (text.len() as u32 * 8).max(8);
                let mut b = Bits::zero(w);
                for (i, byte) in text.iter().copied().rev().enumerate() {
                    b.insert(i as i64 * 8, &Bits::from_u64(8, byte as u64));
                }
                let c = cx.b.emit(Op::Const(b), self.bt(w, false, false), at);
                Ok(self.resize(
                    cx,
                    c,
                    STy::Bits {
                        w,
                        s: false,
                        f: false,
                    },
                    want,
                    at,
                ))
            }
            Expr::Ident(_)
            | Expr::Member { .. }
            | Expr::Index { .. }
            | Expr::Slice { .. }
            | Expr::Scoped { .. } => {
                let Some(p) = self.path(cx, e)? else {
                    return Err(self.not_yet(at, "this kind of name"));
                };
                let (v, st) = self.load_path(cx, &p)?;
                if st == STy::Real {
                    return Ok(cx.b.emit(
                        Op::Resize {
                            value: v,
                            extend: Extend::Sign,
                        },
                        t,
                        at,
                    ));
                }
                Ok(self.resize(cx, v, st, want, at))
            }
            Expr::Unary { op, arg } => match *op {
                "+" => self.lower(cx, arg, want),
                "-" | "~" => {
                    let a = self.lower(cx, arg, want)?;
                    let u = if *op == "-" { UnOp::Neg } else { UnOp::Not };
                    Ok(cx.b.emit(Op::Unary(u, a), t, at))
                }
                _ => {
                    let (a, st) = self.lower_self(cx, arg)?;
                    let u = match *op {
                        "!" => UnOp::LogNot,
                        "&" => UnOp::RedAnd,
                        "~&" => UnOp::RedNand,
                        "|" => UnOp::RedOr,
                        "~|" => UnOp::RedNor,
                        "^" => UnOp::RedXor,
                        "~^" | "^~" => UnOp::RedXnor,
                        _ => return Err(self.error(op, format!("Unknown unary operator {op}"))),
                    };
                    let bit = self.bt(1, false, st.four());
                    let v = cx.b.emit(Op::Unary(u, a), bit, at);
                    Ok(self.resize(
                        cx,
                        v,
                        STy::Bits {
                            w: 1,
                            s: false,
                            f: st.four(),
                        },
                        want,
                        at,
                    ))
                }
            },
            Expr::Binary { op, lhs, rhs } => self.lower_binary(cx, op, lhs, rhs, want),
            Expr::Cond {
                cond, then, els, ..
            } => {
                let c = self.truth(cx, cond)?;
                if has_effects(then) || has_effects(els) {
                    let (tb, eb, join) = (cx.b.new_block(), cx.b.new_block(), cx.b.new_block());
                    let r = cx.b.block_param(join, t);
                    cx.b.terminate(Terminator::Branch {
                        cond: c,
                        then: (tb, vec![]),
                        els: (eb, vec![]),
                    });
                    cx.b.switch_to(tb);
                    let a = self.lower(cx, then, want)?;
                    cx.b.terminate(Terminator::Jump(join, vec![a]));
                    cx.b.switch_to(eb);
                    let b = self.lower(cx, els, want)?;
                    cx.b.terminate(Terminator::Jump(join, vec![b]));
                    cx.b.switch_to(join);
                    return Ok(r);
                }
                let a = self.lower(cx, then, want)?;
                let b = self.lower(cx, els, want)?;
                Ok(cx.b.emit(
                    Op::Mux {
                        cond: c,
                        then: a,
                        els: b,
                    },
                    t,
                    at,
                ))
            }
            Expr::Concat(items) => {
                let mut parts = Vec::new();
                let mut w = 0;
                let mut f = false;
                // `{0{x}}` contributes nothing inside a larger concatenation.
                for i in items {
                    if self.is_zero_repl(i) {
                        continue;
                    }
                    let (v, st) = self.lower_self(cx, i)?;
                    if st == STy::Real {
                        return Err(self.error(i.at(), "Real value in a concatenation"));
                    }
                    w += st.width();
                    f |= st.four();
                    parts.push(v);
                }
                let ct = self.bt(w, false, f);
                let v = cx.b.emit(Op::Concat(parts), ct, at);
                Ok(self.resize(cx, v, STy::Bits { w, s: false, f }, want, at))
            }
            Expr::Repl { count, items } => {
                let n = self.const_int(count)?;
                if n <= 0 {
                    return Err(self.error(count.at(), "Replication count must be positive"));
                }
                let inner = Expr::Concat(items.clone());
                let (v, st) = self.lower_self(cx, &inner)?;
                let w = st.width() * n as u32;
                let rt = self.bt(w, false, st.four());
                let r = cx.b.emit(
                    Op::Repl {
                        value: v,
                        count: n as u32,
                    },
                    rt,
                    at,
                );
                Ok(self.resize(
                    cx,
                    r,
                    STy::Bits {
                        w,
                        s: false,
                        f: st.four(),
                    },
                    want,
                    at,
                ))
            }
            Expr::Inside { expr, set } => {
                let v = self.lower_inside(cx, expr, set)?;
                Ok(self.resize(
                    cx,
                    v,
                    STy::Bits {
                        w: 1,
                        s: false,
                        f: true,
                    },
                    want,
                    at,
                ))
            }
            Expr::MinTypMax(v) => self.lower(cx, &v[1], want),
            Expr::Call { func, args } => {
                let (v, st) = self.lower_call(cx, func, args)?;
                Ok(self.resize(cx, v, st, want, at))
            }
            Expr::SysCall { name, args } => {
                let (v, st) = self.lower_sys(cx, name, args)?;
                if st == STy::Real {
                    return Ok(cx.b.emit(
                        Op::Resize {
                            value: v,
                            extend: Extend::Sign,
                        },
                        t,
                        at,
                    ));
                }
                Ok(self.resize(cx, v, st, want, at))
            }
            Expr::Cast { ty, expr } => {
                let target = match &**ty {
                    Expr::Keyword("signed" | "unsigned") => {
                        let (v, st) = self.lower_self(cx, expr)?;
                        let s = matches!(&**ty, Expr::Keyword("signed"));
                        let w = st.width();
                        let rt = self.bt(w, s, st.four());
                        let r = cx.b.emit(
                            Op::Resize {
                                value: v,
                                extend: Extend::Zero,
                            },
                            rt,
                            at,
                        );
                        return Ok(self.resize(cx, r, STy::Bits { w, s, f: st.four() }, want, at));
                    }
                    Expr::Type(dt) => self.resolve_type(dt)?,
                    Expr::Ident(n) if matches!(self.lookup_cx(Some(cx), n), Some(Sym::Type(_))) => {
                        let Some(Sym::Type(t)) = self.lookup_cx(Some(cx), n) else {
                            unreachable!()
                        };
                        t
                    }
                    size => {
                        let w = self.const_int(size)?.max(1) as u32;
                        let st = self.self_type_cx(Some(cx), expr)?;
                        Ty::bits(w, st.signed(), st.four())
                    }
                };
                let v = self.lower_to(cx, expr, &target)?;
                let st = sty_of(&target);
                Ok(self.resize(cx, v, st, want, at))
            }
            Expr::IncDec { .. } | Expr::Assign { .. } => {
                Err(self.not_yet(at, "assignments inside expressions"))
            }
            other => Err(self.not_yet(other.at(), "this expression")),
        }
    }

    fn lower_binary(
        &mut self,
        cx: &mut Cx<'a>,
        op: &'a str,
        lhs: &Expr<'a>,
        rhs: &Expr<'a>,
        want: Want,
    ) -> EResult<Val> {
        let t = self.want_type(want);
        let at = op;
        if let Some(b) = lookup_op(BINARY, op) {
            let a = self.lower(cx, lhs, want)?;
            let c = self.lower(cx, rhs, want)?;
            return Ok(cx.b.emit(Op::Binary(b, a, c), t, at));
        }
        match op {
            "<<" | ">>" | "<<<" | ">>>" | "**" => {
                let a = self.lower(cx, lhs, want)?;
                let (c, _) = self.lower_self(cx, rhs)?;
                let b = match op {
                    "<<" | "<<<" => BinOp::Shl,
                    ">>" => BinOp::Shr,
                    ">>>" => BinOp::AShr,
                    _ => BinOp::Pow,
                };
                Ok(cx.b.emit(Op::Binary(b, a, c), t, at))
            }
            "&&" | "||" | "->" | "<->" => {
                let a = self.truth(cx, lhs)?;
                let bit = self.bt(1, false, true);
                let v = if matches!(op, "&&" | "||") && has_effects(rhs) {
                    // Short circuit: only evaluate the right side when needed.
                    let (rb, join) = (cx.b.new_block(), cx.b.new_block());
                    let r = cx.b.block_param(join, bit);
                    let (then, els) = if op == "&&" {
                        ((rb, vec![]), (join, vec![a]))
                    } else {
                        ((join, vec![a]), (rb, vec![]))
                    };
                    cx.b.terminate(Terminator::Branch { cond: a, then, els });
                    cx.b.switch_to(rb);
                    let c = self.truth(cx, rhs)?;
                    cx.b.terminate(Terminator::Jump(join, vec![c]));
                    cx.b.switch_to(join);
                    r
                } else {
                    let c = self.truth(cx, rhs)?;
                    match op {
                        "&&" => cx.b.emit(Op::Binary(BinOp::And, a, c), bit, at),
                        "||" => cx.b.emit(Op::Binary(BinOp::Or, a, c), bit, at),
                        "->" => {
                            let na = cx.b.emit(Op::Unary(UnOp::Not, a), bit, at);
                            cx.b.emit(Op::Binary(BinOp::Or, na, c), bit, at)
                        }
                        _ => cx.b.emit(Op::Binary(BinOp::Xnor, a, c), bit, at),
                    }
                };
                Ok(self.resize(
                    cx,
                    v,
                    STy::Bits {
                        w: 1,
                        s: false,
                        f: true,
                    },
                    want,
                    at,
                ))
            }
            _ => {
                let Some(b) = lookup_op(COMPARE, op) else {
                    return Err(self.error(op, format!("Unknown operator {op}")));
                };
                let (x, y) = (
                    self.self_type_cx(Some(cx), lhs)?,
                    self.self_type_cx(Some(cx), rhs)?,
                );
                let bit = self.bt(1, false, !matches!(b, BinOp::CaseEq | BinOp::CaseNe));
                let array_ty = match self.array_type(cx, lhs) {
                    Some(t) => Some(t),
                    None => self.array_type(cx, rhs),
                };
                let v = if matches!(x, STy::Class(_)) || matches!(y, STy::Class(_)) {
                    let a = self.lower_handle(cx, lhs, None)?;
                    let c = self.lower_handle(cx, rhs, None)?;
                    cx.b.emit(Op::Binary(b, a, c), bit, at)
                } else if let Some(t) = array_ty {
                    let a = self.lower_array(cx, lhs, &t)?;
                    let c = self.lower_array(cx, rhs, &t)?;
                    cx.b.emit(Op::Binary(b, a, c), bit, at)
                } else if x == STy::Str || y == STy::Str {
                    let a = self.lower_str(cx, lhs)?;
                    let c = self.lower_str(cx, rhs)?;
                    cx.b.emit(Op::Binary(b, a, c), bit, at)
                } else if x == STy::Real || y == STy::Real {
                    let a = self.lower_real(cx, lhs)?;
                    let c = self.lower_real(cx, rhs)?;
                    cx.b.emit(Op::Binary(b, a, c), bit, at)
                } else {
                    let ow = Want {
                        w: x.width().max(y.width()),
                        s: x.signed() && y.signed(),
                        f: x.four() || y.four(),
                    };
                    let a = self.lower(cx, lhs, ow)?;
                    let c = self.lower(cx, rhs, ow)?;
                    cx.b.emit(Op::Binary(b, a, c), bit, at)
                };
                let f = !matches!(b, BinOp::CaseEq | BinOp::CaseNe);
                Ok(self.resize(cx, v, STy::Bits { w: 1, s: false, f }, want, at))
            }
        }
    }

    /// `expr inside { ... }`: `==?` for values, `<=` bounds for ranges.
    pub(crate) fn lower_inside(
        &mut self,
        cx: &mut Cx<'a>,
        expr: &Expr<'a>,
        set: &[Expr<'a>],
    ) -> EResult<Val> {
        let bit = self.bt(1, false, true);
        let mut acc: Option<Val> = None;
        for item in set {
            let m = match item {
                Expr::Range { lo, hi } => {
                    let ge = Expr::Binary {
                        op: ">=",
                        lhs: Box::new(expr.clone()),
                        rhs: lo.clone(),
                    };
                    let le = Expr::Binary {
                        op: "<=",
                        lhs: Box::new(expr.clone()),
                        rhs: hi.clone(),
                    };
                    let a = self.lower(
                        cx,
                        &ge,
                        Want {
                            w: 1,
                            s: false,
                            f: true,
                        },
                    )?;
                    let b = self.lower(
                        cx,
                        &le,
                        Want {
                            w: 1,
                            s: false,
                            f: true,
                        },
                    )?;
                    cx.b.emit(Op::Binary(BinOp::And, a, b), bit, item.at())
                }
                v => {
                    let eq = Expr::Binary {
                        op: "==?",
                        lhs: Box::new(expr.clone()),
                        rhs: Box::new(v.clone()),
                    };
                    self.lower(
                        cx,
                        &eq,
                        Want {
                            w: 1,
                            s: false,
                            f: true,
                        },
                    )?
                }
            };
            acc = Some(match acc {
                Some(a) => cx.b.emit(Op::Binary(BinOp::Or, a, m), bit, item.at()),
                None => m,
            });
        }
        match acc {
            Some(a) => Ok(a),
            None => Ok(cx.b.emit(Op::Const(Bits::zero(1)), bit, expr.at())),
        }
    }

    /// A real-valued expression.
    pub(crate) fn lower_real(&mut self, cx: &mut Cx<'a>, e: &Expr<'a>) -> EResult<Val> {
        let rt = self.add_type(Type::Real);
        let at = e.at();
        match e {
            Expr::Number(n) => match parse_literal(n) {
                Ok(Literal::Real(r)) => Ok(cx.b.emit(Op::ConstReal(r), rt, at)),
                Ok(Literal::Time(v, exp)) => {
                    let unit = self.d.scopes[self.cur.0 as usize].unit;
                    Ok(cx
                        .b
                        .emit(Op::ConstReal(v * 10f64.powi((exp - unit) as i32)), rt, at))
                }
                Ok(_) => self.int_to_real(cx, e),
                Err(m) => Err(self.error(n, m)),
            },
            Expr::Unary { op: "-", arg } => {
                let a = self.lower_real(cx, arg)?;
                Ok(cx.b.emit(Op::Unary(UnOp::Neg, a), rt, at))
            }
            Expr::Unary { op: "+", arg } => self.lower_real(cx, arg),
            Expr::Binary { op, lhs, rhs } if matches!(*op, "+" | "-" | "*" | "/" | "**") => {
                let a = self.lower_real(cx, lhs)?;
                let b = self.lower_real(cx, rhs)?;
                let op = match *op {
                    "+" => BinOp::Add,
                    "-" => BinOp::Sub,
                    "*" => BinOp::Mul,
                    "/" => BinOp::Div,
                    _ => BinOp::Pow,
                };
                Ok(cx.b.emit(Op::Binary(op, a, b), rt, at))
            }
            Expr::Cond {
                cond, then, els, ..
            } => {
                let c = self.truth(cx, cond)?;
                let a = self.lower_real(cx, then)?;
                let b = self.lower_real(cx, els)?;
                Ok(cx.b.emit(
                    Op::Mux {
                        cond: c,
                        then: a,
                        els: b,
                    },
                    rt,
                    at,
                ))
            }
            _ => {
                let st = self.self_type_cx(Some(cx), e)?;
                match st {
                    STy::Real => match e {
                        Expr::Ident(_)
                        | Expr::Member { .. }
                        | Expr::Index { .. }
                        | Expr::Scoped { .. } => {
                            let Some(p) = self.path(cx, e)? else {
                                unreachable!()
                            };
                            Ok(self.load_path(cx, &p)?.0)
                        }
                        Expr::SysCall { name, args } => Ok(self.lower_sys(cx, name, args)?.0),
                        Expr::Call { func, args } => Ok(self.lower_call(cx, func, args)?.0),
                        _ => Err(self.not_yet(at, "this real expression")),
                    },
                    _ => self.int_to_real(cx, e),
                }
            }
        }
    }

    fn int_to_real(&mut self, cx: &mut Cx<'a>, e: &Expr<'a>) -> EResult<Val> {
        let rt = self.add_type(Type::Real);
        let (v, _) = self.lower_self(cx, e)?;
        Ok(cx.b.emit(Op::Convert(v), rt, e.at()))
    }

    /// A function call; returns the value and its type.
    pub(crate) fn lower_call(
        &mut self,
        cx: &mut Cx<'a>,
        func: &Expr<'a>,
        args: &[Arg<'a>],
    ) -> EResult<(Val, STy)> {
        if let Expr::Member { base, name } = func
            && self.is_str(Some(cx), base)
        {
            return self.str_method(cx, base, name, args);
        }
        if let Expr::Member { base, name } = func
            && self.array_type(cx, base).is_some()
        {
            return self.array_method(cx, base, name, args);
        }
        match self.call_expr(cx, func, args)? {
            Some(r) => Ok(r),
            None => Err(self.error(func.at(), "Void function used in an expression")),
        }
    }

    /// A call of a function, task or method; its value if it has one.
    pub(crate) fn call_expr(
        &mut self,
        cx: &mut Cx<'a>,
        func: &Expr<'a>,
        args: &[Arg<'a>],
    ) -> EResult<Option<(Val, STy)>> {
        match func {
            // `super.f()`: the base class's implementation, on this object.
            Expr::Member { base, name } if matches!(&**base, Expr::Keyword("super")) => {
                let Some(c) = self.handle_class(Some(cx), base) else {
                    return Err(self.error(name, "'super' used outside a derived class"));
                };
                let this = self.this_handle(cx, name)?;
                return self.method_call(cx, c, Some(this), name, args, true);
            }
            Expr::Member { base, name } if self.handle_class(Some(cx), base).is_some() => {
                let c = self.handle_class(Some(cx), base).unwrap();
                let obj = self.lower_handle(cx, base, None)?;
                return self.method_call(cx, c, Some(obj), name, args, false);
            }
            // `C::f()`: a static method, or a base class's on this object.
            Expr::Scoped { scope, name } if self.scope_class(scope).is_some() => {
                let c = self.scope_class(scope).unwrap()?;
                return self.method_call(cx, c, None, name, args, true);
            }
            _ => {}
        }
        let f = self.resolve_func(Some(cx), func)?;
        if let Some(&(c, _)) = self.func_class.get(&f) {
            // A method called by name inside its class: on this object,
            // through the vtable if virtual.
            let name = self.d.funcs[f.0 as usize].name;
            let c = self.this_class(cx).unwrap_or(c);
            return self.method_call(cx, c, None, name, args, false);
        }
        if cx.const_mode {
            // A constant function: make sure its body exists to be evaluated.
            self.ensure_lowered(f)?;
        }
        self.emit_call(cx, f, args, func.at())
    }

    /// Call `f`, then copy its output arguments back to the caller's
    /// expressions. Returns the value, if it has one.
    pub(crate) fn emit_call(
        &mut self,
        cx: &mut Cx<'a>,
        f: FuncId,
        args: &[Arg<'a>],
        at: &'a str,
    ) -> EResult<Option<(Val, STy)>> {
        self.emit_call_on(cx, f, None, None, args, at)
    }

    /// Call `f`, with `this` as the first argument of a method, through
    /// vtable `slot` if given.
    pub(crate) fn emit_call_on(
        &mut self,
        cx: &mut Cx<'a>,
        f: FuncId,
        this: Option<Val>,
        slot: Option<u32>,
        args: &[Arg<'a>],
        at: &'a str,
    ) -> EResult<Option<(Val, STy)>> {
        let sig = self.sig(f)?;
        let mut vals: Vec<Val> = this.into_iter().collect();
        vals.extend(self.call_args(cx, f, args, at)?);
        let outs: Vec<usize> = (0..sig.params.len())
            .filter(|&i| sig.dirs[i] != super::Dir::Input)
            .collect();
        let call = match slot {
            Some(slot) => Op::VCall { slot, args: vals },
            None => Op::Call {
                func: f,
                args: vals,
            },
        };
        if outs.is_empty() {
            return Ok(match &sig.ret {
                Some(ret) => {
                    let rt = self.ir_type(ret);
                    Some((cx.b.emit(call, rt, at), sty_of(ret)))
                }
                None => {
                    cx.b.effect(call, at);
                    None
                }
            });
        }
        let tt = self.tuple_type(&sig);
        let t = cx.b.emit(call, tt, at);
        let int = self.bt(32, true, false);
        let mut k = 0;
        let mut elem = |this: &mut Self, cx: &mut Cx<'a>, ty: &Ty<'a>| {
            let i = cx.b.emit(Op::Const(Bits::from_u64(32, k)), int, at);
            k += 1;
            let et = this.ir_type(ty);
            cx.b.emit(Op::ArrayElem { value: t, index: i }, et, at)
        };
        let ret = sig.ret.as_ref().map(|r| (elem(self, cx, r), sty_of(r)));
        for i in outs {
            let (name, ty) = &sig.params[i];
            let v = elem(self, cx, ty);
            let actual = match args.get(i) {
                Some(Arg::Ordered(Some(e))) => e.clone(),
                Some(Arg::Named(n, Some(e))) if n == name => e.clone(),
                // An omitted argument's default is where its value goes.
                _ => match sig.defaults.get(i).cloned().flatten() {
                    Some(e) => e,
                    None => continue,
                },
            };
            self.store_value(cx, &actual, v, ty)?;
        }
        Ok(ret)
    }

    /// The result type of a subroutine with output arguments.
    pub(crate) fn tuple_type(&mut self, sig: &super::Sig<'a>) -> TypeId {
        let mut ts = Vec::new();
        if let Some(r) = &sig.ret {
            ts.push(self.ir_type(r));
        }
        for (i, (_, ty)) in sig.params.iter().enumerate() {
            if sig.dirs[i] != super::Dir::Input {
                ts.push(self.ir_type(ty));
            }
        }
        self.add_type(Type::Tuple(ts))
    }

    /// Store `v`, a value of type `ty`, into `lhs`, converting as an assignment does.
    pub(crate) fn store_value(&mut self, cx: &mut Cx<'a>, lhs: &Expr<'a>, v: Val, ty: &Ty<'a>) -> EResult<()> {
        if ty.is_integral() {
            return self.assign_val(cx, lhs, v, sty_of(ty), false);
        }
        let Some(p) = self.path(cx, lhs)? else {
            return Err(self.error(lhs.at(), "Illegal assignment target"));
        };
        let v = if ty.is_real() && p.ty.is_integral() {
            let t = self.bt(p.width, p.ty.signed, p.ty.four_state());
            cx.b.emit(
                Op::Resize {
                    value: v,
                    extend: Extend::Sign,
                },
                t,
                lhs.at(),
            )
        } else if ty.base == Base::Str && p.ty.is_integral() {
            let t = self.bt(p.width, p.ty.signed, p.ty.four_state());
            cx.b.emit(Op::Convert(v), t, lhs.at())
        } else {
            v
        };
        self.store_path(cx, &p, v, false)
    }

    pub(crate) fn call_args(
        &mut self,
        cx: &mut Cx<'a>,
        f: FuncId,
        args: &[Arg<'a>],
        at: &'a str,
    ) -> EResult<Vec<Val>> {
        let sig = self.sig(f)?;
        if args.len() > sig.params.len() {
            return Err(self.error(at, format!("Too many arguments in call to '{at}'")));
        }
        let mut vals = Vec::new();
        for (i, (name, ty)) in sig.params.iter().enumerate() {
            if sig.dirs[i] == super::Dir::Output {
                // An output starts at its type's default value.
                let irt = self.ir_type(ty);
                let d = crate::eval::default_for(&self.d.types, &self.d.types[irt.0 as usize]);
                vals.push(self.emit_value(cx, &d, ty, at));
                continue;
            }
            let given = match args.get(i) {
                Some(Arg::Ordered(Some(e))) => Some(e.clone()),
                Some(Arg::Named(n, Some(e))) if n == name => Some(e.clone()),
                Some(Arg::Named(..)) => {
                    return Err(self.not_yet(at, "named arguments out of order"));
                }
                _ => None,
            };
            let e = match given.or_else(|| sig.defaults.get(i).cloned().flatten()) {
                Some(e) => e,
                None => {
                    return Err(
                        self.error(at, format!("Missing argument '{name}' in call to '{at}'"))
                    );
                }
            };
            vals.push(self.lower_to(cx, &e, ty)?);
        }
        Ok(vals)
    }

    /// A system function in an expression.
    pub(crate) fn lower_sys(
        &mut self,
        cx: &mut Cx<'a>,
        name: &'a str,
        args: &[Arg<'a>],
    ) -> EResult<(Val, STy)> {
        let st = self.sys_type(Some(cx), name, args)?;
        let arg = |i: usize| match args.get(i) {
            Some(Arg::Ordered(Some(e))) => Some(e),
            _ => None,
        };
        let int = self.bt(32, true, false);
        let konst =
            |cx: &mut Cx<'a>, v: i64| cx.b.emit(Op::Const(Bits::from_i64(32, v)), int, name);
        match name {
            "$signed" | "$unsigned" => {
                let Some(a) = arg(0) else {
                    return Err(self.error(name, format!("{name} needs one argument")));
                };
                let (v, at) = self.lower_self(cx, a)?;
                let t = self.bt(at.width(), name == "$signed", at.four());
                Ok((
                    cx.b.emit(
                        Op::Resize {
                            value: v,
                            extend: Extend::Zero,
                        },
                        t,
                        name,
                    ),
                    st,
                ))
            }
            "$size" | "$left" | "$right" | "$low" | "$high" | "$dimensions" | "$increment" => {
                let Some(a) = arg(0) else {
                    return Err(self.error(name, format!("{name} needs an argument")));
                };
                // The type: of a name, or a type itself.
                let t = match a {
                    Expr::Type(t) => self.resolve_type(t)?,
                    Expr::Ident(n) if matches!(self.lookup_cx(Some(cx), n), Some(Sym::Type(_))) => {
                        let Some(Sym::Type(t)) = self.lookup_cx(Some(cx), n) else {
                            unreachable!()
                        };
                        t
                    }
                    e => match self.path_type(Some(cx), e)? {
                        Some((t, _)) => t,
                        None => self.self_type_cx(Some(cx), e)?.ty(),
                    },
                };
                let dim = match arg(1) {
                    Some(d) => self.const_int(d)?,
                    None => 1,
                };
                if name == "$dimensions" {
                    let n = t.unpacked.len() + t.packed.len().max(usize::from(t.width() > 1));
                    return Ok((konst(cx, n as i64), st));
                }
                // Dimensions: unpacked ones first, then packed.
                let mut ranges: Vec<Option<(i64, i64)>> = t
                    .unpacked
                    .iter()
                    .map(|d| match d {
                        UDim::Fixed(l, r) => Some((*l, *r)),
                        _ => None,
                    })
                    .collect();
                if t.packed.is_empty() && t.unpacked.is_empty() {
                    ranges.push(Some((t.width() as i64 - 1, 0)));
                }
                ranges.extend(t.packed.iter().map(|r| Some(*r)));
                let Some(range) = ranges.get(dim as usize - 1).copied() else {
                    return Err(self.error(name, format!("{name} dimension {dim} is out of range")));
                };
                match range {
                    Some((l, r)) => {
                        let v = match name {
                            "$size" => range_len((l, r)) as i64,
                            "$left" => l,
                            "$right" => r,
                            "$low" => l.min(r),
                            "$high" => l.max(r),
                            _ => if l >= r { 1 } else { -1 },
                        };
                        Ok((konst(cx, v), st))
                    }
                    None => {
                        // A dynamic array or queue: from its size, at run time.
                        let int_ty = Ty::bits(32, true, false);
                        let (size, _) = self.array_method(cx, a, "size", &[])?;
                        let one = konst(cx, 1);
                        let zero = konst(cx, 0);
                        let last = cx.b.emit(Op::Binary(BinOp::Sub, size, one), int, name);
                        let _ = int_ty;
                        Ok((
                            match name {
                                "$size" => size,
                                "$left" | "$low" => zero,
                                "$increment" => cx.b.emit(Op::Const(Bits::from_i64(32, -1)), int, name),
                                _ => last,
                            },
                            st,
                        ))
                    }
                }
            }
            "$bits" => {
                let w = match arg(0) {
                    Some(Expr::Type(t)) => self.resolve_type(t)?.width(),
                    Some(Expr::Ident(n))
                        if matches!(self.lookup_cx(Some(cx), n), Some(Sym::Type(_))) =>
                    {
                        let Some(Sym::Type(t)) = self.lookup_cx(Some(cx), n) else {
                            unreachable!()
                        };
                        t.width()
                    }
                    Some(e) if self.is_str(Some(cx), e) => {
                        // A string's size is its length in bytes.
                        let (len, _) = self.str_method(cx, e, "len", &[])?;
                        let eight = konst(cx, 8);
                        return Ok((cx.b.emit(Op::Binary(BinOp::Mul, len, eight), int, name), st));
                    }
                    Some(e) => match self.path_type(Some(cx), e)? {
                        Some((t, _)) => {
                            let n: u32 = t
                                .unpacked
                                .iter()
                                .map(|d| match d {
                                    UDim::Fixed(l, r) => range_len((*l, *r)),
                                    _ => 1,
                                })
                                .product();
                            t.width() * n
                        }
                        None => self.self_type_cx(Some(cx), e)?.width(),
                    },
                    None => return Err(self.error(name, "$bits needs an argument")),
                };
                Ok((konst(cx, w as i64), st))
            }
            "$clog2" => {
                let Some(a) = arg(0) else {
                    return Err(self.error(name, "$clog2 needs an argument"));
                };
                let n = self.diags.len();
                match self.const_value(a, None) {
                    Ok(Value::Bits(b)) => Ok((konst(cx, b.clog2() as i64), st)),
                    Ok(_) => Err(self.error(a.at(), "$clog2 needs an integral argument")),
                    Err(_) => {
                        self.diags.truncate(n);
                        let (v, _) = self.lower_self(cx, a)?;
                        Ok((
                            cx.b.emit(
                                Op::SysFunc {
                                    func: SysFunc::Clog2,
                                    args: vec![v],
                                },
                                int,
                                name,
                            ),
                            st,
                        ))
                    }
                }
            }
            "$time" | "$stime" | "$realtime" | "$random" | "$urandom" => {
                if cx.const_mode {
                    self.nonconst = Some(name);
                    return Err(Stop);
                }
                let func = match name {
                    "$time" => SysFunc::Time,
                    "$stime" => SysFunc::Stime,
                    "$realtime" => SysFunc::Realtime,
                    "$random" => SysFunc::Random,
                    _ => SysFunc::Urandom,
                };
                let ty = match st {
                    STy::Real => self.add_type(Type::Real),
                    STy::Bits { w, s, f } => self.bt(w, s, f),
                    STy::Str | STy::Class(_) => unreachable!(),
                };
                if arg(0).is_some() && name == "$random" {
                    return Err(self.not_yet(name, "$random with a seed"));
                }
                Ok((cx.b.emit(Op::SysFunc { func, args: vec![] }, ty, name), st))
            }
            "$isunknown" | "$onehot" | "$onehot0" | "$countones" => {
                let Some(a) = arg(0) else {
                    return Err(self.error(name, format!("{name} needs an argument")));
                };
                let (v, _) = self.lower_self(cx, a)?;
                let func = match name {
                    "$isunknown" => SysFunc::IsUnknown,
                    "$onehot" => SysFunc::Onehot,
                    "$onehot0" => SysFunc::Onehot0,
                    _ => SysFunc::Countones,
                };
                let STy::Bits { w, s, f } = st else {
                    unreachable!()
                };
                let ty = self.bt(w, s, f);
                Ok((
                    cx.b.emit(
                        Op::SysFunc {
                            func,
                            args: vec![v],
                        },
                        ty,
                        name,
                    ),
                    st,
                ))
            }
            "$cast" => {
                let (Some(dst), Some(src)) = (arg(0), arg(1)) else {
                    return Err(self.error(name, "$cast needs two arguments"));
                };
                Ok((self.lower_cast(cx, dst, src, name)?, st))
            }
            "$sformatf" | "$psprintf" => {
                let (format, vals) = self.build_format(cx, args, 'd', name)?;
                let stt = self.add_type(Type::String);
                Ok((cx.b.emit(Op::Sformat { format, args: vals }, stt, name), st))
            }
            "$itor" => {
                let Some(a) = arg(0) else {
                    return Err(self.error(name, "$itor needs an argument"));
                };
                Ok((self.int_to_real(cx, a)?, st))
            }
            "$rtoi" => {
                let Some(a) = arg(0) else {
                    return Err(self.error(name, "$rtoi needs an argument"));
                };
                let r = self.lower_real(cx, a)?;
                let t = self.bt(32, true, false);
                Ok((
                    cx.b.emit(
                        Op::Resize {
                            value: r,
                            extend: Extend::Truncate,
                        },
                        t,
                        name,
                    ),
                    st,
                ))
            }
            _ => Err(self.not_yet(name, &format!("system function {name}"))),
        }
    }

    // ------------------------------------------------------------ strings

    /// Is `e` string-valued? (Quietly false if its type can't be worked out.)
    pub(crate) fn is_str(&mut self, cx: Option<&Cx<'a>>, e: &Expr<'a>) -> bool {
        if matches!(e, Expr::Str(_) | Expr::Number(_)) {
            return false;
        }
        let n = self.diags.len();
        let r = self.self_type_cx(cx, e).ok() == Some(STy::Str);
        self.diags.truncate(n);
        r
    }

    /// A string-valued expression. Integral values convert to strings.
    pub(crate) fn lower_str(&mut self, cx: &mut Cx<'a>, e: &Expr<'a>) -> EResult<Val> {
        let stt = self.add_type(Type::String);
        let at = e.at();
        if let Expr::Str(s) = e {
            // `\0` characters are left out of a string (LRM 6.16).
            let mut bytes = super::decode_string_bytes(super::str_body(s));
            bytes.retain(|&b| b != 0);
            let text = crate::eval::latin1(&bytes);
            return Ok(cx.b.emit(Op::ConstStr(text), stt, at));
        }
        // In a string context, a replication of literals is string replication.
        let st = if matches!(e, Expr::Repl { .. }) {
            STy::Str
        } else {
            self.self_type_cx(Some(cx), e)?
        };
        match st {
            STy::Real => return Err(self.error(at, "Real value used as a string")),
            STy::Class(_) => return Err(self.error(at, "Class handle used as a string")),
            STy::Bits { .. } => {
                let (v, _) = self.lower_self(cx, e)?;
                return Ok(cx.b.emit(Op::Convert(v), stt, at));
            }
            STy::Str => {}
        }
        match e {
            Expr::Concat(items) => {
                let mut parts = Vec::new();
                for i in items {
                    if !self.is_zero_repl(i) {
                        parts.push(self.lower_str(cx, i)?);
                    }
                }
                Ok(cx.b.emit(Op::Concat(parts), stt, at))
            }
            Expr::Repl { count, items } => {
                let v = self.lower_str(cx, &Expr::Concat(items.clone()))?;
                let n = self.diags.len();
                let Ok(n) = self.const_int(count) else {
                    // A string may be replicated a variable number of times.
                    self.diags.truncate(n);
                    let c = self.lower_to(cx, count, &Ty::bits(32, true, false))?;
                    return Ok(cx.b.emit(
                        Op::StrFunc {
                            func: StrFunc::Repeat,
                            args: vec![v, c],
                        },
                        stt,
                        at,
                    ));
                };
                Ok(cx.b.emit(
                    Op::Repl {
                        value: v,
                        count: n.max(0) as u32,
                    },
                    stt,
                    at,
                ))
            }
            Expr::Cond {
                cond, then, els, ..
            } => {
                let c = self.truth(cx, cond)?;
                let a = self.lower_str(cx, then)?;
                let b = self.lower_str(cx, els)?;
                Ok(cx.b.emit(
                    Op::Mux {
                        cond: c,
                        then: a,
                        els: b,
                    },
                    stt,
                    at,
                ))
            }
            Expr::Call { func, args } => Ok(self.lower_call(cx, func, args)?.0),
            Expr::SysCall { name, args } => Ok(self.lower_sys(cx, name, args)?.0),
            Expr::Member { base, name } if self.is_str(Some(cx), base) => {
                Ok(self.str_method(cx, base, name, &[])?.0)
            }
            Expr::Ident(_) | Expr::Member { .. } | Expr::Index { .. } | Expr::Scoped { .. } => {
                let Some(p) = self.path(cx, e)? else {
                    return Err(self.not_yet(at, "this kind of name"));
                };
                Ok(self.load_path(cx, &p)?.0)
            }
            Expr::Cast { expr, .. } => self.lower_str(cx, expr),
            other => Err(self.not_yet(other.at(), "this string expression")),
        }
    }

    /// A string method that returns a value: `s.len()`, `s.substr(i, j)`...
    pub(crate) fn str_method(
        &mut self,
        cx: &mut Cx<'a>,
        base: &Expr<'a>,
        name: &'a str,
        args: &[Arg<'a>],
    ) -> EResult<(Val, STy)> {
        let Some(st) = str_method_type(name) else {
            return Err(self.not_yet(name, &format!("string method {name}")));
        };
        let s = self.lower_str(cx, base)?;
        let arg = |i: usize| match args.get(i) {
            Some(Arg::Ordered(Some(e))) => Some(e.clone()),
            _ => None,
        };
        let int = Ty::bits(32, true, false);
        let need = |this: &mut Self, cx: &mut Cx<'a>, i: usize, string: bool| -> EResult<Val> {
            let Some(e) = arg(i) else {
                return Err(
                    this.error(name, format!("Too few arguments to string method '{name}'"))
                );
            };
            if string {
                this.lower_str(cx, &e)
            } else {
                this.lower_to(cx, &e, &int)
            }
        };
        let (func, mut extra) = match name {
            "len" => (StrFunc::Len, vec![]),
            "getc" => (StrFunc::Getc, vec![need(self, cx, 0, false)?]),
            "toupper" => (StrFunc::ToUpper, vec![]),
            "tolower" => (StrFunc::ToLower, vec![]),
            "compare" => (StrFunc::Compare, vec![need(self, cx, 0, true)?]),
            "icompare" => (StrFunc::Icompare, vec![need(self, cx, 0, true)?]),
            "substr" => (
                StrFunc::Substr,
                vec![need(self, cx, 0, false)?, need(self, cx, 1, false)?],
            ),
            "atoi" => (StrFunc::Atoi, vec![]),
            "atohex" => (StrFunc::Atohex, vec![]),
            "atooct" => (StrFunc::Atooct, vec![]),
            "atobin" => (StrFunc::Atobin, vec![]),
            _ => (StrFunc::Atoreal, vec![]),
        };
        let mut all = vec![s];
        all.append(&mut extra);
        let t = match st {
            STy::Str => self.add_type(Type::String),
            STy::Real => self.add_type(Type::Real),
            STy::Bits { w, s, f } => self.bt(w, s, f),
            STy::Class(_) => unreachable!(),
        };
        Ok((cx.b.emit(Op::StrFunc { func, args: all }, t, name), st))
    }

    /// A string method that changes the string: `putc`, `itoa` and friends.
    /// False if `name` is not one.
    pub(crate) fn str_mutate(
        &mut self,
        cx: &mut Cx<'a>,
        base: &Expr<'a>,
        name: &'a str,
        args: &[Arg<'a>],
    ) -> EResult<bool> {
        let arg = |i: usize| match args.get(i) {
            Some(Arg::Ordered(Some(e))) => Some(e.clone()),
            _ => None,
        };
        let func = match name {
            "putc" => StrFunc::Putc,
            "itoa" => StrFunc::Itoa,
            "hextoa" => StrFunc::Hextoa,
            "octtoa" => StrFunc::Octtoa,
            "bintoa" => StrFunc::Bintoa,
            "realtoa" => StrFunc::Realtoa,
            _ => return Ok(false),
        };
        let Some(a0) = arg(0) else {
            return Err(self.error(name, format!("Too few arguments to string method '{name}'")));
        };
        let vals = match func {
            StrFunc::Putc => {
                let Some(a1) = arg(1) else {
                    return Err(self.error(name, "putc needs an index and a character"));
                };
                let s = self.lower_str(cx, base)?;
                let i = self.lower_to(cx, &a0, &Ty::bits(32, true, false))?;
                let c = self.lower_to(cx, &a1, &Ty::bits(8, false, false))?;
                vec![s, i, c]
            }
            StrFunc::Realtoa => vec![self.lower_real(cx, &a0)?],
            _ => vec![self.lower_self(cx, &a0)?.0],
        };
        let stt = self.add_type(Type::String);
        let v = cx.b.emit(Op::StrFunc { func, args: vals }, stt, name);
        self.store_string(cx, base, v)?;
        Ok(true)
    }

    /// Store a string value into `lhs`, converting if it is integral.
    pub(crate) fn store_string(&mut self, cx: &mut Cx<'a>, lhs: &Expr<'a>, v: Val) -> EResult<()> {
        let Some(p) = self.path(cx, lhs)? else {
            return Err(self.error(lhs.at(), "Illegal assignment target"));
        };
        let v = if p.ty.base == Base::Str {
            v
        } else {
            let t = self.bt(p.width, p.ty.signed, p.ty.four_state());
            cx.b.emit(Op::Convert(v), t, lhs.at())
        };
        self.store_path(cx, &p, v, false)
    }

    /// Assign to `lhs`, which may be a concatenation of paths.
    pub(crate) fn assign(
        &mut self,
        cx: &mut Cx<'a>,
        lhs: &Expr<'a>,
        rhs: &Expr<'a>,
        nba: bool,
    ) -> EResult<()> {
        if let Expr::Index { base, index } = lhs
            && self.is_str(Some(cx), base)
        {
            let args = [
                Arg::Ordered(Some((**index).clone())),
                Arg::Ordered(Some(rhs.clone())),
            ];
            self.str_mutate(cx, base, "putc", &args)?;
            return Ok(());
        }
        if let Expr::Concat(parts) = lhs {
            let mut paths = Vec::new();
            for p in parts {
                match self.path(cx, p)? {
                    Some(path) => paths.push(path),
                    None => return Err(self.error(p.at(), "Illegal assignment target")),
                }
            }
            let w: u32 = paths.iter().map(|p| p.width).sum();
            let v = self.lower_to(cx, rhs, &Ty::bits(w, false, true))?;
            return self.store_split(cx, &paths, v, w, nba);
        }
        let Some(p) = self.path(cx, lhs)? else {
            return Err(self.error(lhs.at(), "Illegal assignment target"));
        };
        let v = self.lower_to(cx, rhs, &p.ty)?;
        self.store_path(cx, &p, v, nba)
    }

    /// Store a value already lowered (`v` of type `st`) into `lhs`.
    pub(crate) fn assign_val(
        &mut self,
        cx: &mut Cx<'a>,
        lhs: &Expr<'a>,
        v: Val,
        st: STy,
        nba: bool,
    ) -> EResult<()> {
        let at = lhs.at();
        let paths: Vec<Path<'a>> = match lhs {
            Expr::Concat(parts) => {
                let mut v = Vec::new();
                for p in parts {
                    match self.path(cx, p)? {
                        Some(path) => v.push(path),
                        None => return Err(self.error(p.at(), "Illegal assignment target")),
                    }
                }
                v
            }
            _ => match self.path(cx, lhs)? {
                Some(p) => vec![p],
                None => return Err(self.error(at, "Illegal assignment target")),
            },
        };
        let w: u32 = paths.iter().map(|p| p.width).sum();
        if paths.len() == 1 {
            let p = &paths[0];
            let target = Want {
                w: p.width,
                s: p.ty.signed,
                f: p.ty.four_state(),
            };
            let v = self.resize(cx, v, st, target, at);
            return self.store_path(cx, p, v, nba);
        }
        let v = self.resize(
            cx,
            v,
            st,
            Want {
                w,
                s: false,
                f: true,
            },
            at,
        );
        self.store_split(cx, &paths, v, w, nba)
    }

    /// Store slices of `v` (most significant first) into `paths`.
    fn store_split(
        &mut self,
        cx: &mut Cx<'a>,
        paths: &[Path<'a>],
        v: Val,
        w: u32,
        nba: bool,
    ) -> EResult<()> {
        let mut at_bit = w;
        let it = self.bt(32, true, false);
        for p in paths {
            at_bit -= p.width;
            let lsb =
                cx.b.emit(Op::Const(Bits::from_u64(32, at_bit as u64)), it, p.at);
            let pt = self.bt(p.width, false, true);
            let part = cx.b.emit(
                Op::Select {
                    value: v,
                    lsb,
                    width: p.width,
                },
                pt,
                p.at,
            );
            let target = Want {
                w: p.width,
                s: p.ty.signed,
                f: p.ty.four_state(),
            };
            let part = self.resize(
                cx,
                part,
                STy::Bits {
                    w: p.width,
                    s: false,
                    f: true,
                },
                target,
                p.at,
            );
            self.store_path(cx, p, part, nba)?;
        }
        Ok(())
    }
}
