//! Elaboration: lower the syntax tree into the linear IR.
//! See `docs/design/20-architecture.md`.
//!
//! Elaboration walks the instance tree from the top modules. Each module
//! instance is processed in two passes over its items:
//!
//! 1. **Declare.** Parameters (with overrides), types, ports, variables,
//!    function signatures. After this the parent can connect the ports.
//! 2. **Build.** Function bodies, processes, continuous assignments, child
//!    instances and generate constructs, lowered into IR bodies.
//!
//! Generate blocks get their own scopes and run both passes inside.
//! Constant expressions (parameters, dimensions, generate conditions) are
//! lowered into a small body and run by [`crate::eval::eval_const`].

mod build;
mod class;
mod expr;
mod stmt;
#[cfg(test)]
mod tests;
mod types;

use crate::ast;
use crate::diag::{Diag, NOT_YET, Severity};
use crate::eval::Value;
use crate::ir::*;
use crate::source::SourceMap;
use std::collections::{BTreeSet, HashMap, HashSet};
pub(crate) use types::{Base, Ty};

/// Elaboration options.
#[derive(Clone, Debug, Default)]
pub struct ElabOptions {
    /// The top module (`--top-module`). By default every module that no other
    /// module instantiates is a top.
    pub top: Option<String>,
    /// See [`Design::root_name`].
    pub root_name: Option<String>,
    /// `-Gname=value`: overrides for the top modules' parameters. A value is
    /// a number or a quoted string.
    pub params: Vec<(String, String)>,
    /// The instance name of the top module (`--l2-name`); by default its module name.
    pub top_instance: Option<String>,
    /// Indices of the files that are libraries (`-v`, `-y`): their modules
    /// are used where instantiated but are never tops.
    pub library_files: Vec<usize>,
}

/// Elaborate parsed files into a design.
pub fn elaborate<'a>(
    sm: &'a SourceMap,
    files: &[ast::SourceText<'a>],
    opts: &ElabOptions,
) -> (Design<'a>, Vec<Diag<'a>>) {
    let made = Arena::default();
    let made_subs = Arena::default();
    let mut e = Elab::new(sm, &made, &made_subs);
    e.d.root_name = opts.root_name.clone();
    e.run(files, opts);
    (e.d, e.diags)
}

/// Modules a file defines, and module names it instantiates.
pub fn module_refs<'a>(t: &ast::SourceText<'a>) -> (Vec<&'a str>, Vec<&'a str>) {
    let mut defined = Vec::new();
    let mut used = HashSet::new();
    for item in &t.items {
        if let ast::Item::Module(m) = item {
            defined.push(m.name);
            collect_instances(&m.items, &mut used);
        }
    }
    let mut used: Vec<&'a str> = used.into_iter().collect();
    used.sort();
    (defined, used)
}

/// The error has been reported; abandon the current construct.
#[derive(Debug)]
pub(crate) struct Stop;

pub(crate) type EResult<T> = Result<T, Stop>;

