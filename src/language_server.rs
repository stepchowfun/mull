use crate::{
    checker::analyze,
    error::{Error, SourceRange},
    parser,
    wiki::{DIRECTORY_LINK_PREFIX, FILE_LINK_PREFIX, Link, TextNode, Wiki},
};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use std::{
    borrow::Cow,
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex, atomic::AtomicBool, atomic::Ordering},
    time::Duration,
};
use tokio::task::JoinHandle;
use tower_lsp_server::{
    Client, LanguageServer, LspService, Server,
    jsonrpc::{Error as JsonRpcError, Result},
    ls_types::{
        CompletionItem, CompletionItemKind, CompletionOptions, CompletionParams,
        CompletionResponse, CompletionTextEdit, Diagnostic, DiagnosticSeverity,
        DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
        DidSaveTextDocumentParams, DocumentFormattingParams, DocumentHighlight,
        DocumentHighlightKind, DocumentHighlightParams, DocumentSymbol, DocumentSymbolParams,
        DocumentSymbolResponse, GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents,
        HoverParams, HoverProviderCapability, InitializeParams, InitializeResult,
        InitializedParams, Location, LocationLink, MarkupContent, MarkupKind, MessageType, OneOf,
        Position, PositionEncodingKind, PrepareRenameResponse, Range, ReferenceParams,
        RenameOptions, RenameParams, ServerCapabilities, ServerInfo, SymbolInformation, SymbolKind,
        TextDocumentPositionParams, TextDocumentSyncCapability, TextDocumentSyncKind,
        TextDocumentSyncOptions, TextEdit, Uri, WorkDoneProgressOptions, WorkspaceEdit,
    },
};

// Wait briefly after edits so filesystem validation does not run on every keystroke.
const CHECK_DELAY: Duration = Duration::from_millis(250);

// This extension command reveals a source range for clickable text links in hover previews.
// [tag:reveal_range_command] Keep in sync with [file:vscode-extension/extension.js].
const REVEAL_RANGE_COMMAND: &str = "mull.revealRange";

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
    supports_hierarchical_document_symbols: AtomicBool,
}

