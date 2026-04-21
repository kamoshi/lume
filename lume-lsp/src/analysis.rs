use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lume_core::{
    ast::{self, Binding, Expr, ExprKind, NodeId, Program, RecordEntry, TopItem, TraitDef},
    error::{LumeError, Span},
    fixity::{self, FixityTable},
    lexer::Lexer,
    loader::{parse_pragmas, resolve_path, PragmaWarning},
    parser,
    types::{
        infer::{elaborate_with_env_partial, TypeEnv, VariantEnv},
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
    /// Parenthesized-expression spans for explicit hover on `(` / `)`.
    pub paren_span_index: HashMap<usize, Vec<(Span, NodeId)>>,
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
    /// Document symbols for file outline.
    pub symbols: Vec<DocumentSymbol>,
    /// All identifier references: name → list of spans where it's used.
    pub references: HashMap<String, Vec<Span>>,
    /// Bindings without type annotations (for "add type annotation" code action).
    /// Each entry is (name, span of name, inferred type string).
    pub unannotated_bindings: Vec<(String, Span, String)>,
    /// Variant environment: variant name → type info (for "fill match arms" code action).
    pub variant_env: VariantEnv,
    /// Match expressions with their scrutinee node and existing arms' variant names.
    pub match_exprs: Vec<MatchExprInfo>,
    /// Imported names: name → resolved file path (for cross-file go-to-definition).
    pub imports: HashMap<String, std::path::PathBuf>,
    /// Operator fixity declarations visible in this file.
    pub fixity_table: FixityTable,
    /// Semantic annotations (type-system-aware) for identifiers and operators.
    /// Used by the semantic tokens provider for rich editor highlighting.
    pub semantic_spans: Vec<SemanticSpan>,
}

/// Info about a `match ... in` expression for the "fill match arms" code action.
pub struct MatchExprInfo {
    /// Span of the entire match expression.
    pub span: Span,
    /// NodeId of the scrutinee expression (look up in `node_types` for type).
    pub scrutinee_id: NodeId,
    /// Variant names already covered in existing arms.
    pub existing_variants: Vec<String>,
}

// ── Semantic token classification ────────────────────────────────────────────

/// Semantic classification for a source span, used to power rich semantic
/// tokens that the editor highlights with semantic meaning beyond syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SemanticClass {
    /// A function or operator binding.
    Function,
    /// A local variable or parameter.
    Variable,
    /// A function parameter (lambda param, match arm bind).
    Parameter,
    /// A data constructor / enum variant.
    EnumMember,
    /// A trait method call.
    Method,
    /// A record field name.
    Property,
    /// A type constructor name.
    Type,
    /// A type variable / type parameter.
    TypeParameter,
    /// An operator symbol.
    Operator,
}

bitflags::bitflags! {
    /// Modifiers that can be applied to any semantic token type.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct SemanticMod: u32 {
        /// This span is the declaration site for the name.
        const DECLARATION = 1 << 0;
        /// The binding is readonly (all Lume top-level bindings are).
        const READONLY    = 1 << 1;
    }
}

/// A single semantic annotation for a source span.
#[derive(Debug, Clone)]
pub struct SemanticSpan {
    pub span: Span,
    pub class: SemanticClass,
    pub mods: SemanticMod,
}

// ── Semantic span collection ──────────────────────────────────────────────────

