//! Concurrent assertions (IEEE 1800-2023 §16), the commonly used part.
//!
//! Each assertion becomes a process that waits for its clock. On every tick
//! (unless `disable iff` holds) it starts an *attempt*: a thread that follows
//! the property from that tick, waiting for later ticks as `##` delays and
//! repetitions need. An attempt that fails runs the `else` action, or
//! reports an error; one that succeeds runs the pass action (for `cover`,
//! only that). `disable iff` is checked again at every tick of an attempt
//! and abandons it quietly.
//!
//! Matching is single-threaded: a delay range or repetition range takes
//! the first match it can, rather than every possible one.
//!
//! `$past`, `$rose`, `$fell`, `$stable`, `$changed` read hidden registers
//! that shift on the clock, made when the assertion is lowered.

use super::expr::{Cx, STy};
use super::stmt::PendingWait;
use super::types::Ty;
use super::{EResult, Elab};
use crate::ast::{self, Expr, Prop, Seq, Timing};
use crate::ir::*;
use std::collections::HashMap;

/// The longest a `[*]`, `[->]` or `##[m:$]` waits, in clock ticks.
const UNBOUNDED: i64 = 100_000;

impl<'a, 't> Elab<'a, 't> {
    /// A named property or sequence declared in scope.
    fn prop_decl(&self, name: &str) -> Option<&'t ast::ModuleItem<'a>> {
        let mut s = Some(self.cur);
        while let Some(id) = s {
            if let Some(i) = self.scopes[id.0 as usize].props.get(name) {
                return Some(*i);
            }
            s = self.scopes[id.0 as usize].lex_parent;
        }
        None
    }

    /// The default clock (`default clocking`) and `default disable iff` in scope.
    fn default_clock(&self) -> (Option<&'t Timing<'a>>, Option<&'t Expr<'a>>) {
        let (mut clock, mut disable) = (None, None);
        let mut s = Some(self.cur);
        while let Some(id) = s {
            let info = &self.scopes[id.0 as usize];
            clock = clock.or(info.default_clock);
            disable = disable.or(info.default_disable);
            s = info.lex_parent;
        }
        (clock, disable)
    }

    /// Lower a concurrent assertion into a process.
    pub(crate) fn lower_assertion(&mut self, a: &'t ast::Assertion<'a>) -> EResult<()> {
        let at = a.kw;
        self.last_at = at;
        let mut spec = a.spec.clone();
        // A named property supplies its own clock and disable condition.
        if let Prop::Seq(Seq::Expr(e)) = &spec.prop
            && let Some((name, args)) = call_parts(e)
            && let Some(ast::ModuleItem::PropertyDecl { ports, spec: ps, .. }) = self.prop_decl(name)
        {
            let map = bind_args(ports, &args);
            let inner = subst_spec(ps, &map);
            spec.clock = spec.clock.or(inner.clock);
            spec.disable = spec.disable.or(inner.disable);
            spec.prop = inner.prop;
        }
        let (dclock, ddisable) = self.default_clock();
        let clock = match spec.clock.clone().or_else(|| dclock.cloned()) {
            Some(c) => c,
            None => return Err(self.not_yet(at, "unclocked concurrent assertions")),
        };
        let disable = spec.disable.clone().or_else(|| ddisable.cloned());
        let prop = self.expand_prop(&spec.prop, 0)?;
        // Hidden registers for `$past` and friends, by expression text.
        let mut past = HashMap::new();
        self.make_past_regs(&prop, &clock, &mut past)?;

        let mut cx = Cx::new(false);
        cx.past = past;
        let mut waits: Vec<PendingWait> = Vec::new();
        let clock_t: &'t Timing<'a> = self.made_timing.alloc(clock.clone());
        // The process: wait for the clock, then start an attempt.
        let top = cx.b.new_block();
        cx.b.goto(top);
        self.lower_timing(&mut cx, clock_t)?;
        let skip = match &disable {
            Some(d) => self.truth(&mut cx, d)?,
            None => {
                let bit = self.bits_type(1, false, false);
                cx.b.emit(Op::Const(Bits::zero(1)), bit, at)
            }
        };
        let (start, attempt) = (cx.b.new_block(), cx.b.new_block());
        cx.b.terminate(Terminator::Branch {
            cond: skip,
            then: (top, vec![]),
            els: (start, vec![]),
        });
        cx.b.switch_to(start);
        cx.b.terminate(Terminator::Fork {
            children: vec![attempt],
            join: Join::None,
            resume: top,
        });
        cx.b.switch_to(attempt);
        let tick = Tick {
            clock: clock_t,
            disable: disable.clone(),
        };
        let ok = self.check_prop(&mut cx, &prop, &tick)?;
        let (pass_b, fail_b) = (cx.b.new_block(), cx.b.new_block());
        cx.b.terminate(Terminator::Branch {
            cond: ok,
            then: (pass_b, vec![]),
            els: (fail_b, vec![]),
        });
        cx.b.switch_to(pass_b);
        if let Some(p) = &a.pass {
            self.lower_stmt(&mut cx, p, &mut waits)?;
        }
        cx.b.terminate(Terminator::EndThread);
        cx.b.switch_to(fail_b);
        if a.kw != "cover" {
            match &a.fail {
                Some(f) => self.lower_stmt(&mut cx, f, &mut waits)?,
                None => {
                    let path = self.d.scope_path(self.cur);
                    let text = self
                        .sm
                        .derive(format!("Assertion failed in {path}: '{}' failed.", a.kw), at);
                    let format = self.add_format(vec![FormatPiece::Text(text)]);
                    cx.b.effect(
                        Op::Report {
                            severity: ReportSeverity::Error,
                            format: Some(format),
                            args: vec![],
                        },
                        at,
                    );
                }
            }
        }
        cx.b.terminate(Terminator::EndThread);
        cx.b.switch_to(BlockId(0));
        self.push_process(cx, ProcKind::Always, at, waits);
        Ok(())
    }

