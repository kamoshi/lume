use std::collections::HashMap;

use dashmap::DashMap;
use lume::{
    ast::{self, Expr, ExprKind, NodeId, Program, TopItem, TraitDef},
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
    /// Trait definitions visible in this file (local + imported).
    trait_env: HashMap<String, TraitDef>,
    /// NodeId → (trait_name, method_name) for every TraitCall expression.
    trait_calls: HashMap<NodeId, (String, String)>,
    /// Extra hover labels for nodes without entries in `node_types`
    /// (trait method declarations, type definitions, etc.).
    extra_hovers: Vec<(Span, String)>,
    /// Name → doc comment string, built from AST `doc` fields.
    doc_comments: HashMap<String, String>,
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
    let (node_types, top_env, trait_env, type_errors) =
        elaborate_with_env_partial(&program, path.as_deref());
    let span_index = collect_spans(&program);
    let trait_calls = collect_trait_calls(&program);
    let extra_hovers = collect_extra_hovers(&program, &trait_env);
    let doc_comments = collect_doc_comments(&program);
    let doc_info = Some(DocInfo {
        node_types,
        span_index,
        top_env,
        trait_env,
        trait_calls,
        extra_hovers,
        doc_comments,
    });
    let diagnostics = type_errors
        .into_iter()
        .map(|e| error_to_diagnostic(LumeError::Type(e)))
        .collect();
    (doc_info, diagnostics)
}

/// Walk every expression and collect NodeId → (trait_name, method_name) for
/// all `TraitCall` nodes.
fn collect_trait_calls(program: &Program) -> HashMap<NodeId, (String, String)> {
    let mut out = HashMap::new();
    fn walk(expr: &Expr, out: &mut HashMap<NodeId, (String, String)>) {
        if let ExprKind::TraitCall { trait_name, method_name } = &expr.kind {
            out.insert(expr.id, (trait_name.clone(), method_name.clone()));
        }
        match &expr.kind {
            ExprKind::List(es) => es.iter().for_each(|e| walk(e, out)),
            ExprKind::Record { base, fields, .. } => {
                if let Some(b) = base { walk(b, out); }
                for f in fields { if let Some(v) = &f.value { walk(v, out); } }
            }
            ExprKind::FieldAccess { record, .. } => walk(record, out),
            ExprKind::Variant { payload: Some(p), .. } => walk(p, out),
            ExprKind::Lambda { body, .. } => walk(body, out),
            ExprKind::Apply { func, arg } => { walk(func, out); walk(arg, out); }
            ExprKind::Binary { left, right, .. } => { walk(left, out); walk(right, out); }
            ExprKind::Unary { operand, .. } => walk(operand, out),
            ExprKind::If { cond, then_branch, else_branch } => {
                walk(cond, out); walk(then_branch, out); walk(else_branch, out);
            }
            ExprKind::Match(arms) => arms.iter().for_each(|a| {
                if let Some(g) = &a.guard { walk(g, out); }
                walk(&a.body, out);
            }),
            ExprKind::MatchExpr { scrutinee, arms } => {
                walk(scrutinee, out);
                arms.iter().for_each(|a| {
                    if let Some(g) = &a.guard { walk(g, out); }
                    walk(&a.body, out);
                });
            }
            ExprKind::LetIn { value, body, .. } => { walk(value, out); walk(body, out); }
            _ => {}
        }
    }
    for item in &program.items {
        match item {
            TopItem::Binding(b) => walk(&b.value, &mut out),
            TopItem::BindingGroup(bs) => bs.iter().for_each(|b| walk(&b.value, &mut out)),
            TopItem::ImplDef(id) => id.methods.iter().for_each(|m| walk(&m.value, &mut out)),
            _ => {}
        }
    }
    walk(&program.exports, &mut out);
    out
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
            TopItem::TypeDef(_) | TopItem::TraitDef(_) => {}
            TopItem::ImplDef(id) => {
                for m in &id.methods {
                    collect_pattern_spans(&m.pattern, &mut out);
                    collect_expr_spans(&m.value, &mut out);
                }
            }
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
        ExprKind::MatchExpr { scrutinee, arms } => {
            collect_expr_spans(scrutinee, out);
            for arm in arms {
                collect_pattern_spans(&arm.pattern, out);
                if let Some(g) = &arm.guard {
                    collect_expr_spans(g, out);
                }
                collect_expr_spans(&arm.body, out);
            }
        }
        ExprKind::LetIn { pattern, value, body } => {
            collect_pattern_spans(pattern, out);
            collect_expr_spans(value, out);
            collect_expr_spans(body, out);
        }
        // Leaves: Number, Text, Bool, Ident, Variant { payload: None }
        _ => {}
    }
}