/// Walk the typed AST and collect semantic annotations for all named references.
///
/// This produces a span-keyed list that `compute_semantic_tokens` merges on top
/// of the raw lexer classification to give editors richer highlighting.
pub fn collect_semantic_spans(
    program: &Program,
    node_types: &HashMap<NodeId, Ty>,
    top_env: &TypeEnv,
    variant_env: &VariantEnv,
) -> Vec<SemanticSpan> {
    let mut out: Vec<SemanticSpan> = Vec::new();

    // Collect top-level binding names as declaration sites.
    for item in &program.items {
        match item {
            TopItem::Binding(b) => collect_binding_decl(b, node_types, top_env, &mut out),
            TopItem::BindingGroup(bs) => {
                for b in bs {
                    collect_binding_decl(b, node_types, top_env, &mut out);
                }
            }
            TopItem::TypeDef(td) => {
                // Type name itself
                if td.name_span.len > 0 {
                    out.push(SemanticSpan {
                        span: td.name_span.clone(),
                        class: SemanticClass::Type,
                        mods: SemanticMod::DECLARATION | SemanticMod::READONLY,
                    });
                }
                // Variant names
                for v in &td.variants {
                    if v.name_span.len > 0 {
                        out.push(SemanticSpan {
                            span: v.name_span.clone(),
                            class: SemanticClass::EnumMember,
                            mods: SemanticMod::DECLARATION | SemanticMod::READONLY,
                        });
                    }
                }
            }
            TopItem::TraitDef(td) => {
                if td.name_span.len > 0 {
                    out.push(SemanticSpan {
                        span: td.name_span.clone(),
                        class: SemanticClass::Type,
                        mods: SemanticMod::DECLARATION | SemanticMod::READONLY,
                    });
                }
                for m in &td.methods {
                    if m.name_span.len > 0 {
                        out.push(SemanticSpan {
                            span: m.name_span.clone(),
                            class: SemanticClass::Method,
                            mods: SemanticMod::DECLARATION | SemanticMod::READONLY,
                        });
                    }
                }
            }
            TopItem::ImplDef(id) => {
                for m in &id.methods {
                    collect_binding_decl(m, node_types, top_env, &mut out);
                    collect_expr_semantic(&m.value, node_types, top_env, variant_env, false, &mut out);
                }
            }
        }
    }

    // Walk all expression bodies.
    for item in &program.items {
        match item {
            TopItem::Binding(b) => {
                collect_expr_semantic(&b.value, node_types, top_env, variant_env, false, &mut out);
            }
            TopItem::BindingGroup(bs) => {
                for b in bs {
                    collect_expr_semantic(&b.value, node_types, top_env, variant_env, false, &mut out);
                }
            }
            TopItem::ImplDef(_) | TopItem::TypeDef(_) | TopItem::TraitDef(_) => {}
        }
    }

    // Walk exports expression.
    collect_expr_semantic(&program.exports, node_types, top_env, variant_env, false, &mut out);

    out
}

fn collect_binding_decl(
    b: &Binding,
    node_types: &HashMap<NodeId, Ty>,
    top_env: &TypeEnv,
    out: &mut Vec<SemanticSpan>,
) {
    if let ast::Pattern::Ident(name, span, id) = &b.pattern {
        if span.len == 0 {
            return;
        }
        // Determine class: function if type is Func, else variable.
        let class = if let Some(ty) = node_types.get(id) {
            classify_ty(ty)
        } else {
            // Fall back to top_env scheme.
            top_env
                .lookup(name)
                .map(|s| classify_ty(&s.ty))
                .unwrap_or(SemanticClass::Variable)
        };
        out.push(SemanticSpan {
            span: span.clone(),
            class,
            mods: SemanticMod::DECLARATION | SemanticMod::READONLY,
        });
    }
}

/// Classify a type into a semantic class.
fn classify_ty(ty: &Ty) -> SemanticClass {
    match ty {
        Ty::Func(_, _) => SemanticClass::Function,
        _ => SemanticClass::Variable,
    }
}

