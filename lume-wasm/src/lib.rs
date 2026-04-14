use std::path::PathBuf;

use lume::{
    ast::{Expr, ExprKind, MatchArm, NodeId, Pattern, Program, TopItem},
    bundle::BundleModule,
    codegen,
    error::Span,
    lexer::Lexer,
    loader::Loader,
    parser,
    types::{self, infer::elaborate},
};
use wasm_bindgen::prelude::*;

// ── Span → byte-offset conversion ────────────────────────────────────────────

/// Convert a 1-indexed (line, col) span into a `(from, to)` byte-offset pair.
/// The lexer produces 1-indexed lines and 1-indexed byte columns.
fn span_to_range(src: &str, span: &Span) -> (usize, usize) {
    if span.line == 0 {
        return (0, 0);
    }
    let bytes = src.as_bytes();
    let mut cur_line = 1usize;
    let mut line_start = 0usize;

    for (i, &b) in bytes.iter().enumerate() {
        if cur_line == span.line {
            let from = (line_start + span.col.saturating_sub(1)).min(src.len());
            let to = (from + span.len).min(src.len());
            return (from, to);
        }
        if b == b'\n' {
            cur_line += 1;
            line_start = i + 1;
        }
    }

    // Last line has no trailing newline.
    if cur_line == span.line {
        let from = (line_start + span.col.saturating_sub(1)).min(src.len());
        let to = (from + span.len).min(src.len());
        return (from, to);
    }

    (src.len(), src.len())
}

// ── AST span collector ────────────────────────────────────────────────────────

fn push_span(src: &str, span: &Span, id: NodeId, out: &mut Vec<(usize, usize, NodeId)>) {
    if span.line == 0 || span.len == 0 {
        return;
    }
    let (from, to) = span_to_range(src, span);
    if from < to {
        out.push((from, to, id));
    }
}

fn collect_expr(src: &str, expr: &Expr, out: &mut Vec<(usize, usize, NodeId)>) {
    push_span(src, &expr.span, expr.id, out);
    match &expr.kind {
        ExprKind::List(items) => items.iter().for_each(|e| collect_expr(src, e, out)),
        ExprKind::Record { base, fields, .. } => {
            if let Some(b) = base {
                collect_expr(src, b, out);
            }
            for f in fields {
                push_span(src, &f.name_span, f.name_node_id, out);
                if let Some(v) = &f.value {
                    collect_expr(src, v, out);
                }
            }
        }
        ExprKind::FieldAccess { record, .. } => collect_expr(src, record, out),
        ExprKind::Variant { payload, .. } => {
            if let Some(p) = payload {
                collect_expr(src, p, out);
            }
        }
        ExprKind::Lambda { param, body } => {
            collect_pat(src, param, out);
            collect_expr(src, body, out);
        }
        ExprKind::Apply { func, arg } => {
            collect_expr(src, func, out);
            collect_expr(src, arg, out);
        }
        ExprKind::Binary { left, right, .. } => {
            collect_expr(src, left, out);
            collect_expr(src, right, out);
        }
        ExprKind::Unary { operand, .. } => collect_expr(src, operand, out),
        ExprKind::If { cond, then_branch, else_branch } => {
            collect_expr(src, cond, out);
            collect_expr(src, then_branch, out);
            collect_expr(src, else_branch, out);
        }
        ExprKind::Match(arms) => arms.iter().for_each(|a| collect_arm(src, a, out)),
        _ => {}
    }
}

fn collect_arm(src: &str, arm: &MatchArm, out: &mut Vec<(usize, usize, NodeId)>) {
    collect_pat(src, &arm.pattern, out);
    if let Some(g) = &arm.guard {
        collect_expr(src, g, out);
    }
    collect_expr(src, &arm.body, out);
}

fn collect_pat(src: &str, pat: &Pattern, out: &mut Vec<(usize, usize, NodeId)>) {
    match pat {
        Pattern::Ident(_, span, id) => push_span(src, span, *id, out),
        Pattern::Variant { payload, .. } => {
            if let Some(p) = payload {
                collect_pat(src, p, out);
            }
        }
        Pattern::Record(rp) => {
            for f in &rp.fields {
                push_span(src, &f.span, f.node_id, out);
                if let Some(p) = &f.pattern {
                    collect_pat(src, p, out);
                }
            }
        }
        Pattern::List(lp) => lp.elements.iter().for_each(|p| collect_pat(src, p, out)),
        _ => {}
    }
}

fn collect_program(src: &str, program: &Program, out: &mut Vec<(usize, usize, NodeId)>) {
    for item in &program.items {
        if let TopItem::Binding(b) = item {
            collect_pat(src, &b.pattern, out);
            collect_expr(src, &b.value, out);
        }
    }
    collect_expr(src, &program.exports, out);
}

