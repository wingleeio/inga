//! Type and effect checking.
//!
//! Value types are inferred by unification (`types.rs`). The two effect rows
//! — errors (`!`) and capabilities (`uses`) — are finite name-sets computed by
//! a monotone fixpoint over the call graph: each pass re-infers every function
//! body using the previous pass's row summaries until nothing changes. `catch`
//! subtracts error names; `provide` subtracts capability names. Declared rows
//! are validated against (and unioned with) inferred rows at the end.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};

use crate::ast::*;
use crate::diag::Diagnostic;
use crate::modules::{ImportInfo, ModuleSrc};
use crate::span::Span;
use crate::types::{FuncType, Type, TypeCtx};
use std::rc::Rc;

pub const DURATION_SUFFIXES: [(&str, i64); 5] =
    [("millis", 1), ("seconds", 1000), ("minutes", 60_000), ("hours", 3_600_000), ("days", 86_400_000)];

/// Byte-size suffixes (`256.kb`): plain Int factors, used by `Arena(...)`.
pub const SIZE_SUFFIXES: [(&str, i64); 3] =
    [("kb", 1024), ("mb", 1024 * 1024), ("gb", 1024 * 1024 * 1024)];

/// Builtin struct raised by `decode`.
pub const DECODE_ERROR: &str = "DecodeError";
pub const ASSERT_FAILED: &str = "AssertionError";
pub const INTERRUPTED: &str = "InterruptedError";
pub const TIMEOUT: &str = "TimeoutError";
/// The capability every `std/fiber` operation needs; satisfied only by the
/// builtin `provide Runtime(n)` resource.
pub const FIBERS_SERVICE: &str = "Fibers";
/// The capability every `std/http` operation needs; satisfied by
/// `provide Http`. Shared, so requests cross fiber boundaries.
pub const HTTP_SERVICE: &str = "Http";
pub const HTTP_ERROR: &str = "HttpError";
/// The capability every `std/fs` operation needs; satisfied by
/// `provide Fs`. Shared, so file access crosses fiber boundaries.
pub const FS_SERVICE: &str = "Fs";
pub const IO_ERROR: &str = "IoError";
/// The capability every `std/net` operation needs; satisfied by
/// `provide Net`. Shared, so sockets cross fiber boundaries.
pub const NET_SERVICE: &str = "Net";
pub const NET_ERROR: &str = "NetError";
/// The capability every `std/term` operation needs; satisfied by
/// `provide Term`.
pub const TERM_SERVICE: &str = "Term";

/// Surface names of primitive types that can appear in a `!` row.
pub const PRIMITIVE_TAGS: [&str; 5] = ["Int", "Float", "Bool", "String", "Duration"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefKind {
    Func,
    Struct,
    Enum,
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
    /// Per `catch`: (span of `catch { ... }`, the error row of the caught
    /// expression) — drives pattern completion in the arms.
    pub catch_rows: Vec<(Span, Vec<String>)>,
    /// Per `match`: (span of the whole match, scrutinee expression key into
    /// `expr_types`) — drives pattern completion in the arms.
    pub match_ctxs: Vec<(Span, (u32, u32))>,
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
    Tuple(Vec<CType>),
    Fiber(Box<CType>),
    Outcome(Box<CType>),
    Struct(String),
    Enum(String),
    Service(String),
    Tag(String),
    MutMap(Box<CType>, Box<CType>),
    MutList(Box<CType>),
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
    /// Resolved parameter / return types, consumed by the backend's
    /// refcount insertion (drop glue needs to know what is heap-shaped).
    pub func_params: HashMap<String, Vec<CType>>,
    pub func_ret: HashMap<String, CType>,
    pub method_params: HashMap<(String, String), Vec<CType>>,
    pub method_ret: HashMap<(String, String), CType>,
    /// Struct field types and enum variant field types, by name.
    pub struct_fields: HashMap<String, Vec<CType>>,
    pub enum_variants: HashMap<String, Vec<(String, Vec<CType>)>>,
    /// Impl instance field types, by impl name.
    pub impl_fields: HashMap<String, Vec<CType>>,
    /// Functions with universal type parameters: the backend exempts their
    /// results from reclamation (uniform representation, no per-instance
    /// drop glue — a bounded, documented leak).
    pub generic_funcs: std::collections::HashSet<String>,
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
    module: usize,
    is_pub: bool,
}

struct EnumInfo {
    /// Variant name -> typed fields, in declaration order.
    variants: Vec<(String, Vec<(String, Type)>)>,
    name_span: Span,
    module: usize,
    is_pub: bool,
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
    module: usize,
    is_pub: bool,
    /// `shared service`: instances may cross fiber boundaries; impls are
    /// checked to carry only scalar state.
    shared: bool,
}

struct ImplInfo {
    service: String,
    fields: Vec<(String, Type)>,
    name_span: Span,
    module: usize,
    is_pub: bool,
}

struct FuncInfo {
    params: Vec<Type>,
    param_names: Vec<String>,
    lazy: Vec<bool>,
    ret: Type,
    declared_errors: Option<BTreeSet<String>>,
    declared_caps: Option<BTreeSet<String>>,
    name_span: Span,
    module: usize,
    is_pub: bool,
    /// Type-variable ids from lowercase names in the signature — universally
    /// quantified: instantiated fresh at every call site, rigid in the body.
    rigid: Vec<u32>,
}

/// Module id for compiler builtins (always visible).
const CORE_MODULE: usize = usize::MAX;

pub fn check(
    program: &Program,
    modules: &[ModuleSrc],
    diagnostics: &mut Vec<Diagnostic>,
) -> CheckInfo {
    let mut checker = Checker::new(program, modules);
    checker.collect_decls();
    // Declaration-level diagnostics survive the fixpoint's per-pass clears.
    let decl_diags = checker.diags.clone();

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
    let raw = std::mem::take(&mut checker.raw_expr_types);
    for (key, ty) in raw {
        let resolved = checker.ctype(&ty);
        checker.info.expr_types.insert(key, resolved);
    }
    checker.validate_declared_rows();
    checker.validate_shared_impls();
    checker.record_def_details();
    checker.record_facts();
    let deferred = std::mem::take(&mut checker.typed_hovers);
    for (span, name, ty) in deferred {
        let rendered = checker.render(&ty);
        checker.info.hovers.push((span, format!("{name} : {rendered}")));
    }

    diagnostics.extend(decl_diags);
    diagnostics.append(&mut checker.diags.clone());
    checker.info
}

struct Checker<'a> {
    program: &'a Program,
    modules: &'a [ModuleSrc],
    ctx: TypeCtx,

    structs: HashMap<String, StructInfo>,
    enums: HashMap<String, EnumInfo>,
    /// Variant name -> owning enum (variant names are globally unique).
    variant_owner: HashMap<String, String>,
    services: HashMap<String, ServiceInfo>,
    impls: HashMap<String, ImplInfo>,
    funcs: HashMap<String, FuncInfo>,

    func_rows: HashMap<String, Rows>,
    method_rows: HashMap<(String, String), Rows>,
    impl_field_rows: HashMap<String, Rows>,

    diags: Vec<Diagnostic>,
    record_info: bool,
    /// Value-type hovers (`name : T`) deferred to the end of checking, so
    /// the rendered type reflects constraints discovered after the use site
    /// (e.g. `xs = MutList()` refined by a later `xs.push(1)`).
    typed_hovers: Vec<(Span, String, Type)>,
    current_rigid: std::collections::HashSet<u32>,
    raw_expr_types: HashMap<(u32, u32), Type>,
    info: CheckInfo,
    changed: bool,

    scopes: Vec<HashMap<String, Type>>,
    row_stack: Vec<Rows>,
}

