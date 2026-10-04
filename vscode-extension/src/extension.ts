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

  // Offer direct access to the two ways to resolve the problem [tag:missing_mull_actions].
  const action = await vscode.window.showErrorMessage(
    "Mull couldn't be found.",
    INSTALLATION_ACTION,
    CONFIGURATION_ACTION,
  );

  // Perform the selected recovery action.
  if (action === INSTALLATION_ACTION) {
    await vscode.env.openExternal(vscode.Uri.parse(INSTALLATION_URL));
  } else if (action === CONFIGURATION_ACTION) {
    await vscode.commands.executeCommand('workbench.action.openSettings', 'mull.executablePath');
  } else if (action === undefined) {
    // Leave Mull inactive when the user dismisses the notification.
    return;
  } else {
    // VS Code returns a configured action or undefined [ref:missing_mull_actions].
    throw new Error('Unexpected missing-Mull action.');
  }
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

// Find the folds around a node: the lines before it and those from the blank line after it, where
// each spans more than one line. The first line of a fold stays visible, so the wiki's first line
// remains above the node, and the blank line after the node remains below it. Folding from the
// node's last line instead would hide the separator, but then a new line typed at the end of the
// node would land in the fold.
function foldsAround(node: vscode.Range | undefined, lineCount: number): vscode.FoldingRange[] {
  if (node === undefined) {
    return [];
  }
  return [
    new vscode.FoldingRange(0, node.start.line - 1),
    new vscode.FoldingRange(node.end.line + 1, lineCount - 1),
  ].filter((range) => range.start < range.end);
}

// This folds everything outside the node containing the cursor, so the node being edited appears to
// be the whole document.
class NodeFocus implements vscode.FoldingRangeProvider {
  // Notify the editor that the folds may have moved, so it requests them again.
  public readonly changeEmitter = new vscode.EventEmitter<void>();
  public readonly onDidChangeFoldingRanges = this.changeEmitter.event;

  // Remember each wiki's nodes, the lines its focused node spans through the blank line after it,
  // and the first lines of its folds, as of the last time the editor requested folds.
  private readonly nodes = new Map<string, vscode.Range[]>();
  private readonly focusedLines = new Map<string, { start: number; end: number }>();
  private readonly foldStartLines = new Map<string, number[]>();

  // Fold around the node containing the cursor of an editor showing the wiki, preferring the active
  // editor. The editor requests folds again after every edit, which keeps them current.
  public async provideFoldingRanges(document: vscode.TextDocument): Promise<vscode.FoldingRange[]> {
    // Find the focused node. The cursor may be before every node, in which case nothing is folded.
    const key = document.uri.toString();
    const editor = [vscode.window.activeTextEditor, ...vscode.window.visibleTextEditors].find(
      (candidate) => candidate !== undefined && candidate.document === document,
    );
    if (editor === undefined) {
      return [];
    }
    const nodes = await nodeRanges(document);
    const node = nodeAt(nodes, editor.selection.active);
    this.nodes.set(key, nodes);
    if (node === undefined) {
      this.focusedLines.delete(key);
    } else {
      this.focusedLines.set(key, { start: node.start.line, end: node.end.line + 1 });
    }

    // Collapse exactly these folds in the active editor once it has them. A fold command issued
    // after the folds are returned waits for the editor to apply them.
    const ranges = foldsAround(node, document.lineCount);
    const startLines = ranges.map((range) => range.start);
    this.foldStartLines.set(key, startLines);
    if (editor === vscode.window.activeTextEditor && startLines.length > 0) {
      setTimeout(() => {
        vscode.commands.executeCommand('editor.fold', {
          levels: 1,
          direction: 'down',
          selectionLines: startLines,
        });
      }, 0);
    }
    return ranges;
  }

  // Fold around another node when the cursor leaves the focused one, as when navigation or Find
  // moves it into folded text, which the editor unfolds to reveal it.
  public async refocus(editor: vscode.TextEditor | undefined): Promise<void> {
    // Do nothing while the cursor stays in the focused node.
    if (editor === undefined || editor.document.languageId !== 'mull') {
      return;
    }
    const key = editor.document.uri.toString();
    const lines = this.focusedLines.get(key);
    const line = editor.selection.active.line;
    if (lines !== undefined && line >= lines.start && line <= lines.end) {
      return;
    }

    // The editor keeps a collapsed fold after it's no longer provided, unless a cursor is in its
    // hidden lines, and a kept fold starting above the new folds displaces them. That happens when
    // the cursor moves onto the visible first line of the wiki, which belongs to the first node,
    // around which there's no fold starting there to replace the old one. So unfold the old folds
    // that won't be replaced before requesting new ones, except those within the new fold after
    // the node, which stay hidden. Also discard any folds the editor kept earlier.
    if (editor === vscode.window.activeTextEditor) {
      const newRanges = foldsAround(
        nodeAt(this.nodes.get(key) ?? [], editor.selection.active),
        editor.document.lineCount,
      );
      const newAfterRange = newRanges.find((range) => range.start > 0);
      const staleStartLines = (this.foldStartLines.get(key) ?? []).filter(
        (startLine) =>
          !newRanges.some((range) => range.start === startLine) &&
          (newAfterRange === undefined || startLine < newAfterRange.start),
      );
      if (staleStartLines.length > 0) {
        await vscode.commands.executeCommand('editor.unfold', {
          levels: 1,
          direction: 'down',
          selectionLines: staleStartLines,
        });
      }
      await vscode.commands.executeCommand('editor.removeManualFoldingRanges');
    }
    this.changeEmitter.fire();
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
    nodeFocus.changeEmitter,
    vscode.languages.registerFoldingRangeProvider({ language: 'mull' }, nodeFocus),
    vscode.window.onDidChangeTextEditorSelection(async (event) => {
      await nodeFocus.refocus(event.textEditor);
    }),
    vscode.window.onDidChangeActiveTextEditor(async (editor) => {
      await nodeFocus.refocus(editor);
    }),
  );

  // Resolve the configured executable before constructing the server process.
  const executablePath = vscode.workspace.getConfiguration('mull').get('executablePath', 'mull');

  // Detect a missing executable before the language client emits its own error.
  const outputChannel = vscode.window.createOutputChannel('Mull', { log: true });
  context.subscriptions.push(outputChannel);
  try {
    await execFileAsync(executablePath, ['--version']);
  } catch (error) {
    if (error instanceof Error && 'code' in error && error.code === 'ENOENT') {
      reportMissingMull(outputChannel, error).catch((reportingError: unknown) => {
        outputChannel.error('Unable to show Mull installation help.', reportingError);
      });
      return;
    }

    throw error;
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
