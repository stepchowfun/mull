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

// Wait this long after scrolling stops before settling the view back onto the current node.
const SETTLE_DELAY_MILLISECONDS = 150;

// Settle the view this many rows further than the allowance for the cursor's surroundings requires.
// Rows are counted as at least as many as lines take, so the view could otherwise land just outside
// the allowance and settle again.
const SETTLE_SLACK_ROWS = 2;

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

// Split text into characters as they appear, for counting the columns a line takes.
const graphemes = new Intl.Segmenter();

// Find a lower bound on the rows a line takes when the editor wraps lines at a known column, as
// with bounded wrapping. Each character takes at least a column, and the editor wraps at word
// boundaries and at the edge of a narrower view, which only adds rows. Without a known wrapping
// column, a line takes at least a row.
function minimumRows(text: string, wrapColumn: number | undefined): number {
  if (wrapColumn === undefined) {
    return 1;
  }
  return Math.max(1, Math.ceil([...graphemes.segment(text)].length / wrapColumn));
}

// Find the column at which an editor wraps a document's lines, if it's known.
function wrappingColumn(document: vscode.TextDocument): number | undefined {
  const configuration = vscode.workspace.getConfiguration('editor', document);
  const wordWrap = configuration.get<string>('wordWrap');
  return wordWrap === 'bounded' || wordWrap === 'wordWrapColumn'
    ? configuration.get<number>('wordWrapColumn')
    : undefined;
}

// Find how many rows the editor keeps visible around the cursor when it moves, which is the larger
// of the configured surrounding lines and the most lines sticky scroll shows, if it's enabled. The
// editor caps this at half the view, so the view never shows more of other nodes for the cursor.
function cursorMargin(document: vscode.TextDocument): number {
  const configuration = vscode.workspace.getConfiguration('editor', document);
  return Math.max(
    configuration.get<number>('cursorSurroundingLines', 0),
    configuration.get<boolean>('stickyScroll.enabled', true)
      ? configuration.get<number>('stickyScroll.maxLineCount', 5)
      : 0,
  );
}

// Determine whether an editor is one of the main editors showing a wiki. Embedded editors, such as
// the preview in the references view, have no view column, and aren't dimmed.
function isMainWikiEditor(editor: vscode.TextEditor | undefined): editor is vscode.TextEditor {
  return (
    editor !== undefined && editor.document.languageId === 'mull' && editor.viewColumn !== undefined
  );
}

// This dims everything outside the node containing the cursor in each main editor showing a wiki,
// so the node being edited stands out. Blank lines at the end of the node aren't dimmed.
class NodeDimmer implements vscode.Disposable {
  // Draw dimmed text with reduced opacity.
  private readonly decorationType = vscode.window.createTextEditorDecorationType({
    opacity: '0.5',
  });

  // Remember each wiki's nodes, which are refreshed after every edit.
  private readonly nodes = new Map<string, vscode.Range[]>();

  // Remember each editor's pending settling, which is postponed while it keeps scrolling, and the
  // first line of the node containing its cursor.
  private readonly settleTimers = new Map<vscode.TextEditor, ReturnType<typeof setTimeout>>();
  private readonly cursorNodeLines = new Map<vscode.TextEditor, number | undefined>();

  // Determine whether the user wants everything outside the current node dimmed.
  private static dimsOtherNodes(): boolean {
    return vscode.workspace.getConfiguration('mull').get('dimOtherNodes', true);
  }

  // Determine whether the user wants the view to settle back onto the current node after scrolling.
  private static snapsBackToCurrentNode(): boolean {
    return vscode.workspace.getConfiguration('mull').get('snapBackToCurrentNode', true);
  }

