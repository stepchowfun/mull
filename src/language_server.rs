use crate::{
    cancellation::{CancellationFlag, Outcome},
    error::{Error, Fix, SourceRange},
    escaping::{Context, escape, unescape},
    file_tree::{Visibility, file_tree_walker, visibility},
    format::CodeStr,
    line_index::LineIndex,
    lsp_position::LspPosition,
    parser,
    scoring::traversal_order,
    spelled_path::{DirectoryListings, FileRoot, SpelledPath, entry_identity},
    validator,
    wiki::{FILESYSTEM_LINK_PREFIX, FilesystemTarget, Link, Page, PageTitle, TITLE_PREFIX, Wiki},
};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    ffi::OsStr,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, OnceLock, atomic::AtomicBool, atomic::Ordering},
    time::Duration,
};
use tokio::{sync::oneshot, task::JoinHandle};
use tower_lsp_server::{
    Client, LanguageServer, LspService, Server,
    jsonrpc::{Error as JsonRpcError, Result},
    ls_types::{
        CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams,
        CodeActionProviderCapability, CodeActionResponse, Command, CompletionItem,
        CompletionItemKind, CompletionList, CompletionOptions, CompletionParams,
        CompletionResponse, CompletionTextEdit, DeleteFile, DeleteFileOptions, Diagnostic,
        DiagnosticSeverity, DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
        DidChangeWatchedFilesRegistrationOptions, DidCloseTextDocumentParams,
        DidOpenTextDocumentParams, DidSaveTextDocumentParams, DocumentChangeOperation,
        DocumentChanges, DocumentFormattingParams, DocumentHighlight, DocumentHighlightKind,
        DocumentHighlightParams, DocumentLink, DocumentLinkOptions, DocumentLinkParams,
        DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse, ExecuteCommandParams,
        FileSystemWatcher, GlobPattern, GotoDefinitionParams, GotoDefinitionResponse, Hover,
        HoverContents, HoverParams, HoverProviderCapability, InitializeParams, InitializeResult,
        InitializedParams, LSPAny, Location, LocationLink, MarkupContent, MarkupKind, MessageType,
        OneOf, OptionalVersionedTextDocumentIdentifier, Position, PositionEncodingKind,
        PrepareRenameResponse, Range, ReferenceParams, Registration, RenameFile, RenameOptions,
        RenameParams, ResourceOp, ResourceOperationKind, ServerCapabilities, ServerInfo,
        SymbolInformation, SymbolKind, TextDocumentEdit, TextDocumentIdentifier,
        TextDocumentPositionParams, TextDocumentSyncCapability, TextDocumentSyncKind,
        TextDocumentSyncOptions, TextEdit, Uri, WorkDoneProgressOptions, WorkspaceEdit,
    },
};

// This request lists the bodies of the embeds in a wiki, which the editor sets apart since
// formatting regenerates them. Keep this in sync with [ref:embeds_request_client].
const EMBEDS_REQUEST: &str = "mull/embeds"; // [tag:embeds_request_server]

// This request plans attaching files, such as pasted images, to a wiki: where to create them and
// how to link to them. Keep this in sync with [ref:attach_files_request_client].
const ATTACH_FILES_REQUEST: &str = "mull/attachFiles"; // [tag:attach_files_request_server]

// Wait briefly after edits so filesystem validation doesn't run on every keystroke.
const CHECK_DELAY: Duration = Duration::from_millis(250);

// Limit the page titles offered as completions so responses remain small in large wikis.
const MAX_TITLE_COMPLETIONS: usize = 100;

// These file extensions identify the images which hovering over a filesystem link previews.
const IMAGE_EXTENSIONS: &[&str] = &["bmp", "gif", "ico", "jpeg", "jpg", "png", "svg", "webp"];

// List at most this many entries when previewing a directory, so a large one doesn't flood the
// hover.
const MAX_DIRECTORY_PREVIEW_ENTRIES: usize = 50;

// Scale image previews down to fit within this many pixels, since hovers don't limit their size.
const MAX_IMAGE_WIDTH: usize = 480;
const MAX_IMAGE_HEIGHT: usize = 360;

// These file names, in lowercase, have languages which editors don't recognize by the names
// themselves, so a code block names the language instead. Editors recognize some other names, like
// `dockerfile`.
const FILE_NAME_LANGUAGES: &[(&str, &str)] = &[
    (".bash_profile", "shellscript"),
    (".bashrc", "shellscript"),
    (".editorconfig", "properties"),
    (".env", "dotenv"),
    (".gitattributes", "properties"),
    (".gitconfig", "properties"),
    (".gitignore", "ignore"),
    (".profile", "shellscript"),
    (".zprofile", "shellscript"),
    (".zshrc", "shellscript"),
    ("brewfile", "ruby"),
    ("gemfile", "ruby"),
    ("gnumakefile", "makefile"),
    ("jenkinsfile", "groovy"),
    ("pkgbuild", "shellscript"),
    ("podfile", "ruby"),
    ("rakefile", "ruby"),
    ("vagrantfile", "ruby"),
];

// These file extensions, in lowercase, have languages which editors don't recognize by the
// extensions themselves, so a code block names the language instead. Editors recognize most other
// extensions.
const EXTENSION_LANGUAGES: &[(&str, &str)] = &[
    ("bib", "bibtex"),
    ("cc", "cpp"),
    ("cfg", "properties"),
    ("cjs", "javascript"),
    ("clj", "clojure"),
    ("cljc", "clojure"),
    ("cljs", "clojure"),
    ("cmd", "bat"),
    ("conf", "properties"),
    ("cs", "csharp"),
    ("csx", "csharp"),
    ("cts", "typescript"),
    ("cu", "cuda-cpp"),
    ("cuh", "cuda-cpp"),
    ("cxx", "cpp"),
    ("edn", "clojure"),
    ("env", "dotenv"),
    ("erb", "ruby"),
    ("fs", "fsharp"),
    ("fsi", "fsharp"),
    ("fsx", "fsharp"),
    ("gemspec", "ruby"),
    ("gradle", "groovy"),
    ("h", "cpp"),
    ("hh", "cpp"),
    ("hpp", "cpp"),
    ("hxx", "cpp"),
    ("jl", "julia"),
    ("m", "objective-c"),
    ("mak", "makefile"),
    ("md", "markdown"),
    ("mjs", "javascript"),
    ("mk", "makefile"),
    ("mm", "objective-cpp"),
    ("mts", "typescript"),
    ("ndjson", "jsonl"),
    ("patch", "diff"),
    ("pl", "perl"),
    ("pm", "perl"),
    ("psd1", "powershell"),
    ("psm1", "powershell"),
    ("pyi", "python"),
    ("pyw", "python"),
    ("rake", "ruby"),
    ("rs", "rust"),
    ("rst", "restructuredtext"),
    ("vbs", "vb"),
    ("xaml", "xml"),
    ("xsd", "xml"),
    ("xslt", "xsl"),
    ("yml", "yaml"),
];

// Read at most this many bytes of a text file and show at most this many of its lines when
// previewing it, so a large file doesn't slow down or flood the hover.
const MAX_TEXT_PREVIEW_BYTES: usize = 64 * 1024;
const MAX_TEXT_PREVIEW_LINES: usize = 50;

// This extension command reveals a source range in a document. Keep this in sync with
// [group:reveal_range_command].
const REVEAL_RANGE_COMMAND: &str = "mull.revealRange";

// This extension command reveals a directory in the explorer. Keep this in sync with
// [group:reveal_in_explorer_command].
const REVEAL_IN_EXPLORER_COMMAND: &str = "mull.revealInExplorer";

// This server command rechecks the open wiki named by its argument, as when the filesystem changed
// without the editor reporting it. Keep this in sync with [group:check_wiki_command].
const CHECK_WIKI_COMMAND: &str = "mull.checkWiki";

// This editor command reopens suggestions so the children of a completed directory can be chosen.
const TRIGGER_SUGGEST_COMMAND: &str = "editor.action.triggerSuggest";

// Serve language-server requests over standard input and output until the client disconnects.
pub async fn run() {
    // Keep ANSI terminal escapes out of protocol diagnostics.
    colored::control::set_override(false);

    // Connect the backend, including its custom request, to the standard streams reserved for LSP
    // messages.
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::build(Backend::new)
        .custom_method(EMBEDS_REQUEST, Backend::embeds)
        .custom_method(ATTACH_FILES_REQUEST, Backend::attach_files)
        .finish();
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

    // Replace an editor snapshot and schedule diagnostics for its new generation. The returned
    // channel reports the check's result if its diagnostics are published, and closes otherwise.
    fn store_and_check_document(
        &self,
        snapshot: Arc<Snapshot>,
        delay: Duration,
    ) -> oneshot::Receiver<PublishedCheck> {
        // Prepare the resources owned by the diagnostic task.
        let client = self.client.clone();
        let (result_sender, result_receiver) = oneshot::channel();
        let documents = Arc::clone(&self.documents);
        let diagnostic_snapshot = Arc::clone(&snapshot);
        let cancellation = CancellationFlag::default();
        let check_cancellation = cancellation.clone();

        // Cancel the preceding task and assign a distinct generation to this snapshot.
        let mut open_documents = lock(&self.documents);
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
                if lock(&documents)
                    .get(&diagnostic_snapshot.uri)
                    .is_some_and(|document| document.generation == generation)
                {
                    let diagnostic_count = diagnostics.len();
                    client
                        .publish_diagnostics(
                            diagnostic_snapshot.uri.clone(),
                            diagnostics,
                            Some(diagnostic_snapshot.version),
                        )
                        .await;

                    // Report the result to anyone waiting for it.
                    let _ = result_sender.send(PublishedCheck { diagnostic_count });
                }
            }),
            cancellation,
        });
        result_receiver
    }

    // Recheck open documents after filesystem changes, which their filesystem links may reflect.
    // Documents whose own files changed are skipped, since editor synchronization covers them.
    fn recheck_open_documents(&self, changed_uris: &[&Uri]) {
        // Collect the snapshots before scheduling, without retaining the lock across that
        // operation.
        let snapshots = lock(&self.documents)
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
        lock(&self.documents)
            .get(uri)
            .map(|document| Arc::clone(&document.snapshot))
    }

    // Respond to a language feature request using the latest synchronized snapshot of an open
    // document. The response is computed on a thread which may block, so parsing a large wiki
    // doesn't stall the asynchronous executor.
    async fn respond<T, F>(&self, uri: &Uri, respond: F) -> Result<Option<T>>
    where
        T: Send + 'static,
        F: FnOnce(&Snapshot) -> Result<Option<T>> + Send + 'static,
    {
        // Report nothing for a document which isn't open.
        let Some(snapshot) = self.snapshot(uri) else {
            return Ok(None);
        };

        // Compute the response, and report a panic as an internal error.
        tokio::task::spawn_blocking(move || respond(&snapshot))
            .await
            .unwrap_or_else(|_| Err(JsonRpcError::internal_error()))
    }

    // List the bodies of the embeds in the latest synchronized editor snapshot.
    async fn embeds(&self, params: EmbedsParams) -> Result<Option<Vec<Range>>> {
        self.respond(&params.text_document.uri, |snapshot| {
            Ok(Some(embeds_for_document(snapshot)))
        })
        .await
    }

    // Plan attaching files to the latest synchronized editor snapshot.
    async fn attach_files(&self, params: AttachFilesParams) -> Result<Option<AttachFilesPlan>> {
        self.respond(&params.text_document.uri, move |snapshot| {
            Ok(attach_files_for_document(snapshot, &params.files))
        })
        .await
    }
}

// These are the parameters of the request to attach files.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttachFilesParams {
    text_document: TextDocumentIdentifier,
    files: Vec<AttachedFile>,
}

// This is a file to attach, as it was pasted or dropped.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttachedFile {
    name: String,
    uri: Option<Uri>, // Where the file is, unless only its data was given
}

// This plan attaches files to a wiki by copying each one which isn't already in its file root, and
// inserting the text, which links to them.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AttachFilesPlan {
    copy_uris: Vec<Option<Uri>>, // Where to copy each file, in the order given, if anywhere
    text: String,
}

// These are the parameters of the request for the bodies of the embeds in a wiki.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EmbedsParams {
    text_document: TextDocumentIdentifier,
}

// Lock the open documents, which no thread holds while panicking.
fn lock(
    documents: &Mutex<HashMap<Uri, OpenDocument>>,
) -> MutexGuard<'_, HashMap<Uri, OpenDocument>> {
    documents
        .lock()
        .expect("The open-document mutex shouldn't be poisoned.")
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

// This is the result of a check whose diagnostics were published. A check which is cancelled or
// superseded by a newer one reports nothing.
struct PublishedCheck {
    diagnostic_count: usize,
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

    // Convert an editor position into a byte offset in the contents.
    fn byte_offset(&self, position: Position) -> Option<usize> {
        self.line_index().byte_offset(&self.contents, position)
    }

    // Convert a byte offset in the contents into an editor position.
    fn position(&self, byte_offset: usize) -> Position {
        self.line_index().position(&self.contents, byte_offset)
    }

    // Convert a source range in the contents into an editor range.
    fn range(&self, source_range: SourceRange) -> Range {
        self.line_index().range(&self.contents, source_range)
    }

    // Parse the contents, recovering from syntax errors so the wiki can still be validated and
    // navigated.
    fn parsed(&self) -> &(Wiki, Vec<Error>) {
        self.parsed
            .get_or_init(|| parser::parse(self.path.as_deref(), &self.contents, self.line_index()))
    }

