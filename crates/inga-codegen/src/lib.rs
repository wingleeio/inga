//! LLVM backend: lowers a checked Inga program to textual LLVM IR.
//!
//! The lowering implements what docs/SPEC.md §6 promises:
//!
//! - **Native values.** Every Inga value is one `i64`. `Int`/`Bool`/`Duration`
//!   are raw machine integers (no boxing, no tags — types are static).
//!   `Float` is an `f64` stored as bits. Strings, structs, maps, lists,
//!   closures are pointers.
//! - **Errors are return values.** A function whose error row is non-empty
//!   (or that has `lazy` parameters) returns `{ i64 value, i64 err }`; `err`
//!   is null on success, else a pointer to `{ i64 tag_id, i64 value }` — the
//!   failed value boxed with its type tag (`fail` accepts any failable
//!   value). `fail` is an alloc + branch; `catch` compares the tag. Functions
//!   with empty rows pay nothing.
//! - **Structs are field tuples** (`{ fields... }`, no header). **Enums** are
//!   `{ i64 variant_id, fields... }` boxes — or raw variant ids when every
//!   variant of the enum is fieldless.
//! - **Capabilities are evidence.** A function's `uses` row becomes hidden
//!   leading parameters, one instance pointer per service (sorted by name).
//!   `provide` allocates an instance `{ method fn-ptrs..., fields... }`;
//!   `Cache cache` is just a reference to the evidence parameter; method
//!   calls are indirect calls through the instance — the same machine code
//!   as a Rust `dyn` call.
//!
//! Codegen style: every local lives in an `alloca` and every construct
//! stores its result to a slot (LLVM's mem2reg rebuilds SSA at -O2). Failure
//! routing uses a per-function error slot plus a stack of handler labels.
//!
//! Unsupported constructs (`encode`/`decode`, showing structs, …) produce
//! compile errors pointing at the offending span — `inga run` remains the
//! reference semantics for the full language.

use std::collections::HashMap;
use std::fmt::Write as _;

use inga_core::ast::*;
use inga_core::check::{CType, CheckInfo, RowFact};
use inga_core::diag::Diagnostic;
use inga_core::span::Span;

pub fn compile(program: &Program, info: &CheckInfo) -> Result<String, Vec<Diagnostic>> {
    let mut cg = Cg::new(program, info);
    cg.collect_decls();
    cg.gen_all();
    if cg.errors.is_empty() {
        Ok(cg.finish())
    } else {
        Err(cg.errors)
    }
}

const DURATION_SUFFIXES: [(&str, i64); 5] = [
    ("millis", 1),
    ("seconds", 1000),
    ("minutes", 60_000),
    ("hours", 3_600_000),
    ("days", 86_400_000),
];

const SIZE_SUFFIXES: [(&str, i64); 3] =
    [("kb", 1024), ("mb", 1024 * 1024), ("gb", 1024 * 1024 * 1024)];

// ---- program-level state -----------------------------------------------------

struct ServiceMeta {
    /// Method names in declaration order (the vtable layout).
    methods: Vec<String>,
}

struct ImplMeta<'a> {
    decl: &'a ImplDecl,
    /// Field names in declaration order, stored after the method slots.
    fields: Vec<String>,
}

#[derive(Clone)]
struct VariantMeta {
    enum_name: String,
    /// Position within the enum (the runtime variant id).
    id: i64,
    fields: Vec<String>,
    /// True when every variant of the owning enum is fieldless — the whole
    /// enum is then represented as raw variant ids, no box.
    simple: bool,
}

struct Cg<'a> {
    program: &'a Program,
    info: &'a CheckInfo,

    services: HashMap<String, ServiceMeta>,
    impls: HashMap<String, ImplMeta<'a>>,
    funcs: HashMap<String, &'a FuncDecl>,
    /// struct name -> field names
    struct_meta: HashMap<String, Vec<String>>,
    /// variant name -> meta (variant names are globally unique)
    variant_meta: HashMap<String, VariantMeta>,
    /// enum name -> whether it is represented as raw variant ids
    enum_simple: HashMap<String, bool>,
    /// `!` row tag -> id (primitives, structs, and enums)
    tag_ids: HashMap<String, i64>,

    globals: String,
    functions: String,
    str_consts: HashMap<Vec<u8>, String>,
    /// Memoized drop-glue symbols, keyed by a canonical type key.
    drop_syms: HashMap<String, String>,
    tmp: u32,
    label: u32,
    errors: Vec<Diagnostic>,
}

