//! Evaluation of pure IR instructions.
//!
//! Elaboration uses this for constant expressions: a parameter's value is
//! computed by lowering its expression to a small [`Body`] and running it
//! here, so constant folding and simulation share one evaluator. The
//! interpreter reuses [`eval_pure`] for every pure instruction.

use crate::bits::{Rel, Shift};
use crate::ir::*;

/// A run-time value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Bits(Bits),
    Real(f64),
    Str(String),
}

impl Value {
    pub fn bits(&self) -> Option<&Bits> {
        match self {
            Value::Bits(b) => Some(b),
            _ => None,
        }
    }
}

/// Width, signedness and 4-state-ness of an integral type.
pub fn bits_info(ty: &Type<'_>) -> Option<(u32, bool, bool)> {
    match ty {
        Type::Bits {
            width,
            signed,
            four_state,
            ..
        } => Some((*width, *signed, *four_state)),
        _ => None,
    }
}

/// Evaluate a pure instruction. `arg` gives each operand's value and type;
/// `ty` is the result type. Returns `None` for an instruction with effects.
pub fn eval_pure<'v, 't: 'v>(
    op: &Op,
    ty: &Type<'t>,
    arg: impl Fn(Val) -> (&'v Value, &'v Type<'t>),
) -> Option<Value> {
    let bits = |v: Val| arg(v).0.bits().cloned();
    let signed = |v: Val| bits_info(arg(v).1).is_some_and(|(_, s, _)| s);
    let out_four = bits_info(ty).is_none_or(|(_, _, f)| f);
    let fix = |mut b: Bits| {
        if !out_four {
            b.to_two_state();
        }
        Value::Bits(b)
    };
    let v = match op {
        Op::Const(b) => fix(b.clone()),
        Op::ConstReal(r) => Value::Real(*r),
        Op::ConstStr(s) => Value::Str(s.clone()),
        Op::Unary(u, a) => {
            if let Value::Real(r) = arg(*a).0 {
                return Some(match u {
                    UnOp::Neg => Value::Real(-r),
                    UnOp::LogNot => fix(Bits::from_bool(*r == 0.0)),
                    _ => return None,
                });
            }
            let a = bits(*a)?;
            fix(match u {
                UnOp::Neg => a.neg(),
                UnOp::Not => a.not(),
                UnOp::LogNot => match a.truth() {
                    Some(t) => Bits::from_bool(!t),
                    None => Bits::all_x(1),
                },
                UnOp::RedAnd => a.reduce_and(),
                UnOp::RedNand => a.reduce_and().not(),
                UnOp::RedOr => a.reduce_or(),
                UnOp::RedNor => a.reduce_or().not(),
                UnOp::RedXor => a.reduce_xor(),
                UnOp::RedXnor => a.reduce_xor().not(),
            })
        }
        Op::Binary(b, x, y) => {
            if let (Value::Real(p), Value::Real(q)) = (arg(*x).0, arg(*y).0) {
                return real_binary(*b, *p, *q, &fix);
            }
            let (p, q) = (bits(*x)?, bits(*y)?);
            let s = signed(*x) && signed(*y);
            fix(match b {
                BinOp::Add => p.add(&q),
                BinOp::Sub => p.sub(&q),
                BinOp::Mul => p.mul(&q),
                BinOp::Div => p.div_rem(&q, s, false),
                BinOp::Mod => p.div_rem(&q, s, true),
                BinOp::Pow => p.pow(&q, signed(*x), signed(*y)),
                BinOp::And => p.and(&q),
                BinOp::Or => p.or(&q),
                BinOp::Xor => p.xor(&q),
                BinOp::Xnor => p.xnor(&q),
                BinOp::Shl => p.shift(&q, Shift::Left),
                BinOp::Shr => p.shift(&q, Shift::Right),
                BinOp::AShr => p.shift(
                    &q,
                    if signed(*x) {
                        Shift::Arith
                    } else {
                        Shift::Right
                    },
                ),
                BinOp::Eq => p.logic_eq(&q),
                BinOp::Ne => p.logic_eq(&q).not(),
                BinOp::CaseEq => Bits::from_bool(p.case_eq(&q)),
                BinOp::CaseNe => Bits::from_bool(!p.case_eq(&q)),
                BinOp::WildEq => p.wild_eq(&q),
                BinOp::WildNe => p.wild_eq(&q).not(),
                BinOp::CaseZEq => Bits::from_bool(p.case_match(&q, false)),
                BinOp::CaseXEq => Bits::from_bool(p.case_match(&q, true)),
                BinOp::Lt => p.relational(&q, s, Rel::Lt),
                BinOp::Le => p.relational(&q, s, Rel::Le),
                BinOp::Gt => p.relational(&q, s, Rel::Gt),
                BinOp::Ge => p.relational(&q, s, Rel::Ge),
            })
        }
        Op::Select { value, lsb, width } => {
            let v = bits(*value)?;
            let l = bits(*lsb)?;
            let four = bits_info(arg(*value).1).is_some_and(|(_, _, f)| f);
            match l.to_i64(signed(*lsb)) {
                Some(l) => fix(v.select(l, *width, four)),
                None => fix(Bits::all_x(*width)),
            }
        }
        Op::Concat(parts) => {
            let parts: Option<Vec<Bits>> = parts.iter().map(|p| bits(*p)).collect();
            fix(Bits::concat(&parts?))
        }
        Op::Repl { value, count } => fix(bits(*value)?.repl(*count)),
        Op::Resize { value, extend } => {
            let (w, _, _) = bits_info(ty)?;
            match arg(*value).0 {
                // A real converted to an integral type rounds to nearest.
                Value::Real(r) => fix(Bits::from_i64(w, r.round() as i64)),
                Value::Bits(b) => fix(b.resize(w, *extend == Extend::Sign)),
                Value::Str(_) => return None,
            }
        }
        Op::Mux { cond, then, els } => match arg(*cond).0.bits()?.truth() {
            Some(true) => arg(*then).0.clone(),
            Some(false) => arg(*els).0.clone(),
            None => {
                // X condition: merge bit by bit (LRM 11.4.11).
                let (a, b) = (bits(*then)?, bits(*els)?);
                let same = a.xnor(&b);
                let mut r = a.and(&same);
                for i in 0..r.width {
                    if same.bit(i) != (true, false) {
                        r.insert(i as i64, &Bits::all_x(1));
                    }
                }
                fix(r)
            }
        },
        Op::Convert(v) => match (arg(*v).0, ty) {
            (Value::Bits(b), Type::Real) => {
                let s = signed(*v);
                Value::Real(match b.to_i64(s) {
                    Some(i) if s => i as f64,
                    Some(i) => i as u64 as f64,
                    None => 0.0,
                })
            }
            (Value::Real(r), Type::Bits { width, .. }) => {
                fix(Bits::from_i64(*width, r.round() as i64))
            }
            (v, _) => v.clone(),
        },
        _ => return None,
    };
    Some(v)
}