    // Retrieve the wiki parsed with recovery from syntax errors.
    fn wiki(&self) -> &Wiki {
        &self.parsed().0
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
        // Full synchronization places the complete latest snapshot in the final change, which
        // succeeds the document's current snapshot.
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
        // Recheck the latest snapshot immediately. Edits are synchronized before the save, so it
        // already contains the saved contents.
        if let Some(snapshot) = self.snapshot(&params.text_document.uri) {
            self.store_and_check_document(snapshot, Duration::ZERO);
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        // Cancel outstanding work before asking the client to clear this document's diagnostics.
        let document = lock(&self.documents).remove(&params.text_document.uri);
        if let Some(pending_check) = document.and_then(|document| document.pending_check) {
            pending_check.cancel();
        }
        self.client
            .publish_diagnostics(params.text_document.uri, Vec::new(), None)
            .await;
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

    async fn execute_command(&self, params: ExecuteCommandParams) -> Result<Option<LSPAny>> {
        // Reject commands this server doesn't provide.
        if params.command != CHECK_WIKI_COMMAND {
            return Err(JsonRpcError::invalid_params(format!(
                "Unknown command {}.",
                params.command.code_str(),
            )));
        }

        // Recheck the named wiki right away.
        let Some(argument) = params.arguments.into_iter().next() else {
            return Err(JsonRpcError::invalid_params(
                "The command requires the URI of a document to check.",
            ));
        };
        let uri = serde_json::from_value::<Uri>(argument).map_err(|error| {
            JsonRpcError::invalid_params(format!("The argument must be a document URI: {error}."))
        })?;
        let Some(snapshot) = self.snapshot(&uri) else {
            self.client
                .show_message(
                    MessageType::WARNING,
                    format!(
                        "Unable to check {}, since it isn't open.",
                        uri.as_str().code_str(),
                    ),
                )
                .await;
            return Ok(None);
        };
        let result = self.store_and_check_document(Arc::clone(&snapshot), Duration::ZERO);

        // Report the result once it's published, or that a newer check superseded it, without
        // claiming a result which isn't shown in the editor.
        let name = snapshot
            .path
            .as_deref()
            .and_then(Path::file_name)
            .map_or_else(
                || snapshot.uri.as_str().code_str(),
                |name| name.to_string_lossy().code_str(),
            );
        let (message_type, message) = match result.await.map(|check| check.diagnostic_count) {
            Ok(0) => (MessageType::INFO, format!("{name} looks good.")),
            Ok(1) => (MessageType::INFO, format!("{name} has 1 problem.")),
            Ok(count) => (MessageType::INFO, format!("{name} has {count} problems.")),
            Err(_) => (
                MessageType::WARNING,
                format!(
                    "The check of {name} was interrupted by a newer change. Its diagnostics will \
                     update when the newer check finishes.",
                ),
            ),
        };
        self.client.show_message(message_type, message).await;
        Ok(None)
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        // Complete links against the latest synchronized editor snapshot.
        let position = params.text_document_position.position;
        self.respond(
            &params.text_document_position.text_document.uri,
            move |snapshot| {
                Ok(completion_for_document(snapshot, position).map(CompletionResponse::List))
            },
        )
        .await
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        // Resolve the title or text link against the latest synchronized editor snapshot.
        let position = params.text_document_position_params.position;
        self.respond(
            &params.text_document_position_params.text_document.uri,
            move |snapshot| Ok(goto_definition_for_document(snapshot, position)),
        )
        .await
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        // Preview what's at the position, using the latest synchronized editor snapshot.
        let position = params.text_document_position_params.position;
        self.respond(
            &params.text_document_position_params.text_document.uri,
            move |snapshot| Ok(hover_for_document(snapshot, position)),
        )
        .await
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        // Find references to the page under the cursor in the latest synchronized snapshot.
        let position = params.text_document_position.position;
        let include_declaration = params.context.include_declaration;
        self.respond(
            &params.text_document_position.text_document.uri,
            move |snapshot| {
                Ok(references_for_document(
                    snapshot,
                    position,
                    include_declaration,
                ))
            },
        )
        .await
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        // Highlight the wiki occurrences related to the item under the cursor.
        let position = params.text_document_position_params.position;
        self.respond(
            &params.text_document_position_params.text_document.uri,
            move |snapshot| Ok(document_highlight_for_document(snapshot, position)),
        )
        .await
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        // Identify the occurrence that the editor should select for rename, or explain why the
        // filesystem entry a link targets can't be renamed before the user enters a new name.
        let position = params.position;
        let supports_file_renames = self.supports_file_renames.load(Ordering::Relaxed);
        self.respond(&params.text_document.uri, move |snapshot| {
            prepare_rename_for_document(snapshot, position, supports_file_renames)
                .map_err(JsonRpcError::invalid_params)
        })
        .await
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        // Rename the entry at the cursor with whichever file operations the client supports,
        // against the latest snapshot, whose version guards the edit against later changes.
        let position = params.text_document_position.position;
        let new_name = params.new_name;
        let file_operation_support = FileOperationSupport {
            rename: self.supports_file_renames.load(Ordering::Relaxed),
            delete: self.supports_file_deletes.load(Ordering::Relaxed),
        };
        self.respond(
            &params.text_document_position.text_document.uri,
            move |snapshot| {
                rename_for_document(snapshot, position, &new_name, file_operation_support)
                    .map_err(JsonRpcError::invalid_params)
            },
        )
        .await
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        // Render the latest synchronized editor snapshot, leaving unparsable contents unchanged.
        self.respond(&params.text_document.uri, |snapshot| {
            Ok(formatting_for_document(snapshot))
        })
        .await
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        // Describe the pages in the latest synchronized editor snapshot.
        let supports_hierarchy = self
            .supports_hierarchical_document_symbols
            .load(Ordering::Relaxed);
        self.respond(&params.text_document.uri, move |snapshot| {
            Ok(Some(document_symbol_for_document(
                snapshot,
                supports_hierarchy,
            )))
        })
        .await
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        // Offer fixes for the latest synchronized editor snapshot.
        let diagnostics = params.context.diagnostics;
        self.respond(&params.text_document.uri, move |snapshot| {
            Ok(code_action_for_document(snapshot, &diagnostics))
        })
        .await
    }

    async fn document_link(&self, params: DocumentLinkParams) -> Result<Option<Vec<DocumentLink>>> {
        // Make the filesystem links in the latest synchronized editor snapshot clickable.
        self.respond(&params.text_document.uri, |snapshot| {
            Ok(document_link_for_document(snapshot))
        })
        .await
    }
}

// Analyze an editor snapshot without checking its formatting.
fn diagnostics_for_document(
    snapshot: &Snapshot,
    cancellation: &CancellationFlag,
) -> Option<Vec<Diagnostic>> {
    // Validate the wiki even if it has syntax errors, so they don't hide validation errors. Report
    // nothing for a cancelled check, whose errors may cover only part of the wiki.
    let (wiki, syntax_errors) = snapshot.parsed();
    let validation_errors = match validator::validate(
        wiki,
        snapshot.path.as_deref(),
        &snapshot.contents,
        snapshot.line_index(),
        cancellation,
    ) {
        Outcome::Completed(result) => result.err().unwrap_or_default(),
        Outcome::Cancelled => return None,
    };

    // Preserve independent Mull errors as independent editor diagnostics, with syntax errors first.
    let line_index = snapshot.line_index();
    Some(
        syntax_errors
            .iter()
            .chain(&validation_errors)
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

// Complete the link target at an editor position with page titles or filesystem paths.
fn completion_for_document(snapshot: &Snapshot, cursor: Position) -> Option<CompletionList> {
    // Find the link whose target contains the cursor, which is after its opening delimiter and no
    // later than its closing delimiter or, if it isn't closed, the end of its line. Offer nothing
    // outside a link, including in code, which never contains one. Targets are trimmed by the
    // parser, so the text typed so far starts after any leading whitespace.
    let cursor_offset = snapshot.byte_offset(cursor)?;
    let link = snapshot.wiki().links().find(|link| {
        let source_range = link.source_range();
        source_range.start < cursor_offset
            && cursor_offset <= link.closing_delimiter().unwrap_or(source_range.end)
    })?;
    let opening_delimiter = link.source_range().start;
    let closing_delimiter = link.closing_delimiter();
    let target_start = opening_delimiter + '['.len_utf8();
    let typed_target = snapshot.contents[target_start..cursor_offset].trim_start();

    // Complete a filesystem link from the file root.
    if typed_target.starts_with(FILESYSTEM_LINK_PREFIX) {
        return Some(CompletionList {
            is_incomplete: false,
            items: filesystem_link_completions(
                snapshot,
                snapshot.path.as_deref()?,
                typed_target,
                cursor_offset,
                closing_delimiter,
            )?,
        });
    }

    // Decline a filesystem link whose prefix follows the cursor.
    if snapshot.contents[target_start..closing_delimiter.unwrap_or(cursor_offset)]
        .trim_start()
        .starts_with(FILESYSTEM_LINK_PREFIX)
    {
        return None;
    }

    // Complete a text link with the titles of the pages in the wiki which match the typed text,
    // replacing a closed link in its entirety and an unfinished link through the cursor. Offer
    // nothing while the typed text has a backslash which doesn't escape anything.
    Some(text_link_completions(
        snapshot,
        &unescape(typed_target, Context::Link)?,
        SourceRange {
            start: opening_delimiter,
            end: closing_delimiter.map_or(cursor_offset, |closing| closing + ']'.len_utf8()),
        },
    ))
}

// Complete the next component of a filesystem link's typed path, which ends at the cursor, from
// the directory named by the path through its last `/`.
fn filesystem_link_completions(
    snapshot: &Snapshot,
    wiki_path: &Path,
    typed_path: &str,
    cursor: usize,
    closing_delimiter: Option<usize>,
) -> Option<Vec<CompletionItem>> {
    // Resolve the typed directory as a link to it would be, declining paths which escape the file
    // tree or have a backslash which doesn't escape anything.
    let typed_directory = &typed_path[..typed_path.rfind('/').map_or(0, |index| index + 1)];
    let segment_start = cursor - (typed_path.len() - typed_directory.len());
    let directory =
        FilesystemTarget::from_name(&unescape(typed_directory, Context::Link)?, true).ok()?;

    // Derive every filesystem path from the file root, as validation does.
    let Ok(file_root) = FileRoot::new(wiki_path) else {
        return Some(Vec::new());
    };

    // Offer each child of the typed directory which a link could name.
    let mut completions = Vec::new();
    for child in visible_children(&file_root, directory.path()) {
        // Leave a directory's link open for its children, and close a file's link.
        let name = child.name.as_str();
        let escaped_name = escape(name, Context::Link);
        let (label, kind, new_text, replacement_end, command) = if child.is_directory {
            (
                format!("{name}/"),
                CompletionItemKind::FOLDER,
                format!("{escaped_name}/"),
                closing_delimiter.unwrap_or(cursor),
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
                closing_delimiter.map_or(cursor, |offset| offset + ']'.len_utf8()),
                None,
            )
        };

        // Replace the typed component so the editor filters candidates against it.
        completions.push(CompletionItem {
            label,
            kind: Some(kind),
            filter_text: Some(escaped_name),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit::new(
                snapshot.range(SourceRange {
                    start: segment_start,
                    end: replacement_end,
                }),
                new_text,
            ))),
            command,
            ..CompletionItem::default()
        });
    }

    // Present entries deterministically.
    completions.sort_by(|a, b| a.label.cmp(&b.label));
    Some(completions)
}

// Complete a text link with the page titles which contain the typed text's characters in order,
// ignoring case, replacing the link at a source range. If there are too many, offer those which
// start with the typed text first, and mark the list as incomplete so the client asks again as more
// is typed.
fn text_link_completions(
    snapshot: &Snapshot,
    typed_text: &str,
    replacement_source_range: SourceRange,
) -> CompletionList {
    // Collect the matches of each kind in title order, ignoring case, and count all of them.
    let typed_text = typed_text.to_lowercase();
    let mut prefix_matches = Vec::new();
    let mut other_matches = Vec::new();
    for title in snapshot.wiki().pages.keys() {
        let lowercase_title = title.text(Context::Verbatim).to_lowercase();
        if lowercase_title.starts_with(&typed_text) {
            prefix_matches.push(title);
        } else if is_subsequence(&typed_text, &lowercase_title) {
            other_matches.push(title);
        }
    }
    prefix_matches.sort();
    other_matches.sort();
    let match_count = prefix_matches.len() + other_matches.len();

    // Replace the whole link, including its delimiters, so the cursor ends up after the closing
    // `]`.
    let replacement_range = snapshot.range(replacement_source_range);
    CompletionList {
        is_incomplete: match_count > MAX_TITLE_COMPLETIONS,
        items: prefix_matches
            .into_iter()
            .chain(other_matches)
            .take(MAX_TITLE_COMPLETIONS)
            .map(|title| {
                let escaped_title = title.text(Context::Link);
                CompletionItem {
                    label: title.text(Context::Verbatim),
                    kind: Some(CompletionItemKind::REFERENCE),
                    filter_text: Some(format!("[{escaped_title}")),
                    text_edit: Some(CompletionTextEdit::Edit(TextEdit::new(
                        replacement_range,
                        format!("[{escaped_title}]"),
                    ))),
                    ..CompletionItem::default()
                }
            })
            .collect(),
    }
}

// Determine whether the characters of a query appear in a text in order. Matching each character
// of the query as early as possible finds a match whenever one exists.
fn is_subsequence(query: &str, text: &str) -> bool {
    let mut query_characters = query.chars().peekable();
    for character in text.chars() {
        if query_characters.peek() == Some(&character) {
            query_characters.next();
        }
    }
    query_characters.peek().is_none()
}

// Locate the page declared or linked at an editor position.
fn goto_definition_for_document(
    snapshot: &Snapshot,
    cursor: Position,
) -> Option<GotoDefinitionResponse> {
    // Parse only the wiki syntax because navigation doesn't require filesystem validation, and
    // recover from syntax errors so navigation keeps working while they're fixed.
    let (page, origin_source_range) =
        page_at(snapshot, snapshot.byte_offset(cursor)?, LinkExtent::Whole)?;

    // Identify the complete source link or title, and target the start of the destination page with
    // an empty range. Editors preview the source of a nonempty target range alongside the hover
    // preview, and the selection range has to be within the target range.
    let target_range = page_start_range(snapshot, page);
    Some(GotoDefinitionResponse::Link(vec![LocationLink {
        origin_selection_range: Some(snapshot.range(origin_source_range)),
        target_uri: snapshot.uri.clone(),
        target_range,
        target_selection_range: target_range,
    }]))
}

// Preview the page or filesystem entry which a title or link at an editor position names.
fn hover_for_document(snapshot: &Snapshot, cursor: Position) -> Option<Hover> {
    // Parse only the wiki syntax because hovering over a text link or title doesn't require
    // filesystem validation, and recover from syntax errors so previews keep working while they're
    // fixed. A filesystem link may preview a directory or a file instead.
    let wiki = snapshot.wiki();
    let (page, source_range) = match occurrence_at(wiki, snapshot.byte_offset(cursor)?)? {
        Occurrence::Title(page) => (page, page.title_source_range),
        Occurrence::Link(Link::Text {
            title,
            source_range,
        }) => (wiki.pages.get(title)?, *source_range),
        Occurrence::Link(Link::Filesystem {
            target,
            source_range,
        }) => return filesystem_hover(snapshot, target, *source_range),
        Occurrence::Link(Link::Malformed { .. }) | Occurrence::Prose(_) => return None,
    };

    // Show the page as written.
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: code_block(
                "mull",
                &snapshot.contents[page.source_range.start..page.source_range.end],
            ),
        }),
        range: Some(snapshot.range(source_range)),
    })
}

// Put source in a Markdown code block, which the editor highlights as the given language and
// displays in the editor's font. The fence is longer than every backtick run in the source.
fn code_block(language: &str, source: &str) -> String {
    let longest_run = source
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .expect("Splitting text should yield at least one piece.");
    let fence = "`".repeat((longest_run + 1).max(3));
    format!("{fence}{language}\n{source}\n{fence}")
}

// Preview the directory or file a filesystem link names, if it exists as spelled in a saved wiki's
// file root. Images are recognized by their extensions, and text files by their contents. Other
// files have no preview.
fn filesystem_hover(
    snapshot: &Snapshot,
    target: &FilesystemTarget,
    source_range: SourceRange,
) -> Option<Hover> {
    // Find the entry as following the link would.
    let file_root = FileRoot::new(snapshot.path.as_deref()?).ok()?;
    let path = filesystem_link_path(&file_root, target, &mut DirectoryListings::new())?;

    // Use the first kind of preview which applies. Directories come first, since a directory's name
    // may have an image's extension, like `[/images.png/]`. Images come before text files, since
    // some images, like SVGs, are also valid text.
    let value = directory_preview(&file_root, target)
        .or_else(|| image_preview(&path))
        .or_else(|| text_preview(&path))?;
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range: Some(snapshot.range(source_range)),
    })
}

