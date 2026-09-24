use crate::{
    analyzer::analyze,
    cancellation::{CancellationFlag, Outcome},
    error::{Error, SourceRange},
    format::CodePath,
    parser::{self, normalize_filesystem_path},
    path_util::relative_path,
    wiki::{
        DIRECTORY_LINK_PREFIX, FILE_LINK_PREFIX, HOME_TITLE, Link, TITLE_MARKER, TITLE_PREFIX,
        TextNode, Wiki,
    },
    wiki_tree::wiki_tree_walker,
};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fs,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, atomic::AtomicBool, atomic::Ordering},
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
        DocumentHighlightParams, DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse,
        FileSystemWatcher, GlobPattern, GotoDefinitionParams, GotoDefinitionResponse, Hover,
        HoverContents, HoverParams, HoverProviderCapability, InitializeParams, InitializeResult,
        InitializedParams, Location, LocationLink, MarkupContent, MarkupKind, MessageType, OneOf,
        OptionalVersionedTextDocumentIdentifier, Position, PositionEncodingKind,
        PrepareRenameResponse, Range, ReferenceParams, Registration, RenameFile, RenameOptions,
        RenameParams, ResourceOp, ResourceOperationKind, ServerCapabilities, ServerInfo,
        SymbolInformation, SymbolKind, TextDocumentEdit, TextDocumentPositionParams,
        TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions, TextEdit, Uri,
        WorkDoneProgressOptions, WorkspaceEdit,
    },
};

// Wait briefly after edits so filesystem validation does not run on every keystroke.
const CHECK_DELAY: Duration = Duration::from_millis(250);

// This extension command reveals a source range for clickable text links in hover previews.
// Keep this in sync with [group:reveal_range_command].
const REVEAL_RANGE_COMMAND: &str = "mull.revealRange";

// This editor command reopens suggestions so the children of a completed directory can be chosen.
const TRIGGER_SUGGEST_COMMAND: &str = "editor.action.triggerSuggest";

// This pairs a scheduled diagnostic task with the flag which stops its filesystem work.
#[derive(Debug)]
struct PendingCheck {
    handle: JoinHandle<()>,
    cancellation: CancellationFlag,
}

impl PendingCheck {
    // Stop the check whether or not it has started. Aborting the task stops it if it has not
    // started checking, and setting its flag stops it if it has already started.
    fn cancel(self) {
        self.cancellation.cancel();
        self.handle.abort();
    }
}

// This state associates the latest editor contents with a pending diagnostic update. The client
// assigns the version, which changes only when the contents do and is reported back with
// diagnostics. The server assigns the generation, which increases every time a snapshot is
// stored, including rechecks of unchanged contents after a save, so it identifies which snapshot
// stale work was computed from.
#[derive(Debug)]
struct OpenDocument {
    contents: String,
    version: i32,
    generation: u64,
    pending_check: Option<PendingCheck>,
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

// This records which file operations the client can perform within a versioned workspace edit.
#[derive(Clone, Copy)]
struct FileOperationSupport {
    rename: bool,
    delete: bool,
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
    fn store_and_check_document(&self, uri: Uri, contents: String, version: i32, delay: Duration) {
        // Prepare the resources owned by the diagnostic task.
        let client = self.client.clone();
        let documents = Arc::clone(&self.documents);
        let diagnostic_uri = uri.clone();
        let diagnostic_contents = contents.clone();
        let cancellation = CancellationFlag::default();
        let check_cancellation = cancellation.clone();

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
            pending_check.cancel();
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
        document.pending_check = Some(PendingCheck {
            handle: tokio::spawn(async move {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                let check_uri = diagnostic_uri.clone();
                let fallback_contents = diagnostic_contents.clone();

                // Publish nothing when a newer snapshot cancelled this check partway through.
                let Some(diagnostics) = tokio::task::spawn_blocking(move || {
                    diagnostics_for_document(&check_uri, &diagnostic_contents, &check_cancellation)
                })
                .await
                .unwrap_or_else(|error| {
                    Some(vec![diagnostic(
                        &fallback_contents,
                        None,
                        format!("Mull was unable to check the wiki: {error}."),
                    )])
                }) else {
                    return;
                };
                if documents
                    .lock()
                    .expect("the open-document mutex should not be poisoned")
                    .get(&diagnostic_uri)
                    .is_some_and(|document| document.generation == generation)
                {
                    client
                        .publish_diagnostics(diagnostic_uri, diagnostics, Some(version))
                        .await;
                }
            }),
            cancellation,
        });
    }

    // Recheck the most recent snapshot immediately after it is saved.
    fn recheck_saved_document(&self, uri: Uri, contents: Option<String>) {
        // Copy the snapshot before scheduling, without retaining the lock across that operation.
        let snapshot = self
            .documents
            .lock()
            .expect("the open-document mutex should not be poisoned")
            .get_mut(&uri)
            .map(|document| {
                if let Some(contents) = contents {
                    document.contents = contents;
                }
                (document.contents.clone(), document.version)
            });
        if let Some((contents, version)) = snapshot {
            self.store_and_check_document(uri, contents, version, Duration::ZERO);
        }
    }

    // Recheck open documents after filesystem changes, which their filesystem links may reflect.
    // Documents whose own files changed are skipped, since editor synchronization covers them.
    fn recheck_open_documents(&self, changed_uris: &[&Uri]) {
        // Copy the snapshots before scheduling, without retaining the lock across that operation.
        let snapshots = self
            .documents
            .lock()
            .expect("the open-document mutex should not be poisoned")
            .iter()
            .filter(|(uri, _document)| !changed_uris.contains(uri))
            .map(|(uri, document)| (uri.clone(), document.contents.clone(), document.version))
            .collect::<Vec<_>>();

        // Debounce the checks, since a single operation can change many files in quick succession.
        for (uri, contents, version) in snapshots {
            self.store_and_check_document(uri, contents, version, CHECK_DELAY);
        }
    }

    // Copy the current editor snapshot for a language feature request.
    fn document_contents(&self, uri: &Uri) -> Option<String> {
        self.document_snapshot(uri)
            .map(|(contents, _version)| contents)
    }

    // Read the latest synchronized contents of an open document with its client-assigned version.
    fn document_snapshot(&self, uri: &Uri) -> Option<(String, i32)> {
        self.documents
            .lock()
            .expect("the open-document mutex should not be poisoned")
            .get(uri)
            .map(|document| (document.contents.clone(), document.version))
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
                    trigger_characters: Some(vec!["[".to_owned(), ":".to_owned(), "/".to_owned()]),
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
            params.text_document.uri,
            params.text_document.text,
            params.text_document.version,
            Duration::ZERO,
        );
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // Full synchronization places the complete latest snapshot in the final change.
        if let Some(change) = params.content_changes.into_iter().next_back() {
            self.store_and_check_document(
                params.text_document.uri,
                change.text,
                params.text_document.version,
                CHECK_DELAY,
            );
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        // Recheck the saved document immediately, adopting any contents the client included.
        self.recheck_saved_document(params.text_document.uri, params.text);
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
        let Some(contents) =
            self.document_contents(&params.text_document_position.text_document.uri)
        else {
            return Ok(None);
        };
        Ok(completion_for_document(
            &params.text_document_position.text_document.uri,
            &contents,
            params.text_document_position.position,
        )
        .map(CompletionResponse::Array))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        // Resolve the title or text link against the latest synchronized editor snapshot.
        let Some(contents) =
            self.document_contents(&params.text_document_position_params.text_document.uri)
        else {
            return Ok(None);
        };
        Ok(goto_definition_for_document(
            &params.text_document_position_params.text_document.uri,
            &contents,
            params.text_document_position_params.position,
        ))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        // Preview the text-link target from the latest synchronized editor snapshot.
        let Some(contents) =
            self.document_contents(&params.text_document_position_params.text_document.uri)
        else {
            return Ok(None);
        };
        Ok(hover_for_document(
            &params.text_document_position_params.text_document.uri,
            &contents,
            params.text_document_position_params.position,
        ))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        // Find references to the node under the cursor in the latest synchronized snapshot.
        let Some(contents) =
            self.document_contents(&params.text_document_position.text_document.uri)
        else {
            return Ok(None);
        };
        Ok(references_for_document(
            &params.text_document_position.text_document.uri,
            &contents,
            params.text_document_position.position,
            params.context.include_declaration,
        ))
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        // Highlight the wiki occurrences related to the item under the cursor.
        let Some(contents) =
            self.document_contents(&params.text_document_position_params.text_document.uri)
        else {
            return Ok(None);
        };
        Ok(document_highlight_for_document(
            &params.text_document_position_params.text_document.uri,
            &contents,
            params.text_document_position_params.position,
        ))
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        // Identify the occurrence that the editor should select for rename, or explain why the
        // filesystem node a link targets cannot be renamed before the user enters a new name.
        let Some(contents) = self.document_contents(&params.text_document.uri) else {
            return Ok(None);
        };
        prepare_rename_for_document(
            &params.text_document.uri,
            &contents,
            params.position,
            self.supports_file_renames.load(Ordering::Relaxed),
        )
        .map_err(JsonRpcError::invalid_params)
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        // Rename against the latest snapshot, whose version guards the edit against later changes.
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let Some((contents, version)) = self.document_snapshot(uri) else {
            return Ok(None);
        };

        // Rename the node at the cursor with whichever file operations the client supports.
        rename_for_document(
            uri,
            &contents,
            version,
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
        let Some(contents) = self.document_contents(&params.text_document.uri) else {
            return Ok(None);
        };
        Ok(formatting_for_document(
            &params.text_document.uri,
            &contents,
        ))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        // Describe the nodes in the latest synchronized editor snapshot.
        let Some(contents) = self.document_contents(&params.text_document.uri) else {
            return Ok(None);
        };
        Ok(document_symbol_for_document(
            &params.text_document.uri,
            &contents,
            self.supports_hierarchical_document_symbols
                .load(Ordering::Relaxed),
        ))
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        // Offer fixes for the latest synchronized editor snapshot.
        let Some(contents) = self.document_contents(&params.text_document.uri) else {
            return Ok(None);
        };
        Ok(code_action_for_document(
            &params.text_document.uri,
            &contents,
            params.range,
            &params.context.diagnostics,
        ))
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        // Cancel outstanding work before asking the client to clear this document's diagnostics.
        let document = self
            .documents
            .lock()
            .expect("the open-document mutex should not be poisoned")
            .remove(&params.text_document.uri);
        if let Some(pending_check) = document.and_then(|document| document.pending_check) {
            pending_check.cancel();
        }
        self.client
            .publish_diagnostics(params.text_document.uri, Vec::new(), None)
            .await;
    }
}

// Analyze an editor snapshot without checking its formatting.
fn diagnostics_for_document(
    uri: &Uri,
    source_contents: &str,
    cancellation: &CancellationFlag,
) -> Option<Vec<Diagnostic>> {
    // Report nothing for a cancelled check, whose errors may cover only part of the wiki.
    let Outcome::Completed(result) =
        analyze(local_path(uri).as_deref(), source_contents, cancellation)
    else {
        return None;
    };

    // Preserve independent Mull errors as independent editor diagnostics.
    Some(result.map_or_else(
        |errors| {
            errors
                .iter()
                .map(|error| diagnostic_from_error(source_contents, error))
                .collect()
        },
        |_wiki| Vec::new(),
    ))
}

// Complete the link target at an editor position with node titles or filesystem paths.
fn completion_for_document(
    uri: &Uri,
    source_contents: &str,
    cursor: Position,
) -> Option<Vec<CompletionItem>> {
    // Complete a filesystem link from the directory containing the wiki.
    let cursor_offset = byte_offset(source_contents, cursor)?;
    if let Some(context) = filesystem_link_context(source_contents, cursor_offset) {
        return Some(filesystem_link_completions(
            &local_path(uri)?,
            source_contents,
            &context,
        ));
    }

    // Complete a text link with the titles of the nodes in the wiki.
    let (wiki, replacement_source_range) =
        text_link_context(local_path(uri).as_deref(), source_contents, cursor_offset)?;
    Some(text_link_completions(
        &wiki,
        source_contents,
        replacement_source_range,
    ))
}

// Locate the node declared or linked at an editor position.
fn goto_definition_for_document(
    uri: &Uri,
    source_contents: &str,
    cursor: Position,
) -> Option<GotoDefinitionResponse> {
    // Parse only the wiki syntax because navigation does not require filesystem validation.
    let wiki = parser::parse(local_path(uri).as_deref(), source_contents).ok()?;
    let (node, origin_source_range) = node_at(
        &wiki,
        source_contents,
        byte_offset(source_contents, cursor)?,
        LinkExtent::Whole,
    )?;

    // Identify the complete source link or title and the destination node while selecting its title
    // on arrival.
    Some(GotoDefinitionResponse::Link(vec![LocationLink {
        origin_selection_range: Some(lsp_range(source_contents, origin_source_range)),
        target_uri: uri.clone(),
        target_range: lsp_range(source_contents, node.source_range),
        target_selection_range: lsp_range(source_contents, node.title_source_range),
    }]))
}

// Preview the destination of a text link at an editor position.
fn hover_for_document(uri: &Uri, source_contents: &str, cursor: Position) -> Option<Hover> {
    // Parse only the wiki syntax because hovering does not require filesystem validation.
    let wiki = parser::parse(local_path(uri).as_deref(), source_contents).ok()?;
    let (node, source_range) = node_at(
        &wiki,
        source_contents,
        byte_offset(source_contents, cursor)?,
        LinkExtent::Whole,
    )?;

    // Render the node as Markdown with commands that navigate its resolvable text links.
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: node.to_markdown(|title| {
                reveal_range_command_url(
                    uri,
                    source_contents,
                    wiki.text_nodes.get(title)?.title_source_range,
                )
            }),
        }),
        range: Some(lsp_range(source_contents, source_range)),
    })
}

