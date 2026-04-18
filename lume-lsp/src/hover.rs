use lume_core::{
    ast::{TraitDef, Type},
    ast::NodeId,
    types::{unify, Scheme, Subst, Ty},
    types::infer::TypeEnv,
};
use tower_lsp::lsp_types::*;

use crate::analysis::DocInfo;

// ── Hover lookup ─────────────────────────────────────────────────────────────

/// Find the type and NodeId of the innermost expression at `pos`.
///
/// Spans are 1-indexed (line and col); LSP positions are 0-indexed.
pub fn type_at_with_id(pos: Position, doc: &DocInfo) -> Option<(NodeId, Ty)> {
    let line = pos.line as usize + 1;
    let col = pos.character as usize + 1;
    doc.span_index
        .get(&line)?
        .iter()
        .find(|(span, _)| span.col <= col && col < span.col + span.len)
        .and_then(|(_, id)| doc.node_types.get(id).map(|ty| (*id, ty.clone())))
}

/// Convert a UTF-16 code-unit offset (LSP `Position.character`) to a byte
/// offset within a single line of UTF-8 text.
pub fn utf16_to_byte(line: &str, utf16_offset: u32) -> usize {
    let mut utf16_count = 0u32;
    for (byte_idx, ch) in line.char_indices() {
        if utf16_count >= utf16_offset {
            return byte_idx;
        }
        utf16_count += ch.len_utf16() as u32;
    }
    line.len()
}

/// Return the identifier word under the cursor (for the hover label).
pub fn word_at(text: &str, line: u32, character: u32) -> Option<&str> {
    let line_text = text.lines().nth(line as usize)?;
    let col = utf16_to_byte(line_text, character);
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

// ── Trait hover helpers ──────────────────────────────────────────────────────

/// Format a trait method type with its constraint for display.
/// e.g. trait `ToText a` with method `toNum : a -> Num` → `ToText a => a -> Num`
pub fn format_trait_method_ty(trait_def: &TraitDef, method_ty: &str) -> String {
    format!("{} {} => {}", trait_def.name, trait_def.type_param, method_ty)
}

/// Walk a generic AST `Type` and a resolved `Ty` in parallel to find the
/// concrete type that the trait's type parameter unified to at a call site.
pub fn extract_trait_param(ast_ty: &Type, resolved: &Ty, param: &str) -> Option<Ty> {
    match (ast_ty, resolved) {
        (Type::Var(v), _) if v == param => Some(resolved.clone()),
        (Type::Func { param: ap, ret: ar }, Ty::Func(rp, rr)) => {
            extract_trait_param(ap, rp, param)
                .or_else(|| extract_trait_param(ar, rr, param))
        }
        (Type::App { .. }, _) | (Type::Constructor(_), _) => {
            // Flatten AST App tree and match against Ty App tree.
            let mut current: &Type = ast_ty;
            let mut args = Vec::new();
            while let Type::App { callee, arg } = current {
                args.push(arg.as_ref());
                current = callee.as_ref();
            }
            args.reverse();
            let (_, ty_args) = resolved.flatten_app();
            args.iter()
                .zip(ty_args.iter())
                .find_map(|(a, t)| extract_trait_param(a, t, param))
        }
        _ => None,
    }
}

/// Format a trait constraint with proper parenthesisation, e.g. `Show (List Num)`.
pub fn format_constraint(trait_name: &str, param_ty: &Ty) -> String {
    let needs_parens = matches!(param_ty, Ty::Func(..) | Ty::App(..));
    if needs_parens {
        format!("{} ({})", trait_name, param_ty)
    } else {
        format!("{} {}", trait_name, param_ty)
    }
}

// ── Typed-hole suggestions ───────────────────────────────────────────────────

/// Instantiate a scheme by substituting each quantified variable with a
/// fresh type variable, returning the concrete (but still possibly generic) Ty.
fn instantiate_fresh(scheme: &Scheme) -> Ty {
    let mut s = Subst::new();
    for &var in &scheme.vars {
        let fresh = s.fresh_var();
        s.bind_ty(var, Ty::Var(fresh));
    }
    s.apply(&scheme.ty)
}

/// Return true if `scheme` can be instantiated to a type that unifies with
/// `hole_ty`. Each call uses a fresh substitution so attempts are independent.
fn fits_hole(hole_ty: &Ty, scheme: &Scheme) -> bool {
    let candidate = instantiate_fresh(scheme);
    let mut s = Subst::new();
    unify(&mut s, candidate, hole_ty.clone()).is_ok()
}

/// Collect up to `limit` binding names (sorted alphabetically) whose type is
/// compatible with `hole_ty`. Returns an empty vec when the hole is a bare
/// free variable (it would match everything, making the list meaningless).
fn suggest_for_hole<'e>(
    hole_ty: &Ty,
    env: &'e TypeEnv,
    limit: usize,
) -> Vec<(&'e str, &'e Scheme)> {
    // A bare free var means the type is unconstrained — suggestions are noise.
    if matches!(hole_ty, Ty::Var(_)) {
        return vec![];
    }

    let mut matches: Vec<(&str, &Scheme)> = env
        .iter()
        .filter(|(name, scheme)| !name.starts_with('_') && fits_hole(hole_ty, scheme))
        .map(|(name, scheme)| (name.as_str(), scheme))
        .collect();

    matches.sort_by_key(|(name, _)| *name);
    matches.truncate(limit);
    matches
}