impl<'a> Cg<'a> {
    fn new(program: &'a Program, info: &'a CheckInfo) -> Cg<'a> {
        Cg {
            program,
            info,
            services: HashMap::new(),
            impls: HashMap::new(),
            funcs: HashMap::new(),
            struct_meta: HashMap::new(),
            variant_meta: HashMap::new(),
            enum_simple: HashMap::new(),
            tag_ids: HashMap::new(),
            globals: String::new(),
            functions: String::new(),
            str_consts: HashMap::new(),
            drop_syms: HashMap::new(),
            tmp: 0,
            label: 0,
            errors: Vec::new(),
        }
    }

    fn unsupported(&mut self, span: Span, what: &str) {
        self.errors.push(Diagnostic::error(
            span,
            format!("{what} is not supported by `inga build` yet — run this program with `inga run`"),
        ));
    }

    fn collect_decls(&mut self) {
        // Fixed tag ids for the builtins, then one per struct/enum.
        self.struct_meta.insert("DecodeError".into(), vec!["message".into()]);
        for (i, tag) in
            ["DecodeError", "Int", "Float", "Bool", "String", "Duration"].iter().enumerate()
        {
            self.tag_ids.insert(tag.to_string(), i as i64);
        }
        let mut next_tag_id = self.tag_ids.len() as i64;
        for decl in &self.program.decls {
            match decl {
                Decl::Struct(d) => {
                    self.struct_meta
                        .insert(d.name.clone(), d.fields.iter().map(|f| f.name.clone()).collect());
                    self.tag_ids.insert(d.name.clone(), next_tag_id);
                    next_tag_id += 1;
                }
                Decl::Enum(d) => {
                    let simple = d.variants.iter().all(|v| v.fields.is_empty());
                    self.enum_simple.insert(d.name.clone(), simple);
                    for (i, v) in d.variants.iter().enumerate() {
                        self.variant_meta.insert(
                            v.name.clone(),
                            VariantMeta {
                                enum_name: d.name.clone(),
                                id: i as i64,
                                fields: v.fields.iter().map(|f| f.name.clone()).collect(),
                                simple,
                            },
                        );
                    }
                    self.tag_ids.insert(d.name.clone(), next_tag_id);
                    next_tag_id += 1;
                }
                Decl::Service(d) => {
                    self.services.insert(
                        d.name.clone(),
                        ServiceMeta { methods: d.methods.iter().map(|m| m.name.clone()).collect() },
                    );
                }
                Decl::Impl(d) => {
                    self.impls.insert(
                        d.name.clone(),
                        ImplMeta {
                            decl: d,
                            fields: d.fields.iter().map(|(n, _, _)| n.clone()).collect(),
                        },
                    );
                }
                Decl::Func(d) => {
                    self.funcs.insert(d.name.clone(), d);
                }
                Decl::Use(_) => {}
            }
        }
    }

    // ---- naming helpers --------------------------------------------------

    fn tmp(&mut self) -> String {
        self.tmp += 1;
        format!("%t{}", self.tmp)
    }

    fn label(&mut self, base: &str) -> String {
        self.label += 1;
        format!("{base}.{}", self.label)
    }

    /// A string literal: `{ i64 meta(-1 = static), i64 len, bytes }`. The
    /// returned constant points past the meta word so dup/release see the
    /// static marker and no-op.
    fn str_const(&mut self, text: &str) -> String {
        let bytes = text.as_bytes().to_vec();
        let len = bytes.len();
        let refer = |name: &str| {
            format!(
                "ptrtoint (ptr getelementptr inbounds (<{{ i64, i64, [{len} x i8] }}>, ptr {name}, i32 0, i32 1) to i64)"
            )
        };
        if let Some(name) = self.str_consts.get(&bytes) {
            return refer(name);
        }
        let name = format!("@ing.s{}", self.str_consts.len());
        let mut escaped = String::new();
        for b in &bytes {
            match b {
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b' ' | b'_' | b'.' | b',' | b':'
                | b'(' | b')' | b'[' | b']' | b'<' | b'>' | b'-' | b'+' | b'=' | b'!' | b'/'
                | b'?' | b'\'' | b'*' | b'|' | b'&' | b'%' | b'@' | b'#' | b';' | b'{' | b'}' => {
                    escaped.push(*b as char)
                }
                _ => {
                    let _ = write!(escaped, "\\{b:02X}");
                }
            }
        }
        let _ = writeln!(
            self.globals,
            "{name} = private unnamed_addr constant <{{ i64, i64, [{len} x i8] }}> <{{ i64 -1, i64 {len}, [{len} x i8] c\"{escaped}\" }}>"
        );
        self.str_consts.insert(bytes, name.clone());
        refer(&name)
    }

    fn func_row(&self, name: &str) -> RowFact {
        self.info.facts.funcs.get(name).cloned().unwrap_or_default()
    }

    fn method_row(&self, service: &str, method: &str) -> RowFact {
        self.info
            .facts
            .methods
            .get(&(service.to_string(), method.to_string()))
            .cloned()
            .unwrap_or_default()
    }

    fn func_fallible(&self, decl: &FuncDecl) -> bool {
        !self.func_row(&decl.name).errors.is_empty()
            || decl.sig.params.iter().any(|p| p.lazy)
    }

    fn method_fallible(&self, service: &str, method: &str) -> bool {
        !self.method_row(service, method).errors.is_empty()
    }

    fn ctype_of(&self, expr: &Expr) -> CType {
        self.info
            .expr_types
            .get(&(expr.span.start, expr.span.end))
            .cloned()
            .unwrap_or(CType::Int)
    }

    // ---- program emission --------------------------------------------------

    fn gen_all(&mut self) {
        for decl in &self.program.decls {
            match decl {
                Decl::Func(d) => self.gen_func(d),
                Decl::Impl(d) => self.gen_impl(d),
                _ => {}
            }
        }
        // C entry point. The checker guarantees `main` has empty rows, so it
        // is infallible and takes no evidence.
        self.functions.push_str(
            "define i32 @main() {\nentry:\n  call i64 @ing.fn.main()\n  ret i32 0\n}\n\n",
        );
    }

    fn finish(self) -> String {
        let mut out = String::new();
        out.push_str("; generated by inga build\n\n");
        out.push_str(RT_DECLS);
        out.push('\n');
        out.push_str(&self.globals);
        out.push('\n');
        out.push_str(&self.functions);
        out
    }

    fn gen_func(&mut self, decl: &'a FuncDecl) {
        let row = self.func_row(&decl.name);
        let fallible = self.func_fallible(decl);
        let mut f = FnCtx::new(fallible);
        // Evidence parameters, then user parameters.
        let mut params = Vec::new();
        for cap in &row.caps {
            let reg = format!("%ev.{cap}");
            params.push(format!("i64 {reg}"));
            f.evidence.insert(cap.clone(), reg);
        }
        let param_ctys =
            self.info.facts.func_params.get(&decl.name).cloned().unwrap_or_default();
        for (i, param) in decl.sig.params.iter().enumerate() {
            let reg = format!("%p.{}", param.name);
            params.push(format!("i64 {reg}"));
            let slot = f.alloca(self, &param.name);
            f.line(format!("store i64 {reg}, ptr {slot}"));
            let cty = param_ctys.get(i).cloned().unwrap_or(CType::Int);
            f.scopes
                .last_mut()
                .unwrap()
                .insert(param.name.clone(), LocalVar { slot, lazy: param.lazy, cty });
        }
        let value = self.gen_block(&mut f, &decl.body);
        let ret_cty = self.info.facts.func_ret.get(&decl.name).cloned();
        f.ret(self, &value, ret_cty.as_ref());
        self.emit_fn(&format!("@ing.fn.{}", decl.name), &params, f);
    }

    fn gen_impl(&mut self, decl: &'a ImplDecl) {
        let service = decl.service.clone();
        let Some(service_meta) = self.services.get(&service) else { return };
        let n_methods = service_meta.methods.len();
        let impl_fields = self.impls[&decl.name].fields.clone();

        for method in &decl.methods {
            let row = self.method_row(&service, &method.name);
            let fallible = self.method_fallible(&service, &method.name);
            let mut f = FnCtx::new(fallible);
            let mut params = vec!["i64 %self".to_string()];
            for cap in &row.caps {
                let reg = format!("%ev.{cap}");
                params.push(format!("i64 {reg}"));
                f.evidence.insert(cap.clone(), reg);
            }
            let mkey = (service.clone(), method.name.clone());
            let param_ctys =
                self.info.facts.method_params.get(&mkey).cloned().unwrap_or_default();
            for (i, param) in method.sig.params.iter().enumerate() {
                if param.lazy {
                    self.unsupported(param.span, "`lazy` parameters on service methods");
                }
                let reg = format!("%p.{}", param.name);
                params.push(format!("i64 {reg}"));
                let slot = f.alloca(self, &param.name);
                f.line(format!("store i64 {reg}, ptr {slot}"));
                let cty = param_ctys.get(i).cloned().unwrap_or(CType::Int);
                f.scopes
                    .last_mut()
                    .unwrap()
                    .insert(param.name.clone(), LocalVar { slot, lazy: false, cty });
            }
            let field_ctys =
                self.info.facts.impl_fields.get(&decl.name).cloned().unwrap_or_default();
            // Impl fields load from the instance (after the method slots).
            for (i, field) in impl_fields.iter().enumerate() {
                let slot = f.alloca(self, field);
                let p = self.tmp();
                f.line(format!("{p} = inttoptr i64 %self to ptr"));
                let gep = self.tmp();
                f.line(format!("{gep} = getelementptr i64, ptr {p}, i64 {}", n_methods + i));
                let v = self.tmp();
                f.line(format!("{v} = load i64, ptr {gep}"));
                f.line(format!("store i64 {v}, ptr {slot}"));
                let cty = field_ctys.get(i).cloned().unwrap_or(CType::Int);
                f.scopes
                    .last_mut()
                    .unwrap()
                    .insert(field.clone(), LocalVar { slot, lazy: false, cty });
            }
            let value = self.gen_block(&mut f, &method.body);
            let ret_cty = self.info.facts.method_ret.get(&mkey).cloned();
            f.ret(self, &value, ret_cty.as_ref());
            self.emit_fn(&format!("@ing.m.{}.{}", decl.name, method.name), &params, f);
        }
    }

    fn emit_fn(&mut self, name: &str, params: &[String], f: FnCtx) {
        let ret_ty = if f.fallible { "{ i64, i64 }" } else { "i64" };
        let _ = writeln!(self.functions, "define {ret_ty} {name}({}) {{", params.join(", "));
        self.functions.push_str("entry:\n");
        for a in &f.allocas {
            let _ = writeln!(self.functions, "  {a}");
        }
        // Pool slots are zeroed up front: a branch may skip the store that
        // fills one, and drop glue treats 0 as "nothing to do".
        for (slot, _) in &f.pool {
            let _ = writeln!(self.functions, "  store i64 0, ptr {slot}");
        }
        for line in &f.body {
            if line.ends_with(':') {
                let _ = writeln!(self.functions, "{line}");
            } else {
                let _ = writeln!(self.functions, "  {line}");
            }
        }
        self.functions.push_str("}\n\n");
    }

    // ---- blocks and statements ------------------------------------------------

    fn gen_block(&mut self, f: &mut FnCtx, block: &Block) -> String {
        f.scopes.push(HashMap::new());
        let saved_evidence = f.evidence.clone();
        let mut result = "0".to_string();
        let count = block.stmts.len();
        for (i, stmt) in block.stmts.iter().enumerate() {
            let last = i + 1 == count;
            match stmt {
                Stmt::Expr(expr) => {
                    let v = self.gen_expr(f, expr);
                    result = if last { v } else { "0".to_string() };
                }
                Stmt::Bind { name, value, .. } => {
                    let v = self.gen_expr(f, value);
                    let slot = f.alloca(self, name);
                    f.line(format!("store i64 {v}, ptr {slot}"));
                    let cty = self.ctype_of(value);
                    f.scopes
                        .last_mut()
                        .unwrap()
                        .insert(name.clone(), LocalVar { slot, lazy: false, cty });
                    result = "0".to_string();
                }
                Stmt::Acquire { service, name, name_span, .. } => {
                    let Some(ev) = f.evidence.get(service).cloned() else {
                        self.unsupported(*name_span, "capability binding outside its evidence scope");
                        continue;
                    };
                    let slot = f.alloca(self, name);
                    f.line(format!("store i64 {ev}, ptr {slot}"));
                    let cty = CType::Service(service.clone());
                    f.scopes
                        .last_mut()
                        .unwrap()
                        .insert(name.clone(), LocalVar { slot, lazy: false, cty });
                    result = "0".to_string();
                }
            }
        }
        f.scopes.pop();
        f.evidence = saved_evidence;
        result
    }

    // ---- expressions -------------------------------------------------------------

    fn gen_expr(&mut self, f: &mut FnCtx, expr: &Expr) -> String {
        match &expr.kind {
            ExprKind::Int(n) => n.to_string(),
            ExprKind::Float(x) => {
                // f64 stored as bits in the i64 value.
                (x.to_bits() as i64).to_string()
            }
            ExprKind::Bool(b) => if *b { "1" } else { "0" }.to_string(),
            ExprKind::Str(pieces) => self.gen_interp(f, pieces, expr.span),
            ExprKind::Var(name) => self.gen_var(f, name, expr.span),
            ExprKind::List(items) => {
                let ptr = self.gen_alloc(f, 1 + items.len() as i64);
                self.store_slot(f, &ptr, 0, &items.len().to_string());
                for (i, item) in items.iter().enumerate() {
                    let v = self.gen_expr(f, item);
                    let icty = self.ctype_of(item);
                    self.dup_value(f, &v, &icty);
                    self.store_slot(f, &ptr, 1 + i as i64, &v);
                }
                let out = self.ptr_to_int(f, &ptr);
                let cty = self.ctype_of(expr);
                self.pool_value(f, &out, &cty);
                out
            }
            ExprKind::Call { callee, args } => {
                let arg_refs: Vec<&Expr> = args.iter().collect();
                self.gen_call(f, callee, &arg_refs, expr.span)
            }
            ExprKind::Method { recv, name, name_span, args } => {
                let arg_refs: Vec<&Expr> = args.iter().collect();
                self.gen_method(f, recv, name, *name_span, &arg_refs, expr.span)
            }
            ExprKind::Field { recv, name, name_span } => self.gen_field(f, recv, name, *name_span),
            ExprKind::Binary { op, lhs, rhs } => self.gen_binary(f, *op, lhs, rhs, expr.span),
            ExprKind::Unary { op, expr: inner } => {
                let v = self.gen_expr(f, inner);
                let out = self.tmp();
                match op {
                    UnOp::Neg => {
                        if self.ctype_of(inner) == CType::Float {
                            let (a, r) = (self.tmp(), self.tmp());
                            f.line(format!("{a} = bitcast i64 {v} to double"));
                            f.line(format!("{r} = fneg double {a}"));
                            f.line(format!("{out} = bitcast double {r} to i64"));
                        } else {
                            f.line(format!("{out} = sub i64 0, {v}"));
                        }
                    }
                    UnOp::Not => f.line(format!("{out} = xor i64 {v}, 1")),
                }
                out
            }
            ExprKind::Pipe { lhs, target } => match target {
                PipeTarget::Call { callee, args } => {
                    let mut all: Vec<&Expr> = vec![lhs];
                    if let Some(extra) = args {
                        all.extend(extra.iter());
                    }
                    self.gen_call(f, callee, &all, expr.span)
                }
                PipeTarget::Catch { arms, .. } => self.gen_catch(f, lhs, arms, expr.span),
            },
            ExprKind::Match { scrutinee, arms } => self.gen_match(f, scrutinee, arms, expr.span),
            ExprKind::Fail { error } => {
                let v = self.gen_expr(f, error);
                let cty = self.ctype_of(error);
                self.emit_fail_value(f, &v, &cty, error.span);
                // `fail` produces no value; continue in a dead block.
                let dead = self.label("dead");
                f.start_block(&dead);
                "0".to_string()
            }
            ExprKind::Provide { impls, body, .. } => self.gen_provide(f, impls, body),
            ExprKind::If { cond, then_block, else_branch } => {
                let c = self.gen_expr(f, cond);
                let slot = f.fresh_slot(self);
                let (then_l, else_l, cont) =
                    (self.label("then"), self.label("else"), self.label("endif"));
                let b = self.tmp();
                f.line(format!("{b} = icmp ne i64 {c}, 0"));
                f.line(format!("br i1 {b}, label %{then_l}, label %{else_l}"));
                f.start_block(&then_l);
                let tv = self.gen_block(f, then_block);
                let tv = if else_branch.is_some() { tv } else { "0".to_string() };
                f.line(format!("store i64 {tv}, ptr {slot}"));
                f.line(format!("br label %{cont}"));
                f.start_block(&else_l);
                let ev = match else_branch {
                    Some(e) => self.gen_expr(f, e),
                    None => "0".to_string(),
                };
                f.line(format!("store i64 {ev}, ptr {slot}"));
                f.line(format!("br label %{cont}"));
                f.start_block(&cont);
                let out = self.tmp();
                f.line(format!("{out} = load i64, ptr {slot}"));
                out
            }
            ExprKind::Block(block) => self.gen_block(f, block),
            ExprKind::Lambda { params, body } => self.gen_closure(f, params, body),
        }
    }

    fn gen_var(&mut self, f: &mut FnCtx, name: &str, span: Span) -> String {
        if let Some(local) = f.lookup(name) {
            let v = self.tmp();
            f.line(format!("{v} = load i64, ptr {}", local.slot));
            if local.lazy {
                return self.force_thunk(f, &v, &local.cty);
            }
            return v;
        }
        match name {
            "None" => return "0".to_string(),
            "true" => return "1".to_string(),
            "false" => return "0".to_string(),
            _ => {}
        }
        if let Some(vmeta) = self.variant_meta.get(name).cloned() {
            if vmeta.fields.is_empty() {
                if vmeta.simple {
                    return vmeta.id.to_string();
                }
                let ptr = self.gen_alloc(f, 1);
                self.store_slot(f, &ptr, 0, &vmeta.id.to_string());
                let out = self.ptr_to_int(f, &ptr);
                self.pool_value(f, &out, &CType::Enum(vmeta.enum_name.clone()));
                return out;
            }
            self.unsupported(span, "using a constructor as a value");
            return "0".to_string();
        }
        if self.funcs.contains_key(name) || name == "Some" || self.struct_meta.contains_key(name) {
            self.unsupported(span, "using a function or constructor as a value");
            return "0".to_string();
        }
        self.unsupported(span, &format!("the name `{name}` here"));
        "0".to_string()
    }

    /// The `!` row tag for a static type, if values of it can be failed.
    fn tag_of_ctype(cty: &CType) -> Option<&str> {
        match cty {
            CType::Struct(n) | CType::Enum(n) => Some(n),
            CType::Int => Some("Int"),
            CType::Float => Some("Float"),
            CType::Bool => Some("Bool"),
            CType::Str => Some("String"),
            CType::Duration => Some("Duration"),
            _ => None,
        }
    }

    /// The value type a `!`-row tag names (for typed-bind payloads).
    fn ctype_of_tag(&self, tag: &str) -> CType {
        match tag {
            "Int" => CType::Int,
            "Float" => CType::Float,
            "Bool" => CType::Bool,
            "String" => CType::Str,
            "Duration" => CType::Duration,
            _ if self.struct_meta.contains_key(tag) => CType::Struct(tag.to_string()),
            _ if self.enum_simple.contains_key(tag) => CType::Enum(tag.to_string()),
            _ => CType::Int,
        }
    }

    /// Box a failed value with its type tag and route it to the handler.
    fn emit_fail_value(&mut self, f: &mut FnCtx, v: &str, cty: &CType, span: Span) {
        let id = match Self::tag_of_ctype(cty).and_then(|t| self.tag_ids.get(t)) {
            Some(id) => *id,
            None => {
                self.unsupported(span, "failing with this value type");
                return;
            }
        };
        // The box outlives any arena scope and is deliberately never freed
        // (it may be re-raised across functions whose pools have drained).
        self.dup_value(f, v, &cty.clone());
        let ptr = self.tmp();
        f.line(format!("{ptr} = call ptr @rt_alloc_global(i64 16)"));
        self.store_slot(f, &ptr, 0, &id.to_string());
        self.store_slot(f, &ptr, 1, v);
        let err = self.ptr_to_int(f, &ptr);
        self.emit_failure(f, &err);
    }

    /// Force a lazy value: thunk = { fnptr, captures... }, fnptr(env) -> {v, err}.
    fn force_thunk(&mut self, f: &mut FnCtx, thunk: &str, cty: &CType) -> String {
        let p = self.tmp();
        f.line(format!("{p} = inttoptr i64 {thunk} to ptr"));
        let fp_i = self.tmp();
        f.line(format!("{fp_i} = load i64, ptr {p}"));
        let fp = self.tmp();
        f.line(format!("{fp} = inttoptr i64 {fp_i} to ptr"));
        let r = self.tmp();
        f.line(format!("{r} = call {{ i64, i64 }} {fp}(ptr {p})"));
        let out = self.check_failure(f, &r);
        self.pool_value(f, &out, &cty.clone());
        out
    }

    /// Extract {value, err} and branch to the failure path when err != 0.
    fn check_failure(&mut self, f: &mut FnCtx, agg: &str) -> String {
        let v = self.tmp();
        f.line(format!("{v} = extractvalue {{ i64, i64 }} {agg}, 0"));
        let e = self.tmp();
        f.line(format!("{e} = extractvalue {{ i64, i64 }} {agg}, 1"));
        let isok = self.tmp();
        let (fail_l, ok_l) = (self.label("onfail"), self.label("ok"));
        f.line(format!("{isok} = icmp eq i64 {e}, 0"));
        f.line(format!("br i1 {isok}, label %{ok_l}, label %{fail_l}"));
        f.start_block(&fail_l);
        self.emit_failure(f, &e);
        f.start_block(&ok_l);
        v
    }

    /// Route an error value to the innermost handler, or out of the function.
    fn emit_failure(&mut self, f: &mut FnCtx, err: &str) {
        f.line(format!("store i64 {err}, ptr %err.slot"));
        if let Some(handler) = f.handlers.last() {
            let target = handler.clone();
            f.line(format!("br label %{target}"));
        } else if f.fallible {
            f.needs_propagate = true;
            f.line("br label %propagate".to_string());
        } else {
            // Statically unreachable (the checker proved the row empty), but
            // the block still needs a terminator.
            f.needs_panic = true;
            f.line("br label %panic.unhandled".to_string());
        }
    }

    // ---- calls ----------------------------------------------------------------------

    fn gen_call(&mut self, f: &mut FnCtx, callee: &Expr, args: &[&Expr], span: Span) -> String {
        // Builtin modules: Schedule.*, Gfx.*
        if let ExprKind::Field { recv, name, .. } | ExprKind::Method { recv, name, .. } =
            &callee.kind
        {
            if let ExprKind::Var(module) = &recv.kind {
                if f.lookup(module).is_none() {
                    if module == "schedule" {
                        return self.gen_schedule(f, name, args, span);
                    }
                    if module == "graphics" {
                        return self.gen_gfx(f, name, args, span);
                    }
                    if let Some(v) = self.gen_qualified(f, module, name, args, span) {
                        return v;
                    }
                }
            }
        }
        if let ExprKind::Var(name) = &callee.kind {
            if f.lookup(name).is_none() {
                if let Some(v) = self.gen_builtin(f, name, args, span) {
                    return v;
                }
                if let Some(fields) = self.struct_meta.get(name).cloned() {
                    let out = self.gen_construct(f, args, None, fields.len());
                    self.pool_value(f, &out.clone(), &CType::Struct(name.clone()));
                    return out;
                }
                if let Some(vmeta) = self.variant_meta.get(name).cloned() {
                    if vmeta.simple {
                        return vmeta.id.to_string();
                    }
                    let out = self.gen_construct(f, args, Some(vmeta.id), vmeta.fields.len());
                    self.pool_value(f, &out.clone(), &CType::Enum(vmeta.enum_name.clone()));
                    return out;
                }
                if let Some(decl) = self.funcs.get(name.as_str()).copied() {
                    return self.gen_user_call(f, decl, args, span);
                }
            }
        }
        // Calling a function value (closure).
        let callee_v = self.gen_expr(f, callee);
        let arg_vals: Vec<String> = args.iter().map(|a| self.gen_expr(f, a)).collect();
        let result_cty = self.ctype_of_span(span);
        self.gen_closure_call(f, &callee_v, &arg_vals, &result_cty)
    }

    /// `alias.member(args)` — module-qualified call; the checker verified
    /// everything, and top-level names are program-unique.
    fn gen_qualified(
        &mut self,
        f: &mut FnCtx,
        module: &str,
        member: &str,
        args: &[&Expr],
        span: Span,
    ) -> Option<String> {
        if self.funcs.contains_key(module)
            || self.struct_meta.contains_key(module)
            || self.variant_meta.contains_key(module)
        {
            return None;
        }
        if let Some(decl) = self.funcs.get(member).copied() {
            return Some(self.gen_user_call(f, decl, args, span));
        }
        if let Some(fields) = self.struct_meta.get(member).cloned() {
            let out = self.gen_construct(f, args, None, fields.len());
            self.pool_value(f, &out.clone(), &CType::Struct(member.to_string()));
            return Some(out);
        }
        if let Some(vmeta) = self.variant_meta.get(member).cloned() {
            if vmeta.simple {
                return Some(vmeta.id.to_string());
            }
            let out = self.gen_construct(f, args, Some(vmeta.id), vmeta.fields.len());
            self.pool_value(f, &out.clone(), &CType::Enum(vmeta.enum_name.clone()));
            return Some(out);
        }
        None
    }

    fn gen_user_call(
        &mut self,
        f: &mut FnCtx,
        decl: &'a FuncDecl,
        args: &[&Expr],
        span: Span,
    ) -> String {
        let row = self.func_row(&decl.name);
        let fallible = self.func_fallible(decl);
        let mut call_args = Vec::new();
        for cap in &row.caps {
            match f.evidence.get(cap) {
                Some(ev) => call_args.push(format!("i64 {ev}")),
                None => {
                    self.unsupported(span, &format!("calling `{}` without `{cap}` provided", decl.name));
                    call_args.push("i64 0".to_string());
                }
            }
        }
        for (i, arg) in args.iter().enumerate() {
            let lazy = decl.sig.params.get(i).is_some_and(|p| p.lazy);
            let v = if lazy { self.gen_thunk(f, arg) } else { self.gen_expr(f, arg) };
            call_args.push(format!("i64 {v}"));
        }
        let name = format!("@ing.fn.{}", decl.name);
        let out = if fallible {
            let r = self.tmp();
            f.line(format!("{r} = call {{ i64, i64 }} {name}({})", call_args.join(", ")));
            self.check_failure(f, &r)
        } else {
            let r = self.tmp();
            f.line(format!("{r} = call i64 {name}({})", call_args.join(", ")));
            r
        };
        // The callee dup'ed the result; this function owns one reference.
        let ret_cty = self.info.facts.func_ret.get(&decl.name).cloned().unwrap_or(CType::Int);
        self.pool_value(f, &out, &ret_cty);
        let _ = span;
        out
    }

    /// Structs are plain field tuples; boxed enum variants carry their
    /// variant id in slot 0.
    fn gen_construct(
        &mut self,
        f: &mut FnCtx,
        args: &[&Expr],
        variant_id: Option<i64>,
        n_fields: usize,
    ) -> String {
        let header = variant_id.map(|_| 1).unwrap_or(0);
        let ptr = self.gen_alloc(f, (header + n_fields.max(args.len())) as i64);
        if let Some(id) = variant_id {
            self.store_slot(f, &ptr, 0, &id.to_string());
        }
        for (i, arg) in args.iter().enumerate() {
            let v = self.gen_expr(f, arg);
            let acty = self.ctype_of(arg);
            self.dup_value(f, &v, &acty);
            self.store_slot(f, &ptr, (header + i) as i64, &v);
        }
        self.ptr_to_int(f, &ptr)
    }

    /// Service method call: indirect call through the instance.
    fn gen_method(
        &mut self,
        f: &mut FnCtx,
        recv: &Expr,
        name: &str,
        name_span: Span,
        args: &[&Expr],
        span: Span,
    ) -> String {
        if let ExprKind::Var(module) = &recv.kind {
            if f.lookup(module).is_none() {
                if module == "schedule" {
                    return self.gen_schedule(f, name, args, span);
                }
                if module == "graphics" {
                    return self.gen_gfx(f, name, args, span);
                }
                if let Some(v) = self.gen_qualified(f, module, name, args, span) {
                    return v;
                }
            }
        }
        let recv_ty = self.ctype_of(recv);
        match recv_ty {
            CType::Service(service) => {
                let Some(meta) = self.services.get(&service) else {
                    self.unsupported(name_span, "this method call");
                    return "0".to_string();
                };
                let Some(idx) = meta.methods.iter().position(|m| m == name) else {
                    self.unsupported(name_span, "this method call");
                    return "0".to_string();
                };
                let row = self.method_row(&service, name);
                let fallible = self.method_fallible(&service, name);
                let inst = self.gen_expr(f, recv);
                let mut call_args = vec![format!("i64 {inst}")];
                for cap in &row.caps {
                    match f.evidence.get(cap) {
                        Some(ev) => call_args.push(format!("i64 {ev}")),
                        None => {
                            self.unsupported(span, &format!("calling `{name}` without `{cap}` provided"));
                            call_args.push("i64 0".to_string());
                        }
                    }
                }
                for arg in args {
                    let v = self.gen_expr(f, arg);
                    call_args.push(format!("i64 {v}"));
                }
                let p = self.tmp();
                f.line(format!("{p} = inttoptr i64 {inst} to ptr"));
                let gep = self.tmp();
                f.line(format!("{gep} = getelementptr i64, ptr {p}, i64 {idx}"));
                let fp_i = self.tmp();
                f.line(format!("{fp_i} = load i64, ptr {gep}"));
                let fp = self.tmp();
                f.line(format!("{fp} = inttoptr i64 {fp_i} to ptr"));
                let out = if fallible {
                    let r = self.tmp();
                    f.line(format!("{r} = call {{ i64, i64 }} {fp}({})", call_args.join(", ")));
                    self.check_failure(f, &r)
                } else {
                    let r = self.tmp();
                    f.line(format!("{r} = call i64 {fp}({})", call_args.join(", ")));
                    r
                };
                let ret_cty = self
                    .info
                    .facts
                    .method_ret
                    .get(&(service.clone(), name.to_string()))
                    .cloned()
                    .unwrap_or(CType::Int);
                self.pool_value(f, &out, &ret_cty);
                out
            }
            CType::MutMap(k, vc) => {
                let m = self.gen_expr(f, recv);
                let kind = match *k {
                    CType::Str => "str",
                    _ => "int",
                };
                match name {
                    "get" if args.len() == 1 => {
                        let key = self.gen_expr(f, args[0]);
                        let r = self.tmp();
                        f.line(format!("{r} = call i64 @rt_map_get_{kind}(i64 {m}, i64 {key})"));
                        // The fresh Some-box is pooled; its drop glue would
                        // steal the map's reference to the inner value, so
                        // take one for the box.
                        let vcty = (*vc).clone();
                        if self.is_rc(&vcty) {
                            let (dup_l, done_l) = (self.label("mg.dup"), self.label("mg.done"));
                            let c = self.tmp();
                            f.line(format!("{c} = icmp ne i64 {r}, 0"));
                            f.line(format!("br i1 {c}, label %{dup_l}, label %{done_l}"));
                            f.start_block(&dup_l);
                            let inner = self.load_slot_from_int(f, &r, 0);
                            let t = self.tmp();
                            f.line(format!("{t} = call i64 @rt_dup(i64 {inner})"));
                            f.line(format!("br label %{done_l}"));
                            f.start_block(&done_l);
                        }
                        self.pool_value(f, &r, &CType::Option(Box::new(vcty)));
                        r
                    }
                    "set" if args.len() == 2 => {
                        let key = self.gen_expr(f, args[0]);
                        let v = self.gen_expr(f, args[1]);
                        let vcty = self.ctype_of(args[1]);
                        self.dup_value(f, &v, &vcty);
                        f.line(format!("call void @rt_map_set_{kind}(i64 {m}, i64 {key}, i64 {v})"));
                        "0".to_string()
                    }
                    "delete" if args.len() == 1 => {
                        let key = self.gen_expr(f, args[0]);
                        f.line(format!("call void @rt_map_del_{kind}(i64 {m}, i64 {key})"));
                        "0".to_string()
                    }
                    "size" => {
                        let r = self.tmp();
                        f.line(format!("{r} = call i64 @rt_map_size(i64 {m})"));
                        r
                    }
                    _ => {
                        self.unsupported(name_span, "this MutMap method");
                        "0".to_string()
                    }
                }
            }
            _ => {
                self.unsupported(name_span, "this method call");
                "0".to_string()
            }
        }
    }

    fn gen_field(&mut self, f: &mut FnCtx, recv: &Expr, name: &str, name_span: Span) -> String {
        let recv_ty = self.ctype_of(recv);
        // Duration and size suffixes on Ints.
        if let Some((_, factor)) = DURATION_SUFFIXES
            .iter()
            .chain(SIZE_SUFFIXES.iter())
            .find(|(s, _)| *s == name)
        {
            if recv_ty == CType::Int {
                let v = self.gen_expr(f, recv);
                let out = self.tmp();
                f.line(format!("{out} = mul i64 {v}, {factor}"));
                return out;
            }
        }
        let (header, fields) = match &recv_ty {
            CType::Struct(n) => (0i64, self.struct_meta.get(n).cloned()),
            _ => (0, None),
        };
        let Some(fields) = fields else {
            self.unsupported(name_span, "this field access");
            return "0".to_string();
        };
        let Some(idx) = fields.iter().position(|fname| fname == name) else {
            self.unsupported(name_span, "this field access");
            return "0".to_string();
        };
        let v = self.gen_expr(f, recv);
        let p = self.tmp();
        f.line(format!("{p} = inttoptr i64 {v} to ptr"));
        let gep = self.tmp();
        f.line(format!("{gep} = getelementptr i64, ptr {p}, i64 {}", header + idx as i64));
        let out = self.tmp();
        f.line(format!("{out} = load i64, ptr {gep}"));
        out
    }

    // ---- operators ---------------------------------------------------------------------

    fn gen_binary(&mut self, f: &mut FnCtx, op: BinOp, lhs: &Expr, rhs: &Expr, span: Span) -> String {
        // Short-circuit logic.
        if matches!(op, BinOp::And | BinOp::Or) {
            let slot = f.fresh_slot(self);
            let l = self.gen_expr(f, lhs);
            f.line(format!("store i64 {l}, ptr {slot}"));
            let (rhs_l, cont) = (self.label("sc.rhs"), self.label("sc.end"));
            let c = self.tmp();
            f.line(format!("{c} = icmp ne i64 {l}, 0"));
            match op {
                BinOp::And => f.line(format!("br i1 {c}, label %{rhs_l}, label %{cont}")),
                _ => f.line(format!("br i1 {c}, label %{cont}, label %{rhs_l}")),
            }
            f.start_block(&rhs_l);
            let r = self.gen_expr(f, rhs);
            f.line(format!("store i64 {r}, ptr {slot}"));
            f.line(format!("br label %{cont}"));
            f.start_block(&cont);
            let out = self.tmp();
            f.line(format!("{out} = load i64, ptr {slot}"));
            return out;
        }

        let lty = self.ctype_of(lhs);
        let l = self.gen_expr(f, lhs);
        let r = self.gen_expr(f, rhs);

        if lty == CType::Float {
            return self.gen_float_binary(f, op, &l, &r, span);
        }
        if lty == CType::Str {
            return match op {
                BinOp::Add => {
                    let out = self.tmp();
                    f.line(format!("{out} = call i64 @rt_str_concat(i64 {l}, i64 {r})"));
                    self.pool_value(f, &out, &CType::Str);
                    out
                }
                BinOp::Eq | BinOp::Ne => {
                    let eq = self.tmp();
                    f.line(format!("{eq} = call i64 @rt_str_eq(i64 {l}, i64 {r})"));
                    if op == BinOp::Eq {
                        eq
                    } else {
                        let out = self.tmp();
                        f.line(format!("{out} = xor i64 {eq}, 1"));
                        out
                    }
                }
                BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                    let c = self.tmp();
                    f.line(format!("{c} = call i64 @rt_str_cmp(i64 {l}, i64 {r})"));
                    let pred = match op {
                        BinOp::Lt => "slt",
                        BinOp::Le => "sle",
                        BinOp::Gt => "sgt",
                        _ => "sge",
                    };
                    let b = self.tmp();
                    f.line(format!("{b} = icmp {pred} i64 {c}, 0"));
                    let out = self.tmp();
                    f.line(format!("{out} = zext i1 {b} to i64"));
                    out
                }
                _ => {
                    self.unsupported(span, "this string operator");
                    "0".to_string()
                }
            };
        }

        // Integer-like (Int, Bool, Duration, pointers for ==).
        match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul => {
                let instr = match op {
                    BinOp::Add => "add",
                    BinOp::Sub => "sub",
                    _ => "mul",
                };
                let out = self.tmp();
                f.line(format!("{out} = {instr} i64 {l}, {r}"));
                out
            }
            BinOp::Div | BinOp::Mod => {
                // Match the interpreter: division by zero is a runtime error.
                let z = self.tmp();
                f.line(format!("{z} = icmp eq i64 {r}, 0"));
                let (bad, good) = (self.label("divzero"), self.label("divok"));
                f.line(format!("br i1 {z}, label %{bad}, label %{good}"));
                f.start_block(&bad);
                let msg = self.str_const("division by zero");
                f.line(format!("call void @rt_panic(i64 {msg})"));
                f.line("unreachable".to_string());
                f.start_block(&good);
                let instr = if op == BinOp::Div { "sdiv" } else { "srem" };
                let out = self.tmp();
                f.line(format!("{out} = {instr} i64 {l}, {r}"));
                out
            }
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let pred = match op {
                    BinOp::Eq => "eq",
                    BinOp::Ne => "ne",
                    BinOp::Lt => "slt",
                    BinOp::Le => "sle",
                    BinOp::Gt => "sgt",
                    _ => "sge",
                };
                let b = self.tmp();
                f.line(format!("{b} = icmp {pred} i64 {l}, {r}"));
                let out = self.tmp();
                f.line(format!("{out} = zext i1 {b} to i64"));
                out
            }
            BinOp::And | BinOp::Or => unreachable!(),
        }
    }

    fn gen_float_binary(&mut self, f: &mut FnCtx, op: BinOp, l: &str, r: &str, span: Span) -> String {
        let (a, b) = (self.tmp(), self.tmp());
        f.line(format!("{a} = bitcast i64 {l} to double"));
        f.line(format!("{b} = bitcast i64 {r} to double"));
        match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => {
                let instr = match op {
                    BinOp::Add => "fadd",
                    BinOp::Sub => "fsub",
                    BinOp::Mul => "fmul",
                    _ => "fdiv",
                };
                let v = self.tmp();
                f.line(format!("{v} = {instr} double {a}, {b}"));
                let out = self.tmp();
                f.line(format!("{out} = bitcast double {v} to i64"));
                out
            }
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let pred = match op {
                    BinOp::Eq => "oeq",
                    BinOp::Ne => "une",
                    BinOp::Lt => "olt",
                    BinOp::Le => "ole",
                    BinOp::Gt => "ogt",
                    _ => "oge",
                };
                let c = self.tmp();
                f.line(format!("{c} = fcmp {pred} double {a}, {b}"));
                let out = self.tmp();
                f.line(format!("{out} = zext i1 {c} to i64"));
                out
            }
            _ => {
                self.unsupported(span, "this float operator");
                "0".to_string()
            }
        }
    }

    // ---- strings -------------------------------------------------------------------------

    /// Interpolation: fold constant text, convert pieces by static type,
    /// concatenate left-to-right.
    fn gen_interp(&mut self, f: &mut FnCtx, pieces: &[StrPiece], span: Span) -> String {
        let mut acc: Option<String> = None;
        for piece in pieces {
            let v = match piece {
                StrPiece::Text(text) => self.str_const(text),
                StrPiece::Expr(e) => {
                    let cty = self.ctype_of(e);
                    let v = self.gen_expr(f, e);
                    self.to_display_str(f, &v, &cty, span)
                }
            };
            acc = Some(match acc {
                None => v,
                Some(prev) => {
                    let out = self.tmp();
                    f.line(format!("{out} = call i64 @rt_str_concat(i64 {prev}, i64 {v})"));
                    self.pool_value(f, &out, &CType::Str);
                    out
                }
            });
        }
        acc.unwrap_or_else(|| self.str_const(""))
    }

    fn to_display_str(&mut self, f: &mut FnCtx, v: &str, cty: &CType, span: Span) -> String {
        let call = match cty {
            CType::Str => return v.to_string(),
            CType::Int => "rt_int_to_str",
            CType::Bool => "rt_bool_to_str",
            CType::Duration => "rt_duration_to_str",
            CType::Float => "rt_float_to_str",
            CType::List(inner) if **inner == CType::Int => "rt_show_list_int",
            CType::List(inner) if **inner == CType::Str => "rt_show_list_str",
            _ => {
                self.unsupported(span, "showing this value type");
                return self.str_const("?");
            }
        };
        let out = self.tmp();
        f.line(format!("{out} = call i64 @{call}(i64 {v})"));
        self.pool_value(f, &out, &CType::Str);
        out
    }

    // ---- catch / match ----------------------------------------------------------------------

    fn gen_catch(&mut self, f: &mut FnCtx, lhs: &Expr, arms: &[Arm], span: Span) -> String {
        let slot = f.fresh_slot(self);
        let handler = self.label("catch");
        let cont = self.label("caught");
        f.handlers.push(handler.clone());
        let v = self.gen_expr(f, lhs);
        f.handlers.pop();
        f.line(format!("store i64 {v}, ptr {slot}"));
        f.line(format!("br label %{cont}"));

        f.start_block(&handler);
        let err = self.tmp();
        f.line(format!("{err} = load i64, ptr %err.slot"));
        // The error box is { tag_id, value }.
        let tag = self.load_slot_from_int(f, &err, 0);
        let payload = self.load_slot_from_int(f, &err, 1);

        let mut next = self.label("arm");
        f.line(format!("br label %{next}"));
        for arm in arms {
            f.start_block(&next);
            next = self.label("arm");
            f.scopes.push(HashMap::new());
            let body_l = self.label("armbody");
            self.gen_catch_arm_test(f, &arm.pattern, &tag, &payload, &body_l, &next);
            f.start_block(&body_l);
            let body_v = self.gen_expr(f, &arm.body);
            f.line(format!("store i64 {body_v}, ptr {slot}"));
            f.line(format!("br label %{cont}"));
            f.scopes.pop();
        }
        // No arm matched: rethrow to the next handler out.
        f.start_block(&next);
        self.emit_failure(f, &err);
        let _ = span;

        f.start_block(&cont);
        let out = self.tmp();
        f.line(format!("{out} = load i64, ptr {slot}"));
        out
    }

    /// Test a catch arm against the failed value's tag and payload; on match
    /// fall through to `ok` with bindings done, else branch to `fail`.
    fn gen_catch_arm_test(
        &mut self,
        f: &mut FnCtx,
        pat: &Pattern,
        tag: &str,
        payload: &str,
        ok: &str,
        fail: &str,
    ) {
        let tag_test = |cg: &mut Self, f: &mut FnCtx, name: &str, then_l: &str| -> bool {
            let Some(id) = cg.tag_ids.get(name).copied() else {
                cg.unsupported(pat.span, "this catch pattern");
                f.line(format!("br label %{fail}"));
                return false;
            };
            let m = cg.tmp();
            f.line(format!("{m} = icmp eq i64 {tag}, {id}"));
            f.line(format!("br i1 {m}, label %{then_l}, label %{fail}"));
            f.start_block(then_l);
            true
        };
        match &pat.kind {
            PatternKind::Ctor { name, args, .. } => {
                if let Some(fields) = self.struct_meta.get(name).cloned() {
                    let bind_l = self.label("structpat");
                    if !tag_test(self, f, name, &bind_l) {
                        return;
                    }
                    self.bind_struct_fields(f, args, payload, &fields, 0);
                    f.line(format!("br label %{ok}"));
                } else if let Some(vmeta) = self.variant_meta.get(name).cloned() {
                    let vtest_l = self.label("varianttag");
                    if !tag_test(self, f, &vmeta.enum_name, &vtest_l) {
                        return;
                    }
                    let vid = if vmeta.simple {
                        payload.to_string()
                    } else {
                        self.load_slot_from_int(f, payload, 0)
                    };
                    let bind_l = self.label("variantpat");
                    let m = self.tmp();
                    f.line(format!("{m} = icmp eq i64 {vid}, {}", vmeta.id));
                    f.line(format!("br i1 {m}, label %{bind_l}, label %{fail}"));
                    f.start_block(&bind_l);
                    self.bind_struct_fields(f, args, payload, &vmeta.fields, 1);
                    f.line(format!("br label %{ok}"));
                } else if self.enum_simple.contains_key(name) {
                    // The bare enum name matches any of its variants.
                    let _ = args;
                    let bind_l = self.label("enumpat");
                    if !tag_test(self, f, name, &bind_l) {
                        return;
                    }
                    f.line(format!("br label %{ok}"));
                } else {
                    self.unsupported(pat.span, "this catch pattern");
                    f.line(format!("br label %{fail}"));
                }
            }
            PatternKind::TypedBind { ty, name, .. } => {
                let bind_l = self.label("typedpat");
                if !tag_test(self, f, ty, &bind_l) {
                    return;
                }
                let cty = self.ctype_of_tag(ty);
                self.bind_local_typed(f, name, payload, cty);
                f.line(format!("br label %{ok}"));
            }
            PatternKind::Int(n) => {
                let val_l = self.label("intpat");
                if !tag_test(self, f, "Int", &val_l) {
                    return;
                }
                let m = self.tmp();
                f.line(format!("{m} = icmp eq i64 {payload}, {n}"));
                f.line(format!("br i1 {m}, label %{ok}, label %{fail}"));
            }
            PatternKind::Bool(b) => {
                let val_l = self.label("boolpat");
                if !tag_test(self, f, "Bool", &val_l) {
                    return;
                }
                let m = self.tmp();
                f.line(format!("{m} = icmp eq i64 {payload}, {}", *b as i64));
                f.line(format!("br i1 {m}, label %{ok}, label %{fail}"));
            }
            PatternKind::Str(text) => {
                let val_l = self.label("strpat");
                if !tag_test(self, f, "String", &val_l) {
                    return;
                }
                let lit = self.str_const(text);
                let eq = self.tmp();
                f.line(format!("{eq} = call i64 @rt_str_eq(i64 {payload}, i64 {lit})"));
                let m = self.tmp();
                f.line(format!("{m} = icmp ne i64 {eq}, 0"));
                f.line(format!("br i1 {m}, label %{ok}, label %{fail}"));
            }
            PatternKind::Bind(name) => {
                self.bind_local(f, name, payload);
                f.line(format!("br label %{ok}"));
            }
            PatternKind::Wildcard => {
                f.line(format!("br label %{ok}"));
            }
        }
    }

    fn bind_local(&mut self, f: &mut FnCtx, name: &str, value: &str) {
        self.bind_local_typed(f, name, value, CType::Int);
    }

    fn bind_local_typed(&mut self, f: &mut FnCtx, name: &str, value: &str, cty: CType) {
        let s = f.alloca(self, name);
        f.line(format!("store i64 {value}, ptr {s}"));
        f.scopes
            .last_mut()
            .unwrap()
            .insert(name.to_string(), LocalVar { slot: s, lazy: false, cty });
    }

    /// Destructure a struct/variant pattern's bindings. `header` is the
    /// number of leading non-field slots (1 for boxed enum variants).
    fn bind_struct_fields(
        &mut self,
        f: &mut FnCtx,
        args: &CtorPatArgs,
        base: &str,
        fields: &[String],
        header: i64,
    ) {
        match args {
            CtorPatArgs::None => {}
            CtorPatArgs::Positional(pats) => {
                for (i, pat) in pats.iter().enumerate() {
                    if let PatternKind::Bind(name) = &pat.kind {
                        let v = self.load_slot_from_int(f, base, header + i as i64);
                        self.bind_local(f, &name.clone(), &v);
                    }
                }
            }
            CtorPatArgs::Fields(names) => {
                for (fname, _) in names {
                    if let Some(idx) = fields.iter().position(|x| x == fname) {
                        let v = self.load_slot_from_int(f, base, header + idx as i64);
                        self.bind_local(f, &fname.clone(), &v);
                    }
                }
            }
        }
    }

    fn gen_match(&mut self, f: &mut FnCtx, scrutinee: &Expr, arms: &[Arm], span: Span) -> String {
        let scrut_ty = self.ctype_of(scrutinee);
        let s = self.gen_expr(f, scrutinee);
        let slot = f.fresh_slot(self);
        let cont = self.label("endmatch");
        let mut next = self.label("case");
        f.line(format!("br label %{next}"));
        for arm in arms {
            f.start_block(&next);
            next = self.label("case");
            f.scopes.push(HashMap::new());
            let body_l = self.label("casebody");
            self.gen_pattern_test(f, &arm.pattern, &s, &scrut_ty, &body_l, &next);
            f.start_block(&body_l);
            let v = self.gen_expr(f, &arm.body);
            f.line(format!("store i64 {v}, ptr {slot}"));
            f.line(format!("br label %{cont}"));
            f.scopes.pop();
        }
        f.start_block(&next);
        let msg = self.str_const("no match arm matched");
        f.line(format!("call void @rt_panic(i64 {msg})"));
        f.line("unreachable".to_string());
        let _ = span;
        f.start_block(&cont);
        let out = self.tmp();
        f.line(format!("{out} = load i64, ptr {slot}"));
        out
    }

    /// Emit a test for `pat` against value `v`; on success fall through to
    /// `ok` (bindings done), else branch to `fail`.
    fn gen_pattern_test(
        &mut self,
        f: &mut FnCtx,
        pat: &Pattern,
        v: &str,
        vty: &CType,
        ok: &str,
        fail: &str,
    ) {
        match &pat.kind {
            PatternKind::Wildcard => f.line(format!("br label %{ok}")),
            PatternKind::Bind(name) => {
                self.bind_local_typed(f, name, v, vty.clone());
                f.line(format!("br label %{ok}"));
            }
            PatternKind::Int(n) => {
                let c = self.tmp();
                f.line(format!("{c} = icmp eq i64 {v}, {n}"));
                f.line(format!("br i1 {c}, label %{ok}, label %{fail}"));
            }
            PatternKind::Bool(b) => {
                let c = self.tmp();
                f.line(format!("{c} = icmp eq i64 {v}, {}", *b as i64));
                f.line(format!("br i1 {c}, label %{ok}, label %{fail}"));
            }
            PatternKind::Str(text) => {
                let lit = self.str_const(text);
                let eq = self.tmp();
                f.line(format!("{eq} = call i64 @rt_str_eq(i64 {v}, i64 {lit})"));
                let c = self.tmp();
                f.line(format!("{c} = icmp ne i64 {eq}, 0"));
                f.line(format!("br i1 {c}, label %{ok}, label %{fail}"));
            }
            PatternKind::Ctor { name, args, .. } => match name.as_str() {
                "None" => {
                    let c = self.tmp();
                    f.line(format!("{c} = icmp eq i64 {v}, 0"));
                    f.line(format!("br i1 {c}, label %{ok}, label %{fail}"));
                }
                "Some" => {
                    let c = self.tmp();
                    let inner_l = self.label("some");
                    f.line(format!("{c} = icmp ne i64 {v}, 0"));
                    f.line(format!("br i1 {c}, label %{inner_l}, label %{fail}"));
                    f.start_block(&inner_l);
                    match args {
                        CtorPatArgs::Positional(pats) if pats.len() == 1 => {
                            let inner = self.load_slot_from_int(f, v, 0);
                            let inner_ty = match vty {
                                CType::Option(t) => (**t).clone(),
                                _ => CType::Int,
                            };
                            self.gen_pattern_test(f, &pats[0], &inner, &inner_ty, ok, fail);
                        }
                        _ => f.line(format!("br label %{ok}")),
                    }
                }
                _ => {
                    if let Some(fields) = self.struct_meta.get(name).cloned() {
                        // Nominal structs are statically typed: always match,
                        // just destructure (fields start at slot 0).
                        self.gen_destructure_test(f, args, v, &fields, 0, ok, fail);
                    } else if let Some(vmeta) = self.variant_meta.get(name).cloned() {
                        let vid = if vmeta.simple {
                            v.to_string()
                        } else {
                            self.load_slot_from_int(f, v, 0)
                        };
                        let c = self.tmp();
                        let bind_l = self.label("variantpat");
                        f.line(format!("{c} = icmp eq i64 {vid}, {}", vmeta.id));
                        f.line(format!("br i1 {c}, label %{bind_l}, label %{fail}"));
                        f.start_block(&bind_l);
                        self.gen_destructure_test(f, args, v, &vmeta.fields, 1, ok, fail);
                    } else if self.enum_simple.contains_key(name) {
                        // The bare enum name matches any of its variants.
                        f.line(format!("br label %{ok}"));
                    } else {
                        self.unsupported(pat.span, "this pattern");
                        f.line(format!("br label %{fail}"));
                    }
                }
            },
            PatternKind::TypedBind { name, .. } => {
                // Statically typed: always matches, binds the whole value.
                self.bind_local(f, name, v);
                f.line(format!("br label %{ok}"));
            }
        }
    }

    /// Destructure positional/field sub-patterns of a matched constructor;
    /// `header` is the number of leading non-field slots.
    fn gen_destructure_test(
        &mut self,
        f: &mut FnCtx,
        args: &CtorPatArgs,
        v: &str,
        fields: &[String],
        header: i64,
        ok: &str,
        fail: &str,
    ) {
        match args {
            CtorPatArgs::None => f.line(format!("br label %{ok}")),
            CtorPatArgs::Positional(pats) => {
                // Chain sub-pattern tests.
                let mut cursor = self.label("sub");
                f.line(format!("br label %{cursor}"));
                for (i, sub) in pats.iter().enumerate() {
                    f.start_block(&cursor);
                    cursor = self.label("sub");
                    let field_v = self.load_slot_from_int(f, v, header + i as i64);
                    let target = if i + 1 == pats.len() { ok } else { &cursor };
                    self.gen_pattern_test(f, sub, &field_v, &CType::Int, target, fail);
                }
            }
            CtorPatArgs::Fields(names) => {
                for (fname, _) in names {
                    if let Some(idx) = fields.iter().position(|x| x == fname) {
                        let field_v = self.load_slot_from_int(f, v, header + idx as i64);
                        self.bind_local(f, &fname.clone(), &field_v);
                    }
                }
                f.line(format!("br label %{ok}"));
            }
        }
    }

    // ---- provide -----------------------------------------------------------------------------

    fn gen_provide(&mut self, f: &mut FnCtx, impls: &[ProvideItem], body: &Block) -> String {
        let saved = f.evidence.clone();
        let mut arenas = 0usize;
        for item in impls {
            if item.name == "Arena" {
                // Push a region; allocations in the dynamic extent of the
                // body come from it and are freed wholesale at scope end.
                let size = match item.args.as_deref() {
                    Some([arg]) => self.gen_expr(f, arg),
                    _ => "0".to_string(),
                };
                f.line(format!("call void @rt_arena_push(i64 {size})"));
                arenas += 1;
                continue;
            }
            let name = &item.name;
            let Some(meta) = self.impls.get(name.as_str()) else {
                self.unsupported(item.name_span, "this implementation");
                continue;
            };
            let decl = meta.decl;
            let service = decl.service.clone();
            let Some(smeta) = self.services.get(&service) else { continue };
            let methods = smeta.methods.clone();
            let n_fields = meta.fields.len();

            let ptr = self.gen_alloc(f, (methods.len() + n_fields) as i64);
            for (i, m) in methods.iter().enumerate() {
                let fnref = format!("ptrtoint (ptr @ing.m.{name}.{m} to i64)");
                self.store_slot(f, &ptr, i as i64, &fnref);
            }
            // Field initializers see earlier fields (a temporary scope).
            f.scopes.push(HashMap::new());
            for (i, (fname, _, init)) in decl.fields.iter().enumerate() {
                let v = self.gen_expr(f, init);
                let fcty = self.ctype_of(init);
                self.dup_value(f, &v, &fcty);
                self.store_slot(f, &ptr, (methods.len() + i) as i64, &v);
                let s = f.alloca(self, fname);
                f.line(format!("store i64 {v}, ptr {s}"));
                f.scopes
                    .last_mut()
                    .unwrap()
                    .insert(fname.clone(), LocalVar { slot: s, lazy: false, cty: fcty });
            }
            f.scopes.pop();
            let inst = self.ptr_to_int(f, &ptr);
            f.evidence.insert(service, inst);
        }
        if arenas == 0 {
            let v = self.gen_block(f, body);
            f.evidence = saved;
            return v;
        }
        // Failures inside the body must pop the region(s) before propagating
        // so the arena stack stays balanced.
        let cleanup = self.label("arena.unwind");
        let done = self.label("arena.done");
        let slot = f.fresh_slot(self);
        f.handlers.push(cleanup.clone());
        let v = self.gen_block(f, body);
        f.handlers.pop();
        f.line(format!("store i64 {v}, ptr {slot}"));
        for _ in 0..arenas {
            f.line("call void @rt_arena_pop()".to_string());
        }
        f.line(format!("br label %{done}"));
        f.start_block(&cleanup);
        for _ in 0..arenas {
            f.line("call void @rt_arena_pop()".to_string());
        }
        let err = self.tmp();
        f.line(format!("{err} = load i64, ptr %err.slot"));
        self.emit_failure(f, &err);
        f.start_block(&done);
        f.evidence = saved;
        let out = self.tmp();
        f.line(format!("{out} = load i64, ptr {slot}"));
        out
    }

    // ---- closures and thunks --------------------------------------------------------------------

    /// Create a thunk value for a by-name argument: `{ fnptr, captures... }`.
    fn gen_thunk(&mut self, f: &mut FnCtx, body: &Expr) -> String {
        self.gen_closure_from(f, &[], body)
    }

    fn gen_closure(&mut self, f: &mut FnCtx, params: &[Param], body: &Expr) -> String {
        self.gen_closure_from(f, params, body)
    }

    fn gen_closure_from(&mut self, f: &mut FnCtx, params: &[Param], body: &Expr) -> String {
        // Capture every local the body references plus all current evidence.
        let mut captured: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let param_names: std::collections::HashSet<&str> =
            params.iter().map(|p| p.name.as_str()).collect();
        collect_vars(body, &mut |name| {
            if !param_names.contains(name) && f.lookup(name).is_some() && seen.insert(name.to_string())
            {
                captured.push(name.to_string());
            }
        });
        let evidence: Vec<(String, String)> =
            f.evidence.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let capture_ctys: Vec<CType> = captured
            .iter()
            .map(|name| f.lookup(name).map(|l| l.cty).unwrap_or(CType::Int))
            .collect();

        // Build the closure body as a fresh function.
        self.label += 1;
        let fn_name = format!("@ing.cl{}", self.label);
        let mut inner = FnCtx::new(true);
        let mut inner_params = vec!["ptr %env".to_string()];
        for p in params {
            inner_params.push(format!("i64 %p.{}", p.name));
        }
        // Captured locals: slots loaded from the environment.
        for (i, name) in captured.iter().enumerate() {
            let slot = inner.alloca(self, name);
            let gep = self.tmp();
            inner.line(format!("{gep} = getelementptr i64, ptr %env, i64 {}", 1 + i));
            let v = self.tmp();
            inner.line(format!("{v} = load i64, ptr {gep}"));
            inner.line(format!("store i64 {v}, ptr {slot}"));
            inner
                .scopes
                .last_mut()
                .unwrap()
                .insert(name.clone(), LocalVar { slot, lazy: false, cty: capture_ctys[i].clone() });
        }
        for (i, (service, _)) in evidence.iter().enumerate() {
            let gep = self.tmp();
            inner.line(format!(
                "{gep} = getelementptr i64, ptr %env, i64 {}",
                1 + captured.len() + i
            ));
            let v = self.tmp();
            inner.line(format!("{v} = load i64, ptr {gep}"));
            inner.evidence.insert(service.clone(), v);
        }
        for p in params {
            let slot = inner.alloca(self, &p.name);
            inner.line(format!("store i64 %p.{}, ptr {slot}", p.name));
            inner
                .scopes
                .last_mut()
                .unwrap()
                .insert(p.name.clone(), LocalVar { slot, lazy: false, cty: CType::Int });
        }
        let v = self.gen_expr(&mut inner, body);
        let body_cty = self.ctype_of(body);
        inner.ret(self, &v, Some(&body_cty));
        self.emit_fn(&fn_name, &inner_params, inner);

        // Allocate the closure record at the creation site. Captured heap
        // values are dup'ed — the record owns its references (released only
        // when closures gain their own drop glue; a known leak).
        let ptr = self.gen_alloc(f, (1 + captured.len() + evidence.len()) as i64);
        self.store_slot(f, &ptr, 0, &format!("ptrtoint (ptr {fn_name} to i64)"));
        for (i, name) in captured.iter().enumerate() {
            let local = f.lookup(name).unwrap();
            let v = self.tmp();
            f.line(format!("{v} = load i64, ptr {}", local.slot));
            let cty = capture_ctys[i].clone();
            self.dup_value(f, &v, &cty);
            self.store_slot(f, &ptr, 1 + i as i64, &v);
        }
        for (i, (_, ev)) in evidence.iter().enumerate() {
            self.store_slot(f, &ptr, (1 + captured.len() + i) as i64, ev);
        }
        let out = self.ptr_to_int(f, &ptr);
        self.pool_value(f, &out, &CType::Func);
        out
    }

    fn gen_closure_call(
        &mut self,
        f: &mut FnCtx,
        closure: &str,
        args: &[String],
        result_cty: &CType,
    ) -> String {
        let p = self.tmp();
        f.line(format!("{p} = inttoptr i64 {closure} to ptr"));
        let fp_i = self.tmp();
        f.line(format!("{fp_i} = load i64, ptr {p}"));
        let fp = self.tmp();
        f.line(format!("{fp} = inttoptr i64 {fp_i} to ptr"));
        let mut call_args = vec![format!("ptr {p}")];
        for a in args {
            call_args.push(format!("i64 {a}"));
        }
        let r = self.tmp();
        f.line(format!("{r} = call {{ i64, i64 }} {fp}({})", call_args.join(", ")));
        let out = self.check_failure(f, &r);
        self.pool_value(f, &out, &result_cty.clone());
        out
    }

    // ---- builtins -----------------------------------------------------------------------------

    fn gen_schedule(&mut self, f: &mut FnCtx, name: &str, args: &[&Expr], span: Span) -> String {
        let kind = match name {
            "exponential" => 0,
            "fixed" => 1,
            _ => {
                self.unsupported(span, "this schedule");
                return "0".to_string();
            }
        };
        let base = args.first().map(|a| self.gen_expr(f, a)).unwrap_or_else(|| "0".into());
        let ptr = self.gen_alloc(f, 3);
        self.store_slot(f, &ptr, 0, &kind.to_string());
        self.store_slot(f, &ptr, 1, &base);
        self.store_slot(f, &ptr, 2, "-1");
        let out = self.ptr_to_int(f, &ptr);
        self.pool_value(f, &out, &CType::Schedule);
        out
    }

    /// The graphics module: thin calls into the GL-backed runtime. `run`
    /// builds the frame closure and hands the loop to the runtime.
    fn gen_gfx(&mut self, f: &mut FnCtx, name: &str, args: &[&Expr], span: Span) -> String {
        if name == "run" {
            if args.len() != 4 {
                self.unsupported(span, "this `Gfx.run` call shape");
                return "0".to_string();
            }
            let w = self.gen_expr(f, args[0]);
            let h = self.gen_expr(f, args[1]);
            let title = self.gen_expr(f, args[2]);
            // The frame argument must be a closure value.
            let frame = self.gen_expr(f, args[3]);
            f.line(format!("call void @rt_gfx_run(i64 {w}, i64 {h}, i64 {title}, i64 {frame})"));
            return "0".to_string();
        }
        let (rt_name, arity, has_ret) = match name {
            "shaderNew" => ("rt_gfx_shader_new", 1, true),
            "shaderUse" => ("rt_gfx_shader_use", 1, false),
            "shaderOff" => ("rt_gfx_shader_off", 0, false),
            "clear" => ("rt_gfx_clear", 3, false),
            "rect" => ("rt_gfx_rect", 8, false),
            "rectLines" => ("rt_gfx_rect_lines", 9, false),
            "circle" => ("rt_gfx_circle", 7, false),
            "text" => ("rt_gfx_text", 7, false),
            "textWidth" => ("rt_gfx_text_width", 2, true),
            "mouseX" => ("rt_gfx_mouse_x", 0, true),
            "mouseY" => ("rt_gfx_mouse_y", 0, true),
            "mousePressed" => ("rt_gfx_mouse_pressed", 0, true),
            _ => {
                self.unsupported(span, "this graphics call");
                return "0".to_string();
            }
        };
        if args.len() != arity {
            self.unsupported(span, "this graphics call shape");
            return "0".to_string();
        }
        let vals: Vec<String> =
            args.iter().map(|a| format!("i64 {}", self.gen_expr(f, a))).collect();
        if has_ret {
            let out = self.tmp();
            f.line(format!("{out} = call i64 @{rt_name}({})", vals.join(", ")));
            out
        } else {
            f.line(format!("call void @{rt_name}({})", vals.join(", ")));
            "0".to_string()
        }
    }

    /// Returns Some(value) when `name` is a builtin call.
    fn gen_builtin(&mut self, f: &mut FnCtx, name: &str, args: &[&Expr], span: Span) -> Option<String> {
        let v = match name {
            "println" | "print" => {
                let mut text: Option<String> = None;
                for arg in args {
                    let cty = self.ctype_of(arg);
                    let v = self.gen_expr(f, arg);
                    let s = self.to_display_str(f, &v, &cty, arg.span);
                    text = Some(match text {
                        None => s,
                        Some(prev) => {
                            let space = self.str_const(" ");
                            let t1 = self.tmp();
                            f.line(format!("{t1} = call i64 @rt_str_concat(i64 {prev}, i64 {space})"));
                            self.pool_value(f, &t1, &CType::Str);
                            let t2 = self.tmp();
                            f.line(format!("{t2} = call i64 @rt_str_concat(i64 {t1}, i64 {s})"));
                            self.pool_value(f, &t2, &CType::Str);
                            t2
                        }
                    });
                }
                let s = text.unwrap_or_else(|| self.str_const(""));
                let func = if name == "println" { "rt_println" } else { "rt_print" };
                f.line(format!("call void @{func}(i64 {s})"));
                "0".to_string()
            }
            "show" if args.len() == 1 => {
                let cty = self.ctype_of(args[0]);
                let v = self.gen_expr(f, args[0]);
                if cty == CType::Str {
                    // show quotes strings.
                    let q = self.str_const("\"");
                    let t1 = self.tmp();
                    f.line(format!("{t1} = call i64 @rt_str_concat(i64 {q}, i64 {v})"));
                    self.pool_value(f, &t1, &CType::Str);
                    let t2 = self.tmp();
                    f.line(format!("{t2} = call i64 @rt_str_concat(i64 {t1}, i64 {q})"));
                    self.pool_value(f, &t2, &CType::Str);
                    t2
                } else {
                    self.to_display_str(f, &v, &cty, span)
                }
            }
            "len" if args.len() == 1 => {
                let cty = self.ctype_of(args[0]);
                match cty {
                    CType::Str => {
                        // The fold: len of an interpolation is the sum of the
                        // pieces' lengths — no string is materialized. (V8's
                        // rope strings do the same thing at run time.)
                        if let ExprKind::Str(pieces) = &args[0].kind {
                            if let Some(v) = self.gen_len_fold(f, pieces) {
                                return Some(v);
                            }
                        }
                        let v = self.gen_expr(f, args[0]);
                        let out = self.tmp();
                        f.line(format!("{out} = call i64 @rt_str_chars(i64 {v})"));
                        out
                    }
                    CType::List(_) => {
                        let v = self.gen_expr(f, args[0]);
                        self.load_slot_from_int(f, &v, 0)
                    }
                    _ => {
                        self.unsupported(span, "`len` on this type");
                        "0".to_string()
                    }
                }
            }
            "Some" if args.len() == 1 => {
                let v = self.gen_expr(f, args[0]);
                let icty = self.ctype_of(args[0]);
                self.dup_value(f, &v, &icty);
                let ptr = self.gen_alloc(f, 1);
                self.store_slot(f, &ptr, 0, &v);
                let out = self.ptr_to_int(f, &ptr);
                self.pool_value(f, &out, &CType::Option(Box::new(icty)));
                out
            }
            "getOrElse" if args.len() == 2 => {
                // Fusion: `map.get(k) |> getOrElse(simple)` probes the map
                // directly, skipping the Option box. Only when the default is
                // pure (literal or variable), since fusing evaluates it eagerly.
                if let ExprKind::Method { recv, name: mname, args: margs, .. } = &args[0].kind {
                    if mname == "get" && margs.len() == 1 && is_pure_simple(args[1]) {
                        if let CType::MutMap(k, _) = self.ctype_of(recv) {
                            let kind = if *k == CType::Str { "str" } else { "int" };
                            let m = self.gen_expr(f, recv);
                            let key = self.gen_expr(f, &margs[0]);
                            let d = self.gen_expr(f, args[1]);
                            let out = self.tmp();
                            f.line(format!(
                                "{out} = call i64 @rt_map_get_or_{kind}(i64 {m}, i64 {key}, i64 {d})"
                            ));
                            // Own the result (map value or default alike),
                            // then let the pool release it.
                            let rcty = self.ctype_of_span(span);
                            if self.is_rc(&rcty) {
                                let t = self.tmp();
                                f.line(format!("{t} = call i64 @rt_dup(i64 {out})"));
                                self.pool_value(f, &out, &rcty);
                            }
                            return Some(out);
                        }
                    }
                }
                let opt = self.gen_expr(f, args[0]);
                let slot = f.fresh_slot(self);
                let (some_l, none_l, cont) =
                    (self.label("some"), self.label("none"), self.label("endopt"));
                let c = self.tmp();
                f.line(format!("{c} = icmp ne i64 {opt}, 0"));
                f.line(format!("br i1 {c}, label %{some_l}, label %{none_l}"));
                f.start_block(&some_l);
                let inner = self.load_slot_from_int(f, &opt, 0);
                f.line(format!("store i64 {inner}, ptr {slot}"));
                f.line(format!("br label %{cont}"));
                f.start_block(&none_l);
                let d = self.gen_expr(f, args[1]);
                f.line(format!("store i64 {d}, ptr {slot}"));
                f.line(format!("br label %{cont}"));
                f.start_block(&cont);
                let out = self.tmp();
                f.line(format!("{out} = load i64, ptr {slot}"));
                out
            }
            "orFail" if args.len() == 2 => {
                let opt = self.gen_expr(f, args[0]);
                let (none_l, some_l) = (self.label("orfail"), self.label("orok"));
                let c = self.tmp();
                f.line(format!("{c} = icmp eq i64 {opt}, 0"));
                f.line(format!("br i1 {c}, label %{none_l}, label %{some_l}"));
                f.start_block(&none_l);
                let err = self.gen_expr(f, args[1]);
                let err_cty = self.ctype_of(args[1]);
                self.emit_fail_value(f, &err, &err_cty, args[1].span);
                f.start_block(&some_l);
                self.load_slot_from_int(f, &opt, 0)
            }
            "ignoreFailure" if args.len() == 1 => {
                let handler = self.label("swallow");
                let cont = self.label("swallowed");
                f.handlers.push(handler.clone());
                let _ = self.gen_expr(f, args[0]);
                f.handlers.pop();
                f.line(format!("br label %{cont}"));
                f.start_block(&handler);
                f.line(format!("br label %{cont}"));
                f.start_block(&cont);
                "0".to_string()
            }
            "retry" if args.len() == 2 => return Some(self.gen_retry(f, args[0], args[1])),
            "upTo" if args.len() == 2 => {
                let sched = self.gen_expr(f, args[0]);
                let n = self.gen_expr(f, args[1]);
                let kind = self.load_slot_from_int(f, &sched, 0);
                let base = self.load_slot_from_int(f, &sched, 1);
                let ptr = self.gen_alloc(f, 3);
                self.store_slot(f, &ptr, 0, &kind);
                self.store_slot(f, &ptr, 1, &base);
                self.store_slot(f, &ptr, 2, &n);
                let out = self.ptr_to_int(f, &ptr);
                self.pool_value(f, &out, &CType::Schedule);
                out
            }
            "sleep" if args.len() == 1 => {
                let v = self.gen_expr(f, args[0]);
                f.line(format!("call void @rt_sleep_millis(i64 {v})"));
                "0".to_string()
            }
            "nowMillis" => {
                let out = self.tmp();
                f.line(format!("{out} = call i64 @rt_now_millis()"));
                out
            }
            "nowMicros" => {
                let out = self.tmp();
                f.line(format!("{out} = call i64 @rt_now_micros()"));
                out
            }
            "range" if args.len() == 1 => {
                let n = self.gen_expr(f, args[0]);
                let out = self.tmp();
                f.line(format!("{out} = call i64 @rt_range(i64 {n})"));
                self.pool_value(f, &out, &CType::List(Box::new(CType::Int)));
                out
            }
            "random" if args.len() == 1 => {
                let n = self.gen_expr(f, args[0]);
                let out = self.tmp();
                f.line(format!("{out} = call i64 @rt_random(i64 {n})"));
                out
            }
            "MutMap" => {
                let out = self.tmp();
                f.line(format!("{out} = call i64 @rt_map_new()"));
                out
            }
            "map" if args.len() == 2 => return Some(self.gen_map_builtin(f, args, span)),
            "encode" | "decode" => {
                self.unsupported(span, &format!("`{name}` (runtime JSON)"));
                "0".to_string()
            }
            _ => return None,
        };
        Some(v)
    }

    /// len("n=${n}") = const + digits(n), with no allocation.
    fn gen_len_fold(&mut self, f: &mut FnCtx, pieces: &[StrPiece]) -> Option<String> {
        let mut const_len = 0i64;
        let mut dynamic: Vec<String> = Vec::new();
        for piece in pieces {
            match piece {
                StrPiece::Text(t) => const_len += t.chars().count() as i64,
                StrPiece::Expr(e) => match self.ctype_of(e) {
                    CType::Int => dynamic.push(format!("int:{}", "")),
                    CType::Str => dynamic.push("str".into()),
                    _ => return None,
                },
            }
        }
        let mut acc = const_len.to_string();
        let mut dyn_idx = 0;
        for piece in pieces {
            if let StrPiece::Expr(e) = piece {
                let v = self.gen_expr(f, e);
                let piece_len = match self.ctype_of(e) {
                    CType::Int => {
                        let out = self.tmp();
                        f.line(format!("{out} = call i64 @rt_int_digits(i64 {v})"));
                        out
                    }
                    CType::Str => {
                        let out = self.tmp();
                        f.line(format!("{out} = call i64 @rt_str_chars(i64 {v})"));
                        out
                    }
                    _ => unreachable!(),
                };
                let out = self.tmp();
                f.line(format!("{out} = add i64 {acc}, {piece_len}"));
                acc = out;
                dyn_idx += 1;
            }
        }
        let _ = (dynamic, dyn_idx);
        Some(acc)
    }

    fn gen_retry(&mut self, f: &mut FnCtx, action: &Expr, schedule: &Expr) -> String {
        let sched = self.gen_expr(f, schedule);
        let kind = self.load_slot_from_int(f, &sched, 0);
        let base = self.load_slot_from_int(f, &sched, 1);
        let max_raw = self.load_slot_from_int(f, &sched, 2);
        // max defaults to 3 when unset (-1).
        let is_default = self.tmp();
        f.line(format!("{is_default} = icmp slt i64 {max_raw}, 0"));
        let max = self.tmp();
        f.line(format!("{max} = select i1 {is_default}, i64 3, i64 {max_raw}"));

        let delay_slot = f.fresh_slot(self);
        let attempt_slot = f.fresh_slot(self);
        let result_slot = f.fresh_slot(self);
        f.line(format!("store i64 {base}, ptr {delay_slot}"));
        f.line(format!("store i64 0, ptr {attempt_slot}"));

        let (loop_l, handler, done) =
            (self.label("retry"), self.label("retryfail"), self.label("retried"));
        f.line(format!("br label %{loop_l}"));
        f.start_block(&loop_l);
        f.handlers.push(handler.clone());
        let v = self.gen_expr(f, action);
        f.handlers.pop();
        f.line(format!("store i64 {v}, ptr {result_slot}"));
        f.line(format!("br label %{done}"));

        f.start_block(&handler);
        let attempt = self.tmp();
        f.line(format!("{attempt} = load i64, ptr {attempt_slot}"));
        let exhausted = self.tmp();
        f.line(format!("{exhausted} = icmp sge i64 {attempt}, {max}"));
        let (give_up, again) = (self.label("giveup"), self.label("again"));
        f.line(format!("br i1 {exhausted}, label %{give_up}, label %{again}"));
        f.start_block(&give_up);
        // err.slot still holds the error; rethrow outward.
        let err = self.tmp();
        f.line(format!("{err} = load i64, ptr %err.slot"));
        self.emit_failure(f, &err);
        f.start_block(&again);
        let next_attempt = self.tmp();
        f.line(format!("{next_attempt} = add i64 {attempt}, 1"));
        f.line(format!("store i64 {next_attempt}, ptr {attempt_slot}"));
        let delay = self.tmp();
        f.line(format!("{delay} = load i64, ptr {delay_slot}"));
        f.line(format!("call void @rt_sleep_millis(i64 {delay})"));
        // Exponential: delay *= 2.
        let is_exp = self.tmp();
        f.line(format!("{is_exp} = icmp eq i64 {kind}, 0"));
        let doubled = self.tmp();
        f.line(format!("{doubled} = mul i64 {delay}, 2"));
        let new_delay = self.tmp();
        f.line(format!("{new_delay} = select i1 {is_exp}, i64 {doubled}, i64 {delay}"));
        f.line(format!("store i64 {new_delay}, ptr {delay_slot}"));
        f.line(format!("br label %{loop_l}"));

        f.start_block(&done);
        let out = self.tmp();
        f.line(format!("{out} = load i64, ptr {result_slot}"));
        out
    }

    fn gen_map_builtin(&mut self, f: &mut FnCtx, args: &[&Expr], span: Span) -> String {
        let container_ty = self.ctype_of(args[0]);
        let container = self.gen_expr(f, args[0]);
        let func = self.gen_expr(f, args[1]);
        let result_cty = self.ctype_of_span(span);
        let elem_cty = match &result_cty {
            CType::List(t) | CType::Option(t) => (**t).clone(),
            _ => CType::Int,
        };
        match container_ty {
            CType::List(_) => {
                let n = self.load_slot_from_int(f, &container, 0);
                let bytes = self.tmp();
                f.line(format!("{bytes} = add i64 {n}, 1"));
                let bytes8 = self.tmp();
                f.line(format!("{bytes8} = mul i64 {bytes}, 8"));
                let out_p = self.tmp();
                f.line(format!("{out_p} = call ptr @rt_alloc(i64 {bytes8})"));
                f.line(format!("store i64 {n}, ptr {out_p}"));
                let i_slot = f.fresh_slot(self);
                f.line(format!("store i64 0, ptr {i_slot}"));
                let (loop_l, body_l, done) =
                    (self.label("maploop"), self.label("mapbody"), self.label("mapdone"));
                f.line(format!("br label %{loop_l}"));
                f.start_block(&loop_l);
                let i = self.tmp();
                f.line(format!("{i} = load i64, ptr {i_slot}"));
                let c = self.tmp();
                f.line(format!("{c} = icmp slt i64 {i}, {n}"));
                f.line(format!("br i1 {c}, label %{body_l}, label %{done}"));
                f.start_block(&body_l);
                let src_p = self.tmp();
                f.line(format!("{src_p} = inttoptr i64 {container} to ptr"));
                let idx1 = self.tmp();
                f.line(format!("{idx1} = add i64 {i}, 1"));
                let gep = self.tmp();
                f.line(format!("{gep} = getelementptr i64, ptr {src_p}, i64 {idx1}"));
                let item = self.tmp();
                f.line(format!("{item} = load i64, ptr {gep}"));
                let mapped = self.gen_closure_call(f, &func, &[item], &elem_cty);
                self.dup_value(f, &mapped, &elem_cty.clone());
                let ogep = self.tmp();
                f.line(format!("{ogep} = getelementptr i64, ptr {out_p}, i64 {idx1}"));
                f.line(format!("store i64 {mapped}, ptr {ogep}"));
                let next = self.tmp();
                f.line(format!("{next} = add i64 {i}, 1"));
                f.line(format!("store i64 {next}, ptr {i_slot}"));
                f.line(format!("br label %{loop_l}"));
                f.start_block(&done);
                let out = self.ptr_to_int(f, &out_p);
                self.pool_value(f, &out, &result_cty);
                out
            }
            CType::Option(_) => {
                let slot = f.fresh_slot(self);
                let (some_l, cont) = (self.label("mapsome"), self.label("mapend"));
                f.line(format!("store i64 0, ptr {slot}"));
                let c = self.tmp();
                f.line(format!("{c} = icmp ne i64 {container}, 0"));
                f.line(format!("br i1 {c}, label %{some_l}, label %{cont}"));
                f.start_block(&some_l);
                let inner = self.load_slot_from_int(f, &container, 0);
                let mapped = self.gen_closure_call(f, &func, &[inner], &elem_cty);
                self.dup_value(f, &mapped, &elem_cty.clone());
                let boxed = self.gen_alloc(f, 1);
                self.store_slot(f, &boxed, 0, &mapped);
                let bi = self.ptr_to_int(f, &boxed);
                self.pool_value(f, &bi, &result_cty);
                f.line(format!("store i64 {bi}, ptr {slot}"));
                f.line(format!("br label %{cont}"));
                f.start_block(&cont);
                let out = self.tmp();
                f.line(format!("{out} = load i64, ptr {slot}"));
                out
            }
            _ => {
                self.unsupported(span, "`map` on this type");
                "0".to_string()
            }
        }
    }

    // ---- low-level helpers --------------------------------------------------------------------

    /// Allocate `slots` i64 slots; returns the ptr register.
    fn gen_alloc(&mut self, f: &mut FnCtx, slots: i64) -> String {
        let p = self.tmp();
        f.line(format!("{p} = call ptr @rt_alloc(i64 {})", slots * 8));
        p
    }

    fn store_slot(&mut self, f: &mut FnCtx, ptr_reg: &str, idx: i64, value: &str) {
        let gep = self.tmp();
        f.line(format!("{gep} = getelementptr i64, ptr {ptr_reg}, i64 {idx}"));
        f.line(format!("store i64 {value}, ptr {gep}"));
    }

    fn load_slot_from_int(&mut self, f: &mut FnCtx, int_val: &str, idx: i64) -> String {
        let p = self.tmp();
        f.line(format!("{p} = inttoptr i64 {int_val} to ptr"));
        let gep = self.tmp();
        f.line(format!("{gep} = getelementptr i64, ptr {p}, i64 {idx}"));
        let out = self.tmp();
        f.line(format!("{out} = load i64, ptr {gep}"));
        out
    }

    fn ptr_to_int(&mut self, f: &mut FnCtx, ptr_reg: &str) -> String {
        let out = self.tmp();
        f.line(format!("{out} = ptrtoint ptr {ptr_reg} to i64"));
        out
    }

    // ---- reference counting -----------------------------------------------------------------
    //
    // Perceus-style ARC with function-scoped reclamation: every fresh heap
    // value is registered in the function's pool (an alloca holding it plus
    // its type's drop glue) and released when the function returns; values
    // stored into longer-lived objects are dup'ed at the store. Refcounts
    // are non-atomic; statics and arena objects are skipped by the runtime.

    fn ctype_of_span(&self, span: Span) -> CType {
        self.info.expr_types.get(&(span.start, span.end)).cloned().unwrap_or(CType::Int)
    }

    /// Canonical key for memoizing drop glue per type shape.
    fn ckey(cty: &CType) -> String {
        match cty {
            CType::Int => "i".into(),
            CType::Float => "f".into(),
            CType::Bool => "b".into(),
            CType::Str => "s".into(),
            CType::Unit => "u".into(),
            CType::Duration => "d".into(),
            CType::Schedule => "h".into(),
            CType::Option(t) => format!("o{}", Self::ckey(t)),
            CType::List(t) => format!("l{}", Self::ckey(t)),
            CType::Struct(n) => format!("S{n}"),
            CType::Enum(n) => format!("E{n}"),
            CType::Service(n) => format!("V{n}"),
            CType::Tag(n) => format!("T{n}"),
            CType::MutMap(..) => "M".into(),
            CType::Func => "F".into(),
        }
    }

    fn is_rc(&mut self, cty: &CType) -> bool {
        self.drop_fn(cty).is_some()
    }

    /// Bump a refcount when a heap-shaped value is stored into something
    /// that outlives the current statement.
    fn dup_value(&mut self, f: &mut FnCtx, v: &str, cty: &CType) {
        if self.is_rc(cty) {
            let t = self.tmp();
            f.line(format!("{t} = call i64 @rt_dup(i64 {v})"));
        }
    }

    /// Register a fresh (owned) heap value in the function's pool; it is
    /// released when the function returns.
    fn pool_value(&mut self, f: &mut FnCtx, v: &str, cty: &CType) {
        if let Some(sym) = self.drop_fn(cty) {
            let slot = f.fresh_slot(self);
            f.line(format!("store i64 {v}, ptr {slot}"));
            f.pool.push((slot, sym));
        }
    }

    /// The drop-glue symbol for a type, or None for non-refcounted types.
    /// Glue releases one reference; on reaching zero it releases heap-typed
    /// children and frees the object. Composite types always get their own
    /// glue (memoized before recursing, so cyclic types terminate).
    fn drop_fn(&mut self, cty: &CType) -> Option<String> {
        match cty {
            CType::Int
            | CType::Float
            | CType::Bool
            | CType::Unit
            | CType::Duration
            | CType::Tag(_)
            | CType::Service(_)
            | CType::MutMap(..) => None,
            CType::Enum(n) if self.enum_simple.get(n).copied().unwrap_or(true) => None,
            CType::Str | CType::Schedule | CType::Func => Some(self.leaf_drop()),
            CType::Option(inner) => {
                let key = Self::ckey(cty);
                if let Some(sym) = self.drop_syms.get(&key) {
                    return Some(sym.clone());
                }
                let sym = format!("@ing.drop.{}", self.drop_syms.len());
                self.drop_syms.insert(key, sym.clone());
                let child = self.drop_fn(inner);
                let mut body = String::new();
                if let Some(child) = child {
                    body.push_str("  %p = inttoptr i64 %v to ptr\n  %iv = load i64, ptr %p\n");
                    let _ = writeln!(body, "  call void {child}(i64 %iv)");
                }
                self.emit_drop_glue(&sym, &body);
                Some(sym)
            }
            CType::List(inner) => {
                let key = Self::ckey(cty);
                if let Some(sym) = self.drop_syms.get(&key) {
                    return Some(sym.clone());
                }
                let sym = format!("@ing.drop.{}", self.drop_syms.len());
                self.drop_syms.insert(key, sym.clone());
                match self.drop_fn(inner) {
                    Some(child) => self.emit_list_drop_glue(&sym, &child),
                    None => self.emit_drop_glue(&sym, ""),
                }
                Some(sym)
            }
            CType::Struct(n) => {
                let key = Self::ckey(cty);
                if let Some(sym) = self.drop_syms.get(&key) {
                    return Some(sym.clone());
                }
                let sym = format!("@ing.drop.{}", self.drop_syms.len());
                self.drop_syms.insert(key, sym.clone());
                let fields = self.info.facts.struct_fields.get(n).cloned().unwrap_or_default();
                let mut body = String::new();
                let mut loaded = false;
                for (i, fcty) in fields.iter().enumerate() {
                    if let Some(child) = self.drop_fn(fcty) {
                        if !loaded {
                            body.push_str("  %p = inttoptr i64 %v to ptr\n");
                            loaded = true;
                        }
                        let _ = writeln!(
                            body,
                            "  %f{i}.p = getelementptr i64, ptr %p, i64 {i}\n  %f{i} = load i64, ptr %f{i}.p\n  call void {child}(i64 %f{i})"
                        );
                    }
                }
                self.emit_drop_glue(&sym, &body);
                Some(sym)
            }
            CType::Enum(n) => {
                let key = Self::ckey(cty);
                if let Some(sym) = self.drop_syms.get(&key) {
                    return Some(sym.clone());
                }
                let sym = format!("@ing.drop.{}", self.drop_syms.len());
                self.drop_syms.insert(key, sym.clone());
                let variants = self.info.facts.enum_variants.get(n).cloned().unwrap_or_default();
                // Variants with heap fields get a switch case; others fall
                // straight through to the free.
                let mut cases = String::new();
                let mut blocks = String::new();
                for (vid, (vname, fields)) in variants.iter().enumerate() {
                    let mut decs = String::new();
                    for (i, fcty) in fields.iter().enumerate() {
                        if let Some(child) = self.drop_fn(fcty) {
                            let off = 1 + i;
                            let _ = writeln!(
                                decs,
                                "  %{vname}.f{i}.p = getelementptr i64, ptr %p, i64 {off}\n  %{vname}.f{i} = load i64, ptr %{vname}.f{i}.p\n  call void {child}(i64 %{vname}.f{i})"
                            );
                        }
                    }
                    if !decs.is_empty() {
                        let _ = writeln!(cases, "    i64 {vid}, label %v{vid}");
                        let _ = writeln!(blocks, "v{vid}:\n{decs}  br label %free.go");
                    }
                }
                let body = if cases.is_empty() {
                    String::new()
                } else {
                    format!(
                        "  %p = inttoptr i64 %v to ptr\n  %vid = load i64, ptr %p\n  switch i64 %vid, label %free.go [\n{cases}  ]\n{blocks}"
                    )
                };
                if cases.is_empty() {
                    self.emit_drop_glue(&sym, &body);
                } else {
                    // Bodies with a switch need the explicit free.go join.
                    let _ = writeln!(
                        self.functions,
                        "define void {sym}(i64 %v) {{\nentry:\n  %dead = call i64 @rt_release(i64 %v)\n  %c = icmp ne i64 %dead, 0\n  br i1 %c, label %free, label %done\nfree:\n{body}\nfree.go:\n  call void @rt_free(i64 %v)\n  br label %done\ndone:\n  ret void\n}}\n"
                    );
                }
                Some(sym)
            }
        }
    }

    fn leaf_drop(&mut self) -> String {
        if let Some(sym) = self.drop_syms.get("leaf") {
            return sym.clone();
        }
        let sym = "@ing.drop.leaf".to_string();
        self.drop_syms.insert("leaf".into(), sym.clone());
        self.emit_drop_glue(&sym, "");
        sym
    }

    /// Standard glue shape: release; if the count hit zero run `free_body`
    /// (child releases) and free the object.
    fn emit_drop_glue(&mut self, sym: &str, free_body: &str) {
        let _ = writeln!(
            self.functions,
            "define void {sym}(i64 %v) {{\nentry:\n  %dead = call i64 @rt_release(i64 %v)\n  %c = icmp ne i64 %dead, 0\n  br i1 %c, label %free, label %done\nfree:\n{free_body}  call void @rt_free(i64 %v)\n  br label %done\ndone:\n  ret void\n}}\n"
        );
    }

    fn emit_list_drop_glue(&mut self, sym: &str, child: &str) {
        let _ = writeln!(
            self.functions,
            "define void {sym}(i64 %v) {{\nentry:\n  %i.slot = alloca i64\n  %dead = call i64 @rt_release(i64 %v)\n  %c = icmp ne i64 %dead, 0\n  br i1 %c, label %head, label %done\nhead:\n  %p = inttoptr i64 %v to ptr\n  %n = load i64, ptr %p\n  store i64 0, ptr %i.slot\n  br label %loop\nloop:\n  %i = load i64, ptr %i.slot\n  %lt = icmp slt i64 %i, %n\n  br i1 %lt, label %body, label %free\nbody:\n  %i1 = add i64 %i, 1\n  %gep = getelementptr i64, ptr %p, i64 %i1\n  %e = load i64, ptr %gep\n  call void {child}(i64 %e)\n  store i64 %i1, ptr %i.slot\n  br label %loop\nfree:\n  call void @rt_free(i64 %v)\n  br label %done\ndone:\n  ret void\n}}\n"
        );
    }
}

