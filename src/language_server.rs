use crate::{
    analyzer::validate_parsed,
    cancellation::{CancellationFlag, Outcome},
    error::{Error, Fix, SourceRange},
    format::{CodePath, CodeStr},
    line_index::LineIndex,
    parser,
    spelled_path::{DirectoryListings, SpelledPath, WikiDirectory, entry_identity},
    wiki::{
        ContentText, FILESYSTEM_LINK_PREFIX, FilesystemTarget, HOME_TITLE, Link, TITLE_MARKER,
        TITLE_PREFIX, TextNode, Wiki, unescaped_characters,
    },
    wiki_tree::{Visibility, visibility, wiki_tree_walker},
};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    fs,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, atomic::AtomicBool, atomic::Ordering},
    time::Duration,
};
use tokio::task::JoinHandle;
use tower_lsp_server::{
    Client, LanguageServer, LspService, Server,
    jsonrpc::{Error as JsonRpcError, Result},
    ls_types::{
        CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams,
        CodeActionProviderCapability, CodeActionResponse, Command, CompletionItem,
        CompletionItemKind, CompletionOptions, CompletionParams, CompletionResponse,
        CompletionTextEdit, DeleteFile, DeleteFileOptions, Diagnostic, DiagnosticSeverity,
        DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
        DidChangeWatchedFilesRegistrationOptions, DidCloseTextDocumentParams,
        DidOpenTextDocumentParams, DidSaveTextDocumentParams, DocumentChangeOperation,
        DocumentChanges, DocumentFormattingParams, DocumentHighlight, DocumentHighlightKind,
        DocumentHighlightParams, DocumentLink, DocumentLinkOptions, DocumentLinkParams,
        DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse, FileSystemWatcher,
        GlobPattern, GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents,
        HoverParams, HoverProviderCapability, InitializeParams, InitializeResult,
        InitializedParams, Location, LocationLink, MarkupContent, MarkupKind, MessageType, OneOf,
        OptionalVersionedTextDocumentIdentifier, Position, PositionEncodingKind,
        PrepareRenameResponse, Range, ReferenceParams, Registration, RenameFile, RenameOptions,
        RenameParams, ResourceOp, ResourceOperationKind, ServerCapabilities, ServerInfo,
        SymbolInformation, SymbolKind, TextDocumentEdit, TextDocumentPositionParams,
        TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions, TextEdit, Uri,
        WorkDoneProgressOptions, WorkspaceEdit,
    },
};

// Wait briefly after edits so filesystem validation doesn't run on every keystroke.
const CHECK_DELAY: Duration = Duration::from_millis(250);

// This extension command reveals a source range in a document. Keep this in sync with
// [group:reveal_range_command].
const REVEAL_RANGE_COMMAND: &str = "mull.revealRange";

// This extension command reveals a directory in the explorer. Keep this in sync with
// [group:reveal_in_explorer_command].
const REVEAL_IN_EXPLORER_COMMAND: &str = "mull.revealInExplorer";

// This editor command reopens suggestions so the children of a completed directory can be chosen.
const TRIGGER_SUGGEST_COMMAND: &str = "editor.action.triggerSuggest";

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

// This backend checks each open wiki and publishes its errors to the language client.
#[derive(Debug)]
struct Backend {
    client: Client,
    documents: Arc<Mutex<HashMap<Uri, OpenDocument>>>,
    supports_file_deletes: AtomicBool,
    supports_file_renames: AtomicBool,
    supports_hierarchical_document_symbols: AtomicBool,
    supports_watched_file_registration: AtomicBool,
}

impl Backend {
    // Construct a backend connected to the editor-side language client.
    fn new(client: Client) -> Self {
        Self {
            client,
            documents: Arc::new(Mutex::new(HashMap::new())),
            supports_file_deletes: AtomicBool::new(false),
            supports_file_renames: AtomicBool::new(false),
            supports_hierarchical_document_symbols: AtomicBool::new(false),
            supports_watched_file_registration: AtomicBool::new(false),
        }
    }

    // Replace an editor snapshot and schedule diagnostics for its new generation.
    fn store_and_check_document(&self, snapshot: Arc<Snapshot>, delay: Duration) {
        // Prepare the resources owned by the diagnostic task.
        let client = self.client.clone();
        let documents = Arc::clone(&self.documents);
        let diagnostic_snapshot = Arc::clone(&snapshot);
        let cancellation = CancellationFlag::default();
        let check_cancellation = cancellation.clone();

        // Cancel the preceding task and assign a distinct generation to this snapshot.
        let mut open_documents = self
            .documents
            .lock()
            .expect("The open-document mutex shouldn't be poisoned.");
        let document = open_documents
            .entry(snapshot.uri.clone())
            .or_insert_with(|| OpenDocument {
                snapshot: Arc::clone(&snapshot),
                generation: 0,
                pending_check: None,
            });
        if let Some(pending_check) = document.pending_check.take() {
            pending_check.cancel();
        }
        document.snapshot = snapshot;
        document.generation = document
            .generation
            .checked_add(1)
            .expect("A document generation should fit in a u64.");
        let generation = document.generation;

        // Check outside the asynchronous executor and publish only if the snapshot is still
        // current.
        document.pending_check = Some(PendingCheck {
            handle: tokio::spawn(async move {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                let check_snapshot = Arc::clone(&diagnostic_snapshot);

                // Publish nothing when a newer snapshot cancelled this check partway through.
                let Some(diagnostics) = tokio::task::spawn_blocking(move || {
                    diagnostics_for_document(&check_snapshot, &check_cancellation)
                })
                .await
                .unwrap_or_else(|error| {
                    Some(vec![diagnostic(
                        Range::default(),
                        format!("Mull was unable to check the wiki: {error}."),
                        None,
                    )])
                }) else {
                    return;
                };
                if documents
                    .lock()
                    .expect("The open-document mutex shouldn't be poisoned.")
                    .get(&diagnostic_snapshot.uri)
                    .is_some_and(|document| document.generation == generation)
                {
                    client
                        .publish_diagnostics(
                            diagnostic_snapshot.uri.clone(),
                            diagnostics,
                            Some(diagnostic_snapshot.version),
                        )
                        .await;
                }
            }),
            cancellation,
        });
    }

    // Recheck the most recent snapshot immediately after it's saved, adopting any different
    // contents the client included.
    fn recheck_saved_document(&self, uri: &Uri, contents: Option<String>) {
        if let Some(snapshot) = self.snapshot(uri) {
            let snapshot = match contents {
                Some(contents) if contents != snapshot.contents => {
                    Arc::new(Snapshot::new(uri.clone(), contents, snapshot.version))
                }
                Some(_) | None => snapshot,
            };
            self.store_and_check_document(snapshot, Duration::ZERO);
        }
    }

    // Recheck open documents after filesystem changes, which their filesystem links may reflect.
    // Documents whose own files changed are skipped, since editor synchronization covers them.
    fn recheck_open_documents(&self, changed_uris: &[&Uri]) {
        // Collect the snapshots before scheduling, without retaining the lock across that
        // operation.
        let snapshots = self
            .documents
            .lock()
            .expect("The open-document mutex shouldn't be poisoned.")
            .iter()
            .filter(|(uri, _document)| !changed_uris.contains(uri))
            .map(|(_uri, document)| Arc::clone(&document.snapshot))
            .collect::<Vec<_>>();

        // Debounce the checks, since a single operation can change many files in quick succession.
        for snapshot in snapshots {
            self.store_and_check_document(snapshot, CHECK_DELAY);
        }
    }

    // Share the latest synchronized snapshot of an open document with a language feature request.
    fn snapshot(&self, uri: &Uri) -> Option<Arc<Snapshot>> {
        self.documents
            .lock()
            .expect("The open-document mutex shouldn't be poisoned.")
            .get(uri)
            .map(|document| Arc::clone(&document.snapshot))
    }
}

// Respond to protocol requests and notifications for document synchronization, diagnostics, and
// language features.
#[allow(
    clippy::unused_async_trait_impl,
    reason = "Some methods mirror the asynchronous language-server interface without awaiting."
)]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // Remember whether document symbols may carry separate full and selection ranges.
        self.supports_hierarchical_document_symbols.store(
            params
                .capabilities
                .text_document
                .as_ref()
                .and_then(|capabilities| capabilities.document_symbol.as_ref())
                .and_then(|capabilities| capabilities.hierarchical_document_symbol_support)
                .unwrap_or(false),
            Ordering::Relaxed,
        );

        // Remember which file operations the client can perform within a versioned workspace edit.
        let resource_operations = params
            .capabilities
            .workspace
            .as_ref()
            .and_then(|capabilities| capabilities.workspace_edit.as_ref())
            .filter(|capabilities| capabilities.document_changes == Some(true))
            .and_then(|capabilities| capabilities.resource_operations.as_deref())
            .unwrap_or_default();
        self.supports_file_renames.store(
            resource_operations.contains(&ResourceOperationKind::Rename),
            Ordering::Relaxed,
        );
        self.supports_file_deletes.store(
            resource_operations.contains(&ResourceOperationKind::Delete),
            Ordering::Relaxed,
        );

        // Remember whether the client can watch files on the server's behalf.
        self.supports_watched_file_registration.store(
            params
                .capabilities
                .workspace
                .as_ref()
                .and_then(|capabilities| capabilities.did_change_watched_files.as_ref())
                .and_then(|capabilities| capabilities.dynamic_registration)
                .unwrap_or(false),
            Ordering::Relaxed,
        );

        // Advertise the language features implemented by this server.
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec!["[".to_owned(), "/".to_owned()]),
                    ..CompletionOptions::default()
                }),
                definition_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                position_encoding: Some(PositionEncodingKind::UTF16),
                references_provider: Some(OneOf::Left(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                })),
                document_formatting_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                document_link_provider: Some(DocumentLinkOptions {
                    resolve_provider: None,
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                }),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
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

    async fn initialized(&self, _params: InitializedParams) {
        // Confirm that the server completed its initialization handshake.
        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "Mull {} language server initialized.",
                    env!("CARGO_PKG_VERSION"),
                ),
            )
            .await;

        // Ask the client to report filesystem changes, which can invalidate filesystem links.
        if self
            .supports_watched_file_registration
            .load(Ordering::Relaxed)
            && let Err(error) = self
                .client
                .register_capability(vec![Registration {
                    id: "mull-watched-files".to_owned(),
                    method: "workspace/didChangeWatchedFiles".to_owned(),
                    register_options: serde_json::to_value(
                        DidChangeWatchedFilesRegistrationOptions {
                            watchers: vec![FileSystemWatcher {
                                glob_pattern: GlobPattern::String("**/*".to_owned()),
                                kind: None,
                            }],
                        },
                    )
                    .ok(),
                }])
                .await
        {
            self.client
                .log_message(
                    MessageType::WARNING,
                    format!("Mull was unable to watch for filesystem changes: {error}."),
                )
                .await;
        }
    }

    async fn shutdown(&self) -> Result<()> {
        // Confirm that the server began its shutdown handshake.
        self.client
            .log_message(MessageType::INFO, "Mull language server shutting down.")
            .await;
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        // Track the newly opened document and check it without waiting for further edits.
        self.store_and_check_document(
            Arc::new(Snapshot::new(
                params.text_document.uri,
                params.text_document.text,
                params.text_document.version,
            )),
            Duration::ZERO,
        );
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // Full synchronization places the complete latest snapshot in the final change.
        if let Some(change) = params.content_changes.into_iter().next_back() {
            self.store_and_check_document(
                Arc::new(Snapshot::new(
                    params.text_document.uri,
                    change.text,
                    params.text_document.version,
                )),
                CHECK_DELAY,
            );
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        // Recheck the saved document immediately, adopting any contents the client included.
        self.recheck_saved_document(&params.text_document.uri, params.text);
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        // Recheck open documents against the changed filesystem.
        self.recheck_open_documents(
            &params
                .changes
                .iter()
                .map(|change| &change.uri)
                .collect::<Vec<_>>(),
        );
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        // Complete links against the latest synchronized editor snapshot.
        let Some(snapshot) = self.snapshot(&params.text_document_position.text_document.uri) else {
            return Ok(None);
        };
        Ok(
            completion_for_document(&snapshot, params.text_document_position.position)
                .map(CompletionResponse::Array),
        )
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        // Resolve the title or text link against the latest synchronized editor snapshot.
        let Some(snapshot) = self.snapshot(&params.text_document_position_params.text_document.uri)
        else {
            return Ok(None);
        };
        Ok(goto_definition_for_document(
            &snapshot,
            params.text_document_position_params.position,
        ))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        // Preview the text-link target from the latest synchronized editor snapshot.
        let Some(snapshot) = self.snapshot(&params.text_document_position_params.text_document.uri)
        else {
            return Ok(None);
        };
        Ok(hover_for_document(
            &snapshot,
            params.text_document_position_params.position,
        ))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        // Find references to the node under the cursor in the latest synchronized snapshot.
        let Some(snapshot) = self.snapshot(&params.text_document_position.text_document.uri) else {
            return Ok(None);
        };
        Ok(references_for_document(
            &snapshot,
            params.text_document_position.position,
            params.context.include_declaration,
        ))
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        // Highlight the wiki occurrences related to the item under the cursor.
        let Some(snapshot) = self.snapshot(&params.text_document_position_params.text_document.uri)
        else {
            return Ok(None);
        };
        Ok(document_highlight_for_document(
            &snapshot,
            params.text_document_position_params.position,
        ))
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        // Identify the occurrence that the editor should select for rename, or explain why the
        // filesystem node a link targets can't be renamed before the user enters a new name.
        let Some(snapshot) = self.snapshot(&params.text_document.uri) else {
            return Ok(None);
        };
        prepare_rename_for_document(
            &snapshot,
            params.position,
            self.supports_file_renames.load(Ordering::Relaxed),
        )
        .map_err(JsonRpcError::invalid_params)
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        // Rename against the latest snapshot, whose version guards the edit against later changes.
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let Some(snapshot) = self.snapshot(uri) else {
            return Ok(None);
        };

        // Rename the node at the cursor with whichever file operations the client supports.
        rename_for_document(
            &snapshot,
            position,
            &params.new_name,
            FileOperationSupport {
                rename: self.supports_file_renames.load(Ordering::Relaxed),
                delete: self.supports_file_deletes.load(Ordering::Relaxed),
            },
        )
        .map_err(JsonRpcError::invalid_params)
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        // Render the latest synchronized editor snapshot, leaving unparsable contents unchanged.
        let Some(snapshot) = self.snapshot(&params.text_document.uri) else {
            return Ok(None);
        };
        Ok(formatting_for_document(&snapshot))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        // Describe the nodes in the latest synchronized editor snapshot.
        let Some(snapshot) = self.snapshot(&params.text_document.uri) else {
            return Ok(None);
        };
        Ok(Some(document_symbol_for_document(
            &snapshot,
            self.supports_hierarchical_document_symbols
                .load(Ordering::Relaxed),
        )))
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        // Offer fixes for the latest synchronized editor snapshot.
        let Some(snapshot) = self.snapshot(&params.text_document.uri) else {
            return Ok(None);
        };
        Ok(code_action_for_document(
            &snapshot,
            &params.context.diagnostics,
        ))
    }

    async fn document_link(&self, params: DocumentLinkParams) -> Result<Option<Vec<DocumentLink>>> {
        // Make the filesystem links in the latest synchronized editor snapshot clickable.
        let Some(snapshot) = self.snapshot(&params.text_document.uri) else {
            return Ok(None);
        };
        Ok(document_link_for_document(&snapshot))
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        // Cancel outstanding work before asking the client to clear this document's diagnostics.
        let document = self
            .documents
            .lock()
            .expect("The open-document mutex shouldn't be poisoned.")
            .remove(&params.text_document.uri);
        if let Some(pending_check) = document.and_then(|document| document.pending_check) {
            pending_check.cancel();
        }
        self.client
            .publish_diagnostics(params.text_document.uri, Vec::new(), None)
            .await;
    }
}

// This state associates the latest editor snapshot with a pending diagnostic update. The server
// assigns the generation, which increases every time a snapshot is stored, including rechecks of
// unchanged contents after a save, so it identifies which snapshot stale work was computed from.
#[derive(Debug)]
struct OpenDocument {
    snapshot: Arc<Snapshot>,
    generation: u64,
    pending_check: Option<PendingCheck>,
}

// This is an immutable version of a document's contents, shared by the requests and checks made
// against it so each derived structure is computed at most once, when it's first needed. The
// client assigns the version, which changes only when the contents do and is reported back with
// diagnostics.
#[derive(Debug)]
struct Snapshot {
    uri: Uri,
    path: Option<PathBuf>,
    contents: String,
    version: i32,
    line_index: OnceLock<LineIndex>,
    parsed: OnceLock<(Wiki, Vec<Error>)>,
}

impl Snapshot {
    // Capture a version of a document without analyzing it yet.
    fn new(uri: Uri, contents: String, version: i32) -> Self {
        Self {
            path: local_path(&uri).map(Cow::into_owned),
            uri,
            contents,
            version,
            line_index: OnceLock::new(),
            parsed: OnceLock::new(),
        }
    }

