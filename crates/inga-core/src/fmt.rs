//! Canonical formatter.
//!
//! Formats from the AST (so it never reflows something it doesn't understand
//! — files with parse errors are left untouched), re-attaching comments by
//! their original position. Style: 4-space indent, one `|>` per line for
//! multi-step pipelines, `->` aligned within match/catch arms, `=` aligned
//! across consecutive `error` declarations, one blank line preserved between
//! groups.

use crate::ast::*;
use crate::diag::Diagnostic;
use crate::lexer;
use crate::parser;
use crate::span::{LineIndex, Span};
use crate::token::TokenKind;

const INDENT: &str = "    ";
const MAX_WIDTH: usize = 100;

/// Returns the formatted source, or the parse diagnostics if the file does
/// not parse (broken code is never reformatted).
pub fn format(src: &str) -> Result<String, Vec<Diagnostic>> {
    let mut diagnostics = Vec::new();
    let tokens = lexer::lex(src, &mut diagnostics);
    let comments: Vec<(Span, String)> = tokens
        .iter()
        .filter_map(|t| match &t.kind {
            TokenKind::Comment(text) => Some((t.span, text.clone())),
            _ => None,
        })
        .collect();
    let program = parser::parse(tokens, &mut diagnostics);
    if diagnostics.iter().any(|d| d.severity == crate::diag::Severity::Error) {
        return Err(diagnostics);
    }
    let mut printer = Printer {
        out: String::new(),
        lines: LineIndex::new(src),
        comments,
        next_comment: 0,
        prev_end_line: None,
    };
    printer.print_program(&program);
    Ok(printer.out)
}

struct Printer {
    out: String,
    lines: LineIndex,
    comments: Vec<(Span, String)>,
    next_comment: usize,
    /// Original end line of the previously printed item (for blank lines).
    prev_end_line: Option<u32>,
}

impl Printer {
    // ---- comment / blank-line bookkeeping --------------------------------

    /// Emit comments that appear before `offset`, preserving blank lines.
    fn flush_comments_before(&mut self, offset: u32, indent: usize) {
        while self.next_comment < self.comments.len() {
            let (span, text) = self.comments[self.next_comment].clone();
            if span.start >= offset {
                break;
            }
            let line = self.lines.line(span.start);
            self.blank_line_if_gap(line);
            for (i, comment_line) in text.lines().enumerate() {
                if i > 0 {
                    self.out.push('\n');
                }
                self.push_indent(indent);
                self.out.push_str(comment_line.trim_end());
            }
            self.out.push('\n');
            self.prev_end_line = Some(self.lines.line(span.end));
            self.next_comment += 1;
        }
    }

    /// Append a comment that sits on the same original line as the item that
    /// just ended at `end`.
    fn attach_trailing_comment(&mut self, end: u32) {
        if self.next_comment >= self.comments.len() {
            return;
        }
        let (span, text) = self.comments[self.next_comment].clone();
        if self.lines.line(span.start) == self.lines.line(end) && span.start >= end {
            // Trailing line comment; multi-line block comments stay put.
            if !text.contains('\n') {
                self.out.push(' ');
                self.out.push_str(text.trim_end());
                self.next_comment += 1;
            }
        }
    }

    fn blank_line_if_gap(&mut self, line: u32) {
        if let Some(prev) = self.prev_end_line {
            if line > prev + 1 && !self.out.is_empty() {
                self.out.push('\n');
            }
        }
    }

    fn push_indent(&mut self, indent: usize) {
        for _ in 0..indent {
            self.out.push_str(INDENT);
        }
    }

    // ---- program ----------------------------------------------------------

