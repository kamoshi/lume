use std::collections::HashMap;

use lume_core::{
    ast::{self, Expr, ExprKind, NodeId, Program, TopItem, TraitDef},
    error::{LumeError, Span},
    lexer::Lexer,
    loader::{parse_pragmas, PragmaWarning},
    parser,
    types::{
        infer::{elaborate_with_env_partial, TypeEnv},
        Ty, TyVar,
    },
};
use tower_lsp::lsp_types::*;

// ── Per-document analysis results ────────────────────────────────────────────

/// All type information derived from one document.
pub struct DocInfo {
    /// NodeId → fully-resolved Ty for every expression in the program.
    pub node_types: HashMap<NodeId, Ty>,
    /// Line number (1-indexed) → spans on that line, sorted by span length
    /// (shortest first) for efficient "find innermost expression" queries.
    pub span_index: HashMap<usize, Vec<(Span, NodeId)>>,
    /// All names in scope at the end of the file (builtins + imports + lets).
    pub top_env: TypeEnv,
    /// Trait definitions visible in this file (local + imported).
    pub trait_env: HashMap<String, TraitDef>,
    /// NodeId → (trait_name, method_name) for every TraitCall expression.
    pub trait_calls: HashMap<NodeId, (String, String)>,
    /// Extra hover labels for nodes without entries in `node_types`
    /// (trait method declarations, type definitions, etc.).
    pub extra_hovers: Vec<(Span, String)>,
    /// Name → doc comment string, built from AST `doc` fields.
    pub doc_comments: HashMap<String, String>,
    /// Name → definition Span for go-to-definition.
    pub definitions: HashMap<String, Span>,
    /// Preferred display names for type variables from annotations.
    pub var_name_hints: HashMap<TyVar, String>,
}

// ── Conversion helpers ───────────────────────────────────────────────────────

pub fn span_to_range(span: &Span) -> Range {
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

pub fn error_to_diagnostic(err: LumeError) -> Diagnostic {
    let (range, message, source) = match &err {
        LumeError::Lex(e) => (span_to_range(&e.span), e.to_string(), "lexer"),
        LumeError::Parse(e) => (span_to_range(&e.span), e.to_string(), "parser"),
        LumeError::Type(e) => (span_to_range(&e.span), e.error.to_string(), "type-checker"),
    };
    Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some(source.to_string()),
        message,
        ..Default::default()
    }
}

fn pragma_warning_to_diagnostic(w: &PragmaWarning) -> Diagnostic {
    let line = w.line.saturating_sub(1) as u32;
    let col = w.col.saturating_sub(1) as u32;
    Diagnostic {
        range: Range {
            start: Position { line, character: col },
            end: Position { line, character: col + w.len as u32 },
        },
        severity: Some(DiagnosticSeverity::WARNING),
        source: Some("pragma".to_string()),
        message: format!("unknown pragma directive '{}'", w.directive),
        ..Default::default()
    }
}

// ── Full analysis pipeline ───────────────────────────────────────────────────

/// Run the full pipeline on `src`, returning a `DocInfo` and any diagnostics.
pub fn analyse(uri: &Url, src: &str) -> (Option<DocInfo>, Vec<Diagnostic>) {
    let tokens = match Lexer::new(src).tokenize() {
        Ok(t) => t,
        Err(e) => return (None, vec![error_to_diagnostic(LumeError::Lex(e))]),
    };
    let mut program = match parser::parse_program(&tokens) {
        Ok(p) => p,
        Err(e) => return (None, vec![error_to_diagnostic(LumeError::Parse(e))]),
    };
    let (pragmas, pragma_warnings) = parse_pragmas(src);
    program.pragmas = pragmas;
    let path = uri.to_file_path().ok();
    let (node_types, top_env, trait_env, type_errors, var_name_hints) =
        elaborate_with_env_partial(&program, path.as_deref());
    let span_index = collect_spans(&program);
    let trait_calls = collect_trait_calls(&program);
    let extra_hovers = collect_extra_hovers(&program);
    let doc_comments = collect_doc_comments(&program);
    let definitions = collect_definitions(&program);
    let doc_info = Some(DocInfo {
        node_types,
        span_index,
        top_env,
        trait_env,
        trait_calls,
        extra_hovers,
        doc_comments,
        definitions,
        var_name_hints,
    });
    let mut diagnostics: Vec<Diagnostic> = pragma_warnings
        .iter()
        .map(pragma_warning_to_diagnostic)
        .collect();
    diagnostics.extend(
        type_errors
            .into_iter()
            .map(|e| error_to_diagnostic(LumeError::Type(e))),
    );
    (doc_info, diagnostics)
}

