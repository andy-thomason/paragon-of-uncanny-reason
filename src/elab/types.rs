//! Elaboration-time types: the declared shape of a value.
//!
//! The IR flattens every packed type to `Bits`, but lowering a select needs
//! the declared ranges (`[7:4]` vs `[0:3]`), the packed and unpacked
//! dimensions, and struct member offsets. [`Ty`] keeps that shape; [`Elab::ir_type`]
//! flattens it.

use super::{EResult, Elab};
use crate::ast;
use crate::ir::{self, Bits, TypeId};
use std::rc::Rc;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Base<'a> {
    /// One bit: 4-state (`logic`, `reg`) or 2-state (`bit`).
    Bit {
        four: bool,
    },
    /// A struct or union. Packed ones are integral.
    Struct {
        fields: Rc<Vec<(&'a str, Ty<'a>)>>,
        packed: bool,
        union: bool,
    },
    Real,
    Str,
    Event,
    Void,
}

/// An unpacked dimension.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum UDim {
    Fixed(i64, i64),
    Dynamic,
    Queue,
    Assoc,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Ty<'a> {
    pub base: Base<'a>,
    pub signed: bool,
    /// Packed dimensions, outermost first, as `(left, right)`.
    pub packed: Vec<(i64, i64)>,
    /// Unpacked dimensions, outermost first.
    pub unpacked: Vec<UDim>,
    /// Value names, for an enum.
    pub names: Option<Rc<Vec<(&'a str, Bits)>>>,
}

pub(crate) fn range_len((l, r): (i64, i64)) -> u32 {
    (l - r).unsigned_abs() as u32 + 1
}

impl<'a> Ty<'a> {
    pub(crate) fn bits(width: u32, signed: bool, four: bool) -> Ty<'a> {
        Ty {
            base: Base::Bit { four },
            signed,
            packed: if width > 1 {
                vec![(width as i64 - 1, 0)]
            } else {
                Vec::new()
            },
            unpacked: Vec::new(),
            names: None,
        }
    }

    pub(crate) fn scalar(base: Base<'a>) -> Ty<'a> {
        Ty {
            base,
            signed: false,
            packed: Vec::new(),
            unpacked: Vec::new(),
            names: None,
        }
    }

    pub(crate) fn is_integral(&self) -> bool {
        self.unpacked.is_empty()
            && matches!(
                self.base,
                Base::Bit { .. } | Base::Struct { packed: true, .. }
            )
    }

    pub(crate) fn is_real(&self) -> bool {
        self.unpacked.is_empty() && self.base == Base::Real
    }

    pub(crate) fn four_state(&self) -> bool {
        match &self.base {
            Base::Bit { four } => *four,
            Base::Struct { fields, .. } => fields.iter().any(|(_, t)| t.four_state()),
            _ => false,
        }
    }

    /// Width of the base element (1 for a bit, the total for a packed struct).
    pub(crate) fn base_width(&self) -> u32 {
        match &self.base {
            Base::Bit { .. } => 1,
            Base::Struct {
                fields,
                union: false,
                ..
            } => fields.iter().map(|(_, t)| t.width()).sum(),
            Base::Struct {
                fields,
                union: true,
                ..
            } => fields.iter().map(|(_, t)| t.width()).max().unwrap_or(1),
            Base::Real => 64,
            _ => 0,
        }
    }

    /// Width of the packed value.
    pub(crate) fn width(&self) -> u32 {
        self.packed.iter().map(|&d| range_len(d)).product::<u32>() * self.base_width()
    }

    /// The type with its unpacked dimensions removed.
    pub(crate) fn element(&self) -> Ty<'a> {
        Ty {
            unpacked: Vec::new(),
            ..self.clone()
        }
    }

    /// The type after indexing the outermost packed dimension.
    pub(crate) fn packed_element(&self) -> Ty<'a> {
        Ty {
            packed: self.packed[1..].to_vec(),
            signed: false,
            names: None,
            ..self.clone()
        }
    }
}

