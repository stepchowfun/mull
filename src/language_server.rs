use crate::{checker::check, error::Error};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::task::JoinHandle;
use tower_lsp_server::{
    Client, LanguageServer, LspService, Server,
    jsonrpc::Result,
    ls_types::{
        Diagnostic, DiagnosticSeverity, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
        DidOpenTextDocumentParams, DidSaveTextDocumentParams, InitializeParams, InitializeResult,
        Position, PositionEncodingKind, Range, ServerCapabilities, ServerInfo,
        TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions, Uri,
    },
};

// Wait briefly after edits so filesystem validation does not run on every keystroke.
const CHECK_DELAY: Duration = Duration::from_millis(250);

// This state associates the latest editor contents with a pending diagnostic update.
#[derive(Debug)]
struct OpenDocument {
    contents: String,
    version: i32,
    generation: u64,
    pending_check: Option<JoinHandle<()>>,
}

// This backend checks each open wiki and publishes its errors to the language client.
#[derive(Debug)]
struct Backend {
    client: Client,
    documents: Arc<Mutex<HashMap<Uri, OpenDocument>>>,
}

impl Backend {
    // Construct a backend connected to the editor-side language client.
    fn new(client: Client) -> Self {
        Self {
            client,
            documents: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    // Replace an editor snapshot and schedule diagnostics for its new generation.
    fn update_document(&self, uri: Uri, contents: String, version: i32, delay: Duration) {
        // Prepare the resources owned by the diagnostic task.
        let client = self.client.clone();
        let documents = Arc::clone(&self.documents);
        let diagnostic_uri = uri.clone();
        let diagnostic_contents = contents.clone();

        // Cancel the preceding task and assign a distinct generation to this snapshot.
        let mut open_documents = self
            .documents
            .lock()
            .expect("the open-document mutex should not be poisoned");
        let document = open_documents.entry(uri).or_insert_with(|| OpenDocument {
            contents: String::new(),
            version,
            generation: 0,
            pending_check: None,
        });
        if let Some(pending_check) = document.pending_check.take() {
            pending_check.abort();
        }
        document.contents = contents;
        document.version = version;
        document.generation = document
            .generation
            .checked_add(1)
            .expect("a document generation should fit in a u64");
        let generation = document.generation;

        // Check outside the asynchronous executor and publish only if the snapshot is still
        // current.
        document.pending_check = Some(tokio::spawn(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let check_uri = diagnostic_uri.clone();
            let fallback_contents = diagnostic_contents.clone();
            let diagnostics = tokio::task::spawn_blocking(move || {
                diagnostics_for_document(&check_uri, &diagnostic_contents)
            })
            .await
            .unwrap_or_else(|error| {
                vec![diagnostic(
                    &fallback_contents,
                    None,
                    format!("Mull failed to check the wiki: {error}."),
                )]
            });
            let is_current = documents
                .lock()
                .expect("the open-document mutex should not be poisoned")
                .get(&diagnostic_uri)
                .is_some_and(|document| {
                    document.version == version && document.generation == generation
                });
            if is_current {
                client
                    .publish_diagnostics(diagnostic_uri, diagnostics, Some(version))
                    .await;
            }
        }));
    }

    // Recheck the most recent snapshot immediately after it is saved.
    fn save_document(&self, uri: Uri, contents: Option<String>) {
        // Copy the snapshot before scheduling, without retaining the lock across that operation.
        let snapshot = {
            let mut open_documents = self
                .documents
                .lock()
                .expect("the open-document mutex should not be poisoned");
            open_documents.get_mut(&uri).map(|document| {
                if let Some(contents) = contents {
                    document.contents = contents;
                }
                (document.contents.clone(), document.version)
            })
        };
        if let Some((contents, version)) = snapshot {
            self.update_document(uri, contents, version, Duration::ZERO);
        }
    }
}

// Respond to protocol requests and notifications required for synchronized diagnostics.
#[allow(
    clippy::unused_async_trait_impl,
    reason = "Some methods mirror the asynchronous language-server interface without awaiting."
)]
impl LanguageServer for Backend {
    async fn initialize(&self, _params: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                position_encoding: Some(PositionEncodingKind::UTF16),
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        save: Some(true.into()),
                        ..TextDocumentSyncOptions::default()
                    },
                )),
                ..ServerCapabilities::default()
            },
            server_info: Some(ServerInfo {
                name: env!("CARGO_PKG_NAME").to_owned(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
            ..InitializeResult::default()
        })
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let document = params.text_document;
        self.update_document(
            document.uri,
            document.text,
            document.version,
            Duration::ZERO,
        );
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // Full synchronization places the complete latest snapshot in the final change.
        if let Some(change) = params.content_changes.into_iter().next_back() {
            self.update_document(
                params.text_document.uri,
                change.text,
                params.text_document.version,
                CHECK_DELAY,
            );
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        self.save_document(params.text_document.uri, params.text);
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        // Cancel outstanding work before asking the client to clear this document's diagnostics.
        let uri = params.text_document.uri;
        let document = self
            .documents
            .lock()
            .expect("the open-document mutex should not be poisoned")
            .remove(&uri);
        if let Some(pending_check) = document.and_then(|document| document.pending_check) {
            pending_check.abort();
        }
        self.client.publish_diagnostics(uri, Vec::new(), None).await;
    }
}

