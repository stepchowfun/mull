// Load the editor API and its Language Server Protocol client.
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import * as vscode from 'vscode';
import { type ExecuteCommandSignature, LanguageClient } from 'vscode-languageclient/node';

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
  revealType: vscode.TextEditorRevealType,
): Promise<void> {
  // Open either a file-backed or untitled document, select the range, which may be empty, and
  // reveal it as the language server asks.
  const document = await vscode.workspace.openTextDocument(vscode.Uri.parse(uriString));
  const editor = await vscode.window.showTextDocument(document);
  const range = new vscode.Range(startLine, startCharacter, endLine, endCharacter);
  editor.selection = new vscode.Selection(range.start, range.end);
  editor.revealRange(range, revealType);
}

// This private command lets the language server reveal a directory in the explorer, or in the
// system's file manager.
// [group:reveal_in_explorer_command]
const REVEAL_IN_EXPLORER_COMMAND = 'mull.revealInExplorer';

// Reveal a directory supplied by the language server in the explorer. The explorer only shows the
// workspace's folders, so a directory outside them opens in the system's file manager instead.
async function revealInExplorer(uriString: string): Promise<void> {
  // VS Code expects a URI object, which the language server can only pass as a string.
  const uri = vscode.Uri.parse(uriString);
  if (vscode.workspace.getWorkspaceFolder(uri) === undefined) {
    await vscode.env.openExternal(uri);
  } else {
    await vscode.commands.executeCommand('revealInExplorer', uri);
  }
}

// Determine whether an editor is one of the main editors showing a wiki. Embedded editors, such as
// the preview in the references view, have no view column, and are left alone.
function isMainWikiEditor(editor: vscode.TextEditor | undefined): editor is vscode.TextEditor {
  return (
    editor !== undefined && editor.document.languageId === 'mull' && editor.viewColumn !== undefined
  );
}

// This language server command checks a wiki again, defaulting to every open wiki. Keep this in
// sync with [group:check_wiki_command].
const CHECK_WIKI_COMMAND = 'mull.checkWiki';

// Check only the wiki being edited when the check command doesn't name one.
function checkActiveWiki(
  command: string,
  args: unknown[],
  next: ExecuteCommandSignature,
): vscode.ProviderResult<unknown> {
  const editor = vscode.window.activeTextEditor;
  if (command === CHECK_WIKI_COMMAND && args.length === 0 && isMainWikiEditor(editor)) {
    return next(command, [editor.document.uri.toString()]);
  }
  return next(command, args);
}

// List a wiki's pages in source order, using the language server's document symbols, which are
// named after the pages' titles and whose ranges span entire pages.
async function wikiPages(document: vscode.TextDocument): Promise<vscode.DocumentSymbol[]> {
  const symbols = await vscode.commands.executeCommand<vscode.DocumentSymbol[] | undefined>(
    'vscode.executeDocumentSymbolProvider',
    document.uri,
  );
  return (symbols ?? []).toSorted((a, b) => a.range.start.compareTo(b.range.start));
}

// Find the page containing a position, which is the last page starting at or before it, so a
// position between pages belongs to the one above. A position before the first page has none.
function pageAt(
  pages: readonly vscode.DocumentSymbol[],
  position: vscode.Position,
): vscode.DocumentSymbol | undefined {
  return pages.findLast((page) => page.range.start.isBeforeOrEqual(position));
}

// These commands move the cursor to the start or end of the page containing it, optionally
// extending the selection.
const GO_TO_PAGE_START_COMMAND = 'mull.goToPageStart';
const GO_TO_PAGE_END_COMMAND = 'mull.goToPageEnd';
const SELECT_TO_PAGE_START_COMMAND = 'mull.selectToPageStart';
const SELECT_TO_PAGE_END_COMMAND = 'mull.selectToPageEnd';

