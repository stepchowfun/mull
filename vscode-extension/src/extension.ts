// Load the editor API and its Language Server Protocol client.
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import * as vscode from 'vscode';
import { LanguageClient } from 'vscode-languageclient/node';

// Make executable probes compatible with the extension's asynchronous startup.
const execFileAsync = promisify(execFile);

// Determine the type of a setting's value.
const isBoolean = (value: unknown): value is boolean => typeof value === 'boolean';
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

// List a wiki's nodes in source order, using the language server's document symbols, which are
// named after the nodes' titles and whose ranges span entire nodes.
async function wikiNodes(document: vscode.TextDocument): Promise<vscode.DocumentSymbol[]> {
  const symbols = await vscode.commands.executeCommand<vscode.DocumentSymbol[] | undefined>(
    'vscode.executeDocumentSymbolProvider',
    document.uri,
  );
  return (symbols ?? []).toSorted((a, b) => a.range.start.compareTo(b.range.start));
}

// Find the node containing a position, which is the last node starting at or before it, so a
// position between nodes belongs to the one above. A position before the first node has none.
function nodeAt(
  nodes: readonly vscode.DocumentSymbol[],
  position: vscode.Position,
): vscode.DocumentSymbol | undefined {
  return nodes.findLast((node) => node.range.start.isBeforeOrEqual(position));
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
  const nodes = await wikiNodes(editor.document);

  // Move the active end of each selection, keeping its anchor when extending it.
  editor.selections = editor.selections.map((selection) => {
    const node = nodeAt(nodes, selection.active);
    if (node === undefined) {
      return selection;
    }
    const position = boundary === 'start' ? node.range.start : node.range.end;
    return new vscode.Selection(select ? selection.anchor : position, position);
  });
  editor.revealRange(new vscode.Range(editor.selection.active, editor.selection.active));
}

// Wait this long after the view stops moving before settling it onto the current node. Scrolling
// with a trackpad or with smooth scrolling changes the view every frame, so this is long enough to
// outlast the gaps between those changes, which would otherwise settle the view while it's still
// being scrolled, yet short enough that settling feels like a response to the scrolling.
const SETTLE_DELAY_MILLISECONDS = 150;

// This keeps the node containing the cursor, the current node, in focus in each main editor showing
// a wiki. It dims everything outside the current node, and once the view and the cursor stop
// moving, it brings the current node back into view, like a rubber band, if it was scrolled away.
//
// Settling waits for the view to stop moving, since there's no way to limit or intercept scrolling,
// only to observe it after the fact. It reveals only the node's nearer edge, since the editor
// reveals a range taller than the view by jumping to its start, which would make the rest of a long
// node unreachable. Revealing a single line scrolls as little as possible, keeping the margin the
// editor keeps around the cursor, so it doesn't fight the editor's own scrolling.
class NodeFocus implements vscode.Disposable {
  // Dim other nodes enough for the current node to stand out, while keeping them readable.
  private readonly decorationType = vscode.window.createTextEditorDecorationType({
    opacity: '0.5',
  });

  // Remember each wiki's nodes, which are found again after every edit.
  private readonly nodes = new Map<string, vscode.DocumentSymbol[]>();

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
    const nextNode = nodes.find((other) => other.range.start.isAfter(cursor));

