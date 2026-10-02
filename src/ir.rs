//! Elaborated intermediate representation. See `docs/design/20-architecture.md`.
//!
//! Elaboration lowers the syntax tree ([`crate::ast`]) into this IR. Where the
//! AST records what was written, the IR records what it means:
//!
//! - **Resolved.** Names are IDs ([`VarId`], [`ScopeId`], [`FuncId`]);
//!   parameters, generate blocks and the hierarchy are evaluated.
//! - **Typed.** Every expression has a [`TypeId`]. The IEEE 1800 width and
//!   signedness rules (11.6–11.8) are applied once, during elaboration, and
//!   every extension or truncation is an explicit node. Back ends then never
//!   need context-determined width logic.
//! - **Normalised.** Fewer forms: `for` and `foreach` become `while`, a select
//!   is always "offset and width", packed structs are bit vectors, and a
//!   continuous assignment is a process.
//!
//! The IR is shared by the reference interpreter and the code generator, so
//! the two can be checked against each other.
//!
//! As in the AST, names and diagnostic sites are `&'a str` slices of source
//! text, so positions still come from [`SourceMap::locate`](crate::source::SourceMap::locate).
//! Everything else lives in index-addressed tables on [`Design`].

// ---------------------------------------------------------------- ids

macro_rules! id {
    ($($(#[$m:meta])* $name:ident),*) => {$(
        $(#[$m])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub u32);
    )*};
}

id!(
    /// A variable, net or parameter in [`Design::vars`].
    VarId,
    /// A type in [`Design::types`].
    TypeId,
    /// An instance or named block in [`Design::scopes`].
    ScopeId,
    /// A process in [`Design::procs`].
    ProcId,
    /// A function or task in [`Design::funcs`].
    FuncId
);

// ---------------------------------------------------------------- design

/// An elaborated design: the whole instance tree under one top module.
#[derive(Clone, Debug, Default)]
pub struct Design<'a> {
    pub scopes: Vec<Scope<'a>>,
    pub vars: Vec<Var<'a>>,
    pub types: Vec<Type<'a>>,
    pub procs: Vec<Process<'a>>,
    pub funcs: Vec<Func<'a>>,
    pub top: Option<ScopeId>,
    /// Time precision as a power of ten of seconds (-9 is 1 ns).
    pub precision: i8,
}

/// An instance, generate block or named block. Scopes give hierarchical
/// names (for `%m`, tracing and hierarchical references) and own variables.
#[derive(Clone, Debug)]
pub struct Scope<'a> {
    /// The instance or block name, or `""` for an unnamed block.
    pub name: &'a str,
    pub parent: Option<ScopeId>,
    pub kind: ScopeKind<'a>,
    /// Time unit of the scope, as a power of ten of seconds.
    pub unit: i8,
}

#[derive(Clone, Debug)]
pub enum ScopeKind<'a> {
    /// An instance of this module (the module name).
    Instance(&'a str),
    /// A generate block; `index` is set for a block inside a generate loop.
    Generate {
        index: Option<i64>,
    },
    /// A named `begin`/`fork` block.
    Block,
    Package,
}

/// Storage: a variable, net, or elaborated parameter.
#[derive(Clone, Debug)]
pub struct Var<'a> {
    pub name: &'a str,
    pub scope: ScopeId,
    pub ty: TypeId,
    pub kind: VarKind,
    /// Initial value, for declarations with `= value`.
    pub init: Option<Expr>,
    /// Where it was declared.
    pub at: &'a str,
}