impl Backend {
    // Construct a backend connected to the editor-side language client.
    fn new(client: Client) -> Self {
        Self {
            client,
            documents: Arc::new(Mutex::new(HashMap::new())),
            supports_hierarchical_document_symbols: AtomicBool::new(false),
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
                    format!("Mull was unable to check the wiki: {error}."),
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

    // Copy the current editor snapshot for a language feature request.
    fn document_contents(&self, uri: &Uri) -> Option<String> {
        self.documents
            .lock()
            .expect("the open-document mutex should not be poisoned")
            .get(uri)
            .map(|document| document.contents.clone())
    }
}

// Respond to protocol requests and notifications required for synchronized diagnostics.
#[allow(
    clippy::unused_async_trait_impl,
    reason = "Some methods mirror the asynchronous language-server interface without awaiting."
)]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // Remember whether document symbols may carry separate full and selection ranges.
        let supports_hierarchical_document_symbols = params
            .capabilities
            .text_document
            .as_ref()
            .and_then(|capabilities| capabilities.document_symbol.as_ref())
            .and_then(|capabilities| capabilities.hierarchical_document_symbol_support)
            .unwrap_or(false);
        self.supports_hierarchical_document_symbols
            .store(supports_hierarchical_document_symbols, Ordering::Relaxed);

        // Advertise the language features implemented by this server.
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec!["[".to_owned()]),
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
            .log_message(MessageType::INFO, "Mull language server initialized.")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        // Confirm that the server began its shutdown handshake.
        self.client
            .log_message(MessageType::INFO, "Mull language server shutting down.")
            .await;
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

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        // Complete text links against the latest synchronized editor snapshot.
        let position_params = params.text_document_position;
        let uri = position_params.text_document.uri;
        let Some(contents) = self.document_contents(&uri) else {
            return Ok(None);
        };
        Ok(
            completions_for_document(&uri, &contents, position_params.position)
                .map(CompletionResponse::Array),
        )
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        // Resolve the text link against the latest synchronized editor snapshot.
        let position_params = params.text_document_position_params;
        let uri = position_params.text_document.uri;
        let Some(contents) = self.document_contents(&uri) else {
            return Ok(None);
        };
        Ok(definition_for_document(
            &uri,
            &contents,
            position_params.position,
        ))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        // Preview the text-link target from the latest synchronized editor snapshot.
        let position_params = params.text_document_position_params;
        let uri = position_params.text_document.uri;
        let Some(contents) = self.document_contents(&uri) else {
            return Ok(None);
        };
        Ok(hover_for_document(
            &uri,
            &contents,
            position_params.position,
        ))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        // Find references to the node under the cursor in the latest synchronized snapshot.
        let position_params = params.text_document_position;
        let uri = position_params.text_document.uri;
        let Some(contents) = self.document_contents(&uri) else {
            return Ok(None);
        };
        Ok(references_for_document(
            &uri,
            &contents,
            position_params.position,
            params.context.include_declaration,
        ))
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        // Highlight the wiki occurrences related to the item under the cursor.
        let position_params = params.text_document_position_params;
        let uri = position_params.text_document.uri;
        let Some(contents) = self.document_contents(&uri) else {
            return Ok(None);
        };
        Ok(document_highlights_for_document(
            &uri,
            &contents,
            position_params.position,
        ))
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        // Identify the node occurrence that the editor should select for rename.
        let uri = params.text_document.uri;
        let Some(contents) = self.document_contents(&uri) else {
            return Ok(None);
        };
        Ok(prepare_rename_for_document(
            &uri,
            &contents,
            params.position,
        ))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        // Rename a node and all of its text-link occurrences in the latest snapshot.
        let position_params = params.text_document_position;
        let uri = position_params.text_document.uri;
        let Some(contents) = self.document_contents(&uri) else {
            return Ok(None);
        };
        rename_for_document(&uri, &contents, position_params.position, &params.new_name)
            .map_err(JsonRpcError::invalid_params)
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        // Copy the latest snapshot without retaining the document lock while formatting.
        let uri = params.text_document.uri;
        let snapshot = self
            .documents
            .lock()
            .expect("the open-document mutex should not be poisoned")
            .get(&uri)
            .map(|document| (document.contents.clone(), document.generation));
        let Some((contents, generation)) = snapshot else {
            return Ok(None);
        };
        let wiki_path = local_path(&uri).map(Cow::into_owned);

        // Parse, validate, and render outside the asynchronous executor.
        let formatting_result = tokio::task::spawn_blocking(move || {
            formatting_edit(wiki_path.as_deref(), &contents).map_err(|_errors| ())
        })
        .await;
        let Ok(Ok(edit)) = formatting_result else {
            return Ok(None);
        };

        // Discard an edit calculated from contents that changed while formatting was underway.
        let is_current = self
            .documents
            .lock()
            .expect("the open-document mutex should not be poisoned")
            .get(&uri)
            .is_some_and(|document| document.generation == generation);
        if !is_current {
            return Ok(None);
        }

        // A successful request returns either one whole-document edit or an empty edit list.
        Ok(Some(edit.into_iter().collect()))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        // Describe the nodes in the latest synchronized editor snapshot.
        let uri = params.text_document.uri;
        let Some(contents) = self.document_contents(&uri) else {
            return Ok(None);
        };
        Ok(document_symbols_for_document(
            &uri,
            &contents,
            self.supports_hierarchical_document_symbols
                .load(Ordering::Relaxed),
        ))
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

// Convert only file-scheme URIs because the URI library does not enforce this distinction.
fn local_path(uri: &Uri) -> Option<Cow<'_, Path>> {
    uri.scheme()
        .as_str()
        .eq_ignore_ascii_case("file")
        .then(|| uri.to_file_path())
        .flatten()
}

// Produce a whole-document formatting edit after applying Mull's normal validation rules.
fn formatting_edit(
    source_path: Option<&Path>,
    source_contents: &str,
) -> std::result::Result<Option<TextEdit>, Vec<Error>> {
    // Render the validated wiki and avoid an edit when its source is already canonical.
    let rendered_wiki = analyze(source_path, source_contents)?.to_string();
    if source_contents == rendered_wiki {
        Ok(None)
    } else {
        Ok(Some(TextEdit::new(
            Range::new(
                Position::new(0, 0),
                position(source_contents, source_contents.len()),
            ),
            rendered_wiki,
        )))
    }
}

