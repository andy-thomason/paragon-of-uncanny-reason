//! Classes (IEEE 1800-2023 §8): declarations, specialisations, objects and
//! methods.
//!
//! A class declaration is kept as written ([`ClassDef`]) and becomes a class
//! ([`ClassInfo`], an [`ir::Class`](crate::ir::Class)) for each set of
//! parameter values it is used with. A class has its own scope: parameters,
//! typedefs, static properties (design variables) and methods live there, and
//! each property is a [`Sym::Field`] naming its place in the object. Lookup in
//! a class scope searches the base classes before the enclosing scope.
//!
//! Classes are completed lazily, on first use, because their members may
//! name classes declared later (`typedef class`).
//!
//! A method is a function whose first argument is `this`. A virtual method
//! has a slot in the class's vtable; calls through a handle to it are
//! [`Op::VCall`]s. Every class has a constructor: the declared `new`, or an
//! empty one. A constructor first calls the base class's (unless its first
//! statement does), then runs the property initialisers, then its body.

use super::expr::{Cx, STy};
use super::types::{Base, Ty};
use super::{EResult, Elab, Override, Sym};
use crate::ast::{self, Arg, Expr};
use crate::ir::*;
use std::collections::HashMap;

pub(crate) struct ClassDef<'a, 't> {
    pub ast: &'t ast::ClassDecl<'a>,
    /// Where it is declared.
    pub scope: ScopeId,
    /// Its specialisations, by parameter values.
    pub specs: Vec<(String, ClassId)>,
}

#[derive(Clone, Debug)]
pub(crate) struct Method {
    pub func: FuncId,
    pub is_static: bool,
    /// The vtable slot of a virtual method.
    pub slot: Option<u32>,
}

impl ClassInfo<'_, '_> {
    /// The handle type of a class.
    pub(crate) fn ty<'a>(c: ClassId) -> Ty<'a> {
        Ty::scalar(Base::Class(c))
    }
}

pub(crate) struct ClassInfo<'a, 't> {
    pub def: usize,
    /// The class's own scope.
    pub scope: ScopeId,
    pub overrides: Vec<(Option<&'a str>, Override<'a>)>,
    pub started: bool,
    pub base: Option<ClassId>,
    /// Every property, inherited ones first.
    pub fields: Vec<(&'a str, Ty<'a>)>,
    /// Properties of this class with initialisers.
    pub inits: Vec<(u32, &'t Expr<'a>)>,
    /// Methods by name, inherited ones included.
    pub methods: HashMap<&'a str, Method>,
    pub ctor: Option<FuncId>,
    pub is_virtual: bool,
}

impl<'a, 't> Elab<'a, 't> {
    /// A class declaration in the current scope.
    pub(crate) fn declare_class(&mut self, c: &'t ast::ClassDecl<'a>) {
        let i = self.class_defs.len();
        self.class_defs.push(ClassDef {
            ast: c,
            scope: self.cur,
            specs: Vec::new(),
        });
        self.declare(c.name, Sym::ClassDef(i));
    }

