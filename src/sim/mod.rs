//! Reference interpreter: runs the IR. See `docs/design/20-architecture.md`.
//!
//! This is the simple, obviously-correct back end the code generator will be
//! checked against, not the fast one. Each process is a thread with a stack
//! of frames; a frame runs one body (a process or a function) from block to
//! block. The scheduler follows the IEEE 1800 time-step regions in a reduced
//! form:
//!
//! 1. **Active:** run every runnable thread until it suspends or ends.
//! 2. **Inactive:** threads that waited `#0` become active.
//! 3. **NBA:** apply non-blocking writes; any that change a value wake waiters.
//! 4. **Postponed:** print `$strobe` output.
//! 5. Advance to the next time with a delayed thread.

mod format;

use crate::eval::{Value, bits_info, eval_pure};
use crate::ir::Bits;
use crate::ir::*;
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

pub use format::{format_display, format_g};

/// Where a simulation sends what it produces.
pub trait Sink {
    /// Text from `$display`, `$write` and friends.
    fn display(&mut self, text: &str, time: u64);
    /// `$error`, `$warning`, `$info` or a failed assertion. `at` is the
    /// source slice of the call.
    fn report(&mut self, severity: ReportSeverity, message: &str, at: &str, time: u64);
    /// True if the simulation should stop early (the receiver went away).
    fn cancelled(&self) -> bool {
        false
    }
}

/// How a simulation ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    Finish,
    Stop,
    Fatal,
    /// Nothing left to run.
    Quiescent,
    /// The sink asked to stop.
    Cancelled,
    /// A step limit was reached (probably an infinite zero-delay loop).
    Hung,
}

/// Stored design state.
#[derive(Clone, Debug)]
enum Store {
    Scalar(Value),
    Array(Vec<Value>),
    Event,
}

type ThreadId = usize;

struct Frame<'d, 'a> {
    body: &'d Body<'a>,
    scope: ScopeId,
    vals: Vec<Option<Value>>,
    /// Shared with forked children, which see the same automatic variables.
    slots: Rc<RefCell<Vec<Value>>>,
    block: BlockId,
    ip: usize,
    /// Where the caller wants the return value.
    ret_to: Option<Val>,
}

#[derive(PartialEq, Eq)]
enum State {
    Ready,
    Waiting,
    Done,
}

struct Thread<'d, 'a> {
    frames: Vec<Frame<'d, 'a>>,
    state: State,
    parent: Option<ThreadId>,
    /// Children still running, for `join` and `wait fork`.
    live_children: usize,
    /// How this thread is waiting for its children.
    join: Option<Join>,
    /// The named blocks and task bodies it is executing: what, the frame
    /// depth it was entered at, and where a `disable` continues.
    blocks: Vec<(DisableTag, usize, BlockId)>,
    /// Blocks its ancestors were executing when they started it: disabling
    /// one ends this thread.
    inherited: Vec<DisableTag>,
    /// Ended by `kill` or `disable`, rather than by finishing.
    killed: bool,
    /// Its `process` object, once asked for.
    handle: Option<crate::eval::ObjRef>,
    /// Threads waiting in `await` for this one to end.
    awaiting: Vec<(ThreadId, u64)>,
}

/// A thread waiting on design state.
struct Waiter {
    thread: ThreadId,
    edge: Edge,
    /// Bumped each time the thread suspends, so stale waiters can be ignored.
    epoch: u64,
}

/// A pending non-blocking write: variable, element, part select, value.
type NbaWrite = (VarId, Option<i64>, Option<(i64, u32)>, Value);

/// Something due at a future time.
#[derive(Clone, Copy)]
enum Wake {
    /// A delayed thread, with its epoch when it was delayed.
    Thread(ThreadId, u64),
    /// Toggle clock `n` of [`Simulator::clocks`].
    Clock(usize),
}

/// A force on a variable (or element): which bits, their value, and the
/// thread that keeps it up to date.
struct Forced {
    mask: Bits,
    value: Bits,
    /// The force statement execution that set it.
    token: u64,
}

pub struct Simulator<'d, 'a> {
    d: &'d Design<'a>,
    store: Vec<Store>,
    threads: Vec<Thread<'d, 'a>>,
    epochs: Vec<u64>,
    active: VecDeque<ThreadId>,
    inactive: VecDeque<(ThreadId, u64)>,
    future: BTreeMap<(u64, u64), Wake>,
    /// Forced variables (and elements).
    forced: std::collections::HashMap<(VarId, Option<i64>), Forced>,
    /// Set while a write to a forced variable must not wake anyone itself.
    quiet: bool,
    /// Combinational logic is settling before time 0 starts.
    settling: bool,
    tokens: u64,
    /// Clocks driven from outside the design: the variable and its half period.
    clocks: Vec<(VarId, u64)>,
    seq: u64,
    nba: Vec<NbaWrite>,
    waiters: Vec<Vec<Waiter>>,
    strobes: Vec<String>,
    pub time: u64,
    rng: u64,
    steps: u64,
    /// Instruction budget, to stop zero-delay infinite loops.
    pub max_steps: u64,
}

impl<'d, 'a> Simulator<'d, 'a> {
    pub fn new(d: &'d Design<'a>) -> Self {
        let store = d
            .vars
            .iter()
            .map(|v| match &d.types[v.ty.0 as usize] {
                Type::Unpacked { elem, left, right } => {
                    let n = (right - left).unsigned_abs() as usize + 1;
                    Store::Array(vec![default(&d.types[elem.0 as usize]); n])
                }
                Type::Event => Store::Event,
                t => Store::Scalar(match &v.init {
                    Some(b) => Value::Bits(b.clone()),
                    None => default(t),
                }),
            })
            .collect();
        Simulator {
            d,
            store,
            threads: Vec::new(),
            epochs: Vec::new(),
            active: VecDeque::new(),
            inactive: VecDeque::new(),
            future: BTreeMap::new(),
            clocks: Vec::new(),
            forced: Default::default(),
            quiet: false,
            settling: false,
            tokens: 0,
            seq: 0,
            nba: Vec::new(),
            waiters: d.vars.iter().map(|_| Vec::new()).collect(),
            strobes: Vec::new(),
            time: 0,
            rng: 0x2545_f491_4f6c_dd1d,
            steps: 0,
            max_steps: 2_000_000_000,
        }
    }

    /// Set a variable (a top-level input, say) before the simulation starts.
    pub fn set_input(&mut self, var: VarId, value: Bits) {
        self.write(var, None, None, Value::Bits(value));
    }

    /// Drive `var` as a clock: 0 at time 0, inverted at `first`, then every
    /// `half_period` (in the design's precision) while the simulation runs.
    pub fn add_clock(&mut self, var: VarId, first: u64, half_period: u64) {
        self.set_input(var, Bits::zero(1));
        self.clocks.push((var, half_period.max(1)));
        self.seq += 1;
        self.future
            .insert((first.max(1), self.seq), Wake::Clock(self.clocks.len() - 1));
    }

