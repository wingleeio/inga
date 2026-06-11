//! Language server for Inga, speaking LSP over stdio.
//!
//! Documents live in memory (didOpen/didChange), so no file-system access is
//! needed. Every feature is derived from `inga_core::check_source`:
//! diagnostics, hover (inferred signatures incl. `!` and `uses` rows),
//! go-to-definition, whole-document formatting, semantic tokens, completion.

use std::collections::HashMap;

use lsp_server::{Connection, ExtractError, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Notification as _,
    PublishDiagnostics,
};
use lsp_types::request::{
    Completion, Formatting, GotoDefinition, HoverRequest, Request as _, SemanticTokensFullRequest,
};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionOptions, CompletionResponse, Diagnostic,
    DiagnosticSeverity, GotoDefinitionResponse, Hover, HoverContents, HoverProviderCapability,
    InitializeParams, Location, MarkedString, OneOf, Position, PublishDiagnosticsParams, Range,
    SemanticToken, SemanticTokenType, SemanticTokens, SemanticTokensFullOptions,
    SemanticTokensLegend, SemanticTokensOptions, SemanticTokensResult,
    SemanticTokensServerCapabilities, ServerCapabilities, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextEdit, Url,
};

use inga_core::check_source as check_single;
use inga_core::Checked;

/// Check an open document. When its URI maps to a real file, imports are
/// resolved relative to it (open documents win over the disk copy); the
/// entry module is always first, so its spans start at 0 and positions in
/// the open file map 1:1.
fn check_document(
    uri: &lsp_types::Url,
    src: &str,
    docs: &HashMap<Url, String>,
) -> (Checked, Vec<inga_core::modules::ModuleSrc>, u32) {
    let Ok(path) = uri.to_file_path() else { return (check_single(src), Vec::new(), 0) };
    // A library module (no `main`) is checked in the context of the sibling
    // program that imports it.
    let entry = inga_core::modules::resolve_entry_for(&path, src).unwrap_or_else(|| path.clone());
    let entry_src = if entry == path {
        src.to_string()
    } else {
        std::fs::read_to_string(&entry).unwrap_or_default()
    };
    let this = path.canonicalize().unwrap_or_else(|_| path.clone());
    let loaded = inga_core::modules::load_program_with(&entry, entry_src, &mut |p| {
        let abs = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        if abs == this {
            return Some(src.to_string());
        }
        Url::from_file_path(&abs)
            .ok()
            .and_then(|u| docs.get(&u).cloned())
            .or_else(|| std::fs::read_to_string(p).ok())
    });
    let (checked, modules) = inga_core::check_loaded(loaded);
    // Diagnostics/hovers/defs surface only for the open file, shifted back
    // to its local coordinates. Refs stay GLOBAL so go-to-definition can
    // jump across modules.
    let module = modules
        .iter()
        .find(|m| std::fs::canonicalize(&m.path).unwrap_or_else(|_| m.path.clone()) == this);
    let (base, end) = module.map(|m| (m.base, m.end)).unwrap_or((0, u32::MAX));
    let inside = |s: inga_core::span::Span| s.start >= base && s.start <= end;
    let shift =
        |s: inga_core::span::Span| inga_core::span::Span::new(s.start - base, s.end - base);
    let mut checked = checked;
    checked.diagnostics.retain(|d| inside(d.span));
    for d in &mut checked.diagnostics {
        d.span = shift(d.span);
    }
    checked.info.hovers.retain(|(s, _)| inside(*s));
    for (s, _) in &mut checked.info.hovers {
        *s = shift(*s);
    }
    checked.info.defs.retain(|d| inside(d.span));
    for d in &mut checked.info.defs {
        d.span = shift(d.span);
    }
    (checked, modules, base)
}
use inga_core::diag::Severity;
use inga_core::span::{LineIndex, Span};
use inga_core::token::{StrPart, Token, TokenKind};