/// Recursively walk an expression and emit semantic spans for identifiers.
fn collect_expr_semantic(
    expr: &Expr,
    node_types: &HashMap<NodeId, Ty>,
    top_env: &TypeEnv,
    variant_env: &VariantEnv,
    in_param_position: bool,
    out: &mut Vec<SemanticSpan>,
) {
    match &expr.kind {
        ExprKind::Ident(name) => {
            if expr.span.len == 0 {
                return;
            }
            // Check if it's a known variant name.
            if variant_env.lookup(name).is_some() {
                out.push(SemanticSpan {
                    span: expr.span.clone(),
                    class: SemanticClass::EnumMember,
                    mods: SemanticMod::READONLY,
                });
                return;
            }
            let class = if in_param_position {
                SemanticClass::Parameter
            } else if let Some(ty) = node_types.get(&expr.id) {
                classify_ty(ty)
            } else {
                top_env
                    .lookup(name)
                    .map(|s| classify_ty(&s.ty))
                    .unwrap_or(SemanticClass::Variable)
            };
            out.push(SemanticSpan {
                span: expr.span.clone(),
                class,
                mods: SemanticMod::READONLY,
            });
        }
        ExprKind::TraitCall { .. } => {
            if expr.span.len > 0 {
                out.push(SemanticSpan {
                    span: expr.span.clone(),
                    class: SemanticClass::Method,
                    mods: SemanticMod::READONLY,
                });
            }
        }
        ExprKind::Variant { name: _, payload } => {
            // The variant name span is the whole `expr.span` minus payload.
            // We don't have the variant-name-only span here; the whole expr
            // span may include `Name { ... }`. We emit for the ident portion
            // (the first token) when the span is valid.
            // We can't easily isolate just the constructor name here, so we
            // annotate the full expression span only if there's no payload
            // (unit constructors).
            if expr.span.len > 0 {
                // Always mark the entire variant reference as EnumMember.
                // Editors will use it for the clickable range.
                out.push(SemanticSpan {
                    span: expr.span.clone(),
                    class: SemanticClass::EnumMember,
                    mods: SemanticMod::empty(),
                });
            }
            if let Some(p) = payload {
                collect_expr_semantic(p, node_types, top_env, variant_env, false, out);
            }
        }
        ExprKind::Lambda { param, body } => {
            collect_pattern_semantic(param, node_types, top_env, variant_env, true, out);
            collect_expr_semantic(body, node_types, top_env, variant_env, false, out);
        }
        ExprKind::Apply { func, arg } => {
            collect_expr_semantic(func, node_types, top_env, variant_env, false, out);
            collect_expr_semantic(arg, node_types, top_env, variant_env, false, out);
        }
        ExprKind::Paren(inner) => {
            collect_expr_semantic(inner, node_types, top_env, variant_env, false, out);
        }
        ExprKind::Binary { left, right, .. } => {
            collect_expr_semantic(left, node_types, top_env, variant_env, false, out);
            collect_expr_semantic(right, node_types, top_env, variant_env, false, out);
        }
        ExprKind::Unary { operand, .. } => {
            collect_expr_semantic(operand, node_types, top_env, variant_env, false, out);
        }
        ExprKind::If { cond, then_branch, else_branch } => {
            collect_expr_semantic(cond, node_types, top_env, variant_env, false, out);
            collect_expr_semantic(then_branch, node_types, top_env, variant_env, false, out);
            collect_expr_semantic(else_branch, node_types, top_env, variant_env, false, out);
        }
        ExprKind::LetIn { pattern, value, body } => {
            collect_pattern_semantic(pattern, node_types, top_env, variant_env, false, out);
            collect_expr_semantic(value, node_types, top_env, variant_env, false, out);
            collect_expr_semantic(body, node_types, top_env, variant_env, false, out);
        }
        ExprKind::Match(arms) => {
            for arm in arms {
                collect_pattern_semantic(
                    &arm.pattern,
                    node_types,
                    top_env,
                    variant_env,
                    true,
                    out,
                );
                if let Some(g) = &arm.guard {
                    collect_expr_semantic(g, node_types, top_env, variant_env, false, out);
                }
                collect_expr_semantic(&arm.body, node_types, top_env, variant_env, false, out);
            }
        }
        ExprKind::MatchExpr { scrutinee, arms } => {
            collect_expr_semantic(scrutinee, node_types, top_env, variant_env, false, out);
            for arm in arms {
                collect_pattern_semantic(
                    &arm.pattern,
                    node_types,
                    top_env,
                    variant_env,
                    true,
                    out,
                );
                if let Some(g) = &arm.guard {
                    collect_expr_semantic(g, node_types, top_env, variant_env, false, out);
                }
                collect_expr_semantic(&arm.body, node_types, top_env, variant_env, false, out);
            }
        }
        ExprKind::Record { entries } => {
            for entry in entries {
                match entry {
                    RecordEntry::Field(f) => {
                        // Field name → property
                        if f.name_span.len > 0 {
                            out.push(SemanticSpan {
                                span: f.name_span.clone(),
                                class: SemanticClass::Property,
                                mods: SemanticMod::empty(),
                            });
                        }
                        if let Some(v) = &f.value {
                            collect_expr_semantic(v, node_types, top_env, variant_env, false, out);
                        }
                    }
                    RecordEntry::Spread(e) => {
                        collect_expr_semantic(e, node_types, top_env, variant_env, false, out);
                    }
                }
            }
        }
        ExprKind::FieldAccess { record, .. } => {
            collect_expr_semantic(record, node_types, top_env, variant_env, false, out);
            // No separate field span available; the field name is inside the expr span.
        }
        ExprKind::List { entries } => {
            for entry in entries {
                match entry {
                    ast::ListEntry::Elem(e) | ast::ListEntry::Spread(e) => {
                        collect_expr_semantic(e, node_types, top_env, variant_env, false, out);
                    }
                }
            }
        }
        // Leaves: literals have no identifiers.
        ExprKind::Number(_) | ExprKind::Text(_) | ExprKind::Bool(_) | ExprKind::Hole => {}
    }
}