// ---- per-function emission context -----------------------------------------------

#[derive(Clone)]
struct LocalVar {
    slot: String,
    lazy: bool,
    /// Static type, when known — drives refcount ops at capture sites.
    cty: CType,
}

struct FnCtx {
    body: Vec<String>,
    allocas: Vec<String>,
    scopes: Vec<HashMap<String, LocalVar>>,
    evidence: HashMap<String, String>,
    handlers: Vec<String>,
    /// Fresh heap values owned by this function: (slot, drop glue symbol).
    /// Slots are zeroed at entry (branches may skip the producing store)
    /// and drained — dropped — on every return path.
    pool: Vec<(String, String)>,
    fallible: bool,
    needs_propagate: bool,
    needs_panic: bool,
    slot_counter: u32,
}

impl FnCtx {
    fn new(fallible: bool) -> FnCtx {
        let mut ctx = FnCtx {
            body: Vec::new(),
            allocas: Vec::new(),
            scopes: vec![HashMap::new()],
            evidence: HashMap::new(),
            handlers: Vec::new(),
            pool: Vec::new(),
            fallible,
            needs_propagate: false,
            needs_panic: false,
            slot_counter: 0,
        };
        ctx.allocas.push("%err.slot = alloca i64".to_string());
        ctx
    }