// Describe every parsed text node for editor outlines and document-symbol navigation.
#[allow(
    deprecated,
    reason = "The protocol's DocumentSymbol type retains a required legacy field."
)]
fn document_symbols_for_document(
    uri: &Uri,
    source_contents: &str,
    supports_hierarchy: bool,
) -> Option<DocumentSymbolResponse> {
    // Parse syntax without semantic validation so structurally valid nodes remain navigable.
    let wiki_path = local_path(uri);
    let wiki = parser::parse(wiki_path.as_deref(), source_contents).ok()?;
    let mut nodes = wiki.text_nodes.values().collect::<Vec<_>>();
    nodes.sort_by_key(|node| node.source_range.start);

    // Use separate node and title ranges when the client supports hierarchical symbols.
    let symbols = nodes
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
        .collect::<Vec<_>>();
    if supports_hierarchy {
        Some(DocumentSymbolResponse::Nested(symbols))
    } else {
        Some(DocumentSymbolResponse::Flat(
            symbols
                .into_iter()
                .map(|symbol| SymbolInformation {
                    name: symbol.name,
                    kind: symbol.kind,
                    tags: symbol.tags,
                    deprecated: None,
                    location: Location::new(uri.clone(), symbol.selection_range),
                    container_name: None,
                })
                .collect(),
        ))
    }
}

// Complete the text-link target at an editor position with every node title.
fn completions_for_document(
    uri: &Uri,
    source_contents: &str,
    cursor: Position,
) -> Option<Vec<CompletionItem>> {
    // Parse either the original source or a temporary source with the active link closed.
    let byte_offset = byte_offset(source_contents, cursor)?;
    let wiki_path = local_path(uri);
    let (wiki, replacement_source_range) =
        completion_context(wiki_path.as_deref(), source_contents, byte_offset)?;

    // Present node titles deterministically and replace the link's inner text and terminator.
    let replacement_range = lsp_range(source_contents, replacement_source_range);
    let mut titles = wiki
        .text_nodes
        .keys()
        .filter(|title| is_text_link_title(title))
        .collect::<Vec<_>>();
    titles.sort();
    Some(
        titles
            .into_iter()
            .map(|title| {
                let escaped_title = escape_text_link_title(title);
                CompletionItem {
                    label: title.clone(),
                    kind: Some(CompletionItemKind::REFERENCE),
                    filter_text: Some(escaped_title.clone()),
                    text_edit: Some(CompletionTextEdit::Edit(TextEdit::new(
                        replacement_range,
                        format!("{escaped_title}]"),
                    ))),
                    ..CompletionItem::default()
                }
            })
            .collect(),
    )
}

// Parse enough of an active text link to identify the source range a completion should replace.
fn completion_context(
    source_path: Option<&Path>,
    source_contents: &str,
    byte_offset: usize,
) -> Option<(Wiki, SourceRange)> {
    // Prefer the unchanged source when the active link is already closed.
    if let Ok(wiki) = parser::parse(source_path, source_contents)
        && let Some(target_source_range) = text_link_target_at(&wiki, source_contents, byte_offset)
    {
        // Absorb the existing terminator, which the completion reinstates after the inserted title.
        return Some((
            wiki,
            SourceRange {
                start: target_source_range.start,
                end: target_source_range.end + ']'.len_utf8(),
            },
        ));
    }

    // Close a link at the cursor temporarily so completion works while it is being authored.
    let mut completed_source = source_contents.to_owned();
    completed_source.insert(byte_offset, ']');
    let wiki = parser::parse(source_path, &completed_source).ok()?;
    let target_source_range = text_link_target_at(&wiki, &completed_source, byte_offset)?;

    // Keep the range inside the original source, which has no terminator to absorb.
    Some((wiki, target_source_range))
}

// Locate the editable inner text of a text link containing a byte offset.
fn text_link_target_at(
    wiki: &Wiki,
    source_contents: &str,
    byte_offset: usize,
) -> Option<SourceRange> {
    // Require the cursor to be within the target rather than on the opening delimiter.
    let (_title, link_source_range) = text_link_at(wiki, byte_offset)?;
    let target_source_range = text_link_target_source_range(source_contents, link_source_range)?;
    (target_source_range.start <= byte_offset && byte_offset <= target_source_range.end)
        .then_some(target_source_range)
}

