//! Statements, timing controls, processes, functions, gates and `$display` formats.

use super::expr::{Cx, STy, Want};
use super::types::{Base, Ty, UDim};
use super::{EResult, Elab, Fixup, Sym};
use crate::ast::{self, Arg, Expr, Stmt};
use crate::bits::{Literal, parse_literal};
use crate::eval::Value;
use crate::ir::*;
use std::collections::{BTreeSet, HashMap};

/// An implicit wait (`@*`) whose read set is known; finished when the process is pushed.
pub(crate) struct PendingWait {
    pub block: BlockId,
    pub reads: BTreeSet<VarId>,
    pub resume: BlockId,
}

impl<'a, 't> Elab<'a, 't> {
    // ------------------------------------------------------------ processes

    pub(crate) fn lower_process(&mut self, kw: &'a str, stmt: &'t Stmt<'a>) -> EResult<()> {
        self.last_at = kw;
        self.predeclare_blocks(self.cur, stmt);
        let mut cx = Cx::new(false);
        let mut waits = Vec::new();
        match kw {
            "initial" | "final" => {
                self.lower_stmt(&mut cx, stmt, &mut waits)?;
                cx.b.terminate(Terminator::Return(None));
                let kind = if kw == "initial" {
                    ProcKind::Initial
                } else {
                    ProcKind::Final
                };
                self.push_process(cx, kind, kw, waits);
            }
            "always" | "always_ff" => {
                if !has_timing(stmt) {
                    return Err(self.not_yet(kw, "always blocks without a timing control"));
                }
                self.lower_stmt(&mut cx, stmt, &mut waits)?;
                cx.b.terminate(Terminator::Jump(BlockId(0), Vec::new()));
                let kind = if kw == "always" {
                    ProcKind::Always
                } else {
                    ProcKind::AlwaysFf
                };
                self.push_process(cx, kind, kw, waits);
            }
            "always_comb" | "always_latch" => {
                self.lower_stmt(&mut cx, stmt, &mut waits)?;
                let kind = if kw == "always_comb" {
                    ProcKind::AlwaysComb
                } else {
                    ProcKind::AlwaysLatch
                };
                self.push_comb(cx, kind, kw, waits);
            }
            _ => return Err(self.error(kw, format!("Unknown process kind {kw}"))),
        }
        Ok(())
    }

    /// Push a finished process body, turning its pending waits into fixups.
    fn push_process(&mut self, cx: Cx<'a>, kind: ProcKind, at: &'a str, waits: Vec<PendingWait>) {
        let (body, calls) = cx.b.finish(Terminator::Unreachable);
        let proc = self.d.procs.len();
        for w in waits {
            self.fixups.push(Fixup {
                proc,
                block: w.block,
                reads: w.reads,
                calls: calls.clone(),
                resume: w.resume,
            });
        }
        self.d.procs.push(Process {
            kind,
            scope: self.cur,
            body,
            at,
        });
    }

    /// End a combinational body with "wait for any input to change, then run again".
    fn push_comb(
        &mut self,
        mut cx: Cx<'a>,
        kind: ProcKind,
        at: &'a str,
        mut waits: Vec<PendingWait>,
    ) {
        if !cx.b.is_terminated() {
            let block = cx.b.current();
            cx.b.terminate(Terminator::Suspend {
                wait: Wait::AnyChange(Vec::new()),
                resume: BlockId(0),
            });
            waits.push(PendingWait {
                block,
                reads: cx.b.reads.clone(),
                resume: BlockId(0),
            });
        }
        self.push_process(cx, kind, at, waits);
    }

    /// `assign lhs = rhs;`
    pub(crate) fn cont_assign(
        &mut self,
        lhs: &'t Expr<'a>,
        rhs: &'t Expr<'a>,
        at: &'a str,
    ) -> EResult<()> {
        let mut cx = Cx::new(false);
        self.assign(&mut cx, lhs, rhs, false)?;
        self.push_comb(cx, ProcKind::ContAssign, at, Vec::new());
        Ok(())
    }

    /// An input port driven by an expression in the parent.
    pub(crate) fn cont_assign_to_var(
        &mut self,
        var: VarId,
        ty: &Ty<'a>,
        e: &Expr<'a>,
    ) -> EResult<()> {
        let mut cx = Cx::new(false);
        let v = self.lower_to(&mut cx, e, ty)?;
        cx.b.effect(
            Op::Store {
                var,
                part: None,
                value: v,
            },
            e.at(),
        );
        self.push_comb(cx, ProcKind::ContAssign, e.at(), Vec::new());
        Ok(())
    }

    /// An output port driving an expression in the parent.
    pub(crate) fn cont_assign_from_var(
        &mut self,
        e: &Expr<'a>,
        var: VarId,
        ty: &Ty<'a>,
    ) -> EResult<()> {
        let mut cx = Cx::new(false);
        let irt = self.ir_type(ty);
        let v = cx.b.emit(Op::Load(var), irt, e.at());
        self.store_value(&mut cx, e, v, ty)?;
        self.push_comb(cx, ProcKind::ContAssign, e.at(), Vec::new());
        Ok(())
    }

    /// The driver for a net initialiser, or the time-0 store for a variable's.
    pub(crate) fn init_process(
        &mut self,
        var: VarId,
        ty: Ty<'a>,
        init: &'t Expr<'a>,
        net: bool,
    ) -> EResult<()> {
        let mut cx = Cx::new(false);
        let at = init.at();
        let v = self.lower_to(&mut cx, init, &ty)?;
        cx.b.effect(
            Op::Store {
                var,
                part: None,
                value: v,
            },
            at,
        );
        if net {
            self.push_comb(cx, ProcKind::ContAssign, at, Vec::new());
        } else {
            cx.b.terminate(Terminator::Return(None));
            let (body, _) = cx.b.finish(Terminator::Unreachable);
            self.init_procs.push(Process {
                kind: ProcKind::Initial,
                scope: self.cur,
                body,
                at,
            });
        }
        Ok(())
    }

    /// A gate primitive as a continuous assignment.
    pub(crate) fn gate(&mut self, g: &'t ast::Gate<'a>) -> EResult<()> {
        for (_, args) in &g.insts {
            let mut cx = Cx::new(false);
            let at = g.kind;
            match g.kind {
                "and" | "nand" | "or" | "nor" | "xor" | "xnor" => {
                    let [out, ins @ ..] = &args[..] else {
                        return Err(self.error(at, "Gate needs an output"));
                    };
                    if ins.is_empty() {
                        return Err(self.error(at, "Gate needs inputs"));
                    }
                    let (_, ot) = self.lower_self_type(&mut cx, out)?;
                    let want = Want {
                        w: ot.0,
                        s: false,
                        f: true,
                    };
                    let t = self.bits_type(want.w, false, true);
                    let op = match g.kind {
                        "and" | "nand" => BinOp::And,
                        "or" | "nor" => BinOp::Or,
                        _ => BinOp::Xor,
                    };
                    let mut acc = self.lower(&mut cx, &ins[0], want)?;
                    for i in &ins[1..] {
                        let v = self.lower(&mut cx, i, want)?;
                        acc = cx.b.emit(Op::Binary(op, acc, v), t, at);
                    }
                    if matches!(g.kind, "nand" | "nor" | "xnor") {
                        acc = cx.b.emit(Op::Unary(UnOp::Not, acc), t, at);
                    }
                    self.assign_val(
                        &mut cx,
                        out,
                        acc,
                        STy::Bits {
                            w: want.w,
                            s: false,
                            f: true,
                        },
                        false,
                    )?;
                }
                "buf" | "not" => {
                    let [outs @ .., input] = &args[..] else {
                        return Err(self.error(at, "Gate needs an input"));
                    };
                    for out in outs {
                        let (_, ot) = self.lower_self_type(&mut cx, out)?;
                        let want = Want {
                            w: ot.0,
                            s: false,
                            f: true,
                        };
                        let t = self.bits_type(want.w, false, true);
                        let mut v = self.lower(&mut cx, input, want)?;
                        if g.kind == "not" {
                            v = cx.b.emit(Op::Unary(UnOp::Not, v), t, at);
                        }
                        self.assign_val(
                            &mut cx,
                            out,
                            v,
                            STy::Bits {
                                w: want.w,
                                s: false,
                                f: true,
                            },
                            false,
                        )?;
                    }
                }
                k => return Err(self.not_yet(k, &format!("{k} gates"))),
            }
            self.push_comb(cx, ProcKind::ContAssign, at, Vec::new());
        }
        Ok(())
    }

