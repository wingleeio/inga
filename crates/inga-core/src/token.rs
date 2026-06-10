//! Token definitions.

use crate::span::Span;

#[derive(Debug, Clone, PartialEq)]
pub enum StrPart {
    /// Literal text (escapes already resolved).
    Text(String),
    /// A `${...}` hole: the tokens between the braces.
    Expr(Vec<Token>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // Literals and identifiers
    Ident(String),
    Int(i64),
    Float(f64),
    Str(Vec<StrPart>),

    // Trivia (kept in the stream; the parser skips it, the formatter uses it)
    Comment(String),
    Newline,

    // Keywords
    KwStruct,
    KwEnum,
    KwService,
    KwMatch,
    KwCatch,
    KwFail,
    KwProvide,
    KwUses,
    KwLazy,
    KwIf,
    KwElse,
    KwTrue,
    KwFalse,

    // Punctuation and operators
    ColonColon, // ::
    Arrow,      // ->
    PipeOp,     // |>
    Eq,         // =
    EqEq,       // ==
    NotEq,      // !=
    Lt,
    Le,
    Gt,
    Ge,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Bang,     // !
    Question, // ? (option types: `User?`)
    Bar,      // | (enum variant separator)
    AndAnd,   // &&
    OrOr,     // ||
    Dot,
    Comma,
    Colon,
    Semi,
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,

    Eof,
}

impl TokenKind {
    pub fn keyword(ident: &str) -> Option<TokenKind> {
        Some(match ident {
            "struct" => TokenKind::KwStruct,
            "enum" => TokenKind::KwEnum,
            "service" => TokenKind::KwService,
            "match" => TokenKind::KwMatch,
            "catch" => TokenKind::KwCatch,
            "fail" => TokenKind::KwFail,
            "provide" => TokenKind::KwProvide,
            "uses" => TokenKind::KwUses,
            "lazy" => TokenKind::KwLazy,
            "if" => TokenKind::KwIf,
            "else" => TokenKind::KwElse,
            "true" => TokenKind::KwTrue,
            "false" => TokenKind::KwFalse,
            _ => return None,
        })
    }

    /// Human-readable name for error messages.
    pub fn describe(&self) -> String {
        match self {
            TokenKind::Ident(name) => format!("`{name}`"),
            TokenKind::Int(_) => "integer literal".into(),
            TokenKind::Float(_) => "float literal".into(),
            TokenKind::Str(_) => "string literal".into(),
            TokenKind::Comment(_) => "comment".into(),
            TokenKind::Newline => "end of line".into(),
            TokenKind::Eof => "end of file".into(),
            TokenKind::KwStruct => "`struct`".into(),
            TokenKind::KwEnum => "`enum`".into(),
            TokenKind::KwService => "`service`".into(),
            TokenKind::KwMatch => "`match`".into(),
            TokenKind::KwCatch => "`catch`".into(),
            TokenKind::KwFail => "`fail`".into(),
            TokenKind::KwProvide => "`provide`".into(),
            TokenKind::KwUses => "`uses`".into(),
            TokenKind::KwLazy => "`lazy`".into(),
            TokenKind::KwIf => "`if`".into(),
            TokenKind::KwElse => "`else`".into(),
            TokenKind::KwTrue => "`true`".into(),
            TokenKind::KwFalse => "`false`".into(),
            other => format!(
                "`{}`",
                match other {
                    TokenKind::ColonColon => "::",
                    TokenKind::Arrow => "->",
                    TokenKind::PipeOp => "|>",
                    TokenKind::Eq => "=",
                    TokenKind::EqEq => "==",
                    TokenKind::NotEq => "!=",
                    TokenKind::Lt => "<",
                    TokenKind::Le => "<=",
                    TokenKind::Gt => ">",
                    TokenKind::Ge => ">=",
                    TokenKind::Plus => "+",
                    TokenKind::Minus => "-",
                    TokenKind::Star => "*",
                    TokenKind::Slash => "/",
                    TokenKind::Percent => "%",
                    TokenKind::Bang => "!",
                    TokenKind::Question => "?",
                    TokenKind::Bar => "|",
                    TokenKind::AndAnd => "&&",
                    TokenKind::OrOr => "||",
                    TokenKind::Dot => ".",
                    TokenKind::Comma => ",",
                    TokenKind::Colon => ":",
                    TokenKind::Semi => ";",
                    TokenKind::LParen => "(",
                    TokenKind::RParen => ")",
                    TokenKind::LBrace => "{",
                    TokenKind::RBrace => "}",
                    TokenKind::LBracket => "[",
                    TokenKind::RBracket => "]",
                    _ => "?",
                }
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

impl Token {
    pub fn new(kind: TokenKind, span: Span) -> Token {
        Token { kind, span }
    }
}