pub fn run_server() {
    let (connection, io_threads) = Connection::stdio();

    let capabilities = ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        document_formatting_provider: Some(OneOf::Left(true)),
        completion_provider: Some(CompletionOptions::default()),
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                legend: SemanticTokensLegend {
                    token_types: legend_types(),
                    token_modifiers: vec![],
                },
                full: Some(SemanticTokensFullOptions::Bool(true)),
                ..Default::default()
            },
        )),
        ..Default::default()
    };

    let initialize_params = match connection.initialize(serde_json::to_value(capabilities).unwrap())
    {
        Ok(params) => params,
        Err(_) => return,
    };
    let _params: InitializeParams = serde_json::from_value(initialize_params).unwrap_or_default();

    let mut server = Server { documents: HashMap::new() };
    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req).unwrap_or(true) {
                    break;
                }
                let response = server.handle_request(req);
                if let Some(response) = response {
                    let _ = connection.sender.send(Message::Response(response));
                }
            }
            Message::Notification(notification) => {
                if let Some((uri, diagnostics)) = server.handle_notification(notification) {
                    let params = PublishDiagnosticsParams { uri, diagnostics, version: None };
                    let _ = connection.sender.send(Message::Notification(Notification::new(
                        PublishDiagnostics::METHOD.to_string(),
                        params,
                    )));
                }
            }
            Message::Response(_) => {}
        }
    }
    let _ = io_threads.join();
}

struct Server {
    documents: HashMap<Url, String>,
}

impl Server {
    // ---- notifications ----------------------------------------------------

    /// Returns (uri, diagnostics) when diagnostics should be (re)published.
    fn handle_notification(
        &mut self,
        notification: Notification,
    ) -> Option<(Url, Vec<Diagnostic>)> {
        match notification.method.as_str() {
            DidOpenTextDocument::METHOD => {
                let params: lsp_types::DidOpenTextDocumentParams =
                    serde_json::from_value(notification.params).ok()?;
                let uri = params.text_document.uri;
                self.documents.insert(uri.clone(), params.text_document.text);
                Some((uri.clone(), self.compute_diagnostics(&uri)))
            }
            DidChangeTextDocument::METHOD => {
                let params: lsp_types::DidChangeTextDocumentParams =
                    serde_json::from_value(notification.params).ok()?;
                let uri = params.text_document.uri;
                if let Some(change) = params.content_changes.into_iter().last() {
                    self.documents.insert(uri.clone(), change.text);
                }
                Some((uri.clone(), self.compute_diagnostics(&uri)))
            }
            DidCloseTextDocument::METHOD => {
                let params: lsp_types::DidCloseTextDocumentParams =
                    serde_json::from_value(notification.params).ok()?;
                self.documents.remove(&params.text_document.uri);
                Some((params.text_document.uri, Vec::new()))
            }
            _ => None,
        }
    }

    fn compute_diagnostics(&self, uri: &Url) -> Vec<Diagnostic> {
        let Some(src) = self.documents.get(uri) else { return Vec::new() };
        let (checked, _mods, _base) = check_document(uri, src, &self.documents);
        let lines = LineIndex::new(src);
        checked
            .diagnostics
            .iter()
            .map(|d| Diagnostic {
                range: span_range(src, &lines, d.span),
                severity: Some(match d.severity {
                    Severity::Error => DiagnosticSeverity::ERROR,
                    Severity::Warning => DiagnosticSeverity::WARNING,
                }),
                source: Some("inga".to_string()),
                message: d.message.clone(),
                ..Default::default()
            })
            .collect()
    }

    // ---- requests -----------------------------------------------------------

    fn handle_request(&mut self, req: Request) -> Option<Response> {
        let id = req.id.clone();
        match req.method.as_str() {
            HoverRequest::METHOD => {
                let (id, params) = cast::<HoverRequest>(req)?;
                let result = self.hover(params);
                Some(Response::new_ok(id, result))
            }
            GotoDefinition::METHOD => {
                let (id, params) = cast::<GotoDefinition>(req)?;
                let result = self.definition(params);
                Some(Response::new_ok(id, result))
            }
            Formatting::METHOD => {
                let (id, params) = cast::<Formatting>(req)?;
                let result = self.format(params);
                Some(Response::new_ok(id, result))
            }
            SemanticTokensFullRequest::METHOD => {
                let (id, params) = cast::<SemanticTokensFullRequest>(req)?;
                let result = self.semantic_tokens(params);
                Some(Response::new_ok(id, result))
            }
            Completion::METHOD => {
                let (id, params) = cast::<Completion>(req)?;
                let result = self.completion(params);
                Some(Response::new_ok(id, result))
            }
            _ => Some(Response::new_ok(id, serde_json::Value::Null)),
        }
    }

