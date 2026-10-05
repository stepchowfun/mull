// Load the editor API and its Language Server Protocol client.
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import * as vscode from 'vscode';
import { LanguageClient } from 'vscode-languageclient/node';

// Make executable probes compatible with the extension's asynchronous startup.
const execFileAsync = promisify(execFile);

// This private command lets the language server reveal a source range in a document.
// [group:reveal_range_command]
const REVEAL_RANGE_COMMAND = 'mull.revealRange';

// This private command lets the language server reveal a directory in the explorer.
// [group:reveal_in_explorer_command]
const REVEAL_IN_EXPLORER_COMMAND = 'mull.revealInExplorer';

// These commands move the cursor to the start or end of the node containing it, optionally
// extending the selection.
const GO_TO_NODE_START_COMMAND = 'mull.goToNodeStart';
const GO_TO_NODE_END_COMMAND = 'mull.goToNodeEnd';
const SELECT_TO_NODE_START_COMMAND = 'mull.selectToNodeStart';
const SELECT_TO_NODE_END_COMMAND = 'mull.selectToNodeEnd';

// Link users to Mull's platform-specific installation instructions.
const INSTALLATION_URL = 'https://github.com/stepchowfun/mull#installation-instructions';
const INSTALLATION_ACTION = 'View installation instructions';
const CONFIGURATION_ACTION = 'Configure executable path';

// Retain the active client so it can be stopped when the extension is deactivated.
let client: LanguageClient | undefined;

// Show a notification offering the two ways to get a suitable executable, installing Mull or
// selecting an existing executable, and perform the one the user selects.
async function offerRecovery(isError: boolean, message: string): Promise<void> {
  // Offer direct access to the two ways to resolve the problem [tag:recovery_actions].
  const action = await (isError
    ? vscode.window.showErrorMessage(message, INSTALLATION_ACTION, CONFIGURATION_ACTION)
    : vscode.window.showWarningMessage(message, INSTALLATION_ACTION, CONFIGURATION_ACTION));

  // Perform the selected recovery action.
  if (action === INSTALLATION_ACTION) {
    await vscode.env.openExternal(vscode.Uri.parse(INSTALLATION_URL));
  } else if (action === CONFIGURATION_ACTION) {
    await vscode.commands.executeCommand('workbench.action.openSettings', 'mull.executablePath');
  } else if (action === undefined) {
    // Do nothing when the user dismisses the notification.
    return;
  } else {
    // VS Code returns an offered action or undefined [ref:recovery_actions].
    throw new Error('Unexpected recovery action.');
  }
}

// Help the user install Mull or select an existing executable.
async function reportMissingMull(
  outputChannel: vscode.LogOutputChannel,
  error: unknown,
): Promise<void> {
  // Record both an actionable explanation and the underlying launch failure.
  outputChannel.error(
    `Mull couldn't be found. Install Mull or configure mull.executablePath. ${INSTALLATION_URL}`,
  );
  outputChannel.debug(String(error));

  // Leave Mull inactive unless the user resolves the problem.
  await offerRecovery(true, "Mull couldn't be found.");
}

// Read the version of this extension, which is the version of Mull it's released with.
function extensionVersion(context: vscode.ExtensionContext): string {
  const manifest: unknown = context.extension.packageJSON;
  if (
    typeof manifest === 'object' &&
    manifest !== null &&
    'version' in manifest &&
    typeof manifest.version === 'string'
  ) {
    return manifest.version;
  }
  throw new Error('The extension manifest should specify a version.');
}

// Warn that the executable isn't the version of Mull this extension was released with, as when
// only one of them was upgraded. Reinstalling Mull upgrades both.
async function reportVersionMismatch(
  outputChannel: vscode.LogOutputChannel,
  expectedVersion: string,
  versionOutput: string,
): Promise<void> {
  const message = `This extension expects Mull ${expectedVersion}, but the executable reports \`${versionOutput}\`.`;
  outputChannel.warn(`${message} Reinstall Mull to update both. ${INSTALLATION_URL}`);
  await offerRecovery(false, message);
}

// Reveal a source range supplied by the language server.
async function revealRange(
  uriString: string,
  startLine: number,
  startCharacter: number,
  endLine: number,
  endCharacter: number,
): Promise<void> {
  // Open either a file-backed or untitled document and select the range, which may be empty.
  const document = await vscode.workspace.openTextDocument(vscode.Uri.parse(uriString));
  const editor = await vscode.window.showTextDocument(document);
  const range = new vscode.Range(startLine, startCharacter, endLine, endCharacter);
  editor.selection = new vscode.Selection(range.start, range.end);
  editor.revealRange(range, vscode.TextEditorRevealType.InCenterIfOutsideViewport);
}

