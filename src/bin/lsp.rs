use dashmap::DashMap;
use lume::{
    error::{LumeError, Span},
    lexer::Lexer,
    parser,
    types::infer::{elaborate, BindingInfo},
};
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

struct Backend {
    client: Client,
    documents: DashMap<Url, String>,
    bindings: DashMap<Url, Vec<BindingInfo>>,
}

fn span_to_range(span: &Span) -> Range {
    let line = span.line.saturating_sub(1) as u32;
    let col = span.col.saturating_sub(1) as u32;
    Range {
        start: Position { line, character: col },
        end: Position { line, character: col + span.len as u32 },
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

/// Run the full pipeline on `src`, returning elaborated bindings and any
/// diagnostics.  Returns on the first error; a more advanced implementation
/// could attempt error recovery and continue.
fn analyse(src: &str) -> (Vec<BindingInfo>, Vec<Diagnostic>) {
    let tokens = match Lexer::new(src).tokenize() {
        Ok(t) => t,
        Err(e) => return (vec![], vec![error_to_diagnostic(LumeError::Lex(e))]),
    };
    let program = match parser::parse_program(&tokens) {
        Ok(p) => p,
        Err(e) => return (vec![], vec![error_to_diagnostic(LumeError::Parse(e))]),
    };
    match elaborate(&program) {
        Ok((bindings, _)) => (bindings, vec![]),
        Err(e) => (vec![], vec![error_to_diagnostic(LumeError::Type(e))]),
    }
}

/// Return the identifier word that spans the cursor position, or `None` if the
/// cursor is on whitespace or punctuation.
fn word_at(text: &str, line: u32, character: u32) -> Option<&str> {
    let line_text = text.lines().nth(line as usize)?;
    let col = character as usize;
    if col > line_text.len() {
        return None;
    }
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    // Walk left to the start of the word.
    let start = line_text[..col]
        .rfind(|c: char| !is_ident(c))
        .map(|i| i + 1)
        .unwrap_or(0);
    // Walk right to the end of the word.
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

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                hover_provider: Some(HoverProviderCapability::Simple(true)),
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
        // We requested FULL sync, so there is always exactly one entry containing
        // the complete new text.
        let text = params.content_changes.remove(0).text;
        self.refresh(&uri, &text).await;
        self.documents.insert(uri, text);
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.documents.remove(&uri);
        self.bindings.remove(&uri);
        // Clear any lingering diagnostics for the closed file.
        self.client.publish_diagnostics(uri, vec![], None).await;
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;

        let text = match self.documents.get(uri) {
            Some(entry) => entry.clone(),
            None => return Ok(None),
        };

        let word = match word_at(&text, pos.line, pos.character) {
            Some(w) => w.to_owned(),
            None => return Ok(None),
        };

        let scheme_str = self.bindings.get(uri).and_then(|bindings| {
            bindings
                .iter()
                .find(|b| b.name == word)
                .map(|b| b.scheme.to_string())
        });

        Ok(scheme_str.map(|s| Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: format!("```\n{word} : {s}\n```"),
            }),
            range: None,
        }))
    }
}

impl Backend {
    /// Re-analyse the document, update the binding cache, and push diagnostics.
    async fn refresh(&self, uri: &Url, text: &str) {
        let (bindings, diagnostics) = analyse(text);
        self.bindings.insert(uri.clone(), bindings);
        self.client
            .publish_diagnostics(uri.clone(), diagnostics, None)
            .await;
    }
}

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(|client| Backend {
        client,
        documents: DashMap::new(),
        bindings: DashMap::new(),
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
