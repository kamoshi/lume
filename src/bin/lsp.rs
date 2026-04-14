use std::collections::HashMap;

use dashmap::DashMap;
use lume::{
    ast::{self, Expr, ExprKind, NodeId, Program, TopItem},
    error::{LumeError, Span},
    lexer::Lexer,
    parser,
    types::{infer::elaborate, Ty},
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
        start: Position { line, character: col },
        end: Position { line, character: col + span.len as u32 },
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
    match elaborate(&program, path.as_deref()) {
        Ok((node_types, _)) => {
            let span_index = collect_spans(&program);
            (Some(DocInfo { node_types, span_index }), vec![])
        }
        Err(e) => (None, vec![error_to_diagnostic(LumeError::Type(e))]),
    }
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
        if let TopItem::Binding(b) = item {
            collect_pattern_spans(&b.pattern, &mut out);
            collect_expr_spans(&b.value, &mut out);
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
        ast::Pattern::Variant { payload: Some(p), .. } => collect_pattern_spans(p, out),
        ast::Pattern::List(lp) => {
            for p in &lp.elements { collect_pattern_spans(p, out); }
        }
        _ => {}
    }
}

fn collect_expr_spans(expr: &Expr, out: &mut Vec<(Span, NodeId)>) {
    out.push((expr.span.clone(), expr.id));
    match &expr.kind {
        ExprKind::List(exprs) => {
            for e in exprs { collect_expr_spans(e, out); }
        }
        ExprKind::Record { base, fields, .. } => {
            if let Some(b) = base { collect_expr_spans(b, out); }
            for f in fields {
                out.push((f.name_span.clone(), f.name_node_id));
                if let Some(v) = &f.value { collect_expr_spans(v, out); }
            }
        }
        ExprKind::FieldAccess { record, .. } => {
            collect_expr_spans(record, out);
        }
        ExprKind::Variant { payload: Some(p), .. } => {
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
        ExprKind::If { cond, then_branch, else_branch } => {
            collect_expr_spans(cond, out);
            collect_expr_spans(then_branch, out);
            collect_expr_spans(else_branch, out);
        }
        ExprKind::Match(arms) => {
            for arm in arms {
                collect_pattern_spans(&arm.pattern, out);
                if let Some(g) = &arm.guard { collect_expr_spans(g, out); }
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
    let col  = pos.character as usize + 1;

    // span_index is sorted shortest-first; the first match is the most specific.
    doc.span_index.iter()
        .find(|(span, _)| {
            span.line == line
                && span.col <= col
                && col < span.col + span.len
        })
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
    if start >= end { None } else { Some(&line_text[start..end]) }
}

// ── Language server impl ──────────────────────────────────────────────────────

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                hover_provider: Some(HoverProviderCapability::Simple(true)),
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

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let pos  = params.text_document_position_params.position;

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
        let (doc_info, diagnostics) = analyse(uri, text);
        if let Some(info) = doc_info {
            self.doc_info.insert(uri.clone(), info);
        } else {
            self.doc_info.remove(uri);
        }
        self.client
            .publish_diagnostics(uri.clone(), diagnostics, None)
            .await;
    }
}

#[tokio::main]
async fn main() {
    let stdin  = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(|client| Backend {
        client,
        documents: DashMap::new(),
        doc_info:  DashMap::new(),
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