    // Index the lines of the contents for position conversions.
    fn line_index(&self) -> &LineIndex {
        self.line_index
            .get_or_init(|| LineIndex::new(&self.contents))
    }

    // Parse the contents, recovering from syntax errors so the wiki can still be validated and
    // navigated.
    fn parsed(&self) -> &(Wiki, Vec<Error>) {
        self.parsed
            .get_or_init(|| parser::parse_with_recovery(self.path.as_deref(), &self.contents))
    }

    // Retrieve the wiki parsed with recovery from syntax errors.
    fn wiki(&self) -> &Wiki {
        &self.parsed().0
    }
}

// This pairs a scheduled diagnostic task with the flag which stops its filesystem work.
#[derive(Debug)]
struct PendingCheck {
    handle: JoinHandle<()>,
    cancellation: CancellationFlag,
}

impl PendingCheck {
    // Stop the check whether or not it has started. Aborting the task stops it if it hasn't
    // started checking, and setting its flag stops it if it has already started.
    fn cancel(self) {
        self.cancellation.cancel();
        self.handle.abort();
    }
}

// Analyze an editor snapshot without checking its formatting.
fn diagnostics_for_document(
    snapshot: &Snapshot,
    cancellation: &CancellationFlag,
) -> Option<Vec<Diagnostic>> {
    // Report nothing for a cancelled check, whose errors may cover only part of the wiki.
    let (wiki, syntax_errors) = snapshot.parsed();
    let errors = match validate_parsed(
        wiki,
        syntax_errors.clone(),
        snapshot.path.as_deref(),
        &snapshot.contents,
        cancellation,
    ) {
        Outcome::Completed(errors) => errors,
        Outcome::Cancelled => return None,
    };

    // Preserve independent Mull errors as independent editor diagnostics.
    let line_index = snapshot.line_index();
    Some(
        errors
            .iter()
            .map(|error| diagnostic_from_error(&snapshot.contents, line_index, error))
            .collect(),
    )
}

// Convert a structured Mull error into the representation expected by language clients.
fn diagnostic_from_error(
    source_contents: &str,
    line_index: &LineIndex,
    error: &Error,
) -> Diagnostic {
    // Include an underlying reason without including terminal prefixes, paths, or source listings.
    // An error without a source range is reported at the start of the document.
    diagnostic(
        error
            .source_range()
            .map_or_else(Range::default, |source_range| {
                line_index.range(source_contents, source_range)
            }),
        error.reason().map_or_else(
            || error.message().to_owned(),
            |reason| format!("{}\n\nReason: {reason}", error.message()),
        ),
        error.fix(),
    )
}

// Construct a Mull error diagnostic at an editor range, with any fix that resolves it.
fn diagnostic(range: Range, message: String, fix: Option<&Fix>) -> Diagnostic {
    Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some(env!("CARGO_PKG_NAME").to_owned()),
        message,
        // Carry any fix along, since the editor sends it back with a code action request.
        data: fix.map(|fix| serde_json::to_value(fix).expect("A fix should serialize to JSON.")),
        ..Diagnostic::default()
    }
}

// Complete the link target at an editor position with node titles or filesystem paths.
fn completion_for_document(snapshot: &Snapshot, cursor: Position) -> Option<Vec<CompletionItem>> {
    // Complete a filesystem link from the directory containing the wiki.
    let line_index = snapshot.line_index();
    let cursor_offset = line_index.byte_offset(&snapshot.contents, cursor)?;
    if let Some(context) = filesystem_link_context(&snapshot.contents, cursor_offset) {
        return Some(filesystem_link_completions(
            snapshot.path.as_deref()?,
            &snapshot.contents,
            line_index,
            &context,
        ));
    }

    // Complete a text link with the titles of the nodes in the wiki.
    let replacement_source_range = text_link_context(snapshot, cursor_offset)?;
    Some(text_link_completions(
        snapshot.wiki(),
        &snapshot.contents,
        line_index,
        replacement_source_range,
    ))
}

// This describes the path of a filesystem link being authored at the cursor.
struct FilesystemLinkContext {
    directory: PathBuf, // The normalized directory named by the typed path through its last `/`
    segment_start: usize, // The start of the path component after that `/`
    cursor: usize,
    closing_delimiter: Option<usize>,
}

// Identify a filesystem link whose path contains the cursor, even if the link is unfinished.
fn filesystem_link_context(source_contents: &str, cursor: usize) -> Option<FilesystemLinkContext> {
    // Confine the search to the cursor's line, since links can't contain line breaks.
    let line_start = source_contents[..cursor]
        .rfind('\n')
        .map_or(0, |index| index + '\n'.len_utf8());
    let line_end = source_contents[cursor..]
        .find('\n')
        .map_or(source_contents.len(), |index| cursor + index);
    let line = &source_contents[line_start..line_end];
    let line = line.strip_suffix('\r').unwrap_or(line);

    // Ignore titles, which can't contain links.
    if line == TITLE_MARKER || line.starts_with(TITLE_PREFIX) {
        return None;
    }

    // Find the open link before the cursor and any closing delimiter after it, skipping escaped
    // delimiters as the parser does.
    let mut opening_delimiter = None;
    let mut closing_delimiter = None;
    for (index, character) in unescaped_characters(line) {
        let offset = line_start + index;
        if offset < cursor {
            match character {
                '[' => opening_delimiter = Some(offset),
                ']' => opening_delimiter = None,
                _ => {}
            }
        } else if matches!(character, '[' | ']') {
            closing_delimiter = (character == ']').then_some(offset);
            break;
        }
    }

    // Require the filesystem-link prefix after any leading whitespace, since the parser trims
    // targets. The prefix is the start of the typed path.
    let typed_path = source_contents[opening_delimiter? + '['.len_utf8()..cursor].trim_start();
    if !typed_path.starts_with(FILESYSTEM_LINK_PREFIX) {
        return None;
    }

    // Resolve the typed directory, declining paths which escape the wiki tree
    // [ref:filesystem_path_components].
    let typed_directory = &typed_path[..typed_path.rfind('/').map_or(0, |index| index + 1)];
    let mut directory = PathBuf::new();
    for component in
        Path::new(&ContentText::from_source(typed_directory.trim_start_matches('/')).unescape())
            .components()
    {
        match component {
            Component::Normal(component) => directory.push(component),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }

    Some(FilesystemLinkContext {
        directory,
        segment_start: cursor - (typed_path.len() - typed_directory.len()),
        cursor,
        closing_delimiter,
    })
}

// Complete the next component of a filesystem link's path from the directory its prefix names.
fn filesystem_link_completions(
    wiki_path: &Path,
    source_contents: &str,
    line_index: &LineIndex,
    context: &FilesystemLinkContext,
) -> Vec<CompletionItem> {
    // Derive every filesystem path from the wiki's containing directory, as validation does.
    let Ok(wiki_directory) = WikiDirectory::new(wiki_path) else {
        return Vec::new();
    };

    // Descend only along the typed directory so large subtrees are read only once they're named,
    // and exclude the wiki itself.
    let mut walker_builder = wiki_tree_walker(wiki_directory.path());
    walker_builder
        .max_depth(Some(context.directory.components().count() + 1))
        .filter_entry({
            let wiki_directory = wiki_directory.clone();
            let directory = context.directory.clone();
            move |entry| {
                let path = wiki_directory.entry_path(entry);
                wiki_directory.wiki_path() != Some(&path)
                    && (directory.starts_with(path.as_path())
                        || path.as_path().parent() == Some(directory.as_path()))
            }
        });

    // Offer each visible child of the typed directory, skipping the ancestors walked to reach it.
    let mut completions = Vec::new();
    for entry in walker_builder.build().flatten() {
        let path = wiki_directory.entry_path(&entry);
        let file_type = entry
            .file_type()
            .expect("Only standard input lacks a file type.");
        let Some(name) = entry.file_name().to_str() else {
            continue;
        };
        if path.as_path().parent() != Some(context.directory.as_path()) {
            continue;
        }

        // Omit a directory which a link couldn't name because it contains no files.
        if file_type.is_dir()
            && !matches!(
                visibility(&wiki_directory, &path, &CancellationFlag::default()).assume_completed(),
                Visibility::Visible,
            )
        {
            continue;
        }

        // Leave a directory's link open for its children, and close a file's link.
        let escaped_name = ContentText::escape(name).into_string();
        let (label, kind, new_text, replacement_end, command) = if file_type.is_dir() {
            (
                format!("{name}/"),
                CompletionItemKind::FOLDER,
                format!("{escaped_name}/"),
                context.closing_delimiter.unwrap_or(context.cursor),
                Some(Command {
                    title: "Suggest".to_owned(),
                    command: TRIGGER_SUGGEST_COMMAND.to_owned(),
                    arguments: None,
                }),
            )
        } else {
            (
                name.to_owned(),
                CompletionItemKind::FILE,
                format!("{escaped_name}]"),
                context
                    .closing_delimiter
                    .map_or(context.cursor, |offset| offset + ']'.len_utf8()),
                None,
            )
        };

        // Replace the typed component so the editor filters candidates against it.
        let replacement_range = line_index.range(
            source_contents,
            SourceRange {
                start: context.segment_start,
                end: replacement_end,
            },
        );
        completions.push(CompletionItem {
            label,
            kind: Some(kind),
            filter_text: Some(escaped_name),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit::new(
                replacement_range,
                new_text,
            ))),
            command,
            ..CompletionItem::default()
        });
    }

    // Present entries deterministically.
    completions.sort_by(|a, b| a.label.cmp(&b.label));
    completions
}

// Parse enough of an active text link to identify the source range a completion should replace.
fn text_link_context(snapshot: &Snapshot, byte_offset: usize) -> Option<SourceRange> {
    // Prefer the unchanged source when the active link is already closed. Recover from syntax
    // errors so errors elsewhere in the wiki don't prevent completion.
    if let Some(Link::Text { source_range, .. }) = link_at(snapshot.wiki(), byte_offset) {
        return Some(*source_range);
    }

    // Close a link at the cursor temporarily so completion works while it's being authored.
    let mut completed_source = snapshot.contents.clone();
    completed_source.insert(byte_offset, ']');
    let (wiki, _) = parser::parse_with_recovery(snapshot.path.as_deref(), &completed_source);
    let Some(Link::Text { source_range, .. }) = link_at(&wiki, byte_offset) else {
        return None;
    };

    // Map the link's end back into the original source, which lacks the temporary delimiter.
    Some(SourceRange {
        start: source_range.start,
        end: source_range.end - ']'.len_utf8(),
    })
}

// Complete a text link with every node title, replacing the link at a source range.
fn text_link_completions(
    wiki: &Wiki,
    source_contents: &str,
    line_index: &LineIndex,
    replacement_source_range: SourceRange,
) -> Vec<CompletionItem> {
    // Present node titles deterministically and replace the whole link, including its delimiters,
    // so the cursor ends up after the closing `]`.
    let replacement_range = line_index.range(source_contents, replacement_source_range);
    let mut titles = wiki.text_nodes.keys().collect::<Vec<_>>();
    titles.sort();
    titles
        .into_iter()
        .map(|title| {
            let escaped_title = ContentText::escape(title).into_string();
            CompletionItem {
                label: title.clone(),
                kind: Some(CompletionItemKind::REFERENCE),
                filter_text: Some(format!("[{escaped_title}")),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit::new(
                    replacement_range,
                    format!("[{escaped_title}]"),
                ))),
                ..CompletionItem::default()
            }
        })
        .collect()
}

// Locate the node declared or linked at an editor position.
fn goto_definition_for_document(
    snapshot: &Snapshot,
    cursor: Position,
) -> Option<GotoDefinitionResponse> {
    // Parse only the wiki syntax because navigation doesn't require filesystem validation, and
    // recover from syntax errors so navigation keeps working while they're fixed.
    let wiki = snapshot.wiki();
    let line_index = snapshot.line_index();
    let (node, origin_source_range) = node_at(
        wiki,
        &snapshot.contents,
        line_index.byte_offset(&snapshot.contents, cursor)?,
        LinkExtent::Whole,
    )?;

    // Identify the complete source link or title and the destination node while selecting its title
    // on arrival.
    Some(GotoDefinitionResponse::Link(vec![LocationLink {
        origin_selection_range: Some(line_index.range(&snapshot.contents, origin_source_range)),
        target_uri: snapshot.uri.clone(),
        target_range: line_index.range(&snapshot.contents, node.source_range),
        target_selection_range: line_index.range(&snapshot.contents, node.title_source_range),
    }]))
}

// Preview the destination of a text link at an editor position.
fn hover_for_document(snapshot: &Snapshot, cursor: Position) -> Option<Hover> {
    // Parse only the wiki syntax because hovering doesn't require filesystem validation, and
    // recover from syntax errors so previews keep working while they're fixed.
    let wiki = snapshot.wiki();
    let line_index = snapshot.line_index();
    let (node, source_range) = node_at(
        wiki,
        &snapshot.contents,
        line_index.byte_offset(&snapshot.contents, cursor)?,
        LinkExtent::Whole,
    )?;

    // Render the node as Markdown, linking its resolvable text links to the nodes they name and,
    // in a saved wiki, its filesystem links to their targets.
    let wiki_directory = snapshot
        .path
        .as_deref()
        .and_then(|wiki_path| WikiDirectory::new(wiki_path).ok());
    let mut listings = DirectoryListings::new();
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: node
                .to_markdown(|link| match link {
                    Link::Text { title, .. } => Some(reveal_range_command_url(
                        &snapshot.uri,
                        line_index.range(
                            &snapshot.contents,
                            wiki.text_nodes.get(title)?.title_source_range,
                        ),
                    )),
                    Link::Filesystem { target, .. } => Some(
                        filesystem_link_target(wiki_directory.as_ref()?, target, &mut listings)?
                            .as_str()
                            .to_owned(),
                    ),
                })
                .into_string(),
        }),
        range: Some(line_index.range(&snapshot.contents, source_range)),
    })
}

// Encode an editor navigation command as a Markdown-safe URI.
fn reveal_range_command_url(uri: &Uri, range: Range) -> String {
    // Pass the document URI and UTF-16 destination range as positional command arguments.
    format!(
        "command:{REVEAL_RANGE_COMMAND}?{}",
        utf8_percent_encode(
            &serde_json::to_string(&(
                uri.as_str(),
                range.start.line,
                range.start.character,
                range.end.line,
                range.end.character,
            ))
            .expect("A string and numbers should serialize to JSON."),
            NON_ALPHANUMERIC,
        ),
    )
}

// Locate every text link to the node at an editor position.
fn references_for_document(
    snapshot: &Snapshot,
    cursor: Position,
    include_declaration: bool,
) -> Option<Vec<Location>> {
    // Parse only the wiki syntax because finding references doesn't require validation, and recover
    // from syntax errors so references can be found while they're fixed.
    let wiki = snapshot.wiki();
    let line_index = snapshot.line_index();
    let (node, _source_range) = node_at(
        wiki,
        &snapshot.contents,
        line_index.byte_offset(&snapshot.contents, cursor)?,
        LinkExtent::Whole,
    )?;

    // Include the declaration only when requested, then restore source order.
    let mut source_ranges = text_link_source_ranges(wiki, &node.title);
    if include_declaration {
        source_ranges.push(node.title_source_range);
    }
    source_ranges.sort_by_key(|source_range| (source_range.start, source_range.end));

    // Return every occurrence in source order within the current wiki.
    Some(
        source_ranges
            .into_iter()
            .map(|source_range| {
                Location::new(
                    snapshot.uri.clone(),
                    line_index.range(&snapshot.contents, source_range),
                )
            })
            .collect(),
    )
}

// Collect every complete text-link range that resolves to a title.
fn text_link_source_ranges(wiki: &Wiki, title: &str) -> Vec<SourceRange> {
    // Links live on nodes in an unordered map, so sort their ranges into source order.
    let mut source_ranges = wiki
        .text_nodes
        .values()
        .flat_map(|node| &node.links)
        .filter_map(|link| match link {
            Link::Text {
                title: link_title,
                source_range,
            } if link_title == title => Some(*source_range),
            Link::Text { .. } | Link::Filesystem { .. } => None,
        })
        .collect::<Vec<_>>();
    source_ranges.sort_by_key(|source_range| (source_range.start, source_range.end));
    source_ranges
}