#[derive(Clone, Debug)]
pub enum VarKind {
    /// A variable: `logic`, `int`, `reg`, a class handle, ...
    Variable { lifetime: Lifetime },
    /// A net. Multiple drivers are combined by `resolution`.
    Net { resolution: NetResolution },
    /// A parameter or localparam, already evaluated.
    Param(Value),
    /// A module port. It aliases the variable or net connected to it.
    Port { dir: PortDir },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lifetime {
    Static,
    Automatic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetResolution {
    /// `wire`, `tri`, `uwire`.
    Wire,
    WiredAnd,
    WiredOr,
    Tri0,
    Tri1,
    Supply0,
    Supply1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortDir {
    Input,
    Output,
    Inout,
    Ref,
}

// ---------------------------------------------------------------- types

/// A resolved type. Packed types are always flattened to [`Type::Bits`].
#[derive(Clone, Debug, PartialEq)]
pub enum Type<'a> {
    /// Any packed integral type: `logic [7:0]`, `int`, `bit`, a packed struct
    /// or enum. Bit 0 is the least significant bit.
    Bits {
        width: u32,
        signed: bool,
        /// 4-state (`logic`, `reg`, `integer`) or 2-state (`bit`, `int`).
        four_state: bool,
        /// Member layout, if the type came from a packed struct or union.
        fields: Option<Vec<Field<'a>>>,
        /// Value names, if the type came from an enum.
        names: Option<Vec<(&'a str, Value)>>,
    },
    Real,
    ShortReal,
    String,
    Event,
    Chandle,
    /// `elem name [left:right]`.
    Unpacked {
        elem: TypeId,
        left: i64,
        right: i64,
    },
    /// `elem name []`.
    Dynamic {
        elem: TypeId,
    },
    /// `elem name [$]` or `[$:max]`.
    Queue {
        elem: TypeId,
        max: Option<u32>,
    },
    /// `elem name [key]`; `key` is `None` for `[*]`.
    Assoc {
        elem: TypeId,
        key: Option<TypeId>,
    },
    /// An unpacked struct.
    Struct {
        fields: Vec<Field<'a>>,
    },
    Void,
}

/// A struct or union member. For packed types `offset` is the bit offset of
/// the member's least significant bit.
#[derive(Clone, Debug, PartialEq)]
pub struct Field<'a> {
    pub name: &'a str,
    pub ty: TypeId,
    pub offset: u32,
}

// ---------------------------------------------------------------- values

/// A constant value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Bits(Bits),
    Real(f64),
    Str(String),
}

/// A bit vector of any width, stored as 64-bit words, least significant first.
///
/// 4-state bits use a second plane: where an `unknown` bit is 0, `val` holds
/// 0 or 1; where it is 1, `val` 0 means X and `val` 1 means Z. A 2-state
/// value has no `unknown` plane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bits {
    pub width: u32,
    pub signed: bool,
    pub val: Vec<u64>,
    pub unknown: Option<Vec<u64>>,
}

// ---------------------------------------------------------------- expressions

