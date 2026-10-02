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
    /// An unpacked array, by linear element: element 0 is the one at the
    /// right-hand index (the last one in an assignment pattern).
    Array(Vec<Value>),
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
            if let (Value::Array(p), Value::Array(q)) = (arg(*x).0, arg(*y).0) {
                let case = matches!(b, BinOp::CaseEq | BinOp::CaseNe);
                let eq = arrays_equal(p, q, case);
                let r = match (b, eq) {
                    (BinOp::Eq | BinOp::CaseEq, Some(e)) => Bits::from_bool(e),
                    (BinOp::Ne | BinOp::CaseNe, Some(e)) => Bits::from_bool(!e),
                    (BinOp::Eq | BinOp::Ne, None) => Bits::all_x(1),
                    _ => return None,
                };
                return Some(fix(r));
            }
            if let (Value::Str(p), Value::Str(q)) = (arg(*x).0, arg(*y).0) {
                let r = match b {
                    BinOp::Eq | BinOp::CaseEq | BinOp::WildEq => p == q,
                    BinOp::Ne | BinOp::CaseNe | BinOp::WildNe => p != q,
                    BinOp::Lt => p < q,
                    BinOp::Le => p <= q,
                    BinOp::Gt => p > q,
                    BinOp::Ge => p >= q,
                    _ => return None,
                };
                return Some(fix(Bits::from_bool(r)));
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
        Op::Concat(parts) if matches!(ty, Type::Unpacked { .. }) => {
            // Parts are given left to right; element 0 is the rightmost.
            let mut v = Vec::new();
            for p in parts.iter().rev() {
                match arg(*p).0 {
                    Value::Array(a) => v.extend(a.iter().cloned()),
                    x => v.push(x.clone()),
                }
            }
            Value::Array(v)
        }
        Op::Tuple(parts) => Value::Array(parts.iter().map(|p| arg(*p).0.clone()).collect()),
        Op::ArrayElem { value, index } => {
            let i = bits(*index).and_then(|b| b.to_i64(signed(*index)));
            match (arg(*value).0, i) {
                (Value::Array(a), Some(i)) if i >= 0 && (i as usize) < a.len() => {
                    a[i as usize].clone()
                }
                _ => default_value(ty),
            }
        }
        Op::ArraySlice { value, start, len } => {
            let s = bits(*start).and_then(|b| b.to_i64(signed(*start)));
            let Value::Array(a) = arg(*value).0 else {
                return None;
            };
            let fill = a.first().cloned().unwrap_or(Value::Bits(Bits::all_x(1)));
            Value::Array(
                (0..*len as i64)
                    .map(|k| match s.map(|s| s + k) {
                        Some(i) if i >= 0 && (i as usize) < a.len() => a[i as usize].clone(),
                        _ => default_like(&fill),
                    })
                    .collect(),
            )
        }
        Op::Concat(parts) if *ty == Type::String => {
            Value::Str(parts.iter().map(|p| to_string(arg(*p).0)).collect())
        }
        Op::Concat(parts) => {
            let parts: Option<Vec<Bits>> = parts.iter().map(|p| bits(*p)).collect();
            fix(Bits::concat(&parts?))
        }
        Op::Repl { value, count } if *ty == Type::String => {
            Value::Str(to_string(arg(*value).0).repeat(*count as usize))
        }
        Op::Repl { value, count } => fix(bits(*value)?.repl(*count)),
        Op::Resize { value, extend } => {
            let (w, _, _) = bits_info(ty)?;
            match arg(*value).0 {
                // A real converted to an integral type rounds to nearest,
                // except for `$rtoi`, which truncates.
                Value::Real(r) if *extend == Extend::Truncate => fix(Bits::from_f64(w, r.trunc())),
                Value::Real(r) => fix(Bits::from_f64(w, r.round())),
                Value::Bits(b) => fix(b.resize(w, *extend == Extend::Sign)),
                Value::Str(_) | Value::Array(_) => return None,
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
            (Value::Bits(b), Type::Real) => Value::Real(bits_to_f64(b, signed(*v))),
            (Value::Real(r), Type::Bits { width, .. }) => fix(Bits::from_f64(*width, r.round())),
            (Value::Bits(b), Type::String) => Value::Str(bits_to_string(b)),
            (Value::Str(s), Type::Bits { width, .. }) => fix(string_to_bits(s, *width)),
            (v, _) => v.clone(),
        },
        Op::StrFunc { func, args } => {
            let a = |i: usize| arg(args[i]).0;
            let int = |i: usize| a(i).bits().and_then(|b| b.to_i64(signed(args[i])));
            let w = bits_info(ty).map_or(32, |(w, _, _)| w);
            let num = |v: i64| fix(Bits::from_i64(w, v));
            let s = to_string(a(0));
            match func {
                StrFunc::Len => num(s.chars().count() as i64),
                StrFunc::Getc => {
                    let c = int(1)
                        .and_then(|i| usize::try_from(i).ok())
                        .and_then(|i| s.chars().nth(i));
                    num(c.map_or(0, |c| c as i64))
                }
                StrFunc::Putc => {
                    let c = int(2).map_or(0, |c| c as u8);
                    let mut chars: Vec<char> = s.chars().collect();
                    if let Some(i) = int(1).and_then(|i| usize::try_from(i).ok())
                        && i < chars.len()
                        && c != 0
                    {
                        chars[i] = c as char;
                    }
                    Value::Str(chars.into_iter().collect())
                }
                StrFunc::ToUpper => Value::Str(s.to_ascii_uppercase()),
                StrFunc::ToLower => Value::Str(s.to_ascii_lowercase()),
                StrFunc::Compare | StrFunc::Icompare => {
                    let t = to_string(a(1));
                    let ord = if *func == StrFunc::Compare {
                        s.cmp(&t)
                    } else {
                        s.to_ascii_lowercase().cmp(&t.to_ascii_lowercase())
                    };
                    num(ord as i64)
                }
                StrFunc::Substr => {
                    let chars: Vec<char> = s.chars().collect();
                    let r = match (int(1), int(2)) {
                        (Some(i), Some(j)) if 0 <= i && i <= j && (j as usize) < chars.len() => {
                            chars[i as usize..=j as usize].iter().collect()
                        }
                        _ => String::new(),
                    };
                    Value::Str(r)
                }
                StrFunc::Atoi => num(parse_radix(&s, 10)),
                StrFunc::Atohex => num(parse_radix(&s, 16)),
                StrFunc::Atooct => num(parse_radix(&s, 8)),
                StrFunc::Atobin => num(parse_radix(&s, 2)),
                StrFunc::Atoreal => Value::Real(parse_real(&s)),
                StrFunc::Itoa | StrFunc::Hextoa | StrFunc::Octtoa | StrFunc::Bintoa => {
                    let Some(b) = a(0).bits() else {
                        return Some(Value::Str(String::new()));
                    };
                    let mut b = b.clone();
                    b.to_two_state();
                    Value::Str(match func {
                        StrFunc::Itoa => match b.to_i64(signed(args[0])) {
                            Some(i) if signed(args[0]) => i.to_string(),
                            _ => to_radix(&b, 10),
                        },
                        StrFunc::Hextoa => to_radix(&b, 16),
                        StrFunc::Octtoa => to_radix(&b, 8),
                        _ => to_radix(&b, 2),
                    })
                }
                StrFunc::Repeat => Value::Str(s.repeat(int(1).unwrap_or(0).max(0) as usize)),
                StrFunc::Realtoa => match a(0) {
                    Value::Real(r) => Value::Str(crate::sim::format_g(*r)),
                    v => Value::Str(to_string(v)),
                },
            }
        }
        _ => return None,
    };
    Some(v)
}

