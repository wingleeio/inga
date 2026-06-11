//! Tree-walking interpreter.
//!
//! Values borrow from the AST (`Value<'a>` holds `&'a` references to function
//! declarations and lambda bodies). Failures — `fail`, `decode` errors — are
//! the `Err` side of `EvalResult` and propagate like exceptions until a
//! `catch` (or `retry`/`ignoreFailure`) intercepts them. Capabilities are a
//! dynamically scoped stack of provided service instances.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::rc::Rc;

use crate::ast::*;
use crate::check::{DECODE_ERROR, DURATION_SUFFIXES, SIZE_SUFFIXES};
use crate::span::Span;

#[derive(Debug, Clone)]
pub struct RuntimeError {
    pub message: String,
    pub span: Option<Span>,
}

#[derive(Clone)]
pub enum Value<'a> {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(Rc<String>),
    Unit,
    /// Milliseconds.
    Duration(i64),
    Option(Option<Rc<Value<'a>>>),
    List(Rc<Vec<Value<'a>>>),
    /// A `struct` value.
    Struct { name: Rc<String>, fields: Rc<Vec<(String, Value<'a>)>> },
    /// An `enum` variant value.
    Enum { enum_name: Rc<String>, variant: Rc<String>, fields: Rc<Vec<(String, Value<'a>)>> },
    /// A type name used as a value (`decode(raw, User)`).
    Tag(Rc<String>),
    Schedule(ScheduleVal),
    MutMap(Rc<RefCell<HashMap<String, Value<'a>>>>),
    /// Reference to a top-level function.
    FuncRef(&'a FuncDecl),
    /// Constructor used as a function (`|> Some`, `map(ids, UserNotFound)`).
    Ctor(Rc<String>),
    /// Builtin used as a value (`map(xs, show)`).
    Builtin(&'static str),
    Closure { params: &'a [Param], body: &'a Expr, env: Scope<'a> },
    /// Provided service instance.
    Service(Rc<ServiceInstance<'a>>),
    /// Unevaluated `lazy` argument.
    Thunk(Rc<ThunkVal<'a>>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScheduleKind {
    Exponential,
    Fixed,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScheduleVal {
    pub kind: ScheduleKind,
    pub base_ms: i64,
    pub max_retries: Option<i64>,
}

pub struct ServiceInstance<'a> {
    pub service: String,
    pub impl_decl: &'a ImplDecl,
    pub fields: Vec<(String, Value<'a>)>,
}

pub struct ThunkVal<'a> {
    pub expr: &'a Expr,
    pub env: Scope<'a>,
}

/// Lexical scope chain; cheap to clone (closures capture it).
#[derive(Clone)]
pub struct Scope<'a> {
    inner: Rc<ScopeInner<'a>>,
}

struct ScopeInner<'a> {
    vars: RefCell<HashMap<String, Value<'a>>>,
    parent: Option<Scope<'a>>,
}

impl<'a> Scope<'a> {
    fn root() -> Scope<'a> {
        Scope { inner: Rc::new(ScopeInner { vars: RefCell::new(HashMap::new()), parent: None }) }
    }

    fn child(&self) -> Scope<'a> {
        Scope {
            inner: Rc::new(ScopeInner {
                vars: RefCell::new(HashMap::new()),
                parent: Some(self.clone()),
            }),
        }
    }

    fn get(&self, name: &str) -> Option<Value<'a>> {
        if let Some(v) = self.inner.vars.borrow().get(name) {
            return Some(v.clone());
        }
        self.inner.parent.as_ref().and_then(|p| p.get(name))
    }

    fn set(&self, name: &str, value: Value<'a>) {
        self.inner.vars.borrow_mut().insert(name.to_string(), value);
    }
}

/// A failure in flight: either a failed Inga value (catchable, tagged with
/// its type name) or a fatal runtime error (propagates to the top).
pub enum Failure<'a> {
    Error { name: String, value: Value<'a>, span: Span },
    Fatal(RuntimeError),
}

/// The `!` row tag for a runtime value, if values of its type can be failed.
fn fail_tag(value: &Value) -> Option<String> {
    match value {
        Value::Struct { name, .. } => Some(name.to_string()),
        Value::Enum { enum_name, .. } => Some(enum_name.to_string()),
        Value::Int(_) => Some("Int".to_string()),
        Value::Float(_) => Some("Float".to_string()),
        Value::Bool(_) => Some("Bool".to_string()),
        Value::Str(_) => Some("String".to_string()),
        Value::Duration(_) => Some("Duration".to_string()),
        _ => None,
    }
}

pub type EvalResult<'a> = Result<Value<'a>, Failure<'a>>;

pub struct Interp<'a> {
    funcs: HashMap<&'a str, &'a FuncDecl>,
    impls: HashMap<&'a str, &'a ImplDecl>,
    /// Field order for struct constructors, by name.
    struct_fields: HashMap<&'a str, Vec<&'a str>>,
    /// Variant name -> (owning enum, field order).
    variants: HashMap<&'a str, (&'a str, Vec<&'a str>)>,
    /// Dynamically scoped provided services.
    provided: RefCell<Vec<HashMap<String, Rc<ServiceInstance<'a>>>>>,
    /// Output sink (stdout normally; captured in tests).
    pub output: RefCell<Option<String>>,
    /// Epoch for `nowMillis()`.
    start: std::time::Instant,
    /// xorshift64* state for `random()` (0 = unseeded).
    rng: std::cell::Cell<u64>,
}

pub fn run(program: &Program, entry: &str) -> Result<(), RuntimeError> {
    let interp = Interp::new(program);
    let Some(func) = interp.funcs.get(entry).copied() else {
        return Err(RuntimeError {
            message: format!("no `{entry}` function found (define `{entry} :: () {{ ... }}`)"),
            span: None,
        });
    };
    match interp.call_func(func, Vec::new(), func.name_span) {
        Ok(value) => {
            if !matches!(value, Value::Unit) {
                interp.emit(&format!("{}\n", show(&value)));
            }
            Ok(())
        }
        Err(Failure::Fatal(err)) => Err(err),
        Err(Failure::Error { value, span, .. }) => Err(RuntimeError {
            message: format!("unhandled error: {}", show(&value)),
            span: Some(span),
        }),
    }
}

/// Run and capture printed output (for tests).
pub fn run_captured(program: &Program, entry: &str) -> Result<String, RuntimeError> {
    let interp = Interp::new(program);
    *interp.output.borrow_mut() = Some(String::new());
    let Some(func) = interp.funcs.get(entry).copied() else {
        return Err(RuntimeError { message: format!("no `{entry}` function found"), span: None });
    };
    let result = interp.call_func(func, Vec::new(), func.name_span);
    let output = interp.output.borrow_mut().take().unwrap_or_default();
    match result {
        Ok(_) => Ok(output),
        Err(Failure::Fatal(err)) => Err(err),
        Err(Failure::Error { value, span, .. }) => Err(RuntimeError {
            message: format!("unhandled error: {}", show(&value)),
            span: Some(span),
        }),
    }
}

impl<'a> Interp<'a> {
    pub fn new(program: &'a Program) -> Interp<'a> {
        let mut funcs = HashMap::new();
        let mut impls = HashMap::new();
        let mut struct_fields: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut variants: HashMap<&str, (&str, Vec<&str>)> = HashMap::new();
        struct_fields.insert(DECODE_ERROR, vec!["message"]);
        for decl in &program.decls {
            match decl {
                Decl::Func(d) => {
                    funcs.insert(d.name.as_str(), d);
                }
                Decl::Impl(d) => {
                    impls.insert(d.name.as_str(), d);
                }
                Decl::Struct(d) => {
                    struct_fields.insert(
                        d.name.as_str(),
                        d.fields.iter().map(|f| f.name.as_str()).collect(),
                    );
                }
                Decl::Enum(d) => {
                    for v in &d.variants {
                        variants.insert(
                            v.name.as_str(),
                            (d.name.as_str(), v.fields.iter().map(|f| f.name.as_str()).collect()),
                        );
                    }
                }
                Decl::Service(_) | Decl::Use(_) => {}
            }
        }
        Interp {
            funcs,
            impls,
            struct_fields,
            variants,
            provided: RefCell::new(Vec::new()),
            output: RefCell::new(None),
            start: std::time::Instant::now(),
            rng: std::cell::Cell::new(0),
        }
    }

    fn emit(&self, text: &str) {
        let mut out = self.output.borrow_mut();
        match out.as_mut() {
            Some(buffer) => buffer.push_str(text),
            None => print!("{text}"),
        }
    }

    fn fatal<T>(&self, span: Span, message: impl Into<String>) -> Result<T, Failure<'a>> {
        Err(Failure::Fatal(RuntimeError { message: message.into(), span: Some(span) }))
    }

    // ---- functions -------------------------------------------------------

    pub fn call_func(
        &self,
        decl: &'a FuncDecl,
        args: Vec<Value<'a>>,
        call_span: Span,
    ) -> EvalResult<'a> {
        if args.len() != decl.sig.params.len() {
            return self.fatal(
                call_span,
                format!(
                    "`{}` expects {} argument(s), found {}",
                    decl.name,
                    decl.sig.params.len(),
                    args.len()
                ),
            );
        }
        let scope = Scope::root();
        for (param, arg) in decl.sig.params.iter().zip(args) {
            scope.set(&param.name, arg);
        }
        self.eval_block(&decl.body, &scope)
    }

    fn eval_block(&self, block: &'a Block, parent: &Scope<'a>) -> EvalResult<'a> {
        let scope = parent.child();
        let mut result = Value::Unit;
        let count = block.stmts.len();
        for (i, stmt) in block.stmts.iter().enumerate() {
            let last = i + 1 == count;
            match stmt {
                Stmt::Expr(expr) => {
                    let value = self.eval(expr, &scope)?;
                    result = if last { value } else { Value::Unit };
                }
                Stmt::Bind { name, value, .. } => {
                    let v = self.eval(value, &scope)?;
                    scope.set(name, v);
                    result = Value::Unit;
                }
                Stmt::Acquire { service, name, name_span, .. } => {
                    match self.lookup_service(service) {
                        Some(inst) => scope.set(name, Value::Service(inst)),
                        None => {
                            return self.fatal(
                                *name_span,
                                format!("service `{service}` has not been provided"),
                            );
                        }
                    }
                    result = Value::Unit;
                }
            }
        }
        Ok(result)
    }

    fn lookup_service(&self, name: &str) -> Option<Rc<ServiceInstance<'a>>> {
        let provided = self.provided.borrow();
        for frame in provided.iter().rev() {
            if let Some(inst) = frame.get(name) {
                return Some(inst.clone());
            }
        }
        None
    }

    // ---- expressions -----------------------------------------------------

    fn eval(&self, expr: &'a Expr, scope: &Scope<'a>) -> EvalResult<'a> {
        match &expr.kind {
            ExprKind::Int(n) => Ok(Value::Int(*n)),
            ExprKind::Float(f) => Ok(Value::Float(*f)),
            ExprKind::Bool(b) => Ok(Value::Bool(*b)),
            ExprKind::Str(pieces) => {
                let mut out = String::new();
                for piece in pieces {
                    match piece {
                        StrPiece::Text(t) => out.push_str(t),
                        StrPiece::Expr(e) => {
                            let v = self.eval(e, scope)?;
                            out.push_str(&display(&v));
                        }
                    }
                }
                Ok(Value::Str(Rc::new(out)))
            }
            ExprKind::Var(name) => self.eval_var(name, expr.span, scope),
            ExprKind::List(items) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(self.eval(item, scope)?);
                }
                Ok(Value::List(Rc::new(values)))
            }
            ExprKind::Call { callee, args } => {
                let arg_refs: Vec<&'a Expr> = args.iter().collect();
                self.eval_call(callee, &arg_refs, expr.span, scope)
            }
            ExprKind::Method { recv, name, name_span, args } => {
                let arg_refs: Vec<&'a Expr> = args.iter().collect();
                self.eval_method(recv, name, *name_span, &arg_refs, expr.span, scope)
            }
            ExprKind::Field { recv, name, name_span } => {
                let value = self.eval(recv, scope)?;
                self.eval_field(&value, name, *name_span)
            }
            ExprKind::Binary { op, lhs, rhs } => self.eval_binary(*op, lhs, rhs, expr.span, scope),
            ExprKind::Unary { op, expr: inner } => {
                let v = self.eval(inner, scope)?;
                match (op, v) {
                    (UnOp::Neg, Value::Int(n)) => Ok(Value::Int(-n)),
                    (UnOp::Neg, Value::Float(f)) => Ok(Value::Float(-f)),
                    (UnOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
                    (_, v) => self.fatal(
                        inner.span,
                        format!("cannot apply unary operator to {}", show(&v)),
                    ),
                }
            }
            ExprKind::Pipe { lhs, target } => self.eval_pipe(lhs, target, expr.span, scope),
            ExprKind::Match { scrutinee, arms } => {
                let value = self.eval(scrutinee, scope)?;
                for arm in arms {
                    let arm_scope = scope.child();
                    if self.match_pattern(&arm.pattern, &value, &arm_scope) {
                        return self.eval(&arm.body, &arm_scope);
                    }
                }
                self.fatal(expr.span, format!("no match arm matched {}", show(&value)))
            }
            ExprKind::Fail { error } => {
                let value = self.eval(error, scope)?;
                match fail_tag(&value) {
                    Some(name) => Err(Failure::Error { name, value, span: expr.span }),
                    None => self.fatal(
                        error.span,
                        format!(
                            "cannot `fail` with {} (use a struct, enum, or primitive value)",
                            show(&value)
                        ),
                    ),
                }
            }
            ExprKind::Provide { impls, body, .. } => {
                // Items scope left to right: each impl's field initializers
                // run with the previous items already provided.
                let mut pushed = 0usize;
                let mut setup = || -> Result<(), Failure<'a>> {
                    for item in impls {
                        if item.name == "Arena" {
                            // Allocation strategy is the host's concern here;
                            // evaluate the size for effects/validation only.
                            for arg in item.args.as_deref().unwrap_or(&[]) {
                                self.eval(arg, scope)?;
                            }
                            continue;
                        }
                        let Some(impl_decl) = self.impls.get(item.name.as_str()).copied() else {
                            return self
                                .fatal(item.name_span, format!("unknown implementation `{}`", item.name));
                        };
                        let instance = self.instantiate_impl(impl_decl)?;
                        let mut frame = HashMap::new();
                        frame.insert(impl_decl.service.clone(), Rc::new(instance));
                        self.provided.borrow_mut().push(frame);
                        pushed += 1;
                    }
                    Ok(())
                };
                let result = match setup() {
                    Ok(()) => self.eval_block(body, scope),
                    Err(failure) => Err(failure),
                };
                for _ in 0..pushed {
                    self.provided.borrow_mut().pop();
                }
                result
            }
            ExprKind::If { cond, then_block, else_branch } => {
                let c = self.eval(cond, scope)?;
                if matches!(c, Value::Bool(true)) {
                    let v = self.eval_block(then_block, scope)?;
                    Ok(if else_branch.is_some() { v } else { Value::Unit })
                } else {
                    match else_branch {
                        Some(else_expr) => self.eval(else_expr, scope),
                        None => Ok(Value::Unit),
                    }
                }
            }
            ExprKind::Block(block) => self.eval_block(block, scope),
            ExprKind::Lambda { params, body } => {
                Ok(Value::Closure { params, body, env: scope.clone() })
            }
        }
    }

    /// `alias.member(args)` — a module-qualified call. Top-level names are
    /// program-unique, so the member resolves directly; the checker already
    /// verified the alias, module, and visibility.
    fn eval_qualified(
        &self,
        module: &str,
        member: &str,
        args: &[&'a Expr],
        span: Span,
        scope: &Scope<'a>,
    ) -> Option<EvalResult<'a>> {
        if self.funcs.contains_key(module)
            || self.struct_fields.contains_key(module)
            || self.variants.contains_key(module)
        {
            return None; // a real value/ctor name, not a module alias
        }
        if let Some(decl) = self.funcs.get(member).copied() {
            let mut arg_values = Vec::with_capacity(args.len());
            for (i, arg) in args.iter().enumerate() {
                let lazy = decl.sig.params.get(i).is_some_and(|p| p.lazy);
                let value = if lazy {
                    Value::Thunk(Rc::new(ThunkVal { expr: arg, env: scope.clone() }))
                } else {
                    match self.eval(arg, scope) {
                        Ok(v) => v,
                        Err(e) => return Some(Err(e)),
                    }
                };
                arg_values.push(value);
            }
            return Some(self.call_func(decl, arg_values, span));
        }
        if let Some(fields) = self.struct_fields.get(member) {
            let fields = fields.clone();
            return Some(self.construct(member, &fields, args, None, span, scope));
        }
        if let Some((enum_name, fields)) = self.variants.get(member) {
            let (enum_name, fields) = (*enum_name, fields.clone());
            return Some(self.construct(member, &fields, args, Some(enum_name), span, scope));
        }
        None
    }

    fn instantiate_impl(&self, decl: &'a ImplDecl) -> Result<ServiceInstance<'a>, Failure<'a>> {
        let scope = Scope::root();
        let mut fields = Vec::new();
        for (name, _span, init) in &decl.fields {
            let value = self.eval(init, &scope)?;
            scope.set(name, value.clone());
            fields.push((name.clone(), value));
        }
        Ok(ServiceInstance { service: decl.service.clone(), impl_decl: decl, fields })
    }

    fn eval_var(&self, name: &str, span: Span, scope: &Scope<'a>) -> EvalResult<'a> {
        if let Some(value) = scope.get(name) {
            // Lazy parameters force on read.
            if let Value::Thunk(thunk) = &value {
                return self.eval(thunk.expr, &thunk.env);
            }
            return Ok(value);
        }
        if let Some(func) = self.funcs.get(name) {
            return Ok(Value::FuncRef(func));
        }
        match name {
            "None" => return Ok(Value::Option(None)),
            "Some" => return Ok(Value::Ctor(Rc::new("Some".to_string()))),
            "show" => return Ok(Value::Builtin("show")),
            "encode" => return Ok(Value::Builtin("encode")),
            _ => {}
        }
        if self.struct_fields.contains_key(name) {
            return Ok(Value::Tag(Rc::new(name.to_string())));
        }
        if let Some((enum_name, fields)) = self.variants.get(name) {
            // Fieldless variants are values; the rest are constructors.
            if fields.is_empty() {
                return Ok(Value::Enum {
                    enum_name: Rc::new(enum_name.to_string()),
                    variant: Rc::new(name.to_string()),
                    fields: Rc::new(Vec::new()),
                });
            }
            return Ok(Value::Ctor(Rc::new(name.to_string())));
        }
        self.fatal(span, format!("unknown name `{name}`"))
    }

    // ---- calls -------------------------------------------------------------

    fn eval_pipe(
        &self,
        lhs: &'a Expr,
        target: &'a PipeTarget,
        span: Span,
        scope: &Scope<'a>,
    ) -> EvalResult<'a> {
        match target {
            PipeTarget::Call { callee, args } => {
                let mut all: Vec<&'a Expr> = vec![lhs];
                if let Some(extra) = args {
                    all.extend(extra.iter());
                }
                self.eval_call(callee, &all, span, scope)
            }
            PipeTarget::Catch { arms, .. } => match self.eval(lhs, scope) {
                Ok(value) => Ok(value),
                Err(Failure::Fatal(err)) => Err(Failure::Fatal(err)),
                Err(Failure::Error { name, value, span: err_span }) => {
                    // Catch arms pattern-match the failed value itself.
                    for arm in arms {
                        let arm_scope = scope.child();
                        if self.match_pattern(&arm.pattern, &value, &arm_scope) {
                            return self.eval(&arm.body, &arm_scope);
                        }
                    }
                    Err(Failure::Error { name, value, span: err_span })
                }
            },
        }
    }

    fn eval_call(
        &self,
        callee: &'a Expr,
        args: &[&'a Expr],
        span: Span,
        scope: &Scope<'a>,
    ) -> EvalResult<'a> {
        // Builtin modules: `Schedule.*`, `Gfx.*`.
        if let ExprKind::Field { recv, name, .. } = &callee.kind {
            if let ExprKind::Var(module) = &recv.kind {
                if scope.get(module).is_none() {
                    if module == "schedule" {
                        return self.eval_schedule_call(name, args, span, scope);
                    }
                    if module == "graphics" {
                        return self.eval_gfx_call(name, args, span, scope);
                    }
                    if let Some(result) = self.eval_qualified(module, name, args, span, scope) {
                        return result;
                    }
                }
            }
        }
        if let ExprKind::Var(name) = &callee.kind {
            if scope.get(name).is_none() {
                // Builtins (some with by-name arguments).
                if let Some(result) = self.eval_builtin(name, args, span, scope) {
                    return result;
                }
                // Constructors.
                if let Some(fields) = self.struct_fields.get(name.as_str()) {
                    let fields = fields.clone();
                    return self.construct(name, &fields, args, None, span, scope);
                }
                if let Some((enum_name, fields)) = self.variants.get(name.as_str()) {
                    let (enum_name, fields) = (*enum_name, fields.clone());
                    return self.construct(name, &fields, args, Some(enum_name), span, scope);
                }
            }
        }
        let callee_value = self.eval(callee, scope)?;
        let mut arg_values = Vec::with_capacity(args.len());
        // Lazy params of user functions: pass thunks where declared.
        if let Value::FuncRef(decl) = &callee_value {
            for (i, arg) in args.iter().enumerate() {
                let lazy = decl.sig.params.get(i).is_some_and(|p| p.lazy);
                if lazy {
                    arg_values
                        .push(Value::Thunk(Rc::new(ThunkVal { expr: arg, env: scope.clone() })));
                } else {
                    arg_values.push(self.eval(arg, scope)?);
                }
            }
        } else {
            for arg in args {
                arg_values.push(self.eval(arg, scope)?);
            }
        }
        self.apply(callee_value, arg_values, span)
    }

    /// Call a value (function reference, closure, constructor, builtin).
    pub fn apply(&self, callee: Value<'a>, args: Vec<Value<'a>>, span: Span) -> EvalResult<'a> {
        match callee {
            Value::FuncRef(decl) => self.call_func(decl, args, span),
            Value::Closure { params, body, env } => {
                if params.len() != args.len() {
                    return self.fatal(
                        span,
                        format!(
                            "lambda expects {} argument(s), found {}",
                            params.len(),
                            args.len()
                        ),
                    );
                }
                let scope = env.child();
                for (param, arg) in params.iter().zip(args) {
                    scope.set(&param.name, arg);
                }
                self.eval(body, &scope)
            }
            Value::Ctor(name) => {
                if name.as_str() == "Some" {
                    if args.len() != 1 {
                        return self.fatal(span, "`Some` takes one argument");
                    }
                    return Ok(Value::Option(Some(Rc::new(args.into_iter().next().unwrap()))));
                }
                let (enum_name, field_names) = match self.variants.get(name.as_str()) {
                    Some((owner, fields)) => (Some(*owner), fields.clone()),
                    None => (None, self.struct_fields.get(name.as_str()).cloned().unwrap_or_default()),
                };
                if field_names.len() != args.len() {
                    return self.fatal(
                        span,
                        format!(
                            "`{name}` has {} field(s) but {} argument(s) were given",
                            field_names.len(),
                            args.len()
                        ),
                    );
                }
                let fields: Vec<(String, Value<'a>)> =
                    field_names.iter().map(|f| f.to_string()).zip(args).collect();
                Ok(match enum_name {
                    Some(owner) => Value::Enum {
                        enum_name: Rc::new(owner.to_string()),
                        variant: Rc::new(name.to_string()),
                        fields: Rc::new(fields),
                    },
                    None => Value::Struct { name: Rc::new(name.to_string()), fields: Rc::new(fields) },
                })
            }
            Value::Builtin(name) => match (name, args.as_slice()) {
                ("show", [v]) => Ok(Value::Str(Rc::new(show(v)))),
                ("encode", [v]) => Ok(Value::Str(Rc::new(encode(v)))),
                _ => {
                    self.fatal(span, format!("cannot call builtin `{name}` with these arguments"))
                }
            },
            Value::Tag(name) => self.fatal(
                span,
                format!("`{name}` is a type; construct values with `{name}(...)`"),
            ),
            other => self.fatal(span, format!("{} is not callable", show(&other))),
        }
    }

    fn construct(
        &self,
        name: &str,
        field_names: &[&str],
        args: &[&'a Expr],
        enum_name: Option<&str>,
        span: Span,
        scope: &Scope<'a>,
    ) -> EvalResult<'a> {
        if field_names.len() != args.len() {
            return self.fatal(
                span,
                format!(
                    "`{name}` has {} field(s) but {} argument(s) were given",
                    field_names.len(),
                    args.len()
                ),
            );
        }
        let mut fields = Vec::with_capacity(args.len());
        for (field, arg) in field_names.iter().zip(args) {
            fields.push((field.to_string(), self.eval(arg, scope)?));
        }
        Ok(match enum_name {
            Some(owner) => Value::Enum {
                enum_name: Rc::new(owner.to_string()),
                variant: Rc::new(name.to_string()),
                fields: Rc::new(fields),
            },
            None => Value::Struct { name: Rc::new(name.to_string()), fields: Rc::new(fields) },
        })
    }

    fn eval_schedule_call(
        &self,
        name: &str,
        args: &[&'a Expr],
        span: Span,
        scope: &Scope<'a>,
    ) -> EvalResult<'a> {
        if name == "upTo" {
            if args.len() != 2 {
                return self.fatal(span, "`schedule.upTo` takes (schedule, times)");
            }
            let sched = self.eval(args[0], scope)?;
            let n = self.eval(args[1], scope)?;
            return match (sched, n) {
                (Value::Schedule(mut sched), Value::Int(times)) => {
                    sched.max_retries = Some(times);
                    Ok(Value::Schedule(sched))
                }
                _ => self.fatal(span, "`schedule.upTo` adjusts a Schedule by an Int"),
            };
        }
        let kind = match name {
            "exponential" => ScheduleKind::Exponential,
            "fixed" => ScheduleKind::Fixed,
            _ => return self.fatal(span, format!("unknown schedule `schedule.{name}`")),
        };
        let Some(arg) = args.first() else {
            return self.fatal(span, format!("`schedule.{name}` takes one Duration argument"));
        };
        let base = self.eval(arg, scope)?;
        let Value::Duration(base_ms) = base else {
            return self.fatal(arg.span, "schedule base must be a Duration (like `100.millis`)");
        };
        Ok(Value::Schedule(ScheduleVal { kind, base_ms, max_retries: None }))
    }

    /// The graphics module (interpreter side). Available when inga-core is
    /// built with the `gfx` feature (the CLI enables it; the LSP does not).
    /// Shader handles index into MATERIALS (module scope, below).
    #[cfg(feature = "gfx")]
    fn eval_gfx_call(
        &self,
        name: &str,
        args: &[&'a Expr],
        span: Span,
        scope: &Scope<'a>,
    ) -> EvalResult<'a> {
        use macroquad::prelude as mq;

        fn color(r: i64, g: i64, b: i64, a: i64) -> mq::Color {
            mq::Color::from_rgba(r as u8, g as u8, b as u8, a as u8)
        }
        // Evaluate all args up front (every Gfx call is eager).
        let mut vals = Vec::with_capacity(args.len());
        for arg in args {
            vals.push(self.eval(arg, scope)?);
        }
        let int = |i: usize| -> i64 {
            match vals.get(i) {
                Some(Value::Int(n)) => *n,
                _ => 0,
            }
        };
        match name {
            "run" => {
                let (Some(Value::Str(title)), Some(frame)) = (vals.get(2), vals.get(3)) else {
                    return self.fatal(span, "`Gfx.run` takes (width, height, title, frame)");
                };
                let conf = macroquad::window::Conf {
                    window_title: title.to_string(),
                    window_width: int(0) as i32,
                    window_height: int(1) as i32,
                    high_dpi: true,
                    ..Default::default()
                };
                // SAFETY: `Window::from_config` blocks until the window
                // closes, and `self`/the AST outlive that call; the 'static
                // lifetimes never escape it.
                let interp: &'static Interp<'static> = unsafe { std::mem::transmute(self) };
                let frame: Value<'static> = unsafe { std::mem::transmute(frame.clone()) };
                // Debug/CI hook shared with the native runtime.
                let shot = std::env::var("INGA_GFX_SHOT").ok();
                let shot_frame: u32 = std::env::var("INGA_GFX_SHOT_FRAME")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(30);
                macroquad::Window::from_config(conf, async move {
                    let mut frame_no = 0u32;
                    loop {
                        match interp.apply(frame.clone(), Vec::new(), Span::default()) {
                            Ok(_) => {}
                            Err(Failure::Fatal(e)) => {
                                eprintln!("runtime error: {}", e.message);
                                std::process::exit(101);
                            }
                            Err(Failure::Error { value, .. }) => {
                                eprintln!(
                                    "runtime error: unhandled error in frame: {}",
                                    show(&value)
                                );
                                std::process::exit(101);
                            }
                        }
                        frame_no += 1;
                        if let Some(path) = &shot {
                            if frame_no == shot_frame {
                                macroquad::texture::get_screen_data().export_png(path);
                                std::process::exit(0);
                            }
                        }
                        macroquad::window::next_frame().await;
                    }
                });
                Ok(Value::Unit)
            }
            "clear" => {
                mq::clear_background(color(int(0), int(1), int(2), 255));
                Ok(Value::Unit)
            }
            "rect" => {
                mq::draw_rectangle(
                    int(0) as f32,
                    int(1) as f32,
                    int(2) as f32,
                    int(3) as f32,
                    color(int(4), int(5), int(6), int(7)),
                );
                Ok(Value::Unit)
            }
            "rectLines" => {
                mq::draw_rectangle_lines(
                    int(0) as f32,
                    int(1) as f32,
                    int(2) as f32,
                    int(3) as f32,
                    int(4) as f32,
                    color(int(5), int(6), int(7), int(8)),
                );
                Ok(Value::Unit)
            }
            "circle" => {
                mq::draw_circle(
                    int(0) as f32,
                    int(1) as f32,
                    int(2) as f32,
                    color(int(3), int(4), int(5), int(6)),
                );
                Ok(Value::Unit)
            }
            "text" => {
                let Some(Value::Str(text)) = vals.first() else {
                    return self.fatal(span, "`Gfx.text` needs a String first");
                };
                mq::draw_text(
                    text.as_str(),
                    int(1) as f32,
                    int(2) as f32,
                    int(3) as f32,
                    color(int(4), int(5), int(6), 255),
                );
                Ok(Value::Unit)
            }
            "textWidth" => {
                let Some(Value::Str(text)) = vals.first() else {
                    return self.fatal(span, "`Gfx.textWidth` needs a String first");
                };
                let dims = mq::measure_text(text.as_str(), None, (int(1) as f32) as u16, 1.0);
                Ok(Value::Int(dims.width as i64))
            }
            "mouseX" => Ok(Value::Int(mq::mouse_position().0 as i64)),
            "mouseY" => Ok(Value::Int(mq::mouse_position().1 as i64)),
            "mousePressed" => Ok(Value::Bool(mq::is_mouse_button_pressed(mq::MouseButton::Left))),
            "shaderNew" => {
                use macroquad::miniquad::{UniformDesc, UniformType};
                let Some(Value::Str(fragment)) = vals.first() else {
                    return self.fatal(span, "`Gfx.shaderNew` needs a GLSL fragment String");
                };
                let result = macroquad::material::load_material(
                    macroquad::miniquad::ShaderSource::Glsl {
                        vertex: GFX_VERTEX_SHADER,
                        fragment,
                    },
                    macroquad::material::MaterialParams {
                        uniforms: vec![
                            UniformDesc::new("iTime", UniformType::Float1),
                            UniformDesc::new("iRes", UniformType::Float2),
                        ],
                        ..Default::default()
                    },
                );
                match result {
                    Ok(material) => Ok(Value::Int(MATERIALS.with(|m| {
                        m.borrow_mut().push(material);
                        m.borrow().len() as i64 - 1
                    }))),
                    Err(e) => {
                        eprintln!("Gfx.shaderNew: shader failed to compile: {e}");
                        Ok(Value::Int(-1))
                    }
                }
            }
            "shaderUse" => {
                MATERIALS.with(|m| {
                    if let Some(material) = m.borrow().get(int(0) as usize) {
                        material.set_uniform("iTime", self.start.elapsed().as_secs_f32());
                        material.set_uniform(
                            "iRes",
                            macroquad::math::Vec2::new(mq::screen_width(), mq::screen_height()),
                        );
                        macroquad::material::gl_use_material(material);
                    }
                });
                Ok(Value::Unit)
            }
            "shaderOff" => {
                macroquad::material::gl_use_default_material();
                Ok(Value::Unit)
            }
            _ => self.fatal(span, format!("unknown graphics call `Gfx.{name}`")),
        }
    }

    #[cfg(not(feature = "gfx"))]
    fn eval_gfx_call(
        &self,
        _name: &str,
        _args: &[&'a Expr],
        span: Span,
        _scope: &Scope<'a>,
    ) -> EvalResult<'a> {
        self.fatal(
            span,
            "this interpreter was built without graphics support — use `inga build`, or build the CLI with the `gfx` feature",
        )
    }

    /// Returns None if `name` is not a builtin.
    fn eval_builtin(
        &self,
        name: &str,
        args: &[&'a Expr],
        span: Span,
        scope: &Scope<'a>,
    ) -> Option<EvalResult<'a>> {
        let result = match name {
            "println" | "print" => {
                let mut text = String::new();
                for (i, arg) in args.iter().enumerate() {
                    match self.eval(arg, scope) {
                        Ok(v) => {
                            if i > 0 {
                                text.push(' ');
                            }
                            text.push_str(&display(&v));
                        }
                        Err(e) => return Some(Err(e)),
                    }
                }
                if name == "println" {
                    text.push('\n');
                }
                self.emit(&text);
                Ok(Value::Unit)
            }
            "show" if args.len() == 1 => match self.eval(args[0], scope) {
                Ok(v) => Ok(Value::Str(Rc::new(show(&v)))),
                Err(e) => Err(e),
            },
            "encode" if args.len() == 1 => match self.eval(args[0], scope) {
                Ok(v) => Ok(Value::Str(Rc::new(encode(&v)))),
                Err(e) => Err(e),
            },
            "decode" if args.len() == 2 => self.builtin_decode(args, span, scope),
            "map" if args.len() == 2 => self.builtin_map(args, span, scope),
            "getOrElse" if args.len() == 2 => match self.eval(args[0], scope) {
                Ok(Value::Option(Some(v))) => Ok((*v).clone()),
                Ok(Value::Option(None)) => self.eval(args[1], scope),
                Ok(other) => self
                    .fatal(args[0].span, format!("`getOrElse` works on options, found {}", show(&other))),
                Err(e) => Err(e),
            },
            "orFail" if args.len() == 2 => match self.eval(args[0], scope) {
                Ok(Value::Option(Some(v))) => Ok((*v).clone()),
                Ok(Value::Option(None)) => match self.eval(args[1], scope) {
                    Ok(err) => match fail_tag(&err) {
                        Some(name) => Err(Failure::Error { name, value: err, span }),
                        None => self.fatal(
                            args[1].span,
                            format!(
                                "cannot fail with {} (use a struct, enum, or primitive value)",
                                show(&err)
                            ),
                        ),
                    },
                    Err(e) => Err(e),
                },
                Ok(other) => self
                    .fatal(args[0].span, format!("`orFail` works on options, found {}", show(&other))),
                Err(e) => Err(e),
            },
            "retry" if args.len() == 2 => self.builtin_retry(args, scope),
            "ignoreFailure" if args.len() == 1 => match self.eval(args[0], scope) {
                Ok(_) => Ok(Value::Unit),
                Err(Failure::Fatal(e)) => Err(Failure::Fatal(e)),
                Err(Failure::Error { .. }) => Ok(Value::Unit),
            },
            "sleep" if args.len() == 1 => match self.eval(args[0], scope) {
                Ok(Value::Duration(ms)) => {
                    std::thread::sleep(std::time::Duration::from_millis(ms.max(0) as u64));
                    Ok(Value::Unit)
                }
                Ok(_) => self.fatal(args[0].span, "`sleep` needs a Duration"),
                Err(e) => Err(e),
            },
            "len" if args.len() == 1 => match self.eval(args[0], scope) {
                Ok(Value::Str(s)) => Ok(Value::Int(s.chars().count() as i64)),
                Ok(Value::List(items)) => Ok(Value::Int(items.len() as i64)),
                Ok(other) => self.fatal(
                    args[0].span,
                    format!("`len` works on String or lists, found {}", show(&other)),
                ),
                Err(e) => Err(e),
            },
            "MutMap" if args.is_empty() => {
                Ok(Value::MutMap(Rc::new(RefCell::new(HashMap::new()))))
            }
            "nowMillis" if args.is_empty() => {
                Ok(Value::Int(self.start.elapsed().as_millis() as i64))
            }
            "nowMicros" if args.is_empty() => {
                Ok(Value::Int(self.start.elapsed().as_micros() as i64))
            }
            "range" if args.len() == 1 => match self.eval(args[0], scope) {
                Ok(Value::Int(n)) => {
                    Ok(Value::List(Rc::new((0..n.max(0)).map(Value::Int).collect())))
                }
                Ok(_) => self.fatal(args[0].span, "`range` needs an Int"),
                Err(e) => Err(e),
            },
            "random" if args.len() == 1 => match self.eval(args[0], scope) {
                Ok(Value::Int(n)) if n > 0 => {
                    let mut s = self.rng.get();
                    if s == 0 {
                        s = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_nanos() as u64)
                            .unwrap_or(0x4d595df4d0f33173)
                            | 1;
                    }
                    s ^= s << 13;
                    s ^= s >> 7;
                    s ^= s << 17;
                    self.rng.set(s);
                    Ok(Value::Int(
                        ((s.wrapping_mul(0x2545F4914F6CDD1D) >> 32) % n as u64) as i64,
                    ))
                }
                Ok(Value::Int(_)) => Ok(Value::Int(0)),
                Ok(_) => self.fatal(args[0].span, "`random` needs an Int"),
                Err(e) => Err(e),
            },
            "Some" if args.len() == 1 => match self.eval(args[0], scope) {
                Ok(v) => Ok(Value::Option(Some(Rc::new(v)))),
                Err(e) => Err(e),
            },
            _ => return None,
        };
        Some(result)
    }

    fn builtin_decode(
        &self,
        args: &[&'a Expr],
        span: Span,
        scope: &Scope<'a>,
    ) -> EvalResult<'a> {
        let raw = self.eval(args[0], scope)?;
        let tag = self.eval(args[1], scope)?;
        let Value::Str(text) = &raw else {
            return self.fatal(args[0].span, "`decode` input must be a String");
        };
        let Value::Tag(type_name) = &tag else {
            return self.fatal(args[1].span, "`decode` needs a type name (like `User`)");
        };
        let Some(field_names) = self.struct_fields.get(type_name.as_str()) else {
            return self.fatal(args[1].span, format!("unknown type `{type_name}`"));
        };
        match decode_json(text, type_name, field_names) {
            Ok(value) => Ok(value),
            Err(message) => Err(self.make_error(
                DECODE_ERROR,
                vec![("message".to_string(), Value::Str(Rc::new(message)))],
                span,
            )),
        }
    }

    fn builtin_map(&self, args: &[&'a Expr], span: Span, scope: &Scope<'a>) -> EvalResult<'a> {
        let container = self.eval(args[0], scope)?;
        let f = self.eval(args[1], scope)?;
        match container {
            Value::Option(None) => Ok(Value::Option(None)),
            Value::Option(Some(v)) => {
                let mapped = self.apply(f, vec![(*v).clone()], span)?;
                Ok(Value::Option(Some(Rc::new(mapped))))
            }
            Value::List(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items.iter() {
                    out.push(self.apply(f.clone(), vec![item.clone()], span)?);
                }
                Ok(Value::List(Rc::new(out)))
            }
            other => self.fatal(
                args[0].span,
                format!("`map` works on options and lists, found {}", show(&other)),
            ),
        }
    }

    fn builtin_retry(&self, args: &[&'a Expr], scope: &Scope<'a>) -> EvalResult<'a> {
        // args[0] is by-name: re-evaluated per attempt.
        let schedule = self.eval(args[1], scope)?;
        let Value::Schedule(sched) = schedule else {
            return self.fatal(args[1].span, "`retry` needs a Schedule");
        };
        let max = sched.max_retries.unwrap_or(3);
        let mut delay = sched.base_ms;
        let mut attempt = 0;
        loop {
            match self.eval(args[0], scope) {
                Ok(value) => return Ok(value),
                Err(Failure::Fatal(e)) => return Err(Failure::Fatal(e)),
                Err(error) => {
                    if attempt >= max {
                        return Err(error);
                    }
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(delay.max(0) as u64));
                    if sched.kind == ScheduleKind::Exponential {
                        delay = delay.saturating_mul(2);
                    }
                }
            }
        }
    }

    fn make_error(
        &self,
        name: &str,
        fields: Vec<(String, Value<'a>)>,
        span: Span,
    ) -> Failure<'a> {
        Failure::Error {
            name: name.to_string(),
            value: Value::Struct { name: Rc::new(name.to_string()), fields: Rc::new(fields) },
            span,
        }
    }

    // ---- methods / fields ---------------------------------------------------

    fn eval_method(
        &self,
        recv: &'a Expr,
        name: &str,
        name_span: Span,
        args: &[&'a Expr],
        span: Span,
        scope: &Scope<'a>,
    ) -> EvalResult<'a> {
        if let ExprKind::Var(module) = &recv.kind {
            if scope.get(module).is_none() {
                if module == "schedule" {
                    return self.eval_schedule_call(name, args, span, scope);
                }
                if module == "graphics" {
                    return self.eval_gfx_call(name, args, span, scope);
                }
                if let Some(result) = self.eval_qualified(module, name, args, span, scope) {
                    return result;
                }
            }
        }
        let recv_value = self.eval(recv, scope)?;
        match &recv_value {
            Value::Service(instance) => {
                let method = instance.impl_decl.methods.iter().find(|m| m.name == name);
                let Some(method) = method else {
                    return self.fatal(
                        name_span,
                        format!(
                            "implementation `{}` has no method `{name}`",
                            instance.impl_decl.name
                        ),
                    );
                };
                if method.sig.params.len() != args.len() {
                    return self.fatal(
                        span,
                        format!(
                            "`{name}` expects {} argument(s), found {}",
                            method.sig.params.len(),
                            args.len()
                        ),
                    );
                }
                let method_scope = Scope::root();
                for (field, value) in &instance.fields {
                    method_scope.set(field, value.clone());
                }
                for (param, arg) in method.sig.params.iter().zip(args) {
                    let value = if param.lazy {
                        Value::Thunk(Rc::new(ThunkVal { expr: arg, env: scope.clone() }))
                    } else {
                        self.eval(arg, scope)?
                    };
                    method_scope.set(&param.name, value);
                }
                self.eval_block(&method.body, &method_scope)
            }
            Value::MutMap(map) => match name {
                "get" if args.len() == 1 => {
                    let key = self.eval(args[0], scope)?;
                    let key = map_key(&key);
                    Ok(match map.borrow().get(&key) {
                        Some(v) => Value::Option(Some(Rc::new(v.clone()))),
                        None => Value::Option(None),
                    })
                }
                "set" if args.len() == 2 => {
                    let key = self.eval(args[0], scope)?;
                    let value = self.eval(args[1], scope)?;
                    map.borrow_mut().insert(map_key(&key), value);
                    Ok(Value::Unit)
                }
                "delete" if args.len() == 1 => {
                    let key = self.eval(args[0], scope)?;
                    map.borrow_mut().remove(&map_key(&key));
                    Ok(Value::Unit)
                }
                "size" if args.is_empty() => Ok(Value::Int(map.borrow().len() as i64)),
                _ => self.fatal(
                    name_span,
                    format!("MutMap has no method `{name}` with {} argument(s)", args.len()),
                ),
            },
            other => self.fatal(name_span, format!("{} has no method `{name}`", show(other))),
        }
    }

    fn eval_field(&self, value: &Value<'a>, name: &str, span: Span) -> EvalResult<'a> {
        if let Value::Int(n) = value {
            if let Some((_, factor)) = DURATION_SUFFIXES.iter().find(|(s, _)| *s == name) {
                return Ok(Value::Duration(n * factor));
            }
            if let Some((_, factor)) = SIZE_SUFFIXES.iter().find(|(s, _)| *s == name) {
                return Ok(Value::Int(n * factor));
            }
        }
        match value {
            Value::Struct { fields, .. } => match fields.iter().find(|(f, _)| f == name) {
                Some((_, v)) => Ok(v.clone()),
                None => self.fatal(span, format!("no field `{name}`")),
            },
            other => self.fatal(span, format!("{} has no field `{name}`", show(other))),
        }
    }

    fn eval_binary(
        &self,
        op: BinOp,
        lhs: &'a Expr,
        rhs: &'a Expr,
        span: Span,
        scope: &Scope<'a>,
    ) -> EvalResult<'a> {
        // Short-circuit logic first.
        if matches!(op, BinOp::And | BinOp::Or) {
            let l = self.eval(lhs, scope)?;
            let Value::Bool(l) = l else {
                return self.fatal(lhs.span, "logical operands must be Bool");
            };
            if (op == BinOp::And && !l) || (op == BinOp::Or && l) {
                return Ok(Value::Bool(l));
            }
            let r = self.eval(rhs, scope)?;
            let Value::Bool(r) = r else {
                return self.fatal(rhs.span, "logical operands must be Bool");
            };
            return Ok(Value::Bool(r));
        }

        let l = self.eval(lhs, scope)?;
        let r = self.eval(rhs, scope)?;
        let result = match (op, &l, &r) {
            (BinOp::Add, Value::Int(a), Value::Int(b)) => Value::Int(a.wrapping_add(*b)),
            (BinOp::Sub, Value::Int(a), Value::Int(b)) => Value::Int(a.wrapping_sub(*b)),
            (BinOp::Mul, Value::Int(a), Value::Int(b)) => Value::Int(a.wrapping_mul(*b)),
            (BinOp::Div | BinOp::Mod, Value::Int(_), Value::Int(0)) => {
                return self.fatal(span, "division by zero");
            }
            (BinOp::Div, Value::Int(a), Value::Int(b)) => Value::Int(a / b),
            (BinOp::Mod, Value::Int(a), Value::Int(b)) => Value::Int(a % b),
            (BinOp::Add, Value::Float(a), Value::Float(b)) => Value::Float(a + b),
            (BinOp::Sub, Value::Float(a), Value::Float(b)) => Value::Float(a - b),
            (BinOp::Mul, Value::Float(a), Value::Float(b)) => Value::Float(a * b),
            (BinOp::Div, Value::Float(a), Value::Float(b)) => Value::Float(a / b),
            (BinOp::Add, Value::Str(a), Value::Str(b)) => Value::Str(Rc::new(format!("{a}{b}"))),
            (BinOp::Add, Value::Duration(a), Value::Duration(b)) => Value::Duration(a + b),
            (BinOp::Sub, Value::Duration(a), Value::Duration(b)) => Value::Duration(a - b),
            (BinOp::Eq, a, b) => Value::Bool(values_equal(a, b)),
            (BinOp::Ne, a, b) => Value::Bool(!values_equal(a, b)),
            (BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge, a, b) => {
                let Some(ordering) = compare(a, b) else {
                    return self
                        .fatal(span, format!("cannot compare {} and {}", show(a), show(b)));
                };
                let ok = match op {
                    BinOp::Lt => ordering.is_lt(),
                    BinOp::Le => ordering.is_le(),
                    BinOp::Gt => ordering.is_gt(),
                    _ => ordering.is_ge(),
                };
                Value::Bool(ok)
            }
            _ => {
                return self.fatal(
                    span,
                    format!("`{}` is not defined for {} and {}", op.symbol(), show(&l), show(&r)),
                );
            }
        };
        Ok(result)
    }

    // ---- pattern matching ------------------------------------------------------

    fn match_pattern(&self, pat: &Pattern, value: &Value<'a>, scope: &Scope<'a>) -> bool {
        match (&pat.kind, value) {
            (PatternKind::Wildcard, _) => true,
            (PatternKind::Bind(name), v) => {
                scope.set(name, v.clone());
                true
            }
            (PatternKind::Int(p), Value::Int(n)) => p == n,
            (PatternKind::Str(p), Value::Str(s)) => p == s.as_str(),
            (PatternKind::Bool(p), Value::Bool(b)) => p == b,
            (PatternKind::TypedBind { ty, name, .. }, v) => {
                if type_matches(ty, v) {
                    scope.set(name, v.clone());
                    true
                } else {
                    false
                }
            }
            (PatternKind::Ctor { name, args, .. }, v) => match (name.as_str(), v) {
                ("Some", Value::Option(Some(inner))) => match args {
                    CtorPatArgs::Positional(pats) if pats.len() == 1 => {
                        self.match_pattern(&pats[0], inner, scope)
                    }
                    CtorPatArgs::None => true,
                    _ => false,
                },
                ("None", Value::Option(None)) => true,
                (_, Value::Struct { name: vname, fields }) if name == vname.as_str() => {
                    self.match_struct_args(args, fields, scope)
                }
                (_, Value::Enum { enum_name, variant, fields }) => {
                    if name == variant.as_str() {
                        self.match_struct_args(args, fields, scope)
                    } else if name == enum_name.as_str() {
                        // The bare enum name matches any of its variants.
                        matches!(args, CtorPatArgs::None)
                    } else {
                        false
                    }
                }
                _ => false,
            },
            _ => false,
        }
    }

    fn match_struct_args(
        &self,
        args: &CtorPatArgs,
        fields: &[(String, Value<'a>)],
        scope: &Scope<'a>,
    ) -> bool {
        match args {
            CtorPatArgs::None => true,
            CtorPatArgs::Positional(pats) => {
                pats.len() == fields.len()
                    && pats
                        .iter()
                        .zip(fields.iter())
                        .all(|(p, (_, v))| self.match_pattern(p, v, scope))
            }
            CtorPatArgs::Fields(names) => {
                for (fname, _) in names {
                    match fields.iter().find(|(f, _)| f == fname) {
                        Some((_, v)) => scope.set(fname, v.clone()),
                        None => return false,
                    }
                }
                true
            }
        }
    }

}

/// Does a runtime value belong to the named type? (TypedBind patterns.)
fn type_matches(ty: &str, value: &Value) -> bool {
    match value {
        Value::Int(_) => ty == "Int",
        Value::Float(_) => ty == "Float",
        Value::Bool(_) => ty == "Bool",
        Value::Str(_) => ty == "String",
        Value::Duration(_) => ty == "Duration",
        Value::Struct { name, .. } => ty == name.as_str(),
        Value::Enum { enum_name, .. } => ty == enum_name.as_str(),
        _ => false,
    }
}

// ---- value helpers --------------------------------------------------------

fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Unit, Value::Unit) => true,
        (Value::Duration(x), Value::Duration(y)) => x == y,
        (Value::Option(None), Value::Option(None)) => true,
        (Value::Option(Some(x)), Value::Option(Some(y))) => values_equal(x, y),
        (Value::List(xs), Value::List(ys)) => {
            xs.len() == ys.len() && xs.iter().zip(ys.iter()).all(|(x, y)| values_equal(x, y))
        }
        (Value::Struct { name: n1, fields: f1 }, Value::Struct { name: n2, fields: f2 }) => {
            n1 == n2
                && f1.len() == f2.len()
                && f1
                    .iter()
                    .zip(f2.iter())
                    .all(|((k1, v1), (k2, v2))| k1 == k2 && values_equal(v1, v2))
        }
        (
            Value::Enum { variant: v1, fields: f1, .. },
            Value::Enum { variant: v2, fields: f2, .. },
        ) => {
            v1 == v2
                && f1.len() == f2.len()
                && f1
                    .iter()
                    .zip(f2.iter())
                    .all(|((k1, x), (k2, y))| k1 == k2 && values_equal(x, y))
        }
        _ => false,
    }
}

