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
    CodeActionRequest, Completion, Formatting, GotoDefinition, HoverRequest, Request as _,
    SemanticTokensFullRequest,
};
use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionProviderCapability,
    CodeActionResponse, CompletionItem, CompletionItemKind, CompletionOptions, CompletionResponse,
    Diagnostic, DiagnosticSeverity, GotoDefinitionResponse, Hover, HoverContents,
    HoverProviderCapability, InitializeParams, Location, MarkedString, OneOf, Position,
    PublishDiagnosticsParams, Range, SemanticToken, SemanticTokenType, SemanticTokens,
    SemanticTokensFullOptions, SemanticTokensLegend, SemanticTokensOptions, SemanticTokensResult,
    SemanticTokensServerCapabilities, ServerCapabilities, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextEdit, Url, WorkspaceEdit,
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
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec![".".to_string()]),
            ..Default::default()
        }),
        code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
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
            CodeActionRequest::METHOD => {
                let (id, params) = cast::<CodeActionRequest>(req)?;
                let result = self.code_action(params);
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
        let position = params.text_document_position.position;
        let lines = LineIndex::new(src);
        let offset = lines.offset_utf16(src, position.line, position.character);
        if let Some(items) = self.member_completion(&uri, src, offset) {
            return Some(CompletionResponse::Array(items));
        }
        if let Some(items) = self.arm_completion(&uri, src, offset) {
            return Some(CompletionResponse::Array(items));
        }
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
            ["use", "pub", "shared", "struct", "enum", "service", "match", "catch", "fail", "provide", "uses", "lazy", "if", "else"]
        {
            items.push(CompletionItem {
                label: keyword.to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                ..Default::default()
            });
        }
        // Names from sibling modules and std module aliases: completing one
        // also inserts (or extends) the `use` line.
        for export in sibling_exports(&uri, &self.documents) {
            match import_state(src, &export.module, Some(&export.import_name)) {
                ImportState::Needs(edit) => items.push(CompletionItem {
                    label: export.label.clone(),
                    kind: Some(export.kind),
                    detail: Some(format!(
                        "auto-import from `{}`{}",
                        export.module,
                        if export.label != export.import_name {
                            format!(" (imports `{}`)", export.import_name)
                        } else {
                            String::new()
                        }
                    )),
                    additional_text_edits: Some(vec![edit]),
                    ..Default::default()
                }),
                ImportState::Imported => items.push(CompletionItem {
                    label: export.label.clone(),
                    kind: Some(export.kind),
                    detail: Some(format!("from `{}`", export.module)),
                    ..Default::default()
                }),
                ImportState::Qualified => {}
            }
        }
        for (alias, target) in STD_ALIASES {
            if let ImportState::Needs(edit) = import_state(src, target, None) {
                items.push(CompletionItem {
                    label: alias.to_string(),
                    kind: Some(CompletionItemKind::MODULE),
                    detail: Some(format!("auto-import `use {target}`")),
                    additional_text_edits: Some(vec![edit]),
                    ..Default::default()
                });
            }
        }
        Some(CompletionResponse::Array(items))
    }

    /// Completions after a `.`: module members for `schedule.`/`fiber.`/
    /// `graphics.`/file-module aliases, and value members (struct fields,
    /// service methods, map ops, tuple indexes, Int suffixes) by checked
    /// type. Returns None when the cursor is not in a member position.
    fn member_completion(
        &self,
        uri: &Url,
        src: &str,
        offset: u32,
    ) -> Option<Vec<CompletionItem>> {
        // Token scan: cursor right after `recv.` or `recv.partial`.
        let mut diags = Vec::new();
        let tokens = inga_core::lexer::lex(src, &mut diags);
        let significant: Vec<&Token> = tokens
            .iter()
            .filter(|t| !matches!(t.kind, TokenKind::Newline | TokenKind::Comment(_) | TokenKind::Eof))
            .collect();
        let at = significant.iter().rposition(|t| t.span.end <= offset && t.span.start < offset)?;
        let (dot_idx, partial) = match &significant[at].kind {
            TokenKind::Dot => (at, false),
            TokenKind::Ident(_) | TokenKind::Int(_)
                if at > 0
                    && matches!(significant[at - 1].kind, TokenKind::Dot)
                    && significant[at].span.end >= offset =>
            {
                (at - 1, true)
            }
            _ => return None,
        };
        let recv = *significant.get(dot_idx.checked_sub(1)?)?;

        // A module alias receiver: list the module's members.
        if let TokenKind::Ident(name) = &recv.kind {
            for (alias, target) in STD_ALIASES {
                if name == alias {
                    // Members complete even before the module is imported —
                    // accepting one brings the `use` line along.
                    let mut items = std_member_items(target);
                    if let ImportState::Needs(edit) = import_state(src, target, None) {
                        for item in &mut items {
                            item.additional_text_edits = Some(vec![edit.clone()]);
                        }
                    }
                    return Some(items);
                }
            }
            for (target, names, _) in current_imports(src) {
                let alias = target.rsplit('/').next().unwrap_or(&target);
                if names.is_none() && name == alias && !target.starts_with("std/") {
                    return Some(self.module_member_items(uri, &target));
                }
            }
        }

        // A value receiver: type it by re-checking with a placeholder member
        // name when nothing is typed after the dot yet (so the file parses).
        let dot_end = significant[dot_idx].span.end;
        let patched: String = if partial {
            src.to_string()
        } else {
            format!("{}zz{}", &src[..dot_end as usize], &src[dot_end as usize..])
        };
        let (checked, _mods, base) = check_document(uri, &patched, &self.documents);
        // The receiver expression ends exactly at the dot; among recorded
        // expressions ending there, the longest is the whole receiver.
        let recv_end = significant[dot_idx].span.start + base;
        let cty = checked
            .info
            .expr_types
            .iter()
            .filter(|((_, end), _)| *end == recv_end)
            .min_by_key(|((start, _), _)| *start)
            .map(|(_, cty)| cty.clone())?;
        Some(value_member_items(&cty, &checked.program))
    }

    /// `pub` members of a file module (for `alias.` completion).
    fn module_member_items(&self, uri: &Url, target: &str) -> Vec<CompletionItem> {
        use inga_core::ast::Decl;
        let mut items = Vec::new();
        // `target` is the use-path text (`cards`, `lib/helpers`), resolved
        // relative to the open file like the module loader does.
        let Ok(this) = uri.to_file_path() else { return items };
        let Some(dir) = this.parent() else { return items };
        let mut p = dir.to_path_buf();
        for seg in target.split('/') {
            p.push(seg);
        }
        p.set_extension("inga");
        let src = Url::from_file_path(&p)
            .ok()
            .and_then(|u| self.documents.get(&u).cloned())
            .or_else(|| std::fs::read_to_string(&p).ok());
        let Some(src) = src else { return items };
        for decl in &parse_loose(&src).decls {
            let (name, kind) = match decl {
                Decl::Func(d) if d.is_pub => (d.name.clone(), CompletionItemKind::FUNCTION),
                Decl::Struct(d) if d.is_pub => (d.name.clone(), CompletionItemKind::STRUCT),
                Decl::Enum(d) if d.is_pub => (d.name.clone(), CompletionItemKind::ENUM),
                Decl::Service(d) if d.is_pub => (d.name.clone(), CompletionItemKind::INTERFACE),
                Decl::Impl(d) if d.is_pub => (d.name.clone(), CompletionItemKind::MODULE),
                _ => continue,
            };
            items.push(CompletionItem { label: name, kind: Some(kind), ..Default::default() });
        }
        items
    }

    /// Completions in pattern position inside `catch { ... }` / `match
    /// scrutinee { ... }`: the error types of the caught expression's row,
    /// or the scrutinee's variants (Some/None, true/false, Ok/Failed, enum
    /// variants). Returns None when the cursor is not in arm position.
    fn arm_completion(&self, uri: &Url, src: &str, offset: u32) -> Option<Vec<CompletionItem>> {
        // Pattern position: before the `->` of the arm being typed.
        let line_start = src[..offset as usize].rfind('\n').map(|i| i + 1).unwrap_or(0);
        if src[line_start..offset as usize].contains("->") {
            return None;
        }
        // A placeholder arm makes a half-typed pattern parse: `Bo` becomes
        // the typed-bind `Bo zzq -> 0`, a bare cursor becomes `zzq -> 0`.
        let patched =
            format!("{} zzq -> 0\n{}", &src[..offset as usize], &src[offset as usize..]);
        let (checked, _mods, base) = check_document(uri, &patched, &self.documents);
        let g = offset + base;
        let catch = checked
            .info
            .catch_rows
            .iter()
            .filter(|(s, _)| s.contains(g))
            .min_by_key(|(s, _)| s.end - s.start);
        let mtch = checked
            .info
            .match_ctxs
            .iter()
            .filter(|(s, key)| s.contains(g) && g > key.1)
            .min_by_key(|(s, _)| s.end - s.start);
        // Innermost wins when nested.
        let use_catch = match (catch, mtch) {
            (Some((cs, _)), Some((ms, _))) => cs.end - cs.start <= ms.end - ms.start,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => return None,
        };
        let enum_variants = |name: &str| -> Vec<String> {
            checked
                .program
                .decls
                .iter()
                .filter_map(|d| match d {
                    inga_core::ast::Decl::Enum(e) if e.name == name => {
                        Some(e.variants.iter().map(|v| v.name.clone()).collect::<Vec<_>>())
                    }
                    _ => None,
                })
                .next()
                .unwrap_or_default()
        };
        let mut items = Vec::new();
        let mut push = |label: String, kind: CompletionItemKind, detail: String| {
            items.push(CompletionItem {
                label,
                kind: Some(kind),
                detail: Some(detail),
                ..Default::default()
            });
        };
        if use_catch {
            let (_, row) = catch?;
            for err in row {
                push(
                    err.clone(),
                    CompletionItemKind::STRUCT,
                    format!("in the error row — `{err}` or bind with `{err} e`"),
                );
                for v in enum_variants(err) {
                    push(
                        v.clone(),
                        CompletionItemKind::ENUM_MEMBER,
                        format!("variant of `{err}` (in the error row)"),
                    );
                }
            }
            if items.is_empty() {
                return None; // empty row: nothing to catch
            }
        } else {
            let (_, key) = mtch?;
            use inga_core::check::CType;
            match checked.info.expr_types.get(key)? {
                CType::Enum(n) => {
                    for v in enum_variants(n) {
                        push(v.clone(), CompletionItemKind::ENUM_MEMBER, format!("variant of `{n}`"));
                    }
                }
                CType::Option(_) => {
                    push("Some".into(), CompletionItemKind::ENUM_MEMBER, "Some(value)".into());
                    push("None".into(), CompletionItemKind::ENUM_MEMBER, "None".into());
                }
                CType::Bool => {
                    push("true".into(), CompletionItemKind::VALUE, "Bool".into());
                    push("false".into(), CompletionItemKind::VALUE, "Bool".into());
                }
                CType::Outcome(_) => {
                    push("Ok".into(), CompletionItemKind::ENUM_MEMBER, "Ok(value)".into());
                    push(
                        "Failed".into(),
                        CompletionItemKind::ENUM_MEMBER,
                        "Failed(error) — patterns speak catch's language".into(),
                    );
                }
                _ => return None,
            }
        }
        Some(items)
    }

    /// Quick fixes (cmd+.): on an unknown-name family diagnostic, offer the
    /// imports that would resolve it.
    fn code_action(&self, params: lsp_types::CodeActionParams) -> Option<CodeActionResponse> {
        let uri = params.text_document.uri;
        let src = self.documents.get(&uri)?;
        let lines = LineIndex::new(src);
        let (checked, _mods, _base) = check_document(&uri, src, &self.documents);
        let overlaps = |s: Span| {
            let r = span_range(src, &lines, s);
            !(r.end.line < params.range.start.line
                || r.start.line > params.range.end.line
                || (r.end.line == params.range.start.line
                    && r.end.character < params.range.start.character)
                || (r.start.line == params.range.end.line
                    && r.start.character > params.range.end.character))
        };
        let mut actions: Vec<CodeActionOrCommand> = Vec::new();
        let mut offered: Vec<String> = Vec::new();
        let exports = sibling_exports(&uri, &self.documents);
        for d in checked.diagnostics.iter().filter(|d| overlaps(d.span)) {
            let unknown = ["unknown name `", "unknown type `", "unknown constructor `",
                "unknown service `", "unknown implementation `"]
                .iter()
                .any(|p| d.message.starts_with(p))
                || d.message.contains("module is not imported");
            if !unknown {
                continue;
            }
            let Some(name) = d.message.split('`').nth(1) else { continue };
            let diag = Diagnostic {
                range: span_range(src, &lines, d.span),
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("inga".to_string()),
                message: d.message.clone(),
                ..Default::default()
            };
            let mut offer = |title: String, edit: TextEdit, actions: &mut Vec<CodeActionOrCommand>| {
                if offered.contains(&title) {
                    return;
                }
                offered.push(title.clone());
                let mut changes = HashMap::new();
                changes.insert(uri.clone(), vec![edit]);
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title,
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: Some(vec![diag.clone()]),
                    edit: Some(WorkspaceEdit { changes: Some(changes), ..Default::default() }),
                    is_preferred: Some(true),
                    ..Default::default()
                }));
            };
            // A std module alias: `schedule.upTo(...)` without the import.
            for (alias, target) in STD_ALIASES {
                if name == alias {
                    if let ImportState::Needs(edit) = import_state(src, target, None) {
                        offer(format!("Add `use {target}`"), edit, &mut actions);
                    }
                }
            }
            // A pub name (or enum variant) from a sibling module.
            for export in exports.iter().filter(|e| e.label == name) {
                if let ImportState::Needs(edit) =
                    import_state(src, &export.module, Some(&export.import_name))
                {
                    let title = if export.label != export.import_name {
                        format!(
                            "Import `{}` from `{}` (brings `{}`)",
                            export.import_name, export.module, export.label
                        )
                    } else {
                        format!("Import `{}` from `{}`", export.import_name, export.module)
                    };
                    offer(title, edit, &mut actions);
                }
            }
        }
        Some(actions)
    }
}