    /// Replace references to named properties and sequences by their bodies.
    fn expand_prop(&mut self, p: &Prop<'a>, depth: u32) -> EResult<Prop<'a>> {
        if depth > 50 {
            return Err(self.error(self.last_at, "Recursive property or sequence"));
        }
        Ok(match p {
            Prop::Seq(s) => {
                if let Seq::Expr(e) = s
                    && let Some((name, args)) = call_parts(e)
                    && let Some(ast::ModuleItem::PropertyDecl { ports, spec, .. }) = self.prop_decl(name)
                {
                    let map = bind_args(ports, &args);
                    let inner = subst_spec(spec, &map);
                    return self.expand_prop(&inner.prop, depth + 1);
                }
                Prop::Seq(self.expand_seq(s, depth)?)
            }
            Prop::Implies { ante, overlap, cons } => Prop::Implies {
                ante: self.expand_seq(ante, depth)?,
                overlap: *overlap,
                cons: Box::new(self.expand_prop(cons, depth)?),
            },
            Prop::Not(x) => Prop::Not(Box::new(self.expand_prop(x, depth)?)),
            Prop::And(x, y) => Prop::And(
                Box::new(self.expand_prop(x, depth)?),
                Box::new(self.expand_prop(y, depth)?),
            ),
            Prop::Or(x, y) => Prop::Or(
                Box::new(self.expand_prop(x, depth)?),
                Box::new(self.expand_prop(y, depth)?),
            ),
            Prop::If { cond, then, els } => Prop::If {
                cond: cond.clone(),
                then: Box::new(self.expand_prop(then, depth)?),
                els: match els {
                    Some(e) => Some(Box::new(self.expand_prop(e, depth)?)),
                    None => None,
                },
            },
            Prop::Unsupported(k) => return Err(self.not_yet(k, &format!("property operator {k}"))),
        })
    }

    fn expand_seq(&mut self, s: &Seq<'a>, depth: u32) -> EResult<Seq<'a>> {
        Ok(match s {
            Seq::Expr(e) => {
                if let Some((name, args)) = call_parts(e)
                    && let Some(ast::ModuleItem::SequenceDecl { ports, seq, .. }) = self.prop_decl(name)
                {
                    let map = bind_args(ports, &args);
                    let inner = subst_seq(seq, &map);
                    return self.expand_seq(&inner, depth + 1);
                }
                Seq::Expr(e.clone())
            }
            Seq::Delay { lhs, min, max, rhs } => Seq::Delay {
                lhs: match lhs {
                    Some(l) => Some(Box::new(self.expand_seq(l, depth)?)),
                    None => None,
                },
                min: min.clone(),
                max: max.clone(),
                rhs: Box::new(self.expand_seq(rhs, depth)?),
            },
            Seq::Repeat { seq, kind, min, max } => Seq::Repeat {
                seq: Box::new(self.expand_seq(seq, depth)?),
                kind,
                min: min.clone(),
                max: max.clone(),
            },
            Seq::Binary { op, lhs, rhs } => Seq::Binary {
                op,
                lhs: Box::new(self.expand_seq(lhs, depth)?),
                rhs: Box::new(self.expand_seq(rhs, depth)?),
            },
        })
    }

    /// Does this property (on its own) take no clock ticks?
    fn instant_prop(p: &Prop) -> bool {
        match p {
            Prop::Seq(s) => Self::instant_seq(s),
            Prop::Not(x) => Self::instant_prop(x),
            Prop::And(x, y) | Prop::Or(x, y) => Self::instant_prop(x) && Self::instant_prop(y),
            Prop::If { then, els, .. } => {
                Self::instant_prop(then) && els.as_ref().is_none_or(|e| Self::instant_prop(e))
            }
            _ => false,
        }
    }

    fn instant_seq(s: &Seq) -> bool {
        match s {
            Seq::Expr(_) => true,
            Seq::Binary { op, lhs, rhs } if matches!(*op, "and" | "or" | "intersect") => {
                Self::instant_seq(lhs) && Self::instant_seq(rhs)
            }
            _ => false,
        }
    }

    /// The property holds for the attempt starting now: a 1-bit value.
    fn check_prop(&mut self, cx: &mut Cx<'a>, p: &Prop<'a>, tick: &Tick<'a, 't>) -> EResult<Val> {
        let bit = self.bits_type(1, false, false);
        match p {
            Prop::Seq(s) => self.check_seq(cx, s, tick),
            Prop::Not(x) => {
                let v = self.check_prop(cx, x, tick)?;
                Ok(cx.b.emit(Op::Unary(UnOp::Not, v), bit, ""))
            }
            Prop::And(x, y) | Prop::Or(x, y) => {
                if !Self::instant_prop(x) || !Self::instant_prop(y) {
                    return Err(self.not_yet(self.last_at, "and/or of temporal properties"));
                }
                let a = self.check_prop(cx, x, tick)?;
                let b = self.check_prop(cx, y, tick)?;
                let op = if matches!(p, Prop::And(..)) {
                    BinOp::And
                } else {
                    BinOp::Or
                };
                Ok(cx.b.emit(Op::Binary(op, a, b), bit, ""))
            }
            Prop::If { cond, then, els } => {
                let c = self.truth(cx, cond)?;
                let c = self.known(cx, c);
                let r = cx.b.new_slot(bit);
                let (tb, eb, join) = (cx.b.new_block(), cx.b.new_block(), cx.b.new_block());
                cx.b.terminate(Terminator::Branch {
                    cond: c,
                    then: (tb, vec![]),
                    els: (eb, vec![]),
                });
                cx.b.switch_to(tb);
                let v = self.check_prop(cx, then, tick)?;
                self.set_slot(cx, r, v);
                cx.b.terminate(Terminator::Jump(join, vec![]));
                cx.b.switch_to(eb);
                let v = match els {
                    Some(e) => self.check_prop(cx, e, tick)?,
                    None => cx.b.emit(Op::Const(Bits::ones(1)), bit, ""),
                };
                self.set_slot(cx, r, v);
                cx.b.terminate(Terminator::Jump(join, vec![]));
                cx.b.switch_to(join);
                Ok(cx.b.emit(Op::LoadSlot(r), bit, ""))
            }
            Prop::Implies { ante, overlap, cons } => {
                // No antecedent match: the property holds (vacuously).
                let m = self.check_seq(cx, ante, tick)?;
                let r = cx.b.new_slot(bit);
                let one = cx.b.emit(Op::Const(Bits::ones(1)), bit, "");
                self.set_slot(cx, r, one);
                let (go, join) = (cx.b.new_block(), cx.b.new_block());
                cx.b.terminate(Terminator::Branch {
                    cond: m,
                    then: (go, vec![]),
                    els: (join, vec![]),
                });
                cx.b.switch_to(go);
                if !overlap {
                    self.tick(cx, tick)?;
                }
                let v = self.check_prop(cx, cons, tick)?;
                self.set_slot(cx, r, v);
                cx.b.terminate(Terminator::Jump(join, vec![]));
                cx.b.switch_to(join);
                Ok(cx.b.emit(Op::LoadSlot(r), bit, ""))
            }
            Prop::Unsupported(k) => Err(self.not_yet(k, &format!("property operator {k}"))),
        }
    }

    /// Does the sequence match from now (ending at the tick this returns at)?
    fn check_seq(&mut self, cx: &mut Cx<'a>, s: &Seq<'a>, tick: &Tick<'a, 't>) -> EResult<Val> {
        let bit = self.bits_type(1, false, false);
        match s {
            Seq::Expr(e) => {
                let v = self.truth(cx, e)?;
                Ok(self.known(cx, v))
            }
            Seq::Binary { op, lhs, rhs } if matches!(*op, "and" | "or" | "intersect") => {
                if !Self::instant_seq(lhs) || !Self::instant_seq(rhs) {
                    return Err(self.not_yet(op, &format!("sequence {op} of temporal sequences")));
                }
                let a = self.check_seq(cx, lhs, tick)?;
                let b = self.check_seq(cx, rhs, tick)?;
                let o = if *op == "or" { BinOp::Or } else { BinOp::And };
                Ok(cx.b.emit(Op::Binary(o, a, b), bit, ""))
            }
            Seq::Binary { op, lhs, rhs } if *op == "throughout" => {
                // `e throughout s`: s, with e holding at each of its ticks
                // (checked at its start and end here).
                let e0 = self.check_seq(cx, lhs, tick)?;
                let m = self.check_seq(cx, rhs, tick)?;
                let e1 = self.check_seq(cx, lhs, tick)?;
                let both = cx.b.emit(Op::Binary(BinOp::And, e0, m), bit, "");
                Ok(cx.b.emit(Op::Binary(BinOp::And, both, e1), bit, ""))
            }
            Seq::Binary { op, .. } => Err(self.not_yet(op, &format!("sequence operator {op}"))),
            Seq::Delay { lhs, min, max, rhs } => {
                let r = cx.b.new_slot(bit);
                let zero = cx.b.emit(Op::Const(Bits::zero(1)), bit, "");
                self.set_slot(cx, r, zero);
                let done = cx.b.new_block();
                if let Some(l) = lhs {
                    let m = self.check_seq(cx, l, tick)?;
                    let go = cx.b.new_block();
                    cx.b.terminate(Terminator::Branch {
                        cond: m,
                        then: (go, vec![]),
                        els: (done, vec![]),
                    });
                    cx.b.switch_to(go);
                }
                let lo = self.const_int(min)?;
                for _ in 0..lo {
                    self.tick(cx, tick)?;
                }
                let span = match max {
                    None => 0,
                    Some(None) => UNBOUNDED,
                    Some(Some(m)) => (self.const_int(m)? - lo).max(0),
                };
                // Try the rest at each tick of the range; the first match wins.
                let it = self.bits_type(32, true, false);
                let k = cx.b.new_slot(it);
                let z = cx.b.emit(Op::Const(Bits::zero(32)), it, "");
                self.set_slot(cx, k, z);
                let (head, again) = (cx.b.new_block(), cx.b.new_block());
                cx.b.goto(head);
                let m = self.check_seq(cx, rhs, tick)?;
                let hit = cx.b.new_block();
                cx.b.terminate(Terminator::Branch {
                    cond: m,
                    then: (hit, vec![]),
                    els: (again, vec![]),
                });
                cx.b.switch_to(hit);
                let one = cx.b.emit(Op::Const(Bits::ones(1)), bit, "");
                self.set_slot(cx, r, one);
                cx.b.terminate(Terminator::Jump(done, vec![]));
                cx.b.switch_to(again);
                let kv = cx.b.emit(Op::LoadSlot(k), it, "");
                let lim = cx.b.emit(Op::Const(Bits::from_i64(32, span)), it, "");
                let more = cx.b.emit(Op::Binary(BinOp::Lt, kv, lim), bit, "");
                let next = cx.b.new_block();
                cx.b.terminate(Terminator::Branch {
                    cond: more,
                    then: (next, vec![]),
                    els: (done, vec![]),
                });
                cx.b.switch_to(next);
                let one32 = cx.b.emit(Op::Const(Bits::from_i64(32, 1)), it, "");
                let k1 = cx.b.emit(Op::Binary(BinOp::Add, kv, one32), it, "");
                self.set_slot(cx, k, k1);
                self.tick(cx, tick)?;
                cx.b.terminate(Terminator::Jump(head, vec![]));
                cx.b.switch_to(done);
                Ok(cx.b.emit(Op::LoadSlot(r), bit, ""))
            }
            Seq::Repeat { seq, kind, min, max } => {
                let n = self.const_int(min)?;
                let r = cx.b.new_slot(bit);
                let done = cx.b.new_block();
                if *kind == "*" {
                    // Consecutive: n matches in a row (more are not needed).
                    let one = cx.b.emit(Op::Const(Bits::ones(1)), bit, "");
                    self.set_slot(cx, r, one);
                    let _ = max;
                    for i in 0..n {
                        if i > 0 {
                            self.tick(cx, tick)?;
                        }
                        let m = self.check_seq(cx, seq, tick)?;
                        let go = cx.b.new_block();
                        let fail = cx.b.new_block();
                        cx.b.terminate(Terminator::Branch {
                            cond: m,
                            then: (go, vec![]),
                            els: (fail, vec![]),
                        });
                        cx.b.switch_to(fail);
                        let zero = cx.b.emit(Op::Const(Bits::zero(1)), bit, "");
                        self.set_slot(cx, r, zero);
                        cx.b.terminate(Terminator::Jump(done, vec![]));
                        cx.b.switch_to(go);
                    }
                    cx.b.terminate(Terminator::Jump(done, vec![]));
                } else {
                    // `[->n]` and `[=n]`: wait for the nth match.
                    let it = self.bits_type(32, true, false);
                    let count = cx.b.new_slot(it);
                    let waited = cx.b.new_slot(it);
                    let z = cx.b.emit(Op::Const(Bits::zero(32)), it, "");
                    self.set_slot(cx, count, z);
                    self.set_slot(cx, waited, z);
                    let zero = cx.b.emit(Op::Const(Bits::zero(1)), bit, "");
                    self.set_slot(cx, r, zero);
                    let head = cx.b.new_block();
                    cx.b.goto(head);
                    let m = self.check_seq(cx, seq, tick)?;
                    let c = cx.b.emit(Op::LoadSlot(count), it, "");
                    let one32 = cx.b.emit(Op::Const(Bits::from_i64(32, 1)), it, "");
                    let m32 = cx.b.emit(
                        Op::Resize {
                            value: m,
                            extend: Extend::Zero,
                        },
                        it,
                        "",
                    );
                    let c1 = cx.b.emit(Op::Binary(BinOp::Add, c, m32), it, "");
                    self.set_slot(cx, count, c1);
                    let target = cx.b.emit(Op::Const(Bits::from_i64(32, n)), it, "");
                    let reached = cx.b.emit(Op::Binary(BinOp::Ge, c1, target), bit, "");
                    let (hit, more) = (cx.b.new_block(), cx.b.new_block());
                    cx.b.terminate(Terminator::Branch {
                        cond: reached,
                        then: (hit, vec![]),
                        els: (more, vec![]),
                    });
                    cx.b.switch_to(hit);
                    let one = cx.b.emit(Op::Const(Bits::ones(1)), bit, "");
                    self.set_slot(cx, r, one);
                    cx.b.terminate(Terminator::Jump(done, vec![]));
                    cx.b.switch_to(more);
                    let w = cx.b.emit(Op::LoadSlot(waited), it, "");
                    let w1 = cx.b.emit(Op::Binary(BinOp::Add, w, one32), it, "");
                    self.set_slot(cx, waited, w1);
                    let lim = cx.b.emit(Op::Const(Bits::from_i64(32, UNBOUNDED)), it, "");
                    let ok = cx.b.emit(Op::Binary(BinOp::Lt, w1, lim), bit, "");
                    let next = cx.b.new_block();
                    cx.b.terminate(Terminator::Branch {
                        cond: ok,
                        then: (next, vec![]),
                        els: (done, vec![]),
                    });
                    cx.b.switch_to(next);
                    self.tick(cx, tick)?;
                    cx.b.terminate(Terminator::Jump(head, vec![]));
                }
                cx.b.switch_to(done);
                Ok(cx.b.emit(Op::LoadSlot(r), bit, ""))
            }
        }
    }

    /// Wait for the next clock tick; abandon the attempt if disabled.
    fn tick(&mut self, cx: &mut Cx<'a>, tick: &Tick<'a, 't>) -> EResult<()> {
        self.lower_timing(cx, tick.clock)?;
        if let Some(d) = &tick.disable {
            let c = self.truth(cx, d)?;
            let c = self.known(cx, c);
            let (stop, go) = (cx.b.new_block(), cx.b.new_block());
            cx.b.terminate(Terminator::Branch {
                cond: c,
                then: (stop, vec![]),
                els: (go, vec![]),
            });
            cx.b.switch_to(stop);
            cx.b.terminate(Terminator::EndThread);
            cx.b.switch_to(go);
        }
        Ok(())
    }

    /// 1 only for a known 1 (X counts as false).
    fn known(&mut self, cx: &mut Cx<'a>, v: Val) -> Val {
        let bit = self.bits_type(1, false, false);
        let one = cx.b.emit(Op::Const(Bits::ones(1)), bit, "");
        cx.b.emit(Op::Binary(BinOp::CaseEq, v, one), bit, "")
    }

    fn set_slot(&mut self, cx: &mut Cx<'a>, slot: SlotId, value: Val) {
        cx.b.effect(
            Op::StoreSlot {
                slot,
                part: None,
                value,
            },
            "",
        );
    }

    /// Make the registers `$past` and friends read, for every use in `p`.
    fn make_past_regs(
        &mut self,
        p: &Prop<'a>,
        clock: &Timing<'a>,
        regs: &mut HashMap<String, Vec<VarId>>,
    ) -> EResult<()> {
        let mut exprs = Vec::new();
        collect_prop_exprs(p, &mut exprs);
        let mut uses = Vec::new();
        for e in exprs {
            collect_past(&e, &mut uses);
        }
        for (arg, depth) in uses {
            let key = format!("{arg:?}");
            let have = regs.get(&key).map_or(0, |v| v.len());
            if have >= depth {
                continue;
            }
            let st = self.self_type(&arg)?;
            let ty = match st {
                STy::Bits { .. } | STy::Real => st.ty(),
                _ => return Err(self.not_yet(arg.at(), "$past of this type")),
            };
            let clock_t: &'t Timing<'a> = self.made_timing.alloc(clock.clone());
            let vars = self.past_registers(&arg, &ty, depth, clock_t)?;
            regs.insert(key, vars);
        }
        Ok(())
    }

    /// `depth` registers holding `e` from 1, 2, ... ticks ago.
    fn past_registers(
        &mut self,
        e: &Expr<'a>,
        ty: &Ty<'a>,
        depth: usize,
        clock: &'t Timing<'a>,
    ) -> EResult<Vec<VarId>> {
        let mut vars = Vec::new();
        for i in 0..depth {
            let name = self.sm.derive(format!("__past{}_{i}", self.d.vars.len()), e.at());
            let v = self.declare_var(name, ty.clone(), VarKind::Variable, e.at());
            vars.push(v);
        }
        let mut cx = Cx::new(false);
        let top = cx.b.new_block();
        cx.b.goto(top);
        self.lower_timing(&mut cx, clock)?;
        // Shift: the oldest first, so each takes its neighbour's old value.
        let irt = self.ir_type(ty);
        for i in (0..depth).rev() {
            let value = if i == 0 {
                self.lower_to(&mut cx, e, ty)?
            } else {
                cx.b.emit(Op::Load(vars[i - 1]), irt, e.at())
            };
            cx.b.effect(
                Op::NbaStore {
                    var: vars[i],
                    part: None,
                    value,
                },
                e.at(),
            );
        }
        cx.b.terminate(Terminator::Jump(top, vec![]));
        cx.b.switch_to(BlockId(0));
        self.push_process(cx, ProcKind::Always, e.at(), Vec::new());
        Ok(vars)
    }

    /// `$past(e, n)`, `$rose(e)` and the like, using the registers made for
    /// the assertion being lowered.
    pub(crate) fn lower_sampled(
        &mut self,
        cx: &mut Cx<'a>,
        name: &'a str,
        args: &[ast::Arg<'a>],
    ) -> EResult<(Val, STy)> {
        let Some(ast::Arg::Ordered(Some(e))) = args.first() else {
            return Err(self.error(name, format!("{name} needs an argument")));
        };
        if name == "$sampled" {
            return self.lower_self(cx, e);
        }
        let depth = match (name, args.get(1)) {
            ("$past", Some(ast::Arg::Ordered(Some(n)))) => self.const_int(n)?.max(1) as usize,
            _ => 1,
        };
        let key = format!("{e:?}");
        let Some(regs) = cx.past.get(&key).cloned() else {
            return Err(self.not_yet(name, &format!("{name} outside a clocked assertion")));
        };
        let st = self.self_type_cx(Some(cx), e)?;
        let ty = st.ty();
        let irt = self.ir_type(&ty);
        let prev = cx.b.emit(Op::Load(regs[depth - 1]), irt, name);
        if name == "$past" {
            return Ok((prev, st));
        }
        let bit = self.bits_type(1, false, false);
        let (cur, _) = self.lower_self(cx, e)?;
        let v = match name {
            "$stable" => cx.b.emit(Op::Binary(BinOp::CaseEq, cur, prev), bit, name),
            "$changed" => cx.b.emit(Op::Binary(BinOp::CaseNe, cur, prev), bit, name),
            _ => {
                // `$rose`: the low bit was not 1 and now is; `$fell` the reverse.
                let it = self.bits_type(32, true, false);
                let zero = cx.b.emit(Op::Const(Bits::zero(32)), it, name);
                let lo_t = self.bits_type(1, false, true);
                let c0 = cx.b.emit(
                    Op::Select {
                        value: cur,
                        lsb: zero,
                        width: 1,
                    },
                    lo_t,
                    name,
                );
                let p0 = cx.b.emit(
                    Op::Select {
                        value: prev,
                        lsb: zero,
                        width: 1,
                    },
                    lo_t,
                    name,
                );
                let one = cx.b.emit(Op::Const(Bits::ones(1)), lo_t, name);
                let zero1 = cx.b.emit(Op::Const(Bits::zero(1)), lo_t, name);
                let (now, before) = if name == "$rose" { (one, zero1) } else { (zero1, one) };
                let a = cx.b.emit(Op::Binary(BinOp::CaseEq, c0, now), bit, name);
                let b = cx.b.emit(Op::Binary(BinOp::CaseNe, p0, now), bit, name);
                let _ = before;
                cx.b.emit(Op::Binary(BinOp::And, a, b), bit, name)
            }
        };
        Ok((
            v,
            STy::Bits {
                w: 1,
                s: false,
                f: false,
            },
        ))
    }
}

/// The clock of an attempt, and its disable condition.
struct Tick<'a, 't> {
    clock: &'t Timing<'a>,
    disable: Option<Expr<'a>>,
}

/// `name` or `name(args)`.
fn call_parts<'a>(e: &Expr<'a>) -> Option<(&'a str, Vec<Expr<'a>>)> {
    match e {
        Expr::Ident(n) => Some((n, Vec::new())),
        Expr::Call { func, args } => match &**func {
            Expr::Ident(n) => Some((
                n,
                args.iter()
                    .filter_map(|a| match a {
                        ast::Arg::Ordered(Some(e)) => Some(e.clone()),
                        _ => None,
                    })
                    .collect(),
            )),
            _ => None,
        },
        _ => None,
    }
}

fn bind_args<'a>(ports: &[&'a str], args: &[Expr<'a>]) -> HashMap<&'a str, Expr<'a>> {
    ports.iter().copied().zip(args.iter().cloned()).collect()
}

fn subst_spec<'a>(s: &ast::PropSpec<'a>, map: &HashMap<&'a str, Expr<'a>>) -> ast::PropSpec<'a> {
    ast::PropSpec {
        clock: s.clock.clone(),
        disable: s.disable.as_ref().map(|e| subst_expr(e, map)),
        prop: subst_prop(&s.prop, map),
    }
}

