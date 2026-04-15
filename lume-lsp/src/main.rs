use std::collections::HashMap;

use dashmap::DashMap;
use lume::{
    ast::{self, Expr, ExprKind, NodeId, Program, TopItem},
    error::{LumeError, Span},
    lexer::Lexer,
    loader::{use_path_context, UsePathContext, UsePathKind, STDLIB_MODULES},
    parser,
    types::{
        infer::{elaborate_with_env_partial, TypeEnv},
        Ty,
    },
};
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

// ── Per-document analysis results ────────────────────────────────────────────

/// All type information derived from one document.
struct DocInfo {
    /// NodeId → fully-resolved Ty for every expression in the program.
    node_types: HashMap<NodeId, Ty>,
    /// All (Span, NodeId) pairs sorted by span length (shortest first) for
    /// efficient "find innermost expression at cursor" queries.
    span_index: Vec<(Span, NodeId)>,
    /// All names in scope at the end of the file (builtins + imports + lets).
    top_env: TypeEnv,
}

// ── LSP backend ───────────────────────────────────────────────────────────────

struct Backend {
    client: Client,
    documents: DashMap<Url, String>,
    doc_info: DashMap<Url, DocInfo>,
}

fn span_to_range(span: &Span) -> Range {
    let line = span.line.saturating_sub(1) as u32;
    let col = span.col.saturating_sub(1) as u32;
    Range {
        start: Position {
            line,
            character: col,
        },
        end: Position {
            line,
            character: col + span.len as u32,
        },
    }
}

fn error_to_diagnostic(err: LumeError) -> Diagnostic {
    let (range, message) = match &err {
        LumeError::Lex(e) => (span_to_range(&e.span), e.to_string()),
        LumeError::Parse(e) => (span_to_range(&e.span), e.to_string()),
        LumeError::Type(e) => (span_to_range(&e.span), e.error.to_string()),
    };
    Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        message,
        ..Default::default()
    }
}

/// Run the full pipeline on `src`, returning a `DocInfo` and any diagnostics.
fn analyse(uri: &Url, src: &str) -> (Option<DocInfo>, Vec<Diagnostic>) {
    let tokens = match Lexer::new(src).tokenize() {
        Ok(t) => t,
        Err(e) => return (None, vec![error_to_diagnostic(LumeError::Lex(e))]),
    };
    let program = match parser::parse_program(&tokens) {
        Ok(p) => p,
        Err(e) => return (None, vec![error_to_diagnostic(LumeError::Parse(e))]),
    };
    let path = uri.to_file_path().ok();
    let (node_types, top_env, type_errors) =
        elaborate_with_env_partial(&program, path.as_deref());
    let span_index = collect_spans(&program);
    let doc_info = Some(DocInfo {
        node_types,
        span_index,
        top_env,
    });
    let diagnostics = type_errors
        .into_iter()
        .map(|e| error_to_diagnostic(LumeError::Type(e)))
        .collect();
    (doc_info, diagnostics)
}

// ── Span index ────────────────────────────────────────────────────────────────

/// Walk every `Expr` in `program` and collect `(Span, NodeId)` pairs.
/// The result is sorted by span length (shortest first) so that hover can find
/// the most-specific expression at the cursor by scanning front-to-back.
fn collect_spans(program: &Program) -> Vec<(Span, NodeId)> {
    let mut out = Vec::new();
    for u in &program.uses {
        match &u.binding {
            ast::UseBinding::Ident(_, span, id) => {
                out.push((span.clone(), *id));
            }
            ast::UseBinding::Record(rp) => {
                for fp in &rp.fields {
                    out.push((fp.span.clone(), fp.node_id));
                }
            }
        }
    }
    for item in &program.items {
        match item {
            TopItem::Binding(b) => {
                collect_pattern_spans(&b.pattern, &mut out);
                collect_expr_spans(&b.value, &mut out);
            }
            TopItem::BindingGroup(bs) => {
                for b in bs {
                    collect_pattern_spans(&b.pattern, &mut out);
                    collect_expr_spans(&b.value, &mut out);
                }
            }
            TopItem::TypeDef(_) => {}
        }
    }
    collect_expr_spans(&program.exports, &mut out);
    // Sort by (len ASC, NodeId DESC).
    // Shortest span first finds the most-specific expression at the cursor.
    // For equal-length spans (e.g. nested Apply nodes sharing the func token),
    // higher NodeId wins because assign_node_ids is pre-order: the innermost
    // child (the Ident leaf) always receives a higher id than its parent Apply.
    out.sort_by(|(sa, ia), (sb, ib)| sa.len.cmp(&sb.len).then(ib.cmp(ia)));
    out
}