/// What a name means in a scope.
#[derive(Clone, Debug)]
pub(crate) enum Sym<'a> {
    Var(VarId, Ty<'a>),
    Slot(SlotId, Ty<'a>),
    Param(Value, Ty<'a>),
    Type(Ty<'a>),
    Func(FuncId),
    /// A child instance or named generate block.
    Scope(ScopeId),
    /// A declared genvar outside its loop.
    Genvar,
    /// A class declaration (an index into `Elab::class_defs`); it becomes a
    /// type once specialised.
    ClassDef(usize),
    /// A property of the class whose method is being lowered: its index in
    /// the object, and its type.
    Field(u32, Ty<'a>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dir {
    Input,
    Output,
    Inout,
    /// An interface port: the name stands for the connected interface
    /// instance (its `var` is not used).
    Interface,
}

#[derive(Clone, Debug)]
pub(crate) struct PortInfo<'a> {
    name: &'a str,
    dir: Dir,
    var: VarId,
    ty: Ty<'a>,
}

/// A function's signature, known after pass 1.
#[derive(Clone, Debug)]
pub(crate) struct Sig<'a> {
    pub params: Vec<(&'a str, Ty<'a>)>,
    /// The direction of each parameter.
    pub dirs: Vec<Dir>,
    /// Default values of the parameters, where given.
    pub defaults: Vec<Option<ast::Expr<'a>>>,
    pub ret: Option<Ty<'a>>,
}

/// Elaboration-side information about a scope (parallel to `Design::scopes`).
struct ScopeInfo<'a, 't> {
    syms: HashMap<&'a str, Sym<'a>>,
    /// Where name lookup continues: the enclosing block or module, then `$unit`.
    lex_parent: Option<ScopeId>,
    /// Packages imported with `import p::*`.
    imports: Vec<ScopeId>,
    /// Ports in header order (instances only).
    ports: Vec<PortInfo<'a>>,
    /// Port directions declared in the body of a non-ANSI module.
    dirs: HashMap<&'a str, Dir>,
    /// Counter for unnamed generate blocks (`genblk1`, ...).
    genblk: u32,
    /// The module items, for pass 2 (instances).
    items: &'t [ast::ModuleItem<'a>],
    /// Functions declared here, waiting for pass 2.
    funcs: Vec<(FuncId, &'t ast::Subroutine<'a>)>,
    /// Net and variable initialisers that need code, waiting for pass 2.
    inits: Vec<(VarId, Ty<'a>, &'t ast::Expr<'a>, bool)>,
    /// For a class scope: the base class's scope, searched before the
    /// enclosing one (inherited members).
    base_scope: Option<ScopeId>,
}

/// An implicit sensitivity list to complete once all functions are lowered.
struct Fixup {
    proc: usize,
    block: BlockId,
    reads: BTreeSet<VarId>,
    calls: BTreeSet<FuncId>,
    resume: BlockId,
}

/// Syntax made during elaboration, such as the connections of each instance
/// of an instance array, kept as long as the parsed files.
pub(crate) struct Arena<T>(std::cell::RefCell<Vec<Box<T>>>);

impl<T> Default for Arena<T> {
    fn default() -> Self {
        Arena(std::cell::RefCell::new(Vec::new()))
    }
}

impl<T> Arena<T> {
    pub(crate) fn alloc(&self, t: T) -> &T {
        let b = Box::new(t);
        let p: *const T = &*b;
        self.0.borrow_mut().push(b);
        // SAFETY: boxes are never removed or moved out of, and a box's
        // contents stay put when the vector grows, so the value lives as
        // long as the arena.
        unsafe { &*p }
    }
}

pub(crate) struct Elab<'a, 't> {
    pub(crate) sm: &'a SourceMap,
    /// Where made syntax lives.
    made: &'t Arena<Vec<ast::PortConn<'a>>>,
    made_subs: &'t Arena<ast::Subroutine<'a>>,
    /// Class declarations, and the classes made from them.
    pub(crate) class_defs: Vec<class::ClassDef<'a, 't>>,
    pub(crate) classes: Vec<class::ClassInfo<'a, 't>>,
    /// Methods: their class, and whether static.
    pub(crate) func_class: HashMap<FuncId, (ClassId, bool)>,
    /// Out-of-class method bodies (`function C::f`), by class and name.
    pub(crate) out_of_class: HashMap<(&'a str, &'a str), &'t ast::Subroutine<'a>>,
    /// Interface instances made early, because a declaration used a type
    /// from them; phase A skips them.
    early_insts: HashSet<usize>,
    pub(crate) d: Design<'a>,
    pub(crate) diags: Vec<Diag<'a>>,
    modules: HashMap<&'a str, (&'t ast::Module<'a>, (i8, i8))>,
    packages: HashMap<&'a str, ScopeId>,
    scopes: Vec<ScopeInfo<'a, 't>>,
    unit_scope: ScopeId,
    /// The scope names are looked up in.
    pub(crate) cur: ScopeId,
    pub(crate) bits_cache: HashMap<(u32, bool, bool), TypeId>,
    pub(crate) sigs: HashMap<FuncId, Sig<'a>>,
    /// Function bodies not yet lowered.
    func_defs: HashMap<FuncId, &'t ast::Subroutine<'a>>,
    /// Every function's definition, for working out signatures.
    func_src: HashMap<FuncId, &'t ast::Subroutine<'a>>,
    /// Parameters and typedefs that couldn't be resolved yet.
    deferred: Vec<Early<'a, 't>>,
    func_calls: HashMap<FuncId, BTreeSet<FuncId>>,
    fixups: Vec<Fixup>,
    /// Processes that set initial values, run before all others.
    init_procs: Vec<Process<'a>>,
    /// The most recent source position, for errors without a better one.
    pub(crate) last_at: &'a str,
    /// Set when constant evaluation meets a variable.
    pub(crate) nonconst: Option<&'a str>,
    depth: usize,
}

impl<'a, 't> Elab<'a, 't> {
    fn new(
        sm: &'a SourceMap,
        made: &'t Arena<Vec<ast::PortConn<'a>>>,
        made_subs: &'t Arena<ast::Subroutine<'a>>,
    ) -> Self {
        let mut e = Elab {
            sm,
            made,
            made_subs,
            class_defs: Vec::new(),
            classes: Vec::new(),
            func_class: HashMap::new(),
            out_of_class: HashMap::new(),
            early_insts: HashSet::new(),
            d: Design::default(),
            diags: Vec::new(),
            modules: HashMap::new(),
            packages: HashMap::new(),
            scopes: Vec::new(),
            unit_scope: ScopeId(0),
            cur: ScopeId(0),
            bits_cache: HashMap::new(),
            sigs: HashMap::new(),
            func_defs: HashMap::new(),
            func_src: HashMap::new(),
            deferred: Vec::new(),
            func_calls: HashMap::new(),
            fixups: Vec::new(),
            init_procs: Vec::new(),
            last_at: "",
            nonconst: None,
            depth: 0,
        };
        e.d.precision = -12;
        e.unit_scope = e.new_scope("$unit", None, None, None, -12, &[]);
        e
    }

    // ------------------------------------------------------------ diagnostics

    pub(crate) fn error(&mut self, at: &'a str, msg: impl Into<String>) -> Stop {
        self.diags.push(Diag::error(at, msg));
        Stop
    }

    pub(crate) fn not_yet(&mut self, at: &'a str, what: &str) -> Stop {
        self.diags
            .push(Diag::error(at, format!("Not yet supported: {what}")).with_code(NOT_YET));
        Stop
    }

    pub(crate) fn warn(&mut self, at: &'a str, code: &'static str, msg: impl Into<String>) {
        self.diags.push(Diag {
            severity: Severity::Warning,
            code: Some(code),
            at,
            message: msg.into(),
            notes: Vec::new(),
        });
    }

    pub(crate) fn here(&self) -> &'a str {
        self.last_at
    }

    // ------------------------------------------------------------ scopes and symbols

    fn new_scope(
        &mut self,
        name: &'a str,
        parent: Option<ScopeId>,
        module: Option<&'a str>,
        lex_parent: Option<ScopeId>,
        unit: i8,
        items: &'t [ast::ModuleItem<'a>],
    ) -> ScopeId {
        self.d.scopes.push(Scope {
            name,
            parent,
            module,
            unit,
        });
        self.scopes.push(ScopeInfo {
            syms: HashMap::new(),
            lex_parent,
            imports: Vec::new(),
            ports: Vec::new(),
            dirs: HashMap::new(),
            genblk: 0,
            items,
            funcs: Vec::new(),
            inits: Vec::new(),
            base_scope: None,
        });
        ScopeId(self.d.scopes.len() as u32 - 1)
    }

    fn info(&mut self, s: ScopeId) -> &mut ScopeInfo<'a, 't> {
        &mut self.scopes[s.0 as usize]
    }

    pub(crate) fn declare(&mut self, name: &'a str, sym: Sym<'a>) {
        let cur = self.cur;
        self.info(cur).syms.insert(name, sym);
    }

    pub(crate) fn declare_enum_value(&mut self, name: &'a str, value: Bits, ty: &Ty<'a>) {
        self.declare(name, Sym::Param(Value::Bits(value), ty.clone()));
    }

    /// A name in one scope, including its wildcard imports.
    fn lookup_in(&self, s: ScopeId, name: &str) -> Option<Sym<'a>> {
        let info = &self.scopes[s.0 as usize];
        if let Some(sym) = info.syms.get(name) {
            return Some(sym.clone());
        }
        info.imports
            .iter()
            .find_map(|p| self.scopes[p.0 as usize].syms.get(name).cloned())
    }

    /// Look a name up lexically from the current scope out to `$unit`. In a
    /// class, its base classes come before the enclosing scope.
    pub(crate) fn lookup(&self, name: &str) -> Option<Sym<'a>> {
        let mut s = Some(self.cur);
        while let Some(id) = s {
            if let Some(sym) = self.lookup_with_bases(id, name) {
                return Some(sym);
            }
            s = self.scopes[id.0 as usize].lex_parent;
        }
        None
    }

    /// A name in a scope or, for a class scope, its base classes.
    pub(crate) fn lookup_with_bases(&self, id: ScopeId, name: &str) -> Option<Sym<'a>> {
        let mut s = Some(id);
        while let Some(x) = s {
            if let Some(sym) = self.lookup_in(x, name) {
                return Some(sym);
            }
            s = self.scopes[x.0 as usize].base_scope;
        }
        None
    }

    /// `pkg::name`.
    pub(crate) fn lookup_scoped(&self, pkg: &str, name: &str) -> Option<Sym<'a>> {
        let p = *self.packages.get(pkg)?;
        self.scopes[p.0 as usize].syms.get(name).cloned()
    }

    /// The first part of an upward hierarchical name: an enclosing instance, or a top.
    pub(crate) fn lookup_upward(&self, name: &str) -> Option<ScopeId> {
        let mut s = Some(self.cur);
        while let Some(id) = s {
            if self.d.scopes[id.0 as usize].name == name
                && self.d.scopes[id.0 as usize].module.is_some()
            {
                return Some(id);
            }
            s = self.d.scopes[id.0 as usize].parent;
        }
        (0..self.d.scopes.len())
            .map(|i| ScopeId(i as u32))
            .find(|&id| {
                self.d.scopes[id.0 as usize].parent.is_none()
                    && self.d.scopes[id.0 as usize].name == name
            })
    }

    /// A name inside another scope, for hierarchical references.
    pub(crate) fn lookup_child(&self, s: ScopeId, name: &str) -> Option<Sym<'a>> {
        self.scopes[s.0 as usize].syms.get(name).cloned()
    }

    pub(crate) fn lookup_type(
        &mut self,
        scope: Option<&'a str>,
        name: &'a str,
    ) -> EResult<Option<Ty<'a>>> {
        let sym = match scope {
            Some(p) => self.lookup_scoped(p, name),
            None => self.lookup(name),
        };
        Ok(match sym {
            Some(Sym::Type(t)) => Some(t),
            // A class without parameter values: its default specialisation.
            Some(Sym::ClassDef(d)) => Some(class::ClassInfo::ty(self.specialise(d, None, name)?)),
            _ => None,
        })
    }

    // ------------------------------------------------------------ top level

    fn run(&mut self, files: &'t [ast::SourceText<'a>], opts: &ElabOptions) {
        // Timescales: a `timescale applies to the modules after it.
        let default = (-12i8, -12i8);
        let mut ts = default;
        for f in files {
            for item in &f.items {
                match item {
                    ast::Item::Directive(d) if d.starts_with("`timescale") => {
                        match parse_timescale(d) {
                            Some(t) => ts = t,
                            None => {
                                self.error(d, "`timescale syntax error");
                            }
                        }
                    }
                    ast::Item::Directive(d) if d.starts_with("`resetall") => ts = default,
                    ast::Item::Module(m) => {
                        let mut t = ts;
                        for it in &m.items {
                            if let ast::ModuleItem::TimeUnits { kw, values } = it {
                                apply_timeunit(&mut t, kw, values);
                            }
                        }
                        self.d.precision = self.d.precision.min(t.1);
                        if self.modules.insert(m.name, (m, t)).is_some() {
                            self.warn(
                                m.name,
                                "MODDUP",
                                format!("Duplicate declaration of module: '{}'", m.name),
                            );
                        }
                    }
                    _ => {}
                }
            }
        }

        // Packages, then `$unit` declarations.
        for f in files {
            for item in &f.items {
                if let ast::Item::Package(p) = item {
                    let s = self.new_scope(
                        p.name,
                        None,
                        None,
                        Some(self.unit_scope),
                        default.0,
                        &p.items,
                    );
                    self.packages.insert(p.name, s);
                    let saved = std::mem::replace(&mut self.cur, s);
                    let _ = self.declare_items(&p.items);
                    self.cur = saved;
                }
            }
        }
        let unit_items: Vec<&'t ast::ModuleItem<'a>> = files
            .iter()
            .flat_map(|f| &f.items)
            .filter_map(|i| match i {
                ast::Item::Decl(d) => Some(d),
                _ => None,
            })
            .collect();
        self.cur = self.unit_scope;
        self.predeclare_funcs(unit_items.iter().copied());
        for item in &unit_items {
            let _ = self.declare_item(item);
        }

        // Tops: modules nobody instantiates, outside library files.
        let mut library = HashSet::new();
        let mut order = HashMap::new();
        for (fi, f) in files.iter().enumerate() {
            for (ii, item) in f.items.iter().enumerate() {
                if let ast::Item::Module(m) = item {
                    order.entry(m.name).or_insert((fi, ii));
                    if opts.library_files.contains(&fi) {
                        library.insert(m.name);
                    }
                }
            }
        }
        let mut used = HashSet::new();
        for (m, _) in self.modules.values() {
            collect_instances(&m.items, &mut used);
        }
        let mut tops: Vec<&'t ast::Module<'a>> = match &opts.top {
            Some(t) => match self.modules.get(t.as_str()) {
                Some((m, _)) => vec![*m],
                None => {
                    let at = files
                        .first()
                        .and_then(|f| f.items.first())
                        .map_or("", item_at);
                    self.error(
                        at,
                        format!("Specified --top-module '{t}' was not found in design."),
                    );
                    return;
                }
            },
            None => self
                .modules
                .values()
                .map(|(m, _)| *m)
                .filter(|m| {
                    !used.contains(m.name)
                        && m.kind != "interface"
                        && !library.contains(m.name)
                })
                .collect(),
        };
        // In source order: file, then position in the file.
        tops.sort_by_key(|m| order.get(m.name).copied().unwrap_or((usize::MAX, 0)));
        let mut overrides = Vec::new();
        for (name, value) in &opts.params {
            let name = self
                .sm
                .add(name.clone(), crate::source::Origin::CommandLine)
                .1;
            match cmdline_override(value) {
                Ok(o) => overrides.push((Some(name), o)),
                Err(m) => {
                    self.error(name, m);
                }
            }
        }
        // Phase A: the whole scope tree, with ports connected and generates expanded.
        for m in tops {
            // Only the parameters a top declares are overridden.
            let ov: Vec<_> = overrides
                .iter()
                .filter(|(n, _)| module_declares_param(m, n.unwrap()))
                .cloned()
                .collect();
            let inst = match &opts.top_instance {
                Some(n) => self.sm.add(n.clone(), crate::source::Origin::CommandLine).1,
                None => m.name,
            };
            if let Ok(s) = self.instantiate_begin(m, inst, None, &ov, &[], m.name) {
                if self.d.top.is_none() {
                    self.d.top = Some(s);
                }
                let inputs: Vec<VarId> = self.scopes[s.0 as usize]
                    .ports
                    .iter()
                    .filter(|p| p.dir == Dir::Input)
                    .map(|p| p.var)
                    .collect();
                self.d.top_inputs.extend(inputs);
                let _ = self.expand_scope(s);
            }
        }
        // Phase B: lower the code in every scope. Hierarchical references
        // can now reach any scope in the design.
        let mut i = 0;
        while i < self.scopes.len() {
            let _ = self.build_scope(ScopeId(i as u32));
            i += 1;
        }
        // Methods of classes completed after their scope was built.
        loop {
            let pending: Vec<usize> = (0..self.scopes.len())
                .filter(|&s| !self.scopes[s].funcs.is_empty())
                .collect();
            if pending.is_empty() {
                break;
            }
            for s in pending {
                let saved = std::mem::replace(&mut self.cur, ScopeId(s as u32));
                let _ = self.build_funcs(ScopeId(s as u32));
                self.cur = saved;
            }
        }
        self.apply_fixups();
        let mut procs = std::mem::take(&mut self.init_procs);
        procs.append(&mut self.d.procs);
        self.d.procs = procs;
    }

    // ------------------------------------------------------------ instances

    /// Pass 1 of a module instance: scope, parameters, ports and declarations.
    fn instantiate_begin(
        &mut self,
        m: &'t ast::Module<'a>,
        name: &'a str,
        parent: Option<ScopeId>,
        overrides: &[(Option<&'a str>, Override<'a>)],
        conns: &'t [ast::PortConn<'a>],
        at: &'a str,
    ) -> EResult<ScopeId> {
        if self.depth > 200 {
            return Err(self.error(
                at,
                format!("Recursive module instantiation of '{}'", m.name),
            ));
        }
        let unit = self.modules.get(m.name).map_or(-12, |(_, t)| t.0);
        let s = self.new_scope(
            name,
            parent,
            Some(m.name),
            Some(self.unit_scope),
            unit,
            &m.items,
        );
        if let Some(p) = parent {
            let saved = std::mem::replace(&mut self.cur, p);
            self.declare(name, Sym::Scope(s));
            self.cur = saved;
        }
        let saved = std::mem::replace(&mut self.cur, s);
        self.depth += 1;
        let r = self
            .module_decls(m, overrides, parent.map(|p| (p, conns)))
            .and_then(|_| self.implicit_nets(&m.items));
        self.depth -= 1;
        self.cur = saved;
        r.map(|_| s)
    }

    /// Phase A for a scope: instantiate children and expand generate constructs.
    fn expand_scope(&mut self, s: ScopeId) -> EResult<()> {
        let saved = std::mem::replace(&mut self.cur, s);
        let items = self.scopes[s.0 as usize].items;
        let r = self.expand(items);
        self.cur = saved;
        r
    }

    fn expand(&mut self, items: &'t [ast::ModuleItem<'a>]) -> EResult<()> {
        use ast::ModuleItem as I;
        for item in items {
            let r = match item {
                I::Instance(inst) => self.instance(inst),
                I::Generate(v) => self.expand(v),
                I::GenIf {
                    cond, then, els, ..
                } => {
                    let n = self.next_genblk();
                    match self.const_int(cond) {
                        Ok(c) if c != 0 => self.gen_block(then, n, None),
                        Ok(_) => els.as_ref().map_or(Ok(()), |e| self.gen_block(e, n, None)),
                        Err(e) => Err(e),
                    }
                }
                I::GenCase { expr, items, .. } => self.gen_case(expr, items),
                I::GenFor {
                    var,
                    init,
                    cond,
                    step,
                    body,
                    kw,
                } => self.gen_for(kw, var, init, cond, step, body),
                I::GenBlock(b) => {
                    let n = self.next_genblk();
                    self.gen_block(b, n, None)
                }
                _ => Ok(()),
            };
            // Keep going after an error, so later independent problems are reported too.
            if r.is_err() && self.diags.iter().any(|d| d.code == Some(NOT_YET)) {
                return r;
            }
        }
        Ok(())
    }

    fn gen_case(
        &mut self,
        expr: &'t ast::Expr<'a>,
        items: &'t [(Vec<ast::Expr<'a>>, ast::GenBlock<'a>)],
    ) -> EResult<()> {
        let n = self.next_genblk();
        let v = self.const_value(expr, None)?;
        let mut chosen = None;
        'outer: for (labels, blk) in items {
            if labels.is_empty() {
                chosen.get_or_insert(blk);
                continue;
            }
            for l in labels {
                if self.const_value(l, None)? == v {
                    chosen = Some(blk);
                    break 'outer;
                }
            }
        }
        match chosen {
            Some(blk) => self.gen_block(blk, n, None),
            None => Ok(()),
        }
    }

    /// Declare 1-bit wires for names used, but never declared, on gate
    /// terminals, port connections and continuous-assignment targets (LRM 6.10).
    fn implicit_nets(&mut self, items: &'t [ast::ModuleItem<'a>]) -> EResult<()> {
        use ast::ModuleItem as I;
        let mut names: Vec<&'a str> = Vec::new();
        fn idents<'a>(e: &ast::Expr<'a>, out: &mut Vec<&'a str>) {
            match e {
                ast::Expr::Ident(n) => out.push(n),
                ast::Expr::Concat(v) => v.iter().for_each(|e| idents(e, out)),
                _ => {}
            }
        }
        for item in items {
            match item {
                I::Instance(inst) => {
                    for i in &inst.insts {
                        for c in &i.conns {
                            match c {
                                ast::PortConn::Ordered(Some(e))
                                | ast::PortConn::Named(_, Some(Some(e))) => idents(e, &mut names),
                                _ => {}
                            }
                        }
                    }
                }
                I::Gate(g) => g
                    .insts
                    .iter()
                    .flat_map(|(_, a)| a)
                    .for_each(|e| idents(e, &mut names)),
                I::Assign { assigns, .. } => {
                    assigns.iter().for_each(|(l, _)| idents(l, &mut names))
                }
                I::Generate(v) => self.implicit_nets(v)?,
                _ => {}
            }
        }
        for n in names {
            if self.lookup(n).is_none() {
                self.declare_var(
                    n,
                    Ty::scalar(Base::Bit { four: true }),
                    VarKind::Net(NetResolution::Wire),
                    n,
                );
            }
        }
        Ok(())
    }

    fn module_decls(
        &mut self,
        m: &'t ast::Module<'a>,
        overrides: &[(Option<&'a str>, Override<'a>)],
        bind: Option<(ScopeId, &'t [ast::PortConn<'a>])>,
    ) -> EResult<()> {
        for imp in &m.imports {
            self.import(imp)?;
        }
        // Parameters in the header are the overridable ones; without a header,
        // the body's `parameter`s are.
        self.predeclare_funcs(m.items.iter());
        let mut ordered = overrides
            .iter()
            .filter(|(n, _)| n.is_none())
            .map(|(_, o)| o);
        let named: HashMap<&str, &Override<'a>> = overrides
            .iter()
            .filter_map(|(n, o)| n.map(|n| (n, o)))
            .collect();
        let mut used_named = 0;
        let mut early: Vec<Early<'a, 't>> = Vec::new();
        if let Some(ps) = &m.params {
            for p in ps {
                let local = p.kw == Some("localparam");
                for a in &p.assigns {
                    let ov = if local {
                        None
                    } else if let Some(o) = named.get(a.name) {
                        used_named += 1;
                        Some(*o)
                    } else {
                        ordered.next()
                    };
                    early.push(Early::Param(p, a, ov.cloned()));
                }
            }
        }
        let header = m.params.is_some();
        // Body parameters take the remaining overrides when there is no header.
        let mut body_overrides =
            |a: &ast::ParamAssign<'a>, p: &ast::ParamDecl<'a>| -> Option<&Override<'a>> {
                if header || p.kw == Some("localparam") {
                    return None;
                }
                if let Some(o) = named.get(a.name) {
                    used_named += 1;
                    return Some(*o);
                }
                ordered.next()
            };
        for item in &m.items {
            match item {
                ast::ModuleItem::Param(p) => {
                    for a in &p.assigns {
                        early.push(Early::Param(p, a, body_overrides(a, p).cloned()));
                    }
                }
                ast::ModuleItem::Typedef(_) => early.push(Early::Item(item)),
                _ => {}
            }
        }
        if ordered.next().is_some() {
            return Err(self.error(
                m.name,
                format!("Too many parameters for module '{}'", m.name),
            ));
        }
        let _ = used_named;
        // Interface ports are bound first: typedefs may use their types.
        if let Some((parent, conns)) = bind {
            self.prebind_ifaces(m, parent, conns);
        }
        // Parameters and typedefs may refer to ones declared later.
        self.resolve_early(early)?;

        // Ports in the header.
        let cur = self.cur;
        match &m.ports {
            ast::Ports::None => {}
            ast::Ports::Wildcard => return Err(self.not_yet(m.name, "wildcard port lists")),
            ast::Ports::Ansi(ports) => {
                for p in ports {
                    self.last_at = p.name;
                    let iface = match (&p.interface, &p.ty) {
                        (Some((i, _)), _) => Some(*i),
                        (None, ast::DataType::Named { scope: None, name, .. })
                            if !self.is_type_name(&p.ty) =>
                        {
                            Some(*name)
                        }
                        _ => None,
                    };
                    if let Some(i) = iface {
                        if i != "interface"
                            && !matches!(self.modules.get(i), Some((m, _)) if m.kind == "interface")
                        {
                            return Err(self.error(p.name, format!("Cannot find interface '{i}'")));
                        }
                        if !p.dims.is_empty() {
                            return Err(self.not_yet(p.name, "arrays of interface ports"));
                        }
                        self.info(cur).ports.push(PortInfo {
                            name: p.name,
                            dir: Dir::Interface,
                            var: VarId(u32::MAX),
                            ty: Ty::scalar(Base::Void),
                        });
                        // Bound now: later port types may come from it.
                        if let Some((parent, conns)) = bind {
                            let saved = std::mem::replace(&mut self.cur, parent);
                            let r = self.connect_ifaces(cur, conns, m.name);
                            self.cur = saved;
                            r?;
                        }
                        continue;
                    }
                    if p.default.is_some() {
                        return Err(self.not_yet(p.name, "port default values"));
                    }
                    let dir = match p.dir.unwrap_or("inout") {
                        "input" => Dir::Input,
                        "output" => Dir::Output,
                        "inout" => Dir::Inout,
                        _ => return Err(self.not_yet(p.name, "ref ports")),
                    };
                    let mut ty = self.resolve_type(&p.ty)?;
                    ty.unpacked = self.unpacked_dims(&p.dims)?;
                    let variable = p.kind == Some("var")
                        || (dir == Dir::Output
                            && matches!(
                                p.ty,
                                ast::DataType::Builtin { .. }
                                    | ast::DataType::Named { .. }
                                    | ast::DataType::Enum { .. }
                                    | ast::DataType::Struct { .. }
                            ));
                    let kind = if variable {
                        VarKind::Variable
                    } else {
                        VarKind::Net(net_kind(p.kind))
                    };
                    let var = self.declare_var(p.name, ty.clone(), kind, p.name);
                    self.info(cur).ports.push(PortInfo {
                        name: p.name,
                        dir,
                        var,
                        ty,
                    });
                }
            }
            ast::Ports::NonAnsi(_) => {}
        }

        // Interface ports first: declarations may use their types.
        if let Some((p, conns)) = bind
            && !conns.is_empty()
        {
            let saved = std::mem::replace(&mut self.cur, p);
            let r = self.connect_ifaces(cur, conns, m.name);
            self.cur = saved;
            r?;
        }

        // The other declarations, in order.
        let deferred = std::mem::take(&mut self.deferred);
        for item in &m.items {
            if !matches!(
                item,
                ast::ModuleItem::Param(_) | ast::ModuleItem::Typedef(_)
            ) {
                self.declare_item(item)?;
            }
        }
        self.deferred = deferred;
        self.report_deferred()?;

        // Non-ANSI ports: the header names, with directions from the body.
        if let ast::Ports::NonAnsi(refs) = &m.ports {
            for r in refs {
                let Some(ast::Expr::Ident(name)) = &r.expr else {
                    let at = r
                        .name
                        .or_else(|| r.expr.as_ref().map(|e| e.at()))
                        .unwrap_or(m.name);
                    return Err(self.not_yet(at, "complex port expressions"));
                };
                if r.name.is_some() {
                    return Err(self.not_yet(name, "named port expressions"));
                }
                let Some(dir) = self.scopes[cur.0 as usize].dirs.get(name).copied() else {
                    return Err(self.error(
                        name,
                        format!("Input/output/inout does not appear in port list: '{name}'"),
                    ));
                };
                let Some(Sym::Var(var, ty)) = self.lookup_in(cur, name) else {
                    return Err(self.error(name, format!("Port not declared: '{name}'")));
                };
                self.info(cur).ports.push(PortInfo { name, dir, var, ty });
            }
        }
        Ok(())
    }

    fn is_type_name(&mut self, t: &ast::DataType<'a>) -> bool {
        match t {
            ast::DataType::Named { scope, name, .. } => {
                matches!(self.lookup_type(*scope, name), Ok(Some(_)))
            }
            _ => true,
        }
    }

    fn import(&mut self, imp: &ast::Import<'a>) -> EResult<()> {
        let Some(&p) = self.packages.get(imp.package) else {
            return Err(self.error(
                imp.package,
                format!("Importing from missing package '{}'", imp.package),
            ));
        };
        if imp.name == "*" {
            let cur = self.cur;
            self.info(cur).imports.push(p);
        } else {
            match self.lookup_in(p, imp.name) {
                Some(sym) => self.declare(imp.name, sym),
                None => {
                    return Err(self.error(
                        imp.name,
                        format!("Import object not found: '{}::{}'", imp.package, imp.name),
                    ));
                }
            }
        }
        Ok(())
    }

    fn declare_param(
        &mut self,
        p: &ast::ParamDecl<'a>,
        a: &ast::ParamAssign<'a>,
        ov: Option<&Override<'a>>,
    ) -> EResult<()> {
        self.last_at = a.name;
        if p.ty.is_none() {
            // Type parameter.
            let ty = match ov {
                Some(Override::Type(t)) => t.clone(),
                Some(Override::Value(..)) => {
                    return Err(self.error(a.name, "Expected a type for a type parameter"));
                }
                None => match &a.value {
                    Some(ast::Expr::Type(t)) => self.resolve_type(t)?,
                    _ => {
                        return Err(self.error(
                            a.name,
                            format!("Type parameter '{}' has no default", a.name),
                        ));
                    }
                },
            };
            self.declare(a.name, Sym::Type(ty));
            return Ok(());
        }
        // `parameter signed P = ...`: the value's width, with this signedness.
        let sign_only = match &p.ty {
            Some(ast::DataType::Implicit {
                signing: Some(s),
                packed,
            }) if packed.is_empty() => Some(*s == "signed"),
            _ => None,
        };
        let declared = match &p.ty {
            Some(ast::DataType::Implicit { packed, .. }) if packed.is_empty() => None,
            Some(t) => {
                let mut ty = self.resolve_type(t)?;
                ty.unpacked = self.unpacked_dims(&a.dims)?;
                Some(ty)
            }
            None => None,
        };
        let (value, ty) = match ov {
            Some(Override::Value(v, vty)) => match &declared {
                Some(t) => (convert_value(v, vty, t), t.clone()),
                None => (v.clone(), vty.clone()),
            },
            Some(Override::Type(_)) => {
                return Err(self.error(a.name, "Expected a value for a value parameter"));
            }
            None => {
                let Some(e) = &a.value else {
                    return Err(self.error(
                        a.name,
                        format!(
                            "Parameter without default value is never given value: '{}'",
                            a.name
                        ),
                    ));
                };
                match &declared {
                    Some(t) => (self.const_value(e, Some(t))?, t.clone()),
                    None => {
                        let st = self.self_type(e)?;
                        let ty = st.ty();
                        (self.const_value(e, Some(&ty))?, ty)
                    }
                }
            }
        };
        let (value, ty) = match sign_only {
            Some(signed) if ty.is_integral() && ty.signed != signed => {
                let mut to = ty.clone();
                to.signed = signed;
                (convert_value(&value, &ty, &to), to)
            }
            _ => (value, ty),
        };
        self.declare(a.name, Sym::Param(value, ty));
        Ok(())
    }

    // ------------------------------------------------------------ pass 1: declarations

    fn declare_items(&mut self, items: &'t [ast::ModuleItem<'a>]) -> EResult<()> {
        self.predeclare_funcs(items.iter());
        let mut early = Vec::new();
        for item in items {
            match item {
                ast::ModuleItem::Param(p) => {
                    early.extend(p.assigns.iter().map(|a| Early::Param(p, a, None)))
                }
                ast::ModuleItem::Typedef(_) => early.push(Early::Item(item)),
                _ => {}
            }
        }
        self.resolve_early(early)?;
        let deferred = std::mem::take(&mut self.deferred);
        for item in items {
            if !matches!(
                item,
                ast::ModuleItem::Param(_) | ast::ModuleItem::Typedef(_)
            ) {
                self.declare_item(item)?;
            }
        }
        self.deferred = deferred;
        self.report_deferred()
    }

    /// Declare parameters and typedefs in dependency order: anything that
    /// fails is retried after the others, until no more progress is made.
    fn resolve_early(&mut self, mut todo: Vec<Early<'a, 't>>) -> EResult<()> {
        loop {
            let mut left = Vec::new();
            let mut progress = false;
            for e in todo {
                let n = self.diags.len();
                let r = match &e {
                    Early::Param(p, a, ov) => self.declare_param(p, a, ov.as_ref()),
                    Early::Item(item) => self.declare_item(item),
                };
                match r {
                    Ok(()) => progress = true,
                    Err(_) => {
                        self.diags.truncate(n);
                        left.push(e);
                    }
                }
            }
            if left.is_empty() {
                return Ok(());
            }
            if !progress {
                // A construct we don't support yet is reported now. Anything
                // else is reported once the other declarations exist, so the
                // error can say what is really wrong.
                let n = self.diags.len();
                let r = match &left[0] {
                    Early::Param(p, a, ov) => self.declare_param(p, a, ov.as_ref()),
                    Early::Item(item) => self.declare_item(item),
                };
                if r.is_err() && self.diags[n..].iter().any(|d| d.code == Some(NOT_YET)) {
                    return r;
                }
                self.diags.truncate(n);
                self.deferred = left;
                return Ok(());
            }
            todo = left;
        }
    }

    /// Retry the declarations that couldn't be resolved earlier, now
    /// reporting their errors.
    fn report_deferred(&mut self) -> EResult<()> {
        let left = std::mem::take(&mut self.deferred);
        for e in &left {
            match e {
                Early::Param(p, a, ov) => self.declare_param(p, a, ov.as_ref())?,
                Early::Item(item) => self.declare_item(item)?,
            }
        }
        Ok(())
    }

    /// Give every function and task in `items` an ID before anything else,
    /// so they can be called before their declaration (including from
    /// parameter expressions). Signatures are worked out on first use.
    fn predeclare_funcs(&mut self, items: impl Iterator<Item = &'t ast::ModuleItem<'a>>) {
        let cur = self.cur;
        for item in items {
            if let ast::ModuleItem::Function(f) | ast::ModuleItem::Task(f) = item
                && let Some(c) = f.class
            {
                // `function C::f`: the body of a method declared `extern` in C.
                self.out_of_class.insert((c, f.name), f);
                continue;
            }
            if let ast::ModuleItem::Function(f) | ast::ModuleItem::Task(f) = item {
                let id = FuncId(self.d.funcs.len() as u32);
                self.d.funcs.push(Func {
                    name: f.name,
                    scope: cur,
                    params: Vec::new(),
                    ret: None,
                    is_task: f.kw == "task",
                    body: Body::default(),
                    at: f.name,
                });
                self.func_defs.insert(id, f);
                self.func_src.insert(id, f);
                self.declare(f.name, Sym::Func(id));
                self.info(cur).funcs.push((id, f));
            }
        }
    }

    /// A function's signature, worked out on first use.
    pub(crate) fn sig(&mut self, f: FuncId) -> EResult<Sig<'a>> {
        if let Some(s) = self.sigs.get(&f) {
            return Ok(s.clone());
        }
        let def = self.func_src[&f];
        let saved = std::mem::replace(&mut self.cur, self.d.funcs[f.0 as usize].scope);
        let r = self.signature(def);
        self.cur = saved;
        let sig = r?;
        let params = sig.params.iter().map(|(_, t)| self.ir_type(t)).collect();
        let ret = sig.ret.as_ref().map(|t| self.ir_type(t));
        let func = &mut self.d.funcs[f.0 as usize];
        func.params = params;
        func.ret = ret;
        self.sigs.insert(f, sig.clone());
        Ok(sig)
    }

    fn declare_item(&mut self, item: &'t ast::ModuleItem<'a>) -> EResult<()> {
        use ast::ModuleItem as I;
        let cur = self.cur;
        match item {
            I::Class(c) => self.declare_class(c),
            I::Param(p) => {
                for a in &p.assigns {
                    self.declare_param(p, a, None)?;
                }
            }
            I::Typedef(t) => {
                self.last_at = t.name;
                let mut ty = self.resolve_type(&t.ty)?;
                ty.unpacked.extend(self.unpacked_dims(&t.dims)?);
                self.declare(t.name, Sym::Type(ty));
            }
            I::ForwardTypedef(_) => {}
            I::Import(imps) => {
                for i in imps {
                    self.import(i)?;
                }
            }
            I::Port(pd) => {
                let dir = match pd.dir {
                    "input" => Dir::Input,
                    "output" => Dir::Output,
                    "inout" => Dir::Inout,
                    _ => return Err(self.not_yet(pd.dir, "ref ports")),
                };
                let variable = pd.decl.qualifiers.contains(&"var")
                    || matches!(&pd.decl.ty, ast::DataType::Builtin { kw, .. } if *kw != "logic" || dir == Dir::Output);
                for v in &pd.decl.vars {
                    self.info(cur).dirs.insert(v.name, dir);
                    let kind = if variable {
                        VarKind::Variable
                    } else {
                        VarKind::Net(net_kind(pd.decl.net))
                    };
                    self.declare_or_merge(v, &pd.decl.ty, kind, true)?;
                }
            }
            I::Var(d) => {
                let kind = match d.net {
                    Some(n) => VarKind::Net(net_kind(Some(n))),
                    None => VarKind::Variable,
                };
                for v in &d.vars {
                    let var = self.declare_or_merge(v, &d.ty, kind, false)?;
                    if let Some(init) = &v.init {
                        let Some(Sym::Var(_, ty)) = self.lookup_in(cur, v.name) else {
                            continue;
                        };
                        self.var_init(var, ty, init, matches!(kind, VarKind::Net(_)))?;
                    }
                }
            }
            // `function C::f`: part of class C.
            I::Function(f) | I::Task(f) if f.class.is_some() => {}
            I::Function(f) | I::Task(f) => {
                self.last_at = f.name;
                if let Some(Sym::Func(id)) = self.lookup_in(cur, f.name) {
                    self.sig(id)?;
                }
            }
            I::Genvar(names) => {
                for n in names {
                    self.declare(n, Sym::Genvar);
                }
            }
            // A modport stands for its interface in dotted names: `ifc.mp.x`.
            I::Modport(mps) => {
                let cur = self.cur;
                for mp in mps {
                    self.declare(mp.name, Sym::Scope(cur));
                }
            }
            I::TimeUnits { .. } | I::Directive(_) => {}
            I::Generate(items) => self.declare_items(items)?,
            I::Defparam(v) => {
                return Err(
                    self.not_yet(v.first().map_or(self.here(), |(e, _)| e.at()), "defparam")
                );
            }
            I::Module(m) => return Err(self.not_yet(m.name, "nested modules")),
            I::Assign { .. }
            | I::Process { .. }
            | I::Instance(_)
            | I::Gate(_)
            | I::GenFor { .. }
            | I::GenIf { .. }
            | I::GenCase { .. }
            | I::GenBlock(_)
            | I::ElabTask(_) => {}
        }
        Ok(())
    }

    /// Declare a net or variable, or complete an earlier declaration of the
    /// same name (`output q; reg q;`).
    fn declare_or_merge(
        &mut self,
        v: &ast::Declarator<'a>,
        dt: &ast::DataType<'a>,
        kind: VarKind,
        is_port: bool,
    ) -> EResult<VarId> {
        self.last_at = v.name;
        let mut ty = self.resolve_type(dt)?;
        ty.unpacked = self.unpacked_dims(&v.dims)?;
        let cur = self.cur;
        if let Some(Sym::Var(id, old)) = self.scopes[cur.0 as usize].syms.get(v.name).cloned() {
            let explicit = !matches!(dt, ast::DataType::Implicit { signing: None, packed } if packed.is_empty());
            let merged = if explicit || old.width() == 1 && ty.width() > 1 {
                ty
            } else {
                old
            };
            let irt = self.ir_type(&merged);
            let var = &mut self.d.vars[id.0 as usize];
            var.ty = irt;
            if !is_port || kind == VarKind::Variable {
                var.kind = kind;
            }
            self.declare(v.name, Sym::Var(id, merged));
            return Ok(id);
        }
        Ok(self.declare_var(v.name, ty, kind, v.name))
    }

    pub(crate) fn declare_var(
        &mut self,
        name: &'a str,
        ty: Ty<'a>,
        kind: VarKind,
        at: &'a str,
    ) -> VarId {
        let irt = self.ir_type(&ty);
        let id = VarId(self.d.vars.len() as u32);
        self.d.vars.push(Var {
            name,
            scope: self.cur,
            ty: irt,
            kind,
            init: None,
            at,
        });
        self.declare(name, Sym::Var(id, ty));
        id
    }

    /// An initialiser: a constant becomes the variable's initial value;
    /// anything else, or any net driver, becomes code in pass 2.
    fn var_init(
        &mut self,
        var: VarId,
        ty: Ty<'a>,
        init: &'t ast::Expr<'a>,
        net: bool,
    ) -> EResult<()> {
        if ty.is_integral()
            && let Some(Value::Bits(b)) = self.try_const(init, &ty)
        {
            self.d.vars[var.0 as usize].init = Some(b);
            return Ok(());
        }
        let cur = self.cur;
        self.info(cur).inits.push((var, ty, init, net));
        Ok(())
    }

    fn signature(&mut self, f: &'t ast::Subroutine<'a>) -> EResult<Sig<'a>> {
        let ret = match &f.ret {
            Some(t) if f.kw == "function" => {
                let ty = self.resolve_type(t)?;
                if ty.base == Base::Void {
                    None
                } else {
                    Some(ty)
                }
            }
            // A constructor has no value.
            None if f.kw == "function" && f.name != "new" => Some(Ty::scalar(Base::Bit { four: true })),
            _ => None,
        };
        let mut params = Vec::new();
        let defaults: Vec<Option<ast::Expr<'a>>> = match &f.ports {
            Some(ps) => ps.iter().map(|p| p.default.clone()).collect(),
            None => Vec::new(),
        };
        let ports: Vec<(
            Option<&'a str>,
            &ast::DataType<'a>,
            &'a str,
            &[ast::Dim<'a>],
        )> = match &f.ports {
            Some(ps) => ps
                .iter()
                .map(|p| (p.dir, &p.ty, p.name, &p.dims[..]))
                .collect(),
            None => f
                .decls
                .iter()
                .filter_map(|d| match d {
                    ast::ModuleItem::Port(pd) => Some(pd),
                    _ => None,
                })
                .flat_map(|pd| {
                    pd.decl
                        .vars
                        .iter()
                        .map(move |v| (Some(pd.dir), &pd.decl.ty, v.name, &v.dims[..]))
                })
                .collect(),
        };
        // `input num; real num;`: a later declaration gives an untyped port its type.
        let retyped = |name: &str| {
            f.decls.iter().find_map(|d| match d {
                ast::ModuleItem::Var(v) if v.vars.iter().any(|x| x.name == name) => Some(&v.ty),
                _ => None,
            })
        };
        let mut dirs = Vec::new();
        // An argument without a direction takes the previous one's (LRM 13.3).
        let mut last = Dir::Input;
        for (dir, ty, name, dims) in ports {
            let ty = match ty {
                ast::DataType::Implicit {
                    signing: None,
                    packed,
                } if packed.is_empty() && f.ports.is_none() => retyped(name).unwrap_or(ty),
                _ => ty,
            };
            last = match dir {
                None => last,
                Some("input") => Dir::Input,
                Some("output") => Dir::Output,
                // `ref` arguments are copied in and out, which is the same
                // unless the callee waits or the caller's variable changes meanwhile.
                Some("inout" | "ref") | Some("const ref") => Dir::Inout,
                Some(d) => return Err(self.not_yet(name, &format!("{d} subroutine arguments"))),
            };
            dirs.push(last);
            let mut t = self.resolve_type(ty)?;
            t.unpacked = self.unpacked_dims(dims)?;
            params.push((name, t));
        }
        Ok(Sig {
            params,
            dirs,
            defaults,
            ret,
        })
    }

    // ------------------------------------------------------------ pass 2: code

    fn build_funcs(&mut self, s: ScopeId) -> EResult<()> {
        let funcs = std::mem::take(&mut self.info(s).funcs);
        for (id, f) in funcs {
            if self.d.funcs[id.0 as usize].body.blocks.is_empty() {
                self.func_defs.remove(&id);
                self.lower_func(id, f)?;
            }
        }
        Ok(())
    }

    /// Lower a function now if it hasn't been, so a constant expression can call it.
    pub(crate) fn ensure_lowered(&mut self, f: FuncId) -> EResult<()> {
        if !self.d.funcs[f.0 as usize].body.blocks.is_empty() {
            return Ok(());
        }
        let Some(def) = self.func_defs.remove(&f) else {
            return Ok(());
        };
        let saved = std::mem::replace(&mut self.cur, self.d.funcs[f.0 as usize].scope);
        let r = self.lower_func(f, def);
        self.cur = saved;
        r
    }

    /// Phase B for one scope: functions, initialisers and processes.
    fn build_scope(&mut self, s: ScopeId) -> EResult<()> {
        let saved = std::mem::replace(&mut self.cur, s);
        let r = (|| {
            self.build_funcs(s)?;
            let inits = std::mem::take(&mut self.info(s).inits);
            for (var, ty, init, net) in inits {
                self.init_process(var, ty, init, net)?;
            }
            let items = self.scopes[s.0 as usize].items;
            self.build_code(items)
        })();
        self.cur = saved;
        r
    }

    fn build_code(&mut self, items: &'t [ast::ModuleItem<'a>]) -> EResult<()> {
        use ast::ModuleItem as I;
        let mut result = Ok(());
        for item in items {
            let r = match item {
                I::Assign { assigns, kw, .. } => assigns
                    .iter()
                    .try_for_each(|(lhs, rhs)| self.cont_assign(lhs, rhs, kw)),
                I::Process { kw, stmt } => self.lower_process(kw, stmt),
                I::Gate(g) => self.gate(g),
                I::Generate(items) => self.build_code(items),
                I::ElabTask(e) => self.elab_task(e),
                _ => Ok(()),
            };
            if r.is_err() && result.is_ok() {
                result = r;
            }
        }
        result
    }

    fn next_genblk(&mut self) -> u32 {
        let cur = self.cur;
        let i = self.info(cur);
        i.genblk += 1;
        i.genblk
    }

    /// Elaborate a generate block in its own scope. `index` is set inside a
    /// generate loop, giving names like `g[3]`.
    fn gen_block(
        &mut self,
        b: &'t ast::GenBlock<'a>,
        n: u32,
        index: Option<(&'a str, i64)>,
    ) -> EResult<()> {
        let base = match b.label {
            Some(l) => l.to_string(),
            None => format!("genblk{n}"),
        };
        let name = match index {
            Some((_, i)) => format!("{base}[{i}]"),
            None => base,
        };
        let at = b.label.unwrap_or(self.here());
        let name: &'a str = match (b.label, index) {
            (Some(l), None) => l,
            _ => self.sm.derive(name, at),
        };
        let unit = self.d.scopes[self.cur.0 as usize].unit;
        let parent = self.cur;
        let s = self.new_scope(name, Some(parent), None, Some(parent), unit, &b.items);
        // Indexed blocks are found by their full name, `g[3]`.
        self.declare(name, Sym::Scope(s));
        let saved = std::mem::replace(&mut self.cur, s);
        if let Some((var, i)) = index {
            self.declare(
                var,
                Sym::Param(
                    Value::Bits(Bits::from_i64(32, i)),
                    Ty::bits(32, true, false),
                ),
            );
        }
        let r = self
            .declare_items(&b.items)
            .and_then(|_| self.implicit_nets(&b.items))
            .and_then(|_| self.expand(&b.items));
        self.cur = saved;
        r
    }

    fn gen_for(
        &mut self,
        kw: &'a str,
        var: &'a str,
        init: &'t ast::Expr<'a>,
        cond: &'t ast::Expr<'a>,
        step: &'t ast::Stmt<'a>,
        body: &'t ast::GenBlock<'a>,
    ) -> EResult<()> {
        let n = self.next_genblk();
        let int = Ty::bits(32, true, false);
        let mut i = self.const_int(init)?;
        // Evaluate `cond` and `step` with the genvar bound in a scratch scope.
        let parent = self.cur;
        let unit = self.d.scopes[parent.0 as usize].unit;
        let scratch = self.new_scope("", Some(parent), None, Some(parent), unit, &[]);
        let mut count = 0;
        loop {
            let saved = std::mem::replace(&mut self.cur, scratch);
            self.declare(
                var,
                Sym::Param(Value::Bits(Bits::from_i64(32, i)), int.clone()),
            );
            let go = self.const_int(cond);
            self.cur = saved;
            if go? == 0 {
                break;
            }
            count += 1;
            if count > 100_000 {
                return Err(self.error(kw, "Generate for loop does not terminate"));
            }
            self.gen_block(body, n, Some((var, i)))?;
            let saved = std::mem::replace(&mut self.cur, scratch);
            let next = self.genvar_step(var, step, i);
            self.cur = saved;
            let next = next?;
            if next == i {
                return Err(self.error(kw, "Generate for loop does not advance its genvar"));
            }
            i = next;
        }
        Ok(())
    }

    fn genvar_step(&mut self, var: &'a str, step: &'t ast::Stmt<'a>, i: i64) -> EResult<i64> {
        match step {
            ast::Stmt::Assign {
                lhs: ast::Expr::Ident(n),
                op,
                rhs,
                ..
            } if *n == var => {
                let r = self.const_int(rhs)?;
                Ok(match *op {
                    "=" => r,
                    "+=" => i + r,
                    "-=" => i - r,
                    "*=" => i * r,
                    "<<=" => i << r,
                    ">>=" => i >> r,
                    _ => return Err(self.not_yet(op, "this genvar step")),
                })
            }
            ast::Stmt::Expr(ast::Expr::IncDec { op, arg, .. }) if matches!(&**arg, ast::Expr::Ident(n) if *n == var) => {
                Ok(if *op == "++" { i + 1 } else { i - 1 })
            }
            _ => Err(self.error(var, "Generate for loop step must assign the genvar")),
        }
    }

    fn elab_task(&mut self, e: &ast::Expr<'a>) -> EResult<()> {
        let ast::Expr::SysCall { name, args } = e else {
            return Ok(());
        };
        let mut text = String::new();
        for a in args {
            if let ast::Arg::Ordered(Some(ast::Expr::Str(s))) = a {
                text.push_str(&decode_string(str_body(s)));
            }
        }
        match *name {
            "$error" | "$fatal" => Err(self.error(name, text)),
            "$warning" => {
                self.warn(name, "USERWARN", text);
                Ok(())
            }
            _ => {
                self.warn(name, "USERINFO", text);
                Ok(())
            }
        }
    }

    fn instance(&mut self, inst: &'t ast::Instance<'a>) -> EResult<()> {
        self.last_at = inst.module;
        let Some((m, _)) = self.modules.get(inst.module).copied() else {
            return Err(self.error(
                inst.module,
                format!("Cannot find file containing module: '{}'", inst.module),
            ));
        };
        // Parameter overrides, evaluated here in the parent.
        let mut overrides = Vec::new();
        for p in inst.params.iter().flatten() {
            let (name, e) = match p {
                ast::ParamArg::Ordered(e) => (None, Some(e)),
                ast::ParamArg::Named(n, e) => (Some(*n), e.as_ref()),
            };
            let Some(e) = e else { continue };
            overrides.push((name, self.override_value(e)?));
        }
        for i in &inst.insts {
            if self.early_insts.contains(&(i as *const ast::Inst as usize)) {
                continue;
            }
            if !i.dims.is_empty() {
                self.instance_array(m, i, &overrides)?;
                continue;
            }
            let parent = self.cur;
            let name = if i.name.is_empty() {
                inst.module
            } else {
                i.name
            };
            let child =
                self.instantiate_begin(m, name, Some(parent), &overrides, &i.conns, name)?;
            self.connect(child, &i.conns, name)?;
            self.depth += 1;
            let r = self.expand_scope(child);
            self.depth -= 1;
            r?;
        }
        Ok(())
    }

    /// Instantiate the interface instance `name` of the current module now,
    /// for a declaration that uses a type from it (`typedef m.t t;`).
    pub(crate) fn early_interface(&mut self, name: &str) -> Option<ScopeId> {
        let items = self.scopes[self.cur.0 as usize].items;
        for item in items {
            let ast::ModuleItem::Instance(inst) = item else {
                continue;
            };
            let Some(i) = inst.insts.iter().find(|i| i.name == name && i.dims.is_empty()) else {
                continue;
            };
            let (m, _) = self.modules.get(inst.module).copied()?;
            if m.kind != "interface" {
                return None;
            }
            let mut overrides = Vec::new();
            for p in inst.params.iter().flatten() {
                let (pn, e) = match p {
                    ast::ParamArg::Ordered(e) => (None, Some(e)),
                    ast::ParamArg::Named(n, e) => (Some(*n), e.as_ref()),
                };
                let Some(e) = e else { continue };
                overrides.push((pn, self.override_value(e).ok()?));
            }
            let parent = self.cur;
            let child = self
                .instantiate_begin(m, i.name, Some(parent), &overrides, &i.conns, i.name)
                .ok()?;
            self.connect(child, &i.conns, i.name).ok()?;
            self.expand_scope(child).ok()?;
            self.early_insts.insert(i as *const ast::Inst as usize);
            return Some(child);
        }
        None
    }

    /// `sub s[l:r] (...)`: one instance per index, named `s[i]`. A
    /// connection N times as wide as its port, or an N-element array, is
    /// shared out from the left; anything else goes to every instance.
    fn instance_array(
        &mut self,
        m: &'t ast::Module<'a>,
        i: &'t ast::Inst<'a>,
        overrides: &[(Option<&'a str>, Override<'a>)],
    ) -> EResult<()> {
        let dims = self.unpacked_dims(&i.dims)?;
        let [types::UDim::Fixed(l, r)] = dims[..] else {
            return Err(self.not_yet(i.name, "multi-dimensional instance arrays"));
        };
        let n = types::range_len((l, r)) as i64;
        let parent = self.cur;
        for k in 0..n {
            let idx = if l <= r { l + k } else { l - k };
            let name = self
                .sm
                .add(format!("{}[{idx}]", i.name), crate::source::Origin::CommandLine)
                .1;
            let child = self.instantiate_begin(m, name, Some(parent), overrides, &[], i.name)?;
            let conns = self.array_conns(child, &i.conns, k, n)?;
            self.connect(child, conns, name)?;
            self.depth += 1;
            let r = self.expand_scope(child);
            self.depth -= 1;
            r?;
        }
        Ok(())
    }

    /// The connections of instance `k` (from the left) of `n`.
    fn array_conns(
        &mut self,
        child: ScopeId,
        conns: &'t [ast::PortConn<'a>],
        k: i64,
        n: i64,
    ) -> EResult<&'t [ast::PortConn<'a>]> {
        let ports = self.scopes[child.0 as usize].ports.clone();
        let num = |this: &mut Self, v: i64| {
            ast::Expr::Number(this.sm.add(v.to_string(), crate::source::Origin::CommandLine).1)
        };
        let mut out = Vec::new();
        for (pos, c) in conns.iter().enumerate() {
            let (port, e) = match c {
                ast::PortConn::Ordered(Some(e)) => (ports.get(pos), e),
                ast::PortConn::Named(name, Some(Some(e))) => {
                    (ports.iter().find(|p| p.name == *name), e)
                }
                _ => {
                    out.push(c.clone());
                    continue;
                }
            };
            let Some(port) = port else {
                out.push(c.clone());
                continue;
            };
            if port.dir == Dir::Interface {
                // An array of interfaces: instance k takes element k.
                if let ast::Expr::Ident(base) = e
                    && let Some(Sym::Scope(_)) = self.lookup(&format!("{base}[{}]", k))
                {
                    let index = Box::new(num(self, k));
                    let p = ast::Expr::Index {
                        base: Box::new(e.clone()),
                        index,
                    };
                    out.push(match c {
                        ast::PortConn::Named(name, _) => ast::PortConn::Named(name, Some(Some(p))),
                        _ => ast::PortConn::Ordered(Some(p)),
                    });
                } else {
                    out.push(c.clone());
                }
                continue;
            }
            let pw = port.ty.width() as i64;
            let cx = expr::Cx::new(false);
            let part = if let Some(at) = self.array_type(&cx, e)
                && port.ty.unpacked.is_empty()
                && expr::fixed_count(&at) == Some(n as u32)
            {
                // An array of connections: element k from the left.
                let types::UDim::Fixed(al, ar) = at.unpacked[0] else {
                    unreachable!()
                };
                let index = if al <= ar { al + k } else { al - k };
                Some(ast::Expr::Index {
                    base: Box::new(e.clone()),
                    index: Box::new(num(self, index)),
                })
            } else {
                let nd = self.diags.len();
                let w = self.self_type(e).map(|t| t.ty().width() as i64);
                self.diags.truncate(nd);
                match w {
                    Ok(w) if port.ty.unpacked.is_empty() && w == n * pw && n > 1 => {
                        // Bits [off +: pw], the leftmost instance taking the top bits.
                        let off = (n - 1 - k) * pw;
                        let range = match self.array_type(&cx, e) {
                            None => self.path_range(e),
                            Some(_) => None,
                        };
                        Some(match range {
                            Some((hi, lo)) if hi >= lo => ast::Expr::Slice {
                                base: Box::new(e.clone()),
                                op: "+:",
                                left: Box::new(num(self, lo + off)),
                                right: Box::new(num(self, pw)),
                            },
                            Some((_, hi)) => ast::Expr::Slice {
                                base: Box::new(e.clone()),
                                op: "-:",
                                left: Box::new(num(self, hi - off)),
                                right: Box::new(num(self, pw)),
                            },
                            // Not a plain vector: shift (an input only).
                            None if port.dir == Dir::Input => ast::Expr::Binary {
                                op: ">>",
                                lhs: Box::new(e.clone()),
                                rhs: Box::new(num(self, off)),
                            },
                            None => {
                                return Err(self.not_yet(
                                    e.at(),
                                    "this output connection to an instance array",
                                ));
                            }
                        })
                    }
                    _ => None,
                }
            };
            out.push(match (c, part) {
                (_, None) => c.clone(),
                (ast::PortConn::Ordered(_), Some(p)) => ast::PortConn::Ordered(Some(p)),
                (ast::PortConn::Named(name, _), Some(p)) => ast::PortConn::Named(name, Some(Some(p))),
                (_, Some(_)) => c.clone(),
            });
        }
        Ok(self.made.alloc(out))
    }

    /// The packed range `(left, right)` of a plain vector variable.
    fn path_range(&mut self, e: &ast::Expr<'a>) -> Option<(i64, i64)> {
        let ast::Expr::Ident(n) = e else { return None };
        match self.lookup(n) {
            Some(Sym::Var(_, t)) if t.unpacked.is_empty() => {
                Some(t.packed.first().copied().unwrap_or((t.width() as i64 - 1, 0)))
            }
            _ => None,
        }
    }

    pub(crate) fn override_value(&mut self, e: &ast::Expr<'a>) -> EResult<Override<'a>> {
        if let ast::Expr::Type(t) = e {
            return Ok(Override::Type(self.resolve_type(t)?));
        }
        if let ast::Expr::Ident(n) = e {
            match self.lookup(n) {
                Some(Sym::Type(t)) => return Ok(Override::Type(t)),
                // A class name as a type parameter.
                Some(Sym::ClassDef(d)) => {
                    let c = self.specialise(d, None, n)?;
                    return Ok(Override::Type(class::ClassInfo::ty(c)));
                }
                _ => {}
            }
        }
        // `my_t[7:0]` as a type: a typedef with packed dimensions added.
        if let ast::Expr::Slice {
            base,
            op: ":",
            left,
            right,
        } = e
            && let ast::Expr::Ident(n) = &**base
            && let Some(Sym::Type(mut t)) = self.lookup(n)
        {
            t.packed
                .insert(0, (self.const_int(left)?, self.const_int(right)?));
            return Ok(Override::Type(t));
        }
        // An array keeps its own type; anything else its self-determined one.
        let ty = match self.array_type(&expr::Cx::new(true), e) {
            Some(t) => t,
            None => self.self_type(e)?.ty(),
        };
        let v = self.const_value(e, Some(&ty))?;
        Ok(Override::Value(v, ty))
    }

    /// Bind the interface ports of the module being declared (the current
    /// scope) from the connections in `parent`, straight from its port list.
    fn prebind_ifaces(
        &mut self,
        m: &'t ast::Module<'a>,
        parent: ScopeId,
        conns: &'t [ast::PortConn<'a>],
    ) {
        let ast::Ports::Ansi(ports) = &m.ports else {
            return;
        };
        let child = self.cur;
        for (pos, p) in ports.iter().enumerate() {
            let is_iface = p.interface.is_some()
                || matches!(&p.ty, ast::DataType::Named { scope: None, name, .. }
                    if matches!(self.modules.get(name), Some((m, _)) if m.kind == "interface"));
            if !is_iface {
                continue;
            }
            let conn = conns.iter().enumerate().find_map(|(i, c)| match c {
                ast::PortConn::Ordered(Some(e)) if i == pos => Some(PortExpr::Expr(e)),
                ast::PortConn::Named(n, Some(Some(e))) if *n == p.name => Some(PortExpr::Expr(e)),
                ast::PortConn::Named(n, None) if *n == p.name => Some(PortExpr::Name(n)),
                ast::PortConn::Wildcard => Some(PortExpr::Name(p.name)),
                _ => None,
            });
            let Some(conn) = conn else { continue };
            let info = PortInfo {
                name: p.name,
                dir: Dir::Interface,
                var: VarId(u32::MAX),
                ty: Ty::scalar(Base::Void),
            };
            self.cur = parent;
            let n = self.diags.len();
            if self.connect_port(child, &info, conn).is_err() {
                // Reported again, in order, when the port itself is connected.
                self.diags.truncate(n);
            }
            self.cur = child;
        }
    }

    /// Bind just the interface ports of `child`.
    fn connect_ifaces(
        &mut self,
        child: ScopeId,
        conns: &'t [ast::PortConn<'a>],
        at: &'a str,
    ) -> EResult<()> {
        let ports = self.scopes[child.0 as usize].ports.clone();
        for (pos, c) in conns.iter().enumerate() {
            let (port, e) = match c {
                ast::PortConn::Ordered(Some(e)) => (ports.get(pos), PortExpr::Expr(e)),
                ast::PortConn::Named(n, Some(Some(e))) => {
                    (ports.iter().find(|p| p.name == *n), PortExpr::Expr(e))
                }
                ast::PortConn::Named(n, None) => {
                    (ports.iter().find(|p| p.name == *n), PortExpr::Name(n))
                }
                _ => continue,
            };
            if let Some(p) = port
                && p.dir == Dir::Interface
            {
                self.connect_port(child, p, e)?;
            }
        }
        if conns.iter().any(|c| matches!(c, ast::PortConn::Wildcard)) {
            for p in ports.iter().filter(|p| p.dir == Dir::Interface) {
                if !self.scopes[child.0 as usize].syms.contains_key(p.name) {
                    self.connect_port(child, p, PortExpr::Name(p.name))?;
                }
            }
        }
        let _ = at;
        Ok(())
    }

    /// Connect a child's ports to expressions in the parent (the current scope).
    fn connect(
        &mut self,
        child: ScopeId,
        conns: &'t [ast::PortConn<'a>],
        at: &'a str,
    ) -> EResult<()> {
        let ports = self.scopes[child.0 as usize].ports.clone();
        let mut bound: Vec<Option<Option<&'t ast::Expr<'a>>>> = vec![None; ports.len()];
        let mut wildcard = false;
        let mut pos = 0;
        for c in conns {
            match c {
                ast::PortConn::Ordered(e) => {
                    if pos >= ports.len() {
                        if e.is_none() && ports.is_empty() {
                            continue;
                        }
                        return Err(
                            self.error(at, format!("Too many ports connected to instance '{at}'"))
                        );
                    }
                    bound[pos] = Some(e.as_ref());
                    pos += 1;
                }
                ast::PortConn::Named(n, e) => {
                    let Some(i) = ports.iter().position(|p| p.name == *n) else {
                        return Err(self.error(n, format!("Pin not found: '{n}'")));
                    };
                    bound[i] = Some(match e {
                        Some(e) => e.as_ref(),
                        None => None,
                    });
                    if e.is_none() {
                        // `.name`: connect to the same name in the parent.
                        bound[i] = Some(None);
                        self.connect_port(child, &ports[i], PortExpr::Name(n))?;
                        bound[i] = Some(None);
                        continue;
                    }
                }
                ast::PortConn::Wildcard => wildcard = true,
            }
        }
        for (i, p) in ports.iter().enumerate() {
            match bound[i] {
                Some(Some(e)) => self.connect_port(child, p, PortExpr::Expr(e))?,
                Some(None) => {}
                None if wildcard => self.connect_port(child, p, PortExpr::Name(p.name))?,
                None => {}
            }
        }
        Ok(())
    }

    fn connect_port(
        &mut self,
        child: ScopeId,
        p: &PortInfo<'a>,
        e: PortExpr<'a, 't>,
    ) -> EResult<()> {
        let name_expr;
        let e: &ast::Expr<'a> = match e {
            PortExpr::Expr(e) => e,
            PortExpr::Name(n) => {
                name_expr = ast::Expr::Ident(n);
                &name_expr
            }
        };
        if p.dir == Dir::Interface {
            // The port names the interface instance; `ifc.modport` names the
            // instance too (modport restrictions are not checked).
            let s = self.hier_scope(None, e).or_else(|| match e {
                ast::Expr::Member { base, .. } => self.hier_scope(None, base),
                _ => None,
            });
            let Some(s) = s else {
                return Err(self.error(
                    e.at(),
                    format!("Interface port '{}' is not connected to an interface", p.name),
                ));
            };
            self.scopes[child.0 as usize]
                .syms
                .insert(p.name, Sym::Scope(s));
            return Ok(());
        }
        // A plain name of the same width: the port and the net are one variable.
        if let ast::Expr::Ident(n) = e
            && let Some(Sym::Var(pv, pty)) = self.lookup(n)
            && pty.is_integral()
            && p.ty.is_integral()
            && pty.width() == p.ty.width()
        {
            self.scopes[child.0 as usize]
                .syms
                .insert(p.name, Sym::Var(pv, p.ty.clone()));
            return Ok(());
        }
        match p.dir {
            Dir::Input => self.cont_assign_to_var(p.var, &p.ty, e),
            Dir::Output => self.cont_assign_from_var(e, p.var, &p.ty),
            Dir::Inout => Err(self.not_yet(e.at(), "inout ports connected to expressions")),
            Dir::Interface => unreachable!("handled above"),
        }
    }

    // ------------------------------------------------------------ fixups

    /// Fill in implicit sensitivity lists now that every function's reads are known.
    fn apply_fixups(&mut self) {
        let fixups = std::mem::take(&mut self.fixups);
        for f in fixups {
            let mut reads = f.reads;
            let mut todo: Vec<FuncId> = f.calls.into_iter().collect();
            let mut seen = HashSet::new();
            while let Some(func) = todo.pop() {
                if !seen.insert(func) {
                    continue;
                }
                reads.extend(self.d.funcs[func.0 as usize].body.reads.iter().copied());
                todo.extend(self.func_calls.get(&func).into_iter().flatten().copied());
            }
            let term = Terminator::Suspend {
                wait: Wait::AnyChange(reads.into_iter().collect()),
                resume: f.resume,
            };
            self.d.procs[f.proc].body.blocks[f.block.0 as usize].term = term;
        }
    }
}