// List a directory's entries as the links which would name them, in a code block like a page
// preview, with directories first. Only so many are listed, followed by a count of the rest. Files
// have no directory preview.
fn directory_preview(file_root: &FileRoot, target: &FilesystemTarget) -> Option<String> {
    // Skip files.
    if !target.is_directory() {
        return None;
    }

    // Describe a directory with nothing to list.
    let mut children = visible_children(file_root, target.path());
    if children.is_empty() {
        return Some("This directory contains no files.".to_owned());
    }

    // List the first entries, keeping each group in name order.
    children.sort_by_key(|child| !child.is_directory);
    let mut lines = children
        .iter()
        .take(MAX_DIRECTORY_PREVIEW_ENTRIES)
        .filter_map(|child| {
            let path = target.path().join(&child.name);
            let child_target =
                FilesystemTarget::from_name(path.to_str()?, child.is_directory).ok()?;
            Some(format!("[{}]", child_target.text(Context::Link)))
        })
        .collect::<Vec<_>>();
    if let Some(remaining) = children.len().checked_sub(MAX_DIRECTORY_PREVIEW_ENTRIES)
        && remaining > 0
    {
        lines.push(format!("…and {remaining} more"));
    }
    Some(code_block("mull", &lines.join("\n")))
}

// Embed an image as HTML, which unlike Markdown can size it. Shrink it to fit within the maximum
// size, keeping its aspect ratio. An image whose size can't be read, such as an SVG, just gets the
// maximum width. Images are recognized by their extensions, and other files have no image preview.
fn image_preview(path: &Path) -> Option<String> {
    // Skip files which aren't images, ignoring the case of their extensions.
    let extension = path.extension()?.to_str()?;
    if !IMAGE_EXTENSIONS
        .iter()
        .any(|image_extension| extension.eq_ignore_ascii_case(image_extension))
    {
        return None;
    }

    // Embed the image, sized to fit.
    let dimensions = match imagesize::size(path) {
        Ok(size) => {
            let (width, height) = image_preview_size(size.width, size.height);
            format!("width=\"{width}\" height=\"{height}\"")
        }
        Err(_) => format!("width=\"{MAX_IMAGE_WIDTH}\""),
    };
    let source = file_uri(path)
        .as_str()
        .replace('&', "&amp;")
        .replace('"', "&quot;");
    Some(format!("<img src=\"{source}\" {dimensions}>"))
}

// Show the first lines of a text file in a code block. A file is text if it's UTF-8 without control
// characters other than line breaks and tabs, and other files have no text preview. Only the start
// of a large file is read, so it may end partway through a character.
fn text_preview(path: &Path) -> Option<String> {
    // Read one byte past the limit to learn whether the file is longer.
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(MAX_TEXT_PREVIEW_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    let is_truncated = bytes.len() > MAX_TEXT_PREVIEW_BYTES;
    bytes.truncate(MAX_TEXT_PREVIEW_BYTES);

    // Decode the text, dropping a character which the limit cut off.
    let text = match str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(error) if is_truncated && error.error_len().is_none() => {
            str::from_utf8(&bytes[..error.valid_up_to()]).ok()?
        }
        Err(_) => return None,
    };
    if text
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return None;
    }

    // Describe a file with nothing to show.
    if text.is_empty() {
        return Some("This file is empty.".to_owned());
    }

    // Show the first lines, followed by an ellipsis if there are more.
    let mut lines = text.lines();
    let block = code_block(
        &code_block_language(path),
        &lines
            .by_ref()
            .take(MAX_TEXT_PREVIEW_LINES)
            .collect::<Vec<_>>()
            .join("\n"),
    );
    Some(if is_truncated || lines.next().is_some() {
        format!("{block}\n\n…")
    } else {
        block
    })
}

// Name the language of a file for a code block. Editors recognize many languages by their usual
// file extensions, like `js`, or by the names of files without extensions, like `Dockerfile`. The
// tables give the names of other languages, like `rust` for `rs`. Editors highlight names they
// don't recognize as plain text. A file without such a name gets plain text explicitly, since
// editors highlight a code block without a language like the document containing it.
fn code_block_language(path: &Path) -> String {
    // Look up the file's name, then its extension, ignoring case.
    let lowercase = |name: Option<&OsStr>| name?.to_str().map(str::to_ascii_lowercase);
    let file_name = lowercase(path.file_name());
    let extension = lowercase(path.extension());
    let look_up = |table: &[(&str, &str)], name: &Option<String>| {
        table
            .iter()
            .find(|(table_name, _)| Some(*table_name) == name.as_deref())
            .map(|(_, language)| (*language).to_owned())
    };
    if let Some(language) = look_up(FILE_NAME_LANGUAGES, &file_name)
        .or_else(|| look_up(EXTENSION_LANGUAGES, &extension))
    {
        return language;
    }

    // Otherwise, use the extension, or the file's name if it has no extension, if that can be a
    // language's name.
    let name = if path.extension().is_some() {
        extension
    } else {
        file_name
    };
    name.filter(|name| {
        !name.is_empty()
            && name.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '_')
            })
    })
    .unwrap_or_else(|| "text".to_owned())
}

// Scale an image's size down to fit within the maximum preview size, keeping its aspect ratio.
// Neither side shrinks below a pixel, and an image which already fits keeps its size.
fn image_preview_size(width: usize, height: usize) -> (usize, usize) {
    if width <= MAX_IMAGE_WIDTH && height <= MAX_IMAGE_HEIGHT {
        (width, height)
    } else if width.saturating_mul(MAX_IMAGE_HEIGHT) > height.saturating_mul(MAX_IMAGE_WIDTH) {
        (
            MAX_IMAGE_WIDTH,
            (height.saturating_mul(MAX_IMAGE_WIDTH) / width).max(1),
        )
    } else {
        (
            (width.saturating_mul(MAX_IMAGE_HEIGHT) / height).max(1),
            MAX_IMAGE_HEIGHT,
        )
    }
}

// Locate every text link to the page at an editor position. Outside a title or link, that's the
// page containing the position.
fn references_for_document(
    snapshot: &Snapshot,
    cursor: Position,
    include_declaration: bool,
) -> Option<Vec<Location>> {
    // Parse only the wiki syntax because finding references doesn't require validation, and recover
    // from syntax errors so references can be found while they're fixed. A link to a missing page,
    // a filesystem link, or a malformed link denotes no page.
    let wiki = snapshot.wiki();
    let page = match occurrence_at(wiki, snapshot.byte_offset(cursor)?)? {
        Occurrence::Title(page) | Occurrence::Prose(page) => page,
        Occurrence::Link(Link::Text { title, .. }) => wiki.pages.get(title)?,
        Occurrence::Link(Link::Filesystem { .. } | Link::Malformed { .. }) => return None,
    };

    // Include the declaration only when requested, then restore source order.
    let mut source_ranges = text_link_source_ranges(wiki, &page.title);
    if include_declaration {
        source_ranges.push(page.title_source_range);
    }
    source_ranges.sort_by_key(|source_range| (source_range.start, source_range.end));

    // Return every occurrence in source order within the current wiki.
    Some(
        source_ranges
            .into_iter()
            .map(|source_range| Location::new(snapshot.uri.clone(), snapshot.range(source_range)))
            .collect(),
    )
}

// Highlight related page or filesystem-link occurrences at an editor position.
fn document_highlight_for_document(
    snapshot: &Snapshot,
    cursor: Position,
) -> Option<Vec<DocumentHighlight>> {
    // Parse only the wiki syntax because document highlights don't require validation, and recover
    // from syntax errors so highlights keep working while they're fixed.
    let wiki = snapshot.wiki();
    let byte_offset = snapshot.byte_offset(cursor)?;

    // Distinguish a page declaration from its references. Filesystem links have no declaration in
    // the wiki, so every matching link is a reference. A link to a missing page, a malformed link,
    // and other text in a page have nothing to highlight.
    let mut highlights = match occurrence_at(wiki, byte_offset)? {
        Occurrence::Title(page) => page_highlights(wiki, page),
        Occurrence::Link(Link::Text { title, .. }) => page_highlights(wiki, wiki.pages.get(title)?),
        Occurrence::Link(Link::Filesystem { target, .. }) => {
            filesystem_link_source_ranges(wiki, target)
                .into_iter()
                .map(|source_range| (source_range, DocumentHighlightKind::READ))
                .collect()
        }
        Occurrence::Link(Link::Malformed { .. }) | Occurrence::Prose(_) => return None,
    };

    // Return every matching source occurrence in wiki order.
    highlights.sort_by_key(|(source_range, _kind)| (source_range.start, source_range.end));
    Some(
        highlights
            .into_iter()
            .map(|(source_range, kind)| DocumentHighlight {
                range: snapshot.range(source_range),
                kind: Some(kind),
            })
            .collect(),
    )
}

// Highlight a page's declaration along with every text link to it.
fn page_highlights(wiki: &Wiki, page: &Page) -> Vec<(SourceRange, DocumentHighlightKind)> {
    let mut highlights = text_link_source_ranges(wiki, &page.title)
        .into_iter()
        .map(|source_range| (source_range, DocumentHighlightKind::READ))
        .collect::<Vec<_>>();
    highlights.push((page.title_source_range, DocumentHighlightKind::WRITE));
    highlights
}

// Collect every complete filesystem-link range with the same target.
fn filesystem_link_source_ranges(wiki: &Wiki, target: &FilesystemTarget) -> Vec<SourceRange> {
    // Match logical paths without resolving symlinks, just as filesystem validation does.
    wiki.links()
        .filter_map(|link| match link {
            Link::Filesystem {
                target: link_target,
                source_range,
            } if link_target == target => Some(*source_range),
            Link::Text { .. } | Link::Filesystem { .. } | Link::Malformed { .. } => None,
        })
        .collect()
}

// Identify the source occurrence that should be selected before renaming a page, file, or
// directory, or explain why the filesystem entry a link targets can't be renamed.
fn prepare_rename_for_document(
    snapshot: &Snapshot,
    cursor: Position,
    supports_file_renames: bool,
) -> std::result::Result<Option<PrepareRenameResponse>, String> {
    // Resolve the cursor in an editor snapshot, which need not pass semantic validation, recovering
    // from syntax errors [ref:rename_despite_syntax_errors].
    let Some(cursor_offset) = snapshot.byte_offset(cursor) else {
        return Ok(None);
    };

    // Select only the path of a filesystem link between its leading `/` and any trailing `/`, and
    // seed the rename prompt with its decoded text as written. The rename writes both slashes
    // itself, so the new name needs neither.
    if let Some(filesystem_entry) =
        renamable_filesystem_entry_at(snapshot, cursor_offset, supports_file_renames)?
    {
        let path_source_range = filesystem_entry.path_source_range;
        let path_source = &snapshot.contents[path_source_range.start..path_source_range.end];
        let start_trimmed = path_source.trim_start_matches('/');
        let trimmed = start_trimmed.trim_end_matches('/');
        let start = path_source_range.end - start_trimmed.len();
        return Ok(Some(PrepareRenameResponse::RangeWithPlaceholder {
            range: snapshot.range(SourceRange {
                start,
                end: start + trimmed.len(),
            }),
            placeholder: unescape(trimmed, Context::Link)
                .expect("A link's escapes should have been checked."),
        }));
    }

    // Otherwise, resolve either a title declaration or text link.
    let Some((page, source_range)) = page_at(snapshot, cursor_offset, LinkExtent::Target) else {
        return Ok(None);
    };

    // Select only the title text and seed the rename prompt with the title as written there.
    Ok(Some(PrepareRenameResponse::RangeWithPlaceholder {
        range: snapshot.range(source_range),
        placeholder: page.title.text(Context::Verbatim),
    }))
}

// This records which file operations the client can perform within a versioned workspace edit.
#[derive(Clone, Copy)]
struct FileOperationSupport {
    rename: bool,
    delete: bool,
}

// Rename the filesystem entry or page at an editor position, along with every link to it.
fn rename_for_document(
    snapshot: &Snapshot,
    cursor: Position,
    new_name: &str,
    file_operation_support: FileOperationSupport,
) -> std::result::Result<Option<WorkspaceEdit>, String> {
    // Resolve the cursor in an editor snapshot, which need not pass semantic validation, recovering
    // from syntax errors. A malformed link to the renamed page isn't updated, but it's reported as
    // a broken link once the syntax errors are fixed, since the old name no longer exists
    // [tag:rename_despite_syntax_errors].
    let Some(cursor_offset) = snapshot.byte_offset(cursor) else {
        return Ok(None);
    };

    // Try the filesystem entry a link targets, and otherwise fall back to a page.
    match rename_filesystem_entry_for_document(
        snapshot,
        cursor_offset,
        new_name,
        file_operation_support,
    ) {
        Ok(None) => rename_page_for_document(snapshot, cursor_offset, new_name),
        result => result,
    }
}

// Rename one page and every text link that targets it.
fn rename_page_for_document(
    snapshot: &Snapshot,
    cursor_offset: usize,
    new_name: &str,
) -> std::result::Result<Option<WorkspaceEdit>, String> {
    // Resolve the page declared or linked at the cursor.
    let Some((page, _source_range)) = page_at(snapshot, cursor_offset, LinkExtent::Target) else {
        return Ok(None);
    };

    // Accept only a title which the parser would accept in a title line and which doesn't already
    // exist.
    let new_title = PageTitle::parse(new_name)?;
    if new_title != page.title && snapshot.wiki().pages.contains_key(&new_title) {
        return Err(format!("Page {} already exists.", new_title.code_str()));
    }

    // Write the title verbatim in its title line, and escaped inside every matching text link.
    let escaped_title = new_title.text(Context::Link);
    let verbatim_title = new_title.text(Context::Verbatim);
    let mut edits = text_link_source_ranges(snapshot.wiki(), &page.title)
        .into_iter()
        .map(|source_range| {
            (
                text_link_target_source_range(&snapshot.contents, source_range),
                escaped_title.as_str(),
            )
        })
        .collect::<Vec<_>>();
    edits.push((page.title_source_range, verbatim_title.as_str()));
    edits.sort_by_key(|(source_range, _new_text)| (source_range.start, source_range.end));

    // Return one non-overlapping edit for each occurrence in the current document.
    Ok(Some(WorkspaceEdit {
        changes: Some(HashMap::from([(
            snapshot.uri.clone(),
            edits
                .into_iter()
                .map(|(source_range, new_text)| {
                    TextEdit::new(snapshot.range(source_range), new_text.to_owned())
                })
                .collect(),
        )])),
        ..WorkspaceEdit::default()
    }))
}