    fn hover(&self, params: lsp_types::HoverParams) -> Option<Hover> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let src = self.documents.get(&uri)?;
        let lines = LineIndex::new(src);
        let offset = lines.offset_utf16(src, position.line, position.character);
        let (checked, _mods, _base) = check_document(&uri, src, &self.documents);
        // Innermost hover span containing the offset.
        let best = checked
            .info
            .hovers
            .iter()
            .filter(|(span, _)| span.contains(offset))
            .min_by_key(|(span, _)| span.end - span.start)?;
        Some(Hover {
            contents: HoverContents::Scalar(MarkedString::LanguageString(
                lsp_types::LanguageString { language: "inga".into(), value: best.1.clone() },
            )),
            range: Some(span_range(src, &lines, best.0)),
        })
    }

    fn definition(&self, params: lsp_types::GotoDefinitionParams) -> Option<GotoDefinitionResponse> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let src = self.documents.get(&uri)?;
        let lines = LineIndex::new(src);
        let offset = lines.offset_utf16(src, position.line, position.character);
        let (checked, modules, base) = check_document(&uri, src, &self.documents);
        // Refs are in the program's global span space; the definition may
        // live in another module's file.
        let global = offset + base;
        let (_, def_span) = checked
            .info
            .refs
            .iter()
            .find(|(use_span, _)| use_span.contains(global))?;
        let target = modules.iter().find(|m| m.contains(*def_span));
        match target {
            Some(m) if m.base != base => {
                let abs = m.path.canonicalize().unwrap_or_else(|_| m.path.clone());
                let target_uri = Url::from_file_path(&abs).ok()?;
                let local = Span::new(def_span.start - m.base, def_span.end - m.base);
                let target_lines = LineIndex::new(&m.src);
                Some(GotoDefinitionResponse::Scalar(Location {
                    uri: target_uri,
                    range: span_range(&m.src, &target_lines, local),
                }))
            }
            _ => {
                let local = Span::new(def_span.start - base, def_span.end - base);
                Some(GotoDefinitionResponse::Scalar(Location {
                    uri,
                    range: span_range(src, &lines, local),
                }))
            }
        }
    }

    fn format(&self, params: lsp_types::DocumentFormattingParams) -> Option<Vec<TextEdit>> {
        let uri = params.text_document.uri;
        let src = self.documents.get(&uri)?;
        let formatted = inga_core::fmt::format(src).ok()?;
        if formatted == *src {
            return Some(Vec::new());
        }
        let lines = LineIndex::new(src);
        let end = span_range(src, &lines, Span::new(0, src.len() as u32)).end;
        Some(vec![TextEdit {
            range: Range { start: Position::new(0, 0), end },
            new_text: formatted,
        }])
    }

    fn semantic_tokens(
        &self,
        params: lsp_types::SemanticTokensParams,
    ) -> Option<SemanticTokensResult> {
        let uri = params.text_document.uri;
        let src = self.documents.get(&uri)?;
        let mut diagnostics = Vec::new();
        let tokens = inga_core::lexer::lex(src, &mut diagnostics);
        let lines = LineIndex::new(src);

        let mut raw: Vec<(u32, u32, u32, u32)> = Vec::new(); // line, col16, len16, type
        collect_semantic_tokens(src, &lines, &tokens, &mut raw);
        raw.sort_unstable();

        let mut data = Vec::with_capacity(raw.len());
        let (mut prev_line, mut prev_col) = (0u32, 0u32);
        for (line, col, len, token_type) in raw {
            let delta_line = line - prev_line;
            let delta_start = if delta_line == 0 { col - prev_col } else { col };
            data.push(SemanticToken {
                delta_line,
                delta_start,
                length: len,
                token_type,
                token_modifiers_bitset: 0,
            });
            prev_line = line;
            prev_col = col;
        }
        Some(SemanticTokensResult::Tokens(SemanticTokens { result_id: None, data }))
    }

    fn completion(&self, params: lsp_types::CompletionParams) -> Option<CompletionResponse> {
        let uri = params.text_document_position.text_document.uri;
        let src = self.documents.get(&uri)?;
        let (checked, _mods, _base) = check_document(&uri, src, &self.documents);
        let mut items: Vec<CompletionItem> = Vec::new();
        for def in &checked.info.defs {
            items.push(CompletionItem {
                label: def.name.clone(),
                kind: Some(match def.kind {
                    inga_core::check::DefKind::Func => CompletionItemKind::FUNCTION,
                    inga_core::check::DefKind::Struct => CompletionItemKind::STRUCT,
                    inga_core::check::DefKind::Enum => CompletionItemKind::ENUM,
                    inga_core::check::DefKind::Service => CompletionItemKind::INTERFACE,
                    inga_core::check::DefKind::Impl => CompletionItemKind::MODULE,
                    inga_core::check::DefKind::Method => CompletionItemKind::METHOD,
                }),
                detail: Some(def.detail.clone()),
                ..Default::default()
            });
        }
        for (name, detail) in inga_core::check::builtin_completions() {
            items.push(CompletionItem {
                label: name.to_string(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some(detail.to_string()),
                ..Default::default()
            });
        }
        for keyword in
            ["use", "pub", "struct", "enum", "service", "match", "catch", "fail", "provide", "uses", "lazy", "if", "else"]
        {
            items.push(CompletionItem {
                label: keyword.to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                ..Default::default()
            });
        }
        Some(CompletionResponse::Array(items))
    }
}