// ---- auto-import -------------------------------------------------------------
//
// Two surfaces share this machinery: completion items for not-yet-imported
// names (the `use` line arrives via additionalTextEdits), and quick fixes
// (cmd+.) on "unknown name/type/..." diagnostics.

/// One importable name from a sibling module (or a std module alias).
struct Export {
    /// The name a `use mod { name }` would bring in.
    import_name: String,
    /// What completing it inserts (= import_name, except enum variants,
    /// which are reachable by importing their enum).
    label: String,
    kind: CompletionItemKind,
    /// `use` path: `cards`, `std/fiber`, ...
    module: String,
}

const STD_ALIASES: [(&str, &str); 4] = [
    ("graphics", "std/graphics"),
    ("schedule", "std/schedule"),
    ("fiber", "std/fiber"),
    ("http", "std/http"),
];

/// Parse a source text, ignoring diagnostics (good enough for listing decls).
fn parse_loose(src: &str) -> inga_core::ast::Program {
    let mut diags = Vec::new();
    let tokens = inga_core::lexer::lex(src, &mut diags);
    inga_core::parser::parse(tokens, &mut diags)
}

/// Every `pub` name exported by the open file's sibling modules. Open
/// documents win over the disk copies.
fn sibling_exports(uri: &Url, docs: &HashMap<Url, String>) -> Vec<Export> {
    use inga_core::ast::Decl;
    let mut out = Vec::new();
    let Ok(path) = uri.to_file_path() else { return out };
    let Some(dir) = path.parent() else { return out };
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("inga") || p == path {
            continue;
        }
        let Some(module) = p.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
            continue;
        };
        let src = Url::from_file_path(&p)
            .ok()
            .and_then(|u| docs.get(&u).cloned())
            .or_else(|| std::fs::read_to_string(&p).ok());
        let Some(src) = src else { continue };
        for decl in &parse_loose(&src).decls {
            let (name, kind) = match decl {
                Decl::Func(d) if d.is_pub => (d.name.clone(), CompletionItemKind::FUNCTION),
                Decl::Struct(d) if d.is_pub => (d.name.clone(), CompletionItemKind::STRUCT),
                Decl::Enum(d) if d.is_pub => {
                    // Importing the enum also grants its variants.
                    for v in &d.variants {
                        out.push(Export {
                            import_name: d.name.clone(),
                            label: v.name.clone(),
                            kind: CompletionItemKind::ENUM_MEMBER,
                            module: module.clone(),
                        });
                    }
                    (d.name.clone(), CompletionItemKind::ENUM)
                }
                Decl::Service(d) if d.is_pub => (d.name.clone(), CompletionItemKind::INTERFACE),
                Decl::Impl(d) if d.is_pub => (d.name.clone(), CompletionItemKind::MODULE),
                _ => continue,
            };
            out.push(Export { import_name: name.clone(), label: name, kind, module: module.clone() });
        }
    }
    out
}