    /// The width of an lvalue, for gates.
    fn lower_self_type(&mut self, cx: &mut Cx<'a>, e: &Expr<'a>) -> EResult<((), (u32, bool))> {
        let st = self.self_type_cx(Some(cx), e)?;
        match st {
            STy::Bits { w, s, .. } => Ok(((), (w, s))),
            _ => Err(self.error(e.at(), "Gate terminal must be integral")),
        }
    }

    // ------------------------------------------------------------ functions

    pub(crate) fn lower_func(&mut self, id: FuncId, f: &'t ast::Subroutine<'a>) -> EResult<()> {
        self.last_at = f.name;
        let sig = self.sig(id)?;
        let mut cx = Cx::new(false);
        cx.is_func = true;
        // A method's first argument is the object.
        let method = self.func_class.get(&id).copied();
        if let Some((c, false)) = method {
            let ty = Self::class_ty(c);
            let irt = self.ir_type(&ty);
            let v = cx.b.block_param(BlockId(0), irt);
            let slot = cx.b.new_slot(irt);
            cx.b.effect(
                Op::StoreSlot {
                    slot,
                    part: None,
                    value: v,
                },
                f.name,
            );
            cx.locals[0].insert("this", Sym::Slot(slot, ty));
        }
        let mut out_slots = Vec::new();
        for (i, (name, ty)) in sig.params.iter().enumerate() {
            let irt = self.ir_type(ty);
            let v = cx.b.block_param(BlockId(0), irt);
            let slot = cx.b.new_slot(irt);
            if sig.dirs[i] != super::Dir::Input {
                out_slots.push((slot, irt));
            }
            cx.b.effect(
                Op::StoreSlot {
                    slot,
                    part: None,
                    value: v,
                },
                name,
            );
            cx.locals[0].insert(name, Sym::Slot(slot, ty.clone()));
        }
        if let Some(ret) = &sig.ret {
            let irt = self.ir_type(ret);
            let slot = cx.b.new_slot(irt);
            cx.locals[0].insert(f.name, Sym::Slot(slot, ret.clone()));
            cx.ret = Some((slot, ret.clone()));
        }
        if !out_slots.is_empty() {
            cx.exit = Some(cx.b.new_block());
        }
        let mut waits = Vec::new();
        for d in &f.decls {
            if matches!(d, ast::ModuleItem::Port(_)) {
                continue;
            }
            self.block_decl(&mut cx, d, true)?;
        }
        let mut stmts = &f.stmts[..];
        if let Some((c, false)) = method
            && f.name == "new"
        {
            // A constructor: the base class's constructor (here, unless the
            // body starts with `super.new`), then the property initialisers.
            let explicit = matches!(
                stmts.first(),
                Some(Stmt::Expr(Expr::Call { func, .. }))
                    if matches!(&**func, Expr::Member { base, name: "new" } if matches!(&**base, Expr::Keyword("super")))
            );
            if explicit {
                self.lower_stmt(&mut cx, &stmts[0], &mut waits)?;
                stmts = &stmts[1..];
                self.field_inits(&mut cx, c, f.name)?;
            } else {
                self.ctor_prologue(&mut cx, c, false, f.name)?;
            }
        }
        if f.kw == "task" {
            self.tagged(&mut cx, DisableTag::Task(id), |this, cx| {
                for s in stmts {
                    this.lower_stmt(cx, s, &mut waits)?;
                }
                Ok(())
            })?;
        } else {
            for s in stmts {
                self.lower_stmt(&mut cx, s, &mut waits)?;
            }
        }
        if let Some(exit) = cx.exit {
            // Every return comes here: the value, then the outputs.
            cx.b.terminate(Terminator::Jump(exit, vec![]));
            cx.b.switch_to(exit);
            let mut vals = Vec::new();
            if let Some((slot, ret)) = cx.ret.clone() {
                let irt = self.ir_type(&ret);
                vals.push(cx.b.emit(Op::LoadSlot(slot), irt, f.end));
            }
            for (slot, irt) in &out_slots {
                vals.push(cx.b.emit(Op::LoadSlot(*slot), *irt, f.end));
            }
            let tt = self.tuple_type(&sig);
            let t = cx.b.emit(Op::Tuple(vals), tt, f.end);
            cx.b.terminate(Terminator::Return(Some(t)));
        } else {
            let end = match cx.ret.clone() {
                Some((slot, ret)) => {
                    let irt = self.ir_type(&ret);
                    let v = cx.b.emit(Op::LoadSlot(slot), irt, f.end);
                    Terminator::Return(Some(v))
                }
                None => Terminator::Return(None),
            };
            cx.b.terminate(end);
        }
        let (mut body, calls) = cx.b.finish(Terminator::Unreachable);
        // Implicit waits inside a task: no callee reads, so finish them now.
        for w in waits {
            body.blocks[w.block.0 as usize].term = Terminator::Suspend {
                wait: Wait::AnyChange(w.reads.into_iter().collect()),
                resume: w.resume,
            };
        }
        self.func_calls.insert(id, calls);
        self.d.funcs[id.0 as usize].body = body;
        Ok(())
    }

    /// A declaration inside a block or function. `automatic` locals become
    /// frame slots; static ones become design state.
    pub(crate) fn block_decl(
        &mut self,
        cx: &mut Cx<'a>,
        d: &'t ast::ModuleItem<'a>,
        automatic: bool,
    ) -> EResult<()> {
        match d {
            ast::ModuleItem::Var(v) => self.local_vars(cx, v, automatic),
            ast::ModuleItem::Param(p) => {
                for a in &p.assigns {
                    let Some(e) = &a.value else { continue };
                    let ty = match &p.ty {
                        Some(ast::DataType::Implicit {
                            signing: None,
                            packed,
                        }) if packed.is_empty() => self.self_type(e)?.ty(),
                        Some(t) => self.resolve_type(t)?,
                        None => return Err(self.not_yet(a.name, "local type parameters")),
                    };
                    let v = self.const_value(e, Some(&ty))?;
                    cx.locals
                        .last_mut()
                        .unwrap()
                        .insert(a.name, Sym::Param(v, ty));
                }
                Ok(())
            }
            ast::ModuleItem::Typedef(t) => Err(self.not_yet(t.name, "typedefs inside blocks")),
            other => Err(self.error(
                self.here(),
                format!("Unexpected declaration in a block: {other:?}"),
            )),
        }
    }