    fn print_program(&mut self, program: &Program) {
        let mut i = 0;
        let decls = &program.decls;
        while i < decls.len() {
            // Align a run of consecutive `struct` declarations.
            if let Decl::Struct(_) = decls[i] {
                let mut j = i;
                let mut width = 0;
                while j < decls.len() {
                    match &decls[j] {
                        Decl::Struct(d) if self.contiguous(i, j, decls) => {
                            width = width.max(d.name.len());
                            j += 1;
                        }
                        _ => break,
                    }
                }
                for d in &decls[i..j] {
                    if let Decl::Struct(d) = d {
                        self.print_struct_decl(d, width);
                    }
                }
                i = j;
                continue;
            }
            match &decls[i] {
                Decl::Struct(_) => unreachable!(),
                Decl::Use(d) => {
                    self.flush_comments_before(d.span.start, 0);
                    self.blank_line_if_gap(self.lines.line(d.span.start));
                    let head = format!("use {}", d.path.join("/"));
                    match &d.names {
                        None => self.out.push_str(&head),
                        Some(names) => {
                            let inner: Vec<String> =
                                names.iter().map(|(n, _)| n.clone()).collect();
                            let inline = format!("{head} {{ {} }}", inner.join(", "));
                            if inline.len() <= MAX_WIDTH {
                                self.out.push_str(&inline);
                            } else {
                                // Fill-wrap the name list at the line width.
                                self.out.push_str(&head);
                                self.out.push_str(" {\n");
                                let mut line = String::new();
                                for (i, name) in inner.iter().enumerate() {
                                    let piece = if i + 1 == inner.len() {
                                        format!("{name},")
                                    } else {
                                        format!("{name}, ")
                                    };
                                    if !line.is_empty()
                                        && INDENT.len() + line.len() + piece.len() > MAX_WIDTH
                                    {
                                        self.out.push_str(INDENT);
                                        self.out.push_str(line.trim_end());
                                        self.out.push('\n');
                                        line.clear();
                                    }
                                    line.push_str(&piece);
                                }
                                if !line.is_empty() {
                                    self.out.push_str(INDENT);
                                    self.out.push_str(line.trim_end());
                                    self.out.push('\n');
                                }
                                self.out.push('}');
                            }
                        }
                    }
                    self.attach_trailing_comment(d.span.end);
                    self.out.push('\n');
                    self.prev_end_line = Some(self.lines.line(d.span.end));
                }
                Decl::Enum(d) => self.print_enum_decl(d),
                Decl::Service(d) => self.print_service(d),
                Decl::Impl(d) => self.print_impl(d),
                Decl::Func(d) => self.print_func(d, 0),
                Decl::TypeAlias(d) => {
                    self.flush_comments_before(d.span.start, 0);
                    self.blank_line_if_gap(self.lines.line(d.span.start));
                    self.out.push_str(pub_prefix(d.is_pub));
                    self.out.push_str(&format!("type {} = {}", d.name, render_type(&d.ty)));
                    self.attach_trailing_comment(d.span.end);
                    self.out.push('\n');
                    self.prev_end_line = Some(self.lines.line(d.span.end));
                }
                Decl::Const(d) => {
                    self.flush_comments_before(d.span.start, 0);
                    self.blank_line_if_gap(self.lines.line(d.span.start));
                    self.out.push_str(pub_prefix(d.is_pub));
                    if let Some(TypeExpr::Name(ty, _)) = &d.ty {
                        self.out.push_str(ty);
                        self.out.push(' ');
                    }
                    self.out.push_str(&d.name);
                    self.out.push_str(" = ");
                    let rendered = self.render_expr(&d.value, 0);
                    self.out.push_str(&rendered);
                    self.attach_trailing_comment(d.span.end);
                    self.out.push('\n');
                    self.prev_end_line = Some(self.lines.line(d.span.end));
                }
            }
            i += 1;
        }
        self.flush_comments_before(u32::MAX, 0);
        // Exactly one trailing newline.
        while self.out.ends_with("\n\n") {
            self.out.pop();
        }
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        }
    }

    /// True when decls i..=j form a run with no comments or blank lines
    /// between them — only such runs get their `=` aligned.
    fn contiguous(&self, i: usize, j: usize, decls: &[Decl]) -> bool {
        if i == j {
            return true;
        }
        let prev_end = decl_span(&decls[j - 1]).end;
        let start = decl_span(&decls[j]).start;
        if self.lines.line(start) > self.lines.line(prev_end) + 1 {
            return false;
        }
        !self.comments.iter().any(|(s, _)| s.start >= prev_end && s.start < start)
    }

    fn print_struct_decl(&mut self, d: &StructDecl, name_width: usize) {
        self.flush_comments_before(d.span.start, 0);
        self.blank_line_if_gap(self.lines.line(d.span.start));
        let body = render_field_block(&d.fields);
        let padded = format!("{:<width$}", d.name, width = name_width);
        self.out.push_str(&format!("{}struct {padded} = {body}", pub_prefix(d.is_pub)));
        self.attach_trailing_comment(d.span.end);
        self.out.push('\n');
        self.prev_end_line = Some(self.lines.line(d.span.end));
    }

    fn print_enum_decl(&mut self, d: &EnumDecl) {
        self.flush_comments_before(d.span.start, 0);
        self.blank_line_if_gap(self.lines.line(d.span.start));
        let variants: Vec<String> = d
            .variants
            .iter()
            .map(|v| {
                if v.fields.is_empty() {
                    v.name.clone()
                } else {
                    format!("{} {}", v.name, render_field_block(&v.fields))
                }
            })
            .collect();
        let inline = format!("{}enum {} = {}", pub_prefix(d.is_pub), d.name, variants.join(" | "));
        if inline.len() <= MAX_WIDTH {
            self.out.push_str(&inline);
        } else {
            // One variant per line, `|` leading.
            self.out.push_str(&format!("{}enum {} =\n", pub_prefix(d.is_pub), d.name));
            for (i, v) in variants.iter().enumerate() {
                self.push_indent(1);
                if i > 0 {
                    self.out.push_str("| ");
                }
                self.out.push_str(v);
                if i + 1 < variants.len() {
                    self.out.push('\n');
                }
            }
        }
        self.attach_trailing_comment(d.span.end);
        self.out.push('\n');
        self.prev_end_line = Some(self.lines.line(d.span.end));
    }

    fn print_service(&mut self, d: &ServiceDecl) {
        self.flush_comments_before(d.span.start, 0);
        self.blank_line_if_gap(self.lines.line(d.span.start));
        let shared = if d.is_shared { "shared " } else { "" };
        self.out.push_str(&format!("{}{shared}service {} {{\n", pub_prefix(d.is_pub), d.name));
        self.prev_end_line = Some(self.lines.line(d.span.start));
        // Value members and methods interleave by source position.
        enum Member<'a> {
            Value(&'a Field),
            Method(&'a MethodSig),
        }
        let mut members: Vec<(u32, Member)> = d
            .values
            .iter()
            .map(|v| (v.span.start, Member::Value(v)))
            .chain(d.methods.iter().map(|m| (m.span.start, Member::Method(m))))
            .collect();
        members.sort_by_key(|(at, _)| *at);
        for (_, member) in members {
            match member {
                Member::Value(v) => {
                    self.flush_comments_before(v.span.start, 1);
                    self.blank_line_if_gap(self.lines.line(v.span.start));
                    self.push_indent(1);
                    if let Some(ty) = &v.ty {
                        self.out.push_str(&format!("{} {}", render_type(ty), v.name));
                    } else {
                        self.out.push_str(&v.name);
                    }
                    self.attach_trailing_comment(v.span.end);
                    self.out.push('\n');
                    self.prev_end_line = Some(self.lines.line(v.span.end));
                }
                Member::Method(m) => {
                    self.flush_comments_before(m.span.start, 1);
                    self.blank_line_if_gap(self.lines.line(m.span.start));
                    self.push_indent(1);
                    self.out.push_str(&format!("{} :: {}", m.name, render_sig(&m.sig)));
                    self.attach_trailing_comment(m.span.end);
                    self.out.push('\n');
                    self.prev_end_line = Some(self.lines.line(m.span.end));
                }
            }
        }
        self.flush_comments_before(d.span.end, 1);
        self.out.push_str("}\n");
        self.prev_end_line = Some(self.lines.line(d.span.end));
    }

    fn print_impl(&mut self, d: &ImplDecl) {
        self.flush_comments_before(d.span.start, 0);
        self.blank_line_if_gap(self.lines.line(d.span.start));
        self.out.push_str(&format!("{}{} :: {} {{\n", pub_prefix(d.is_pub), d.name, d.service));
        self.prev_end_line = Some(self.lines.line(d.span.start));
        for p in &d.params {
            self.flush_comments_before(p.span.start, 1);
            self.blank_line_if_gap(self.lines.line(p.span.start));
            self.push_indent(1);
            if let Some(ty) = &p.ty {
                self.out.push_str(&format!("{} {}", render_type(ty), p.name));
            } else {
                self.out.push_str(&p.name);
            }
            self.attach_trailing_comment(p.span.end);
            self.out.push('\n');
            self.prev_end_line = Some(self.lines.line(p.span.end));
        }
        for (name, span, value) in &d.fields {
            self.flush_comments_before(span.start, 1);
            self.blank_line_if_gap(self.lines.line(span.start));
            self.push_indent(1);
            let rendered = self.render_expr(value, 1);
            self.out.push_str(&format!("{name} = {rendered}"));
            self.attach_trailing_comment(value.span.end);
            self.out.push('\n');
            self.prev_end_line = Some(self.lines.line(value.span.end));
        }
        for method in &d.methods {
            self.print_func(method, 1);
        }
        self.flush_comments_before(d.span.end, 1);
        self.out.push_str("}\n");
        self.prev_end_line = Some(self.lines.line(d.span.end));
    }

    fn print_func(&mut self, d: &FuncDecl, indent: usize) {
        self.flush_comments_before(d.span.start, indent);
        self.blank_line_if_gap(self.lines.line(d.span.start));
        self.push_indent(indent);
        self.out.push_str(&format!("{}{} :: {} ", pub_prefix(d.is_pub), d.name, render_sig(&d.sig)));
        self.print_block(&d.body, indent);
        self.out.push('\n');
        self.prev_end_line = Some(self.lines.line(d.span.end));
    }

    /// Prints `{ ... }` starting at the current position; no trailing newline.
    fn print_block(&mut self, block: &Block, indent: usize) {
        if block.stmts.is_empty() {
            self.out.push_str("{}");
            return;
        }
        self.out.push_str("{\n");
        self.prev_end_line = Some(self.lines.line(block.span.start));
        self.print_stmts(&block.stmts, indent + 1);
        self.flush_comments_before(block.span.end, indent + 1);
        self.push_indent(indent);
        self.out.push('}');
    }

    fn print_stmts(&mut self, stmts: &[Stmt], indent: usize) {
        for stmt in stmts {
            // Braceless `provide` prints as a header line; its body is the
            // rest of the enclosing block, at the same indent.
            if let Stmt::Expr(expr) = stmt {
                if let ExprKind::Provide { impls, body, inline: true } = &expr.kind {
                    self.flush_comments_before(expr.span.start, indent);
                    self.blank_line_if_gap(self.lines.line(expr.span.start));
                    self.push_indent(indent);
                    let items = self.render_provide_items(impls, indent);
                    self.out.push_str(&format!("provide {items}"));
                    let header_end = provide_items_end(impls).unwrap_or(expr.span.start);
                    self.attach_trailing_comment(header_end);
                    self.out.push('\n');
                    self.prev_end_line = Some(self.lines.line(header_end));
                    self.print_stmts(&body.stmts, indent);
                    continue;
                }
            }
            let span = stmt_span(stmt);
            self.flush_comments_before(span.start, indent);
            self.blank_line_if_gap(self.lines.line(span.start));
            self.push_indent(indent);
            match stmt {
                Stmt::Expr(expr) => {
                    let rendered = self.render_expr(expr, indent);
                    self.out.push_str(&rendered);
                }
                Stmt::Bind { ty, name, value, .. } => {
                    let prefix = match ty {
                        Some(t) => format!("{} {name} = ", render_type(t)),
                        None => format!("{name} = "),
                    };
                    self.out.push_str(&prefix);
                    let rendered = self.render_expr(value, indent);
                    self.out.push_str(&rendered);
                }
                Stmt::Acquire { service, name, .. } => {
                    self.out.push_str(&format!("{service} {name}"));
                }
            }
            self.attach_trailing_comment(span.end);
            self.out.push('\n');
            self.prev_end_line = Some(self.lines.line(span.end));
        }
    }

    fn render_provide_items(&mut self, impls: &[ProvideItem], indent: usize) -> String {
        let rendered: Vec<String> = impls
            .iter()
            .map(|item| match &item.args {
                None => item.name.clone(),
                Some(args) => {
                    let inner: Vec<String> =
                        args.iter().map(|a| self.render_expr(a, indent)).collect();
                    format!("{}({})", item.name, inner.join(", "))
                }
            })
            .collect();
        rendered.join(", ")
    }

    // ---- expressions --------------------------------------------------------

    /// Render an expression assuming it starts mid-line at `indent` depth.
    fn render_expr(&mut self, expr: &Expr, indent: usize) -> String {
        match &expr.kind {
            ExprKind::Int(n) => n.to_string(),
            ExprKind::Float(f) => render_float(*f),
            ExprKind::Bool(b) => b.to_string(),
            ExprKind::Str(pieces, triple) => self.render_str(pieces, indent, *triple),
            ExprKind::Var(name) => name.clone(),
            ExprKind::Tuple(items) => {
                let inner: Vec<String> = items.iter().map(|e| self.render_expr(e, indent)).collect();
                format!("({})", inner.join(", "))
            }
            ExprKind::TupleIndex { recv, index, .. } => {
                let mut recv_str = self.render_expr(recv, indent);
                if postfix_operand_needs_parens(recv) {
                    recv_str = format!("({recv_str})");
                }
                format!("{recv_str}.{index}")
            }
            ExprKind::RecordUpdate { name, base, fields, .. } => {
                let mut parts = Vec::new();
                if let Some(base) = base {
                    parts.push(format!("..{}", self.render_expr(base, indent)));
                }
                for (fname, _, value) in fields {
                    parts.push(format!("{fname}: {}", self.render_expr(value, indent)));
                }
                format!("{name} {{ {} }}", parts.join(", "))
            }
            ExprKind::List(items) => {
                let inner: Vec<String> = items.iter().map(|e| self.render_expr(e, indent)).collect();
                format!("[{}]", inner.join(", "))
            }
            ExprKind::Call { callee, args } => {
                let mut callee_str = self.render_expr(callee, indent);
                if postfix_operand_needs_parens(callee) {
                    callee_str = format!("({callee_str})");
                }
                let args_str: Vec<String> =
                    args.iter().map(|a| self.render_expr(a, indent)).collect();
                format!("{callee_str}({})", args_str.join(", "))
            }
            ExprKind::Method { recv, name, args, .. } => {
                let mut recv_str = self.render_expr(recv, indent);
                if postfix_operand_needs_parens(recv) {
                    recv_str = format!("({recv_str})");
                }
                let args_str: Vec<String> =
                    args.iter().map(|a| self.render_expr(a, indent)).collect();
                format!("{recv_str}.{name}({})", args_str.join(", "))
            }
            ExprKind::Field { recv, name, .. } => {
                let mut recv_str = self.render_expr(recv, indent);
                if postfix_operand_needs_parens(recv) {
                    recv_str = format!("({recv_str})");
                }
                format!("{recv_str}.{name}")
            }
            ExprKind::Binary { op, lhs, rhs } => {
                // Long `+` chains (string building, e.g. shader sources) break
                // with one operand per line; the parser continues `+` lines.
                // Decide on the whole flattened chain so ancestors and inner
                // nodes agree.
                if *op == BinOp::Add {
                    let mut operands = Vec::new();
                    flatten_add_chain(expr, &mut operands);
                    let rendered: Vec<String> = operands
                        .iter()
                        .map(|e| self.render_binary_operand(e, BinOp::Add, false, indent))
                        .collect();
                    let inline = rendered.join(" + ");
                    if operands.len() > 2
                        && (inline.contains('\n') || indent * 4 + inline.len() > MAX_WIDTH)
                    {
                        return rendered.join(&format!("\n{}+ ", indent_str(indent + 1)));
                    }
                    return inline;
                }
                let l = self.render_binary_operand(lhs, *op, true, indent);
                let r = self.render_binary_operand(rhs, *op, false, indent);
                format!("{l} {} {r}", op.symbol())
            }
            ExprKind::Unary { op, expr: inner } => {
                let symbol = match op {
                    UnOp::Neg => "-",
                    UnOp::Not => "!",
                };
                let rendered = self.render_expr(inner, indent);
                if matches!(inner.kind, ExprKind::Binary { .. } | ExprKind::Pipe { .. }) {
                    format!("{symbol}({rendered})")
                } else {
                    format!("{symbol}{rendered}")
                }
            }
            ExprKind::Pipe { .. } => self.render_pipe(expr, indent),
            ExprKind::Match { scrutinee, arms } => {
                let scrut = self.render_expr(scrutinee, indent);
                let mut out = format!("match {scrut} {{\n");
                self.render_arms(arms, indent + 1, &mut out);
                out.push_str(&indent_str(indent));
                out.push('}');
                out
            }
            ExprKind::Fail { error } => {
                format!("fail {}", self.render_expr(error, indent))
            }
            ExprKind::Provide { impls, body, .. } => {
                let items = self.render_provide_items(impls, indent);
                let mut out = format!("provide {items} ");
                out.push_str(&self.render_block_inline(body, indent));
                out
            }
            ExprKind::If { cond, then_block, else_branch } => {
                let cond_str = self.render_expr(cond, indent);
                let mut out = format!("if {cond_str} ");
                out.push_str(&self.render_block_inline(then_block, indent));
                if let Some(else_expr) = else_branch {
                    out.push_str(" else ");
                    match &else_expr.kind {
                        ExprKind::Block(block) => {
                            out.push_str(&self.render_block_inline(block, indent));
                        }
                        _ => out.push_str(&self.render_expr(else_expr, indent)),
                    }
                }
                out
            }
            ExprKind::Block(block) => self.render_block_inline(block, indent),
            ExprKind::Lambda { params, body } => {
                let params_str: Vec<String> = params.iter().map(render_param).collect();
                let body_str = self.render_expr(body, indent);
                format!("({}) -> {body_str}", params_str.join(", "))
            }
        }
    }

    fn render_binary_operand(
        &mut self,
        operand: &Expr,
        parent: BinOp,
        is_lhs: bool,
        indent: usize,
    ) -> String {
        let rendered = self.render_expr(operand, indent);
        let needs_parens = match &operand.kind {
            ExprKind::Binary { op, .. } => {
                let (po, oo) = (precedence(parent), precedence(*op));
                oo < po || (oo == po && !is_lhs)
            }
            ExprKind::Pipe { .. } | ExprKind::Lambda { .. } => true,
            _ => false,
        };
        if needs_parens {
            format!("({rendered})")
        } else {
            rendered
        }
    }

    fn render_str(&mut self, pieces: &[StrPiece], indent: usize, triple: bool) -> String {
        if triple {
            return self.render_triple_str(pieces, indent);
        }
        let mut out = String::from("\"");
        for piece in pieces {
            match piece {
                StrPiece::Text(text) => {
                    for c in text.chars() {
                        match c {
                            '"' => out.push_str("\\\""),
                            '\\' => out.push_str("\\\\"),
                            '\n' => out.push_str("\\n"),
                            '\t' => out.push_str("\\t"),
                            '\r' => out.push_str("\\r"),
                            '\x1b' => out.push_str("\\e"),
                            '$' => out.push_str("\\$"),
                            c => out.push(c),
                        }
                    }
                }
                StrPiece::Expr(e) => {
                    out.push_str("${");
                    out.push_str(&self.render_expr(e, indent));
                    out.push('}');
                }
            }
        }
        out.push('"');
        out
    }

    /// `"""` form: real newlines, bare quotes; content and the closing
    /// delimiter sit at the opener's line indentation, so both delimiter
    /// lines align (the lexer strips that indent back out — round-trips).
    fn render_triple_str(&mut self, pieces: &[StrPiece], indent: usize) -> String {
        let pad = "    ".repeat(indent);
        let mut body = String::new();
        // Three raw quotes in content would close the literal early when
        // re-lexed; escape every third consecutive quote.
        let mut quotes = 0u32;
        for piece in pieces {
            match piece {
                StrPiece::Text(text) => {
                    for c in text.chars() {
                        if c == '"' {
                            quotes += 1;
                            if quotes == 3 {
                                body.push_str("\\\"");
                                quotes = 0;
                            } else {
                                body.push('"');
                            }
                            continue;
                        }
                        quotes = 0;
                        match c {
                            '\\' => body.push_str("\\\\"),
                            '\r' => body.push_str("\\r"),
                            '$' => body.push_str("\\$"),
                            '\0' => body.push_str("\\0"),
                            c => body.push(c),
                        }
                    }
                }
                StrPiece::Expr(e) => {
                    quotes = 0;
                    body.push_str("${");
                    body.push_str(&self.render_expr(e, indent));
                    body.push('}');
                }
            }
        }
        let mut out = String::from("\"\"\"\n");
        for line in body.split('\n') {
            if line.is_empty() {
                out.push('\n');
            } else {
                out.push_str(&pad);
                out.push_str(line);
                out.push('\n');
            }
        }
        out.push_str(&pad);
        out.push_str("\"\"\"");
        out
    }

    fn render_block_inline(&mut self, block: &Block, indent: usize) -> String {
        // Reuse print_block by writing into a scratch buffer.
        let saved = std::mem::take(&mut self.out);
        self.print_block(block, indent);
        std::mem::replace(&mut self.out, saved)
    }

    fn render_pipe(&mut self, expr: &Expr, indent: usize) -> String {
        // Flatten the chain.
        let mut targets: Vec<&PipeTarget> = Vec::new();
        let mut base = expr;
        while let ExprKind::Pipe { lhs, target } = &base.kind {
            targets.push(target);
            base = lhs;
        }
        targets.reverse();

        let base_str = self.render_expr(base, indent);
        let rendered_targets: Vec<String> =
            targets.iter().map(|t| self.render_pipe_target(t, indent + 1)).collect();

        // Inline when short and simple.
        let inline = format!("{base_str} |> {}", rendered_targets.join(" |> "));
        let simple = !inline.contains('\n') && targets.len() <= 2;
        if simple && indent * 4 + inline.len() <= MAX_WIDTH {
            return inline;
        }
        let mut out = base_str;
        for t in &rendered_targets {
            out.push('\n');
            out.push_str(&indent_str(indent + 1));
            out.push_str("|> ");
            out.push_str(t);
        }
        out
    }

    fn render_pipe_target(&mut self, target: &PipeTarget, indent: usize) -> String {
        match target {
            PipeTarget::Call { callee, args } => {
                let callee_str = self.render_expr(callee, indent);
                match args {
                    None => callee_str,
                    Some(args) => {
                        let args_str: Vec<String> =
                            args.iter().map(|a| self.render_expr(a, indent)).collect();
                        format!("{callee_str}({})", args_str.join(", "))
                    }
                }
            }
            PipeTarget::Catch { arms, .. } => {
                // A single simple arm inlines: `catch { CacheMiss -> None }`
                if arms.len() == 1 {
                    let arm = &arms[0];
                    let pat = render_pattern(&arm.pattern);
                    let body = self.render_expr(&arm.body, indent);
                    let inline = format!("catch {{ {pat} -> {body} }}");
                    if !inline.contains('\n') && inline.len() + indent * 4 <= MAX_WIDTH {
                        return inline;
                    }
                }
                let mut out = String::from("catch {\n");
                self.render_arms(arms, indent + 1, &mut out);
                out.push_str(&indent_str(indent));
                out.push('}');
                out
            }
        }
    }

    fn render_arms(&mut self, arms: &[Arm], indent: usize, out: &mut String) {
        let patterns: Vec<String> = arms.iter().map(|a| render_pattern(&a.pattern)).collect();
        let width = patterns.iter().map(|p| p.len()).max().unwrap_or(0);
        for (arm, pat) in arms.iter().zip(patterns) {
            out.push_str(&indent_str(indent));
            let body = self.render_expr(&arm.body, indent);
            if body.contains('\n') {
                // Don't pad before a multiline body; keep the arrow tight.
                out.push_str(&format!("{pat} -> {body}\n"));
            } else {
                out.push_str(&format!("{pat:<width$} -> {body}\n"));
            }
        }
    }
}