fn subst_prop<'a>(p: &Prop<'a>, map: &HashMap<&'a str, Expr<'a>>) -> Prop<'a> {
    match p {
        Prop::Seq(s) => Prop::Seq(subst_seq(s, map)),
        Prop::Implies { ante, overlap, cons } => Prop::Implies {
            ante: subst_seq(ante, map),
            overlap: *overlap,
            cons: Box::new(subst_prop(cons, map)),
        },
        Prop::Not(x) => Prop::Not(Box::new(subst_prop(x, map))),
        Prop::And(x, y) => Prop::And(Box::new(subst_prop(x, map)), Box::new(subst_prop(y, map))),
        Prop::Or(x, y) => Prop::Or(Box::new(subst_prop(x, map)), Box::new(subst_prop(y, map))),
        Prop::If { cond, then, els } => Prop::If {
            cond: subst_expr(cond, map),
            then: Box::new(subst_prop(then, map)),
            els: els.as_ref().map(|e| Box::new(subst_prop(e, map))),
        },
        Prop::Unsupported(k) => Prop::Unsupported(k),
    }
}

fn subst_seq<'a>(s: &Seq<'a>, map: &HashMap<&'a str, Expr<'a>>) -> Seq<'a> {
    match s {
        Seq::Expr(e) => Seq::Expr(subst_expr(e, map)),
        Seq::Delay { lhs, min, max, rhs } => Seq::Delay {
            lhs: lhs.as_ref().map(|l| Box::new(subst_seq(l, map))),
            min: subst_expr(min, map),
            max: max.clone(),
            rhs: Box::new(subst_seq(rhs, map)),
        },
        Seq::Repeat { seq, kind, min, max } => Seq::Repeat {
            seq: Box::new(subst_seq(seq, map)),
            kind,
            min: subst_expr(min, map),
            max: max.clone(),
        },
        Seq::Binary { op, lhs, rhs } => Seq::Binary {
            op,
            lhs: Box::new(subst_seq(lhs, map)),
            rhs: Box::new(subst_seq(rhs, map)),
        },
    }
}