    /// The class for declaration `def` with parameter values `params`
    /// (evaluated in the current scope).
    pub(crate) fn specialise(
        &mut self,
        def: usize,
        params: Option<&[ast::ParamArg<'a>]>,
        at: &'a str,
    ) -> EResult<ClassId> {
        let mut overrides = Vec::new();
        for p in params.into_iter().flatten() {
            let (name, e) = match p {
                ast::ParamArg::Ordered(e) => (None, Some(e)),
                ast::ParamArg::Named(n, e) => (Some(*n), e.as_ref()),
            };
            let Some(e) = e else { continue };
            overrides.push((name, self.override_value(e)?));
        }
        let key = format!("{overrides:?}");
        if let Some((_, c)) = self.class_defs[def].specs.iter().find(|(k, _)| *k == key) {
            return Ok(*c);
        }
        let ast = self.class_defs[def].ast;
        let parent = self.class_defs[def].scope;
        let unit = self.d.scopes[parent.0 as usize].unit;
        let scope = self.new_scope(ast.name, Some(parent), None, Some(parent), unit, &[]);
        let id = ClassId(self.d.classes.len() as u32);
        self.d.classes.push(Class {
            name: ast.name,
            base: None,
            fields: Vec::new(),
            vtable: Vec::new(),
        });
        self.classes.push(ClassInfo {
            def,
            scope,
            overrides,
            started: false,
            base: None,
            fields: Vec::new(),
            inits: Vec::new(),
            methods: HashMap::new(),
            ctor: None,
            is_virtual: ast.kind == Some("virtual"),
        });
        self.class_defs[def].specs.push((key, id));
        let _ = at;
        Ok(id)
    }

    /// Fill in a class's members, if not done yet.
    pub(crate) fn ensure_class(&mut self, c: ClassId) -> EResult<()> {
        if self.classes[c.0 as usize].started {
            return Ok(());
        }
        self.classes[c.0 as usize].started = true;
        let scope = self.classes[c.0 as usize].scope;
        let saved = std::mem::replace(&mut self.cur, scope);
        let r = self.complete_class(c);
        self.cur = saved;
        r
    }

    fn complete_class(&mut self, c: ClassId) -> EResult<()> {
        let ci = c.0 as usize;
        let def = self.classes[ci].def;
        let ast = self.class_defs[def].ast;
        let scope = self.classes[ci].scope;
        // Parameters, with the specialisation's values.
        let overrides = self.classes[ci].overrides.clone();
        let mut pos = 0;
        for p in ast.params.iter().flatten() {
            for a in &p.assigns {
                let ov = overrides
                    .iter()
                    .find(|(n, _)| *n == Some(a.name))
                    .or_else(|| {
                        overrides
                            .iter()
                            .filter(|(n, _)| n.is_none())
                            .nth(pos)
                    })
                    .map(|(_, o)| o);
                pos += 1;
                self.declare_param(p, a, ov)?;
            }
        }
        // The base class.
        if let Some((bt, _)) = &ast.extends {
            let t = self.resolve_type(bt)?;
            let Base::Class(b) = t.base else {
                return Err(self.error(ast.name, "A class can only extend a class"));
            };
            self.ensure_class(b)?;
            let base = &self.classes[b.0 as usize];
            let (fields, methods, bscope) = (base.fields.clone(), base.methods.clone(), base.scope);
            let vtable = self.d.classes[b.0 as usize].vtable.clone();
            let info = &mut self.classes[ci];
            info.base = Some(b);
            info.fields = fields;
            info.methods = methods;
            self.d.classes[ci].base = Some(b);
            self.d.classes[ci].vtable = vtable;
            self.scopes[scope.0 as usize].base_scope = Some(bscope);
        }
        // Declarations first, then methods, which may use them.
        let mut methods = Vec::new();
        for item in &ast.items {
            let ast::ClassMember::Item(it) = &item.item else {
                continue;
            };
            let is_static = item.quals.contains(&"static");
            match &**it {
                ast::ModuleItem::Var(v) if !is_static => {
                    let base = self.resolve_type(&v.ty)?;
                    for d in &v.vars {
                        let mut ty = base.clone();
                        ty.unpacked = self.unpacked_dims(&d.dims)?;
                        let idx = self.classes[ci].fields.len() as u32;
                        self.classes[ci].fields.push((d.name, ty.clone()));
                        self.declare(d.name, Sym::Field(idx, ty));
                        if let Some(e) = &d.init {
                            self.classes[ci].inits.push((idx, e));
                        }
                    }
                }
                ast::ModuleItem::Function(f) | ast::ModuleItem::Task(f) => {
                    methods.push((item, f));
                }
                other => self.declare_item(other)?,
            }
        }
        for (item, f) in methods {
            let is_static = item.quals.contains(&"static");
            let pure = item.quals.contains(&"pure");
            // An extern prototype's body is defined outside the class.
            let src = if f.proto && !pure {
                match self.out_of_class.get(&(ast.name, f.name)) {
                    Some(d) => *d,
                    None => {
                        return Err(self.error(
                            f.name,
                            format!("Definition not found for extern method '{}'", f.name),
                        ));
                    }
                }
            } else {
                f
            };
            let id = self.new_method(scope, src, c, is_static, !pure);
            let inherited = self.classes[ci].methods.get(f.name).cloned();
            let slot = if f.name == "new" || is_static {
                None
            } else {
                match inherited.and_then(|m| m.slot) {
                    Some(s) => Some(s),
                    None if item.quals.contains(&"virtual") || pure => {
                        Some(self.d.classes[ci].vtable.len() as u32)
                    }
                    None => None,
                }
            };
            if let Some(s) = slot {
                let vt = &mut self.d.classes[ci].vtable;
                if s as usize == vt.len() {
                    vt.push(id);
                } else if !pure {
                    vt[s as usize] = id;
                }
            }
            if f.name == "new" {
                self.classes[ci].ctor = Some(id);
            } else {
                self.classes[ci].methods.insert(
                    f.name,
                    Method {
                        func: id,
                        is_static,
                        slot,
                    },
                );
            }
        }
        if self.classes[ci].ctor.is_none() {
            // An empty constructor, which still calls the base one and runs
            // the initialisers.
            let sub = self.made_subs.alloc(ast::Subroutine {
                kw: "function",
                class: None,
                proto: false,
                lifetime: None,
                ret: None,
                name: "new",
                ports: Some(Vec::new()),
                decls: Vec::new(),
                stmts: Vec::new(),
                end: ast.name,
            });
            let id = self.new_method(scope, sub, c, false, true);
            self.classes[ci].ctor = Some(id);
        }
        let fields: Vec<(&'a str, TypeId)> = self.classes[ci]
            .fields
            .clone()
            .iter()
            .map(|(n, t)| (*n, self.ir_type(t)))
            .collect();
        self.d.classes[ci].fields = fields;
        Ok(())
    }

    /// A method's function, lowered with the class scope's other code.
    fn new_method(
        &mut self,
        scope: ScopeId,
        f: &'t ast::Subroutine<'a>,
        c: ClassId,
        is_static: bool,
        lower: bool,
    ) -> FuncId {
        let id = FuncId(self.d.funcs.len() as u32);
        self.d.funcs.push(Func {
            name: f.name,
            scope,
            params: Vec::new(),
            ret: None,
            is_task: f.kw == "task",
            body: Body::default(),
            at: f.name,
        });
        self.func_src.insert(id, f);
        self.func_class.insert(id, (c, is_static));
        if f.name != "new" {
            self.declare(f.name, Sym::Func(id));
        }
        if lower {
            self.func_defs.insert(id, f);
            self.info(scope).funcs.push((id, f));
        }
        id
    }

    /// The built-in `process` class (LRM 9.7), made on first use.
    pub(crate) fn process_class(&mut self) -> ClassId {
        if let Some(c) = self.d.process_class {
            return c;
        }
        let unit = self.unit_scope;
        let scope = self.new_scope("process", Some(unit), None, Some(unit), -12, &[]);
        let int = Ty::bits(32, true, false);
        for (i, n) in ["FINISHED", "RUNNING", "WAITING", "SUSPENDED", "KILLED"]
            .iter()
            .enumerate()
        {
            self.scopes[scope.0 as usize].syms.insert(
                n,
                Sym::Param(crate::eval::Value::Bits(Bits::from_u64(32, i as u64)), int.clone()),
            );
        }
        let id = ClassId(self.d.classes.len() as u32);
        let it = self.ir_type(&int);
        self.d.classes.push(Class {
            name: "process",
            base: None,
            fields: vec![("id", it)],
            vtable: Vec::new(),
        });
        self.classes.push(ClassInfo {
            def: usize::MAX,
            scope,
            overrides: Vec::new(),
            started: true,
            base: None,
            fields: vec![("id", int)],
            inits: Vec::new(),
            methods: HashMap::new(),
            ctor: None,
            is_virtual: true,
        });
        self.d.process_class = Some(id);
        id
    }

    /// A method of the built-in `process` class.
    fn process_method(
        &mut self,
        cx: &mut Cx<'a>,
        obj: Option<Val>,
        name: &'a str,
    ) -> EResult<Option<(Val, STy)>> {
        let int = STy::Bits {
            w: 32,
            s: true,
            f: false,
        };
        let c = self.process_class();
        if name == "self" {
            let t = self.ir_type(&Self::class_ty(c));
            let v = cx.b.emit(
                Op::Process {
                    func: ProcFunc::SelfHandle,
                    args: vec![],
                },
                t,
                name,
            );
            return Ok(Some((v, STy::Class(Some(c)))));
        }
        let Some(h) = obj else {
            return Err(self.error(name, format!("process::{name} needs a process handle")));
        };
        Ok(match name {
            "status" => {
                let t = self.bits_type(32, true, false);
                let v = cx.b.emit(
                    Op::Process {
                        func: ProcFunc::Status,
                        args: vec![h],
                    },
                    t,
                    name,
                );
                Some((v, int))
            }
            "kill" => {
                cx.b.effect(
                    Op::Process {
                        func: ProcFunc::Kill,
                        args: vec![h],
                    },
                    name,
                );
                None
            }
            "await" => {
                let resume = cx.b.new_block();
                cx.b.terminate(Terminator::Suspend {
                    wait: Wait::Process(h),
                    resume,
                });
                cx.b.switch_to(resume);
                None
            }
            "srandom" | "set_randstate" => {
                cx.b.effect(
                    Op::Process {
                        func: ProcFunc::Ignore,
                        args: vec![h],
                    },
                    name,
                );
                None
            }
            "get_randstate" => {
                let t = self.add_type(Type::String);
                let v = cx.b.emit(
                    Op::Process {
                        func: ProcFunc::GetRandstate,
                        args: vec![h],
                    },
                    t,
                    name,
                );
                Some((v, STy::Str))
            }
            _ => return Err(self.not_yet(name, &format!("process::{name}"))),
        })
    }

    /// The class of a handle type.
    pub(crate) fn class_of(t: &Ty<'a>) -> Option<ClassId> {
        match t.base {
            Base::Class(c) if t.unpacked.is_empty() => Some(c),
            _ => None,
        }
    }

    /// A member of class `c` (or its bases): property, static, method...
    pub(crate) fn class_member(&mut self, c: ClassId, name: &str) -> EResult<Option<Sym<'a>>> {
        self.ensure_class(c)?;
        let scope = self.classes[c.0 as usize].scope;
        Ok(self.lookup_with_bases(scope, name))
    }

    /// The handle type of a class.
    pub(crate) fn class_ty(c: ClassId) -> Ty<'a> {
        ClassInfo::ty(c)
    }