/// A parameter or typedef declared before other items, in dependency order.
enum Early<'a, 't> {
    Param(
        &'t ast::ParamDecl<'a>,
        &'t ast::ParamAssign<'a>,
        Option<Override<'a>>,
    ),
    Item(&'t ast::ModuleItem<'a>),
}

/// A `-G` value: a number (`10`, `8'hff`, `1.5`) or a quoted string.
fn cmdline_override<'a>(value: &str) -> Result<Override<'a>, String> {
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        let text = decode_string_bytes(&value[1..value.len() - 1]);
        let w = (text.len() as u32 * 8).max(8);
        let mut b = Bits::zero(w);
        for (i, byte) in text.iter().copied().rev().enumerate() {
            b.insert(i as i64 * 8, &Bits::from_u64(8, byte as u64));
        }
        let mut ty = Ty::scalar(Base::Bit { four: true });
        ty.packed = vec![(w as i64 - 1, 0)];
        return Ok(Override::Value(Value::Bits(b), ty));
    }
    match crate::bits::parse_literal(value) {
        Ok(crate::bits::Literal::Bits { bits, signed, .. }) => {
            let mut ty = Ty::scalar(Base::Bit { four: true });
            ty.packed = vec![(bits.width as i64 - 1, 0)];
            ty.signed = signed;
            Ok(Override::Value(Value::Bits(bits), ty))
        }
        Ok(crate::bits::Literal::Real(r)) => {
            Ok(Override::Value(Value::Real(r), Ty::scalar(Base::Real)))
        }
        _ => Err(format!("Unsupported parameter value: '{value}'")),
    }
}