/// A typed expression. `ty` is the type of the result after the LRM width
/// rules have been applied.
#[derive(Clone, Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub ty: TypeId,
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    Const(Value),
    Var(VarId),
    /// A bit or part select of a packed value: `width` bits from bit `lsb`.
    /// Every form (`a[i]`, `a[7:4]`, `a[i+:4]`, `a[i-:4]`, a packed struct
    /// member) is lowered to this, with any declared range offset applied.
    Select {
        base: Box<Expr>,
        lsb: Box<Expr>,
        width: u32,
    },
    /// An element of an unpacked, dynamic, queue or associative array.
    Index {
        base: Box<Expr>,
        index: Box<Expr>,
    },
    /// A member of an unpacked struct, by field position.
    Field {
        base: Box<Expr>,
        field: u32,
    },
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// `cond ? then : els`. With an X/Z condition the LRM merges both results.
    Cond {
        cond: Box<Expr>,
        then: Box<Expr>,
        els: Box<Expr>,
    },
    /// Most significant first, as written.
    Concat(Vec<Expr>),
    Repl {
        count: u32,
        expr: Box<Expr>,
    },
    /// Change width or signedness. Inserted by elaboration wherever the LRM
    /// implicitly extends or truncates.
    Resize {
        expr: Box<Expr>,
        to: TypeId,
        extend: Extend,
    },
    /// A conversion that is not a resize: integral to real, real to integral,
    /// or to string.
    Convert {
        expr: Box<Expr>,
        to: TypeId,
    },
    Inside {
        expr: Box<Expr>,
        set: Vec<InsideItem>,
    },
    Call {
        func: FuncId,
        args: Vec<Expr>,
    },
    SysFunc {
        func: SysFunc,
        args: Vec<Expr>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Extend {
    Zero,
    Sign,
    /// Narrowing: drop the high bits.
    Truncate,
}

#[derive(Clone, Debug)]
pub enum InsideItem {
    Value(Expr),
    Range(Expr, Expr),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    /// `-`
    Neg,
    /// `~`
    Not,
    /// `!`
    LogNot,
    RedAnd,
    RedNand,
    RedOr,
    RedNor,
    RedXor,
    RedXnor,
}

/// Binary operators. Signedness comes from the operand types, so there is
/// one `Lt`, not a signed and an unsigned one.
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
    /// `>>`: logical shift right.
    Shr,
    /// `>>>`: arithmetic when the left operand is signed.
    AShr,
    Eq,
    Ne,
    /// `===`
    CaseEq,
    /// `!==`
    CaseNe,
    /// `==?`
    WildEq,
    /// `!=?`
    WildNe,
    Lt,
    Le,
    Gt,
    Ge,
    LogAnd,
    LogOr,
    /// `->`
    LogImplies,
    /// `<->`
    LogEquiv,
}

/// System functions that return a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SysFunc {
    Time,
    Stime,
    Realtime,
    Random,
    Urandom,
    UrandomRange,
    Signed,
    Unsigned,
    Clog2,
    Countones,
    Onehot,
    Onehot0,
    IsUnknown,
    Sformatf,
    TestPlusargs,
    ValuePlusargs,
    Itor,
    Rtoi,
    RealToBits,
    BitsToReal,
    Fopen,
    Feof,
    Fgetc,
}

// ---------------------------------------------------------------- statements

/// Something that can be assigned to.
#[derive(Clone, Debug)]
pub enum LValue {
    Var(VarId),
    Select {
        base: Box<LValue>,
        lsb: Expr,
        width: u32,
    },
    Index {
        base: Box<LValue>,
        index: Expr,
    },
    Field {
        base: Box<LValue>,
        field: u32,
    },
    /// `{a, b} = ...`, most significant first.
    Concat(Vec<LValue>),
}