fn cast<R: lsp_types::request::Request>(req: Request) -> Option<(RequestId, R::Params)> {
    match req.extract(R::METHOD) {
        Ok(value) => Some(value),
        Err(ExtractError::MethodMismatch(_)) | Err(ExtractError::JsonError { .. }) => None,
    }
}

// ---- semantic tokens ---------------------------------------------------------

// Legend indices.
const T_KEYWORD: u32 = 0;
const T_TYPE: u32 = 1;
const T_STRING: u32 = 2;
const T_NUMBER: u32 = 3;
const T_COMMENT: u32 = 4;
const T_OPERATOR: u32 = 5;
const T_VARIABLE: u32 = 6;
const T_FUNCTION: u32 = 7;
const T_METHOD: u32 = 8;
const T_PROPERTY: u32 = 9;
const T_ENUM_MEMBER: u32 = 10;
const T_NAMESPACE: u32 = 11;

fn legend_types() -> Vec<SemanticTokenType> {
    vec![
        SemanticTokenType::KEYWORD,
        SemanticTokenType::TYPE,
        SemanticTokenType::STRING,
        SemanticTokenType::NUMBER,
        SemanticTokenType::COMMENT,
        SemanticTokenType::OPERATOR,
        SemanticTokenType::VARIABLE,
        SemanticTokenType::FUNCTION,
        SemanticTokenType::METHOD,
        SemanticTokenType::PROPERTY,
        SemanticTokenType::ENUM_MEMBER,
        SemanticTokenType::NAMESPACE,
    ]
}