    /// Release every pooled value (the function is about to return).
    fn drain_pool(&mut self, cg: &mut Cg) {
        let pool = self.pool.clone();
        for (slot, sym) in pool {
            let v = cg.tmp();
            self.line(format!("{v} = load i64, ptr {slot}"));
            self.line(format!("call void {sym}(i64 {v})"));
        }
    }

    fn line(&mut self, s: String) {
        self.body.push(s);
    }

    fn start_block(&mut self, label: &str) {
        self.body.push(format!("{label}:"));
    }

    fn lookup(&self, name: &str) -> Option<LocalVar> {
        for scope in self.scopes.iter().rev() {
            if let Some(v) = scope.get(name) {
                return Some(v.clone());
            }
        }
        None
    }

    fn alloca(&mut self, cg: &mut Cg, name: &str) -> String {
        self.slot_counter += 1;
        let _ = name;
        let _ = &cg;
        let slot = format!("%s{}", self.slot_counter);
        self.allocas.push(format!("{slot} = alloca i64"));
        slot
    }

    fn fresh_slot(&mut self, cg: &mut Cg) -> String {
        self.alloca(cg, "tmp")
    }

    /// Final return + the propagate/panic epilogue blocks if needed. A
    /// heap-shaped result is dup'ed before the pool drains so the caller
    /// receives an owned reference.
    fn ret(&mut self, cg: &mut Cg, value: &str, ret_cty: Option<&CType>) {
        if let Some(cty) = ret_cty {
            if cg.is_rc(cty) {
                let t = cg.tmp();
                self.line(format!("{t} = call i64 @rt_dup(i64 {value})"));
            }
        }
        self.drain_pool(cg);
        if self.fallible {
            let a = cg.tmp();
            self.line(format!("{a} = insertvalue {{ i64, i64 }} undef, i64 {value}, 0"));
            let b = cg.tmp();
            self.line(format!("{b} = insertvalue {{ i64, i64 }} {a}, i64 0, 1"));
            self.line(format!("ret {{ i64, i64 }} {b}"));
        } else {
            self.line(format!("ret i64 {value}"));
        }
        if self.needs_propagate {
            self.start_block("propagate");
            let e = cg.tmp();
            self.line(format!("{e} = load i64, ptr %err.slot"));
            self.drain_pool(cg);
            let a = cg.tmp();
            self.line(format!("{a} = insertvalue {{ i64, i64 }} undef, i64 0, 0"));
            let b = cg.tmp();
            self.line(format!("{b} = insertvalue {{ i64, i64 }} {a}, i64 {e}, 1"));
            self.line(format!("ret {{ i64, i64 }} {b}"));
        }
        if self.needs_panic {
            self.start_block("panic.unhandled");
            let msg = cg.str_const("unhandled error escaped an infallible function");
            self.line(format!("call void @rt_panic(i64 {msg})"));
            self.line("unreachable".to_string());
        }
    }
}

