use lume_core::{
    ast::{FixityAssoc, TraitDef, Type},
    ast::NodeId,
    error::Span,
    types::{format_ty_with_hints, unify, Scheme, Subst, Ty},
    types::infer::TypeEnv,
};
use tower_lsp::lsp_types::*;

use crate::analysis::{is_op_name, DocInfo};

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

fn paren_type_at_with_id(pos: Position, text: &str, doc: &DocInfo) -> Option<(NodeId, Ty)> {
    let span = paren_hover_span(pos, text, doc)?;
    doc.paren_span_index
        .get(&span.line)?
        .iter()
        .find(|(s, _)| s.line == span.line && s.col == span.col && s.len == span.len)
        .and_then(|(_, id)| doc.node_types.get(id).map(|ty| (*id, ty.clone())))
}

pub fn paren_hover_span(pos: Position, text: &str, doc: &DocInfo) -> Option<Span> {
    let line_text = text.lines().nth(pos.line as usize)?;
    let byte_col = utf16_to_byte(line_text, pos.character);
    let line = pos.line as usize + 1;
    let candidates = [byte_col, byte_col.saturating_sub(1)];

    for candidate in candidates {
        let Some(ch) = line_text.get(candidate..).and_then(|s| s.chars().next()) else {
            continue;
        };
        if ch != '(' && ch != ')' {
            continue;
        }
        let col = candidate + 1;
        if let Some(found) = doc
            .paren_span_index
            .get(&line)?
            .iter()
            .find(|(span, _)| span.col <= col && col <= span.col + span.len)
            .map(|(span, _)| span.clone())
        {
            return Some(found);
        }
    }

    None
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

/// Return the identifier OR operator word under the cursor (for the hover label).
///
/// First tries alphanumeric identifier characters. If no identifier is found,
/// looks for a contiguous run of operator characters at the cursor position.
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
    if start < end {
        return Some(&line_text[start..end]);
    }

    // No identifier found — try operator characters.
    let is_op = |c: char| "!#$%&*+./<=>?@\\^|~-:".contains(c);
    let op_start = line_text[..col]
        .rfind(|c: char| !is_op(c))
        .map(|i| i + 1)
        .unwrap_or(0);
    let op_end = line_text[col..]
        .find(|c: char| !is_op(c))
        .map(|i| i + col)
        .unwrap_or(line_text.len());
    if op_start < op_end {
        Some(&line_text[op_start..op_end])
    } else {
        None
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
        let _ = s.bind_ty(var, Ty::Var(fresh));
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

/// Wrap `content` in a Lume fenced code block for syntax-highlighted hover display.
fn lume_block(content: &str) -> String {
    format!("```lume\n{content}\n```")
}

/// Build the hover label for a given cursor position.
///
/// Returns `None` when no hover information is available.
pub fn hover_label(pos: Position, text: &str, doc: &DocInfo) -> Option<String> {
    if let Some((_, ty)) = paren_type_at_with_id(pos, text, doc) {
        let ty_str = format_ty_with_hints(&ty, &doc.var_name_hints);
        return Some(lume_block(&ty_str));
    }

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
                        .map(|(_, l)| lume_block(l))
                        .unwrap_or_else(|| lume_block(&format!("trait {trait_name}"))),
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
                    return Some(lume_block(&format!("{method_name} : {constraint} => {ty}")));
                }
            }
            return Some(lume_block(&format!("{method_name} : {ty}")));
        }

        // Regular expression — show word : type.
        let ty_str = format_ty_with_hints(&ty, &doc.var_name_hints);
        match word_at(text, pos.line, pos.character) {
            Some("_") => {
                // Typed hole: show the expected type and suggest fitting bindings.
                let mut label = lume_block(&format!("_ : {ty_str}"));
                let suggestions = suggest_for_hole(&ty, &doc.top_env, 5);
                if !suggestions.is_empty() {
                    label.push_str("\n\n**Fits:**");
                    for (name, scheme) in suggestions {
                        label.push_str(&format!("\n- `{name}` : `{scheme}`"));
                    }
                }
                Some(label)
            }
            Some(w) => {
                let (display, fixity_suffix) = operator_display(w, doc);
                if let Some(scheme) = doc.top_env.lookup(w) {
                    Some(lume_block(&format!("{display} : {scheme}{fixity_suffix}")))
                } else if is_op_name(w) {
                    // Operator not in top_env — it may be a trait method operator.
                    // Search trait_env for an unambiguous match.
                    let mut matches: Vec<(&str, &str, &lume_core::ast::Type)> = doc
                        .trait_env
                        .values()
                        .flat_map(|td| {
                            td.methods.iter().filter(|m| m.name == w).map(move |m| {
                                (td.name.as_str(), td.type_param.as_str(), &m.ty)
                            })
                        })
                        .collect();
                    if matches.len() == 1 {
                        let (trait_name, type_param, method_ty) = matches.remove(0);
                        Some(lume_block(&format!(
                            "{display} : ({trait_name} {type_param}) => {method_ty}{fixity_suffix}"
                        )))
                    } else if !matches.is_empty() {
                        // Ambiguous — list all candidates.
                        let options: Vec<String> = matches
                            .iter()
                            .map(|(tn, tp, _)| format!("({tn} {tp})"))
                            .collect();
                        Some(lume_block(&format!("{display} : {ty_str}  -- ambiguous: {}{fixity_suffix}", options.join(", "))))
                    } else {
                        Some(lume_block(&format!("{display} : {ty_str}{fixity_suffix}")))
                    }
                } else {
                    Some(lume_block(&format!("{display} : {ty_str}{fixity_suffix}")))
                }
            }
            _ => Some(lume_block(&ty_str)),
        }
    } else {
        // No type in node_types — try extra_hovers (trait methods, etc.)
        let lsp_line = pos.line as usize + 1;
        let lsp_col = pos.character as usize + 1;
        if let Some((_, label)) = doc.extra_hovers.iter().find(|(span, _)| {
            span.line == lsp_line && span.col <= lsp_col && lsp_col < span.col + span.len
        }) {
            return Some(lume_block(label));
        }
        // Last resort: try top_env by word under cursor (identifier or operator).
        match word_at(text, pos.line, pos.character) {
            Some(w) => {
                let (display, fixity_suffix) = operator_display(w, doc);
                doc.top_env.lookup(w).map(|scheme| lume_block(&format!("{display} : {scheme}{fixity_suffix}")))
            }
            _ => None,
        }
    }
}