fn collect_pattern_semantic(
    pat: &ast::Pattern,
    node_types: &HashMap<NodeId, Ty>,
    top_env: &TypeEnv,
    variant_env: &VariantEnv,
    is_param: bool,
    out: &mut Vec<SemanticSpan>,
) {
    match pat {
        ast::Pattern::Ident(_, span, _) => {
            if span.len > 0 {
                out.push(SemanticSpan {
                    span: span.clone(),
                    class: if is_param { SemanticClass::Parameter } else { SemanticClass::Variable },
                    mods: SemanticMod::DECLARATION,
                });
            }
        }
        ast::Pattern::Variant { payload, .. } => {
            if let Some(p) = payload {
                collect_pattern_semantic(p, node_types, top_env, variant_env, is_param, out);
            }
        }
        ast::Pattern::Record(rp) => {
            for fp in &rp.fields {
                if fp.span.len > 0 {
                    out.push(SemanticSpan {
                        span: fp.span.clone(),
                        class: SemanticClass::Property,
                        mods: SemanticMod::empty(),
                    });
                }
                if let Some(p) = &fp.pattern {
                    collect_pattern_semantic(p, node_types, top_env, variant_env, is_param, out);
                }
            }
        }
        ast::Pattern::List(lp) => {
            for p in &lp.elements {
                collect_pattern_semantic(p, node_types, top_env, variant_env, is_param, out);
            }
        }
        ast::Pattern::Wildcard | ast::Pattern::Literal(_) => {}
    }
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
    let (node_types, top_env, trait_env, type_errors, var_name_hints, variant_env) =
        elaborate_with_env_partial(&program, path.as_deref());
    let span_index = collect_spans(&program);
    let paren_span_index = collect_paren_spans(&program);
    let trait_calls = collect_trait_calls(&program);
    let extra_hovers = collect_extra_hovers(&program);
    let doc_comments = collect_doc_comments(&program);
    let definitions = collect_definitions(&program);
    let symbols = collect_document_symbols(&program, &top_env, &var_name_hints);
    let references = collect_references(&program);
    let unannotated_bindings =
        collect_unannotated_bindings(&program, &top_env, &var_name_hints);
    let match_exprs = collect_match_exprs(&program);
    let imports = collect_imports(&program, path.as_deref());
    let fixity_table =
        collect_fixity_with_imports(&program, path.as_deref());
    let semantic_spans = collect_semantic_spans(&program, &node_types, &top_env, &variant_env);
    let doc_info = Some(DocInfo {
        node_types,
        span_index,
        paren_span_index,
        top_env,
        trait_env,
        trait_calls,
        extra_hovers,
        doc_comments,
        definitions,
        var_name_hints,
        symbols,
        references,
        unannotated_bindings,
        variant_env,
        match_exprs,
        imports,
        fixity_table,
        semantic_spans,
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
            ExprKind::List { entries } => entries.iter().for_each(|entry| match entry {
                ast::ListEntry::Elem(e) | ast::ListEntry::Spread(e) => walk(e, out),
            }),
            ExprKind::Record { entries } => {
                for entry in entries {
                    match entry {
                        RecordEntry::Spread(e) => walk(e, out),
                        RecordEntry::Field(f) => { if let Some(v) = &f.value { walk(v, out); } }
                    }
                }
            }
            ExprKind::FieldAccess { record, .. } => walk(record, out),
            ExprKind::Variant { payload: Some(p), .. } => walk(p, out),
            ExprKind::Lambda { body, .. } => walk(body, out),
            ExprKind::Apply { func, arg } => { walk(func, out); walk(arg, out); }
            ExprKind::Paren(inner) => walk(inner, out),
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
                } else if fp.span.len > 0 {
                    // Shorthand `{ bar }` — the field name IS the binding.
                    out.push((fp.span.clone(), fp.node_id));
                }
            }
            if let Some(Some((_, rest_span, rest_nid))) = &rp.rest {
                if rest_span.len > 0 {
                    out.push((rest_span.clone(), *rest_nid));
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
            if let Some(Some((_, rest_span, rest_nid))) = &lp.rest {
                if rest_span.len > 0 {
                    out.push((rest_span.clone(), *rest_nid));
                }
            }
        }
        _ => {}
    }
}