// ── Trait call collection ────────────────────────────────────────────────────

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

// ── Span index ───────────────────────────────────────────────────────────────

/// Walk every `Expr` in `program` and collect `(Span, NodeId)` pairs,
/// grouped by line number with each bucket sorted by span length (shortest
/// first) so hover can find the most-specific expression at the cursor.
fn collect_spans(program: &Program) -> HashMap<usize, Vec<(Span, NodeId)>> {
    let mut flat = Vec::new();
    for item in &program.items {
        match item {
            TopItem::Binding(b) => {
                collect_pattern_spans(&b.pattern, &mut flat);
                collect_expr_spans(&b.value, &mut flat);
            }
            TopItem::BindingGroup(bs) => {
                for b in bs {
                    collect_pattern_spans(&b.pattern, &mut flat);
                    collect_expr_spans(&b.value, &mut flat);
                }
            }
            TopItem::ImplDef(id) => {
                for m in &id.methods {
                    collect_pattern_spans(&m.pattern, &mut flat);
                    collect_expr_spans(&m.value, &mut flat);
                }
            }
            TopItem::TypeDef(_) | TopItem::TraitDef(_) => {}
        }
    }
    collect_expr_spans(&program.exports, &mut flat);

    let mut by_line: HashMap<usize, Vec<(Span, NodeId)>> = HashMap::new();
    for (span, nid) in flat {
        by_line.entry(span.line).or_default().push((span, nid));
    }
    for bucket in by_line.values_mut() {
        // Primary: shortest span first (most specific).
        // Secondary: highest node_id first — in a pre-order walk the innermost
        // sub-expression gets the highest id, so this picks e.g. a TraitCall
        // over the Apply/Pipe wrappers that share the same source span.
        bucket.sort_by(|(s1, n1), (s2, n2)| s1.len.cmp(&s2.len).then(n2.cmp(n1)));
    }
    by_line
}

fn collect_pattern_spans(pat: &ast::Pattern, out: &mut Vec<(Span, NodeId)>) {
    match pat {
        ast::Pattern::Ident(_, span, nid) if span.len > 0 => {
            out.push((span.clone(), *nid));
        }
        ast::Pattern::Record(rp) => {
            for fp in &rp.fields {
                if let Some(inner) = &fp.pattern {
                    collect_pattern_spans(inner, out);
                }
            }
        }
        ast::Pattern::Variant { payload: Some(p), .. } => {
            collect_pattern_spans(p, out);
        }
        ast::Pattern::List(lp) => {
            for p in &lp.elements {
                collect_pattern_spans(p, out);
            }
        }
        _ => {}
    }
}

fn collect_expr_spans(expr: &Expr, out: &mut Vec<(Span, NodeId)>) {
    if expr.span.len > 0 {
        out.push((expr.span.clone(), expr.id));
    }
    match &expr.kind {
        ExprKind::List(es) => es.iter().for_each(|e| collect_expr_spans(e, out)),
        ExprKind::Record { base, fields, .. } => {
            if let Some(b) = base {
                collect_expr_spans(b, out);
            }
            for f in fields {
                if let Some(v) = &f.value {
                    collect_expr_spans(v, out);
                }
            }
        }
        ExprKind::FieldAccess { record, .. } => collect_expr_spans(record, out),
        ExprKind::Variant { payload: Some(p), .. } => collect_expr_spans(p, out),
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
        ExprKind::Unary { operand, .. } => collect_expr_spans(operand, out),
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
            for a in arms {
                collect_pattern_spans(&a.pattern, out);
                if let Some(g) = &a.guard {
                    collect_expr_spans(g, out);
                }
                collect_expr_spans(&a.body, out);
            }
        }
        ExprKind::MatchExpr { scrutinee, arms } => {
            collect_expr_spans(scrutinee, out);
            for a in arms {
                collect_pattern_spans(&a.pattern, out);
                if let Some(g) = &a.guard {
                    collect_expr_spans(g, out);
                }
                collect_expr_spans(&a.body, out);
            }
        }
        ExprKind::LetIn { pattern, value, body } => {
            collect_pattern_spans(pattern, out);
            collect_expr_spans(value, out);
            collect_expr_spans(body, out);
        }
        _ => {}
    }
}

