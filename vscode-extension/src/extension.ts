// Load the editor API and its Language Server Protocol client.
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import * as vscode from 'vscode';
import { LanguageClient } from 'vscode-languageclient/node';

// Make executable probes compatible with the extension's asynchronous startup.
const execFileAsync = promisify(execFile);

// Determine the type of a setting's value.
const isBoolean = (value: unknown): value is boolean => typeof value === 'boolean';
const isNumber = (value: unknown): value is number => typeof value === 'number';
const isString = (value: unknown): value is string => typeof value === 'string';

// Read a setting, failing if it's missing, which can't happen since every setting read here has a
// default, or if it was given a value of the wrong type.
function setting<T>(
  section: string,
  key: string,
  isExpectedType: (value: unknown) => value is T,
  scope?: vscode.ConfigurationScope,
): T {
  const value = vscode.workspace.getConfiguration(section, scope).get<unknown>(key);
  if (!isExpectedType(value)) {
    throw new Error(`The setting \`${section}.${key}\` is missing or has the wrong type.`);
  }
  return value;
}

// Link users to Mull's platform-specific installation instructions.
const INSTALLATION_URL = 'https://github.com/stepchowfun/mull#installation-instructions';
const INSTALLATION_ACTION = 'View installation instructions';
const CONFIGURATION_ACTION = 'Configure executable path';

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

// This private command lets the language server reveal a source range in a document.
// [group:reveal_range_command]
const REVEAL_RANGE_COMMAND = 'mull.revealRange';

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

// This private command lets the language server reveal a directory in the explorer.
// [group:reveal_in_explorer_command]
const REVEAL_IN_EXPLORER_COMMAND = 'mull.revealInExplorer';

// Reveal a directory supplied by the language server in the explorer.
async function revealInExplorer(uriString: string): Promise<void> {
  // VS Code's command expects a URI object, which the language server can only pass as a string.
  await vscode.commands.executeCommand('revealInExplorer', vscode.Uri.parse(uriString));
}