impl<'a> Checker<'a> {
    fn new(program: &'a Program, modules: &'a [ModuleSrc]) -> Checker<'a> {
        let mut checker = Checker {
            program,
            modules,
            ctx: TypeCtx::default(),
            structs: HashMap::new(),
            enums: HashMap::new(),
            variant_owner: HashMap::new(),
            services: HashMap::new(),
            impls: HashMap::new(),
            funcs: HashMap::new(),
            func_rows: HashMap::new(),
            method_rows: HashMap::new(),
            impl_field_rows: HashMap::new(),
            diags: Vec::new(),
            record_info: false,
            typed_hovers: Vec::new(),
            current_rigid: std::collections::HashSet::new(),
            raw_expr_types: HashMap::new(),
            info: CheckInfo::default(),
            changed: false,
            scopes: Vec::new(),
            row_stack: Vec::new(),
        };
        // Builtin structs available to every program (raised by `decode`,
        // `assert`/`assertEq`, and std/http) — shapes from one shared table.
        for name in [
            DECODE_ERROR,
            ASSERT_FAILED,
            "HttpResponse",
            HTTP_ERROR,
            "HttpStream",
            "HttpRequest",
            IO_ERROR,
            "File",
            NET_ERROR,
            "Socket",
            "Listener",
            "DateTime",
        ] {
            let fields = builtin_struct_fields(name)
                .iter()
                .map(|(f, t)| {
                    let ty = match *t {
                        "Int" => Type::Int,
                        "String" => Type::Str,
                        other => unreachable!("unmapped builtin field type {other}"),
                    };
                    (f.to_string(), ty)
                })
                .collect();
            checker.structs.insert(
                name.to_string(),
                StructInfo {
                    fields,
                    name_span: Span::default(),
                    module: CORE_MODULE,
                    is_pub: true,
                },
            );
        }
        for name in [HTTP_SERVICE, FS_SERVICE, NET_SERVICE, TERM_SERVICE] {
            checker.services.insert(
                name.to_string(),
                ServiceInfo {
                    methods: Vec::new(),
                    name_span: Span::default(),
                    module: CORE_MODULE,
                    is_pub: true,
                    shared: true,
                },
            );
        }
        // Fieldless builtins raised by the fiber machinery.
        for name in [INTERRUPTED, TIMEOUT] {
            checker.structs.insert(
                name.to_string(),
                StructInfo {
                    fields: Vec::new(),
                    name_span: Span::default(),
                    module: CORE_MODULE,
                    is_pub: true,
                },
            );
        }
        // The capability carried by every std/fiber operation; satisfied
        // only by `provide Runtime(n)`. Shared so nested forks are fine.
        checker.services.insert(
            FIBERS_SERVICE.to_string(),
            ServiceInfo {
                methods: Vec::new(),
                name_span: Span::default(),
                module: CORE_MODULE,
                is_pub: true,
                shared: true,
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

    // ---- modules / visibility --------------------------------------------

    fn module_of(&self, span: Span) -> usize {
        self.modules.iter().position(|m| m.contains(span)).unwrap_or(0)
    }

    /// Enforce cross-module visibility: a reference from another module
    /// needs the definition to be `pub` and its module to be imported.
    /// Bare cross-module names resolve only when selectively imported:
    /// `use cards { rankName }`. A plain `use cards` binds the qualified
    /// alias only. `covered_by` lets an enum's name also grant its variants.
    fn gate(&mut self, name: &str, def_module: usize, is_pub: bool, ref_span: Span) {
        self.gate_covered(name, None, def_module, is_pub, ref_span);
    }

    fn gate_covered(
        &mut self,
        name: &str,
        covered_by: Option<&str>,
        def_module: usize,
        is_pub: bool,
        ref_span: Span,
    ) {
        if def_module == CORE_MODULE || self.modules.len() <= 1 {
            return;
        }
        let ref_module = self.module_of(ref_span);
        if ref_module == def_module {
            return;
        }
        let Some(def) = self.modules.get(def_module) else { return };
        let (def_key, def_name) = (def.key.clone(), def.name.clone());
        let granted = self.modules[ref_module].imports.iter().any(|i| {
            i.target == def_key
                && i.names.as_ref().is_some_and(|ns| {
                    ns.iter().any(|n| n == name || Some(n.as_str()) == covered_by)
                })
        });
        if granted {
            if !is_pub {
                self.error(
                    ref_span,
                    format!("`{name}` is private to module `{def_name}` (mark it `pub` to export it)"),
                );
            }
            return;
        }
        let imported_plain =
            self.modules[ref_module].imports.iter().any(|i| i.target == def_key);
        if imported_plain {
            self.error(
                ref_span,
                format!(
                    "`{name}` is not imported here: call it as `{def_name}.{name}` or import it with `use {def_name} {{ {name} }}`"
                ),
            );
        } else {
            self.error(
                ref_span,
                format!("`{name}` is defined in module `{def_name}`; add `use {def_name} {{ {name} }}`"),
            );
        }
    }

    /// The import a qualified `alias.member` reference resolves through
    /// (plain imports only — selective imports do not bind the alias).
    fn import_for_alias(&self, span: Span, alias: &str) -> Option<ImportInfo> {
        self.modules
            .get(self.module_of(span))?
            .imports
            .iter()
            .find(|i| i.alias == alias && i.names.is_none())
            .cloned()
    }

    fn std_imported(&self, span: Span, target: &str) -> bool {
        self.modules
            .get(self.module_of(span))
            .is_some_and(|m| m.imports.iter().any(|i| i.target == target))
    }

    // ---- declaration collection -----------------------------------------

    fn collect_decls(&mut self) {
        // First sweep: names only, so types can reference each other.
        for decl in &self.program.decls {
            let (name, span, kind, is_pub) = match decl {
                Decl::Use(_) => continue,
                Decl::Struct(d) => (&d.name, d.name_span, DefKind::Struct, d.is_pub),
                Decl::Enum(d) => (&d.name, d.name_span, DefKind::Enum, d.is_pub),
                Decl::Service(d) => (&d.name, d.name_span, DefKind::Service, d.is_pub),
                Decl::Impl(d) => (&d.name, d.name_span, DefKind::Impl, d.is_pub),
                Decl::Func(d) => (&d.name, d.name_span, DefKind::Func, d.is_pub),
            };
            let module = self.module_of(span);
            let dup = match kind {
                DefKind::Struct | DefKind::Enum => {
                    self.structs.contains_key(name) || self.enums.contains_key(name)
                }
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
                DefKind::Struct => {
                    self.structs.insert(
                        name.clone(),
                        StructInfo { fields: Vec::new(), name_span: span, module, is_pub },
                    );
                }
                DefKind::Enum => {
                    self.enums.insert(
                        name.clone(),
                        EnumInfo { variants: Vec::new(), name_span: span, module, is_pub },
                    );
                }
                DefKind::Service => {
                    self.services.insert(
                        name.clone(),
                        ServiceInfo {
                            methods: Vec::new(),
                            name_span: span,
                            module,
                            is_pub,
                            shared: false,
                        },
                    );
                }
                _ => {}
            }
        }

        // Second sweep: full signatures.
        for decl in &self.program.decls {
            match decl {
                Decl::Use(_) => {}
                Decl::Struct(d) => {
                    let mut fields = Vec::new();
                    let mut tyvars = HashMap::new();
                    for field in &d.fields {
                        let ty = match &field.ty {
                            Some(t) => self.resolve_type_expr(t, &mut tyvars),
                            None => self.ctx.fresh(),
                        };
                        fields.push((field.name.clone(), ty));
                    }
                    if let Some(info) = self.structs.get_mut(&d.name) {
                        info.fields = fields;
                    }
                }
                Decl::Enum(d) => {
                    let mut variants = Vec::new();
                    for variant in &d.variants {
                        let taken = variant.name == "Some"
                            || variant.name == "None"
                            || self.structs.contains_key(&variant.name)
                            || self.enums.contains_key(&variant.name)
                            || self.variant_owner.contains_key(&variant.name);
                        if taken {
                            self.error(
                                variant.name_span,
                                format!("variant name `{}` is already taken", variant.name),
                            );
                            continue;
                        }
                        let mut tyvars = HashMap::new();
                        let mut fields = Vec::new();
                        for field in &variant.fields {
                            let ty = match &field.ty {
                                Some(t) => self.resolve_type_expr(t, &mut tyvars),
                                None => self.ctx.fresh(),
                            };
                            fields.push((field.name.clone(), ty));
                        }
                        self.variant_owner.insert(variant.name.clone(), d.name.clone());
                        variants.push((variant.name.clone(), fields));
                    }
                    if let Some(info) = self.enums.get_mut(&d.name) {
                        info.variants = variants;
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
                        info.shared = d.is_shared;
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
                    let module = self.module_of(d.name_span);
                    self.impls.insert(
                        d.name.clone(),
                        ImplInfo {
                            service: d.service.clone(),
                            fields,
                            name_span: d.name_span,
                            module,
                            is_pub: d.is_pub,
                        },
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
                    let rigid: Vec<u32> = tyvars
                        .values()
                        .filter_map(|t| match t {
                            Type::Var(v) => Some(*v),
                            _ => None,
                        })
                        .collect();
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
                            module: self.module_of(d.name_span),
                            is_pub: d.is_pub,
                            rigid,
                        },
                    );
                }
            }
        }
    }

    /// A `!` row names the *types* of values the function can fail with:
    /// structs, enums, or primitives.
    fn resolve_error_list(&mut self, list: Option<&[(String, Span)]>) -> BTreeSet<String> {
        let mut set = BTreeSet::new();
        for (name, span) in list.unwrap_or(&[]) {
            if PRIMITIVE_TAGS.contains(&name.as_str())
                || self.structs.contains_key(name)
                || self.enums.contains_key(name)
            {
                set.insert(name.clone());
            } else {
                self.error(*span, format!("unknown type `{name}` in `!` row"));
            }
        }
        set
    }

    /// The `!` row tag for a type, if values of it can be failed.
    fn row_tag_of(&self, ty: &Type) -> Result<Option<String>, ()> {
        Ok(match self.ctx.resolve(ty) {
            Type::Named(n) | Type::Enum(n) => Some(n),
            Type::Int => Some("Int".into()),
            Type::Float => Some("Float".into()),
            Type::Bool => Some("Bool".into()),
            Type::Str => Some("String".into()),
            Type::Duration => Some("Duration".into()),
            Type::Var(_) | Type::Unknown => None,
            _ => return Err(()),
        })
    }

    /// Add a failed value's type to the error row; diagnose unfailable types.
    fn add_fail_row(&mut self, ty: &Type, span: Span, what: &str) {
        if self.is_rigid(ty) {
            let rendered = self.render(ty);
            self.error(
                span,
                format!("cannot `{what}` with the type parameter {rendered} (the `!` row needs a concrete type)"),
            );
            return;
        }
        match self.row_tag_of(ty) {
            Ok(Some(tag)) => self.add_error_row(&tag),
            Ok(None) => {}
            Err(()) => {
                let rendered = self.render(&self.ctx.resolve(ty));
                self.error(
                    span,
                    format!(
                        "cannot `{what}` with a value of type {rendered} (use a struct, enum, or primitive value)"
                    ),
                );
            }
        }
    }

    /// Resolve a row tag name back to a value type (`String` -> Str, struct
    /// and enum names -> their nominal types).
    fn tag_type(&self, name: &str) -> Option<Type> {
        match name {
            "Int" => Some(Type::Int),
            "Float" => Some(Type::Float),
            "Bool" => Some(Type::Bool),
            "String" => Some(Type::Str),
            "Duration" => Some(Type::Duration),
            _ if self.structs.contains_key(name) => Some(Type::Named(name.to_string())),
            _ if self.enums.contains_key(name) => Some(Type::Enum(name.to_string())),
            _ => None,
        }
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
                _ if self.structs.contains_key(name) => {
                    let info = &self.structs[name];
                    let (m, p) = (info.module, info.is_pub);
                    self.gate(name, m, p, *span);
                    Type::Named(name.clone())
                }
                _ if self.enums.contains_key(name) => {
                    let info = &self.enums[name];
                    let (m, p) = (info.module, info.is_pub);
                    self.gate(name, m, p, *span);
                    Type::Enum(name.clone())
                }
                _ if self.services.contains_key(name) => {
                    let info = &self.services[name];
                    let (m, p) = (info.module, info.is_pub);
                    self.gate(name, m, p, *span);
                    Type::Service(name.clone())
                }
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
            TypeExpr::Apply { name, name_span, args, row, .. } => {
                if !row.is_empty() && name != "Fiber" && name != "Outcome" {
                    self.error(
                        *name_span,
                        format!("only `Fiber` and `Outcome` carry an error row, not `{name}`"),
                    );
                }
                let mut row_set = BTreeSet::new();
                for (n, span) in row {
                    if self.tag_type(n).is_some() {
                        row_set.insert(n.clone());
                    } else {
                        self.diags
                            .push(Diagnostic::error(*span, format!("unknown error type `{n}`")));
                    }
                }
                match (name.as_str(), args.len()) {
                    ("MutMap", 2) => Type::MutMap(
                        Box::new(self.resolve_type_expr(&args[0], tyvars)),
                        Box::new(self.resolve_type_expr(&args[1], tyvars)),
                    ),
                    ("MutList", 1) => {
                        Type::MutList(Box::new(self.resolve_type_expr(&args[0], tyvars)))
                    }
                    ("Fiber", 1) => Type::Fiber(
                        Box::new(self.resolve_type_expr(&args[0], tyvars)),
                        Rc::new(RefCell::new(row_set)),
                    ),
                    ("Outcome", 1) => Type::Outcome(
                        Box::new(self.resolve_type_expr(&args[0], tyvars)),
                        Rc::new(RefCell::new(row_set)),
                    ),
                    _ => {
                        for arg in args {
                            self.resolve_type_expr(arg, tyvars);
                        }
                        self.error(
                            *name_span,
                            format!(
                                "`{name}` does not take type arguments (only `MutMap<K, V>`, `MutList<T>`, `Fiber<T ! E>`, and `Outcome<T ! E>` do)"
                            ),
                        );
                        Type::Unknown
                    }
                }
            }
            TypeExpr::Option(inner, _) => {
                Type::Option(Box::new(self.resolve_type_expr(inner, tyvars)))
            }
            TypeExpr::List(inner, _) => {
                Type::List(Box::new(self.resolve_type_expr(inner, tyvars)))
            }
            TypeExpr::Tuple(items, _) => {
                Type::Tuple(items.iter().map(|t| self.resolve_type_expr(t, tyvars)).collect())
            }
            TypeExpr::Func { params, ret, errors, caps, .. } => {
                let params: Vec<Type> =
                    params.iter().map(|t| self.resolve_type_expr(t, tyvars)).collect();
                let ret = self.resolve_type_expr(ret, tyvars);
                let errors = self.resolve_error_list(Some(errors));
                let mut cap_set = BTreeSet::new();
                for (name, span) in caps {
                    if self.services.contains_key(name) {
                        cap_set.insert(name.clone());
                    } else {
                        self.error(*span, format!("unknown service `{name}` in `uses`"));
                    }
                }
                Type::Func(Rc::new(FuncType { params, ret, errors, caps: cap_set }))
            }
        }
    }

    /// Check an argument against its expected type. Lambda arguments get
    /// their parameters seeded from the expected function type, so tuple
    /// and field access on them infers in one pass (bidirectional checking
    /// for the common callback case).
    fn check_arg_expecting(&mut self, arg: &Expr, expected: &Type) -> Type {
        if let ExprKind::Lambda { params, body } = &arg.kind {
            if let Type::Func(ef) = self.ctx.resolve(expected) {
                if ef.params.len() == params.len() {
                    let mut scope = HashMap::new();
                    let mut tyvars = HashMap::new();
                    let mut param_types = Vec::new();
                    for (param, ety) in params.iter().zip(ef.params.iter()) {
                        let ty = match &param.ty {
                            Some(t) => self.resolve_type_expr(t, &mut tyvars),
                            None => self.ctx.fresh(),
                        };
                        let _ = self.ctx.unify(&ty, ety);
                        scope.insert(param.name.clone(), ty.clone());
                        param_types.push(ty);
                    }
                    self.scopes.push(scope);
                    let (ret, rows) = self.with_rows(|s| s.check_expr(body));
                    self.scopes.pop();
                    let ty = Type::Func(Rc::new(FuncType {
                        params: param_types,
                        ret,
                        errors: rows.errors,
                        caps: rows.caps,
                    }));
                    if self.record_info {
                        self.raw_expr_types
                            .insert((arg.span.start, arg.span.end), ty.clone());
                    }
                    return ty;
                }
            }
        }
        self.check_expr(arg)
    }

    /// The variable id a rigid (universal) parameter currently resolves to —
    /// body inference may alias it into another variable.
    fn rigid_ids(&self, rigid: &[u32]) -> Vec<u32> {
        rigid
            .iter()
            .filter_map(|v| match self.ctx.resolve(&Type::Var(*v)) {
                Type::Var(w) => Some(w),
                _ => None, // bound to a concrete type: monomorphic now
            })
            .collect()
    }

    /// Instantiate a generic function type: rigid (universal) variables that
    /// are still unbound get fresh variables, consistently across the type.
    fn instantiate(&mut self, ty: &Type, rigid: &[u32], map: &mut HashMap<u32, Type>) -> Type {
        match self.ctx.resolve(ty) {
            Type::Var(v) if rigid.contains(&v) => {
                if let Some(t) = map.get(&v) {
                    t.clone()
                } else {
                    let fresh = self.ctx.fresh();
                    map.insert(v, fresh.clone());
                    fresh
                }
            }
            Type::Option(t) => Type::Option(Box::new(self.instantiate(&t, rigid, map))),
            Type::List(t) => Type::List(Box::new(self.instantiate(&t, rigid, map))),
            Type::Tuple(ts) => {
                Type::Tuple(ts.iter().map(|t| self.instantiate(t, rigid, map)).collect())
            }
            Type::MutMap(k, v) => Type::MutMap(
                Box::new(self.instantiate(&k, rigid, map)),
                Box::new(self.instantiate(&v, rigid, map)),
            ),
            Type::MutList(t) => Type::MutList(Box::new(self.instantiate(&t, rigid, map))),
            Type::Func(f) => Type::Func(Rc::new(FuncType {
                params: f.params.iter().map(|t| self.instantiate(t, rigid, map)).collect(),
                ret: self.instantiate(&f.ret, rigid, map),
                errors: f.errors.clone(),
                caps: f.caps.clone(),
            })),
            other => other,
        }
    }

    /// Is this an unbound type parameter of the function being checked?
    fn is_rigid(&self, ty: &Type) -> bool {
        matches!(self.ctx.resolve(ty), Type::Var(v) if self.current_rigid.contains(&v))
    }

    /// Annotated function types are contracts: a function value flowing
    /// into one must not have effects the annotation doesn't declare.
    fn enforce_func_rows(&mut self, expected: &Type, found: &Type, span: Span) {
        let exp = self.ctx.resolve(expected);
        let act = self.ctx.resolve(found);
        let (Type::Func(exp), Type::Func(act)) = (&exp, &act) else { return };
        for e in act.errors.difference(&exp.errors) {
            let rendered = self.render(expected);
            self.error(
                span,
                format!("this function can fail with `{e}`, but the expected type {rendered} does not declare it"),
            );
        }
        for c in act.caps.difference(&exp.caps) {
            let rendered = self.render(expected);
            self.error(
                span,
                format!("this function uses `{c}`, but the expected type {rendered} does not declare it"),
            );
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
        let rigid = info.rigid.clone();
        self.current_rigid = self.rigid_ids(&rigid).into_iter().collect();

        let mut scope = HashMap::new();
        for (param, ty) in d.sig.params.iter().zip(params.iter()) {
            scope.insert(param.name.clone(), ty.clone());
            if self.record_info {
                self.typed_hovers.push((param.span, param.name.clone(), ty.clone()));
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
        self.current_rigid.clear();
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
                            self.enforce_func_rows(&annotated, &value_ty, value.span);
                            annotated
                        }
                        None => value_ty,
                    };
                    if self.record_info {
                        self.typed_hovers.push((*name_span, name.clone(), bound_ty.clone()));
                    }
                    self.scopes.last_mut().unwrap().insert(name.clone(), bound_ty);
                    result = Type::Unit;
                }
                Stmt::Acquire { service, service_span, name, name_span } => {
                    if service == "<error>" {
                        // Parser already reported.
                    } else if self.services.contains_key(service) {
                        let info = &self.services[service];
                        let (m, p) = (info.module, info.is_pub);
                        self.gate(&service.clone(), m, p, *service_span);
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
            // Raw types; resolved to CTypes only after the pass completes,
            // so later unifications (e.g. a map's key type fixed by a later
            // call) are reflected in earlier expressions' records.
            self.raw_expr_types.insert((expr.span.start, expr.span.end), ty.clone());
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
            Type::Tuple(ts) => CType::Tuple(ts.iter().map(|t| self.ctype(t)).collect()),
            Type::Fiber(t, _) => CType::Fiber(Box::new(self.ctype(&t))),
            Type::Outcome(t, _) => CType::Outcome(Box::new(self.ctype(&t))),
            Type::Named(n) => CType::Struct(n),
            Type::Enum(n) => CType::Enum(n),
            Type::Service(n) => CType::Service(n),
            Type::Tag(n) => CType::Tag(n),
            Type::MutMap(k, v) => {
                CType::MutMap(Box::new(self.ctype(&k)), Box::new(self.ctype(&v)))
            }
            Type::MutList(t) => CType::MutList(Box::new(self.ctype(&t))),
            Type::Func(_) => CType::Func,
            // Unconstrained or error-recovery types default to Int.
            Type::Var(_) | Type::Unknown => CType::Int,
        }
    }

    /// Export effective rows for codegen (called once, after the final pass).
    fn record_facts(&mut self) {
        for name in self.funcs.keys().cloned().collect::<Vec<_>>() {
            let rows = self.func_effective_rows(&name);
            let info = &self.funcs[&name];
            let params: Vec<CType> = info.params.iter().map(|t| self.ctype(t)).collect();
            let ret = self.ctype(&info.ret.clone());
            let generic = !self.rigid_ids(&info.rigid).is_empty();
            self.info.facts.func_params.insert(name.clone(), params);
            self.info.facts.func_ret.insert(name.clone(), ret);
            if generic {
                self.info.facts.generic_funcs.insert(name.clone());
            }
            self.info.facts.funcs.insert(
                name,
                RowFact {
                    errors: rows.errors.iter().cloned().collect(),
                    caps: rows.caps.iter().cloned().collect(),
                },
            );
        }
        for (sname, sinfo) in &self.services {
            for (mname, m) in &sinfo.methods {
                let key = (sname.clone(), mname.clone());
                let params: Vec<CType> = m.params.iter().map(|t| self.ctype(t)).collect();
                self.info.facts.method_params.insert(key.clone(), params);
                self.info.facts.method_ret.insert(key, self.ctype(&m.ret));
            }
        }
        for (name, info) in &self.structs {
            let fields: Vec<CType> = info.fields.iter().map(|(_, t)| self.ctype(t)).collect();
            self.info.facts.struct_fields.insert(name.clone(), fields);
        }
        for (name, info) in &self.enums {
            let variants: Vec<(String, Vec<CType>)> = info
                .variants
                .iter()
                .map(|(v, fields)| {
                    (v.clone(), fields.iter().map(|(_, t)| self.ctype(t)).collect())
                })
                .collect();
            self.info.facts.enum_variants.insert(name.clone(), variants);
        }
        for (name, info) in &self.impls {
            let fields: Vec<CType> = info.fields.iter().map(|(_, t)| self.ctype(t)).collect();
            self.info.facts.impl_fields.insert(name.clone(), fields);
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
            ExprKind::Str(pieces, _) => {
                for piece in pieces {
                    if let StrPiece::Expr(e) = piece {
                        self.check_expr(e);
                    }
                }
                Type::Str
            }
            ExprKind::Var(name) => self.check_var(name, expr.span),
            ExprKind::Tuple(items) => {
                Type::Tuple(items.iter().map(|e| self.check_expr(e)).collect())
            }
            ExprKind::TupleIndex { recv, index, index_span } => {
                let recv_ty = self.check_expr(recv);
                match self.ctx.resolve(&recv_ty) {
                    Type::Tuple(ts) => match usize::try_from(*index).ok().and_then(|i| ts.get(i)) {
                        Some(t) => {
                            if self.record_info {
                                self.typed_hovers.push((*index_span, format!(".{index}"), t.clone()));
                            }
                            t.clone()
                        }
                        None => {
                            self.error(
                                *index_span,
                                format!("tuple has {} element(s), no `.{index}`", ts.len()),
                            );
                            Type::Unknown
                        }
                    },
                    Type::Unknown => Type::Unknown,
                    Type::Var(_) => {
                        self.error(
                            recv.span,
                            "cannot infer the tuple's type here; add a type annotation",
                        );
                        Type::Unknown
                    }
                    other => {
                        let rendered = self.render(&other);
                        self.error(*index_span, format!("{rendered} is not a tuple"));
                        Type::Unknown
                    }
                }
            }
            ExprKind::RecordUpdate { name, name_span, base, fields } => {
                let result = if self.structs.contains_key(name) {
                    let info = &self.structs[name];
                    let (m, p, def_span) = (info.module, info.is_pub, info.name_span);
                    self.gate(name, m, p, *name_span);
                    if self.record_info {
                        self.info.hovers.push((*name_span, self.render_struct_sig(name)));
                        self.info.refs.push((*name_span, def_span));
                    }
                    Type::Named(name.clone())
                } else {
                    let what = if base.is_some() { "record update" } else { "construction" };
                    self.error(*name_span, format!("unknown struct `{name}` in {what}"));
                    Type::Unknown
                };
                if let Some(base) = base {
                    let base_ty = self.check_expr(base);
                    self.unify_at(&result, &base_ty, base.span, "record update base");
                }
                let decl_fields =
                    self.structs.get(name).map(|i| i.fields.clone()).unwrap_or_default();
                let mut seen: Vec<&str> = Vec::new();
                for (fname, fspan, value) in fields {
                    let value_ty = self.check_expr(value);
                    if seen.contains(&fname.as_str()) {
                        self.error(*fspan, format!("field `{fname}` is given twice"));
                    }
                    seen.push(fname);
                    match decl_fields.iter().find(|(f, _)| f == fname) {
                        Some((_, fty)) => {
                            self.unify_at(fty, &value_ty, value.span, "record update field");
                            if self.record_info {
                                self.typed_hovers.push((*fspan, fname.clone(), fty.clone()));
                            }
                        }
                        None => {
                            self.error(*fspan, format!("`{name}` has no field `{fname}`"));
                        }
                    }
                }
                if base.is_none() {
                    let missing: Vec<String> = decl_fields
                        .iter()
                        .filter(|(f, _)| !seen.contains(&f.as_str()))
                        .map(|(f, _)| format!("`{f}`"))
                        .collect();
                    if !missing.is_empty() {
                        self.error(
                            *name_span,
                            format!(
                                "`{name}` is missing {} {} (use `{name} {{ ..base, … }}` to copy the rest from an existing value)",
                                if missing.len() == 1 { "field" } else { "fields" },
                                missing.join(", ")
                            ),
                        );
                    }
                }
                result
            }
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
                        if self.is_rigid(&ty) {
                            let rendered = self.render(&ty);
                            self.error(
                                inner.span,
                                format!("cannot negate the type parameter {rendered}; constrain it with an annotation"),
                            );
                            return Type::Unknown;
                        }
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
            ExprKind::Match { scrutinee, arms } => {
                if self.record_info {
                    self.info
                        .match_ctxs
                        .push((expr.span, (scrutinee.span.start, scrutinee.span.end)));
                }
                self.check_match(scrutinee, arms)
            }
            ExprKind::Fail { error } => {
                let ty = self.check_expr(error);
                self.add_fail_row(&ty, error.span, "fail");
                // `fail` never produces a value; it unifies with anything.
                self.ctx.fresh()
            }
            ExprKind::Provide { impls, body, .. } => self.check_provide(impls, body),
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
                    self.typed_hovers.push((span, name.to_string(), ty.clone()));
                }
                return ty;
            }
        }
        if let Some(info) = self.funcs.get(name) {
            let (def_module, is_pub, def_span) = (info.module, info.is_pub, info.name_span);
            let rigid = self.rigid_ids(&info.rigid);
            let rows = self.func_effective_rows(name);
            let info = &self.funcs[name];
            let (params, ret) = (info.params.clone(), info.ret.clone());
            let mut inst = HashMap::new();
            let func = Type::Func(Rc::new(FuncType {
                params: params.iter().map(|t| self.instantiate(t, &rigid, &mut inst)).collect(),
                ret: self.instantiate(&ret, &rigid, &mut inst),
                errors: rows.errors,
                caps: rows.caps,
            }));
            self.gate(name, def_module, is_pub, span);
            if self.record_info {
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
        if let Some(owner) = self.variant_owner.get(name).cloned() {
            // Enum variants: fieldless ones are values, the rest construct.
            let fields = self.variant_fields(&owner, name);
            if let Some(info) = self.enums.get(&owner) {
                let (m, p) = (info.module, info.is_pub);
                self.gate(name, m, p, span);
            }
            if self.record_info {
                if let Some(info) = self.enums.get(&owner) {
                    self.info.refs.push((span, info.name_span));
                }
            }
            if fields.is_empty() {
                return Type::Enum(owner);
            }
            return Type::Func(Rc::new(FuncType {
                params: fields.iter().map(|(_, t)| t.clone()).collect(),
                ret: Type::Enum(owner),
                errors: BTreeSet::new(),
                caps: BTreeSet::new(),
            }));
        }
        if let Some(info) = self.structs.get(name) {
            // A bare struct name is a type tag (`decode(raw, User)`); calling
            // it constructs a value — `check_call` handles that case directly.
            let (m, p, def_span) = (info.module, info.is_pub, info.name_span);
            self.gate(name, m, p, span);
            if self.record_info {
                self.info.refs.push((span, def_span));
            }
            return Type::Tag(name.to_string());
        }
        if let Some(info) = self.enums.get(name) {
            let (m, p, def_span) = (info.module, info.is_pub, info.name_span);
            self.gate(name, m, p, span);
            if self.record_info {
                self.info.refs.push((span, def_span));
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
        if name == "graphics" || name == "schedule" {
            self.error(
                span,
                format!("the `{name}` module is not imported here: add `use std/{name}`"),
            );
            return Type::Unknown;
        }
        if let Some(import) = self.import_for_alias(span, name) {
            let what = import.target.clone();
            self.error(
                span,
                format!("`{name}` is a module ({what}); call a member like `{name}.something(...)`"),
            );
            return Type::Unknown;
        }
        if let Some(ty) = self.builtin_value_type(name) {
            if self.record_info {
                if let Some(doc) = builtin_doc(name) {
                    self.info.hovers.push((span, doc.to_string()));
                }
            }
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
            _ if BUILTIN_NAMES.contains(&name) => {
                // Other builtins need call-site special handling.
                None
            }
            _ => None,
        }
    }

    // ---- calls -----------------------------------------------------------------

    fn check_call(&mut self, callee: &Expr, args: &[&Expr], span: Span) -> Type {
        // Module-qualified calls: `graphics.rect(...)`, `cards.rankName(c)`.
        if let ExprKind::Field { recv, name, name_span } = &callee.kind {
            if let ExprKind::Var(alias) = &recv.kind {
                if !self.scope_has(alias) {
                    if let Some(import) = self.import_for_alias(recv.span, alias) {
                        return self.check_module_member_call(&import, name, *name_span, args, span);
                    }
                }
            }
        }
        if let ExprKind::Var(name) = &callee.kind {
            if !self.scope_has(name) {
                if let Some(ty) = self.check_builtin_call(name, callee.span, args, span) {
                    return ty;
                }
                // Struct / enum-variant constructors.
                if let Some(fields) = self.structs.get(name).map(|i| i.fields.clone()) {
                    let info = &self.structs[name];
                    let (m, p, def_span) = (info.module, info.is_pub, info.name_span);
                    self.gate(name, m, p, callee.span);
                    if self.record_info {
                        self.info.refs.push((callee.span, def_span));
                        self.info.hovers.push((callee.span, self.render_struct_sig(name)));
                    }
                    return self.check_ctor(name, &fields, args, span, Type::Named(name.clone()));
                }
                if let Some(owner) = self.variant_owner.get(name).cloned() {
                    let fields = self.variant_fields(&owner, name);
                    if let Some(info) = self.enums.get(&owner) {
                        let (m, p) = (info.module, info.is_pub);
                        self.gate(name, m, p, callee.span);
                    }
                    if self.record_info {
                        if let Some(info) = self.enums.get(&owner) {
                            self.info.refs.push((callee.span, info.name_span));
                        }
                        let mut names = Vec::new();
                        let rendered: Vec<String> = fields
                            .iter()
                            .map(|(f, t)| format!("{} {f}", self.ctx.render(t, &mut names)))
                            .collect();
                        self.info.hovers.push((
                            callee.span,
                            format!("{name}({}) — variant of {owner}", rendered.join(", ")),
                        ));
                    }
                    return self.check_ctor(name, &fields, args, span, Type::Enum(owner));
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
                    // A parameter already shaped as a function carries its
                    // row contract; the callee's inferred rows account for
                    // invoking it, so the conservative merge below would
                    // double-count (enforce_func_rows keeps it honest).
                    let contracted = matches!(self.ctx.resolve(param_ty), Type::Func(_));
                    let arg_ty = self.check_arg_expecting(arg, param_ty);
                    self.unify_at(param_ty, &arg_ty, arg.span, "argument");
                    self.enforce_func_rows(param_ty, &arg_ty, arg.span);
                    if !contracted {
                        self.add_func_arg_rows(&arg_ty);
                    }
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

    fn variant_fields(&self, enum_name: &str, variant: &str) -> Vec<(String, Type)> {
        self.enums
            .get(enum_name)
            .and_then(|e| e.variants.iter().find(|(n, _)| n == variant))
            .map(|(_, fields)| fields.clone())
            .unwrap_or_default()
    }

    /// `alias.member(args)` — a std-module builtin or a `pub` member of a
    /// file module.
    fn check_module_member_call(
        &mut self,
        import: &ImportInfo,
        member: &str,
        member_span: Span,
        args: &[&Expr],
        span: Span,
    ) -> Type {
        // Every std member hovers with its signature, in one place.
        if self.record_info {
            if let Some((_, doc)) = std_module_members(&import.target)
                .iter()
                .find(|(n, _)| *n == member)
            {
                self.info.hovers.push((member_span, doc.to_string()));
            }
        }
        match import.target.as_str() {
            "std/graphics" => return self.check_gfx_call(member, member_span, args, span),
            "std/schedule" => return self.check_schedule_call(member, member_span, args, span),
            "std/fiber" => return self.check_fiber_call(member, member_span, args, span),
            "std/http" => return self.check_http_call(member, member_span, args, span),
            "std/json" => return self.check_json_call(member, member_span, args, span),
            "std/fs" => return self.check_fs_call(member, member_span, args, span),
            "std/process" => return self.check_process_call(member, member_span, args, span),
            "std/net" => return self.check_net_call(member, member_span, args, span),
            "std/time" => return self.check_time_call(member, member_span, args, span),
            "std/term" => return self.check_term_call(member, member_span, args, span),
            _ => {}
        }
        let Some(target) = self.modules.iter().position(|m| m.key == import.target) else {
            for arg in args {
                self.check_expr(arg);
            }
            return Type::Unknown;
        };
        let module_name = self.modules[target].name.clone();
        if let Some(info) = self.funcs.get(member) {
            if info.module != target {
                self.error(
                    member_span,
                    format!("module `{module_name}` has no member `{member}`"),
                );
            } else if !info.is_pub {
                self.error(
                    member_span,
                    format!("`{member}` is private to module `{module_name}` (mark it `pub` to export it)"),
                );
            }
            let rigid = self.rigid_ids(&self.funcs[member].rigid);
            let rows = self.func_effective_rows(member);
            let info = &self.funcs[member];
            let (params, ret, def_span) = (info.params.clone(), info.ret.clone(), info.name_span);
            let mut inst = HashMap::new();
            let params: Vec<Type> =
                params.iter().map(|t| self.instantiate(t, &rigid, &mut inst)).collect();
            let ret = self.instantiate(&ret, &rigid, &mut inst);
            if params.len() != args.len() {
                self.error(
                    span,
                    format!("`{member}` expects {} argument(s), found {}", params.len(), args.len()),
                );
            }
            for (param_ty, arg) in params.iter().zip(args.iter()) {
                let arg_ty = self.check_expr(arg);
                self.unify_at(param_ty, &arg_ty, arg.span, "argument");
                self.enforce_func_rows(param_ty, &arg_ty, arg.span);
                self.add_func_arg_rows(&arg_ty);
            }
            for arg in args.iter().skip(params.len()) {
                self.check_expr(arg);
            }
            self.merge_rows(&rows);
            if self.record_info {
                self.info.refs.push((member_span, def_span));
                let sig = self.render_func_signature(member);
                self.info.hovers.push((member_span, sig));
            }
            return ret;
        }
        if let Some(info) = self.structs.get(member) {
            let (m, p, fields) = (info.module, info.is_pub, info.fields.clone());
            if m != target {
                self.error(member_span, format!("module `{module_name}` has no member `{member}`"));
            } else if !p {
                self.error(
                    member_span,
                    format!("`{member}` is private to module `{module_name}` (mark it `pub` to export it)"),
                );
            }
            return self.check_ctor(member, &fields, args, span, Type::Named(member.to_string()));
        }
        if let Some(owner) = self.variant_owner.get(member).cloned() {
            let fields = self.variant_fields(&owner, member);
            if let Some(info) = self.enums.get(&owner) {
                if info.module != target {
                    self.error(member_span, format!("module `{module_name}` has no member `{member}`"));
                } else if !info.is_pub {
                    self.error(
                        member_span,
                        format!("`{owner}` is private to module `{module_name}` (mark it `pub` to export it)"),
                    );
                }
            }
            return self.check_ctor(member, &fields, args, span, Type::Enum(owner));
        }
        self.error(member_span, format!("module `{module_name}` has no member `{member}`"));
        for arg in args {
            self.check_expr(arg);
        }
        Type::Unknown
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
            "upTo" => {
                if args.len() != 2 {
                    self.error(span, "`schedule.upTo` takes (schedule, times)");
                }
                if let Some(arg) = args.first() {
                    let ty = self.check_expr(arg);
                    self.unify_at(&Type::Schedule, &ty, arg.span, "upTo input");
                }
                if let Some(arg) = args.get(1) {
                    let ty = self.check_expr(arg);
                    self.unify_at(&Type::Int, &ty, arg.span, "upTo count");
                }
                if self.record_info {
                    if let Some(doc) = builtin_doc("schedule.upTo") {
                        self.info.hovers.push((name_span, doc.to_string()));
                    }
                }
                Type::Schedule
            }
            "exponential" | "fixed" => {
                if args.len() != 1 {
                    self.error(span, format!("`schedule.{name}` takes one Duration argument"));
                }
                if let Some(arg) = args.first() {
                    let ty = self.check_expr(arg);
                    self.unify_at(&Type::Duration, &ty, arg.span, "schedule base");
                }
                if self.record_info {
                    if let Some(doc) = builtin_doc(&format!("schedule.{name}")) {
                        self.info.hovers.push((name_span, doc.to_string()));
                    }
                }
                Type::Schedule
            }
            _ => {
                self.error(
                    name_span,
                    format!("unknown schedule `schedule.{name}` (try `exponential`, `fixed`, or `upTo`)"),
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
    /// Check a forked-by-name action: its capability row merges into the
    /// spawner (evidence is captured at the fork site) after the `shared`
    /// contract check; its error row is returned to ride in the Fiber type.
    fn check_forked(&mut self, action: &Expr) -> (Type, BTreeSet<String>) {
        let (ty, rows) = self.with_rows(|s| s.check_expr(action));
        for cap in &rows.caps {
            let shared = self.services.get(cap).map(|s| s.shared).unwrap_or(false);
            if !shared {
                self.error(
                    action.span,
                    format!(
                        "only `shared` services cross fiber boundaries: declare \
                         `shared service {cap}` (scalar-only state), or provide a \
                         fresh `{cap}` inside the forked expression"
                    ),
                );
            }
        }
        self.merge_rows(&Rows { errors: BTreeSet::new(), caps: rows.caps });
        self.add_cap_row(FIBERS_SERVICE);
        (ty, rows.errors)
    }

    /// Merge a fiber's error row into the current channel (a join site).
    fn merge_fiber_errors(&mut self, errs: &Rc<RefCell<BTreeSet<String>>>) {
        let errors = errs.borrow().clone();
        self.merge_rows(&Rows { errors, caps: BTreeSet::new() });
    }

    /// `fiber.*` — the std/fiber module (see docs/SPEC.md §6.5).
    fn check_fiber_call(
        &mut self,
        member: &str,
        member_span: Span,
        args: &[&Expr],
        span: Span,
    ) -> Type {
        let arity = |s: &mut Self, n: usize| -> bool {
            if args.len() != n {
                s.error(span, format!("`fiber.{member}` expects {n} argument(s), found {}", args.len()));
                for arg in args {
                    s.check_expr(arg);
                }
                return false;
            }
            true
        };
        match member {
            "fork" => {
                if !arity(self, 1) {
                    return Type::Unknown;
                }
                // Reject the no-op pipeline `x |> fiber.fork |> fiber.join`-style
                // catch confusion early: catching a Fiber-typed expression is a
                // dedicated diagnostic at the catch site, not here.
                let (ty, errs) = self.check_forked(args[0]);
                Type::Fiber(Box::new(ty), Rc::new(RefCell::new(errs)))
            }
            "join" => {
                if !arity(self, 1) {
                    return Type::Unknown;
                }
                let t = self.check_expr(args[0]);
                self.add_cap_row(FIBERS_SERVICE);
                match self.ctx.resolve(&t) {
                    Type::Fiber(a, e) => {
                        self.merge_fiber_errors(&e);
                        *a
                    }
                    Type::Tuple(ts) => {
                        let mut elems = Vec::with_capacity(ts.len());
                        for (i, ft) in ts.iter().enumerate() {
                            match self.ctx.resolve(ft) {
                                Type::Fiber(a, e) => {
                                    self.merge_fiber_errors(&e);
                                    elems.push(*a);
                                }
                                Type::Unknown | Type::Var(_) => elems.push(Type::Unknown),
                                other => {
                                    let rendered = self.render(&other);
                                    self.error(
                                        args[0].span,
                                        format!("`fiber.join` on a tuple needs every slot to be a fiber; slot {i} is {rendered}"),
                                    );
                                    elems.push(Type::Unknown);
                                }
                            }
                        }
                        Type::Tuple(elems)
                    }
                    Type::List(elem) => match self.ctx.resolve(&elem) {
                        Type::Fiber(a, e) => {
                            self.merge_fiber_errors(&e);
                            Type::List(a)
                        }
                        Type::Var(_) => {
                            let a = self.ctx.fresh();
                            let cell = Rc::new(RefCell::new(BTreeSet::new()));
                            let _ = self
                                .ctx
                                .unify(&elem, &Type::Fiber(Box::new(a.clone()), cell.clone()));
                            self.merge_fiber_errors(&cell);
                            Type::List(Box::new(a))
                        }
                        other => {
                            let rendered = self.render(&other);
                            self.error(
                                args[0].span,
                                format!("`fiber.join` on a list needs fiber elements, found [{rendered}]"),
                            );
                            Type::Unknown
                        }
                    },
                    Type::Var(_) => {
                        let a = self.ctx.fresh();
                        let cell = Rc::new(RefCell::new(BTreeSet::new()));
                        self.unify_at(
                            &Type::Fiber(Box::new(a.clone()), cell.clone()),
                            &t,
                            args[0].span,
                            "join input",
                        );
                        self.merge_fiber_errors(&cell);
                        a
                    }
                    Type::Unknown => Type::Unknown,
                    other => {
                        let rendered = self.render(&other);
                        self.error(
                            args[0].span,
                            format!("`fiber.join` works on a fiber, a tuple of fibers, or a list of fibers; found {rendered}"),
                        );
                        Type::Unknown
                    }
                }
            }
            "poll" => {
                if !arity(self, 1) {
                    return Type::Unknown;
                }
                let t = self.check_expr(args[0]);
                let a = self.ctx.fresh();
                let cell = Rc::new(RefCell::new(BTreeSet::new()));
                self.unify_at(
                    &Type::Fiber(Box::new(a.clone()), cell.clone()),
                    &t,
                    args[0].span,
                    "poll input",
                );
                // A failed fiber re-raises at the poll (it is a join with the
                // answer ready), so the row enters the channel here too.
                self.merge_fiber_errors(&cell);
                self.add_cap_row(FIBERS_SERVICE);
                Type::Option(Box::new(a))
            }
            "interrupt" => {
                if !arity(self, 1) {
                    return Type::Unknown;
                }
                let t = self.check_expr(args[0]);
                let a = self.ctx.fresh();
                let cell = Rc::new(RefCell::new(BTreeSet::new()));
                self.unify_at(
                    &Type::Fiber(Box::new(a), cell),
                    &t,
                    args[0].span,
                    "interrupt input",
                );
                self.add_cap_row(FIBERS_SERVICE);
                Type::Unit
            }
            "settle" => {
                if !arity(self, 1) {
                    return Type::Unknown;
                }
                // Runs the action inline (no fork): the error channel moves
                // into the Outcome type; capabilities pass through untouched
                // and no Fibers row is added — settle is an error-channel
                // operator, not a scheduling one.
                let (ty, rows) = self.with_rows(|s| s.check_expr(args[0]));
                if matches!(self.ctx.resolve(&ty), Type::Fiber(..)) {
                    self.error(
                        args[0].span,
                        "`fiber.settle` wraps a failing action, not a fiber; move it inside the forked expression, or settle the join: `fiber.join(f) |> fiber.settle`",
                    );
                }
                self.merge_rows(&Rows { errors: BTreeSet::new(), caps: rows.caps });
                Type::Outcome(Box::new(ty), Rc::new(RefCell::new(rows.errors)))
            }
            "unsettle" => {
                if !arity(self, 1) {
                    return Type::Unknown;
                }
                let t = self.check_expr(args[0]);
                let a = self.ctx.fresh();
                let cell = Rc::new(RefCell::new(BTreeSet::new()));
                self.unify_at(
                    &Type::Outcome(Box::new(a.clone()), cell.clone()),
                    &t,
                    args[0].span,
                    "unsettle input",
                );
                let errors = cell.borrow().clone();
                self.merge_rows(&Rows { errors, caps: BTreeSet::new() });
                a
            }
            "par" => {
                if args.len() < 2 {
                    self.error(span, "`fiber.par` needs at least two actions");
                    for arg in args {
                        self.check_expr(arg);
                    }
                    return Type::Unknown;
                }
                // fork each + join immediately: all error rows enter the
                // channel here, in shape order.
                let mut elems = Vec::with_capacity(args.len());
                for arg in args {
                    let (ty, errs) = self.check_forked(arg);
                    self.merge_rows(&Rows { errors: errs, caps: BTreeSet::new() });
                    elems.push(ty);
                }
                Type::Tuple(elems)
            }
            "parMap" => {
                if !arity(self, 2) {
                    return Type::Unknown;
                }
                let list_ty = self.check_expr(args[0]);
                let a = self.ctx.fresh();
                self.unify_at(
                    &Type::List(Box::new(a.clone())),
                    &list_ty,
                    args[0].span,
                    "parMap input",
                );
                let b = self.ctx.fresh();
                let expected_f = Type::Func(Rc::new(FuncType {
                    params: vec![a],
                    ret: b.clone(),
                    errors: BTreeSet::new(),
                    caps: BTreeSet::new(),
                }));
                let func_ty = self.check_arg_expecting(args[1], &expected_f);
                self.unify_at(&expected_f, &func_ty, args[1].span, "parMap function");
                // The element function's rows behave like a forked action's:
                // caps cross (shared-checked), errors surface at this
                // immediate join.
                if let Type::Func(f) = self.ctx.resolve(&func_ty) {
                    for cap in &f.caps {
                        let shared =
                            self.services.get(cap).map(|s| s.shared).unwrap_or(false);
                        if !shared {
                            self.error(
                                args[1].span,
                                format!(
                                    "only `shared` services cross fiber boundaries: declare \
                                     `shared service {cap}`, or provide a fresh `{cap}` inside \
                                     the element function"
                                ),
                            );
                        }
                    }
                    self.merge_rows(&Rows { errors: f.errors.clone(), caps: f.caps.clone() });
                }
                self.add_cap_row(FIBERS_SERVICE);
                Type::List(Box::new(b))
            }
            "race" => {
                if !arity(self, 2) {
                    return Type::Unknown;
                }
                let (ta, ea) = self.check_forked(args[0]);
                let (tb, eb) = self.check_forked(args[1]);
                self.unify_at(&ta, &tb, args[1].span, "race branches");
                self.merge_rows(&Rows { errors: ea, caps: BTreeSet::new() });
                self.merge_rows(&Rows { errors: eb, caps: BTreeSet::new() });
                ta
            }
            "within" => {
                if !arity(self, 2) {
                    return Type::Unknown;
                }
                let (ty, errs) = self.check_forked(args[0]);
                let d = self.check_expr(args[1]);
                self.unify_at(&Type::Duration, &d, args[1].span, "within deadline");
                self.merge_rows(&Rows { errors: errs, caps: BTreeSet::new() });
                self.add_error_row(TIMEOUT);
                ty
            }
            "partition" => {
                if !arity(self, 1) {
                    return Type::Unknown;
                }
                let t = self.check_expr(args[0]);
                let a = self.ctx.fresh();
                let cell = Rc::new(RefCell::new(BTreeSet::new()));
                let outcome = Type::Outcome(Box::new(a.clone()), cell);
                self.unify_at(
                    &Type::List(Box::new(outcome.clone())),
                    &t,
                    args[0].span,
                    "partition input",
                );
                Type::Tuple(vec![Type::List(Box::new(a)), Type::List(Box::new(outcome))])
            }
            _ => {
                self.error(
                    member_span,
                    format!("`std/fiber` has no member `{member}` (fork, join, poll, interrupt, settle, unsettle, par, parMap, race, within, partition)"),
                );
                for arg in args {
                    self.check_expr(arg);
                }
                Type::Unknown
            }
        }
    }

    /// `http.*` — std/http: a blocking client whose calls park the fiber.
    /// Transport failures raise HttpError; a non-2xx status is data.
    fn check_http_call(
        &mut self,
        member: &str,
        member_span: Span,
        args: &[&Expr],
        span: Span,
    ) -> Type {
        let arity = |s: &mut Self, n: usize| -> bool {
            if args.len() != n {
                s.error(
                    span,
                    format!("`http.{member}` expects {n} argument(s), found {}", args.len()),
                );
                for arg in args {
                    s.check_expr(arg);
                }
                return false;
            }
            true
        };
        let str_arg = |s: &mut Self, i: usize| {
            let t = s.check_expr(args[i]);
            s.unify_at(&Type::Str, &t, args[i].span, "http argument");
        };
        match member {
            "get" | "openStream" => {
                if !arity(self, 1) {
                    return Type::Unknown;
                }
                str_arg(self, 0);
                self.add_cap_row(HTTP_SERVICE);
                self.add_error_row(HTTP_ERROR);
                if member == "get" {
                    Type::Named("HttpResponse".into())
                } else {
                    Type::Named("HttpStream".into())
                }
            }
            "post" => {
                if !arity(self, 2) {
                    return Type::Unknown;
                }
                str_arg(self, 0);
                str_arg(self, 1);
                self.add_cap_row(HTTP_SERVICE);
                self.add_error_row(HTTP_ERROR);
                Type::Named("HttpResponse".into())
            }
            "send" => {
                if !arity(self, 4) {
                    return Type::Unknown;
                }
                str_arg(self, 0);
                str_arg(self, 1);
                str_arg(self, 2);
                let headers = self.check_expr(args[3]);
                self.unify_at(
                    &Type::List(Box::new(Type::Tuple(vec![Type::Str, Type::Str]))),
                    &headers,
                    args[3].span,
                    "http headers",
                );
                self.add_cap_row(HTTP_SERVICE);
                self.add_error_row(HTTP_ERROR);
                Type::Named("HttpResponse".into())
            }
            "read" => {
                if !arity(self, 1) {
                    return Type::Unknown;
                }
                let t = self.check_expr(args[0]);
                self.unify_at(&Type::Named("HttpStream".into()), &t, args[0].span, "http stream");
                self.add_cap_row(HTTP_SERVICE);
                self.add_error_row(HTTP_ERROR);
                Type::Option(Box::new(Type::Str))
            }
            "close" => {
                if !arity(self, 1) {
                    return Type::Unknown;
                }
                let t = self.check_expr(args[0]);
                self.unify_at(&Type::Named("HttpStream".into()), &t, args[0].span, "http stream");
                self.add_cap_row(HTTP_SERVICE);
                Type::Unit
            }
            "serve" => {
                if !arity(self, 2) {
                    return Type::Unknown;
                }
                let port_ty = self.check_expr(args[0]);
                self.unify_at(&Type::Int, &port_ty, args[0].span, "http.serve port");
                let expected = Type::Func(Rc::new(FuncType {
                    params: vec![Type::Named("HttpRequest".into())],
                    ret: Type::Named("HttpResponse".into()),
                    errors: BTreeSet::new(),
                    caps: BTreeSet::new(),
                }));
                let handler_ty = self.check_arg_expecting(args[1], &expected);
                self.unify_at(&expected, &handler_ty, args[1].span, "http.serve handler");
                // The handler's rows surface here: a failing handler makes
                // `serve` fallible (it answers that client 500, then
                // re-raises) — catch inside the handler to keep serving.
                self.add_func_arg_rows(&handler_ty);
                self.add_cap_row(HTTP_SERVICE);
                self.add_error_row(HTTP_ERROR);
                Type::Unit
            }
            _ => {
                self.error(
                    member_span,
                    format!("`std/http` has no member `{member}` (get, post, send, openStream, read, close, serve)"),
                );
                for arg in args {
                    self.check_expr(arg);
                }
                Type::Unknown
            }
        }
    }

    /// `fs.*` — std/fs: blocking file I/O. Failures raise IoError; the
    /// `Fs` capability names the file system in the row, like `Http`.
    fn check_fs_call(
        &mut self,
        member: &str,
        member_span: Span,
        args: &[&Expr],
        span: Span,
    ) -> Type {
        let arity = |s: &mut Self, n: usize| -> bool {
            if args.len() != n {
                s.error(
                    span,
                    format!("`fs.{member}` expects {n} argument(s), found {}", args.len()),
                );
                for arg in args {
                    s.check_expr(arg);
                }
                return false;
            }
            true
        };
        let str_arg = |s: &mut Self, i: usize| {
            let t = s.check_expr(args[i]);
            s.unify_at(&Type::Str, &t, args[i].span, "fs argument");
        };
        match member {
            "read" => {
                if !arity(self, 1) {
                    return Type::Unknown;
                }
                str_arg(self, 0);
                self.add_cap_row(FS_SERVICE);
                self.add_error_row(IO_ERROR);
                Type::Str
            }
            "write" | "append" => {
                if !arity(self, 2) {
                    return Type::Unknown;
                }
                str_arg(self, 0);
                str_arg(self, 1);
                self.add_cap_row(FS_SERVICE);
                self.add_error_row(IO_ERROR);
                Type::Unit
            }
            "exists" => {
                if !arity(self, 1) {
                    return Type::Unknown;
                }
                str_arg(self, 0);
                self.add_cap_row(FS_SERVICE);
                Type::Bool
            }
            "list" => {
                if !arity(self, 1) {
                    return Type::Unknown;
                }
                str_arg(self, 0);
                self.add_cap_row(FS_SERVICE);
                self.add_error_row(IO_ERROR);
                Type::List(Box::new(Type::Str))
            }
            "remove" | "createDir" => {
                if !arity(self, 1) {
                    return Type::Unknown;
                }
                str_arg(self, 0);
                self.add_cap_row(FS_SERVICE);
                self.add_error_row(IO_ERROR);
                Type::Unit
            }
            "open" => {
                if !arity(self, 2) {
                    return Type::Unknown;
                }
                str_arg(self, 0);
                str_arg(self, 1);
                self.add_cap_row(FS_SERVICE);
                self.add_error_row(IO_ERROR);
                Type::Named("File".into())
            }
            "readAt" => {
                if !arity(self, 3) {
                    return Type::Unknown;
                }
                let t = self.check_expr(args[0]);
                self.unify_at(&Type::Named("File".into()), &t, args[0].span, "fs file");
                for arg in &args[1..] {
                    let t = self.check_expr(arg);
                    self.unify_at(&Type::Int, &t, arg.span, "fs argument");
                }
                self.add_cap_row(FS_SERVICE);
                self.add_error_row(IO_ERROR);
                Type::Str
            }
            "writeAt" => {
                if !arity(self, 3) {
                    return Type::Unknown;
                }
                let t = self.check_expr(args[0]);
                self.unify_at(&Type::Named("File".into()), &t, args[0].span, "fs file");
                let off = self.check_expr(args[1]);
                self.unify_at(&Type::Int, &off, args[1].span, "fs offset");
                let bytes = self.check_expr(args[2]);
                self.unify_at(&Type::Str, &bytes, args[2].span, "fs bytes");
                self.add_cap_row(FS_SERVICE);
                self.add_error_row(IO_ERROR);
                Type::Unit
            }
            "size" | "sync" | "close" => {
                if !arity(self, 1) {
                    return Type::Unknown;
                }
                let t = self.check_expr(args[0]);
                self.unify_at(&Type::Named("File".into()), &t, args[0].span, "fs file");
                self.add_cap_row(FS_SERVICE);
                if member != "close" {
                    self.add_error_row(IO_ERROR);
                }
                match member {
                    "size" => Type::Int,
                    _ => Type::Unit,
                }
            }
            _ => {
                self.error(
                    member_span,
                    format!("`std/fs` has no member `{member}` (read, write, append, exists, list, remove, createDir, open, readAt, writeAt, size, sync, close)"),
                );
                for arg in args {
                    self.check_expr(arg);
                }
                Type::Unknown
            }
        }
    }

    /// `term.*` — std/term: the controlling terminal (raw mode, keys,
    /// size). ANSI output goes through plain `print`.
    fn check_term_call(
        &mut self,
        member: &str,
        member_span: Span,
        args: &[&Expr],
        span: Span,
    ) -> Type {
        let arity = |s: &mut Self, n: usize| -> bool {
            if args.len() != n {
                s.error(
                    span,
                    format!("`term.{member}` expects {n} argument(s), found {}", args.len()),
                );
                for arg in args {
                    s.check_expr(arg);
                }
                return false;
            }
            true
        };
        match member {
            "rawOn" => {
                if !arity(self, 0) {
                    return Type::Unknown;
                }
                self.add_cap_row(TERM_SERVICE);
                self.add_error_row(IO_ERROR);
                Type::Unit
            }
            "rawOff" => {
                if !arity(self, 0) {
                    return Type::Unknown;
                }
                self.add_cap_row(TERM_SERVICE);
                Type::Unit
            }
            "readKey" => {
                if !arity(self, 0) {
                    return Type::Unknown;
                }
                self.add_cap_row(TERM_SERVICE);
                Type::Str
            }
            "size" => {
                if !arity(self, 0) {
                    return Type::Unknown;
                }
                self.add_cap_row(TERM_SERVICE);
                Type::Tuple(vec![Type::Int, Type::Int])
            }
            _ => {
                self.error(
                    member_span,
                    format!("`std/term` has no member `{member}` (rawOn, rawOff, readKey, size)"),
                );
                for arg in args {
                    self.check_expr(arg);
                }
                Type::Unknown
            }
        }
    }

    /// `time.*` — std/time: the wall clock. Ambient like `env` and
    /// `nowMillis` — reading the clock is not an effect worth a row.
    fn check_time_call(
        &mut self,
        member: &str,
        member_span: Span,
        args: &[&Expr],
        span: Span,
    ) -> Type {
        let arity = |s: &mut Self, n: usize| -> bool {
            if args.len() != n {
                s.error(
                    span,
                    format!("`time.{member}` expects {n} argument(s), found {}", args.len()),
                );
                for arg in args {
                    s.check_expr(arg);
                }
                return false;
            }
            true
        };
        match member {
            "now" => {
                if !arity(self, 0) {
                    return Type::Unknown;
                }
                Type::Int
            }
            "utc" => {
                if !arity(self, 1) {
                    return Type::Unknown;
                }
                let t = self.check_expr(args[0]);
                self.unify_at(&Type::Int, &t, args[0].span, "time argument");
                Type::Named("DateTime".into())
            }
            "iso" => {
                if !arity(self, 1) {
                    return Type::Unknown;
                }
                let t = self.check_expr(args[0]);
                self.unify_at(&Type::Int, &t, args[0].span, "time argument");
                Type::Str
            }
            _ => {
                self.error(
                    member_span,
                    format!("`std/time` has no member `{member}` (now, utc, iso)"),
                );
                for arg in args {
                    self.check_expr(arg);
                }
                Type::Unknown
            }
        }
    }

    /// `net.*` — std/net: raw TCP. Failures raise NetError; `Net` names
    /// the network in the row, like `Http` and `Fs`.
    fn check_net_call(
        &mut self,
        member: &str,
        member_span: Span,
        args: &[&Expr],
        span: Span,
    ) -> Type {
        let arity = |s: &mut Self, n: usize| -> bool {
            if args.len() != n {
                s.error(
                    span,
                    format!("`net.{member}` expects {n} argument(s), found {}", args.len()),
                );
                for arg in args {
                    s.check_expr(arg);
                }
                return false;
            }
            true
        };
        match member {
            "connect" => {
                if !arity(self, 2) {
                    return Type::Unknown;
                }
                let host = self.check_expr(args[0]);
                self.unify_at(&Type::Str, &host, args[0].span, "net host");
                let port = self.check_expr(args[1]);
                self.unify_at(&Type::Int, &port, args[1].span, "net port");
                self.add_cap_row(NET_SERVICE);
                self.add_error_row(NET_ERROR);
                Type::Named("Socket".into())
            }
            "listen" => {
                if !arity(self, 1) {
                    return Type::Unknown;
                }
                let port = self.check_expr(args[0]);
                self.unify_at(&Type::Int, &port, args[0].span, "net port");
                self.add_cap_row(NET_SERVICE);
                self.add_error_row(NET_ERROR);
                Type::Named("Listener".into())
            }
            "accept" => {
                if !arity(self, 1) {
                    return Type::Unknown;
                }
                let l = self.check_expr(args[0]);
                self.unify_at(&Type::Named("Listener".into()), &l, args[0].span, "net listener");
                self.add_cap_row(NET_SERVICE);
                self.add_error_row(NET_ERROR);
                Type::Named("Socket".into())
            }
            "read" => {
                if !arity(self, 2) {
                    return Type::Unknown;
                }
                let sock = self.check_expr(args[0]);
                self.unify_at(&Type::Named("Socket".into()), &sock, args[0].span, "net socket");
                let max = self.check_expr(args[1]);
                self.unify_at(&Type::Int, &max, args[1].span, "net read size");
                self.add_cap_row(NET_SERVICE);
                self.add_error_row(NET_ERROR);
                Type::Option(Box::new(Type::Str))
            }
            "write" => {
                if !arity(self, 2) {
                    return Type::Unknown;
                }
                let sock = self.check_expr(args[0]);
                self.unify_at(&Type::Named("Socket".into()), &sock, args[0].span, "net socket");
                let bytes = self.check_expr(args[1]);
                self.unify_at(&Type::Str, &bytes, args[1].span, "net bytes");
                self.add_cap_row(NET_SERVICE);
                self.add_error_row(NET_ERROR);
                Type::Unit
            }
            "close" => {
                if !arity(self, 1) {
                    return Type::Unknown;
                }
                let sock = self.check_expr(args[0]);
                self.unify_at(&Type::Named("Socket".into()), &sock, args[0].span, "net socket");
                self.add_cap_row(NET_SERVICE);
                Type::Unit
            }
            "stop" => {
                if !arity(self, 1) {
                    return Type::Unknown;
                }
                let l = self.check_expr(args[0]);
                self.unify_at(&Type::Named("Listener".into()), &l, args[0].span, "net listener");
                self.add_cap_row(NET_SERVICE);
                Type::Unit
            }
            _ => {
                self.error(
                    member_span,
                    format!("`std/net` has no member `{member}` (connect, listen, accept, read, write, close, stop)"),
                );
                for arg in args {
                    self.check_expr(arg);
                }
                Type::Unknown
            }
        }
    }

    /// `process.*` — std/process: the program's own runtime context.
    /// Ambient like `env` — process metadata is not an effect worth a row.
    fn check_process_call(
        &mut self,
        member: &str,
        member_span: Span,
        args: &[&Expr],
        span: Span,
    ) -> Type {
        let arity = |s: &mut Self, n: usize| -> bool {
            if args.len() != n {
                s.error(
                    span,
                    format!("`process.{member}` expects {n} argument(s), found {}", args.len()),
                );
                for arg in args {
                    s.check_expr(arg);
                }
                return false;
            }
            true
        };
        match member {
            "args" => {
                if !arity(self, 0) {
                    return Type::Unknown;
                }
                Type::List(Box::new(Type::Str))
            }
            "cwd" => {
                if !arity(self, 0) {
                    return Type::Unknown;
                }
                Type::Str
            }
            "exit" => {
                if !arity(self, 1) {
                    return Type::Unknown;
                }
                let t = self.check_expr(args[0]);
                self.unify_at(&Type::Int, &t, args[0].span, "exit code");
                Type::Unit
            }
            _ => {
                self.error(
                    member_span,
                    format!("`std/process` has no member `{member}` (args, cwd, exit)"),
                );
                for arg in args {
                    self.check_expr(arg);
                }
                Type::Unknown
            }
        }
    }

    /// `json.*` — std/json: JSON text to and from Inga values.
    fn check_json_call(
        &mut self,
        member: &str,
        member_span: Span,
        args: &[&Expr],
        span: Span,
    ) -> Type {
        let arity = |s: &mut Self, n: usize| -> bool {
            if args.len() != n {
                s.error(
                    span,
                    format!("`json.{member}` expects {n} argument(s), found {}", args.len()),
                );
                for arg in args {
                    s.check_expr(arg);
                }
                return false;
            }
            true
        };
        match member {
            "encode" => {
                if !arity(self, 1) {
                    return Type::Unknown;
                }
                self.check_expr(args[0]);
                Type::Str
            }
            "decode" => {
                if !arity(self, 2) {
                    return Type::Unknown;
                }
                let raw_ty = self.check_expr(args[0]);
                self.unify_at(&Type::Str, &raw_ty, args[0].span, "json.decode input");
                self.add_error_row(DECODE_ERROR);
                let tag_ty = self.check_expr(args[1]);
                match self.ctx.resolve(&tag_ty) {
                    Type::Tag(type_name) if self.structs.contains_key(&type_name) => {
                        Type::Named(type_name)
                    }
                    Type::Unknown => Type::Unknown,
                    other => {
                        let rendered = self.render(&other);
                        self.error(
                            args[1].span,
                            format!("`json.decode` needs a struct name (like `User`), found {rendered}"),
                        );
                        Type::Unknown
                    }
                }
            }
            _ => {
                self.error(
                    member_span,
                    format!("`std/json` has no member `{member}` (encode, decode)"),
                );
                for arg in args {
                    self.check_expr(arg);
                }
                Type::Unknown
            }
        }
    }

    fn check_gfx_call(
        &mut self,
        name: &str,
        name_span: Span,
        args: &[&Expr],
        span: Span,
    ) -> Type {
        if !self.std_imported(name_span, "std/graphics") {
            self.error(name_span, "the graphics module is not imported here: add `use std/graphics`");
        }
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
            "shaderNew" => Some((vec![Type::Str], Type::Int)),
            "imageNew" => Some((vec![Type::Str], Type::Int)),
            "image" => Some((vec![Type::Int; 5], Type::Unit)),
            "shaderUse" => Some((vec![Type::Int], Type::Unit)),
            "shaderOff" => Some((vec![], Type::Unit)),
            _ => {
                self.error(
                    name_span,
                    format!(
                        "unknown graphics call `graphics.{name}` (run, clear, rect, rectLines, circle, text, textWidth, mouseX, mouseY, mousePressed, shaderNew, shaderUse, shaderOff, imageNew, image)"
                    ),
                );
                for arg in args {
                    self.check_expr(arg);
                }
                return Type::Unknown;
            }
        };
        if self.record_info {
            if let Some(doc) = builtin_doc(&format!("graphics.{name}")) {
                self.info.hovers.push((name_span, doc.to_string()));
            }
        }
        if name == "run" {
            // Gfx.run(Int width, Int height, String title, frame) — the
            // runtime owns the event loop and calls `frame` once per frame.
            if args.len() != 4 {
                self.error(span, "`graphics.run` takes (width, height, title, frame)");
            }
            for (i, expected) in [Type::Int, Type::Int, Type::Str].iter().enumerate() {
                if let Some(arg) = args.get(i) {
                    let ty = self.check_expr(arg);
                    self.unify_at(expected, &ty, arg.span, "graphics.run argument");
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
                self.unify_at(&expected, &frame_ty, frame.span, "graphics.run frame closure");
                // The closure's rows surface at this call site.
                self.add_func_arg_rows(&frame_ty);
            }
            return Type::Unit;
        }
        let (params, ret) = sig.unwrap();
        if args.len() != params.len() {
            self.error(
                span,
                format!("`graphics.{name}` expects {} argument(s), found {}", params.len(), args.len()),
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
            "show" => {
                if check_arity(self, 1) {
                    self.check_expr(args[0]);
                }
                Type::Str
            }
            "map" => {
                if !check_arity(self, 2) {
                    return Some(Type::Unknown);
                }
                let container_ty = self.check_expr(args[0]);
                let a = self.ctx.fresh();
                let b = self.ctx.fresh();
                // Learn the element type from the container first, so the
                // lambda's parameter is seeded.
                match self.ctx.resolve(&container_ty) {
                    Type::List(_) => {
                        let _ = self.ctx.unify(&Type::List(Box::new(a.clone())), &container_ty);
                    }
                    Type::Option(_) => {
                        let _ = self.ctx.unify(&Type::Option(Box::new(a.clone())), &container_ty);
                    }
                    _ => {}
                }
                let expected_f = Type::Func(Rc::new(FuncType {
                    params: vec![a.clone()],
                    ret: b.clone(),
                    errors: BTreeSet::new(),
                    caps: BTreeSet::new(),
                }));
                let func_ty = self.check_arg_expecting(args[1], &expected_f);
                self.add_func_arg_rows(&func_ty);
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
                self.add_fail_row(&err_ty, args[1].span, "orFail");
                a
            }
            "then" => {
                if !check_arity(self, 2) {
                    return Some(Type::Unknown);
                }
                // Transform the value mid-pipe: `x |> then((u) -> u.name)`.
                // `map` transforms each element of a list; `then` transforms
                // the value itself. The function's rows merge like any call.
                let v_ty = self.check_expr(args[0]);
                let out = self.ctx.fresh();
                let expected_f = Type::Func(Rc::new(FuncType {
                    params: vec![v_ty],
                    ret: out.clone(),
                    errors: BTreeSet::new(),
                    caps: BTreeSet::new(),
                }));
                let func_ty = self.check_arg_expecting(args[1], &expected_f);
                self.add_func_arg_rows(&func_ty);
                self.unify_at(&expected_f, &func_ty, args[1].span, "then function");
                out
            }
            "tap" => {
                if !check_arity(self, 2) {
                    return Some(Type::Unknown);
                }
                // Run a side effect on the value flowing through a pipe,
                // pass the value along untouched. The effect returns Unit —
                // tap is for effects; a result would be discarded anyway.
                let v_ty = self.check_expr(args[0]);
                let expected_f = Type::Func(Rc::new(FuncType {
                    params: vec![v_ty.clone()],
                    ret: Type::Unit,
                    errors: BTreeSet::new(),
                    caps: BTreeSet::new(),
                }));
                let func_ty = self.check_arg_expecting(args[1], &expected_f);
                self.add_func_arg_rows(&func_ty);
                self.unify_at(&expected_f, &func_ty, args[1].span, "tap function");
                v_ty
            }
            "tapError" => {
                if !check_arity(self, 2) {
                    return Some(Type::Unknown);
                }
                // Observe the error channel without consuming it: on failure
                // the effect runs on the failed value and the SAME error
                // re-raises — the row is preserved, so a later `catch` (or
                // the caller) still has to handle it.
                let (ty, rows) = self.with_rows(|s| s.check_expr(args[0]));
                let err_ty = if rows.errors.len() == 1 {
                    let only = rows.errors.iter().next().unwrap().clone();
                    self.tag_type(&only).unwrap_or(Type::Unknown)
                } else {
                    Type::Unknown
                };
                if rows.errors.is_empty() {
                    self.warn(
                        args[0].span,
                        "this expression cannot fail, so `tapError` never runs its effect",
                    );
                }
                self.merge_rows(&rows);
                let expected_f = Type::Func(Rc::new(FuncType {
                    params: vec![err_ty],
                    ret: Type::Unit,
                    errors: BTreeSet::new(),
                    caps: BTreeSet::new(),
                }));
                let func_ty = self.check_arg_expecting(args[1], &expected_f);
                self.add_func_arg_rows(&func_ty);
                self.unify_at(&expected_f, &func_ty, args[1].span, "tapError function");
                ty
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
            "assert" => {
                if check_arity(self, 1) {
                    let t = self.check_expr(args[0]);
                    self.unify_at(&Type::Bool, &t, args[0].span, "assert condition");
                }
                self.add_error_row(ASSERT_FAILED);
                Type::Unit
            }
            "assertEq" => {
                if !check_arity(self, 2) {
                    return Some(Type::Unit);
                }
                let a = self.check_expr(args[0]);
                let b = self.check_expr(args[1]);
                self.unify_at(&a, &b, args[1].span, "assertEq operands");
                self.add_error_row(ASSERT_FAILED);
                Type::Unit
            }
            "env" => {
                if check_arity(self, 1) {
                    let t = self.check_expr(args[0]);
                    self.unify_at(&Type::Str, &t, args[0].span, "env name");
                }
                Type::Option(Box::new(Type::Str))
            }
            "sleep" => {
                if check_arity(self, 1) {
                    let ty = self.check_expr(args[0]);
                    self.unify_at(&Type::Duration, &ty, args[0].span, "sleep duration");
                }
                Type::Unit
            }
            "filter" => {
                if !check_arity(self, 2) {
                    return Some(Type::Unknown);
                }
                let list_ty = self.check_expr(args[0]);
                let a = self.ctx.fresh();
                self.unify_at(&Type::List(Box::new(a.clone())), &list_ty, args[0].span, "filter input");
                let expected_f = Type::Func(Rc::new(FuncType {
                    params: vec![a.clone()],
                    ret: Type::Bool,
                    errors: BTreeSet::new(),
                    caps: BTreeSet::new(),
                }));
                let func_ty = self.check_arg_expecting(args[1], &expected_f);
                self.add_func_arg_rows(&func_ty);
                self.unify_at(&expected_f, &func_ty, args[1].span, "filter predicate");
                Type::List(Box::new(a))
            }
            "fold" => {
                if !check_arity(self, 3) {
                    return Some(Type::Unknown);
                }
                let list_ty = self.check_expr(args[0]);
                let a = self.ctx.fresh();
                self.unify_at(&Type::List(Box::new(a.clone())), &list_ty, args[0].span, "fold input");
                let acc = self.check_expr(args[1]);
                let expected_f = Type::Func(Rc::new(FuncType {
                    params: vec![acc.clone(), a],
                    ret: acc.clone(),
                    errors: BTreeSet::new(),
                    caps: BTreeSet::new(),
                }));
                let func_ty = self.check_arg_expecting(args[2], &expected_f);
                self.add_func_arg_rows(&func_ty);
                self.unify_at(&expected_f, &func_ty, args[2].span, "fold function");
                acc
            }
            "at" => {
                if !check_arity(self, 2) {
                    return Some(Type::Unknown);
                }
                let list_ty = self.check_expr(args[0]);
                let a = self.ctx.fresh();
                self.unify_at(&Type::List(Box::new(a.clone())), &list_ty, args[0].span, "at input");
                let i_ty = self.check_expr(args[1]);
                self.unify_at(&Type::Int, &i_ty, args[1].span, "index");
                Type::Option(Box::new(a))
            }
            "concat" => {
                if !check_arity(self, 2) {
                    return Some(Type::Unknown);
                }
                let a = self.ctx.fresh();
                let want = Type::List(Box::new(a));
                let x = self.check_expr(args[0]);
                self.unify_at(&want, &x, args[0].span, "concat input");
                let y = self.check_expr(args[1]);
                self.unify_at(&want, &y, args[1].span, "concat input");
                want
            }
            "reverse" => {
                if !check_arity(self, 1) {
                    return Some(Type::Unknown);
                }
                let a = self.ctx.fresh();
                let want = Type::List(Box::new(a));
                let x = self.check_expr(args[0]);
                self.unify_at(&want, &x, args[0].span, "reverse input");
                want
            }
            "split" => {
                if !check_arity(self, 2) {
                    return Some(Type::List(Box::new(Type::Str)));
                }
                for arg in args {
                    let t = self.check_expr(arg);
                    self.unify_at(&Type::Str, &t, arg.span, "split argument");
                }
                Type::List(Box::new(Type::Str))
            }
            "readLine" => {
                if !check_arity(self, 0) {
                    return Some(Type::Option(Box::new(Type::Str)));
                }
                Type::Option(Box::new(Type::Str))
            }
            "bitAnd" | "bitOr" | "bitXor" | "shiftL" | "shiftR" => {
                if !check_arity(self, 2) {
                    return Some(Type::Int);
                }
                for arg in args {
                    let t = self.check_expr(arg);
                    self.unify_at(&Type::Int, &t, arg.span, "bitwise argument");
                }
                Type::Int
            }
            "bitNot" => {
                if !check_arity(self, 1) {
                    return Some(Type::Int);
                }
                let t = self.check_expr(args[0]);
                self.unify_at(&Type::Int, &t, args[0].span, "bitwise argument");
                Type::Int
            }
            "byteAt" => {
                if !check_arity(self, 2) {
                    return Some(Type::Option(Box::new(Type::Int)));
                }
                let s = self.check_expr(args[0]);
                self.unify_at(&Type::Str, &s, args[0].span, "byteAt input");
                let i = self.check_expr(args[1]);
                self.unify_at(&Type::Int, &i, args[1].span, "byteAt index");
                Type::Option(Box::new(Type::Int))
            }
            "byteLen" => {
                if !check_arity(self, 1) {
                    return Some(Type::Int);
                }
                let s = self.check_expr(args[0]);
                self.unify_at(&Type::Str, &s, args[0].span, "byteLen input");
                Type::Int
            }
            "intToBytes" => {
                if !check_arity(self, 2) {
                    return Some(Type::Str);
                }
                for arg in args {
                    let t = self.check_expr(arg);
                    self.unify_at(&Type::Int, &t, arg.span, "intToBytes argument");
                }
                Type::Str
            }
            "bytesToInt" => {
                if !check_arity(self, 3) {
                    return Some(Type::Int);
                }
                let s = self.check_expr(args[0]);
                self.unify_at(&Type::Str, &s, args[0].span, "bytesToInt input");
                for arg in &args[1..] {
                    let t = self.check_expr(arg);
                    self.unify_at(&Type::Int, &t, arg.span, "bytesToInt argument");
                }
                Type::Int
            }
            "fromBytes" => {
                if !check_arity(self, 1) {
                    return Some(Type::Str);
                }
                let xs = self.check_expr(args[0]);
                self.unify_at(&Type::List(Box::new(Type::Int)), &xs, args[0].span, "fromBytes input");
                Type::Str
            }
            "contains" | "startsWith" | "endsWith" => {
                if !check_arity(self, 2) {
                    return Some(Type::Bool);
                }
                for arg in args {
                    let t = self.check_expr(arg);
                    self.unify_at(&Type::Str, &t, arg.span, "string argument");
                }
                Type::Bool
            }
            "replace" => {
                if !check_arity(self, 3) {
                    return Some(Type::Str);
                }
                for arg in args {
                    let t = self.check_expr(arg);
                    self.unify_at(&Type::Str, &t, arg.span, "replace argument");
                }
                Type::Str
            }
            "toUpper" | "toLower" => {
                if !check_arity(self, 1) {
                    return Some(Type::Str);
                }
                let t = self.check_expr(args[0]);
                self.unify_at(&Type::Str, &t, args[0].span, "string argument");
                Type::Str
            }
            "join" => {
                if !check_arity(self, 2) {
                    return Some(Type::Str);
                }
                let xs = self.check_expr(args[0]);
                self.unify_at(&Type::List(Box::new(Type::Str)), &xs, args[0].span, "join input");
                let sep = self.check_expr(args[1]);
                self.unify_at(&Type::Str, &sep, args[1].span, "join separator");
                Type::Str
            }
            "sort" => {
                if !check_arity(self, 1) {
                    return Some(Type::Unknown);
                }
                let elem = self.ctx.fresh();
                let want = Type::List(Box::new(elem.clone()));
                let xs = self.check_expr(args[0]);
                self.unify_at(&want, &xs, args[0].span, "sort input");
                match self.ctx.resolve(&elem) {
                    Type::Int | Type::Float | Type::Str | Type::Bool | Type::Duration
                    | Type::Var(_) | Type::Unknown => {}
                    other => {
                        let rendered = self.render(&other);
                        self.error(
                            args[0].span,
                            format!("`sort` orders [Int], [Float], or [String]; found [{rendered}] — use `sortBy` with a key"),
                        );
                    }
                }
                want
            }
            "sortBy" => {
                if !check_arity(self, 2) {
                    return Some(Type::Unknown);
                }
                let a = self.ctx.fresh();
                let want = Type::List(Box::new(a.clone()));
                let xs = self.check_expr(args[0]);
                self.unify_at(&want, &xs, args[0].span, "sortBy input");
                let expected_f = Type::Func(Rc::new(FuncType {
                    params: vec![a],
                    ret: Type::Int,
                    errors: BTreeSet::new(),
                    caps: BTreeSet::new(),
                }));
                let func_ty = self.check_arg_expecting(args[1], &expected_f);
                self.add_func_arg_rows(&func_ty);
                self.unify_at(&expected_f, &func_ty, args[1].span, "sortBy key (returns Int)");
                want
            }
            "min" | "max" => {
                if !check_arity(self, 2) {
                    return Some(Type::Unknown);
                }
                let lhs = self.check_expr(args[0]);
                let rhs = self.check_expr(args[1]);
                self.unify_at(&lhs, &rhs, args[1].span, "min/max arguments");
                match self.ctx.resolve(&lhs) {
                    Type::Int | Type::Float | Type::Duration | Type::Unknown => {}
                    Type::Var(_) => {
                        self.unify_at(&Type::Int, &lhs, args[0].span, "min/max argument");
                    }
                    other => {
                        let rendered = self.render(&other);
                        self.error(
                            args[0].span,
                            format!("`{name}` compares Int, Float, or Duration; found {rendered}"),
                        );
                    }
                }
                self.ctx.resolve(&lhs)
            }
            "abs" => {
                if !check_arity(self, 1) {
                    return Some(Type::Unknown);
                }
                let t = self.check_expr(args[0]);
                match self.ctx.resolve(&t) {
                    Type::Int | Type::Float | Type::Unknown => {}
                    Type::Var(_) => {
                        self.unify_at(&Type::Int, &t, args[0].span, "abs argument");
                    }
                    other => {
                        let rendered = self.render(&other);
                        self.error(args[0].span, format!("`abs` takes an Int or Float; found {rendered}"));
                    }
                }
                self.ctx.resolve(&t)
            }
            "slice" => {
                if !check_arity(self, 3) {
                    return Some(Type::Str);
                }
                let s = self.check_expr(args[0]);
                self.unify_at(&Type::Str, &s, args[0].span, "slice input");
                for arg in &args[1..] {
                    let t = self.check_expr(arg);
                    self.unify_at(&Type::Int, &t, arg.span, "slice bound");
                }
                Type::Str
            }
            "indexOf" => {
                if !check_arity(self, 2) {
                    return Some(Type::Int);
                }
                for arg in args {
                    let t = self.check_expr(arg);
                    self.unify_at(&Type::Str, &t, arg.span, "indexOf argument");
                }
                Type::Int
            }
            "trim" => {
                if check_arity(self, 1) {
                    let t = self.check_expr(args[0]);
                    self.unify_at(&Type::Str, &t, args[0].span, "trim input");
                }
                Type::Str
            }
            "parseInt" => {
                if check_arity(self, 1) {
                    let t = self.check_expr(args[0]);
                    self.unify_at(&Type::Str, &t, args[0].span, "parseInt input");
                }
                Type::Option(Box::new(Type::Int))
            }
            "toFloat" => {
                if check_arity(self, 1) {
                    let t = self.check_expr(args[0]);
                    self.unify_at(&Type::Int, &t, args[0].span, "toFloat input");
                }
                Type::Float
            }
            "floor" => {
                if check_arity(self, 1) {
                    let t = self.check_expr(args[0]);
                    self.unify_at(&Type::Float, &t, args[0].span, "floor input");
                }
                Type::Int
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
            "MutList" => {
                check_arity(self, 0);
                let t = self.ctx.fresh();
                Type::MutList(Box::new(t))
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
            let doc = builtin_doc(name).map(str::to_string).unwrap_or(format!("{name} (builtin)"));
            self.info.hovers.push((callee_span, doc));
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
        // `graphics.rect(...)` / `cards.rankName(c)` arrive as Method calls.
        if let ExprKind::Var(alias) = &recv.kind {
            if !self.scope_has(alias) {
                if let Some(import) = self.import_for_alias(recv.span, alias) {
                    return self.check_module_member_call(&import, name, name_span, args, span);
                }
            }
        }
        let recv_ty = self.check_expr(recv);
        let resolved = {
            let r = self.ctx.resolve(&recv_ty);
            // `get`/`set`/`delete`/`size` are map vocabulary: an otherwise
            // unconstrained receiver (e.g. an untyped parameter in a
            // function nothing calls yet) defaults to MutMap.
            if matches!(r, Type::Var(_)) && matches!(name, "get" | "set" | "delete" | "size") {
                let m = Type::MutMap(Box::new(self.ctx.fresh()), Box::new(self.ctx.fresh()));
                let _ = self.ctx.unify(&recv_ty, &m);
                self.ctx.resolve(&recv_ty)
            } else {
                r
            }
        };
        match resolved {
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
                    self.enforce_func_rows(param_ty, &arg_ty, arg.span);
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
            Type::MutMap(k, v) => {
                let result = match name {
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
                };
                // Doc hover after the args are checked, so the key/value
                // types reflect this very call's constraints.
                if self.record_info {
                    let mut names = Vec::new();
                    let (rk, rv) =
                        (self.ctx.render(&k, &mut names), self.ctx.render(&v, &mut names));
                    let doc = match name {
                        "get" => Some(format!("get({rk} key) -> {rv}?")),
                        "set" => Some(format!("set({rk} key, {rv} value) -> Unit")),
                        "delete" => Some(format!("delete({rk} key) -> Unit")),
                        "size" => Some("size() -> Int".to_string()),
                        _ => None,
                    };
                    if let Some(doc) = doc {
                        self.info.hovers.push((name_span, doc));
                    }
                }
                result
            }
            Type::MutList(t) => {
                let result = match name {
                "push" => {
                    if args.len() == 1 {
                        let val_ty = self.check_expr(args[0]);
                        self.unify_at(&t, &val_ty, args[0].span, "list element");
                    } else {
                        self.error(span, "`push` expects 1 argument (the value)");
                    }
                    Type::Unit
                }
                "pop" => {
                    if !args.is_empty() {
                        self.error(span, "`pop` takes no arguments");
                    }
                    Type::Option(t.clone())
                }
                "get" => {
                    if args.len() == 1 {
                        let idx_ty = self.check_expr(args[0]);
                        self.unify_at(&Type::Int, &idx_ty, args[0].span, "list index");
                    } else {
                        self.error(span, "`get` expects 1 argument (the index)");
                    }
                    Type::Option(t.clone())
                }
                "set" => {
                    if args.len() == 2 {
                        let idx_ty = self.check_expr(args[0]);
                        self.unify_at(&Type::Int, &idx_ty, args[0].span, "list index");
                        let val_ty = self.check_expr(args[1]);
                        self.unify_at(&t, &val_ty, args[1].span, "list element");
                    } else {
                        self.error(span, "`set` expects 2 arguments (index, value)");
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
                            format!("MutList has no method `{name}` (push, pop, get, set, size)"),
                        );
                        Type::Unknown
                    }
                };
                if self.record_info {
                    let mut names = Vec::new();
                    let rt = self.ctx.render(&t, &mut names);
                    let doc = match name {
                        "push" => Some(format!("push({rt} value) -> Unit")),
                        "pop" => Some(format!("pop() -> {rt}?")),
                        "get" => Some(format!("get(Int index) -> {rt}?")),
                        "set" => Some(format!("set(Int index, {rt} value) -> Unit")),
                        "size" => Some("size() -> Int".to_string()),
                        _ => None,
                    };
                    if let Some(doc) = doc {
                        self.info.hovers.push((name_span, doc));
                    }
                }
                result
            }
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
                    if self.record_info {
                        self.info.hovers.push((name_span, format!(".{name} — Int to Duration")));
                    }
                    return Type::Duration;
                }
                _ => {}
            }
        }
        // Size suffixes: `256.kb` is just an Int in bytes.
        if SIZE_SUFFIXES.iter().any(|(s, _)| *s == name) {
            match resolved {
                Type::Int | Type::Var(_) | Type::Unknown => {
                    self.unify_at(&Type::Int, &recv_ty, recv.span, "size value");
                    if self.record_info {
                        if let Some((_, factor)) = SIZE_SUFFIXES.iter().find(|(s, _)| *s == name) {
                            self.info.hovers.push((
                                name_span,
                                format!(".{name} — Int bytes (×{factor})"),
                            ));
                        }
                    }
                    return Type::Int;
                }
                _ => {}
            }
        }

        match resolved {
            Type::Named(type_name) => {
                let fty = self.struct_field_type(&type_name, name, name_span);
                if self.record_info && !matches!(fty, Type::Unknown) {
                    self.typed_hovers.push((name_span, name.to_string(), fty.clone()));
                    if let Some(info) = self.structs.get(&type_name) {
                        self.info.refs.push((name_span, info.name_span));
                    }
                }
                fty
            }
            Type::Var(_) => {
                // Try unique-field inference: if exactly one struct has this
                // field, the receiver must be it.
                let mut owners: Vec<(Type, Type)> = Vec::new();
                for (tname, info) in &self.structs {
                    if let Some((_, fty)) = info.fields.iter().find(|(f, _)| f == name) {
                        owners.push((Type::Named(tname.clone()), fty.clone()));
                    }
                }
                if owners.len() == 1 {
                    let (owner, field_ty) = owners.pop().unwrap();
                    self.unify_at(&owner, &recv_ty, recv.span, "field access");
                    if self.record_info {
                        self.typed_hovers.push((name_span, name.to_string(), field_ty.clone()));
                    }
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

    fn struct_field_type(&self, type_name: &str, field: &str, _name_span: Span) -> Type {
        match self
            .structs
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
                if self.record_info {
                    self.info
                        .catch_rows
                        .push((*catch_span, rows.errors.iter().cloned().collect()));
                }
                if rows.errors.is_empty() {
                    if let Type::Fiber(_, errs) = self.ctx.resolve(&lhs_ty) {
                        if !errs.borrow().is_empty() {
                            self.error(
                                *catch_span,
                                "the fiber's errors surface at `fiber.join`; catch there, or catch inside the forked expression",
                            );
                        }
                    }
                }
                let result_ty = lhs_ty;
                // Variant arms only clear an enum's tag once every variant is
                // covered; partially-caught enums stay in the row.
                let mut covered: HashMap<String, BTreeSet<String>> = HashMap::new();
                for arm in arms {
                    self.scopes.push(HashMap::new());
                    match &arm.pattern.kind {
                        PatternKind::Ctor { name, name_span, args } => {
                            if self.structs.contains_key(name) {
                                if !rows.errors.remove(name) {
                                    self.warn_unreachable_arm(*name_span, name);
                                }
                                let fields = self.structs[name].fields.clone();
                                self.bind_ctor_pattern(name, &fields, args, arm.pattern.span);
                            } else if let Some(owner) = self.variant_owner.get(name).cloned() {
                                let known = covered.contains_key(&owner);
                                if !known && !rows.errors.contains(&owner) {
                                    self.warn_unreachable_arm(*name_span, &owner);
                                }
                                let set = covered.entry(owner.clone()).or_default();
                                set.insert(name.clone());
                                let all = self.enums[&owner].variants.len();
                                if set.len() == all {
                                    rows.errors.remove(&owner);
                                }
                                let fields = self.variant_fields(&owner, name);
                                self.bind_ctor_pattern(name, &fields, args, arm.pattern.span);
                            } else if self.enums.contains_key(name) {
                                if !rows.errors.remove(name) {
                                    self.warn_unreachable_arm(*name_span, name);
                                }
                                self.bind_ctor_pattern(name, &[], args, arm.pattern.span);
                            } else {
                                self.error(
                                    *name_span,
                                    format!("unknown type `{name}` in `catch`"),
                                );
                            }
                        }
                        PatternKind::TypedBind { ty, ty_span, name: bind_name } => {
                            match self.tag_type(ty) {
                                Some(bound) => {
                                    if !rows.errors.remove(ty) {
                                        self.warn_unreachable_arm(*ty_span, ty);
                                    }
                                    if self.record_info {
                                        let rendered = self.render(&bound);
                                        self.info.hovers.push((
                                            arm.pattern.span,
                                            format!("{bind_name} : {rendered}"),
                                        ));
                                    }
                                    self.scopes
                                        .last_mut()
                                        .unwrap()
                                        .insert(bind_name.clone(), bound);
                                }
                                None => {
                                    self.error(
                                        *ty_span,
                                        format!("unknown type `{ty}` in `catch`"),
                                    );
                                }
                            }
                        }
                        // Literal arms match one failed value; they never
                        // clear a tag from the row.
                        PatternKind::Int(_) => {
                            if !rows.errors.contains("Int") {
                                self.warn_unreachable_arm(arm.pattern.span, "Int");
                            }
                        }
                        PatternKind::Str(_) => {
                            if !rows.errors.contains("String") {
                                self.warn_unreachable_arm(arm.pattern.span, "String");
                            }
                        }
                        PatternKind::StrTemplate(pieces) => {
                            if !rows.errors.contains("String") {
                                self.warn_unreachable_arm(arm.pattern.span, "String");
                            }
                            self.check_str_template(pieces, arm.pattern.span);
                        }
                        PatternKind::Bool(_) => {
                            if !rows.errors.contains("Bool") {
                                self.warn_unreachable_arm(arm.pattern.span, "Bool");
                            }
                        }
                        PatternKind::Tuple(_) => {
                            self.error(
                                arm.pattern.span,
                                "tuples cannot be failed, so a tuple pattern never matches in `catch`",
                            );
                        }
                        PatternKind::Bind(bind_name) => {
                            // A single possible failure type makes the binder
                            // concrete (so field access compiles natively);
                            // with several it stays unknown.
                            let bound = if rows.errors.len() == 1 {
                                let only = rows.errors.iter().next().unwrap().clone();
                                self.tag_type(&only).unwrap_or(Type::Unknown)
                            } else {
                                Type::Unknown
                            };
                            rows.errors.clear();
                            self.scopes.last_mut().unwrap().insert(bind_name.clone(), bound);
                        }
                        PatternKind::Wildcard => {
                            rows.errors.clear();
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

    fn warn_unreachable_arm(&mut self, span: Span, tag: &str) {
        // Cancellation can surface at any `fiber.join`/`fiber.poll`; it is
        // deliberately not part of inferred rows (every join would carry it),
        // so an `InterruptedError` arm is always considered reachable.
        if tag == INTERRUPTED {
            return;
        }
        self.warn(
            span,
            format!("this `catch` arm is unreachable: the expression cannot fail with `{tag}`"),
        );
    }

    /// Bind a constructor pattern's names: positional patterns destructure
    /// the fields in order, `{ a, b }` binds the named fields. (To bind the
    /// whole value use a typed-bind pattern: `DbError e`.)
    fn bind_ctor_pattern(
        &mut self,
        display: &str,
        fields: &[(String, Type)],
        args: &CtorPatArgs,
        span: Span,
    ) {
        match args {
            CtorPatArgs::None => {}
            CtorPatArgs::Positional(pats) => {
                if pats.len() != fields.len() {
                    self.error(
                        span,
                        format!(
                            "`{display}` has {} field(s) but the pattern has {}",
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
                                format!("`{display}` has no field `{fname}`"),
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
        self.check_exhaustive(scrutinee, arms, &scrut_ty);
        result
    }

    /// Matches must cover every value: enums by variant, Bool by both
    /// literals, options by Some/None — or include an irrefutable arm.
    fn check_exhaustive(&mut self, scrutinee: &Expr, arms: &[Arm], scrut_ty: &Type) {
        if arms.iter().any(|a| self.pattern_irrefutable(&a.pattern)) {
            return;
        }
        match self.ctx.resolve(scrut_ty) {
            Type::Enum(name) => {
                let variants: Vec<String> = self
                    .enums
                    .get(&name)
                    .map(|e| e.variants.iter().map(|(v, _)| v.clone()).collect())
                    .unwrap_or_default();
                let mut missing = Vec::new();
                for v in &variants {
                    let covered = arms.iter().any(|a| match &a.pattern.kind {
                        PatternKind::Ctor { name: pn, args, .. } if pn == v => match args {
                            CtorPatArgs::None | CtorPatArgs::Fields(_) => true,
                            CtorPatArgs::Positional(ps) => {
                                ps.iter().all(|p| self.pattern_irrefutable(p))
                            }
                        },
                        PatternKind::Ctor { name: pn, .. } => pn == &name,
                        _ => false,
                    });
                    if !covered {
                        missing.push(v.clone());
                    }
                }
                if !missing.is_empty() {
                    self.error(
                        scrutinee.span,
                        format!(
                            "this `match` is not exhaustive: missing {} (or add a catch-all `_ ->` arm)",
                            missing.iter().map(|v| format!("`{v}`")).collect::<Vec<_>>().join(", ")
                        ),
                    );
                }
            }
            Type::Bool => {
                let has = |b: bool| {
                    arms.iter().any(|a| matches!(a.pattern.kind, PatternKind::Bool(x) if x == b))
                };
                if !(has(true) && has(false)) {
                    self.error(
                        scrutinee.span,
                        "this `match` is not exhaustive: cover both `true` and `false` (or add `_ ->`)",
                    );
                }
            }
            Type::Option(_) => {
                let has_none = arms.iter().any(|a| {
                    matches!(&a.pattern.kind, PatternKind::Ctor { name, .. } if name == "None")
                });
                let has_some = arms.iter().any(|a| match &a.pattern.kind {
                    PatternKind::Ctor { name, args, .. } if name == "Some" => match args {
                        CtorPatArgs::None => true,
                        CtorPatArgs::Positional(ps) => {
                            ps.iter().all(|p| self.pattern_irrefutable(p))
                        }
                        _ => false,
                    },
                    _ => false,
                });
                if !(has_none && has_some) {
                    self.error(
                        scrutinee.span,
                        "this `match` is not exhaustive: cover `Some(...)` and `None` (or add `_ ->`)",
                    );
                }
            }
            Type::Outcome(_, cell) => {
                let inner_total = |s: &Self, args: &CtorPatArgs| match args {
                    CtorPatArgs::None => true,
                    CtorPatArgs::Positional(ps) if ps.len() == 1 => {
                        s.pattern_irrefutable(&ps[0])
                    }
                    _ => false,
                };
                let has_ok = arms.iter().any(|a| matches!(&a.pattern.kind,
                    PatternKind::Ctor { name, args, .. } if name == "Ok" && inner_total(self, args)));
                let failed_catchall = arms.iter().any(|a| matches!(&a.pattern.kind,
                    PatternKind::Ctor { name, args, .. } if name == "Failed" && inner_total(self, args)));
                let mut missing = Vec::new();
                if !has_ok {
                    missing.push("`Ok(...)`".to_string());
                }
                if !failed_catchall {
                    let row = cell.borrow().clone();
                    for t in &row {
                        let covered = arms.iter().any(|a| match &a.pattern.kind {
                            PatternKind::Ctor { name, args, .. } if name == "Failed" => {
                                match args {
                                    CtorPatArgs::Positional(ps) if ps.len() == 1 => {
                                        match &ps[0].kind {
                                            PatternKind::Ctor { name: inner, .. } => {
                                                inner == t
                                                    || self
                                                        .variant_owner
                                                        .get(inner)
                                                        .is_some_and(|o| o == t)
                                            }
                                            PatternKind::TypedBind { ty, .. } => ty == t,
                                            _ => false,
                                        }
                                    }
                                    _ => false,
                                }
                            }
                            _ => false,
                        });
                        if !covered {
                            missing.push(format!("`Failed({t} ...)`"));
                        }
                    }
                }
                if !missing.is_empty() {
                    self.error(
                        scrutinee.span,
                        format!(
                            "this `match` over an Outcome is not exhaustive: missing {} (or a `Failed(other)` catch-all)",
                            missing.join(", ")
                        ),
                    );
                }
            }
            Type::Var(_) | Type::Unknown => {}
            _ => {
                self.error(
                    scrutinee.span,
                    "this `match` is not exhaustive: add a catch-all arm (`_ ->` or a binding)",
                );
            }
        }
    }

    fn pattern_irrefutable(&self, pat: &Pattern) -> bool {
        match &pat.kind {
            PatternKind::Wildcard | PatternKind::Bind(_) | PatternKind::TypedBind { .. } => true,
            PatternKind::Tuple(ps) => ps.iter().all(|p| self.pattern_irrefutable(p)),
            PatternKind::Ctor { name, args, .. } if self.structs.contains_key(name) => match args {
                CtorPatArgs::None | CtorPatArgs::Fields(_) => true,
                CtorPatArgs::Positional(ps) => ps.iter().all(|p| self.pattern_irrefutable(p)),
            },
            PatternKind::Ctor { name, args, .. } if self.enums.contains_key(name) => {
                matches!(args, CtorPatArgs::None)
            }
            _ => false,
        }
    }

    /// Validate a string-template pattern and bind its capture holes.
    /// Holes capture Int or String (omitted = String); adjacent holes have
    /// no text boundary to split on and are rejected.
    fn check_str_template(&mut self, pieces: &[StrPatPiece], span: Span) {
        let mut seen: Vec<&str> = Vec::new();
        let mut prev_was_hole = false;
        for piece in pieces {
            match piece {
                StrPatPiece::Text(t) => {
                    if !t.is_empty() {
                        prev_was_hole = false;
                    }
                }
                StrPatPiece::Hole { ty, name, span: hspan } => {
                    if prev_was_hole {
                        self.error(
                            *hspan,
                            "adjacent capture holes are ambiguous; put literal text between them",
                        );
                    }
                    prev_was_hole = true;
                    let bound = match ty.as_deref() {
                        None | Some("String") => Type::Str,
                        Some("Int") => Type::Int,
                        Some(other) => {
                            self.error(
                                *hspan,
                                format!("string captures are `Int` or `String`, not `{other}`"),
                            );
                            Type::Unknown
                        }
                    };
                    if seen.contains(&name.as_str()) {
                        self.error(*hspan, format!("capture `{name}` is bound twice"));
                    }
                    seen.push(name);
                    self.scopes.last_mut().unwrap().insert(name.clone(), bound.clone());
                    if self.record_info {
                        self.typed_hovers.push((*hspan, name.clone(), bound));
                    }
                }
            }
        }
        let _ = span;
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
                    self.typed_hovers.push((pat.span, name.clone(), scrut_ty.clone()));
                }
            }
            PatternKind::Int(_) => {
                self.unify_at(&Type::Int, scrut_ty, pat.span, "pattern");
            }
            PatternKind::Str(_) => {
                self.unify_at(&Type::Str, scrut_ty, pat.span, "pattern");
            }
            PatternKind::StrTemplate(pieces) => {
                self.unify_at(&Type::Str, scrut_ty, pat.span, "pattern");
                self.check_str_template(pieces, pat.span);
            }
            PatternKind::Bool(_) => {
                self.unify_at(&Type::Bool, scrut_ty, pat.span, "pattern");
            }
            PatternKind::Ctor { name, name_span, args } => match name.as_str() {
                "Ok" => {
                    let inner = self.ctx.fresh();
                    let cell = Rc::new(RefCell::new(BTreeSet::new()));
                    let outcome = Type::Outcome(Box::new(inner.clone()), cell);
                    self.unify_at(&outcome, scrut_ty, pat.span, "pattern");
                    match args {
                        CtorPatArgs::Positional(pats) if pats.len() == 1 => {
                            self.check_pattern_against(&pats[0], &inner);
                        }
                        CtorPatArgs::None => {}
                        _ => self.error(pat.span, "`Ok` takes one pattern: `Ok(value)`"),
                    }
                }
                "Failed" => {
                    let inner = self.ctx.fresh();
                    let cell = Rc::new(RefCell::new(BTreeSet::new()));
                    let outcome = Type::Outcome(Box::new(inner), cell.clone());
                    self.unify_at(&outcome, scrut_ty, pat.span, "pattern");
                    match args {
                        CtorPatArgs::Positional(pats) if pats.len() == 1 => {
                            // The inner pattern speaks `catch`'s language; a
                            // catch-all binder takes the row's single type
                            // when there is exactly one.
                            let bound = {
                                let row = cell.borrow();
                                if row.len() == 1 {
                                    self.tag_type(row.iter().next().unwrap())
                                        .unwrap_or(Type::Unknown)
                                } else {
                                    Type::Unknown
                                }
                            };
                            self.check_pattern_against(&pats[0], &bound);
                        }
                        CtorPatArgs::None => {}
                        _ => self.error(
                            pat.span,
                            "`Failed` takes one pattern: `Failed(HttpError e)`, `Failed(TimeoutError)`, or `Failed(other)`",
                        ),
                    }
                }
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
                _ if self.structs.contains_key(name) => {
                    self.unify_at(&Type::Named(name.clone()), scrut_ty, pat.span, "pattern");
                    let fields = self.structs[name].fields.clone();
                    self.bind_ctor_pattern(name, &fields, args, pat.span);
                }
                _ if self.variant_owner.contains_key(name) => {
                    let owner = self.variant_owner[name].clone();
                    self.unify_at(&Type::Enum(owner.clone()), scrut_ty, pat.span, "pattern");
                    let fields = self.variant_fields(&owner, name);
                    self.bind_ctor_pattern(name, &fields, args, pat.span);
                }
                _ if self.enums.contains_key(name) => {
                    self.unify_at(&Type::Enum(name.clone()), scrut_ty, pat.span, "pattern");
                    self.bind_ctor_pattern(name, &[], args, pat.span);
                }
                _ => {
                    self.error(*name_span, format!("unknown constructor `{name}` in pattern"));
                }
            },
            PatternKind::Tuple(pats) => {
                let elems: Vec<Type> = pats.iter().map(|_| self.ctx.fresh()).collect();
                self.unify_at(&Type::Tuple(elems.clone()), scrut_ty, pat.span, "pattern");
                for (p, t) in pats.iter().zip(elems.iter()) {
                    self.check_pattern_against(p, t);
                }
            }
            PatternKind::TypedBind { ty, ty_span, name } => match self.tag_type(ty) {
                Some(bound) => {
                    self.unify_at(&bound, scrut_ty, pat.span, "pattern");
                    if self.record_info {
                        self.typed_hovers.push((pat.span, name.clone(), bound.clone()));
                    }
                    self.scopes.last_mut().unwrap().insert(name.clone(), bound);
                }
                None => {
                    self.error(*ty_span, format!("unknown type `{ty}` in pattern"));
                }
            },
        }
    }

    // ---- provide ------------------------------------------------------------------

    fn check_provide(&mut self, impls: &[ProvideItem], body: &Block) -> Type {
        // Items scope left to right: a later impl's field initializers may
        // use the services provided before it in the same list.
        let mut provided: BTreeSet<String> = BTreeSet::new();
        let mut has_arena = false;
        for item in impls {
            if item.name == "Http" {
                if let Some(args) = &item.args {
                    self.error(item.name_span, "`Http` takes no arguments: `provide Http`");
                    for arg in args {
                        self.check_expr(arg);
                    }
                }
                if self.record_info {
                    self.info.hovers.push((
                        item.name_span,
                        "Http — the built-in HTTP client; satisfies `Http` for this scope"
                            .to_string(),
                    ));
                }
                provided.insert(HTTP_SERVICE.to_string());
                continue;
            }
            if item.name == "Fs" {
                if let Some(args) = &item.args {
                    self.error(item.name_span, "`Fs` takes no arguments: `provide Fs`");
                    for arg in args {
                        self.check_expr(arg);
                    }
                }
                if self.record_info {
                    self.info.hovers.push((
                        item.name_span,
                        "Fs — the built-in file system; satisfies `Fs` for this scope"
                            .to_string(),
                    ));
                }
                provided.insert(FS_SERVICE.to_string());
                continue;
            }
            if item.name == "Net" {
                if let Some(args) = &item.args {
                    self.error(item.name_span, "`Net` takes no arguments: `provide Net`");
                    for arg in args {
                        self.check_expr(arg);
                    }
                }
                if self.record_info {
                    self.info.hovers.push((
                        item.name_span,
                        "Net — built-in raw TCP; satisfies `Net` for this scope".to_string(),
                    ));
                }
                provided.insert(NET_SERVICE.to_string());
                continue;
            }
            if item.name == "Term" {
                if let Some(args) = &item.args {
                    self.error(item.name_span, "`Term` takes no arguments: `provide Term`");
                    for arg in args {
                        self.check_expr(arg);
                    }
                }
                if self.record_info {
                    self.info.hovers.push((
                        item.name_span,
                        "Term — the controlling terminal; satisfies `Term` for this scope"
                            .to_string(),
                    ));
                }
                provided.insert(TERM_SERVICE.to_string());
                continue;
            }
            if item.name == "Runtime" {
                // The fiber runtime: `provide Runtime(n)` (n workers; default
                // = cores). Satisfies the builtin `Fibers` capability.
                match item.args.as_deref() {
                    None | Some([]) => {}
                    Some([arg]) => {
                        let ty = self.check_expr(arg);
                        self.unify_at(&Type::Int, &ty, arg.span, "worker count");
                    }
                    Some(args) => {
                        self.error(
                            item.name_span,
                            "`Runtime` takes at most one Int argument, like `Runtime(4)`",
                        );
                        for arg in args {
                            self.check_expr(arg);
                        }
                    }
                }
                if self.record_info {
                    self.info.hovers.push((
                        item.name_span,
                        "Runtime(Int workers) — the fiber runtime; satisfies `Fibers` for this scope"
                            .to_string(),
                    ));
                }
                provided.insert(FIBERS_SERVICE.to_string());
                continue;
            }
            if item.name == "Arena" {
                has_arena = true;
                match item.args.as_deref() {
                    Some([arg]) => {
                        let ty = self.check_expr(arg);
                        self.unify_at(&Type::Int, &ty, arg.span, "arena size");
                    }
                    _ => {
                        self.error(
                            item.name_span,
                            "`Arena` takes one Int size argument, like `Arena(256.kb)`",
                        );
                        for arg in item.args.as_deref().unwrap_or(&[]) {
                            self.check_expr(arg);
                        }
                    }
                }
                if self.record_info {
                    self.info.hovers.push((
                        item.name_span,
                        "Arena(Int bytes) — allocate this scope in a region, freed when it ends"
                            .to_string(),
                    ));
                }
                continue;
            }
            if let Some(args) = &item.args {
                self.error(
                    item.name_span,
                    format!(
                        "`{}` does not take arguments in `provide` (only `Arena(size)` does)",
                        item.name
                    ),
                );
                for arg in args {
                    self.check_expr(arg);
                }
            }
            match self.impls.get(&item.name) {
                Some(info) => {
                    let def_span = info.name_span;
                    let service = info.service.clone();
                    let (m, p) = (info.module, info.is_pub);
                    self.gate(&item.name.clone(), m, p, item.name_span);
                    if self.record_info {
                        self.info.refs.push((item.name_span, def_span));
                        self.info
                            .hovers
                            .push((item.name_span, format!("{} :: {service}", item.name)));
                    }
                    // Constructing the impl runs its field initializers; they
                    // see only the services provided earlier in this list.
                    let mut field_rows =
                        self.impl_field_rows.get(&item.name).cloned().unwrap_or_default();
                    field_rows.caps.retain(|c| !provided.contains(c));
                    self.merge_rows(&field_rows);
                    provided.insert(service);
                }
                None => {
                    self.error(
                        item.name_span,
                        format!("unknown implementation `{}` (declare it like `{} :: SomeService {{ ... }}`)", item.name, item.name),
                    );
                }
            }
        }
        let (body_ty, mut rows) = self.with_rows(|s| s.check_block(body));
        rows.caps.retain(|c| !provided.contains(c));
        self.merge_rows(&rows);
        // An arena is freed when the scope ends; the scope's value is
        // deep-copied out first. Only plain data can be copied — functions
        // and mutable maps are shared by reference and would dangle.
        if has_arena {
            let mut seen = std::collections::HashSet::new();
            if !self.arena_copyable(&body_ty, &mut seen) {
                let rendered = self.render(&self.ctx.resolve(&body_ty));
                self.error(
                    last_span(body),
                    format!(
                        "the value of an `Arena` scope is copied out when the scope ends, but {rendered} contains a function or mutable map, which cannot be copied (return plain data, or move the consumer inside the scope)"
                    ),
                );
            }
        }
        body_ty
    }

    /// `shared service` is the contract that instances may cross fiber
    /// boundaries; every implementation must then carry only scalar state
    /// (no maps, strings, functions, or nested services), so two fibers can
    /// never race on a refcount or shared mutation. Checked at each impl.
    fn validate_shared_impls(&mut self) {
        let impls: Vec<(String, String, Span)> = self
            .impls
            .iter()
            .map(|(n, i)| (n.clone(), i.service.clone(), i.name_span))
            .collect();
        for (impl_name, service, span) in impls {
            if !self.services.get(&service).map(|s| s.shared).unwrap_or(false) {
                continue;
            }
            let fields = match self.impls.get(&impl_name) {
                Some(i) => i.fields.clone(),
                None => continue,
            };
            for (fname, ty) in &fields {
                let ok = matches!(
                    self.ctx.resolve(ty),
                    Type::Int | Type::Float | Type::Bool | Type::Duration | Type::Unit
                );
                if !ok {
                    let rendered = self.render(&self.ctx.resolve(ty));
                    self.error(
                        span,
                        format!(
                            "`{impl_name}` implements the shared service `{service}`, but field `{fname}` is {rendered} — shared services may carry only scalar state (Int/Float/Bool/Duration)"
                        ),
                    );
                }
            }
        }
    }

    /// Can a value of this type be deep-copied out of an arena region?
    /// Functions and mutable maps are shared by reference, so a copy of a
    /// value containing one would still point into the freed region.
    fn arena_copyable(&self, ty: &Type, seen: &mut std::collections::HashSet<String>) -> bool {
        match self.ctx.resolve(ty) {
            Type::Func(_) | Type::Service(_) | Type::MutMap(..) | Type::MutList(_) => false,
            Type::Option(t) | Type::List(t) => self.arena_copyable(&t, seen),
            Type::Tuple(ts) => ts.iter().all(|t| self.arena_copyable(t, seen)),
            Type::Named(n) => {
                if !seen.insert(n.clone()) {
                    return true; // recursive type: already being checked
                }
                match self.structs.get(&n) {
                    Some(info) => {
                        let fields = info.fields.clone();
                        fields.iter().all(|(_, t)| self.arena_copyable(t, seen))
                    }
                    None => true,
                }
            }
            Type::Enum(n) => {
                if !seen.insert(n.clone()) {
                    return true;
                }
                match self.enums.get(&n) {
                    Some(info) => {
                        let variants = info.variants.clone();
                        variants
                            .iter()
                            .all(|(_, fs)| fs.iter().all(|(_, t)| self.arena_copyable(t, seen)))
                    }
                    None => true,
                }
            }
            // Task payloads live on the task's own heap, never in a region.
            _ => true,
        }
    }

    // ---- binary ---------------------------------------------------------------------

    fn check_binary(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr, span: Span) -> Type {
        let lhs_ty = self.check_expr(lhs);
        let rhs_ty = self.check_expr(rhs);
        self.unify_at(&lhs_ty, &rhs_ty, rhs.span, &format!("`{}` operands", op.symbol()));
        let operand = self.ctx.resolve(&lhs_ty);
        // Type parameters are opaque: only `==`/`!=` (identity) apply.
        if self.is_rigid(&lhs_ty) && !matches!(op, BinOp::Eq | BinOp::Ne) {
            let rendered = self.render(&lhs_ty);
            self.error(
                span,
                format!("`{}` is not defined for the type parameter {rendered}; constrain it with an annotation", op.symbol()),
            );
            return Type::Unknown;
        }
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
                if cap == FIBERS_SERVICE {
                    self.diags.push(Diagnostic::error(
                        name_span,
                        "this program forks fibers; provide the runtime in `main`: `provide Runtime(4)` (workers; default = cores)",
                    ));
                    continue;
                }
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
            if let Decl::Use(u) = decl {
                self.record_use_info(u);
                continue;
            }
            let def = match decl {
                Decl::Use(_) => continue,
                Decl::Struct(d) => DefInfo {
                    name: d.name.clone(),
                    span: d.name_span,
                    kind: DefKind::Struct,
                    detail: self.render_struct_sig(&d.name),
                },
                Decl::Enum(d) => DefInfo {
                    name: d.name.clone(),
                    span: d.name_span,
                    kind: DefKind::Enum,
                    detail: format!(
                        "enum {} = {}",
                        d.name,
                        d.variants.iter().map(|v| v.name.clone()).collect::<Vec<_>>().join(" | ")
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
            // Hover for struct/enum declarations themselves.
            match decl {
                Decl::Struct(d) => {
                    let detail = self.info.defs.last().unwrap().detail.clone();
                    self.info.hovers.push((d.name_span, detail));
                }
                Decl::Enum(d) => {
                    let detail = self.info.defs.last().unwrap().detail.clone();
                    self.info.hovers.push((d.name_span, detail));
                }
                _ => {}
            }
        }
    }

    /// Hover + go-to-definition for `use` lines: the path hovers with the
    /// module's exports (and jumps to the file); selected names hover with
    /// their signatures and jump to their declarations.
    fn record_use_info(&mut self, u: &UseDecl) {
        let joined = u.path.join("/");
        match joined.as_str() {
            "std/graphics" => {
                self.info.hovers.push((
                    u.path_span,
                    "std/graphics — GL-backed 2D graphics: graphics.run/clear/rect/rectLines/circle/text/textWidth/mouseX/mouseY/mousePressed/shaderNew/shaderUse/shaderOff".to_string(),
                ));
                return;
            }
            "std/schedule" => {
                self.info.hovers.push((
                    u.path_span,
                    "std/schedule — retry schedules: schedule.exponential(base), schedule.fixed(interval), schedule.upTo(schedule, times)".to_string(),
                ));
                return;
            }
            "std/fiber" => {
                self.info.hovers.push((
                    u.path_span,
                    "std/fiber — fibers: fiber.fork/join/poll/interrupt/settle/unsettle/par/parMap/race/within/partition; needs `provide Runtime(n)`".to_string(),
                ));
                return;
            }
            "std/http" => {
                self.info.hovers.push((
                    u.path_span,
                    "std/http — HTTP client and server: http.get/post/send/openStream/read/close/serve; needs `provide Http`".to_string(),
                ));
                return;
            }
            "std/json" => {
                self.info.hovers.push((
                    u.path_span,
                    "std/json — JSON: json.encode(value), json.decode(raw, StructName)".to_string(),
                ));
                return;
            }
            "std/fs" => {
                self.info.hovers.push((
                    u.path_span,
                    "std/fs — file system: fs.read/write/append/exists/list/remove/createDir; needs `provide Fs`".to_string(),
                ));
                return;
            }
            "std/process" => {
                self.info.hovers.push((
                    u.path_span,
                    "std/process — the running program: process.args(), process.cwd(), process.exit(code)".to_string(),
                ));
                return;
            }
            "std/net" => {
                self.info.hovers.push((
                    u.path_span,
                    "std/net — raw TCP: net.connect/listen/accept/read/write/close/stop; needs `provide Net`".to_string(),
                ));
                return;
            }
            "std/time" => {
                self.info.hovers.push((
                    u.path_span,
                    "std/time — wall clock: time.now() unix millis, time.utc(millis) -> DateTime, time.iso(millis)".to_string(),
                ));
                return;
            }
            "std/term" => {
                self.info.hovers.push((
                    u.path_span,
                    "std/term — the terminal: term.rawOn/rawOff/readKey/size; needs `provide Term`".to_string(),
                ));
                return;
            }
            _ => {}
        }
        let ref_module = self.module_of(u.path_span);
        let Some(import) = self.modules[ref_module]
            .imports
            .iter()
            .find(|i| i.span == u.path_span)
            .cloned()
        else {
            return;
        };
        let Some(target) = self.modules.iter().position(|m| m.key == import.target) else {
            return;
        };
        // The module's exports, for the path hover.
        let mut exports: Vec<String> = Vec::new();
        for (name, info) in &self.funcs {
            if info.module == target && info.is_pub {
                exports.push(name.clone());
            }
        }
        for (name, info) in &self.structs {
            if info.module == target && info.is_pub {
                exports.push(name.clone());
            }
        }
        for (name, info) in &self.enums {
            if info.module == target && info.is_pub {
                exports.push(name.clone());
            }
        }
        for (name, info) in &self.services {
            if info.module == target && info.is_pub {
                exports.push(name.clone());
            }
        }
        for (name, info) in &self.impls {
            if info.module == target && info.is_pub {
                exports.push(name.clone());
            }
        }
        exports.sort();
        let listing = if exports.is_empty() {
            "no pub declarations".to_string()
        } else {
            format!("pub: {}", exports.join(", "))
        };
        let module = &self.modules[target];
        self.info
            .hovers
            .push((u.path_span, format!("module {} ({}) — {listing}", module.name, module.path.display())));
        // Jump from the path to the top of the file.
        let file_start = Span::new(module.base, module.base);
        self.info.refs.push((u.path_span, file_start));
        // Selected names jump to (and hover as) their declarations.
        for (name, nspan) in u.names.as_deref().unwrap_or(&[]) {
            if let Some(info) = self.funcs.get(name) {
                if info.module == target {
                    self.info.refs.push((*nspan, info.name_span));
                    let sig = self.render_func_signature(name);
                    self.info.hovers.push((*nspan, sig));
                }
                continue;
            }
            let def_span = self
                .structs
                .get(name)
                .map(|i| i.name_span)
                .or_else(|| self.enums.get(name).map(|i| i.name_span))
                .or_else(|| {
                    self.variant_owner
                        .get(name)
                        .and_then(|owner| self.enums.get(owner))
                        .map(|i| i.name_span)
                })
                .or_else(|| self.services.get(name).map(|i| i.name_span))
                .or_else(|| self.impls.get(name).map(|i| i.name_span));
            if let Some(def_span) = def_span {
                self.info.refs.push((*nspan, def_span));
            } else {
                self.warn(*nspan, format!("module `{}` has no pub `{name}`", self.modules[target].name));
            }
        }
    }

    // ---- rendering ----------------------------------------------------------------------

    fn render(&self, ty: &Type) -> String {
        let mut names = Vec::new();
        self.ctx.render(ty, &mut names)
    }

    /// `struct Stats = { Int visits, String label }` — typed, for hovers.
    fn render_struct_sig(&self, name: &str) -> String {
        let Some(info) = self.structs.get(name) else { return name.to_string() };
        let mut names = Vec::new();
        let fields: Vec<String> = info
            .fields
            .iter()
            .map(|(f, t)| format!("{} {f}", self.ctx.render(t, &mut names)))
            .collect();
        format!("struct {name} = {{ {} }}", fields.join(", "))
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

/// Builtin struct shapes: (field, type) pairs — one table powering the
/// checker's registrations and the LSP's `.`-member completion.
pub fn builtin_struct_fields(name: &str) -> &'static [(&'static str, &'static str)] {
    match name {
        "HttpResponse" => &[("status", "Int"), ("body", "String")],
        "HttpRequest" => {
            &[("method", "String"), ("path", "String"), ("query", "String"), ("body", "String")]
        }
        "HttpError" => &[("status", "Int"), ("message", "String")],
        "HttpStream" => &[("handle", "Int"), ("status", "Int")],
        "DecodeError" | "AssertionError" => &[("message", "String")],
        "IoError" => &[("path", "String"), ("message", "String")],
        "File" => &[("handle", "Int")],
        "NetError" => &[("message", "String")],
        "Socket" | "Listener" => &[("handle", "Int")],
        "DateTime" => &[
            ("year", "Int"),
            ("month", "Int"),
            ("day", "Int"),
            ("hour", "Int"),
            ("minute", "Int"),
            ("second", "Int"),
            ("millis", "Int"),
        ],
        _ => &[],
    }
}

/// The std modules' members with their docs — one table powering checker
/// hovers and the LSP's `.`-member completion.
pub fn std_module_members(target: &str) -> &'static [(&'static str, &'static str)] {
    match target {
        "std/schedule" => &[
            ("exponential", "schedule.exponential(base) -> Schedule — delay doubles per attempt"),
            ("fixed", "schedule.fixed(interval) -> Schedule"),
            ("upTo", "schedule.upTo(schedule, times) -> Schedule — cap the retry count"),
        ],
        "std/fiber" => &[
            ("fork", "fiber.fork(lazy action) -> Fiber<a ! E> uses Fibers — start now, return immediately; errors surface at the join"),
            ("join", "fiber.join(fiber | (fibers...) | [fibers]) uses Fibers — park, take the result(s), re-raise the error channel"),
            ("poll", "fiber.poll(fiber) -> a? ! E uses Fibers — non-blocking probe (frame loops)"),
            ("interrupt", "fiber.interrupt(fiber) uses Fibers — request cooperative cancellation"),
            ("settle", "fiber.settle(lazy action) -> Outcome<a ! E> — the error channel as data (row-free)"),
            ("unsettle", "fiber.unsettle(outcome) -> a ! E — put the error back in the channel"),
            ("par", "fiber.par(lazy a, lazy b, ...) -> (a, b, ...) uses Fibers — fork all + join"),
            ("parMap", "fiber.parMap(xs, f) -> [b] ! E uses Fibers — one fiber per element; first failure cancels the batch"),
            ("race", "fiber.race(lazy a, lazy b) -> a uses Fibers — first completion wins, loser interrupted"),
            ("within", "fiber.within(lazy action, deadline) -> a ! E, TimeoutError uses Fibers"),
            ("partition", "fiber.partition(outcomes) -> ([a], [Outcome<a ! E>])"),
        ],
        "std/json" => &[
            ("encode", "json.encode(value) -> String — JSON"),
            ("decode", "json.decode(raw, StructName) -> a ! DecodeError — parse JSON into a struct"),
        ],
        "std/term" => &[
            ("rawOn", "term.rawOn() -> Unit ! IoError uses Term — raw mode: no echo, keys arrive immediately; restored at exit even on a crash"),
            ("rawOff", "term.rawOff() uses Term — restore the terminal"),
            ("readKey", "term.readKey() -> String uses Term — one key, blocking: up/down/left/right, enter, esc, tab, space, backspace, home, end, ctrl+<letter>, a character, or eof"),
            ("size", "term.size() -> (Int, Int) uses Term — (cols, rows); (0, 0) off-terminal"),
        ],
        "std/time" => &[
            ("now", "time.now() -> Int — unix milliseconds (wall clock; nowMillis is monotonic)"),
            ("utc", "time.utc(millis) -> DateTime — { year, month, day, hour, minute, second, millis } in UTC"),
            ("iso", "time.iso(millis) -> String — YYYY-MM-DDTHH:MM:SS.mmmZ"),
        ],
        "std/net" => &[
            ("connect", "net.connect(host, port) -> Socket ! NetError uses Net"),
            ("listen", "net.listen(port) -> Listener ! NetError uses Net — binds 0.0.0.0"),
            ("accept", "net.accept(listener) -> Socket ! NetError uses Net — blocks for a client"),
            ("read", "net.read(socket, maxBytes) -> String? ! NetError uses Net — one read; None at end of stream"),
            ("write", "net.write(socket, bytes) -> Unit ! NetError uses Net — writes all bytes"),
            ("close", "net.close(socket) uses Net"),
            ("stop", "net.stop(listener) uses Net"),
        ],
        "std/process" => &[
            ("args", "process.args() -> [String] — command-line arguments (after the program name)"),
            ("cwd", "process.cwd() -> String — the working directory"),
            ("exit", "process.exit(code) -> Unit — end the process now"),
        ],
        "std/fs" => &[
            ("read", "fs.read(path) -> String ! IoError uses Fs — the whole file; binary passes through"),
            ("write", "fs.write(path, contents) -> Unit ! IoError uses Fs — create or truncate"),
            ("append", "fs.append(path, contents) -> Unit ! IoError uses Fs — create if missing"),
            ("exists", "fs.exists(path) -> Bool uses Fs"),
            ("list", "fs.list(dir) -> [String] ! IoError uses Fs — entry names, sorted"),
            ("remove", "fs.remove(path) -> Unit ! IoError uses Fs — file or directory tree"),
            ("createDir", "fs.createDir(path) -> Unit ! IoError uses Fs — like mkdir -p"),
            ("open", "fs.open(path, mode) -> File ! IoError uses Fs — mode r | w | a | rw; positional I/O via readAt/writeAt"),
            ("readAt", "fs.readAt(file, offset, len) -> String ! IoError uses Fs — up to len bytes; shorter at end of file"),
            ("writeAt", "fs.writeAt(file, offset, bytes) -> Unit ! IoError uses Fs"),
            ("size", "fs.size(file) -> Int ! IoError uses Fs — in bytes"),
            ("sync", "fs.sync(file) -> Unit ! IoError uses Fs — fsync to stable storage"),
            ("close", "fs.close(file) uses Fs"),
        ],
        "std/http" => &[
            ("get", "http.get(url) -> HttpResponse ! HttpError uses Http — a non-2xx status is data, not a failure"),
            ("post", "http.post(url, body) -> HttpResponse ! HttpError uses Http"),
            ("send", "http.send(method, url, body, headers) -> HttpResponse ! HttpError uses Http — headers: [(String, String)]"),
            ("openStream", "http.openStream(url) -> HttpStream ! HttpError uses Http — GET with a streamed body"),
            ("read", "http.read(stream) -> String? ! HttpError uses Http — next chunk; None at end"),
            ("close", "http.close(stream) uses Http"),
            ("serve", "http.serve(port, (HttpRequest) -> HttpResponse) -> Unit ! HttpError uses Http — serve until failure; a handler failure answers 500 and re-raises here"),
        ],
        "std/graphics" => &[
            ("run", "graphics.run(width, height, title, frame) — runtime-owned loop; frame runs once per frame"),
            ("clear", "graphics.clear(r, g, b) — 0–255 channels"),
            ("rect", "graphics.rect(x, y, w, h, r, g, b, a)"),
            ("rectLines", "graphics.rectLines(x, y, w, h, thick, r, g, b, a)"),
            ("circle", "graphics.circle(x, y, radius, r, g, b, a)"),
            ("text", "graphics.text(s, x, y, size, r, g, b)"),
            ("textWidth", "graphics.textWidth(s, size) -> Int"),
            ("mouseX", "graphics.mouseX() -> Int"),
            ("mouseY", "graphics.mouseY() -> Int"),
            ("mousePressed", "graphics.mousePressed() -> Bool"),
            ("imageNew", "graphics.imageNew(pngBytes) -> Int — decode PNG bytes (e.g. an http body) into a texture; -1 on failure"),
            ("image", "graphics.image(handle, x, y, w, h) — draw a loaded image scaled to (w, h)"),
            ("shaderNew", "graphics.shaderNew(fragGlsl) -> Int — compile GLSL ES; uniforms iTime, iRes"),
            ("shaderUse", "graphics.shaderUse(handle)"),
            ("shaderOff", "graphics.shaderOff()"),
        ],
        _ => &[],
    }
}

/// Hover documentation for a builtin: the completion table doubles as the
/// "definition" an editor can show, since builtins have no Inga source.
pub fn builtin_doc(name: &str) -> Option<&'static str> {
    builtin_completions().into_iter().find(|(n, _)| *n == name).map(|(_, doc)| doc)
}

const BUILTIN_NAMES: [&str; 59] = [
    "println",
    "print",
    "show",
    "readLine",
    "bitAnd",
    "bitOr",
    "bitXor",
    "bitNot",
    "shiftL",
    "shiftR",
    "byteAt",
    "byteLen",
    "intToBytes",
    "bytesToInt",
    "fromBytes",
    "map",
    "contains",
    "startsWith",
    "endsWith",
    "replace",
    "toUpper",
    "toLower",
    "join",
    "sort",
    "sortBy",
    "min",
    "max",
    "abs",
    "getOrElse",
    "orFail",
    "retry",
    "ignoreFailure",
    "tap",
    "tapError",
    "then",
    "env",
    "sleep",
    "assert",
    "assertEq",
    "len",
    "filter",
    "fold",
    "at",
    "concat",
    "reverse",
    "split",
    "slice",
    "indexOf",
    "trim",
    "parseInt",
    "toFloat",
    "floor",
    "MutMap",
    "MutList",
    "Some",
    "nowMillis",
    "nowMicros",
    "range",
    "random",
];

/// Names the LSP offers as completions alongside user definitions.
pub fn builtin_completions() -> Vec<(&'static str, &'static str)> {
    vec![
        ("println", "println(values...) -> Unit — print space-separated, with a newline"),
        ("print", "print(values...) -> Unit"),
        ("show", "show(value) -> String — developer-facing rendering (quotes strings)"),
        ("readLine", "readLine() -> String? — one line from stdin, without the newline; None at end of input"),
        ("map", "map(container, f) -> mapped — over a list or an option"),
        ("getOrElse", "getOrElse(option, default) -> a"),
        ("orFail", "orFail(option, error) -> a — unwrap Some, or fail with `error`"),
        ("retry", "retry(lazy action, schedule) -> a — re-run per the Schedule; the error row is kept (a retried action can still fail)"),
        ("schedule.upTo", "schedule.upTo(schedule, times) -> Schedule — cap the retry count"),
        ("ignoreFailure", "ignoreFailure(lazy action) -> Unit — swallow the error channel"),
        ("then", "then(value, f) -> b — transform the value mid-pipe: x |> then((u) -> u.name); rows of f merge like any call"),
        ("tap", "tap(value, f) -> value — run a side effect on the value mid-pipe, pass it along untouched"),
        ("tapError", "tapError(lazy action, f) -> a — run a side effect on a failure, then re-raise it (the row is preserved)"),
        ("env", "env(name) -> String? — read an environment variable"),
        ("sleep", "sleep(duration) -> Unit"),
        ("assert", "assert(condition) -> Unit ! AssertionError — for `inga test`"),
        ("assertEq", "assertEq(actual, expected) -> Unit ! AssertionError — for `inga test`"),
        ("len", "len(stringOrList) -> Int"),
        ("filter", "filter(list, predicate) -> [a]"),
        ("fold", "fold(list, init, f) -> b — f(acc, item) left to right"),
        ("at", "at(list, index) -> a? — None when out of bounds"),
        ("concat", "concat(xs, ys) -> [a]"),
        ("reverse", "reverse(list) -> [a]"),
        ("split", "split(s, separator) -> [String]"),
        ("join", "join(strings, separator) -> String"),
        ("contains", "contains(s, needle) -> Bool"),
        ("startsWith", "startsWith(s, prefix) -> Bool"),
        ("endsWith", "endsWith(s, suffix) -> Bool"),
        ("replace", "replace(s, old, new) -> String — every occurrence"),
        ("toUpper", "toUpper(s) -> String"),
        ("toLower", "toLower(s) -> String"),
        ("sort", "sort(list) -> [a] — ascending; works on [Int], [Float], [String]"),
        ("sortBy", "sortBy(list, key) -> [a] — stable, ascending by key(item) -> Int"),
        ("bitAnd", "bitAnd(a, b) -> Int"),
        ("bitOr", "bitOr(a, b) -> Int"),
        ("bitXor", "bitXor(a, b) -> Int"),
        ("bitNot", "bitNot(a) -> Int"),
        ("shiftL", "shiftL(a, n) -> Int — shift left n bits"),
        ("shiftR", "shiftR(a, n) -> Int — logical shift right (the Int as 64 unsigned bits)"),
        ("byteAt", "byteAt(s, i) -> Int? — the i-th byte (0–255); None out of bounds"),
        ("byteLen", "byteLen(s) -> Int — length in bytes (len counts characters)"),
        ("intToBytes", "intToBytes(n, width) -> String — n as `width` little-endian bytes"),
        ("bytesToInt", "bytesToInt(s, offset, width) -> Int — little-endian; missing bytes read 0"),
        ("fromBytes", "fromBytes(bytes) -> String — [Int] (each 0–255) to a byte string"),
        ("min", "min(a, b) -> a — Int, Float, or Duration"),
        ("max", "max(a, b) -> a — Int, Float, or Duration"),
        ("abs", "abs(n) -> n — Int or Float"),
        ("slice", "slice(s, start, end) -> String — by character index"),
        ("indexOf", "indexOf(s, needle) -> Int — -1 when absent"),
        ("trim", "trim(s) -> String"),
        ("parseInt", "parseInt(s) -> Int?"),
        ("toFloat", "toFloat(n) -> Float"),
        ("floor", "floor(x) -> Int"),
        ("MutMap", "MutMap() -> MutMap<k, v> — built-in mutable map"),
        ("MutList", "MutList() -> MutList<a> — built-in mutable array: push/pop/get/set/size"),
        ("nowMillis", "nowMillis() -> Int — monotonic milliseconds since program start"),
        ("nowMicros", "nowMicros() -> Int — monotonic microseconds since program start"),
        ("range", "range(n) -> [Int] — the list [0, 1, ..., n-1]"),
        ("random", "random(n) -> Int — uniform in 0..n-1"),
        ("graphics.run", "graphics.run(width, height, title, frame) — open a window, call frame each frame"),
        ("graphics.clear", "graphics.clear(r, g, b)"),
        ("graphics.rect", "graphics.rect(x, y, w, h, r, g, b, a)"),
        ("graphics.rectLines", "graphics.rectLines(x, y, w, h, thickness, r, g, b, a)"),
        ("graphics.circle", "graphics.circle(x, y, radius, r, g, b, a)"),
        ("graphics.text", "graphics.text(s, x, y, size, r, g, b)"),
        ("graphics.textWidth", "graphics.textWidth(s, size) -> Int"),
        ("graphics.mouseX", "graphics.mouseX() -> Int"),
        ("graphics.mouseY", "graphics.mouseY() -> Int"),
        ("graphics.mousePressed", "graphics.mousePressed() -> Bool — left click this frame"),
        ("graphics.shaderNew", "graphics.shaderNew(fragmentGlsl) -> Int — compile a fragment shader (uniforms: iTime, iRes)"),
        ("graphics.shaderUse", "graphics.shaderUse(handle) — draw subsequent shapes through the shader"),
        ("graphics.shaderOff", "graphics.shaderOff() — back to the default pipeline"),
        ("Some", "Some(value) -> value?"),
        ("None", "None : a?"),
        ("schedule.exponential", "schedule.exponential(base) -> Schedule"),
        ("schedule.fixed", "schedule.fixed(interval) -> Schedule"),
        ("json.encode", "json.encode(value) -> String — JSON"),
        ("json.decode", "json.decode(raw, StructName) -> a ! DecodeError — parse JSON into a struct"),
    ]
}