impl<'a, 't> Elab<'a, 't> {
    /// Resolve a syntax-tree type, adding `extra` packed dimensions outermost.
    pub(crate) fn resolve_type(&mut self, dt: &ast::DataType<'a>) -> EResult<Ty<'a>> {
        use ast::DataType as D;
        Ok(match dt {
            D::Implicit { signing, packed } => {
                let mut t = Ty::scalar(Base::Bit { four: true });
                t.signed = *signing == Some("signed");
                t.packed = self.packed_dims(packed)?;
                t
            }
            D::Builtin {
                kw,
                signing,
                packed,
            } => {
                let (mut t, def_signed) = match *kw {
                    "logic" | "reg" => (Ty::scalar(Base::Bit { four: true }), false),
                    "bit" => (Ty::scalar(Base::Bit { four: false }), false),
                    "byte" => (Ty::bits(8, true, false), true),
                    "shortint" => (Ty::bits(16, true, false), true),
                    "int" => (Ty::bits(32, true, false), true),
                    "longint" => (Ty::bits(64, true, false), true),
                    "integer" => (Ty::bits(32, true, true), true),
                    "time" => (Ty::bits(64, false, true), false),
                    "real" | "realtime" | "shortreal" => (Ty::scalar(Base::Real), true),
                    "string" => (Ty::scalar(Base::Str), false),
                    "event" => (Ty::scalar(Base::Event), false),
                    "void" => (Ty::scalar(Base::Void), false),
                    "chandle" => return Err(self.not_yet(kw, "chandle")),
                    _ => return Err(self.not_yet(kw, kw)),
                };
                t.signed = match *signing {
                    Some("signed") => true,
                    Some("unsigned") => false,
                    _ => def_signed,
                };
                let mut dims = self.packed_dims(packed)?;
                dims.extend(t.packed);
                t.packed = dims;
                t
            }
            D::Named {
                scope,
                name,
                params,
                packed,
            } => {
                if params.is_some() {
                    return Err(self.not_yet(name, "parameterised types (classes)"));
                }
                let mut t = match self.lookup_type(*scope, name)? {
                    Some(t) => t,
                    None if matches!(*name, "process" | "semaphore" | "mailbox") => {
                        return Err(
                            self.not_yet(name, "built-in classes (process, semaphore, mailbox)")
                        );
                    }
                    None if *name == "wreal" => return Err(self.not_yet(name, "Verilog-AMS wreal")),
                    None => return Err(self.error(name, format!("Can't find typedef: '{name}'"))),
                };
                let mut dims = self.packed_dims(packed)?;
                dims.extend(t.packed);
                t.packed = dims;
                t
            }
            D::ClassMember { name, .. } => return Err(self.not_yet(name, "class member types")),
            D::IfaceType {
                iface,
                name,
                packed,
            } => {
                let s = self.hier_scope(None, iface).or_else(|| match &**iface {
                    ast::Expr::Ident(n) => self.early_interface(n),
                    _ => None,
                });
                let Some(s) = s else {
                    return Err(self.error(iface.at(), format!("Can't find interface '{}'", iface.at())));
                };
                let Some(super::Sym::Type(mut t)) = self.lookup_child(s, name) else {
                    return Err(self.error(name, format!("Can't find typedef: '{name}'")));
                };
                let mut dims = self.packed_dims(packed)?;
                dims.extend(t.packed);
                t.packed = dims;
                t
            }
            D::Enum {
                kw,
                base,
                items,
                packed,
            } => {
                let base = match base {
                    Some(b) => self.resolve_type(b)?,
                    None => Ty::bits(32, true, false),
                };
                if !base.is_integral() {
                    return Err(self.error(kw, "Enum base type must be integral"));
                }
                let w = base.width();
                let mut names = Vec::new();
                let mut next = Bits::zero(w);
                for item in items {
                    if item.range.is_some() {
                        return Err(self.not_yet(item.name, "enum name ranges"));
                    }
                    if let Some(v) = &item.value {
                        next = self.const_bits(v, &base)?;
                    }
                    names.push((item.name, next.clone()));
                    self.declare_enum_value(item.name, next.clone(), &base);
                    next = next.add(&Bits::from_u64(w, 1));
                }
                let mut t = base;
                t.names = Some(Rc::new(names));
                let mut dims = self.packed_dims(packed)?;
                dims.extend(t.packed);
                t.packed = dims;
                t
            }
            D::Struct {
                kw,
                packed,
                signing,
                members,
                dims,
            } => {
                let mut fields = Vec::new();
                for m in members {
                    let ty = self.resolve_type(&m.ty)?;
                    if *packed && !ty.is_integral() {
                        return Err(self.error(kw, "Packed struct members must be integral"));
                    }
                    for v in &m.vars {
                        let mut t = ty.clone();
                        t.unpacked = self.unpacked_dims(&v.dims)?;
                        fields.push((v.name, t));
                    }
                }
                // An unpacked struct of integral members is laid out like a
                // packed one: members, patterns and copies behave the same.
                if !packed && !fields.iter().all(|(_, t)| t.is_integral()) {
                    return Err(self.not_yet(kw, "unpacked structs with non-integral members"));
                }
                let mut t = Ty::scalar(Base::Struct {
                    fields: Rc::new(fields),
                    packed: true,
                    union: *kw == "union",
                });
                t.signed = *signing == Some("signed");
                t.packed = self.packed_dims(dims)?;
                t
            }
            D::TypeOf(e) => match &**e {
                ast::Expr::Type(t) => self.resolve_type(t)?,
                other => {
                    let st = self.self_type(other)?;
                    st.ty()
                }
            },
        })
    }

    /// Packed dimensions; each must be a constant `[l:r]` or `[n]`.
    pub(crate) fn packed_dims(&mut self, dims: &[ast::Dim<'a>]) -> EResult<Vec<(i64, i64)>> {
        let mut v = Vec::new();
        for d in dims {
            match d {
                ast::Dim::Range(l, r) => v.push((self.const_int(l)?, self.const_int(r)?)),
                ast::Dim::Size(n) => {
                    let n = self.const_int(n)?;
                    v.push((n - 1, 0));
                }
                _ => {
                    return Err(
                        self.error(self.here(), "Packed dimension must be a constant range")
                    );
                }
            }
        }
        Ok(v)
    }

    pub(crate) fn unpacked_dims(&mut self, dims: &[ast::Dim<'a>]) -> EResult<Vec<UDim>> {
        let mut v = Vec::new();
        for d in dims {
            v.push(match d {
                ast::Dim::Range(l, r) => UDim::Fixed(self.const_int(l)?, self.const_int(r)?),
                ast::Dim::Size(n) => UDim::Fixed(0, self.const_int(n)? - 1),
                ast::Dim::Dynamic => UDim::Dynamic,
                ast::Dim::Queue(_) => UDim::Queue,
                ast::Dim::Assoc(_) => UDim::Assoc,
            });
        }
        Ok(v)
    }

    /// The flattened IR type.
    pub(crate) fn ir_type(&mut self, t: &Ty<'a>) -> TypeId {
        if !t.unpacked.is_empty() {
            let elem = self.ir_type(&t.element());
            let n: u32 = t
                .unpacked
                .iter()
                .map(|d| match d {
                    UDim::Fixed(l, r) => range_len((*l, *r)),
                    _ => 1,
                })
                .product();
            let ty = match t.unpacked[0] {
                UDim::Fixed(..) => ir::Type::Unpacked {
                    elem,
                    left: 0,
                    right: n as i64 - 1,
                },
                UDim::Dynamic => ir::Type::Dynamic { elem },
                UDim::Queue => ir::Type::Queue { elem, max: None },
                UDim::Assoc => ir::Type::Assoc { elem, key: None },
            };
            return self.add_type(ty);
        }
        match &t.base {
            Base::Bit { .. } | Base::Struct { packed: true, .. } => {
                if t.names.is_none() && !matches!(t.base, Base::Struct { .. }) {
                    return self.bits_type(t.width(), t.signed, t.four_state());
                }
                let fields = match &t.base {
                    Base::Struct { fields, union, .. } => {
                        let mut off = if *union { 0 } else { t.base_width() };
                        let mut v = Vec::new();
                        for (name, ft) in fields.iter() {
                            let w = ft.width();
                            if !*union {
                                off -= w;
                            }
                            let ty = self.ir_type(ft);
                            v.push(ir::Field {
                                name,
                                ty,
                                offset: off,
                            });
                        }
                        Some(v)
                    }
                    _ => None,
                };
                let names = t.names.as_ref().map(|n| n.to_vec());
                self.add_type(ir::Type::Bits {
                    width: t.width(),
                    signed: t.signed,
                    four_state: t.four_state(),
                    fields,
                    names,
                })
            }
            Base::Real => self.add_type(ir::Type::Real),
            Base::Str => self.add_type(ir::Type::String),
            Base::Event => self.add_type(ir::Type::Event),
            Base::Void | Base::Struct { .. } => {
                self.add_type(ir::Type::Struct { fields: Vec::new() })
            }
        }
    }

    pub(crate) fn add_type(&mut self, t: ir::Type<'a>) -> TypeId {
        if let Some(i) = self.d.types.iter().position(|x| *x == t) {
            return TypeId(i as u32);
        }
        self.d.types.push(t);
        TypeId(self.d.types.len() as u32 - 1)
    }

    /// The interned IR type for a plain bit vector.
    pub(crate) fn bits_type(&mut self, width: u32, signed: bool, four: bool) -> TypeId {
        if let Some(t) = self.bits_cache.get(&(width, signed, four)) {
            return *t;
        }
        let t = self.add_type(ir::Type::Bits {
            width,
            signed,
            four_state: four,
            fields: None,
            names: None,
        });
        self.bits_cache.insert((width, signed, four), t);
        t
    }
}