/// Replace formal argument names by the actual expressions.
fn subst_expr<'a>(e: &Expr<'a>, map: &HashMap<&'a str, Expr<'a>>) -> Expr<'a> {
    if map.is_empty() {
        return e.clone();
    }
    let b = |x: &Expr<'a>| Box::new(subst_expr(x, map));
    match e {
        Expr::Ident(n) => match map.get(n) {
            Some(r) => r.clone(),
            None => e.clone(),
        },
        Expr::Unary { op, arg } => Expr::Unary { op, arg: b(arg) },
        Expr::Binary { op, lhs, rhs } => Expr::Binary {
            op,
            lhs: b(lhs),
            rhs: b(rhs),
        },
        Expr::Cond {
            op,
            cond,
            then,
            els,
        } => Expr::Cond {
            op,
            cond: b(cond),
            then: b(then),
            els: b(els),
        },
        Expr::Index { base, index } => Expr::Index {
            base: b(base),
            index: b(index),
        },
        Expr::Member { base, name } => Expr::Member {
            base: b(base),
            name,
        },
        Expr::Concat(v) => Expr::Concat(v.iter().map(|x| subst_expr(x, map)).collect()),
        Expr::SysCall { name, args } => Expr::SysCall {
            name,
            args: args
                .iter()
                .map(|a| match a {
                    ast::Arg::Ordered(Some(x)) => ast::Arg::Ordered(Some(subst_expr(x, map))),
                    other => other.clone(),
                })
                .collect(),
        },
        Expr::Call { func, args } => Expr::Call {
            func: func.clone(),
            args: args
                .iter()
                .map(|a| match a {
                    ast::Arg::Ordered(Some(x)) => ast::Arg::Ordered(Some(subst_expr(x, map))),
                    other => other.clone(),
                })
                .collect(),
        },
        other => other.clone(),
    }
}