fn real_binary(op: BinOp, p: f64, q: f64, fix: &dyn Fn(Bits) -> Value) -> Option<Value> {
    let b = |x: bool| Some(fix(Bits::from_bool(x)));
    match op {
        BinOp::Add => Some(Value::Real(p + q)),
        BinOp::Sub => Some(Value::Real(p - q)),
        BinOp::Mul => Some(Value::Real(p * q)),
        BinOp::Div => Some(Value::Real(p / q)),
        BinOp::Pow => Some(Value::Real(p.powf(q))),
        BinOp::Eq | BinOp::CaseEq => b(p == q),
        BinOp::Ne | BinOp::CaseNe => b(p != q),
        BinOp::Lt => b(p < q),
        BinOp::Le => b(p <= q),
        BinOp::Gt => b(p > q),
        BinOp::Ge => b(p >= q),
        _ => None,
    }
}

/// Why a body could not be evaluated as a constant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotConst {
    /// The instruction at this source position reads state or has an effect.
    Impure(String),
    /// Too many steps: a loop that does not end.
    TooLong,
    NoResult,
}

/// Run a body that only computes, returning the value it returns. Calls to
/// functions in `design` and frame slots are allowed (constant functions);
/// design state is not.
pub fn eval_const(body: &Body<'_>, design: &Design<'_>) -> Result<Value, NotConst> {
    eval_body(body, design, Vec::new(), 0)
}

