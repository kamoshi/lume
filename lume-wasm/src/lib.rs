use std::collections::HashSet;
use std::path::{Path, PathBuf};

use lume_core::{
    ast::{Expr, ExprKind, MatchArm, NodeId, Pattern, Program, TopItem},
    bundle::BundleModule,
    codegen,
    error::Span,
    lexer::Lexer,
    loader::{stdlib_path, stdlib_source, use_path_context, Loader, UsePathKind, STDLIB_MODULES},
    lower,
    parser,
    types::{self, infer::builtin_env, infer::elaborate_with_env_partial, Ty},
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
        ExprKind::Variant { payload: Some(p), .. } => {
            collect_expr(src, p, out);
        }
        ExprKind::Variant { payload: None, .. } => {}
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
        ExprKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            collect_expr(src, cond, out);
            collect_expr(src, then_branch, out);
            collect_expr(src, else_branch, out);
        }
        ExprKind::Match(arms) => arms.iter().for_each(|a| collect_arm(src, a, out)),
        ExprKind::MatchExpr { scrutinee, arms } => {
            collect_expr(src, scrutinee, out);
            arms.iter().for_each(|a| collect_arm(src, a, out));
        }
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
        Pattern::Variant { payload: Some(p), .. } => {
            collect_pat(src, p, out);
        }
        Pattern::Variant { payload: None, .. } => {}
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
        match item {
            TopItem::Binding(b) => {
                collect_pat(src, &b.pattern, out);
                collect_expr(src, &b.value, out);
            }
            TopItem::BindingGroup(bs) => {
                for b in bs {
                    collect_pat(src, &b.pattern, out);
                    collect_expr(src, &b.value, out);
                }
            }
            TopItem::TypeDef(_) | TopItem::TraitDef(_) => {}
            TopItem::ImplDef(id) => {
                for m in &id.methods {
                    collect_pat(src, &m.pattern, out);
                    collect_expr(src, &m.value, out);
                }
            }
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
    format!(
        r#"{{"from":{},"to":{},"message":"{}"}}"#,
        from,
        to,
        escape_json_str(message)
    )
}

// ── Single-file bundle helper ─────────────────────────────────────────────────

const WASM_ENTRY_PATH: &str = "main.lume";

fn single_bundle(src: &str) -> Result<Vec<BundleModule>, String> {
    let mut visited = HashSet::new();
    let mut bundle = Vec::new();
    collect_embedded_bundle(
        PathBuf::from(WASM_ENTRY_PATH),
        "_mod_main".to_string(),
        Loader::parse(src)?,
        &mut visited,
        &mut bundle,
    )?;
    Ok(bundle)
}

fn collect_embedded_bundle(
    canonical: PathBuf,
    var: String,
    program: Program,
    visited: &mut HashSet<PathBuf>,
    bundle: &mut Vec<BundleModule>,
) -> Result<(), String> {
    if !visited.insert(canonical.clone()) {
        return Ok(());
    }

    for use_decl in &program.uses {
        let dep_src = stdlib_source(&use_decl.path).ok_or_else(|| {
            format!(
                "WASM codegen only supports embedded stdlib imports (lume:*); unsupported import: {}",
                use_decl.path
            )
        })?;
        let dep_program = Loader::parse(dep_src)?;
        collect_embedded_bundle(
            stdlib_path(&use_decl.path),
            stdlib_var(&use_decl.path),
            dep_program,
            visited,
            bundle,
        )?;
    }

    bundle.push(BundleModule {
        canonical,
        var,
        program,
    });
    Ok(())
}

fn stdlib_var(path: &str) -> String {
    let suffix: String = path
        .trim_start_matches("lume:")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("_mod_{suffix}")
}