// Reveal a directory supplied by the language server in the explorer.
async function revealInExplorer(uriString: string): Promise<void> {
  // VS Code's command expects a URI object, which the language server can only pass as a string.
  await vscode.commands.executeCommand('revealInExplorer', vscode.Uri.parse(uriString));
}

// List the ranges of a wiki's nodes in source order, using the language server's document symbols,
// whose ranges span entire nodes.
async function nodeRanges(document: vscode.TextDocument): Promise<vscode.Range[]> {
  const symbols = await vscode.commands.executeCommand<vscode.DocumentSymbol[] | undefined>(
    'vscode.executeDocumentSymbolProvider',
    document.uri,
  );
  return (symbols ?? [])
    .map((symbol) => symbol.range)
    .toSorted((a, b) => a.start.compareTo(b.start));
}

// Find the node containing a position, which is the last node starting at or before it, so a
// position between nodes belongs to the one above. A position before the first node has none.
function nodeAt(
  ranges: readonly vscode.Range[],
  position: vscode.Position,
): vscode.Range | undefined {
  return ranges.findLast((range) => range.start.isBeforeOrEqual(position));
}

// Move each cursor to the start or end of the node containing it. A cursor before the first node
// stays put, and repeating the command changes nothing.
async function moveToNodeBoundary(boundary: 'start' | 'end', select: boolean): Promise<void> {
  // Find the nodes of the active wiki.
  const editor = vscode.window.activeTextEditor;
  if (editor === undefined) {
    return;
  }
  const ranges = await nodeRanges(editor.document);

  // Move the active end of each selection, keeping its anchor when extending it.
  editor.selections = editor.selections.map((selection) => {
    const range = nodeAt(ranges, selection.active);
    if (range === undefined) {
      return selection;
    }
    const position = boundary === 'start' ? range.start : range.end;
    return new vscode.Selection(select ? selection.anchor : position, position);
  });
  editor.revealRange(new vscode.Range(editor.selection.active, editor.selection.active));
}

// This describes how to focus on a node: the lines it spans through the line before the next node,
// and the folds before and after it.
interface Focus {
  startLine: number;
  endLine: number;
  before: vscode.FoldingRange | undefined;
  after: vscode.FoldingRange | undefined;
}

// Make a fold of some lines, if they span more than one line.
function foldingRange(start: number, end: number): vscode.FoldingRange | undefined {
  return start < end ? new vscode.FoldingRange(start, end) : undefined;
}

// Find how to focus on the node containing a position. The folds cover the lines before the node
// and those from the blank line before the next node. A fold's first line stays visible, so the
// wiki's first line and that blank line remain on screen, as do blank lines at the end of the node.
function focusAt(
  nodes: readonly vscode.Range[],
  position: vscode.Position,
  lineCount: number,
): Focus | undefined {
  const node = nodeAt(nodes, position);
  if (node === undefined) {
    return undefined;
  }
  const nextNode = nodes.find((range) => range.start.isAfter(position));
  return {
    startLine: node.start.line,
    endLine: nextNode === undefined ? lineCount - 1 : nextNode.start.line - 1,
    before: foldingRange(0, node.start.line - 1),
    after:
      nextNode === undefined
        ? undefined
        : foldingRange(Math.max(node.end.line + 1, nextNode.start.line - 1), lineCount - 1),
  };
}

// Fold or unfold exactly the folds starting on some lines of the active editor.
async function setFolded(folded: boolean, startLines: number[]): Promise<void> {
  if (startLines.length > 0) {
    await vscode.commands.executeCommand(folded ? 'editor.fold' : 'editor.unfold', {
      levels: 1,
      direction: 'down',
      selectionLines: startLines,
    });
  }
}

// Determine whether an editor is one of the main editors showing a wiki. Embedded editors, such as
// the preview in the references view, have no view column, and their cursors don't determine the
// focus. One becomes the active editor while it has focus, and fold commands then act on it.
function isMainWikiEditor(editor: vscode.TextEditor | undefined): editor is vscode.TextEditor {
  return (
    editor !== undefined && editor.document.languageId === 'mull' && editor.viewColumn !== undefined
  );
}

// This folds everything outside the node containing the cursor, so the node being edited appears to
// be the whole document.
class NodeFocus implements vscode.FoldingRangeProvider, vscode.Disposable {
  // Notify the editor that the folds may have moved, so it requests them again.
  private readonly changeEmitter = new vscode.EventEmitter<void>();
  public readonly onDidChangeFoldingRanges = this.changeEmitter.event;