/// The open file's `use` declarations: (path text, selective names, decl span).
fn current_imports(src: &str) -> Vec<(String, Option<Vec<String>>, Span)> {
    parse_loose(src)
        .decls
        .iter()
        .filter_map(|d| match d {
            inga_core::ast::Decl::Use(u) => Some((
                u.path.join("/"),
                u.names.as_ref().map(|ns| ns.iter().map(|(n, _)| n.clone()).collect()),
                u.span,
            )),
            _ => None,
        })
        .collect()
}

/// How a name from `module` relates to the open file's imports.
enum ImportState {
    /// Not imported: the edit adds/extends a `use` line.
    Needs(TextEdit),
    /// Already reachable unqualified — nothing to do.
    Imported,
    /// The module is plain-imported (qualified alias); adding a selective
    /// line too would be ambiguous — offer nothing.
    Qualified,
}

/// Compute the edit that makes `selective` (or, for std modules, the
/// qualified alias) available: extend an existing selective `use`, or
/// insert a new line after the last `use` / the leading comment block.
fn import_state(src: &str, module: &str, selective: Option<&str>) -> ImportState {
    let lines = LineIndex::new(src);
    let imports = current_imports(src);
    let mut last_use_end: Option<u32> = None;
    for (target, names, span) in &imports {
        last_use_end = Some(last_use_end.unwrap_or(0).max(span.end));
        if target != module {
            continue;
        }
        match (selective, names) {
            (None, _) => return ImportState::Imported,
            (Some(n), Some(names)) => {
                if names.iter().any(|x| x == n) {
                    return ImportState::Imported;
                }
                // Extend the brace list; the formatter re-wraps long lines.
                let mut all: Vec<String> = names.clone();
                all.push(n.to_string());
                all.sort();
                let text = format!("use {module} {{ {} }}", all.join(", "));
                return ImportState::Needs(TextEdit {
                    range: span_range(src, &lines, *span),
                    new_text: text,
                });
            }
            (Some(_), None) => return ImportState::Qualified,
        }
    }
    // No import of this module yet: insert a fresh line.
    let line_text = match selective {
        Some(n) => format!("use {module} {{ {n} }}\n"),
        None => format!("use {module}\n"),
    };
    let insert_at = match last_use_end {
        Some(end) => {
            // The line after the last use decl.
            let (line, _) = lines.line_col_utf16(src, end);
            Position::new(line + 1, 0)
        }
        None => {
            // After the leading comment block (and its trailing blank).
            let mut line = 0u32;
            for (i, l) in src.lines().enumerate() {
                if l.trim_start().starts_with("//") || l.trim().is_empty() {
                    line = i as u32 + 1;
                } else {
                    break;
                }
            }
            return ImportState::Needs(TextEdit {
                range: Range { start: Position::new(line, 0), end: Position::new(line, 0) },
                new_text: format!("{line_text}\n"),
            });
        }
    };
    ImportState::Needs(TextEdit {
        range: Range { start: insert_at, end: insert_at },
        new_text: line_text,
    })
}