fn module_declares_param(m: &ast::Module<'_>, name: &str) -> bool {
    let in_items = m.items.iter().any(
        |i| matches!(i, ast::ModuleItem::Param(p) if p.assigns.iter().any(|a| a.name == name)),
    );
    in_items
        || m.params
            .iter()
            .flatten()
            .any(|p| p.assigns.iter().any(|a| a.name == name))
}

#[derive(Clone, Debug)]
pub(crate) enum Override<'a> {
    Value(Value, Ty<'a>),
    Type(Ty<'a>),
}

enum PortExpr<'a, 't> {
    Expr(&'t ast::Expr<'a>),
    Name(&'a str),
}

/// Convert a constant to another integral type, as an assignment would.
fn convert_value<'a>(v: &Value, from: &Ty<'a>, to: &Ty<'a>) -> Value {
    match (v, to.is_integral()) {
        (Value::Bits(b), true) => {
            let mut r = b.resize(to.width(), from.signed);
            if !to.four_state() {
                r.to_two_state();
            }
            Value::Bits(r)
        }
        _ => v.clone(),
    }
}

fn net_kind(kw: Option<&str>) -> NetResolution {
    match kw {
        Some("wand" | "triand") => NetResolution::WiredAnd,
        Some("wor" | "trior") => NetResolution::WiredOr,
        Some("tri0") => NetResolution::Tri0,
        Some("tri1") => NetResolution::Tri1,
        Some("supply0") => NetResolution::Supply0,
        Some("supply1") => NetResolution::Supply1,
        _ => NetResolution::Wire,
    }
}

fn collect_instances<'a>(items: &[ast::ModuleItem<'a>], out: &mut HashSet<&'a str>) {
    for item in items {
        match item {
            ast::ModuleItem::Instance(i) => {
                out.insert(i.module);
            }
            ast::ModuleItem::Generate(v) => collect_instances(v, out),
            ast::ModuleItem::GenFor { body, .. } => collect_instances(&body.items, out),
            ast::ModuleItem::GenIf { then, els, .. } => {
                collect_instances(&then.items, out);
                if let Some(e) = els {
                    collect_instances(&e.items, out);
                }
            }
            ast::ModuleItem::GenCase { items, .. } => {
                for (_, b) in items {
                    collect_instances(&b.items, out);
                }
            }
            ast::ModuleItem::GenBlock(b) => collect_instances(&b.items, out),
            _ => {}
        }
    }
}