    fn spawn(
        &mut self,
        body: &'d Body<'a>,
        scope: ScopeId,
        block: BlockId,
        slots: Option<Rc<RefCell<Vec<Value>>>>,
        vals: Vec<Option<Value>>,
        parent: Option<ThreadId>,
    ) -> ThreadId {
        let slots = slots.unwrap_or_else(|| Rc::new(RefCell::new(self.default_slots(body))));
        let vals = if vals.is_empty() {
            vec![None; body.vals.len()]
        } else {
            vals
        };
        let frame = Frame {
            body,
            scope,
            vals,
            slots,
            block,
            ip: 0,
            ret_to: None,
        };
        self.threads.push(Thread {
            frames: vec![frame],
            state: State::Ready,
            parent,
            live_children: 0,
            join: None,
            blocks: Vec::new(),
            inherited: Vec::new(),
            killed: false,
            handle: None,
            awaiting: Vec::new(),
        });
        self.epochs.push(0);
        self.threads.len() - 1
    }

    fn default_slots(&self, body: &Body<'a>) -> Vec<Value> {
        body.slots
            .iter()
            .map(|t| crate::eval::default_for(&self.d.types, &self.d.types[t.0 as usize]))
            .collect()
    }

    /// Run the whole simulation.
    pub fn run(&mut self, sink: &mut dyn Sink) -> End {
        let d = self.d;
        let comb = |k: ProcKind| {
            matches!(
                k,
                ProcKind::ContAssign | ProcKind::AlwaysComb | ProcKind::AlwaysLatch
            )
        };
        // Combinational logic settles before anything else starts, so
        // `initial` blocks see its values at time 0, as in Verilator. (The
        // LRM leaves the order open.)
        let combs: Vec<&Process> = d.procs.iter().filter(|p| comb(p.kind)).collect();
        for p in data_flow_order(&combs) {
            let t = self.spawn(&p.body, p.scope, BlockId(0), None, Vec::new(), None);
            self.active.push_back(t);
        }
        let mut settled = None;
        self.settling = true;
        while let Some(t) = self.active.pop_front() {
            if let Some(end) = self.run_thread(t, sink) {
                settled = Some(end);
                break;
            }
        }
        self.settling = false;
        for p in &d.procs {
            if p.kind == ProcKind::Final || comb(p.kind) {
                continue;
            }
            let t = self.spawn(&p.body, p.scope, BlockId(0), None, Vec::new(), None);
            self.active.push_back(t);
        }
        let end = match settled {
            Some(end) => end,
            None => self.schedule(sink),
        };
        // `final` blocks run once, at the end, whatever ended the simulation.
        if matches!(end, End::Finish | End::Stop | End::Quiescent | End::Fatal) {
            for p in &d.procs {
                if p.kind == ProcKind::Final {
                    let t = self.spawn(&p.body, p.scope, BlockId(0), None, Vec::new(), None);
                    self.active.push_back(t);
                }
            }
            while let Some(t) = self.active.pop_front() {
                if let Some(e) = self.run_thread(t, sink) {
                    let _ = e;
                    break;
                }
            }
        }
        end
    }

    fn schedule(&mut self, sink: &mut dyn Sink) -> End {
        loop {
            while let Some(t) = self.active.pop_front() {
                if sink.cancelled() {
                    return End::Cancelled;
                }
                if let Some(end) = self.run_thread(t, sink) {
                    self.flush_strobes(sink);
                    return end;
                }
            }
            if !self.inactive.is_empty() {
                let ready: Vec<(ThreadId, u64)> = self.inactive.drain(..).collect();
                for (t, e) in ready {
                    self.resume_delayed(t, e);
                }
                continue;
            }
            if !self.nba.is_empty() {
                let nba = std::mem::take(&mut self.nba);
                for (var, elem, part, value) in nba {
                    self.write(var, elem, part, value);
                }
                continue;
            }
            self.flush_strobes(sink);
            // Clocks alone do not keep a simulation going.
            if self.future.values().all(|w| matches!(w, Wake::Clock(_)))
                && self.threads.iter().all(|t| t.state == State::Done)
            {
                return End::Quiescent;
            }
            let Some((&(when, _), _)) = self.future.iter().next() else {
                return End::Quiescent;
            };
            self.time = when;
            // Everything due at this time runs in the same step.
            while let Some((&(w, s), &wake)) = self.future.iter().next() {
                if w != when {
                    break;
                }
                self.future.remove(&(w, s));
                match wake {
                    Wake::Thread(t, e) => self.resume_delayed(t, e),
                    Wake::Clock(n) => {
                        self.steps += 1;
                        if self.steps > self.max_steps {
                            return End::Hung;
                        }
                        let (var, half) = self.clocks[n];
                        let high = self
                            .read(var)
                            .bits()
                            .is_some_and(|b| b.bit(0) == (true, false));
                        self.write(var, None, None, Value::Bits(Bits::from_bool(!high)));
                        self.seq += 1;
                        self.future.insert((when + half, self.seq), Wake::Clock(n));
                    }
                }
            }
        }
    }

    /// Run a thread whose delay is over, unless it was disabled meanwhile.
    fn resume_delayed(&mut self, t: ThreadId, epoch: u64) {
        if self.epochs[t] == epoch && self.threads[t].state == State::Waiting {
            self.epochs[t] += 1;
            self.threads[t].state = State::Ready;
            self.active.push_back(t);
        }
    }

    fn flush_strobes(&mut self, sink: &mut dyn Sink) {
        for s in std::mem::take(&mut self.strobes) {
            sink.display(&s, self.time);
        }
    }

    // ------------------------------------------------------------ state

    fn read(&self, var: VarId) -> Value {
        let raw = self.read_raw(var);
        self.overlay(var, None, raw)
    }

    /// A value with any force on it applied.
    fn overlay(&self, var: VarId, elem: Option<i64>, v: Value) -> Value {
        match (self.forced.get(&(var, elem)), v) {
            (Some(f), Value::Bits(b)) => {
                let b = b.resize(f.mask.width, false);
                Value::Bits(b.and(&f.mask.not()).or(&f.value.and(&f.mask)))
            }
            (_, v) => v,
        }
    }

    fn read_raw(&self, var: VarId) -> Value {
        match &self.store[var.0 as usize] {
            Store::Scalar(v) => v.clone(),
            Store::Array(a) => Value::Array(a.clone()),
            Store::Event => Value::Bits(Bits::zero(1)),
        }
    }

    /// `len` elements from linear element `start`; out of range ones are the default.
    fn read_range(&self, var: VarId, start: Option<i64>, len: u32) -> Value {
        Value::Array(
            (0..len as i64)
                .map(|k| self.read_elem(var, start.map(|s| s + k)))
                .collect(),
        )
    }