  // Dim around the cursor of an editor, using the nodes last found in its wiki.
  public dim(editor: vscode.TextEditor): void {
    // Find the node containing the cursor and the node after it.
    const nodes = this.nodes.get(editor.document.uri.toString()) ?? [];
    const cursor = editor.selection.active;
    const node = NodeDimmer.dimsOtherNodes() ? nodeAt(nodes, cursor) : undefined;
    if (node === undefined) {
      editor.setDecorations(this.decorationType, []);
      return;
    }
    const nextNode = nodes.find((range) => range.start.isAfter(cursor));

    // Dim the lines before the node, and those from the next node to the end of the wiki.
    const documentEnd = editor.document.lineAt(editor.document.lineCount - 1).range.end;
    editor.setDecorations(
      this.decorationType,
      [
        new vscode.Range(new vscode.Position(0, 0), node.start),
        ...(nextNode === undefined ? [] : [new vscode.Range(nextNode.start, documentEnd)]),
      ].filter((range) => !range.isEmpty),
    );
  }

  // Find a wiki's nodes again and dim its editors accordingly. A result is discarded if the wiki
  // changed while it was being found, since a later refresh will replace it.
  public async refresh(document: vscode.TextDocument): Promise<void> {
    const { version } = document;
    const nodes = await nodeRanges(document);
    if (document.version !== version) {
      return;
    }
    this.nodes.set(document.uri.toString(), nodes);
    for (const editor of vscode.window.visibleTextEditors) {
      if (isMainWikiEditor(editor) && editor.document === document) {
        this.dim(editor);
      }
    }
  }

  // Find the nodes of every wiki shown in a main editor.
  public async refreshVisible(): Promise<void> {
    const documents = new Set(
      vscode.window.visibleTextEditors.filter(isMainWikiEditor).map((editor) => editor.document),
    );
    await Promise.all([...documents].map(async (document) => this.refresh(document)));
  }

  // Settle an editor's view onto the node containing its cursor when the cursor moves to another
  // node, as when clicking a dimmed node, pulling the node into view.
  public followCursor(editor: vscode.TextEditor): void {
    const key = editor.document.uri.toString();
    const node = nodeAt(this.nodes.get(key) ?? [], editor.selection.active);
    const line = node === undefined ? undefined : node.start.line;
    const hadLine = this.cursorNodeLines.has(editor);
    const previousLine = this.cursorNodeLines.get(editor);
    this.cursorNodeLines.set(editor, line);
    if (hadLine && line !== previousLine) {
      this.scheduleSettle(editor);
    }
  }

  // Settle an editor's view back onto the node containing its cursor once it stops scrolling, like
  // a rubber band, so scrolling past the node shows its neighbors only until the scrolling stops.
  public scheduleSettle(editor: vscode.TextEditor): void {
    clearTimeout(this.settleTimers.get(editor));
    this.settleTimers.set(
      editor,
      setTimeout(async () => {
        this.settleTimers.delete(editor);
        await this.settle(editor);
      }, SETTLE_DELAY_MILLISECONDS),
    );
  }

