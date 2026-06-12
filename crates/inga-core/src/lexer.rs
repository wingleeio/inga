//! Hand-written lexer. Produces a flat token stream that includes newline and
//! comment trivia; `${...}` string interpolation holes are lexed recursively
//! into nested token streams.

use crate::diag::Diagnostic;
use crate::span::Span;
use crate::token::{StrPart, Token, TokenKind};

pub fn lex(src: &str, diagnostics: &mut Vec<Diagnostic>) -> Vec<Token> {
    lex_from(src, 0, diagnostics)
}

/// Lex with all spans offset by `base` — multi-module programs share one
/// global span space (each module gets a disjoint range).
pub fn lex_from(src: &str, base: u32, diagnostics: &mut Vec<Diagnostic>) -> Vec<Token> {
    let mut lexer = Lexer { src: src.as_bytes(), pos: 0, base, diagnostics };
    let mut tokens = Vec::new();
    loop {
        let token = lexer.next_token();
        let done = token.kind == TokenKind::Eof;
        tokens.push(token);
        if done {
            break;
        }
    }
    tokens
}

struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    base: u32,
    diagnostics: &'a mut Vec<Diagnostic>,
}

impl<'a> Lexer<'a> {
    fn peek(&self) -> u8 {
        self.src.get(self.pos).copied().unwrap_or(0)
    }

    fn peek2(&self) -> u8 {
        self.src.get(self.pos + 1).copied().unwrap_or(0)
    }

    fn bump(&mut self) -> u8 {
        let b = self.peek();
        self.pos += 1;
        b
    }

    fn span_from(&self, start: usize) -> Span {
        Span::new(self.base + start as u32, self.base + self.pos as u32)
    }