// ---- pure render helpers -----------------------------------------------------

/// A postfix form (`f(x)`, `v.field`, `v.0`) binds tighter than pipes,
/// operators, and lambdas — those operands must keep their parentheses
/// or the printed program changes meaning: `(f |> mw)(x)`.
fn postfix_operand_needs_parens(e: &Expr) -> bool {
    matches!(
        e.kind,
        ExprKind::Pipe { .. }
            | ExprKind::Binary { .. }
            | ExprKind::Unary { .. }
            | ExprKind::Lambda { .. }
            | ExprKind::Match { .. }
            | ExprKind::If { .. }
            | ExprKind::Fail { .. }
    )
}

fn pub_prefix(is_pub: bool) -> &'static str {
    if is_pub { "pub " } else { "" }
}

fn decl_span(decl: &Decl) -> Span {
    match decl {
        Decl::Use(d) => d.span,
        Decl::Struct(d) => d.span,
        Decl::Enum(d) => d.span,
        Decl::Service(d) => d.span,
        Decl::Impl(d) => d.span,
        Decl::Func(d) => d.span,
        Decl::Const(d) => d.span,
        Decl::TypeAlias(d) => d.span,
    }
}

fn render_field_block(fields: &[Field]) -> String {
    if fields.is_empty() {
        return "{}".to_string();
    }
    let rendered: Vec<String> = fields
        .iter()
        .map(|f| match &f.ty {
            Some(ty) => format!("{} {}", render_type(ty), f.name),
            None => f.name.clone(),
        })
        .collect();
    format!("{{ {} }}", rendered.join(", "))
}

