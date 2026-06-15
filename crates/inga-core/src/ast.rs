//! Abstract syntax tree. The AST mirrors the source closely (pipes are kept
//! as pipe nodes, not desugared) so the formatter can reproduce the program's
//! shape; the checker and interpreter desugar where needed.

use crate::span::Span;

#[derive(Debug, Default)]
pub struct Program {
    pub decls: Vec<Decl>,
}

#[derive(Debug)]
pub enum Decl {
    /// `use cards` / `use std/graphics` (qualified: `graphics.rect(...)`)
    /// or `use cards { rankName, suitCol }` (unqualified, listed names only).
    Use(UseDecl),
    /// `struct User = { Int id, String name }`
    Struct(StructDecl),
    /// `enum Shape = Circle { Float radius } | Rect { Float w, Float h } | Dot`
    Enum(EnumDecl),
    /// `service Logger { info :: (String msg) ... }`
    Service(ServiceDecl),
    /// `provider consoleLogger :: () -> Logger { ... }` — a provider yields a
    /// service instance; it is a function with inferred rows, so its setup may
    /// fail and depend on other services.
    Provider(ProviderDecl),
    /// `getUserById :: (id) { ... }`
    Func(FuncDecl),
    /// `maxRetries = 3` — a top-level constant, evaluated once at startup
    /// in declaration order. Initializers are pure (no `!`, no `uses`).
    Const(ConstDecl),
    /// `type Handler = (HttpRequest) -> HttpResponse uses Session` — a
    /// transparent alias, resolved wherever the name appears in a type.
    TypeAlias(TypeAliasDecl),
}

#[derive(Debug)]
pub struct TypeAliasDecl {
    pub is_pub: bool,
    pub name: String,
    pub name_span: Span,
    pub ty: TypeExpr,
    pub span: Span,
}

#[derive(Debug)]
pub struct ConstDecl {
    pub is_pub: bool,
    pub ty: Option<TypeExpr>,
    pub name: String,
    pub name_span: Span,
    pub value: Expr,
    pub span: Span,
}

#[derive(Debug)]
pub struct UseDecl {
    /// Path segments: `std/graphics` -> ["std", "graphics"]. The last
    /// segment is the qualified alias.
    pub path: Vec<String>,
    pub path_span: Span,
    /// `use m { a, b }` imports only these names, unqualified.
    pub names: Option<Vec<(String, Span)>>,
    pub span: Span,
}

#[derive(Debug)]
pub struct StructDecl {
    /// Exported to importing modules?
    pub is_pub: bool,
    pub name: String,
    pub name_span: Span,
    pub fields: Vec<Field>,
    pub span: Span,
}

#[derive(Debug)]
pub struct EnumDecl {
    pub is_pub: bool,
    pub name: String,
    pub name_span: Span,
    pub variants: Vec<Variant>,
    pub span: Span,
}

/// One enum variant: `Circle { Float radius }` or a bare `Dot`.
#[derive(Debug)]
pub struct Variant {
    pub name: String,
    pub name_span: Span,
    pub fields: Vec<Field>,
    pub span: Span,
}

#[derive(Debug)]
pub struct Field {
    pub ty: Option<TypeExpr>,
    pub name: String,
    pub span: Span,
}

#[derive(Debug)]
pub struct ServiceDecl {
    pub is_pub: bool,
    /// `shared service`: instances may cross fiber boundaries; every
    /// implementation is checked to carry only scalar state.
    pub is_shared: bool,
    pub name: String,
    pub name_span: Span,
    pub methods: Vec<MethodSig>,
    /// `User user` — typed value members; impls satisfy them with fields
    /// of the same name, callers read them as `instance.user`.
    pub values: Vec<Field>,
    pub span: Span,
}

#[derive(Debug)]
pub struct MethodSig {
    pub name: String,
    pub name_span: Span,
    pub sig: Sig,
    pub span: Span,
}

/// `provider WithSession :: (HttpRequest req) { Session { user: … } }`
///
/// A provider is a function whose result *constructs* the service it provides
/// — `Session { user: u }`, `Logger { info: (m) -> … }`. The service, the
/// `uses`/`!` rows, and the (always-the-service) return type are all inferred.
/// The body may acquire other services and `fail` during construction (those
/// rows ride at the provide site); the method closures themselves are pure
/// (they may not acquire services — the provider captures what they need).
#[derive(Debug)]
pub struct ProviderDecl {
    pub is_pub: bool,
    pub name: String,
    pub name_span: Span,
    /// The service this provider satisfies — inferred by the checker from the
    /// body's trailing construction (empty until then).
    pub service: String,
    pub service_span: Span,
    /// `(params) [! Errs] [uses Deps]` — no `-> ret`; rows optional/inferred.
    pub sig: Sig,
    /// The function body; its result constructs the service instance.
    pub body: Block,
    pub span: Span,
}