// Move each cursor to the start or end of the page containing it. A cursor before the first page
// stays put, and repeating the command changes nothing.
async function moveToPageBoundary(boundary: 'start' | 'end', select: boolean): Promise<void> {
  // Find the pages of the active wiki.
  const editor = vscode.window.activeTextEditor;
  if (editor === undefined) {
    return;
  }
  const pages = await wikiPages(editor.document);

  // Move the active end of each selection, keeping its anchor when extending it.
  editor.selections = editor.selections.map((selection) => {
    const page = pageAt(pages, selection.active);
    if (page === undefined) {
      return selection;
    }
    const position = boundary === 'start' ? page.range.start : page.range.end;
    return new vscode.Selection(select ? selection.anchor : position, position);
  });
  editor.revealRange(new vscode.Range(editor.selection.active, editor.selection.active));
}

// Wait this long after the view stops moving before settling it onto the current page. Scrolling
// with a trackpad or with smooth scrolling changes the view every frame, so this is long enough to
// outlast the gaps between those changes, which would otherwise settle the view while it's still
// being scrolled, yet short enough that settling feels like a response to the scrolling.
const SETTLE_DELAY_MILLISECONDS = 150;

// This keeps the page containing the cursor, the current page, in focus in each main editor showing
// a wiki. It dims everything outside the current page, and once the view and the cursor stop
// moving, it brings the current page back into view, like a rubber band, if it was scrolled away.
//
// Settling waits for the view to stop moving, since there's no way to limit or intercept scrolling,
// only to observe it after the fact. It reveals only the page's nearer edge, since the editor
// reveals a range taller than the view by jumping to its start, which would make the rest of a long
// page unreachable. Revealing a single line scrolls as little as possible, keeping the margin the
// editor keeps around the cursor, so it doesn't fight the editor's own scrolling.
class PageFocus implements vscode.Disposable {
  // Dim other pages enough for the current page to stand out, while keeping them readable.
  private readonly decorationType = vscode.window.createTextEditorDecorationType({
    opacity: '0.5',
  });

  // Remember each wiki's pages, which are found again after every edit.
  private readonly pages = new Map<string, vscode.DocumentSymbol[]>();

  // Remember each editor's pending settling, which is postponed while its view keeps moving.
  private readonly settleTimers = new Map<vscode.TextEditor, ReturnType<typeof setTimeout>>();

  // Determine whether the user wants other pages dimmed.
  private static dimsOtherPages(): boolean {
    return setting('mull', 'dimOtherPages', isBoolean);
  }

  // Determine whether the user wants the view to settle back toward the current page.
  private static snapsBackToCurrentPage(): boolean {
    return setting('mull', 'snapBackToCurrentPage', isBoolean);
  }

  // Dim around an editor's current page, using the pages last found in its wiki. Blank lines at the
  // end of the current page aren't dimmed, since they belong to it.
  public dim(editor: vscode.TextEditor): void {
    // Find the current page and the page after it.
    const pages = this.pages.get(editor.document.uri.toString()) ?? [];
    const cursor = editor.selection.active;
    const page = PageFocus.dimsOtherPages() ? pageAt(pages, cursor) : undefined;
    if (page === undefined) {
      editor.setDecorations(this.decorationType, []);
      return;
    }
    const nextPage = pages.find((other) => other.range.start.isAfter(cursor));

    // Dim the lines before the current page, and those from the next page to the end of the wiki.
    const documentEnd = editor.document.lineAt(editor.document.lineCount - 1).range.end;
    editor.setDecorations(
      this.decorationType,
      [
        new vscode.Range(new vscode.Position(0, 0), page.range.start),
        ...(nextPage === undefined ? [] : [new vscode.Range(nextPage.range.start, documentEnd)]),
      ].filter((range) => !range.isEmpty),
    );
  }

  // Find a wiki's pages again and dim its editors accordingly. A result is discarded if the wiki
  // changed while it was being found, since a later refresh will replace it.
  public async refresh(document: vscode.TextDocument): Promise<void> {
    const { version } = document;
    const pages = await wikiPages(document);
    if (document.version !== version) {
      return;
    }
    this.pages.set(document.uri.toString(), pages);
    for (const editor of vscode.window.visibleTextEditors) {
      if (isMainWikiEditor(editor) && editor.document === document) {
        this.dim(editor);
      }
    }
  }

  // Find the pages of every wiki shown in a main editor.
  public async refreshVisible(): Promise<void> {
    const documents = new Set(
      vscode.window.visibleTextEditors.filter(isMainWikiEditor).map((editor) => editor.document),
    );
    await Promise.all([...documents].map(async (document) => this.refresh(document)));
  }

