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
    /// `error UserNotFound = { id }`
    Error(StructDecl),
    /// `type User = { Int id, String name }`
    Type(StructDecl),
    /// `service Logger { info :: (String msg) ... }`
    Service(ServiceDecl),
    /// `consoleLogger :: Logger { ... }`
    Impl(ImplDecl),
    /// `getUserById :: (id) { ... }`
    Func(FuncDecl),
}

#[derive(Debug)]
pub struct StructDecl {
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
    pub name: String,
    pub name_span: Span,
    pub methods: Vec<MethodSig>,
    pub span: Span,
}

#[derive(Debug)]
pub struct MethodSig {
    pub name: String,
    pub name_span: Span,
    pub sig: Sig,
    pub span: Span,
}

#[derive(Debug)]
pub struct ImplDecl {
    pub name: String,
    pub name_span: Span,
    pub service: String,
    pub service_span: Span,
    /// `store = MutMap()` — instance state, evaluated when the impl is provided.
    pub fields: Vec<(String, Span, Expr)>,
    pub methods: Vec<FuncDecl>,
    pub span: Span,
}

#[derive(Debug)]
pub struct FuncDecl {
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
}

impl TypeExpr {
    pub fn span(&self) -> Span {
        match self {
            TypeExpr::Name(_, s) | TypeExpr::Option(_, s) | TypeExpr::List(_, s) => *s,
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
    Str(Vec<StrPiece>),
    Var(String),
    List(Vec<Expr>),
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
    /// `provide consoleLogger, memoryCache { body }`
    Provide { impls: Vec<(String, Span)>, body: Block },
    If { cond: Box<Expr>, then_block: Block, else_branch: Option<Box<Expr>> },
    Block(Block),
    /// `(x, y) -> expr`
    Lambda { params: Vec<Param>, body: Box<Expr> },
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

pub fn is_upper(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}
