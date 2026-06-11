//! Type representation and unification.
//!
//! Inga's value types are inferred with plain unification (whole-program,
//! monomorphic user functions; builtins are instantiated fresh per use).
//! Effect rows — the error row (`!`) and capability row (`uses`) — are *not*
//! part of unification; they are finite sets of names computed by a fixpoint
//! in `check.rs`.

use std::collections::BTreeSet;
use std::rc::Rc;

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Int,
    Float,
    Bool,
    Str,
    Unit,
    Duration,
    Schedule,
    /// `T?`
    Option(Box<Type>),
    /// `[T]`
    List(Box<Type>),
    /// `(T, U)`
    Tuple(Vec<Type>),
    /// A running task; `await` yields the payload.
    Task(Box<Type>),
    /// A `struct` declaration (nominal record).
    Named(String),
    /// An `enum` declaration (nominal sum type).
    Enum(String),
    /// A capability bound by `Cache cache`.
    Service(String),
    /// A type name used as a value, e.g. `decode(raw, User)`.
    Tag(String),
    /// Built-in mutable map (impl instance state).
    MutMap(Box<Type>, Box<Type>),
    Func(Rc<FuncType>),
    Var(u32),
    /// Error-recovery type: unifies with anything, reports nothing.
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FuncType {
    pub params: Vec<Type>,
    pub ret: Type,
    pub errors: BTreeSet<String>,
    pub caps: BTreeSet<String>,
}

/// Unification context: bindings for type variables.
#[derive(Default)]
pub struct TypeCtx {
    bindings: Vec<Option<Type>>,
}

impl TypeCtx {
    pub fn fresh(&mut self) -> Type {
        self.bindings.push(None);
        Type::Var(self.bindings.len() as u32 - 1)
    }

    /// Follow variable bindings one level.
    pub fn resolve(&self, ty: &Type) -> Type {
        let mut ty = ty.clone();
        while let Type::Var(v) = ty {
            match &self.bindings[v as usize] {
                Some(bound) => ty = bound.clone(),
                None => break,
            }
        }
        ty
    }

    /// Deeply resolve a type for display.
    pub fn apply(&self, ty: &Type) -> Type {
        let ty = self.resolve(ty);
        match ty {
            Type::Option(t) => Type::Option(Box::new(self.apply(&t))),
            Type::List(t) => Type::List(Box::new(self.apply(&t))),
            Type::Tuple(ts) => Type::Tuple(ts.iter().map(|t| self.apply(t)).collect()),
            Type::Task(t) => Type::Task(Box::new(self.apply(&t))),
            Type::MutMap(k, v) => {
                Type::MutMap(Box::new(self.apply(&k)), Box::new(self.apply(&v)))
            }
            Type::Func(f) => Type::Func(Rc::new(FuncType {
                params: f.params.iter().map(|p| self.apply(p)).collect(),
                ret: self.apply(&f.ret),
                errors: f.errors.clone(),
                caps: f.caps.clone(),
            })),
            other => other,
        }
    }

    fn occurs(&self, var: u32, ty: &Type) -> bool {
        match self.resolve(ty) {
            Type::Var(v) => v == var,
            Type::Option(t) | Type::List(t) | Type::Task(t) => self.occurs(var, &t),
            Type::Tuple(ts) => ts.iter().any(|t| self.occurs(var, t)),
            Type::MutMap(k, v) => self.occurs(var, &k) || self.occurs(var, &v),
            Type::Func(f) => {
                f.params.iter().any(|p| self.occurs(var, p)) || self.occurs(var, &f.ret)
            }
            _ => false,
        }
    }