  // Remember each wiki's nodes and focus as of the last time the editor requested folds, the first
  // lines of the folds last collapsed in its active editor, and whether that editor may not have
  // them.
  private readonly states = new Map<string, { nodes: vscode.Range[]; focus: Focus | undefined }>();
  private readonly collapsedStartLines = new Map<string, number[]>();
  private readonly uncollapsed = new Set<string>();

  // Determine whether the user wants everything outside the focused node folded.
  private static isEnabled(): boolean {
    return vscode.workspace.getConfiguration('mull').get('foldOtherNodes', true);
  }

  // Fold around the node containing the cursor of a main editor showing the wiki, preferring the
  // active editor. The editor requests folds again after every edit, which keeps them current.
  public async provideFoldingRanges(document: vscode.TextDocument): Promise<vscode.FoldingRange[]> {
    // Find the focus, if the cursor is in a node.
    const editor = [vscode.window.activeTextEditor, ...vscode.window.visibleTextEditors].find(
      (candidate) => isMainWikiEditor(candidate) && candidate.document === document,
    );
    if (editor === undefined) {
      return [];
    }
    const key = document.uri.toString();
    const nodes = await nodeRanges(document);
    const focus = NodeFocus.isEnabled()
      ? focusAt(nodes, editor.selection.active, document.lineCount)
      : undefined;
    this.states.set(key, { nodes, focus });

    // Collapse the new folds in the active editor once it has them, then discard any folds it kept
    // because they were collapsed when they stopped being provided. Collapsing first keeps kept
    // folds within the new ones from showing in between. Old folds that may still be collapsed are
    // unfolded before new folds are requested, since by now one may be gone, and unfolding its
    // first line would unfold a new fold containing it instead. Fold commands reveal the cursor, so
    // they're skipped when the folds haven't moved, as when the editor requests them again without
    // an edit, which would otherwise interrupt scrolling. Commands issued after the folds are
    // returned wait for the editor to apply them.
    const folds =
      focus === undefined ? [] : [focus.before, focus.after].filter((fold) => fold !== undefined);
    const startLines = folds.map((fold) => fold.start);
    if (
      editor === vscode.window.activeTextEditor &&
      (this.uncollapsed.delete(key) ||
        (this.collapsedStartLines.get(key) ?? []).join() !== startLines.join())
    ) {
      this.collapsedStartLines.set(key, startLines);
      setTimeout(async () => {
        await setFolded(true, startLines);
        await vscode.commands.executeCommand('editor.removeManualFoldingRanges');
      }, 0);
    }
    return folds;
  }

  // Fold around another node when the cursor leaves the focused one, as when navigation or Find
  // moves it into folded text, which the editor unfolds to reveal it.
  public async refocus(editor: vscode.TextEditor | undefined): Promise<void> {
    // Do nothing while the cursor stays in the focused node or folding is off.
    if (!isMainWikiEditor(editor) || !NodeFocus.isEnabled()) {
      return;
    }
    const state = this.states.get(editor.document.uri.toString());
    if (state === undefined) {
      return;
    }
    const { focus } = state;
    const line = editor.selection.active.line;
    if (focus !== undefined && line >= focus.startLine && line <= focus.endLine) {
      return;
    }

    // Unfold the old folds that the new ones won't replace before requesting them, since the editor
    // keeps a collapsed fold that's no longer provided unless the cursor is in its hidden lines, and
    // a kept fold can displace the new ones. An old fold within the new one after the node can stay.
    if (editor === vscode.window.activeTextEditor && focus !== undefined) {
      const newFocus = focusAt(state.nodes, editor.selection.active, editor.document.lineCount);
      const staleStartLines = [];
      if (focus.before !== undefined && (newFocus === undefined || newFocus.before === undefined)) {
        staleStartLines.push(focus.before.start);
      }
      if (
        focus.after !== undefined &&
        (newFocus === undefined ||
          newFocus.after === undefined ||
          newFocus.after.start > focus.after.start)
      ) {
        staleStartLines.push(focus.after.start);
      }
      await setFolded(false, staleStartLines);
    }
    this.changeEmitter.fire();
  }

  // Collapse the folds again when a wiki's editor becomes active, since it may not have them.
  public activate(editor: vscode.TextEditor | undefined): void {
    if (isMainWikiEditor(editor)) {
      this.uncollapsed.add(editor.document.uri.toString());
      this.changeEmitter.fire();
    }
  }

