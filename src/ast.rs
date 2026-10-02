//! Syntax tree.
//!
//! The tree is light. Every name, literal, operator and keyword is a `&'a str`
//! slice of source text, as tokens are, and nothing is decoded. A node's
//! position is the position of one of its slices (see
//! [`SourceMap::locate`](crate::source::SourceMap::locate)), so nodes carry
//! no spans.

/// A parsed source file.
#[derive(Clone, Debug, Default)]
pub struct SourceText<'a> {
    pub items: Vec<Item<'a>>,
}

/// Something at file (`$unit`) scope.
#[derive(Clone, Debug)]
pub enum Item<'a> {
    /// `module`, `macromodule`, `interface` or `program`.
    Module(Module<'a>),
    Package(Package<'a>),
    /// A declaration at `$unit` scope: typedef, function, parameter, variable, import.
    Decl(ModuleItem<'a>),
    Directive(&'a str),
}

#[derive(Clone, Debug)]
pub struct Module<'a> {
    /// `module`, `macromodule`, `interface` or `program`.
    pub kind: &'a str,
    pub lifetime: Option<&'a str>,
    pub name: &'a str,
    pub imports: Vec<Import<'a>>,
    /// The `#( ... )` parameter port list, if any.
    pub params: Option<Vec<ParamDecl<'a>>>,
    pub ports: Ports<'a>,
    pub items: Vec<ModuleItem<'a>>,
    /// The `endmodule` (or similar) keyword.
    pub end: &'a str,
}

#[derive(Clone, Debug)]
pub struct Package<'a> {
    pub name: &'a str,
    pub items: Vec<ModuleItem<'a>>,
    pub end: &'a str,
}

/// `import pkg::name;` or `import pkg::*;`
#[derive(Clone, Debug)]
pub struct Import<'a> {
    pub package: &'a str,
    /// The imported name, or `*`.
    pub name: &'a str,
}

#[derive(Clone, Debug)]
pub enum Ports<'a> {
    /// No port list, or `()`.
    None,
    /// `module m(a, b);` with directions declared in the body.
    NonAnsi(Vec<PortRef<'a>>),
    /// `module m(input a, output [7:0] b);`
    Ansi(Vec<AnsiPort<'a>>),
    /// `module m(.*);`
    Wildcard,
}

/// One entry in a non-ANSI port list: `a`, `a[3:0]`, `{a, b}` or `.name(expr)`.
#[derive(Clone, Debug)]
pub struct PortRef<'a> {
    /// The external name, when given as `.name(...)`.
    pub name: Option<&'a str>,
    pub expr: Option<Expr<'a>>,
}

#[derive(Clone, Debug)]
pub struct AnsiPort<'a> {
    /// `input`, `output`, `inout` or `ref`. Inherited from the previous port if omitted.
    pub dir: Option<&'a str>,
    /// `wire`, `var`, or another net type.
    pub kind: Option<&'a str>,
    pub ty: DataType<'a>,
    /// For an interface port: the interface and optional modport.
    pub interface: Option<(&'a str, Option<&'a str>)>,
    pub name: &'a str,
    pub dims: Vec<Dim<'a>>,
    pub default: Option<Expr<'a>>,
}

#[derive(Clone, Debug)]
pub enum DataType<'a> {
    /// No type keyword, only optional signing and packed dimensions: `[7:0]`, `signed`.
    Implicit {
        signing: Option<&'a str>,
        packed: Vec<Dim<'a>>,
    },
    /// A built-in type keyword: `logic`, `bit`, `reg`, `int`, `integer`, `real`, `string`, ...
    Builtin {
        kw: &'a str,
        signing: Option<&'a str>,
        packed: Vec<Dim<'a>>,
    },
    /// A type from an interface port or instance: `ifc.data_t`, `ifc[0].sub.data_t`.
    IfaceType {
        iface: Box<Expr<'a>>,
        name: &'a str,
        packed: Vec<Dim<'a>>,
    },
    /// A member type of a (parameterised) class: `C#(8)::t`.
    ClassMember {
        class: Box<DataType<'a>>,
        name: &'a str,
        packed: Vec<Dim<'a>>,
    },
    /// A typedef, class or package-scoped name: `my_t`, `pkg::my_t`.
    Named {
        scope: Option<&'a str>,
        name: &'a str,
        params: Option<Vec<ParamArg<'a>>>,
        packed: Vec<Dim<'a>>,
    },
    Enum {
        kw: &'a str,
        base: Option<Box<DataType<'a>>>,
        items: Vec<EnumItem<'a>>,
        packed: Vec<Dim<'a>>,
    },
    /// `struct` or `union`.
    Struct {
        kw: &'a str,
        packed: bool,
        signing: Option<&'a str>,
        members: Vec<VarDecl<'a>>,
        dims: Vec<Dim<'a>>,
    },
    /// `type(expr)`.
    TypeOf(Box<Expr<'a>>),
}