    fn read_elem(&self, var: VarId, i: Option<i64>) -> Value {
        let raw = self.read_elem_raw(var, i);
        match i {
            Some(_) => self.overlay(var, i, raw),
            None => raw,
        }
    }

    fn read_elem_raw(&self, var: VarId, i: Option<i64>) -> Value {
        match (&self.store[var.0 as usize], i) {
            // A dynamic array or queue.
            (Store::Scalar(Value::Array(a)), i) => match i {
                Some(i) if i >= 0 && (i as usize) < a.len() => a[i as usize].clone(),
                _ => match &self.d.types[self.d.vars[var.0 as usize].ty.0 as usize] {
                    Type::Dynamic { elem } | Type::Queue { elem, .. } => {
                        crate::eval::default_for(&self.d.types, &self.d.types[elem.0 as usize])
                    }
                    _ => Value::Bits(Bits::all_x(1)),
                },
            },
            (Store::Array(a), Some(i)) if i >= 0 && (i as usize) < a.len() => a[i as usize].clone(),
            (Store::Array(a), _) => {
                // Out of range: the element type's default (X for 4-state).
                let elem = match &self.d.types[self.d.vars[var.0 as usize].ty.0 as usize] {
                    Type::Unpacked { elem, .. } => *elem,
                    _ => return a.first().cloned().unwrap_or(Value::Bits(Bits::all_x(1))),
                };
                default(&self.d.types[elem.0 as usize])
            }
            _ => self.read(var),
        }
    }

    /// Write design state, converting to the stored type, and wake waiters if it changed.
    fn write(&mut self, var: VarId, elem: Option<i64>, part: Option<(i64, u32)>, value: Value) {
        if !self.forced.is_empty() && self.forced.contains_key(&(var, elem)) {
            // Forced: the stored value changes, but waiters see only what
            // reads see.
            let before = self.visible(var, elem);
            self.quiet = true;
            self.write_raw(var, elem, part, value);
            self.quiet = false;
            let after = self.visible(var, elem);
            if before != after {
                self.wake(var, &before, &after);
            }
            return;
        }
        self.write_raw(var, elem, part, value)
    }

    /// What reads of a variable (or element) see.
    fn visible(&self, var: VarId, elem: Option<i64>) -> Value {
        match elem {
            Some(_) => self.read_elem(var, elem),
            None => self.read(var),
        }
    }

    /// The bits of `part` (all of them if `None`) of a `width`-bit value.
    fn part_mask(width: u32, part: Option<(i64, u32)>) -> Bits {
        match part {
            None => Bits::ones(width),
            Some((lsb, w)) => {
                let mut m = Bits::zero(width);
                m.insert(lsb, &Bits::ones(w));
                m
            }
        }
    }

    /// `force`: keep the forced bits, and wake anyone who sees a change.
    fn force(&mut self, token: u64, var: VarId, elem: Option<i64>, part: Option<(i64, u32)>, value: Value) {
        let Value::Bits(v) = self.visible(var, elem) else {
            return;
        };
        let before = Value::Bits(v.clone());
        let width = v.width;
        let mask = Self::part_mask(width, part);
        let Value::Bits(nv) = value else { return };
        let placed = match part {
            None => nv.resize(width, false),
            Some((lsb, w)) => {
                let mut p = Bits::zero(width);
                p.insert(lsb, &nv.resize(w, false));
                p
            }
        };
        let e = self.forced.entry((var, elem)).or_insert(Forced {
            mask: Bits::zero(width),
            value: Bits::zero(width),
            token,
        });
        e.value = e.value.and(&mask.not()).or(&placed.and(&mask));
        e.mask = e.mask.or(&mask);
        e.token = token;
        let after = self.visible(var, elem);
        if before != after {
            self.wake(var, &before, &after);
        }
    }

    /// `release`: a variable keeps the forced value until next assigned;
    /// a net goes back to what drives it.
    fn release(&mut self, var: VarId, elem: Option<i64>, part: Option<(i64, u32)>) {
        let Some(f) = self.forced.get(&(var, elem)) else {
            return;
        };
        let before = self.visible(var, elem);
        let mask = Self::part_mask(f.mask.width, part).and(&f.mask);
        if matches!(self.d.vars[var.0 as usize].kind, VarKind::Variable)
            && let Value::Bits(b) = &before
        {
            // Keep the forced bits as the variable's own value.
            let keep = b.clone();
            self.quiet = true;
            self.write_raw(var, elem, None, Value::Bits(keep));
            self.quiet = false;
        }
        let f = self.forced.get_mut(&(var, elem)).unwrap();
        f.mask = f.mask.and(&mask.not());
        if f.mask.is_zero() {
            self.forced.remove(&(var, elem));
        }
        let after = self.visible(var, elem);
        if before != after {
            self.wake(var, &before, &after);
        }
    }

    fn write_raw(&mut self, var: VarId, elem: Option<i64>, part: Option<(i64, u32)>, value: Value) {
        if let Store::Scalar(Value::Array(_)) = &self.store[var.0 as usize] {
            // A dynamic array or queue: the whole value, or one element.
            let ety = match &self.d.types[self.d.vars[var.0 as usize].ty.0 as usize] {
                Type::Dynamic { elem } | Type::Queue { elem, .. } => *elem,
                _ => return,
            };
            let ety = &self.d.types[ety.0 as usize];
            let Store::Scalar(slot) = &mut self.store[var.0 as usize] else {
                unreachable!()
            };
            let changed = match elem {
                None => {
                    let changed = *slot != value;
                    *slot = value;
                    changed
                }
                Some(i) => crate::eval::store_elem(slot, i, part, value, ety),
            };
            if changed {
                self.wake(var, &Value::Bits(Bits::zero(1)), &Value::Bits(Bits::ones(1)));
            }
            return;
        }
        if let Value::Array(vals) = value {
            // Consecutive elements from `elem` (the whole array from 0).
            let start = elem.unwrap_or(0);
            let ety = match &self.d.types[self.d.vars[var.0 as usize].ty.0 as usize] {
                Type::Unpacked { elem, .. } => *elem,
                _ => return,
            };
            let ety = &self.d.types[ety.0 as usize];
            let Store::Array(a) = &mut self.store[var.0 as usize] else {
                return;
            };
            let mut changed = false;
            for (k, v) in vals.into_iter().enumerate() {
                let i = start + k as i64;
                if i < 0 || i as usize >= a.len() {
                    continue;
                }
                let new = merge(&a[i as usize], None, v, ety);
                if a[i as usize] != new {
                    a[i as usize] = new;
                    changed = true;
                }
            }
            if changed {
                self.wake(var, &Value::Bits(Bits::zero(1)), &Value::Bits(Bits::ones(1)));
            }
            return;
        }
        let vty = self.d.vars[var.0 as usize].ty;
        let ety = match &self.d.types[vty.0 as usize] {
            Type::Unpacked { elem, .. } => *elem,
            _ => vty,
        };
        let ety = &self.d.types[ety.0 as usize];
        let slot = match (&mut self.store[var.0 as usize], elem) {
            (Store::Scalar(v), None) => v,
            (Store::Array(a), Some(i)) if i >= 0 && (i as usize) < a.len() => &mut a[i as usize],
            // Writes out of range are ignored.
            _ => return,
        };
        let new = merge(slot, part, value, ety);
        if *slot == new {
            return;
        }
        let old = std::mem::replace(slot, new);
        let new = slot.clone();
        self.wake(var, &old, &new);
    }