// ── Extra hovers (trait declarations, type defs, etc.) ─────────────────────

/// Build span-based hover entries for nodes that the type checker doesn't track
/// in `node_types` — e.g. trait method declarations.
fn collect_extra_hovers(
    program: &Program,
    _trait_env: &HashMap<String, TraitDef>,
) -> Vec<(Span, String)> {
    let mut out = Vec::new();
    for item in &program.items {
        if let TopItem::TraitDef(td) = item {
            for m in &td.methods {
                let label = format!(
                    "{} : {} {} => {}",
                    m.name, td.name, td.type_param, m.ty
                );
                out.push((m.name_span.clone(), label));
            }
        }
    }
    out
}

/// Build a name → doc-comment map from the AST `doc` fields on definitions.
fn collect_doc_comments(program: &Program) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for item in &program.items {
        match item {
            TopItem::Binding(b) => {
                if let (ast::Pattern::Ident(name, _, _), Some(doc)) = (&b.pattern, &b.doc) {
                    out.insert(name.clone(), doc.clone());
                }
            }
            TopItem::BindingGroup(bs) => {
                for b in bs {
                    if let (ast::Pattern::Ident(name, _, _), Some(doc)) = (&b.pattern, &b.doc) {
                        out.insert(name.clone(), doc.clone());
                    }
                }
            }
            TopItem::TraitDef(td) => {
                if let Some(doc) = &td.doc {
                    out.insert(td.name.clone(), doc.clone());
                }
                for m in &td.methods {
                    if let Some(doc) = &m.doc {
                        out.insert(m.name.clone(), doc.clone());
                    }
                }
            }
            TopItem::ImplDef(id) => {
                if let Some(doc) = &id.doc {
                    let key = format!("{}_{}", id.trait_name, id.type_name);
                    out.insert(key, doc.clone());
                }
                for m in &id.methods {
                    if let (ast::Pattern::Ident(name, _, _), Some(doc)) = (&m.pattern, &m.doc) {
                        let key = format!("{}_{}.{}", id.trait_name, id.type_name, name);
                        out.insert(key, doc.clone());
                    }
                }
            }
            TopItem::TypeDef(td) => {
                if let Some(doc) = &td.doc {
                    out.insert(td.name.clone(), doc.clone());
                }
            }
        }
    }
    out
}

// ── Hover lookup ─────────────────────────────────────────────────────────────