// Rename the file or directory of a filesystem link on disk and update every link to it or, for a
// directory, to anything within it. The client creates any missing directories, and directories
// that contained nothing but the renamed entry are deleted when the client supports it.
fn rename_filesystem_entry_for_document(
    snapshot: &Snapshot,
    cursor_offset: usize,
    new_name: &str,
    file_operation_support: FileOperationSupport,
) -> std::result::Result<Option<WorkspaceEdit>, String> {
    // Find the renamable filesystem entry at the cursor, leaving other positions to pages.
    let Some(RenamableFilesystemEntry {
        file_root,
        old_target,
        old_path,
        ..
    }) = renamable_filesystem_entry_at(snapshot, cursor_offset, file_operation_support.rename)?
    else {
        return Ok(None);
    };

    // Accept only a new path which the parser would accept in a link, for an entry of the same
    // kind. A new target written exactly like the old one, which is spelled as on disk, changes
    // nothing.
    let is_directory = old_target.is_directory();
    let new_target = FilesystemTarget::from_name(new_name.trim(), is_directory)?;
    if new_target == old_target {
        return Ok(Some(WorkspaceEdit::default()));
    }
    if new_target.is_file_root() {
        return Err("A file or directory can't be renamed to `/`.".to_owned());
    }

    // Require the existing directories along the new path to be spelled as they are on disk, as a
    // link would have to be. A filesystem that ignores case would otherwise put the entry in a
    // directory whose path doesn't match the new path as written, and the comparisons below would
    // go wrong. For example, a directory which will contain the entry could look empty after the
    // rename and be deleted along with it. The rest of the new path doesn't exist, other than
    // possibly its final name, so it has no other spelling.
    let (new_ancestor, new_suffix) = file_root
        .spell_existing_ancestor(&new_target)
        .map_err(|error| error.message)?;
    let old_absolute_path = file_root.resolve(&old_path);
    let new_absolute_path = file_root.resolve(&new_ancestor).join(new_suffix);

    // Require the destination to be free and creatable, unless a directory moves into itself. Then
    // everything at its destination moves along with it, so nothing there can conflict. The old
    // path exists, so it's a prefix of the new one exactly when it's one of the new one's existing
    // ancestors.
    let moves_into_itself = is_directory && new_ancestor.starts_with(&old_path);
    if is_directory
        && !moves_into_itself
        && resolves_within(&file_root.resolve(&new_ancestor), &old_absolute_path).unwrap_or(false)
    {
        // A move into itself goes through a temporary sibling, which would leave the symlink
        // leading nowhere, so refuse to move a directory into itself through one.
        return Err(format!(
            "{} can't be moved into itself through a symlink.",
            old_path.code_str(),
        ));
    }
    if !moves_into_itself {
        check_rename_destination(
            &file_root,
            &old_path,
            &new_target,
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
                OneOf::Left(TextEdit::new(snapshot.range(source_range), new_text))
            })
            .collect(),
    });

    // Rename the entry. No filesystem can move a directory into itself directly, so such a move
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
    // directories once the renamed entry has moved.
    if file_operation_support.delete
        && let Some(directory) =
            outermost_directory_emptied_by_rename(&file_root, &old_path, &new_ancestor)
    {
        operations.push(DocumentChangeOperation::Op(ResourceOp::Delete(
            DeleteFile {
                uri: file_uri(&file_root.resolve(&directory)),
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

// This describes the filesystem entry targeted by the link at the cursor, once it's known to be
// renamable regardless of its new name.
struct RenamableFilesystemEntry {
    file_root: FileRoot,
    path_source_range: SourceRange,
    old_target: FilesystemTarget,
    old_path: SpelledPath,
}

// Find the filesystem entry targeted by the link at the cursor and check whether it can be renamed
// at all. Every other position yields no page, leaving it to page renaming.
fn renamable_filesystem_entry_at(
    snapshot: &Snapshot,
    cursor_offset: usize,
    supports_file_renames: bool,
) -> std::result::Result<Option<RenamableFilesystemEntry>, String> {
    // Resolve the filesystem link at the cursor and its target.
    let Some(Occurrence::Link(Link::Filesystem {
        target: old_target,
        source_range,
    })) = occurrence_at(snapshot.wiki(), cursor_offset)
    else {
        return Ok(None);
    };
    let (is_directory, old_path, source_range) =
        (old_target.is_directory(), old_target.path(), *source_range);
    let path_source_range = filesystem_link_path_source_range(&snapshot.contents, source_range);

    // The client renames the entry on disk, which requires a saved wiki and a capable client.
    let Some(wiki_path) = &snapshot.path else {
        return Err("Save the wiki before renaming the files it links to.".to_owned());
    };
    if !supports_file_renames {
        return Err("This editor doesn't support renaming files.".to_owned());
    }

    // Require the linked entry to exist as the kind the link names, resolving it from the file root
    // as validation does.
    let file_root = FileRoot::new(wiki_path)?;
    if old_target.is_file_root() {
        let name = Path::new(file_root.path().file_name().unwrap_or_default());
        return Err(format!("{} can't be renamed.", name.code_str()));
    }
    let kind = if is_directory { "Directory" } else { "File" };
    if !fs::metadata(file_root.path().join(old_path))
        .is_ok_and(|metadata| metadata.is_dir() == is_directory)
    {
        return Err(format!("{kind} {} not found.", old_target.code_str()));
    }

    // Require the path to be spelled as it is on disk, as the checker does. Otherwise, the rename
    // would update only the links spelled like this one, breaking any spelled correctly.
    let old_path = file_root
        .spell(old_target, &mut DirectoryListings::new())
        .map_err(|error| error.message)?;

    Ok(Some(RenamableFilesystemEntry {
        file_root,
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
// an existing file. The new target is written as the user typed it, and its deepest existing
// ancestor is spelled as on disk.
fn check_rename_destination(
    file_root: &FileRoot,
    old_path: &SpelledPath,
    new_target: &FilesystemTarget,
    new_ancestor: &SpelledPath,
    new_absolute_path: &Path,
) -> std::result::Result<(), String> {
    // Refuse to replace another entry. Something exists at the new path if its own metadata can be
    // read, even if it's a broken symlink. On a filesystem that ignores case, such as macOS's
    // default one, the new path may instead name the entry being renamed, spelled differently, as
    // when renaming `photo.jpg` to `Photo.jpg`. Refuse that too, since VS Code treats both
    // spellings as the same file and silently skips the rename while still editing the links,
    // which leaves them misspelled.
    if fs::symlink_metadata(new_absolute_path).is_ok() {
        return Err(
            if entry_identity(new_absolute_path) == entry_identity(&file_root.resolve(old_path)) {
                format!(
                    "{} and {} differ only in case, which VS Code can't rename. Rename it in the \
                        explorer instead, then fix its links.",
                    old_path.code_str(),
                    new_target.code_str(),
                )
            } else {
                format!("{} already exists.", new_target.code_str())
            },
        );
    }

    // Refuse to create a directory beneath an existing file.
    if !file_root.resolve(new_ancestor).is_dir() {
        return Err(format!(
            "Path {} isn't a directory.",
            new_ancestor.code_str(),
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
    for link in wiki.links() {
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
                moved_target.text(Context::Link),
            ));
        }
    }

    // Present the edits in source order.
    edits.sort_by_key(|(source_range, _new_text)| (source_range.start, source_range.end));
    edits
}

// Choose an unused, hidden name beside an entry for a temporary rename. Staying in the same
// directory keeps the rename on the same filesystem.
fn unused_sibling_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("A renamed entry should have a UTF-8 name.");
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

// Build an operation that renames an entry from one absolute path to another.
fn rename_operation(old_path: &Path, new_path: &Path) -> DocumentChangeOperation {
    DocumentChangeOperation::Op(ResourceOp::Rename(RenameFile {
        old_uri: file_uri(old_path),
        new_uri: file_uri(new_path),
        options: None,
        annotation_id: None,
    }))
}

// Find the outermost directory whose only entry is the one being moved, directly or through
// directories whose only entry leads to it. Any other entry keeps a directory, even an empty
// directory or an ignored file. A directory is also kept if it will contain the new path, which,
// since it exists, is when the new path's deepest existing ancestor leads into it, even through a
// symlink, or if that can't be determined. It isn't kept merely because a directory link names
// it, since such a link only stands for the files within the directory. The search never reaches
// the file root itself.
fn outermost_directory_emptied_by_rename(
    file_root: &FileRoot,
    old_path: &SpelledPath,
    new_ancestor: &SpelledPath,
) -> Option<SpelledPath> {
    // Ascend while each directory contains nothing but the entry being moved or deleted from it.
    let new_ancestor_absolute_path = file_root.resolve(new_ancestor);
    let mut emptied_directory = None;
    let mut removed_entry = old_path.clone();
    while let Some(directory) = removed_entry
        .parent()
        .filter(|directory| !directory.is_file_root())
    {
        let absolute_directory = file_root.resolve(&directory);
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
                snapshot.position(snapshot.contents.len()),
            ),
            rendered_wiki,
        )])
    }
}

// Describe every parsed page for editor outlines and document-symbol navigation.
#[allow(
    deprecated,
    reason = "The protocol's DocumentSymbol type retains a required legacy field."
)]
fn document_symbol_for_document(
    snapshot: &Snapshot,
    supports_hierarchy: bool,
) -> DocumentSymbolResponse {
    // Parse syntax without semantic validation, recovering from syntax errors so the pages remain
    // navigable while they're fixed.
    let mut pages = snapshot.wiki().pages.values().collect::<Vec<_>>();
    pages.sort_by_key(|page| page.source_range.start);

    // Use separate page and selection ranges when the client supports hierarchical symbols, and
    // locate each flat symbol by its selection range otherwise. Selecting a symbol leads to the
    // start of its page.
    if supports_hierarchy {
        DocumentSymbolResponse::Nested(
            pages
                .into_iter()
                .map(|page| DocumentSymbol {
                    name: page.title.text(Context::Verbatim),
                    detail: None,
                    kind: SymbolKind::OBJECT,
                    tags: None,
                    deprecated: None,
                    range: snapshot.range(page.source_range),
                    selection_range: page_start_range(snapshot, page),
                    children: None,
                })
                .collect(),
        )
    } else {
        DocumentSymbolResponse::Flat(
            pages
                .into_iter()
                .map(|page| SymbolInformation {
                    name: page.title.text(Context::Verbatim),
                    kind: SymbolKind::OBJECT,
                    tags: None,
                    deprecated: None,
                    location: Location::new(snapshot.uri.clone(), page_start_range(snapshot, page)),
                    container_name: None,
                })
                .collect(),
        )
    }
}

// List the bodies of the embeds in a wiki, from the line after each opening fence through the line
// before its closing fence, omitting empty bodies. Embeds within others are part of their bodies.
fn embeds_for_document(snapshot: &Snapshot) -> Vec<Range> {
    let contents = &snapshot.contents;
    let mut embeds = snapshot
        .wiki()
        .pages
        .values()
        .flat_map(|page| &page.embeds)
        .map(|embed| SourceRange {
            start: contents[embed.source_range.start..]
                .find('\n')
                .map_or(embed.source_range.end, |index| {
                    embed.source_range.start + index + 1
                }),
            end: contents[..embed.source_range.end]
                .rfind('\n')
                .map_or(embed.source_range.start, |index| index + 1),
        })
        .filter(|source_range| source_range.start < source_range.end)
        .collect::<Vec<_>>();
    embeds.sort_by_key(|source_range| source_range.start);
    embeds
        .into_iter()
        .map(|source_range| snapshot.range(source_range))
        .collect()
}

// Plan attaching files to a saved wiki. A file already in the file root is linked where it is.
// Any other file is copied directly into the file root, under its own name unless an entry or
// another attached file already has it.
fn attach_files_for_document(
    snapshot: &Snapshot,
    files: &[AttachedFile],
) -> Option<AttachFilesPlan> {
    // Find or choose a target for each file.
    let file_root = FileRoot::new(snapshot.path.as_deref()?).ok()?;
    let mut targets = Vec::<FilesystemTarget>::new();
    let mut copy_uris = Vec::new();
    for file in files {
        if let Some(target) = file
            .uri
            .as_ref()
            .and_then(|uri| file_root_target(&file_root, uri))
        {
            targets.push(target);
            copy_uris.push(None);
        } else {
            let target = available_file_target(&file_root, &file.name, &targets)?;
            copy_uris.push(Some(file_uri(&file_root.path().join(target.path()))));
            targets.push(target);
        }
    }

    // Link to each file, separating the links with spaces.
    Some(AttachFilesPlan {
        copy_uris,
        text: targets
            .iter()
            .map(|target| format!("[{}]", target.text(Context::Link)))
            .collect::<Vec<_>>()
            .join(" "),
    })
}

// Find the target of a file in the file root, spelled as it is on disk, if a URI locates one.
fn file_root_target(file_root: &FileRoot, uri: &Uri) -> Option<FilesystemTarget> {
    let path = local_path(uri)?;
    let target =
        FilesystemTarget::from_name(path.strip_prefix(file_root.path()).ok()?.to_str()?, false)
            .ok()?;
    let spelled_path = file_root
        .spell(&target, &mut DirectoryListings::new())
        .ok()?;
    if !fs::metadata(file_root.resolve(&spelled_path)).is_ok_and(|metadata| metadata.is_file()) {
        return None;
    }
    FilesystemTarget::from_name(spelled_path.as_path().to_str()?, false).ok()
}

// Name a file directly in the file root, numbering the name's stem if no entry or other attached
// file may already have it, comparing names without regard to case for case-insensitive
// filesystems. A name which a link couldn't name, such as one containing `/`, has no target.
fn available_file_target(
    file_root: &FileRoot,
    file_name: &str,
    taken: &[FilesystemTarget],
) -> Option<FilesystemTarget> {
    // Require a name which links to a file directly in the file root.
    let target = FilesystemTarget::from_name(file_name, false).ok()?;
    if target.path().components().count() != 1 {
        return None;
    }

    // Try the name as given, then with numbered stems.
    let name = Path::new(file_name);
    let stem = name.file_stem()?.to_str()?;
    let extension = name
        .extension()
        .and_then(OsStr::to_str)
        .map_or_else(String::new, |extension| format!(".{extension}"));
    let mut candidate = target;
    let mut number = 0_u64;
    loop {
        // Accept a name which neither an entry nor another attached file has.
        if fs::symlink_metadata(file_root.path().join(candidate.path())).is_err()
            && !taken.iter().any(|other| {
                other.path().to_string_lossy().to_lowercase()
                    == candidate.path().to_string_lossy().to_lowercase()
            })
        {
            return Some(candidate);
        }

        // Number the stem, or count up from the last number.
        number = number.checked_add(1)?;
        candidate =
            FilesystemTarget::from_name(&format!("{stem}-{number}{extension}"), false).ok()?;
    }
}

// Offer the fixes that the checker attached to the diagnostics at an editor range, each resolving
// every diagnostic it was attached to.
#[allow(
    clippy::too_many_lines,
    reason = "Each kind of fix is described where the fixes are turned into code actions."
)]
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
    // recovering from syntax errors since validation reports missing pages despite them.
    let wiki = snapshot.wiki();
    let actions = fixes
        .into_iter()
        .filter_map(|(fix, diagnostics)| {
            let (title, edit, command) = match fix {
                Fix::CreatePage(title) if wiki.pages.contains_key(&title) => return None,
                Fix::CreatePage(title) => {
                    // Insert the page where the formatter would render it, which doesn't move any
                    // other page since the new one has no links. That's after the page it would
                    // follow or, if it would come first, as the home page does, before the first
                    // page. A wiki without pages gets it at the end.
                    let traversal_indices = traversal_order(wiki, Some(&title))
                        .into_iter()
                        .enumerate()
                        .map(|(index, title)| (title, index))
                        .collect::<HashMap<_, _>>();
                    let new_key =
                        Wiki::rendering_order_key(traversal_indices.get(&title).copied(), &title);
                    let preceding_page = wiki
                        .pages
                        .values()
                        .map(|page| {
                            let index = traversal_indices.get(&page.title).copied();
                            (Wiki::rendering_order_key(index, &page.title), page)
                        })
                        .filter(|(key, _page)| *key < new_key)
                        .max_by_key(|(key, _page)| *key);
                    let first_page = wiki
                        .pages
                        .values()
                        .min_by_key(|page| page.source_range.start);
                    let (offset, before_title, after_title) = match (preceding_page, first_page) {
                        (Some((_key, page)), _) => (page.source_range.end, "\n\n", ""),
                        (None, Some(page)) => (page.source_range.start, "", "\n\n"),
                        (None, None) => {
                            let before_title = if snapshot.contents.is_empty()
                                || snapshot.contents.ends_with("\n\n")
                            {
                                ""
                            } else if snapshot.contents.ends_with('\n') {
                                "\n"
                            } else {
                                "\n\n"
                            };
                            (snapshot.contents.len(), before_title, "\n")
                        }
                    };
                    let insertion = snapshot.position(offset);
                    let edit = TextEdit::new(
                        Range::new(insertion, insertion),
                        format!(
                            "{before_title}{TITLE_PREFIX}{}{after_title}",
                            title.text(Context::Verbatim),
                        ),
                    );

                    // Place the cursor right before the new title's `#` once the edit is applied.
                    // The title starts the line after any line breaks inserted before it, or else
                    // starts where the insertion is.
                    let line_breaks = before_title.matches('\n').count();
                    let cursor = if line_breaks == 0 {
                        insertion
                    } else {
                        Position::new(
                            insertion.line
                                + u32::try_from(line_breaks)
                                    .expect("The line breaks before a title should be few."),
                            0,
                        )
                    };
                    let command = reveal_range_command(
                        format!("Reveal page {}", title.code_str()),
                        &snapshot.uri,
                        Range::new(cursor, cursor),
                        RevealType::AtTop,
                    );
                    (
                        format!("Create page {}", title.code_str()),
                        edit,
                        Some(command),
                    )
                }
                Fix::EscapeBackslash(offset) => {
                    // Skip a fix from a stale diagnostic whose backslash no longer needs escaping.
                    let (_wiki, syntax_errors) = snapshot.parsed();
                    if !syntax_errors
                        .iter()
                        .any(|error| error.fix() == Some(&Fix::EscapeBackslash(offset)))
                    {
                        return None;
                    }

                    // Double the backslash so it stands for itself.
                    let position = snapshot.position(offset);
                    (
                        "Escape the backslash".to_owned(),
                        TextEdit::new(Range::new(position, position), "\\".to_owned()),
                        None,
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

// These are the ways the extension can reveal a range, numbered as in VS Code's
// `TextEditorRevealType`.
#[allow(
    dead_code,
    reason = "The type mirrors VS Code's, including the ways not chosen yet."
)]
#[derive(Clone, Copy)]
enum RevealType {
    Default = 0,
    InCenter = 1,
    InCenterIfOutsideViewport = 2,
    AtTop = 3,
}

// Build an editor navigation command for a code action.
fn reveal_range_command(
    title: String,
    uri: &Uri,
    range: Range,
    reveal_type: RevealType,
) -> Command {
    Command::new(
        title,
        REVEAL_RANGE_COMMAND.to_owned(),
        Some(reveal_range_arguments(uri, range, reveal_type)),
    )
}

// Pass the document URI, UTF-16 destination range, and reveal type as positional command
// arguments.
fn reveal_range_arguments(
    uri: &Uri,
    range: Range,
    reveal_type: RevealType,
) -> Vec<serde_json::Value> {
    vec![
        uri.as_str().into(),
        range.start.line.into(),
        range.start.character.into(),
        range.end.line.into(),
        range.end.character.into(),
        (reveal_type as u8).into(),
    ]
}

// Make each filesystem link whose target exists clickable: a file opens in the editor, and a
// directory is revealed in the explorer.
fn document_link_for_document(snapshot: &Snapshot) -> Option<Vec<DocumentLink>> {
    // Resolve filesystem links from a saved wiki's file root, recovering from syntax
    // errors so the links remain clickable while they're fixed.
    let file_root = FileRoot::new(snapshot.path.as_deref()?).ok()?;

    // Link each filesystem link to its target, skipping any that the checker would report.
    let mut listings = DirectoryListings::new();
    let mut document_links = snapshot
        .wiki()
        .links()
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
                range: snapshot.range(*source_range),
                target: Some(filesystem_link_target(&file_root, target, &mut listings)?),
                tooltip: Some(tooltip.to_owned()),
                data: None,
            })
        })
        .collect::<Vec<_>>();

    // Report the links in source order.
    document_links.sort_by_key(|document_link| document_link.range.start);
    Some(document_links)
}

