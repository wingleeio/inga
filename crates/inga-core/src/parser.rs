//! Recursive-descent parser.
//!
//! Newline policy: statements are separated by newlines (or `;`). An
//! expression continues across a newline when the next meaningful token is a
//! pipe `|>`, a binary operator (except `-` and `!`, which would be ambiguous
//! with a new statement), or a `.` chain. Inside parentheses, brackets, and
//! argument lists newlines are insignificant.

use crate::ast::*;
use crate::diag::Diagnostic;
use crate::span::Span;
use crate::token::{StrPart, Token, TokenKind};

pub fn parse(tokens: Vec<Token>, diagnostics: &mut Vec<Diagnostic>) -> Program {
    // The parser never wants comments; the formatter re-lexes for them.
    let tokens: Vec<Token> =
        tokens.into_iter().filter(|t| !matches!(t.kind, TokenKind::Comment(_))).collect();
    let mut parser = Parser { tokens, pos: 0, diagnostics };
    parser.parse_program()
}

/// Parse a single expression from a nested token stream (string interpolation).
fn parse_sub_expr(tokens: &[Token], diagnostics: &mut Vec<Diagnostic>) -> Expr {
    let tokens: Vec<Token> =
        tokens.iter().filter(|t| !matches!(t.kind, TokenKind::Comment(_))).cloned().collect();
    let mut parser = Parser { tokens, pos: 0, diagnostics };
    parser.skip_newlines();
    let expr = parser.parse_expr();
    parser.skip_newlines();
    if !parser.at(&TokenKind::Eof) {
        let span = parser.peek().span;
        parser.diagnostics.push(Diagnostic::error(span, "expected end of interpolation"));
    }
    expr
}

struct Parser<'a> {
    tokens: Vec<Token>,
    pos: usize,
    diagnostics: &'a mut Vec<Diagnostic>,
}

impl<'a> Parser<'a> {
    // ---- token plumbing -------------------------------------------------

    fn peek(&self) -> &Token {
        &self.tokens[self.pos.min(self.tokens.len() - 1)]
    }

    fn nth(&self, n: usize) -> &Token {
        &self.tokens[(self.pos + n).min(self.tokens.len() - 1)]
    }

    fn at(&self, kind: &TokenKind) -> bool {
        &self.peek().kind == kind
    }

    fn at_ident(&self) -> bool {
        matches!(self.peek().kind, TokenKind::Ident(_))
    }

    fn bump(&mut self) -> Token {
        let token = self.tokens[self.pos.min(self.tokens.len() - 1)].clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        token
    }

    fn prev_span(&self) -> Span {
        self.tokens[self.pos.saturating_sub(1)].span
    }

    fn error_here(&mut self, message: impl Into<String>) {
        let span = self.peek().span;
        self.diagnostics.push(Diagnostic::error(span, message));
    }