/// Pure-and-cheap expressions that may be evaluated eagerly when fusing.
fn is_pure_simple(expr: &Expr) -> bool {
    matches!(
        expr.kind,
        ExprKind::Int(_) | ExprKind::Bool(_) | ExprKind::Float(_) | ExprKind::Var(_)
    )
}

/// Collect every identifier the expression references (for closure capture).
fn collect_vars(expr: &Expr, visit: &mut impl FnMut(&str)) {
    match &expr.kind {
        ExprKind::Var(name) => visit(name),
        ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Bool(_) => {}
        ExprKind::Str(pieces) => {
            for p in pieces {
                if let StrPiece::Expr(e) = p {
                    collect_vars(e, visit);
                }
            }
        }
        ExprKind::List(items) => items.iter().for_each(|e| collect_vars(e, visit)),
        ExprKind::Call { callee, args } => {
            collect_vars(callee, visit);
            args.iter().for_each(|e| collect_vars(e, visit));
        }
        ExprKind::Method { recv, args, .. } => {
            collect_vars(recv, visit);
            args.iter().for_each(|e| collect_vars(e, visit));
        }
        ExprKind::Field { recv, .. } => collect_vars(recv, visit),
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_vars(lhs, visit);
            collect_vars(rhs, visit);
        }
        ExprKind::Unary { expr, .. } => collect_vars(expr, visit),
        ExprKind::Pipe { lhs, target } => {
            collect_vars(lhs, visit);
            match target {
                PipeTarget::Call { callee, args } => {
                    collect_vars(callee, visit);
                    if let Some(args) = args {
                        args.iter().for_each(|e| collect_vars(e, visit));
                    }
                }
                PipeTarget::Catch { arms, .. } => {
                    arms.iter().for_each(|a| collect_vars(&a.body, visit));
                }
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            collect_vars(scrutinee, visit);
            arms.iter().for_each(|a| collect_vars(&a.body, visit));
        }
        ExprKind::Fail { error } => collect_vars(error, visit),
        ExprKind::Provide { body, .. } => collect_block(body, visit),
        ExprKind::If { cond, then_block, else_branch } => {
            collect_vars(cond, visit);
            collect_block(then_block, visit);
            if let Some(e) = else_branch {
                collect_vars(e, visit);
            }
        }
        ExprKind::Block(block) => collect_block(block, visit),
        ExprKind::Lambda { body, .. } => collect_vars(body, visit),
    }
}