// Determine whether an editor is one of the main editors showing a wiki. Embedded editors, such as
// the preview in the references view, have no view column, and are left alone.
function isMainWikiEditor(editor: vscode.TextEditor | undefined): editor is vscode.TextEditor {
  return (
    editor !== undefined && editor.document.languageId === 'mull' && editor.viewColumn !== undefined
  );
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

// These commands move the cursor to the start or end of the node containing it, optionally
// extending the selection.
const GO_TO_NODE_START_COMMAND = 'mull.goToNodeStart';
const GO_TO_NODE_END_COMMAND = 'mull.goToNodeEnd';
const SELECT_TO_NODE_START_COMMAND = 'mull.selectToNodeStart';
const SELECT_TO_NODE_END_COMMAND = 'mull.selectToNodeEnd';

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
const graphemeSegmenter = new Intl.Segmenter();

// Find the column at which an editor wraps a document's lines, if it's known. Bounded wrapping
// wraps at that column or at the edge of a narrower view, so it's at most that column.
function wordWrapColumn(document: vscode.TextDocument): number | undefined {
  const wordWrap = setting('editor', 'wordWrap', isString, document);
  return wordWrap === 'bounded' || wordWrap === 'wordWrapColumn'
    ? setting('editor', 'wordWrapColumn', isNumber, document)
    : undefined;
}

// Find the fewest rows that some lines could take on screen. The first and last lines may be
// counted from or to a character, for a view that begins or ends partway through a wrapped line,
// since counting all of such a line would overcount the rows in view. Each character takes at least
// a column, and wrapping at word boundaries or at the edge of a narrower view only adds rows, so
// each line takes at least its characters divided by the wrapping column, rounded up, and at least
// a row. Without a known wrapping column, each line is counted as a row.
function minimumRowsOfLines(
  document: vscode.TextDocument,
  wrapColumn: number | undefined,
  firstLine: number,
  lastLine: number,
  from = 0,
  to?: number,
): number {
  let rows = 0;
  for (let line = firstLine; line <= lastLine; line += 1) {
    const text = document
      .lineAt(line)
      .text.slice(line === firstLine ? from : 0, line === lastLine ? to : undefined);
    const characters = [...graphemeSegmenter.segment(text)].length;
    rows += wrapColumn === undefined ? 1 : Math.max(1, Math.ceil(characters / wrapColumn));
  }
  return rows;
}

// Find how many rows the editor keeps in view around the cursor when the cursor moves: the larger
// of the configured surrounding lines and the most lines sticky scroll shows, if it's enabled. The
// editor also caps this at half the view, which isn't known here, so this may be more than the
// editor keeps in view, but never less.
function cursorSurroundingRows(document: vscode.TextDocument): number {
  return Math.max(
    setting('editor', 'cursorSurroundingLines', isNumber, document),
    setting('editor', 'stickyScroll.enabled', isBoolean, document)
      ? setting('editor', 'stickyScroll.maxLineCount', isNumber, document)
      : 0,
  );
}

// Wait this long after the view stops moving before settling it onto the current node. Scrolling
// with a trackpad or with smooth scrolling changes the view every frame, so this is long enough to
// outlast the gaps between those changes, which would otherwise settle the view while it's still
// being scrolled, yet short enough that settling feels like a response to the scrolling.
const SETTLE_DELAY_MILLISECONDS = 150;

// When settling, aim this many rows inside the allowance for other nodes. Rows are counted as the
// fewest that lines could take, and the editor's wrapping at word boundaries usually adds a row or
// so, so aiming at the allowance itself tends to land just outside it and settle again in a second,
// small step. Two rows absorbed that shortfall in testing.
const SETTLE_SLACK_ROWS = 2;

// This keeps the node containing the cursor, the current node, in focus in each main editor showing
// a wiki. It dims everything outside the current node, and once the view and the cursor stop
// moving, it settles the view back toward the current node, like a rubber band, if the view shows
// too much of the nodes before or after it.
//
// The editor gives extensions little control over scrolling, which shapes how settling works:
//
// - Settling happens after the view stops moving, since there's no way to limit or intercept
//   scrolling, only to observe it after the fact.
// - The view may show as many rows of other nodes as the editor keeps in view around the cursor,
//   the allowance. The editor scrolls that far on its own when the cursor nears the current node's
//   edge, and settling within the allowance would fight it.
// - Settling scrolls back only the rows beyond the allowance, so crossing it isn't abrupt.
// - Revealing a range pads it with as many rows as the allowance, so settling scrolls by lines or
//   rows instead, with the `editorScroll` command, which acts on the focused editor. So only the
//   active editor settles.
// - The visible ranges are in lines, but the view is in rows, since lines can wrap. Settling counts
//   the fewest rows lines could take, which errs toward settling too little rather than too much,
//   so it never scrolls past the current node's edge.
class NodeFocus implements vscode.Disposable {
  // Dim other nodes enough for the current node to stand out, while keeping them readable.
  private readonly decorationType = vscode.window.createTextEditorDecorationType({
    opacity: '0.5',
  });

  // Remember each wiki's nodes, which are found again after every edit.
  private readonly nodes = new Map<string, vscode.Range[]>();

  // Remember each editor's pending settling, which is postponed while its view keeps moving.
  private readonly settleTimers = new Map<vscode.TextEditor, ReturnType<typeof setTimeout>>();

  // Determine whether the user wants other nodes dimmed.
  private static dimsOtherNodes(): boolean {
    return setting('mull', 'dimOtherNodes', isBoolean);
  }

  // Determine whether the user wants the view to settle back toward the current node.
  private static snapsBackToCurrentNode(): boolean {
    return setting('mull', 'snapBackToCurrentNode', isBoolean);
  }

  // Dim around an editor's current node, using the nodes last found in its wiki. Blank lines at the
  // end of the current node aren't dimmed, since they belong to it.
  public dim(editor: vscode.TextEditor): void {
    // Find the current node and the node after it.
    const nodes = this.nodes.get(editor.document.uri.toString()) ?? [];
    const cursor = editor.selection.active;
    const node = NodeFocus.dimsOtherNodes() ? nodeAt(nodes, cursor) : undefined;
    if (node === undefined) {
      editor.setDecorations(this.decorationType, []);
      return;
    }
    const nextNode = nodes.find((range) => range.start.isAfter(cursor));

    // Dim the lines before the current node, and those from the next node to the end of the wiki.
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

  // Settle an editor's view once it and the cursor stop moving, postponing any pending settling.
  // Settling after the cursor moves pulls the current node into view when the cursor moves to
  // another node, as when clicking a dimmed node. Otherwise it finds nothing to do, since the view
  // already settled when it last moved.
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

  // Scroll the view back toward the current node if it shows more rows of the node before it than
  // the allowance, or more rows of the node after it while the current node's top is out of view.
  // It scrolls back to show the target number of rows of that node, which is a little inside the
  // allowance. See the class's description for why it works this way.
  private async settle(editor: vscode.TextEditor): Promise<void> {
    // Find the current node, and the lines in view from the first completely visible character to
    // the last one, without assuming the visible ranges are in order. Only the active editor can be
    // scrolled without padding.
    const nodes = this.nodes.get(editor.document.uri.toString()) ?? [];
    const cursor = editor.selection.active;
    const node = nodeAt(nodes, cursor);
    let visibleRange: vscode.Range | undefined = undefined;
    for (const range of editor.visibleRanges) {
      visibleRange = visibleRange === undefined ? range : visibleRange.union(range);
    }
    if (
      !NodeFocus.snapsBackToCurrentNode() ||
      editor !== vscode.window.activeTextEditor ||
      node === undefined ||
      visibleRange === undefined
    ) {
      return;
    }
    const firstVisible = visibleRange.start;
    const lastVisible = visibleRange.end;

    // Find the current node's last line, including the blank lines after its text, which belong to
    // it. It ends just before the next node, or at the end of the wiki.
    const nextNode = nodes.find((range) => range.start.isAfter(cursor));
    const nodeLastLine =
      nextNode === undefined ? editor.document.lineCount - 1 : nextNode.start.line - 1;

    // Count the rows in view before and after the current node.
    const wrapColumn = wordWrapColumn(editor.document);
    const rows = (firstLine: number, lastLine: number, from?: number, to?: number): number =>
      minimumRowsOfLines(editor.document, wrapColumn, firstLine, lastLine, from, to);
    const rowsBefore =
      firstVisible.line < node.start.line
        ? rows(firstVisible.line, node.start.line - 1, firstVisible.character)
        : 0;
    const rowsAfter =
      lastVisible.line > nodeLastLine
        ? rows(nodeLastLine + 1, lastVisible.line, 0, lastVisible.character)
        : 0;
    const allowance = cursorSurroundingRows(editor.document);
    const target = Math.max(allowance - SETTLE_SLACK_ROWS, 0);

    // Scroll down by the fewest lines that leave no more than the target number of rows of the node
    // before the current one. The editor counts these lines from the first completely visible one
    // and aligns them exactly, so scrolling down by a line puts the next line at the top. The first
    // visible line may be only partly in view, so scrolling starts at the line after it.
    if (rowsBefore > allowance) {
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

    // Scroll up by the rows of the node after the current one beyond the target. This scrolls by
    // rows rather than lines, since the editor would count lines at the top of the view, where the
    // current node's wrapped lines can take several rows each. Those rows are counted as the fewest
    // the lines could take, so the current node's end isn't passed, and they're capped at the
    // fewest rows the current node's text before the view could take, so its top isn't passed
    // either. A current node whose top is in view isn't scrolled up, since that would only bring
    // the node before it into view.
    if (firstVisible.line > node.start.line && rowsAfter > allowance) {
      const rowsHidden =
        firstVisible.character > 0
          ? rows(node.start.line, firstVisible.line, 0, firstVisible.character)
          : rows(node.start.line, firstVisible.line - 1);
      await vscode.commands.executeCommand('editorScroll', {
        to: 'up',
        by: 'wrappedLine',
        value: Math.min(rowsAfter - target, rowsHidden),
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

// Retain the active client so it can be stopped when the extension is deactivated.
let client: LanguageClient | undefined;

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

  // Keep the node containing the cursor in focus, following the cursor, edits, scrolling, the
  // visible editors, and the settings.
  const nodeFocus = new NodeFocus();
  context.subscriptions.push(
    nodeFocus,
    vscode.window.onDidChangeTextEditorSelection((event) => {
      if (isMainWikiEditor(event.textEditor)) {
        nodeFocus.dim(event.textEditor);
        nodeFocus.scheduleSettle(event.textEditor);
      }
    }),
    vscode.workspace.onDidChangeTextDocument(async (event) => {
      if (event.document.languageId === 'mull') {
        await nodeFocus.refresh(event.document);
      }
    }),
    vscode.window.onDidChangeVisibleTextEditors(async () => nodeFocus.refreshVisible()),
    vscode.window.onDidChangeTextEditorVisibleRanges((event) => {
      if (isMainWikiEditor(event.textEditor)) {
        nodeFocus.scheduleSettle(event.textEditor);
      }
    }),
    vscode.workspace.onDidCloseTextDocument((document) => {
      nodeFocus.forget(document);
    }),
    vscode.workspace.onDidChangeConfiguration(async (event) => {
      if (event.affectsConfiguration('mull.dimOtherNodes')) {
        await nodeFocus.refreshVisible();
      }
    }),
  );

  // Resolve the configured executable before constructing the server process.
  const executablePath = setting('mull', 'executablePath', isString);

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
  await nodeFocus.refreshVisible();
}

// Shut down the language client and its server process with the extension.
export async function deactivate(): Promise<void> {
  if (client !== undefined) {
    await client.dispose();
  }
  client = undefined;
}