// ── Extra hovers (trait declarations, type defs, etc.) ───────────────────────

/// Build span-based hover entries for nodes that the type checker doesn't track
/// in `node_types` — e.g. trait method declarations, trait names, impl headers.
fn collect_extra_hovers(
    program: &Program,
) -> Vec<(Span, String)> {
    let mut out = Vec::new();
    // Collect all impls for richer trait name hovers
    let mut impls_by_trait: HashMap<String, Vec<String>> = HashMap::new();
    for item in &program.items {
        if let TopItem::ImplDef(id) = item {
            impls_by_trait
                .entry(id.trait_name.clone())
                .or_default()
                .push(id.type_name.clone());
        }
    }

    for item in &program.items {
        match item {
            TopItem::TraitDef(td) => {
                // Hover on trait name → show full trait signature with methods
                let methods_str: Vec<String> = td
                    .methods
                    .iter()
                    .map(|m| format!("  let {} : {}", m.name, m.ty))
                    .collect();
                let mut label = format!("trait {} {}", td.name, td.type_param);
                if !methods_str.is_empty() {
                    label.push_str(" {\n");
                    label.push_str(&methods_str.join("\n"));
                    label.push_str("\n}");
                }
                // Append known impls
                if let Some(types) = impls_by_trait.get(&td.name) {
                    label.push_str("\n\n-- impls:\n");
                    for t in types {
                        label.push_str(&format!("--   {} {}\n", td.name, t));
                    }
                }
                out.push((td.name_span.clone(), label));

                // Hover on each method name → show constrained signature
                for m in &td.methods {
                    let label = format!(
                        "{} : ({} {}) => {}",
                        m.name, td.name, td.type_param, m.ty
                    );
                    out.push((m.name_span.clone(), label));
                }
            }
            TopItem::ImplDef(id) => {
                // Hover on trait name in impl header — use Lume syntax
                let mut label = if id.impl_constraints.is_empty() {
                    format!("use {} in {}", id.trait_name, id.type_name)
                } else {
                    let constraints: Vec<String> = id
                        .impl_constraints
                        .iter()
                        .map(|(t, p)| format!("{} {}", t, p))
                        .collect();
                    format!(
                        "use {} in {} => {}",
                        id.trait_name,
                        constraints.join(", "),
                        id.type_name,
                    )
                };
                // List methods in the impl
                let method_names: Vec<String> = id
                    .methods
                    .iter()
                    .filter_map(|m| match &m.pattern {
                        ast::Pattern::Ident(name, _, _) => Some(name.clone()),
                        _ => None,
                    })
                    .collect();
                if !method_names.is_empty() {
                    label.push_str(&format!("\n-- methods: {}", method_names.join(", ")));
                }
                out.push((id.trait_name_span.clone(), label.clone()));

                // Hover on type name in impl header
                out.push((id.type_name_span.clone(), label));
            }
            _ => {}
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

// ── Definition sites (go-to-definition) ──────────────────────────────────────

/// Walk the AST and collect name → definition-site Span for every top-level
/// definition: let bindings, use imports, trait names, trait methods.
fn collect_definitions(program: &Program) -> HashMap<String, Span> {
    let mut out = HashMap::new();

    // use declarations
    for u in &program.uses {
        match &u.binding {
            ast::UseBinding::Ident(name, span, _) => {
                out.insert(name.clone(), span.clone());
            }
            ast::UseBinding::Record(rp) => {
                for fp in &rp.fields {
                    out.insert(fp.name.clone(), fp.span.clone());
                }
            }
        }
    }

    for item in &program.items {
        match item {
            TopItem::Binding(b) => {
                if let ast::Pattern::Ident(name, span, _) = &b.pattern {
                    out.insert(name.clone(), span.clone());
                }
            }
            TopItem::BindingGroup(bs) => {
                for b in bs {
                    if let ast::Pattern::Ident(name, span, _) = &b.pattern {
                        out.insert(name.clone(), span.clone());
                    }
                }
            }
            TopItem::TraitDef(td) => {
                out.insert(td.name.clone(), td.name_span.clone());
                for m in &td.methods {
                    out.insert(m.name.clone(), m.name_span.clone());
                }
            }
            TopItem::ImplDef(_) | TopItem::TypeDef(_) => {}
        }
    }
    out
}