fn default_value(ty: &Type<'_>) -> Value {
    match ty {
        Type::Bits {
            width,
            four_state: true,
            ..
        } => Value::Bits(Bits::all_x(*width)),
        Type::Bits { width, .. } => Value::Bits(Bits::zero(*width)),
        Type::Real => Value::Real(0.0),
        _ => Value::Str(String::new()),
    }
}

fn eval_body(
    body: &Body<'_>,
    design: &Design<'_>,
    args: Vec<Value>,
    depth: usize,
) -> Result<Value, NotConst> {
    if depth > 1000 {
        return Err(NotConst::TooLong);
    }
    let types = &design.types;
    let mut vals: Vec<Option<Value>> = vec![None; body.vals.len()];
    let mut slots: Vec<Value> = body
        .slots
        .iter()
        .map(|t| default_value(&types[t.0 as usize]))
        .collect();
    let mut block = BlockId(0);
    let mut incoming: Vec<Value> = args;
    for _ in 0..10_000_000 {
        let b = &body.blocks[block.0 as usize];
        for (p, v) in b.params.iter().zip(incoming.drain(..)) {
            vals[p.0 as usize] = Some(v);
        }
        for inst in &b.insts {
            let ty = inst
                .dst
                .map_or(&Type::Real, |d| &types[body.vals[d.0 as usize].0 as usize]);
            let get = |v: Val| {
                (
                    vals[v.0 as usize]
                        .as_ref()
                        .expect("value defined before use"),
                    &types[body.vals[v.0 as usize].0 as usize],
                )
            };
            let r = match &inst.op {
                Op::LoadSlot(s) => Some(slots[s.0 as usize].clone()),
                Op::StoreSlot { slot, part, value } => {
                    let v = vals[value.0 as usize].clone().ok_or(NotConst::NoResult)?;
                    match (part, &mut slots[slot.0 as usize], v) {
                        (None, dst, v) => *dst = v,
                        (Some(p), Value::Bits(dst), Value::Bits(v)) => {
                            let lsb = vals[p.lsb.0 as usize]
                                .as_ref()
                                .and_then(Value::bits)
                                .and_then(|b| b.to_i64(true));
                            if let Some(l) = lsb {
                                dst.insert(l, &v);
                            }
                        }
                        _ => return Err(NotConst::Impure(inst.at.to_string())),
                    }
                    None
                }
                Op::Call { func, args } => {
                    let f = &design.funcs[func.0 as usize];
                    if f.body.blocks.is_empty() {
                        return Err(NotConst::Impure(inst.at.to_string()));
                    }
                    let a: Vec<Value> = args
                        .iter()
                        .map(|v| vals[v.0 as usize].clone().unwrap())
                        .collect();
                    Some(eval_body(&f.body, design, a, depth + 1)?)
                }
                op => Some(
                    eval_pure(op, ty, get).ok_or_else(|| NotConst::Impure(inst.at.to_string()))?,
                ),
            };
            if let (Some(d), Some(r)) = (inst.dst, r) {
                vals[d.0 as usize] = Some(r);
            }
        }
        let take = |v: &Val| vals[v.0 as usize].clone().ok_or(NotConst::NoResult);
        match &b.term {
            Terminator::Return(Some(v)) => return take(v),
            Terminator::Return(None) => return Ok(Value::Bits(Bits::zero(1))),
            Terminator::Jump(t, args) => {
                incoming = args.iter().map(take).collect::<Result<_, _>>()?;
                block = *t;
            }
            Terminator::Branch { cond, then, els } => {
                let c = vals[cond.0 as usize]
                    .as_ref()
                    .and_then(Value::bits)
                    .and_then(Bits::truth)
                    .unwrap_or(false);
                let (t, args) = if c { then } else { els };
                incoming = args.iter().map(take).collect::<Result<_, _>>()?;
                block = *t;
            }
            _ => return Err(NotConst::NoResult),
        }
    }
    Err(NotConst::TooLong)
}
