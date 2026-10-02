//! Linear intermediate representation. See `docs/design/20-architecture.md` §2.
//!
//! Elaboration lowers the syntax tree ([`crate::ast`]) straight into this IR.
//! The IR is linear:
//!
//! - Each process and function body is a control-flow graph of basic
//!   [`Block`]s. A block is a list of three-address [`Inst`]s ending in one
//!   [`Terminator`].
//! - Instructions compute SSA [`Val`]ues: each value is defined once, and
//!   every value has a type. Values flowing between blocks are passed as
//!   block parameters (no phi nodes).
//! - A process can suspend: a delay, an event control or `wait` is a
//!   [`Terminator::Suspend`] that names the block to resume at. Every resume
//!   point is therefore an explicit block, which both the interpreter and the
//!   code generator (a state machine over blocks) need.
//!
//! There are three kinds of storage:
//!
//! | Storage | Lives | Accessed by |
//! |---|---|---|
//! | [`VarId`]: design state (nets, static variables) | the whole simulation | `Load`/`Store`/`NbaStore`; the scheduler watches these |
//! | [`SlotId`]: a frame slot (automatic locals, loop counters) | one activation of a body | `LoadSlot`/`StoreSlot` |
//! | [`Val`]: an SSA temporary | one block, or passed on as a block parameter | operands |
//!
//! Locals start as slots so lowering stays simple; a later pass can promote
//! them to SSA values.
//!
//! The IEEE 1800 width and signedness rules (11.6–11.8) are applied during
//! lowering: every extension and truncation is an explicit `Resize`, and every
//! select is "offset and width". Back ends never repeat that logic.
//!
//! As in the AST, names and diagnostic sites are `&'a str` slices of source
//! text, so positions come from [`SourceMap::locate`](crate::source::SourceMap::locate).

mod print;

// ---------------------------------------------------------------- ids

macro_rules! id {
    ($($(#[$m:meta])* $name:ident),*) => {$(
        $(#[$m])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub u32);
    )*};
}

id!(
    /// Design state: a net or static variable in [`Design::vars`].
    VarId,
    /// A type in [`Design::types`].
    TypeId,
    /// An instance, generate block or named block in [`Design::scopes`].
    ScopeId,
    /// A function or task in [`Design::funcs`].
    FuncId,
    /// A block within a [`Body`].
    BlockId,
    /// An SSA value within a [`Body`].
    Val,
    /// A frame slot (automatic local) within a [`Body`].
    SlotId,
    /// A class (one specialisation of a parameterised class) in [`Design::classes`].
    ClassId,
    /// A display format in [`Design::formats`].
    FormatId
);

// ---------------------------------------------------------------- design

/// An elaborated, flattened design.
#[derive(Clone, Debug, Default)]
pub struct Design<'a> {
    pub scopes: Vec<Scope<'a>>,
    pub vars: Vec<Var<'a>>,
    pub types: Vec<Type<'a>>,
    pub procs: Vec<Process<'a>>,
    pub funcs: Vec<Func<'a>>,
    /// Parsed `$display`-style format strings.
    pub formats: Vec<Format<'a>>,
    pub top: Option<ScopeId>,
    /// Time precision as a power of ten of seconds (-9 is 1 ns).
    pub precision: i8,
    /// Input ports of the top modules, which a test bench may drive.
    pub top_inputs: Vec<VarId>,
    pub classes: Vec<Class<'a>>,
    /// The built-in `process` class, once used.
    pub process_class: Option<ClassId>,
    /// The name of the test bench the tops are instantiated in, if any: it
    /// prefixes hierarchical names (`%m` is `top.t` under Verilator's).
    pub root_name: Option<String>,
}

/// A class: the layout of its objects and its virtual methods.
#[derive(Clone, Debug)]
pub struct Class<'a> {
    pub name: &'a str,
    pub base: Option<ClassId>,
    /// Every property of an object, inherited ones first.
    pub fields: Vec<(&'a str, TypeId)>,
    /// The implementation of each virtual method slot.
    pub vtable: Vec<FuncId>,
}

/// An instance, generate block, named block or package. Scopes exist for
/// names (`%m`, tracing, hierarchical references); they do not own code.
#[derive(Clone, Debug)]
pub struct Scope<'a> {
    pub name: &'a str,
    pub parent: Option<ScopeId>,
    /// The module name, for an instance.
    pub module: Option<&'a str>,
    /// Time unit as a power of ten of seconds.
    pub unit: i8,
}