/// Are two arrays equal element by element? `None` if unknown (X bits
/// under `==`).
fn arrays_equal(p: &[Value], q: &[Value], case: bool) -> Option<bool> {
    if p.len() != q.len() {
        return Some(false);
    }
    let mut unknown = false;
    for (a, b) in p.iter().zip(q) {
        match (a, b) {
            (Value::Bits(x), Value::Bits(y)) if case => {
                if !x.case_eq(y) {
                    return Some(false);
                }
            }
            (Value::Bits(x), Value::Bits(y)) => match x.logic_eq(y).truth() {
                Some(false) => return Some(false),
                None => unknown = true,
                Some(true) => {}
            },
            (Value::Array(x), Value::Array(y)) => match arrays_equal(x, y, case) {
                Some(false) => return Some(false),
                None => unknown = true,
                Some(true) => {}
            },
            (a, b) => {
                if a != b {
                    return Some(false);
                }
            }
        }
    }
    if unknown { None } else { Some(true) }
}

/// The default value of something shaped like `v` (for out-of-range elements).
pub fn default_like(v: &Value) -> Value {
    match v {
        Value::Bits(b) => Value::Bits(Bits::all_x(b.width)),
        Value::Real(_) => Value::Real(0.0),
        Value::Str(_) => Value::Str(String::new()),
        Value::Array(a) => Value::Array(a.iter().map(default_like).collect()),
    }
}