/// The Boolean expressions in a property.
fn collect_prop_exprs<'a>(p: &Prop<'a>, out: &mut Vec<Expr<'a>>) {
    match p {
        Prop::Seq(s) => collect_seq_exprs(s, out),
        Prop::Implies { ante, cons, .. } => {
            collect_seq_exprs(ante, out);
            collect_prop_exprs(cons, out);
        }
        Prop::Not(x) => collect_prop_exprs(x, out),
        Prop::And(x, y) | Prop::Or(x, y) => {
            collect_prop_exprs(x, out);
            collect_prop_exprs(y, out);
        }
        Prop::If { cond, then, els } => {
            out.push(cond.clone());
            collect_prop_exprs(then, out);
            if let Some(e) = els {
                collect_prop_exprs(e, out);
            }
        }
        Prop::Unsupported(_) => {}
    }
}

fn collect_seq_exprs<'a>(s: &Seq<'a>, out: &mut Vec<Expr<'a>>) {
    match s {
        Seq::Expr(e) => out.push(e.clone()),
        Seq::Delay { lhs, rhs, .. } => {
            if let Some(l) = lhs {
                collect_seq_exprs(l, out);
            }
            collect_seq_exprs(rhs, out);
        }
        Seq::Repeat { seq, .. } => collect_seq_exprs(seq, out),
        Seq::Binary { lhs, rhs, .. } => {
            collect_seq_exprs(lhs, out);
            collect_seq_exprs(rhs, out);
        }
    }
}