// Locate the destination of a text link at an editor position.
fn definition_for_document(
    uri: &Uri,
    source_contents: &str,
    cursor: Position,
) -> Option<GotoDefinitionResponse> {
    // Parse only the wiki syntax because navigation does not require filesystem validation.
    let wiki_path = local_path(uri);
    let wiki = parser::parse(wiki_path.as_deref(), source_contents).ok()?;
    let (node, link_source_range) = linked_node_at(&wiki, source_contents, cursor)?;

    // Identify the complete source link and destination node while selecting its title on arrival.
    Some(GotoDefinitionResponse::Link(vec![LocationLink {
        origin_selection_range: Some(lsp_range(source_contents, link_source_range)),
        target_uri: uri.clone(),
        target_range: lsp_range(source_contents, node.source_range),
        target_selection_range: lsp_range(source_contents, node.title_source_range),
    }]))
}

// Preview the destination of a text link at an editor position.
fn hover_for_document(uri: &Uri, source_contents: &str, cursor: Position) -> Option<Hover> {
    // Parse only the wiki syntax because hovering does not require filesystem validation.
    let wiki_path = local_path(uri);
    let wiki = parser::parse(wiki_path.as_deref(), source_contents).ok()?;
    let byte_offset = byte_offset(source_contents, cursor)?;
    let (node, source_range) = node_at(&wiki, source_contents, byte_offset, LinkExtent::Whole)?;

    // Render the node as Markdown with commands that navigate its resolvable text links.
    let markdown = node.to_markdown(|title| {
        let target = wiki.text_nodes.get(title)?;
        reveal_range_command_url(uri, source_contents, target.title_source_range)
    });
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: markdown,
        }),
        range: Some(lsp_range(source_contents, source_range)),
    })
}

// Encode an editor navigation command as a Markdown-safe URI.
fn reveal_range_command_url(
    uri: &Uri,
    source_contents: &str,
    source_range: SourceRange,
) -> Option<String> {
    // Pass the document URI and UTF-16 destination range as positional command arguments.
    let range = lsp_range(source_contents, source_range);
    let arguments = serde_json::to_string(&(
        uri.as_str(),
        range.start.line,
        range.start.character,
        range.end.line,
        range.end.character,
    ))
    .ok()?;
    Some(format!(
        "command:{REVEAL_RANGE_COMMAND}?{}",
        utf8_percent_encode(&arguments, NON_ALPHANUMERIC),
    ))
}

