//! Type and effect checking.
//!
//! Value types are inferred by unification (`types.rs`). The two effect rows
//! — errors (`!`) and capabilities (`uses`) — are finite name-sets computed by
//! a monotone fixpoint over the call graph: each pass re-infers every function
//! body using the previous pass's row summaries until nothing changes. `catch`
//! subtracts error names; `provide` subtracts capability names. Declared rows
//! are validated against (and unioned with) inferred rows at the end.

use std::collections::{BTreeSet, HashMap};

use crate::ast::*;
use crate::diag::Diagnostic;
use crate::span::Span;
use crate::types::{FuncType, Type, TypeCtx};
use std::rc::Rc;

pub const DURATION_SUFFIXES: [(&str, i64); 5] =
    [("millis", 1), ("seconds", 1000), ("minutes", 60_000), ("hours", 3_600_000), ("days", 86_400_000)];

/// Builtin error raised by `decode`.
pub const DECODE_ERROR: &str = "DecodeError";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefKind {
    Func,
    Error,
    Type,
    Service,
    Impl,
    Method,
}

#[derive(Debug, Clone)]
pub struct DefInfo {
    pub name: String,
    pub span: Span,
    pub kind: DefKind,
    pub detail: String,
}

/// Side tables for tooling (hover, go-to-definition, completion).
#[derive(Debug, Default)]
pub struct CheckInfo {
    /// Hover text keyed by span; innermost-containing span wins.
    pub hovers: Vec<(Span, String)>,
    pub defs: Vec<DefInfo>,
    /// (use span, definition span)
    pub refs: Vec<(Span, Span)>,
    /// Resolved type of every checked expression, keyed by span — consumed by
    /// the LLVM backend.
    pub expr_types: HashMap<(u32, u32), CType>,
    /// Effect-row facts per function and per service method.
    pub facts: Facts,
}

/// Codegen-facing view of a fully resolved type. Unresolved type variables
/// (values that are never used) default to `Int`.
#[derive(Debug, Clone, PartialEq)]
pub enum CType {
    Int,
    Float,
    Bool,
    Str,
    Unit,
    Duration,
    Schedule,
    Option(Box<CType>),
    List(Box<CType>),
    Named(String),
    ErrorTy(String),
    Service(String),
    Tag(String),
    MutMap(Box<CType>, Box<CType>),
    Func,
}