    fn error(&mut self, span: Span, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic::error(span, message));
    }

    fn next_token(&mut self) -> Token {
        // Skip horizontal whitespace (newlines are tokens).
        while matches!(self.peek(), b' ' | b'\t' | b'\r') {
            self.pos += 1;
        }
        let start = self.pos;
        if self.pos >= self.src.len() {
            return Token::new(TokenKind::Eof, self.span_from(start));
        }
        let b = self.bump();
        let kind = match b {
            b'\n' => TokenKind::Newline,
            b'/' if self.peek() == b'/' => {
                while self.pos < self.src.len() && self.peek() != b'\n' {
                    self.pos += 1;
                }
                let text = String::from_utf8_lossy(&self.src[start..self.pos]).into_owned();
                TokenKind::Comment(text)
            }
            b'/' if self.peek() == b'*' => {
                self.pos += 1;
                let mut depth = 1;
                while self.pos < self.src.len() && depth > 0 {
                    if self.peek() == b'/' && self.peek2() == b'*' {
                        depth += 1;
                        self.pos += 2;
                    } else if self.peek() == b'*' && self.peek2() == b'/' {
                        depth -= 1;
                        self.pos += 2;
                    } else {
                        self.pos += 1;
                    }
                }
                if depth > 0 {
                    let span = self.span_from(start);
                    self.error(span, "unterminated block comment");
                }
                let text = String::from_utf8_lossy(&self.src[start..self.pos]).into_owned();
                TokenKind::Comment(text)
            }
            b'"' => {
                if self.peek() == b'"' && self.peek_at(1) == b'"' {
                    self.pos += 2;
                    self.lex_triple_string(start)
                } else {
                    self.lex_string(start)
                }
            }
            b'0'..=b'9' => self.lex_number(start),
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.lex_ident(start),
            b':' if self.peek() == b':' => {
                self.pos += 1;
                TokenKind::ColonColon
            }
            b':' => TokenKind::Colon,
            b'-' if self.peek() == b'>' => {
                self.pos += 1;
                TokenKind::Arrow
            }
            b'-' => TokenKind::Minus,
            b'|' if self.peek() == b'>' => {
                self.pos += 1;
                TokenKind::PipeOp
            }
            b'|' if self.peek() == b'|' => {
                self.pos += 1;
                TokenKind::OrOr
            }
            b'|' => TokenKind::Bar,
            b'&' if self.peek() == b'&' => {
                self.pos += 1;
                TokenKind::AndAnd
            }
            b'=' if self.peek() == b'=' => {
                self.pos += 1;
                TokenKind::EqEq
            }
            b'=' => TokenKind::Eq,
            b'!' if self.peek() == b'=' => {
                self.pos += 1;
                TokenKind::NotEq
            }
            b'!' => TokenKind::Bang,
            b'?' => TokenKind::Question,
            b'<' if self.peek() == b'=' => {
                self.pos += 1;
                TokenKind::Le
            }
            b'<' => TokenKind::Lt,
            b'>' if self.peek() == b'=' => {
                self.pos += 1;
                TokenKind::Ge
            }
            b'>' => TokenKind::Gt,
            b'+' => TokenKind::Plus,
            b'*' => TokenKind::Star,
            b'/' => TokenKind::Slash,
            b'%' => TokenKind::Percent,
            b'.' => TokenKind::Dot,
            b',' => TokenKind::Comma,
            b';' => TokenKind::Semi,
            b'(' => TokenKind::LParen,
            b')' => TokenKind::RParen,
            b'{' => TokenKind::LBrace,
            b'}' => TokenKind::RBrace,
            b'[' => TokenKind::LBracket,
            b']' => TokenKind::RBracket,
            other => {
                // Consume the rest of a multi-byte UTF-8 sequence so we report
                // one error per character, not per byte.
                while self.pos < self.src.len() && (self.peek() & 0xC0) == 0x80 {
                    self.pos += 1;
                }
                let span = self.span_from(start);
                let ch = String::from_utf8_lossy(&self.src[start..self.pos]).into_owned();
                self.error(
                    span,
                    if other.is_ascii() {
                        format!("unexpected character `{}`", other as char)
                    } else {
                        format!("unexpected character `{ch}`")
                    },
                );
                return self.next_token();
            }
        };
        Token::new(kind, self.span_from(start))
    }

    fn lex_ident(&mut self, start: usize) -> TokenKind {
        while matches!(self.peek(), b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_') {
            self.pos += 1;
        }
        let text = std::str::from_utf8(&self.src[start..self.pos]).unwrap();
        TokenKind::keyword(text).unwrap_or_else(|| TokenKind::Ident(text.to_string()))
    }

    fn lex_number(&mut self, start: usize) -> TokenKind {
        while self.peek().is_ascii_digit() || self.peek() == b'_' {
            self.pos += 1;
        }
        // Only a float if a digit follows the dot — `100.millis` stays an Int
        // followed by `.millis`.
        let mut is_float = false;
        if self.peek() == b'.' && self.peek2().is_ascii_digit() {
            is_float = true;
            self.pos += 1;
            while self.peek().is_ascii_digit() || self.peek() == b'_' {
                self.pos += 1;
            }
        }
        let text: String = std::str::from_utf8(&self.src[start..self.pos])
            .unwrap()
            .chars()
            .filter(|&c| c != '_')
            .collect();
        if is_float {
            TokenKind::Float(text.parse().unwrap_or(0.0))
        } else {
            match text.parse() {
                Ok(n) => TokenKind::Int(n),
                Err(_) => {
                    let span = self.span_from(start);
                    self.error(span, "integer literal is too large");
                    TokenKind::Int(0)
                }
            }
        }
    }

    fn lex_string(&mut self, start: usize) -> TokenKind {
        let mut parts: Vec<StrPart> = Vec::new();
        let mut text = String::new();
        loop {
            if self.pos >= self.src.len() || self.peek() == b'\n' {
                let span = self.span_from(start);
                self.error(span, "unterminated string literal");
                break;
            }
            match self.bump() {
                b'"' => break,
                b'\\' => {
                    let esc_start = self.pos - 1;
                    match self.bump() {
                        b'n' => text.push('\n'),
                        b't' => text.push('\t'),
                        b'r' => text.push('\r'),
                        b'\\' => text.push('\\'),
                        b'"' => text.push('"'),
                        b'$' => text.push('$'),
                        b'0' => text.push('\0'),
                        b'e' => text.push('\x1b'),
                        other => {
                            let span = self.span_from(esc_start);
                            self.error(
                                span,
                                format!("unknown escape sequence `\\{}`", other as char),
                            );
                        }
                    }
                }
                b'$' if self.peek() == b'{' => {
                    self.pos += 1; // consume `{`
                    if !text.is_empty() {
                        parts.push(StrPart::Text(std::mem::take(&mut text)));
                    }
                    parts.push(StrPart::Expr(self.lex_interpolation(start)));
                }
                other => {
                    // Copy raw bytes; multi-byte UTF-8 passes through intact.
                    text.push(other as char);
                    if !other.is_ascii() {
                        text.pop();
                        let seq_start = self.pos - 1;
                        while self.pos < self.src.len() && (self.peek() & 0xC0) == 0x80 {
                            self.pos += 1;
                        }
                        text.push_str(&String::from_utf8_lossy(&self.src[seq_start..self.pos]));
                    }
                }
            }
        }
        if !text.is_empty() || parts.is_empty() {
            parts.push(StrPart::Text(text));
        }
        TokenKind::Str(parts, false)
    }

    /// `"""` multiline string: newlines and bare quotes are literal,
    /// escapes and `${...}` interpolation still work. Swift-style dedent:
    /// a leading newline right after the opener is dropped, and the
    /// indentation of the closing `"""` is stripped from every line.
    fn lex_triple_string(&mut self, start: usize) -> TokenKind {
        let mut parts: Vec<StrPart> = Vec::new();
        let mut text = String::new();
        loop {
            if self.pos >= self.src.len() {
                let span = self.span_from(start);
                self.error(span, "unterminated multiline string (close it with `\"\"\"`)");
                break;
            }
            match self.bump() {
                b'"' if self.peek() == b'"' && self.peek_at(1) == b'"' => {
                    self.pos += 2;
                    break;
                }
                b'\\' => {
                    let esc_start = self.pos - 1;
                    match self.bump() {
                        b'n' => text.push('\n'),
                        b't' => text.push('\t'),
                        b'r' => text.push('\r'),
                        b'\\' => text.push('\\'),
                        b'"' => text.push('"'),
                        b'$' => text.push('$'),
                        b'0' => text.push('\0'),
                        b'e' => text.push('\x1b'),
                        other => {
                            let span = self.span_from(esc_start);
                            self.error(
                                span,
                                format!("unknown escape sequence `\\{}`", other as char),
                            );
                        }
                    }
                }
                b'$' if self.peek() == b'{' => {
                    self.pos += 1;
                    if !text.is_empty() {
                        parts.push(StrPart::Text(std::mem::take(&mut text)));
                    }
                    parts.push(StrPart::Expr(self.lex_interpolation(start)));
                }
                other => {
                    text.push(other as char);
                    if !other.is_ascii() {
                        text.pop();
                        let seq_start = self.pos - 1;
                        while self.pos < self.src.len() && (self.peek() & 0xC0) == 0x80 {
                            self.pos += 1;
                        }
                        text.push_str(&String::from_utf8_lossy(&self.src[seq_start..self.pos]));
                    }
                }
            }
        }
        if !text.is_empty() || parts.is_empty() {
            parts.push(StrPart::Text(text));
        }
        dedent_triple(&mut parts);
        TokenKind::Str(parts, true)
    }

    fn peek_at(&self, ahead: usize) -> u8 {
        self.src.get(self.pos + ahead).copied().unwrap_or(0)
    }

    /// Lex tokens inside a `${...}` hole until the matching `}`.
    fn lex_interpolation(&mut self, str_start: usize) -> Vec<Token> {
        let mut tokens = Vec::new();
        let mut depth = 1u32;
        loop {
            if self.pos >= self.src.len() || self.peek() == b'\n' {
                let span = self.span_from(str_start);
                self.error(span, "unterminated `${...}` interpolation");
                break;
            }
            // Peek at brace depth before handing off to next_token.
            if self.peek() == b'}' {
                depth -= 1;
                self.pos += 1;
                if depth == 0 {
                    break;
                }
                tokens.push(Token::new(
                    TokenKind::RBrace,
                    Span::new(self.base + self.pos as u32 - 1, self.base + self.pos as u32),
                ));
                continue;
            }
            let token = self.next_token();
            if token.kind == TokenKind::LBrace {
                depth += 1;
            }
            if token.kind == TokenKind::Eof {
                break;
            }
            tokens.push(token);
        }
        let end = self.base + self.pos as u32;
        tokens.push(Token::new(TokenKind::Eof, Span::new(end, end)));
        tokens
    }
}