#[derive(Debug)]
pub struct FuncDecl {
    pub is_pub: bool,
    pub name: String,
    pub name_span: Span,
    pub sig: Sig,
    pub body: Block,
    pub span: Span,
}

/// `(String id, lazy a action) -> User ! UserNotFound, DbError uses Database, Cache`
/// Every part except the parameter list is optional and inferred when absent.
#[derive(Debug, Default)]
pub struct Sig {
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    pub errors: Option<Vec<(String, Span)>>,
    pub uses: Option<Vec<(String, Span)>>,
}

#[derive(Debug)]
pub struct Param {
    pub lazy: bool,
    pub ty: Option<TypeExpr>,
    pub name: String,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum TypeExpr {
    /// `Int`, `User`, `a`
    Name(String, Span),
    /// `User?`
    Option(Box<TypeExpr>, Span),
    /// `[User]`
    List(Box<TypeExpr>, Span),
    /// `(Int, String)` — a tuple type.
    Tuple(Vec<TypeExpr>, Span),
    /// `MutMap<Int, String>`, `Fiber<Int ! Boom>`, `Outcome<a ! E>` — the
    /// builtin generic types, written the way hover renders them. The row
    /// (after `!`) is meaningful only for Fiber/Outcome.
    Apply {
        name: String,
        name_span: Span,
        args: Vec<TypeExpr>,
        row: Vec<(String, Span)>,
        span: Span,
    },
    /// `(Int, String) -> Bool`, optionally with effect rows:
    /// `(Int) -> User ! DbError uses Logger`. A plain arrow type is a
    /// *pure* contract — no failures, no capabilities.
    Func {
        params: Vec<TypeExpr>,
        ret: Box<TypeExpr>,
        errors: Vec<(String, Span)>,
        caps: Vec<(String, Span)>,
        span: Span,
    },
}

impl TypeExpr {
    pub fn span(&self) -> Span {
        match self {
            TypeExpr::Name(_, s) | TypeExpr::Option(_, s) | TypeExpr::List(_, s) => *s,
            TypeExpr::Tuple(_, s) => *s,
            TypeExpr::Apply { span, .. } | TypeExpr::Func { span, .. } => *span,
        }
    }
}

#[derive(Debug)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug)]
pub enum Stmt {
    /// `user = expr` / `String user = expr`
    Bind { ty: Option<TypeExpr>, name: String, name_span: Span, value: Expr },
    /// `Cache cache` — bind the Cache capability from the environment.
    Acquire { service: String, service_span: Span, name: String, name_span: Span },
    Expr(Expr),
}

#[derive(Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug)]
pub enum ExprKind {
    Int(i64),
    Float(f64),
    Bool(bool),
    /// String literal; interpolation holes are sub-expressions.
    Str(Vec<StrPiece>, bool),
    Var(String),
    List(Vec<Expr>),
    /// `(1, "x")` — a tuple (two or more elements).
    Tuple(Vec<Expr>),
    /// `pair.0` — tuple element access.
    TupleIndex { recv: Box<Expr>, index: i64, index_span: Span },
    /// `User { name: expr }` — named-field construction — or
    /// `User { ..base, name: expr }` — functional record update.
    RecordUpdate { name: String, name_span: Span, base: Option<Box<Expr>>, fields: Vec<(String, Span, Expr)> },
    /// `f(a, b)`
    Call { callee: Box<Expr>, args: Vec<Expr> },
    /// `recv.name(a, b)`
    Method { recv: Box<Expr>, name: String, name_span: Span, args: Vec<Expr> },
    /// `recv.name` (also `100.millis`, `Schedule.exponential`)
    Field { recv: Box<Expr>, name: String, name_span: Span },
    Binary { op: BinOp, lhs: Box<Expr>, rhs: Box<Expr> },
    Unary { op: UnOp, expr: Box<Expr> },
    /// `lhs |> target`
    Pipe { lhs: Box<Expr>, target: PipeTarget },
    Match { scrutinee: Box<Expr>, arms: Vec<Arm> },
    /// `fail UserNotFound(id)`
    Fail { error: Box<Expr> },
    /// `provide consoleLogger, memoryCache { body }`, or the braceless
    /// statement form whose body is the rest of the enclosing block
    /// (`inline`). Items scope left to right: later items' field
    /// initializers see the services provided before them.
    Provide { impls: Vec<ProvideItem>, body: Block, inline: bool },
    If { cond: Box<Expr>, then_block: Block, else_branch: Option<Box<Expr>> },
    Block(Block),
    /// `(x, y) -> expr`
    Lambda { params: Vec<Param>, body: Box<Expr> },
}

/// One item of a `provide` list: an implementation name, or a configured
/// builtin resource like `Arena(256.kb)`.
#[derive(Debug)]
pub struct ProvideItem {
    pub name: String,
    pub name_span: Span,
    pub args: Option<Vec<Expr>>,
}

#[derive(Debug)]
pub enum StrPiece {
    Text(String),
    Expr(Box<Expr>),
}

#[derive(Debug)]
pub enum PipeTarget {
    /// `|> f(a)` or bare `|> f` (args is None when bare).
    Call { callee: Box<Expr>, args: Option<Vec<Expr>> },
    /// `|> catch { CacheMiss -> ... }`
    Catch { arms: Vec<Arm>, span: Span },
}

#[derive(Debug)]
pub struct Arm {
    pub pattern: Pattern,
    pub body: Expr,
    pub span: Span,
}

#[derive(Debug)]
pub struct Pattern {
    pub kind: PatternKind,
    pub span: Span,
}

#[derive(Debug)]
pub enum PatternKind {
    Wildcard,
    /// Lowercase identifier: binds the value.
    Bind(String),
    Int(i64),
    Str(String),
    Bool(bool),
    /// `Some(x)`, `None`, `CacheMiss`, `DbError(e)`, `UserNotFound { id }`
    Ctor { name: String, name_span: Span, args: CtorPatArgs },
    /// `String msg`, `Shape s` — matches a value by its type and binds it
    /// (type-before-name, like parameters). Mainly for `catch` arms.
    TypedBind { ty: String, ty_span: Span, name: String },
    /// `(a, b)` — destructures a tuple.
    Tuple(Vec<Pattern>),
    /// A string template: literal text must match, `${Type name}`
    /// holes capture the text between and bind it.
    StrTemplate(Vec<StrPatPiece>),
}

#[derive(Debug)]
pub enum StrPatPiece {
    Text(String),
    /// `${Int id}` / `${String slug}` / `${name}` (String when omitted).
    Hole { ty: Option<String>, name: String, span: Span },
}

#[derive(Debug)]
pub enum CtorPatArgs {
    /// `CacheMiss` — matches any value of that constructor.
    None,
    /// `Some(x)` / `DbError(e)` — positional; for errors a single pattern
    /// binds the whole error value.
    Positional(Vec<Pattern>),
    /// `UserNotFound { id }` — destructures named fields.
    Fields(Vec<(String, Span)>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Mod => "%",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::And => "&&",
            BinOp::Or => "||",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}

/// Spans of self-calls in tail position: the block's trailing expression,
/// recursing through if/match/catch arms and nested blocks. Provide bodies
/// are excluded (arena scopes run cleanup after their body). Both backends
/// turn these calls into jumps (tail-call elimination).
pub fn collect_tail_spans(
    block: &Block,
    name: &str,
    out: &mut std::collections::HashSet<(u32, u32)>,
) {
    if let Some(Stmt::Expr(e)) = block.stmts.last() {
        tail_expr(e, name, out);
    }
}

fn tail_expr(expr: &Expr, name: &str, out: &mut std::collections::HashSet<(u32, u32)>) {
    match &expr.kind {
        ExprKind::Call { callee, .. } => {
            if let ExprKind::Var(n) = &callee.kind {
                if n == name {
                    out.insert((expr.span.start, expr.span.end));
                }
            }
        }
        ExprKind::Pipe { target, .. } => match target {
            PipeTarget::Call { callee, .. } => {
                if let ExprKind::Var(n) = &callee.kind {
                    if n == name {
                        out.insert((expr.span.start, expr.span.end));
                    }
                }
            }
            PipeTarget::Catch { arms, .. } => {
                for arm in arms {
                    tail_expr(&arm.body, name, out);
                }
            }
        },
        ExprKind::If { then_block, else_branch, .. } => {
            collect_tail_spans(then_block, name, out);
            if let Some(e) = else_branch {
                tail_expr(e, name, out);
            }
        }
        ExprKind::Match { arms, .. } => {
            for arm in arms {
                tail_expr(&arm.body, name, out);
            }
        }
        ExprKind::Block(block) => collect_tail_spans(block, name, out),
        _ => {}
    }
}

pub fn is_upper(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}