/// A net or static variable. Ports are not separate: a port connection makes
/// the port and the connected net the same `VarId`, or adds a continuous
/// assignment process between them.
#[derive(Clone, Debug)]
pub struct Var<'a> {
    pub name: &'a str,
    pub scope: ScopeId,
    pub ty: TypeId,
    pub kind: VarKind,
    /// Value at time 0, before any process runs.
    pub init: Option<Bits>,
    pub at: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VarKind {
    Variable,
    /// A net. When it has several drivers they are combined by the resolution.
    Net(NetResolution),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetResolution {
    Wire,
    WiredAnd,
    WiredOr,
    Tri0,
    Tri1,
    Supply0,
    Supply1,
}

// ---------------------------------------------------------------- types

/// A resolved type. Every packed type is [`Type::Bits`].
#[derive(Clone, Debug, PartialEq)]
pub enum Type<'a> {
    Bits {
        width: u32,
        signed: bool,
        four_state: bool,
        /// Member layout, for packed structs (for `%p` and tracing).
        fields: Option<Vec<Field<'a>>>,
        /// Value names, for enums (for `%p`, `.name()` and tracing).
        names: Option<Vec<(&'a str, Bits)>>,
    },
    Real,
    String,
    Event,
    /// `[left:right]` unpacked array.
    Unpacked {
        elem: TypeId,
        left: i64,
        right: i64,
    },
    Dynamic {
        elem: TypeId,
    },
    Queue {
        elem: TypeId,
        max: Option<u32>,
    },
    Assoc {
        elem: TypeId,
        key: Option<TypeId>,
    },
    Struct {
        fields: Vec<Field<'a>>,
    },
    /// A handle to an object of the class or one derived from it.
    Class(ClassId),
    /// The type of `null`.
    Null,
    /// The result of a subroutine with `output` or `inout` arguments: its
    /// value (if any) then the final value of each such argument, as an
    /// array value.
    Tuple(Vec<TypeId>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Field<'a> {
    pub name: &'a str,
    pub ty: TypeId,
    /// Bit offset of the least significant bit, for packed members.
    pub offset: u32,
}

/// A constant bit vector, least significant 64-bit word first.
///
/// 4-state values carry an `unknown` plane: where it is 0, `val` is the bit;
/// where it is 1, `val` 0 means X and 1 means Z.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bits {
    pub width: u32,
    pub val: Vec<u64>,
    pub unknown: Option<Vec<u64>>,
}

// ---------------------------------------------------------------- code

/// A process. Every process is a coroutine: its body loops forever (or runs
/// once, for `initial`/`final`), suspending at [`Terminator::Suspend`].
///
/// `always @(posedge clk) s` lowers to
/// `entry: suspend Edge(clk, Pos) -> body;  body: s; jump entry`.
/// `always_comb` and `assign` lower to
/// `entry: <compute>; suspend AnyChange(reads) -> entry`.
#[derive(Clone, Debug)]
pub struct Process<'a> {
    pub kind: ProcKind,
    pub scope: ScopeId,
    pub body: Body<'a>,
    pub at: &'a str,
}

/// What a process came from. Only a hint: the body alone defines behaviour,
/// but the scheduler can use the kind (and [`Body::reads`]) to run
/// combinational processes in dependency order instead of as coroutines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcKind {
    Initial,
    Final,
    Always,
    AlwaysComb,
    AlwaysLatch,
    AlwaysFf,
    ContAssign,
}

#[derive(Clone, Debug)]
pub struct Func<'a> {
    pub name: &'a str,
    pub scope: ScopeId,
    /// Arguments arrive as the entry block's parameters.
    pub params: Vec<TypeId>,
    pub ret: Option<TypeId>,
    pub is_task: bool,
    pub body: Body<'a>,
    pub at: &'a str,
}

/// A control-flow graph.
#[derive(Clone, Debug, Default)]
pub struct Body<'a> {
    /// `blocks[0]` is the entry.
    pub blocks: Vec<Block<'a>>,
    /// The type of each SSA value, indexed by [`Val`].
    pub vals: Vec<TypeId>,
    /// The type of each frame slot, indexed by [`SlotId`].
    pub slots: Vec<TypeId>,
    /// Design state this body reads, for sensitivity and scheduling.
    pub reads: Vec<VarId>,
    /// Design state this body writes.
    pub writes: Vec<VarId>,
}

#[derive(Clone, Debug)]
pub struct Block<'a> {
    /// Values passed in by the jumps that reach this block.
    pub params: Vec<Val>,
    pub insts: Vec<Inst<'a>>,
    pub term: Terminator,
}