/// Type-check and lower a bundle of AST modules into IR modules.
fn lower_bundle(
    b: &[BundleModule],
) -> Result<(Vec<codegen::IrModule>, types::infer::VariantEnv), String> {
    use std::collections::HashMap;

    let mut global = lower::GlobalCtx {
        traits: HashMap::new(),
        impls: HashMap::new(),
        param_impls: Vec::new(),
        variants: HashMap::new(),
    };
    for m in b.iter() {
        for item in &m.program.items {
            match item {
                TopItem::TraitDef(td) => {
                    global.traits.insert(td.name.clone(), td.clone());
                }
                TopItem::ImplDef(id) => {
                    let dict = lower::dict_name(&id.trait_name, &id.type_name);
                    if id.impl_constraints.is_empty() {
                        global.impls.insert(
                            (id.trait_name.clone(), id.type_name.clone()),
                            lower::ImplEntry {
                                module_var: Some(m.var.clone()),
                                dict_ident: dict,
                            },
                        );
                    } else {
                        global.param_impls.push(lower::ParamImplEntry {
                            trait_name: id.trait_name.clone(),
                            target_type: id.target_type.clone(),
                            constraints: id.impl_constraints.clone(),
                            module_var: Some(m.var.clone()),
                            dict_ident: dict,
                        });
                    }
                }
                TopItem::TypeDef(td) => {
                    for variant in &td.variants {
                        global.variants.insert(
                            variant.name.clone(),
                            types::infer::VariantInfo {
                                type_name: td.name.clone(),
                                type_params: td.params.clone(),
                                wraps: variant.wraps.clone(),
                            },
                        );
                    }
                }
                _ => {}
            }
        }
    }

    {
        let mut scratch = types::Subst::new();
        let (_, builtin_variants) = builtin_env(&mut scratch);
        for (name, info) in builtin_variants.all() {
            global.variants.entry(name.clone()).or_insert_with(|| info.clone());
        }
    }

    let mut ir_modules = Vec::new();
    for m in b.iter() {
        let local_global = lower::GlobalCtx {
            traits: global.traits.clone(),
            impls: global
                .impls
                .iter()
                .map(|(k, e)| {
                    let is_local = e.module_var.as_deref() == Some(&m.var);
                    (
                        k.clone(),
                        lower::ImplEntry {
                            module_var: if is_local { None } else { e.module_var.clone() },
                            dict_ident: e.dict_ident.clone(),
                        },
                    )
                })
                .collect(),
            param_impls: global
                .param_impls
                .iter()
                .map(|pi| lower::ParamImplEntry {
                    trait_name: pi.trait_name.clone(),
                    target_type: pi.target_type.clone(),
                    constraints: pi.constraints.clone(),
                    module_var: if pi.module_var.as_deref() == Some(&m.var) {
                        None
                    } else {
                        pi.module_var.clone()
                    },
                    dict_ident: pi.dict_ident.clone(),
                })
                .collect(),
            variants: global.variants.clone(),
        };

        let module_path = Some(m.canonical.as_path());
        let (node_types, type_env) = types::infer::elaborate_with_env(&m.program, module_path)
            .map(|(nt, env, _)| (nt, env))
            .map_err(|e| format!("{}: type error: {e}", m.canonical.display()))?;
        let ir_mod = lower::lower(m.program.clone(), &node_types, &type_env, &local_global);
        ir_modules.push(codegen::IrModule {
            canonical: m.canonical.clone(),
            module: ir_mod,
            var: m.var.clone(),
        });
    }

    let mut variant_env = types::infer::VariantEnv::default();
    for (name, info) in global.variants {
        variant_env.insert(name, info);
    }
    Ok((ir_modules, variant_env))
}

// ── Public WASM API ───────────────────────────────────────────────────────────

/// Returns a JSON array of the built-in stdlib module paths,
/// e.g. `["lume:list","lume:math",…]`.
#[wasm_bindgen]
pub fn stdlib_modules() -> String {
    let items: Vec<String> = STDLIB_MODULES
        .iter()
        .map(|&m| format!("\"{}\"", escape_json_str(m)))
        .collect();
    format!("[{}]", items.join(","))
}

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
    types::infer::check_program(&program, Some(Path::new(WASM_ENTRY_PATH)))
        .map(|ty| JsValue::from_str(&ty.to_string()))
        .map_err(|e| JsValue::from_str(&e.to_string()))
}

/// Transpile to JavaScript (type-checks first). Returns JS code or throws.
#[wasm_bindgen]
pub fn to_js(src: &str) -> Result<JsValue, JsValue> {
    let bundle = single_bundle(src).map_err(|e| JsValue::from_str(&e))?;
    let entry = bundle
        .last()
        .ok_or_else(|| JsValue::from_str("internal error: empty bundle"))?;
    types::infer::check_program(&entry.program, Some(Path::new(WASM_ENTRY_PATH)))
        .map_err(|e| JsValue::from_str(&e.to_string()))?;
    let (ir_modules, variant_env) =
        lower_bundle(&bundle).map_err(|e| JsValue::from_str(&e))?;
    Ok(JsValue::from_str(&codegen::js::emit(&ir_modules, variant_env)))
}