fn item_at<'a>(i: &ast::Item<'a>) -> &'a str {
    match i {
        ast::Item::Module(m) => m.name,
        ast::Item::Package(p) => p.name,
        ast::Item::Directive(d) => d,
        ast::Item::Decl(_) => "",
    }
}

/// `1ns` → -9, `100ps` → -10.
fn time_exp(s: &str) -> Option<i8> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    let (n, u) = (&s[..split], s[split..].trim());
    let mag = match n {
        "1" => 0,
        "10" => 1,
        "100" => 2,
        _ => return None,
    };
    let unit = match u {
        "s" => 0,
        "ms" => -3,
        "us" => -6,
        "ns" => -9,
        "ps" => -12,
        "fs" => -15,
        _ => return None,
    };
    Some(unit + mag)
}

/// `` `timescale 1ns / 1ps `` → (unit, precision).
fn parse_timescale(d: &str) -> Option<(i8, i8)> {
    let rest = d.strip_prefix("`timescale")?;
    let (u, p) = rest.split_once('/')?;
    let (u, p) = (time_exp(u)?, time_exp(p)?);
    (p <= u).then_some((u, p))
}

fn apply_timeunit(t: &mut (i8, i8), kw: &str, values: &[&str]) {
    let text = values.concat();
    let (first, second) = match text.split_once('/') {
        Some((a, b)) => (a.to_string(), Some(b.to_string())),
        None => (text, None),
    };
    if let Some(e) = time_exp(&first) {
        if kw == "timeunit" { t.0 = e } else { t.1 = e }
    }
    if let Some(p) = second.and_then(|s| time_exp(&s)) {
        t.1 = p;
    }
}