/// One instruction. `dst` is the value it defines, if any.
#[derive(Clone, Debug)]
pub struct Inst<'a> {
    pub dst: Option<Val>,
    pub op: Op,
    /// Source position, for run-time diagnostics.
    pub at: &'a str,
}

#[derive(Clone, Debug)]
pub enum Op {
    // Constants and storage
    Const(Bits),
    ConstReal(f64),
    ConstStr(String),
    /// Read design state.
    Load(VarId),
    /// Write design state now (blocking), optionally only `width` bits from bit `lsb`.
    Store {
        var: VarId,
        part: Option<Part>,
        value: Val,
    },
    /// Schedule a write for the NBA region (`<=`).
    NbaStore {
        var: VarId,
        part: Option<Part>,
        value: Val,
    },
    /// Read or write an element of an unpacked or dynamic array in design state.
    LoadElem {
        var: VarId,
        index: Val,
    },
    StoreElem {
        var: VarId,
        index: Val,
        part: Option<Part>,
        value: Val,
        nba: bool,
    },
    /// `len` elements of an unpacked array from linear element `start`, as
    /// an array value. (`Load` of an array variable gives the whole array; a
    /// `Store` or `StoreElem` of an array value writes consecutive elements.)
    LoadRange {
        var: VarId,
        start: Val,
        len: u32,
    },
    LoadSlot(SlotId),
    /// Store into element `index` of a frame slot holding an unpacked array
    /// (consecutive elements from `index`, for an array value).
    StoreSlotElem {
        slot: SlotId,
        index: Val,
        part: Option<Part>,
        value: Val,
    },
    StoreSlot {
        slot: SlotId,
        part: Option<Part>,
        value: Val,
    },

    // Bit vectors
    Unary(UnOp, Val),
    Binary(BinOp, Val, Val),
    /// `width` bits of `value` from bit `lsb` (constant or computed).
    Select {
        value: Val,
        lsb: Val,
        width: u32,
    },
    /// Most significant first.
    Concat(Vec<Val>),
    Repl {
        value: Val,
        count: u32,
    },
    /// Change width or signedness; the result type is the instruction's type.
    Resize {
        value: Val,
        extend: Extend,
    },
    /// `cond ? a : b` without branching. With an X/Z condition the LRM
    /// merges the two values bit by bit.
    Mux {
        cond: Val,
        then: Val,
        els: Val,
    },
    /// Integral to real, real to integral, number to string.
    Convert(Val),

    // Calls and system services
    Call {
        func: FuncId,
        args: Vec<Val>,
    },
    /// Format and emit text (`$display`, `$write`, `$fdisplay`, ...).
    Display {
        kind: DisplayKind,
        format: FormatId,
        args: Vec<Val>,
    },
    /// A system function returning a value (`$time`, `$random`, `$clog2` at run time, ...).
    SysFunc {
        func: SysFunc,
        args: Vec<Val>,
    },
    /// `-> ev`
    TriggerEvent(VarId),
    /// `$error`, `$warning`, `$info`, or a failed assertion.
    Report {
        severity: ReportSeverity,
        format: Option<FormatId>,
        args: Vec<Val>,
    },