// Locate every text link to the node at an editor position.
fn references_for_document(
    uri: &Uri,
    source_contents: &str,
    cursor: Position,
    include_declaration: bool,
) -> Option<Vec<Location>> {
    // Parse only the wiki syntax because finding references does not require validation.
    let wiki = parser::parse(local_path(uri).as_deref(), source_contents).ok()?;
    let (node, _source_range) = node_at(
        &wiki,
        source_contents,
        byte_offset(source_contents, cursor)?,
        LinkExtent::Whole,
    )?;

    // Include the declaration only when requested, then restore source order.
    let mut source_ranges = text_link_source_ranges(&wiki, &node.title);
    if include_declaration {
        source_ranges.push(node.title_source_range);
    }
    source_ranges.sort_by_key(|source_range| (source_range.start, source_range.end));

    // Return every occurrence in source order within the current wiki.
    Some(
        source_ranges
            .into_iter()
            .map(|source_range| {
                Location::new(uri.clone(), lsp_range(source_contents, source_range))
            })
            .collect(),
    )
}

// Highlight related node or filesystem-link occurrences at an editor position.
fn document_highlight_for_document(
    uri: &Uri,
    source_contents: &str,
    cursor: Position,
) -> Option<Vec<DocumentHighlight>> {
    // Parse only the wiki syntax because document highlights do not require validation.
    let wiki = parser::parse(local_path(uri).as_deref(), source_contents).ok()?;
    let byte_offset = byte_offset(source_contents, cursor)?;

    // Distinguish a text-node declaration from its references.
    let mut highlights = if let Some((node, _source_range)) =
        node_at(&wiki, source_contents, byte_offset, LinkExtent::Whole)
    {
        let mut highlights = text_link_source_ranges(&wiki, &node.title)
            .into_iter()
            .map(|source_range| (source_range, DocumentHighlightKind::READ))
            .collect::<Vec<_>>();
        highlights.push((node.title_source_range, DocumentHighlightKind::WRITE));
        highlights
    } else {
        // Filesystem links have no declaration in the wiki, so every matching link is a reference.
        // A text link reaches this branch only when its target does not exist, so it has nothing
        // to highlight.
        let link = link_at(&wiki, byte_offset).filter(|link| !matches!(link, Link::Text { .. }))?;
        filesystem_link_source_ranges(&wiki, link)
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
                range: lsp_range(source_contents, source_range),
                kind: Some(kind),
            })
            .collect(),
    )
}

// Identify the source occurrence that should be selected before renaming a node, file, or
// directory, or explain why the filesystem node a link targets cannot be renamed.
fn prepare_rename_for_document(
    uri: &Uri,
    source_contents: &str,
    cursor: Position,
    supports_file_renames: bool,
) -> std::result::Result<Option<PrepareRenameResponse>, String> {
    // Resolve the cursor in a parseable editor snapshot, which need not pass semantic validation.
    let Ok(wiki) = parser::parse(local_path(uri).as_deref(), source_contents) else {
        return Ok(None);
    };
    let Some(cursor_offset) = byte_offset(source_contents, cursor) else {
        return Ok(None);
    };

    // Select only the path of a filesystem link and seed the rename prompt with its decoded text
    // as written.
    if let Some(filesystem_node) = renamable_filesystem_node_at(
        &wiki,
        uri,
        source_contents,
        cursor_offset,
        supports_file_renames,
    )? {
        return Ok(Some(PrepareRenameResponse::RangeWithPlaceholder {
            range: lsp_range(source_contents, filesystem_node.path_source_range),
            placeholder: unescape_link_delimiters(
                &source_contents[filesystem_node.path_source_range.start
                    ..filesystem_node.path_source_range.end],
            ),
        }));
    }

    // Otherwise, resolve either a title declaration or text link.
    let Some((node, source_range)) =
        node_at(&wiki, source_contents, cursor_offset, LinkExtent::Target)
    else {
        return Ok(None);
    };

    // Select only the title text and seed the rename prompt with its decoded value.
    Ok(Some(PrepareRenameResponse::RangeWithPlaceholder {
        range: lsp_range(source_contents, source_range),
        placeholder: node.title.clone(),
    }))
}

// Rename the filesystem node or text node at an editor position, along with every link to it.
fn rename_for_document(
    uri: &Uri,
    source_contents: &str,
    version: i32,
    cursor: Position,
    new_name: &str,
    file_operation_support: FileOperationSupport,
) -> std::result::Result<Option<WorkspaceEdit>, String> {
    // Resolve the cursor in a parseable editor snapshot, which need not pass semantic validation.
    let Ok(wiki) = parser::parse(local_path(uri).as_deref(), source_contents) else {
        return Ok(None);
    };
    let Some(cursor_offset) = byte_offset(source_contents, cursor) else {
        return Ok(None);
    };

    // Try the filesystem node a link targets, and otherwise fall back to a text node.
    match rename_filesystem_node_for_document(
        &wiki,
        uri,
        source_contents,
        version,
        cursor_offset,
        new_name,
        file_operation_support,
    ) {
        Ok(None) => {
            rename_text_node_for_document(&wiki, uri, source_contents, cursor_offset, new_name)
        }
        result => result,
    }
}

// Rename one text node and every text link that targets it.
fn rename_text_node_for_document(
    wiki: &Wiki,
    uri: &Uri,
    source_contents: &str,
    cursor_offset: usize,
    new_name: &str,
) -> std::result::Result<Option<WorkspaceEdit>, String> {
    // Resolve the text node declared or linked at the cursor.
    let Some((node, _source_range)) =
        node_at(wiki, source_contents, cursor_offset, LinkExtent::Target)
    else {
        return Ok(None);
    };

    // Normalize surrounding whitespace, then reject titles that the parser would not accept: those
    // that span multiple lines, are empty, start with a filesystem-link prefix, or already exist.
    if new_name
        .chars()
        .any(|character| matches!(character, '\r' | '\n'))
    {
        return Err("A node title cannot contain a line break.".to_owned());
    }
    let new_title = new_name.trim();
    if new_title.is_empty() {
        return Err("A node title cannot be empty.".to_owned());
    }
    if new_title.starts_with(FILE_LINK_PREFIX) || new_title.starts_with(DIRECTORY_LINK_PREFIX) {
        return Err(format!(
            "A node title cannot start with `{FILE_LINK_PREFIX}` or `{DIRECTORY_LINK_PREFIX}`.",
        ));
    }
    if new_title != node.title && wiki.text_nodes.contains_key(new_title) {
        return Err(format!("Node `{new_title}` already exists."));
    }

    // Replace the declaration literally and encode the title inside every matching text link.
    let mut edits = vec![(node.title_source_range, new_title.to_owned())];
    for link in wiki.text_nodes.values().flat_map(|node| &node.links) {
        if let Link::Text {
            title,
            source_range,
        } = link
            && title == &node.title
            && let Some(target_source_range) =
                text_link_target_source_range(source_contents, *source_range)
        {
            edits.push((target_source_range, escape_link_delimiters(new_title)));
        }
    }
    edits.sort_by_key(|(source_range, _new_text)| (source_range.start, source_range.end));

    // Return one non-overlapping edit for each occurrence in the current document.
    Ok(Some(WorkspaceEdit {
        changes: Some(HashMap::from([(
            uri.clone(),
            edits
                .into_iter()
                .map(|(source_range, new_text)| {
                    TextEdit::new(lsp_range(source_contents, source_range), new_text)
                })
                .collect(),
        )])),
        ..WorkspaceEdit::default()
    }))
}