// This describes which part of a resolved text link a caller considers relevant.
#[derive(Clone, Copy)]
enum LinkExtent {
    // The complete link, including its square-bracket delimiters.
    Whole,

    // The link's inner text, which excludes its delimiters.
    Target,
}

// This is what a source offset within a page falls on.
enum Occurrence<'a> {
    // The page's title line, from the `#` through the end of the line, including trailing
    // whitespace. Navigating to a page leaves the cursor before the `#`.
    Title(&'a Page),

    // A link of any kind in the page's content.
    Link(&'a Link),

    // Anything else in the page.
    Prose(&'a Page),
}

// Find what a source offset falls on, if it's within a page. A page includes the offset just past
// its end, so a cursor there still belongs to it.
fn occurrence_at(wiki: &Wiki, byte_offset: usize) -> Option<Occurrence<'_>> {
    // Find the page containing the offset. Its title line may extend past the rest of the page
    // through trailing whitespace.
    let page = wiki.pages.values().find(|page| {
        page.source_range.start <= byte_offset
            && byte_offset <= page.source_range.end.max(page.title_source_range.end)
    })?;
    if byte_offset <= page.title_source_range.end {
        return Some(Occurrence::Title(page));
    }

    // Look for a link among the page's own links. Links never overlap, so at most one contains the
    // offset.
    Some(
        page.links
            .iter()
            .find(|link| {
                let source_range = link.source_range();
                source_range.start <= byte_offset && byte_offset < source_range.end
            })
            .map_or(Occurrence::Prose(page), Occurrence::Link),
    )
}

// Resolve the page denoted by a declaration or text link at a source offset.
fn page_at(
    snapshot: &Snapshot,
    byte_offset: usize,
    link_extent: LinkExtent,
) -> Option<(&Page, SourceRange)> {
    // A declaration contributes only its title's range. A reference reports whichever extent of
    // the link the caller asked for.
    let wiki = snapshot.wiki();
    match occurrence_at(wiki, byte_offset)? {
        Occurrence::Title(page) => Some((page, page.title_source_range)),
        Occurrence::Link(Link::Text {
            title,
            source_range,
        }) => Some((
            wiki.pages.get(title)?,
            match link_extent {
                LinkExtent::Whole => *source_range,
                LinkExtent::Target => {
                    text_link_target_source_range(&snapshot.contents, *source_range)
                }
            },
        )),
        Occurrence::Link(Link::Filesystem { .. } | Link::Malformed { .. })
        | Occurrence::Prose(_) => None,
    }
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

// Collect every complete text-link range that resolves to a title.
fn text_link_source_ranges(wiki: &Wiki, title: &PageTitle) -> Vec<SourceRange> {
    // Links live on pages in an unordered map, so sort their ranges into source order.
    let mut source_ranges = wiki
        .links()
        .filter_map(|link| match link {
            Link::Text {
                title: link_title,
                source_range,
            } if link_title == title => Some(*source_range),
            Link::Text { .. } | Link::Filesystem { .. } | Link::Malformed { .. } => None,
        })
        .collect::<Vec<_>>();
    source_ranges.sort_by_key(|source_range| (source_range.start, source_range.end));
    source_ranges
}

// Find where navigating to a page leads: an empty range at its start, right before its `#`.
fn page_start_range(snapshot: &Snapshot, page: &Page) -> Range {
    let start = snapshot.position(page.source_range.start);
    Range::new(start, start)
}

// This is an entry of a directory in the file tree which a link could name.
struct VisibleChild {
    name: String,
    is_directory: bool,
}

// List the entries of a directory in the file tree which a link could name, sorted by name. The
// walk descends only along the directory, so large subtrees are read only once they're named.
// Ignored entries, names which aren't UTF-8, and directories without files are left out.
fn visible_children(file_root: &FileRoot, directory: &Path) -> Vec<VisibleChild> {
    // Keep only the directory's ancestors and children.
    let mut walker_builder = file_tree_walker(file_root.path());
    walker_builder
        .max_depth(Some(directory.components().count() + 1))
        .filter_entry({
            let file_root = file_root.clone();
            let directory = directory.to_owned();
            move |entry| {
                let path = file_root.entry_path(entry);
                directory.starts_with(path.as_path())
                    || path.as_path().parent() == Some(directory.as_path())
            }
        });

    // Collect the children, skipping the ancestors walked to reach them.
    let mut children = Vec::new();
    for entry in walker_builder.build().flatten() {
        let path = file_root.entry_path(&entry);
        let is_directory = entry
            .file_type()
            .expect("Only standard input lacks a file type.")
            .is_dir();
        let Some(name) = entry.file_name().to_str() else {
            continue;
        };
        if path.as_path().parent() != Some(directory) {
            continue;
        }

        // Omit a directory which a link couldn't name because it contains no files.
        if is_directory
            && !matches!(
                visibility(file_root, &path, &CancellationFlag::default()).assume_completed(),
                Visibility::Visible,
            )
        {
            continue;
        }
        children.push(VisibleChild {
            name: name.to_owned(),
            is_directory,
        });
    }
    children.sort_by(|a, b| a.name.cmp(&b.name));
    children
}

// Find where following a filesystem link should lead: a file opens in the editor, and a directory
// is revealed in the explorer. A link whose target is missing, of the wrong kind, or spelled
// differently than on disk leads nowhere, just as the checker reports it.
fn filesystem_link_target(
    file_root: &FileRoot,
    target: &FilesystemTarget,
    listings: &mut DirectoryListings,
) -> Option<Uri> {
    // Open a file directly, and reveal a directory through the extension by passing its URI as
    // the command's only positional argument.
    let target_uri = file_uri(&filesystem_link_path(file_root, target, listings)?);
    Some(if target.is_directory() {
        command_url(REVEAL_IN_EXPLORER_COMMAND, &[target_uri.as_str().into()])
            .parse()
            .expect("A command URL should be a valid URI.")
    } else {
        target_uri
    })
}

// Find the entry a filesystem link names, if it's spelled as it is on disk and exists as the kind
// of entry the link names.
fn filesystem_link_path(
    file_root: &FileRoot,
    target: &FilesystemTarget,
    listings: &mut DirectoryListings,
) -> Option<PathBuf> {
    let path = file_root.resolve(&file_root.spell(target, listings).ok()?);
    fs::metadata(&path)
        .is_ok_and(|metadata| metadata.is_dir() == target.is_directory())
        .then_some(path)
}

// Encode a command and its positional arguments as a Markdown-safe URI.
fn command_url(command: &str, arguments: &[serde_json::Value]) -> String {
    format!(
        "command:{command}?{}",
        utf8_percent_encode(
            &serde_json::to_string(arguments).expect("JSON values should serialize."),
            NON_ALPHANUMERIC,
        ),
    )
}

// Convert a path within a saved wiki's file root, which is absolute, into a file URI.
fn file_uri(path: &Path) -> Uri {
    Uri::from_file_path(path).expect("A path within a saved wiki's file root should be absolute.")
}

#[cfg(test)]
mod tests {
    use super::{
        AttachedFile, FileOperationSupport, MAX_DIRECTORY_PREVIEW_ENTRIES, MAX_IMAGE_HEIGHT,
        MAX_IMAGE_WIDTH, MAX_TEXT_PREVIEW_BYTES, MAX_TEXT_PREVIEW_LINES, MAX_TITLE_COMPLETIONS,
        RevealType, Snapshot, attach_files_for_document, code_action_for_document,
        code_block_language, completion_for_document, diagnostic_from_error,
        diagnostics_for_document, document_highlight_for_document, document_link_for_document,
        document_symbol_for_document, embeds_for_document, formatting_for_document,
        goto_definition_for_document, hover_for_document, image_preview_size, is_subsequence,
        prepare_rename_for_document, references_for_document, rename_for_document,
    };
    use crate::{
        cancellation::CancellationFlag, error::SourceRange, line_index::LineIndex,
        lsp_position::LspPosition, parser,
    };
    use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
    use std::{
        fmt::Write,
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

    // Construct the URI assigned to a new editor buffer before its first save.
    fn untitled_uri() -> Uri {
        "untitled:Untitled-1".parse().unwrap()
    }

    // Convert an editor position in a test fixture into a byte offset.
    fn byte_offset(source: &str, position: Position) -> Option<usize> {
        LineIndex::new(source).byte_offset(source, position)
    }

    // This guard owns a temporary directory containing a wiki and removes it after each test.
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

        // Expose the file root, creating it so tests can fill it.
        fn directory(&self) -> PathBuf {
            let directory = self.0.with_file_name("wiki_files");
            fs::create_dir_all(&directory).unwrap();
            directory
        }
    }

    impl Drop for TestWiki {
        // Remove the complete fixture directory when the test finishes.
        fn drop(&mut self) {
            fs::remove_dir_all(self.0.parent().unwrap()).unwrap();
        }
    }

    // Analyze ordinary page structure in a new editor buffer.
    #[test]
    fn untitled_text_only_wikis_receive_diagnostics() {
        let source = "# Home\nSee [Missing].";
        let diagnostics = diagnostics(&untitled_uri(), source);

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].message, "Page `Missing` not found.");
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
                "Page `Missing` not found.",
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

    #[test]
    fn source_errors_become_precise_diagnostics() {
        let source = "# Home\n😀 ]";
        let error = parser::parse(
            Some(Path::new("wiki.mull")),
            source,
            &LineIndex::new(source),
        )
        .1
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
        let source = "# Home\n";
        let error = crate::error::Error::new("Something went wrong.", None, None, None, None);
        let diagnostic = diagnostic_from_error(source, &LineIndex::new(source), &error);

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
            Some((
                source,
                &LineIndex::new(source),
                SourceRange { start: 0, end: 9 },
            )),
            None,
            None,
        );
        let diagnostic = diagnostic_from_error(source, &LineIndex::new(source), &error);

        assert_eq!(
            diagnostic.range,
            Range::new(Position::new(0, 0), Position::new(1, 2)),
        );
    }

    // Complete a partial target by replacing the whole link, including its delimiters.
    #[test]
    fn completions_replace_closed_links() {
        let source = "# Home\n\n[Gr]\n\n# Greeting\n\n# Other";
        let completions =
            completion_for_document(&snapshot(&untitled_uri(), source), Position::new(2, 3))
                .unwrap()
                .items;

        assert_eq!(
            completions
                .iter()
                .map(|completion| completion.label.as_str())
                .collect::<Vec<_>>(),
            vec!["Greeting"],
        );
        let Some(CompletionTextEdit::Edit(edit)) = &completions[0].text_edit else {
            panic!("A completion should replace the link.");
        };
        let link_range = Range::new(Position::new(2, 0), Position::new(2, 4));
        assert_eq!(edit.range, link_range);
        assert_eq!(edit.new_text, "[Greeting]");

        // Offer nothing from a cursor before the link's opening delimiter.
        assert!(
            completion_for_document(&snapshot(&untitled_uri(), source), Position::new(2, 0))
                .is_none(),
        );
    }

    // Complete an unfinished link despite a syntax error elsewhere in the wiki.
    #[test]
    fn completions_recover_from_syntax_errors() {
        let source = "# Home\n\n[Gre\n\n# Greeting\n\nUnexpected]";
        let completions =
            completion_for_document(&snapshot(&untitled_uri(), source), Position::new(2, 4))
                .unwrap()
                .items;

        assert!(
            completions
                .iter()
                .any(|completion| completion.label == "Greeting"),
        );
    }

    // Replace an unfinished link from its opening delimiter through the cursor.
    #[test]
    fn completions_support_unfinished_links() {
        let source = "# Home\n\n[Gre\n\n# Greeting";
        let completions =
            completion_for_document(&snapshot(&untitled_uri(), source), Position::new(2, 4))
                .unwrap()
                .items;
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

    // Find an unfinished link's opening delimiter after a closed link on the same line.
    #[test]
    fn completions_follow_closed_links_on_the_same_line() {
        let source = "# Home\n\nSee [Home] and [Gre\n\n# Greeting";
        let completions =
            completion_for_document(&snapshot(&untitled_uri(), source), Position::new(2, 19))
                .unwrap()
                .items;
        let greeting = completions
            .iter()
            .find(|completion| completion.label == "Greeting")
            .unwrap();
        let Some(CompletionTextEdit::Edit(edit)) = &greeting.text_edit else {
            panic!("A completion should replace the unfinished link.");
        };

        assert_eq!(
            edit.range,
            Range::new(Position::new(2, 15), Position::new(2, 19)),
        );
    }

    // Offer the titles containing the typed characters in order, ignoring case, with those starting
    // with the typed text first.
    #[test]
    fn completions_filter_titles() {
        let source = "# Home\n\n[gre\n\n# Greeting\n\n# Agree\n\n# Greek\n\n# Other";
        let completions =
            completion_for_document(&snapshot(&untitled_uri(), source), Position::new(2, 4))
                .unwrap();

        assert!(!completions.is_incomplete);
        assert_eq!(
            completions
                .items
                .iter()
                .map(|completion| completion.label.as_str())
                .collect::<Vec<_>>(),
            vec!["Greek", "Greeting", "Agree"],
        );
    }

    // Match escaped link delimiters in the typed text against the titles that contain them.
    #[test]
    fn completions_filter_titles_with_escapes() {
        let source = "# Home\n\n[A\\[\n\n# A[B]\n\n# AB";
        let completions =
            completion_for_document(&snapshot(&untitled_uri(), source), Position::new(2, 4))
                .unwrap();

        assert_eq!(
            completions
                .items
                .iter()
                .map(|completion| completion.label.as_str())
                .collect::<Vec<_>>(),
            vec!["A[B]"],
        );
    }

    // Offer a limited number of titles, keeping those which start with the typed text, and mark
    // the list as incomplete.
    #[test]
    fn completions_limit_titles() {
        let mut source = "# Home\n\n[x\n\n# X\n".to_owned();
        for i in 0_usize..200 {
            writeln!(source, "\n# Ax {i:03}").unwrap();
        }
        let completions =
            completion_for_document(&snapshot(&untitled_uri(), &source), Position::new(2, 2))
                .unwrap();

        assert!(completions.is_incomplete);
        assert_eq!(completions.items.len(), MAX_TITLE_COMPLETIONS);
        assert_eq!(completions.items[0].label, "X");
        assert_eq!(completions.items[1].label, "Ax 000");
        assert_eq!(
            completions.items[MAX_TITLE_COMPLETIONS - 1].label,
            format!("Ax {:03}", MAX_TITLE_COMPLETIONS - 2),
        );
    }

    // Match a query's characters in order, wherever they appear.
    #[test]
    fn subsequences_match_in_order() {
        assert!(is_subsequence("grt", "greeting"));
        assert!(is_subsequence("", "greeting"));
        assert!(is_subsequence("greeting", "greeting"));
        assert!(!is_subsequence("gtr", "greeting"));
        assert!(!is_subsequence("x", ""));
    }

    // Complete a link at the end of the document, but not from before its opening delimiter or
    // outside of any link.
    #[test]
    fn completions_at_the_end_of_the_document() {
        let source = "# Home\n\n[";

        assert!(
            completion_for_document(&snapshot(&untitled_uri(), source), Position::new(2, 0))
                .is_none(),
        );
        assert!(
            completion_for_document(&snapshot(&untitled_uri(), source), Position::new(2, 1))
                .is_some(),
        );
        assert!(
            completion_for_document(
                &snapshot(&untitled_uri(), "# Home\n\nx"),
                Position::new(2, 1),
            )
            .is_none(),
        );
    }

    // Leave the cursor after a single closing delimiter once a completion has been applied.
    #[test]
    fn completions_do_not_duplicate_closing_delimiters() {
        // Model an editor that has already auto-closed the link the cursor sits inside.
        let source = "# Home\n\n[Gr]\n\n# Greeting";
        let completions =
            completion_for_document(&snapshot(&untitled_uri(), source), Position::new(2, 3))
                .unwrap()
                .items;
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

    // Escape link delimiters when inserting a page title as a completion.
    #[test]
    fn completions_escape_title_delimiters() {
        let source = "# Home\n\n[]\n\n# A[B]";
        let completions =
            completion_for_document(&snapshot(&untitled_uri(), source), Position::new(2, 1))
                .unwrap()
                .items;
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

    // Offer no completions while the typed text has a backslash which doesn't escape anything, as
    // right after typing a backslash, but offer them again once the escape is complete.
    #[test]
    fn completions_wait_for_complete_escapes() {
        let incomplete = "# Home\n\n[A\\\n\n# A[B]";
        let complete = "# Home\n\n[A\\[\n\n# A[B]";

        assert!(
            completion_for_document(&snapshot(&untitled_uri(), incomplete), Position::new(2, 3))
                .is_none(),
        );
        assert!(
            completion_for_document(&snapshot(&untitled_uri(), complete), Position::new(2, 4))
                .unwrap()
                .items
                .iter()
                .any(|completion| completion.label == "A[B]"),
        );
    }

    // Offer no completions in code, even in what looks like a link, but offer them after it.
    #[test]
    fn completions_ignore_code() {
        let source = "# Home\n\n```\n[]\n```\n\n`[` []";

        assert!(
            completion_for_document(&snapshot(&untitled_uri(), source), Position::new(3, 1))
                .is_none(),
        );
        assert!(
            completion_for_document(&snapshot(&untitled_uri(), source), Position::new(6, 2))
                .is_none(),
        );
        assert!(
            completion_for_document(&snapshot(&untitled_uri(), source), Position::new(6, 5))
                .is_some(),
        );
    }

    // Complete a link with an unexpected opening delimiter from its first one, so completing a
    // title which contains the delimiter escapes it and repairs the link.
    #[test]
    fn completions_repair_unexpected_opening_delimiters() {
        let source = "# Home\n\n[a [b\n\n# a [b";
        let completions =
            completion_for_document(&snapshot(&untitled_uri(), source), Position::new(2, 5))
                .unwrap();

        assert_eq!(
            completion_edits(&completions.items),
            vec![(
                "a [b",
                Range::new(Position::new(2, 0), Position::new(2, 5)),
                "[a \\[b]",
            )],
        );
    }

    // Offer page completions only while the cursor is inside a text link target.
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

    // Complete a filesystem link with the visible entries of the file root.
    #[test]
    fn completions_list_filesystem_entries() {
        let source = "# Home\n\n[/]";
        let wiki = TestWiki::new(source);
        let directory = wiki.directory();
        fs::write(directory.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(directory.join("ignored.txt"), "ignored").unwrap();
        fs::write(directory.join("notes.txt"), "notes").unwrap();
        fs::create_dir(directory.join("images")).unwrap();
        fs::write(directory.join("images/photo.jpg"), "photo").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let completions = completion_for_document(&snapshot(&uri, source), Position::new(2, 2))
            .unwrap()
            .items;

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
        fs::write(wiki.directory().join("notes.txt"), "notes").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let completions = completion_for_document(&snapshot(&uri, source), Position::new(2, 4))
            .unwrap()
            .items;

        assert_eq!(
            completion_edits(&completions),
            vec![(
                "notes.txt",
                Range::new(Position::new(2, 2), Position::new(2, 4)),
                "notes.txt]",
            )],
        );
    }

    // Complete only entries within the file root, which may not exist, and never those
    // beside the wiki.
    #[test]
    fn completions_list_only_the_file_root() {
        let source = "# Home\n\n[/";
        let wiki = TestWiki::new(source);
        fs::write(wiki.path().with_file_name("beside.txt"), "beside").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let completions = || {
            completion_for_document(&snapshot(&uri, source), Position::new(2, 2))
                .unwrap()
                .items
        };

        assert_eq!(completion_edits(&completions()), vec![]);
        fs::write(wiki.directory().join("within.txt"), "within").unwrap();
        assert_eq!(
            completion_edits(&completions()),
            vec![(
                "within.txt",
                Range::new(Position::new(2, 2), Position::new(2, 2)),
                "within.txt]",
            )],
        );
    }

    // Complete the children of the directory named by an unfinished link, replacing only the
    // component being typed and omitting directories without files.
    #[test]
    fn completions_list_nested_directories() {
        let source = "# Home\n\n[/images/r";
        let wiki = TestWiki::new(source);
        let directory = wiki.directory();
        fs::create_dir_all(directory.join("images/raw/large")).unwrap();
        fs::write(directory.join("images/raw/large/photo.tiff"), "photo").unwrap();
        fs::create_dir_all(directory.join("images/rejected/nested")).unwrap();
        fs::write(directory.join("images/photo.jpg"), "photo").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let completions = completion_for_document(&snapshot(&uri, source), Position::new(2, 10))
            .unwrap()
            .items;

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
        let directory = wiki.directory();
        fs::create_dir_all(directory.join("a[b]")).unwrap();
        fs::write(directory.join("a[b]/c[d].txt"), "content").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        let completions = completion_for_document(&snapshot(&uri, source), Position::new(2, 2))
            .unwrap()
            .items;
        assert_eq!(
            completion_edits(&completions),
            vec![(
                "a[b]/",
                Range::new(Position::new(2, 2), Position::new(2, 9)),
                "a\\[b\\]/",
            )],
        );

        let completions = completion_for_document(&snapshot(&uri, source), Position::new(2, 9))
            .unwrap()
            .items;
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
        fs::write(wiki.directory().join("notes.txt"), "notes").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        // Ignore titles, escaping paths, and escaped delimiters.
        for cursor in [
            Position::new(0, 4),
            Position::new(2, 5),
            Position::new(2, 10),
        ] {
            assert!(
                completion_for_document(&snapshot(&uri, source), cursor)
                    .is_none_or(|completions| completions.items.is_empty()),
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
        assert_eq!(
            link.origin_selection_range,
            Some(Range::new(Position::new(0, 2), Position::new(0, 6))),
        );
        assert_eq!(
            link.target_selection_range,
            Range::new(Position::new(0, 0), Position::new(0, 0)),
        );
    }

    // Jump from a text link to the start of its destination page.
    #[test]
    fn definitions_target_page_titles() {
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
            Range::new(Position::new(4, 0), Position::new(4, 0)),
        );
        assert_eq!(
            link.target_selection_range,
            Range::new(Position::new(4, 0), Position::new(4, 0)),
        );
    }

    // Preview the complete destination page as written while highlighting the source link.
    #[test]
    fn hovers_preview_pages() {
        let source = "# Home\n\n😀 [Greeting]\n\n# Greeting\n\nLiteral \\[brackets\\] and [Home].";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let hover = hover_for_document(&snapshot(&uri, source), Position::new(2, 5)).unwrap();

        let HoverContents::Markup(contents) = hover.contents else {
            panic!("A page preview should use markup content.");
        };
        assert_eq!(contents.kind, MarkupKind::Markdown);
        assert_eq!(
            contents.value,
            "```mull\n# Greeting\n\nLiteral \\[brackets\\] and [Home].\n```",
        );
        assert_eq!(
            hover.range,
            Some(Range::new(Position::new(2, 3), Position::new(2, 13))),
        );
    }

    // Fence a preview with more backticks than any run in the page, so the page can't end it.
    #[test]
    fn hovers_fence_backticks() {
        let source = "# Home\n\n[Code]\n\n# Code\n\n````\nfn main() {}\n````";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let hover = hover_for_document(&snapshot(&uri, source), Position::new(2, 2)).unwrap();

        let HoverContents::Markup(contents) = hover.contents else {
            panic!("A page preview should use markup content.");
        };
        assert_eq!(
            contents.value,
            "`````mull\n# Code\n\n````\nfn main() {}\n````\n`````",
        );
    }

    // Preview a page directly from its title declaration.
    #[test]
    fn hovers_preview_page_titles() {
        let source = "# Home\n\n[Greeting]\n\n# Greeting\n\nHello!";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let hover = hover_for_document(&snapshot(&uri, source), Position::new(4, 4)).unwrap();

        let HoverContents::Markup(contents) = hover.contents else {
            panic!("A page preview should use markup content.");
        };
        assert_eq!(contents.kind, MarkupKind::Markdown);
        assert_eq!(contents.value, "```mull\n# Greeting\n\nHello!\n```");
        assert_eq!(
            hover.range,
            Some(Range::new(Position::new(4, 2), Position::new(4, 10))),
        );
    }

    // Write the start of a PNG file with the given size, enough for its size to be read.
    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut header = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        header.extend(width.to_be_bytes());
        header.extend(height.to_be_bytes());
        header.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
        header
    }

    // Preview images which filesystem links name, but not binary files, missing images, or images
    // in an unsaved wiki. A directory with an image's extension is previewed as a directory.
    #[test]
    fn hovers_preview_images() {
        let source = "# Home\n\n[/photo.PNG] [/data.bin] [/images.png/] [/missing.png]";
        let wiki = TestWiki::new(source);
        let directory = wiki.directory();
        fs::write(directory.join("photo.PNG"), png_header(100, 50)).unwrap();
        fs::write(directory.join("data.bin"), [0, 1, 2]).unwrap();
        fs::create_dir(directory.join("images.png")).unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        let hover = hover_for_document(&snapshot(&uri, source), Position::new(2, 3)).unwrap();
        let HoverContents::Markup(contents) = hover.contents else {
            panic!("An image preview should use markup content.");
        };
        assert_eq!(contents.kind, MarkupKind::Markdown);
        assert_eq!(
            contents.value,
            format!(
                "<img src=\"{}\" width=\"100\" height=\"50\">",
                Uri::from_file_path(directory.join("photo.PNG"))
                    .unwrap()
                    .as_str(),
            ),
        );
        assert_eq!(
            hover.range,
            Some(Range::new(Position::new(2, 0), Position::new(2, 12))),
        );
        for column in [15, 45] {
            assert!(
                hover_for_document(&snapshot(&uri, source), Position::new(2, column)).is_none(),
            );
        }
        let HoverContents::Markup(contents) =
            hover_for_document(&snapshot(&uri, source), Position::new(2, 30))
                .unwrap()
                .contents
        else {
            panic!("A directory preview should use markup content.");
        };
        assert_eq!(contents.value, "This directory contains no files.");
        assert!(
            hover_for_document(&snapshot(&untitled_uri(), source), Position::new(2, 3)).is_none(),
        );
    }

    // Show text files in code blocks, but not files which are invalid UTF-8 or contain control
    // characters other than line breaks and tabs.
    #[test]
    fn hovers_preview_text_files() {
        let source = "# Home\n\n[/notes.txt] [/empty.txt] [/control.txt] [/latin1.txt]";
        let wiki = TestWiki::new(source);
        let directory = wiki.directory();
        fs::write(directory.join("notes.txt"), "a\tb\r\n```\nc").unwrap();
        fs::write(directory.join("empty.txt"), "").unwrap();
        fs::write(directory.join("control.txt"), "a\u{7}b").unwrap();
        fs::write(directory.join("latin1.txt"), b"caf\xe9").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        assert_eq!(
            hover_markdown(&uri, source, Position::new(2, 3)).unwrap(),
            "````txt\na\tb\n```\nc\n````",
        );
        assert_eq!(
            hover_markdown(&uri, source, Position::new(2, 16)).unwrap(),
            "This file is empty.",
        );
        for column in [28, 43] {
            assert!(hover_markdown(&uri, source, Position::new(2, column)).is_none());
        }
    }

    // Show only the first lines of a long text file, and only the start of a large one, even if it
    // cuts off a character, followed by an ellipsis.
    #[test]
    fn hovers_preview_large_text_files() {
        let source = "# Home\n\n[/long.txt] [/large.txt]";
        let wiki = TestWiki::new(source);
        let directory = wiki.directory();
        fs::write(
            directory.join("long.txt"),
            (0..=MAX_TEXT_PREVIEW_LINES)
                .map(|index| index.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        fs::write(
            directory.join("large.txt"),
            format!("a{}", "é".repeat(MAX_TEXT_PREVIEW_BYTES / 2)),
        )
        .unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        let markdown = hover_markdown(&uri, source, Position::new(2, 3)).unwrap();
        let lines = markdown.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), MAX_TEXT_PREVIEW_LINES + 4);
        assert_eq!(
            lines[MAX_TEXT_PREVIEW_LINES],
            format!("{}", MAX_TEXT_PREVIEW_LINES - 1),
        );
        assert_eq!(lines[MAX_TEXT_PREVIEW_LINES + 3], "…");
        assert_eq!(
            hover_markdown(&uri, source, Position::new(2, 15)).unwrap(),
            format!(
                "```txt\na{}\n```\n\n…",
                "é".repeat(MAX_TEXT_PREVIEW_BYTES / 2 - 1),
            ),
        );
    }

    // Name a file's language by the language which a table gives for its name or extension, or
    // else by its extension or, without one, its name, ignoring case. A file without such a name is
    // plain text.
    #[test]
    fn code_block_languages() {
        for (path, language) in [
            ("Gemfile", "ruby"),
            (".bashrc", "shellscript"),
            ("main.RS", "rust"),
            ("notes.md", "markdown"),
            ("script.JS", "js"),
            ("archive.tar.gz", "gz"),
            ("program.c++", "c++"),
            ("Dockerfile", "dockerfile"),
            (".hidden", "text"),
            ("odd name", "text"),
            ("notes.", "text"),
            ("odd.a`b", "text"),
            ("odd.a b", "text"),
        ] {
            assert_eq!(code_block_language(Path::new(path)), language);
        }
    }

    // Find the Markdown which hovering over a test wiki's source at a position shows, if any.
    fn hover_markdown(uri: &Uri, source: &str, position: Position) -> Option<String> {
        let HoverContents::Markup(contents) =
            hover_for_document(&snapshot(uri, source), position)?.contents
        else {
            panic!("A preview should use markup content.");
        };
        Some(contents.value)
    }

    // List a directory's visible entries as links, with directories first, leaving out ignored
    // entries and directories without files. A missing directory has no preview.
    #[test]
    fn hovers_preview_directories() {
        let source = "# Home\n\n[/] [/b/] [/missing/]";
        let wiki = TestWiki::new(source);
        let directory = wiki.directory();
        fs::write(directory.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(directory.join("ignored.txt"), "ignored").unwrap();
        fs::write(directory.join("a.txt"), "a").unwrap();
        fs::create_dir_all(directory.join("b")).unwrap();
        fs::write(directory.join("b").join("c [1].txt"), "c").unwrap();
        fs::create_dir(directory.join("empty")).unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        assert_eq!(
            hover_markdown(&uri, source, Position::new(2, 1)).unwrap(),
            "```mull\n[/b/]\n[/.gitignore]\n[/a.txt]\n```",
        );
        assert_eq!(
            hover_markdown(&uri, source, Position::new(2, 5)).unwrap(),
            "```mull\n[/b/c \\[1\\].txt]\n```",
        );
        assert!(hover_markdown(&uri, source, Position::new(2, 12)).is_none());
    }

    // List only so many of a directory's entries, followed by a count of the rest.
    #[test]
    fn hovers_preview_large_directories() {
        let source = "# Home\n\n[/]";
        let wiki = TestWiki::new(source);
        for index in 0..MAX_DIRECTORY_PREVIEW_ENTRIES + 2 {
            fs::write(wiki.directory().join(format!("{index:03}.txt")), "").unwrap();
        }
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        let markdown = hover_markdown(&uri, source, Position::new(2, 1)).unwrap();
        let lines = markdown.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), MAX_DIRECTORY_PREVIEW_ENTRIES + 3);
        assert_eq!(lines[1], "[/000.txt]");
        assert_eq!(lines[MAX_DIRECTORY_PREVIEW_ENTRIES + 1], "…and 2 more");
    }

    // Give an image whose size can't be read the maximum preview width.
    #[test]
    fn hovers_preview_unreadable_images() {
        let source = "# Home\n\n[/drawing.svg]";
        let wiki = TestWiki::new(source);
        fs::write(wiki.directory().join("drawing.svg"), "<svg/>").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        let HoverContents::Markup(contents) =
            hover_for_document(&snapshot(&uri, source), Position::new(2, 3))
                .unwrap()
                .contents
        else {
            panic!("An image preview should use markup content.");
        };
        assert!(
            contents
                .value
                .ends_with(&format!(" width=\"{MAX_IMAGE_WIDTH}\">")),
        );
    }

    // Shrink images to fit within the maximum preview size, keeping their aspect ratios, but leave
    // smaller images as they are.
    #[test]
    fn image_preview_sizes() {
        assert_eq!(image_preview_size(100, 50), (100, 50));
        assert_eq!(
            image_preview_size(MAX_IMAGE_WIDTH * 2, MAX_IMAGE_HEIGHT),
            (MAX_IMAGE_WIDTH, MAX_IMAGE_HEIGHT / 2),
        );
        assert_eq!(
            image_preview_size(MAX_IMAGE_WIDTH, MAX_IMAGE_HEIGHT * 3),
            (MAX_IMAGE_WIDTH / 3, MAX_IMAGE_HEIGHT),
        );
        assert_eq!(image_preview_size(100_000, 1), (MAX_IMAGE_WIDTH, 1));
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

    // Distinguish a known page with no references from a cursor that denotes no page.
    #[test]
    fn references_can_be_empty() {
        let source = "# Home\n\nText [Missing] [/notes.txt]";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        for column in [0, 3] {
            assert_eq!(
                references_for_document(&snapshot(&uri, source), Position::new(0, column), false),
                Some(Vec::new()),
            );
        }
        for column in [7, 18] {
            assert!(
                references_for_document(&snapshot(&uri, source), Position::new(2, column), false)
                    .is_none(),
            );
        }
    }

    // Find the links to the page containing the cursor when it isn't on a title or link, but not
    // between pages. A link in the page still denotes the page it names.
    #[test]
    fn references_find_links_to_the_current_page() {
        let source = "# Home\n\n[Greeting]\n\n# Greeting\n\nHello, [Home]!\n\nGoodbye.";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        for position in [Position::new(6, 2), Position::new(8, 8)] {
            assert_eq!(
                references_for_document(&snapshot(&uri, source), position, false)
                    .unwrap()
                    .iter()
                    .map(|location| location.range)
                    .collect::<Vec<_>>(),
                vec![Range::new(Position::new(2, 0), Position::new(2, 10))],
            );
        }
        assert_eq!(
            references_for_document(&snapshot(&uri, source), Position::new(6, 9), false)
                .unwrap()
                .iter()
                .map(|location| location.range)
                .collect::<Vec<_>>(),
            vec![Range::new(Position::new(6, 7), Position::new(6, 13))],
        );
        assert!(
            references_for_document(&snapshot(&uri, source), Position::new(3, 0), false).is_none(),
        );
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

    // Keep navigating a wiki with syntax errors, previewing a page with them as written.
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
            Range::new(Position::new(4, 0), Position::new(4, 0)),
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
            panic!("A page preview should use markup content.");
        };
        assert_eq!(
            contents.value,
            "```mull\n# Greeting\n\nUnexpected] [Home]\n```",
        );
    }

    // Navigate among pages without requiring a new editor buffer to have a path.
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

    // Highlight a page declaration and all its text links from either kind of occurrence.
    #[test]
    fn document_highlights_find_page_occurrences() {
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

    // Highlight an unreferenced declaration while ignoring a cursor outside page occurrences.
    #[test]
    fn document_highlights_distinguish_unreferenced_pages() {
        let source = "# Home\n\nText";
        let uri = untitled_uri();

        for column in [0, 3] {
            assert_eq!(
                document_highlight_for_document(&snapshot(&uri, source), Position::new(0, column)),
                Some(vec![DocumentHighlight {
                    range: Range::new(Position::new(0, 2), Position::new(0, 6)),
                    kind: Some(DocumentHighlightKind::WRITE),
                }]),
            );
        }
        assert!(
            document_highlight_for_document(&snapshot(&uri, source), Position::new(2, 1)).is_none(),
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

    // Rename a page from the start of its title line, where navigating to it leaves the cursor,
    // from the space before its title, or from the end of its title line, even after trailing
    // whitespace, whatever the line break. The title's range, which the rename replaces, extends
    // through the trailing whitespace.
    #[test]
    fn rename_from_the_edges_of_titles() {
        for (source, column, title_end) in [
            ("# Home\n\n[Greeting]\n\n# Greeting", 0, 10),
            ("# Home\n\n[Greeting]\n\n# Greeting", 1, 10),
            ("# Home\n\n[Greeting]\n\n# Greeting", 10, 10),
            ("# Home\n\n[Greeting]\n\n# Greeting  \n\nHi", 12, 12),
            (
                "# Home\r\n\r\n[Greeting]\r\n\r\n# Greeting  \r\n\r\nHi",
                12,
                12,
            ),
        ] {
            assert_eq!(
                prepare_rename_for_document(
                    &snapshot(&untitled_uri(), source),
                    Position::new(4, column),
                    false,
                ),
                Ok(Some(PrepareRenameResponse::RangeWithPlaceholder {
                    range: Range::new(Position::new(4, 2), Position::new(4, title_end)),
                    placeholder: "Greeting".to_owned(),
                })),
            );
            let workspace_edit = rename_for_document(
                &snapshot(&untitled_uri(), source),
                Position::new(4, column),
                "Salutation",
                ALL_FILE_OPERATIONS,
            )
            .unwrap()
            .unwrap();
            assert_eq!(
                workspace_edit.changes.unwrap()[&untitled_uri()]
                    .iter()
                    .map(|edit| (edit.range, edit.new_text.as_str()))
                    .collect::<Vec<_>>(),
                vec![
                    (
                        Range::new(Position::new(2, 1), Position::new(2, 9)),
                        "Salutation",
                    ),
                    (
                        Range::new(Position::new(4, 2), Position::new(4, title_end)),
                        "Salutation",
                    ),
                ],
            );
        }
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

    // Allow renaming the structural home page even though validation will report its absence.
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

    // Reject only syntactically unusable or duplicate page titles during rename.
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
            "A page title can't be empty.",
        );
        assert_eq!(
            rename_for_document(
                &snapshot(&untitled_uri(), source),
                cursor,
                "Hello\nworld",
                ALL_FILE_OPERATIONS,
            )
            .unwrap_err(),
            "A page title can't contain a line break.",
        );
        assert_eq!(
            rename_for_document(
                &snapshot(&untitled_uri(), source),
                cursor,
                "Home",
                ALL_FILE_OPERATIONS,
            )
            .unwrap_err(),
            "Page `Home` already exists.",
        );
        assert_eq!(
            rename_for_document(
                &snapshot(&untitled_uri(), source),
                cursor,
                "/notes.txt",
                ALL_FILE_OPERATIONS,
            )
            .unwrap_err(),
            "A page title can't start with `/`.",
        );
    }

    // Grant every file operation a filesystem rename can use.
    const ALL_FILE_OPERATIONS: FileOperationSupport = FileOperationSupport {
        rename: true,
        delete: true,
    };

    // Apply a filesystem rename's text edits to a source, and return the result with the entry
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
            panic!("A filesystem rename should edit the wiki and then rename one entry.");
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
                    panic!("A filesystem rename should only delete directories after the rename.");
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

    // Rename whichever kind of target the link at the cursor names.
    #[test]
    fn rename_dispatches_by_target_kind() {
        let source = "# Home\n\n[Home] [/notes.txt]";
        let wiki = TestWiki::new(source);
        fs::write(wiki.directory().join("notes.txt"), "notes").unwrap();
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

        // A page is renamed with plain text edits, and a filesystem entry with file operations.
        let page_edit = rename(2, "Start").unwrap();
        assert!(page_edit.changes.is_some() && page_edit.document_changes.is_none());
        let filesystem_entry_edit = rename(10, "renamed.txt").unwrap();
        assert!(
            filesystem_entry_edit.changes.is_none()
                && filesystem_entry_edit.document_changes.is_some(),
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
        let directory = wiki.directory();
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

    // Explain why a filesystem entry can't be renamed before asking for a new name.
    #[test]
    fn rename_preparation_rejects_unrenamable_entries() {
        let source = "# Home\n\n[/notes.txt] [/missing.txt] [/]";
        let wiki = TestWiki::new(source);
        fs::write(wiki.directory().join("notes.txt"), "notes").unwrap();
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
        assert_eq!(prepare(&uri, 14, true), "File `/missing.txt` not found.");
        assert_eq!(prepare(&uri, 29, true), "`wiki_files` can't be renamed.");
    }

    // Rename a linked file and update each link to it in the style it was written.
    #[test]
    fn rename_moves_linked_files() {
        let source = "# Home\n\n[/notes.txt] [/notes.txt] [/notes/]";
        let wiki = TestWiki::new(source);
        let directory = wiki.directory();
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
        let directory = wiki.directory();
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

    // Treat renaming a filesystem entry to its own path, however it's written, as a no-op.
    #[test]
    fn rename_to_same_path_does_nothing() {
        let source = "# Home\n\n[/notes.txt]";
        let wiki = TestWiki::new(source);
        fs::write(wiki.directory().join("notes.txt"), "notes").unwrap();
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
    // nothing but the renamed entry, keeping any which will contain the new path.
    #[test]
    fn rename_creates_and_deletes_directories() {
        let source = "# Home\n\n[/a/b/photo.jpg] [/c/d/e.txt] [/f/] [/f/g/h.txt]";
        let wiki = TestWiki::new(source);
        let directory = wiki.directory();
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

    // Keep a directory that a new path leads into through a symlink, rather than deleting the entry
    // just moved into it, and refuse to move a directory into itself through a symlink.
    #[cfg(unix)]
    #[test]
    fn rename_through_symlinks() {
        use std::os::unix::fs::symlink;

        let source = "# Home\n\n[/d/x.txt] [/d/] [/alias/]";
        let wiki = TestWiki::new(source);
        let directory = wiki.directory();
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
            "`/d` can't be moved into itself through a symlink.",
        );
    }

    // Move a directory into itself through an unused temporary sibling, with the text edit between
    // the two renames, and update links to the directory and to everything within it.
    #[test]
    fn rename_moves_directories_into_themselves() {
        let source = "# Home\n\n[/images/] [/images/photo.jpg] [/notes.txt]";
        let wiki = TestWiki::new(source);
        let directory = wiki.directory();
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
        let source = "# Home\n\n[/notes.txt] [/images/] [/missing.txt] [/] [/NOTES.txt]";
        let wiki = TestWiki::new(source);
        let directory = wiki.directory();
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
            "A file or directory can't be renamed to `/`.",
        );
        assert_eq!(
            rename(&uri, 2, "other.txt", ALL_FILE_OPERATIONS),
            "`/other.txt` already exists.",
        );
        assert_eq!(
            rename(&uri, 2, "notes.txt/inner.txt", ALL_FILE_OPERATIONS),
            "Path `/notes.txt` isn't a directory.",
        );
        assert_eq!(
            rename(&uri, 25, "found.txt", ALL_FILE_OPERATIONS),
            "File `/missing.txt` not found.",
        );
        assert_eq!(
            rename(&uri, 40, "elsewhere", ALL_FILE_OPERATIONS),
            "`wiki_files` can't be renamed.",
        );

        // Reject a link or a new path through an existing directory spelled differently than on
        // disk, which only a filesystem that ignores case finds.
        if fs::metadata(directory.join("IMAGES")).is_ok() {
            assert_eq!(
                rename(&uri, 45, "renamed.txt", ALL_FILE_OPERATIONS),
                "`/NOTES.txt` doesn't match the spelling of any name on disk.",
            );
            assert_eq!(
                rename(&uri, 2, "IMAGES/notes.txt", ALL_FILE_OPERATIONS),
                "`/IMAGES` doesn't match the spelling of any name on disk.",
            );
            assert_eq!(
                rename(&uri, 15, "IMAGES/raw", ALL_FILE_OPERATIONS),
                "`/IMAGES` doesn't match the spelling of any name on disk.",
            );

            // Reject a rename which only changes the case of a name, which VS Code would skip.
            assert_eq!(
                rename(&uri, 2, "NOTES.txt", ALL_FILE_OPERATIONS),
                "`/notes.txt` and `/NOTES.txt` differ only in case, which VS Code can't rename. \
                    Rename it in the explorer instead, then fix its links.",
            );
            assert_eq!(
                rename(&uri, 15, "Images", ALL_FILE_OPERATIONS),
                "`/images` and `/Images` differ only in case, which VS Code can't rename. Rename \
                    it in the explorer instead, then fix its links.",
            );
        }
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
    // page.
    #[test]
    fn formatting_rejects_source_it_would_lose() {
        let source = "# Home\n\n# Home\n\nThis would be lost.\n";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        assert!(formatting_for_document(&snapshot(&uri, source)).is_none());
    }

    // Format wikis that parse but fail validation, such as one without a home page.
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

    // Keep the outline of a wiki with syntax errors.
    #[test]
    fn document_symbols_recover_from_syntax_errors() {
        let source = "# Zebra\n\nUnexpected]\n\n# Alpha";
        let DocumentSymbolResponse::Nested(symbols) =
            document_symbol_for_document(&snapshot(&untitled_uri(), source), true)
        else {
            panic!("Pages should be represented as nested document symbols.");
        };

        assert_eq!(
            symbols
                .iter()
                .map(|symbol| symbol.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Zebra", "Alpha"],
        );
    }

    // List the bodies of top-level embeds in source order, including any embeds within them, and
    // omit empty bodies.
    #[test]
    fn embeds_list_bodies() {
        let source = "# Zebra\n\n::: embed [Alpha]\nFirst\n:::\n\n# Alpha\n\nFirst\n\n\
                      ::: embed [Beta]\n:::\n\n# Beta\n\n:::: embed [Alpha]\n\
                      ::: embed [Beta]\nNested\n:::\n::::";
        assert_eq!(
            embeds_for_document(&snapshot(&untitled_uri(), source)),
            vec![
                Range::new(Position::new(3, 0), Position::new(4, 0)),
                Range::new(Position::new(16, 0), Position::new(19, 0)),
            ],
        );
    }

    // Link to files already in the file root where they are, and copy other files into it under
    // names no entry or other attached file has.
    #[test]
    fn attaching_files_links_or_copies_them() {
        let source = "# Home\n\nTart.\n";
        let wiki = TestWiki::new(source);
        fs::create_dir(wiki.directory().join("photos")).unwrap();
        fs::write(wiki.directory().join("photos/beach.jpg"), "beach").unwrap();
        fs::write(wiki.directory().join("image.png"), "image").unwrap();
        let elsewhere = wiki.path().with_file_name("elsewhere.pdf");
        let saved = snapshot(&Uri::from_file_path(wiki.path()).unwrap(), source);
        let attach = |snapshot: &Snapshot, files: &[(&str, Option<&Path>)]| {
            let files = files
                .iter()
                .map(|&(name, path)| AttachedFile {
                    name: name.to_owned(),
                    uri: path.map(|path| Uri::from_file_path(path).unwrap()),
                })
                .collect::<Vec<_>>();
            attach_files_for_document(snapshot, &files).map(|plan| {
                (
                    plan.copy_uris
                        .iter()
                        .map(|uri| {
                            uri.as_ref()
                                .map(|uri| uri.to_file_path().unwrap().into_owned())
                        })
                        .collect::<Vec<_>>(),
                    plan.text,
                )
            })
        };

        // Number names which are taken, and escape special characters in links.
        assert_eq!(
            attach(
                &saved,
                &[
                    ("image.png", None),
                    ("image.png", None),
                    ("a [b].txt", None),
                ],
            ),
            Some((
                vec![
                    Some(wiki.directory().join("image-1.png")),
                    Some(wiki.directory().join("image-2.png")),
                    Some(wiki.directory().join("a [b].txt")),
                ],
                "[/image-1.png] [/image-2.png] [/a \\[b\\].txt]".to_owned(),
            )),
        );

        // Link to a file in the file root without copying it, and copy a file from elsewhere.
        assert_eq!(
            attach(
                &saved,
                &[
                    (
                        "beach.jpg",
                        Some(&wiki.directory().join("photos/beach.jpg")),
                    ),
                    ("elsewhere.pdf", Some(&elsewhere)),
                ],
            ),
            Some((
                vec![None, Some(wiki.directory().join("elsewhere.pdf"))],
                "[/photos/beach.jpg] [/elsewhere.pdf]".to_owned(),
            )),
        );

        // Decline names a link can't give directly in the file root, and untitled wikis.
        assert_eq!(attach(&saved, &[("a/b.png", None)]), None);
        assert_eq!(
            attach(&snapshot(&untitled_uri(), source), &[("notes.txt", None)]),
            None,
        );
    }

    // Expose pages in source order for saved and untitled editor outlines.
    #[test]
    fn document_symbols_describe_pages() {
        let source = "# Zebra\n\nFirst\n\n# Alpha\n\nSecond";
        let wiki = TestWiki::new(source);
        let uris = [untitled_uri(), Uri::from_file_path(wiki.path()).unwrap()];

        for uri in uris {
            let response = document_symbol_for_document(&snapshot(&uri, source), true);
            let DocumentSymbolResponse::Nested(symbols) = response else {
                panic!("Pages should be represented as nested document symbols.");
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
                Range::new(Position::new(0, 0), Position::new(0, 0)),
            );
            assert_eq!(
                symbols[1].range,
                Range::new(Position::new(4, 0), Position::new(6, 6)),
            );
            assert_eq!(
                symbols[1].selection_range,
                Range::new(Position::new(4, 0), Position::new(4, 0)),
            );

            // Fall back to universally supported flat symbols at the start of each page.
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
                Range::new(Position::new(0, 0), Position::new(0, 0)),
            );
            assert_eq!(
                symbols[1].location.range,
                Range::new(Position::new(4, 0), Position::new(4, 0)),
            );
        }
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
        assert_eq!(arguments[5], RevealType::AtTop as u8);
        let cursor = Position::new(
            u32::try_from(arguments[1].as_u64().unwrap()).unwrap(),
            u32::try_from(arguments[2].as_u64().unwrap()).unwrap(),
        );
        applied.insert(byte_offset(&applied, cursor).unwrap(), '|');
        applied
    }

    // Create the missing destination of text links, resolving every diagnostic the page would fix
    // and no others.
    #[test]
    fn code_actions_create_missing_pages() {
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
        assert_eq!(code_action.title, "Create page `Greeting`");
        assert_eq!(code_action.kind, Some(CodeActionKind::QUICKFIX));
        assert_eq!(code_action.diagnostics, Some(link_diagnostics));
        assert_eq!(code_action.is_preferred, Some(true));

        // Confirm that the created page makes the wiki valid.
        let applied = apply_code_action(&uri, source, action);
        assert_eq!(applied, "# Home\n\n[Greeting] [Greeting]\n\n|# Greeting");
        assert_eq!(
            diagnostics(&uri, &applied.replace('|', "")),
            Vec::<Diagnostic>::new(),
        );
    }

    // Offer to create a missing page despite a syntax error elsewhere in the wiki.
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
            "# Home\n\n[Greeting] Unexpected]\n\n|# Greeting",
        );
    }

    // Insert the created page where the formatter renders it, after the subtrees of its siblings
    // with earlier titles rather than right after the page linking to it.
    #[test]
    fn code_actions_insert_in_rendering_order() {
        let source = concat!(
            "# Home\n\n[Apple] [Cherry] [Banana]\n\n",
            "# Apple\n\n[Apricot]\n\n",
            "# Apricot\n\n",
            "# Cherry\n",
        );
        let uri = untitled_uri();
        let actions =
            code_action_for_document(&snapshot(&uri, source), &diagnostics(&uri, source)).unwrap();

        let applied = apply_code_action(&uri, source, &actions[0]);
        assert_eq!(
            applied,
            concat!(
                "# Home\n\n[Apple] [Cherry] [Banana]\n\n",
                "# Apple\n\n[Apricot]\n\n",
                "# Apricot\n\n",
                "|# Banana\n\n",
                "# Cherry\n",
            ),
        );
        assert_eq!(
            formatting_for_document(&snapshot(&uri, &applied.replace('|', ""))),
            Some(Vec::new()),
        );
    }

    // Insert a page that only unreachable pages link to among them in title order.
    #[test]
    fn code_actions_insert_unreachable_pages_in_title_order() {
        let source = "# Home\n\n[First]\n\n# First\n\n# Alpha\n\n[Missing]\n\n# Zebra\n";
        let uri = untitled_uri();
        let actions =
            code_action_for_document(&snapshot(&uri, source), &diagnostics(&uri, source)).unwrap();

        let applied = apply_code_action(&uri, source, &actions[0]);
        assert_eq!(
            applied,
            "# Home\n\n[First]\n\n# First\n\n# Alpha\n\n[Missing]\n\n|# Missing\n\n# Zebra\n",
        );
        assert_eq!(
            formatting_for_document(&snapshot(&uri, &applied.replace('|', ""))),
            Some(Vec::new()),
        );
    }

    // Place the page from a stale diagnostic according to the current wiki, regardless of where the
    // diagnostic was.
    #[test]
    fn code_actions_append_without_linking_pages() {
        let uri = untitled_uri();
        let stale_diagnostics = diagnostics(&uri, "# Home\n\nSome text first, then [Greeting]\n");
        let source = "# Home\n";

        let actions =
            code_action_for_document(&snapshot(&uri, source), &stale_diagnostics).unwrap();
        assert_eq!(
            apply_code_action(&uri, source, &actions[0]),
            "# Home\n\n|# Greeting\n",
        );
    }

    // Create a missing home page at the start of the document, resolving only its diagnostic
    // among those without source ranges, which are all reported there.
    #[test]
    fn code_actions_create_missing_home_pages() {
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
            panic!("A missing home page should have exactly one code action.");
        };
        let CodeActionOrCommand::CodeAction(code_action) = action else {
            panic!("A code action shouldn't be a bare command.");
        };
        assert_eq!(code_action.title, "Create page `Home`");
        assert_eq!(code_action.diagnostics, Some(vec![home_diagnostic]));
        let applied = apply_code_action(&uri, "", action);
        assert_eq!(applied, "|# Home\n");
        assert_eq!(
            diagnostics(&uri, &applied.replace('|', "")),
            Vec::<Diagnostic>::new(),
        );

        // Separate the home page from the pages that follow it.
        let source = "# Greeting\n";
        let actions =
            code_action_for_document(&snapshot(&uri, source), &diagnostics(&uri, source)).unwrap();
        assert_eq!(
            apply_code_action(&uri, source, &actions[0]),
            "|# Home\n\n# Greeting\n",
        );
    }

    // Skip a fix from a stale diagnostic whose page now exists.
    #[test]
    fn code_actions_skip_stale_fixes() {
        let uri = untitled_uri();
        let stale_diagnostics = diagnostics(&uri, "");

        assert!(
            code_action_for_document(&snapshot(&uri, "# Home\n"), &stale_diagnostics).is_none(),
        );
    }

    // Offer no fixes for diagnostics that declaring a page wouldn't resolve.
    #[test]
    fn code_actions_ignore_other_diagnostics() {
        let source = "# Home\n\n[Home] [] [/notes.txt] prose";
        let uri = untitled_uri();
        let source_diagnostics = diagnostics(&uri, source);
        assert_ne!(source_diagnostics, Vec::<Diagnostic>::new());

        assert!(code_action_for_document(&snapshot(&uri, source), &source_diagnostics).is_none());
    }

    // Offer to escape each backslash which doesn't escape anything, making the wiki valid, but not
    // once a stale diagnostic's backslash is escaped.
    #[test]
    fn code_actions_escape_backslashes() {
        let source = "# Home\n\nC:\\Users\\me";
        let uri = untitled_uri();
        let escape_diagnostics = diagnostics(&uri, source);
        assert_eq!(escape_diagnostics.len(), 2);

        // Apply the edits from the end of the source so the earlier ones stay where they are.
        let actions =
            code_action_for_document(&snapshot(&uri, source), &escape_diagnostics).unwrap();
        assert_eq!(actions.len(), 2);
        let mut applied = source.to_owned();
        for action in actions.iter().rev() {
            let CodeActionOrCommand::CodeAction(action) = action else {
                panic!("A code action shouldn't be a bare command.");
            };
            assert_eq!(action.title, "Escape the backslash");
            assert_eq!(action.command, None);
            let changes = action.edit.as_ref().unwrap().changes.as_ref().unwrap();
            let [edit] = changes[&uri].as_slice() else {
                panic!("A code action should make exactly one edit.");
            };
            assert_eq!(edit.range.start, edit.range.end);
            applied.insert_str(
                byte_offset(source, edit.range.start).unwrap(),
                &edit.new_text,
            );
        }
        assert_eq!(applied, "# Home\n\nC:\\\\Users\\\\me");
        assert_eq!(diagnostics(&uri, &applied), Vec::<Diagnostic>::new());
        assert!(code_action_for_document(&snapshot(&uri, &applied), &escape_diagnostics).is_none());
    }

    // Keep filesystem links clickable in a wiki with syntax errors.
    #[test]
    fn document_links_recover_from_syntax_errors() {
        let source = "# Home\n\n[/notes.txt] Unexpected]";
        let wiki = TestWiki::new(source);
        fs::write(wiki.directory().join("notes.txt"), "notes").unwrap();
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
        let directory = wiki.directory();
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

    // Resolve filesystem links within the file root rather than beside the wiki.
    #[test]
    fn document_links_resolve_within_the_file_root() {
        let source = "# Home\n\n[/beside.txt] [/within.txt]";
        let wiki = TestWiki::new(source);
        fs::write(wiki.path().with_file_name("beside.txt"), "beside").unwrap();
        fs::write(wiki.directory().join("within.txt"), "within").unwrap();
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        let document_links = document_link_for_document(&snapshot(&uri, source)).unwrap();
        assert_eq!(document_links.len(), 1);
        assert_eq!(
            document_links[0].target,
            Some(Uri::from_file_path(wiki.directory().join("within.txt")).unwrap()),
        );
    }
}