  // Settle an editor's view once it and the cursor stop moving, postponing any pending settling.
  // Settling after the cursor moves pulls the current page into view when the cursor moves to
  // another page, as when clicking a dimmed page. Otherwise it finds nothing to do, since the view
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

  // Reveal the nearer edge of an editor's current page if the page is entirely out of view, without
  // assuming the visible ranges are in order. The page includes the blank lines after its text,
  // which belong to it, so it ends just before the next page, or at the end of the wiki.
  private settle(editor: vscode.TextEditor): void {
    // Find the current page and the lines in view.
    const pages = this.pages.get(editor.document.uri.toString()) ?? [];
    const cursor = editor.selection.active;
    const page = pageAt(pages, cursor);
    let visibleRange: vscode.Range | undefined = undefined;
    for (const range of editor.visibleRanges) {
      visibleRange = visibleRange === undefined ? range : visibleRange.union(range);
    }
    if (!PageFocus.snapsBackToCurrentPage() || page === undefined || visibleRange === undefined) {
      return;
    }
    const nextPage = pages.find((other) => other.range.start.isAfter(cursor));
    const lastLine =
      nextPage === undefined ? editor.document.lineCount - 1 : nextPage.range.start.line - 1;

    // Reveal the page's first line if it's below the view, or its last line if it's above.
    if (page.range.start.line > visibleRange.end.line) {
      editor.revealRange(new vscode.Range(page.range.start, page.range.start));
    } else if (lastLine < visibleRange.start.line) {
      const end = editor.document.lineAt(lastLine).range.end;
      editor.revealRange(new vscode.Range(end, end));
    }
  }

  // List the pages last found in a wiki.
  public pagesOf(document: vscode.TextDocument): readonly vscode.DocumentSymbol[] {
    return this.pages.get(document.uri.toString()) ?? [];
  }

  // Forget the pages of a wiki that's no longer open.
  public forget(document: vscode.TextDocument): void {
    this.pages.delete(document.uri.toString());
  }

  // Release the decoration type, which removes the dimming, and cancel any pending settling.
  public dispose(): void {
    this.decorationType.dispose();
    for (const timer of this.settleTimers.values()) {
      clearTimeout(timer);
    }
  }
}

// These commands move through the pages visited in a wiki, like a browser's back and forward
// buttons.
const GO_BACK_COMMAND = 'mull.goBack';
const GO_FORWARD_COMMAND = 'mull.goForward';

// This caps how many visits each wiki's trail remembers, forgetting the oldest first.
const MAX_HISTORY_LENGTH = 100;

// This caps how many of the latest visits the back link's tooltip lists.
const MAX_TOOLTIP_VISITS = 10;

// A visit to a page, with the cursor's position in it, relative to the page's start, for returning
// to where the cursor was.
interface Visit {
  readonly title: string;
  readonly line: number;
  readonly character: number;
}

// A wiki's trail of visits, ending with the current page, and the visits that going back left
// ahead, the next one last.
interface Trail {
  back: Visit[];
  forward: Visit[];
}

// This keeps a trail of the pages visited in each wiki, and shows the previous page above the
// current page's title, like a browser's back button.
//
// Moving the cursor to another page, by any means, visits that page. Like a browser's history, the
// trail is never pruned of loops, so going back always returns to the page visited just before,
// and visiting a page discards the visits that going back left ahead.
class PageHistory implements vscode.CodeLensProvider, vscode.Disposable {
  // Remember each wiki's trail.
  private readonly trails = new Map<string, Trail>();

  // Tell the editor to ask for the code lens again when a trail changes.
  private readonly changeEmitter = new vscode.EventEmitter<void>();

  public readonly onDidChangeCodeLenses = this.changeEmitter.event;

  // Find the pages the dimming last found, which are current enough for following the cursor.
  private readonly pageFocus: PageFocus;

  public constructor(pageFocus: PageFocus) {
    this.pageFocus = pageFocus;
  }