// Run Mull's complete check against the editor snapshot associated with a file URI.
fn diagnostics_for_document(uri: &Uri, source_contents: &str) -> Vec<Diagnostic> {
    // Reject URIs that cannot supply the filesystem context required by wiki validation.
    let Some(wiki_path) = uri.to_file_path() else {
        return vec![diagnostic(
            source_contents,
            None,
            "Mull could not determine the wiki's filesystem path.".to_owned(),
        )];
    };

    // Preserve independent Mull errors as independent editor diagnostics.
    check(&wiki_path, &wiki_path, source_contents).map_or_else(
        |errors| {
            errors
                .iter()
                .map(|error| diagnostic_from_error(source_contents, error))
                .collect()
        },
        |_wiki| Vec::new(),
    )
}

// Convert a structured Mull error into the representation expected by language clients.
fn diagnostic_from_error(source_contents: &str, error: &Error) -> Diagnostic {
    // Include an underlying reason without including terminal prefixes, paths, or source listings.
    let message = error.reason().map_or_else(
        || error.message().to_owned(),
        |reason| format!("{}\n\nReason: {reason}", error.message()),
    );
    diagnostic(source_contents, error.source_range(), message)
}

// Construct a Mull error diagnostic at a source range or at the start of the document.
fn diagnostic(
    source_contents: &str,
    source_range: Option<crate::error::SourceRange>,
    message: String,
) -> Diagnostic {
    let source_range = source_range.unwrap_or(crate::error::SourceRange { start: 0, end: 0 });
    Diagnostic {
        range: Range::new(
            position(source_contents, source_range.start),
            position(source_contents, source_range.end),
        ),
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some(env!("CARGO_PKG_NAME").to_owned()),
        message,
        ..Diagnostic::default()
    }
}

// Convert a UTF-8 byte offset into a zero-based LSP position measured in UTF-16 code units.
fn position(source_contents: &str, byte_offset: usize) -> Position {
    // Source ranges originate at character boundaries and cannot extend beyond the source.
    let byte_offset = byte_offset.min(source_contents.len());
    let prefix = source_contents
        .get(..byte_offset)
        .expect("source ranges should end on UTF-8 character boundaries");
    let line_start = prefix
        .rfind('\n')
        .map_or(0, |index| index + '\n'.len_utf8());
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count();
    let character = source_contents[line_start..byte_offset]
        .encode_utf16()
        .count();
    Position::new(
        u32::try_from(line).unwrap_or(u32::MAX),
        u32::try_from(character).unwrap_or(u32::MAX),
    )
}

// Serve language-server requests over standard input and output until the client disconnects.
pub async fn run() {
    // Keep ANSI terminal escapes out of protocol diagnostics.
    colored::control::set_override(false);

    // Connect the backend to the standard streams reserved for LSP messages.
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}

#[cfg(test)]
mod tests {
    use super::{diagnostic_from_error, position};
    use crate::{error::SourceRange, parser};
    use std::path::Path;
    use tower_lsp_server::ls_types::{DiagnosticSeverity, Position, Range};

    #[test]
    fn positions_use_utf16_code_units() {
        let source = "zero\n😀 café";

        assert_eq!(position(source, 0), Position::new(0, 0));
        assert_eq!(position(source, 5), Position::new(1, 0));
        assert_eq!(position(source, 9), Position::new(1, 2));
        assert_eq!(position(source, source.len()), Position::new(1, 7));
    }

    #[test]
    fn source_errors_become_precise_diagnostics() {
        let source = "# Home\n😀 ]";
        let error = parser::parse(Path::new("wiki.mull"), source)
            .unwrap_err()
            .into_iter()
            .next()
            .unwrap();
        let diagnostic = diagnostic_from_error(source, &error);

        assert_eq!(
            diagnostic.range,
            Range::new(Position::new(1, 3), Position::new(1, 4)),
        );
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diagnostic.source.as_deref(), Some("mull"));
        assert_eq!(
            diagnostic.message,
            "Unexpected closing link delimiter in node `Home`.",
        );
    }

    #[test]
    fn errors_without_ranges_point_to_document_start() {
        let error = crate::error::Error::new("Something went wrong.", None, None, None);
        let diagnostic = diagnostic_from_error("# Home\n", &error);

        assert_eq!(
            diagnostic.range,
            Range::new(Position::new(0, 0), Position::new(0, 0)),
        );
    }

    #[test]
    fn ranges_can_span_windows_line_endings() {
        let source = "first\r\nsecond";
        let error = crate::error::Error::new(
            "Something went wrong.",
            Some(Path::new("wiki.mull")),
            Some((source, SourceRange { start: 0, end: 9 })),
            None,
        );
        let diagnostic = diagnostic_from_error(source, &error);

        assert_eq!(
            diagnostic.range,
            Range::new(Position::new(0, 0), Position::new(1, 2)),
        );
    }
}