fn collect_block(block: &Block, visit: &mut impl FnMut(&str)) {
    for stmt in &block.stmts {
        match stmt {
            Stmt::Expr(e) => collect_vars(e, visit),
            Stmt::Bind { value, .. } => collect_vars(value, visit),
            Stmt::Acquire { .. } => {}
        }
    }
}

const RT_DECLS: &str = r#"declare ptr @rt_alloc(i64)
declare i64 @rt_str_concat(i64, i64)
declare i64 @rt_int_to_str(i64)
declare i64 @rt_int_digits(i64)
declare i64 @rt_float_to_str(i64)
declare i64 @rt_bool_to_str(i64)
declare i64 @rt_duration_to_str(i64)
declare i64 @rt_str_chars(i64)
declare i64 @rt_str_eq(i64, i64)
declare i64 @rt_str_cmp(i64, i64)
declare i64 @rt_show_list_int(i64)
declare i64 @rt_show_list_str(i64)
declare void @rt_print(i64)
declare void @rt_println(i64)
declare i64 @rt_now_millis()
declare i64 @rt_now_micros()
declare void @rt_sleep_millis(i64)
declare void @rt_panic(i64)
declare i64 @rt_map_new()
declare void @rt_map_set_int(i64, i64, i64)
declare i64 @rt_map_get_int(i64, i64)
declare i64 @rt_map_get_or_int(i64, i64, i64)
declare i64 @rt_map_get_or_str(i64, i64, i64)
declare void @rt_map_del_int(i64, i64)
declare void @rt_map_set_str(i64, i64, i64)
declare i64 @rt_map_get_str(i64, i64)
declare void @rt_map_del_str(i64, i64)
declare i64 @rt_map_size(i64)
declare void @rt_arena_push(i64)
declare void @rt_arena_pop()
declare ptr @rt_alloc_global(i64)
declare i64 @rt_dup(i64)
declare i64 @rt_release(i64)
declare void @rt_free(i64)
declare i64 @rt_range(i64)
declare i64 @rt_random(i64)
declare void @rt_gfx_run(i64, i64, i64, i64)
declare void @rt_gfx_clear(i64, i64, i64)
declare void @rt_gfx_rect(i64, i64, i64, i64, i64, i64, i64, i64)
declare void @rt_gfx_rect_lines(i64, i64, i64, i64, i64, i64, i64, i64, i64)
declare void @rt_gfx_circle(i64, i64, i64, i64, i64, i64, i64)
declare void @rt_gfx_text(i64, i64, i64, i64, i64, i64, i64)
declare i64 @rt_gfx_text_width(i64, i64)
declare i64 @rt_gfx_mouse_x()
declare i64 @rt_gfx_mouse_y()
declare i64 @rt_gfx_mouse_pressed()
declare i64 @rt_gfx_shader_new(i64)
declare void @rt_gfx_shader_use(i64)
declare void @rt_gfx_shader_off()
"#;