#[derive(Clone, Debug)]
pub struct EnumItem<'a> {
    pub name: &'a str,
    /// `A[3]` or `A[1:4]`: a range of names.
    pub range: Option<(Expr<'a>, Option<Expr<'a>>)>,
    pub value: Option<Expr<'a>>,
}

/// A packed or unpacked dimension.
#[derive(Clone, Debug)]
pub enum Dim<'a> {
    /// `[msb:lsb]`
    Range(Expr<'a>, Expr<'a>),
    /// `[n]`, meaning `[0:n-1]` for an unpacked dimension.
    Size(Expr<'a>),
    /// `[]`: dynamic array.
    Dynamic,
    /// `[$]` or `[$:max]`: queue.
    Queue(Option<Expr<'a>>),
    /// `[*]` (`None`) or `[type]`: associative array.
    Assoc(Option<Box<DataType<'a>>>),
}

/// A net or variable declaration: `wire [7:0] a = b, c;`, `logic x;`.
#[derive(Clone, Debug)]
pub struct VarDecl<'a> {
    /// `const`, `var`, `static`, `automatic`, `rand`, `randc`.
    pub qualifiers: Vec<&'a str>,
    /// A net type such as `wire` or `tri`, for nets.
    pub net: Option<&'a str>,
    pub ty: DataType<'a>,
    pub vars: Vec<Declarator<'a>>,
}

#[derive(Clone, Debug)]
pub struct Declarator<'a> {
    pub name: &'a str,
    pub dims: Vec<Dim<'a>>,
    pub init: Option<Expr<'a>>,
}

/// A port declaration in a module body: `input [7:0] a, b;`
#[derive(Clone, Debug)]
pub struct PortDecl<'a> {
    pub dir: &'a str,
    pub decl: VarDecl<'a>,
}

#[derive(Clone, Debug)]
pub struct ParamDecl<'a> {
    /// `parameter` or `localparam`; `None` inside `#( ... )` when omitted.
    pub kw: Option<&'a str>,
    /// `None` for `parameter type T = ...`.
    pub ty: Option<DataType<'a>>,
    pub assigns: Vec<ParamAssign<'a>>,
}

#[derive(Clone, Debug)]
pub struct ParamAssign<'a> {
    pub name: &'a str,
    pub dims: Vec<Dim<'a>>,
    /// The default. For a type parameter this is an [`Expr::Type`].
    pub value: Option<Expr<'a>>,
}

/// A parameter override in an instantiation or a parameterised type.
#[derive(Clone, Debug)]
pub enum ParamArg<'a> {
    Ordered(Expr<'a>),
    Named(&'a str, Option<Expr<'a>>),
}

#[derive(Clone, Debug)]
pub struct Typedef<'a> {
    pub ty: DataType<'a>,
    pub name: &'a str,
    pub dims: Vec<Dim<'a>>,
}

/// `class C #(...) extends B #(...) (args); ... endclass` (LRM 8).
#[derive(Clone, Debug)]
pub struct ClassDecl<'a> {
    pub kw: &'a str,
    /// `virtual class` (abstract) or `interface class`.
    pub kind: Option<&'a str>,
    pub name: &'a str,
    pub params: Option<Vec<ParamDecl<'a>>>,
    /// The base class type, and arguments passed to its constructor.
    pub extends: Option<(DataType<'a>, Vec<Arg<'a>>)>,
    pub implements: Vec<DataType<'a>>,
    pub items: Vec<ClassItem<'a>>,
    pub end: &'a str,
}