/// Strip the incidental indentation of a `"""` string: drop one leading
/// newline after the opener; take the whitespace following the LAST newline
/// (the closing delimiter's indentation) and remove that prefix from every
/// line, plus the trailing newline itself.
fn dedent_triple(parts: &mut [StrPart]) {
    // The closing indent lives at the end of the final literal piece.
    let indent = match parts.iter().rev().find_map(|p| match p {
        StrPart::Text(t) => Some(t),
        _ => None,
    }) {
        Some(t) if parts.last().is_some_and(|l| matches!(l, StrPart::Text(_))) => {
            match t.rfind('\n') {
                Some(i) if t[i + 1..].chars().all(|c| c == ' ' || c == '\t') => {
                    t[i + 1..].to_string()
                }
                _ => String::new(),
            }
        }
        _ => String::new(),
    };
    // Drop the trailing newline + closing indent.
    if let Some(StrPart::Text(t)) = parts.last_mut() {
        if t.ends_with(&indent) {
            let cut = t.len() - indent.len();
            t.truncate(cut);
        }
        if t.ends_with('\n') {
            t.pop();
        }
    }
    // Drop one newline right after the opener; the first line's indent
    // follows it directly (not a remaining `\n`), so strip it here too.
    if let Some(StrPart::Text(t)) = parts.first_mut() {
        if t.starts_with('\n') {
            t.remove(0);
            if let Some(stripped) = t.strip_prefix(indent.as_str()) {
                *t = stripped.to_string();
            }
        }
    }
    if indent.is_empty() {
        return;
    }
    // Strip the closing indent after every remaining newline.
    for part in parts.iter_mut() {
        if let StrPart::Text(t) = part {
            let mut out = String::with_capacity(t.len());
            let mut rest = t.as_str();
            loop {
                match rest.find('\n') {
                    Some(i) => {
                        out.push_str(&rest[..=i]);
                        rest = &rest[i + 1..];
                        if let Some(stripped) = rest.strip_prefix(indent.as_str()) {
                            rest = stripped;
                        }
                    }
                    None => {
                        out.push_str(rest);
                        break;
                    }
                }
            }
            *t = out;
        }
    }
}