/// Transpile to Lua (type-checks first). Returns Lua code or throws.
#[wasm_bindgen]
pub fn to_lua(src: &str) -> Result<JsValue, JsValue> {
    let bundle = single_bundle(src).map_err(|e| JsValue::from_str(&e))?;
    let entry = bundle
        .last()
        .ok_or_else(|| JsValue::from_str("internal error: empty bundle"))?;
    types::infer::check_program(&entry.program, Some(Path::new(WASM_ENTRY_PATH)))
        .map_err(|e| JsValue::from_str(&e.to_string()))?;
    let (ir_modules, variant_env) =
        lower_bundle(&bundle).map_err(|e| JsValue::from_str(&e))?;
    Ok(JsValue::from_str(&codegen::lua::emit(&ir_modules, variant_env)))
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
    match types::infer::check_program(&program, Some(Path::new(WASM_ENTRY_PATH))) {
        Err(e) => {
            let (from, to) = span_to_range(src, &e.span);
            let to = to.max(from + 1);
            format!("[{}]", diag_json(from, to, &e.error.to_string()))
        }
        Ok(_) => "[]".to_string(),
    }
}

// ── Autocomplete ──────────────────────────────────────────────────────────────

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn ident_start_at(bytes: &[u8], end: usize) -> usize {
    let mut i = end;
    while i > 0 && is_ident_char(bytes[i - 1]) {
        i -= 1;
    }
    i
}

/// Run `elaborate_with_env_partial` on `src`, returning `(name, type)` pairs.
/// Always succeeds - type errors are ignored so completions work even with errors.
fn try_elaborate_env(src: &str) -> Option<Vec<(String, String)>> {
    let tokens = Lexer::new(src).tokenize().ok()?;
    let program = parser::parse_program(&tokens).ok()?;
    let (_, env, _, _) = elaborate_with_env_partial(&program, Some(Path::new(WASM_ENTRY_PATH)));
    Some(
        env.iter()
            .map(|(name, scheme): (&String, &types::Scheme)| (name.clone(), scheme.ty.to_string()))
            .collect(),
    )
}

fn build_completions_json(mut items: Vec<(String, String)>, prefix: &str) -> String {
    items.retain(|(label, _)| label.starts_with(prefix) && label.as_str() != prefix);
    items.sort_by(|a, b| a.0.cmp(&b.0));
    items.dedup_by(|a, b| a.0 == b.0);
    let entries: Vec<String> = items
        .into_iter()
        .map(|(label, detail)| {
            format!(
                r#"{{"label":"{}","detail":"{}"}}"#,
                escape_json_str(&label),
                escape_json_str(&detail)
            )
        })
        .collect();
    format!("[{}]", entries.join(","))
}

/// Returns a JSON array of completion items `[{label, detail}]` for the given
/// byte `offset` in `src`.  Handles use-path completions (inside `use … = "…"`),
/// field/record completions (cursor follows `.`), and identifier completions.
#[wasm_bindgen]
pub fn complete(src: &str, offset: usize) -> String {
    let bytes = src.as_bytes();
    let offset = offset.min(src.len());

    // Use-path completions: check whether the cursor is inside the path string
    // of a `use` declaration.  Must run before the word/dot checks below.
    let line_start = src[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0);
    if let Some(ctx) = use_path_context(&src[line_start..offset]) {
        return use_path_completions_json(ctx);
    }

    let word_start = ident_start_at(bytes, offset);
    let prefix = &src[word_start..offset];

    // Suppress completions when the cursor is on a binding name (after `let`).
    let before_word = src[line_start..word_start].trim_end();
    let last_token = before_word
        .rsplit(|c: char| !c.is_alphanumeric() && c != '_')
        .find(|s| !s.is_empty())
        .unwrap_or("");
    if last_token == "let" {
        return "[]".to_string();
    }

    // Field access: character immediately before the current word is '.'
    if word_start > 0 && bytes[word_start - 1] == b'.' {
        let dot_pos = word_start - 1;
        return field_completions(src, dot_pos, offset, prefix);
    }

    ident_completions(src, word_start, offset, prefix)
}