    /// `new(args)` for class `c`: a new object, constructed.
    pub(crate) fn lower_new(
        &mut self,
        cx: &mut Cx<'a>,
        c: ClassId,
        args: &[Arg<'a>],
        at: &'a str,
    ) -> EResult<Val> {
        self.ensure_class(c)?;
        if self.classes[c.0 as usize].is_virtual {
            return Err(self.error(
                at,
                format!(
                    "Illegal to call 'new' using an abstract virtual class '{}'",
                    self.d.classes[c.0 as usize].name
                ),
            ));
        }
        let ht = self.ir_type(&Self::class_ty(c));
        let h = cx.b.emit(Op::New(c), ht, at);
        let ctor = self.classes[c.0 as usize].ctor.unwrap();
        self.emit_call_on(cx, ctor, Some(h), None, args, at)?;
        Ok(h)
    }

    /// A method call `obj.name(args)` on a handle of class `c` (`obj` is
    /// `None` for a static call). `direct` calls this class's
    /// implementation even if the method is virtual (as `super.f()` does).
    pub(crate) fn method_call(
        &mut self,
        cx: &mut Cx<'a>,
        c: ClassId,
        obj: Option<Val>,
        name: &'a str,
        args: &[Arg<'a>],
        direct: bool,
    ) -> EResult<Option<(Val, STy)>> {
        if Some(c) == self.d.process_class {
            let _ = args;
            return self.process_method(cx, obj, name);
        }
        self.ensure_class(c)?;
        let m = if name == "new" {
            self.classes[c.0 as usize].ctor.map(|func| Method {
                func,
                is_static: false,
                slot: None,
            })
        } else {
            self.classes[c.0 as usize].methods.get(name).cloned()
        };
        let Some(m) = m else {
            if matches!(name, "randomize" | "srandom" | "get_randstate" | "set_randstate") {
                return Err(self.not_yet(name, "randomization"));
            }
            return Err(self.error(
                name,
                format!(
                    "Class method '{name}' not found in class '{}'",
                    self.d.classes[c.0 as usize].name
                ),
            ));
        };
        let this = if m.is_static {
            None
        } else {
            match obj {
                Some(o) => Some(o),
                None => Some(self.this_handle(cx, name)?),
            }
        };
        let slot = if direct { None } else { m.slot };
        self.emit_call_on(cx, m.func, this, slot, args, name)
    }

    /// The `this` of the method being lowered.
    pub(crate) fn this_handle(&mut self, cx: &mut Cx<'a>, at: &'a str) -> EResult<Val> {
        match cx.locals.iter().rev().find_map(|l| l.get("this").cloned()) {
            Some(Sym::Slot(s, t)) => {
                let it = self.ir_type(&t);
                Ok(cx.b.emit(Op::LoadSlot(s), it, at))
            }
            _ => Err(self.error(at, "'this' used outside a non-static method")),
        }
    }

    /// The class whose method is being lowered.
    pub(crate) fn this_class(&self, cx: &Cx<'a>) -> Option<ClassId> {
        match cx.locals.iter().rev().find_map(|l| l.get("this")) {
            Some(Sym::Slot(_, t)) => Self::class_of(t),
            _ => None,
        }
    }

    /// The constructor's prologue: the base constructor (unless the body
    /// starts with `super.new`), then the property initialisers.
    pub(crate) fn ctor_prologue(
        &mut self,
        cx: &mut Cx<'a>,
        c: ClassId,
        explicit_super: bool,
        at: &'a str,
    ) -> EResult<()> {
        let ci = c.0 as usize;
        if !explicit_super && let Some(b) = self.classes[ci].base {
            let def = self.classes[ci].def;
            let args: &'t [Arg<'a>] = match &self.class_defs[def].ast.extends {
                Some((_, a)) => a,
                None => &[],
            };
            let this = self.this_handle(cx, at)?;
            let ctor = {
                self.ensure_class(b)?;
                self.classes[b.0 as usize].ctor.unwrap()
            };
            self.emit_call_on(cx, ctor, Some(this), None, args, at)?;
        }
        self.field_inits(cx, c, at)
    }

    /// The property initialisers of class `c`.
    pub(crate) fn field_inits(&mut self, cx: &mut Cx<'a>, c: ClassId, at: &'a str) -> EResult<()> {
        let inits = self.classes[c.0 as usize].inits.clone();
        for (idx, e) in inits {
            let ty = self.classes[c.0 as usize].fields[idx as usize].1.clone();
            let v = self.lower_to(cx, e, &ty)?;
            let this = self.this_handle(cx, at)?;
            let op = Op::StoreField {
                obj: this,
                field: idx,
                part: None,
                value: v,
            };
            cx.b.effect(op, at);
        }
        Ok(())
    }

    /// A handle-valued expression; `target` is the class a `new` makes.
    pub(crate) fn lower_handle(
        &mut self,
        cx: &mut Cx<'a>,
        e: &Expr<'a>,
        target: Option<ClassId>,
    ) -> EResult<Val> {
        let at = e.at();
        match e {
            Expr::Keyword("null") => {
                let t = self.add_type(Type::Null);
                Ok(cx.b.emit(Op::Null, t, at))
            }
            Expr::New {
                copy: Some(src), ..
            } => {
                let v = self.lower_handle(cx, src, None)?;
                let t = cx.b.val_type(v);
                Ok(cx.b.emit(Op::CopyObj(v), t, at))
            }
            Expr::New { args, size: None, .. } => {
                let Some(c) = target else {
                    return Err(self.error(at, "'new' needs a class handle to assign to"));
                };
                self.lower_new(cx, c, args, at)
            }
            Expr::Cond {
                cond, then, els, ..
            } => {
                let c = self.truth(cx, cond)?;
                let a = self.lower_handle(cx, then, target)?;
                let b = self.lower_handle(cx, els, target)?;
                let t = match target {
                    Some(c) => self.ir_type(&Self::class_ty(c)),
                    None => cx.b.val_type(a),
                };
                Ok(cx.b.emit(
                    Op::Mux {
                        cond: c,
                        then: a,
                        els: b,
                    },
                    t,
                    at,
                ))
            }
            Expr::Call { func, args } => Ok(self.lower_call(cx, func, args)?.0),
            Expr::SysCall { name, args } => Ok(self.lower_sys(cx, name, args)?.0),
            Expr::Cast { expr, .. } => self.lower_handle(cx, expr, target),
            _ => {
                let Some(p) = self.path(cx, e)? else {
                    return Err(self.not_yet(at, "this class handle expression"));
                };
                Ok(self.load_path(cx, &p)?.0)
            }
        }
    }

    /// `$cast(dst, src)` for class handles: store and give 1 if `src` is of
    /// `dst`'s class, else give 0.
    pub(crate) fn lower_cast(
        &mut self,
        cx: &mut Cx<'a>,
        dst: &Expr<'a>,
        src: &Expr<'a>,
        at: &'a str,
    ) -> EResult<Val> {
        let Some(p) = self.path(cx, dst)? else {
            return Err(self.error(dst.at(), "$cast needs a variable to assign to"));
        };
        let bit = self.bits_type(1, false, false);
        let Some(c) = Self::class_of(&p.ty) else {
            // An ordinary (enum or integral) cast always succeeds here.
            self.assign(cx, dst, src, false)?;
            return Ok(cx.b.emit(Op::Const(Bits::ones(1)), bit, at));
        };
        let v = self.lower_handle(cx, src, None)?;
        let ok = cx.b.emit(Op::IsA { value: v, class: c }, bit, at);
        // A null source casts successfully (to null).
        let nt = self.add_type(Type::Null);
        let null = cx.b.emit(Op::Null, nt, at);
        let is_null = cx.b.emit(Op::Binary(BinOp::Eq, v, null), bit, at);
        let ok = cx.b.emit(Op::Binary(BinOp::Or, ok, is_null), bit, at);
        let (yes, join) = (cx.b.new_block(), cx.b.new_block());
        cx.b.terminate(Terminator::Branch {
            cond: ok,
            then: (yes, vec![]),
            els: (join, vec![]),
        });
        cx.b.switch_to(yes);
        self.store_path(cx, &p, v, false)?;
        cx.b.terminate(Terminator::Jump(join, vec![]));
        cx.b.switch_to(join);
        Ok(ok)
    }
}