  // Find a wiki's trail, dropping visits to pages that no longer exist, as after renaming them.
  private trailOf(document: vscode.TextDocument, pages: readonly vscode.DocumentSymbol[]): Trail {
    const key = document.uri.toString();
    const titles = new Set(pages.map((page) => page.name));
    const trail = this.trails.get(key) ?? { back: [], forward: [] };
    trail.back = PageHistory.existingVisits(trail.back, titles);
    trail.forward = PageHistory.existingVisits(trail.forward, titles);
    this.trails.set(key, trail);
    return trail;
  }

  // Keep the visits to pages that still exist, merging visits to the same page that become
  // adjacent into the later one, so going back or forward always changes the page.
  private static existingVisits(visits: readonly Visit[], titles: ReadonlySet<string>): Visit[] {
    const kept: Visit[] = [];
    for (const visit of visits) {
      if (!titles.has(visit.title)) {
        continue;
      }
      const last = kept.at(-1);
      if (last !== undefined && last.title === visit.title) {
        kept[kept.length - 1] = visit;
      } else {
        kept.push(visit);
      }
    }
    return kept;
  }

  // Find the active editor's wiki, its pages, and its trail, if it's showing one.
  private activeTrail():
    | { editor: vscode.TextEditor; pages: readonly vscode.DocumentSymbol[]; trail: Trail }
    | undefined {
    const editor = vscode.window.activeTextEditor;
    if (!isMainWikiEditor(editor)) {
      return undefined;
    }
    const pages = this.pageFocus.pagesOf(editor.document);
    return { editor, pages, trail: this.trailOf(editor.document, pages) };
  }

  // Record where an editor's cursor is, visiting its page if it moved to another one.
  public record(editor: vscode.TextEditor): void {
    // Find the cursor's page and its position in it.
    const pages = this.pageFocus.pagesOf(editor.document);
    const cursor = editor.selection.active;
    const page = pageAt(pages, cursor);
    if (page === undefined) {
      return;
    }
    const visit = {
      title: page.name,
      line: cursor.line - page.range.start.line,
      character: cursor.character,
    };

    // Within the current page, only remember the cursor's position.
    const trail = this.trailOf(editor.document, pages);
    const current = trail.back.at(-1);
    if (current !== undefined && current.title === visit.title) {
      trail.back[trail.back.length - 1] = visit;
      return;
    }

    // Visit the page, forgetting the oldest visit once there are too many.
    trail.back.push(visit);
    if (trail.back.length > MAX_HISTORY_LENGTH) {
      trail.back.shift();
    }
    trail.forward = [];
    this.changeEmitter.fire();
  }

  // Record where the active editor's cursor is once its wiki's pages are found again. This also
  // starts the trail of a newly shown wiki.
  public recordActive(): void {
    const editor = vscode.window.activeTextEditor;
    if (isMainWikiEditor(editor)) {
      this.record(editor);
    }
  }

