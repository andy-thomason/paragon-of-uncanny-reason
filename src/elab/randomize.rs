//! `randomize()` (IEEE 1800-2023 §18).
//!
//! A call is lowered in place as a loop: each `rand` property gets a random
//! value from a domain worked out from the simple constraints on it (bounds,
//! `inside` sets, `dist` values); a property a constraint makes equal to an
//! expression of the others is then computed from them; and the whole set of
//! constraints is checked. The loop tries again up to [`TRIES`] times, and the
//! call gives 1 if they hold, else 0. `pre_randomize` and `post_randomize`
//! run before and after, when the class has them.
//!
//! Exact random sequences are not reproduced (decision 3); constraints are.

use super::expr::{Cx, STy};
use super::types::Ty;
use super::{EResult, Elab};
use crate::ast::{ConstraintItem, Expr};
use crate::eval::Value;
use crate::ir::*;

const TRIES: u64 = 2000;

/// Where a `rand` property's values come from.
struct Domain<'a> {
    /// Ranges (inclusive, signed 64-bit) to choose from.
    ranges: Vec<(i64, i64)>,
    /// `x == expr` of other properties: computed rather than chosen.
    equal: Option<Expr<'a>>,
}

impl<'a, 't> Elab<'a, 't> {
    /// `obj.randomize() [with { extra }]` for an object of class `c`.
    pub(crate) fn lower_randomize(
        &mut self,
        cx: &mut Cx<'a>,
        obj: Val,
        c: ClassId,
        extra: &[ConstraintItem<'a>],
        at: &'a str,
    ) -> EResult<Val> {
        self.ensure_class(c)?;
        let ci = c.0 as usize;
        let rand: Vec<(u32, &'a str, Ty<'a>)> = self.classes[ci]
            .rand_fields
            .iter()
            .map(|&i| {
                let (n, t) = &self.classes[ci].fields[i as usize];
                (i, *n, t.clone())
            })
            .collect();
        let class_items: Vec<&'t [ConstraintItem<'a>]> =
            self.classes[ci].constraints.iter().map(|(_, i)| *i).collect();
        let saved = cx.with_obj.replace((obj, c));
        let r = self.randomize_loop(cx, obj, c, &rand, &class_items, extra, at);
        cx.with_obj = saved;
        r
    }