/// Context-aware classification over the raw token stream:
/// `name ::` and `name(` are functions, `.name(` is a method, `.name` a
/// property, `Some`/`None` enum members, `Gfx.`/`Schedule.` namespaces.
fn collect_semantic_tokens(
    src: &str,
    lines: &LineIndex,
    tokens: &[Token],
    out: &mut Vec<(u32, u32, u32, u32)>,
) {
    // Neighbor lookups skip trivia.
    let significant: Vec<usize> = tokens
        .iter()
        .enumerate()
        .filter(|(_, t)| !matches!(t.kind, TokenKind::Newline | TokenKind::Comment(_)))
        .map(|(i, _)| i)
        .collect();
    let sig_pos = |i: usize| significant.binary_search(&i).ok();
    let next_kind = |i: usize| -> Option<&TokenKind> {
        sig_pos(i).and_then(|p| significant.get(p + 1)).map(|&j| &tokens[j].kind)
    };
    let prev_kind = |i: usize| -> Option<&TokenKind> {
        sig_pos(i)
            .and_then(|p| p.checked_sub(1))
            .and_then(|p| significant.get(p))
            .map(|&j| &tokens[j].kind)
    };

    for (i, token) in tokens.iter().enumerate() {
        let token_type = match &token.kind {
            TokenKind::Comment(_) => T_COMMENT,
            TokenKind::Str(parts) => {
                // Emit non-overlapping string segments around the `${...}`
                // holes; hole contents classify themselves.
                let mut cursor = token.span.start;
                for part in parts {
                    if let StrPart::Expr(inner) = part {
                        let exprs: Vec<&Token> =
                            inner.iter().filter(|t| t.kind != TokenKind::Eof).collect();
                        let (Some(first), Some(last)) = (exprs.first(), exprs.last()) else {
                            continue;
                        };
                        if first.span.start > cursor {
                            push_span(
                                src,
                                lines,
                                Span::new(cursor, first.span.start),
                                T_STRING,
                                out,
                            );
                        }
                        collect_semantic_tokens(src, lines, inner, out);
                        cursor = last.span.end;
                    }
                }
                if token.span.end > cursor {
                    push_span(src, lines, Span::new(cursor, token.span.end), T_STRING, out);
                }
                continue;
            }
            TokenKind::Int(_) | TokenKind::Float(_) | TokenKind::KwTrue | TokenKind::KwFalse => {
                T_NUMBER
            }
            TokenKind::Ident(name) => {
                let upper = name.chars().next().is_some_and(|c| c.is_ascii_uppercase());
                let after_dot = matches!(prev_kind(i), Some(TokenKind::Dot));
                let before_paren = matches!(next_kind(i), Some(TokenKind::LParen));
                if upper {
                    if name == "Some" || name == "None" {
                        T_ENUM_MEMBER
                    } else if matches!(next_kind(i), Some(TokenKind::Dot))
                        && (name == "Gfx" || name == "Schedule")
                    {
                        T_NAMESPACE
                    } else {
                        T_TYPE
                    }
                } else if matches!(next_kind(i), Some(TokenKind::ColonColon)) {
                    T_FUNCTION // declaration
                } else if after_dot && before_paren {
                    T_METHOD
                } else if after_dot {
                    T_PROPERTY
                } else if before_paren {
                    T_FUNCTION // call
                } else {
                    T_VARIABLE
                }
            }
            TokenKind::KwStruct
            | TokenKind::KwEnum
            | TokenKind::KwService
            | TokenKind::KwMatch
            | TokenKind::KwCatch
            | TokenKind::KwFail
            | TokenKind::KwProvide
            | TokenKind::KwUses
            | TokenKind::KwLazy
            | TokenKind::KwIf
            | TokenKind::KwElse => T_KEYWORD,
            TokenKind::ColonColon
            | TokenKind::Arrow
            | TokenKind::PipeOp
            | TokenKind::Bang
            | TokenKind::Question
            | TokenKind::Eq
            | TokenKind::EqEq
            | TokenKind::NotEq
            | TokenKind::Lt
            | TokenKind::Le
            | TokenKind::Gt
            | TokenKind::Ge
            | TokenKind::Plus
            | TokenKind::Minus
            | TokenKind::Star
            | TokenKind::Slash
            | TokenKind::Percent
            | TokenKind::AndAnd
            | TokenKind::OrOr => T_OPERATOR,
            _ => continue,
        };
        push_span(src, lines, token.span, token_type, out);
    }
}

/// Split a span into per-line semantic tokens (LSP tokens cannot span lines).
fn push_span(
    src: &str,
    lines: &LineIndex,
    span: Span,
    token_type: u32,
    out: &mut Vec<(u32, u32, u32, u32)>,
) {
    let mut start = span.start;
    let end = span.end.min(src.len() as u32);
    while start < end {
        let (line, col16) = lines.line_col_utf16(src, start);
        let line_end_offset = if line + 1 < lines.line_count() {
            (lines.line_start(line + 1) - 1).min(end)
        } else {
            end
        };
        let segment_end = line_end_offset.max(start);
        let text = &src[start as usize..segment_end as usize];
        let len16: u32 = text.chars().map(|c| c.len_utf16() as u32).sum();
        if len16 > 0 {
            out.push((line, col16, len16, token_type));
        }
        if segment_end >= end {
            break;
        }
        start = segment_end + 1; // skip the newline
    }
}

fn span_range(src: &str, lines: &LineIndex, span: Span) -> Range {
    let (sl, sc) = lines.line_col_utf16(src, span.start);
    let (el, ec) = lines.line_col_utf16(src, span.end);
    Range { start: Position::new(sl, sc), end: Position::new(el, ec) }
}