    fn expect(&mut self, kind: &TokenKind, what: &str) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            let found = self.peek().kind.describe();
            self.error_here(format!("expected {what}, found {found}"));
            false
        }
    }

    fn expect_ident(&mut self, what: &str) -> (String, Span) {
        if let TokenKind::Ident(name) = &self.peek().kind {
            let name = name.clone();
            let span = self.peek().span;
            self.bump();
            (name, span)
        } else {
            let found = self.peek().kind.describe();
            self.error_here(format!("expected {what}, found {found}"));
            ("<error>".into(), self.peek().span)
        }
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek().kind, TokenKind::Newline | TokenKind::Semi) {
            self.bump();
        }
    }

    /// If the next non-newline token satisfies `pred`, consume the newlines
    /// and return true (expression continuation).
    fn continue_past_newlines(&mut self, pred: impl Fn(&TokenKind) -> bool) -> bool {
        let mut n = 0;
        while matches!(self.nth(n).kind, TokenKind::Newline) {
            n += 1;
        }
        if pred(&self.nth(n).kind) {
            for _ in 0..n {
                self.bump();
            }
            true
        } else {
            false
        }
    }

    // ---- program / declarations ----------------------------------------

    fn parse_program(&mut self) -> Program {
        let mut decls = Vec::new();
        loop {
            self.skip_newlines();
            if self.at(&TokenKind::Eof) {
                break;
            }
            let before = self.pos;
            if let Some(decl) = self.parse_decl() {
                decls.push(decl);
            }
            if self.pos == before {
                // No progress: skip a token to avoid looping forever.
                self.bump();
            }
        }
        Program { decls }
    }

    fn parse_decl(&mut self) -> Option<Decl> {
        if self.at(&TokenKind::KwUse) {
            let start = self.peek().span;
            self.bump();
            let (first, first_span) = self.expect_ident("a module path");
            let mut path = vec![first];
            let mut path_span = first_span;
            while self.at(&TokenKind::Slash) {
                self.bump();
                let (seg, seg_span) = self.expect_ident("a module path segment");
                if seg == "<error>" {
                    break;
                }
                path.push(seg);
                path_span = path_span.to(seg_span);
            }
            let names = if self.at(&TokenKind::LBrace) {
                self.bump();
                self.skip_newlines();
                let mut names = Vec::new();
                while !self.at(&TokenKind::RBrace) && !self.at(&TokenKind::Eof) {
                    let (n, n_span) = self.expect_ident("an imported name");
                    if n == "<error>" {
                        break;
                    }
                    names.push((n, n_span));
                    self.skip_newlines();
                    if self.at(&TokenKind::Comma) {
                        self.bump();
                        self.skip_newlines();
                    }
                }
                self.expect(&TokenKind::RBrace, "`}`");
                Some(names)
            } else {
                None
            };
            return Some(Decl::Use(UseDecl {
                path,
                path_span,
                names,
                span: start.to(self.prev_span()),
            }));
        }
        let is_pub = if self.at(&TokenKind::KwPub) {
            self.bump();
            true
        } else {
            false
        };
        let is_shared = if self.at(&TokenKind::KwShared) {
            self.bump();
            if !self.at(&TokenKind::KwService) {
                self.error_here("`shared` applies to services: `shared service Name { ... }`");
            }
            true
        } else {
            false
        };
        match self.peek().kind.clone() {
            TokenKind::KwStruct => {
                self.bump();
                let mut d = self.parse_struct_decl();
                d.is_pub = is_pub;
                Some(Decl::Struct(d))
            }
            TokenKind::KwEnum => {
                self.bump();
                let mut d = self.parse_enum_decl();
                d.is_pub = is_pub;
                Some(Decl::Enum(d))
            }
            TokenKind::KwService => {
                self.bump();
                let mut d = self.parse_service_decl();
                d.is_pub = is_pub;
                d.is_shared = is_shared;
                Some(Decl::Service(d))
            }
            TokenKind::Ident(_) => {
                let (name, name_span) = self.expect_ident("a declaration name");
                if !self.expect(&TokenKind::ColonColon, "`::`") {
                    return None;
                }
                if self.at(&TokenKind::LParen) {
                    let mut d = self.parse_func_decl(name, name_span);
                    d.is_pub = is_pub;
                    Some(Decl::Func(d))
                } else if self.at_ident() {
                    let mut d = self.parse_impl_decl(name, name_span);
                    d.is_pub = is_pub;
                    Some(Decl::Impl(d))
                } else {
                    self.error_here("expected `(` (function) or a service name (implementation) after `::`");
                    None
                }
            }
            _ => {
                let found = self.peek().kind.describe();
                self.error_here(format!(
                    "expected a declaration (`use`, `struct`, `enum`, `service`, or `name :: ...`), found {found}"
                ));
                // Synchronize to the next line.
                while !matches!(self.peek().kind, TokenKind::Newline | TokenKind::Eof) {
                    self.bump();
                }
                None
            }
        }
    }

    /// After `struct`: `Name = { fields }`
    fn parse_struct_decl(&mut self) -> StructDecl {
        let start = self.prev_span();
        let (name, name_span) = self.expect_ident("a type name");
        if !is_upper(&name) && name != "<error>" {
            self.diagnostics.push(Diagnostic::error(
                name_span,
                format!("type names start with an uppercase letter: `{name}`"),
            ));
        }
        self.expect(&TokenKind::Eq, "`=`");
        let mut fields = Vec::new();
        if self.expect(&TokenKind::LBrace, "`{`") {
            fields = self.parse_field_list();
        }
        StructDecl { is_pub: false, name, name_span, fields, span: start.to(self.prev_span()) }
    }

    /// Fields inside an already-opened `{ ... }`; consumes the closing brace.
    fn parse_field_list(&mut self) -> Vec<Field> {
        let mut fields = Vec::new();
        loop {
            self.skip_newlines();
            if self.at(&TokenKind::RBrace) || self.at(&TokenKind::Eof) {
                break;
            }
            if let Some(field) = self.parse_field() {
                fields.push(field);
            } else {
                break;
            }
            self.skip_newlines();
            if self.at(&TokenKind::Comma) {
                self.bump();
            }
        }
        self.expect(&TokenKind::RBrace, "`}`");
        fields
    }

    /// After `enum`: `Name = Variant | Variant { fields } | ...`
    /// A newline before `|` continues the variant list.
    fn parse_enum_decl(&mut self) -> EnumDecl {
        let start = self.prev_span();
        let (name, name_span) = self.expect_ident("a type name");
        if !is_upper(&name) && name != "<error>" {
            self.diagnostics.push(Diagnostic::error(
                name_span,
                format!("type names start with an uppercase letter: `{name}`"),
            ));
        }
        self.expect(&TokenKind::Eq, "`=`");
        self.skip_newlines();
        let mut variants = Vec::new();
        loop {
            let v_start = self.peek().span;
            let (v_name, v_span) = self.expect_ident("a variant name");
            if v_name == "<error>" {
                break;
            }
            if !is_upper(&v_name) {
                self.diagnostics.push(Diagnostic::error(
                    v_span,
                    format!("variant names start with an uppercase letter: `{v_name}`"),
                ));
            }
            let mut fields = Vec::new();
            if self.at(&TokenKind::LBrace) {
                self.bump();
                fields = self.parse_field_list();
            }
            variants.push(Variant {
                name: v_name,
                name_span: v_span,
                fields,
                span: v_start.to(self.prev_span()),
            });
            if self.at(&TokenKind::Bar) || self.continue_past_newlines(|k| *k == TokenKind::Bar) {
                self.bump();
                self.skip_newlines();
            } else {
                break;
            }
        }
        if variants.is_empty() {
            self.diagnostics.push(Diagnostic::error(
                name_span,
                format!("enum `{name}` needs at least one variant"),
            ));
        }
        EnumDecl { is_pub: false, name, name_span, variants, span: start.to(self.prev_span()) }
    }

    /// `String id` or `id` — type is optional.
    fn parse_field(&mut self) -> Option<Field> {
        let start = self.peek().span;
        let save = self.pos;
        match self.try_parse_type() {
            Some(ty) if self.at_ident() => {
                let (name, _) = self.expect_ident("a field name");
                return Some(Field { ty: Some(ty), name, span: start.to(self.prev_span()) });
            }
            _ => self.pos = save,
        }
        if self.at_ident() {
            let (name, span) = self.expect_ident("a field name");
            Some(Field { ty: None, name, span })
        } else {
            let found = self.peek().kind.describe();
            self.error_here(format!("expected a field, found {found}"));
            None
        }
    }

    fn parse_service_decl(&mut self) -> ServiceDecl {
        let start = self.prev_span();
        let (name, name_span) = self.expect_ident("a service name");
        if !is_upper(&name) && name != "<error>" {
            self.diagnostics.push(Diagnostic::error(
                name_span,
                format!("service names start with an uppercase letter: `{name}`"),
            ));
        }
        let mut methods = Vec::new();
        if self.expect(&TokenKind::LBrace, "`{`") {
            loop {
                self.skip_newlines();
                if self.at(&TokenKind::RBrace) || self.at(&TokenKind::Eof) {
                    break;
                }
                let m_start = self.peek().span;
                let (m_name, m_span) = self.expect_ident("a method name");
                if m_name == "<error>" {
                    break;
                }
                self.expect(&TokenKind::ColonColon, "`::`");
                let sig = self.parse_sig();
                methods.push(MethodSig {
                    name: m_name,
                    name_span: m_span,
                    sig,
                    span: m_start.to(self.prev_span()),
                });
            }
            self.expect(&TokenKind::RBrace, "`}`");
        }
        ServiceDecl {
            is_pub: false,
            is_shared: false,
            name,
            name_span,
            methods,
            span: start.to(self.prev_span()),
        }
    }

    fn parse_impl_decl(&mut self, name: String, name_span: Span) -> ImplDecl {
        let (service, service_span) = self.expect_ident("a service name");
        let mut fields = Vec::new();
        let mut methods = Vec::new();
        if self.expect(&TokenKind::LBrace, "`{`") {
            loop {
                self.skip_newlines();
                if self.at(&TokenKind::RBrace) || self.at(&TokenKind::Eof) {
                    break;
                }
                let (item_name, item_span) = self.expect_ident("a field or method");
                if item_name == "<error>" {
                    break;
                }
                if self.at(&TokenKind::Eq) {
                    self.bump();
                    let value = self.parse_expr();
                    fields.push((item_name, item_span, value));
                } else if self.at(&TokenKind::ColonColon) {
                    self.bump();
                    methods.push(self.parse_func_decl(item_name, item_span));
                } else {
                    self.error_here("expected `=` (field) or `::` (method)");
                    break;
                }
            }
            self.expect(&TokenKind::RBrace, "`}`");
        }
        ImplDecl {
            is_pub: false,
            name,
            name_span,
            service,
            service_span,
            fields,
            methods,
            span: name_span.to(self.prev_span()),
        }
    }

    fn parse_func_decl(&mut self, name: String, name_span: Span) -> FuncDecl {
        let sig = self.parse_sig();
        self.skip_newlines();
        let body = if self.at(&TokenKind::LBrace) {
            self.parse_block()
        } else {
            self.error_here("expected `{` to start the function body");
            Block { stmts: Vec::new(), span: self.peek().span }
        };
        FuncDecl { is_pub: false, name, name_span, sig, body, span: name_span.to(self.prev_span()) }
    }

    /// `(params) [-> Type] [! Err, Err] [uses Cap, Cap]`
    fn parse_sig(&mut self) -> Sig {
        let mut sig = Sig::default();
        if self.expect(&TokenKind::LParen, "`(`") {
            self.skip_newlines();
            while !self.at(&TokenKind::RParen) && !self.at(&TokenKind::Eof) {
                sig.params.push(self.parse_param());
                self.skip_newlines();
                if self.at(&TokenKind::Comma) {
                    self.bump();
                    self.skip_newlines();
                } else {
                    break;
                }
            }
            self.expect(&TokenKind::RParen, "`)`");
        }
        if self.at(&TokenKind::Arrow) {
            self.bump();
            match self.try_parse_type() {
                Some(ty) => sig.ret = Some(ty),
                None => self.error_here("expected a return type after `->`"),
            }
        }
        if self.at(&TokenKind::Bang) {
            self.bump();
            sig.errors = Some(self.parse_name_list("an error type"));
        }
        if self.at(&TokenKind::KwUses) {
            self.bump();
            sig.uses = Some(self.parse_name_list("a service name"));
        }
        sig
    }

    fn parse_name_list(&mut self, what: &str) -> Vec<(String, Span)> {
        let mut names = Vec::new();
        loop {
            let (name, span) = self.expect_ident(what);
            if name == "<error>" {
                break;
            }
            names.push((name, span));
            if self.at(&TokenKind::Comma) {
                self.bump();
            } else {
                break;
            }
        }
        names
    }

    fn parse_param(&mut self) -> Param {
        let start = self.peek().span;
        let lazy = if self.at(&TokenKind::KwLazy) {
            self.bump();
            true
        } else {
            false
        };
        let save = self.pos;
        match self.try_parse_type() {
            Some(ty) if self.at_ident() => {
                let (name, _) = self.expect_ident("a parameter name");
                return Param { lazy, ty: Some(ty), name, span: start.to(self.prev_span()) };
            }
            _ => self.pos = save,
        }
        let (name, span) = self.expect_ident("a parameter name");
        Param { lazy, ty: None, name, span: start.to(span) }
    }

    /// Type grammar: `Name`, `[T]` (list), postfix `?` (option),
    /// `(T, ...) -> T [! rows] [uses rows]` (function), `(T)` (grouping).
    fn try_parse_type(&mut self) -> Option<TypeExpr> {
        let start = self.peek().span;
        let mut ty = match self.peek().kind.clone() {
            TokenKind::Ident(name) => {
                self.bump();
                // `MutMap<Int, String>` / `Task<Int>` — type arguments only
                // ever follow an uppercase name, which keeps speculative
                // type parses from swallowing `x < y` comparisons.
                let upper = name.starts_with(char::is_uppercase);
                if upper && self.at(&TokenKind::Lt) {
                    self.bump();
                    self.skip_newlines();
                    let mut args = Vec::new();
                    loop {
                        args.push(self.try_parse_type()?);
                        self.skip_newlines();
                        if self.at(&TokenKind::Comma) {
                            self.bump();
                            self.skip_newlines();
                        } else {
                            break;
                        }
                    }
                    let row = if self.at(&TokenKind::Bang) {
                        self.bump();
                        self.parse_name_list("an error type")
                    } else {
                        Vec::new()
                    };
                    if !self.at(&TokenKind::Gt) {
                        return None;
                    }
                    self.bump();
                    TypeExpr::Apply {
                        name,
                        name_span: start,
                        args,
                        row,
                        span: start.to(self.prev_span()),
                    }
                } else {
                    TypeExpr::Name(name, start)
                }
            }
            TokenKind::LBracket => {
                self.bump();
                let inner = self.try_parse_type()?;
                if !self.at(&TokenKind::RBracket) {
                    return None;
                }
                self.bump();
                TypeExpr::List(Box::new(inner), start.to(self.prev_span()))
            }
            TokenKind::LParen => {
                self.bump();
                self.skip_newlines();
                let mut params = Vec::new();
                while !self.at(&TokenKind::RParen) && !self.at(&TokenKind::Eof) {
                    params.push(self.try_parse_type()?);
                    self.skip_newlines();
                    if self.at(&TokenKind::Comma) {
                        self.bump();
                        self.skip_newlines();
                    } else {
                        break;
                    }
                }
                if !self.at(&TokenKind::RParen) {
                    return None;
                }
                self.bump();
                if self.at(&TokenKind::Arrow) {
                    self.bump();
                    let ret = self.try_parse_type()?;
                    // Rows bind to the innermost function type.
                    let errors = if self.at(&TokenKind::Bang) {
                        self.bump();
                        self.parse_name_list("an error type")
                    } else {
                        Vec::new()
                    };
                    let caps = if self.at(&TokenKind::KwUses) {
                        self.bump();
                        self.parse_name_list("a service name")
                    } else {
                        Vec::new()
                    };
                    TypeExpr::Func {
                        params,
                        ret: Box::new(ret),
                        errors,
                        caps,
                        span: start.to(self.prev_span()),
                    }
                } else if params.len() == 1 {
                    // `(T)` — grouping, e.g. `((Int) -> Int)?`.
                    params.pop().unwrap()
                } else if params.len() >= 2 {
                    TypeExpr::Tuple(params, start.to(self.prev_span()))
                } else {
                    return None;
                }
            }
            _ => return None,
        };
        while self.at(&TokenKind::Question) {
            self.bump();
            ty = TypeExpr::Option(Box::new(ty), start.to(self.prev_span()));
        }
        Some(ty)
    }

    // ---- blocks and statements ------------------------------------------

    fn parse_block(&mut self) -> Block {
        let start = self.peek().span;
        self.expect(&TokenKind::LBrace, "`{`");
        let stmts = self.parse_stmts_until_rbrace();
        self.expect(&TokenKind::RBrace, "`}`");
        Block { stmts, span: start.to(self.prev_span()) }
    }

    /// Statements inside an already-opened `{ ... }`; stops at (without
    /// consuming) the closing brace.
    fn parse_stmts_until_rbrace(&mut self) -> Vec<Stmt> {
        let mut stmts = Vec::new();
        loop {
            self.skip_newlines();
            if self.at(&TokenKind::RBrace) || self.at(&TokenKind::Eof) {
                break;
            }
            let before = self.pos;
            // The braceless `provide` statement scopes over the rest of the
            // enclosing block; it must be intercepted before parse_stmt.
            if self.at(&TokenKind::KwProvide) && self.provide_is_inline() {
                let start = self.peek().span;
                self.bump();
                let impls = self.parse_provide_items();
                let body_start = self.peek().span;
                let body_stmts = self.parse_stmts_until_rbrace();
                let body = Block { stmts: body_stmts, span: body_start.to(self.prev_span()) };
                let span = start.to(self.prev_span());
                stmts.push(Stmt::Expr(Expr {
                    kind: ExprKind::Provide { impls, body, inline: true },
                    span,
                }));
                break;
            }
            stmts.push(self.parse_stmt());
            // Statements must be followed by a newline, `;`, or `}`.
            if !matches!(
                self.peek().kind,
                TokenKind::Newline | TokenKind::Semi | TokenKind::RBrace | TokenKind::Eof
            ) {
                let found = self.peek().kind.describe();
                self.error_here(format!("expected end of statement, found {found}"));
                while !matches!(self.peek().kind, TokenKind::Newline | TokenKind::RBrace | TokenKind::Eof)
                {
                    self.bump();
                }
            }
            if self.pos == before {
                self.bump();
            }
        }
        stmts
    }

    /// After `provide`, decide between the braced and braceless forms: scan
    /// past the item list (idents, commas, balanced parens); a `{` before the
    /// end of the line means a braced body.
    fn provide_is_inline(&self) -> bool {
        let mut n = 1; // past `provide`
        let mut depth = 0usize;
        loop {
            match &self.nth(n).kind {
                TokenKind::LParen => depth += 1,
                TokenKind::RParen => depth = depth.saturating_sub(1),
                TokenKind::LBrace if depth == 0 => return false,
                TokenKind::Newline | TokenKind::Semi | TokenKind::RBrace | TokenKind::Eof
                    if depth == 0 =>
                {
                    return true;
                }
                TokenKind::Eof => return true,
                _ => {}
            }
            n += 1;
        }
    }

    /// `consoleLogger, Arena(256.kb), fakeDb`
    fn parse_provide_items(&mut self) -> Vec<ProvideItem> {
        let mut items = Vec::new();
        loop {
            let (name, name_span) = self.expect_ident("an implementation name");
            if name == "<error>" {
                break;
            }
            let args = if self.at(&TokenKind::LParen) { Some(self.parse_args()) } else { None };
            items.push(ProvideItem { name, name_span, args });
            if self.at(&TokenKind::Comma) {
                self.bump();
                self.skip_newlines();
            } else {
                break;
            }
        }
        items
    }

    fn parse_stmt(&mut self) -> Stmt {
        // `name = expr` (untyped binding)
        if let TokenKind::Ident(name) = self.peek().kind.clone() {
            if self.nth(1).kind == TokenKind::Eq {
                let name_span = self.peek().span;
                self.bump();
                self.bump();
                self.skip_newlines();
                let value = self.parse_expr();
                return Stmt::Bind { ty: None, name, name_span, value };
            }
        }
        // `Type name = expr` (typed binding) or `Service name` (acquire)
        let save = self.pos;
        if matches!(
            self.peek().kind,
            TokenKind::Ident(_) | TokenKind::LBracket | TokenKind::LParen
        ) {
            // `(` is ambiguous (function type vs lambda/parens); a failed
            // speculative type parse must rewind.
            let parsed = self.try_parse_type();
            if parsed.is_none() {
                self.pos = save;
            }
            if let Some(ty) = parsed {
                if let TokenKind::Ident(name) = self.peek().kind.clone() {
                    let name_span = self.peek().span;
                    self.bump();
                    if self.at(&TokenKind::Eq) {
                        self.bump();
                        self.skip_newlines();
                        let value = self.parse_expr();
                        return Stmt::Bind { ty: Some(ty), name, name_span, value };
                    }
                    if matches!(
                        self.peek().kind,
                        TokenKind::Newline | TokenKind::Semi | TokenKind::RBrace | TokenKind::Eof
                    ) {
                        if let TypeExpr::Name(service, service_span) = &ty {
                            if is_upper(service) {
                                return Stmt::Acquire {
                                    service: service.clone(),
                                    service_span: *service_span,
                                    name,
                                    name_span,
                                };
                            }
                        }
                        self.diagnostics.push(Diagnostic::error(
                            ty.span(),
                            "expected a service name (capability bindings look like `Cache cache`)",
                        ));
                        return Stmt::Acquire {
                            service: "<error>".into(),
                            service_span: ty.span(),
                            name,
                            name_span,
                        };
                    }
                }
                self.pos = save;
            }
        }
        Stmt::Expr(self.parse_expr())
    }

    // ---- expressions -----------------------------------------------------

    pub fn parse_expr(&mut self) -> Expr {
        self.parse_pipe()
    }

    fn parse_pipe(&mut self) -> Expr {
        let mut lhs = self.parse_or();
        while self.at(&TokenKind::PipeOp)
            || self.continue_past_newlines(|k| *k == TokenKind::PipeOp)
        {
            self.bump();
            self.skip_newlines();
            let target = self.parse_pipe_target();
            let end = self.prev_span();
            let span = lhs.span.to(end);
            lhs = Expr { kind: ExprKind::Pipe { lhs: Box::new(lhs), target }, span };
        }
        lhs
    }

    fn parse_pipe_target(&mut self) -> PipeTarget {
        if self.at(&TokenKind::KwCatch) {
            let start = self.peek().span;
            self.bump();
            self.skip_newlines();
            let arms = self.parse_arms("an error pattern");
            return PipeTarget::Catch { arms, span: start.to(self.prev_span()) };
        }
        // Parse a postfix expression and reinterpret the outermost call.
        let expr = self.parse_unary();
        match expr.kind {
            ExprKind::Call { callee, args } => PipeTarget::Call { callee, args: Some(args) },
            ExprKind::Method { recv, name, name_span, args } => {
                let span = recv.span.to(name_span);
                let callee = Expr { kind: ExprKind::Field { recv, name, name_span }, span };
                PipeTarget::Call { callee: Box::new(callee), args: Some(args) }
            }
            _ => PipeTarget::Call { callee: Box::new(expr), args: None },
        }
    }

    fn parse_or(&mut self) -> Expr {
        let mut lhs = self.parse_and();
        while self.at(&TokenKind::OrOr) || self.continue_past_newlines(|k| *k == TokenKind::OrOr) {
            self.bump();
            self.skip_newlines();
            let rhs = self.parse_and();
            let span = lhs.span.to(rhs.span);
            lhs = Expr {
                kind: ExprKind::Binary { op: BinOp::Or, lhs: Box::new(lhs), rhs: Box::new(rhs) },
                span,
            };
        }
        lhs
    }

    fn parse_and(&mut self) -> Expr {
        let mut lhs = self.parse_cmp();
        while self.at(&TokenKind::AndAnd) || self.continue_past_newlines(|k| *k == TokenKind::AndAnd)
        {
            self.bump();
            self.skip_newlines();
            let rhs = self.parse_cmp();
            let span = lhs.span.to(rhs.span);
            lhs = Expr {
                kind: ExprKind::Binary { op: BinOp::And, lhs: Box::new(lhs), rhs: Box::new(rhs) },
                span,
            };
        }
        lhs
    }

    fn cmp_op(kind: &TokenKind) -> Option<BinOp> {
        Some(match kind {
            TokenKind::EqEq => BinOp::Eq,
            TokenKind::NotEq => BinOp::Ne,
            TokenKind::Lt => BinOp::Lt,
            TokenKind::Le => BinOp::Le,
            TokenKind::Gt => BinOp::Gt,
            TokenKind::Ge => BinOp::Ge,
            _ => return None,
        })
    }

    fn parse_cmp(&mut self) -> Expr {
        let mut lhs = self.parse_add();
        while let Some(op) = Self::cmp_op(&self.peek().kind) {
            self.bump();
            self.skip_newlines();
            let rhs = self.parse_add();
            let span = lhs.span.to(rhs.span);
            lhs = Expr {
                kind: ExprKind::Binary { op, lhs: Box::new(lhs), rhs: Box::new(rhs) },
                span,
            };
        }
        lhs
    }

    fn parse_add(&mut self) -> Expr {
        let mut lhs = self.parse_mul();
        loop {
            // `+` continues across newlines; `-` must be on the same line
            // (a leading `-` on a new line starts a new statement).
            let op = if self.at(&TokenKind::Plus)
                || self.continue_past_newlines(|k| *k == TokenKind::Plus)
            {
                BinOp::Add
            } else if self.at(&TokenKind::Minus) {
                BinOp::Sub
            } else {
                break;
            };
            self.bump();
            self.skip_newlines();
            let rhs = self.parse_mul();
            let span = lhs.span.to(rhs.span);
            lhs = Expr {
                kind: ExprKind::Binary { op, lhs: Box::new(lhs), rhs: Box::new(rhs) },
                span,
            };
        }
        lhs
    }

    fn parse_mul(&mut self) -> Expr {
        let mut lhs = self.parse_unary();
        loop {
            let op = match self.peek().kind {
                TokenKind::Star => BinOp::Mul,
                TokenKind::Slash => BinOp::Div,
                TokenKind::Percent => BinOp::Mod,
                _ => break,
            };
            self.bump();
            self.skip_newlines();
            let rhs = self.parse_unary();
            let span = lhs.span.to(rhs.span);
            lhs = Expr {
                kind: ExprKind::Binary { op, lhs: Box::new(lhs), rhs: Box::new(rhs) },
                span,
            };
        }
        lhs
    }

    fn parse_unary(&mut self) -> Expr {
        match self.peek().kind {
            TokenKind::Minus => {
                let start = self.peek().span;
                self.bump();
                let expr = self.parse_unary();
                let span = start.to(expr.span);
                Expr { kind: ExprKind::Unary { op: UnOp::Neg, expr: Box::new(expr) }, span }
            }
            TokenKind::Bang => {
                let start = self.peek().span;
                self.bump();
                let expr = self.parse_unary();
                let span = start.to(expr.span);
                Expr { kind: ExprKind::Unary { op: UnOp::Not, expr: Box::new(expr) }, span }
            }
            _ => self.parse_postfix(),
        }
    }

    fn parse_postfix(&mut self) -> Expr {
        let mut expr = self.parse_primary();
        loop {
            if self.at(&TokenKind::Dot) || self.continue_past_newlines(|k| *k == TokenKind::Dot) {
                self.bump();
                if let TokenKind::Int(index) = self.peek().kind {
                    let index_span = self.peek().span;
                    self.bump();
                    let span = expr.span.to(index_span);
                    expr = Expr {
                        kind: ExprKind::TupleIndex { recv: Box::new(expr), index, index_span },
                        span,
                    };
                    continue;
                }
                let (name, name_span) = self.expect_ident("a field or method name");
                if self.at(&TokenKind::LParen) {
                    let args = self.parse_args();
                    let span = expr.span.to(self.prev_span());
                    expr = Expr {
                        kind: ExprKind::Method { recv: Box::new(expr), name, name_span, args },
                        span,
                    };
                } else {
                    let span = expr.span.to(name_span);
                    expr = Expr {
                        kind: ExprKind::Field { recv: Box::new(expr), name, name_span },
                        span,
                    };
                }
            } else if self.at(&TokenKind::LParen) {
                let args = self.parse_args();
                let span = expr.span.to(self.prev_span());
                expr = Expr { kind: ExprKind::Call { callee: Box::new(expr), args }, span };
            } else {
                break;
            }
        }
        expr
    }

    fn parse_args(&mut self) -> Vec<Expr> {
        let mut args = Vec::new();
        self.expect(&TokenKind::LParen, "`(`");
        self.skip_newlines();
        while !self.at(&TokenKind::RParen) && !self.at(&TokenKind::Eof) {
            args.push(self.parse_expr());
            self.skip_newlines();
            if self.at(&TokenKind::Comma) {
                self.bump();
                self.skip_newlines();
            } else {
                break;
            }
        }
        self.expect(&TokenKind::RParen, "`)`");
        args
    }

    fn parse_primary(&mut self) -> Expr {
        let start = self.peek().span;
        match self.peek().kind.clone() {
            TokenKind::Int(n) => {
                self.bump();
                Expr { kind: ExprKind::Int(n), span: start }
            }
            TokenKind::Float(f) => {
                self.bump();
                Expr { kind: ExprKind::Float(f), span: start }
            }
            TokenKind::KwTrue => {
                self.bump();
                Expr { kind: ExprKind::Bool(true), span: start }
            }
            TokenKind::KwFalse => {
                self.bump();
                Expr { kind: ExprKind::Bool(false), span: start }
            }
            TokenKind::Str(parts, triple) => {
                self.bump();
                let mut pieces = Vec::new();
                for part in parts {
                    match part {
                        StrPart::Text(text) => pieces.push(StrPiece::Text(text)),
                        StrPart::Expr(tokens) => {
                            let expr = parse_sub_expr(&tokens, self.diagnostics);
                            pieces.push(StrPiece::Expr(Box::new(expr)));
                        }
                    }
                }
                Expr { kind: ExprKind::Str(pieces, triple), span: start }
            }
            TokenKind::Ident(name) => {
                self.bump();
                // `User { ..base, field: value }` — record update (the `..`
                // disambiguates from blocks and patterns).
                if is_upper(&name)
                    && self.at(&TokenKind::LBrace)
                    && self.nth(1).kind == TokenKind::Dot
                    && self.nth(2).kind == TokenKind::Dot
                {
                    self.bump(); // {
                    self.bump(); // .
                    self.bump(); // .
                    let base = self.parse_expr();
                    let mut fields = Vec::new();
                    while self.at(&TokenKind::Comma) {
                        self.bump();
                        self.skip_newlines();
                        if self.at(&TokenKind::RBrace) {
                            break;
                        }
                        let (fname, fspan) = self.expect_ident("a field name");
                        if fname == "<error>" {
                            break;
                        }
                        self.expect(&TokenKind::Colon, "`:`");
                        self.skip_newlines();
                        let value = self.parse_expr();
                        fields.push((fname, fspan, value));
                        self.skip_newlines();
                    }
                    self.expect(&TokenKind::RBrace, "`}`");
                    return Expr {
                        kind: ExprKind::RecordUpdate {
                            name,
                            name_span: start,
                            base: Box::new(base),
                            fields,
                        },
                        span: start.to(self.prev_span()),
                    };
                }
                Expr { kind: ExprKind::Var(name), span: start }
            }
            TokenKind::LBracket => {
                self.bump();
                self.skip_newlines();
                let mut items = Vec::new();
                while !self.at(&TokenKind::RBracket) && !self.at(&TokenKind::Eof) {
                    items.push(self.parse_expr());
                    self.skip_newlines();
                    if self.at(&TokenKind::Comma) {
                        self.bump();
                        self.skip_newlines();
                    } else {
                        break;
                    }
                }
                self.expect(&TokenKind::RBracket, "`]`");
                Expr { kind: ExprKind::List(items), span: start.to(self.prev_span()) }
            }
            TokenKind::LParen => {
                if let Some(lambda) = self.try_parse_lambda() {
                    return lambda;
                }
                self.bump();
                self.skip_newlines();
                let expr = self.parse_expr();
                self.skip_newlines();
                if self.at(&TokenKind::Comma) {
                    // `(a, b, ...)` — a tuple.
                    let mut items = vec![expr];
                    while self.at(&TokenKind::Comma) {
                        self.bump();
                        self.skip_newlines();
                        if self.at(&TokenKind::RParen) {
                            break;
                        }
                        items.push(self.parse_expr());
                        self.skip_newlines();
                    }
                    self.expect(&TokenKind::RParen, "`)`");
                    return Expr {
                        kind: ExprKind::Tuple(items),
                        span: start.to(self.prev_span()),
                    };
                }
                self.expect(&TokenKind::RParen, "`)`");
                Expr { kind: expr.kind, span: start.to(self.prev_span()) }
            }
            TokenKind::LBrace => {
                let block = self.parse_block();
                let span = block.span;
                Expr { kind: ExprKind::Block(block), span }
            }
            TokenKind::KwMatch => {
                self.bump();
                let scrutinee = self.parse_or(); // no top-level pipe before `{`
                self.skip_newlines();
                self.expect(&TokenKind::LBrace, "`{`");
                self.skip_newlines();
                let arms = self.parse_arms_until_brace("a pattern");
                Expr {
                    kind: ExprKind::Match { scrutinee: Box::new(scrutinee), arms },
                    span: start.to(self.prev_span()),
                }
            }
            TokenKind::KwFail => {
                self.bump();
                let error = self.parse_or();
                let span = start.to(error.span);
                Expr { kind: ExprKind::Fail { error: Box::new(error) }, span }
            }
            TokenKind::KwProvide => {
                self.bump();
                let impls = self.parse_provide_items();
                self.skip_newlines();
                let body = self.parse_block();
                Expr {
                    kind: ExprKind::Provide { impls, body, inline: false },
                    span: start.to(self.prev_span()),
                }
            }
            TokenKind::KwIf => {
                self.bump();
                let cond = self.parse_or();
                self.skip_newlines();
                let then_block = self.parse_block();
                let mut else_branch = None;
                if self.continue_past_newlines(|k| *k == TokenKind::KwElse)
                    || self.at(&TokenKind::KwElse)
                {
                    self.bump();
                    self.skip_newlines();
                    let else_expr = if self.at(&TokenKind::KwIf) {
                        self.parse_primary()
                    } else {
                        let block = self.parse_block();
                        let span = block.span;
                        Expr { kind: ExprKind::Block(block), span }
                    };
                    else_branch = Some(Box::new(else_expr));
                }
                Expr {
                    kind: ExprKind::If { cond: Box::new(cond), then_block, else_branch },
                    span: start.to(self.prev_span()),
                }
            }
            ref other => {
                let found = other.describe();
                self.error_here(format!("expected an expression, found {found}"));
                Expr { kind: ExprKind::Var("<error>".into()), span: start }
            }
        }
    }

    /// `(x, y) -> expr` — only committed to when the matching `)` is directly
    /// followed by `->`.
    fn try_parse_lambda(&mut self) -> Option<Expr> {
        debug_assert!(self.at(&TokenKind::LParen));
        // Scan ahead for the matching `)` and check the next token.
        let mut depth = 0usize;
        let mut n = 0usize;
        loop {
            match &self.nth(n).kind {
                TokenKind::LParen => depth += 1,
                TokenKind::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                TokenKind::Eof => return None,
                _ => {}
            }
            n += 1;
        }
        let mut after = n + 1;
        while matches!(self.nth(after).kind, TokenKind::Newline) {
            after += 1;
        }
        if self.nth(after).kind != TokenKind::Arrow {
            return None;
        }
        let start = self.peek().span;
        self.bump(); // (
        self.skip_newlines();
        let mut params = Vec::new();
        while !self.at(&TokenKind::RParen) && !self.at(&TokenKind::Eof) {
            params.push(self.parse_param());
            self.skip_newlines();
            if self.at(&TokenKind::Comma) {
                self.bump();
                self.skip_newlines();
            } else {
                break;
            }
        }
        self.expect(&TokenKind::RParen, "`)`");
        self.skip_newlines();
        self.expect(&TokenKind::Arrow, "`->`");
        self.skip_newlines();
        let body = self.parse_expr();
        let span = start.to(body.span);
        Some(Expr { kind: ExprKind::Lambda { params, body: Box::new(body) }, span })
    }

    // ---- patterns and arms ------------------------------------------------

    /// Arms inside an already-opened `{ ... }`; consumes the closing brace.
    fn parse_arms_until_brace(&mut self, what: &str) -> Vec<Arm> {
        let mut arms = Vec::new();
        loop {
            self.skip_newlines();
            if self.at(&TokenKind::RBrace) || self.at(&TokenKind::Eof) {
                break;
            }
            let before = self.pos;
            arms.push(self.parse_arm(what));
            if self.at(&TokenKind::Comma) {
                self.bump();
            }
            if self.pos == before {
                self.bump();
            }
        }
        self.expect(&TokenKind::RBrace, "`}`");
        arms
    }

    /// `catch { ... }` — expects and consumes both braces.
    fn parse_arms(&mut self, what: &str) -> Vec<Arm> {
        self.expect(&TokenKind::LBrace, "`{`");
        self.parse_arms_until_brace(what)
    }

    fn parse_arm(&mut self, what: &str) -> Arm {
        let start = self.peek().span;
        let pattern = self.parse_pattern(what);
        self.expect(&TokenKind::Arrow, "`->`");
        self.skip_newlines();
        let body = self.parse_expr();
        Arm { pattern, body, span: start.to(self.prev_span()) }
    }

    fn parse_pattern(&mut self, what: &str) -> Pattern {
        let start = self.peek().span;
        match self.peek().kind.clone() {
            TokenKind::LParen => {
                self.bump();
                self.skip_newlines();
                let mut pats = Vec::new();
                while !self.at(&TokenKind::RParen) && !self.at(&TokenKind::Eof) {
                    pats.push(self.parse_pattern(what));
                    self.skip_newlines();
                    if self.at(&TokenKind::Comma) {
                        self.bump();
                        self.skip_newlines();
                    } else {
                        break;
                    }
                }
                self.expect(&TokenKind::RParen, "`)`");
                Pattern { kind: PatternKind::Tuple(pats), span: start.to(self.prev_span()) }
            }
            TokenKind::Int(n) => {
                self.bump();
                Pattern { kind: PatternKind::Int(n), span: start }
            }
            TokenKind::Minus => {
                self.bump();
                if let TokenKind::Int(n) = self.peek().kind {
                    self.bump();
                    Pattern { kind: PatternKind::Int(-n), span: start.to(self.prev_span()) }
                } else {
                    self.error_here("expected an integer after `-` in a pattern");
                    Pattern { kind: PatternKind::Wildcard, span: start }
                }
            }
            TokenKind::Str(parts, _) => {
                self.bump();
                let text = match parts.as_slice() {
                    [StrPart::Text(t)] => t.clone(),
                    _ => {
                        self.diagnostics.push(Diagnostic::error(
                            start,
                            "string patterns cannot contain interpolation",
                        ));
                        String::new()
                    }
                };
                Pattern { kind: PatternKind::Str(text), span: start }
            }
            TokenKind::KwTrue => {
                self.bump();
                Pattern { kind: PatternKind::Bool(true), span: start }
            }
            TokenKind::KwFalse => {
                self.bump();
                Pattern { kind: PatternKind::Bool(false), span: start }
            }
            TokenKind::Ident(name) if name == "_" => {
                self.bump();
                Pattern { kind: PatternKind::Wildcard, span: start }
            }
            TokenKind::Ident(name) if !is_upper(&name) => {
                self.bump();
                Pattern { kind: PatternKind::Bind(name), span: start }
            }
            TokenKind::Ident(name) => {
                self.bump();
                let name_span = start;
                // `String msg` / `Shape s` — a type name binding the value.
                if let TokenKind::Ident(bind) = self.peek().kind.clone() {
                    if !is_upper(&bind) && bind != "_" {
                        self.bump();
                        return Pattern {
                            kind: PatternKind::TypedBind { ty: name, ty_span: name_span, name: bind },
                            span: start.to(self.prev_span()),
                        };
                    }
                }
                let args = if self.at(&TokenKind::LParen) {
                    self.bump();
                    self.skip_newlines();
                    let mut pats = Vec::new();
                    while !self.at(&TokenKind::RParen) && !self.at(&TokenKind::Eof) {
                        pats.push(self.parse_pattern(what));
                        self.skip_newlines();
                        if self.at(&TokenKind::Comma) {
                            self.bump();
                            self.skip_newlines();
                        } else {
                            break;
                        }
                    }
                    self.expect(&TokenKind::RParen, "`)`");
                    CtorPatArgs::Positional(pats)
                } else if self.at(&TokenKind::LBrace) {
                    self.bump();
                    self.skip_newlines();
                    let mut fields = Vec::new();
                    while !self.at(&TokenKind::RBrace) && !self.at(&TokenKind::Eof) {
                        let (field, field_span) = self.expect_ident("a field name");
                        if field == "<error>" {
                            break;
                        }
                        fields.push((field, field_span));
                        self.skip_newlines();
                        if self.at(&TokenKind::Comma) {
                            self.bump();
                            self.skip_newlines();
                        } else {
                            break;
                        }
                    }
                    self.expect(&TokenKind::RBrace, "`}`");
                    CtorPatArgs::Fields(fields)
                } else {
                    CtorPatArgs::None
                };
                Pattern {
                    kind: PatternKind::Ctor { name, name_span, args },
                    span: start.to(self.prev_span()),
                }
            }
            ref other => {
                let found = other.describe();
                self.error_here(format!("expected {what}, found {found}"));
                self.bump();
                Pattern { kind: PatternKind::Wildcard, span: start }
            }
        }
    }
}