    fn local_vars(
        &mut self,
        cx: &mut Cx<'a>,
        d: &'t ast::VarDecl<'a>,
        automatic: bool,
    ) -> EResult<()> {
        let automatic =
            (automatic || d.qualifiers.contains(&"automatic")) && !d.qualifiers.contains(&"static");
        let base = self.resolve_type(&d.ty)?;
        for v in &d.vars {
            self.last_at = v.name;
            // The type declaration of a non-ANSI argument, already a slot.
            if cx.is_func
                && cx.locals.len() == 1
                && v.init.is_none()
                && matches!(cx.locals[0].get(v.name), Some(Sym::Slot(..)))
            {
                continue;
            }
            let mut ty = base.clone();
            ty.unpacked = self.unpacked_dims(&v.dims)?;
            if automatic
                && !ty.unpacked.is_empty()
                && super::expr::fixed_count(&ty).is_none()
                && !(matches!(ty.unpacked[0], UDim::Dynamic | UDim::Queue)
                    && super::expr::fixed_count(&{
                        let mut t = ty.clone();
                        t.unpacked.remove(0);
                        t
                    })
                    .is_some())
            {
                return Err(self.not_yet(v.name, "dynamic arrays, queues and associative arrays"));
            }
            let irt = self.ir_type(&ty);
            if automatic {
                let slot = cx.b.new_slot(irt);
                cx.locals
                    .last_mut()
                    .unwrap()
                    .insert(v.name, Sym::Slot(slot, ty.clone()));
                // An automatic variable starts at its default value on each entry.
                let init = match &v.init {
                    Some(e) => self.lower_to(cx, e, &ty)?,
                    // (An array starts at its default when the frame is made.)
                    None if !ty.unpacked.is_empty() => continue,
                    None => {
                        let b = if ty.four_state() {
                            Bits::all_x(ty.width().max(1))
                        } else {
                            Bits::zero(ty.width().max(1))
                        };
                        cx.b.emit(Op::Const(b), irt, v.name)
                    }
                };
                cx.b.effect(
                    Op::StoreSlot {
                        slot,
                        part: None,
                        value: init,
                    },
                    v.name,
                );
            } else {
                let id = VarId(self.d.vars.len() as u32);
                let scope = cx.block_scope.unwrap_or(self.cur);
                self.d.vars.push(Var {
                    name: v.name,
                    scope,
                    ty: irt,
                    kind: VarKind::Variable,
                    init: None,
                    at: v.name,
                });
                cx.locals
                    .last_mut()
                    .unwrap()
                    .insert(v.name, Sym::Var(id, ty.clone()));
                // Named blocks' variables can be reached hierarchically: blk.v
                if let Some(b) = cx.block_scope {
                    self.scopes[b.0 as usize]
                        .syms
                        .insert(v.name, Sym::Var(id, ty.clone()));
                }
                if let Some(e) = &v.init {
                    match self.try_const(e, &ty) {
                        Some(Value::Bits(b)) => self.d.vars[id.0 as usize].init = Some(b),
                        _ => {
                            return Err(self.not_yet(
                                v.name,
                                "non-constant initialisers of static block variables",
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Lower `f` inside markers that let `disable tag` leave it.
    fn tagged(
        &mut self,
        cx: &mut Cx<'a>,
        tag: DisableTag,
        f: impl FnOnce(&mut Self, &mut Cx<'a>) -> EResult<()>,
    ) -> EResult<()> {
        let exit = cx.b.new_block();
        cx.b.effect(Op::BlockEnter { tag, exit }, "");
        f(self, cx)?;
        cx.b.terminate(Terminator::Jump(exit, vec![]));
        cx.b.switch_to(exit);
        cx.b.effect(Op::BlockLeave(tag), "");
        Ok(())
    }

    /// What `disable name` names: a named block (searched from the current
    /// block outwards, or hierarchically) or a task.
    fn disable_target(&mut self, cx: &Cx<'a>, e: &Expr<'a>) -> EResult<DisableTag> {
        if let Expr::Ident(n) = e {
            let mut s = cx.block_scope;
            while let Some(id) = s {
                if let Some(Sym::Scope(b)) = self.scopes[id.0 as usize].syms.get(n) {
                    return Ok(DisableTag::Block(*b));
                }
                s = self.d.scopes[id.0 as usize].parent;
            }
            match self.lookup_cx(Some(cx), n) {
                Some(Sym::Func(f)) => return Ok(DisableTag::Task(f)),
                Some(Sym::Scope(b)) => return Ok(DisableTag::Block(b)),
                _ => {}
            }
        }
        if let Expr::Member { base, name } = e {
            let parent = match &**base {
                Expr::Ident(_) => self.disable_target(cx, base).ok().and_then(|t| match t {
                    DisableTag::Block(b) => Some(b),
                    _ => None,
                }),
                _ => None,
            }
            .or_else(|| self.hier_scope(Some(cx), base));
            if let Some(p) = parent {
                match self.lookup_child(p, name) {
                    Some(Sym::Scope(b)) => return Ok(DisableTag::Block(b)),
                    Some(Sym::Func(f)) => return Ok(DisableTag::Task(f)),
                    _ => {}
                }
            }
        }
        if let Some(s) = self.hier_scope(Some(cx), e) {
            return Ok(DisableTag::Block(s));
        }
        Err(self.error(
            e.at(),
            format!("Can't find definition of block or task: '{}'", e.at()),
        ))
    }

    /// Make the scopes of the named blocks in `s` ahead of lowering it, so
    /// `disable` can name a block that comes later.
    fn predeclare_blocks(&mut self, parent: ScopeId, s: &Stmt<'a>) {
        let inner = |this: &mut Self, label: &'a str| {
            let unit = this.d.scopes[this.cur.0 as usize].unit;
            if let Some(Sym::Scope(b)) = this.scopes[parent.0 as usize].syms.get(label) {
                return *b;
            }
            let b = this.new_scope(label, Some(parent), None, Some(parent), unit, &[]);
            this.scopes[parent.0 as usize].syms.insert(label, Sym::Scope(b));
            b
        };
        match s {
            Stmt::Block {
                label: Some(l),
                stmts,
                ..
            } => {
                let b = inner(self, l);
                stmts.iter().for_each(|x| self.predeclare_blocks(b, x));
            }
            Stmt::Labeled { label, stmt } => {
                let b = inner(self, label);
                match &**stmt {
                    Stmt::Block { stmts, .. } => {
                        stmts.iter().for_each(|x| self.predeclare_blocks(b, x));
                    }
                    other => self.predeclare_blocks(b, other),
                }
            }
            Stmt::Block { stmts, .. } => stmts.iter().for_each(|x| self.predeclare_blocks(parent, x)),
            Stmt::If { then, els, .. } => {
                self.predeclare_blocks(parent, then);
                if let Some(e) = els {
                    self.predeclare_blocks(parent, e);
                }
            }
            Stmt::Case { items, .. } => items.iter().for_each(|i| self.predeclare_blocks(parent, &i.stmt)),
            Stmt::For { body, .. }
            | Stmt::Foreach { body, .. }
            | Stmt::While { body, .. }
            | Stmt::DoWhile { body, .. }
            | Stmt::Repeat { body, .. }
            | Stmt::Forever { body, .. }
            | Stmt::Timed { stmt: body, .. }
            | Stmt::Wait { stmt: body, .. } => self.predeclare_blocks(parent, body),
            _ => {}
        }
    }

    /// A scope for a named block, inside the enclosing named block or the module.
    fn block_scope(&mut self, cx: &Cx<'a>, label: &'a str) -> ScopeId {
        let parent = cx.block_scope.unwrap_or(self.cur);
        // Made ahead by `predeclare_blocks`?
        if let Some(Sym::Scope(s)) = self.scopes[parent.0 as usize].syms.get(label)
            && self.d.scopes[s.0 as usize].module.is_none()
            && self.d.scopes[s.0 as usize].parent == Some(parent)
        {
            return *s;
        }
        let unit = self.d.scopes[self.cur.0 as usize].unit;
        let s = self.new_scope(label, Some(parent), None, Some(parent), unit, &[]);
        self.scopes[parent.0 as usize]
            .syms
            .insert(label, Sym::Scope(s));
        s
    }

    /// `begin ... end` or `fork ... join*`.
    fn lower_block(
        &mut self,
        cx: &mut Cx<'a>,
        kw: &'a str,
        decls: &'t [ast::ModuleItem<'a>],
        stmts: &'t [Stmt<'a>],
        end: &'a str,
        waits: &mut Vec<PendingWait>,
    ) -> EResult<()> {
        cx.locals.push(HashMap::new());
        let r = (|| {
            for d in decls {
                self.block_decl(cx, d, cx.is_func)?;
            }
            if kw != "fork" {
                for st in stmts {
                    self.lower_stmt(cx, st, waits)?;
                }
                return Ok(());
            }
            // Each statement of a fork is a child thread sharing this frame.
            let resume = cx.b.new_block();
            let fork_at = cx.b.current();
            let mut children = Vec::new();
            for st in stmts {
                let b = cx.b.new_block();
                children.push(b);
                cx.b.switch_to(b);
                self.lower_stmt(cx, st, waits)?;
                cx.b.terminate(Terminator::EndThread);
            }
            cx.b.switch_to(fork_at);
            let join = match end {
                "join" => Join::All,
                "join_any" => Join::Any,
                _ => Join::None,
            };
            cx.b.terminate(Terminator::Fork {
                children,
                join,
                resume,
            });
            cx.b.switch_to(resume);
            Ok(())
        })();
        cx.locals.pop();
        r
    }

    // ------------------------------------------------------------ statements

    pub(crate) fn lower_stmt(
        &mut self,
        cx: &mut Cx<'a>,
        s: &'t Stmt<'a>,
        waits: &mut Vec<PendingWait>,
    ) -> EResult<()> {
        match s {
            Stmt::Null(_) => Ok(()),
            // `name: begin ... end` is the same as `begin : name ... end`.
            Stmt::Labeled { label, stmt } => {
                let scope = self.block_scope(cx, label);
                let saved = cx.block_scope.replace(scope);
                let r = self.tagged(cx, DisableTag::Block(scope), |this, cx| {
                    this.lower_stmt(cx, stmt, waits)
                });
                cx.block_scope = saved;
                r
            }
            Stmt::Block {
                label,
                kw,
                decls,
                stmts,
                end,
            } => {
                let saved = cx.block_scope;
                let r = match label {
                    Some(l) => {
                        let scope = self.block_scope(cx, l);
                        cx.block_scope = Some(scope);
                        self.tagged(cx, DisableTag::Block(scope), |this, cx| {
                            this.lower_block(cx, kw, decls, stmts, end, waits)
                        })
                    }
                    None => self.lower_block(cx, kw, decls, stmts, end, waits),
                };
                cx.block_scope = saved;
                r
            }
            Stmt::Decl(d) => self.local_vars(cx, d, cx.is_func),
            Stmt::Assign {
                lhs,
                op,
                timing,
                rhs,
            } => self.lower_assign(cx, lhs, op, timing.as_ref(), rhs),
            Stmt::Expr(e) => self.expr_stmt(cx, e),
            Stmt::If {
                cond, then, els, ..
            } => {
                let c = self.truth(cx, cond)?;
                let (tb, eb, join) = (cx.b.new_block(), cx.b.new_block(), cx.b.new_block());
                cx.b.terminate(Terminator::Branch {
                    cond: c,
                    then: (tb, vec![]),
                    els: (eb, vec![]),
                });
                cx.b.switch_to(tb);
                self.lower_stmt(cx, then, waits)?;
                cx.b.goto(join);
                cx.b.switch_to(eb);
                if let Some(e) = els {
                    self.lower_stmt(cx, e, waits)?;
                }
                cx.b.goto(join);
                Ok(())
            }
            Stmt::Case {
                kw,
                expr,
                inside,
                items,
                ..
            } => self.lower_case(cx, kw, expr, *inside, items, waits),
            Stmt::For {
                init,
                cond,
                step,
                body,
                ..
            } => {
                cx.locals.push(HashMap::new());
                for st in init {
                    match st {
                        Stmt::Decl(d) => self.local_vars(cx, d, true)?,
                        other => self.lower_stmt(cx, other, waits)?,
                    }
                }
                let (head, body_b, step_b, exit) = (
                    cx.b.new_block(),
                    cx.b.new_block(),
                    cx.b.new_block(),
                    cx.b.new_block(),
                );
                cx.b.goto(head);
                match cond {
                    Some(c) => {
                        let c = self.truth(cx, c)?;
                        cx.b.terminate(Terminator::Branch {
                            cond: c,
                            then: (body_b, vec![]),
                            els: (exit, vec![]),
                        });
                    }
                    None => cx.b.terminate(Terminator::Jump(body_b, vec![])),
                }
                cx.b.switch_to(body_b);
                cx.b.loops.push((step_b, exit));
                self.lower_stmt(cx, body, waits)?;
                cx.b.loops.pop();
                cx.b.goto(step_b);
                for st in step {
                    self.lower_stmt(cx, st, waits)?;
                }
                cx.b.terminate(Terminator::Jump(head, vec![]));
                cx.b.switch_to(exit);
                cx.locals.pop();
                Ok(())
            }
            Stmt::While { cond, body, .. } => {
                let (head, body_b, exit) = (cx.b.new_block(), cx.b.new_block(), cx.b.new_block());
                cx.b.goto(head);
                let c = self.truth(cx, cond)?;
                cx.b.terminate(Terminator::Branch {
                    cond: c,
                    then: (body_b, vec![]),
                    els: (exit, vec![]),
                });
                cx.b.switch_to(body_b);
                cx.b.loops.push((head, exit));
                self.lower_stmt(cx, body, waits)?;
                cx.b.loops.pop();
                cx.b.terminate(Terminator::Jump(head, vec![]));
                cx.b.switch_to(exit);
                Ok(())
            }
            Stmt::DoWhile { body, cond, .. } => {
                let (body_b, cond_b, exit) = (cx.b.new_block(), cx.b.new_block(), cx.b.new_block());
                cx.b.goto(body_b);
                cx.b.loops.push((cond_b, exit));
                self.lower_stmt(cx, body, waits)?;
                cx.b.loops.pop();
                cx.b.goto(cond_b);
                let c = self.truth(cx, cond)?;
                cx.b.terminate(Terminator::Branch {
                    cond: c,
                    then: (body_b, vec![]),
                    els: (exit, vec![]),
                });
                cx.b.switch_to(exit);
                Ok(())
            }
            Stmt::Repeat { count, body, .. } => {
                let (v, st) = self.lower_self(cx, count)?;
                let STy::Bits { w, s, f } = st else {
                    return Err(self.error(count.at(), "repeat count must be integral"));
                };
                let t = self.bits_type(w, s, f);
                let slot = cx.b.new_slot(t);
                cx.b.effect(
                    Op::StoreSlot {
                        slot,
                        part: None,
                        value: v,
                    },
                    count.at(),
                );
                let (head, body_b, exit) = (cx.b.new_block(), cx.b.new_block(), cx.b.new_block());
                cx.b.goto(head);
                let n = cx.b.emit(Op::LoadSlot(slot), t, count.at());
                let zero = cx.b.emit(Op::Const(Bits::zero(w)), t, count.at());
                let bit = self.bits_type(1, false, true);
                let more = cx.b.emit(Op::Binary(BinOp::Gt, n, zero), bit, count.at());
                cx.b.terminate(Terminator::Branch {
                    cond: more,
                    then: (body_b, vec![]),
                    els: (exit, vec![]),
                });
                cx.b.switch_to(body_b);
                let one = cx.b.emit(Op::Const(Bits::from_u64(w, 1)), t, count.at());
                let n1 = cx.b.emit(Op::Binary(BinOp::Sub, n, one), t, count.at());
                cx.b.effect(
                    Op::StoreSlot {
                        slot,
                        part: None,
                        value: n1,
                    },
                    count.at(),
                );
                cx.b.loops.push((head, exit));
                self.lower_stmt(cx, body, waits)?;
                cx.b.loops.pop();
                cx.b.terminate(Terminator::Jump(head, vec![]));
                cx.b.switch_to(exit);
                Ok(())
            }
            Stmt::Forever { body, .. } => {
                let (head, exit) = (cx.b.new_block(), cx.b.new_block());
                cx.b.goto(head);
                cx.b.loops.push((head, exit));
                self.lower_stmt(cx, body, waits)?;
                cx.b.loops.pop();
                cx.b.terminate(Terminator::Jump(head, vec![]));
                cx.b.switch_to(exit);
                Ok(())
            }
            Stmt::Foreach {
                array,
                vars,
                body,
                kw,
            } => self.lower_foreach(cx, kw, array, vars, body, waits),
            Stmt::Timed {
                timing: ast::Timing::Event(None),
                stmt,
            } => {
                // @*: wait for a change in anything the statement reads. The
                // read set (plus what called functions read) is filled in later.
                let resume = cx.b.new_block();
                let block = cx.b.current();
                cx.b.terminate(Terminator::Suspend {
                    wait: Wait::AnyChange(Vec::new()),
                    resume,
                });
                cx.b.switch_to(resume);
                cx.b.read_scopes.push(BTreeSet::new());
                let r = self.lower_stmt(cx, stmt, waits);
                let reads = cx.b.read_scopes.pop().unwrap();
                r?;
                waits.push(PendingWait {
                    block,
                    reads,
                    resume,
                });
                Ok(())
            }
            Stmt::Timed { timing, stmt } => {
                self.lower_timing(cx, timing)?;
                self.lower_stmt(cx, stmt, waits)
            }
            Stmt::Wait { cond: None, .. } => {
                let resume = cx.b.new_block();
                cx.b.terminate(Terminator::Suspend {
                    wait: Wait::Children,
                    resume,
                });
                cx.b.switch_to(resume);
                Ok(())
            }
            Stmt::Wait {
                cond: Some(c),
                stmt,
                ..
            } => {
                let (head, wait_b, done) = (cx.b.new_block(), cx.b.new_block(), cx.b.new_block());
                cx.b.goto(head);
                cx.b.read_scopes.push(BTreeSet::new());
                let r = self.truth(cx, c);
                let reads = cx.b.read_scopes.pop().unwrap();
                let c = r?;
                cx.b.terminate(Terminator::Branch {
                    cond: c,
                    then: (done, vec![]),
                    els: (wait_b, vec![]),
                });
                cx.b.switch_to(wait_b);
                cx.b.terminate(Terminator::Suspend {
                    wait: Wait::AnyChange(reads.into_iter().collect()),
                    resume: head,
                });
                cx.b.switch_to(done);
                self.lower_stmt(cx, stmt, waits)
            }
            Stmt::Trigger { op: "->", target } => {
                let Some(p) = self.path(cx, target)? else {
                    return Err(self.error(target.at(), "Expecting an event"));
                };
                let super::expr::Root::Var(v) = p.root else {
                    return Err(self.error(target.at(), "Expecting an event"));
                };
                if p.ty.base != Base::Event {
                    return Err(self.error(target.at(), "Expecting an event"));
                }
                cx.b.effect(Op::TriggerEvent(v), target.at());
                Ok(())
            }
            Stmt::Trigger { op, .. } => Err(self.not_yet(op, "non-blocking event triggers")),
            Stmt::Disable { kw, target: None } => {
                cx.b.effect(Op::DisableFork, kw);
                Ok(())
            }
            Stmt::Disable { kw, target: Some(t) } => {
                let tag = self.disable_target(cx, t)?;
                let resume = cx.b.new_block();
                cx.b.terminate(Terminator::Disable { tag, resume });
                cx.b.switch_to(resume);
                let _ = kw;
                Ok(())
            }
            Stmt::Return { kw, value } => {
                if !cx.is_func {
                    return Err(self.error(kw, "return outside a function or task"));
                }
                match (value, cx.ret.clone()) {
                    (Some(v), Some((slot, ty))) if cx.exit.is_some() => {
                        let v = self.lower_to(cx, v, &ty)?;
                        cx.b.effect(
                            Op::StoreSlot {
                                slot,
                                part: None,
                                value: v,
                            },
                            kw,
                        );
                        cx.b.terminate(Terminator::Jump(cx.exit.unwrap(), vec![]));
                    }
                    (None, None) if cx.exit.is_some() => {
                        cx.b.terminate(Terminator::Jump(cx.exit.unwrap(), vec![]));
                    }
                    (Some(v), Some((_, ty))) => {
                        let v = self.lower_to(cx, v, &ty)?;
                        cx.b.terminate(Terminator::Return(Some(v)));
                    }
                    (None, None) => cx.b.terminate(Terminator::Return(None)),
                    (Some(v), None) => {
                        return Err(
                            self.error(v.at(), "return with a value in a task or void function")
                        );
                    }
                    (None, Some(_)) => {
                        return Err(self.error(kw, "return without a value in a function"));
                    }
                }
                Ok(())
            }
            Stmt::Break(kw) | Stmt::Continue(kw) => {
                let Some(&(cont, brk)) = cx.b.loops.last() else {
                    return Err(self.error(kw, format!("{kw} outside a loop")));
                };
                let target = if *kw == "break" { brk } else { cont };
                cx.b.terminate(Terminator::Jump(target, vec![]));
                Ok(())
            }
            Stmt::ProcAssign { kw, .. } => Err(self.not_yet(kw, &format!("procedural {kw}"))),
            Stmt::Assert {
                kw,
                cond,
                pass,
                fail,
            } => {
                let c = self.truth(cx, cond)?;
                let (pb, fb, join) = (cx.b.new_block(), cx.b.new_block(), cx.b.new_block());
                cx.b.terminate(Terminator::Branch {
                    cond: c,
                    then: (pb, vec![]),
                    els: (fb, vec![]),
                });
                cx.b.switch_to(pb);
                if let Some(p) = pass {
                    self.lower_stmt(cx, p, waits)?;
                }
                cx.b.goto(join);
                cx.b.switch_to(fb);
                match fail {
                    Some(f) => self.lower_stmt(cx, f, waits)?,
                    None if *kw == "assert" => {
                        let text = format!("Assertion failed in {}", self.d.scope_path(self.cur));
                        let text = self.sm.derive(text, kw);
                        let format = self.add_format(vec![FormatPiece::Text(text)]);
                        cx.b.effect(
                            Op::Report {
                                severity: ReportSeverity::Error,
                                format: Some(format),
                                args: vec![],
                            },
                            kw,
                        );
                    }
                    None => {}
                }
                cx.b.goto(join);
                Ok(())
            }
        }
    }

    fn lower_assign(
        &mut self,
        cx: &mut Cx<'a>,
        lhs: &'t Expr<'a>,
        op: &'a str,
        timing: Option<&'t ast::Timing<'a>>,
        rhs: &'t Expr<'a>,
    ) -> EResult<()> {
        let nba = op == "<=";
        let rhs_expr;
        let rhs: &Expr<'a> = if op != "=" && op != "<=" {
            rhs_expr = Expr::Binary {
                op: &op[..op.len() - 1],
                lhs: Box::new(lhs.clone()),
                rhs: Box::new(rhs.clone()),
            };
            &rhs_expr
        } else {
            rhs
        };
        match timing {
            None => self.assign(cx, lhs, rhs, nba),
            Some(_) if nba => Err(self.not_yet(op, "delayed non-blocking assignments")),
            Some(t) => {
                // Intra-assignment delay: evaluate now, assign later.
                let (v, st) = self.lower_self(cx, rhs)?;
                self.lower_timing(cx, t)?;
                self.assign_val(cx, lhs, v, st, false)
            }
        }
    }

    fn expr_stmt(&mut self, cx: &mut Cx<'a>, e: &'t Expr<'a>) -> EResult<()> {
        match e {
            Expr::SysCall { name, args } => self.lower_systask(cx, name, args),
            Expr::Call { func, args }
                if matches!(&**func, Expr::Member { base, .. } if self.array_type(cx, base).is_some()) =>
            {
                let Expr::Member { base, name } = &**func else {
                    unreachable!()
                };
                if !self.array_mutate(cx, base, name, args)? {
                    self.array_method(cx, base, name, args)?;
                }
                Ok(())
            }
            Expr::Call { func, args } if matches!(&**func, Expr::Member { base, .. } if self.is_str(Some(cx), base)) =>
            {
                let Expr::Member { base, name } = &**func else {
                    unreachable!()
                };
                if !self.str_mutate(cx, base, name, args)? {
                    self.str_method(cx, base, name, args)?;
                }
                Ok(())
            }
            Expr::Call { func, args } => {
                self.call_expr(cx, func, args)?;
                Ok(())
            }
            // `obj.task;` or a method called without parentheses.
            Expr::Member { .. } | Expr::Scoped { .. }
                if matches!(e, Expr::Member { base, .. } if self.handle_class(Some(cx), base).is_some())
                    || matches!(e, Expr::Scoped { scope, .. } if self.scope_class(scope).is_some()) =>
            {
                self.call_expr(cx, e, &[])?;
                Ok(())
            }
            Expr::Ident(n) if matches!(self.lookup_cx(Some(cx), n), Some(Sym::Func(f)) if self.func_class.contains_key(&f)) => {
                self.call_expr(cx, e, &[])?;
                Ok(())
            }
            Expr::Ident(n) | Expr::Scoped { name: n, .. } => match self.lookup_cx(Some(cx), n) {
                Some(Sym::Func(_)) => {
                    let call = Expr::Call {
                        func: Box::new(e.clone()),
                        args: Vec::new(),
                    };
                    // Lower a call with no arguments.
                    let f = self.resolve_func(Some(cx), e)?;
                    let _ = call;
                    match self.sig(f)?.ret {
                        Some(t) => {
                            let rt = self.ir_type(&t);
                            cx.b.emit(
                                Op::Call {
                                    func: f,
                                    args: vec![],
                                },
                                rt,
                                n,
                            );
                        }
                        None => cx.b.effect(
                            Op::Call {
                                func: f,
                                args: vec![],
                            },
                            n,
                        ),
                    }
                    Ok(())
                }
                _ => Err(self.error(n, format!("Statement has no effect: '{n}'"))),
            },
            Expr::IncDec { op, arg, .. } => {
                let one = Expr::Number("1");
                let rhs = Expr::Binary {
                    op: if *op == "++" { "+" } else { "-" },
                    lhs: arg.clone(),
                    rhs: Box::new(one),
                };
                self.assign(cx, arg, &rhs, false)
            }
            Expr::Cast { ty, expr } if matches!(&**ty, Expr::Type(t) if matches!(&**t, ast::DataType::Builtin { kw: "void", .. })) => {
                self.expr_stmt(cx, expr)
            }
            other => Err(self.not_yet(other.at(), "this expression as a statement")),
        }
    }

    fn lower_case(
        &mut self,
        cx: &mut Cx<'a>,
        kw: &'a str,
        expr: &'t Expr<'a>,
        inside: bool,
        items: &'t [ast::CaseItem<'a>],
        waits: &mut Vec<PendingWait>,
    ) -> EResult<()> {
        let join = cx.b.new_block();
        let mut default = None;
        if inside {
            for item in items {
                if item.labels.is_empty() {
                    default = Some(&item.stmt);
                    continue;
                }
                let m = self.lower_inside(cx, expr, &item.labels)?;
                let (hit, next) = (cx.b.new_block(), cx.b.new_block());
                cx.b.terminate(Terminator::Branch {
                    cond: m,
                    then: (hit, vec![]),
                    els: (next, vec![]),
                });
                cx.b.switch_to(hit);
                self.lower_stmt(cx, &item.stmt, waits)?;
                cx.b.goto(join);
                cx.b.switch_to(next);
            }
        } else {
            // The case expression and all labels are sized to the widest (LRM 12.5).
            let mut st = self.self_type_cx(Some(cx), expr)?;
            for item in items {
                for l in &item.labels {
                    let lt = self.self_type_cx(Some(cx), l)?;
                    st = match (st, lt) {
                        (
                            STy::Bits { w, s, f },
                            STy::Bits {
                                w: w2,
                                s: s2,
                                f: f2,
                            },
                        ) => STy::Bits {
                            w: w.max(w2),
                            s: s && s2,
                            f: f || f2,
                        },
                        _ => return Err(self.not_yet(l.at(), "case on non-integral values")),
                    };
                }
            }
            let STy::Bits { w, s, f } = st else {
                return Err(self.not_yet(expr.at(), "case on non-integral values"));
            };
            let want = Want { w, s, f };
            let sel = self.lower(cx, expr, want)?;
            let op = match kw {
                "casez" => BinOp::CaseZEq,
                "casex" => BinOp::CaseXEq,
                _ => BinOp::CaseEq,
            };
            let bit = self.bits_type(1, false, false);
            for item in items {
                if item.labels.is_empty() {
                    default = Some(&item.stmt);
                    continue;
                }
                let hit = cx.b.new_block();
                for l in &item.labels {
                    let lv = self.lower(cx, l, want)?;
                    let m = cx.b.emit(Op::Binary(op, sel, lv), bit, l.at());
                    let next = cx.b.new_block();
                    cx.b.terminate(Terminator::Branch {
                        cond: m,
                        then: (hit, vec![]),
                        els: (next, vec![]),
                    });
                    cx.b.switch_to(next);
                }
                let after = cx.b.current();
                cx.b.switch_to(hit);
                self.lower_stmt(cx, &item.stmt, waits)?;
                cx.b.goto(join);
                cx.b.switch_to(after);
            }
        }
        if let Some(d) = default {
            self.lower_stmt(cx, d, waits)?;
        }
        cx.b.goto(join);
        Ok(())
    }

    /// `foreach (a[i, j]) body` over fixed-size dimensions, left index to right.
    fn lower_foreach(
        &mut self,
        cx: &mut Cx<'a>,
        kw: &'a str,
        array: &'t Expr<'a>,
        vars: &'t [Option<&'a str>],
        body: &'t Stmt<'a>,
        waits: &mut Vec<PendingWait>,
    ) -> EResult<()> {
        let Some(p) = self.path(cx, array)? else {
            return Err(self.error(array.at(), "foreach needs an array"));
        };
        let mut dims: Vec<(i64, i64)> = Vec::new();
        // A dynamic first dimension runs from 0 to its size less one, at run time.
        let mut dyn_end = None;
        for (k, d) in p.ty.unpacked.iter().enumerate() {
            match d {
                UDim::Fixed(l, r) => dims.push((*l, *r)),
                UDim::Dynamic | UDim::Queue if k == 0 => {
                    let (whole, _) = self.load_path(cx, &p)?;
                    let int = Ty::bits(32, true, false);
                    let size = self.arr_op(cx, ArrFunc::Size, vec![whole], &int, kw);
                    let it = self.ir_type(&int);
                    let one = cx.b.emit(Op::Const(Bits::from_i64(32, 1)), it, kw);
                    dyn_end = Some(cx.b.emit(Op::Binary(BinOp::Sub, size, one), it, kw));
                    dims.push((0, i64::MAX));
                }
                _ => return Err(self.not_yet(kw, "foreach over associative arrays")),
            }
        }
        dims.extend(p.ty.packed.iter().copied());
        if p.ty.packed.is_empty() && p.ty.unpacked.is_empty() && p.ty.width() > 1 {
            dims.push((p.ty.width() as i64 - 1, 0));
        }
        if vars.len() > dims.len() {
            return Err(self.error(kw, "foreach has more loop variables than dimensions"));
        }
        let int = Ty::bits(32, true, false);
        let it = self.ir_type(&int);
        cx.locals.push(HashMap::new());
        let mut exits = Vec::new();
        let mut heads = Vec::new();
        for (var, &(l, r)) in vars.iter().zip(&dims) {
            let Some(name) = var else { continue };
            let slot = cx.b.new_slot(it);
            cx.locals
                .last_mut()
                .unwrap()
                .insert(name, Sym::Slot(slot, int.clone()));
            let start = cx.b.emit(Op::Const(Bits::from_i64(32, l)), it, name);
            cx.b.effect(
                Op::StoreSlot {
                    slot,
                    part: None,
                    value: start,
                },
                name,
            );
            let (head, body_b, exit) = (cx.b.new_block(), cx.b.new_block(), cx.b.new_block());
            cx.b.goto(head);
            let i = cx.b.emit(Op::LoadSlot(slot), it, name);
            let end = match dyn_end {
                Some(e) if r == i64::MAX => e,
                _ => cx.b.emit(Op::Const(Bits::from_i64(32, r)), it, name),
            };
            let bit = self.bits_type(1, false, false);
            let cmp = if l <= r { BinOp::Le } else { BinOp::Ge };
            let c = cx.b.emit(Op::Binary(cmp, i, end), bit, name);
            cx.b.terminate(Terminator::Branch {
                cond: c,
                then: (body_b, vec![]),
                els: (exit, vec![]),
            });
            cx.b.switch_to(body_b);
            heads.push((slot, head, l <= r, *name));
            exits.push(exit);
        }
        let step = cx.b.new_block();
        let brk = *exits.first().unwrap_or(&step);
        cx.b.loops.push((step, brk));
        self.lower_stmt(cx, body, waits)?;
        cx.b.loops.pop();
        cx.b.goto(step);
        // Step the innermost variable; when it ends, step the next one out.
        for ((slot, head, up, name), exit) in heads.into_iter().rev().zip(exits.into_iter().rev()) {
            let i = cx.b.emit(Op::LoadSlot(slot), it, name);
            let one = cx.b.emit(
                Op::Const(Bits::from_i64(32, if up { 1 } else { -1 })),
                it,
                name,
            );
            let n = cx.b.emit(Op::Binary(BinOp::Add, i, one), it, name);
            cx.b.effect(
                Op::StoreSlot {
                    slot,
                    part: None,
                    value: n,
                },
                name,
            );
            cx.b.terminate(Terminator::Jump(head, vec![]));
            cx.b.switch_to(exit);
        }
        cx.locals.pop();
        Ok(())
    }

    // ------------------------------------------------------------ timing

    fn lower_timing(&mut self, cx: &mut Cx<'a>, t: &'t ast::Timing<'a>) -> EResult<()> {
        let resume = cx.b.new_block();
        match t {
            ast::Timing::Delay(e) => {
                let wait = match self.delay_ticks(cx, e)? {
                    None => Wait::Inactive,
                    Some(v) => Wait::Delay(v),
                };
                cx.b.terminate(Terminator::Suspend { wait, resume });
                cx.b.switch_to(resume);
            }
            ast::Timing::Event(None) => {
                return Err(self.not_yet(self.here(), "@* as an intra-assignment control"));
            }
            ast::Timing::Event(Some(events)) => {
                let mut edges = Vec::new();
                for ev in events {
                    if ev.iff.is_some() {
                        return Err(self.not_yet(ev.expr.at(), "iff in event controls"));
                    }
                    let Some(p) = self.path(cx, &ev.expr)? else {
                        return Err(self.not_yet(ev.expr.at(), "event controls on expressions"));
                    };
                    let super::expr::Root::Var(v) = p.root else {
                        return Err(
                            self.not_yet(ev.expr.at(), "event controls on this kind of name")
                        );
                    };
                    if p.lsb.is_some() || p.elem.is_some() {
                        return Err(self.not_yet(ev.expr.at(), "event controls on selects"));
                    }
                    if p.ty.base == Base::Event {
                        if events.len() != 1 {
                            return Err(
                                self.not_yet(ev.expr.at(), "named events mixed with other events")
                            );
                        }
                        cx.b.terminate(Terminator::Suspend {
                            wait: Wait::Event(v),
                            resume,
                        });
                        cx.b.switch_to(resume);
                        return Ok(());
                    }
                    let edge = match ev.edge {
                        Some("posedge") => Edge::Pos,
                        Some("negedge") => Edge::Neg,
                        Some(_) => Edge::Both,
                        None => Edge::Any,
                    };
                    edges.push((v, edge));
                }
                cx.b.terminate(Terminator::Suspend {
                    wait: Wait::Edge(edges),
                    resume,
                });
                cx.b.switch_to(resume);
            }
            ast::Timing::Cycle(e) => return Err(self.not_yet(e.at(), "cycle delays")),
            ast::Timing::Repeat(e, _) => return Err(self.not_yet(e.at(), "repeat event controls")),
        }
        Ok(())
    }

    /// A delay in ticks of the design's precision; `None` for `#0`.
    fn delay_ticks(&mut self, cx: &mut Cx<'a>, e: &'t Expr<'a>) -> EResult<Option<Val>> {
        let unit = self.d.scopes[self.cur.0 as usize].unit;
        let prec = self.d.precision;
        let t64 = self.bits_type(64, false, false);
        let at = e.at();
        let scale = |exp: i8| 10f64.powi((exp - prec) as i32);
        if let Expr::Number(n) = e {
            let ticks = match parse_literal(n) {
                Ok(Literal::Bits { bits, .. }) => match bits.to_i64(false) {
                    Some(v) => (v as f64 * scale(unit)).round(),
                    None => return Err(self.error(at, "Delay must be a known value")),
                },
                Ok(Literal::Real(r)) => (r * scale(unit)).round(),
                Ok(Literal::Time(v, exp)) => (v * scale(exp)).round(),
                Ok(Literal::Fill(_)) => return Err(self.error(at, "Bad delay")),
                Err(m) => return Err(self.error(at, m)),
            };
            if ticks == 0.0 {
                return Ok(None);
            }
            return Ok(Some(cx.b.emit(
                Op::Const(Bits::from_u64(64, ticks as u64)),
                t64,
                at,
            )));
        }
        let st = self.self_type_cx(Some(cx), e)?;
        let factor = scale(unit);
        if st == STy::Real {
            let r = self.lower_real(cx, e)?;
            let rt = self.add_type(Type::Real);
            let k = cx.b.emit(Op::ConstReal(factor), rt, at);
            let m = cx.b.emit(Op::Binary(BinOp::Mul, r, k), rt, at);
            return Ok(Some(cx.b.emit(
                Op::Resize {
                    value: m,
                    extend: Extend::Sign,
                },
                t64,
                at,
            )));
        }
        let v = self.lower(
            cx,
            e,
            Want {
                w: 64,
                s: false,
                f: false,
            },
        )?;
        if factor == 1.0 {
            return Ok(Some(v));
        }
        let k =
            cx.b.emit(Op::Const(Bits::from_u64(64, factor as u64)), t64, at);
        Ok(Some(cx.b.emit(Op::Binary(BinOp::Mul, v, k), t64, at)))
    }

    // ------------------------------------------------------------ system tasks

    fn lower_systask(
        &mut self,
        cx: &mut Cx<'a>,
        name: &'a str,
        args: &'t [Arg<'a>],
    ) -> EResult<()> {
        let display = |radix: char, kind: DisplayKind| Some((radix, kind));
        let d = match name {
            "$display" => display('d', DisplayKind::Display),
            "$displayb" => display('b', DisplayKind::Display),
            "$displayh" => display('h', DisplayKind::Display),
            "$displayo" => display('o', DisplayKind::Display),
            "$write" => display('d', DisplayKind::Write),
            "$writeb" => display('b', DisplayKind::Write),
            "$writeh" => display('h', DisplayKind::Write),
            "$writeo" => display('o', DisplayKind::Write),
            "$strobe" => display('d', DisplayKind::Strobe),
            _ => None,
        };
        if let Some((radix, kind)) = d {
            let (format, vals) = self.build_format(cx, args, radix, name)?;
            cx.b.effect(
                Op::Display {
                    kind,
                    format,
                    args: vals,
                },
                name,
            );
            return Ok(());
        }
        match name {
            "$finish" | "$stop" | "$exit" => {
                let kind = if name == "$stop" {
                    FinishKind::Stop
                } else {
                    FinishKind::Finish
                };
                cx.b.terminate(Terminator::Finish(kind));
                Ok(())
            }
            "$fatal" => {
                let rest = if args.is_empty() { args } else { &args[1..] };
                let (format, vals) = self.build_format(cx, rest, 'd', name)?;
                let format = if rest.is_empty() { None } else { Some(format) };
                cx.b.effect(
                    Op::Report {
                        severity: ReportSeverity::Error,
                        format,
                        args: vals,
                    },
                    name,
                );
                cx.b.terminate(Terminator::Finish(FinishKind::Fatal));
                Ok(())
            }
            "$error" | "$warning" | "$info" => {
                let severity = match name {
                    "$error" => ReportSeverity::Error,
                    "$warning" => ReportSeverity::Warning,
                    _ => ReportSeverity::Info,
                };
                let (format, vals) = self.build_format(cx, args, 'd', name)?;
                let format = if args.is_empty() { None } else { Some(format) };
                cx.b.effect(
                    Op::Report {
                        severity,
                        format,
                        args: vals,
                    },
                    name,
                );
                Ok(())
            }
            "$cast" => {
                let (Some(Arg::Ordered(Some(dst))), Some(Arg::Ordered(Some(src)))) =
                    (args.first(), args.get(1))
                else {
                    return Err(self.error(name, "$cast needs two arguments"));
                };
                let ok = self.lower_cast(cx, dst, src, name)?;
                // As a task, a failed cast is an error.
                let (bad, join) = (cx.b.new_block(), cx.b.new_block());
                cx.b.terminate(Terminator::Branch {
                    cond: ok,
                    then: (join, vec![]),
                    els: (bad, vec![]),
                });
                cx.b.switch_to(bad);
                cx.b.effect(
                    Op::Report {
                        severity: ReportSeverity::Error,
                        format: None,
                        args: vec![],
                    },
                    name,
                );
                cx.b.terminate(Terminator::Jump(join, vec![]));
                cx.b.switch_to(join);
                Ok(())
            }
            "$sformat" | "$swrite" | "$swriteb" | "$swriteh" | "$swriteo" => {
                let Some(Arg::Ordered(Some(dst))) = args.first() else {
                    return Err(self.error(name, format!("{name} needs a destination")));
                };
                let radix = match name {
                    "$swriteb" => 'b',
                    "$swriteh" => 'h',
                    "$swriteo" => 'o',
                    _ => 'd',
                };
                let (format, vals) = self.build_format(cx, &args[1..], radix, name)?;
                let stt = self.add_type(Type::String);
                let v = cx.b.emit(Op::Sformat { format, args: vals }, stt, name);
                self.store_string(cx, dst, v)
            }
            // Waveform dumping is not modelled yet; these have no effect on behaviour.
            "$dumpfile" | "$dumpvars" | "$dumpon" | "$dumpoff" | "$dumpall" | "$dumpflush"
            | "$dumplimit" => Ok(()),
            _ => Err(self.not_yet(name, &format!("system task {name}"))),
        }
    }

    pub(crate) fn add_format(&mut self, pieces: Vec<FormatPiece<'a>>) -> FormatId {
        self.d.formats.push(Format { pieces });
        FormatId(self.d.formats.len() as u32 - 1)
    }

    /// Turn `$display`-style arguments into a format and its values. String
    /// literal arguments are formats for the arguments after them; other
    /// arguments print in the default radix.
    pub(crate) fn build_format(
        &mut self,
        cx: &mut Cx<'a>,
        args: &[Arg<'a>],
        radix: char,
        at: &'a str,
    ) -> EResult<(FormatId, Vec<Val>)> {
        let mut pieces = Vec::new();
        let mut vals = Vec::new();
        let mut i = 0;
        while i < args.len() {
            match &args[i] {
                Arg::Ordered(Some(Expr::Str(s))) => {
                    i += 1;
                    let text = super::decode_string(super::str_body(s));
                    for piece in parse_format(&text) {
                        match piece {
                            Piece::Text(t) => pieces.push(FormatPiece::Text(self.sm.derive(t, s))),
                            Piece::Scope => {
                                let path = self.d.scope_path(self.cur);
                                pieces.push(FormatPiece::Text(self.sm.derive(path, s)));
                            }
                            Piece::Conv {
                                spec,
                                width,
                                zero_pad,
                                left,
                            } => {
                                let Some(Arg::Ordered(Some(e))) = args.get(i) else {
                                    return Err(self.error(
                                        s,
                                        format!("Missing argument for %{spec} in format"),
                                    ));
                                };
                                i += 1;
                                vals.push(self.format_arg(cx, e, spec)?);
                                pieces.push(FormatPiece::Conv {
                                    spec,
                                    width,
                                    zero_pad,
                                    left,
                                });
                            }
                        }
                    }
                }
                Arg::Ordered(Some(e)) => {
                    i += 1;
                    vals.push(self.format_arg(cx, e, radix)?);
                    pieces.push(FormatPiece::Conv {
                        spec: radix,
                        width: None,
                        zero_pad: false,
                        left: false,
                    });
                }
                Arg::Ordered(None) => {
                    i += 1;
                    pieces.push(FormatPiece::Text(" "));
                }
                Arg::Named(n, _) => {
                    return Err(self.error(n, "Named arguments are not allowed here"));
                }
            }
        }
        let _ = at;
        Ok((self.add_format(pieces), vals))
    }

    fn format_arg(&mut self, cx: &mut Cx<'a>, e: &Expr<'a>, spec: char) -> EResult<Val> {
        if matches!(spec, 'e' | 'f' | 'g') {
            return self.lower_real(cx, e);
        }
        if let Some(t) = self.array_type(cx, e) {
            // A whole array (for `%p`).
            return self.lower_array(cx, e, &t);
        }
        let (v, st) = self.lower_self(cx, e)?;
        if spec == 't' && st != STy::Real {
            return Ok(v);
        }
        Ok(v)
    }
}

/// Does a statement contain a timing control?
fn has_timing(s: &Stmt) -> bool {
    match s {
        Stmt::Timed { .. } | Stmt::Wait { .. } => true,
        Stmt::Assign {
            timing: Some(_), ..
        } => true,
        Stmt::Block { stmts, .. } => stmts.iter().any(has_timing),
        Stmt::If { then, els, .. } => has_timing(then) || els.as_deref().is_some_and(has_timing),
        Stmt::Case { items, .. } => items.iter().any(|i| has_timing(&i.stmt)),
        Stmt::For { body, .. }
        | Stmt::Foreach { body, .. }
        | Stmt::While { body, .. }
        | Stmt::DoWhile { body, .. }
        | Stmt::Repeat { body, .. }
        | Stmt::Forever { body, .. } => has_timing(body),
        Stmt::Labeled { stmt, .. } => has_timing(stmt),
        // A task call may wait inside the task.
        Stmt::Expr(Expr::Call { .. }) => true,
        _ => false,
    }
}

enum Piece {
    Text(String),
    Scope,
    Conv {
        spec: char,
        width: Option<u32>,
        zero_pad: bool,
        left: bool,
    },
}

/// Split a format string into text and conversions.
fn parse_format(s: &str) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            text.push(c);
            continue;
        }
        let mut digits = String::new();
        while let Some(&d) = chars.peek() {
            if d.is_ascii_digit() || d == '-' || d == '.' {
                digits.push(d);
                chars.next();
            } else {
                break;
            }
        }
        let Some(spec) = chars.next() else {
            text.push('%');
            break;
        };
        match spec.to_ascii_lowercase() {
            '%' => text.push('%'),
            'm' => {
                if !text.is_empty() {
                    out.push(Piece::Text(std::mem::take(&mut text)));
                }
                out.push(Piece::Scope);
            }
            'l' => text.push_str("work"),
            spec => {
                if !text.is_empty() {
                    out.push(Piece::Text(std::mem::take(&mut text)));
                }
                let left = digits.starts_with('-');
                let plain: String = digits
                    .trim_start_matches('-')
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                let zero_pad = plain.len() > 1 && plain.starts_with('0');
                let width = if plain.is_empty() {
                    None
                } else {
                    plain.parse().ok()
                };
                let spec = if spec == 'x' { 'h' } else { spec };
                out.push(Piece::Conv {
                    spec,
                    width,
                    zero_pad,
                    left,
                });
            }
        }
    }
    if !text.is_empty() {
        out.push(Piece::Text(text));
    }
    out
}