  // Scroll the view back toward the node when it shows more of the node before it than the editor
  // keeps visible around the cursor, or more of the node after it while the node's top is out of
  // view, until it shows that much. Allowing that much keeps settling from fighting the editor,
  // which may scroll that far to keep the cursor's surroundings in view, and scrolling back only
  // the excess keeps it gentle. Revealing a range would leave padding around it, so the view is
  // scrolled by lines instead. Only the active editor can be scrolled this way.
  private async settle(editor: vscode.TextEditor): Promise<void> {
    // Find the node containing the cursor and the lines in view, which are the completely visible
    // ones.
    const nodes = this.nodes.get(editor.document.uri.toString()) ?? [];
    const node = NodeDimmer.snapsBackToCurrentNode()
      ? nodeAt(nodes, editor.selection.active)
      : undefined;
    const firstVisibleRange = editor.visibleRanges.at(0);
    const lastVisibleRange = editor.visibleRanges.at(-1);
    if (
      editor !== vscode.window.activeTextEditor ||
      node === undefined ||
      firstVisibleRange === undefined ||
      lastVisibleRange === undefined
    ) {
      return;
    }
    const firstVisible = firstVisibleRange.start;
    const lastVisible = lastVisibleRange.end;

    // Count the rows that some lines take at least, which errs toward not settling. The first and
    // last lines may be counted from or to a character, since the view can begin or end partway
    // through a wrapped line, and counting all of it would overcount the rows in view.
    const column = wrappingColumn(editor.document);
    const rows = (firstLine: number, lastLine: number, from = 0, to?: number): number => {
      let total = 0;
      for (let line = firstLine; line <= lastLine; line += 1) {
        const text = editor.document.lineAt(line).text;
        total += minimumRows(
          text.slice(line === firstLine ? from : 0, line === lastLine ? to : undefined),
          column,
        );
      }
      return total;
    };

    // Count the rows in view before and after the node, and find how many rows of other nodes the
    // view may show.
    const rowsBefore =
      firstVisible.line < node.start.line
        ? rows(firstVisible.line, node.start.line - 1, firstVisible.character)
        : 0;
    const rowsAfter =
      lastVisible.line > node.end.line
        ? rows(node.end.line + 1, lastVisible.line, 0, lastVisible.character)
        : 0;
    const margin = cursorMargin(editor.document);
    const target = Math.max(margin - SETTLE_SLACK_ROWS, 0);

    // Scroll down by the fewest lines that leave no more of the node before it than the target. The
    // editor counts lines from the first completely visible one and aligns them exactly.
    if (rowsBefore > margin) {
      let topLine = firstVisible.line + 1;
      while (topLine < node.start.line && rows(topLine, node.start.line - 1) > target) {
        topLine += 1;
      }
      await vscode.commands.executeCommand('editorScroll', {
        to: 'down',
        by: 'line',
        value: topLine - firstVisible.line,
      });
      return;
    }

    // Scroll up by the rows the lines after the node take beyond the target. Those lines take at
    // least as many rows as counted, so the node's end isn't passed, nor its top, since the node's
    // text before the view takes at least as many rows as counted for it.
    if (firstVisible.line > node.start.line && rowsAfter > margin) {
      const rowsHidden =
        firstVisible.character > 0
          ? rows(node.start.line, firstVisible.line, 0, firstVisible.character)
          : rows(node.start.line, firstVisible.line - 1);
      const value = Math.min(rowsAfter - target, rowsHidden);
      await vscode.commands.executeCommand('editorScroll', {
        to: 'up',
        by: 'wrappedLine',
        value,
      });
    }
  }

  // Forget the nodes of a wiki that's no longer open.
  public forget(document: vscode.TextDocument): void {
    this.nodes.delete(document.uri.toString());
  }

  // Release the decoration type, which removes the dimming, and cancel any pending settling.
  public dispose(): void {
    this.decorationType.dispose();
    for (const timer of this.settleTimers.values()) {
      clearTimeout(timer);
    }
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

  // Dim everything outside the node containing the cursor, following the cursor, edits, the visible
  // editors, and the setting.
  const nodeDimmer = new NodeDimmer();
  context.subscriptions.push(
    nodeDimmer,
    vscode.window.onDidChangeTextEditorSelection((event) => {
      if (isMainWikiEditor(event.textEditor)) {
        nodeDimmer.dim(event.textEditor);
        nodeDimmer.followCursor(event.textEditor);
      }
    }),
    vscode.workspace.onDidChangeTextDocument(async (event) => {
      if (event.document.languageId === 'mull') {
        await nodeDimmer.refresh(event.document);
      }
    }),
    vscode.window.onDidChangeVisibleTextEditors(async () => nodeDimmer.refreshVisible()),
    vscode.window.onDidChangeTextEditorVisibleRanges((event) => {
      if (isMainWikiEditor(event.textEditor)) {
        nodeDimmer.scheduleSettle(event.textEditor);
      }
    }),
    vscode.workspace.onDidCloseTextDocument((document) => {
      nodeDimmer.forget(document);
    }),
    vscode.workspace.onDidChangeConfiguration(async (event) => {
      if (event.affectsConfiguration('mull.dimOtherNodes')) {
        await nodeDimmer.refreshVisible();
      }
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

  // Find the nodes of the wikis already shown, which needs the language server.
  await nodeDimmer.refreshVisible();
}

// Shut down the language client and its server process with the extension.
export async function deactivate(): Promise<void> {
  if (client !== undefined) {
    await client.dispose();
  }
  client = undefined;
}