    #[allow(clippy::too_many_arguments)]
    fn randomize_loop(
        &mut self,
        cx: &mut Cx<'a>,
        obj: Val,
        c: ClassId,
        rand: &[(u32, &'a str, Ty<'a>)],
        class_items: &[&[ConstraintItem<'a>]],
        extra: &[ConstraintItem<'a>],
        at: &'a str,
    ) -> EResult<Val> {
        let all: Vec<&ConstraintItem<'a>> = class_items
            .iter()
            .flat_map(|i| i.iter())
            .chain(extra.iter())
            .collect();
        let has = |this: &Self, name: &str| this.classes[c.0 as usize].methods.contains_key(name);
        if has(self, "pre_randomize") {
            self.method_call(cx, c, Some(obj), "pre_randomize", &[], false)?;
        }
        let int = Ty::bits(32, true, false);
        let it = self.ir_type(&int);
        let bit = self.bits_type(1, false, false);
        let ok = cx.b.new_slot(it);
        let tries = cx.b.new_slot(it);
        let zero = cx.b.emit(Op::Const(Bits::zero(32)), it, at);
        for s in [ok, tries] {
            cx.b.effect(
                Op::StoreSlot {
                    slot: s,
                    part: None,
                    value: zero,
                },
                at,
            );
        }
        let (head, body, found, exit) = (
            cx.b.new_block(),
            cx.b.new_block(),
            cx.b.new_block(),
            cx.b.new_block(),
        );
        cx.b.goto(head);
        let n = cx.b.emit(Op::LoadSlot(tries), it, at);
        let limit = cx.b.emit(Op::Const(Bits::from_u64(32, TRIES)), it, at);
        let more = cx.b.emit(Op::Binary(BinOp::Lt, n, limit), bit, at);
        cx.b.terminate(Terminator::Branch {
            cond: more,
            then: (body, vec![]),
            els: (exit, vec![]),
        });
        cx.b.switch_to(body);
        let one = cx.b.emit(Op::Const(Bits::from_u64(32, 1)), it, at);
        let n1 = cx.b.emit(Op::Binary(BinOp::Add, n, one), it, at);
        cx.b.effect(
            Op::StoreSlot {
                slot: tries,
                part: None,
                value: n1,
            },
            at,
        );
        // Choose, then compute the dependent properties.
        let mut derived = Vec::new();
        for (idx, name, ty) in rand {
            if !ty.is_integral() || !ty.unpacked.is_empty() {
                continue;
            }
            let d = self.domain(name, ty, &all);
            let v = match d.equal {
                Some(e) => {
                    derived.push((*idx, ty.clone(), e));
                    continue;
                }
                None => self.random_in(cx, ty, &d.ranges, at),
            };
            self.store_field(cx, obj, *idx, v, at);
        }
        for (idx, ty, e) in derived {
            let v = self.lower_to(cx, &e, &ty)?;
            self.store_field(cx, obj, idx, v, at);
        }
        let holds = self.constraints_hold(cx, &all)?;
        cx.b.terminate(Terminator::Branch {
            cond: holds,
            then: (found, vec![]),
            els: (head, vec![]),
        });
        cx.b.switch_to(found);
        cx.b.effect(
            Op::StoreSlot {
                slot: ok,
                part: None,
                value: one,
            },
            at,
        );
        if has(self, "post_randomize") {
            self.method_call(cx, c, Some(obj), "post_randomize", &[], false)?;
        }
        cx.b.terminate(Terminator::Jump(exit, vec![]));
        cx.b.switch_to(exit);
        Ok(cx.b.emit(Op::LoadSlot(ok), it, at))
    }

    fn store_field(&mut self, cx: &mut Cx<'a>, obj: Val, field: u32, value: Val, at: &'a str) {
        cx.b.effect(
            Op::StoreField {
                obj,
                field,
                part: None,
                value,
            },
            at,
        );
    }

    /// A random value of type `ty` from `ranges` (all values if empty).
    fn random_in(&mut self, cx: &mut Cx<'a>, ty: &Ty<'a>, ranges: &[(i64, i64)], at: &'a str) -> Val {
        let t = self.ir_type(ty);
        if ranges.is_empty() {
            return cx.b.emit(
                Op::SysFunc {
                    func: SysFunc::Urandom,
                    args: vec![],
                },
                t,
                at,
            );
        }
        let lt = self.bits_type(64, true, false);
        let mut args = Vec::new();
        for (lo, hi) in ranges {
            args.push(cx.b.emit(Op::Const(Bits::from_i64(64, *lo)), lt, at));
            args.push(cx.b.emit(Op::Const(Bits::from_i64(64, *hi)), lt, at));
        }
        cx.b.emit(
            Op::SysFunc {
                func: SysFunc::RandomPick,
                args,
            },
            t,
            at,
        )
    }

    /// The domain of property `name` from the constraints that are plain
    /// conjuncts. Anything else is left to the check.
    fn domain(&mut self, name: &str, ty: &Ty<'a>, items: &[&ConstraintItem<'a>]) -> Domain<'a> {
        let w = ty.width();
        let (mut lo, mut hi): (i128, i128) = if w >= 64 {
            (i64::MIN as i128, i64::MAX as i128)
        } else if ty.signed {
            (-(1i128 << (w - 1)), (1i128 << (w - 1)) - 1)
        } else {
            (0, (1i128 << w) - 1)
        };
        let full = (lo, hi);
        let mut sets: Option<Vec<(i128, i128)>> = None;
        let mut equal = None;
        let mut conj: Vec<Expr<'a>> = Vec::new();
        for it in items {
            match it {
                ConstraintItem::Expr(e) | ConstraintItem::Soft(e) => conj.push(e.clone()),
                ConstraintItem::Dist { expr, items } if is_name(expr, name) => {
                    let set: Vec<Expr<'a>> = items
                        .iter()
                        .filter(|(v, _)| !matches!(v, Expr::Keyword("default")))
                        .map(|(v, _)| v.clone())
                        .collect();
                    if let Some(r) = self.const_set(&set) {
                        sets = Some(intersect_sets(sets, r));
                    }
                }
                _ => {}
            }
        }
        while let Some(e) = conj.pop() {
            match &e {
                Expr::Binary { op: "&&", lhs, rhs } => {
                    conj.push((**lhs).clone());
                    conj.push((**rhs).clone());
                }
                Expr::Inside { expr, set } if is_name(expr, name) => {
                    if let Some(r) = self.const_set(set) {
                        sets = Some(intersect_sets(sets, r));
                    }
                }
                Expr::Binary { op, lhs, rhs } if matches!(*op, "<" | "<=" | ">" | ">=" | "==") => {
                    let (op, other) = if is_name(lhs, name) {
                        (*op, &**rhs)
                    } else if is_name(rhs, name) {
                        // Mirror `c < x` to `x > c`.
                        let m = match *op {
                            "<" => ">",
                            "<=" => ">=",
                            ">" => "<",
                            ">=" => "<=",
                            o => o,
                        };
                        (m, &**lhs)
                    } else {
                        continue;
                    };
                    match self.quiet_const(other) {
                        Some(v) => match op {
                            "<" => hi = hi.min(v - 1),
                            "<=" => hi = hi.min(v),
                            ">" => lo = lo.max(v + 1),
                            ">=" => lo = lo.max(v),
                            _ => {
                                lo = lo.max(v);
                                hi = hi.min(v);
                            }
                        },
                        None if op == "==" && !mentions(other, name) => equal = Some(other.clone()),
                        None => {}
                    }
                }
                _ => {}
            }
        }
        let mut ranges: Vec<(i128, i128)> = match sets {
            Some(s) => s
                .into_iter()
                .map(|(a, b)| (a.max(lo), b.min(hi)))
                .filter(|(a, b)| a <= b)
                .collect(),
            None if lo <= hi => vec![(lo, hi)],
            None => Vec::new(),
        };
        if ranges == [full] && w > 64 {
            ranges.clear();
        }
        Domain {
            ranges: ranges
                .into_iter()
                .map(|(a, b)| {
                    (
                        a.clamp(i64::MIN as i128, i64::MAX as i128) as i64,
                        b.clamp(i64::MIN as i128, i64::MAX as i128) as i64,
                    )
                })
                .collect(),
            equal,
        }
    }

    /// Constant values and `[lo:hi]` ranges, if they all are constant.
    fn const_set(&mut self, set: &[Expr<'a>]) -> Option<Vec<(i128, i128)>> {
        let mut r = Vec::new();
        for e in set {
            match e {
                Expr::Range { lo, hi } => r.push((self.quiet_const(lo)?, self.quiet_const(hi)?)),
                e => {
                    let v = self.quiet_const(e)?;
                    r.push((v, v));
                }
            }
        }
        Some(r)
    }

    /// A constant integer, without reporting errors if it is not one.
    fn quiet_const(&mut self, e: &Expr<'a>) -> Option<i128> {
        let n = self.diags.len();
        let st = self.self_type(e).ok();
        let v = self.const_value(e, None).ok();
        self.diags.truncate(n);
        match (v, st) {
            (Some(Value::Bits(b)), Some(STy::Bits { s, .. })) => b.to_i64(s).map(|v| v as i128),
            _ => None,
        }
    }

    /// Do all the constraints hold? A 1-bit value.
    pub(crate) fn constraints_hold(
        &mut self,
        cx: &mut Cx<'a>,
        items: &[&ConstraintItem<'a>],
    ) -> EResult<Val> {
        let bit = self.bits_type(1, false, false);
        let mut acc = cx.b.emit(Op::Const(Bits::ones(1)), bit, "");
        for it in items {
            let v = self.constraint_holds(cx, it)?;
            acc = cx.b.emit(Op::Binary(BinOp::And, acc, v), bit, "");
        }
        Ok(acc)
    }

    fn constraint_holds(&mut self, cx: &mut Cx<'a>, it: &ConstraintItem<'a>) -> EResult<Val> {
        let bit = self.bits_type(1, false, false);
        // A known true or false: an X result counts as false.
        let known = |this: &mut Self, cx: &mut Cx<'a>, v: Val| {
            let one = cx.b.emit(Op::Const(Bits::ones(1)), bit, "");
            let _ = this;
            cx.b.emit(Op::Binary(BinOp::CaseEq, v, one), bit, "")
        };
        Ok(match it {
            ConstraintItem::Expr(e) | ConstraintItem::Soft(e) => {
                let v = self.truth(cx, e)?;
                known(self, cx, v)
            }
            ConstraintItem::Implies(c, items) => {
                let c = self.truth(cx, c)?;
                let c = known(self, cx, c);
                let refs: Vec<&ConstraintItem<'a>> = items.iter().collect();
                let t = self.constraints_hold(cx, &refs)?;
                let nc = cx.b.emit(Op::Unary(UnOp::Not, c), bit, "");
                cx.b.emit(Op::Binary(BinOp::Or, nc, t), bit, "")
            }
            ConstraintItem::If { cond, then, els } => {
                let c = self.truth(cx, cond)?;
                let c = known(self, cx, c);
                let t: Vec<&ConstraintItem<'a>> = then.iter().collect();
                let e: Vec<&ConstraintItem<'a>> = els.iter().collect();
                let t = self.constraints_hold(cx, &t)?;
                let e = self.constraints_hold(cx, &e)?;
                cx.b.emit(
                    Op::Mux {
                        cond: c,
                        then: t,
                        els: e,
                    },
                    bit,
                    "",
                )
            }
            ConstraintItem::Dist { expr, items } => {
                let set: Vec<Expr<'a>> = items
                    .iter()
                    .filter(|(v, _)| !matches!(v, Expr::Keyword("default")))
                    .map(|(v, _)| v.clone())
                    .collect();
                if set.len() < items.len() {
                    // `default` takes anything.
                    cx.b.emit(Op::Const(Bits::ones(1)), bit, "")
                } else {
                    let v = self.lower_inside(cx, expr, &set)?;
                    known(self, cx, v)
                }
            }
            ConstraintItem::Unique(set) => {
                let mut acc = cx.b.emit(Op::Const(Bits::ones(1)), bit, "");
                for (i, a) in set.iter().enumerate() {
                    for b in &set[i + 1..] {
                        let ne = Expr::Binary {
                            op: "!=",
                            lhs: Box::new(a.clone()),
                            rhs: Box::new(b.clone()),
                        };
                        let v = self.truth(cx, &ne)?;
                        let v = known(self, cx, v);
                        acc = cx.b.emit(Op::Binary(BinOp::And, acc, v), bit, "");
                    }
                }
                acc
            }
            ConstraintItem::Foreach { array, .. } => {
                return Err(self.not_yet(array.at(), "foreach constraints"));
            }
            ConstraintItem::Ignored(_) => cx.b.emit(Op::Const(Bits::ones(1)), bit, ""),
        })
    }

    /// `x.randomize() with { ... }`.
    pub(crate) fn lower_with_constraints(
        &mut self,
        cx: &mut Cx<'a>,
        call: &Expr<'a>,
        items: &[ConstraintItem<'a>],
    ) -> EResult<Val> {
        let (base, at) = match call {
            Expr::Call { func, args } if args.is_empty() => match &**func {
                Expr::Member {
                    base,
                    name: "randomize",
                } => (Some(&**base), "randomize"),
                Expr::Ident("randomize") => (None, "randomize"),
                _ => return Err(self.not_yet(call.at(), "inline constraints on this call")),
            },
            _ => return Err(self.not_yet(call.at(), "inline constraints on this call")),
        };
        let (obj, c) = match base {
            Some(b) => {
                let Some(c) = self.handle_class(Some(cx), b) else {
                    return Err(self.not_yet(b.at(), "std::randomize"));
                };
                (self.lower_handle(cx, b, None)?, c)
            }
            None => {
                let Some(c) = self.this_class(cx) else {
                    return Err(self.not_yet(at, "std::randomize"));
                };
                (self.this_handle(cx, at)?, c)
            }
        };
        self.lower_randomize(cx, obj, c, items, at)
    }
}

fn is_name(e: &Expr, name: &str) -> bool {
    matches!(e, Expr::Ident(n) if *n == name)
}

/// Does `e` mention the name?
fn mentions(e: &Expr, name: &str) -> bool {
    match e {
        Expr::Ident(n) => *n == name,
        Expr::Unary { arg, .. } => mentions(arg, name),
        Expr::Binary { lhs, rhs, .. } => mentions(lhs, name) || mentions(rhs, name),
        Expr::Cond { cond, then, els, .. } => {
            mentions(cond, name) || mentions(then, name) || mentions(els, name)
        }
        Expr::Concat(v) => v.iter().any(|x| mentions(x, name)),
        Expr::Index { base, index } => mentions(base, name) || mentions(index, name),
        Expr::Member { base, .. } => mentions(base, name),
        Expr::Call { .. } | Expr::SysCall { .. } => true,
        _ => false,
    }
}

/// The ranges in both `a` (if given) and `b`.
fn intersect_sets(a: Option<Vec<(i128, i128)>>, b: Vec<(i128, i128)>) -> Vec<(i128, i128)> {
    let Some(a) = a else { return b };
    let mut r = Vec::new();
    for (x0, x1) in &a {
        for (y0, y1) in &b {
            let (lo, hi) = ((*x0).max(*y0), (*x1).min(*y1));
            if lo <= hi {
                r.push((lo, hi));
            }
        }
    }
    r
}