/// Decode the escapes in a string literal's contents.
/// The body of a string literal token: inside `"..."` or `"""..."""`.
pub(crate) fn str_body(s: &str) -> &str {
    if s.len() >= 6 && s.starts_with("\"\"\"") && s.ends_with("\"\"\"") {
        &s[3..s.len() - 3]
    } else {
        &s[1..s.len() - 1]
    }
}

/// The bytes of a string literal's body, with escapes decoded.
pub(crate) fn decode_string_bytes(s: &str) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let push = |out: &mut Vec<u8>, c: char| {
        let mut buf = [0; 4];
        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    };
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            push(&mut out, c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push(b'\n'),
            Some('t') => out.push(b'\t'),
            Some('v') => out.push(b'\x0b'),
            Some('f') => out.push(b'\x0c'),
            Some('a') => out.push(b'\x07'),
            Some('\\') => out.push(b'\\'),
            Some('"') => out.push(b'"'),
            Some('\n') => {}
            Some('x') => {
                let mut v = 0u32;
                for _ in 0..2 {
                    match chars.peek().and_then(|c| c.to_digit(16)) {
                        Some(d) => {
                            v = v * 16 + d;
                            chars.next();
                        }
                        None => break,
                    }
                }
                out.push(v as u8);
            }
            Some(d @ '0'..='7') => {
                let mut v = d.to_digit(8).unwrap();
                for _ in 0..2 {
                    match chars.peek().and_then(|c| c.to_digit(8)) {
                        Some(d) => {
                            v = v * 8 + d;
                            chars.next();
                        }
                        None => break,
                    }
                }
                out.push(v as u8);
            }
            Some(other) => push(&mut out, other),
            None => out.push(b'\\'),
        }
    }
    out
}

/// A string literal's body as text, for messages and format strings.
pub(crate) fn decode_string(s: &str) -> String {
    String::from_utf8_lossy(&decode_string_bytes(s)).into_owned()
}