/// A concurrent assertion (LRM 16.14).
#[derive(Clone, Debug)]
pub struct Assertion<'a> {
    /// `assert`, `assume`, `cover` or `restrict`.
    pub kw: &'a str,
    pub label: Option<&'a str>,
    pub spec: PropSpec<'a>,
    pub pass: Option<Box<Stmt<'a>>>,
    pub fail: Option<Box<Stmt<'a>>>,
}

/// `[@(clock)] [disable iff (cond)] property`
#[derive(Clone, Debug)]
pub struct PropSpec<'a> {
    pub clock: Option<Timing<'a>>,
    pub disable: Option<Expr<'a>>,
    pub prop: Prop<'a>,
}

/// A property expression (LRM 16.12), the commonly used part.
#[derive(Clone, Debug)]
pub enum Prop<'a> {
    Seq(Seq<'a>),
    /// `ante |-> cons` (`overlap`) or `ante |=> cons`.
    Implies {
        ante: Seq<'a>,
        overlap: bool,
        cons: Box<Prop<'a>>,
    },
    Not(Box<Prop<'a>>),
    And(Box<Prop<'a>>, Box<Prop<'a>>),
    Or(Box<Prop<'a>>, Box<Prop<'a>>),
    If {
        cond: Expr<'a>,
        then: Box<Prop<'a>>,
        els: Option<Box<Prop<'a>>>,
    },
    /// `s_eventually`, `until`, `nexttime` and the like: by keyword.
    Unsupported(&'a str),
}

/// A sequence expression (LRM 16.7).
#[derive(Clone, Debug)]
pub enum Seq<'a> {
    Expr(Expr<'a>),
    /// `lhs ##[min:max] rhs`; no `lhs` for a leading delay. `max` is `None`
    /// for a single delay, `Some(None)` for `$`.
    Delay {
        lhs: Option<Box<Seq<'a>>>,
        min: Expr<'a>,
        max: Option<Option<Expr<'a>>>,
        rhs: Box<Seq<'a>>,
    },
    /// `s[*n]`, `s[*m:n]`, `e[->n]`, `e[=n]`.
    Repeat {
        seq: Box<Seq<'a>>,
        kind: &'a str,
        min: Expr<'a>,
        max: Option<Option<Expr<'a>>>,
    },
    /// `and`, `or`, `intersect`, `throughout`, `within`.
    Binary {
        op: &'a str,
        lhs: Box<Seq<'a>>,
        rhs: Box<Seq<'a>>,
    },
}

/// An item of a constraint block (LRM 18.5).
#[derive(Clone, Debug)]
pub enum ConstraintItem<'a> {
    /// A Boolean expression, including `a -> b` and `x inside {...}`.
    Expr(Expr<'a>),
    /// `soft expr;`
    Soft(Expr<'a>),
    /// `cond -> { items }`
    Implies(Expr<'a>, Vec<ConstraintItem<'a>>),
    If {
        cond: Expr<'a>,
        then: Vec<ConstraintItem<'a>>,
        els: Vec<ConstraintItem<'a>>,
    },
    Foreach {
        array: Expr<'a>,
        vars: Vec<Option<&'a str>>,
        items: Vec<ConstraintItem<'a>>,
    },
    /// `x dist { v := w, [lo:hi] :/ w }`: the values (or `Range`s), with weights.
    Dist {
        expr: Expr<'a>,
        items: Vec<(Expr<'a>, Option<Expr<'a>>)>,
    },
    /// `unique { a, b, c }`
    Unique(Vec<Expr<'a>>),
    /// `solve ... before ...;` and `disable soft ...;`: no effect here.
    Ignored(&'a str),
}

/// A member of a class with its qualifiers (`static`, `local`, `protected`,
/// `rand`, `randc`, `virtual`, `pure`, `extern`, `const`).
#[derive(Clone, Debug)]
pub struct ClassItem<'a> {
    pub quals: Vec<&'a str>,
    pub item: ClassMember<'a>,
}

#[derive(Clone, Debug)]
pub enum ClassMember<'a> {
    /// A property, method, typedef, parameter or nested class.
    Item(Box<ModuleItem<'a>>),
    /// `constraint name { ... }`; `None` items for a prototype.
    Constraint(&'a str, Option<Vec<ConstraintItem<'a>>>),
    /// A covergroup, kept by name only.
    Covergroup(&'a str),
}

#[derive(Clone, Debug)]
pub enum ModuleItem<'a> {
    Port(PortDecl<'a>),
    Var(VarDecl<'a>),
    Param(ParamDecl<'a>),
    Typedef(Typedef<'a>),
    /// A forward typedef: `typedef name;` or `typedef enum name;`.
    ForwardTypedef(&'a str),
    Class(ClassDecl<'a>),
    /// `[label:] assert|assume|cover property (...) [pass] [else fail];`
    Assertion(Assertion<'a>),
    /// `property name [(args)]; ... endproperty`
    PropertyDecl {
        name: &'a str,
        ports: Vec<&'a str>,
        spec: PropSpec<'a>,
    },
    /// `sequence name [(args)]; ... endsequence`
    SequenceDecl {
        name: &'a str,
        ports: Vec<&'a str>,
        seq: Seq<'a>,
    },
    /// `default clocking [name] @(...); endclocking`
    DefaultClocking(Timing<'a>),
    /// `default disable iff (expr);`
    DefaultDisable(Expr<'a>),
    Import(Vec<Import<'a>>),
    /// `assign [#delay] lhs = rhs, ...;`
    Assign {
        kw: &'a str,
        delay: Option<Expr<'a>>,
        assigns: Vec<(Expr<'a>, Expr<'a>)>,
    },
    /// `always`, `always_comb`, `always_ff`, `always_latch`, `initial` or `final`.
    Process {
        kw: &'a str,
        stmt: Stmt<'a>,
    },
    Instance(Instance<'a>),
    Gate(Gate<'a>),
    /// `generate ... endgenerate`.
    Generate(Vec<ModuleItem<'a>>),
    GenFor {
        kw: &'a str,
        var: &'a str,
        init: Expr<'a>,
        cond: Expr<'a>,
        step: Box<Stmt<'a>>,
        body: Box<GenBlock<'a>>,
    },
    GenIf {
        kw: &'a str,
        cond: Expr<'a>,
        then: Box<GenBlock<'a>>,
        els: Option<Box<GenBlock<'a>>>,
    },
    GenCase {
        kw: &'a str,
        expr: Expr<'a>,
        items: Vec<(Vec<Expr<'a>>, GenBlock<'a>)>,
    },
    /// A bare `begin ... end` in module scope.
    GenBlock(GenBlock<'a>),
    Genvar(Vec<&'a str>),
    Function(Subroutine<'a>),
    Task(Subroutine<'a>),
    Defparam(Vec<(Expr<'a>, Expr<'a>)>),
    /// `timeunit` / `timeprecision`.
    TimeUnits {
        kw: &'a str,
        values: Vec<&'a str>,
    },
    Modport(Vec<Modport<'a>>),
    Module(Module<'a>),
    /// A compiler directive passed through by the preprocessor.
    Directive(&'a str),
    /// An elaboration-time `$info`, `$warning`, `$error` or `$fatal`.
    ElabTask(Expr<'a>),
}

#[derive(Clone, Debug)]
pub struct GenBlock<'a> {
    pub label: Option<&'a str>,
    pub items: Vec<ModuleItem<'a>>,
}

#[derive(Clone, Debug)]
pub struct Modport<'a> {
    pub name: &'a str,
    /// Each entry is a direction (`input`, `output`, `inout`, `ref`, `import`,
    /// `export`) and the names it applies to.
    pub ports: Vec<(&'a str, Vec<&'a str>)>,
}

/// A module, interface or primitive instantiation.
#[derive(Clone, Debug)]
pub struct Instance<'a> {
    pub module: &'a str,
    pub params: Option<Vec<ParamArg<'a>>>,
    pub insts: Vec<Inst<'a>>,
}

#[derive(Clone, Debug)]
pub struct Inst<'a> {
    pub name: &'a str,
    pub dims: Vec<Dim<'a>>,
    pub conns: Vec<PortConn<'a>>,
}

#[derive(Clone, Debug)]
pub enum PortConn<'a> {
    /// A positional connection; `None` leaves the port unconnected.
    Ordered(Option<Expr<'a>>),
    /// `.name(expr)`, `.name()` (`Some(None)`) or `.name` (`None`).
    Named(&'a str, Option<Option<Expr<'a>>>),
    /// `.*`
    Wildcard,
}

/// A gate primitive instantiation: `and g1 (y, a, b), g2 (z, c, d);`
#[derive(Clone, Debug)]
pub struct Gate<'a> {
    pub kind: &'a str,
    pub delay: Option<Expr<'a>>,
    pub insts: Vec<(Option<&'a str>, Vec<Expr<'a>>)>,
}

/// A function or task.
#[derive(Clone, Debug)]
pub struct Subroutine<'a> {
    pub kw: &'a str,
    /// `C` for an out-of-class definition `function C::f`.
    pub class: Option<&'a str>,
    /// A prototype only (`extern`, `pure virtual`): no body.
    pub proto: bool,
    pub lifetime: Option<&'a str>,
    /// The return type of a function; `None` for a task or an implicit type.
    pub ret: Option<DataType<'a>>,
    pub name: &'a str,
    /// Ports declared in the header, `None` if there was no `( ... )`.
    pub ports: Option<Vec<TfPort<'a>>>,
    /// Declarations in the body, including old-style port declarations.
    pub decls: Vec<ModuleItem<'a>>,
    pub stmts: Vec<Stmt<'a>>,
    pub end: &'a str,
}

#[derive(Clone, Debug)]
pub struct TfPort<'a> {
    pub dir: Option<&'a str>,
    pub ty: DataType<'a>,
    pub name: &'a str,
    pub dims: Vec<Dim<'a>>,
    pub default: Option<Expr<'a>>,
}

#[derive(Clone, Debug)]
pub enum Stmt<'a> {
    /// `;`
    Null(&'a str),
    /// `begin ... end` or `fork ... join[_any|_none]`.
    Block {
        kw: &'a str,
        label: Option<&'a str>,
        decls: Vec<ModuleItem<'a>>,
        stmts: Vec<Stmt<'a>>,
        /// `end`, `join`, `join_any` or `join_none`.
        end: &'a str,
    },
    /// `lhs op [timing] rhs;` where `op` is `=`, `<=` or a compound operator.
    Assign {
        lhs: Expr<'a>,
        op: &'a str,
        timing: Option<Timing<'a>>,
        rhs: Expr<'a>,
    },
    /// A call, increment or other expression used as a statement.
    Expr(Expr<'a>),
    /// A declaration among the statements (for example in a `for` header).
    Decl(VarDecl<'a>),
    If {
        unique: Option<&'a str>,
        kw: &'a str,
        cond: Expr<'a>,
        then: Box<Stmt<'a>>,
        els: Option<Box<Stmt<'a>>>,
    },
    Case {
        unique: Option<&'a str>,
        /// `case`, `casez` or `casex`.
        kw: &'a str,
        expr: Expr<'a>,
        inside: bool,
        items: Vec<CaseItem<'a>>,
    },
    For {
        kw: &'a str,
        init: Vec<Stmt<'a>>,
        cond: Option<Expr<'a>>,
        step: Vec<Stmt<'a>>,
        body: Box<Stmt<'a>>,
    },
    Foreach {
        kw: &'a str,
        array: Expr<'a>,
        vars: Vec<Option<&'a str>>,
        body: Box<Stmt<'a>>,
    },
    While {
        kw: &'a str,
        cond: Expr<'a>,
        body: Box<Stmt<'a>>,
    },
    DoWhile {
        kw: &'a str,
        body: Box<Stmt<'a>>,
        cond: Expr<'a>,
    },
    Repeat {
        kw: &'a str,
        count: Expr<'a>,
        body: Box<Stmt<'a>>,
    },
    Forever {
        kw: &'a str,
        body: Box<Stmt<'a>>,
    },
    /// A statement under a delay or event control: `#5 x = 1;`, `@(posedge clk) ...`.
    Timed {
        timing: Timing<'a>,
        stmt: Box<Stmt<'a>>,
    },
    /// `wait (cond) stmt`, or `wait fork;` (`cond` is `None`).
    Wait {
        kw: &'a str,
        cond: Option<Expr<'a>>,
        stmt: Box<Stmt<'a>>,
    },
    /// `-> ev;` or `->> ev;`
    Trigger {
        op: &'a str,
        target: Expr<'a>,
    },
    /// `disable name;` or `disable fork;` (`target` is `None`).
    Disable {
        kw: &'a str,
        target: Option<Expr<'a>>,
    },
    Return {
        kw: &'a str,
        value: Option<Expr<'a>>,
    },
    Break(&'a str),
    Continue(&'a str),
    /// `assign`/`deassign`/`force`/`release` in a procedure.
    ProcAssign {
        kw: &'a str,
        lhs: Expr<'a>,
        rhs: Option<Expr<'a>>,
    },
    /// An immediate assertion: `assert (cond) pass; else fail;`.
    Assert {
        kw: &'a str,
        cond: Expr<'a>,
        pass: Option<Box<Stmt<'a>>>,
        fail: Option<Box<Stmt<'a>>>,
    },
    /// `label: stmt`.
    Labeled {
        label: &'a str,
        stmt: Box<Stmt<'a>>,
    },
}

#[derive(Clone, Debug)]
pub struct CaseItem<'a> {
    /// Empty for `default`.
    pub labels: Vec<Expr<'a>>,
    pub stmt: Stmt<'a>,
}

#[derive(Clone, Debug)]
pub enum Timing<'a> {
    /// `#expr`
    Delay(Expr<'a>),
    /// `@(...)`, `@name`; `None` for `@*` and `@(*)`.
    Event(Option<Vec<EventExpr<'a>>>),
    /// `##n`
    Cycle(Expr<'a>),
    /// `repeat (n) @(...)` in an intra-assignment control.
    Repeat(Expr<'a>, Box<Timing<'a>>),
}

#[derive(Clone, Debug)]
pub struct EventExpr<'a> {
    /// `posedge`, `negedge` or `edge`.
    pub edge: Option<&'a str>,
    pub expr: Expr<'a>,
    pub iff: Option<Expr<'a>>,
}

/// An argument to a call.
#[derive(Clone, Debug)]
pub enum Arg<'a> {
    /// A positional argument; `None` when empty (`f(a, , b)`).
    Ordered(Option<Expr<'a>>),
    Named(&'a str, Option<Expr<'a>>),
}

/// An entry in an assignment pattern `'{ ... }`.
#[derive(Clone, Debug)]
pub enum PatItem<'a> {
    Value(Expr<'a>),
    /// `key: value`, where the key may be a name, an index, a type or `default`.
    Keyed(Expr<'a>, Expr<'a>),
    /// `n{a, b}`: replication.
    Repeat(Expr<'a>, Vec<Expr<'a>>),
}

#[derive(Clone, Debug)]
pub enum Expr<'a> {
    /// A literal: `12`, `8'hFF`, `'1`, `1.5`, `10ns`.
    Number(&'a str),
    Str(&'a str),
    Ident(&'a str),
    /// `null`, `this`, `super` or `$`.
    Keyword(&'a str),
    /// `pkg::name`, `$unit::name`, `class::name`, `std::process::self`.
    Scoped {
        scope: Box<Expr<'a>>,
        name: &'a str,
    },
    Member {
        base: Box<Expr<'a>>,
        name: &'a str,
    },
    Index {
        base: Box<Expr<'a>>,
        index: Box<Expr<'a>>,
    },
    /// `base[left op right]` where `op` is `:`, `+:` or `-:`.
    Slice {
        base: Box<Expr<'a>>,
        op: &'a str,
        left: Box<Expr<'a>>,
        right: Box<Expr<'a>>,
    },
    Unary {
        op: &'a str,
        arg: Box<Expr<'a>>,
    },
    /// `++x`, `x++`, `--x`, `x--`.
    IncDec {
        op: &'a str,
        prefix: bool,
        arg: Box<Expr<'a>>,
    },
    Binary {
        op: &'a str,
        lhs: Box<Expr<'a>>,
        rhs: Box<Expr<'a>>,
    },
    Cond {
        op: &'a str,
        cond: Box<Expr<'a>>,
        then: Box<Expr<'a>>,
        els: Box<Expr<'a>>,
    },
    /// `expr inside { ... }`
    Inside {
        expr: Box<Expr<'a>>,
        set: Vec<Expr<'a>>,
    },
    /// `[lo:hi]` inside `inside` or a case item.
    Range {
        lo: Box<Expr<'a>>,
        hi: Box<Expr<'a>>,
    },
    /// `(a : b : c)`
    MinTypMax(Box<[Expr<'a>; 3]>),
    Concat(Vec<Expr<'a>>),
    /// `{n{a, b}}`
    Repl {
        count: Box<Expr<'a>>,
        items: Vec<Expr<'a>>,
    },
    /// `{<< 8 {a, b}}`
    Stream {
        op: &'a str,
        slice: Option<Box<Expr<'a>>>,
        items: Vec<Expr<'a>>,
    },
    /// `'{ ... }`, optionally typed: `t'{ ... }`.
    Pattern {
        ty: Option<Box<Expr<'a>>>,
        items: Vec<PatItem<'a>>,
    },
    Call {
        func: Box<Expr<'a>>,
        args: Vec<Arg<'a>>,
    },
    /// `$name(args)`; `args` is empty when there are no parentheses.
    SysCall {
        name: &'a str,
        args: Vec<Arg<'a>>,
    },
    /// `arr.find with (item > 2)`
    With {
        base: Box<Expr<'a>>,
        expr: Box<Expr<'a>>,
    },
    /// `obj.randomize() with { ... }`: a call with inline constraints.
    WithConstraints {
        call: Box<Expr<'a>>,
        items: Vec<ConstraintItem<'a>>,
    },
    /// `type'(expr)`, `8'(expr)`, `signed'(expr)`.
    Cast {
        ty: Box<Expr<'a>>,
        expr: Box<Expr<'a>>,
    },
    /// A data type where an expression is allowed: `$bits(logic [3:0])`, `#(.T(int))`.
    Type(Box<DataType<'a>>),
    /// `(a = b)` used as an expression.
    Assign {
        lhs: Box<Expr<'a>>,
        op: &'a str,
        rhs: Box<Expr<'a>>,
    },
    /// `new`, `new(args)` or `new[size]`.
    New {
        kw: &'a str,
        args: Vec<Arg<'a>>,
        size: Option<Box<Expr<'a>>>,
        /// `new obj`: a shallow copy of `obj`.
        copy: Option<Box<Expr<'a>>>,
    },
}

impl<'a> Expr<'a> {
    /// A slice of source text inside this expression, for its position.
    pub fn at(&self) -> &'a str {
        match self {
            Expr::Number(s) | Expr::Str(s) | Expr::Ident(s) | Expr::Keyword(s) => s,
            Expr::Scoped { scope, .. } => scope.at(),
            Expr::Member { base, .. } | Expr::Index { base, .. } | Expr::Slice { base, .. } => {
                base.at()
            }
            Expr::With { base, .. } => base.at(),
            Expr::WithConstraints { call, .. } => call.at(),
            Expr::Unary { op, .. } | Expr::IncDec { op, .. } => op,
            Expr::Binary { lhs, .. } | Expr::Assign { lhs, .. } => lhs.at(),
            Expr::Cond { cond, .. } => cond.at(),
            Expr::Inside { expr, .. } => expr.at(),
            Expr::Range { lo, .. } => lo.at(),
            Expr::MinTypMax(v) => v[0].at(),
            Expr::Concat(v) => v.first().map_or("", |e| e.at()),
            Expr::Repl { count, .. } => count.at(),
            Expr::Stream { op, .. } => op,
            Expr::Pattern { items, .. } => items.first().map_or("", |i| match i {
                PatItem::Value(e) | PatItem::Keyed(e, _) | PatItem::Repeat(e, _) => e.at(),
            }),
            Expr::Call { func, .. } => func.at(),
            Expr::SysCall { name, .. } => name,
            Expr::Cast { ty, .. } => ty.at(),
            Expr::Type(_) => "",
            Expr::New { kw, .. } => kw,
        }
    }
}