    // Dim the lines before the current node, and those from the next node to the end of the wiki.
    const documentEnd = editor.document.lineAt(editor.document.lineCount - 1).range.end;
    editor.setDecorations(
      this.decorationType,
      [
        new vscode.Range(new vscode.Position(0, 0), node.range.start),
        ...(nextNode === undefined ? [] : [new vscode.Range(nextNode.range.start, documentEnd)]),
      ].filter((range) => !range.isEmpty),
    );
  }

  // Find a wiki's nodes again and dim its editors accordingly. A result is discarded if the wiki
  // changed while it was being found, since a later refresh will replace it.
  public async refresh(document: vscode.TextDocument): Promise<void> {
    const { version } = document;
    const nodes = await wikiNodes(document);
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
      setTimeout(() => {
        this.settleTimers.delete(editor);
        this.settle(editor);
      }, SETTLE_DELAY_MILLISECONDS),
    );
  }

  // Reveal the nearer edge of an editor's current node if the node is entirely out of view, without
  // assuming the visible ranges are in order. The node includes the blank lines after its text,
  // which belong to it, so it ends just before the next node, or at the end of the wiki.
  private settle(editor: vscode.TextEditor): void {
    // Find the current node and the lines in view.
    const nodes = this.nodes.get(editor.document.uri.toString()) ?? [];
    const cursor = editor.selection.active;
    const node = nodeAt(nodes, cursor);
    let visibleRange: vscode.Range | undefined = undefined;
    for (const range of editor.visibleRanges) {
      visibleRange = visibleRange === undefined ? range : visibleRange.union(range);
    }
    if (!NodeFocus.snapsBackToCurrentNode() || node === undefined || visibleRange === undefined) {
      return;
    }
    const nextNode = nodes.find((other) => other.range.start.isAfter(cursor));
    const lastLine =
      nextNode === undefined ? editor.document.lineCount - 1 : nextNode.range.start.line - 1;

    // Reveal the node's first line if it's below the view, or its last line if it's above.
    if (node.range.start.line > visibleRange.end.line) {
      editor.revealRange(new vscode.Range(node.range.start, node.range.start));
    } else if (lastLine < visibleRange.start.line) {
      const end = editor.document.lineAt(lastLine).range.end;
      editor.revealRange(new vscode.Range(end, end));
    }
  }

  // List the nodes last found in a wiki.
  public nodesOf(document: vscode.TextDocument): readonly vscode.DocumentSymbol[] {
    return this.nodes.get(document.uri.toString()) ?? [];
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

// These commands move through the nodes visited in a wiki, like a browser's back and forward
// buttons.
const GO_BACK_COMMAND = 'mull.goBack';
const GO_FORWARD_COMMAND = 'mull.goForward';

// This title identifies the root of every wiki's text-link graph. Keep this in sync with
// [group:home_title].
const HOME_TITLE = 'Home';

// A visit to a node, with the cursor's position in it, relative to the node's start, for returning
// to where the cursor was.
interface Visit {
  readonly title: string;
  readonly line: number;
  readonly character: number;
}

// A wiki's trail of visits, ending with the current node, and the visits that going back left
// ahead, the next one last.
interface Trail {
  back: Visit[];
  forward: Visit[];
}

// This keeps a trail of the nodes visited in each wiki, which leads from Home to the current node,
// and shows the previous node above the current node's title, like a browser's back button.
//
// Moving the cursor to another node visits that node. Visiting a node already in the trail cuts the
// trail back to it, so the trail never loops, and visiting any node discards the visits that going
// back left ahead. The trail always starts with Home, as if every visit began there, so going back
// always leads home, and visiting Home starts the trail over.
class NodeHistory implements vscode.CodeLensProvider, vscode.Disposable {
  // Remember each wiki's trail.
  private readonly trails = new Map<string, Trail>();

  // Tell the editor to ask for the code lens again when a trail changes.
  private readonly changeEmitter = new vscode.EventEmitter<void>();

  public readonly onDidChangeCodeLenses = this.changeEmitter.event;

  // Find the nodes the dimming last found, which are current enough for following the cursor.
  private readonly nodeFocus: NodeFocus;

  public constructor(nodeFocus: NodeFocus) {
    this.nodeFocus = nodeFocus;
  }

  // Find a wiki's trail, dropping visits to nodes that no longer exist, as after renaming them.
  private trailOf(document: vscode.TextDocument, nodes: readonly vscode.DocumentSymbol[]): Trail {
    const key = document.uri.toString();
    const titles = new Set(nodes.map((node) => node.name));
    const trail = this.trails.get(key) ?? { back: [], forward: [] };
    trail.back = trail.back.filter((visit) => titles.has(visit.title));
    trail.forward = trail.forward.filter((visit) => titles.has(visit.title));
    this.trails.set(key, trail);
    return trail;
  }

  // Find the active editor's wiki, its nodes, and its trail, if it's showing one.
  private activeTrail():
    | { editor: vscode.TextEditor; nodes: readonly vscode.DocumentSymbol[]; trail: Trail }
    | undefined {
    const editor = vscode.window.activeTextEditor;
    if (!isMainWikiEditor(editor)) {
      return undefined;
    }
    const nodes = this.nodeFocus.nodesOf(editor.document);
    return { editor, nodes, trail: this.trailOf(editor.document, nodes) };
  }

  // Record where an editor's cursor is, visiting its node if it moved to another one.
  public record(editor: vscode.TextEditor): void {
    // Find the cursor's node and its position in it.
    const nodes = this.nodeFocus.nodesOf(editor.document);
    const cursor = editor.selection.active;
    const node = nodeAt(nodes, cursor);
    if (node === undefined) {
      return;
    }
    const visit = {
      title: node.name,
      line: cursor.line - node.range.start.line,
      character: cursor.character,
    };

    // Within the current node, only remember the cursor's position.
    const trail = this.trailOf(editor.document, nodes);
    const current = trail.back.at(-1);
    if (current !== undefined && current.title === visit.title) {
      trail.back[trail.back.length - 1] = visit;
      return;
    }

    // Visit the node, cutting the trail back to it if it's already in it, and start the trail with
    // Home, unless the wiki has none.
    const index = trail.back.findIndex((other) => other.title === visit.title);
    if (index !== -1) {
      trail.back.splice(index);
    }
    trail.back.push(visit);
    const [first] = trail.back;
    if (first.title !== HOME_TITLE && nodes.some((other) => other.name === HOME_TITLE)) {
      trail.back.unshift({ title: HOME_TITLE, line: 0, character: 0 });
    }
    trail.forward = [];
    this.changeEmitter.fire();
  }

  // Record where the active editor's cursor is once its wiki's nodes are found again. This also
  // starts the trail of a newly shown wiki.
  public recordActive(): void {
    const editor = vscode.window.activeTextEditor;
    if (isMainWikiEditor(editor)) {
      this.record(editor);
    }
  }

  // Go back to the previous node in the active wiki's trail, keeping the current one for going
  // forward again.
  public goBack(): void {
    const active = this.activeTrail();
    if (active === undefined) {
      return;
    }
    const { back, forward } = active.trail;
    const current = back.at(-1);
    const previous = back.at(-2);
    if (current === undefined || previous === undefined) {
      return;
    }
    back.pop();
    forward.push(current);
    NodeHistory.revisit(active.editor, active.nodes, previous);
    this.changeEmitter.fire();
  }

  // Go forward to the node that going back left ahead in the active wiki.
  public goForward(): void {
    const active = this.activeTrail();
    if (active === undefined) {
      return;
    }
    const { back, forward } = active.trail;
    const next = forward.pop();
    if (next === undefined) {
      return;
    }
    back.push(next);
    NodeHistory.revisit(active.editor, active.nodes, next);
    this.changeEmitter.fire();
  }

  // Move the cursor back to where it was in a visited node. The resulting selection change finds
  // the cursor already in the trail's current node, so it doesn't count as a new visit.
  private static revisit(
    editor: vscode.TextEditor,
    nodes: readonly vscode.DocumentSymbol[],
    visit: Visit,
  ): void {
    const node = nodes.find((other) => other.name === visit.title);
    if (node === undefined) {
      return;
    }
    const line = Math.min(node.range.start.line + visit.line, node.range.end.line);
    const position = editor.document.validatePosition(new vscode.Position(line, visit.character));
    editor.selection = new vscode.Selection(position, position);
    editor.revealRange(
      new vscode.Range(position, position),
      vscode.TextEditorRevealType.InCenterIfOutsideViewport,
    );
  }

  // Show the previous node above the current node's title, as a link back to it, with the whole
  // trail in its tooltip.
  public async provideCodeLenses(document: vscode.TextDocument): Promise<vscode.CodeLens[]> {
    // Find the current and previous nodes.
    const nodes = await wikiNodes(document);
    const { back } = this.trailOf(document, nodes);
    const currentVisit = back.at(-1);
    const previous = back.at(-2);
    const current =
      currentVisit === undefined
        ? undefined
        : nodes.find((node) => node.name === currentVisit.title);
    if (current === undefined || previous === undefined) {
      return [];
    }

    // Place the link above the current node's title. The command takes no arguments, since the
    // editor may keep showing a lens's command after the lens is replaced.
    return [
      new vscode.CodeLens(document.lineAt(current.range.start.line).range, {
        title: `← ${previous.title}`,
        tooltip: back.map((visit) => visit.title).join(' › '),
        command: GO_BACK_COMMAND,
      }),
    ];
  }

  // Forget the trail of a wiki that's no longer open.
  public forget(document: vscode.TextDocument): void {
    this.trails.delete(document.uri.toString());
  }

  // Release the event emitter.
  public dispose(): void {
    this.changeEmitter.dispose();
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
  const nodeHistory = new NodeHistory(nodeFocus);
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
        nodeHistory.recordActive();
      }
    }),
    vscode.window.onDidChangeVisibleTextEditors(async () => {
      await nodeFocus.refreshVisible();
      nodeHistory.recordActive();
    }),
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

  // Keep a trail of the nodes visited in each wiki, shown as a link back to the previous node, and
  // expose the commands that move along it. The trail follows the nodes found above.
  context.subscriptions.push(
    nodeHistory,
    vscode.languages.registerCodeLensProvider({ language: 'mull' }, nodeHistory),
    vscode.commands.registerCommand(GO_BACK_COMMAND, () => {
      nodeHistory.goBack();
    }),
    vscode.commands.registerCommand(GO_FORWARD_COMMAND, () => {
      nodeHistory.goForward();
    }),
    vscode.window.onDidChangeTextEditorSelection((event) => {
      if (isMainWikiEditor(event.textEditor)) {
        nodeHistory.record(event.textEditor);
      }
    }),
    vscode.workspace.onDidCloseTextDocument((document) => {
      nodeHistory.forget(document);
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

  // Find the nodes of the wikis already shown, which needs the language server, and start the
  // active wiki's trail.
  await nodeFocus.refreshVisible();
  nodeHistory.recordActive();
}

// Shut down the language client and its server process with the extension.
export async function deactivate(): Promise<void> {
  if (client !== undefined) {
    await client.dispose();
  }
  client = undefined;
}