/// End offset of a provide item list (for trailing-comment attachment).
fn provide_items_end(impls: &[ProvideItem]) -> Option<u32> {
    let last = impls.last()?;
    Some(match &last.args {
        Some(args) => args.last().map(|a| a.span.end).unwrap_or(last.name_span.end),
        None => last.name_span.end,
    })
}

fn stmt_span(stmt: &Stmt) -> Span {
    match stmt {
        Stmt::Expr(e) => e.span,
        Stmt::Bind { name_span, value, .. } => name_span.to(value.span),
        Stmt::Acquire { service_span, name_span, .. } => service_span.to(*name_span),
    }
}

fn render_sig(sig: &Sig) -> String {
    let params: Vec<String> = sig.params.iter().map(render_param).collect();
    let mut out = format!("({})", params.join(", "));
    if let Some(ret) = &sig.ret {
        out.push_str(&format!(" -> {}", render_type(ret)));
    }
    if let Some(errors) = &sig.errors {
        let names: Vec<String> = errors.iter().map(|(n, _)| n.clone()).collect();
        out.push_str(&format!(" ! {}", names.join(", ")));
    }
    if let Some(uses) = &sig.uses {
        let names: Vec<String> = uses.iter().map(|(n, _)| n.clone()).collect();
        out.push_str(&format!(" uses {}", names.join(", ")));
    }
    out
}