/// Text as a string value: one `char` per byte (Latin-1), so every byte value
/// survives conversion to bits and back.
pub fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

/// An integral value as a string: its bytes, most significant first, with
/// zero bytes left out (LRM 6.16). X and Z bits count as 0.
pub fn bits_to_string(b: &Bits) -> String {
    let mut b = b.clone();
    b.to_two_state();
    (0..b.width.div_ceil(8))
        .rev()
        .map(|i| b.select(i as i64 * 8, 8, false).to_u64() as u8)
        .filter(|&c| c != 0)
        .map(|c| c as char)
        .collect()
}

/// A string as an integral value of `width` bits: the last character in
/// the low byte; truncated or zero-extended on the left.
pub fn string_to_bits(s: &str, width: u32) -> Bits {
    let mut b = Bits::zero(width.max(1));
    for (i, c) in s.chars().rev().enumerate() {
        let at = i as u32 * 8;
        if at >= width {
            break;
        }
        b.insert(at as i64, &Bits::from_u64(8, c as u64 & 0xff));
    }
    b.resize(width, false)
}

/// Any value as a string, for string concatenation.
pub fn to_string(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::Bits(b) => bits_to_string(b),
        Value::Real(r) => r.to_string(),
        Value::Array(_) => String::new(),
    }
}