/// Find the type and NodeId of the innermost expression at `pos`.
///
/// Spans are 1-indexed (line and col); LSP positions are 0-indexed.
fn type_at_with_id(pos: Position, doc: &DocInfo) -> Option<(NodeId, Ty)> {
    let line = pos.line as usize + 1;
    let col = pos.character as usize + 1;
    doc.span_index
        .iter()
        .find(|(span, _)| span.line == line && span.col <= col && col < span.col + span.len)
        .and_then(|(_, id)| doc.node_types.get(id).map(|ty| (*id, ty.clone())))
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
    /// Cursor is in a position like `record.` or `record.partial` - suggest fields.
    FieldAccess {
        record: String,
        prefix: String,
        replace_range: Range,
    },
    /// Cursor is in a position like `TraitName.` or `TraitName.partial`.
    TraitAccess {
        trait_name: String,
        prefix: String,
        replace_range: Range,
    },
    /// Cursor is on a plain identifier - suggest all in-scope names.
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

    // If the char immediately before the partial word is '.', it's field or trait access.
    if partial_start > 0 && before.as_bytes()[partial_start - 1] == b'.' {
        let before_dot = &before[..partial_start - 1];
        let rec_start = before_dot
            .rfind(|c: char| !is_ident(c))
            .map(|i| i + 1)
            .unwrap_or(0);
        let record = &before_dot[rec_start..];
        if !record.is_empty() {
            // If the name starts with an uppercase letter it's a trait access.
            if record.starts_with(|c: char| c.is_uppercase()) {
                return CompletionCtx::TraitAccess {
                    trait_name: record.to_string(),
                    prefix,
                    replace_range,
                };
            }
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

/// Format a trait method type with its constraint for display.
/// e.g. trait `ToText a` with method `toNum : a -> Num` → `ToText a => a -> Num`
fn format_trait_method_ty(trait_def: &TraitDef, method_ty: &str) -> String {
    format!("{} {} => {}", trait_def.name, trait_def.type_param, method_ty)
}

/// Return completion items for `TraitName.` — the methods of that trait.
fn trait_completions(
    trait_name: &str,
    prefix: &str,
    replace_range: Range,
    doc: &DocInfo,
) -> Vec<CompletionItem> {
    let trait_def = match doc.trait_env.get(trait_name) {
        Some(td) => td,
        None => return vec![],
    };
    let lower = prefix.to_lowercase();
    trait_def
        .methods
        .iter()
        .filter(|m| lower.is_empty() || m.name.to_lowercase().contains(&lower))
        .map(|m| {
            let detail = format_trait_method_ty(trait_def, &m.ty.to_string());
            CompletionItem {
                label: m.name.clone(),
                filter_text: Some(m.name.clone()),
                insert_text: Some(m.name.clone()),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range: replace_range,
                    new_text: m.name.clone(),
                })),
                detail: Some(detail),
                kind: Some(CompletionItemKind::METHOD),
                ..Default::default()
            }
        })
        .collect()
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
                    range: replace_range,
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
            let detail = scheme.to_string();
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
                    range: replace_range,
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
        start: Position {
            line: pos.line,
            character: prefix_col as u32,
        },
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
        start: Position {
            line: pos.line,
            character: prefix_col as u32,
        },
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

        // Use-path completions don't need type information - serve them even
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
                        pos.line,
                        pos.character,
                        items.len()
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
            CompletionCtx::TraitAccess { trait_name, prefix, .. } => {
                format!("TraitAccess(trait={trait_name}, prefix={prefix:?})")
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
            CompletionCtx::TraitAccess {
                trait_name,
                prefix,
                replace_range,
            } => trait_completions(&trait_name, &prefix, replace_range, &doc),
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

        let label = if let Some((node_id, ty)) = type_at_with_id(pos, &doc) {
            // Check if this node is a TraitCall — if so, show the constrained type.
            if let Some((trait_name, method_name)) = doc.trait_calls.get(&node_id) {
                if let Some(trait_def) = doc.trait_env.get(trait_name) {
                    if let Some(method) = trait_def.methods.iter().find(|m| &m.name == method_name) {
                        let constrained = format_trait_method_ty(trait_def, &method.ty.to_string());
                        format!("{method_name} : {constrained}")
                    } else {
                        format!("{method_name} : {ty}")
                    }
                } else {
                    format!("{method_name} : {ty}")
                }
            } else {
                match word_at(&text, pos.line, pos.character) {
                    Some(w) if w.starts_with(|c: char| c.is_alphabetic() || c == '_') => {
                        if let Some(scheme) = doc.top_env.lookup(w) {
                            format!("{w} : {scheme}")
                        } else {
                            format!("{w} : {ty}")
                        }
                    }
                    _ => ty.to_string(),
                }
            }
        } else {
            // No type in node_types — try extra_hovers (trait methods, etc.)
            let lsp_line = pos.line as usize + 1;
            let lsp_col = pos.character as usize + 1;
            if let Some((_, label)) = doc.extra_hovers.iter().find(|(span, _)| {
                span.line == lsp_line && span.col <= lsp_col && lsp_col < span.col + span.len
            }) {
                label.clone()
            } else {
                // Last resort: try top_env by word under cursor
                match word_at(&text, pos.line, pos.character) {
                    Some(w) if w.starts_with(|c: char| c.is_alphabetic() || c == '_') => {
                        if let Some(scheme) = doc.top_env.lookup(w) {
                            format!("{w} : {scheme}")
                        } else {
                            return Ok(None);
                        }
                    }
                    _ => return Ok(None),
                }
            }
        };

        // Look up doc comment by name under cursor
        let doc_comment = word_at(&text, pos.line, pos.character)
            .and_then(|w| doc.doc_comments.get(w))
            .cloned()
            .unwrap_or_default();

        let mut value = format!("```\n{label}\n```");
        if !doc_comment.is_empty() {
            value.push_str("\n\n---\n\n");
            value.push_str(&doc_comment);
        }

        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
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
    // Log to stderr - Zed captures this and shows it in the LSP log panel.
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