/// Effective effect rows (declared ∪ inferred), sorted, for codegen.
#[derive(Debug, Clone, Default)]
pub struct RowFact {
    pub errors: Vec<String>,
    pub caps: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Facts {
    pub funcs: HashMap<String, RowFact>,
    /// Keyed by (service, method): the union row across all implementations.
    pub methods: HashMap<(String, String), RowFact>,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Rows {
    errors: BTreeSet<String>,
    caps: BTreeSet<String>,
}

impl Rows {
    fn merge(&mut self, other: &Rows) {
        self.errors.extend(other.errors.iter().cloned());
        self.caps.extend(other.caps.iter().cloned());
    }
}

struct StructInfo {
    fields: Vec<(String, Type)>,
    name_span: Span,
}

struct MethodInfo {
    params: Vec<Type>,
    ret: Type,
    declared_errors: BTreeSet<String>,
    name_span: Span,
}

struct ServiceInfo {
    methods: Vec<(String, MethodInfo)>,
    name_span: Span,
}

struct ImplInfo {
    service: String,
    fields: Vec<(String, Type)>,
    name_span: Span,
}

struct FuncInfo {
    params: Vec<Type>,
    param_names: Vec<String>,
    lazy: Vec<bool>,
    ret: Type,
    declared_errors: Option<BTreeSet<String>>,
    declared_caps: Option<BTreeSet<String>>,
    name_span: Span,
}

pub fn check(program: &Program, diagnostics: &mut Vec<Diagnostic>) -> CheckInfo {
    let mut checker = Checker::new(program);
    checker.collect_decls();

    // Fixpoint over effect rows; value-type unification state persists across
    // passes (re-unification is idempotent). Diagnostics from warm-up passes
    // are discarded; the final pass reports.
    for _ in 0..20 {
        checker.changed = false;
        checker.record_info = false;
        checker.diags.clear();
        checker.run_pass();
        if !checker.changed {
            break;
        }
    }
    checker.diags.clear();
    checker.record_info = true;
    checker.run_pass();
    checker.validate_declared_rows();
    checker.record_def_details();
    checker.record_facts();

    diagnostics.append(&mut checker.diags.clone());
    checker.info
}

struct Checker<'a> {
    program: &'a Program,
    ctx: TypeCtx,

    errors_decl: HashMap<String, StructInfo>,
    types_decl: HashMap<String, StructInfo>,
    services: HashMap<String, ServiceInfo>,
    impls: HashMap<String, ImplInfo>,
    funcs: HashMap<String, FuncInfo>,

    func_rows: HashMap<String, Rows>,
    method_rows: HashMap<(String, String), Rows>,
    impl_field_rows: HashMap<String, Rows>,

    diags: Vec<Diagnostic>,
    record_info: bool,
    info: CheckInfo,
    changed: bool,

    scopes: Vec<HashMap<String, Type>>,
    row_stack: Vec<Rows>,
}

impl<'a> Checker<'a> {
    fn new(program: &'a Program) -> Checker<'a> {
        let mut checker = Checker {
            program,
            ctx: TypeCtx::default(),
            errors_decl: HashMap::new(),
            types_decl: HashMap::new(),
            services: HashMap::new(),
            impls: HashMap::new(),
            funcs: HashMap::new(),
            func_rows: HashMap::new(),
            method_rows: HashMap::new(),
            impl_field_rows: HashMap::new(),
            diags: Vec::new(),
            record_info: false,
            info: CheckInfo::default(),
            changed: false,
            scopes: Vec::new(),
            row_stack: Vec::new(),
        };
        // Builtin error available to every program.
        checker.errors_decl.insert(
            DECODE_ERROR.to_string(),
            StructInfo {
                fields: vec![("message".into(), Type::Str)],
                name_span: Span::default(),
            },
        );
        checker
    }

    fn error(&mut self, span: Span, message: impl Into<String>) {
        self.diags.push(Diagnostic::error(span, message));
    }

    fn warn(&mut self, span: Span, message: impl Into<String>) {
        self.diags.push(Diagnostic::warning(span, message));
    }

    // ---- declaration collection -----------------------------------------

    fn collect_decls(&mut self) {
        // First sweep: names only, so types can reference each other.
        for decl in &self.program.decls {
            let (name, span, kind) = match decl {
                Decl::Error(d) => (&d.name, d.name_span, DefKind::Error),
                Decl::Type(d) => (&d.name, d.name_span, DefKind::Type),
                Decl::Service(d) => (&d.name, d.name_span, DefKind::Service),
                Decl::Impl(d) => (&d.name, d.name_span, DefKind::Impl),
                Decl::Func(d) => (&d.name, d.name_span, DefKind::Func),
            };
            let dup = match kind {
                DefKind::Error => self.errors_decl.contains_key(name),
                DefKind::Type => self.types_decl.contains_key(name),
                DefKind::Service => self.services.contains_key(name),
                DefKind::Impl | DefKind::Func | DefKind::Method => {
                    self.impls.contains_key(name) || self.funcs.contains_key(name)
                }
            };
            if dup {
                self.error(span, format!("`{name}` is declared more than once"));
                continue;
            }
            match kind {
                DefKind::Error | DefKind::Type => {
                    let table = if kind == DefKind::Error {
                        &mut self.errors_decl
                    } else {
                        &mut self.types_decl
                    };
                    table.insert(
                        name.clone(),
                        StructInfo { fields: Vec::new(), name_span: span },
                    );
                }
                DefKind::Service => {
                    self.services.insert(
                        name.clone(),
                        ServiceInfo { methods: Vec::new(), name_span: span },
                    );
                }
                _ => {}
            }
        }

        // Second sweep: full signatures.
        for decl in &self.program.decls {
            match decl {
                Decl::Error(d) | Decl::Type(d) => {
                    let mut fields = Vec::new();
                    let mut tyvars = HashMap::new();
                    for field in &d.fields {
                        let ty = match &field.ty {
                            Some(t) => self.resolve_type_expr(t, &mut tyvars),
                            None => self.ctx.fresh(),
                        };
                        fields.push((field.name.clone(), ty));
                    }
                    let table = if matches!(decl, Decl::Error(_)) {
                        &mut self.errors_decl
                    } else {
                        &mut self.types_decl
                    };
                    if let Some(info) = table.get_mut(&d.name) {
                        info.fields = fields;
                    }
                }
                Decl::Service(d) => {
                    let mut methods = Vec::new();
                    for m in &d.methods {
                        let mut tyvars = HashMap::new();
                        let params: Vec<Type> = m
                            .sig
                            .params
                            .iter()
                            .map(|p| match &p.ty {
                                Some(t) => self.resolve_type_expr(t, &mut tyvars),
                                None => self.ctx.fresh(),
                            })
                            .collect();
                        let ret = match &m.sig.ret {
                            Some(t) => self.resolve_type_expr(t, &mut tyvars),
                            None => self.ctx.fresh(),
                        };
                        let declared_errors = self.resolve_error_list(m.sig.errors.as_deref());
                        methods.push((
                            m.name.clone(),
                            MethodInfo {
                                params,
                                ret,
                                declared_errors,
                                name_span: m.name_span,
                            },
                        ));
                    }
                    if let Some(info) = self.services.get_mut(&d.name) {
                        info.methods = methods;
                    }
                }
                Decl::Impl(d) => {
                    if !self.services.contains_key(&d.service) {
                        self.error(
                            d.service_span,
                            format!("unknown service `{}`", d.service),
                        );
                    }
                    let fields: Vec<(String, Type)> =
                        d.fields.iter().map(|(name, _, _)| (name.clone(), self.ctx.fresh())).collect();
                    self.impls.insert(
                        d.name.clone(),
                        ImplInfo { service: d.service.clone(), fields, name_span: d.name_span },
                    );
                }
                Decl::Func(d) => {
                    let mut tyvars = HashMap::new();
                    let params: Vec<Type> = d
                        .sig
                        .params
                        .iter()
                        .map(|p| match &p.ty {
                            Some(t) => self.resolve_type_expr(t, &mut tyvars),
                            None => self.ctx.fresh(),
                        })
                        .collect();
                    let ret = match &d.sig.ret {
                        Some(t) => self.resolve_type_expr(t, &mut tyvars),
                        None => self.ctx.fresh(),
                    };
                    let declared_errors = d
                        .sig
                        .errors
                        .as_ref()
                        .map(|list| self.resolve_error_list(Some(list)));
                    let declared_caps = d.sig.uses.as_ref().map(|list| {
                        let mut set = BTreeSet::new();
                        for (name, span) in list {
                            if self.services.contains_key(name) {
                                set.insert(name.clone());
                            } else {
                                self.error(*span, format!("unknown service `{name}` in `uses`"));
                            }
                        }
                        set
                    });
                    self.funcs.insert(
                        d.name.clone(),
                        FuncInfo {
                            params,
                            param_names: d.sig.params.iter().map(|p| p.name.clone()).collect(),
                            lazy: d.sig.params.iter().map(|p| p.lazy).collect(),
                            ret,
                            declared_errors,
                            declared_caps,
                            name_span: d.name_span,
                        },
                    );
                }
            }
        }
    }

    fn resolve_error_list(&mut self, list: Option<&[(String, Span)]>) -> BTreeSet<String> {
        let mut set = BTreeSet::new();
        for (name, span) in list.unwrap_or(&[]) {
            if self.errors_decl.contains_key(name) {
                set.insert(name.clone());
            } else {
                self.error(*span, format!("unknown error type `{name}`"));
            }
        }
        set
    }

    fn resolve_type_expr(&mut self, ty: &TypeExpr, tyvars: &mut HashMap<String, Type>) -> Type {
        match ty {
            TypeExpr::Name(name, span) => match name.as_str() {
                "Int" => Type::Int,
                "Float" => Type::Float,
                "Bool" => Type::Bool,
                "String" => Type::Str,
                "Unit" => Type::Unit,
                "Duration" => Type::Duration,
                "Schedule" => Type::Schedule,
                _ if self.types_decl.contains_key(name) => Type::Named(name.clone()),
                _ if self.errors_decl.contains_key(name) => Type::Error(name.clone()),
                _ if self.services.contains_key(name) => Type::Service(name.clone()),
                _ if !is_upper(name) => {
                    // Lowercase: a type parameter; same name = same variable
                    // within one signature.
                    tyvars.entry(name.clone()).or_insert_with(|| self.ctx.fresh()).clone()
                }
                _ => {
                    self.error(*span, format!("unknown type `{name}`"));
                    Type::Unknown
                }
            },
            TypeExpr::Option(inner, _) => {
                Type::Option(Box::new(self.resolve_type_expr(inner, tyvars)))
            }
            TypeExpr::List(inner, _) => {
                Type::List(Box::new(self.resolve_type_expr(inner, tyvars)))
            }
        }
    }

    // ---- fixpoint pass ----------------------------------------------------

    fn run_pass(&mut self) {
        for decl in &self.program.decls {
            match decl {
                Decl::Func(d) => self.check_func(d),
                Decl::Impl(d) => self.check_impl(d),
                _ => {}
            }
        }
    }

    fn check_func(&mut self, d: &FuncDecl) {
        let Some(info) = self.funcs.get(&d.name) else { return };
        let params = info.params.clone();
        let ret = info.ret.clone();

        let mut scope = HashMap::new();
        for (param, ty) in d.sig.params.iter().zip(params.iter()) {
            scope.insert(param.name.clone(), ty.clone());
            if self.record_info {
                let rendered = self.render(ty);
                self.info.hovers.push((param.span, format!("{} : {}", param.name, rendered)));
            }
        }
        self.scopes = vec![scope];
        self.row_stack = vec![Rows::default()];

        let body_ty = self.check_block(&d.body);
        self.unify_at(&ret, &body_ty, last_span(&d.body), "function body");

        let rows = self.row_stack.pop().unwrap_or_default();
        let prev = self.func_rows.get(&d.name);
        if prev != Some(&rows) {
            self.changed = true;
            self.func_rows.insert(d.name.clone(), rows);
        }

        if self.record_info {
            let sig = self.render_func_signature(&d.name);
            self.info.hovers.push((d.name_span, sig));
        }
    }

    fn check_impl(&mut self, d: &ImplDecl) {
        let Some(info) = self.impls.get(&d.name) else { return };
        let field_types: Vec<(String, Type)> = info.fields.clone();
        let service = info.service.clone();

        // Field initializers.
        let mut field_rows = Rows::default();
        let mut scope = HashMap::new();
        for ((name, _span, value), (_, ty)) in d.fields.iter().zip(field_types.iter()) {
            self.scopes = vec![scope.clone()];
            self.row_stack = vec![Rows::default()];
            let value_ty = self.check_expr(value);
            self.unify_at(ty, &value_ty, value.span, "field initializer");
            field_rows.merge(&self.row_stack.pop().unwrap_or_default());
            scope.insert(name.clone(), ty.clone());
        }
        if self.impl_field_rows.get(&d.name) != Some(&field_rows) {
            self.changed = true;
            self.impl_field_rows.insert(d.name.clone(), field_rows);
        }

        // Methods, unified against the service signature.
        for method in &d.methods {
            let sig_info = self.services.get(&service).and_then(|s| {
                s.methods.iter().find(|(n, _)| n == &method.name).map(|(_, m)| {
                    (m.params.clone(), m.ret.clone())
                })
            });
            let Some((sig_params, sig_ret)) = sig_info else {
                if self.services.contains_key(&service) {
                    self.error(
                        method.name_span,
                        format!("service `{service}` has no method `{}`", method.name),
                    );
                }
                continue;
            };
            if sig_params.len() != method.sig.params.len() {
                self.error(
                    method.name_span,
                    format!(
                        "method `{}` has {} parameter(s) but service `{service}` declares {}",
                        method.name,
                        method.sig.params.len(),
                        sig_params.len()
                    ),
                );
                continue;
            }
            let mut method_scope = scope.clone();
            let mut tyvars = HashMap::new();
            for (param, sig_ty) in method.sig.params.iter().zip(sig_params.iter()) {
                if let Some(t) = &param.ty {
                    let annotated = self.resolve_type_expr(t, &mut tyvars);
                    self.unify_at(sig_ty, &annotated, param.span, "parameter annotation");
                }
                method_scope.insert(param.name.clone(), sig_ty.clone());
            }
            self.scopes = vec![method_scope];
            self.row_stack = vec![Rows::default()];
            let body_ty = self.check_block(&method.body);
            self.unify_at(&sig_ret, &body_ty, last_span(&method.body), "method body");
            let rows = self.row_stack.pop().unwrap_or_default();

            let key = (service.clone(), method.name.clone());
            let entry = self.method_rows.entry(key).or_default();
            let before = entry.clone();
            entry.merge(&rows);
            if *entry != before {
                self.changed = true;
            }

            if self.record_info {
                let detail = format!("{} :: {} method of {service}", method.name, d.name);
                self.info.hovers.push((method.name_span, detail));
            }
        }
    }

    // ---- rows helpers ------------------------------------------------------

    fn add_error_row(&mut self, name: &str) {
        if let Some(top) = self.row_stack.last_mut() {
            top.errors.insert(name.to_string());
        }
    }

    fn add_cap_row(&mut self, name: &str) {
        if let Some(top) = self.row_stack.last_mut() {
            top.caps.insert(name.to_string());
        }
    }

    fn merge_rows(&mut self, rows: &Rows) {
        if let Some(top) = self.row_stack.last_mut() {
            top.merge(rows);
        }
    }

    /// Check `f` with a private row scope; returns (result, rows of f).
    fn with_rows<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> (T, Rows) {
        self.row_stack.push(Rows::default());
        let result = f(self);
        let rows = self.row_stack.pop().unwrap_or_default();
        (result, rows)
    }

    /// Rows callers observe when calling `name`: declared ∪ inferred.
    fn func_effective_rows(&self, name: &str) -> Rows {
        let mut rows = self.func_rows.get(name).cloned().unwrap_or_default();
        if let Some(info) = self.funcs.get(name) {
            if let Some(declared) = &info.declared_errors {
                rows.errors.extend(declared.iter().cloned());
            }
            if let Some(declared) = &info.declared_caps {
                rows.caps.extend(declared.iter().cloned());
            }
        }
        rows
    }

    fn method_effective_rows(&self, service: &str, method: &str) -> Rows {
        let mut rows = self
            .method_rows
            .get(&(service.to_string(), method.to_string()))
            .cloned()
            .unwrap_or_default();
        if let Some(s) = self.services.get(service) {
            if let Some((_, m)) = s.methods.iter().find(|(n, _)| n == method) {
                rows.errors.extend(m.declared_errors.iter().cloned());
            }
        }
        rows
    }

    // ---- statements / blocks ------------------------------------------------

    fn check_block(&mut self, block: &Block) -> Type {
        self.scopes.push(HashMap::new());
        let mut result = Type::Unit;
        let count = block.stmts.len();
        for (i, stmt) in block.stmts.iter().enumerate() {
            let last = i + 1 == count;
            match stmt {
                Stmt::Expr(expr) => {
                    let ty = self.check_expr(expr);
                    result = if last { ty } else { Type::Unit };
                }
                Stmt::Bind { ty, name, name_span, value } => {
                    let value_ty = self.check_expr(value);
                    let bound_ty = match ty {
                        Some(annotation) => {
                            let mut tyvars = HashMap::new();
                            let annotated = self.resolve_type_expr(annotation, &mut tyvars);
                            self.unify_at(&annotated, &value_ty, value.span, "binding");
                            annotated
                        }
                        None => value_ty,
                    };
                    if self.record_info {
                        let rendered = self.render(&bound_ty);
                        self.info.hovers.push((*name_span, format!("{name} : {rendered}")));
                    }
                    self.scopes.last_mut().unwrap().insert(name.clone(), bound_ty);
                    result = Type::Unit;
                }
                Stmt::Acquire { service, service_span, name, name_span } => {
                    if service == "<error>" {
                        // Parser already reported.
                    } else if self.services.contains_key(service) {
                        self.add_cap_row(service);
                        if self.record_info {
                            self.info.hovers.push((*name_span, format!("{name} : {service}")));
                            let def_span = self.services[service].name_span;
                            self.info.refs.push((*service_span, def_span));
                        }
                    } else {
                        self.error(
                            *service_span,
                            format!("unknown service `{service}` (capability bindings look like `Cache cache`)"),
                        );
                    }
                    self.scopes
                        .last_mut()
                        .unwrap()
                        .insert(name.clone(), Type::Service(service.clone()));
                    result = Type::Unit;
                }
            }
        }
        self.scopes.pop();
        result
    }

    // ---- expressions ---------------------------------------------------------

    fn check_expr(&mut self, expr: &Expr) -> Type {
        let ty = self.check_expr_inner(expr);
        if self.record_info {
            let resolved = self.ctype(&ty);
            self.info.expr_types.insert((expr.span.start, expr.span.end), resolved);
        }
        ty
    }

    /// Resolve a checker type into the codegen-facing representation.
    fn ctype(&self, ty: &Type) -> CType {
        match self.ctx.resolve(ty) {
            Type::Int => CType::Int,
            Type::Float => CType::Float,
            Type::Bool => CType::Bool,
            Type::Str => CType::Str,
            Type::Unit => CType::Unit,
            Type::Duration => CType::Duration,
            Type::Schedule => CType::Schedule,
            Type::Option(t) => CType::Option(Box::new(self.ctype(&t))),
            Type::List(t) => CType::List(Box::new(self.ctype(&t))),
            Type::Named(n) => CType::Named(n),
            Type::Error(n) => CType::ErrorTy(n),
            Type::Service(n) => CType::Service(n),
            Type::Tag(n) => CType::Tag(n),
            Type::MutMap(k, v) => {
                CType::MutMap(Box::new(self.ctype(&k)), Box::new(self.ctype(&v)))
            }
            Type::Func(_) => CType::Func,
            // Unconstrained or error-recovery types default to Int.
            Type::Var(_) | Type::Unknown => CType::Int,
        }
    }

    /// Export effective rows for codegen (called once, after the final pass).
    fn record_facts(&mut self) {
        for name in self.funcs.keys().cloned().collect::<Vec<_>>() {
            let rows = self.func_effective_rows(&name);
            self.info.facts.funcs.insert(
                name,
                RowFact {
                    errors: rows.errors.iter().cloned().collect(),
                    caps: rows.caps.iter().cloned().collect(),
                },
            );
        }
        let pairs: Vec<(String, String)> = self
            .services
            .iter()
            .flat_map(|(s, info)| {
                info.methods.iter().map(move |(m, _)| (s.clone(), m.clone()))
            })
            .collect();
        for (service, method) in pairs {
            let rows = self.method_effective_rows(&service, &method);
            self.info.facts.methods.insert(
                (service, method),
                RowFact {
                    errors: rows.errors.iter().cloned().collect(),
                    caps: rows.caps.iter().cloned().collect(),
                },
            );
        }
    }

    fn check_expr_inner(&mut self, expr: &Expr) -> Type {
        match &expr.kind {
            ExprKind::Int(_) => Type::Int,
            ExprKind::Float(_) => Type::Float,
            ExprKind::Bool(_) => Type::Bool,
            ExprKind::Str(pieces) => {
                for piece in pieces {
                    if let StrPiece::Expr(e) = piece {
                        self.check_expr(e);
                    }
                }
                Type::Str
            }
            ExprKind::Var(name) => self.check_var(name, expr.span),
            ExprKind::List(items) => {
                let elem = self.ctx.fresh();
                for item in items {
                    let ty = self.check_expr(item);
                    self.unify_at(&elem, &ty, item.span, "list element");
                }
                Type::List(Box::new(elem))
            }
            ExprKind::Call { callee, args } => {
                let arg_refs: Vec<&Expr> = args.iter().collect();
                self.check_call(callee, &arg_refs, expr.span)
            }
            ExprKind::Method { recv, name, name_span, args } => {
                let arg_refs: Vec<&Expr> = args.iter().collect();
                self.check_method(recv, name, *name_span, &arg_refs, expr.span)
            }
            ExprKind::Field { recv, name, name_span } => {
                self.check_field(recv, name, *name_span)
            }
            ExprKind::Binary { op, lhs, rhs } => self.check_binary(*op, lhs, rhs, expr.span),
            ExprKind::Unary { op, expr: inner } => {
                let ty = self.check_expr(inner);
                match op {
                    UnOp::Not => {
                        self.unify_at(&Type::Bool, &ty, inner.span, "operand of `!`");
                        Type::Bool
                    }
                    UnOp::Neg => {
                        let resolved = self.ctx.resolve(&ty);
                        match resolved {
                            Type::Int | Type::Float | Type::Var(_) | Type::Unknown => ty,
                            other => {
                                let rendered = self.render(&other);
                                self.error(
                                    inner.span,
                                    format!("cannot negate a value of type {rendered}"),
                                );
                                Type::Unknown
                            }
                        }
                    }
                }
            }
            ExprKind::Pipe { lhs, target } => self.check_pipe(lhs, target, expr.span),
            ExprKind::Match { scrutinee, arms } => self.check_match(scrutinee, arms),
            ExprKind::Fail { error } => {
                let ty = self.check_expr(error);
                match self.ctx.resolve(&ty) {
                    Type::Error(name) => self.add_error_row(&name),
                    Type::Unknown | Type::Var(_) => {}
                    other => {
                        let rendered = self.render(&other);
                        self.error(
                            error.span,
                            format!("`fail` needs an error value, found {rendered}"),
                        );
                    }
                }
                // `fail` never produces a value; it unifies with anything.
                self.ctx.fresh()
            }
            ExprKind::Provide { impls, body } => self.check_provide(impls, body),
            ExprKind::If { cond, then_block, else_branch } => {
                let cond_ty = self.check_expr(cond);
                self.unify_at(&Type::Bool, &cond_ty, cond.span, "`if` condition");
                let then_ty = self.check_block(then_block);
                match else_branch {
                    Some(else_expr) => {
                        let else_ty = self.check_expr(else_expr);
                        self.unify_at(&then_ty, &else_ty, else_expr.span, "`else` branch");
                        then_ty
                    }
                    None => Type::Unit,
                }
            }
            ExprKind::Block(block) => self.check_block(block),
            ExprKind::Lambda { params, body } => {
                let mut scope = HashMap::new();
                let mut tyvars = HashMap::new();
                let mut param_types = Vec::new();
                for param in params {
                    let ty = match &param.ty {
                        Some(t) => self.resolve_type_expr(t, &mut tyvars),
                        None => self.ctx.fresh(),
                    };
                    scope.insert(param.name.clone(), ty.clone());
                    param_types.push(ty);
                }
                self.scopes.push(scope);
                let (ret, rows) = self.with_rows(|s| s.check_expr(body));
                self.scopes.pop();
                Type::Func(Rc::new(FuncType {
                    params: param_types,
                    ret,
                    errors: rows.errors,
                    caps: rows.caps,
                }))
            }
        }
    }

    fn check_var(&mut self, name: &str, span: Span) -> Type {
        if name == "<error>" {
            return Type::Unknown;
        }
        for scope in self.scopes.iter().rev() {
            if let Some(ty) = scope.get(name) {
                let ty = ty.clone();
                if self.record_info {
                    let rendered = self.render(&ty);
                    self.info.hovers.push((span, format!("{name} : {rendered}")));
                }
                return ty;
            }
        }
        if let Some(info) = self.funcs.get(name) {
            let rows = self.func_effective_rows(name);
            let func = Type::Func(Rc::new(FuncType {
                params: info.params.clone(),
                ret: info.ret.clone(),
                errors: rows.errors,
                caps: rows.caps,
            }));
            if self.record_info {
                let def_span = info.name_span;
                self.info.refs.push((span, def_span));
                let sig = self.render_func_signature(name);
                self.info.hovers.push((span, sig));
            }
            return func;
        }
        if name == "None" {
            return Type::Option(Box::new(self.ctx.fresh()));
        }
        if name == "Some" {
            let a = self.ctx.fresh();
            return Type::Func(Rc::new(FuncType {
                params: vec![a.clone()],
                ret: Type::Option(Box::new(a)),
                errors: BTreeSet::new(),
                caps: BTreeSet::new(),
            }));
        }
        if let Some(info) = self.errors_decl.get(name) {
            let def_span = info.name_span;
            let func = Type::Func(Rc::new(FuncType {
                params: info.fields.iter().map(|(_, t)| t.clone()).collect(),
                ret: Type::Error(name.to_string()),
                errors: BTreeSet::new(),
                caps: BTreeSet::new(),
            }));
            if self.record_info {
                self.info.refs.push((span, def_span));
            }
            return func;
        }
        if let Some(info) = self.types_decl.get(name) {
            // A bare type name is a type tag (`decode(raw, User)`); calling it
            // constructs a value — `check_call` handles that case directly.
            if self.record_info {
                self.info.refs.push((span, info.name_span));
            }
            return Type::Tag(name.to_string());
        }
        if self.services.contains_key(name) {
            self.error(
                span,
                format!("`{name}` is a service; bind it as a capability first: `{name} {}`",
                    name.to_lowercase()),
            );
            return Type::Unknown;
        }
        if self.impls.contains_key(name) {
            self.error(
                span,
                format!("`{name}` is an implementation; use it with `provide {name} {{ ... }}`"),
            );
            return Type::Unknown;
        }
        if let Some(ty) = self.builtin_value_type(name) {
            return ty;
        }
        self.error(span, format!("unknown name `{name}`"));
        Type::Unknown
    }

    /// Builtins usable as bare values (passed to higher-order functions).
    fn builtin_value_type(&mut self, name: &str) -> Option<Type> {
        let func = |params: Vec<Type>, ret: Type| {
            Type::Func(Rc::new(FuncType {
                params,
                ret,
                errors: BTreeSet::new(),
                caps: BTreeSet::new(),
            }))
        };
        match name {
            "show" => {
                let a = self.ctx.fresh();
                Some(func(vec![a], Type::Str))
            }
            "encode" => {
                let a = self.ctx.fresh();
                Some(func(vec![a], Type::Str))
            }
            _ if BUILTIN_NAMES.contains(&name) => {
                // Other builtins need call-site special handling.
                None
            }
            _ => None,
        }
    }

    // ---- calls -----------------------------------------------------------------

    fn check_call(&mut self, callee: &Expr, args: &[&Expr], span: Span) -> Type {
        // Builtin modules: `Schedule.exponential(...)`, `Gfx.rect(...)`.
        if let ExprKind::Field { recv, name, name_span } = &callee.kind {
            if let ExprKind::Var(module) = &recv.kind {
                if module == "Schedule" && !self.scope_has(module) {
                    return self.check_schedule_call(name, *name_span, args, span);
                }
                if module == "Gfx" && !self.scope_has(module) {
                    return self.check_gfx_call(name, *name_span, args, span);
                }
            }
        }
        if let ExprKind::Var(name) = &callee.kind {
            if !self.scope_has(name) {
                if let Some(ty) = self.check_builtin_call(name, callee.span, args, span) {
                    return ty;
                }
                // Error / type constructors.
                if let Some(fields) =
                    self.errors_decl.get(name).map(|i| i.fields.clone())
                {
                    if self.record_info {
                        let def_span = self.errors_decl[name].name_span;
                        self.info.refs.push((callee.span, def_span));
                    }
                    return self.check_ctor(name, &fields, args, span, Type::Error(name.clone()));
                }
                if let Some(fields) = self.types_decl.get(name).map(|i| i.fields.clone()) {
                    if self.record_info {
                        let def_span = self.types_decl[name].name_span;
                        self.info.refs.push((callee.span, def_span));
                    }
                    return self.check_ctor(name, &fields, args, span, Type::Named(name.clone()));
                }
            }
        }

        // General case: callee must be a function value.
        let callee_ty = self.check_expr(callee);
        match self.ctx.resolve(&callee_ty) {
            Type::Func(f) => {
                if f.params.len() != args.len() {
                    self.error(
                        span,
                        format!("expected {} argument(s), found {}", f.params.len(), args.len()),
                    );
                    return Type::Unknown;
                }
                for (param_ty, arg) in f.params.iter().zip(args.iter()) {
                    let arg_ty = self.check_expr(arg);
                    self.unify_at(param_ty, &arg_ty, arg.span, "argument");
                    self.add_func_arg_rows(&arg_ty);
                }
                self.merge_rows(&Rows { errors: f.errors.clone(), caps: f.caps.clone() });
                f.ret.clone()
            }
            Type::Unknown => {
                for arg in args {
                    self.check_expr(arg);
                }
                Type::Unknown
            }
            Type::Var(_) => {
                // Unresolved: assume a function of these args.
                let arg_types: Vec<Type> =
                    args.iter().map(|a| self.check_expr(a)).collect();
                let ret = self.ctx.fresh();
                let func = Type::Func(Rc::new(FuncType {
                    params: arg_types,
                    ret: ret.clone(),
                    errors: BTreeSet::new(),
                    caps: BTreeSet::new(),
                }));
                self.unify_at(&func, &callee_ty, callee.span, "call");
                ret
            }
            other => {
                let rendered = self.render(&other);
                self.error(callee.span, format!("{rendered} is not callable"));
                for arg in args {
                    self.check_expr(arg);
                }
                Type::Unknown
            }
        }
    }

    /// If an argument is function-typed, its rows flow to the caller (we
    /// conservatively assume the callee invokes it).
    fn add_func_arg_rows(&mut self, arg_ty: &Type) {
        if let Type::Func(f) = self.ctx.resolve(arg_ty) {
            self.merge_rows(&Rows { errors: f.errors.clone(), caps: f.caps.clone() });
        }
    }

    fn scope_has(&self, name: &str) -> bool {
        self.scopes.iter().any(|s| s.contains_key(name))
    }

    fn check_ctor(
        &mut self,
        name: &str,
        fields: &[(String, Type)],
        args: &[&Expr],
        span: Span,
        result: Type,
    ) -> Type {
        if args.len() != fields.len() {
            self.error(
                span,
                format!(
                    "`{name}` has {} field(s) but {} argument(s) were given",
                    fields.len(),
                    args.len()
                ),
            );
        }
        for ((_, field_ty), arg) in fields.iter().zip(args.iter()) {
            let arg_ty = self.check_expr(arg);
            self.unify_at(field_ty, &arg_ty, arg.span, "field");
        }
        for arg in args.iter().skip(fields.len()) {
            self.check_expr(arg);
        }
        result
    }

    fn check_schedule_call(
        &mut self,
        name: &str,
        name_span: Span,
        args: &[&Expr],
        span: Span,
    ) -> Type {
        match name {
            "exponential" | "fixed" => {
                if args.len() != 1 {
                    self.error(span, format!("`Schedule.{name}` takes one Duration argument"));
                }
                if let Some(arg) = args.first() {
                    let ty = self.check_expr(arg);
                    self.unify_at(&Type::Duration, &ty, arg.span, "schedule base");
                }
                Type::Schedule
            }
            _ => {
                self.error(
                    name_span,
                    format!("unknown schedule `Schedule.{name}` (try `exponential` or `fixed`)"),
                );
                for arg in args {
                    self.check_expr(arg);
                }
                Type::Schedule
            }
        }
    }

    /// The GL-backed graphics module. Signatures are (Int coordinates,
    /// 0–255 Int color channels); `run` takes the per-frame closure.
    fn check_gfx_call(
        &mut self,
        name: &str,
        name_span: Span,
        args: &[&Expr],
        span: Span,
    ) -> Type {
        // (param types, return type); String = Str, closure handled separately.
        let sig: Option<(Vec<Type>, Type)> = match name {
            "run" => None, // special-cased below
            "clear" => Some((vec![Type::Int; 3], Type::Unit)),
            "rect" => Some((vec![Type::Int; 8], Type::Unit)),
            "rectLines" => Some((vec![Type::Int; 9], Type::Unit)),
            "circle" => Some((vec![Type::Int; 7], Type::Unit)),
            "text" => Some((
                vec![Type::Str, Type::Int, Type::Int, Type::Int, Type::Int, Type::Int, Type::Int],
                Type::Unit,
            )),
            "textWidth" => Some((vec![Type::Str, Type::Int], Type::Int)),
            "mouseX" | "mouseY" => Some((vec![], Type::Int)),
            "mousePressed" => Some((vec![], Type::Bool)),
            _ => {
                self.error(
                    name_span,
                    format!(
                        "unknown graphics call `Gfx.{name}` (run, clear, rect, rectLines, circle, text, textWidth, mouseX, mouseY, mousePressed)"
                    ),
                );
                for arg in args {
                    self.check_expr(arg);
                }
                return Type::Unknown;
            }
        };
        if name == "run" {
            // Gfx.run(Int width, Int height, String title, frame) — the
            // runtime owns the event loop and calls `frame` once per frame.
            if args.len() != 4 {
                self.error(span, "`Gfx.run` takes (width, height, title, frame)");
            }
            for (i, expected) in [Type::Int, Type::Int, Type::Str].iter().enumerate() {
                if let Some(arg) = args.get(i) {
                    let ty = self.check_expr(arg);
                    self.unify_at(expected, &ty, arg.span, "Gfx.run argument");
                }
            }
            if let Some(frame) = args.get(3) {
                let frame_ty = self.check_expr(frame);
                let expected = Type::Func(Rc::new(FuncType {
                    params: vec![],
                    ret: self.ctx.fresh(),
                    errors: BTreeSet::new(),
                    caps: BTreeSet::new(),
                }));
                self.unify_at(&expected, &frame_ty, frame.span, "Gfx.run frame closure");
                // The closure's rows surface at this call site.
                self.add_func_arg_rows(&frame_ty);
            }
            return Type::Unit;
        }
        let (params, ret) = sig.unwrap();
        if args.len() != params.len() {
            self.error(
                span,
                format!("`Gfx.{name}` expects {} argument(s), found {}", params.len(), args.len()),
            );
        }
        for (param, arg) in params.iter().zip(args.iter()) {
            let ty = self.check_expr(arg);
            self.unify_at(param, &ty, arg.span, "graphics argument");
        }
        for arg in args.iter().skip(params.len()) {
            self.check_expr(arg);
        }
        ret
    }

    /// Handles builtin function calls; returns None when `name` is not builtin.
    fn check_builtin_call(
        &mut self,
        name: &str,
        callee_span: Span,
        args: &[&Expr],
        span: Span,
    ) -> Option<Type> {
        let check_arity = |checker: &mut Self, n: usize| {
            if args.len() != n {
                checker.error(span, format!("`{name}` expects {n} argument(s), found {}", args.len()));
                false
            } else {
                true
            }
        };
        let ty = match name {
            "println" | "print" => {
                for arg in args {
                    self.check_expr(arg);
                }
                Type::Unit
            }
            "show" | "encode" => {
                if check_arity(self, 1) {
                    self.check_expr(args[0]);
                }
                Type::Str
            }
            "decode" => {
                if !check_arity(self, 2) {
                    return Some(Type::Unknown);
                }
                let raw_ty = self.check_expr(args[0]);
                self.unify_at(&Type::Str, &raw_ty, args[0].span, "decode input");
                self.add_error_row(DECODE_ERROR);
                let tag_ty = self.check_expr(args[1]);
                match self.ctx.resolve(&tag_ty) {
                    Type::Tag(type_name) => Type::Named(type_name),
                    Type::Unknown => Type::Unknown,
                    other => {
                        let rendered = self.render(&other);
                        self.error(
                            args[1].span,
                            format!("`decode` needs a type name (like `User`), found {rendered}"),
                        );
                        Type::Unknown
                    }
                }
            }
            "map" => {
                if !check_arity(self, 2) {
                    return Some(Type::Unknown);
                }
                let container_ty = self.check_expr(args[0]);
                let func_ty = self.check_expr(args[1]);
                self.add_func_arg_rows(&func_ty);
                let a = self.ctx.fresh();
                let b = self.ctx.fresh();
                let expected_f = Type::Func(Rc::new(FuncType {
                    params: vec![a.clone()],
                    ret: b.clone(),
                    errors: BTreeSet::new(),
                    caps: BTreeSet::new(),
                }));
                self.unify_at(&expected_f, &func_ty, args[1].span, "map function");
                match self.ctx.resolve(&container_ty) {
                    Type::List(elem) => {
                        self.unify_at(&a, &elem, args[0].span, "map input");
                        Type::List(Box::new(b))
                    }
                    Type::Unknown => Type::Unknown,
                    _ => {
                        // Default to Option (also constrains unresolved vars).
                        let opt = Type::Option(Box::new(a));
                        self.unify_at(&opt, &container_ty, args[0].span, "map input");
                        Type::Option(Box::new(b))
                    }
                }
            }
            "getOrElse" => {
                if !check_arity(self, 2) {
                    return Some(Type::Unknown);
                }
                let opt_ty = self.check_expr(args[0]);
                let a = self.ctx.fresh();
                let opt = Type::Option(Box::new(a.clone()));
                self.unify_at(&opt, &opt_ty, args[0].span, "getOrElse input");
                let default_ty = self.check_expr(args[1]);
                self.unify_at(&a, &default_ty, args[1].span, "getOrElse default");
                a
            }
            "orFail" => {
                if !check_arity(self, 2) {
                    return Some(Type::Unknown);
                }
                let opt_ty = self.check_expr(args[0]);
                let a = self.ctx.fresh();
                let opt = Type::Option(Box::new(a.clone()));
                self.unify_at(&opt, &opt_ty, args[0].span, "orFail input");
                let err_ty = self.check_expr(args[1]);
                match self.ctx.resolve(&err_ty) {
                    Type::Error(err_name) => self.add_error_row(&err_name),
                    Type::Unknown | Type::Var(_) => {}
                    other => {
                        let rendered = self.render(&other);
                        self.error(
                            args[1].span,
                            format!("`orFail` needs an error value, found {rendered}"),
                        );
                    }
                }
                a
            }
            "retry" => {
                if !check_arity(self, 2) {
                    return Some(Type::Unknown);
                }
                // First argument is by-name (re-evaluated per attempt); its
                // rows still propagate — retrying can still fail.
                let action_ty = self.check_expr(args[0]);
                let schedule_ty = self.check_expr(args[1]);
                self.unify_at(&Type::Schedule, &schedule_ty, args[1].span, "retry schedule");
                action_ty
            }
            "upTo" => {
                if !check_arity(self, 2) {
                    return Some(Type::Unknown);
                }
                let schedule_ty = self.check_expr(args[0]);
                self.unify_at(&Type::Schedule, &schedule_ty, args[0].span, "upTo input");
                let n_ty = self.check_expr(args[1]);
                self.unify_at(&Type::Int, &n_ty, args[1].span, "upTo count");
                Type::Schedule
            }
            "ignoreFailure" => {
                if !check_arity(self, 1) {
                    return Some(Type::Unit);
                }
                // By-name: failures of the argument are swallowed.
                let ((), rows) = self.with_rows(|s| {
                    s.check_expr(args[0]);
                });
                self.merge_rows(&Rows { errors: BTreeSet::new(), caps: rows.caps });
                Type::Unit
            }
            "sleep" => {
                if check_arity(self, 1) {
                    let ty = self.check_expr(args[0]);
                    self.unify_at(&Type::Duration, &ty, args[0].span, "sleep duration");
                }
                Type::Unit
            }
            "len" => {
                if check_arity(self, 1) {
                    let ty = self.check_expr(args[0]);
                    match self.ctx.resolve(&ty) {
                        Type::Str | Type::List(_) | Type::Var(_) | Type::Unknown => {}
                        other => {
                            let rendered = self.render(&other);
                            self.error(
                                args[0].span,
                                format!("`len` works on String or lists, found {rendered}"),
                            );
                        }
                    }
                }
                Type::Int
            }
            "MutMap" => {
                check_arity(self, 0);
                let k = self.ctx.fresh();
                let v = self.ctx.fresh();
                Type::MutMap(Box::new(k), Box::new(v))
            }
            "nowMillis" | "nowMicros" => {
                check_arity(self, 0);
                Type::Int
            }
            "range" => {
                if check_arity(self, 1) {
                    let ty = self.check_expr(args[0]);
                    self.unify_at(&Type::Int, &ty, args[0].span, "range bound");
                }
                Type::List(Box::new(Type::Int))
            }
            "random" => {
                if check_arity(self, 1) {
                    let ty = self.check_expr(args[0]);
                    self.unify_at(&Type::Int, &ty, args[0].span, "random bound");
                }
                Type::Int
            }
            "Some" => {
                if !check_arity(self, 1) {
                    return Some(Type::Option(Box::new(Type::Unknown)));
                }
                let ty = self.check_expr(args[0]);
                Type::Option(Box::new(ty))
            }
            _ => return None,
        };
        if self.record_info {
            self.info.hovers.push((callee_span, format!("{name} (builtin)")));
        }
        Some(ty)
    }

    // ---- methods and fields ------------------------------------------------------

    fn check_method(
        &mut self,
        recv: &Expr,
        name: &str,
        name_span: Span,
        args: &[&Expr],
        span: Span,
    ) -> Type {
        // `Schedule.x(...)` / `Gfx.x(...)` arrive as Method when called directly.
        if let ExprKind::Var(module) = &recv.kind {
            if module == "Schedule" && !self.scope_has(module) {
                return self.check_schedule_call(name, name_span, args, span);
            }
            if module == "Gfx" && !self.scope_has(module) {
                return self.check_gfx_call(name, name_span, args, span);
            }
        }
        let recv_ty = self.check_expr(recv);
        match self.ctx.resolve(&recv_ty) {
            Type::Service(service) => {
                let method_info = self.services.get(&service).and_then(|s| {
                    s.methods
                        .iter()
                        .find(|(n, _)| n == name)
                        .map(|(_, m)| (m.params.clone(), m.ret.clone(), m.name_span))
                });
                let Some((params, ret, def_span)) = method_info else {
                    self.error(
                        name_span,
                        format!("service `{service}` has no method `{name}`"),
                    );
                    for arg in args {
                        self.check_expr(arg);
                    }
                    return Type::Unknown;
                };
                if params.len() != args.len() {
                    self.error(
                        span,
                        format!("`{name}` expects {} argument(s), found {}", params.len(), args.len()),
                    );
                }
                for (param_ty, arg) in params.iter().zip(args.iter()) {
                    let arg_ty = self.check_expr(arg);
                    self.unify_at(param_ty, &arg_ty, arg.span, "argument");
                    self.add_func_arg_rows(&arg_ty);
                }
                for arg in args.iter().skip(params.len()) {
                    self.check_expr(arg);
                }
                let rows = self.method_effective_rows(&service, name);
                self.merge_rows(&rows);
                if self.record_info {
                    self.info.refs.push((name_span, def_span));
                    let rendered = self.render(&ret);
                    self.info.hovers.push((name_span, format!("{service}.{name} -> {rendered}")));
                }
                ret
            }
            Type::MutMap(k, v) => match name {
                "get" => {
                    if args.len() == 1 {
                        let arg_ty = self.check_expr(args[0]);
                        self.unify_at(&k, &arg_ty, args[0].span, "map key");
                    } else {
                        self.error(span, "`get` expects 1 argument (the key)");
                    }
                    Type::Option(v.clone())
                }
                "set" => {
                    if args.len() == 2 {
                        let key_ty = self.check_expr(args[0]);
                        self.unify_at(&k, &key_ty, args[0].span, "map key");
                        let val_ty = self.check_expr(args[1]);
                        self.unify_at(&v, &val_ty, args[1].span, "map value");
                    } else {
                        self.error(span, "`set` expects 2 arguments (key, value)");
                    }
                    Type::Unit
                }
                "delete" => {
                    if args.len() == 1 {
                        let arg_ty = self.check_expr(args[0]);
                        self.unify_at(&k, &arg_ty, args[0].span, "map key");
                    } else {
                        self.error(span, "`delete` expects 1 argument (the key)");
                    }
                    Type::Unit
                }
                "size" => {
                    if !args.is_empty() {
                        self.error(span, "`size` takes no arguments");
                    }
                    Type::Int
                }
                _ => {
                    self.error(
                        name_span,
                        format!("MutMap has no method `{name}` (get, set, delete, size)"),
                    );
                    Type::Unknown
                }
            },
            Type::Unknown => {
                for arg in args {
                    self.check_expr(arg);
                }
                Type::Unknown
            }
            Type::Var(_) => {
                self.error(
                    recv.span,
                    "cannot infer the receiver's type here; add a type annotation",
                );
                for arg in args {
                    self.check_expr(arg);
                }
                Type::Unknown
            }
            other => {
                let rendered = self.render(&other);
                self.error(name_span, format!("{rendered} has no method `{name}`"));
                for arg in args {
                    self.check_expr(arg);
                }
                Type::Unknown
            }
        }
    }

    fn check_field(&mut self, recv: &Expr, name: &str, name_span: Span) -> Type {
        let recv_ty = self.check_expr(recv);
        let resolved = self.ctx.resolve(&recv_ty);

        // Duration suffixes: `100.millis`, `5.minutes`
        if DURATION_SUFFIXES.iter().any(|(s, _)| *s == name) {
            match resolved {
                Type::Int | Type::Var(_) | Type::Unknown => {
                    self.unify_at(&Type::Int, &recv_ty, recv.span, "duration value");
                    return Type::Duration;
                }
                _ => {}
            }
        }

        match resolved {
            Type::Named(type_name) => {
                self.struct_field_type(&self.types_decl, &type_name, name, name_span)
            }
            Type::Error(err_name) => {
                self.struct_field_type(&self.errors_decl, &err_name, name, name_span)
            }
            Type::Var(_) => {
                // Try unique-field inference: if exactly one type/error has
                // this field, the receiver must be it.
                let mut owners: Vec<(Type, Type)> = Vec::new();
                for (tname, info) in &self.types_decl {
                    if let Some((_, fty)) = info.fields.iter().find(|(f, _)| f == name) {
                        owners.push((Type::Named(tname.clone()), fty.clone()));
                    }
                }
                for (ename, info) in &self.errors_decl {
                    if let Some((_, fty)) = info.fields.iter().find(|(f, _)| f == name) {
                        owners.push((Type::Error(ename.clone()), fty.clone()));
                    }
                }
                if owners.len() == 1 {
                    let (owner, field_ty) = owners.pop().unwrap();
                    self.unify_at(&owner, &recv_ty, recv.span, "field access");
                    field_ty
                } else if owners.is_empty() {
                    self.error(name_span, format!("no type has a field named `{name}`"));
                    Type::Unknown
                } else {
                    self.error(
                        recv.span,
                        format!(
                            "cannot infer which type `.{name}` belongs to; add a type annotation"
                        ),
                    );
                    Type::Unknown
                }
            }
            Type::Unknown => Type::Unknown,
            other => {
                let rendered = self.render(&other);
                self.error(name_span, format!("{rendered} has no field `{name}`"));
                Type::Unknown
            }
        }
    }

    fn struct_field_type(
        &self,
        table: &HashMap<String, StructInfo>,
        type_name: &str,
        field: &str,
        _name_span: Span,
    ) -> Type {
        match table
            .get(type_name)
            .and_then(|i| i.fields.iter().find(|(f, _)| f == field))
        {
            Some((_, ty)) => ty.clone(),
            None => Type::Unknown,
        }
    }

    // The above returns Unknown without a diagnostic; wrap it:
    // (kept separate so hover lookups can reuse it silently)

    // ---- pipes / catch ----------------------------------------------------------

    fn check_pipe(&mut self, lhs: &Expr, target: &PipeTarget, span: Span) -> Type {
        match target {
            PipeTarget::Call { callee, args } => {
                let empty = Vec::new();
                let extra = args.as_ref().unwrap_or(&empty);
                let mut all: Vec<&Expr> = Vec::with_capacity(extra.len() + 1);
                all.push(lhs);
                all.extend(extra.iter());
                self.check_call(callee, &all, span)
            }
            PipeTarget::Catch { arms, span: catch_span } => {
                let (lhs_ty, mut rows) = self.with_rows(|s| s.check_expr(lhs));
                let result_ty = lhs_ty;
                for arm in arms {
                    self.scopes.push(HashMap::new());
                    match &arm.pattern.kind {
                        PatternKind::Ctor { name, name_span, args } => {
                            if !self.errors_decl.contains_key(name) {
                                self.error(
                                    *name_span,
                                    format!("unknown error type `{name}` in `catch`"),
                                );
                            } else {
                                if !rows.errors.remove(name) {
                                    self.warn(
                                        *name_span,
                                        format!(
                                            "this `catch` arm is unreachable: the expression cannot fail with `{name}`"
                                        ),
                                    );
                                }
                                self.bind_error_pattern(name, args, arm.pattern.span);
                            }
                        }
                        PatternKind::Bind(bind_name) => {
                            rows.errors.clear();
                            self.scopes
                                .last_mut()
                                .unwrap()
                                .insert(bind_name.clone(), Type::Unknown);
                        }
                        PatternKind::Wildcard => {
                            rows.errors.clear();
                        }
                        _ => {
                            self.error(
                                arm.pattern.span,
                                "`catch` patterns match errors: use `ErrorName`, `ErrorName(e)`, a name, or `_`",
                            );
                        }
                    }
                    let arm_ty = self.check_expr(&arm.body);
                    self.unify_at(&result_ty, &arm_ty, arm.body.span, "catch arm");
                    self.scopes.pop();
                }
                let _ = catch_span;
                self.merge_rows(&rows);
                result_ty
            }
        }
    }

    fn bind_error_pattern(&mut self, err_name: &str, args: &CtorPatArgs, span: Span) {
        let fields = self.errors_decl[err_name].fields.clone();
        match args {
            CtorPatArgs::None => {}
            CtorPatArgs::Positional(pats) => {
                if pats.len() == 1 {
                    // Single pattern binds the whole error value.
                    if let PatternKind::Bind(name) = &pats[0].kind {
                        self.scopes
                            .last_mut()
                            .unwrap()
                            .insert(name.clone(), Type::Error(err_name.to_string()));
                        if self.record_info {
                            self.info
                                .hovers
                                .push((pats[0].span, format!("{name} : {err_name}")));
                        }
                        return;
                    }
                }
                if pats.len() != fields.len() {
                    self.error(
                        span,
                        format!(
                            "`{err_name}` has {} field(s) but the pattern has {}",
                            fields.len(),
                            pats.len()
                        ),
                    );
                }
                for (pat, (_, fty)) in pats.iter().zip(fields.iter()) {
                    self.check_pattern(pat, fty);
                }
            }
            CtorPatArgs::Fields(names) => {
                for (fname, fspan) in names {
                    match fields.iter().find(|(f, _)| f == fname) {
                        Some((_, fty)) => {
                            self.scopes.last_mut().unwrap().insert(fname.clone(), fty.clone());
                        }
                        None => {
                            self.error(
                                *fspan,
                                format!("`{err_name}` has no field `{fname}`"),
                            );
                        }
                    }
                }
            }
        }
    }

    // ---- match -------------------------------------------------------------------

    fn check_match(&mut self, scrutinee: &Expr, arms: &[Arm]) -> Type {
        let scrut_ty = self.check_expr(scrutinee);
        let result = self.ctx.fresh();
        for arm in arms {
            self.scopes.push(HashMap::new());
            self.check_pattern_against(&arm.pattern, &scrut_ty);
            let arm_ty = self.check_expr(&arm.body);
            self.unify_at(&result, &arm_ty, arm.body.span, "match arm");
            self.scopes.pop();
        }
        if arms.is_empty() {
            return Type::Unknown;
        }
        result
    }

    fn check_pattern(&mut self, pat: &Pattern, expected: &Type) {
        self.check_pattern_against(pat, expected);
    }

    fn check_pattern_against(&mut self, pat: &Pattern, scrut_ty: &Type) {
        match &pat.kind {
            PatternKind::Wildcard => {}
            PatternKind::Bind(name) => {
                self.scopes.last_mut().unwrap().insert(name.clone(), scrut_ty.clone());
                if self.record_info {
                    let rendered = self.render(scrut_ty);
                    self.info.hovers.push((pat.span, format!("{name} : {rendered}")));
                }
            }
            PatternKind::Int(_) => {
                self.unify_at(&Type::Int, scrut_ty, pat.span, "pattern");
            }
            PatternKind::Str(_) => {
                self.unify_at(&Type::Str, scrut_ty, pat.span, "pattern");
            }
            PatternKind::Bool(_) => {
                self.unify_at(&Type::Bool, scrut_ty, pat.span, "pattern");
            }
            PatternKind::Ctor { name, name_span, args } => match name.as_str() {
                "Some" => {
                    let inner = self.ctx.fresh();
                    let opt = Type::Option(Box::new(inner.clone()));
                    self.unify_at(&opt, scrut_ty, pat.span, "pattern");
                    match args {
                        CtorPatArgs::Positional(pats) if pats.len() == 1 => {
                            self.check_pattern_against(&pats[0], &inner);
                        }
                        CtorPatArgs::None => {}
                        _ => self.error(pat.span, "`Some` takes one pattern: `Some(x)`"),
                    }
                }
                "None" => {
                    let opt = Type::Option(Box::new(self.ctx.fresh()));
                    self.unify_at(&opt, scrut_ty, pat.span, "pattern");
                    if !matches!(args, CtorPatArgs::None) {
                        self.error(pat.span, "`None` takes no arguments");
                    }
                }
                _ if self.errors_decl.contains_key(name) => {
                    self.unify_at(&Type::Error(name.clone()), scrut_ty, pat.span, "pattern");
                    self.bind_error_pattern(name, args, pat.span);
                }
                _ if self.types_decl.contains_key(name) => {
                    self.unify_at(&Type::Named(name.clone()), scrut_ty, pat.span, "pattern");
                    let fields = self.types_decl[name].fields.clone();
                    match args {
                        CtorPatArgs::None => {}
                        CtorPatArgs::Positional(pats) => {
                            if pats.len() != fields.len() {
                                self.error(
                                    pat.span,
                                    format!(
                                        "`{name}` has {} field(s) but the pattern has {}",
                                        fields.len(),
                                        pats.len()
                                    ),
                                );
                            }
                            for (p, (_, fty)) in pats.iter().zip(fields.iter()) {
                                self.check_pattern_against(p, fty);
                            }
                        }
                        CtorPatArgs::Fields(names) => {
                            for (fname, fspan) in names {
                                match fields.iter().find(|(f, _)| f == fname) {
                                    Some((_, fty)) => {
                                        self.scopes
                                            .last_mut()
                                            .unwrap()
                                            .insert(fname.clone(), fty.clone());
                                    }
                                    None => {
                                        self.error(
                                            *fspan,
                                            format!("`{name}` has no field `{fname}`"),
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {
                    self.error(*name_span, format!("unknown constructor `{name}` in pattern"));
                }
            },
        }
    }

    // ---- provide ------------------------------------------------------------------

    fn check_provide(&mut self, impls: &[(String, Span)], body: &Block) -> Type {
        let mut provided: BTreeSet<String> = BTreeSet::new();
        for (name, span) in impls {
            match self.impls.get(name) {
                Some(info) => {
                    provided.insert(info.service.clone());
                    let def_span = info.name_span;
                    if self.record_info {
                        self.info.refs.push((*span, def_span));
                        let service = info.service.clone();
                        self.info.hovers.push((*span, format!("{name} :: {service}")));
                    }
                    // Constructing the impl runs its field initializers.
                    let field_rows = self.impl_field_rows.get(name).cloned().unwrap_or_default();
                    self.merge_rows(&field_rows);
                }
                None => {
                    self.error(
                        *span,
                        format!("unknown implementation `{name}` (declare it like `{name} :: SomeService {{ ... }}`)"),
                    );
                }
            }
        }
        let (body_ty, mut rows) = self.with_rows(|s| s.check_block(body));
        rows.caps.retain(|c| !provided.contains(c));
        self.merge_rows(&rows);
        body_ty
    }

    // ---- binary ---------------------------------------------------------------------

    fn check_binary(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr, span: Span) -> Type {
        let lhs_ty = self.check_expr(lhs);
        let rhs_ty = self.check_expr(rhs);
        self.unify_at(&lhs_ty, &rhs_ty, rhs.span, &format!("`{}` operands", op.symbol()));
        let operand = self.ctx.resolve(&lhs_ty);
        match op {
            BinOp::Add => match operand {
                Type::Int | Type::Float | Type::Str | Type::Duration | Type::Var(_)
                | Type::Unknown => lhs_ty,
                other => {
                    let rendered = self.render(&other);
                    self.error(span, format!("`+` is not defined for {rendered}"));
                    Type::Unknown
                }
            },
            BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => match operand {
                Type::Int | Type::Float | Type::Var(_) | Type::Unknown => lhs_ty,
                Type::Duration if matches!(op, BinOp::Sub) => Type::Duration,
                other => {
                    let rendered = self.render(&other);
                    self.error(
                        span,
                        format!("`{}` is not defined for {rendered}", op.symbol()),
                    );
                    Type::Unknown
                }
            },
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => match operand {
                Type::Int | Type::Float | Type::Str | Type::Duration | Type::Var(_)
                | Type::Unknown => Type::Bool,
                other => {
                    let rendered = self.render(&other);
                    self.error(
                        span,
                        format!("`{}` is not defined for {rendered}", op.symbol()),
                    );
                    Type::Bool
                }
            },
            BinOp::Eq | BinOp::Ne => Type::Bool,
            BinOp::And | BinOp::Or => {
                self.unify_at(&Type::Bool, &lhs_ty, lhs.span, "logical operand");
                Type::Bool
            }
        }
    }

    // ---- finalization ------------------------------------------------------------------

    fn validate_declared_rows(&mut self) {
        let names: Vec<String> = self.funcs.keys().cloned().collect();
        for name in names {
            let info = &self.funcs[&name];
            let name_span = info.name_span;
            let declared_errors = info.declared_errors.clone();
            let declared_caps = info.declared_caps.clone();
            let inferred = self.func_rows.get(&name).cloned().unwrap_or_default();
            if let Some(declared) = declared_errors {
                for err in inferred.errors.difference(&declared) {
                    self.diags.push(Diagnostic::error(
                        name_span,
                        format!(
                            "`{name}` can fail with `{err}` but its signature does not declare it (add `! {err}` or handle it with `catch`)"
                        ),
                    ));
                }
            }
            if let Some(declared) = declared_caps {
                for cap in inferred.caps.difference(&declared) {
                    self.diags.push(Diagnostic::error(
                        name_span,
                        format!(
                            "`{name}` uses `{cap}` but its signature does not declare it (add `uses {cap}` or provide it)"
                        ),
                    ));
                }
            }
        }

        // `main` must be self-contained: every error handled, every service provided.
        if let Some(info) = self.funcs.get("main") {
            let name_span = info.name_span;
            let rows = self.func_effective_rows("main");
            for err in &rows.errors {
                self.diags.push(Diagnostic::error(
                    name_span,
                    format!("`main` does not handle the error `{err}`; add a `catch` for it"),
                ));
            }
            for cap in &rows.caps {
                self.diags.push(Diagnostic::error(
                    name_span,
                    format!("`main` requires the service `{cap}`; wrap the code in `provide`"),
                ));
            }
        }
    }

    fn record_def_details(&mut self) {
        if !self.record_info {
            return;
        }
        for decl in &self.program.decls {
            let def = match decl {
                Decl::Error(d) => DefInfo {
                    name: d.name.clone(),
                    span: d.name_span,
                    kind: DefKind::Error,
                    detail: format!(
                        "error {} = {{ {} }}",
                        d.name,
                        d.fields.iter().map(|f| f.name.clone()).collect::<Vec<_>>().join(", ")
                    ),
                },
                Decl::Type(d) => DefInfo {
                    name: d.name.clone(),
                    span: d.name_span,
                    kind: DefKind::Type,
                    detail: format!(
                        "type {} = {{ {} }}",
                        d.name,
                        d.fields.iter().map(|f| f.name.clone()).collect::<Vec<_>>().join(", ")
                    ),
                },
                Decl::Service(d) => DefInfo {
                    name: d.name.clone(),
                    span: d.name_span,
                    kind: DefKind::Service,
                    detail: format!("service {}", d.name),
                },
                Decl::Impl(d) => DefInfo {
                    name: d.name.clone(),
                    span: d.name_span,
                    kind: DefKind::Impl,
                    detail: format!("{} :: {}", d.name, d.service),
                },
                Decl::Func(d) => DefInfo {
                    name: d.name.clone(),
                    span: d.name_span,
                    kind: DefKind::Func,
                    detail: self.render_func_signature(&d.name),
                },
            };
            self.info.defs.push(def);
            // Hover for error/type/service declarations themselves.
            match decl {
                Decl::Error(d) => {
                    let detail = self.info.defs.last().unwrap().detail.clone();
                    self.info.hovers.push((d.name_span, detail));
                }
                Decl::Type(d) => {
                    let detail = self.info.defs.last().unwrap().detail.clone();
                    self.info.hovers.push((d.name_span, detail));
                }
                _ => {}
            }
        }
    }

    // ---- rendering ----------------------------------------------------------------------

    fn render(&self, ty: &Type) -> String {
        let mut names = Vec::new();
        self.ctx.render(ty, &mut names)
    }

    fn render_func_signature(&self, name: &str) -> String {
        let Some(info) = self.funcs.get(name) else { return name.to_string() };
        let mut names = Vec::new();
        let params: Vec<String> = info
            .params
            .iter()
            .zip(info.param_names.iter())
            .zip(info.lazy.iter())
            .map(|((ty, pname), lazy)| {
                let rendered = self.ctx.render(ty, &mut names);
                let lazy_prefix = if *lazy { "lazy " } else { "" };
                format!("{lazy_prefix}{rendered} {pname}")
            })
            .collect();
        let ret = self.ctx.render(&info.ret, &mut names);
        let rows = self.func_effective_rows(name);
        let mut sig = format!("{name} :: ({}) -> {ret}", params.join(", "));
        if !rows.errors.is_empty() {
            sig.push_str(" ! ");
            sig.push_str(&rows.errors.iter().cloned().collect::<Vec<_>>().join(", "));
        }
        if !rows.caps.is_empty() {
            sig.push_str(" uses ");
            sig.push_str(&rows.caps.iter().cloned().collect::<Vec<_>>().join(", "));
        }
        sig
    }

    fn unify_at(&mut self, expected: &Type, found: &Type, span: Span, what: &str) {
        if let Err((a, b)) = self.ctx.unify(expected, found) {
            let ra = self.render(&a);
            let rb = self.render(&b);
            self.error(span, format!("type mismatch in {what}: expected {ra}, found {rb}"));
        }
    }
}

fn last_span(block: &Block) -> Span {
    match block.stmts.last() {
        Some(Stmt::Expr(e)) => e.span,
        Some(Stmt::Bind { value, .. }) => value.span,
        Some(Stmt::Acquire { name_span, .. }) => *name_span,
        None => block.span,
    }
}

const BUILTIN_NAMES: [&str; 19] = [
    "println",
    "print",
    "show",
    "encode",
    "decode",
    "map",
    "getOrElse",
    "orFail",
    "retry",
    "upTo",
    "ignoreFailure",
    "sleep",
    "len",
    "MutMap",
    "Some",
    "nowMillis",
    "nowMicros",
    "range",
    "random",
];

/// Names the LSP offers as completions alongside user definitions.
pub fn builtin_completions() -> Vec<(&'static str, &'static str)> {
    vec![
        ("println", "println(value) -> Unit"),
        ("print", "print(value) -> Unit"),
        ("show", "show(value) -> String"),
        ("encode", "encode(value) -> String"),
        ("decode", "decode(raw, TypeName) -> a ! DecodeError"),
        ("map", "map(container, f) -> mapped"),
        ("getOrElse", "getOrElse(option, default) -> a"),
        ("orFail", "orFail(option, error) -> a ! error"),
        ("retry", "retry(action, schedule) -> a"),
        ("upTo", "upTo(schedule, times) -> Schedule"),
        ("ignoreFailure", "ignoreFailure(action) -> Unit"),
        ("sleep", "sleep(duration) -> Unit"),
        ("len", "len(stringOrList) -> Int"),
        ("MutMap", "MutMap() -> MutMap<k, v>"),
        ("nowMillis", "nowMillis() -> Int — monotonic milliseconds since program start"),
        ("nowMicros", "nowMicros() -> Int — monotonic microseconds since program start"),
        ("range", "range(n) -> [Int] — the list [0, 1, ..., n-1]"),
        ("random", "random(n) -> Int — uniform in 0..n-1"),
        ("Gfx.run", "Gfx.run(width, height, title, frame) — open a window, call frame each frame"),
        ("Gfx.clear", "Gfx.clear(r, g, b)"),
        ("Gfx.rect", "Gfx.rect(x, y, w, h, r, g, b, a)"),
        ("Gfx.rectLines", "Gfx.rectLines(x, y, w, h, thickness, r, g, b, a)"),
        ("Gfx.circle", "Gfx.circle(x, y, radius, r, g, b, a)"),
        ("Gfx.text", "Gfx.text(s, x, y, size, r, g, b)"),
        ("Gfx.textWidth", "Gfx.textWidth(s, size) -> Int"),
        ("Gfx.mouseX", "Gfx.mouseX() -> Int"),
        ("Gfx.mouseY", "Gfx.mouseY() -> Int"),
        ("Gfx.mousePressed", "Gfx.mousePressed() -> Bool — left click this frame"),
        ("Some", "Some(value) -> value?"),
        ("None", "None : a?"),
        ("Schedule.exponential", "Schedule.exponential(base) -> Schedule"),
        ("Schedule.fixed", "Schedule.fixed(interval) -> Schedule"),
    ]
}