/// `atoi` and friends: an optional sign, then digits of `radix` (and `_`),
/// stopping at the first other character.
fn parse_radix(s: &str, radix: u32) -> i64 {
    let s = s.trim_start();
    let (neg, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let mut v: i64 = 0;
    for c in rest.chars() {
        if c == '_' {
            continue;
        }
        let Some(d) = c.to_digit(radix) else { break };
        v = v.wrapping_mul(radix as i64).wrapping_add(d as i64);
    }
    if neg { v.wrapping_neg() } else { v }
}

/// `atoreal`: the longest prefix that reads as a real number.
fn parse_real(s: &str) -> f64 {
    let s: String = s.trim_start().chars().filter(|&c| c != '_').collect();
    (1..=s.len())
        .rev()
        .find_map(|n| s.get(..n).and_then(|p| p.parse::<f64>().ok()))
        .unwrap_or(0.0)
}

/// Digits of a two-state value in a radix, without leading zeros.
fn to_radix(b: &Bits, radix: u32) -> String {
    if b.is_zero() {
        return "0".into();
    }
    let bits_per = match radix {
        16 => 4,
        8 => 3,
        2 => 1,
        _ => {
            // Decimal: repeated division by 10.
            let mut digits = Vec::new();
            let mut v = b.clone();
            let ten = Bits::from_u64(v.width, 10);
            while !v.is_zero() {
                digits.push(
                    std::char::from_digit(v.div_rem(&ten, false, true).to_u64() as u32, 10)
                        .unwrap(),
                );
                v = v.div_rem(&ten, false, false);
            }
            return digits.iter().rev().collect();
        }
    };
    let n = b.width.div_ceil(bits_per);
    let s: String = (0..n)
        .rev()
        .map(|d| {
            let v = b.select((d * bits_per) as i64, bits_per, false).to_u64() as u32;
            std::char::from_digit(v, radix).unwrap()
        })
        .collect();
    s.trim_start_matches('0').to_string()
}

/// The real value of an integral value; X and Z bits count as 0.
pub fn bits_to_f64(b: &Bits, signed: bool) -> f64 {
    let mut b = b.clone();
    b.to_two_state();
    if let Some(i) = b.to_i64(signed) {
        return if signed { i as f64 } else { i as u64 as f64 };
    }
    let neg = signed && b.msb().0;
    let mag = if neg { b.neg() } else { b };
    let mut r = 0.0;
    for i in (0..mag.width).rev() {
        r = r * 2.0 + if mag.bit(i).0 { 1.0 } else { 0.0 };
    }
    if neg { -r } else { r }
}

fn real_binary(op: BinOp, p: f64, q: f64, fix: &dyn Fn(Bits) -> Value) -> Option<Value> {
    let b = |x: bool| Some(fix(Bits::from_bool(x)));
    match op {
        BinOp::Add => Some(Value::Real(p + q)),
        BinOp::Sub => Some(Value::Real(p - q)),
        BinOp::Mul => Some(Value::Real(p * q)),
        BinOp::Div => Some(Value::Real(p / q)),
        BinOp::Pow => Some(Value::Real(p.powf(q))),
        BinOp::Eq | BinOp::CaseEq | BinOp::WildEq => b(p == q),
        BinOp::Ne | BinOp::CaseNe | BinOp::WildNe => b(p != q),
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

/// The initial value of a variable of type `ty`: X for 4-state bits, an
/// array of defaults for an unpacked array.
pub fn default_for(types: &[Type<'_>], ty: &Type<'_>) -> Value {
    match ty {
        Type::Unpacked { elem, left, right } => {
            let n = (right - left).unsigned_abs() as usize + 1;
            Value::Array(vec![default_for(types, &types[elem.0 as usize]); n])
        }
        t => default_value(t),
    }
}

/// Store `value` into element `index` of the array `dst` (or consecutive
/// elements, for an array value), into bits `part` if given. Element values
/// are converted to `elem`. Returns whether anything changed.
pub fn store_elem(
    dst: &mut Value,
    index: i64,
    part: Option<(i64, u32)>,
    value: Value,
    elem: &Type<'_>,
) -> bool {
    let Value::Array(a) = dst else {
        return false;
    };
    let vals = match value {
        Value::Array(v) if part.is_none() => v,
        v => vec![v],
    };
    let mut changed = false;
    for (k, v) in vals.into_iter().enumerate() {
        let i = index + k as i64;
        if i < 0 || i as usize >= a.len() {
            continue;
        }
        let new = convert_into(&a[i as usize], part, v, elem);
        if a[i as usize] != new {
            a[i as usize] = new;
            changed = true;
        }
    }
    changed
}

/// The stored value after writing `value` (into `part`, if given) over `old`,
/// converted to the stored type.
pub fn convert_into(old: &Value, part: Option<(i64, u32)>, value: Value, ty: &Type<'_>) -> Value {
    match (old, part, value, ty) {
        (
            _,
            None,
            Value::Bits(b),
            Type::Bits {
                width, four_state, ..
            },
        ) => {
            let mut b = if b.width == *width {
                b
            } else {
                b.resize(*width, false)
            };
            if !four_state {
                b.to_two_state();
            }
            Value::Bits(b)
        }
        (Value::Bits(o), Some((lsb, w)), Value::Bits(b), Type::Bits { four_state, .. }) => {
            let mut n = o.clone();
            let mut b = if b.width == w { b } else { b.resize(w, false) };
            if !four_state {
                b.to_two_state();
            }
            n.insert(lsb, &b);
            Value::Bits(n)
        }
        (_, _, v, _) => v,
    }
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
        .map(|t| default_for(types, &types[t.0 as usize]))
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
                Op::StoreSlotElem {
                    slot,
                    index,
                    part,
                    value,
                } => {
                    let int = |v: &Val| {
                        vals[v.0 as usize]
                            .as_ref()
                            .and_then(Value::bits)
                            .and_then(|b| b.to_i64(true))
                    };
                    let v = vals[value.0 as usize].clone().ok_or(NotConst::NoResult)?;
                    let elem = match &types[body.slots[slot.0 as usize].0 as usize] {
                        Type::Unpacked { elem, .. } => &types[elem.0 as usize],
                        _ => return Err(NotConst::Impure(inst.at.to_string())),
                    };
                    let p = match part {
                        Some(p) => match int(&p.lsb) {
                            Some(l) => Some((l, p.width)),
                            None => None,
                        },
                        None => None,
                    };
                    if let Some(i) = int(index) {
                        store_elem(&mut slots[slot.0 as usize], i, p, v, elem);
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
                Op::Sformat { format, args } => {
                    let a: Vec<(Value, &Type)> = args
                        .iter()
                        .map(|v| {
                            let (v, t) = get(*v);
                            (v.clone(), t)
                        })
                        .collect();
                    let text = crate::sim::format_display(
                        &design.formats[format.0 as usize],
                        &a,
                        0,
                        design.precision,
                        design.precision,
                    );
                    Some(Value::Str(text))
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