// ── JSON helpers (avoids a serde_json dep) ────────────────────────────────────

fn escape_json_str(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            '\n' => vec!['\\', 'n'],
            '\r' => vec!['\\', 'r'],
            '\t' => vec!['\\', 't'],
            c => vec![c],
        })
        .collect()
}

fn diag_json(from: usize, to: usize, message: &str) -> String {
    format!(r#"{{"from":{},"to":{},"message":"{}"}}"#, from, to, escape_json_str(message))
}

// ── Single-file bundle helper ─────────────────────────────────────────────────

fn single_bundle(src: &str) -> Result<Vec<BundleModule>, String> {
    let program = Loader::parse(src)?;
    Ok(vec![BundleModule {
        canonical: PathBuf::from("main.lume"),
        var: "_mod_main".to_string(),
        program,
    }])
}

// ── Public WASM API ───────────────────────────────────────────────────────────

/// Parse Lume source. Returns `"ok"` or throws an error string.
#[wasm_bindgen]
pub fn parse(src: &str) -> Result<JsValue, JsValue> {
    Loader::parse(src)
        .map(|_| JsValue::from_str("ok"))
        .map_err(|e| JsValue::from_str(&e))
}

/// Parse and type-check. Returns the inferred export type or throws.
#[wasm_bindgen]
pub fn typecheck(src: &str) -> Result<JsValue, JsValue> {
    let program = Loader::parse(src).map_err(|e| JsValue::from_str(&e))?;
    types::infer::check_program(&program, None)
        .map(|ty| JsValue::from_str(&ty.to_string()))
        .map_err(|e| JsValue::from_str(&e.to_string()))
}

/// Transpile to JavaScript (type-checks first). Returns JS code or throws.
#[wasm_bindgen]
pub fn to_js(src: &str) -> Result<JsValue, JsValue> {
    let bundle = single_bundle(src).map_err(|e| JsValue::from_str(&e))?;
    types::infer::check_program(&bundle[0].program, None)
        .map_err(|e| JsValue::from_str(&e.to_string()))?;
    Ok(JsValue::from_str(&codegen::js::emit(&bundle)))
}

/// Transpile to Lua (type-checks first). Returns Lua code or throws.
#[wasm_bindgen]
pub fn to_lua(src: &str) -> Result<JsValue, JsValue> {
    let bundle = single_bundle(src).map_err(|e| JsValue::from_str(&e))?;
    types::infer::check_program(&bundle[0].program, None)
        .map_err(|e| JsValue::from_str(&e.to_string()))?;
    Ok(JsValue::from_str(&codegen::lua::emit(&bundle)))
}

/// Returns a JSON array of diagnostics: `[{from, to, message}]`.
/// Covers lex errors, parse errors, and type errors.
/// Designed for use with `@codemirror/lint`.
#[wasm_bindgen]
pub fn lint(src: &str) -> String {
    // Lex
    let tokens = match Lexer::new(src).tokenize() {
        Err(e) => {
            let (from, to) = span_to_range(src, &e.span);
            let to = to.max(from + 1);
            return format!("[{}]", diag_json(from, to, &e.to_string()));
        }
        Ok(t) => t,
    };

    // Parse
    let program = match parser::parse_program(&tokens) {
        Err(e) => {
            let (from, to) = span_to_range(src, &e.span);
            let to = to.max(from + 1);
            return format!("[{}]", diag_json(from, to, &e.to_string()));
        }
        Ok(p) => p,
    };

    // Type-check
    match types::infer::check_program(&program, None) {
        Err(e) => {
            let (from, to) = span_to_range(src, &e.span);
            let to = to.max(from + 1);
            format!("[{}]", diag_json(from, to, &e.error.to_string()))
        }
        Ok(_) => "[]".to_string(),
    }
}

/// Returns the inferred type of the expression under `offset` (byte offset),
/// or `null` if no type information is available at that position.
/// Designed for use with `hoverTooltip` in CodeMirror.
#[wasm_bindgen]
pub fn type_at(src: &str, offset: usize) -> Option<String> {
    let tokens = Lexer::new(src).tokenize().ok()?;
    let program = parser::parse_program(&tokens).ok()?;
    let (node_types, _) = elaborate(&program, None).ok()?;

    let mut spans: Vec<(usize, usize, NodeId)> = Vec::new();
    collect_program(src, &program, &mut spans);

    // Keep only spans that contain the cursor offset.
    spans.retain(|(from, to, _)| *from <= offset && offset < *to);
    // Sort ascending by range size — smallest (innermost) first.
    spans.sort_by_key(|(from, to, _)| to - from);

    for (_, _, id) in &spans {
        if let Some(ty) = node_types.get(id) {
            return Some(ty.to_string());
        }
    }
    None
}