fn collect_pattern_spans(pat: &ast::Pattern, out: &mut Vec<(Span, NodeId)>) {
    match pat {
        ast::Pattern::Ident(_, span, id) => {
            out.push((span.clone(), *id));
        }
        ast::Pattern::Record(rp) => {
            for fp in &rp.fields {
                out.push((fp.span.clone(), fp.node_id));
                if let Some(sub) = &fp.pattern {
                    collect_pattern_spans(sub, out);
                }
            }
        }
        ast::Pattern::Variant {
            payload: Some(p), ..
        } => collect_pattern_spans(p, out),
        ast::Pattern::List(lp) => {
            for p in &lp.elements {
                collect_pattern_spans(p, out);
            }
        }
        _ => {}
    }
}

fn collect_expr_spans(expr: &Expr, out: &mut Vec<(Span, NodeId)>) {
    out.push((expr.span.clone(), expr.id));
    match &expr.kind {
        ExprKind::List(exprs) => {
            for e in exprs {
                collect_expr_spans(e, out);
            }
        }
        ExprKind::Record { base, fields, .. } => {
            if let Some(b) = base {
                collect_expr_spans(b, out);
            }
            for f in fields {
                out.push((f.name_span.clone(), f.name_node_id));
                if let Some(v) = &f.value {
                    collect_expr_spans(v, out);
                }
            }
        }
        ExprKind::FieldAccess { record, .. } => {
            collect_expr_spans(record, out);
        }
        ExprKind::Variant {
            payload: Some(p), ..
        } => {
            collect_expr_spans(p, out);
        }
        ExprKind::Lambda { param, body } => {
            collect_pattern_spans(param, out);
            collect_expr_spans(body, out);
        }
        ExprKind::Apply { func, arg } => {
            collect_expr_spans(func, out);
            collect_expr_spans(arg, out);
        }
        ExprKind::Binary { left, right, .. } => {
            collect_expr_spans(left, out);
            collect_expr_spans(right, out);
        }
        ExprKind::Unary { operand, .. } => {
            collect_expr_spans(operand, out);
        }
        ExprKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            collect_expr_spans(cond, out);
            collect_expr_spans(then_branch, out);
            collect_expr_spans(else_branch, out);
        }
        ExprKind::Match(arms) => {
            for arm in arms {
                collect_pattern_spans(&arm.pattern, out);
                if let Some(g) = &arm.guard {
                    collect_expr_spans(g, out);
                }
                collect_expr_spans(&arm.body, out);
            }
        }
        // Leaves: Number, Text, Bool, Ident, Variant { payload: None }
        _ => {}
    }
}

// ── Hover lookup ─────────────────────────────────────────────────────────────

/// Find the type of the innermost (shortest-span) expression that contains
/// `pos` in `doc`.
///
/// Spans are 1-indexed (line and col); LSP positions are 0-indexed.
fn type_at(pos: Position, doc: &DocInfo) -> Option<Ty> {
    let line = pos.line as usize + 1; // convert to 1-indexed
    let col = pos.character as usize + 1;

    // span_index is sorted shortest-first; the first match is the most specific.
    doc.span_index
        .iter()
        .find(|(span, _)| span.line == line && span.col <= col && col < span.col + span.len)
        .and_then(|(_, id)| doc.node_types.get(id).cloned())
}

/// Return the identifier word under the cursor (for the hover label).
fn word_at(text: &str, line: u32, character: u32) -> Option<&str> {
    let line_text = text.lines().nth(line as usize)?;
    let col = character as usize;
    if col > line_text.len() {
        return None;
    }
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let start = line_text[..col]
        .rfind(|c: char| !is_ident(c))
        .map(|i| i + 1)
        .unwrap_or(0);
    let end = line_text[col..]
        .find(|c: char| !is_ident(c))
        .map(|i| i + col)
        .unwrap_or(line_text.len());
    if start >= end {
        None
    } else {
        Some(&line_text[start..end])
    }
}