  // Add or remove the folds when the user turns folding on or off. Turning it off unfolds the
  // active editor's folds before requesting new ones, since the editor keeps a fold that's collapsed
  // when it stops being provided.
  public async configure(event: vscode.ConfigurationChangeEvent): Promise<void> {
    if (!event.affectsConfiguration('mull.foldOtherNodes')) {
      return;
    }
    const editor = vscode.window.activeTextEditor;
    if (!NodeFocus.isEnabled() && isMainWikiEditor(editor)) {
      await setFolded(false, this.collapsedStartLines.get(editor.document.uri.toString()) ?? []);
    }
    this.changeEmitter.fire();
  }

  // Release the event emitter.
  public dispose(): void {
    this.changeEmitter.dispose();
  }
}

// Start a Mull language server for local and untitled Mull documents.
export async function activate(context: vscode.ExtensionContext): Promise<void> {
  // Expose the navigation commands that the language server's responses refer to.
  context.subscriptions.push(vscode.commands.registerCommand(REVEAL_RANGE_COMMAND, revealRange));
  context.subscriptions.push(
    vscode.commands.registerCommand(REVEAL_IN_EXPLORER_COMMAND, revealInExplorer),
  );

  // Expose the commands that move the cursor within the node containing it.
  const nodeBoundaryCommands = [
    [GO_TO_NODE_START_COMMAND, 'start', false],
    [GO_TO_NODE_END_COMMAND, 'end', false],
    [SELECT_TO_NODE_START_COMMAND, 'start', true],
    [SELECT_TO_NODE_END_COMMAND, 'end', true],
  ] as const;
  for (const [command, boundary, select] of nodeBoundaryCommands) {
    context.subscriptions.push(
      vscode.commands.registerCommand(command, async () => moveToNodeBoundary(boundary, select)),
    );
  }

  // Show only the node containing the cursor, as if it were the whole document.
  const nodeFocus = new NodeFocus();
  context.subscriptions.push(
    nodeFocus,
    vscode.languages.registerFoldingRangeProvider({ language: 'mull' }, nodeFocus),
    vscode.window.onDidChangeTextEditorSelection(async (event) => {
      await nodeFocus.refocus(event.textEditor);
    }),
    vscode.window.onDidChangeActiveTextEditor((editor) => {
      nodeFocus.activate(editor);
    }),
    vscode.workspace.onDidChangeConfiguration(async (event) => {
      await nodeFocus.configure(event);
    }),
  );

  // Resolve the configured executable before constructing the server process.
  const executablePath = vscode.workspace.getConfiguration('mull').get('executablePath', 'mull');

  // Detect a missing executable before the language client emits its own error.
  const outputChannel = vscode.window.createOutputChannel('Mull', { log: true });
  context.subscriptions.push(outputChannel);
  let versionOutput: string;
  try {
    versionOutput = (await execFileAsync(executablePath, ['--version'])).stdout.trim();
  } catch (error) {
    if (error instanceof Error && 'code' in error && error.code === 'ENOENT') {
      reportMissingMull(outputChannel, error).catch((reportingError: unknown) => {
        outputChannel.error('Unable to show Mull installation help.', reportingError);
      });
      return;
    }

    throw error;
  }

  // Warn when the executable isn't the version of Mull this extension was released with. The
  // language server starts anyway, since it mostly works with nearby versions.
  const expectedVersion = extensionVersion(context);
  if (versionOutput !== `Mull ${expectedVersion}`) {
    reportVersionMismatch(outputChannel, expectedVersion, versionOutput).catch(
      (reportingError: unknown) => {
        outputChannel.error('Unable to show the Mull version warning.', reportingError);
      },
    );
  }

  // Connect Mull documents to the server and complete the LSP handshake.
  const serverOptions = {
    command: executablePath,
    args: ['language-server'],
  };
  const clientOptions = {
    outputChannel,
    documentSelector: [
      { scheme: 'file', language: 'mull' },
      { scheme: 'untitled', language: 'mull' },
    ],
    markdown: {
      isTrusted: {
        enabledCommands: [REVEAL_RANGE_COMMAND, REVEAL_IN_EXPLORER_COMMAND],
      },
    },
  };
  client = new LanguageClient('mull', 'Mull', serverOptions, clientOptions);
  await client.start();
}

// Shut down the language client and its server process with the extension.
export async function deactivate(): Promise<void> {
  if (client !== undefined) {
    await client.dispose();
  }
  client = undefined;
}