    // Strings. `Concat`, `Repl`, `Mux` and the comparisons also work on
    // strings when their operands or result are strings; `Convert` turns
    // integral values into strings and back (LRM 6.16).
    // Classes (LRM 8).
    /// A new object with every property at its default.
    New(ClassId),
    /// `new obj`: a shallow copy.
    CopyObj(Val),
    Null,
    LoadField { obj: Val, field: u32 },
    StoreField {
        obj: Val,
        field: u32,
        part: Option<Part>,
        value: Val,
    },
    /// Element `index` of an array property.
    StoreFieldElem {
        obj: Val,
        field: u32,
        index: Val,
        part: Option<Part>,
        value: Val,
    },
    /// A virtual method call: `args[0]` is the object, whose class's
    /// vtable gives the function for `slot`.
    VCall { slot: u32, args: Vec<Val> },
    /// `force` (LRM 10.6.2): reads of the variable (element, bits) see
    /// `value` until released. Gives a token for the force; with `token`,
    /// renews that force instead.
    Force {
        var: VarId,
        elem: Option<Val>,
        part: Option<Part>,
        value: Val,
        token: Option<Val>,
    },
    /// A `unique`/`priority` violation: an error, then a stop. Not
    /// reported while combinational logic first settles at time 0.
    Violation { format: FormatId },
    /// `release`.
    Release {
        var: VarId,
        elem: Option<Val>,
        part: Option<Part>,
    },
    /// Is the force with `token` still in effect on the variable?
    IsForcer {
        var: VarId,
        elem: Option<Val>,
        part: Option<Part>,
        token: Val,
    },
    /// The built-in `process` class (LRM 9.7).
    Process { func: ProcFunc, args: Vec<Val> },
    /// Is the handle not null and its object of `class` or derived from it?
    IsA { value: Val, class: ClassId },
    /// Entering a named block or task body that `disable` can name; `exit`
    /// is where a disabled execution continues (it starts with the matching
    /// `BlockLeave`).
    BlockEnter { tag: DisableTag, exit: BlockId },
    BlockLeave(DisableTag),
    /// `disable fork`: end every process this one started, and theirs.
    DisableFork,
    /// The values, in order, as an array value (a [`Type::Tuple`]).
    Tuple(Vec<Val>),
    /// Element `index` (linear) of an array value.
    ArrayElem { value: Val, index: Val },
    /// `len` elements of an array value from linear element `start`.
    ArraySlice { value: Val, start: Val, len: u32 },
    /// A built-in array method or array operation; pure.
    ArrFunc { func: ArrFunc, args: Vec<Val> },
    /// A built-in string method; pure.
    StrFunc {
        func: StrFunc,
        args: Vec<Val>,
    },
    /// `$sformatf`: a formatted string.
    Sformat {
        format: FormatId,
        args: Vec<Val>,
    },
}

/// Built-in operations on dynamic arrays and queues (LRM 7.5, 7.10) and
/// array methods (7.12). Element 0 of a dynamic array or queue value is its
/// leftmost (index 0); those that change the array return the new array.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArrFunc {
    Size,
    /// `(n, default, [old])`: `new[n]` or `new[n](old)`.
    New,
    /// `(a, x)`
    PushBack,
    PushFront,
    /// `(a)`: `a` without its last or first element.
    DropBack,
    DropFront,
    /// `(a)`: the last or first element (the default if empty).
    Last,
    First,
    /// `(a, i, x)`
    Insert,
    /// `(a, i)`
    DeleteAt,
    /// `(a)`: empty.
    Clear,
    /// `(a, lo, hi)`: `a[lo:hi]`.
    Slice,
    /// Reverses the order of elements: also converts between a fixed-size
    /// array's order (element 0 at the right-hand index) and a queue's.
    Reverse,
    Sort,
    Rsort,
    Sum,
    Product,
    And,
    Or,
    Xor,
    Min,
    Max,
    // Associative arrays (LRM 7.8). Keys are integral or strings.
    /// `()`: an empty associative array.
    MapNew,
    /// `(map, key)`: the element, or the element type's default.
    MapGet,
    /// `(map, key, value)`: the map with the element set.
    MapPut,
    /// `(map, key)`: 1 if the key is present.
    MapExists,
    /// `(map, key)`: the map without the key.
    MapDelete,
    /// `(map)`: the keys in order, as a queue.
    MapKeys,
    /// `(map, key, mode)`, mode 0 first, 1 last, 2 next, 3 prev: 1 if there
    /// is such a key.
    MapFindOk,
    /// The same, giving that key (or `key` if none).
    MapFindKey,
}

/// Methods of the built-in `process` class. A process handle is an object
/// of [`Design::process_class`] whose one property is the thread number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcFunc {
    /// `process::self()`
    SelfHandle,
    /// `p.status()`: FINISHED 0, RUNNING 1, WAITING 2, SUSPENDED 3, KILLED 4.
    Status,
    Kill,
    /// `srandom`, `set_randstate`: accepted, with no effect.
    Ignore,
    /// `get_randstate`: a string.
    GetRandstate,
}

/// What `disable` names: a named block (by its scope) or a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisableTag {
    Block(ScopeId),
    Task(FuncId),
}

/// The built-in string methods (LRM 6.16). Those that change the string
/// (`putc`, `itoa`, ...) return the new string, which elaboration stores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrFunc {
    Len,
    /// `s.getc(i)`, also `s[i]`.
    Getc,
    /// `(s, i, c)`: `s` with byte `i` replaced, if it is in range and `c` is not 0.
    Putc,
    ToUpper,
    ToLower,
    Compare,
    Icompare,
    /// `(s, i, j)`.
    Substr,
    Atoi,
    Atohex,
    Atooct,
    Atobin,
    Atoreal,
    /// `(value)`: the decimal text of an integer.
    Itoa,
    Hextoa,
    Octtoa,
    Bintoa,
    Realtoa,
    /// `(s, n)`: `{n{s}}` with a count known only at run time.
    Repeat,
}