    /// Unify two types. On mismatch returns the (resolved) conflicting pair.
    pub fn unify(&mut self, a: &Type, b: &Type) -> Result<(), (Type, Type)> {
        let a = self.resolve(a);
        let b = self.resolve(b);
        match (&a, &b) {
            (Type::Unknown, _) | (_, Type::Unknown) => Ok(()),
            (Type::Var(v), _) => {
                if let Type::Var(w) = b {
                    if w == *v {
                        return Ok(());
                    }
                }
                if self.occurs(*v, &b) {
                    return Err((a.clone(), b.clone()));
                }
                self.bindings[*v as usize] = Some(b.clone());
                Ok(())
            }
            (_, Type::Var(_)) => self.unify(&b, &a),
            (Type::Option(x), Type::Option(y))
            | (Type::List(x), Type::List(y))
            | (Type::Task(x), Type::Task(y)) => self.unify(x, y),
            (Type::Tuple(xs), Type::Tuple(ys)) => {
                if xs.len() != ys.len() {
                    return Err((a.clone(), b.clone()));
                }
                for (x, y) in xs.iter().zip(ys.iter()) {
                    self.unify(x, y)?;
                }
                Ok(())
            }
            (Type::MutMap(k1, v1), Type::MutMap(k2, v2)) => {
                self.unify(k1, k2)?;
                self.unify(v1, v2)
            }
            (Type::Func(f), Type::Func(g)) => {
                if f.params.len() != g.params.len() {
                    return Err((a.clone(), b.clone()));
                }
                for (p, q) in f.params.iter().zip(g.params.iter()) {
                    self.unify(p, q)?;
                }
                self.unify(&f.ret, &g.ret)
            }
            _ if a == b => Ok(()),
            _ => Err((a, b)),
        }
    }

    /// Render a type for diagnostics/hover. Unbound variables get stable
    /// lowercase names scoped to `names`.
    pub fn render(&self, ty: &Type, names: &mut Vec<u32>) -> String {
        match self.resolve(ty) {
            Type::Int => "Int".into(),
            Type::Float => "Float".into(),
            Type::Bool => "Bool".into(),
            Type::Str => "String".into(),
            Type::Unit => "Unit".into(),
            Type::Duration => "Duration".into(),
            Type::Schedule => "Schedule".into(),
            Type::Option(t) => {
                let inner = self.render(&t, names);
                if matches!(self.resolve(&t), Type::Func(_)) {
                    format!("({inner})?")
                } else {
                    format!("{inner}?")
                }
            }
            Type::List(t) => format!("[{}]", self.render(&t, names)),
            Type::Task(t) => format!("Task<{}>", self.render(&t, names)),
            Type::Tuple(ts) => {
                let inner: Vec<String> = ts.iter().map(|t| self.render(t, names)).collect();
                format!("({})", inner.join(", "))
            }
            Type::Named(n) | Type::Enum(n) | Type::Service(n) => n,
            Type::Tag(n) => format!("Type<{n}>"),
            Type::MutMap(k, v) => {
                format!("MutMap<{}, {}>", self.render(&k, names), self.render(&v, names))
            }
            Type::Func(f) => {
                let params: Vec<String> = f.params.iter().map(|p| self.render(p, names)).collect();
                let mut out = format!("({}) -> {}", params.join(", "), self.render(&f.ret, names));
                if !f.errors.is_empty() {
                    out.push_str(" ! ");
                    out.push_str(&f.errors.iter().cloned().collect::<Vec<_>>().join(", "));
                }
                if !f.caps.is_empty() {
                    out.push_str(" uses ");
                    out.push_str(&f.caps.iter().cloned().collect::<Vec<_>>().join(", "));
                }
                out
            }
            Type::Var(v) => {
                let idx = match names.iter().position(|&n| n == v) {
                    Some(i) => i,
                    None => {
                        names.push(v);
                        names.len() - 1
                    }
                };
                let letter = (b'a' + (idx % 26) as u8) as char;
                if idx < 26 {
                    letter.to_string()
                } else {
                    format!("{letter}{}", idx / 26)
                }
            }
            Type::Unknown => "?".into(),
        }
    }
}