#[derive(Clone, Debug)]
pub enum Stmt<'a> {
    Block(Vec<Stmt<'a>>),
    /// `lhs = rhs` (blocking) or `lhs <= rhs` (non-blocking). Compound
    /// assignments (`+=`) are lowered to plain ones.
    Assign {
        lhs: LValue,
        rhs: Expr,
        kind: AssignKind,
        delay: Option<Delay>,
    },
    If {
        cond: Expr,
        then: Box<Stmt<'a>>,
        els: Option<Box<Stmt<'a>>>,
        check: CaseCheck,
    },
    Case {
        kind: CaseKind,
        expr: Expr,
        items: Vec<(Vec<CaseLabel>, Stmt<'a>)>,
        default: Option<Box<Stmt<'a>>>,
        check: CaseCheck,
    },
    /// All loops are `while`, `do while`, `repeat` or `forever`; `for` and
    /// `foreach` become a block with a `while`.
    While {
        cond: Expr,
        body: Box<Stmt<'a>>,
        test_first: bool,
    },
    Repeat {
        count: Expr,
        body: Box<Stmt<'a>>,
    },
    Forever(Box<Stmt<'a>>),
    Break,
    Continue,
    Return(Option<Expr>),
    /// `#delay`.
    Delay(Delay),
    /// `@(...)`: suspend until one of the triggers fires.
    WaitEvent(Vec<Trigger>),
    /// `wait (cond)`.
    Wait(Expr),
    /// `wait fork`.
    WaitFork,
    Fork {
        join: Join,
        branches: Vec<Stmt<'a>>,
    },
    /// `-> ev`.
    TriggerEvent(VarId),
    /// `disable name`.
    Disable(ScopeId),
    DisableFork,
    Call {
        func: FuncId,
        args: Vec<Expr>,
    },
    SysTask {
        task: SysTask,
        args: Vec<Expr>,
        at: &'a str,
    },
    /// An immediate assertion. `pass` and `fail` are the action blocks.
    Assert {
        cond: Expr,
        pass: Option<Box<Stmt<'a>>>,
        fail: Option<Box<Stmt<'a>>>,
        at: &'a str,
    },
    Force {
        lhs: LValue,
        rhs: Expr,
    },
    Release(LValue),
    Nop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignKind {
    Blocking,
    NonBlocking,
}

/// A delay, already scaled to the design's time precision.
#[derive(Clone, Debug)]
pub enum Delay {
    Const(u64),
    Expr(Expr),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseKind {
    /// `case`: 4-state equality.
    Exact,
    /// `casez`: Z and `?` are wildcards.
    Z,
    /// `casex`: X, Z and `?` are wildcards.
    X,
    /// `case ... inside`.
    Inside,
}

#[derive(Clone, Debug)]
pub enum CaseLabel {
    Value(Expr),
    Range(Expr, Expr),
}

/// `unique`, `unique0` and `priority` checks on `if` and `case`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseCheck {
    None,
    Unique,
    Unique0,
    Priority,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Join {
    All,
    Any,
    None,
}

/// One entry in an event control.
#[derive(Clone, Debug)]
pub struct Trigger {
    pub edge: Edge,
    pub expr: Expr,
    pub iff: Option<Expr>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    /// Any change.
    Any,
    Pos,
    Neg,
    /// `edge`: either edge.
    Both,
}

/// System tasks: called for their effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SysTask {
    Display,
    Write,
    Strobe,
    Monitor,
    Fdisplay,
    Fwrite,
    Finish,
    Stop,
    Fatal,
    Error,
    Warning,
    Info,
    Readmemh,
    Readmemb,
    Writememh,
    Fclose,
    Dumpfile,
    Dumpvars,
}

// ---------------------------------------------------------------- processes

/// A process: something the scheduler runs. Continuous assignments are
/// processes too, so the scheduler sees one kind of thing.
#[derive(Clone, Debug)]
pub struct Process<'a> {
    pub kind: ProcKind,
    pub scope: ScopeId,
    pub body: Stmt<'a>,
    /// When the process wakes. For `always_comb`, `@*` and continuous
    /// assignments, elaboration computes this from what the body reads.
    pub sensitivity: Sensitivity,
    pub at: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcKind {
    Initial,
    Final,
    Always,
    AlwaysComb,
    AlwaysLatch,
    AlwaysFf,
    /// `assign lhs = rhs;`, or a net declaration's initialiser.
    ContAssign,
}

#[derive(Clone, Debug)]
pub enum Sensitivity {
    /// Runs once (`initial`, `final`), or its body contains its own timing
    /// controls (`always begin @(...) ... end`).
    None,
    /// An explicit event control at the top of the process.
    Events(Vec<Trigger>),
    /// Implicit: wake when any of these variables changes.
    Reads(Vec<VarId>),
}

/// A function or task, elaborated once per scope it is declared in.
#[derive(Clone, Debug)]
pub struct Func<'a> {
    pub name: &'a str,
    pub scope: ScopeId,
    /// `None` for a task or a void function.
    pub ret: Option<TypeId>,
    /// Arguments, in order. Each is a variable in the function's scope.
    pub args: Vec<(VarId, PortDir)>,
    pub body: Stmt<'a>,
    pub lifetime: Lifetime,
    pub is_task: bool,
    pub at: &'a str,
}