/// Returns `true` if the word consists entirely of operator characters.
fn is_operator_word(w: &str) -> bool {
    !w.is_empty() && w.chars().all(|c| "!#$%&*+./<=>?@\\^|~-:".contains(c))
}

/// Built-in operator fixity descriptions (operator → "assoc prec" string).
///
/// Derived from `infix_bp` in parser.rs.  User-defined operators override
/// these via the `fixity_table` in `DocInfo`.
fn builtin_fixity(op: &str) -> Option<&'static str> {
    match op {
        "|>" => Some("infixl 1"),
        "||" => Some("infixl 2"),
        "&&" => Some("infixl 3"),
        "==" | "!=" | "<" | ">" | "<=" | ">=" => Some("infixl 5"),
        "++" => Some("infixr 6"),
        "+" | "-" => Some("infixl 7"),
        "*" | "/" => Some("infixl 8"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{hover_label, paren_hover_span};
    use crate::analysis::analyse;
    use tower_lsp::lsp_types::{Position, Url};

    #[test]
    fn hover_on_left_paren_shows_group_type() {
        let src = "let x = (1 + 2)\n";
        let uri = Url::parse("file:///hover_paren_left.lume").unwrap();
        let (doc, diagnostics) = analyse(&uri, src);
        assert!(diagnostics.is_empty());
        let doc = doc.unwrap();

        let label = hover_label(Position::new(0, 8), src, &doc).unwrap();
        assert_eq!(label, "```lume\nNum\n```");
        let span = paren_hover_span(Position::new(0, 8), src, &doc).unwrap();
        assert_eq!(span.line, 1);
        assert_eq!(span.col, 9);
        assert_eq!(span.len, 7);
    }

    #[test]
    fn hover_on_right_paren_shows_group_type() {
        let src = "let x = (1 + 2)\n";
        let uri = Url::parse("file:///hover_paren_right.lume").unwrap();
        let (doc, diagnostics) = analyse(&uri, src);
        assert!(diagnostics.is_empty());
        let doc = doc.unwrap();

        let label = hover_label(Position::new(0, 14), src, &doc).unwrap();
        assert_eq!(label, "```lume\nNum\n```");
        let span = paren_hover_span(Position::new(0, 14), src, &doc).unwrap();
        assert_eq!(span.line, 1);
        assert_eq!(span.col, 9);
        assert_eq!(span.len, 7);
    }
}

/// For a word under the cursor, return:
/// - the display name (wrapped in parens for operators, plain otherwise)
/// - a fixity suffix string (e.g. `"\ninfixl 2"`) for ALL operators (builtin or user-defined)
fn operator_display<'a>(w: &'a str, doc: &DocInfo) -> (String, String) {
    if is_operator_word(w) {
        let display = format!("({})", w);
        let fixity_str = if let Some(entry) = doc.fixity_table.get(w) {
            format_fixity(entry.bps, &entry.assoc)
        } else if let Some(s) = builtin_fixity(w) {
            s.to_string()
        } else {
            // Unknown operator — show default precedence matching parser.rs default
            "infixr 6".to_string()
        };
        (display, format!("\n{fixity_str}"))
    } else {
        (w.to_string(), String::new())
    }
}

/// Format the fixity of an operator for display in hover info.
///
/// Recovers the precedence from binding-power pair: `prec = bps.0 / 8`.
/// Returns e.g. `"infixl 2"`, `"infixr 6"`, `"infix 5"`.
pub fn format_fixity(bps: (u8, u8), assoc: &FixityAssoc) -> String {
    let prec = bps.0 / 8;
    match assoc {
        FixityAssoc::Left => format!("infixl {prec}"),
        FixityAssoc::Right => format!("infixr {prec}"),
        FixityAssoc::None => format!("infix {prec}"),
    }
}