fn collect_paren_spans(program: &Program) -> HashMap<usize, Vec<(Span, NodeId)>> {
    let mut flat = Vec::new();

    fn walk(expr: &Expr, out: &mut Vec<(Span, NodeId)>) {
        if let ExprKind::Paren(inner) = &expr.kind {
            if expr.span.len > 0 {
                out.push((expr.span.clone(), expr.id));
            }
            walk(inner, out);
            return;
        }

        match &expr.kind {
            ExprKind::List { entries } => entries.iter().for_each(|entry| match entry {
                ast::ListEntry::Elem(e) | ast::ListEntry::Spread(e) => walk(e, out),
            }),
            ExprKind::Record { entries } => {
                for entry in entries {
                    match entry {
                        RecordEntry::Spread(e) => walk(e, out),
                        RecordEntry::Field(f) => {
                            if let Some(v) = &f.value {
                                walk(v, out);
                            }
                        }
                    }
                }
            }
            ExprKind::FieldAccess { record, .. } => walk(record, out),
            ExprKind::Variant { payload: Some(p), .. } => walk(p, out),
            ExprKind::Lambda { body, .. } => walk(body, out),
            ExprKind::Apply { func, arg } => {
                walk(func, out);
                walk(arg, out);
            }
            ExprKind::Binary { left, right, .. } => {
                walk(left, out);
                walk(right, out);
            }
            ExprKind::Unary { operand, .. } => walk(operand, out),
            ExprKind::If { cond, then_branch, else_branch } => {
                walk(cond, out);
                walk(then_branch, out);
                walk(else_branch, out);
            }
            ExprKind::Match(arms) => {
                for a in arms {
                    if let Some(g) = &a.guard {
                        walk(g, out);
                    }
                    walk(&a.body, out);
                }
            }
            ExprKind::MatchExpr { scrutinee, arms } => {
                walk(scrutinee, out);
                for a in arms {
                    if let Some(g) = &a.guard {
                        walk(g, out);
                    }
                    walk(&a.body, out);
                }
            }
            ExprKind::LetIn { value, body, .. } => {
                walk(value, out);
                walk(body, out);
            }
            _ => {}
        }
    }

    for item in &program.items {
        match item {
            TopItem::Binding(b) => walk(&b.value, &mut flat),
            TopItem::BindingGroup(bs) => {
                for b in bs {
                    walk(&b.value, &mut flat);
                }
            }
            TopItem::ImplDef(id) => {
                for m in &id.methods {
                    walk(&m.value, &mut flat);
                }
            }
            TopItem::TypeDef(_) | TopItem::TraitDef(_) => {}
        }
    }
    walk(&program.exports, &mut flat);

    let mut by_line: HashMap<usize, Vec<(Span, NodeId)>> = HashMap::new();
    for (span, nid) in flat {
        by_line.entry(span.line).or_default().push((span, nid));
    }
    for bucket in by_line.values_mut() {
        bucket.sort_by(|(s1, n1), (s2, n2)| s1.len.cmp(&s2.len).then(n2.cmp(n1)));
    }
    by_line
}