// Rename the file or directory of a filesystem link on disk and update every link to it or, for a
// directory, to anything within it. The client creates any missing directories, and directories
// that the rename leaves empty are deleted when the client supports it.
fn rename_filesystem_node_for_document(
    wiki: &Wiki,
    uri: &Uri,
    source_contents: &str,
    version: i32,
    cursor_offset: usize,
    new_name: &str,
    file_operation_support: FileOperationSupport,
) -> std::result::Result<Option<WorkspaceEdit>, String> {
    // Find the renamable filesystem node at the cursor, leaving other positions to text nodes.
    let Some(RenamableFilesystemNode {
        wiki_directory,
        old_path,
        is_directory,
        ..
    }) = renamable_filesystem_node_at(
        wiki,
        uri,
        source_contents,
        cursor_offset,
        file_operation_support.rename,
    )?
    else {
        return Ok(None);
    };
    let wiki_directory = wiki_directory.as_path();
    let old_path = old_path.as_path();

    // Accept only a new path which the parser would accept in a link.
    let new_path = normalize_filesystem_path(new_name.trim())?;
    if new_path == old_path {
        return Ok(Some(WorkspaceEdit::default()));
    }
    if new_path.as_os_str().is_empty() {
        return Err("A file or directory cannot be renamed to the wiki directory.".to_owned());
    }
    if is_directory && new_path.starts_with(old_path) {
        return Err(format!(
            "Directory {} cannot be moved into itself.",
            old_path.code_path(),
        ));
    }

    // Require the destination to be free and creatable.
    check_rename_destination(wiki_directory, old_path, &new_path)?;
    let old_absolute_path = wiki_directory.join(old_path);
    let new_absolute_path = wiki_directory.join(&new_path);

    // Rewrite the path of every link to the renamed entry.
    let edits = filesystem_rename_edits(wiki, source_contents, old_path, &new_path, is_directory);

    // Edit the wiki at the version the edits were computed from, then rename the entry.
    let (Some(old_uri), Some(new_uri)) = (
        Uri::from_file_path(&old_absolute_path),
        Uri::from_file_path(&new_absolute_path),
    ) else {
        return Err(format!("Path {} cannot be renamed.", old_path.code_path()));
    };
    let mut operations = vec![
        DocumentChangeOperation::Edit(TextDocumentEdit {
            text_document: OptionalVersionedTextDocumentIdentifier {
                uri: uri.clone(),
                version: Some(version),
            },
            edits: edits
                .into_iter()
                .map(|(source_range, new_text)| {
                    OneOf::Left(TextEdit::new(
                        lsp_range(source_contents, source_range),
                        new_text,
                    ))
                })
                .collect(),
        }),
        DocumentChangeOperation::Op(ResourceOp::Rename(RenameFile {
            old_uri,
            new_uri,
            options: None,
            annotation_id: None,
        })),
    ];

    // Finally, delete the directories the rename leaves empty. VS Code validates a run of deletions
    // before performing any of them, so a nested empty directory would block deleting its parent.
    // Instead, one recursive deletion removes the outermost directory, which contains only empty
    // directories once the renamed node has moved.
    if file_operation_support.delete
        && let Some(directory) =
            outermost_directory_emptied_by_rename(wiki, wiki_directory, old_path, &new_path)
        && let Some(uri) = Uri::from_file_path(wiki_directory.join(directory))
    {
        operations.push(DocumentChangeOperation::Op(ResourceOp::Delete(
            DeleteFile {
                uri,
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

// Produce a whole-document formatting edit for any wiki that parses, even if it is invalid.
fn formatting_for_document(uri: &Uri, source_contents: &str) -> Option<Vec<TextEdit>> {
    // Render the parsed wiki without reporting syntax errors, which diagnostics already cover.
    let rendered_wiki = parser::parse(local_path(uri).as_deref(), source_contents)
        .ok()?
        .to_string();

    // A successful request returns either one whole-document edit or an empty edit list.
    if source_contents == rendered_wiki {
        Some(Vec::new())
    } else {
        Some(vec![TextEdit::new(
            Range::new(
                Position::new(0, 0),
                lsp_position(source_contents, source_contents.len()),
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
    uri: &Uri,
    source_contents: &str,
    supports_hierarchy: bool,
) -> Option<DocumentSymbolResponse> {
    // Parse syntax without semantic validation so structurally valid nodes remain navigable.
    let wiki = parser::parse(local_path(uri).as_deref(), source_contents).ok()?;
    let mut nodes = wiki.text_nodes.values().collect::<Vec<_>>();
    nodes.sort_by_key(|node| node.source_range.start);

    // Use separate node and title ranges when the client supports hierarchical symbols, and locate
    // each flat symbol at its title otherwise.
    Some(if supports_hierarchy {
        DocumentSymbolResponse::Nested(
            nodes
                .into_iter()
                .map(|node| DocumentSymbol {
                    name: node.title.clone(),
                    detail: None,
                    kind: SymbolKind::OBJECT,
                    tags: None,
                    deprecated: None,
                    range: lsp_range(source_contents, node.source_range),
                    selection_range: lsp_range(source_contents, node.title_source_range),
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
                        uri.clone(),
                        lsp_range(source_contents, node.title_source_range),
                    ),
                    container_name: None,
                })
                .collect(),
        )
    })
}

// Offer to create a missing home node or the missing destination of a text link at an editor range.
fn code_action_for_document(
    uri: &Uri,
    source_contents: &str,
    range: Range,
    diagnostics: &[Diagnostic],
) -> Option<CodeActionResponse> {
    // Parse only the wiki syntax so fixes are available before the debounced check completes.
    let wiki = parser::parse(local_path(uri).as_deref(), source_contents).ok()?;
    let mut actions = Vec::new();

    // Prepend a missing home node where its diagnostic is reported, since it has no source range.
    let document_start = Range::new(Position::new(0, 0), Position::new(0, 0));
    if range.start == document_start.start && !wiki.text_nodes.contains_key(HOME_TITLE) {
        let separator = if source_contents.is_empty() { "" } else { "\n" };
        actions.push(create_node_action(
            uri,
            HOME_TITLE,
            TextEdit::new(
                document_start,
                format!("{TITLE_PREFIX}{HOME_TITLE}\n{separator}"),
            ),
            diagnostics,
            document_start,
        ));
    }

    // Append the missing destination of a text link after a blank line, leaving its placement to
    // the formatter. Skip empty targets, which no title can declare.
    if let Some(byte_offset) = byte_offset(source_contents, range.start)
        && let Some(Link::Text {
            title,
            source_range,
        }) = link_at(&wiki, byte_offset)
        && !title.is_empty()
        && !wiki.text_nodes.contains_key(title)
    {
        let separator = if source_contents.ends_with("\n\n") {
            ""
        } else if source_contents.ends_with('\n') {
            "\n"
        } else {
            "\n\n"
        };
        let end = lsp_position(source_contents, source_contents.len());
        actions.push(create_node_action(
            uri,
            title,
            TextEdit::new(
                Range::new(end, end),
                format!("{separator}{TITLE_PREFIX}{title}\n"),
            ),
            diagnostics,
            lsp_range(source_contents, *source_range),
        ));
    }

    // Report the absence of fixes as no response.
    (!actions.is_empty()).then_some(actions)
}

// This describes the path of a filesystem link being authored at the cursor.
struct FilesystemLinkContext {
    is_directory_link: bool,
    directory: PathBuf, // The normalized directory named by the typed path through its last `/`
    segment_start: usize, // The start of the path component after that `/`
    cursor: usize,
    closing_delimiter: Option<usize>,
}

// Identify a filesystem link whose path contains the cursor, even if the link is unfinished.
fn filesystem_link_context(source_contents: &str, cursor: usize) -> Option<FilesystemLinkContext> {
    // Confine the search to the cursor's line, since links cannot contain line breaks.
    let line_start = source_contents[..cursor]
        .rfind('\n')
        .map_or(0, |index| index + '\n'.len_utf8());
    let line_end = source_contents[cursor..]
        .find('\n')
        .map_or(source_contents.len(), |index| cursor + index);
    let line = &source_contents[line_start..line_end];
    let line = line.strip_suffix('\r').unwrap_or(line);

    // Ignore titles, which cannot contain links.
    if line == TITLE_MARKER || line.starts_with(TITLE_PREFIX) {
        return None;
    }

    // Find the open link before the cursor and any closing delimiter after it, skipping escaped
    // delimiters as the parser does.
    let mut opening_delimiter = None;
    let mut closing_delimiter = None;
    let mut previous_was_backslash = false;
    for (index, character) in line.char_indices() {
        let offset = line_start + index;
        let is_escaped_delimiter = previous_was_backslash && matches!(character, '[' | ']');
        previous_was_backslash = character == '\\';
        if is_escaped_delimiter {
            continue;
        }
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

    // Require a filesystem prefix after any leading whitespace, since the parser trims targets.
    let target = source_contents[opening_delimiter? + '['.len_utf8()..cursor].trim_start();
    let (is_directory_link, typed_path) = match target.strip_prefix(FILE_LINK_PREFIX) {
        Some(typed_path) => (false, typed_path),
        None => (true, target.strip_prefix(DIRECTORY_LINK_PREFIX)?),
    };

    // Resolve the typed directory, declining paths which escape the wiki tree
    // [ref:filesystem_path_components].
    let typed_directory = &typed_path[..typed_path.rfind('/').map_or(0, |index| index + 1)];
    let mut directory = PathBuf::new();
    for component in
        Path::new(&typed_directory.replace("\\[", "[").replace("\\]", "]")).components()
    {
        match component {
            Component::Normal(component) => directory.push(component),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }

    Some(FilesystemLinkContext {
        is_directory_link,
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
    context: &FilesystemLinkContext,
) -> Vec<CompletionItem> {
    // Derive every filesystem path from the wiki's containing directory, as validation does.
    let wiki_directory = wiki_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    // Descend only along the typed directory so large subtrees are read only once they are named,
    // and exclude the wiki itself.
    let Ok(mut walker_builder) = wiki_tree_walker(wiki_directory) else {
        return Vec::new();
    };
    walker_builder
        .max_depth(Some(context.directory.components().count() + 1))
        .filter_entry({
            let wiki_directory = wiki_directory.to_owned();
            let relative_wiki_path = relative_path(&wiki_directory, wiki_path).to_owned();
            let directory = context.directory.clone();
            move |entry| {
                let path = relative_path(&wiki_directory, entry.path());
                path != relative_wiki_path
                    && (directory.starts_with(path) || path.parent() == Some(directory.as_path()))
            }
        });

    // Offer each visible child of the typed directory, skipping the ancestors walked to reach it.
    let mut completions = Vec::new();
    for entry in walker_builder.build().flatten() {
        let path = relative_path(wiki_directory, entry.path());
        let (Some(file_type), Some(name)) = (entry.file_type(), entry.file_name().to_str()) else {
            continue;
        };
        if path.parent() != Some(context.directory.as_path()) {
            continue;
        }

        // Leave a directory's link open for its children, and close a file's link.
        let escaped_name = escape_link_delimiters(name);
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
        } else if context.is_directory_link {
            continue;
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
        let replacement_range = lsp_range(
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
fn text_link_context(
    source_path: Option<&Path>,
    source_contents: &str,
    byte_offset: usize,
) -> Option<(Wiki, SourceRange)> {
    // Prefer the unchanged source when the active link is already closed.
    if let Ok(wiki) = parser::parse(source_path, source_contents)
        && let Some(Link::Text { source_range, .. }) = link_at(&wiki, byte_offset)
    {
        let source_range = *source_range;
        return Some((wiki, source_range));
    }

    // Close a link at the cursor temporarily so completion works while it is being authored.
    let mut completed_source = source_contents.to_owned();
    completed_source.insert(byte_offset, ']');
    let wiki = parser::parse(source_path, &completed_source).ok()?;
    let Some(Link::Text { source_range, .. }) = link_at(&wiki, byte_offset) else {
        return None;
    };

    // Map the link's end back into the original source, which lacks the temporary delimiter.
    let source_range = SourceRange {
        start: source_range.start,
        end: source_range.end - ']'.len_utf8(),
    };
    Some((wiki, source_range))
}

// Complete a text link with every node title, replacing the link at a source range.
fn text_link_completions(
    wiki: &Wiki,
    source_contents: &str,
    replacement_source_range: SourceRange,
) -> Vec<CompletionItem> {
    // Present node titles deterministically and replace the whole link, including its delimiters,
    // so the cursor ends up after the closing `]`.
    let replacement_range = lsp_range(source_contents, replacement_source_range);
    let mut titles = wiki.text_nodes.keys().collect::<Vec<_>>();
    titles.sort();
    titles
        .into_iter()
        .map(|title| {
            let escaped_title = escape_link_delimiters(title);
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

// This describes the filesystem node targeted by the link at the cursor, once it is known to be
// renamable regardless of its new name.
struct RenamableFilesystemNode {
    wiki_directory: PathBuf,
    path_source_range: SourceRange,
    old_path: PathBuf,
    is_directory: bool,
}

// Find the filesystem node targeted by the link at the cursor and check whether it can be renamed
// at all. Every other position yields no node, leaving it to text node renaming.
fn renamable_filesystem_node_at(
    wiki: &Wiki,
    uri: &Uri,
    source_contents: &str,
    cursor_offset: usize,
    supports_file_renames: bool,
) -> std::result::Result<Option<RenamableFilesystemNode>, String> {
    // Resolve the filesystem link at the cursor and the path within it.
    let (is_directory, old_path, source_range) = match link_at(wiki, cursor_offset) {
        Some(Link::File { path, source_range }) => (false, path.clone(), *source_range),
        Some(Link::Directory { path, source_range }) => (true, path.clone(), *source_range),
        Some(Link::Text { .. }) | None => return Ok(None),
    };
    let Some(path_source_range) = filesystem_link_path_source_range(source_contents, source_range)
    else {
        return Ok(None);
    };

    // The client renames the node on disk, which requires a saved wiki and a capable client.
    let Some(wiki_path) = local_path(uri) else {
        return Err("Save the wiki before renaming the files it links to.".to_owned());
    };
    if !supports_file_renames {
        return Err("This editor does not support renaming files.".to_owned());
    }

    // Require the linked node to exist as the kind the link names, other than the wiki directory
    // and the wiki itself, resolving it from the wiki's containing directory as validation does.
    let wiki_directory = wiki_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if old_path.as_os_str().is_empty() {
        return Err("The wiki directory cannot be renamed.".to_owned());
    }
    let kind = if is_directory { "Directory" } else { "File" };
    if !fs::metadata(wiki_directory.join(&old_path))
        .is_ok_and(|metadata| metadata.is_dir() == is_directory)
    {
        return Err(format!("{kind} {} does not exist.", old_path.code_path()));
    }
    if old_path == relative_path(wiki_directory, &wiki_path) {
        return Err("The wiki cannot be renamed through one of its own links.".to_owned());
    }

    Ok(Some(RenamableFilesystemNode {
        wiki_directory: wiki_directory.to_owned(),
        path_source_range,
        old_path,
        is_directory,
    }))
}

// Require a rename's destination to be free, other than by a change to the case of a name on a
// case-insensitive filesystem, where both paths resolve to the same entry. Missing directories will
// be created, but not beneath an existing file.
fn check_rename_destination(
    wiki_directory: &Path,
    old_path: &Path,
    new_path: &Path,
) -> std::result::Result<(), String> {
    // Refuse to replace another entry.
    let new_absolute_path = wiki_directory.join(new_path);
    if fs::symlink_metadata(&new_absolute_path).is_ok()
        && fs::canonicalize(&new_absolute_path).ok()
            != fs::canonicalize(wiki_directory.join(old_path)).ok()
    {
        return Err(format!("Path {} already exists.", new_path.code_path()));
    }

    // Refuse to create a directory beneath an existing file.
    if let Some(ancestor) = new_path
        .ancestors()
        .skip(1)
        .find(|ancestor| wiki_directory.join(ancestor).exists())
        && !wiki_directory.join(ancestor).is_dir()
    {
        return Err(format!("Path {} is not a directory.", ancestor.code_path()));
    }
    Ok(())
}

// Rewrite the path of every link to a renamed entry. Renaming a directory also moves everything
// within it, while a file is referenced only by file links with its exact path.
fn filesystem_rename_edits(
    wiki: &Wiki,
    source_contents: &str,
    old_path: &Path,
    new_path: &Path,
    is_directory: bool,
) -> Vec<(SourceRange, String)> {
    // Select filesystem links at the renamed path or, for a directory, within it.
    let mut edits = Vec::new();
    for link in wiki.text_nodes.values().flat_map(|node| &node.links) {
        let (Link::File { path, source_range } | Link::Directory { path, source_range }) = link
        else {
            continue;
        };
        let Ok(suffix) = path.strip_prefix(old_path) else {
            continue;
        };
        if !is_directory && (matches!(link, Link::Directory { .. }) || path != old_path) {
            continue;
        }

        // Replace the link's path, keeping anything below a renamed directory.
        let Some(path_source_range) =
            filesystem_link_path_source_range(source_contents, *source_range)
        else {
            continue;
        };
        edits.push((
            path_source_range,
            render_link_path(
                &source_contents[path_source_range.start..path_source_range.end],
                &new_path.join(suffix),
            ),
        ));
    }

    // Present the edits in source order.
    edits.sort_by_key(|(source_range, _new_text)| (source_range.start, source_range.end));
    edits
}

// Write a normalized link path in the style of the path it replaces, keeping a leading `./` or a
// trailing `/`, and escape any link delimiters.
fn render_link_path(old_source: &str, path: &Path) -> String {
    // Join the components with the separator that links use on every platform. Both the new path
    // and any suffix below a renamed directory come from UTF-8 text.
    let mut rendered = path
        .components()
        .map(|component| {
            component
                .as_os_str()
                .to_str()
                .expect("link paths should come from UTF-8 text")
        })
        .collect::<Vec<_>>()
        .join("/");

    // Keep the replaced path's leading `./` and trailing `/`.
    if old_source.starts_with("./") {
        rendered.insert_str(0, "./");
    }
    if old_source.len() > 1 && old_source.ends_with('/') {
        rendered.push('/');
    }

    // Escape the finished text once, just before it returns to the source.
    escape_link_delimiters(&rendered)
}

// Find the outermost directory that moving a node out of it would leave containing nothing but
// empty directories. A directory is kept if it will contain the new path or a directory link names
// it, and the search never reaches the wiki directory itself.
fn outermost_directory_emptied_by_rename(
    wiki: &Wiki,
    wiki_directory: &Path,
    old_path: &Path,
    new_path: &Path,
) -> Option<PathBuf> {
    // Collect the directories that links name, which must survive the rename.
    let linked_directories = wiki
        .text_nodes
        .values()
        .flat_map(|node| &node.links)
        .filter_map(|link| match link {
            Link::Directory { path, .. } => Some(path.as_path()),
            Link::Text { .. } | Link::File { .. } => None,
        })
        .collect::<HashSet<_>>();

    // Ascend while each directory contains nothing but the entry being moved or deleted from it.
    let mut emptied_directory = None;
    let mut removed_entry = old_path;
    while let Some(directory) = removed_entry
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty())
    {
        if new_path.starts_with(directory) || linked_directories.contains(directory) {
            break;
        }
        let Ok(mut entries) = fs::read_dir(wiki_directory.join(directory)) else {
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
        emptied_directory = Some(directory.to_owned());
        removed_entry = directory;
    }
    emptied_directory
}

// Describe a preferred quick fix that declares a node and resolves the diagnostics at a range.
fn create_node_action(
    uri: &Uri,
    title: &str,
    edit: TextEdit,
    diagnostics: &[Diagnostic],
    diagnostic_range: Range,
) -> CodeActionOrCommand {
    CodeActionOrCommand::CodeAction(CodeAction {
        title: format!("Create node `{title}`"),
        kind: Some(CodeActionKind::QUICKFIX),
        diagnostics: Some(
            diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.range == diagnostic_range)
                .cloned()
                .collect(),
        ),
        edit: Some(WorkspaceEdit {
            changes: Some(HashMap::from([(uri.clone(), vec![edit])])),
            ..WorkspaceEdit::default()
        }),
        is_preferred: Some(true),
        ..CodeAction::default()
    })
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
            LinkExtent::Target => text_link_target_source_range(source_contents, source_range)?,
        },
    ))
}

// Find the link of any kind at a source offset without resolving its destination. Links never
// overlap, so at most one link can contain the offset.
fn link_at(wiki: &Wiki, byte_offset: usize) -> Option<&Link> {
    wiki.text_nodes.values().find_map(|node| {
        node.links.iter().find(|link| {
            let (Link::Text { source_range, .. }
            | Link::File { source_range, .. }
            | Link::Directory { source_range, .. }) = link;
            source_range.start <= byte_offset && byte_offset < source_range.end
        })
    })
}

// Exclude the delimiters from a source range known to represent a complete link.
fn text_link_target_source_range(
    source_contents: &str,
    source_range: SourceRange,
) -> Option<SourceRange> {
    // Confirm the parser-provided range still addresses square-bracket delimiters.
    let target_source = source_contents
        .get(source_range.start..source_range.end)?
        .strip_prefix('[')?
        .strip_suffix(']')?;
    let start = source_range.start + '['.len_utf8();
    Some(SourceRange {
        start,
        end: start + target_source.len(),
    })
}

// Locate the path of a complete filesystem link, excluding its delimiters, prefix, and surrounding
// whitespace.
fn filesystem_link_path_source_range(
    source_contents: &str,
    source_range: SourceRange,
) -> Option<SourceRange> {
    // Trim the link's inner text as the parser does, then skip the filesystem-link prefix.
    let target_source_range = text_link_target_source_range(source_contents, source_range)?;
    let target = &source_contents[target_source_range.start..target_source_range.end];
    let trimmed_target = target.trim();
    let path = trimmed_target
        .strip_prefix(FILE_LINK_PREFIX)
        .or_else(|| trimmed_target.strip_prefix(DIRECTORY_LINK_PREFIX))?;
    let start = target_source_range.start
        + (target.len() - target.trim_start().len())
        + (trimmed_target.len() - path.len());
    Some(SourceRange {
        start,
        end: start + path.len(),
    })
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
            Link::Text { .. } | Link::File { .. } | Link::Directory { .. } => None,
        })
        .collect::<Vec<_>>();
    source_ranges.sort_by_key(|source_range| (source_range.start, source_range.end));
    source_ranges
}

// Collect every complete filesystem-link range with the same kind and path as a target.
fn filesystem_link_source_ranges(wiki: &Wiki, target: &Link) -> Vec<SourceRange> {
    // Match logical paths without resolving symlinks, just as filesystem validation does.
    wiki.text_nodes
        .values()
        .flat_map(|node| &node.links)
        .filter_map(|link| match (link, target) {
            (
                Link::File { path, source_range },
                Link::File {
                    path: target_path, ..
                },
            )
            | (
                Link::Directory { path, source_range },
                Link::Directory {
                    path: target_path, ..
                },
            ) if path == target_path => Some(*source_range),
            (
                Link::Text { .. } | Link::File { .. } | Link::Directory { .. },
                Link::Text { .. } | Link::File { .. } | Link::Directory { .. },
            ) => None,
        })
        .collect()
}

// Convert a zero-based LSP position measured in UTF-16 code units into a UTF-8 byte offset.
fn byte_offset(source_contents: &str, position: Position) -> Option<usize> {
    // Locate the requested line without counting its line terminator as editor content.
    let mut line_start = 0;
    for _ in 0..position.line {
        line_start += source_contents[line_start..].find('\n')? + '\n'.len_utf8();
    }
    let line_end = source_contents[line_start..]
        .find('\n')
        .map_or(source_contents.len(), |index| line_start + index);
    let content_end = if line_end > line_start
        && source_contents.as_bytes()[line_end - 1] == b'\r'
        && source_contents.as_bytes().get(line_end) == Some(&b'\n')
    {
        line_end - '\r'.len_utf8()
    } else {
        line_end
    };

    // Reject positions that split a surrogate pair or extend past the line's contents.
    let requested_character = usize::try_from(position.character).ok()?;
    let mut utf16_character = 0;
    for (index, character) in source_contents[line_start..content_end].char_indices() {
        if utf16_character == requested_character {
            return Some(line_start + index);
        }
        utf16_character += character.len_utf16();
        if utf16_character > requested_character {
            return None;
        }
    }
    (utf16_character == requested_character).then_some(content_end)
}

// Convert a UTF-8 byte offset into a zero-based LSP position measured in UTF-16 code units.
fn lsp_position(source_contents: &str, byte_offset: usize) -> Position {
    // Source ranges originate at character boundaries and cannot extend beyond the source.
    let byte_offset = byte_offset.min(source_contents.len());
    let prefix = source_contents
        .get(..byte_offset)
        .expect("source ranges should end on UTF-8 character boundaries");
    Position::new(
        u32::try_from(prefix.bytes().filter(|byte| *byte == b'\n').count()).unwrap_or(u32::MAX),
        u32::try_from(
            source_contents[prefix
                .rfind('\n')
                .map_or(0, |index| index + '\n'.len_utf8())
                ..byte_offset]
                .encode_utf16()
                .count(),
        )
        .unwrap_or(u32::MAX),
    )
}

// Convert a source range into the representation expected by the language server protocol.
fn lsp_range(source_contents: &str, source_range: SourceRange) -> Range {
    Range::new(
        lsp_position(source_contents, source_range.start),
        lsp_position(source_contents, source_range.end),
    )
}

// Encode an editor navigation command as a Markdown-safe URI.
fn reveal_range_command_url(
    uri: &Uri,
    source_contents: &str,
    source_range: SourceRange,
) -> Option<String> {
    // Pass the document URI and UTF-16 destination range as positional command arguments.
    let range = lsp_range(source_contents, source_range);
    Some(format!(
        "command:{REVEAL_RANGE_COMMAND}?{}",
        utf8_percent_encode(
            &serde_json::to_string(&(
                uri.as_str(),
                range.start.line,
                range.start.character,
                range.end.line,
                range.end.character,
            ))
            .ok()?,
            NON_ALPHANUMERIC,
        ),
    ))
}

// Escape delimiters so an arbitrary node title or path retains its meaning inside a link.
fn escape_link_delimiters(title: &str) -> String {
    title.replace('[', "\\[").replace(']', "\\]")
}

// Decode escaped delimiters in link text, as the parser does.
fn unescape_link_delimiters(source: &str) -> String {
    source.replace("\\[", "[").replace("\\]", "]")
}

// Convert a structured Mull error into the representation expected by language clients.
fn diagnostic_from_error(source_contents: &str, error: &Error) -> Diagnostic {
    // Include an underlying reason without including terminal prefixes, paths, or source listings.
    diagnostic(
        source_contents,
        error.source_range(),
        error.reason().map_or_else(
            || error.message().to_owned(),
            |reason| format!("{}\n\nReason: {reason}", error.message()),
        ),
    )
}

// Construct a Mull error diagnostic at a source range or at the start of the document.
fn diagnostic(
    source_contents: &str,
    source_range: Option<crate::error::SourceRange>,
    message: String,
) -> Diagnostic {
    Diagnostic {
        range: lsp_range(
            source_contents,
            source_range.unwrap_or(crate::error::SourceRange { start: 0, end: 0 }),
        ),
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some(env!("CARGO_PKG_NAME").to_owned()),
        message,
        ..Diagnostic::default()
    }
}

// Convert only file-scheme URIs because the URI library does not enforce this distinction.
fn local_path(uri: &Uri) -> Option<Cow<'_, Path>> {
    uri.scheme()
        .as_str()
        .eq_ignore_ascii_case("file")
        .then(|| uri.to_file_path())
        .flatten()
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
    use super::{
        FileOperationSupport, byte_offset, code_action_for_document, completion_for_document,
        diagnostic_from_error, diagnostics_for_document, document_highlight_for_document,
        document_symbol_for_document, formatting_for_document, goto_definition_for_document,
        hover_for_document, lsp_position, prepare_rename_for_document, references_for_document,
        rename_for_document, reveal_range_command_url,
    };
    use crate::{cancellation::CancellationFlag, error::SourceRange, parser};
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

    // Compute diagnostics for a check which nothing cancels.
    fn diagnostics(uri: &Uri, source_contents: &str) -> Vec<Diagnostic> {
        diagnostics_for_document(uri, source_contents, &CancellationFlag::default())
            .expect("a check without cancellation should complete")
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

    #[test]
    fn positions_use_utf16_code_units() {
        let source = "zero\n😀 café";

        assert_eq!(lsp_position(source, 0), Position::new(0, 0));
        assert_eq!(lsp_position(source, 5), Position::new(1, 0));
        assert_eq!(lsp_position(source, 9), Position::new(1, 2));
        assert_eq!(lsp_position(source, source.len()), Position::new(1, 7));
    }

    // Convert editor positions back to byte offsets without splitting Unicode characters.
    #[test]
    fn byte_offsets_use_utf16_code_units() {
        let source = "zero\n😀 café";

        assert_eq!(byte_offset(source, Position::new(0, 0)), Some(0));
        assert_eq!(byte_offset(source, Position::new(1, 0)), Some(5));
        assert_eq!(byte_offset(source, Position::new(1, 1)), None);
        assert_eq!(byte_offset(source, Position::new(1, 2)), Some(9));
        assert_eq!(byte_offset(source, Position::new(1, 7)), Some(source.len()));
        assert_eq!(byte_offset(source, Position::new(1, 8)), None);
        assert_eq!(byte_offset(source, Position::new(2, 0)), None);
    }

    // Encode a document URI and UTF-16 title range for the trusted editor command.
    #[test]
    fn reveal_range_commands_encode_destinations() {
        let source = "# Home";
        let url =
            reveal_range_command_url(&untitled_uri(), source, SourceRange { start: 2, end: 6 })
                .unwrap();

        assert_eq!(
            url,
            concat!(
                "command:mull.revealRange?",
                "%5B%22untitled%3AUntitled%2D1%22%2C0%2C2%2C0%2C6%5D",
            ),
        );
    }

    // Expose text nodes in source order for saved and untitled editor outlines.
    #[test]
    fn document_symbols_describe_text_nodes() {
        let source = "# Zebra\n\nFirst\n\n# Alpha\n\nSecond";
        let wiki = TestWiki::new(source);
        let uris = [untitled_uri(), Uri::from_file_path(wiki.path()).unwrap()];

        for uri in uris {
            let response = document_symbol_for_document(&uri, source, true).unwrap();
            let DocumentSymbolResponse::Nested(symbols) = response else {
                panic!("text nodes should be represented as nested document symbols");
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
            let response = document_symbol_for_document(&uri, source, false).unwrap();
            let DocumentSymbolResponse::Flat(symbols) = response else {
                panic!("clients without hierarchy support should receive flat symbols");
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

    // Require a first save before resolving filesystem links from a new editor buffer.
    #[test]
    fn untitled_filesystem_links_receive_diagnostics() {
        let source = concat!("# Home\n[", "file:notes.txt] [", "dir:images]");
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
                Range::new(Position::new(1, 0), Position::new(1, 16)),
                Range::new(Position::new(1, 17), Position::new(1, 29)),
            ],
        );
    }

    // Navigate among text nodes without requiring a new editor buffer to have a path.
    #[test]
    fn untitled_wikis_support_navigation() {
        let source = "# Home\n\n[Greeting]\n\n# Greeting";

        assert!(
            goto_definition_for_document(&untitled_uri(), source, Position::new(2, 4)).is_some(),
        );
        assert!(hover_for_document(&untitled_uri(), source, Position::new(2, 4)).is_some());
        assert!(
            references_for_document(&untitled_uri(), source, Position::new(2, 4), false).is_some(),
        );
    }

    // Complete a partial target by replacing the whole link, including its delimiters.
    #[test]
    fn completions_replace_closed_links() {
        let source = "# Home\n\n[Gr]\n\n# Greeting\n\n# Other";
        let completions =
            completion_for_document(&untitled_uri(), source, Position::new(2, 3)).unwrap();

        assert_eq!(
            completions
                .iter()
                .map(|completion| completion.label.as_str())
                .collect::<Vec<_>>(),
            vec!["Greeting", "Home", "Other"],
        );
        let Some(CompletionTextEdit::Edit(edit)) = &completions[0].text_edit else {
            panic!("a completion should replace the link");
        };
        let link_range = Range::new(Position::new(2, 0), Position::new(2, 4));
        assert_eq!(edit.range, link_range);
        assert_eq!(edit.new_text, "[Greeting]");

        // Replace the same link from a cursor before its opening delimiter.
        let completions =
            completion_for_document(&untitled_uri(), source, Position::new(2, 0)).unwrap();
        let Some(CompletionTextEdit::Edit(edit)) = &completions[0].text_edit else {
            panic!("a completion should replace the link");
        };
        assert_eq!(edit.range, link_range);
    }

    // Close an unfinished link temporarily while calculating its completions.
    #[test]
    fn completions_support_unfinished_links() {
        let source = "# Home\n\n[Gre\n\n# Greeting";
        let completions =
            completion_for_document(&untitled_uri(), source, Position::new(2, 4)).unwrap();
        let greeting = completions
            .iter()
            .find(|completion| completion.label == "Greeting")
            .unwrap();
        let Some(CompletionTextEdit::Edit(edit)) = &greeting.text_edit else {
            panic!("a completion should replace the unfinished link");
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
            completion_for_document(&untitled_uri(), source, Position::new(2, 3)).unwrap();
        let greeting = completions
            .iter()
            .find(|completion| completion.label == "Greeting")
            .unwrap();
        let Some(CompletionTextEdit::Edit(edit)) = &greeting.text_edit else {
            panic!("a completion should replace the link");
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
            completion_for_document(&untitled_uri(), source, Position::new(2, 1)).unwrap();
        let bracketed = completions
            .iter()
            .find(|completion| completion.label == "A[B]")
            .unwrap();
        let Some(CompletionTextEdit::Edit(edit)) = &bracketed.text_edit else {
            panic!("a completion should encode the title as a text link");
        };

        assert_eq!(bracketed.filter_text.as_deref(), Some("[A\\[B\\]"));
        assert_eq!(edit.new_text, "[A\\[B\\]]");
    }

    // Offer text-node completions only while the cursor is inside a text link target.
    #[test]
    fn completions_ignore_other_contexts() {
        let source = concat!("# Home\n\nprose [", "file:notes.txt]");

        assert!(completion_for_document(&untitled_uri(), source, Position::new(2, 2)).is_none());
        assert!(completion_for_document(&untitled_uri(), source, Position::new(2, 10)).is_none());
        assert!(completion_for_document(&untitled_uri(), source, Position::new(0, 3)).is_none());
    }

    // Summarize each completion by its label, replacement range, and inserted text.
    fn completion_edits(completions: &[CompletionItem]) -> Vec<(&str, Range, &str)> {
        completions
            .iter()
            .map(|completion| {
                let Some(CompletionTextEdit::Edit(edit)) = &completion.text_edit else {
                    panic!("a completion should replace part of the link");
                };
                (
                    completion.label.as_str(),
                    edit.range,
                    edit.new_text.as_str(),
                )
            })
            .collect()
    }

    // Complete a file link with the visible entries of the wiki directory.
    #[test]
    fn completions_list_filesystem_entries() {
        let source = concat!("# Home\n\n[", "file:]");
        let wiki = TestWiki::new(source);
        let directory = wiki.path().parent().unwrap();
        fs::write(directory.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(directory.join("ignored.txt"), "ignored").unwrap();
        fs::write(directory.join("notes.txt"), "notes").unwrap();
        fs::create_dir(directory.join("images")).unwrap();
        fs::write(directory.join("images/photo.jpg"), "photo").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let completions = completion_for_document(&uri, source, Position::new(2, 6)).unwrap();

        // Close a file's link but leave a directory's link open for its children.
        let empty_path = Range::new(Position::new(2, 6), Position::new(2, 6));
        let closed_path = Range::new(Position::new(2, 6), Position::new(2, 7));
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

    // Complete only directories within the directory named by an unfinished directory link,
    // replacing only the component being typed.
    #[test]
    fn completions_list_nested_directories() {
        let source = concat!("# Home\n\n[", "dir:./images/r");
        let wiki = TestWiki::new(source);
        let directory = wiki.path().parent().unwrap();
        fs::create_dir_all(directory.join("images/raw/large")).unwrap();
        fs::write(directory.join("images/photo.jpg"), "photo").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let completions = completion_for_document(&uri, source, Position::new(2, 15)).unwrap();

        assert_eq!(
            completion_edits(&completions),
            vec![(
                "raw/",
                Range::new(Position::new(2, 14), Position::new(2, 15)),
                "raw/",
            )],
        );
        assert_eq!(completions[0].filter_text.as_deref(), Some("raw"));
    }

    // Escape link delimiters in completed names and interpret them in typed directories, which
    // completions leave untouched.
    #[test]
    fn completions_escape_path_delimiters() {
        let source = concat!("# Home\n\n[", "file:a\\[b\\]/]");
        let wiki = TestWiki::new(source);
        let directory = wiki.path().parent().unwrap();
        fs::create_dir_all(directory.join("a[b]")).unwrap();
        fs::write(directory.join("a[b]/c[d].txt"), "content").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        let completions = completion_for_document(&uri, source, Position::new(2, 6)).unwrap();
        assert_eq!(
            completion_edits(&completions),
            vec![(
                "a[b]/",
                Range::new(Position::new(2, 6), Position::new(2, 13)),
                "a\\[b\\]/",
            )],
        );

        let completions = completion_for_document(&uri, source, Position::new(2, 13)).unwrap();
        assert_eq!(
            completion_edits(&completions),
            vec![(
                "c[d].txt",
                Range::new(Position::new(2, 13), Position::new(2, 14)),
                "c\\[d\\].txt]",
            )],
        );
    }

    // Decline filesystem completions where the parser would not recognize a valid link path.
    #[test]
    fn completions_ignore_invalid_filesystem_contexts() {
        let source = concat!("# [", "file:\n\n[", "file:../] [", "dir:/] \\[", "file:");
        let wiki = TestWiki::new(source);
        fs::write(wiki.path().parent().unwrap().join("notes.txt"), "notes").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        // Ignore titles, escaping paths, and escaped delimiters.
        for cursor in [
            Position::new(0, 8),
            Position::new(2, 9),
            Position::new(2, 17),
            Position::new(2, 26),
        ] {
            assert!(
                completion_for_document(&uri, source, cursor)
                    .is_none_or(|completions| completions.is_empty()),
            );
        }

        // Require a filesystem to list entries from.
        let source = concat!("# Home\n\n[", "file:]");
        assert!(completion_for_document(&untitled_uri(), source, Position::new(2, 6)).is_none());
    }

    // Treat a title as its own definition, which lets editors fall back to finding references.
    #[test]
    fn definitions_of_titles_target_themselves() {
        let source = "# Home\n\n[Home]";
        let definition =
            goto_definition_for_document(&untitled_uri(), source, Position::new(0, 3)).unwrap();

        let GotoDefinitionResponse::Link(links) = definition else {
            panic!("a title should have one definition");
        };
        let [link] = links.as_slice() else {
            panic!("a title should have exactly one definition");
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
        let definition = goto_definition_for_document(&uri, source, Position::new(2, 5)).unwrap();

        let GotoDefinitionResponse::Link(links) = definition else {
            panic!("a text link should have one definition");
        };
        let [link] = links.as_slice() else {
            panic!("a text link should have exactly one definition");
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
        let hover = hover_for_document(&uri, source, Position::new(2, 5)).unwrap();

        let HoverContents::Markup(contents) = hover.contents else {
            panic!("a node preview should use markup content");
        };
        assert_eq!(contents.kind, MarkupKind::Markdown);
        let home_url =
            reveal_range_command_url(&uri, source, SourceRange { start: 2, end: 6 }).unwrap();
        assert_eq!(
            contents.value,
            format!(
                "# Greeting\n\nLiteral &#91;brackets&#93; and \
                    [&#91;Home&#93;]({home_url}).",
            ),
        );
        assert_eq!(
            hover.range,
            Some(Range::new(Position::new(2, 3), Position::new(2, 13))),
        );
    }

    // Preview a node directly from its title declaration.
    #[test]
    fn hovers_preview_node_titles() {
        let source = "# Home\n\n[Greeting]\n\n# Greeting\n\nHello!";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let hover = hover_for_document(&uri, source, Position::new(4, 4)).unwrap();

        let HoverContents::Markup(contents) = hover.contents else {
            panic!("a node preview should use markup content");
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
        let from_title = references_for_document(&uri, source, Position::new(8, 3), false).unwrap();
        let from_link = references_for_document(&uri, source, Position::new(2, 3), false).unwrap();

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
        let locations = references_for_document(&uri, source, Position::new(4, 3), true).unwrap();

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
            references_for_document(&uri, source, Position::new(0, 3), false),
            Some(Vec::new()),
        );
        assert!(references_for_document(&uri, source, Position::new(0, 0), false).is_none());
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
            document_highlight_for_document(&uri, source, Position::new(8, 3)).unwrap();
        let from_link = document_highlight_for_document(&uri, source, Position::new(2, 3)).unwrap();
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
            document_highlight_for_document(&uri, source, Position::new(0, 3)),
            Some(vec![DocumentHighlight {
                range: Range::new(Position::new(0, 2), Position::new(0, 6)),
                kind: Some(DocumentHighlightKind::WRITE),
            }]),
        );
        assert!(document_highlight_for_document(&uri, source, Position::new(0, 0)).is_none());
    }

    // Highlight nothing for a text link whose target does not exist.
    #[test]
    fn document_highlights_ignore_unresolved_text_links() {
        let source = "# Home\n\n[Missing]";

        assert!(
            document_highlight_for_document(&untitled_uri(), source, Position::new(2, 3)).is_none(),
        );
    }

    // Highlight matching filesystem links without conflating file and directory references.
    #[test]
    fn document_highlights_find_filesystem_links() {
        let source = concat!(
            "# Home\n\n",
            "[",
            "file:foo] [",
            "file:foo] [",
            "dir:bar]\n\n",
            "# Other\n\n",
            "[",
            "dir:bar]",
        );
        let uri = untitled_uri();
        let file_highlights =
            document_highlight_for_document(&uri, source, Position::new(2, 3)).unwrap();
        let directory_highlights =
            document_highlight_for_document(&uri, source, Position::new(2, 25)).unwrap();

        assert_eq!(
            file_highlights,
            vec![
                DocumentHighlight {
                    range: Range::new(Position::new(2, 0), Position::new(2, 10)),
                    kind: Some(DocumentHighlightKind::READ),
                },
                DocumentHighlight {
                    range: Range::new(Position::new(2, 11), Position::new(2, 21)),
                    kind: Some(DocumentHighlightKind::READ),
                },
            ],
        );
        assert_eq!(
            directory_highlights,
            vec![
                DocumentHighlight {
                    range: Range::new(Position::new(2, 22), Position::new(2, 31)),
                    kind: Some(DocumentHighlightKind::READ),
                },
                DocumentHighlight {
                    range: Range::new(Position::new(6, 0), Position::new(6, 9)),
                    kind: Some(DocumentHighlightKind::READ),
                },
            ],
        );
    }

    // Prepare rename from either a declaration or text link without selecting its delimiters.
    #[test]
    fn rename_preparation_selects_title_text() {
        let source = "# Home\n\n[Greeting]\n\n# Greeting";
        let from_title =
            prepare_rename_for_document(&untitled_uri(), source, Position::new(4, 3), false)
                .unwrap()
                .unwrap();
        let from_link =
            prepare_rename_for_document(&untitled_uri(), source, Position::new(2, 4), false)
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

    // Rename a declaration and every text link while trimming the requested title.
    #[test]
    fn rename_updates_every_occurrence() {
        let source = "# Home\n\n[Greeting] and [Greeting]\n\n# Greeting";
        let workspace_edit = rename_for_document(
            &untitled_uri(),
            source,
            7,
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
            &untitled_uri(),
            source,
            7,
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
            &untitled_uri(),
            source,
            7,
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
                &untitled_uri(),
                source,
                7,
                cursor,
                " \t",
                ALL_FILE_OPERATIONS,
            )
            .unwrap_err(),
            "A node title cannot be empty.",
        );
        assert_eq!(
            rename_for_document(
                &untitled_uri(),
                source,
                7,
                cursor,
                "Hello\nworld",
                ALL_FILE_OPERATIONS,
            )
            .unwrap_err(),
            "A node title cannot contain a line break.",
        );
        assert_eq!(
            rename_for_document(
                &untitled_uri(),
                source,
                7,
                cursor,
                "Home",
                ALL_FILE_OPERATIONS,
            )
            .unwrap_err(),
            "Node `Home` already exists.",
        );
        assert_eq!(
            rename_for_document(
                &untitled_uri(),
                source,
                7,
                cursor,
                "file:notes.txt",
                ALL_FILE_OPERATIONS,
            )
            .unwrap_err(),
            "A node title cannot start with `file:` or `dir:`.",
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
            panic!("a filesystem rename should consist of document change operations");
        };
        let [
            DocumentChangeOperation::Edit(text_document_edit),
            DocumentChangeOperation::Op(ResourceOp::Rename(rename)),
            deletions @ ..,
        ] = operations.as_slice()
        else {
            panic!("a filesystem rename should edit the wiki and then rename one node");
        };
        assert_eq!(text_document_edit.text_document.version, Some(7_i32));

        // Apply the edits from the end so earlier ranges stay valid.
        let mut applied = source.to_owned();
        for edit in text_document_edit.edits.iter().rev() {
            let OneOf::Left(edit) = edit else {
                panic!("a filesystem rename should not annotate its edits");
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
                        "a filesystem rename should only delete directories after renaming a node",
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
        let source = concat!("# Home\n\n[Home] [", "file:notes.txt]");
        let wiki = TestWiki::new(source);
        fs::write(wiki.path().parent().unwrap().join("notes.txt"), "notes").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let rename = |character, new_name| {
            rename_for_document(
                &uri,
                source,
                7,
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

    // Prepare to rename a filesystem link by selecting its decoded path as written.
    #[test]
    fn rename_preparation_selects_filesystem_paths() {
        let source = concat!("# Home\n\n[ ", "file:./a\\[1\\].txt ]");
        let wiki = TestWiki::new(source);
        fs::write(wiki.path().parent().unwrap().join("a[1].txt"), "a").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        assert_eq!(
            prepare_rename_for_document(&uri, source, Position::new(2, 4), true)
                .unwrap()
                .unwrap(),
            PrepareRenameResponse::RangeWithPlaceholder {
                range: Range::new(Position::new(2, 7), Position::new(2, 19)),
                placeholder: "./a[1].txt".to_owned(),
            },
        );
    }

    // Explain why a filesystem node cannot be renamed before asking for a new name.
    #[test]
    fn rename_preparation_rejects_unrenamable_entries() {
        let source = concat!(
            "# Home\n\n[",
            "file:notes.txt] [",
            "file:missing.txt] [",
            "dir:.] [",
            "file:wiki.mull]",
        );
        let wiki = TestWiki::new(source);
        fs::write(wiki.path().parent().unwrap().join("notes.txt"), "notes").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let prepare = |uri: &Uri, character, supports_file_renames| {
            prepare_rename_for_document(
                uri,
                source,
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
            "This editor does not support renaming files.",
        );
        assert_eq!(
            prepare(&uri, 18, true),
            "File `missing.txt` does not exist.",
        );
        assert_eq!(
            prepare(&uri, 37, true),
            "The wiki directory cannot be renamed.",
        );
        assert_eq!(
            prepare(&uri, 45, true),
            "The wiki cannot be renamed through one of its own links.",
        );
    }

    // Rename a linked file and update each link to it in the style it was written.
    #[test]
    fn rename_moves_linked_files() {
        let source = concat!(
            "# Home\n\n[",
            "file:./notes.txt] [",
            "file:notes.txt] [",
            "dir:notes]",
        );
        let wiki = TestWiki::new(source);
        let directory = wiki.path().parent().unwrap();
        fs::write(directory.join("notes.txt"), "notes").unwrap();
        fs::create_dir(directory.join("notes")).unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        let workspace_edit = rename_for_document(
            &uri,
            source,
            7,
            Position::new(2, 4),
            " notes/a[1].txt ",
            ALL_FILE_OPERATIONS,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            apply_filesystem_rename(source, workspace_edit),
            (
                concat!(
                    "# Home\n\n[",
                    "file:./notes/a\\[1\\].txt] [",
                    "file:notes/a\\[1\\].txt] [",
                    "dir:notes]",
                )
                .to_owned(),
                Uri::from_file_path(directory.join("notes.txt")).unwrap(),
                Uri::from_file_path(directory.join("notes/a[1].txt")).unwrap(),
                Vec::new(),
            ),
        );
    }

    // Rename a linked directory and update links to it and to everything within it.
    #[test]
    fn rename_moves_linked_directories() {
        let source = concat!(
            "# Home\n\n[",
            "dir:images/] [",
            "dir:./images/raw] [",
            "file:images/photo.jpg] [",
            "file:images.txt]",
        );
        let wiki = TestWiki::new(source);
        let directory = wiki.path().parent().unwrap();
        fs::create_dir_all(directory.join("images/raw")).unwrap();
        fs::write(directory.join("images/photo.jpg"), "photo").unwrap();
        fs::write(directory.join("images.txt"), "images").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        let workspace_edit = rename_for_document(
            &uri,
            source,
            7,
            Position::new(2, 4),
            "photos",
            ALL_FILE_OPERATIONS,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            apply_filesystem_rename(source, workspace_edit),
            (
                concat!(
                    "# Home\n\n[",
                    "dir:photos/] [",
                    "dir:./photos/raw] [",
                    "file:photos/photo.jpg] [",
                    "file:images.txt]",
                )
                .to_owned(),
                Uri::from_file_path(directory.join("images")).unwrap(),
                Uri::from_file_path(directory.join("photos")).unwrap(),
                Vec::new(),
            ),
        );
    }

    // Treat renaming a filesystem node to its own path, however it is written, as a no-op.
    #[test]
    fn rename_to_same_path_does_nothing() {
        let source = concat!("# Home\n\n[", "file:notes.txt]");
        let wiki = TestWiki::new(source);
        fs::write(wiki.path().parent().unwrap().join("notes.txt"), "notes").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        assert_eq!(
            rename_for_document(
                &uri,
                source,
                7,
                Position::new(2, 3),
                "./notes.txt",
                ALL_FILE_OPERATIONS,
            )
            .unwrap(),
            Some(WorkspaceEdit::default()),
        );
    }

    // Leave missing directories to the client, and delete the outermost directory a rename leaves
    // containing only empty directories, keeping any which will contain the new path or which a
    // directory link names.
    #[test]
    fn rename_creates_and_deletes_directories() {
        let source = concat!(
            "# Home\n\n[",
            "file:a/b/photo.jpg] [",
            "file:c/d/e.txt] [",
            "dir:f] [",
            "file:f/g/h.txt]",
        );
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
                &uri,
                source,
                7,
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

        // Keep a directory which a link names.
        assert_eq!(
            deleted_uris(46, "h.txt", ALL_FILE_OPERATIONS),
            vec![directory_uri("f/g")],
        );

        // Skip deletions when the client cannot perform them.
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

    // Reject filesystem renames which the parser, the filesystem, or the editor cannot support.
    #[test]
    fn rename_rejects_invalid_filesystem_renames() {
        let source = concat!(
            "# Home\n\n[",
            "file:notes.txt] [",
            "dir:images] [",
            "file:missing.txt] [",
            "dir:.] [",
            "file:wiki.mull]",
        );
        let wiki = TestWiki::new(source);
        let directory = wiki.path().parent().unwrap();
        fs::write(directory.join("notes.txt"), "notes").unwrap();
        fs::write(directory.join("other.txt"), "other").unwrap();
        fs::create_dir(directory.join("images")).unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let rename = |uri: &Uri, character, new_name, file_operation_support| {
            rename_for_document(
                uri,
                source,
                7,
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
            "This editor does not support renaming files.",
        );
        assert_eq!(
            rename(&uri, 2, "../notes.txt", ALL_FILE_OPERATIONS),
            "Path `../notes.txt` must not contain `..`.",
        );
        assert_eq!(
            rename(&uri, 2, "./", ALL_FILE_OPERATIONS),
            "A file or directory cannot be renamed to the wiki directory.",
        );
        assert_eq!(
            rename(&uri, 2, "other.txt", ALL_FILE_OPERATIONS),
            "Path `other.txt` already exists.",
        );
        assert_eq!(
            rename(&uri, 2, "notes.txt/inner.txt", ALL_FILE_OPERATIONS),
            "Path `notes.txt` is not a directory.",
        );
        assert_eq!(
            rename(&uri, 18, "images/raw", ALL_FILE_OPERATIONS),
            "Directory `images` cannot be moved into itself.",
        );
        assert_eq!(
            rename(&uri, 31, "found.txt", ALL_FILE_OPERATIONS),
            "File `missing.txt` does not exist.",
        );
        assert_eq!(
            rename(&uri, 50, "elsewhere", ALL_FILE_OPERATIONS),
            "The wiki directory cannot be renamed.",
        );
        assert_eq!(
            rename(&uri, 58, "renamed.mull", ALL_FILE_OPERATIONS),
            "The wiki cannot be renamed through one of its own links.",
        );
    }

    // Apply the single text edit of a code action's workspace edit to a source.
    fn apply_code_action(uri: &Uri, source: &str, action: &CodeActionOrCommand) -> String {
        let CodeActionOrCommand::CodeAction(action) = action else {
            panic!("a code action should not be a bare command");
        };
        let changes = action.edit.as_ref().unwrap().changes.as_ref().unwrap();
        let [edit] = changes[uri].as_slice() else {
            panic!("a code action should make exactly one edit");
        };
        let start = byte_offset(source, edit.range.start).unwrap();
        let end = byte_offset(source, edit.range.end).unwrap();
        let mut applied = source.to_owned();
        applied.replace_range(start..end, &edit.new_text);
        applied
    }

    // Create the missing destination of a text link, resolving its diagnostic.
    #[test]
    fn code_actions_create_missing_nodes() {
        let source = "# Home\n\n[Greeting]";
        let uri = untitled_uri();
        let link_range = Range::new(Position::new(2, 0), Position::new(2, 10));
        let link_diagnostic = diagnostics(&uri, source).remove(0);
        let other_diagnostic = Diagnostic {
            range: Range::new(Position::new(0, 0), Position::new(0, 1)),
            ..link_diagnostic.clone()
        };
        assert_eq!(link_diagnostic.range, link_range);

        let actions = code_action_for_document(
            &uri,
            source,
            Range::new(Position::new(2, 3), Position::new(2, 3)),
            &[other_diagnostic, link_diagnostic.clone()],
        )
        .unwrap();
        let [action] = actions.as_slice() else {
            panic!("a missing destination should have exactly one code action");
        };
        let CodeActionOrCommand::CodeAction(code_action) = action else {
            panic!("a code action should not be a bare command");
        };
        assert_eq!(code_action.title, "Create node `Greeting`");
        assert_eq!(code_action.kind, Some(CodeActionKind::QUICKFIX));
        assert_eq!(code_action.diagnostics, Some(vec![link_diagnostic]));
        assert_eq!(code_action.is_preferred, Some(true));

        // Confirm that the created node makes the wiki valid.
        let applied = apply_code_action(&uri, source, action);
        assert_eq!(applied, "# Home\n\n[Greeting]\n\n# Greeting\n");
        assert!(diagnostics(&uri, &applied).is_empty());
    }

    // Separate the created node with exactly one blank line after a trailing line break.
    #[test]
    fn code_actions_reuse_trailing_line_breaks() {
        let source = "# Home\n\n[Greeting]\n";
        let uri = untitled_uri();
        let actions = code_action_for_document(
            &uri,
            source,
            Range::new(Position::new(2, 1), Position::new(2, 1)),
            &[],
        )
        .unwrap();

        assert_eq!(
            apply_code_action(&uri, source, &actions[0]),
            "# Home\n\n[Greeting]\n\n# Greeting\n",
        );
    }

    // Create a missing home node at the start of the document, where its diagnostic is reported.
    #[test]
    fn code_actions_create_missing_home_nodes() {
        let uri = untitled_uri();
        let document_start = Range::new(Position::new(0, 0), Position::new(0, 0));
        let home_diagnostic = diagnostics(&uri, "").remove(0);
        assert_eq!(home_diagnostic.range, document_start);

        let actions = code_action_for_document(
            &uri,
            "",
            document_start,
            std::slice::from_ref(&home_diagnostic),
        )
        .unwrap();
        let [action] = actions.as_slice() else {
            panic!("a missing home node should have exactly one code action");
        };
        let CodeActionOrCommand::CodeAction(code_action) = action else {
            panic!("a code action should not be a bare command");
        };
        assert_eq!(code_action.title, "Create node `Home`");
        assert_eq!(code_action.diagnostics, Some(vec![home_diagnostic]));
        let applied = apply_code_action(&uri, "", action);
        assert_eq!(applied, "# Home\n");
        assert!(diagnostics(&uri, &applied).is_empty());

        // Separate the home node from the nodes that follow it.
        let source = "# Greeting\n";
        let actions = code_action_for_document(&uri, source, document_start, &[]).unwrap();
        assert_eq!(
            apply_code_action(&uri, source, &actions[0]),
            "# Home\n\n# Greeting\n",
        );
    }

    // Offer to create a home node only when it is missing and the request starts the document.
    #[test]
    fn code_actions_omit_unneeded_home_nodes() {
        let uri = untitled_uri();
        let at = |line, character| {
            let position = Position::new(line, character);
            Range::new(position, position)
        };

        assert!(code_action_for_document(&uri, "# Home\n", at(0, 0), &[]).is_none());
        assert!(code_action_for_document(&uri, "# Greeting\n", at(0, 3), &[]).is_none());
    }

    // Offer to create nodes only for text links whose destinations are missing and declarable.
    #[test]
    fn code_actions_ignore_other_contexts() {
        let source = concat!("# Home\n\n[Home] [] [", "file:notes.txt] prose");
        let uri = untitled_uri();
        let actions_at = |character| {
            let position = Position::new(2, character);
            code_action_for_document(&uri, source, Range::new(position, position), &[])
        };

        assert!(actions_at(1).is_none());
        assert!(actions_at(8).is_none());
        assert!(actions_at(14).is_none());
        assert!(actions_at(29).is_none());
    }

    // Leave filesystem links to ordinary editor and filesystem navigation.
    #[test]
    fn navigation_ignores_filesystem_links() {
        let source = concat!("# Home\n\n[", "file:notes.txt]");
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        assert!(goto_definition_for_document(&uri, source, Position::new(2, 4)).is_none());
        assert!(hover_for_document(&uri, source, Position::new(2, 4)).is_none());
        assert!(references_for_document(&uri, source, Position::new(2, 4), false).is_none());
    }

    // Omit navigation results when the deliberately simple parser cannot produce a wiki.
    #[test]
    fn navigation_requires_parseable_source() {
        let source = "# Home\n\n[Greeting]\n\n# Greeting\n\nUnexpected]";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        assert!(goto_definition_for_document(&uri, source, Position::new(2, 4)).is_none());
        assert!(hover_for_document(&uri, source, Position::new(2, 4)).is_none());
        assert!(references_for_document(&uri, source, Position::new(2, 4), false).is_none());
    }

    #[test]
    fn formatting_replaces_noncanonical_source() {
        let source = "# Zulu\n\n😀\n\n# Home\n\n[Zulu]";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let edits = formatting_for_document(&uri, source).unwrap();

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

        assert!(formatting_for_document(&uri, source).unwrap().is_empty());
    }

    #[test]
    fn formatting_rejects_unparsable_source() {
        let source = "# Home\n😀 ]";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        assert!(formatting_for_document(&uri, source).is_none());
    }

    // Format wikis that parse but fail validation, such as one without a home node.
    #[test]
    fn formatting_supports_invalid_wikis() {
        let source = "# Zulu\n\n# Elsewhere";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let edits = formatting_for_document(&uri, source).unwrap();

        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "# Elsewhere\n\n# Zulu\n");
    }

    // Format a new editor buffer without requiring a filesystem path.
    #[test]
    fn formatting_supports_untitled_wikis() {
        let source = "# Zulu\n\n# Home\n\n[Zulu]";
        let edits = formatting_for_document(&untitled_uri(), source).unwrap();

        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "# Home\n\n[Zulu]\n\n# Zulu\n");
    }

    #[test]
    fn formatting_differences_are_not_diagnostics() {
        let source = "# Zulu\n\n# Home\n\n[Zulu]";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        assert!(diagnostics(&uri, source).is_empty());
    }

    #[test]
    fn source_errors_become_precise_diagnostics() {
        let source = "# Home\n😀 ]";
        let error = parser::parse(Some(Path::new("wiki.mull")), source)
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
        assert_eq!(diagnostic.message, "Unexpected closing link delimiter.");
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