// Highlight related node or filesystem-link occurrences at an editor position.
fn document_highlight_for_document(
    snapshot: &Snapshot,
    cursor: Position,
) -> Option<Vec<DocumentHighlight>> {
    // Parse only the wiki syntax because document highlights don't require validation, and recover
    // from syntax errors so highlights keep working while they're fixed.
    let wiki = snapshot.wiki();
    let line_index = snapshot.line_index();
    let byte_offset = line_index.byte_offset(&snapshot.contents, cursor)?;

    // Distinguish a text-node declaration from its references.
    let mut highlights = if let Some((node, _source_range)) =
        node_at(wiki, &snapshot.contents, byte_offset, LinkExtent::Whole)
    {
        let mut highlights = text_link_source_ranges(wiki, &node.title)
            .into_iter()
            .map(|source_range| (source_range, DocumentHighlightKind::READ))
            .collect::<Vec<_>>();
        highlights.push((node.title_source_range, DocumentHighlightKind::WRITE));
        highlights
    } else {
        // Filesystem links have no declaration in the wiki, so every matching link is a reference.
        // A text link reaches this branch only when its target doesn't exist, so it has nothing
        // to highlight.
        let Some(Link::Filesystem { target, .. }) = link_at(wiki, byte_offset) else {
            return None;
        };
        filesystem_link_source_ranges(wiki, target)
            .into_iter()
            .map(|source_range| (source_range, DocumentHighlightKind::READ))
            .collect()
    };

    // Return every matching source occurrence in wiki order.
    highlights.sort_by_key(|(source_range, _kind)| (source_range.start, source_range.end));
    Some(
        highlights
            .into_iter()
            .map(|(source_range, kind)| DocumentHighlight {
                range: line_index.range(&snapshot.contents, source_range),
                kind: Some(kind),
            })
            .collect(),
    )
}

// Collect every complete filesystem-link range with the same target.
fn filesystem_link_source_ranges(wiki: &Wiki, target: &FilesystemTarget) -> Vec<SourceRange> {
    // Match logical paths without resolving symlinks, just as filesystem validation does.
    wiki.text_nodes
        .values()
        .flat_map(|node| &node.links)
        .filter_map(|link| match link {
            Link::Filesystem {
                target: link_target,
                source_range,
            } if link_target == target => Some(*source_range),
            Link::Text { .. } | Link::Filesystem { .. } => None,
        })
        .collect()
}

// Identify the source occurrence that should be selected before renaming a node, file, or
// directory, or explain why the filesystem node a link targets can't be renamed.
fn prepare_rename_for_document(
    snapshot: &Snapshot,
    cursor: Position,
    supports_file_renames: bool,
) -> std::result::Result<Option<PrepareRenameResponse>, String> {
    // Resolve the cursor in an editor snapshot, which need not pass semantic validation, recovering
    // from syntax errors [ref:rename_despite_syntax_errors].
    let wiki = snapshot.wiki();
    let line_index = snapshot.line_index();
    let Some(cursor_offset) = line_index.byte_offset(&snapshot.contents, cursor) else {
        return Ok(None);
    };

    // Select only the path of a filesystem link between its leading `/` and any trailing `/`, and
    // seed the rename prompt with its decoded text as written. The rename writes both slashes
    // itself, so the new name needs neither.
    if let Some(filesystem_node) =
        renamable_filesystem_node_at(snapshot, cursor_offset, supports_file_renames)?
    {
        let path_source_range = filesystem_node.path_source_range;
        let path_source = &snapshot.contents[path_source_range.start..path_source_range.end];
        let start_trimmed = path_source.trim_start_matches('/');
        let trimmed = start_trimmed.trim_end_matches('/');
        let start = path_source_range.end - start_trimmed.len();
        return Ok(Some(PrepareRenameResponse::RangeWithPlaceholder {
            range: line_index.range(
                &snapshot.contents,
                SourceRange {
                    start,
                    end: start + trimmed.len(),
                },
            ),
            placeholder: ContentText::from_source(trimmed).unescape(),
        }));
    }

    // Otherwise, resolve either a title declaration or text link.
    let Some((node, source_range)) =
        node_at(wiki, &snapshot.contents, cursor_offset, LinkExtent::Target)
    else {
        return Ok(None);
    };

    // Select only the title text and seed the rename prompt with its decoded value.
    Ok(Some(PrepareRenameResponse::RangeWithPlaceholder {
        range: line_index.range(&snapshot.contents, source_range),
        placeholder: node.title.clone(),
    }))
}

// This records which file operations the client can perform within a versioned workspace edit.
#[derive(Clone, Copy)]
struct FileOperationSupport {
    rename: bool,
    delete: bool,
}

// Rename the filesystem node or text node at an editor position, along with every link to it.
fn rename_for_document(
    snapshot: &Snapshot,
    cursor: Position,
    new_name: &str,
    file_operation_support: FileOperationSupport,
) -> std::result::Result<Option<WorkspaceEdit>, String> {
    // Resolve the cursor in an editor snapshot, which need not pass semantic validation, recovering
    // from syntax errors. A malformed link to the renamed node isn't updated, but it's reported as
    // a broken link once the syntax errors are fixed, since the old name no longer exists
    // [tag:rename_despite_syntax_errors].
    let Some(cursor_offset) = snapshot
        .line_index()
        .byte_offset(&snapshot.contents, cursor)
    else {
        return Ok(None);
    };

    // Try the filesystem node a link targets, and otherwise fall back to a text node.
    match rename_filesystem_node_for_document(
        snapshot,
        cursor_offset,
        new_name,
        file_operation_support,
    ) {
        Ok(None) => rename_text_node_for_document(snapshot, cursor_offset, new_name),
        result => result,
    }
}

// Rename one text node and every text link that targets it.
fn rename_text_node_for_document(
    snapshot: &Snapshot,
    cursor_offset: usize,
    new_name: &str,
) -> std::result::Result<Option<WorkspaceEdit>, String> {
    // Resolve the text node declared or linked at the cursor.
    let Some((node, _source_range)) = node_at(
        snapshot.wiki(),
        &snapshot.contents,
        cursor_offset,
        LinkExtent::Target,
    ) else {
        return Ok(None);
    };

    // Normalize surrounding whitespace, then reject titles that the parser wouldn't accept: those
    // that span multiple lines, are empty, start with a filesystem-link prefix, or already exist.
    if new_name
        .chars()
        .any(|character| matches!(character, '\r' | '\n'))
    {
        return Err("A node title can't contain a line break.".to_owned());
    }
    let new_title = new_name.trim();
    if new_title.is_empty() {
        return Err("A node title can't be empty.".to_owned());
    }
    if new_title.starts_with(FILESYSTEM_LINK_PREFIX) {
        return Err(format!(
            "A node title can't start with {}.",
            FILESYSTEM_LINK_PREFIX.code_str(),
        ));
    }
    if new_title != node.title && snapshot.wiki().text_nodes.contains_key(new_title) {
        return Err(format!("Node {} already exists.", new_title.code_str()));
    }

    // Replace the declaration literally, since a heading isn't content, and escape the title inside
    // every matching text link.
    let mut edits = vec![(node.title_source_range, new_title.to_owned())];
    let escaped_title = ContentText::escape(new_title).into_string();
    for link in snapshot
        .wiki()
        .text_nodes
        .values()
        .flat_map(|node| &node.links)
    {
        if let Link::Text {
            title,
            source_range,
        } = link
            && title == &node.title
        {
            edits.push((
                text_link_target_source_range(&snapshot.contents, *source_range),
                escaped_title.clone(),
            ));
        }
    }
    edits.sort_by_key(|(source_range, _new_text)| (source_range.start, source_range.end));

    // Return one non-overlapping edit for each occurrence in the current document.
    Ok(Some(WorkspaceEdit {
        changes: Some(HashMap::from([(
            snapshot.uri.clone(),
            edits
                .into_iter()
                .map(|(source_range, new_text)| {
                    TextEdit::new(
                        snapshot
                            .line_index()
                            .range(&snapshot.contents, source_range),
                        new_text,
                    )
                })
                .collect(),
        )])),
        ..WorkspaceEdit::default()
    }))
}

// Rename the file or directory of a filesystem link on disk and update every link to it or, for a
// directory, to anything within it. The client creates any missing directories, and directories
// that contained nothing but the renamed node are deleted when the client supports it.
fn rename_filesystem_node_for_document(
    snapshot: &Snapshot,
    cursor_offset: usize,
    new_name: &str,
    file_operation_support: FileOperationSupport,
) -> std::result::Result<Option<WorkspaceEdit>, String> {
    // Find the renamable filesystem node at the cursor, leaving other positions to text nodes.
    let Some(RenamableFilesystemNode {
        wiki_directory,
        old_target,
        old_path,
        ..
    }) = renamable_filesystem_node_at(snapshot, cursor_offset, file_operation_support.rename)?
    else {
        return Ok(None);
    };

    // Accept only a new path which the parser would accept in a link, for a node of the same kind.
    // A new target written exactly like the old one, which is spelled as on disk, changes nothing.
    let is_directory = old_target.is_directory();
    let new_target = FilesystemTarget::from_name(new_name.trim(), is_directory)?;
    if new_target == old_target {
        return Ok(Some(WorkspaceEdit::default()));
    }
    if new_target.is_wiki_directory() {
        return Err("A file or directory can't be renamed to the wiki directory.".to_owned());
    }

    // Require the existing directories along the new path to be spelled as they are on disk, as a
    // link would have to be. A filesystem that ignores case would otherwise put the node in a
    // directory whose path doesn't match the new path as written, and the comparisons below would
    // go wrong. For example, a directory which will contain the node could look empty after the
    // rename and be deleted along with it. The rest of the new path doesn't exist, other than
    // possibly its final name, so it has no other spelling.
    let (new_ancestor, new_suffix) = wiki_directory
        .spell_existing_ancestor(&new_target)
        .map_err(|error| error.message)?;
    let old_absolute_path = wiki_directory.resolve(&old_path);
    let new_absolute_path = wiki_directory.resolve(&new_ancestor).join(new_suffix);

    // Require the destination to be free and creatable, unless a directory moves into itself. Then
    // everything at its destination moves along with it, so nothing there can conflict. The old
    // path exists, so it's a prefix of the new one exactly when it's one of the new one's existing
    // ancestors.
    let moves_into_itself = is_directory && new_ancestor.starts_with(&old_path);
    if is_directory
        && !moves_into_itself
        && resolves_within(&wiki_directory.resolve(&new_ancestor), &old_absolute_path)
            .unwrap_or(false)
    {
        // A move into itself goes through a temporary sibling, which would leave the symlink
        // leading nowhere, so refuse to move a directory into itself through one.
        return Err(format!(
            "{} can't be moved into itself through a symlink.",
            old_path.code_path(),
        ));
    }
    if !moves_into_itself {
        check_rename_destination(
            &wiki_directory,
            &old_path,
            new_target.path(),
            &new_ancestor,
            &new_absolute_path,
        )?;
    }

    // Rewrite the path of every link to the renamed entry.
    let edits = filesystem_rename_edits(
        snapshot.wiki(),
        &snapshot.contents,
        &old_target,
        &new_target,
    );

    // Edit the wiki at the version the edits were computed from.
    let text_document_edit = DocumentChangeOperation::Edit(TextDocumentEdit {
        text_document: OptionalVersionedTextDocumentIdentifier {
            uri: snapshot.uri.clone(),
            version: Some(snapshot.version),
        },
        edits: edits
            .into_iter()
            .map(|(source_range, new_text)| {
                OneOf::Left(TextEdit::new(
                    snapshot
                        .line_index()
                        .range(&snapshot.contents, source_range),
                    new_text,
                ))
            })
            .collect(),
    });

    // Rename the node. No filesystem can move a directory into itself directly, so such a move
    // goes through a temporary sibling, from which the directory moves to its new path, recreating
    // its old path as a parent. VS Code validates a run of renames before performing any of them,
    // so the text edit separates the two renames to let the first one happen before the second is
    // validated.
    let mut operations = if moves_into_itself {
        let temporary_path = unused_sibling_path(&old_absolute_path);
        vec![
            rename_operation(&old_absolute_path, &temporary_path),
            text_document_edit,
            rename_operation(&temporary_path, &new_absolute_path),
        ]
    } else {
        vec![
            text_document_edit,
            rename_operation(&old_absolute_path, &new_absolute_path),
        ]
    };

    // Finally, delete the directories the rename leaves empty. VS Code validates a run of deletions
    // before performing any of them, so a nested empty directory would block deleting its parent.
    // Instead, one recursive deletion removes the outermost directory, which contains only empty
    // directories once the renamed node has moved.
    if file_operation_support.delete
        && let Some(directory) =
            outermost_directory_emptied_by_rename(&wiki_directory, &old_path, &new_ancestor)
    {
        operations.push(DocumentChangeOperation::Op(ResourceOp::Delete(
            DeleteFile {
                uri: Uri::from_file_path(wiki_directory.resolve(&directory))
                    .expect("A path within a saved wiki's directory should be absolute."),
                options: Some(DeleteFileOptions {
                    recursive: Some(true),
                    ignore_if_not_exists: Some(true),
                }),
                annotation_id: None,
            },
        )));
    }
    Ok(Some(WorkspaceEdit {
        document_changes: Some(DocumentChanges::Operations(operations)),
        ..WorkspaceEdit::default()
    }))
}

// This describes the filesystem node targeted by the link at the cursor, once it's known to be
// renamable regardless of its new name.
struct RenamableFilesystemNode {
    wiki_directory: WikiDirectory,
    path_source_range: SourceRange,
    old_target: FilesystemTarget,
    old_path: SpelledPath,
}

// Find the filesystem node targeted by the link at the cursor and check whether it can be renamed
// at all. Every other position yields no node, leaving it to text node renaming.
fn renamable_filesystem_node_at(
    snapshot: &Snapshot,
    cursor_offset: usize,
    supports_file_renames: bool,
) -> std::result::Result<Option<RenamableFilesystemNode>, String> {
    // Resolve the filesystem link at the cursor and its target.
    let Some(Link::Filesystem {
        target: old_target,
        source_range,
    }) = link_at(snapshot.wiki(), cursor_offset)
    else {
        return Ok(None);
    };
    let (is_directory, old_path, source_range) =
        (old_target.is_directory(), old_target.path(), *source_range);
    let path_source_range = filesystem_link_path_source_range(&snapshot.contents, source_range);

    // The client renames the node on disk, which requires a saved wiki and a capable client.
    let Some(wiki_path) = &snapshot.path else {
        return Err("Save the wiki before renaming the files it links to.".to_owned());
    };
    if !supports_file_renames {
        return Err("This editor doesn't support renaming files.".to_owned());
    }

    // Require the linked node to exist as the kind the link names, other than the wiki directory,
    // resolving it from the wiki's containing directory as validation does.
    let wiki_directory = WikiDirectory::new(wiki_path).map_err(|error| error.message)?;
    if old_target.is_wiki_directory() {
        return Err("The wiki directory can't be renamed.".to_owned());
    }
    let kind = if is_directory { "Directory" } else { "File" };
    if !fs::metadata(wiki_directory.path().join(old_path))
        .is_ok_and(|metadata| metadata.is_dir() == is_directory)
    {
        return Err(format!("{kind} {} not found.", old_path.code_path()));
    }

    // Require the path to be spelled as it is on disk, as the checker does. Otherwise, the rename
    // would update only the links spelled like this one, breaking any spelled correctly.
    let old_path = wiki_directory
        .spell(old_target, &mut DirectoryListings::new())
        .map_err(|error| error.message)?;
    if wiki_directory.wiki_path() == Some(&old_path) {
        return Err("The wiki can't be renamed through one of its own links.".to_owned());
    }

    Ok(Some(RenamableFilesystemNode {
        wiki_directory,
        path_source_range,
        old_target: old_target.clone(),
        old_path,
    }))
}

// Locate the path of a filesystem link that the parser produced from the given source, including
// its leading `/` but excluding its delimiters and surrounding whitespace.
fn filesystem_link_path_source_range(
    source_contents: &str,
    source_range: SourceRange,
) -> SourceRange {
    // Trim the link's inner text as the parser does. The leading `/` is part of the path.
    let target_source_range = text_link_target_source_range(source_contents, source_range);
    let target = &source_contents[target_source_range.start..target_source_range.end];
    let start = target_source_range.start + (target.len() - target.trim_start().len());
    SourceRange {
        start,
        end: start + target.trim().len(),
    }
}

// Require a rename's destination to be free. Missing directories will be created, but not beneath
// an existing file. The new path is written as the user typed it, and its deepest existing
// ancestor is spelled as on disk.
fn check_rename_destination(
    wiki_directory: &WikiDirectory,
    old_path: &SpelledPath,
    new_path: &Path,
    new_ancestor: &SpelledPath,
    new_absolute_path: &Path,
) -> std::result::Result<(), String> {
    // Refuse to replace another node. Something exists at the new path if its own metadata can be
    // read, even if it's a broken symlink. On a filesystem that ignores case, such as macOS's
    // default one, the new path may instead name the node being renamed, spelled differently, as
    // when renaming `photo.jpg` to `Photo.jpg`. Refuse that too, since VS Code treats both
    // spellings as the same file and silently skips the rename while still editing the links,
    // which leaves them misspelled.
    if fs::symlink_metadata(new_absolute_path).is_ok() {
        return Err(
            if entry_identity(new_absolute_path)
                == entry_identity(&wiki_directory.resolve(old_path))
            {
                format!(
                    "{} and {} differ only in case, which VS Code can't rename. Rename it in the \
                        explorer instead, then fix its links.",
                    old_path.code_path(),
                    new_path.code_path(),
                )
            } else {
                format!("{} already exists.", new_path.code_path())
            },
        );
    }

    // Refuse to create a directory beneath an existing file.
    if !wiki_directory.resolve(new_ancestor).is_dir() {
        return Err(format!(
            "Path {} isn't a directory.",
            new_ancestor.code_path(),
        ));
    }
    Ok(())
}