/// Build the hover label for a given cursor position.
///
/// Returns `None` when no hover information is available.
pub fn hover_label(pos: Position, text: &str, doc: &DocInfo) -> Option<String> {
    if let Some((node_id, ty)) = type_at_with_id(pos, doc) {
        // Check if this node is a TraitCall — if so, split hover between
        // the trait name part and the method name part.
        if let Some((trait_name, method_name)) = doc.trait_calls.get(&node_id) {
            let word = word_at(text, pos.line, pos.character);
            if word == Some(trait_name.as_str()) {
                // Cursor is on the trait name (e.g. "Render" in Render.render)
                // → show the trait definition, same as hovering on a TraitDef.
                return Some(
                    doc.extra_hovers
                        .iter()
                        .find(|(_, l)| l.starts_with(&format!("trait {} ", trait_name)))
                        .map(|(_, l)| l.clone())
                        .unwrap_or_else(|| format!("trait {trait_name}")),
                );
            }
            // Cursor is on the method name (e.g. "render" in Render.render)
            // → show the resolved type with concrete constraint.
            if let Some(trait_def) = doc.trait_env.get(trait_name) {
                if let Some(method) = trait_def.methods.iter().find(|m| &m.name == method_name) {
                    let param_ty =
                        extract_trait_param(&method.ty, &ty, &trait_def.type_param);
                    let constraint = match &param_ty {
                        Some(pt) => format_constraint(trait_name, pt),
                        None => format!("{} {}", trait_name, trait_def.type_param),
                    };
                    return Some(format!("{method_name} : {constraint} => {ty}"));
                }
            }
            return Some(format!("{method_name} : {ty}"));
        }

        // Regular expression — show word : type.
        match word_at(text, pos.line, pos.character) {
            Some("_") => {
                // Typed hole: show the expected type and suggest fitting bindings.
                let mut label = format!("_ : {ty}");
                let suggestions = suggest_for_hole(&ty, &doc.top_env, 5);
                if !suggestions.is_empty() {
                    label.push_str("\n\n**Fits:**");
                    for (name, scheme) in suggestions {
                        label.push_str(&format!("\n- `{name}` : `{scheme}`"));
                    }
                }
                Some(label)
            }
            Some(w) if w.starts_with(|c: char| c.is_alphabetic() || c == '_') => {
                if let Some(scheme) = doc.top_env.lookup(w) {
                    Some(format!("{w} : {scheme}"))
                } else {
                    Some(format!("{w} : {ty}"))
                }
            }
            _ => Some(ty.to_string()),
        }
    } else {
        // No type in node_types — try extra_hovers (trait methods, etc.)
        let lsp_line = pos.line as usize + 1;
        let lsp_col = pos.character as usize + 1;
        if let Some((_, label)) = doc.extra_hovers.iter().find(|(span, _)| {
            span.line == lsp_line && span.col <= lsp_col && lsp_col < span.col + span.len
        }) {
            return Some(label.clone());
        }
        // Last resort: try top_env by word under cursor
        match word_at(text, pos.line, pos.character) {
            Some(w) if w.starts_with(|c: char| c.is_alphabetic() || c == '_') => {
                doc.top_env.lookup(w).map(|scheme| format!("{w} : {scheme}"))
            }
            _ => None,
        }
    }
}