fn render_param(param: &Param) -> String {
    let mut out = String::new();
    if param.lazy {
        out.push_str("lazy ");
    }
    if let Some(ty) = &param.ty {
        out.push_str(&render_type(ty));
        out.push(' ');
    }
    out.push_str(&param.name);
    out
}

fn render_type(ty: &TypeExpr) -> String {
    match ty {
        TypeExpr::Name(name, _) => name.clone(),
        TypeExpr::Option(inner, _) => {
            if matches!(**inner, TypeExpr::Func { .. }) {
                format!("({})?", render_type(inner))
            } else {
                format!("{}?", render_type(inner))
            }
        }
        TypeExpr::List(inner, _) => format!("[{}]", render_type(inner)),
        TypeExpr::Apply { name, args, row, .. } => {
            let args: Vec<String> = args.iter().map(render_type).collect();
            if row.is_empty() {
                format!("{name}<{}>", args.join(", "))
            } else {
                let row: Vec<&str> = row.iter().map(|(n, _)| n.as_str()).collect();
                format!("{name}<{} ! {}>", args.join(", "), row.join(", "))
            }
        }
        TypeExpr::Tuple(items, _) => {
            let inner: Vec<String> = items.iter().map(render_type).collect();
            format!("({})", inner.join(", "))
        }
        TypeExpr::Func { params, ret, errors, caps, .. } => {
            let inner: Vec<String> = params.iter().map(render_type).collect();
            let mut out = format!("({}) -> {}", inner.join(", "), render_type(ret));
            if !errors.is_empty() {
                let names: Vec<String> = errors.iter().map(|(n, _)| n.clone()).collect();
                out.push_str(&format!(" ! {}", names.join(", ")));
            }
            if !caps.is_empty() {
                let names: Vec<String> = caps.iter().map(|(n, _)| n.clone()).collect();
                out.push_str(&format!(" uses {}", names.join(", ")));
            }
            out
        }
    }
}