// Locate every text link to the node at an editor position.
fn references_for_document(
    uri: &Uri,
    source_contents: &str,
    cursor: Position,
    include_declaration: bool,
) -> Option<Vec<Location>> {
    // Parse only the wiki syntax because finding references does not require validation.
    let wiki_path = local_path(uri);
    let wiki = parser::parse(wiki_path.as_deref(), source_contents).ok()?;
    let byte_offset = byte_offset(source_contents, cursor)?;
    let (node, _source_range) = node_at(&wiki, source_contents, byte_offset, LinkExtent::Whole)?;

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
fn document_highlights_for_document(
    uri: &Uri,
    source_contents: &str,
    cursor: Position,
) -> Option<Vec<DocumentHighlight>> {
    // Parse only the wiki syntax because document highlights do not require validation.
    let wiki_path = local_path(uri);
    let wiki = parser::parse(wiki_path.as_deref(), source_contents).ok()?;
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
        let link = filesystem_link_at(&wiki, byte_offset)?;
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

// Find a file or directory link at a source offset.
fn filesystem_link_at(wiki: &Wiki, byte_offset: usize) -> Option<&Link> {
    wiki.text_nodes.values().find_map(|node| {
        node.links.iter().find(|link| match link {
            Link::File { source_range, .. } | Link::Directory { source_range, .. } => {
                source_range.start <= byte_offset && byte_offset < source_range.end
            }
            Link::Text { .. } => false,
        })
    })
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

// Identify the source occurrence that should be selected before renaming a node.
fn prepare_rename_for_document(
    uri: &Uri,
    source_contents: &str,
    cursor: Position,
) -> Option<PrepareRenameResponse> {
    // Resolve either a title declaration or text link in a parseable editor snapshot.
    let wiki_path = local_path(uri);
    let wiki = parser::parse(wiki_path.as_deref(), source_contents).ok()?;
    let byte_offset = byte_offset(source_contents, cursor)?;
    let (node, source_range) = node_at(&wiki, source_contents, byte_offset, LinkExtent::Target)?;

    // Select only the title text and seed the rename prompt with its decoded value.
    Some(PrepareRenameResponse::RangeWithPlaceholder {
        range: lsp_range(source_contents, source_range),
        placeholder: node.title.clone(),
    })
}

// Rename one text node and every text link that targets it.
fn rename_for_document(
    uri: &Uri,
    source_contents: &str,
    cursor: Position,
    new_name: &str,
) -> std::result::Result<Option<WorkspaceEdit>, String> {
    // Resolve the requested node without requiring the wiki to pass semantic validation.
    let wiki_path = local_path(uri);
    let Some(wiki) = parser::parse(wiki_path.as_deref(), source_contents).ok() else {
        return Ok(None);
    };
    let Some(byte_offset) = byte_offset(source_contents, cursor) else {
        return Ok(None);
    };
    let Some((node, _source_range)) =
        node_at(&wiki, source_contents, byte_offset, LinkExtent::Target)
    else {
        return Ok(None);
    };

    // Normalize surrounding whitespace while rejecting titles that cannot occupy one source line.
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
    if !is_text_link_title(new_title) {
        return Err(format!(
            "A text-linked node title cannot start with `{FILE_LINK_PREFIX}` or \
                `{DIRECTORY_LINK_PREFIX}`.",
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
            edits.push((target_source_range, escape_text_link_title(new_title)));
        }
    }
    edits.sort_by_key(|(source_range, _new_text)| (source_range.start, source_range.end));

    // Return one non-overlapping edit for each occurrence in the current document.
    let edits = edits
        .into_iter()
        .map(|(source_range, new_text)| {
            TextEdit::new(lsp_range(source_contents, source_range), new_text)
        })
        .collect();
    Ok(Some(WorkspaceEdit {
        changes: Some(HashMap::from([(uri.clone(), edits)])),
        ..WorkspaceEdit::default()
    }))
}

// Escape delimiters so an arbitrary node title retains its meaning inside a text link.
fn escape_text_link_title(title: &str) -> String {
    title.replace('[', "\\[").replace(']', "\\]")
}

// Distinguish node titles that can be encoded without becoming filesystem links.
fn is_text_link_title(title: &str) -> bool {
    !title.starts_with(FILE_LINK_PREFIX) && !title.starts_with(DIRECTORY_LINK_PREFIX)
}

// Resolve the text link under the cursor to its destination node.
fn linked_node_at<'a>(
    wiki: &'a Wiki,
    source_contents: &str,
    cursor: Position,
) -> Option<(&'a TextNode, SourceRange)> {
    // Match the cursor against complete link ranges, including their delimiters.
    let byte_offset = byte_offset(source_contents, cursor)?;
    let (title, source_range) = text_link_at(wiki, byte_offset)?;
    wiki.text_nodes.get(title).map(|node| (node, source_range))
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
    if let Some(node) = declaration_at(wiki, byte_offset) {
        return Some((node, node.title_source_range));
    }

    // Resolve a reference, reporting whichever extent of the link the caller asked for.
    let (title, source_range) = text_link_at(wiki, byte_offset)?;
    let node = wiki.text_nodes.get(title)?;
    let source_range = match link_extent {
        LinkExtent::Whole => source_range,
        LinkExtent::Target => text_link_target_source_range(source_contents, source_range)?,
    };
    Some((node, source_range))
}

// Find the node whose title is declared at a source offset.
fn declaration_at(wiki: &Wiki, byte_offset: usize) -> Option<&TextNode> {
    wiki.text_nodes.values().find(|node| {
        node.title_source_range.start <= byte_offset && byte_offset < node.title_source_range.end
    })
}

// Find a text link at a source offset without resolving its destination.
fn text_link_at(wiki: &Wiki, byte_offset: usize) -> Option<(&str, SourceRange)> {
    wiki.text_nodes.values().find_map(|node| {
        node.links.iter().find_map(|link| match link {
            Link::Text {
                title,
                source_range,
            } if source_range.start <= byte_offset && byte_offset < source_range.end => {
                Some((title.as_str(), *source_range))
            }
            Link::Text { .. } | Link::File { .. } | Link::Directory { .. } => None,
        })
    })
}

// Exclude the delimiters from a source range known to represent a complete link.
fn text_link_target_source_range(
    source_contents: &str,
    source_range: SourceRange,
) -> Option<SourceRange> {
    // Confirm the parser-provided range still addresses square-bracket delimiters.
    let link_source = source_contents.get(source_range.start..source_range.end)?;
    let target_source = link_source.strip_prefix('[')?.strip_suffix(']')?;
    let start = source_range.start + '['.len_utf8();
    Some(SourceRange {
        start,
        end: start + target_source.len(),
    })
}

// Analyze an editor snapshot without checking its formatting.
fn diagnostics_for_document(uri: &Uri, source_contents: &str) -> Vec<Diagnostic> {
    // Use local filesystem context when the editor document has one.
    let wiki_path = local_path(uri);

    // Preserve independent Mull errors as independent editor diagnostics.
    analyze(wiki_path.as_deref(), source_contents).map_or_else(
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
        range: lsp_range(source_contents, source_range),
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some(env!("CARGO_PKG_NAME").to_owned()),
        message,
        ..Diagnostic::default()
    }
}

// Convert a source range into the representation expected by the language server protocol.
fn lsp_range(source_contents: &str, source_range: SourceRange) -> Range {
    Range::new(
        position(source_contents, source_range.start),
        position(source_contents, source_range.end),
    )
}

// Convert a zero-based LSP position measured in UTF-16 code units into a UTF-8 byte offset.
fn byte_offset(source_contents: &str, position: Position) -> Option<usize> {
    // Locate the requested line without counting its line terminator as editor content.
    let mut line_start = 0;
    for _ in 0..position.line {
        let line_break = source_contents[line_start..].find('\n')?;
        line_start += line_break + '\n'.len_utf8();
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
    use super::{
        byte_offset, completions_for_document, definition_for_document, diagnostic_from_error,
        diagnostics_for_document, document_highlights_for_document, document_symbols_for_document,
        formatting_edit, hover_for_document, position, prepare_rename_for_document,
        references_for_document, rename_for_document, reveal_range_command_url,
    };
    use crate::{error::SourceRange, parser};
    use std::{
        fs,
        path::{Path, PathBuf},
        process,
        sync::atomic::{AtomicUsize, Ordering},
    };
    use tower_lsp_server::ls_types::{
        CompletionTextEdit, DiagnosticSeverity, DocumentHighlight, DocumentHighlightKind,
        DocumentSymbolResponse, GotoDefinitionResponse, HoverContents, MarkupKind, Position,
        PrepareRenameResponse, Range, SymbolKind, Uri,
    };

    // Assign each formatting fixture a distinct directory when tests run concurrently.
    static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

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

        assert_eq!(position(source, 0), Position::new(0, 0));
        assert_eq!(position(source, 5), Position::new(1, 0));
        assert_eq!(position(source, 9), Position::new(1, 2));
        assert_eq!(position(source, source.len()), Position::new(1, 7));
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
            let response = document_symbols_for_document(&uri, source, true).unwrap();
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
                Range::new(Position::new(0, 0), Position::new(4, 0)),
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
            let response = document_symbols_for_document(&uri, source, false).unwrap();
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
        let diagnostics = diagnostics_for_document(&untitled_uri(), source);

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
        let diagnostics = diagnostics_for_document(&untitled_uri(), source);

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
        let diagnostics = diagnostics_for_document(&untitled_uri(), source);

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

        assert!(definition_for_document(&untitled_uri(), source, Position::new(2, 4)).is_some());
        assert!(hover_for_document(&untitled_uri(), source, Position::new(2, 4)).is_some());
        assert!(
            references_for_document(&untitled_uri(), source, Position::new(2, 4), false).is_some(),
        );
    }

    // Complete a partial target by absorbing and reinstating the existing closing delimiter.
    #[test]
    fn completions_replace_closed_link_targets() {
        let source = "# Home\n\n[Gr]\n\n# Greeting\n\n# Other";
        let completions =
            completions_for_document(&untitled_uri(), source, Position::new(2, 3)).unwrap();

        assert_eq!(
            completions
                .iter()
                .map(|completion| completion.label.as_str())
                .collect::<Vec<_>>(),
            vec!["Greeting", "Home", "Other"],
        );
        let Some(CompletionTextEdit::Edit(edit)) = &completions[0].text_edit else {
            panic!("a completion should replace the link target");
        };
        assert_eq!(
            edit.range,
            Range::new(Position::new(2, 1), Position::new(2, 4)),
        );
        assert_eq!(edit.new_text, "Greeting]");
    }

    // Close an unfinished link temporarily while calculating its completions.
    #[test]
    fn completions_support_unfinished_links() {
        let source = "# Home\n\n[Gre\n\n# Greeting";
        let completions =
            completions_for_document(&untitled_uri(), source, Position::new(2, 4)).unwrap();
        let greeting = completions
            .iter()
            .find(|completion| completion.label == "Greeting")
            .unwrap();
        let Some(CompletionTextEdit::Edit(edit)) = &greeting.text_edit else {
            panic!("a completion should replace the unfinished target");
        };

        assert_eq!(
            edit.range,
            Range::new(Position::new(2, 1), Position::new(2, 4)),
        );
        assert_eq!(edit.new_text, "Greeting]");
    }

    // Leave the cursor after a single closing delimiter once a completion has been applied.
    #[test]
    fn completions_do_not_duplicate_closing_delimiters() {
        // Model an editor that has already auto-closed the link the cursor sits inside.
        let source = "# Home\n\n[Gr]\n\n# Greeting";
        let completions =
            completions_for_document(&untitled_uri(), source, Position::new(2, 3)).unwrap();
        let greeting = completions
            .iter()
            .find(|completion| completion.label == "Greeting")
            .unwrap();
        let Some(CompletionTextEdit::Edit(edit)) = &greeting.text_edit else {
            panic!("a completion should replace the link target");
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
            completions_for_document(&untitled_uri(), source, Position::new(2, 1)).unwrap();
        let bracketed = completions
            .iter()
            .find(|completion| completion.label == "A[B]")
            .unwrap();
        let Some(CompletionTextEdit::Edit(edit)) = &bracketed.text_edit else {
            panic!("a completion should encode the title as a text link");
        };

        assert_eq!(bracketed.filter_text.as_deref(), Some("A\\[B\\]"));
        assert_eq!(edit.new_text, "A\\[B\\]]");
    }

    // Offer text-node completions only while the cursor is inside a text link target.
    #[test]
    fn completions_ignore_other_contexts() {
        let source = concat!("# Home\n\nprose [", "file:notes.txt]");

        assert!(completions_for_document(&untitled_uri(), source, Position::new(2, 2)).is_none());
        assert!(completions_for_document(&untitled_uri(), source, Position::new(2, 10)).is_none());
        assert!(completions_for_document(&untitled_uri(), source, Position::new(0, 3)).is_none());
    }

    // Omit node titles whose reserved prefixes would produce filesystem links.
    #[test]
    fn completions_omit_filesystem_link_titles() {
        let source = "# Home\n\n[]\n\n# file:notes.txt\n\n# dir:images\n\n# Other";
        let completions =
            completions_for_document(&untitled_uri(), source, Position::new(2, 1)).unwrap();

        assert_eq!(
            completions
                .iter()
                .map(|completion| completion.label.as_str())
                .collect::<Vec<_>>(),
            vec!["Home", "Other"],
        );
    }

    // Jump from a text link to the title of its destination node.
    #[test]
    fn definitions_target_node_titles() {
        let source = "# Home\n\n😀 [Greeting]\n\n# Greeting\n\nHello!";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();
        let definition = definition_for_document(&uri, source, Position::new(2, 5)).unwrap();

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
            document_highlights_for_document(&uri, source, Position::new(8, 3)).unwrap();
        let from_link =
            document_highlights_for_document(&uri, source, Position::new(2, 3)).unwrap();
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
            document_highlights_for_document(&uri, source, Position::new(0, 3)),
            Some(vec![DocumentHighlight {
                range: Range::new(Position::new(0, 2), Position::new(0, 6)),
                kind: Some(DocumentHighlightKind::WRITE),
            }]),
        );
        assert!(document_highlights_for_document(&uri, source, Position::new(0, 0)).is_none());
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
            document_highlights_for_document(&uri, source, Position::new(2, 3)).unwrap();
        let directory_highlights =
            document_highlights_for_document(&uri, source, Position::new(2, 25)).unwrap();

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
            prepare_rename_for_document(&untitled_uri(), source, Position::new(4, 3)).unwrap();
        let from_link =
            prepare_rename_for_document(&untitled_uri(), source, Position::new(2, 4)).unwrap();

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
            Position::new(4, 3),
            "  Salutation\t",
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
        let workspace_edit =
            rename_for_document(&untitled_uri(), source, Position::new(2, 4), "A[B]")
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
        let workspace_edit =
            rename_for_document(&untitled_uri(), source, Position::new(0, 3), "Start")
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
            rename_for_document(&untitled_uri(), source, cursor, " \t").unwrap_err(),
            "A node title cannot be empty.",
        );
        assert_eq!(
            rename_for_document(&untitled_uri(), source, cursor, "Hello\nworld").unwrap_err(),
            "A node title cannot contain a line break.",
        );
        assert_eq!(
            rename_for_document(&untitled_uri(), source, cursor, "Home").unwrap_err(),
            "Node `Home` already exists.",
        );
        assert_eq!(
            rename_for_document(&untitled_uri(), source, cursor, "file:notes.txt").unwrap_err(),
            "A text-linked node title cannot start with `file:` or `dir:`.",
        );
    }

    // Leave filesystem links to ordinary editor and filesystem navigation.
    #[test]
    fn navigation_ignores_filesystem_links() {
        let source = concat!("# Home\n\n[", "file:notes.txt]");
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        assert!(definition_for_document(&uri, source, Position::new(2, 4)).is_none());
        assert!(hover_for_document(&uri, source, Position::new(2, 4)).is_none());
        assert!(references_for_document(&uri, source, Position::new(2, 4), false).is_none());
    }

    // Omit navigation results when the deliberately simple parser cannot produce a wiki.
    #[test]
    fn navigation_requires_parseable_source() {
        let source = "# Home\n\n[Greeting]\n\n# Greeting\n\nUnexpected]";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        assert!(definition_for_document(&uri, source, Position::new(2, 4)).is_none());
        assert!(hover_for_document(&uri, source, Position::new(2, 4)).is_none());
        assert!(references_for_document(&uri, source, Position::new(2, 4), false).is_none());
    }

    #[test]
    fn formatting_replaces_noncanonical_source() {
        let source = "# Zulu\n\n😀\n\n# Home\n\n[Zulu]";
        let wiki = TestWiki::new(source);
        let edit = formatting_edit(Some(wiki.path()), source).unwrap().unwrap();

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

        assert!(
            formatting_edit(Some(wiki.path()), source)
                .unwrap()
                .is_none(),
        );
    }

    #[test]
    fn formatting_rejects_invalid_source() {
        let source = "# Elsewhere\n";
        let wiki = TestWiki::new(source);

        assert!(formatting_edit(Some(wiki.path()), source).is_err());
    }

    // Format a new editor buffer without requiring a filesystem path.
    #[test]
    fn formatting_supports_untitled_wikis() {
        let source = "# Zulu\n\n# Home\n\n[Zulu]";
        let edit = formatting_edit(None, source).unwrap().unwrap();

        assert_eq!(edit.new_text, "# Home\n\n[Zulu]\n\n# Zulu\n");
    }

    #[test]
    fn formatting_differences_are_not_diagnostics() {
        let source = "# Zulu\n\n# Home\n\n[Zulu]";
        let wiki = TestWiki::new(source);
        let uri = Uri::from_file_path(wiki.path()).unwrap();

        assert!(diagnostics_for_document(&uri, source).is_empty());
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