/// Member lists for the std modules — the same table the checker's hovers
/// use (inga_core::check::std_module_members).
fn std_member_items(target: &str) -> Vec<CompletionItem> {
    inga_core::check::std_module_members(target)
        .iter()
        .map(|(name, detail)| CompletionItem {
            label: name.to_string(),
            kind: Some(CompletionItemKind::FUNCTION),
            detail: Some(detail.to_string()),
            ..Default::default()
        })
        .collect()
}

/// Members of a VALUE by its checked type: struct fields, service methods,
/// map operations, tuple indexes, Int duration/size suffixes.
fn value_member_items(
    cty: &inga_core::check::CType,
    program: &inga_core::ast::Program,
) -> Vec<CompletionItem> {
    use inga_core::ast::Decl;
    use inga_core::check::CType;
    let mut items = Vec::new();
    let field = |name: &str, detail: String| CompletionItem {
        label: name.to_string(),
        kind: Some(CompletionItemKind::FIELD),
        detail: Some(detail),
        ..Default::default()
    };
    match cty {
        CType::Struct(n) => {
            for decl in &program.decls {
                if let Decl::Struct(d) = decl {
                    if &d.name == n {
                        for f in &d.fields {
                            items.push(field(&f.name, format!("field of {n}")));
                        }
                    }
                }
            }
            // Builtin structs (HttpResponse, HttpError, ...) have no decl.
            if items.is_empty() {
                for (fname, fty) in inga_core::check::builtin_struct_fields(n) {
                    items.push(field(fname, format!("{fty} — field of {n}")));
                }
            }
        }
        CType::Service(n) => {
            for decl in &program.decls {
                if let Decl::Service(d) = decl {
                    if &d.name == n {
                        for m in &d.methods {
                            items.push(CompletionItem {
                                label: m.name.clone(),
                                kind: Some(CompletionItemKind::METHOD),
                                detail: Some(format!("method of {n}")),
                                ..Default::default()
                            });
                        }
                    }
                }
            }
        }
        CType::MutMap(..) => {
            for (name, detail) in [
                ("get", "get(key) -> value?"),
                ("set", "set(key, value)"),
                ("delete", "delete(key)"),
                ("size", "size() -> Int"),
            ] {
                items.push(CompletionItem {
                    label: name.to_string(),
                    kind: Some(CompletionItemKind::METHOD),
                    detail: Some(detail.to_string()),
                    ..Default::default()
                });
            }
        }
        CType::Tuple(ts) => {
            for i in 0..ts.len() {
                items.push(field(&i.to_string(), format!("tuple slot {i}")));
            }
        }
        CType::Int => {
            for (suffix, _) in inga_core::check::DURATION_SUFFIXES {
                items.push(field(suffix, "Duration".to_string()));
            }
            for (suffix, _) in inga_core::check::SIZE_SUFFIXES {
                items.push(field(suffix, "Int (bytes)".to_string()));
            }
        }
        _ => {}
    }
    items
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

#[cfg(test)]
mod tests {
    use super::*;

    fn edit_text(state: ImportState) -> TextEdit {
        match state {
            ImportState::Needs(e) => e,
            _ => panic!("expected an edit"),
        }
    }

    #[test]
    fn import_inserts_after_leading_comments() {
        let src = "// a comment\n// more\n\nmain :: () {\n    fiber.fork(1)\n}\n";
        let e = edit_text(import_state(src, "std/fiber", None));
        assert_eq!(e.range.start, Position::new(3, 0));
        assert_eq!(e.new_text, "use std/fiber\n\n");
    }

    #[test]
    fn import_inserts_after_last_use() {
        let src = "use std/graphics\nuse cards { rankName }\n\nmain :: () {\n}\n";
        let e = edit_text(import_state(src, "std/fiber", None));
        assert_eq!(e.range.start, Position::new(2, 0));
        assert_eq!(e.new_text, "use std/fiber\n");
    }

    #[test]
    fn import_extends_selective_list_sorted() {
        let src = "use cards { rankName, suitCol }\n\nmain :: () {\n}\n";
        let e = edit_text(import_state(src, "cards", Some("chipsOf")));
        assert_eq!(e.new_text, "use cards { chipsOf, rankName, suitCol }");
        assert_eq!(e.range.start, Position::new(0, 0));
    }

    #[test]
    fn import_recognizes_existing_and_qualified() {
        let src = "use cards { rankName }\nuse jokers\n";
        assert!(matches!(import_state(src, "cards", Some("rankName")), ImportState::Imported));
        assert!(matches!(import_state(src, "jokers", None), ImportState::Imported));
        assert!(matches!(import_state(src, "jokers", Some("jokerName")), ImportState::Qualified));
    }

    #[test]
    fn sibling_exports_and_quick_fixes_end_to_end() {
        let dir = std::env::temp_dir().join(format!("inga-lsp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("geometry.inga"),
            "pub enum Shape = Circle { Float radius } | Dot\n\npub area :: (Shape s) -> Float {\n    1.0\n}\n\nsecret :: () -> Int {\n    1\n}\n",
        )
        .unwrap();
        let main_path = dir.join("main.inga");
        let main_src = "main :: () {\n    println(area(Circle(2.0)))\n    schedule.fixed(1.millis)\n}\n";
        std::fs::write(&main_path, main_src).unwrap();

        let uri = Url::from_file_path(&main_path).unwrap();
        let mut docs = HashMap::new();
        docs.insert(uri.clone(), main_src.to_string());

        // Exports: pub names + variants via their enum; private excluded.
        let exports = sibling_exports(&uri, &docs);
        let labels: Vec<&str> = exports.iter().map(|e| e.label.as_str()).collect();
        assert!(labels.contains(&"area"), "got: {labels:?}");
        assert!(labels.contains(&"Shape"));
        assert!(labels.contains(&"Circle"));
        assert!(!labels.contains(&"secret"));
        let circle = exports.iter().find(|e| e.label == "Circle").unwrap();
        assert_eq!(circle.import_name, "Shape");

        // Quick fixes on the unknown names.
        let server = Server { documents: docs };
        let params = lsp_types::CodeActionParams {
            text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range { start: Position::new(0, 0), end: Position::new(4, 0) },
            context: Default::default(),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        let actions = server.code_action(params).unwrap();
        let titles: Vec<String> = actions
            .iter()
            .map(|a| match a {
                CodeActionOrCommand::CodeAction(c) => c.title.clone(),
                CodeActionOrCommand::Command(c) => c.title.clone(),
            })
            .collect();
        assert!(
            titles.iter().any(|t| t == "Import `area` from `geometry`"),
            "got: {titles:?}"
        );
        assert!(
            titles.iter().any(|t| t == "Import `Shape` from `geometry` (brings `Circle`)"),
            "got: {titles:?}"
        );
        assert!(titles.iter().any(|t| t == "Add `use std/schedule`"), "got: {titles:?}");

        // Completions carry the auto-import edit.
        let completion_params = lsp_types::CompletionParams {
            text_document_position: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position::new(1, 4),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        };
        let Some(CompletionResponse::Array(items)) = server.completion(completion_params) else {
            panic!("no completions");
        };
        let area = items.iter().find(|i| i.label == "area").expect("area completion");
        let edits = area.additional_text_edits.as_ref().expect("auto-import edit");
        assert_eq!(edits[0].new_text, "use geometry { area }\n\n");
        let sched = items.iter().find(|i| i.label == "schedule").expect("schedule completion");
        assert!(sched.additional_text_edits.is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod member_tests {
    use super::*;

    fn server_with(path: &std::path::Path, src: &str) -> (Server, Url) {
        std::fs::write(path, src).unwrap();
        let uri = Url::from_file_path(path).unwrap();
        let mut docs = HashMap::new();
        docs.insert(uri.clone(), src.to_string());
        (Server { documents: docs }, uri)
    }

    fn labels(items: &[CompletionItem]) -> Vec<String> {
        items.iter().map(|i| i.label.clone()).collect()
    }

    fn offset_of(src: &str, needle: &str) -> u32 {
        (src.find(needle).unwrap() + needle.len()) as u32
    }

    #[test]
    fn dot_members_by_kind() {
        let dir = std::env::temp_dir().join(format!("inga-member-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = "struct Report = { String label, Int total }\n\nservice Log {\n    note :: (String s)\n}\n\nstderrLog :: Log {\n    note :: (s) {\n        println(s)\n    }\n}\n\nmain :: () {\n    provide stderrLog\n    Log log\n    r = Report(\"x\", 1)\n    m = MutMap()\n    m.set(1, 2)\n    pair = (1, \"two\")\n    n = 5\n    println(r.label, pair.0, n)\n    log.note(\"hi\")\n}\n";
        let (server, uri) = server_with(&dir.join("members.inga"), src);

        // Struct fields after `r.`
        let items = server.member_completion(&uri, src, offset_of(src, "println(r.")).unwrap();
        assert_eq!(labels(&items), vec!["label", "total"]);

        // Service methods after `log.`
        let items = server.member_completion(&uri, src, offset_of(src, "log.")).unwrap();
        assert!(labels(&items).contains(&"note".to_string()));

        // Map ops after `m.` (partial `se` already typed)
        let items = server.member_completion(&uri, src, offset_of(src, "m.se")).unwrap();
        assert_eq!(labels(&items), vec!["get", "set", "delete", "size"]);

        // Tuple indexes after `pair.` (numeric partial)
        let items = server.member_completion(&uri, src, offset_of(src, "pair.0")).unwrap();
        assert_eq!(labels(&items), vec!["0", "1"]);

        // Duration/size suffixes after an Int
        let items = server.member_completion(&uri, src, offset_of(src, "n") + 1).unwrap_or_default();
        let _ = items; // `n` alone isn't a member position; check `5.` style below
        let src2 = "main :: () {\n    sleep(5.)\n}\n";
        let (server2, uri2) = server_with(&dir.join("suffix.inga"), src2);
        let items = server2.member_completion(&uri2, src2, offset_of(src2, "5.")).unwrap();
        assert!(labels(&items).contains(&"millis".to_string()));
        assert!(labels(&items).contains(&"kb".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dot_members_for_modules() {
        let dir =
            std::env::temp_dir().join(format!("inga-member-mod-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("cards.inga"),
            "pub rankName :: (Int r) -> String {\n    \"x\"\n}\n\nhidden :: () -> Int {\n    1\n}\n",
        )
        .unwrap();

        // std module, not yet imported: members complete AND carry the
        // auto-import edit.
        let src = "main :: () {\n    schedule.\n}\n";
        let (server, uri) = server_with(&dir.join("m1.inga"), src);
        let items = server.member_completion(&uri, src, offset_of(src, "schedule.")).unwrap();
        assert_eq!(labels(&items), vec!["exponential", "fixed", "upTo"]);
        assert!(items[0].additional_text_edits.is_some(), "auto-import edit expected");

        // std module already imported: no edit attached.
        let src = "use std/fiber\n\nmain :: () {\n    provide Runtime(1)\n    fiber.\n}\n";
        let (server, uri) = server_with(&dir.join("m2.inga"), src);
        let items = server.member_completion(&uri, src, offset_of(src, "fiber.")).unwrap();
        assert!(labels(&items).contains(&"fork".to_string()));
        assert!(labels(&items).contains(&"parMap".to_string()));
        assert!(items[0].additional_text_edits.is_none());

        // http: members complete with auto-import; resp. lists the
        // builtin struct's fields (no decl to walk).
        let src = "main :: () {\n    http.\n}\n";
        let (server, uri) = server_with(&dir.join("m4.inga"), src);
        let items = server.member_completion(&uri, src, offset_of(src, "http.")).unwrap();
        assert!(labels(&items).contains(&"get".to_string()), "got: {:?}", labels(&items));
        assert!(labels(&items).contains(&"openStream".to_string()));
        assert!(items[0].additional_text_edits.is_some());

        let src = "use std/http\n\nmain :: () {\n    provide Http\n    resp = http.get(\"http://x\") |> catch { HttpError -> HttpResponse(0, \"\") }\n    println(resp.)\n}\n";
        let (server, uri) = server_with(&dir.join("m5.inga"), src);
        let items = server.member_completion(&uri, src, offset_of(src, "resp.")).unwrap();
        assert_eq!(labels(&items), vec!["status", "body"]);

        // file module alias: pub members only.
        let src = "use cards\n\nmain :: () {\n    cards.\n}\n";
        let (server, uri) = server_with(&dir.join("m3.inga"), src);
        let items = server.member_completion(&uri, src, offset_of(src, "cards.")).unwrap();
        assert_eq!(labels(&items), vec!["rankName"]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod arm_tests {
    use super::*;

    fn server_with(path: &std::path::Path, src: &str) -> (Server, Url) {
        std::fs::write(path, src).unwrap();
        let uri = Url::from_file_path(path).unwrap();
        let mut docs = HashMap::new();
        docs.insert(uri.clone(), src.to_string());
        (Server { documents: docs }, uri)
    }

    fn labels(items: &[CompletionItem]) -> Vec<String> {
        items.iter().map(|i| i.label.clone()).collect()
    }

    #[test]
    fn catch_and_match_arms_complete() {
        let dir = std::env::temp_dir().join(format!("inga-arm-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // catch: the error row's types (and an enum row entry's variants).
        let src = "struct DbError = { String cause }\nenum NetError = Refused | Reset { Int code }\n\nrisky :: () -> Int ! DbError, NetError {\n    fail DbError(\"x\")\n}\n\nmain :: () {\n    n = risky() |> catch {\n        \n    }\n    println(n)\n}\n";
        let (server, uri) = server_with(&dir.join("a1.inga"), src);
        let offset = (src.find("catch {").unwrap() + "catch {\n        ".len()) as u32;
        let items = server.arm_completion(&uri, src, offset).expect("catch arm items");
        let ls = labels(&items);
        assert!(ls.contains(&"DbError".to_string()), "got: {ls:?}");
        assert!(ls.contains(&"NetError".to_string()));
        assert!(ls.contains(&"Refused".to_string()));
        assert!(ls.contains(&"Reset".to_string()));

        // catch with a partial pattern already typed.
        let src2 = src.replace("catch {\n        \n", "catch {\n        Db\n");
        let (server, uri) = server_with(&dir.join("a2.inga"), &src2);
        let offset = (src2.find("        Db").unwrap() + "        Db".len()) as u32;
        let items = server.arm_completion(&uri, &src2, offset).expect("partial arm items");
        assert!(labels(&items).contains(&"DbError".to_string()));

        // body position (after ->) must NOT offer arm completions.
        let src3 = src.replace("catch {\n        \n", "catch {\n        DbError -> \n");
        let (server, uri) = server_with(&dir.join("a3.inga"), &src3);
        let offset = (src3.find("DbError -> ").unwrap() + "DbError -> ".len()) as u32;
        assert!(server.arm_completion(&uri, &src3, offset).is_none());

        // match on an enum: variants.
        let src = "enum Shape = Circle { Float radius } | Dot\n\nmain :: () {\n    s = Dot\n    n = match s {\n        \n    }\n    println(n)\n}\n";
        let (server, uri) = server_with(&dir.join("a4.inga"), src);
        let offset = (src.find("match s {").unwrap() + "match s {\n        ".len()) as u32;
        let items = server.arm_completion(&uri, src, offset).expect("match arm items");
        assert_eq!(labels(&items), vec!["Circle", "Dot"]);

        // match on an option / an outcome.
        let src = "main :: () {\n    o = at([1], 0)\n    n = match o {\n        \n    }\n    println(n)\n}\n";
        let (server, uri) = server_with(&dir.join("a5.inga"), src);
        let offset = (src.find("match o {").unwrap() + "match o {\n        ".len()) as u32;
        let items = server.arm_completion(&uri, src, offset).expect("option arm items");
        assert_eq!(labels(&items), vec!["Some", "None"]);

        let src = "use std/fiber\n\nstruct WeirdError = { Int n }\n\nrisky :: () -> Int ! WeirdError {\n    fail WeirdError(1)\n}\n\nmain :: () {\n    o = risky() |> fiber.settle\n    n = match o {\n        \n    }\n    println(n)\n}\n";
        let (server, uri) = server_with(&dir.join("a6.inga"), src);
        let offset = (src.find("match o {").unwrap() + "match o {\n        ".len()) as u32;
        let items = server.arm_completion(&uri, src, offset).expect("outcome arm items");
        assert_eq!(labels(&items), vec!["Ok", "Failed"]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
