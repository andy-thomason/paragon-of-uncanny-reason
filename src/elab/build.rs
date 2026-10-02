//! Building a [`Body`]: blocks, values and the current insertion point.

use crate::ir::*;
use std::collections::BTreeSet;

/// A body under construction. Instructions go into the current block until
/// it is terminated; after that, new code goes into a fresh block that
/// nothing reaches (code after `$finish` or `return`).
pub(crate) struct Builder<'a> {
    pub(crate) body: Body<'a>,
    cur: BlockId,
    terms: Vec<Option<Terminator>>,
    /// `(continue target, break target)` for enclosing loops.
    pub(crate) loops: Vec<(BlockId, BlockId)>,
    /// Design state read and written, for the whole body.
    pub(crate) reads: BTreeSet<VarId>,
    pub(crate) writes: BTreeSet<VarId>,
    /// Nested read sets, for `@*` and `wait`: every load adds to all of them.
    pub(crate) read_scopes: Vec<BTreeSet<VarId>>,
    /// Functions called, so implicit sensitivity can include what they read.
    pub(crate) calls: BTreeSet<FuncId>,
}

impl<'a> Builder<'a> {
    pub(crate) fn new() -> Self {
        let mut b = Builder {
            body: Body::default(),
            cur: BlockId(0),
            terms: Vec::new(),
            loops: Vec::new(),
            reads: BTreeSet::new(),
            writes: BTreeSet::new(),
            read_scopes: Vec::new(),
            calls: BTreeSet::new(),
        };
        b.cur = b.new_block();
        b
    }

    pub(crate) fn new_block(&mut self) -> BlockId {
        self.body.blocks.push(Block {
            params: Vec::new(),
            insts: Vec::new(),
            term: Terminator::Unreachable,
        });
        self.terms.push(None);
        BlockId(self.body.blocks.len() as u32 - 1)
    }

    pub(crate) fn current(&self) -> BlockId {
        self.cur
    }

    /// Continue emitting into `b`.
    pub(crate) fn switch_to(&mut self, b: BlockId) {
        self.cur = b;
    }

    pub(crate) fn is_terminated(&self) -> bool {
        self.terms[self.cur.0 as usize].is_some()
    }

    pub(crate) fn new_val(&mut self, ty: TypeId) -> Val {
        self.body.vals.push(ty);
        Val(self.body.vals.len() as u32 - 1)
    }

    pub(crate) fn new_slot(&mut self, ty: TypeId) -> SlotId {
        self.body.slots.push(ty);
        SlotId(self.body.slots.len() as u32 - 1)
    }

    pub(crate) fn val_type(&self, v: Val) -> TypeId {
        self.body.vals[v.0 as usize]
    }

    /// Add a parameter to block `b`.
    pub(crate) fn block_param(&mut self, b: BlockId, ty: TypeId) -> Val {
        let v = self.new_val(ty);
        self.body.blocks[b.0 as usize].params.push(v);
        v
    }

    fn ensure_open(&mut self) {
        if self.is_terminated() {
            let b = self.new_block();
            self.cur = b;
        }
    }

    /// Emit an instruction that defines a value of type `ty`.
    pub(crate) fn emit(&mut self, op: Op, ty: TypeId, at: &'a str) -> Val {
        self.ensure_open();
        self.note(&op);
        let v = self.new_val(ty);
        self.body.blocks[self.cur.0 as usize].insts.push(Inst {
            dst: Some(v),
            op,
            at,
        });
        v
    }

    /// Emit an instruction with no result.
    pub(crate) fn effect(&mut self, op: Op, at: &'a str) {
        self.ensure_open();
        self.note(&op);
        self.body.blocks[self.cur.0 as usize]
            .insts
            .push(Inst { dst: None, op, at });
    }

    fn note(&mut self, op: &Op) {
        match op {
            Op::Load(v) | Op::LoadElem { var: v, .. } => {
                self.reads.insert(*v);
                for s in &mut self.read_scopes {
                    s.insert(*v);
                }
            }
            Op::Store { var, .. } | Op::NbaStore { var, .. } | Op::StoreElem { var, .. } => {
                self.writes.insert(*var);
            }
            Op::Call { func, .. } => {
                self.calls.insert(*func);
            }
            _ => {}
        }
    }

    /// End the current block. Does nothing if it is already terminated.
    pub(crate) fn terminate(&mut self, t: Terminator) {
        if !self.is_terminated() {
            self.terms[self.cur.0 as usize] = Some(t);
        }
    }

    /// Jump to `b` (if the current block is still open) and continue there.
    pub(crate) fn goto(&mut self, b: BlockId) {
        self.terminate(Terminator::Jump(b, Vec::new()));
        self.cur = b;
    }

    /// Finish the body. Open blocks end with `default`.
    pub(crate) fn finish(mut self, default: Terminator) -> (Body<'a>, BTreeSet<FuncId>) {
        for (b, t) in self.body.blocks.iter_mut().zip(self.terms) {
            b.term = t.unwrap_or_else(|| default.clone());
        }
        self.body.reads = self.reads.into_iter().collect();
        self.body.writes = self.writes.into_iter().collect();
        (self.body, self.calls)
    }
}