// Rewrite the target of every link to a renamed entry. Renaming a directory also moves everything
// within it, while a file is referenced only by file links with its exact path.
fn filesystem_rename_edits(
    wiki: &Wiki,
    source_contents: &str,
    old_target: &FilesystemTarget,
    new_target: &FilesystemTarget,
) -> Vec<(SourceRange, String)> {
    // Move each link to the renamed entry or, for a directory, to anything within it.
    let mut edits = Vec::new();
    for link in wiki.text_nodes.values().flat_map(|node| &node.links) {
        let Link::Filesystem {
            target,
            source_range,
        } = link
        else {
            continue;
        };
        let moved_target = if old_target.is_directory() {
            target.moved(old_target, new_target)
        } else {
            (target == old_target).then(|| new_target.clone())
        };

        // Replace the link's target with its canonical text.
        if let Some(moved_target) = moved_target {
            edits.push((
                filesystem_link_path_source_range(source_contents, *source_range),
                moved_target.text().into_string(),
            ));
        }
    }

    // Present the edits in source order.
    edits.sort_by_key(|(source_range, _new_text)| (source_range.start, source_range.end));
    edits
}

// Choose an unused, hidden name beside a node for a temporary rename. Staying in the same directory
// keeps the rename on the same filesystem.
fn unused_sibling_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("A renamed node should have a UTF-8 name.");
    (1..=u64::MAX)
        .map(|attempt| {
            path.with_file_name(if attempt == 1 {
                format!(".{name}.mull-rename")
            } else {
                format!(".{name}.mull-rename-{attempt}")
            })
        })
        .find(|candidate| fs::symlink_metadata(candidate).is_err())
        .expect("An unused temporary name should exist.")
}

// Build an operation that renames a node from one absolute path to another.
fn rename_operation(old_path: &Path, new_path: &Path) -> DocumentChangeOperation {
    DocumentChangeOperation::Op(ResourceOp::Rename(RenameFile {
        old_uri: Uri::from_file_path(old_path)
            .expect("A path within a saved wiki's directory should be absolute."),
        new_uri: Uri::from_file_path(new_path)
            .expect("A path within a saved wiki's directory should be absolute."),
        options: None,
        annotation_id: None,
    }))
}

// Find the outermost directory whose only entry is the node being moved, directly or through
// directories whose only entry leads to it. Any other entry keeps a directory, even an empty
// directory or an ignored file. A directory is also kept if it will contain the new path, which,
// since it exists, is when the new path's deepest existing ancestor leads into it, even through a
// symlink, or if that can't be determined. It isn't kept merely because a directory link names
// it, since such a link only stands for the files within the directory. The search never reaches
// the wiki directory itself.
fn outermost_directory_emptied_by_rename(
    wiki_directory: &WikiDirectory,
    old_path: &SpelledPath,
    new_ancestor: &SpelledPath,
) -> Option<SpelledPath> {
    // Ascend while each directory contains nothing but the entry being moved or deleted from it.
    let new_ancestor_absolute_path = wiki_directory.resolve(new_ancestor);
    let mut emptied_directory = None;
    let mut removed_entry = old_path.clone();
    while let Some(directory) = removed_entry
        .parent()
        .filter(|directory| !directory.is_wiki_directory())
    {
        let absolute_directory = wiki_directory.resolve(&directory);
        if resolves_within(&new_ancestor_absolute_path, &absolute_directory).unwrap_or(true) {
            break;
        }
        let Ok(mut entries) = fs::read_dir(absolute_directory) else {
            break;
        };
        let contains_only_removed_entry = entries
            .next()
            .and_then(std::result::Result::ok)
            .is_some_and(|entry| Some(entry.file_name().as_os_str()) == removed_entry.file_name())
            && entries.next().is_none();
        if !contains_only_removed_entry {
            break;
        }
        emptied_directory = Some(directory.clone());
        removed_entry = directory;
    }
    emptied_directory
}

// Determine whether a path leads into a directory, or to the directory itself, once symlinks are
// resolved. Report nothing if either can't be resolved.
fn resolves_within(path: &Path, directory: &Path) -> Option<bool> {
    Some(
        fs::canonicalize(path)
            .ok()?
            .starts_with(fs::canonicalize(directory).ok()?),
    )
}

// Produce a whole-document formatting edit for any wiki that parses, even if it's invalid.
fn formatting_for_document(snapshot: &Snapshot) -> Option<Vec<TextEdit>> {
    // Render the parsed wiki without reporting syntax errors, which diagnostics already cover.
    let (wiki, syntax_errors) = snapshot.parsed();
    if !syntax_errors.is_empty() {
        return None;
    }
    let rendered_wiki = wiki.to_string();

    // A successful request returns either one whole-document edit or an empty edit list.
    if snapshot.contents == rendered_wiki {
        Some(Vec::new())
    } else {
        Some(vec![TextEdit::new(
            Range::new(
                Position::new(0, 0),
                snapshot
                    .line_index()
                    .position(&snapshot.contents, snapshot.contents.len()),
            ),
            rendered_wiki,
        )])
    }
}

// Describe every parsed text node for editor outlines and document-symbol navigation.
#[allow(
    deprecated,
    reason = "The protocol's DocumentSymbol type retains a required legacy field."
)]
fn document_symbol_for_document(
    snapshot: &Snapshot,
    supports_hierarchy: bool,
) -> DocumentSymbolResponse {
    // Parse syntax without semantic validation, recovering from syntax errors so the nodes remain
    // navigable while they're fixed.
    let wiki = snapshot.wiki();
    let line_index = snapshot.line_index();
    let mut nodes = wiki.text_nodes.values().collect::<Vec<_>>();
    nodes.sort_by_key(|node| node.source_range.start);

    // Use separate node and title ranges when the client supports hierarchical symbols, and locate
    // each flat symbol at its title otherwise.
    if supports_hierarchy {
        DocumentSymbolResponse::Nested(
            nodes
                .into_iter()
                .map(|node| DocumentSymbol {
                    name: node.title.clone(),
                    detail: None,
                    kind: SymbolKind::OBJECT,
                    tags: None,
                    deprecated: None,
                    range: line_index.range(&snapshot.contents, node.source_range),
                    selection_range: line_index.range(&snapshot.contents, node.title_source_range),
                    children: None,
                })
                .collect(),
        )
    } else {
        DocumentSymbolResponse::Flat(
            nodes
                .into_iter()
                .map(|node| SymbolInformation {
                    name: node.title.clone(),
                    kind: SymbolKind::OBJECT,
                    tags: None,
                    deprecated: None,
                    location: Location::new(
                        snapshot.uri.clone(),
                        line_index.range(&snapshot.contents, node.title_source_range),
                    ),
                    container_name: None,
                })
                .collect(),
        )
    }
}

// Offer the fixes that the checker attached to the diagnostics at an editor range, each resolving
// every diagnostic it was attached to.
fn code_action_for_document(
    snapshot: &Snapshot,
    diagnostics: &[Diagnostic],
) -> Option<CodeActionResponse> {
    // Collect the distinct fixes, reading each from the data that `diagnostic` wrote.
    let mut fixes = BTreeMap::<Fix, Vec<Diagnostic>>::new();
    for diagnostic in diagnostics {
        let Some(fix) = diagnostic
            .data
            .clone()
            .and_then(|data| serde_json::from_value::<Fix>(data).ok())
        else {
            continue;
        };
        fixes.entry(fix).or_default().push(diagnostic.clone());
    }

    // Parse the current snapshot so a stale diagnostic doesn't lead to a fix that's already made,
    // recovering from syntax errors since validation reports missing nodes despite them.
    let wiki = snapshot.wiki();
    let line_index = snapshot.line_index();
    let actions = fixes
        .into_iter()
        .filter_map(|(fix, diagnostics)| {
            let (title, edit, command) = match fix {
                Fix::CreateNode(title) if wiki.text_nodes.contains_key(&title) => return None,
                Fix::CreateNode(title) => {
                    // Prepend the home node, and insert any other node after the last node that
                    // starts before the end of its first diagnostic, which is the node linking to
                    // it, or else at the end of the wiki. The formatter decides where it ends up.
                    let linking_node = diagnostics
                        .iter()
                        .filter_map(|diagnostic| {
                            line_index.byte_offset(&snapshot.contents, diagnostic.range.end)
                        })
                        .min()
                        .and_then(|offset| {
                            wiki.text_nodes
                                .values()
                                .filter(|node| node.source_range.start < offset)
                                .max_by_key(|node| node.source_range.start)
                        });
                    let (offset, before_title, after_title) = if title == HOME_TITLE {
                        let after_title = if snapshot.contents.is_empty() {
                            "\n"
                        } else {
                            "\n\n"
                        };
                        (0, "", after_title)
                    } else if let Some(node) = linking_node {
                        (node.source_range.end, "\n\n", "")
                    } else {
                        let before_title = if snapshot.contents.ends_with("\n\n") {
                            ""
                        } else if snapshot.contents.ends_with('\n') {
                            "\n"
                        } else {
                            "\n\n"
                        };
                        (snapshot.contents.len(), before_title, "\n")
                    };
                    let insertion = line_index.position(&snapshot.contents, offset);
                    let edit = TextEdit::new(
                        Range::new(insertion, insertion),
                        format!("{before_title}{TITLE_PREFIX}{title}{after_title}"),
                    );

                    // Place the cursor at the end of the new title once the edit is applied. The
                    // source before the insertion is unchanged, and the inserted text leads up to
                    // the title's end.
                    let text_before_cursor = format!(
                        "{}{before_title}{TITLE_PREFIX}{title}",
                        &snapshot.contents[..offset],
                    );
                    let cursor = LineIndex::new(&text_before_cursor)
                        .position(&text_before_cursor, text_before_cursor.len());
                    let command = Command::new(
                        format!("Reveal node {}", title.code_str()),
                        REVEAL_RANGE_COMMAND.to_owned(),
                        Some(vec![
                            snapshot.uri.as_str().into(),
                            cursor.line.into(),
                            cursor.character.into(),
                            cursor.line.into(),
                            cursor.character.into(),
                        ]),
                    );
                    (
                        format!("Create node {}", title.code_str()),
                        edit,
                        Some(command),
                    )
                }
            };
            Some(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(diagnostics),
                edit: Some(WorkspaceEdit {
                    changes: Some(HashMap::from([(snapshot.uri.clone(), vec![edit])])),
                    ..WorkspaceEdit::default()
                }),
                command,
                is_preferred: Some(true),
                ..CodeAction::default()
            }))
        })
        .collect::<Vec<_>>();

    // Report the absence of fixes as no response.
    (!actions.is_empty()).then_some(actions)
}

// Make each filesystem link whose target exists clickable: a file opens in the editor, and a
// directory is revealed in the explorer.
fn document_link_for_document(snapshot: &Snapshot) -> Option<Vec<DocumentLink>> {
    // Resolve filesystem links from the directory containing a saved wiki, recovering from syntax
    // errors so the links remain clickable while they're fixed.
    let wiki_path = snapshot.path.as_deref()?;
    let wiki = snapshot.wiki();
    let line_index = snapshot.line_index();
    let wiki_directory = WikiDirectory::new(wiki_path).ok()?;

    // Link each filesystem link to its target, skipping any that the checker would report.
    let mut listings = DirectoryListings::new();
    let mut document_links = wiki
        .text_nodes
        .values()
        .flat_map(|node| &node.links)
        .filter_map(|link| {
            let Link::Filesystem {
                target,
                source_range,
            } = link
            else {
                return None;
            };
            let tooltip = if target.is_directory() {
                "Reveal in Explorer"
            } else {
                "Open file"
            };
            Some(DocumentLink {
                range: line_index.range(&snapshot.contents, *source_range),
                target: Some(filesystem_link_target(
                    &wiki_directory,
                    target,
                    &mut listings,
                )?),
                tooltip: Some(tooltip.to_owned()),
                data: None,
            })
        })
        .collect::<Vec<_>>();

    // Report the links in source order.
    document_links.sort_by_key(|document_link| {
        (
            document_link.range.start.line,
            document_link.range.start.character,
        )
    });
    Some(document_links)
}

// Find where following a filesystem link should lead: a file opens in the editor, and a directory
// is revealed in the explorer. A link whose target is missing, of the wrong kind, or spelled
// differently than on disk leads nowhere, just as the checker reports it.
fn filesystem_link_target(
    wiki_directory: &WikiDirectory,
    target: &FilesystemTarget,
    listings: &mut DirectoryListings,
) -> Option<Uri> {
    // Require the target to be spelled as it is on disk and to exist as the kind of entry the link
    // names.
    let is_directory = target.is_directory();
    let target_path = wiki_directory.resolve(&wiki_directory.spell(target, listings).ok()?);
    if !fs::metadata(&target_path).is_ok_and(|metadata| metadata.is_dir() == is_directory) {
        return None;
    }

    // Open a file directly, and reveal a directory through the extension.
    let target_uri = Uri::from_file_path(&target_path)
        .expect("A path within a saved wiki's directory should be absolute.");
    Some(if is_directory {
        reveal_in_explorer_command_url(&target_uri)
            .parse()
            .expect("A command URL should be a valid URI.")
    } else {
        target_uri
    })
}

// Build a command link that reveals a directory in the extension's explorer.
fn reveal_in_explorer_command_url(directory_uri: &Uri) -> String {
    // Pass the directory URI as the command's only positional argument.
    format!(
        "command:{REVEAL_IN_EXPLORER_COMMAND}?{}",
        utf8_percent_encode(
            &serde_json::to_string(&[directory_uri.as_str()])
                .expect("A list of strings should serialize to JSON."),
            NON_ALPHANUMERIC,
        ),
    )
}

// This describes which part of a resolved text link a caller considers relevant.
#[derive(Clone, Copy)]
enum LinkExtent {
    // The complete link, including its square-bracket delimiters.
    Whole,

    // The link's inner text, which excludes its delimiters.
    Target,
}

// Resolve the node denoted by a declaration or text link at a source offset.
fn node_at<'a>(
    wiki: &'a Wiki,
    source_contents: &str,
    byte_offset: usize,
    link_extent: LinkExtent,
) -> Option<(&'a TextNode, SourceRange)> {
    // Prefer a declaration, whose title is the only range it can contribute.
    if let Some(node) = wiki.text_nodes.values().find(|node| {
        node.title_source_range.start <= byte_offset && byte_offset < node.title_source_range.end
    }) {
        return Some((node, node.title_source_range));
    }

    // Resolve a reference, reporting whichever extent of the link the caller asked for.
    let Link::Text {
        title,
        source_range,
    } = link_at(wiki, byte_offset)?
    else {
        return None;
    };
    let source_range = *source_range;
    Some((
        wiki.text_nodes.get(title)?,
        match link_extent {
            LinkExtent::Whole => source_range,
            LinkExtent::Target => text_link_target_source_range(source_contents, source_range),
        },
    ))
}

// Find the link of any kind at a source offset without resolving its destination. Links never
// overlap, so at most one link can contain the offset.
fn link_at(wiki: &Wiki, byte_offset: usize) -> Option<&Link> {
    wiki.text_nodes.values().find_map(|node| {
        node.links.iter().find(|link| {
            let source_range = link.source_range();
            source_range.start <= byte_offset && byte_offset < source_range.end
        })
    })
}

// Exclude the delimiters from a source range known to represent a complete link.
fn text_link_target_source_range(source_contents: &str, source_range: SourceRange) -> SourceRange {
    // Confirm the parser-provided range still addresses square-bracket delimiters.
    let target_source = source_contents
        .get(source_range.start..source_range.end)
        .and_then(|link_source| link_source.strip_prefix('['))
        .and_then(|link_source| link_source.strip_suffix(']'))
        .expect("A parsed link should be delimited by square brackets.");
    let start = source_range.start + '['.len_utf8();
    SourceRange {
        start,
        end: start + target_source.len(),
    }
}