/// `$past(e, n)` and friends in an expression: each argument and how far back.
fn collect_past<'a>(e: &Expr<'a>, out: &mut Vec<(Expr<'a>, usize)>) {
    match e {
        Expr::SysCall { name, args }
            if matches!(*name, "$past" | "$rose" | "$fell" | "$stable" | "$changed") =>
        {
            if let Some(ast::Arg::Ordered(Some(a))) = args.first() {
                let depth = match (name, args.get(1)) {
                    (&"$past", Some(ast::Arg::Ordered(Some(Expr::Number(n))))) => {
                        n.parse::<usize>().unwrap_or(1).max(1)
                    }
                    _ => 1,
                };
                out.push((a.clone(), depth));
                collect_past(a, out);
            }
        }
        Expr::Unary { arg, .. } => collect_past(arg, out),
        Expr::Binary { lhs, rhs, .. } => {
            collect_past(lhs, out);
            collect_past(rhs, out);
        }
        Expr::Cond { cond, then, els, .. } => {
            collect_past(cond, out);
            collect_past(then, out);
            collect_past(els, out);
        }
        Expr::Index { base, index } => {
            collect_past(base, out);
            collect_past(index, out);
        }
        Expr::Concat(v) => v.iter().for_each(|x| collect_past(x, out)),
        Expr::SysCall { args, .. } | Expr::Call { args, .. } => {
            for a in args {
                if let ast::Arg::Ordered(Some(x)) = a {
                    collect_past(x, out);
                }
            }
        }
        _ => {}
    }
}
