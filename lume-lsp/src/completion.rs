use lume::{
    loader::{use_path_context, UsePathContext, STDLIB_MODULES},
    types::Ty,
};
use tower_lsp::lsp_types::*;

use crate::analysis::DocInfo;
use crate::hover::{format_trait_method_ty, utf16_to_byte};

// ── Completion context ───────────────────────────────────────────────────────

/// The completion context derived from the text before the cursor.
pub enum CompletionCtx {
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
pub fn completion_ctx(text: &str, pos: Position) -> CompletionCtx {
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
    let col = utf16_to_byte(line, pos.character);
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

// ── Completion item builders ─────────────────────────────────────────────────

/// Return completion items for `TraitName.` — the methods of that trait.
pub fn trait_completions(
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
pub fn field_completions(
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
pub fn ident_completions(doc: &DocInfo, prefix: &str, replace_range: Range) -> Vec<CompletionItem> {
    let lower = prefix.to_lowercase();
    doc.top_env
        .iter()
        .filter_map(|(name, scheme)| {
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
pub fn stdlib_path_completions(
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
pub fn file_path_completions(
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
