mod analysis;
mod completion;
mod hover;
mod semantic_tokens;

use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use lume::loader::UsePathKind;
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

/// Debounce delay before re-analysing a changed document.
const DEBOUNCE_MS: u64 = 150;

// ── LSP backend ──────────────────────────────────────────────────────────────

struct Backend {
    client: Client,
    documents: DashMap<Url, String>,
    doc_info: DashMap<Url, DocInfo>,
    /// Monotonically increasing edit generation per document.  A scheduled
    /// `refresh` only runs if the generation hasn't changed since it was
    /// enqueued, providing simple debouncing without extra dependencies.
    edit_gen: DashMap<Url, AtomicU64>,
}

impl Backend {
    /// Bump the generation counter for `uri` and return the new value.
    fn bump_gen(&self, uri: &Url) -> u64 {
        let entry = self.edit_gen.entry(uri.clone()).or_insert_with(|| AtomicU64::new(0));
        entry.value().fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Current generation for `uri`.
    fn current_gen(&self, uri: &Url) -> u64 {
        self.edit_gen
            .get(uri)
            .map(|e| e.value().load(Ordering::Relaxed))
            .unwrap_or(0)
    }

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

// ── Language server trait impl ────────────────────────────────────────────────

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
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
        // With FULL sync mode there is exactly one change containing the
        // entire document.  Use the *last* change to be safe if incremental
        // events are ever mixed in.
        let text = match params.content_changes.into_iter().last() {
            Some(c) => c.text,
            None => return,
        };
        self.documents.insert(uri.clone(), text.clone());
        // Debounce: bump generation, sleep, then only analyse if still current.
        let gen = self.bump_gen(&uri);
        tokio::time::sleep(tokio::time::Duration::from_millis(DEBOUNCE_MS)).await;
        if self.current_gen(&uri) != gen {
            return; // a newer edit superseded this one
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

// ── Entry point ──────────────────────────────────────────────────────────────

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
        edit_gen: DashMap::new(),
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