// ── Completion helpers ────────────────────────────────────────────────────────

/// The completion context derived from the text before the cursor.
enum CompletionCtx {
    /// Suppress completions (e.g. cursor is on a binding name after `let`).
    None,
    /// Cursor is in a position like `record.` or `record.partial` — suggest fields.
    FieldAccess {
        record: String,
        prefix: String,
        replace_range: Range,
    },
    /// Cursor is on a plain identifier — suggest all in-scope names.
    Ident {
        prefix: String,
        replace_range: Range,
    },
    /// Cursor is inside the path string of a `use` declaration.
    UsePath(UsePathContext),
}

/// Analyse the text before `pos` to determine what kind of completion is wanted.
///
/// Handles both the immediate-dot case (`math.`) and the continuing case
/// (`math.po`) so field completions keep working as the user types.
fn completion_ctx(text: &str, pos: Position) -> CompletionCtx {
    let col = pos.character as usize;
    let line = match text.lines().nth(pos.line as usize) {
        Some(l) => l,
        None => {
            return CompletionCtx::Ident {
                prefix: String::new(),
                replace_range: Range {
                    start: pos,
                    end: pos,
                },
            };
        }
    };
    let before = &line[..col.min(line.len())];

    // Check for use-path context before anything else.
    if let Some(ctx) = use_path_context(before) {
        return CompletionCtx::UsePath(ctx);
    }
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';

    // Find the partial word at the cursor (e.g. "po" in "math.po|").
    let partial_start = before
        .rfind(|c: char| !is_ident(c))
        .map(|i| i + 1)
        .unwrap_or(0);
    let prefix = before[partial_start..].to_string();
    let replace_range = Range {
        start: Position {
            line: pos.line,
            character: partial_start as u32,
        },
        end: pos,
    };

    // Suppress completions when the cursor is on a binding name (right after `let`).
    // Detect by finding the last non-empty ident token before the partial word.
    let before_prefix = before[..partial_start].trim_end();
    let last_token = before_prefix
        .rsplit(|c: char| !c.is_alphanumeric() && c != '_')
        .find(|s| !s.is_empty())
        .unwrap_or("");
    if last_token == "let" {
        return CompletionCtx::None;
    }

    // If the char immediately before the partial word is '.', it's field access.
    if partial_start > 0 && before.as_bytes()[partial_start - 1] == b'.' {
        let before_dot = &before[..partial_start - 1];
        let rec_start = before_dot
            .rfind(|c: char| !is_ident(c))
            .map(|i| i + 1)
            .unwrap_or(0);
        let record = &before_dot[rec_start..];
        if !record.is_empty() {
            return CompletionCtx::FieldAccess {
                record: record.to_string(),
                prefix,
                replace_range,
            };
        }
    }

    CompletionCtx::Ident {
        prefix,
        replace_range,
    }
}

/// Return field completion items for a record variable name.
fn field_completions(
    record: &str,
    prefix: &str,
    replace_range: Range,
    doc: &DocInfo,
) -> Vec<CompletionItem> {
    let ty = match doc.top_env.lookup(record) {
        Some(scheme) => &scheme.ty,
        None => return vec![],
    };
    if let Ty::Record(row) = ty {
        let lower = prefix.to_lowercase();
        row.fields
            .iter()
            .filter(|(name, _)| lower.is_empty() || name.to_lowercase().contains(&lower))
            .map(|(name, field_ty)| CompletionItem {
                label: name.clone(),
                filter_text: Some(name.clone()),
                insert_text: Some(name.clone()),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range: replace_range.clone(),
                    new_text: name.clone(),
                })),
                detail: Some(field_ty.to_string()),
                kind: Some(CompletionItemKind::FIELD),
                ..Default::default()
            })
            .collect()
    } else {
        vec![]
    }
}