fn render_pattern(pat: &Pattern) -> String {
    match &pat.kind {
        PatternKind::Wildcard => "_".to_string(),
        PatternKind::Bind(name) => name.clone(),
        PatternKind::Int(n) => n.to_string(),
        PatternKind::Str(s) => format!("{s:?}"),
        PatternKind::StrTemplate(pieces) => {
            let mut out = String::from("\"");
            for piece in pieces {
                match piece {
                    StrPatPiece::Text(t) => {
                        for c in t.chars() {
                            match c {
                                '"' => out.push_str("\\\""),
                                '\\' => out.push_str("\\\\"),
                                '\n' => out.push_str("\\n"),
                                '\t' => out.push_str("\\t"),
                                '$' => out.push_str("\\$"),
                                c => out.push(c),
                            }
                        }
                    }
                    StrPatPiece::Hole { ty, name, .. } => {
                        out.push_str("${");
                        if let Some(ty) = ty {
                            out.push_str(ty);
                            out.push(' ');
                        }
                        out.push_str(name);
                        out.push('}');
                    }
                }
            }
            out.push('"');
            out
        }
        PatternKind::Bool(b) => b.to_string(),
        PatternKind::TypedBind { ty, name, .. } => format!("{ty} {name}"),
        PatternKind::Tuple(pats) => {
            let inner: Vec<String> = pats.iter().map(render_pattern).collect();
            format!("({})", inner.join(", "))
        }
        PatternKind::Ctor { name, args, .. } => match args {
            CtorPatArgs::None => name.clone(),
            CtorPatArgs::Positional(pats) => {
                let inner: Vec<String> = pats.iter().map(render_pattern).collect();
                format!("{name}({})", inner.join(", "))
            }
            CtorPatArgs::Fields(fields) => {
                let inner: Vec<String> = fields.iter().map(|(n, _)| n.clone()).collect();
                format!("{name} {{ {} }}", inner.join(", "))
            }
        },
    }
}

fn render_float(f: f64) -> String {
    if f.fract() == 0.0 && f.is_finite() {
        format!("{f:.1}")
    } else {
        f.to_string()
    }
}

fn precedence(op: BinOp) -> u8 {
    match op {
        BinOp::Or => 1,
        BinOp::And => 2,
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => 3,
        BinOp::Add | BinOp::Sub => 4,
        BinOp::Mul | BinOp::Div | BinOp::Mod => 5,
    }
}

fn indent_str(indent: usize) -> String {
    INDENT.repeat(indent)
}

/// Flatten a left-leaning `a + b + c` tree into its operands, in order.
fn flatten_add_chain<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    if let ExprKind::Binary { op: BinOp::Add, lhs, rhs } = &expr.kind {
        flatten_add_chain(lhs, out);
        out.push(rhs);
    } else {
        out.push(expr);
    }
}