fn use_path_completions_json(ctx: lume_core::loader::UsePathContext) -> String {
    match ctx.kind {
        UsePathKind::Stdlib => {
            let lower = ctx.prefix.to_lowercase();
            let mut items: Vec<String> = STDLIB_MODULES
                .iter()
                .filter_map(|&m| {
                    let name = m.strip_prefix("lume:").unwrap();
                    if lower.is_empty() || name.contains(&*lower) {
                        Some(format!(
                            r#"{{"label":"{}","detail":"stdlib"}}"#,
                            escape_json_str(name)
                        ))
                    } else {
                        None
                    }
                })
                .collect();
            items.sort();
            format!("[{}]", items.join(","))
        }
        // No filesystem access in WASM.
        UsePathKind::File => "[]".to_string(),
    }
}

fn ident_completions(src: &str, word_start: usize, offset: usize, prefix: &str) -> String {
    // Try the source as-is.
    if let Some(items) = try_elaborate_env(src) {
        return build_completions_json(items, prefix);
    }

    // Replace the partial word with `0` so the source is more likely to
    // type-check (fixes "unbound variable" errors at the cursor).
    if !prefix.is_empty() {
        let modified = format!("{}0{}", &src[..word_start], &src[offset..]);
        if let Some(items) = try_elaborate_env(&modified) {
            return build_completions_json(items, prefix);
        }
    }

    // Fallback: builtins only.
    let mut subst = types::Subst::new();
    let (env, _) = builtin_env(&mut subst);
    let items: Vec<(String, String)> = env
        .iter()
        .map(|(n, s)| (n.clone(), s.ty.to_string()))
        .collect();
    build_completions_json(items, prefix)
}

fn field_completions(src: &str, dot_pos: usize, cursor: usize, prefix: &str) -> String {
    // Identifier immediately before the dot.
    let rec_end = dot_pos;
    let rec_start = ident_start_at(src.as_bytes(), rec_end);
    let record_name = &src[rec_start..rec_end];
    if record_name.is_empty() {
        return "[]".to_string();
    }

    // Build a modified source with the entire `.FIELD` removed so the
    // record identifier can be resolved cleanly by the type checker.
    // Skip any remaining ident chars after the cursor too.
    let bytes = src.as_bytes();
    let mut after = cursor;
    while after < src.len() && is_ident_char(bytes[after]) {
        after += 1;
    }
    let modified = format!("{}{}", &src[..dot_pos], &src[after..]);

    let get_record_fields = |s: &str| -> Option<Vec<(String, String)>> {
        let tokens = Lexer::new(s).tokenize().ok()?;
        let program = parser::parse_program(&tokens).ok()?;
        let (_, env, _, _) = elaborate_with_env_partial(&program, Some(Path::new(WASM_ENTRY_PATH)));
        let scheme = env.lookup(record_name)?;
        if let Ty::Record(row) = &scheme.ty {
            Some(
                row.fields
                    .iter()
                    .map(|(name, ty)| (name.clone(), ty.to_string()))
                    .collect(),
            )
        } else {
            None
        }
    };

    let fields = get_record_fields(&modified).or_else(|| get_record_fields(src));
    match fields {
        None => "[]".to_string(),
        Some(fields) => build_completions_json(fields, prefix),
    }
}

/// Returns the inferred type of the expression under `offset` (byte offset),
/// or `null` if no type information is available at that position.
/// Designed for use with `hoverTooltip` in CodeMirror.
#[wasm_bindgen]
pub fn type_at(src: &str, offset: usize) -> Option<String> {
    let tokens = Lexer::new(src).tokenize().ok()?;
    let program = parser::parse_program(&tokens).ok()?;
    let (node_types, _, _, _) = elaborate_with_env_partial(&program, Some(Path::new(WASM_ENTRY_PATH)));

    let mut spans: Vec<(usize, usize, NodeId)> = Vec::new();
    collect_program(src, &program, &mut spans);

    // Keep only spans that contain the cursor offset.
    spans.retain(|(from, to, _)| *from <= offset && offset < *to);
    // Sort ascending by range size - smallest (innermost) first.
    // For equal-length spans (e.g. Apply nodes sharing the func token's span),
    // sort by NodeId descending: assign_node_ids is pre-order, so inner leaves
    // always have a higher id than the parent Apply that wraps them.
    spans.sort_by(|(fa, ta, ia), (fb, tb, ib)| (ta - fa).cmp(&(tb - fb)).then(ib.cmp(ia)));

    for (_, _, id) in &spans {
        if let Some(ty) = node_types.get(id) {
            return Some(ty.to_string());
        }
    }
    None
}