fn compare(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Some(x.cmp(y)),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y),
        (Value::Str(x), Value::Str(y)) => Some(x.cmp(y)),
        (Value::Duration(x), Value::Duration(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

/// Developer-facing rendering (`show`, match-failure messages).
pub fn show(value: &Value) -> String {
    match value {
        Value::Int(n) => n.to_string(),
        Value::Float(f) => format_float(*f),
        Value::Bool(b) => b.to_string(),
        Value::Str(s) => format!("{s:?}"),
        Value::Unit => "()".to_string(),
        Value::Duration(ms) => format_duration(*ms),
        Value::Option(None) => "None".to_string(),
        Value::Option(Some(v)) => format!("Some({})", show(v)),
        Value::List(items) => {
            let inner: Vec<String> = items.iter().map(show).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Struct { name, fields } => {
            let inner: Vec<String> =
                fields.iter().map(|(k, v)| format!("{k}: {}", show(v))).collect();
            format!("{name}({})", inner.join(", "))
        }
        Value::Enum { variant, fields, .. } => {
            if fields.is_empty() {
                variant.to_string()
            } else {
                let inner: Vec<String> =
                    fields.iter().map(|(k, v)| format!("{k}: {}", show(v))).collect();
                format!("{variant}({})", inner.join(", "))
            }
        }
        Value::Tag(name) => format!("Type<{name}>"),
        Value::Schedule(s) => {
            let kind = match s.kind {
                ScheduleKind::Exponential => "exponential",
                ScheduleKind::Fixed => "fixed",
            };
            match s.max_retries {
                Some(n) => {
                    format!("schedule.{kind}({}) |> schedule.upTo({n})", format_duration(s.base_ms))
                }
                None => format!("schedule.{kind}({})", format_duration(s.base_ms)),
            }
        }
        Value::MutMap(map) => format!("MutMap(size: {})", map.borrow().len()),
        Value::FuncRef(decl) => format!("<function {}>", decl.name),
        Value::Ctor(name) => format!("<constructor {name}>"),
        Value::Builtin(name) => format!("<builtin {name}>"),
        Value::Closure { .. } => "<lambda>".to_string(),
        Value::Service(instance) => format!("<service {}>", instance.service),
        Value::Thunk(_) => "<lazy>".to_string(),
    }
}

/// String-interpolation rendering: strings stay raw, everything else `show`s.
fn display(value: &Value) -> String {
    match value {
        Value::Str(s) => s.to_string(),
        other => show(other),
    }
}

fn format_float(f: f64) -> String {
    if f.fract() == 0.0 && f.is_finite() {
        format!("{f:.1}")
    } else {
        f.to_string()
    }
}

fn format_duration(ms: i64) -> String {
    if ms % 3_600_000 == 0 && ms != 0 {
        format!("{}.hours", ms / 3_600_000)
    } else if ms % 60_000 == 0 && ms != 0 {
        format!("{}.minutes", ms / 60_000)
    } else if ms % 1000 == 0 && ms != 0 {
        format!("{}.seconds", ms / 1000)
    } else {
        format!("{ms}.millis")
    }
}

fn map_key(value: &Value) -> String {
    // Canonical key encoding; a kind prefix avoids cross-type collisions.
    match value {
        Value::Str(s) => format!("s:{s}"),
        Value::Int(n) => format!("i:{n}"),
        Value::Bool(b) => format!("b:{b}"),
        other => format!("v:{}", encode(other)),
    }
}

// ---- JSON encode/decode -----------------------------------------------------

pub fn encode(value: &Value) -> String {
    let mut out = String::new();
    encode_into(value, &mut out);
    out
}

fn encode_into(value: &Value, out: &mut String) {
    match value {
        Value::Int(n) => {
            let _ = write!(out, "{n}");
        }
        Value::Float(f) => {
            let _ = write!(out, "{f}");
        }
        Value::Bool(b) => {
            let _ = write!(out, "{b}");
        }
        Value::Str(s) => encode_json_string(s, out),
        Value::Unit => out.push_str("null"),
        Value::Duration(ms) => {
            let _ = write!(out, "{ms}");
        }
        Value::Option(None) => out.push_str("null"),
        Value::Option(Some(v)) => encode_into(v, out),
        Value::List(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                encode_into(item, out);
            }
            out.push(']');
        }
        Value::Struct { fields, .. } => {
            out.push('{');
            for (i, (k, v)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                encode_json_string(k, out);
                out.push(':');
                encode_into(v, out);
            }
            out.push('}');
        }
        Value::Enum { variant, fields, .. } => {
            out.push('{');
            encode_json_string("$variant", out);
            out.push(':');
            encode_json_string(variant, out);
            for (k, v) in fields.iter() {
                out.push(',');
                encode_json_string(k, out);
                out.push(':');
                encode_into(v, out);
            }
            out.push('}');
        }
        other => encode_json_string(&show(other), out),
    }
}

fn encode_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Decode a JSON object into a struct with the given fields.
fn decode_json<'a>(text: &str, type_name: &str, field_names: &[&str]) -> Result<Value<'a>, String> {
    let mut parser = JsonParser { bytes: text.as_bytes(), pos: 0 };
    let json = parser.parse_value()?;
    parser.skip_ws();
    if parser.pos < parser.bytes.len() {
        return Err("trailing characters after JSON value".to_string());
    }
    let Json::Object(entries) = json else {
        return Err(format!("expected a JSON object for `{type_name}`"));
    };
    let mut fields = Vec::with_capacity(field_names.len());
    for fname in field_names {
        match entries.iter().find(|(k, _)| k == fname) {
            Some((_, v)) => fields.push((fname.to_string(), json_to_value(v))),
            None => return Err(format!("missing field `{fname}` for `{type_name}`")),
        }
    }
    Ok(Value::Struct { name: Rc::new(type_name.to_string()), fields: Rc::new(fields) })
}

enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

fn json_to_value<'a>(json: &Json) -> Value<'a> {
    match json {
        Json::Null => Value::Option(None),
        Json::Bool(b) => Value::Bool(*b),
        Json::Int(n) => Value::Int(*n),
        Json::Float(f) => Value::Float(*f),
        Json::Str(s) => Value::Str(Rc::new(s.clone())),
        Json::Array(items) => Value::List(Rc::new(items.iter().map(json_to_value).collect())),
        Json::Object(entries) => Value::Struct {
            name: Rc::new("Object".to_string()),
            fields: Rc::new(entries.iter().map(|(k, v)| (k.clone(), json_to_value(v))).collect()),
        },
    }
}

struct JsonParser<'s> {
    bytes: &'s [u8],
    pos: usize,
}

impl<'s> JsonParser<'s> {
    fn skip_ws(&mut self) {
        while matches!(self.bytes.get(self.pos), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn parse_value(&mut self) -> Result<Json, String> {
        self.skip_ws();
        match self.bytes.get(self.pos) {
            Some(b'n') => self.expect_word("null", Json::Null),
            Some(b't') => self.expect_word("true", Json::Bool(true)),
            Some(b'f') => self.expect_word("false", Json::Bool(false)),
            Some(b'"') => self.parse_string().map(Json::Str),
            Some(b'[') => {
                self.pos += 1;
                let mut items = Vec::new();
                self.skip_ws();
                if self.bytes.get(self.pos) == Some(&b']') {
                    self.pos += 1;
                    return Ok(Json::Array(items));
                }
                loop {
                    items.push(self.parse_value()?);
                    self.skip_ws();
                    match self.bytes.get(self.pos) {
                        Some(b',') => self.pos += 1,
                        Some(b']') => {
                            self.pos += 1;
                            return Ok(Json::Array(items));
                        }
                        _ => return Err("expected `,` or `]` in JSON array".to_string()),
                    }
                }
            }
            Some(b'{') => {
                self.pos += 1;
                let mut entries = Vec::new();
                self.skip_ws();
                if self.bytes.get(self.pos) == Some(&b'}') {
                    self.pos += 1;
                    return Ok(Json::Object(entries));
                }
                loop {
                    self.skip_ws();
                    let key = self.parse_string()?;
                    self.skip_ws();
                    if self.bytes.get(self.pos) != Some(&b':') {
                        return Err("expected `:` in JSON object".to_string());
                    }
                    self.pos += 1;
                    let value = self.parse_value()?;
                    entries.push((key, value));
                    self.skip_ws();
                    match self.bytes.get(self.pos) {
                        Some(b',') => self.pos += 1,
                        Some(b'}') => {
                            self.pos += 1;
                            return Ok(Json::Object(entries));
                        }
                        _ => return Err("expected `,` or `}` in JSON object".to_string()),
                    }
                }
            }
            Some(c) if c.is_ascii_digit() || *c == b'-' => self.parse_number(),
            _ => Err("unexpected character in JSON".to_string()),
        }
    }

    fn expect_word(&mut self, word: &str, value: Json) -> Result<Json, String> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(format!("invalid JSON literal (expected `{word}`)"))
        }
    }

    fn parse_string(&mut self) -> Result<String, String> {
        if self.bytes.get(self.pos) != Some(&b'"') {
            return Err("expected a JSON string".to_string());
        }
        self.pos += 1;
        let mut out = String::new();
        loop {
            match self.bytes.get(self.pos) {
                None => return Err("unterminated JSON string".to_string()),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    match self.bytes.get(self.pos) {
                        Some(b'n') => out.push('\n'),
                        Some(b't') => out.push('\t'),
                        Some(b'r') => out.push('\r'),
                        Some(b'"') => out.push('"'),
                        Some(b'\\') => out.push('\\'),
                        Some(b'/') => out.push('/'),
                        Some(b'u') => {
                            let hex = self
                                .bytes
                                .get(self.pos + 1..self.pos + 5)
                                .and_then(|h| std::str::from_utf8(h).ok())
                                .and_then(|h| u32::from_str_radix(h, 16).ok())
                                .and_then(char::from_u32);
                            match hex {
                                Some(c) => {
                                    out.push(c);
                                    self.pos += 4;
                                }
                                None => return Err("invalid \\u escape in JSON".to_string()),
                            }
                        }
                        _ => return Err("invalid escape in JSON string".to_string()),
                    }
                    self.pos += 1;
                }
                Some(_) => {
                    let start = self.pos;
                    self.pos += 1;
                    while self.bytes.get(self.pos).is_some_and(|b| (b & 0xC0) == 0x80) {
                        self.pos += 1;
                    }
                    out.push_str(&String::from_utf8_lossy(&self.bytes[start..self.pos]));
                }
            }
        }
    }

    fn parse_number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        if self.bytes.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        let mut is_float = false;
        while let Some(c) = self.bytes.get(self.pos) {
            match c {
                b'0'..=b'9' => self.pos += 1,
                b'.' | b'e' | b'E' | b'+' | b'-' => {
                    is_float = true;
                    self.pos += 1;
                }
                _ => break,
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| "invalid number".to_string())?;
        if is_float {
            text.parse().map(Json::Float).map_err(|_| "invalid number".to_string())
        } else {
            text.parse().map(Json::Int).map_err(|_| "invalid number".to_string())
        }
    }
}

// ---- shader registry (interpreter gfx support) -------------------------------

#[cfg(feature = "gfx")]
const GFX_VERTEX_SHADER: &str = r#"#version 100
attribute vec3 position;
attribute vec2 texcoord;
attribute vec4 color0;
varying lowp vec4 color;
varying vec2 uv;
uniform mat4 Model;
uniform mat4 Projection;
void main() {
    gl_Position = Projection * Model * vec4(position, 1);
    color = color0 / 255.0;
    uv = texcoord;
}"#;

#[cfg(feature = "gfx")]
thread_local! {
    static MATERIALS: RefCell<Vec<macroquad::material::Material>> =
        const { RefCell::new(Vec::new()) };
}
