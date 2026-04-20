mod analysis;
mod completion;
mod hover;
mod semantic_tokens;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use lume_core::loader::UsePathKind;
use lume_core::types::Ty;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

use analysis::{analyse, span_to_range, DocInfo};
use completion::{
    completion_ctx, field_completions, file_path_completions, ident_completions,
    stdlib_path_completions, trait_completions, CompletionCtx,
};
use hover::{hover_label, word_at};
use semantic_tokens::{compute_semantic_tokens, LEGEND_TOKEN_TYPES};

const DEBOUNCE_MS: u64 = 150;

struct Backend {
    client: Client,
    documents: DashMap<Url, String>,
    doc_info: DashMap<Url, DocInfo>,
    edit_gen: DashMap<Url, AtomicU64>,
}

impl Backend {
    fn bump_gen(&self, uri: &Url) -> u64 {
        let entry = self.edit_gen.entry(uri.clone()).or_insert_with(|| AtomicU64::new(0));
        entry.value().fetch_add(1, Ordering::Relaxed) + 1
    }

    fn current_gen(&self, uri: &Url) -> u64 {
        self.edit_gen
            .get(uri)
            .map(|e| e.value().load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    async fn refresh(&self, uri: &Url, text: &str) {
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

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
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
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec![" ".to_string(), "(".to_string()]),
                    retrigger_characters: None,
                    work_done_progress_options: Default::default(),
                }),
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend: SemanticTokensLegend {
                                token_types: LEGEND_TOKEN_TYPES.to_vec(),
                                token_modifiers: vec![],
                            },
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                            range: None,
                            ..Default::default()
                        },
                    ),
                ),
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

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = match params.content_changes.into_iter().last() {
            Some(c) => c.text,
            None => return,
        };
        self.documents.insert(uri.clone(), text.clone());
        let gen = self.bump_gen(&uri);
        tokio::time::sleep(tokio::time::Duration::from_millis(DEBOUNCE_MS)).await;
        if self.current_gen(&uri) != gen {
            return;
        }
        self.refresh(&uri, &text).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.documents.remove(&uri);
        self.doc_info.remove(&uri);
        self.edit_gen.remove(&uri);
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
            CompletionCtx::TraitAccess {
                trait_name, prefix, ..
            } => {
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

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = &params.text_document.uri;
        let text = match self.documents.get(uri) {
            Some(t) => t.clone(),
            None => return Ok(None),
        };
        let tokens = compute_semantic_tokens(&text);
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data: tokens,
        })))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;

        let text = match self.documents.get(uri) {
            Some(t) => t.clone(),
            None => return Ok(None),
        };

        let doc = match self.doc_info.get(uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let word = match word_at(&text, pos.line, pos.character) {
            Some(w) => w.to_string(),
            None => return Ok(None),
        };

        // Check if this name is imported from another file.
        if let Some(target_path) = doc.imports.get(&word) {
            let target_uri = match Url::from_file_path(target_path) {
                Ok(u) => u,
                Err(_) => return Ok(None),
            };

            // Try to find the definition in the target file.
            // First check if we already have doc_info for it.
            if let Some(target_doc) = self.doc_info.get(&target_uri) {
                if let Some(span) = target_doc.definitions.get(&word) {
                    return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                        uri: target_uri,
                        range: span_to_range(span),
                    })));
                }
            }

            // If not cached, read and analyze the target file on demand.
            if let Ok(src) = std::fs::read_to_string(target_path) {
                let (target_info, _) = analyse(&target_uri, &src);
                if let Some(target_doc) = target_info {
                    let result = target_doc.definitions.get(&word).map(|span| {
                        GotoDefinitionResponse::Scalar(Location {
                            uri: target_uri.clone(),
                            range: span_to_range(span),
                        })
                    });
                    // Cache for future lookups.
                    self.doc_info.insert(target_uri, target_doc);
                    return Ok(result);
                }
            }

            // Fallback: jump to start of the target file.
            return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                uri: target_uri,
                range: Range::default(),
            })));
        }

        // Local definition in current file.
        let span = match doc.definitions.get(&word) {
            Some(s) => s,
            None => return Ok(None),
        };

        let range = span_to_range(span);
        Ok(Some(GotoDefinitionResponse::Scalar(Location {
            uri: uri.clone(),
            range,
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

        let label = match hover_label(pos, &text, &doc) {
            Some(l) => l,
            None => return Ok(None),
        };

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

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = &params.text_document.uri;
        let doc = match self.doc_info.get(uri) {
            Some(d) => d,
            None => return Ok(None),
        };
        Ok(Some(DocumentSymbolResponse::Nested(doc.symbols.clone())))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = &params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;

        let text = match self.documents.get(uri) {
            Some(t) => t.clone(),
            None => return Ok(None),
        };

        let doc = match self.doc_info.get(uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let word = match word_at(&text, pos.line, pos.character) {
            Some(w) => w.to_string(),
            None => return Ok(None),
        };

        let mut locations = Vec::new();

        // Include the definition site if requested
        if params.context.include_declaration {
            if let Some(def_span) = doc.definitions.get(&word) {
                locations.push(Location {
                    uri: uri.clone(),
                    range: span_to_range(def_span),
                });
            }
        }

        // Include all reference sites
        if let Some(refs) = doc.references.get(&word) {
            for r in refs {
                locations.push(Location {
                    uri: uri.clone(),
                    range: span_to_range(r),
                });
            }
        }

        if locations.is_empty() {
            Ok(None)
        } else {
            Ok(Some(locations))
        }
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;

        let text = match self.documents.get(uri) {
            Some(t) => t.clone(),
            None => return Ok(None),
        };

        let doc = match self.doc_info.get(uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let word = match word_at(&text, pos.line, pos.character) {
            Some(w) => w.to_string(),
            None => return Ok(None),
        };

        let mut highlights = Vec::new();

        // Highlight the definition site as Write
        if let Some(def_span) = doc.definitions.get(&word) {
            highlights.push(DocumentHighlight {
                range: span_to_range(def_span),
                kind: Some(DocumentHighlightKind::WRITE),
            });
        }

        // Highlight all reference sites as Read
        if let Some(refs) = doc.references.get(&word) {
            for r in refs {
                highlights.push(DocumentHighlight {
                    range: span_to_range(r),
                    kind: Some(DocumentHighlightKind::READ),
                });
            }
        }

        if highlights.is_empty() {
            Ok(None)
        } else {
            Ok(Some(highlights))
        }
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;

        let text = match self.documents.get(uri) {
            Some(t) => t.clone(),
            None => return Ok(None),
        };

        let doc = match self.doc_info.get(uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        // Find the function being called at the cursor position.
        // Look backwards from cursor to find the function name.
        let line = match text.lines().nth(pos.line as usize) {
            Some(l) => l,
            None => return Ok(None),
        };
        let col = hover::utf16_to_byte(line, pos.character);
        let before = &line[..col.min(line.len())];

        // Find the last function-like identifier before the cursor
        // by scanning backward past arguments.
        let func_name = find_func_at_cursor(before);
        let func_name = match func_name {
            Some(n) => n,
            None => return Ok(None),
        };

        let scheme = match doc.top_env.lookup(func_name) {
            Some(s) => s,
            None => return Ok(None),
        };

        // Build the signature string and parameter info
        let sig_label = format!("{} : {}", func_name, scheme);
        let params_info = extract_param_labels(&scheme.ty);

        Ok(Some(SignatureHelp {
            signatures: vec![SignatureInformation {
                label: sig_label,
                documentation: doc
                    .doc_comments
                    .get(func_name)
                    .map(|d| Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: d.clone(),
                    })),
                parameters: if params_info.is_empty() {
                    None
                } else {
                    Some(params_info)
                },
                active_parameter: None,
            }],
            active_signature: Some(0),
            active_parameter: None,
        }))
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = &params.text_document.uri;
        let range = params.range;

        let text = match self.documents.get(uri) {
            Some(t) => t.clone(),
            None => return Ok(None),
        };

        let doc = match self.doc_info.get(uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let mut actions = Vec::new();

        // "Add type annotation" for unannotated bindings at cursor
        for (_name, span, ty_str) in &doc.unannotated_bindings {
            let binding_range = span_to_range(span);
            if range.start.line == binding_range.start.line {
                let annotation = format!(" : {}", ty_str);
                let insert_pos = Position {
                    line: binding_range.start.line,
                    character: binding_range.end.character,
                };
                let edit = TextEdit {
                    range: Range {
                        start: insert_pos,
                        end: insert_pos,
                    },
                    new_text: annotation,
                };
                let mut changes = HashMap::new();
                changes.insert(uri.clone(), vec![edit]);
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: format!("Add type annotation: {}", ty_str),
                    kind: Some(CodeActionKind::QUICKFIX),
                    edit: Some(WorkspaceEdit {
                        changes: Some(changes),
                        ..Default::default()
                    }),
                    ..Default::default()
                }));
            }
        }

        // "Fill match arms" — when cursor is on a `match ... in` with missing arms
        if let Some(action) = fill_match_arms_action(uri, &range, &text, &doc) {
            actions.push(CodeActionOrCommand::CodeAction(action));
        }

        if actions.is_empty() {
            Ok(None)
        } else {
            Ok(Some(actions))
        }
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri = &params.text_document.uri;
        let pos = params.position;

        let text = match self.documents.get(uri) {
            Some(t) => t.clone(),
            None => return Ok(None),
        };

        let doc = match self.doc_info.get(uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let word = match word_at(&text, pos.line, pos.character) {
            Some(w) => w.to_string(),
            None => return Ok(None),
        };

        // Only allow rename if the symbol has a definition in this file
        if doc.definitions.contains_key(&word) {
            let line = text.lines().nth(pos.line as usize).unwrap_or("");
            let col = hover::utf16_to_byte(line, pos.character);
            let is_ident = |c: char| c.is_alphanumeric() || c == '_';
            let start = line[..col]
                .rfind(|c: char| !is_ident(c))
                .map(|i| i + 1)
                .unwrap_or(0);
            let end = line[col..]
                .find(|c: char| !is_ident(c))
                .map(|i| i + col)
                .unwrap_or(line.len());
            Ok(Some(PrepareRenameResponse::Range(Range {
                start: Position {
                    line: pos.line,
                    character: start as u32,
                },
                end: Position {
                    line: pos.line,
                    character: end as u32,
                },
            })))
        } else {
            Ok(None)
        }
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = &params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let new_name = &params.new_name;

        let text = match self.documents.get(uri) {
            Some(t) => t.clone(),
            None => return Ok(None),
        };

        let doc = match self.doc_info.get(uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let word = match word_at(&text, pos.line, pos.character) {
            Some(w) => w.to_string(),
            None => return Ok(None),
        };

        // Collect all locations to rename: definition + references
        let mut edits = Vec::new();

        if let Some(def_span) = doc.definitions.get(&word) {
            edits.push(TextEdit {
                range: span_to_range(def_span),
                new_text: new_name.clone(),
            });
        }

        if let Some(refs) = doc.references.get(&word) {
            for r in refs {
                edits.push(TextEdit {
                    range: span_to_range(r),
                    new_text: new_name.clone(),
                });
            }
        }

        if edits.is_empty() {
            return Ok(None);
        }

        let mut changes = HashMap::new();
        changes.insert(uri.clone(), edits);
        Ok(Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }))
    }
}

// ── Signature help utilities ──────────────────────────────────────────────────

/// Scan backward from cursor to find the most likely function being applied.
/// In Lume, application is juxtaposition: `f x y` — so the function name is
/// the first identifier token in the current "application chain".
fn find_func_at_cursor(before: &str) -> Option<&str> {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    // Walk backwards past the current partial argument
    let trimmed = before.trim_end();
    if trimmed.is_empty() {
        return None;
    }

    // Find start of current token
    let last_word_end = trimmed.len();
    let last_word_start = trimmed
        .rfind(|c: char| !is_ident(c))
        .map(|i| i + 1)
        .unwrap_or(0);
    let last_word = &trimmed[last_word_start..last_word_end];

    // If the cursor is right after a space following an ident, that ident is the function.
    // If we're in an argument, look further back for the function.
    if before.ends_with(' ') || before.ends_with('(') {
        // The word just before the space is the function
        if !last_word.is_empty() && last_word.starts_with(|c: char| c.is_alphabetic() || c == '_') {
            return Some(last_word);
        }
    }

    // Look for function before the arguments: scan back past balanced parens and idents
    let before_current = &trimmed[..last_word_start].trim_end();
    if before_current.is_empty() {
        return None;
    }
    let func_end = before_current.len();
    let func_start = before_current
        .rfind(|c: char| !is_ident(c))
        .map(|i| i + 1)
        .unwrap_or(0);
    let func = &before_current[func_start..func_end];
    if !func.is_empty() && func.starts_with(|c: char| c.is_alphabetic() || c == '_') {
        Some(func)
    } else {
        None
    }
}

/// Extract parameter labels from a function type for signature help.
/// For `a -> b -> c -> d`, returns labels for params "a", "b", "c".
fn extract_param_labels(ty: &Ty) -> Vec<ParameterInformation> {
    let mut params = Vec::new();
    let mut current = ty;
    while let Ty::Func(param, ret) = current {
        let label = param.to_string();
        params.push(ParameterInformation {
            label: ParameterLabel::Simple(label),
            documentation: None,
        });
        current = ret;
    }
    params
}

// ── Fill match arms code action ─────────────────────────────────────────────

/// Generate a "Fill match arms" code action if the cursor is inside a
/// `match ... in` expression with missing variant arms.
fn fill_match_arms_action(
    uri: &Url,
    range: &Range,
    _text: &str,
    doc: &DocInfo,
) -> Option<CodeAction> {
    // Find a match expression whose span contains the cursor.
    let cursor_line = range.start.line as usize + 1; // Span uses 1-indexed lines
    let match_info = doc.match_exprs.iter().find(|m| {
        let m_line = m.span.line;
        let m_end = m.span.line + 20; // heuristic: match exprs span multiple lines
        cursor_line >= m_line && cursor_line <= m_end
    })?;

    // Look up the scrutinee's type.
    let scrut_ty = doc.node_types.get(&match_info.scrutinee_id)?;

    // Extract the type constructor name.
    let type_name = scrut_ty.con_name()?;

    // Get all variants of this type.
    let all_variants = doc.variant_env.variants_of_type(type_name);
    if all_variants.is_empty() {
        return None;
    }

    // Determine which variants are missing.
    let missing: Vec<&String> = all_variants
        .iter()
        .filter(|v| !match_info.existing_variants.contains(v))
        .collect();
    if missing.is_empty() {
        return None;
    }

    // Build the stub arms text.
    let mut arms_text = String::new();
    for variant_name in &missing {
        let info = doc.variant_env.lookup(variant_name)?;
        let pattern = match &info.wraps {
            None => variant_name.to_string(),
            Some(ast_ty) => match ast_ty {
                lume_core::ast::Type::Record(rec) => {
                    let fields: Vec<&str> =
                        rec.fields.iter().map(|f| f.name.as_str()).collect();
                    format!("{} {{ {} }}", variant_name, fields.join(", "))
                }
                _ => format!("{} _", variant_name),
            },
        };
        arms_text.push_str(&format!("  | {} -> _\n", pattern));
    }

    // Insert the arms at the end of the match expression (after the last existing arm).
    // We place them at the end of the match span's starting line area.
    let insert_pos = Position {
        line: (match_info.span.line as u32 - 1) + match_info.existing_variants.len() as u32 + 1,
        character: 0,
    };

    let edit = TextEdit {
        range: Range { start: insert_pos, end: insert_pos },
        new_text: arms_text,
    };
    let mut changes = HashMap::new();
    changes.insert(uri.clone(), vec![edit]);

    Some(CodeAction {
        title: format!("Fill missing match arms ({})", missing.len()),
        kind: Some(CodeActionKind::QUICKFIX),
        edit: Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }),
        ..Default::default()
    })
}

pub async fn run() {
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
        edit_gen: DashMap::new(),
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