/// All in-scope identifier completions (builtins + imports + let bindings).
///
/// `prefix` is the partially-typed word; we include items that are a
/// case-insensitive substring match so editors get useful results even
/// when the user hasn't typed from the start of the identifier.
fn ident_completions(doc: &DocInfo, prefix: &str, replace_range: Range) -> Vec<CompletionItem> {
    let lower = prefix.to_lowercase();
    doc.top_env
        .iter()
        .filter_map(|(name, scheme)| {
            // Include all items when prefix is empty; otherwise require substring match.
            if !lower.is_empty() && !name.to_lowercase().contains(&lower) {
                return None;
            }
            let detail = scheme.ty.to_string();
            let kind = if matches!(scheme.ty, Ty::Func(..)) {
                CompletionItemKind::FUNCTION
            } else {
                CompletionItemKind::VARIABLE
            };
            Some(CompletionItem {
                label: name.clone(),
                filter_text: Some(name.clone()),
                insert_text: Some(name.clone()),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range: replace_range.clone(),
                    new_text: name.clone(),
                })),
                detail: Some(detail),
                kind: Some(kind),
                ..Default::default()
            })
        })
        .collect()
}

/// Completion items for `use … = "lume:<prefix>"`.
fn stdlib_path_completions(prefix: &str, prefix_col: usize, pos: Position) -> Vec<CompletionItem> {
    let replace_range = Range {
        start: Position { line: pos.line, character: prefix_col as u32 },
        end: pos,
    };
    let lower = prefix.to_lowercase();
    STDLIB_MODULES
        .iter()
        .filter_map(|&m| {
            let name = m.strip_prefix("lume:").unwrap();
            if lower.is_empty() || name.contains(&*lower) {
                Some(CompletionItem {
                    label: name.to_string(),
                    detail: Some("stdlib".to_string()),
                    kind: Some(CompletionItemKind::MODULE),
                    text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                        range: replace_range,
                        new_text: name.to_string(),
                    })),
                    ..Default::default()
                })
            } else {
                None
            }
        })
        .collect()
}

/// Completion items for `use … = "./<prefix>"` (file-system paths).
/// Lists `.lume` files and subdirectories relative to `doc_uri`.
fn file_path_completions(
    doc_uri: &Url,
    prefix: &str,
    prefix_col: usize,
    pos: Position,
) -> Vec<CompletionItem> {
    let replace_range = Range {
        start: Position { line: pos.line, character: prefix_col as u32 },
        end: pos,
    };
    let doc_path = match doc_uri.to_file_path() {
        Ok(p) => p,
        Err(_) => return vec![],
    };
    let doc_dir = match doc_path.parent() {
        Some(d) => d,
        None => return vec![],
    };

    // Split prefix into a directory part and the name fragment being typed.
    let (dir_part, name_part) = match prefix.rfind('/') {
        Some(i) => (&prefix[..=i], &prefix[i + 1..]),
        None => ("", prefix),
    };

    let search_dir = if dir_part.is_empty() {
        doc_dir.to_path_buf()
    } else {
        doc_dir.join(dir_part.trim_start_matches("./"))
    };

    let entries = match std::fs::read_dir(&search_dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };

    let lower = name_part.to_lowercase();
    let mut items = Vec::new();
    for entry in entries.flatten() {
        let file_name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if entry.path().is_dir() {
            if lower.is_empty() || file_name.to_lowercase().contains(&*lower) {
                let label = format!("{}{}/", dir_part, file_name);
                items.push(CompletionItem {
                    label: label.clone(),
                    detail: Some("directory".to_string()),
                    kind: Some(CompletionItemKind::FOLDER),
                    text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                        range: replace_range,
                        new_text: label,
                    })),
                    ..Default::default()
                });
            }
        } else if let Some(stem) = file_name.strip_suffix(".lume") {
            if lower.is_empty() || stem.to_lowercase().contains(&*lower) {
                let label = format!("{}{}", dir_part, stem);
                items.push(CompletionItem {
                    label: label.clone(),
                    detail: Some("local module".to_string()),
                    kind: Some(CompletionItemKind::FILE),
                    text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                        range: replace_range,
                        new_text: label,
                    })),
                    ..Default::default()
                });
            }
        }
    }
    items
}