  // Go back to the previous page in the active wiki's trail, keeping the current one for going
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
    PageHistory.revisit(active.editor, active.pages, previous);
    this.changeEmitter.fire();
  }

  // Go forward to the page that going back left ahead in the active wiki.
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
    PageHistory.revisit(active.editor, active.pages, next);
    this.changeEmitter.fire();
  }

  // Move the cursor back to where it was in a visited page. The resulting selection change finds
  // the cursor already in the trail's current page, so it doesn't count as a new visit.
  private static revisit(
    editor: vscode.TextEditor,
    pages: readonly vscode.DocumentSymbol[],
    visit: Visit,
  ): void {
    const page = pages.find((other) => other.name === visit.title);
    if (page === undefined) {
      return;
    }
    const line = Math.min(page.range.start.line + visit.line, page.range.end.line);
    const position = editor.document.validatePosition(new vscode.Position(line, visit.character));
    editor.selection = new vscode.Selection(position, position);
    editor.revealRange(
      new vscode.Range(position, position),
      vscode.TextEditorRevealType.InCenterIfOutsideViewport,
    );
  }

  // Show the previous page above the current page's title, as a link back to it, with the latest
  // visits in its tooltip.
  public async provideCodeLenses(document: vscode.TextDocument): Promise<vscode.CodeLens[]> {
    // Find the current and previous pages.
    const pages = await wikiPages(document);
    const { back } = this.trailOf(document, pages);
    const currentVisit = back.at(-1);
    const previous = back.at(-2);
    const current =
      currentVisit === undefined
        ? undefined
        : pages.find((page) => page.name === currentVisit.title);
    if (current === undefined || previous === undefined) {
      return [];
    }

    // List the latest visits, marking where older ones are left out.
    const titles = back.slice(-MAX_TOOLTIP_VISITS).map((visit) => visit.title);
    if (back.length > MAX_TOOLTIP_VISITS) {
      titles.unshift('…');
    }

    // Place the link above the current page's title. The command takes no arguments, since the
    // editor may keep showing a lens's command after the lens is replaced.
    return [
      new vscode.CodeLens(document.lineAt(current.range.start.line).range, {
        title: `← ${previous.title}`,
        tooltip: titles.join(' › '),
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

  // Expose the commands that move the cursor within the page containing it.
  const pageBoundaryCommands = [
    [GO_TO_PAGE_START_COMMAND, 'start', false],
    [GO_TO_PAGE_END_COMMAND, 'end', false],
    [SELECT_TO_PAGE_START_COMMAND, 'start', true],
    [SELECT_TO_PAGE_END_COMMAND, 'end', true],
  ] as const;
  for (const [command, boundary, select] of pageBoundaryCommands) {
    context.subscriptions.push(
      vscode.commands.registerCommand(command, async () => moveToPageBoundary(boundary, select)),
    );
  }

  // Keep the page containing the cursor in focus, following the cursor, edits, scrolling, the
  // visible editors, and the settings.
  const pageFocus = new PageFocus();
  const pageHistory = new PageHistory(pageFocus);
  context.subscriptions.push(
    pageFocus,
    vscode.window.onDidChangeTextEditorSelection((event) => {
      if (isMainWikiEditor(event.textEditor)) {
        pageFocus.dim(event.textEditor);
        pageFocus.scheduleSettle(event.textEditor);
      }
    }),
    vscode.workspace.onDidChangeTextDocument(async (event) => {
      if (event.document.languageId === 'mull') {
        await pageFocus.refresh(event.document);
        pageHistory.recordActive();
      }
    }),
    vscode.window.onDidChangeVisibleTextEditors(async () => {
      await pageFocus.refreshVisible();
      pageHistory.recordActive();
    }),
    vscode.window.onDidChangeTextEditorVisibleRanges((event) => {
      if (isMainWikiEditor(event.textEditor)) {
        pageFocus.scheduleSettle(event.textEditor);
      }
    }),
    vscode.workspace.onDidCloseTextDocument((document) => {
      pageFocus.forget(document);
    }),
    vscode.workspace.onDidChangeConfiguration(async (event) => {
      if (event.affectsConfiguration('mull.dimOtherPages')) {
        await pageFocus.refreshVisible();
      }
    }),
  );

  // Keep a trail of the pages visited in each wiki, shown as a link back to the previous page, and
  // expose the commands that move along it. The trail follows the pages found above.
  context.subscriptions.push(
    pageHistory,
    vscode.languages.registerCodeLensProvider({ language: 'mull' }, pageHistory),
    vscode.commands.registerCommand(GO_BACK_COMMAND, () => {
      pageHistory.goBack();
    }),
    vscode.commands.registerCommand(GO_FORWARD_COMMAND, () => {
      pageHistory.goForward();
    }),
    vscode.window.onDidChangeTextEditorSelection((event) => {
      if (isMainWikiEditor(event.textEditor)) {
        pageHistory.record(event.textEditor);
      }
    }),
    vscode.workspace.onDidCloseTextDocument((document) => {
      pageHistory.forget(document);
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
    middleware: { executeCommand: checkActiveWiki },
  };
  client = new LanguageClient('mull', 'Mull', serverOptions, clientOptions);
  await client.start();

  // Find the pages of the wikis already shown, which needs the language server, and start the
  // active wiki's trail.
  await pageFocus.refreshVisible();
  pageHistory.recordActive();
}

// Shut down the language client and its server process with the extension.
export async function deactivate(): Promise<void> {
  if (client !== undefined) {
    await client.dispose();
  }
  client = undefined;
}