/// A part of a value to write: `width` bits from bit `lsb`.
#[derive(Clone, Copy, Debug)]
pub struct Part {
    pub lsb: Val,
    pub width: u32,
}

#[derive(Clone, Debug)]
pub enum Terminator {
    Jump(BlockId, Vec<Val>),
    Branch {
        cond: Val,
        then: (BlockId, Vec<Val>),
        els: (BlockId, Vec<Val>),
    },
    /// Multi-way branch on exact values (lowered `case`). `casez`, `casex`
    /// and `inside` are lowered to compares and branches instead.
    Switch {
        value: Val,
        cases: Vec<(Bits, BlockId)>,
        default: BlockId,
    },
    Return(Option<Val>),
    /// Stop running until `wait` is satisfied, then continue at `resume`.
    Suspend {
        wait: Wait,
        resume: BlockId,
    },
    /// Start child threads (`fork`), each a body sharing this frame's slots,
    /// and continue at `resume` when the join condition is met.
    Fork {
        children: Vec<BlockId>,
        join: Join,
        resume: BlockId,
    },
    /// End of a forked child thread.
    EndThread,
    /// `disable` (LRM 9.6.2): processes started inside `tag` end; ones
    /// executing inside it continue after it. If this one is not inside
    /// it, it continues at `resume`.
    Disable { tag: DisableTag, resume: BlockId },
    /// `$finish`, `$stop` or `$fatal`: end the simulation.
    Finish(FinishKind),
    Unreachable,
}

/// What a suspended process waits for.
#[derive(Clone, Debug)]
pub enum Wait {
    /// `#delay`, in the design's precision.
    Delay(Val),
    /// The next time-step region with no delay (`#0`).
    Inactive,
    /// A change on any of these. `@(a or b)`, `@*`, `always_comb`, and
    /// `wait (cond)` loops (which re-check `cond` after the change).
    AnyChange(Vec<VarId>),
    /// An edge on a single-bit value: `@(posedge clk)`. An event control on an
    /// arbitrary expression is lowered to `AnyChange` on what it reads plus a
    /// compare loop.
    Edge(Vec<(VarId, Edge)>),
    /// `@ev` on a named event.
    Event(VarId),
    /// `wait fork`.
    Children,
    /// `p.await()`: until the process `p` (a `process` handle) has ended.
    Process(Val),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    /// Any change of value (`@(a)`).
    Any,
    Pos,
    Neg,
    Both,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Join {
    All,
    Any,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Extend {
    Zero,
    Sign,
    Truncate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
    LogNot,
    RedAnd,
    RedNand,
    RedOr,
    RedNor,
    RedXor,
    RedXnor,
}

/// Signedness comes from the operand types, so there is one `Lt`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    And,
    Or,
    Xor,
    Xnor,
    Shl,
    Shr,
    AShr,
    Eq,
    Ne,
    CaseEq,
    CaseNe,
    WildEq,
    WildNe,
    /// `casez` item match: Z or `?` bits in either operand match anything.
    CaseZEq,
    /// `casex` item match: X, Z or `?` bits in either operand match anything.
    CaseXEq,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayKind {
    /// `$display`: ends with a newline.
    Display,
    /// `$write`: no newline.
    Write,
    /// `$strobe`: at the end of the time step.
    Strobe,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SysFunc {
    /// `$clog2` of a value known only at run time.
    Clog2,
    Time,
    Stime,
    Realtime,
    Random,
    Urandom,
    UrandomRange,
    Countones,
    Onehot,
    Onehot0,
    IsUnknown,
    TestPlusargs,
    ValuePlusargs,
    RealToBits,
    BitsToReal,
    /// `(lo0, hi0, lo1, hi1, ...)`: a random value in one of the ranges
    /// (64-bit bounds, signed), each range as likely as its size; for
    /// randomization.
    RandomPick,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinishKind {
    Finish,
    Stop,
    Fatal,
}

/// A parsed format string: literal text and `%` conversions.
#[derive(Clone, Debug)]
pub struct Format<'a> {
    pub pieces: Vec<FormatPiece<'a>>,
}

#[derive(Clone, Debug)]
pub enum FormatPiece<'a> {
    Text(&'a str),
    /// `%d`, `%0h`, `%5b`, `%s`, `%t`, `%m`, ... consuming the next argument (except `%m`).
    Conv {
        spec: char,
        width: Option<u32>,
        zero_pad: bool,
        /// `%-5d`: left-justified.
        left: bool,
    },
}