// ── Language server impl ──────────────────────────────────────────────────────

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![
                        ".".to_string(),
                        "\"".to_string(),
                        ":".to_string(),
                        "/".to_string(),
                    ]),
                    resolve_provider: Some(false),
                    ..Default::default()
                }),
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "lume-lsp started")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = params.text_document.text;
        self.refresh(&uri, &text).await;
        self.documents.insert(uri, text);
    }

    async fn did_change(&self, mut params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = params.content_changes.remove(0).text;
        self.refresh(&uri, &text).await;
        self.documents.insert(uri, text);
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.documents.remove(&uri);
        self.doc_info.remove(&uri);
        self.client.publish_diagnostics(uri, vec![], None).await;
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = &params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;

        let text = match self.documents.get(uri) {
            Some(t) => t.clone(),
            None => return Ok(None),
        };

        let ctx = completion_ctx(&text, pos);

        if matches!(ctx, CompletionCtx::None) {
            return Ok(None);
        }

        // Use-path completions don't need type information — serve them even
        // when the document has errors and doc_info is unavailable.
        if let CompletionCtx::UsePath(up) = ctx {
            let ctx_label = format!("UsePath(prefix={:?})", up.prefix);
            let items = match up.kind {
                UsePathKind::Stdlib => stdlib_path_completions(&up.prefix, up.prefix_col, pos),
                UsePathKind::File => file_path_completions(uri, &up.prefix, up.prefix_col, pos),
            };
            self.client
                .log_message(
                    MessageType::LOG,
                    format!(
                        "completion pos={},{} ctx={ctx_label} items={}",
                        pos.line, pos.character, items.len()
                    ),
                )
                .await;
            return Ok(Some(CompletionResponse::List(CompletionList {
                is_incomplete: false,
                items,
            })));
        }

        let doc = match self.doc_info.get(uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let ctx_label = match &ctx {
            CompletionCtx::FieldAccess { record, prefix, .. } => {
                format!("FieldAccess(record={record}, prefix={prefix:?})")
            }
            CompletionCtx::Ident { prefix, .. } => format!("Ident(prefix={prefix:?})"),
            CompletionCtx::None | CompletionCtx::UsePath(_) => unreachable!(),
        };

        let items = match ctx {
            CompletionCtx::FieldAccess {
                record,
                prefix,
                replace_range,
            } => field_completions(&record, &prefix, replace_range, &doc),
            CompletionCtx::Ident {
                prefix,
                replace_range,
            } => ident_completions(&doc, &prefix, replace_range),
            CompletionCtx::None | CompletionCtx::UsePath(_) => unreachable!(),
        };

        self.client
            .log_message(
                MessageType::LOG,
                format!(
                    "completion pos={},{} ctx={ctx_label} items={}",
                    pos.line,
                    pos.character,
                    items.len()
                ),
            )
            .await;

        Ok(Some(CompletionResponse::List(CompletionList {
            is_incomplete: true,
            items,
        })))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;

        let text = match self.documents.get(uri) {
            Some(entry) => entry.clone(),
            None => return Ok(None),
        };

        let doc = match self.doc_info.get(uri) {
            Some(entry) => entry,
            None => return Ok(None),
        };

        let ty = match type_at(pos, &doc) {
            Some(t) => t,
            None => return Ok(None),
        };

        // Use the identifier under the cursor as a label only when it is a
        // genuine identifier (starts with a letter or `_`), not a number fragment.
        let label = match word_at(&text, pos.line, pos.character) {
            Some(w) if w.starts_with(|c: char| c.is_alphabetic() || c == '_') => {
                format!("{w} : {ty}")
            }
            _ => ty.to_string(),
        };

        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: format!("```\n{label}\n```"),
            }),
            range: None,
        }))
    }
}

impl Backend {
    async fn refresh(&self, uri: &Url, text: &str) {
        // Run the blocking, deeply-recursive type-checker on a dedicated OS
        // thread so it gets a full platform stack and doesn't starve the async
        // executor.
        let uri_owned = uri.clone();
        let text_owned = text.to_string();
        let (doc_info, diagnostics) =
            tokio::task::spawn_blocking(move || analyse(&uri_owned, &text_owned))
                .await
                .unwrap_or_else(|_| (None, vec![]));

        if let Some(info) = doc_info {
            self.doc_info.insert(uri.clone(), info);
        }
        self.client
            .publish_diagnostics(uri.clone(), diagnostics, None)
            .await;
    }
}

#[tokio::main]
async fn main() {
    // Log to stderr — Zed captures this and shows it in the LSP log panel.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(|client| Backend {
        client,
        documents: DashMap::new(),
        doc_info: DashMap::new(),
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