    fn wake(&mut self, var: VarId, old: &Value, new: &Value) {
        if self.quiet {
            return;
        }
        let ws = std::mem::take(&mut self.waiters[var.0 as usize]);
        let mut keep = Vec::new();
        for w in ws {
            if self.epochs[w.thread] != w.epoch || self.threads[w.thread].state != State::Waiting {
                continue;
            }
            if edge_fired(w.edge, old, new) {
                self.epochs[w.thread] += 1;
                self.threads[w.thread].state = State::Ready;
                self.active.push_back(w.thread);
            } else {
                keep.push(w);
            }
        }
        self.waiters[var.0 as usize].extend(keep);
    }

    fn rand(&mut self) -> u64 {
        // xorshift64*: any decent generator will do; tests that depend on a
        // particular random sequence are out of scope (decision 3).
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        self.rng.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// The current time in a scope's time unit.
    fn time_in(&self, scope: ScopeId) -> f64 {
        let unit = self.d.scopes[scope.0 as usize].unit;
        self.time as f64 / 10f64.powi((unit - self.d.precision) as i32)
    }

    // ------------------------------------------------------------ threads

    /// Run a thread until it suspends or ends. Returns `Some` if the simulation ends.
    fn run_thread(&mut self, t: ThreadId, sink: &mut dyn Sink) -> Option<End> {
        if self.threads[t].state == State::Done {
            return None;
        }
        self.threads[t].state = State::Ready;
        loop {
            self.steps += 1;
            if self.steps > self.max_steps {
                return Some(End::Hung);
            }
            if self.steps.is_multiple_of(65536) && sink.cancelled() {
                return Some(End::Cancelled);
            }
            let Some(frame) = self.threads[t].frames.last_mut() else {
                self.finish_thread(t);
                return None;
            };
            let body = frame.body;
            let block = &body.blocks[frame.block.0 as usize];
            if frame.ip < block.insts.len() {
                let inst = &block.insts[frame.ip];
                frame.ip += 1;
                if let Some(end) = self.exec(t, inst, sink) {
                    return Some(end);
                }
                continue;
            }
            // Terminator.
            match self.terminate(t, &block.term, sink) {
                Flow::Continue => {}
                Flow::Suspend => return None,
                Flow::End(e) => return Some(e),
            }
        }
    }

    fn val(&self, t: ThreadId, v: Val) -> Value {
        self.threads[t].frames.last().unwrap().vals[v.0 as usize]
            .clone()
            .unwrap_or(Value::Bits(Bits::all_x(1)))
    }

    fn val_ty(&self, t: ThreadId, v: Val) -> &'d Type<'a> {
        let f = self.threads[t].frames.last().unwrap();
        &self.d.types[f.body.vals[v.0 as usize].0 as usize]
    }

    fn set(&mut self, t: ThreadId, v: Val, value: Value) {
        self.threads[t].frames.last_mut().unwrap().vals[v.0 as usize] = Some(value);
    }

    fn int(&self, t: ThreadId, v: Val) -> Option<i64> {
        let signed = bits_info(self.val_ty(t, v)).is_some_and(|(_, s, _)| s);
        self.val(t, v).bits().and_then(|b| b.to_i64(signed))
    }

    fn part(&self, t: ThreadId, p: &Option<Part>) -> Option<Option<(i64, u32)>> {
        match p {
            None => Some(None),
            Some(p) => self.int(t, p.lsb).map(|l| Some((l, p.width))),
        }
    }

    fn exec(&mut self, t: ThreadId, inst: &'d Inst<'a>, sink: &mut dyn Sink) -> Option<End> {
        let frame_scope = self.threads[t].frames.last().unwrap().scope;
        let result: Option<Value> = match &inst.op {
            Op::Load(v) => Some(self.read(*v)),
            Op::LoadElem { var, index } => {
                let i = self.int(t, *index);
                Some(self.read_elem(*var, i))
            }
            Op::LoadRange { var, start, len } => {
                let s = self.int(t, *start);
                Some(self.read_range(*var, s, *len))
            }
            Op::Store { var, part, value } => {
                if let Some(p) = self.part(t, part) {
                    let v = self.val(t, *value);
                    self.write(*var, None, p, v);
                }
                None
            }
            Op::NbaStore { var, part, value } => {
                if let Some(p) = self.part(t, part) {
                    let v = self.val(t, *value);
                    self.nba.push((*var, None, p, v));
                }
                None
            }
            Op::StoreElem {
                var,
                index,
                part,
                value,
                nba,
            } => {
                if let (Some(i), Some(p)) = (self.int(t, *index), self.part(t, part)) {
                    let v = self.val(t, *value);
                    if *nba {
                        self.nba.push((*var, Some(i), p, v));
                    } else {
                        self.write(*var, Some(i), p, v);
                    }
                }
                None
            }
            Op::LoadSlot(s) => {
                Some(self.threads[t].frames.last().unwrap().slots.borrow()[s.0 as usize].clone())
            }
            Op::StoreSlotElem {
                slot,
                index,
                part,
                value,
            } => {
                let v = self.val(t, *value);
                if let (Some(i), Some(p)) = (self.int(t, *index), self.part(t, part)) {
                    let f = self.threads[t].frames.last().unwrap();
                    let elem = match &self.d.types[f.body.slots[slot.0 as usize].0 as usize] {
                        Type::Unpacked { elem, .. } => &self.d.types[elem.0 as usize],
                        _ => return None,
                    };
                    let mut slots = f.slots.borrow_mut();
                    crate::eval::store_elem(&mut slots[slot.0 as usize], i, p, v, elem);
                }
                None
            }
            Op::StoreSlot { slot, part, value } => {
                let v = self.val(t, *value);
                if let Some(p) = self.part(t, part) {
                    let f = self.threads[t].frames.last().unwrap();
                    let ty = &self.d.types[f.body.slots[slot.0 as usize].0 as usize];
                    let mut slots = f.slots.borrow_mut();
                    let new = merge(&slots[slot.0 as usize], p, v, ty);
                    slots[slot.0 as usize] = new;
                }
                None
            }
            Op::Call { func, args } => {
                if self.d.funcs[func.0 as usize].body.blocks.is_empty() {
                    return Some(self.no_body(*func, inst.at, sink));
                }
                let vals: Vec<Value> = args.iter().map(|a| self.val(t, *a)).collect();
                self.push_call(t, *func, vals, inst.dst);
                return None;
            }
            Op::VCall { slot, args } => {
                let vals: Vec<Value> = args.iter().map(|a| self.val(t, *a)).collect();
                let Some(Value::Obj(Some(o))) = vals.first() else {
                    return Some(self.null_error(inst.at, sink));
                };
                let class = o.0.borrow().class;
                let func = self.d.classes[class.0 as usize].vtable[*slot as usize];
                if self.d.funcs[func.0 as usize].body.blocks.is_empty() {
                    return Some(self.no_body(func, inst.at, sink));
                }
                self.push_call(t, func, vals, inst.dst);
                return None;
            }
            Op::New(c) => {
                let fields = self.d.classes[c.0 as usize]
                    .fields
                    .iter()
                    .map(|(_, ty)| crate::eval::default_for(&self.d.types, &self.d.types[ty.0 as usize]))
                    .collect();
                let names = Rc::new(
                    self.d.classes[c.0 as usize]
                        .fields
                        .iter()
                        .map(|(n, _)| n.to_string())
                        .collect(),
                );
                Some(Value::Obj(Some(crate::eval::ObjRef(Rc::new(RefCell::new(
                    crate::eval::Object {
                        class: *c,
                        fields,
                        names,
                    },
                ))))))
            }
            Op::CopyObj(v) => match self.val(t, *v) {
                Value::Obj(Some(o)) => {
                    let copy = o.0.borrow().clone();
                    Some(Value::Obj(Some(crate::eval::ObjRef(Rc::new(RefCell::new(copy))))))
                }
                _ => return Some(self.null_error(inst.at, sink)),
            },
            Op::LoadField { obj, field } => match self.val(t, *obj) {
                Value::Obj(Some(o)) => Some(o.0.borrow().fields[*field as usize].clone()),
                _ => return Some(self.null_error(inst.at, sink)),
            },
            Op::StoreField {
                obj,
                field,
                part,
                value,
            } => {
                let Value::Obj(Some(o)) = self.val(t, *obj) else {
                    return Some(self.null_error(inst.at, sink));
                };
                if let Some(p) = self.part(t, part) {
                    let v = self.val(t, *value);
                    let class = o.0.borrow().class;
                    let fty = self.d.classes[class.0 as usize].fields[*field as usize].1;
                    let fty = &self.d.types[fty.0 as usize];
                    let mut ob = o.0.borrow_mut();
                    let new = merge(&ob.fields[*field as usize], p, v, fty);
                    ob.fields[*field as usize] = new;
                }
                None
            }
            Op::StoreFieldElem {
                obj,
                field,
                index,
                part,
                value,
            } => {
                let Value::Obj(Some(o)) = self.val(t, *obj) else {
                    return Some(self.null_error(inst.at, sink));
                };
                if let (Some(i), Some(p)) = (self.int(t, *index), self.part(t, part)) {
                    let v = self.val(t, *value);
                    let class = o.0.borrow().class;
                    let fty = self.d.classes[class.0 as usize].fields[*field as usize].1;
                    let elem = match &self.d.types[fty.0 as usize] {
                        Type::Unpacked { elem, .. } | Type::Dynamic { elem } | Type::Queue { elem, .. } => {
                            &self.d.types[elem.0 as usize]
                        }
                        _ => return None,
                    };
                    let mut ob = o.0.borrow_mut();
                    crate::eval::store_elem(&mut ob.fields[*field as usize], i, p, v, elem);
                }
                None
            }
            Op::Force {
                var,
                elem,
                part,
                value,
                token,
            } => {
                let tok = match token {
                    Some(k) => self.int(t, *k).unwrap_or(0) as u64,
                    None => {
                        self.tokens += 1;
                        self.tokens
                    }
                };
                let e = elem.map(|e| self.int(t, e));
                if let (Some(p), e) = (self.part(t, part), e)
                    && e != Some(None)
                {
                    let v = self.val(t, *value);
                    self.force(tok, *var, e.flatten(), p, v);
                }
                Some(Value::Bits(Bits::from_u64(64, tok)))
            }
            Op::Violation { format } => {
                if self.settling {
                    return None;
                }
                let unit = self.d.scopes[frame_scope.0 as usize].unit;
                let text = format_display(
                    &self.d.formats[format.0 as usize],
                    &[],
                    self.time,
                    unit,
                    self.d.precision,
                );
                sink.report(ReportSeverity::Error, &text, inst.at, self.time);
                return Some(End::Stop);
            }
            Op::Release { var, elem, part } => {
                let e = elem.map(|e| self.int(t, e)).flatten();
                if let Some(p) = self.part(t, part) {
                    self.release(*var, e, p);
                }
                None
            }
            Op::IsForcer {
                var,
                elem,
                part,
                token,
            } => {
                let e = elem.map(|e| self.int(t, e)).flatten();
                let tok = self.int(t, *token).unwrap_or(0) as u64;
                let mine = match (self.forced.get(&(*var, e)), self.part(t, part)) {
                    (Some(f), Some(p)) => {
                        f.token == tok && !Self::part_mask(f.mask.width, p).and(&f.mask).is_zero()
                    }
                    _ => false,
                };
                Some(Value::Bits(Bits::from_bool(mine)))
            }
            Op::Process { func, args } => {
                let target = |this: &Self, i: usize| -> Option<ThreadId> {
                    match this.val(t, args[i]) {
                        Value::Obj(Some(o)) => o.0.borrow().fields[0]
                            .bits()
                            .map(|b| b.to_u64() as ThreadId),
                        _ => None,
                    }
                };
                match func {
                    ProcFunc::SelfHandle => {
                        if self.threads[t].handle.is_none() {
                            let class = self.d.process_class.expect("process class");
                            let o = crate::eval::ObjRef(Rc::new(RefCell::new(crate::eval::Object {
                                class,
                                fields: vec![Value::Bits(Bits::from_u64(32, t as u64))],
                                names: Rc::new(vec!["id".into()]),
                            })));
                            self.threads[t].handle = Some(o);
                        }
                        Some(Value::Obj(self.threads[t].handle.clone()))
                    }
                    ProcFunc::Status => {
                        let Some(p) = target(self, 0) else {
                            return Some(self.null_error(inst.at, sink));
                        };
                        let th = &self.threads[p];
                        let s = match th.state {
                            State::Done if th.killed => 4,
                            State::Done => 0,
                            State::Waiting => 2,
                            State::Ready if p == t => 1,
                            State::Ready => 1,
                        };
                        Some(Value::Bits(Bits::from_u64(32, s)))
                    }
                    ProcFunc::Kill => {
                        let Some(p) = target(self, 0) else {
                            return Some(self.null_error(inst.at, sink));
                        };
                        for d in self.descendants(p) {
                            self.threads[d].killed = true;
                            self.finish_thread(d);
                        }
                        if self.threads[p].state != State::Done {
                            self.threads[p].killed = true;
                            self.finish_thread(p);
                        }
                        None
                    }
                    ProcFunc::Ignore => None,
                    ProcFunc::GetRandstate => Some(Value::Str(format!("{:016x}", self.rng))),
                }
            }
            Op::IsA { value, class } => {
                let mut ok = false;
                if let Value::Obj(Some(o)) = self.val(t, *value) {
                    let mut c = Some(o.0.borrow().class);
                    while let Some(x) = c {
                        if x == *class {
                            ok = true;
                            break;
                        }
                        c = self.d.classes[x.0 as usize].base;
                    }
                }
                Some(Value::Bits(Bits::from_bool(ok)))
            }
            Op::Display { kind, format, args } => {
                let vals: Vec<(Value, &Type)> = args
                    .iter()
                    .map(|a| (self.val(t, *a), self.val_ty(t, *a)))
                    .collect();
                let unit = self.d.scopes[frame_scope.0 as usize].unit;
                let mut text = format_display(
                    &self.d.formats[format.0 as usize],
                    &vals,
                    self.time,
                    unit,
                    self.d.precision,
                );
                match kind {
                    DisplayKind::Display => {
                        text.push('\n');
                        sink.display(&text, self.time);
                    }
                    DisplayKind::Write => sink.display(&text, self.time),
                    DisplayKind::Strobe => {
                        text.push('\n');
                        self.strobes.push(text);
                    }
                }
                None
            }
            Op::Report {
                severity,
                format,
                args,
            } => {
                let msg = match format {
                    Some(f) => {
                        let vals: Vec<(Value, &Type)> = args
                            .iter()
                            .map(|a| (self.val(t, *a), self.val_ty(t, *a)))
                            .collect();
                        let unit = self.d.scopes[frame_scope.0 as usize].unit;
                        format_display(
                            &self.d.formats[f.0 as usize],
                            &vals,
                            self.time,
                            unit,
                            self.d.precision,
                        )
                    }
                    None => String::new(),
                };
                sink.report(*severity, &msg, inst.at, self.time);
                None
            }
            Op::BlockEnter { tag, exit } => {
                let depth = self.threads[t].frames.len();
                self.threads[t].blocks.push((*tag, depth, *exit));
                None
            }
            Op::BlockLeave(tag) => {
                let b = &mut self.threads[t].blocks;
                if let Some(k) = b.iter().rposition(|x| x.0 == *tag) {
                    b.truncate(k);
                }
                None
            }
            Op::DisableFork => {
                for d in self.descendants(t) {
                    self.finish_thread(d);
                }
                None
            }
            Op::Sformat { format, args } => {
                let vals: Vec<(Value, &Type)> = args
                    .iter()
                    .map(|a| (self.val(t, *a), self.val_ty(t, *a)))
                    .collect();
                let unit = self.d.scopes[frame_scope.0 as usize].unit;
                let text = format_display(
                    &self.d.formats[format.0 as usize],
                    &vals,
                    self.time,
                    unit,
                    self.d.precision,
                );
                Some(Value::Str(text))
            }
            Op::TriggerEvent(v) => {
                self.wake(*v, &Value::Bits(Bits::zero(1)), &Value::Bits(Bits::ones(1)));
                None
            }
            Op::SysFunc { func, args } => {
                let ty = self.val_ty(t, inst.dst.unwrap());
                let (w, _, _) = bits_info(ty).unwrap_or((64, false, false));
                Some(match func {
                    SysFunc::Time => {
                        Value::Bits(Bits::from_u64(64, self.time_in(frame_scope).round() as u64))
                    }
                    SysFunc::Stime => {
                        Value::Bits(Bits::from_u64(32, self.time_in(frame_scope).round() as u64))
                    }
                    SysFunc::Realtime => Value::Real(self.time_in(frame_scope)),
                    SysFunc::Random | SysFunc::Urandom => {
                        // As many random bits as the result needs.
                        let mut b = Bits::from_u64(w.min(64), self.rand());
                        while b.width < w {
                            let more = Bits::from_u64(64, self.rand());
                            b = Bits::concat(&[more, b]);
                        }
                        Value::Bits(b.resize(w, false))
                    }
                    SysFunc::UrandomRange => {
                        let a = self.int(t, args[0]).unwrap_or(0) as u64;
                        let b = args.get(1).and_then(|v| self.int(t, *v)).unwrap_or(0) as u64;
                        let (lo, hi) = (a.min(b), a.max(b));
                        let span = hi.wrapping_sub(lo).wrapping_add(1);
                        let r = if span == 0 { self.rand() } else { lo + self.rand() % span };
                        Value::Bits(Bits::from_u64(w, r))
                    }
                    SysFunc::RandomPick => {
                        let bounds: Vec<i64> = args.iter().map(|a| self.int(t, *a).unwrap_or(0)).collect();
                        let ranges: Vec<(i64, i64)> = bounds.chunks(2).map(|p| (p[0], p[1])).collect();
                        let total: u128 = ranges.iter().map(|(l, h)| (*h as i128 - *l as i128 + 1) as u128).sum();
                        let r = ((self.rand() as u128) << 64 | self.rand() as u128) % total.max(1);
                        let mut acc = 0u128;
                        let mut v = 0i64;
                        for (l, h) in &ranges {
                            let n = (*h as i128 - *l as i128 + 1) as u128;
                            if r < acc + n {
                                v = (*l as i128 + (r - acc) as i128) as i64;
                                break;
                            }
                            acc += n;
                        }
                        Value::Bits(Bits::from_i64(w, v))
                    }
                    SysFunc::Clog2 => {
                        let v = self.val(t, args[0]);
                        Value::Bits(Bits::from_u64(w, v.bits().map_or(0, |b| b.clog2()) as u64))
                    }
                    SysFunc::Countones => {
                        let v = self.val(t, args[0]);
                        Value::Bits(Bits::from_u64(
                            w,
                            v.bits().map_or(0, |b| b.count_ones() as u64),
                        ))
                    }
                    SysFunc::Onehot | SysFunc::Onehot0 => {
                        let n = self.val(t, args[0]).bits().map_or(0, |b| b.count_ones());
                        let ok = if *func == SysFunc::Onehot {
                            n == 1
                        } else {
                            n <= 1
                        };
                        Value::Bits(Bits::from_bool(ok))
                    }
                    SysFunc::IsUnknown => Value::Bits(Bits::from_bool(
                        self.val(t, args[0]).bits().is_some_and(|b| b.has_unknown()),
                    )),
                    _ => Value::Bits(Bits::all_x(w)),
                })
            }
            op => {
                let ty = inst.dst.map_or(&Type::Real, |d| self.val_ty(t, d));
                let f = self.threads[t].frames.last().unwrap();
                let x1 = Value::Real(0.0);
                let get = |v: Val| {
                    (
                        f.vals[v.0 as usize].as_ref().unwrap_or(&x1),
                        &self.d.types[f.body.vals[v.0 as usize].0 as usize],
                    )
                };
                eval_pure(op, ty, get)
            }
        };
        if let (Some(d), Some(r)) = (inst.dst, result) {
            self.set(t, d, r);
        }
        None
    }

    /// Call `func` with `vals` as its arguments; the result goes to `dst`.
    fn push_call(&mut self, t: ThreadId, func: FuncId, vals: Vec<Value>, dst: Option<Val>) {
        let f = &self.d.funcs[func.0 as usize];
        let mut frame_vals = vec![None; f.body.vals.len()];
        for (p, v) in f.body.blocks[0].params.iter().zip(vals) {
            frame_vals[p.0 as usize] = Some(v);
        }
        let slots = Rc::new(RefCell::new(self.default_slots(&f.body)));
        self.threads[t].frames.push(Frame {
            body: &f.body,
            scope: f.scope,
            vals: frame_vals,
            slots,
            block: BlockId(0),
            ip: 0,
            ret_to: dst,
        });
    }

    /// Calling a pure virtual method (or one never lowered) is an error.
    fn no_body(&mut self, func: FuncId, at: &str, sink: &mut dyn Sink) -> End {
        let name = self.d.funcs[func.0 as usize].name;
        sink.report(
            ReportSeverity::Error,
            &format!("Call of '{name}', which has no implementation"),
            at,
            self.time,
        );
        End::Fatal
    }

    /// Using a null handle ends the simulation, as in Verilator.
    fn null_error(&mut self, at: &str, sink: &mut dyn Sink) -> End {
        sink.report(ReportSeverity::Error, "Null pointer dereferenced", at, self.time);
        End::Fatal
    }

    fn jump(&mut self, t: ThreadId, (b, args): &(BlockId, Vec<Val>)) {
        let vals: Vec<Value> = args.iter().map(|a| self.val(t, *a)).collect();
        let f = self.threads[t].frames.last_mut().unwrap();
        let params = &f.body.blocks[b.0 as usize].params;
        for (p, v) in params.iter().zip(vals) {
            f.vals[p.0 as usize] = Some(v);
        }
        f.block = *b;
        f.ip = 0;
    }

    fn terminate(&mut self, t: ThreadId, term: &'d Terminator, sink: &mut dyn Sink) -> Flow {
        let _ = sink;
        match term {
            Terminator::Jump(b, args) => {
                self.jump(t, &(*b, args.clone()));
                Flow::Continue
            }
            Terminator::Branch { cond, then, els } => {
                let c = self
                    .val(t, *cond)
                    .bits()
                    .and_then(Bits::truth)
                    .unwrap_or(false);
                self.jump(t, if c { then } else { els });
                Flow::Continue
            }
            Terminator::Switch {
                value,
                cases,
                default,
            } => {
                let v = self.val(t, *value);
                let target = cases
                    .iter()
                    .find(|(c, _)| v.bits().is_some_and(|b| b.case_eq(c)))
                    .map_or(*default, |(_, b)| *b);
                self.jump(t, &(target, vec![]));
                Flow::Continue
            }
            Terminator::Return(v) => {
                let value = v.map(|v| self.val(t, v));
                let frame = self.threads[t].frames.pop().unwrap();
                let depth = self.threads[t].frames.len();
                self.threads[t].blocks.retain(|b| b.1 <= depth);
                if self.threads[t].frames.is_empty() {
                    self.finish_thread(t);
                    return Flow::Suspend;
                }
                if let (Some(dst), Some(value)) = (frame.ret_to, value) {
                    // Convert to the caller's view of the result type.
                    self.set(t, dst, value);
                }
                Flow::Continue
            }
            Terminator::Suspend { wait, resume } => {
                self.jump(t, &(*resume, vec![]));
                self.suspend(t, wait);
                Flow::Suspend
            }
            Terminator::Fork {
                children,
                join,
                resume,
            } => {
                let f = self.threads[t].frames.last().unwrap();
                let (body, scope, slots, vals) = (f.body, f.scope, f.slots.clone(), f.vals.clone());
                let mut inherited = self.threads[t].inherited.clone();
                inherited.extend(self.threads[t].blocks.iter().map(|b| b.0));
                for c in children {
                    let ct =
                        self.spawn(body, scope, *c, Some(slots.clone()), vals.clone(), Some(t));
                    self.threads[ct].inherited = inherited.clone();
                    self.active.push_back(ct);
                }
                self.threads[t].live_children += children.len();
                self.jump(t, &(*resume, vec![]));
                match join {
                    Join::None => Flow::Continue,
                    _ if children.is_empty() => Flow::Continue,
                    j => {
                        self.threads[t].join = Some(*j);
                        self.threads[t].state = State::Waiting;
                        Flow::Suspend
                    }
                }
            }
            Terminator::EndThread => {
                self.finish_thread(t);
                Flow::Suspend
            }
            Terminator::Disable { tag, resume } => {
                self.jump(t, &(*resume, vec![]));
                self.disable(*tag, t);
                if self.threads[t].state == State::Done {
                    Flow::Suspend
                } else {
                    Flow::Continue
                }
            }
            Terminator::Finish(k) => Flow::End(match k {
                FinishKind::Finish => End::Finish,
                FinishKind::Stop => End::Stop,
                FinishKind::Fatal => End::Fatal,
            }),
            Terminator::Unreachable => {
                self.finish_thread(t);
                Flow::Suspend
            }
        }
    }

    fn suspend(&mut self, t: ThreadId, wait: &Wait) {
        self.threads[t].state = State::Waiting;
        self.epochs[t] += 1;
        let epoch = self.epochs[t];
        match wait {
            Wait::Delay(v) => {
                let n = self.val(t, *v).bits().map_or(0, |b| b.to_u64());
                if n == 0 {
                    self.inactive.push_back((t, epoch));
                } else {
                    self.seq += 1;
                    self.future
                        .insert((self.time + n, self.seq), Wake::Thread(t, epoch));
                }
            }
            Wait::Inactive => self.inactive.push_back((t, epoch)),
            Wait::AnyChange(vars) => {
                for v in vars {
                    self.waiters[v.0 as usize].push(Waiter {
                        thread: t,
                        edge: Edge::Any,
                        epoch,
                    });
                }
            }
            Wait::Edge(es) => {
                for (v, e) in es {
                    self.waiters[v.0 as usize].push(Waiter {
                        thread: t,
                        edge: *e,
                        epoch,
                    });
                }
            }
            Wait::Event(v) => self.waiters[v.0 as usize].push(Waiter {
                thread: t,
                edge: Edge::Any,
                epoch,
            }),
            Wait::Process(v) => {
                let p = match self.val(t, *v) {
                    Value::Obj(Some(o)) => o.0.borrow().fields[0].bits().map(|b| b.to_u64() as ThreadId),
                    _ => None,
                };
                match p {
                    Some(p) if self.threads[p].state != State::Done => {
                        self.threads[p].awaiting.push((t, epoch));
                    }
                    // Already ended (or null): carry on.
                    _ => {
                        self.threads[t].state = State::Ready;
                        self.active.push_back(t);
                    }
                }
            }
            Wait::Children => {
                if self.threads[t].live_children == 0 {
                    self.threads[t].state = State::Ready;
                    self.active.push_back(t);
                } else {
                    self.threads[t].join = Some(Join::All);
                }
            }
        }
        if self.threads[t].state == State::Waiting
            && matches!(wait, Wait::Delay(_) | Wait::Inactive)
        {
            // Delayed threads are woken by the scheduler, not by waiters.
            self.threads[t].state = State::Waiting;
        }
    }

    /// `disable tag`, executed by thread `me`.
    fn disable(&mut self, tag: DisableTag, me: ThreadId) {
        for u in 0..self.threads.len() {
            if self.threads[u].state == State::Done {
                continue;
            }
            if self.threads[u].inherited.contains(&tag) {
                self.finish_thread(u);
                continue;
            }
            let Some(k) = self.threads[u].blocks.iter().position(|b| b.0 == tag) else {
                continue;
            };
            // Unwind to the block's exit, which leaves it.
            let (_, depth, exit) = self.threads[u].blocks[k];
            let th = &mut self.threads[u];
            th.blocks.truncate(k + 1);
            th.frames.truncate(depth);
            th.join = None;
            if let Some(f) = th.frames.last_mut() {
                f.block = exit;
                f.ip = 0;
            }
            if u != me && th.state == State::Waiting {
                self.epochs[u] += 1;
                self.threads[u].state = State::Ready;
                self.active.push_back(u);
            }
        }
    }

    /// Every thread started by `t`, and by those, still running.
    fn descendants(&self, t: ThreadId) -> Vec<ThreadId> {
        (0..self.threads.len())
            .filter(|&u| {
                let mut p = self.threads[u].parent;
                while let Some(x) = p {
                    if x == t {
                        return self.threads[u].state != State::Done;
                    }
                    p = self.threads[x].parent;
                }
                false
            })
            .collect()
    }

    fn finish_thread(&mut self, t: ThreadId) {
        self.threads[t].state = State::Done;
        self.threads[t].frames.clear();
        for (w, e) in std::mem::take(&mut self.threads[t].awaiting) {
            if self.epochs[w] == e && self.threads[w].state == State::Waiting {
                self.epochs[w] += 1;
                self.threads[w].state = State::Ready;
                self.active.push_back(w);
            }
        }
        if let Some(p) = self.threads[t].parent {
            let parent = &mut self.threads[p];
            parent.live_children = parent.live_children.saturating_sub(1);
            let wake = match parent.join {
                Some(Join::Any) => true,
                Some(Join::All) => parent.live_children == 0,
                _ => false,
            };
            if wake && parent.state == State::Waiting {
                parent.join = None;
                parent.state = State::Ready;
                self.active.push_back(p);
            }
        }
    }

    /// Delayed threads are resumed by the scheduler; mark them ready when popped.
    #[allow(dead_code)]
    fn ready(&mut self, t: ThreadId) {
        self.threads[t].state = State::Ready;
    }
}

/// Processes ordered so that one writing a variable comes before those
/// reading it, where that is possible; otherwise in their given order.
fn data_flow_order<'p, 'a>(procs: &[&'p Process<'a>]) -> Vec<&'p Process<'a>> {
    let vars = |p: &Process, write: bool| -> Vec<VarId> {
        let mut v = Vec::new();
        for b in &p.body.blocks {
            for i in &b.insts {
                match (&i.op, write) {
                    (
                        Op::Store { var, .. }
                        | Op::NbaStore { var, .. }
                        | Op::StoreElem { var, .. },
                        true,
                    )
                    | (Op::Load(var) | Op::LoadElem { var, .. }, false) => v.push(*var),
                    _ => {}
                }
            }
        }
        v
    };
    let writes: Vec<Vec<VarId>> = procs.iter().map(|p| vars(p, true)).collect();
    let reads: Vec<Vec<VarId>> = procs.iter().map(|p| vars(p, false)).collect();
    // Process i must wait for each j != i that writes something i reads.
    let n = procs.len();
    let mut before: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut writers: std::collections::HashMap<VarId, Vec<usize>> = Default::default();
    for (j, w) in writes.iter().enumerate() {
        for v in w {
            writers.entry(*v).or_default().push(j);
        }
    }
    for (i, r) in reads.iter().enumerate() {
        for v in r {
            for &j in writers.get(v).into_iter().flatten() {
                if j != i && !before[i].contains(&j) {
                    before[i].push(j);
                }
            }
        }
    }
    // Repeatedly take the first process whose writers have all been taken;
    // on a cycle, take the first remaining one.
    let mut done = vec![false; n];
    let mut order = Vec::with_capacity(n);
    while order.len() < n {
        let next = (0..n)
            .find(|&i| !done[i] && before[i].iter().all(|&j| done[j]))
            .or_else(|| (0..n).find(|&i| !done[i]))
            .unwrap();
        done[next] = true;
        order.push(procs[next]);
    }
    order
}

enum Flow {
    Continue,
    Suspend,
    End(End),
}


fn default(t: &Type<'_>) -> Value {
    crate::eval::default_for(&[], t)
}

fn merge(old: &Value, part: Option<(i64, u32)>, value: Value, ty: &Type<'_>) -> Value {
    crate::eval::convert_into(old, part, value, ty)
}

/// Did a change from `old` to `new` fire this edge? Edges look at bit 0.
fn edge_fired(edge: Edge, old: &Value, new: &Value) -> bool {
    if edge == Edge::Any {
        return old != new;
    }
    let (Value::Bits(o), Value::Bits(n)) = (old, new) else {
        return old != new;
    };
    let bit = |b: &Bits| match b.bit(0) {
        (false, false) => 0u8,
        (true, false) => 1,
        _ => 2, // X or Z
    };
    let (a, b) = (bit(o), bit(n));
    let pos = matches!((a, b), (0, 1) | (0, 2) | (2, 1));
    let neg = matches!((a, b), (1, 0) | (1, 2) | (2, 0));
    match edge {
        Edge::Pos => pos,
        Edge::Neg => neg,
        _ => pos || neg,
    }
}

#[cfg(test)]
mod tests;