// Convert only file-scheme URIs because the URI library doesn't enforce this distinction.
fn local_path(uri: &Uri) -> Option<Cow<'_, Path>> {
    uri.scheme()
        .as_str()
        .eq_ignore_ascii_case("file")
        .then(|| uri.to_file_path())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::{
        FileOperationSupport, Snapshot, code_action_for_document, completion_for_document,
        diagnostic_from_error, diagnostics_for_document, document_highlight_for_document,
        document_link_for_document, document_symbol_for_document, formatting_for_document,
        goto_definition_for_document, hover_for_document, prepare_rename_for_document,
        references_for_document, rename_for_document, reveal_range_command_url,
    };
    use crate::{
        cancellation::CancellationFlag,
        error::{Fix, SourceRange},
        line_index::LineIndex,
        parser,
    };
    use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
    use std::{
        fs,
        path::{Path, PathBuf},
        process,
        sync::atomic::{AtomicUsize, Ordering},
    };
    use tower_lsp_server::ls_types::{
        CodeActionKind, CodeActionOrCommand, CompletionItem, CompletionItemKind,
        CompletionTextEdit, Diagnostic, DiagnosticSeverity, DocumentChangeOperation,
        DocumentChanges, DocumentHighlight, DocumentHighlightKind, DocumentSymbolResponse,
        GotoDefinitionResponse, HoverContents, MarkupKind, OneOf, Position, PrepareRenameResponse,
        Range, ResourceOp, SymbolKind, Uri, WorkspaceEdit,
    };

    // Assign each formatting fixture a distinct directory when tests run concurrently.
    static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    // Identify the version of every test snapshot, which renames report back.
    const TEST_VERSION: i32 = 7;

    // Capture a test fixture as the snapshot of an open document.
    fn snapshot(uri: &Uri, source: &str) -> Snapshot {
        Snapshot::new(uri.clone(), source.to_owned(), TEST_VERSION)
    }

    // Compute diagnostics for a check which nothing cancels.
    fn diagnostics(uri: &Uri, source_contents: &str) -> Vec<Diagnostic> {
        diagnostics_for_document(
            &snapshot(uri, source_contents),
            &CancellationFlag::default(),
        )
        .expect("A check without cancellation should complete.")
    }

    // This guard owns a temporary wiki directory and removes it after each test.
    struct TestWiki(PathBuf);

    impl TestWiki {
        // Create a file-backed wiki so validation sees the same environment as the editor.
        fn new(source_contents: &str) -> Self {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir()
                .join(format!("mull-language-server-{}-{sequence}", process::id()));
            fs::create_dir(&directory).unwrap();
            let wiki_path = directory.join("wiki.mull");
            fs::write(&wiki_path, source_contents).unwrap();
            Self(wiki_path)
        }

        // Expose the temporary wiki path to the formatter.
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestWiki {
        // Remove the complete fixture directory when the test finishes.
        fn drop(&mut self) {
            fs::remove_dir_all(self.0.parent().unwrap()).unwrap();
        }
    }

    // Construct the URI assigned to a new editor buffer before its first save.
    fn untitled_uri() -> Uri {
        "untitled:Untitled-1".parse().unwrap()
    }

    // Convert an editor position in a test fixture into a byte offset.
    fn byte_offset(source: &str, position: Position) -> Option<usize> {
        LineIndex::new(source).byte_offset(source, position)
    }

    // Encode a document URI and UTF-16 title range for the trusted editor command.
    #[test]
    fn reveal_range_commands_encode_destinations() {
        let url = reveal_range_command_url(
            &untitled_uri(),
            Range::new(Position::new(0, 2), Position::new(0, 6)),
        );

        assert_eq!(
            url,
            concat!(
                "command:mull.revealRange?",
                "%5B%22untitled%3AUntitled%2D1%22%2C0%2C2%2C0%2C6%5D",
            ),
        );
    }

    // Keep the outline of a wiki with syntax errors.
    #[test]
    fn document_symbols_recover_from_syntax_errors() {
        let source = "# Zebra\n\nUnexpected]\n\n# Alpha";
        let DocumentSymbolResponse::Nested(symbols) =
            document_symbol_for_document(&snapshot(&untitled_uri(), source), true)
        else {
            panic!("Text nodes should be represented as nested document symbols.");
        };

        assert_eq!(
            symbols
                .iter()
                .map(|symbol| symbol.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Zebra", "Alpha"],
        );
    }

    // Expose text nodes in source order for saved and untitled editor outlines.
    #[test]
    fn document_symbols_describe_text_nodes() {
        let source = "# Zebra\n\nFirst\n\n# Alpha\n\nSecond";
        let wiki = TestWiki::new(source);
        let uris = [untitled_uri(), Uri::from_file_path(wiki.path()).unwrap()];

        for uri in uris {
            let response = document_symbol_for_document(&snapshot(&uri, source), true);
            let DocumentSymbolResponse::Nested(symbols) = response else {
                panic!("Text nodes should be represented as nested document symbols.");
            };
            assert_eq!(
                symbols
                    .iter()
                    .map(|symbol| symbol.name.as_str())
                    .collect::<Vec<_>>(),
                vec!["Zebra", "Alpha"],
            );
            assert!(symbols.iter().all(|symbol| {
                symbol.kind == SymbolKind::OBJECT
                    && symbol.detail.is_none()
                    && symbol.tags.is_none()
                    && symbol.children.is_none()
            }));
            assert_eq!(
                symbols[0].range,
                Range::new(Position::new(0, 0), Position::new(2, 5)),
            );
            assert_eq!(
                symbols[0].selection_range,
                Range::new(Position::new(0, 2), Position::new(0, 7)),
            );
            assert_eq!(
                symbols[1].range,
                Range::new(Position::new(4, 0), Position::new(6, 6)),
            );
            assert_eq!(
                symbols[1].selection_range,
                Range::new(Position::new(4, 2), Position::new(4, 7)),
            );

            // Fall back to universally supported flat symbols at each title range.
            let response = document_symbol_for_document(&snapshot(&uri, source), false);
            let DocumentSymbolResponse::Flat(symbols) = response else {
                panic!("Clients without hierarchy support should receive flat symbols.");
            };
            assert_eq!(
                symbols
                    .iter()
                    .map(|symbol| symbol.name.as_str())
                    .collect::<Vec<_>>(),
                vec!["Zebra", "Alpha"],
            );
            assert_eq!(
                symbols[0].location.range,
                Range::new(Position::new(0, 2), Position::new(0, 7)),
            );
            assert_eq!(
                symbols[1].location.range,
                Range::new(Position::new(4, 2), Position::new(4, 7)),
            );
        }
    }

    // Analyze ordinary text-node structure in a new editor buffer.
    #[test]
    fn untitled_text_only_wikis_receive_diagnostics() {
        let source = "# Home\nSee [Missing].";
        let diagnostics = diagnostics(&untitled_uri(), source);

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].message, "Node `Missing` not found.");
        assert_eq!(
            diagnostics[0].range,
            Range::new(Position::new(1, 4), Position::new(1, 13)),
        );
    }

    // Report parser errors from a new editor buffer at their exact source locations.
    #[test]
    fn untitled_syntax_errors_receive_diagnostics() {
        let source = "# Home\nUnexpected]";
        let diagnostics = diagnostics(&untitled_uri(), source);

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].message, "Unexpected closing link delimiter.");
        assert_eq!(
            diagnostics[0].range,
            Range::new(Position::new(1, 10), Position::new(1, 11)),
        );
    }

    // Report validation errors in a new editor buffer despite its syntax errors.
    #[test]
    fn untitled_syntax_errors_accompany_validation_errors() {
        let source = "# Home\nUnexpected] [Missing]";
        let diagnostics = diagnostics(&untitled_uri(), source);

        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            vec![
                "Unexpected closing link delimiter.",
                "Node `Missing` not found.",
            ],
        );
    }

    // Require a first save before resolving filesystem links from a new editor buffer.
    #[test]
    fn untitled_filesystem_links_receive_diagnostics() {
        let source = "# Home\n[/notes.txt] [/images/]";
        let diagnostics = diagnostics(&untitled_uri(), source);

        assert_eq!(diagnostics.len(), 2);
        assert!(diagnostics.iter().all(|diagnostic| {
            diagnostic.message == "Save the wiki to validate this filesystem link."
        }));
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.range)
                .collect::<Vec<_>>(),
            vec![
                Range::new(Position::new(1, 0), Position::new(1, 12)),
                Range::new(Position::new(1, 13), Position::new(1, 23)),
            ],
        );
    }

    // Navigate among text nodes without requiring a new editor buffer to have a path.
    #[test]
    fn untitled_wikis_support_navigation() {
        let source = "# Home\n\n[Greeting]\n\n# Greeting";

        assert!(
            goto_definition_for_document(&snapshot(&untitled_uri(), source), Position::new(2, 4))
                .is_some(),
        );
        assert!(
            hover_for_document(&snapshot(&untitled_uri(), source), Position::new(2, 4)).is_some(),
        );
        assert!(
            references_for_document(
                &snapshot(&untitled_uri(), source),
                Position::new(2, 4),
                false,
            )
            .is_some(),
        );
    }

    // Complete a partial target by replacing the whole link, including its delimiters.
    #[test]
    fn completions_replace_closed_links() {
        let source = "# Home\n\n[Gr]\n\n# Greeting\n\n# Other";
        let completions =
            completion_for_document(&snapshot(&untitled_uri(), source), Position::new(2, 3))
                .unwrap();

        assert_eq!(
            completions
                .iter()
                .map(|completion| completion.label.as_str())
                .collect::<Vec<_>>(),
            vec!["Greeting", "Home", "Other"],
        );
        let Some(CompletionTextEdit::Edit(edit)) = &completions[0].text_edit else {
            panic!("A completion should replace the link.");
        };
        let link_range = Range::new(Position::new(2, 0), Position::new(2, 4));
        assert_eq!(edit.range, link_range);
        assert_eq!(edit.new_text, "[Greeting]");

        // Replace the same link from a cursor before its opening delimiter.
        let completions =
            completion_for_document(&snapshot(&untitled_uri(), source), Position::new(2, 0))
                .unwrap();
        let Some(CompletionTextEdit::Edit(edit)) = &completions[0].text_edit else {
            panic!("A completion should replace the link.");
        };
        assert_eq!(edit.range, link_range);
    }

    // Complete an unfinished link despite a syntax error elsewhere in the wiki.
    #[test]
    fn completions_recover_from_syntax_errors() {
        let source = "# Home\n\n[Gre\n\n# Greeting\n\nUnexpected]";
        let completions =
            completion_for_document(&snapshot(&untitled_uri(), source), Position::new(2, 4))
                .unwrap();

        assert!(
            completions
                .iter()
                .any(|completion| completion.label == "Greeting"),
        );
    }

    // Close an unfinished link temporarily while calculating its completions.
    #[test]
    fn completions_support_unfinished_links() {
        let source = "# Home\n\n[Gre\n\n# Greeting";
        let completions =
            completion_for_document(&snapshot(&untitled_uri(), source), Position::new(2, 4))
                .unwrap();
        let greeting = completions
            .iter()
            .find(|completion| completion.label == "Greeting")
            .unwrap();
        let Some(CompletionTextEdit::Edit(edit)) = &greeting.text_edit else {
            panic!("A completion should replace the unfinished link.");
        };

        assert_eq!(
            edit.range,
            Range::new(Position::new(2, 0), Position::new(2, 4)),
        );
        assert_eq!(edit.new_text, "[Greeting]");
    }

    // Leave the cursor after a single closing delimiter once a completion has been applied.
    #[test]
    fn completions_do_not_duplicate_closing_delimiters() {
        // Model an editor that has already auto-closed the link the cursor sits inside.
        let source = "# Home\n\n[Gr]\n\n# Greeting";
        let completions =
            completion_for_document(&snapshot(&untitled_uri(), source), Position::new(2, 3))
                .unwrap();
        let greeting = completions
            .iter()
            .find(|completion| completion.label == "Greeting")
            .unwrap();
        let Some(CompletionTextEdit::Edit(edit)) = &greeting.text_edit else {
            panic!("A completion should replace the link.");
        };

        // Apply the edit to confirm the link is closed exactly once.
        let start = byte_offset(source, edit.range.start).unwrap();
        let end = byte_offset(source, edit.range.end).unwrap();
        let mut applied = source.to_owned();
        applied.replace_range(start..end, &edit.new_text);
        assert_eq!(applied, "# Home\n\n[Greeting]\n\n# Greeting");
    }

    // Escape link delimiters when inserting a node title as a completion.
    #[test]
    fn completions_escape_title_delimiters() {
        let source = "# Home\n\n[]\n\n# A[B]";
        let completions =
            completion_for_document(&snapshot(&untitled_uri(), source), Position::new(2, 1))
                .unwrap();
        let bracketed = completions
            .iter()
            .find(|completion| completion.label == "A[B]")
            .unwrap();
        let Some(CompletionTextEdit::Edit(edit)) = &bracketed.text_edit else {
            panic!("A completion should encode the title as a text link.");
        };

        assert_eq!(bracketed.filter_text.as_deref(), Some("[A\\[B\\]"));
        assert_eq!(edit.new_text, "[A\\[B\\]]");
    }

    // Offer text-node completions only while the cursor is inside a text link target.
    #[test]
    fn completions_ignore_other_contexts() {
        let source = "# Home\n\nprose [/notes.txt]";

        assert!(
            completion_for_document(&snapshot(&untitled_uri(), source), Position::new(2, 2))
                .is_none(),
        );
        assert!(
            completion_for_document(&snapshot(&untitled_uri(), source), Position::new(2, 10))
                .is_none(),
        );
        assert!(
            completion_for_document(&snapshot(&untitled_uri(), source), Position::new(0, 3))
                .is_none(),
        );
    }

    // Summarize each completion by its label, replacement range, and inserted text.
    fn completion_edits(completions: &[CompletionItem]) -> Vec<(&str, Range, &str)> {
        completions
            .iter()
            .map(|completion| {
                let Some(CompletionTextEdit::Edit(edit)) = &completion.text_edit else {
                    panic!("A completion should replace part of the link.");
                };
                (
                    completion.label.as_str(),
                    edit.range,
                    edit.new_text.as_str(),
                )
            })
            .collect()
    }

    // Complete a filesystem link with the visible entries of the wiki directory.
    #[test]
    fn completions_list_filesystem_entries() {
        let source = "# Home\n\n[/]";
        let wiki = TestWiki::new(source);
        let directory = wiki.path().parent().unwrap();
        fs::write(directory.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(directory.join("ignored.txt"), "ignored").unwrap();
        fs::write(directory.join("notes.txt"), "notes").unwrap();
        fs::create_dir(directory.join("images")).unwrap();
        fs::write(directory.join("images/photo.jpg"), "photo").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let completions =
            completion_for_document(&snapshot(&uri, source), Position::new(2, 2)).unwrap();

        // Close a file's link but leave a directory's link open for its children.
        let empty_path = Range::new(Position::new(2, 2), Position::new(2, 2));
        let closed_path = Range::new(Position::new(2, 2), Position::new(2, 3));
        assert_eq!(
            completion_edits(&completions),
            vec![
                (".gitignore", closed_path, ".gitignore]"),
                ("images/", empty_path, "images/"),
                ("notes.txt", closed_path, "notes.txt]"),
            ],
        );
        assert_eq!(completions[0].kind, Some(CompletionItemKind::FILE));
        assert_eq!(completions[1].kind, Some(CompletionItemKind::FOLDER));
        assert!(completions[0].command.is_none());
        assert_eq!(
            completions[1]
                .command
                .as_ref()
                .map(|command| command.command.as_str()),
            Some("editor.action.triggerSuggest"),
        );
    }

    // Complete the path after the filesystem-link prefix, which the typed path includes.
    #[test]
    fn completions_follow_prefix() {
        let source = "# Home\n\n[/no";
        let wiki = TestWiki::new(source);
        fs::write(wiki.path().parent().unwrap().join("notes.txt"), "notes").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let completions =
            completion_for_document(&snapshot(&uri, source), Position::new(2, 4)).unwrap();

        assert_eq!(
            completion_edits(&completions),
            vec![(
                "notes.txt",
                Range::new(Position::new(2, 2), Position::new(2, 4)),
                "notes.txt]",
            )],
        );
    }

    // Complete the children of the directory named by an unfinished link, replacing only the
    // component being typed and omitting directories without files.
    #[test]
    fn completions_list_nested_directories() {
        let source = "# Home\n\n[/images/r";
        let wiki = TestWiki::new(source);
        let directory = wiki.path().parent().unwrap();
        fs::create_dir_all(directory.join("images/raw/large")).unwrap();
        fs::write(directory.join("images/raw/large/photo.tiff"), "photo").unwrap();
        fs::create_dir_all(directory.join("images/rejected/nested")).unwrap();
        fs::write(directory.join("images/photo.jpg"), "photo").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let completions =
            completion_for_document(&snapshot(&uri, source), Position::new(2, 10)).unwrap();

        let typed_component = Range::new(Position::new(2, 9), Position::new(2, 10));
        assert_eq!(
            completion_edits(&completions),
            vec![
                ("photo.jpg", typed_component, "photo.jpg]"),
                ("raw/", typed_component, "raw/"),
            ],
        );
        assert_eq!(completions[1].filter_text.as_deref(), Some("raw"));
    }

    // Escape link delimiters in completed names and interpret them in typed directories, which
    // completions leave untouched.
    #[test]
    fn completions_escape_path_delimiters() {
        let source = "# Home\n\n[/a\\[b\\]/]";
        let wiki = TestWiki::new(source);
        let directory = wiki.path().parent().unwrap();
        fs::create_dir_all(directory.join("a[b]")).unwrap();
        fs::write(directory.join("a[b]/c[d].txt"), "content").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        let completions =
            completion_for_document(&snapshot(&uri, source), Position::new(2, 2)).unwrap();
        assert_eq!(
            completion_edits(&completions),
            vec![(
                "a[b]/",
                Range::new(Position::new(2, 2), Position::new(2, 9)),
                "a\\[b\\]/",
            )],
        );

        let completions =
            completion_for_document(&snapshot(&uri, source), Position::new(2, 9)).unwrap();
        assert_eq!(
            completion_edits(&completions),
            vec![(
                "c[d].txt",
                Range::new(Position::new(2, 9), Position::new(2, 10)),
                "c\\[d\\].txt]",
            )],
        );
    }

    // Decline filesystem completions where the parser wouldn't recognize a valid link path.
    #[test]
    fn completions_ignore_invalid_filesystem_contexts() {
        let source = "# [/\n\n[/../] \\[/";
        let wiki = TestWiki::new(source);
        fs::write(wiki.path().parent().unwrap().join("notes.txt"), "notes").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        // Ignore titles, escaping paths, and escaped delimiters.
        for cursor in [
            Position::new(0, 4),
            Position::new(2, 5),
            Position::new(2, 10),
        ] {
            assert!(
                completion_for_document(&snapshot(&uri, source), cursor)
                    .is_none_or(|completions| completions.is_empty()),
            );
        }

        // Require a filesystem to list entries from.
        let source = "# Home\n\n[/]";
        assert!(
            completion_for_document(&snapshot(&untitled_uri(), source), Position::new(2, 2))
                .is_none(),
        );
    }

    // Treat a title as its own definition, which lets editors fall back to finding references.
    #[test]
    fn definitions_of_titles_target_themselves() {
        let source = "# Home\n\n[Home]";
        let definition =
            goto_definition_for_document(&snapshot(&untitled_uri(), source), Position::new(0, 3))
                .unwrap();

        let GotoDefinitionResponse::Link(links) = definition else {
            panic!("A title should have one definition.");
        };
        let [link] = links.as_slice() else {
            panic!("A title should have exactly one definition.");
        };
        let title_range = Range::new(Position::new(0, 2), Position::new(0, 6));
        assert_eq!(link.origin_selection_range, Some(title_range));
        assert_eq!(link.target_selection_range, title_range);
    }

    // Jump from a text link to the title of its destination node.
    #[test]
    fn definitions_target_node_titles() {
        let source = "# Home\n\n😀 [Greeting]\n\n# Greeting\n\nHello!";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let definition =
            goto_definition_for_document(&snapshot(&uri, source), Position::new(2, 5)).unwrap();

        let GotoDefinitionResponse::Link(links) = definition else {
            panic!("A text link should have one definition.");
        };
        let [link] = links.as_slice() else {
            panic!("A text link should have exactly one definition.");
        };
        assert_eq!(link.target_uri, uri);
        assert_eq!(
            link.origin_selection_range,
            Some(Range::new(Position::new(2, 3), Position::new(2, 13))),
        );
        assert_eq!(
            link.target_range,
            Range::new(Position::new(4, 0), Position::new(6, 6)),
        );
        assert_eq!(
            link.target_selection_range,
            Range::new(Position::new(4, 2), Position::new(4, 10)),
        );
    }

    // Preview the complete destination node while highlighting the source link.
    #[test]
    fn hovers_preview_nodes() {
        let source = "# Home\n\n😀 [Greeting]\n\n# Greeting\n\nLiteral \\[brackets\\] and [Home].";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let hover = hover_for_document(&snapshot(&uri, source), Position::new(2, 5)).unwrap();

        let HoverContents::Markup(contents) = hover.contents else {
            panic!("A node preview should use markup content.");
        };
        assert_eq!(contents.kind, MarkupKind::Markdown);
        let home_url =
            reveal_range_command_url(&uri, Range::new(Position::new(0, 2), Position::new(0, 6)));
        assert_eq!(
            contents.value,
            format!(
                "# Greeting\n\nLiteral [brackets] and \
                    [&#91;Home&#93;]({home_url}).",
            ),
        );
        assert_eq!(
            hover.range,
            Some(Range::new(Position::new(2, 3), Position::new(2, 13))),
        );
    }

    // Link filesystem links in previews to their targets, leaving missing targets as plain code.
    #[test]
    fn hovers_link_filesystem_targets() {
        let source = "# Home\n\n[Other]\n\n# Other\n\n[/notes.txt] [/images/] [/missing.txt]";
        let wiki = TestWiki::new(source);
        let directory = wiki.path().parent().unwrap();
        fs::write(directory.join("notes.txt"), "notes").unwrap();
        fs::create_dir(directory.join("images")).unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let hover = hover_for_document(&snapshot(&uri, source), Position::new(2, 2)).unwrap();

        let HoverContents::Markup(contents) = hover.contents else {
            panic!("A node preview should use markup content.");
        };
        let file_uri = Uri::from_file_path(directory.join("notes.txt")).unwrap();
        let directory_uri = Uri::from_file_path(directory.join("images")).unwrap();
        let reveal_url = format!(
            "command:mull.revealInExplorer?{}",
            utf8_percent_encode(
                &format!("[\"{}\"]", directory_uri.as_str()),
                NON_ALPHANUMERIC,
            ),
        );
        assert_eq!(
            contents.value,
            format!(
                "# Other\n\n[`[/notes.txt]`](<{}>) [`[/images/]`](<{reveal_url}>) \
                    `[/missing.txt]`",
                file_uri.as_str(),
            ),
        );

        // Leave filesystem links unlinked in an unsaved wiki.
        let hover =
            hover_for_document(&snapshot(&untitled_uri(), source), Position::new(2, 2)).unwrap();
        let HoverContents::Markup(contents) = hover.contents else {
            panic!("A node preview should use markup content.");
        };
        assert_eq!(
            contents.value,
            "# Other\n\n`[/notes.txt]` `[/images/]` `[/missing.txt]`",
        );
    }

    // Preview a node directly from its title declaration.
    #[test]
    fn hovers_preview_node_titles() {
        let source = "# Home\n\n[Greeting]\n\n# Greeting\n\nHello!";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let hover = hover_for_document(&snapshot(&uri, source), Position::new(4, 4)).unwrap();

        let HoverContents::Markup(contents) = hover.contents else {
            panic!("A node preview should use markup content.");
        };
        assert_eq!(contents.kind, MarkupKind::Markdown);
        assert_eq!(contents.value, "# Greeting\n\nHello!");
        assert_eq!(
            hover.range,
            Some(Range::new(Position::new(4, 2), Position::new(4, 10))),
        );
    }

    // Find every text link from either a declaration or one of its references.
    #[test]
    fn references_find_text_links() {
        let source = concat!(
            "# Home\n\n",
            "[Greeting] and [Greeting].\n\n",
            "# Other\n\n",
            "[Greeting]\n\n",
            "# Greeting",
        );
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let from_title =
            references_for_document(&snapshot(&uri, source), Position::new(8, 3), false).unwrap();
        let from_link =
            references_for_document(&snapshot(&uri, source), Position::new(2, 3), false).unwrap();

        assert_eq!(from_title, from_link);
        assert!(from_title.iter().all(|location| location.uri == uri));
        assert_eq!(
            from_title
                .iter()
                .map(|location| location.range)
                .collect::<Vec<_>>(),
            vec![
                Range::new(Position::new(2, 0), Position::new(2, 10)),
                Range::new(Position::new(2, 15), Position::new(2, 25)),
                Range::new(Position::new(6, 0), Position::new(6, 10)),
            ],
        );
    }

    // Include the declaration only when the language client requests it.
    #[test]
    fn references_optionally_include_declarations() {
        let source = "# Home\n\n[Greeting]\n\n# Greeting";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let locations =
            references_for_document(&snapshot(&uri, source), Position::new(4, 3), true).unwrap();

        assert_eq!(
            locations
                .iter()
                .map(|location| location.range)
                .collect::<Vec<_>>(),
            vec![
                Range::new(Position::new(2, 0), Position::new(2, 10)),
                Range::new(Position::new(4, 2), Position::new(4, 10)),
            ],
        );
    }

    // Distinguish a known node with no references from a cursor that denotes no node.
    #[test]
    fn references_can_be_empty() {
        let source = "# Home";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        assert_eq!(
            references_for_document(&snapshot(&uri, source), Position::new(0, 3), false),
            Some(Vec::new()),
        );
        assert!(
            references_for_document(&snapshot(&uri, source), Position::new(0, 0), false).is_none(),
        );
    }

    // Highlight a node declaration and all its text links from either kind of occurrence.
    #[test]
    fn document_highlights_find_node_occurrences() {
        let source = concat!(
            "# Home\n\n",
            "[Greeting] and [Greeting].\n\n",
            "# Other\n\n",
            "[Greeting]\n\n",
            "# Greeting",
        );
        let uri = untitled_uri();
        let from_title =
            document_highlight_for_document(&snapshot(&uri, source), Position::new(8, 3)).unwrap();
        let from_link =
            document_highlight_for_document(&snapshot(&uri, source), Position::new(2, 3)).unwrap();
        let expected = vec![
            DocumentHighlight {
                range: Range::new(Position::new(2, 0), Position::new(2, 10)),
                kind: Some(DocumentHighlightKind::READ),
            },
            DocumentHighlight {
                range: Range::new(Position::new(2, 15), Position::new(2, 25)),
                kind: Some(DocumentHighlightKind::READ),
            },
            DocumentHighlight {
                range: Range::new(Position::new(6, 0), Position::new(6, 10)),
                kind: Some(DocumentHighlightKind::READ),
            },
            DocumentHighlight {
                range: Range::new(Position::new(8, 2), Position::new(8, 10)),
                kind: Some(DocumentHighlightKind::WRITE),
            },
        ];

        assert_eq!(from_title, expected);
        assert_eq!(from_link, expected);
    }

    // Highlight an unreferenced declaration while ignoring a cursor outside node occurrences.
    #[test]
    fn document_highlights_distinguish_unreferenced_nodes() {
        let source = "# Home";
        let uri = untitled_uri();

        assert_eq!(
            document_highlight_for_document(&snapshot(&uri, source), Position::new(0, 3)),
            Some(vec![DocumentHighlight {
                range: Range::new(Position::new(0, 2), Position::new(0, 6)),
                kind: Some(DocumentHighlightKind::WRITE),
            }]),
        );
        assert!(
            document_highlight_for_document(&snapshot(&uri, source), Position::new(0, 0)).is_none(),
        );
    }

    // Highlight nothing for a text link whose target doesn't exist.
    #[test]
    fn document_highlights_ignore_unresolved_text_links() {
        let source = "# Home\n\n[Missing]";

        assert!(
            document_highlight_for_document(
                &snapshot(&untitled_uri(), source),
                Position::new(2, 3),
            )
            .is_none(),
        );
    }

    // Highlight matching filesystem links without conflating file and directory references.
    #[test]
    fn document_highlights_find_filesystem_links() {
        let source = concat!(
            "# Home\n\n",
            "[/foo] [/foo] [/bar/]\n\n",
            "# Other\n\n",
            "[/bar/]",
        );
        let uri = untitled_uri();
        let file_highlights =
            document_highlight_for_document(&snapshot(&uri, source), Position::new(2, 3)).unwrap();
        let directory_highlights =
            document_highlight_for_document(&snapshot(&uri, source), Position::new(2, 15)).unwrap();

        assert_eq!(
            file_highlights,
            vec![
                DocumentHighlight {
                    range: Range::new(Position::new(2, 0), Position::new(2, 6)),
                    kind: Some(DocumentHighlightKind::READ),
                },
                DocumentHighlight {
                    range: Range::new(Position::new(2, 7), Position::new(2, 13)),
                    kind: Some(DocumentHighlightKind::READ),
                },
            ],
        );
        assert_eq!(
            directory_highlights,
            vec![
                DocumentHighlight {
                    range: Range::new(Position::new(2, 14), Position::new(2, 21)),
                    kind: Some(DocumentHighlightKind::READ),
                },
                DocumentHighlight {
                    range: Range::new(Position::new(6, 0), Position::new(6, 7)),
                    kind: Some(DocumentHighlightKind::READ),
                },
            ],
        );
    }

    // Prepare rename from either a declaration or text link without selecting its delimiters.
    #[test]
    fn rename_preparation_selects_title_text() {
        let source = "# Home\n\n[Greeting]\n\n# Greeting";
        let from_title = prepare_rename_for_document(
            &snapshot(&untitled_uri(), source),
            Position::new(4, 3),
            false,
        )
        .unwrap()
        .unwrap();
        let from_link = prepare_rename_for_document(
            &snapshot(&untitled_uri(), source),
            Position::new(2, 4),
            false,
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            from_title,
            PrepareRenameResponse::RangeWithPlaceholder {
                range: Range::new(Position::new(4, 2), Position::new(4, 10)),
                placeholder: "Greeting".to_owned(),
            },
        );
        assert_eq!(
            from_link,
            PrepareRenameResponse::RangeWithPlaceholder {
                range: Range::new(Position::new(2, 1), Position::new(2, 9)),
                placeholder: "Greeting".to_owned(),
            },
        );
    }

    // Rename in a wiki with syntax errors, leaving malformed links unchanged.
    #[test]
    fn rename_recovers_from_syntax_errors() {
        let source = "# Home\n\n[Greeting] [Gree[ting]\n\n# Greeting";

        assert_eq!(
            prepare_rename_for_document(
                &snapshot(&untitled_uri(), source),
                Position::new(4, 3),
                false,
            ),
            Ok(Some(PrepareRenameResponse::RangeWithPlaceholder {
                range: Range::new(Position::new(4, 2), Position::new(4, 10)),
                placeholder: "Greeting".to_owned(),
            })),
        );
        let workspace_edit = rename_for_document(
            &snapshot(&untitled_uri(), source),
            Position::new(4, 3),
            "Salutation",
            ALL_FILE_OPERATIONS,
        )
        .unwrap()
        .unwrap();
        let edits = &workspace_edit.changes.unwrap()[&untitled_uri()];

        assert_eq!(
            edits
                .iter()
                .map(|edit| edit.range.start)
                .collect::<Vec<_>>(),
            vec![Position::new(2, 1), Position::new(4, 2)],
        );
    }

    // Rename a declaration and every text link while trimming the requested title.
    #[test]
    fn rename_updates_every_occurrence() {
        let source = "# Home\n\n[Greeting] and [Greeting]\n\n# Greeting";
        let workspace_edit = rename_for_document(
            &snapshot(&untitled_uri(), source),
            Position::new(4, 3),
            "  Salutation\t",
            ALL_FILE_OPERATIONS,
        )
        .unwrap()
        .unwrap();
        let edits = &workspace_edit.changes.unwrap()[&untitled_uri()];

        assert_eq!(edits.len(), 3);
        assert_eq!(edits[0].new_text, "Salutation");
        assert_eq!(edits[0].range.start, Position::new(2, 1));
        assert_eq!(edits[1].new_text, "Salutation");
        assert_eq!(edits[1].range.start, Position::new(2, 16));
        assert_eq!(edits[2].new_text, "Salutation");
        assert_eq!(edits[2].range.start, Position::new(4, 2));
    }

    // Preserve renamed titles containing delimiters by escaping only their link occurrences.
    #[test]
    fn rename_escapes_link_delimiters() {
        let source = "# Home\n\n[Greeting]\n\n# Greeting";
        let workspace_edit = rename_for_document(
            &snapshot(&untitled_uri(), source),
            Position::new(2, 4),
            "A[B]",
            ALL_FILE_OPERATIONS,
        )
        .unwrap()
        .unwrap();
        let edits = &workspace_edit.changes.unwrap()[&untitled_uri()];

        assert_eq!(edits[0].new_text, "A\\[B\\]");
        assert_eq!(edits[1].new_text, "A[B]");
    }

    // Allow renaming the structural home node even though validation will report its absence.
    #[test]
    fn rename_allows_home() {
        let source = "# Home";
        let workspace_edit = rename_for_document(
            &snapshot(&untitled_uri(), source),
            Position::new(0, 3),
            "Start",
            ALL_FILE_OPERATIONS,
        )
        .unwrap()
        .unwrap();
        let edits = &workspace_edit.changes.unwrap()[&untitled_uri()];

        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "Start");
    }

    // Reject only syntactically unusable or duplicate node titles during rename.
    #[test]
    fn rename_rejects_invalid_titles() {
        let source = "# Home\n\n[Greeting]\n\n# Greeting";
        let cursor = Position::new(4, 3);

        assert_eq!(
            rename_for_document(
                &snapshot(&untitled_uri(), source),
                cursor,
                " \t",
                ALL_FILE_OPERATIONS,
            )
            .unwrap_err(),
            "A node title can't be empty.",
        );
        assert_eq!(
            rename_for_document(
                &snapshot(&untitled_uri(), source),
                cursor,
                "Hello\nworld",
                ALL_FILE_OPERATIONS,
            )
            .unwrap_err(),
            "A node title can't contain a line break.",
        );
        assert_eq!(
            rename_for_document(
                &snapshot(&untitled_uri(), source),
                cursor,
                "Home",
                ALL_FILE_OPERATIONS,
            )
            .unwrap_err(),
            "Node `Home` already exists.",
        );
        assert_eq!(
            rename_for_document(
                &snapshot(&untitled_uri(), source),
                cursor,
                "/notes.txt",
                ALL_FILE_OPERATIONS,
            )
            .unwrap_err(),
            "A node title can't start with `/`.",
        );
    }

    // Grant every file operation a filesystem rename can use.
    const ALL_FILE_OPERATIONS: FileOperationSupport = FileOperationSupport {
        rename: true,
        delete: true,
    };

    // Apply a filesystem rename's text edits to a source, and return the result with the node
    // rename's old and new URIs and the URIs of the directories it deletes.
    fn apply_filesystem_rename(
        source: &str,
        workspace_edit: WorkspaceEdit,
    ) -> (String, Uri, Uri, Vec<Uri>) {
        let Some(DocumentChanges::Operations(operations)) = workspace_edit.document_changes else {
            panic!("A filesystem rename should consist of document change operations.");
        };
        let [
            DocumentChangeOperation::Edit(text_document_edit),
            DocumentChangeOperation::Op(ResourceOp::Rename(rename)),
            deletions @ ..,
        ] = operations.as_slice()
        else {
            panic!("A filesystem rename should edit the wiki and then rename one node.");
        };
        assert_eq!(text_document_edit.text_document.version, Some(TEST_VERSION));

        // Apply the edits from the end so earlier ranges stay valid.
        let mut applied = source.to_owned();
        for edit in text_document_edit.edits.iter().rev() {
            let OneOf::Left(edit) = edit else {
                panic!("A filesystem rename shouldn't annotate its edits.");
            };
            let start = byte_offset(source, edit.range.start).unwrap();
            let end = byte_offset(source, edit.range.end).unwrap();
            applied.replace_range(start..end, &edit.new_text);
        }

        // Require any remaining operations to delete directories along with the empty ones inside.
        let deleted_uris = deletions
            .iter()
            .map(|operation| {
                let DocumentChangeOperation::Op(ResourceOp::Delete(deletion)) = operation else {
                    panic!(
                        "A filesystem rename should only delete directories after renaming a node.",
                    );
                };
                assert_eq!(
                    deletion
                        .options
                        .as_ref()
                        .and_then(|options| options.recursive),
                    Some(true),
                );
                deletion.uri.clone()
            })
            .collect();
        (
            applied,
            rename.old_uri.clone(),
            rename.new_uri.clone(),
            deleted_uris,
        )
    }

    // Rename whichever kind of node the link at the cursor targets.
    #[test]
    fn rename_dispatches_by_node_kind() {
        let source = "# Home\n\n[Home] [/notes.txt]";
        let wiki = TestWiki::new(source);
        fs::write(wiki.path().parent().unwrap().join("notes.txt"), "notes").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let rename = |character, new_name| {
            rename_for_document(
                &snapshot(&uri, source),
                Position::new(2, character),
                new_name,
                ALL_FILE_OPERATIONS,
            )
            .unwrap()
        };

        // A text node is renamed with plain text edits, and a filesystem node with file operations.
        let text_node_edit = rename(2, "Start").unwrap();
        assert!(text_node_edit.changes.is_some() && text_node_edit.document_changes.is_none());
        let filesystem_node_edit = rename(10, "renamed.txt").unwrap();
        assert!(
            filesystem_node_edit.changes.is_none()
                && filesystem_node_edit.document_changes.is_some(),
        );

        // Other positions have nothing to rename.
        assert!(rename(6, "Start").is_none());
    }

    // Prepare to rename a filesystem link by selecting its decoded path as written, without its
    // leading `/` or a directory's trailing `/`.
    #[test]
    fn rename_preparation_selects_filesystem_paths() {
        let source = "# Home\n\n[ /a\\[1\\].txt ] [/images/raw/]";
        let wiki = TestWiki::new(source);
        let directory = wiki.path().parent().unwrap();
        fs::write(directory.join("a[1].txt"), "a").unwrap();
        fs::create_dir_all(directory.join("images/raw")).unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let prepare = |character| {
            prepare_rename_for_document(&snapshot(&uri, source), Position::new(2, character), true)
                .unwrap()
                .unwrap()
        };

        assert_eq!(
            prepare(4),
            PrepareRenameResponse::RangeWithPlaceholder {
                range: Range::new(Position::new(2, 3), Position::new(2, 13)),
                placeholder: "a[1].txt".to_owned(),
            },
        );
        assert_eq!(
            prepare(18),
            PrepareRenameResponse::RangeWithPlaceholder {
                range: Range::new(Position::new(2, 18), Position::new(2, 28)),
                placeholder: "images/raw".to_owned(),
            },
        );
    }

    // Explain why a filesystem node can't be renamed before asking for a new name.
    #[test]
    fn rename_preparation_rejects_unrenamable_entries() {
        let source = "# Home\n\n[/notes.txt] [/missing.txt] [/] [/wiki.mull]";
        let wiki = TestWiki::new(source);
        fs::write(wiki.path().parent().unwrap().join("notes.txt"), "notes").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let prepare = |uri: &Uri, character, supports_file_renames| {
            prepare_rename_for_document(
                &snapshot(uri, source),
                Position::new(2, character),
                supports_file_renames,
            )
            .unwrap_err()
        };

        assert_eq!(
            prepare(&untitled_uri(), 2, true),
            "Save the wiki before renaming the files it links to.",
        );
        assert_eq!(
            prepare(&uri, 2, false),
            "This editor doesn't support renaming files.",
        );
        assert_eq!(prepare(&uri, 14, true), "File `missing.txt` not found.");
        assert_eq!(
            prepare(&uri, 29, true),
            "The wiki directory can't be renamed.",
        );
        assert_eq!(
            prepare(&uri, 33, true),
            "The wiki can't be renamed through one of its own links.",
        );
    }

    // Rename a linked file and update each link to it in the style it was written.
    #[test]
    fn rename_moves_linked_files() {
        let source = "# Home\n\n[/notes.txt] [/notes.txt] [/notes/]";
        let wiki = TestWiki::new(source);
        let directory = wiki.path().parent().unwrap();
        fs::write(directory.join("notes.txt"), "notes").unwrap();
        fs::create_dir(directory.join("notes")).unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        let workspace_edit = rename_for_document(
            &snapshot(&uri, source),
            Position::new(2, 4),
            " notes/a[1].txt ",
            ALL_FILE_OPERATIONS,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            apply_filesystem_rename(source, workspace_edit),
            (
                "# Home\n\n[/notes/a\\[1\\].txt] [/notes/a\\[1\\].txt] [/notes/]".to_owned(),
                Uri::from_file_path(directory.join("notes.txt")).unwrap(),
                Uri::from_file_path(directory.join("notes/a[1].txt")).unwrap(),
                Vec::new(),
            ),
        );
    }

    // Rename a linked directory and update links to it and to everything within it.
    #[test]
    fn rename_moves_linked_directories() {
        let source = "# Home\n\n[/images/] [/images/raw/] [/images/photo.jpg] [/images.txt]";
        let wiki = TestWiki::new(source);
        let directory = wiki.path().parent().unwrap();
        fs::create_dir_all(directory.join("images/raw")).unwrap();
        fs::write(directory.join("images/photo.jpg"), "photo").unwrap();
        fs::write(directory.join("images.txt"), "images").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        let workspace_edit = rename_for_document(
            &snapshot(&uri, source),
            Position::new(2, 4),
            "/photos",
            ALL_FILE_OPERATIONS,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            apply_filesystem_rename(source, workspace_edit),
            (
                "# Home\n\n[/photos/] [/photos/raw/] [/photos/photo.jpg] [/images.txt]".to_owned(),
                Uri::from_file_path(directory.join("images")).unwrap(),
                Uri::from_file_path(directory.join("photos")).unwrap(),
                Vec::new(),
            ),
        );
    }

    // Treat renaming a filesystem node to its own path, however it's written, as a no-op.
    #[test]
    fn rename_to_same_path_does_nothing() {
        let source = "# Home\n\n[/notes.txt]";
        let wiki = TestWiki::new(source);
        fs::write(wiki.path().parent().unwrap().join("notes.txt"), "notes").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        assert_eq!(
            rename_for_document(
                &snapshot(&uri, source),
                Position::new(2, 3),
                "/notes.txt",
                ALL_FILE_OPERATIONS,
            )
            .unwrap(),
            Some(WorkspaceEdit::default()),
        );
    }

    // Leave missing directories to the client, and delete the outermost directory which contained
    // nothing but the renamed node, keeping any which will contain the new path.
    #[test]
    fn rename_creates_and_deletes_directories() {
        let source = "# Home\n\n[/a/b/photo.jpg] [/c/d/e.txt] [/f/] [/f/g/h.txt]";
        let wiki = TestWiki::new(source);
        let directory = wiki.path().parent().unwrap();
        fs::create_dir_all(directory.join("a/b")).unwrap();
        fs::write(directory.join("a/b/photo.jpg"), "photo").unwrap();
        fs::create_dir_all(directory.join("c/d")).unwrap();
        fs::write(directory.join("c/d/e.txt"), "e").unwrap();
        fs::write(directory.join("c/sibling.txt"), "sibling").unwrap();
        fs::create_dir_all(directory.join("f/g")).unwrap();
        fs::write(directory.join("f/g/h.txt"), "h").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let deleted_uris = |character, new_name, file_operation_support| {
            let workspace_edit = rename_for_document(
                &snapshot(&uri, source),
                Position::new(2, character),
                new_name,
                file_operation_support,
            )
            .unwrap()
            .unwrap();
            apply_filesystem_rename(source, workspace_edit).3
        };
        let directory_uri = |path| Uri::from_file_path(directory.join(path)).unwrap();

        // Delete the outermost directory left empty with a single operation, even when creating
        // new directories, since clients may validate every deletion before performing any.
        assert_eq!(
            deleted_uris(2, "new/photos/photo.jpg", ALL_FILE_OPERATIONS),
            vec![directory_uri("a")],
        );

        // Keep a directory which still contains another entry or will contain the new path.
        assert_eq!(
            deleted_uris(22, "e.txt", ALL_FILE_OPERATIONS),
            vec![directory_uri("c/d")],
        );
        assert_eq!(
            deleted_uris(2, "a/photo.jpg", ALL_FILE_OPERATIONS),
            vec![directory_uri("a/b")],
        );

        // Delete a directory even if a link names it, since the link only stands for its files.
        assert_eq!(
            deleted_uris(46, "h.txt", ALL_FILE_OPERATIONS),
            vec![directory_uri("f")],
        );

        // Skip deletions when the client can't perform them.
        assert_eq!(
            deleted_uris(
                2,
                "photo.jpg",
                FileOperationSupport {
                    rename: true,
                    delete: false,
                },
            ),
            Vec::<Uri>::new(),
        );
    }

    // Keep a directory that a new path leads into through a symlink, rather than deleting the node
    // just moved into it, and refuse to move a directory into itself through a symlink.
    #[cfg(unix)]
    #[test]
    fn rename_through_symlinks() {
        use std::os::unix::fs::symlink;

        let source = "# Home\n\n[/d/x.txt] [/d/] [/alias/]";
        let wiki = TestWiki::new(source);
        let directory = wiki.path().parent().unwrap();
        fs::create_dir(directory.join("d")).unwrap();
        fs::write(directory.join("d/x.txt"), "x").unwrap();
        symlink("d", directory.join("alias")).unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let rename = |character, new_name| {
            rename_for_document(
                &snapshot(&uri, source),
                Position::new(2, character),
                new_name,
                ALL_FILE_OPERATIONS,
            )
        };

        let (applied, _old_uri, _new_uri, deleted_uris) =
            apply_filesystem_rename(source, rename(2, "alias/y.txt").unwrap().unwrap());
        assert_eq!(applied, "# Home\n\n[/alias/y.txt] [/d/] [/alias/]");
        assert_eq!(deleted_uris, Vec::<Uri>::new());
        assert_eq!(
            rename(13, "alias/sub").unwrap_err(),
            "`d` can't be moved into itself through a symlink.",
        );
    }

    // Move a directory into itself through an unused temporary sibling, with the text edit between
    // the two renames, and update links to the directory and to everything within it.
    #[test]
    fn rename_moves_directories_into_themselves() {
        let source = "# Home\n\n[/images/] [/images/photo.jpg] [/notes.txt]";
        let wiki = TestWiki::new(source);
        let directory = wiki.path().parent().unwrap();
        fs::create_dir_all(directory.join("images/raw")).unwrap();
        fs::write(directory.join("images/photo.jpg"), "photo").unwrap();
        fs::write(directory.join("notes.txt"), "notes").unwrap();
        fs::create_dir(directory.join(".images.mull-rename")).unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        let workspace_edit = rename_for_document(
            &snapshot(&uri, source),
            Position::new(2, 2),
            "images/raw",
            ALL_FILE_OPERATIONS,
        )
        .unwrap()
        .unwrap();
        let Some(DocumentChanges::Operations(operations)) = workspace_edit.document_changes else {
            panic!("A filesystem rename should consist of document change operations.");
        };
        let [
            DocumentChangeOperation::Op(ResourceOp::Rename(to_temporary)),
            DocumentChangeOperation::Edit(text_document_edit),
            DocumentChangeOperation::Op(ResourceOp::Rename(from_temporary)),
        ] = operations.as_slice()
        else {
            panic!("The text edit should separate a rename to a temporary path from a rename out.");
        };

        // Skip the temporary name that's already taken.
        let temporary_uri = Uri::from_file_path(directory.join(".images.mull-rename-2")).unwrap();
        assert_eq!(
            (&to_temporary.old_uri, &to_temporary.new_uri),
            (
                &Uri::from_file_path(directory.join("images")).unwrap(),
                &temporary_uri,
            ),
        );
        assert_eq!(
            (&from_temporary.old_uri, &from_temporary.new_uri),
            (
                &temporary_uri,
                &Uri::from_file_path(directory.join("images/raw")).unwrap(),
            ),
        );

        // Links to the directory and to its contents follow it, and other links are unchanged.
        let mut applied = source.to_owned();
        for edit in text_document_edit.edits.iter().rev() {
            let OneOf::Left(edit) = edit else {
                panic!("A filesystem rename shouldn't annotate its edits.");
            };
            let start = byte_offset(source, edit.range.start).unwrap();
            let end = byte_offset(source, edit.range.end).unwrap();
            applied.replace_range(start..end, &edit.new_text);
        }
        assert_eq!(
            applied,
            "# Home\n\n[/images/raw/] [/images/raw/photo.jpg] [/notes.txt]",
        );
    }

    // Reject filesystem renames which the parser, the filesystem, or the editor can't support.
    #[test]
    fn rename_rejects_invalid_filesystem_renames() {
        let source =
            "# Home\n\n[/notes.txt] [/images/] [/missing.txt] [/] [/wiki.mull] [/NOTES.txt]";
        let wiki = TestWiki::new(source);
        let directory = wiki.path().parent().unwrap();
        fs::write(directory.join("notes.txt"), "notes").unwrap();
        fs::write(directory.join("other.txt"), "other").unwrap();
        fs::create_dir(directory.join("images")).unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let rename = |uri: &Uri, character, new_name, file_operation_support| {
            rename_for_document(
                &snapshot(uri, source),
                Position::new(2, character),
                new_name,
                file_operation_support,
            )
            .unwrap_err()
        };

        assert_eq!(
            rename(&untitled_uri(), 2, "other.txt", ALL_FILE_OPERATIONS),
            "Save the wiki before renaming the files it links to.",
        );
        assert_eq!(
            rename(
                &uri,
                2,
                "renamed.txt",
                FileOperationSupport {
                    rename: false,
                    delete: true,
                },
            ),
            "This editor doesn't support renaming files.",
        );
        assert_eq!(
            rename(&uri, 2, "../notes.txt", ALL_FILE_OPERATIONS),
            "Path `../notes.txt` must not contain `..`.",
        );
        assert_eq!(
            rename(&uri, 2, "/", ALL_FILE_OPERATIONS),
            "A file or directory can't be renamed to the wiki directory.",
        );
        assert_eq!(
            rename(&uri, 2, "other.txt", ALL_FILE_OPERATIONS),
            "`other.txt` already exists.",
        );
        assert_eq!(
            rename(&uri, 2, "notes.txt/inner.txt", ALL_FILE_OPERATIONS),
            "Path `notes.txt` isn't a directory.",
        );
        assert_eq!(
            rename(&uri, 25, "found.txt", ALL_FILE_OPERATIONS),
            "File `missing.txt` not found.",
        );
        assert_eq!(
            rename(&uri, 40, "elsewhere", ALL_FILE_OPERATIONS),
            "The wiki directory can't be renamed.",
        );
        assert_eq!(
            rename(&uri, 44, "renamed.mull", ALL_FILE_OPERATIONS),
            "The wiki can't be renamed through one of its own links.",
        );

        // Reject a link or a new path through an existing directory spelled differently than on
        // disk, which only a filesystem that ignores case finds.
        if fs::metadata(directory.join("IMAGES")).is_ok() {
            assert_eq!(
                rename(&uri, 58, "renamed.txt", ALL_FILE_OPERATIONS),
                "`NOTES.txt` doesn't match the spelling of any name on disk.",
            );
            assert_eq!(
                rename(&uri, 2, "IMAGES/notes.txt", ALL_FILE_OPERATIONS),
                "`IMAGES` doesn't match the spelling of any name on disk.",
            );
            assert_eq!(
                rename(&uri, 15, "IMAGES/raw", ALL_FILE_OPERATIONS),
                "`IMAGES` doesn't match the spelling of any name on disk.",
            );

            // Reject a wiki whose path the editor spells differently than on disk.
            assert_eq!(
                rename(
                    &Uri::from_file_path(directory.join("WIKI.mull")).unwrap(),
                    44,
                    "renamed.mull",
                    ALL_FILE_OPERATIONS,
                ),
                "`WIKI.mull` doesn't match the spelling of any name on disk.",
            );

            // Reject a rename which only changes the case of a name, which VS Code would skip.
            assert_eq!(
                rename(&uri, 2, "NOTES.txt", ALL_FILE_OPERATIONS),
                "`notes.txt` and `NOTES.txt` differ only in case, which VS Code can't rename. \
                    Rename it in the explorer instead, then fix its links.",
            );
            assert_eq!(
                rename(&uri, 15, "Images", ALL_FILE_OPERATIONS),
                "`images` and `Images` differ only in case, which VS Code can't rename. Rename \
                    it in the explorer instead, then fix its links.",
            );
        }
    }

    // Keep filesystem links clickable in a wiki with syntax errors.
    #[test]
    fn document_links_recover_from_syntax_errors() {
        let source = "# Home\n\n[/notes.txt] Unexpected]";
        let wiki = TestWiki::new(source);
        fs::write(wiki.path().parent().unwrap().join("notes.txt"), "notes").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        assert_eq!(
            document_link_for_document(&snapshot(&uri, source))
                .unwrap()
                .len(),
            1,
        );
    }

    // Link files to themselves and directories to a command that reveals them, skipping links
    // whose targets are missing, of the wrong kind, or spelled differently than on disk.
    #[test]
    fn document_links_open_existing_targets() {
        let source =
            "# Home\n\n[Home] [/missing.txt] [/notes.txt]\n[/notes.txt/] [/images/] [/NOTES.txt]";
        let wiki = TestWiki::new(source);
        let directory = wiki.path().parent().unwrap();
        fs::write(directory.join("notes.txt"), "notes").unwrap();
        fs::create_dir(directory.join("images")).unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let links = document_link_for_document(&snapshot(&uri, source)).unwrap();

        // Expect the file link and then the directory link.
        let [file_link, directory_link] = links.as_slice() else {
            panic!("Only the two links with existing targets should be clickable.");
        };
        assert_eq!(
            file_link.range,
            Range::new(Position::new(2, 22), Position::new(2, 34)),
        );
        assert_eq!(
            file_link.target,
            Some(Uri::from_file_path(directory.join("notes.txt")).unwrap()),
        );
        assert_eq!(
            directory_link.range,
            Range::new(Position::new(3, 14), Position::new(3, 24)),
        );
        let directory_uri = Uri::from_file_path(directory.join("images")).unwrap();
        assert_eq!(
            directory_link.target.as_ref().map(|target| target.as_str()),
            Some(
                format!(
                    "command:mull.revealInExplorer?{}",
                    utf8_percent_encode(
                        &format!("[\"{}\"]", directory_uri.as_str()),
                        NON_ALPHANUMERIC,
                    ),
                )
                .as_str(),
            ),
        );

        // Decline to resolve links relative to an unsaved wiki.
        assert!(document_link_for_document(&snapshot(&untitled_uri(), source)).is_none());
    }

    // Apply the single text edit of a code action's workspace edit to a source, then mark the
    // cursor position that its command reveals with `|`.
    fn apply_code_action(uri: &Uri, source: &str, action: &CodeActionOrCommand) -> String {
        // Apply the edit.
        let CodeActionOrCommand::CodeAction(action) = action else {
            panic!("A code action shouldn't be a bare command.");
        };
        let changes = action.edit.as_ref().unwrap().changes.as_ref().unwrap();
        let [edit] = changes[uri].as_slice() else {
            panic!("A code action should make exactly one edit.");
        };
        let start = byte_offset(source, edit.range.start).unwrap();
        let end = byte_offset(source, edit.range.end).unwrap();
        let mut applied = source.to_owned();
        applied.replace_range(start..end, &edit.new_text);

        // Mark the empty range that the command reveals in the edited document.
        let command = action.command.as_ref().unwrap();
        assert_eq!(command.command, "mull.revealRange");
        let arguments = command.arguments.as_ref().unwrap();
        assert_eq!(arguments[0], uri.as_str());
        assert_eq!(arguments[1..3], arguments[3..5]);
        let cursor = Position::new(
            u32::try_from(arguments[1].as_u64().unwrap()).unwrap(),
            u32::try_from(arguments[2].as_u64().unwrap()).unwrap(),
        );
        applied.insert(byte_offset(&applied, cursor).unwrap(), '|');
        applied
    }

    // Create the missing destination of text links, resolving every diagnostic the node would fix
    // and no others.
    #[test]
    fn code_actions_create_missing_nodes() {
        let source = "# Home\n\n[Greeting] [Greeting]";
        let uri = untitled_uri();
        let link_diagnostics = diagnostics(&uri, source);
        assert_eq!(link_diagnostics.len(), 2);
        let unrelated_diagnostic = Diagnostic {
            data: None,
            message: "Something else is wrong with this link.".to_owned(),
            ..link_diagnostics[0].clone()
        };

        let mut request_diagnostics = link_diagnostics.clone();
        request_diagnostics.push(unrelated_diagnostic);
        let actions =
            code_action_for_document(&snapshot(&uri, source), &request_diagnostics).unwrap();
        let [action] = actions.as_slice() else {
            panic!("A missing destination should have exactly one code action.");
        };
        let CodeActionOrCommand::CodeAction(code_action) = action else {
            panic!("A code action shouldn't be a bare command.");
        };
        assert_eq!(code_action.title, "Create node `Greeting`");
        assert_eq!(code_action.kind, Some(CodeActionKind::QUICKFIX));
        assert_eq!(code_action.diagnostics, Some(link_diagnostics));
        assert_eq!(code_action.is_preferred, Some(true));

        // Confirm that the created node makes the wiki valid.
        let applied = apply_code_action(&uri, source, action);
        assert_eq!(applied, "# Home\n\n[Greeting] [Greeting]\n\n# Greeting|");
        assert_eq!(
            diagnostics(&uri, &applied.replace('|', "")),
            Vec::<Diagnostic>::new(),
        );
    }

    // Offer to create a missing node despite a syntax error elsewhere in the wiki.
    #[test]
    fn code_actions_recover_from_syntax_errors() {
        let source = "# Home\n\n[Greeting] Unexpected]";
        let uri = untitled_uri();
        let actions =
            code_action_for_document(&snapshot(&uri, source), &diagnostics(&uri, source)).unwrap();
        let [action] = actions.as_slice() else {
            panic!("A missing destination should have exactly one code action.");
        };

        assert_eq!(
            apply_code_action(&uri, source, action),
            "# Home\n\n[Greeting] Unexpected]\n\n# Greeting|",
        );
    }

    // Place the cursor after a created title containing characters outside the Basic Multilingual
    // Plane, whose editor columns are counted in UTF-16 code units.
    #[test]
    fn code_actions_reveal_unicode_titles() {
        let source = "# Home\n\n[Grüße 😀]";
        let uri = untitled_uri();
        let actions =
            code_action_for_document(&snapshot(&uri, source), &diagnostics(&uri, source)).unwrap();

        assert_eq!(
            apply_code_action(&uri, source, &actions[0]),
            "# Home\n\n[Grüße 😀]\n\n# Grüße 😀|",
        );
    }

    // Insert the created node right after the first node linking to it, between blank lines.
    #[test]
    fn code_actions_insert_after_linking_nodes() {
        let source = "# Home\n\n[Other]\n\n# Other\n\n[Greeting]\n\n# Last\n\n[Greeting]\n";
        let uri = untitled_uri();
        let actions =
            code_action_for_document(&snapshot(&uri, source), &diagnostics(&uri, source)).unwrap();

        assert_eq!(
            apply_code_action(&uri, source, &actions[0]),
            "# Home\n\n[Other]\n\n# Other\n\n[Greeting]\n\n# Greeting|\n\n# Last\n\n[Greeting]\n",
        );
    }

    // Insert the created node after the last node that a diagnostic touches, even if the diagnostic
    // spans several nodes or ends between them.
    #[test]
    fn code_actions_insert_after_last_touched_nodes() {
        let source = "# Home\n\n[First]\n\n# First\n\nText.\n\n# Second\n\nText.\n";
        let uri = untitled_uri();
        let fix_diagnostic = |start, end| Diagnostic {
            range: Range::new(start, end),
            data: Some(serde_json::to_value(Fix::CreateNode("New".to_owned())).unwrap()),
            ..Diagnostic::default()
        };
        let apply = |diagnostic| {
            let actions = code_action_for_document(&snapshot(&uri, source), &[diagnostic]).unwrap();
            apply_code_action(&uri, source, &actions[0])
        };

        // Insert after the second node when the diagnostic spans into it.
        assert_eq!(
            apply(fix_diagnostic(Position::new(4, 0), Position::new(8, 3))),
            "# Home\n\n[First]\n\n# First\n\nText.\n\n# Second\n\nText.\n\n# New|\n",
        );

        // Insert after the first node when the diagnostic ends in the blank line after it.
        assert_eq!(
            apply(fix_diagnostic(Position::new(4, 0), Position::new(7, 0))),
            "# Home\n\n[First]\n\n# First\n\nText.\n\n# New|\n\n# Second\n\nText.\n",
        );
    }

    // Append the created node when a stale diagnostic's position is no longer within any node.
    #[test]
    fn code_actions_append_without_linking_nodes() {
        let uri = untitled_uri();
        let stale_diagnostics = diagnostics(&uri, "# Home\n\nSome text first, then [Greeting]\n");
        let source = "# Home\n";

        let actions =
            code_action_for_document(&snapshot(&uri, source), &stale_diagnostics).unwrap();
        assert_eq!(
            apply_code_action(&uri, source, &actions[0]),
            "# Home\n\n# Greeting|\n",
        );
    }

    // Create a missing home node at the start of the document, resolving only its diagnostic
    // among those without source ranges, which are all reported there.
    #[test]
    fn code_actions_create_missing_home_nodes() {
        let uri = untitled_uri();
        let home_diagnostic = diagnostics(&uri, "").remove(0);
        let unrelated_diagnostic = Diagnostic {
            data: None,
            message: "File `notes.txt` isn't linked to.".to_owned(),
            ..home_diagnostic.clone()
        };

        let actions = code_action_for_document(
            &snapshot(&uri, ""),
            &[home_diagnostic.clone(), unrelated_diagnostic],
        )
        .unwrap();
        let [action] = actions.as_slice() else {
            panic!("A missing home node should have exactly one code action.");
        };
        let CodeActionOrCommand::CodeAction(code_action) = action else {
            panic!("A code action shouldn't be a bare command.");
        };
        assert_eq!(code_action.title, "Create node `Home`");
        assert_eq!(code_action.diagnostics, Some(vec![home_diagnostic]));
        let applied = apply_code_action(&uri, "", action);
        assert_eq!(applied, "# Home|\n");
        assert_eq!(
            diagnostics(&uri, &applied.replace('|', "")),
            Vec::<Diagnostic>::new(),
        );

        // Separate the home node from the nodes that follow it.
        let source = "# Greeting\n";
        let actions =
            code_action_for_document(&snapshot(&uri, source), &diagnostics(&uri, source)).unwrap();
        assert_eq!(
            apply_code_action(&uri, source, &actions[0]),
            "# Home|\n\n# Greeting\n",
        );
    }

    // Skip a fix from a stale diagnostic whose node now exists.
    #[test]
    fn code_actions_skip_stale_fixes() {
        let uri = untitled_uri();
        let stale_diagnostics = diagnostics(&uri, "");

        assert!(
            code_action_for_document(&snapshot(&uri, "# Home\n"), &stale_diagnostics).is_none(),
        );
    }

    // Offer no fixes for diagnostics that declaring a node wouldn't resolve.
    #[test]
    fn code_actions_ignore_other_diagnostics() {
        let source = "# Home\n\n[Home] [] [/notes.txt] prose";
        let uri = untitled_uri();
        let source_diagnostics = diagnostics(&uri, source);
        assert_ne!(source_diagnostics, Vec::<Diagnostic>::new());

        assert!(code_action_for_document(&snapshot(&uri, source), &source_diagnostics).is_none());
    }

    // Leave filesystem links to ordinary editor and filesystem navigation.
    #[test]
    fn navigation_ignores_filesystem_links() {
        let source = "# Home\n\n[/notes.txt]";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        assert!(
            goto_definition_for_document(&snapshot(&uri, source), Position::new(2, 4)).is_none(),
        );
        assert!(hover_for_document(&snapshot(&uri, source), Position::new(2, 4)).is_none());
        assert!(
            references_for_document(&snapshot(&uri, source), Position::new(2, 4), false).is_none(),
        );
    }

    // Keep navigating a wiki with syntax errors, previewing only the title of a node with them.
    #[test]
    fn navigation_recovers_from_syntax_errors() {
        let source = "# Home\n\n[Greeting]\n\n# Greeting\n\nUnexpected] [Home]";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        let Some(GotoDefinitionResponse::Link(links)) =
            goto_definition_for_document(&snapshot(&uri, source), Position::new(2, 4))
        else {
            panic!("Navigation should produce a location link.");
        };
        assert_eq!(
            links[0].target_selection_range,
            Range::new(Position::new(4, 2), Position::new(4, 10)),
        );
        assert_eq!(
            references_for_document(&snapshot(&uri, source), Position::new(0, 3), false)
                .unwrap()
                .len(),
            1,
        );
        let HoverContents::Markup(contents) =
            hover_for_document(&snapshot(&uri, source), Position::new(2, 4))
                .unwrap()
                .contents
        else {
            panic!("A node preview should use markup content.");
        };
        assert_eq!(contents.value, "# Greeting");
    }

    #[test]
    fn formatting_replaces_noncanonical_source() {
        let source = "# Zulu\n\n😀\n\n# Home\n\n[Zulu]";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let edits = formatting_for_document(&snapshot(&uri, source)).unwrap();

        assert_eq!(edits.len(), 1);
        let edit = &edits[0];
        assert_eq!(
            edit.range,
            Range::new(Position::new(0, 0), Position::new(6, 6)),
        );
        assert_eq!(edit.new_text, "# Home\n\n[Zulu]\n\n# Zulu\n\n😀\n");
    }

    #[test]
    fn formatting_omits_edits_for_canonical_source() {
        let source = "# Home\n";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        assert_eq!(
            formatting_for_document(&snapshot(&uri, source)),
            Some(Vec::new()),
        );
    }

    #[test]
    fn formatting_rejects_unparsable_source() {
        let source = "# Home\n😀 ]";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        assert!(formatting_for_document(&snapshot(&uri, source)).is_none());
    }

    // Refuse to format a wiki whose recovered form would omit some of its source, like a duplicate
    // node.
    #[test]
    fn formatting_rejects_source_it_would_lose() {
        let source = "# Home\n\n# Home\n\nThis would be lost.\n";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        assert!(formatting_for_document(&snapshot(&uri, source)).is_none());
    }

    // Format wikis that parse but fail validation, such as one without a home node.
    #[test]
    fn formatting_supports_invalid_wikis() {
        let source = "# Zulu\n\n# Elsewhere";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let edits = formatting_for_document(&snapshot(&uri, source)).unwrap();

        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "# Elsewhere\n\n# Zulu\n");
    }

    // Format a new editor buffer without requiring a filesystem path.
    #[test]
    fn formatting_supports_untitled_wikis() {
        let source = "# Zulu\n\n# Home\n\n[Zulu]";
        let edits = formatting_for_document(&snapshot(&untitled_uri(), source)).unwrap();

        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "# Home\n\n[Zulu]\n\n# Zulu\n");
    }

    #[test]
    fn formatting_differences_are_not_diagnostics() {
        let source = "# Zulu\n\n# Home\n\n[Zulu]";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        assert_eq!(diagnostics(&uri, source), Vec::<Diagnostic>::new());
    }

    #[test]
    fn source_errors_become_precise_diagnostics() {
        let source = "# Home\n😀 ]";
        let error = parser::parse(Some(Path::new("wiki.mull")), source)
            .unwrap_err()
            .into_iter()
            .next()
            .unwrap();
        let diagnostic = diagnostic_from_error(source, &LineIndex::new(source), &error);

        assert_eq!(
            diagnostic.range,
            Range::new(Position::new(1, 3), Position::new(1, 4)),
        );
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diagnostic.source.as_deref(), Some("mull"));
        assert_eq!(diagnostic.message, "Unexpected closing link delimiter.");
    }

    #[test]
    fn errors_without_ranges_point_to_document_start() {
        let error = crate::error::Error::new("Something went wrong.", None, None, None, None);
        let diagnostic = diagnostic_from_error("# Home\n", &LineIndex::new("# Home\n"), &error);

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
            None,
        );
        let diagnostic = diagnostic_from_error(source, &LineIndex::new(source), &error);

        assert_eq!(
            diagnostic.range,
            Range::new(Position::new(0, 0), Position::new(1, 2)),
        );
    }
}