fn collect_expr_spans(expr: &Expr, out: &mut Vec<(Span, NodeId)>) {
    if expr.span.len > 0 {
        out.push((expr.span.clone(), expr.id));
    }
    match &expr.kind {
        ExprKind::List { entries } => entries.iter().for_each(|entry| match entry {
            ast::ListEntry::Elem(e) | ast::ListEntry::Spread(e) => collect_expr_spans(e, out),
        }),
        ExprKind::Record { entries } => {
            for entry in entries {
                match entry {
                    RecordEntry::Spread(e) => collect_expr_spans(e, out),
                    RecordEntry::Field(f) => {
                        if let Some(v) = &f.value {
                            collect_expr_spans(v, out);
                        }
                    }
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
        ExprKind::Paren(inner) => collect_expr_spans(inner, out),
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

    // ── Use declarations ─────────────────────────────────────────────────────
    // Hovering over a module alias shows the import declaration rather than
    // the raw module record type (which is verbose and unreadable).
    for u in &program.uses {
        match &u.binding {
            ast::UseBinding::Ident(name, span, _) => {
                out.push((span.clone(), format!("use {} = \"{}\"", name, u.path)));
            }
            ast::UseBinding::Record(rp) => {
                // For destructure imports, each field hover shows where it
                // came from so the user can navigate to the source module.
                for fp in &rp.fields {
                    out.push((fp.span.clone(), format!("-- from \"{}\"\n{}", u.path, fp.name)));
                }
            }
        }
    }

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
            TopItem::TypeDef(td) => {
                // Hover on type name → show the full type definition
                let mut label = format!("type {} ", td.name);
                for p in &td.params {
                    label.push_str(p);
                    label.push(' ');
                }
                label.push('=');
                for v in &td.variants {
                    label.push_str("\n  | ");
                    label.push_str(&v.name);
                    if let Some(ref wraps) = v.wraps {
                        label.push(' ');
                        label.push_str(&wraps.to_string());
                    }
                }
                out.push((td.name_span.clone(), label.clone()));

                // Hover on each variant name → show which type it belongs to
                for v in &td.variants {
                    let variant_label = if let Some(ref wraps) = v.wraps {
                        format!("| {} {} : {} {}", v.name, wraps, td.name, td.params.join(" "))
                    } else {
                        format!("| {} : {} {}", v.name, td.name, td.params.join(" "))
                    };
                    out.push((v.name_span.clone(), variant_label.trim_end().to_string()));
                }
            }
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

// ── Import resolution ────────────────────────────────────────────────────────

/// Build the fixity table for a file, including fixity propagated from imports.
///
/// Rules:
/// - The current file's own fixity declarations are always included.
/// - Trait method fixity from any directly imported module is included (traits
///   propagate implicitly via their impls being brought into scope).
/// - For `use { (op), ... } = "..."` (explicit destructure imports), fixity
///   declared on the imported binding itself is also included.
fn collect_fixity_with_imports(program: &Program, base_path: Option<&Path>) -> fixity::FixityTable {
    let mut table = fixity::collect_for_program(program);
    let base = match base_path {
        Some(p) => p,
        None => return table,
    };
    for u in &program.uses {
        if u.path.starts_with("lume:") {
            continue;
        }
        let import_path = match resolve_path(&u.path, base) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let src = match std::fs::read_to_string(&import_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let imported = match lume_core::loader::Loader::parse(&src) {
            Ok(p) => p,
            Err(_) => continue,
        };

        // Always: trait method fixity propagates implicitly.
        let imported_all = fixity::collect_for_program(&imported);
        for item in &imported.items {
            if let ast::TopItem::TraitDef(td) = item {
                for method in &td.methods {
                    if let Some(entry) = imported_all.get(&method.name) {
                        table.insert(method.name.clone(), entry.clone());
                    }
                }
            }
        }

        // Explicit destructure imports: also include fixity for named operators.
        if let ast::UseBinding::Record(rp) = &u.binding {
            for fp in &rp.fields {
                if let Some(entry) = imported_all.get(&fp.name) {
                    table.insert(fp.name.clone(), entry.clone());
                }
            }
        }
    }
    table
}

/// Map each imported name to the resolved file path of its source module.
/// This enables cross-file go-to-definition.
fn collect_imports(program: &Program, base_path: Option<&Path>) -> HashMap<String, PathBuf> {
    let mut out = HashMap::new();
    let base = match base_path {
        Some(p) => p,
        None => return out,
    };
    for u in &program.uses {
        // Skip stdlib imports (lume:prelude, etc.) — they don't have local files
        if u.path.starts_with("lume:") {
            continue;
        }
        let resolved = match resolve_path(&u.path, base) {
            Ok(p) => p,
            Err(_) => continue,
        };
        match &u.binding {
            ast::UseBinding::Ident(name, _, _) => {
                out.insert(name.clone(), resolved);
            }
            ast::UseBinding::Record(rp) => {
                for fp in &rp.fields {
                    out.insert(fp.name.clone(), resolved.clone());
                }
            }
        }
    }
    out
}

// ── Document symbols ─────────────────────────────────────────────────────────

/// Build a list of `DocumentSymbol` entries for the file outline.
/// Includes let bindings, type definitions, trait definitions, and impl blocks.
#[allow(deprecated)] // DocumentSymbol::deprecated field is deprecated itself
fn collect_document_symbols(
    program: &Program,
    top_env: &TypeEnv,
    var_name_hints: &HashMap<TyVar, String>,
) -> Vec<DocumentSymbol> {
    let mut symbols = Vec::new();

    for item in &program.items {
        match item {
            TopItem::Binding(b) => {
                if let Some(sym) = binding_symbol(b, top_env, var_name_hints) {
                    symbols.push(sym);
                }
            }
            TopItem::BindingGroup(bs) => {
                for b in bs {
                    if let Some(sym) = binding_symbol(b, top_env, var_name_hints) {
                        symbols.push(sym);
                    }
                }
            }
            TopItem::TypeDef(td) => {
                let range = span_to_range(&td.name_span);
                let detail = if td.variants.is_empty() {
                    None
                } else {
                    let vs: Vec<&str> = td.variants.iter().map(|v| v.name.as_str()).collect();
                    Some(vs.join(" | "))
                };
                symbols.push(DocumentSymbol {
                    name: td.name.clone(),
                    detail,
                    kind: SymbolKind::ENUM,
                    range,
                    selection_range: range,
                    children: Some(
                        td.variants
                            .iter()
                            .map(|v| DocumentSymbol {
                                name: v.name.clone(),
                                detail: None,
                                kind: SymbolKind::ENUM_MEMBER,
                                range: span_to_range(&v.name_span),
                                selection_range: span_to_range(&v.name_span),
                                children: None,
                                tags: None,
                                deprecated: None,
                            })
                            .collect(),
                    ),
                    tags: None,
                    deprecated: None,
                });
            }
            TopItem::TraitDef(td) => {
                let range = span_to_range(&td.name_span);
                let children: Vec<DocumentSymbol> = td
                    .methods
                    .iter()
                    .map(|m| {
                        let method_range = span_to_range(&m.name_span);
                        DocumentSymbol {
                            name: m.name.clone(),
                            detail: Some(m.ty.to_string()),
                            kind: SymbolKind::METHOD,
                            range: method_range,
                            selection_range: method_range,
                            children: None,
                            tags: None,
                            deprecated: None,
                        }
                    })
                    .collect();
                symbols.push(DocumentSymbol {
                    name: td.name.clone(),
                    detail: Some(format!("trait {} {}", td.name, td.type_param)),
                    kind: SymbolKind::INTERFACE,
                    range,
                    selection_range: range,
                    children: Some(children),
                    tags: None,
                    deprecated: None,
                });
            }
            TopItem::ImplDef(id) => {
                let range = span_to_range(&id.trait_name_span);
                let children: Vec<DocumentSymbol> = id
                    .methods
                    .iter()
                    .filter_map(|m| {
                        if let ast::Pattern::Ident(name, span, _) = &m.pattern {
                            Some(DocumentSymbol {
                                name: name.clone(),
                                detail: None,
                                kind: SymbolKind::METHOD,
                                range: span_to_range(span),
                                selection_range: span_to_range(span),
                                children: None,
                                tags: None,
                                deprecated: None,
                            })
                        } else {
                            None
                        }
                    })
                    .collect();
                symbols.push(DocumentSymbol {
                    name: format!("{} {}", id.trait_name, id.type_name),
                    detail: Some(format!("use {} in {}", id.trait_name, id.type_name)),
                    kind: SymbolKind::CLASS,
                    range,
                    selection_range: range,
                    children: Some(children),
                    tags: None,
                    deprecated: None,
                });
            }
        }
    }
    symbols
}

#[allow(deprecated)]
fn binding_symbol(
    b: &Binding,
    top_env: &TypeEnv,
    _var_name_hints: &HashMap<TyVar, String>,
) -> Option<DocumentSymbol> {
    if let ast::Pattern::Ident(name, span, _) = &b.pattern {
        let range = span_to_range(span);
        let detail = top_env.lookup(name).map(|scheme| scheme.to_string());
        let kind = match top_env.lookup(name) {
            Some(scheme) if matches!(scheme.ty, Ty::Func(..)) => SymbolKind::FUNCTION,
            _ => SymbolKind::VARIABLE,
        };
        Some(DocumentSymbol {
            name: name.clone(),
            detail,
            kind,
            range,
            selection_range: range,
            children: None,
            tags: None,
            deprecated: None,
        })
    } else {
        None
    }
}

// ── References collection ────────────────────────────────────────────────────

/// Walk all expressions and patterns to find every identifier usage.
/// Returns name → Vec<Span> for all identifier references (not definitions).
fn collect_references(program: &Program) -> HashMap<String, Vec<Span>> {
    let mut out: HashMap<String, Vec<Span>> = HashMap::new();

    for item in &program.items {
        match item {
            TopItem::Binding(b) => collect_refs_expr(&b.value, &mut out),
            TopItem::BindingGroup(bs) => {
                for b in bs {
                    collect_refs_expr(&b.value, &mut out);
                }
            }
            TopItem::ImplDef(id) => {
                for m in &id.methods {
                    collect_refs_expr(&m.value, &mut out);
                }
            }
            TopItem::TypeDef(_) | TopItem::TraitDef(_) => {}
        }
    }
    collect_refs_expr(&program.exports, &mut out);
    out
}

fn collect_refs_expr(expr: &Expr, out: &mut HashMap<String, Vec<Span>>) {
    match &expr.kind {
        ExprKind::Ident(name) => {
            if expr.span.len > 0 {
                out.entry(name.clone()).or_default().push(expr.span.clone());
            }
        }
        ExprKind::TraitCall { trait_name, method_name } => {
            if expr.span.len > 0 {
                let full_name = format!("{}.{}", trait_name, method_name);
                out.entry(full_name).or_default().push(expr.span.clone());
            }
        }
        ExprKind::List { entries } => entries.iter().for_each(|entry| match entry {
            ast::ListEntry::Elem(e) | ast::ListEntry::Spread(e) => collect_refs_expr(e, out),
        }),
        ExprKind::Record { entries } => {
            for entry in entries {
                match entry {
                    RecordEntry::Spread(e) => collect_refs_expr(e, out),
                    RecordEntry::Field(f) => {
                        if let Some(v) = &f.value {
                            collect_refs_expr(v, out);
                        } else {
                            // Shorthand field: `{ area }` is a reference to `area`
                            if f.name_span.len > 0 {
                                out.entry(f.name.clone())
                                    .or_default()
                                    .push(f.name_span.clone());
                            }
                        }
                    }
                }
            }
        }
        ExprKind::FieldAccess { record, .. } => collect_refs_expr(record, out),
        ExprKind::Variant { payload: Some(p), .. } => collect_refs_expr(p, out),
        ExprKind::Lambda { body, .. } => collect_refs_expr(body, out),
        ExprKind::Apply { func, arg } => {
            collect_refs_expr(func, out);
            collect_refs_expr(arg, out);
        }
        ExprKind::Paren(inner) => collect_refs_expr(inner, out),
        ExprKind::Binary { left, right, .. } => {
            collect_refs_expr(left, out);
            collect_refs_expr(right, out);
        }
        ExprKind::Unary { operand, .. } => collect_refs_expr(operand, out),
        ExprKind::If { cond, then_branch, else_branch } => {
            collect_refs_expr(cond, out);
            collect_refs_expr(then_branch, out);
            collect_refs_expr(else_branch, out);
        }
        ExprKind::Match(arms) => {
            for a in arms {
                if let Some(g) = &a.guard {
                    collect_refs_expr(g, out);
                }
                collect_refs_expr(&a.body, out);
            }
        }
        ExprKind::MatchExpr { scrutinee, arms } => {
            collect_refs_expr(scrutinee, out);
            for a in arms {
                if let Some(g) = &a.guard {
                    collect_refs_expr(g, out);
                }
                collect_refs_expr(&a.body, out);
            }
        }
        ExprKind::LetIn { value, body, .. } => {
            collect_refs_expr(value, out);
            collect_refs_expr(body, out);
        }
        _ => {}
    }
}

// ── Unannotated bindings (for code actions) ──────────────────────────────────

/// Collect bindings that lack a type annotation, pairing them with their
/// inferred type string so a code action can offer to insert it.
fn collect_unannotated_bindings(
    program: &Program,
    top_env: &TypeEnv,
    _var_name_hints: &HashMap<TyVar, String>,
) -> Vec<(String, Span, String)> {
    let mut out = Vec::new();

    let mut check_binding = |b: &Binding| {
        if b.ty.is_none() {
            if let ast::Pattern::Ident(name, span, _) = &b.pattern {
                if let Some(scheme) = top_env.lookup(name) {
                    let ty_str = scheme.to_string();
                    out.push((name.clone(), span.clone(), ty_str));
                }
            }
        }
    };

    for item in &program.items {
        match item {
            TopItem::Binding(b) => check_binding(b),
            TopItem::BindingGroup(bs) => {
                for b in bs {
                    check_binding(b);
                }
            }
            _ => {}
        }
    }
    out
}

// ── Match expression collection ─────────────────────────────────────────────

/// Collect all `match ... in` expressions with their scrutinee NodeId and
/// existing arm variant names so the "fill match arms" code action can find them.
fn collect_match_exprs(program: &Program) -> Vec<MatchExprInfo> {
    let mut out = Vec::new();

    fn existing_variants(arms: &[ast::MatchArm]) -> Vec<String> {
        arms.iter()
            .filter_map(|arm| match &arm.pattern {
                ast::Pattern::Variant { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect()
    }

    fn walk(expr: &Expr, out: &mut Vec<MatchExprInfo>) {
        match &expr.kind {
            ExprKind::MatchExpr { scrutinee, arms } => {
                out.push(MatchExprInfo {
                    span: expr.span.clone(),
                    scrutinee_id: scrutinee.id,
                    existing_variants: existing_variants(arms),
                });
                walk(scrutinee, out);
                for arm in arms {
                    if let Some(g) = &arm.guard {
                        walk(g, out);
                    }
                    walk(&arm.body, out);
                }
            }
            ExprKind::Match(arms) => {
                for arm in arms {
                    if let Some(g) = &arm.guard {
                        walk(g, out);
                    }
                    walk(&arm.body, out);
                }
            }
            ExprKind::List { entries } => entries.iter().for_each(|entry| match entry {
                ast::ListEntry::Elem(e) | ast::ListEntry::Spread(e) => walk(e, out),
            }),
            ExprKind::Record { entries } => {
                for entry in entries {
                    match entry {
                        RecordEntry::Spread(e) => walk(e, out),
                        RecordEntry::Field(f) => {
                            if let Some(v) = &f.value {
                                walk(v, out);
                            }
                        }
                    }
                }
            }
            ExprKind::FieldAccess { record, .. } => walk(record, out),
            ExprKind::Variant { payload: Some(p), .. } => walk(p, out),
            ExprKind::Lambda { body, .. } => walk(body, out),
            ExprKind::Apply { func, arg } => {
                walk(func, out);
                walk(arg, out);
            }
            ExprKind::Paren(inner) => walk(inner, out),
            ExprKind::Binary { left, right, .. } => {
                walk(left, out);
                walk(right, out);
            }
            ExprKind::Unary { operand, .. } => walk(operand, out),
            ExprKind::If { cond, then_branch, else_branch } => {
                walk(cond, out);
                walk(then_branch, out);
                walk(else_branch, out);
            }
            ExprKind::LetIn { value, body, .. } => {
                walk(value, out);
                walk(body, out);
            }
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

// ── Operator helpers ─────────────────────────────────────────────────────────

/// Returns `true` if every character in `s` is an operator character.
/// Mirrors `is_operator_name` in the parser (kept private there).
pub(crate) fn is_op_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| "!#$%&*+./<=>?@\\^|~-:".contains(c))
}
