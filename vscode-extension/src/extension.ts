// Load the editor API and its Language Server Protocol client.
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import * as vscode from 'vscode';
import {
  DocumentFormattingRequest,
  ExecuteCommandRequest,
  type HandleDiagnosticsSignature,
  LanguageClient,
  type ProvideDocumentFormattingEditsSignature,
} from 'vscode-languageclient/node';

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

// Make executable probes compatible with the extension's asynchronous startup.
const execFileAsync = promisify(execFile);

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

// Retain the active client so it can be stopped when the extension is deactivated.
let client: LanguageClient | undefined;

// Determine whether an editor is one of the main editors showing a wiki. Embedded editors, such as
// the preview in the references view, have no view column, and are left alone.
function isMainWikiEditor(editor: vscode.TextEditor | undefined): editor is vscode.TextEditor {
  return (
    editor !== undefined && editor.document.languageId === 'mull' && editor.viewColumn !== undefined
  );
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

// This command checks the wiki being edited again, through the language server command of the same
// name. Keep this in sync with [group:check_wiki_command].
const CHECK_WIKI_COMMAND = 'mull.checkWiki';

// Ask the language server to check the wiki being edited, which reports the result.
async function checkActiveWiki(): Promise<void> {
  const editor = vscode.window.activeTextEditor;
  if (client === undefined || !isMainWikiEditor(editor)) {
    return;
  }
  await client.sendRequest(ExecuteCommandRequest.type, {
    command: CHECK_WIKI_COMMAND,
    arguments: [editor.document.uri.toString()],
  });
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
// a wiki. It dims everything outside the current page, and once the view and the cursor stop moving,
// it brings the current page back into view, like a rubber band, if it was scrolled away.
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
    const nextPage = pages.find((other) => other.range.start.isAfter(cursor));
    if (page === undefined) {
      editor.setDecorations(this.decorationType, []);
      return;
    }

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

  // Release the decoration types, which removes the dimming, and cancel any pending settling.
  public dispose(): void {
    this.decorationType.dispose();
    for (const timer of this.settleTimers.values()) {
      clearTimeout(timer);
    }
  }
}

// This dims the fences around blocks, including the hashes after closing fences, so a block's
// contents stand out from its fences in each main editor showing a wiki. Fences inside fenced code
// are code, so they're left alone.
class FenceDimming implements vscode.Disposable {
  // Dim the fences enough to recede like punctuation, while keeping them readable.
  private readonly decorationType = vscode.window.createTextEditorDecorationType({
    opacity: '0.5',
  });

  // Find an editor's fences and dim them.
  public dim(editor: vscode.TextEditor): void {
    const ranges: vscode.Range[] = [];
    let codeFenceLength: number | undefined = undefined;
    for (let line = 0; line < editor.document.lineCount; line += 1) {
      const text = editor.document.lineAt(line).text;

      // Skip fenced code, which starts and ends with lines starting with runs of the same number of
      // backticks.
      const codeFence = /^\s*(`{3,})/u.exec(text);
      if (codeFence !== null) {
        const length = codeFence[1].length;
        if (codeFenceLength === undefined) {
          codeFenceLength = length;
        } else if (length === codeFenceLength) {
          codeFenceLength = undefined;
        }
        continue;
      }
      if (codeFenceLength !== undefined) {
        continue;
      }

      // Dim the run of colons starting a fence, and what ends it after any argument: the run of
      // colons ending a block written on one line, or the hash after a closing fence.
      const fence = /^(\s*)(:{3,})(.*?)((?:\s:{3,})?(?:\s#[0-9a-f]{12})?)\s*$/u.exec(text);
      if (fence === null) {
        continue;
      }
      const [, indentation, opening, argument, ending] = fence;
      const openingEnd = indentation.length + opening.length;
      const endingStart = openingEnd + argument.length;
      ranges.push(new vscode.Range(line, indentation.length, line, openingEnd));
      if (ending !== '') {
        ranges.push(new vscode.Range(line, endingStart, line, endingStart + ending.length));
      }
    }
    editor.setDecorations(this.decorationType, ranges);
  }

  // Dim the fences in every main editor showing a wiki.
  public dimVisible(): void {
    for (const editor of vscode.window.visibleTextEditors) {
      if (isMainWikiEditor(editor)) {
        this.dim(editor);
      }
    }
  }

  // Release the decoration type, which removes the dimming.
  public dispose(): void {
    this.decorationType.dispose();
  }
}

// Hide the diagnostics at the cursor while the user types there, such as the error for a link whose
// target hasn't been typed yet, until the cursor moves away from them.
class TypingDiagnostics {
  // Remember each document's diagnostics, including hidden ones, and how to show them.
  private readonly diagnostics = new Map<string, vscode.Diagnostic[]>();
  private show: HandleDiagnosticsSignature | undefined = undefined;

  // Track the document being typed in, along with the ranges of its latest edit and its hidden
  // diagnostics, which the cursor stays within while the user types.
  private typingUri: string | undefined = undefined;
  private edits: vscode.Range[] = [];
  private hidden: vscode.Range[] = [];

  // Show a document's diagnostics from the language server, except those hidden at the cursor.
  public readonly handleDiagnostics = (
    uri: vscode.Uri,
    diagnostics: vscode.Diagnostic[],
    next: HandleDiagnosticsSignature,
  ): void => {
    this.diagnostics.set(uri.toString(), diagnostics);
    this.show = next;
    next(uri, this.visible(uri, diagnostics));
  };

  // Start hiding the diagnostics at the cursor when the user edits there, but not for edits
  // elsewhere, such as formatting.
  public recordEdit(event: vscode.TextDocumentChangeEvent): void {
    const { document } = event;
    const editor = vscode.window.activeTextEditor;
    if (document.languageId !== 'mull' || editor === undefined || editor.document !== document) {
      return;
    }
    const edits = event.contentChanges.map(
      (change) =>
        new vscode.Range(
          change.range.start,
          document.positionAt(change.rangeOffset + change.text.length),
        ),
    );
    if (!edits.some((range) => range.contains(editor.selection.active))) {
      return;
    }
    if (this.typingUri !== document.uri.toString()) {
      this.stopTyping();
      this.typingUri = document.uri.toString();
    }
    this.edits = edits;
  }

  // Show the hidden diagnostics once the cursor moves away from where the user is typing.
  public recordSelection(editor: vscode.TextEditor): void {
    const cursor = editor.selection.active;
    if (
      this.typingUri === editor.document.uri.toString() &&
      ![...this.edits, ...this.hidden].some((range) => range.contains(cursor))
    ) {
      this.stopTyping();
    }
  }

  // Show the hidden diagnostics when the user switches to another editor.
  public recordActiveEditor(editor: vscode.TextEditor | undefined): void {
    if (editor === undefined || this.typingUri !== editor.document.uri.toString()) {
      this.stopTyping();
    }
  }

  // Show every diagnostic of the document being typed in again.
  private stopTyping(): void {
    if (this.typingUri === undefined) {
      return;
    }
    const uri = this.typingUri;
    this.typingUri = undefined;
    this.edits = [];
    this.hidden = [];
    const diagnostics = this.diagnostics.get(uri);
    if (this.show !== undefined && diagnostics !== undefined) {
      this.show(vscode.Uri.parse(uri), diagnostics);
    }
  }

  // Leave out the diagnostics containing the cursor in the document being typed in, remembering
  // where they are.
  private visible(uri: vscode.Uri, diagnostics: vscode.Diagnostic[]): vscode.Diagnostic[] {
    const editor = vscode.window.activeTextEditor;
    if (
      this.typingUri !== uri.toString() ||
      editor === undefined ||
      editor.document.uri.toString() !== this.typingUri
    ) {
      return diagnostics;
    }
    const cursor = editor.selection.active;
    this.hidden = diagnostics
      .filter((diagnostic) => diagnostic.range.contains(cursor))
      .map((diagnostic) => diagnostic.range);
    return diagnostics.filter((diagnostic) => !diagnostic.range.contains(cursor));
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

// This request plans attaching files which are pasted or dropped into a wiki. Keep this in sync
// with [ref:attach_files_request_server].
const ATTACH_FILES_REQUEST = 'mull/attachFiles'; // [tag:attach_files_request_client]

// This is a file to attach to a wiki: its name, where it is if that's known, and its data if it
// came from outside the editor. One of the last two is always given.
interface AttachableFile {
  name: string;
  uri: vscode.Uri | undefined;
  data: vscode.DataTransferFile | undefined;
}

// This is the language server's plan for attaching files: copy each file to its URI, if it has
// one, in the order they were given, and insert the text, which links to them.
interface AttachFilesPlan {
  copyUris: (string | null)[];
  text: string;
}

// This kind identifies pasting or dropping files into a wiki as links to them.
const ATTACH_FILES_KIND = vscode.DocumentDropOrPasteEditKind.Empty.append('mull', 'file');

// This describes pasting or dropping files into a wiki to the user.
const ATTACH_FILES_TITLE = 'Link to the files, copying them into the wiki if needed';

// Collect the files being pasted or dropped. They come as data, like a screenshot, or as locations,
// like a file copied in a file manager or dragged from the explorer, or both. Folders, and
// locations which aren't on disk, like web pages, are left out.
async function attachableFiles(dataTransfer: vscode.DataTransfer): Promise<AttachableFile[]> {
  // Collect the files given as data, each of which may be offered under several types.
  const files: AttachableFile[] = [];
  const seenData = new Set<vscode.DataTransferFile>();
  for (const [, item] of dataTransfer) {
    const data = item.asFile();
    if (data !== undefined && !seenData.has(data)) {
      seenData.add(data);
      files.push({ name: data.name, uri: data.uri, data });
    }
  }

  // Collect the files given only as locations, skipping the list's comments.
  const uriListItem = dataTransfer.get('text/uri-list');
  const uriList = uriListItem === undefined ? '' : await uriListItem.asString();
  const uris = uriList
    .split(/\r?\n/u)
    .map((line) => line.trim())
    .filter((line) => line !== '' && !line.startsWith('#'))
    .map((line) => vscode.Uri.parse(line))
    .filter(
      (uri) =>
        uri.scheme === 'file' &&
        !files.some((file) => file.uri !== undefined && file.uri.toString() === uri.toString()),
    );
  const types = await Promise.all(
    uris.map(async (uri) => {
      try {
        return (await vscode.workspace.fs.stat(uri)).type;
      } catch {
        return vscode.FileType.Unknown;
      }
    }),
  );
  for (const [index, uri] of uris.entries()) {
    if (types[index] === vscode.FileType.File) {
      files.push({ name: uri.path.split('/').at(-1) ?? '', uri, data: undefined });
    }
  }
  return files;
}

// Attach files to a saved wiki: link to those already in its file root where they are, and copy
// the others into it and link to the copies, as one edit. The language server chooses the copies'
// names and writes the links.
async function attachFiles(
  document: vscode.TextDocument,
  files: readonly AttachableFile[],
  token: vscode.CancellationToken,
): Promise<{ text: string; additionalEdit: vscode.WorkspaceEdit } | undefined> {
  // Ask the language server where the files go and how to link to them.
  const activeClient = client;
  if (files.length === 0 || activeClient === undefined || !activeClient.isRunning()) {
    return undefined;
  }
  const plan = await activeClient.sendRequest<AttachFilesPlan | null>(
    ATTACH_FILES_REQUEST,
    {
      textDocument: { uri: activeClient.code2ProtocolConverter.asUri(document.uri) },
      files: files.map((file) => ({
        name: file.name,
        uri: file.uri === undefined ? null : activeClient.code2ProtocolConverter.asUri(file.uri),
      })),
    },
    token,
  );
  if (plan === null || token.isCancellationRequested) {
    return undefined;
  }

  // Copy the files which need it along with the links, reading any given only as locations. Give
  // up if a file can't be read, rather than link to a copy which wouldn't exist.
  const copies = await Promise.all(
    plan.copyUris.map(async (copyUri, index) => {
      const file = files[index];
      if (copyUri === null || file === undefined) {
        return undefined;
      }
      let contents: Uint8Array | vscode.DataTransferFile | undefined = file.data;
      if (contents === undefined && file.uri !== undefined) {
        try {
          contents = await vscode.workspace.fs.readFile(file.uri);
        } catch {
          return null;
        }
      }
      return { uri: activeClient.protocol2CodeConverter.asUri(copyUri), contents };
    }),
  );
  const additionalEdit = new vscode.WorkspaceEdit();
  for (const copy of copies) {
    if (copy === null || (copy !== undefined && copy.contents === undefined)) {
      return undefined;
    }
    if (copy !== undefined) {
      additionalEdit.createFile(copy.uri, { contents: copy.contents });
    }
  }
  return { text: plan.text, additionalEdit };
}

// Paste files into a saved wiki as links to them, copying them into its file root if needed.
async function pasteFiles(
  document: vscode.TextDocument,
  _ranges: readonly vscode.Range[],
  dataTransfer: vscode.DataTransfer,
  _context: vscode.DocumentPasteEditContext,
  token: vscode.CancellationToken,
): Promise<vscode.DocumentPasteEdit[] | undefined> {
  // Plan the edit.
  const files = await attachableFiles(dataTransfer);
  const attachment = await attachFiles(document, files, token);
  if (attachment === undefined) {
    return undefined;
  }
  const edit = new vscode.DocumentPasteEdit(attachment.text, ATTACH_FILES_TITLE, ATTACH_FILES_KIND);
  edit.additionalEdit = attachment.additionalEdit;

  // Prefer pasting text when every file came only as data, as when apps like spreadsheets copy an
  // image of their content along with its text, leaving this edit in the paste widget.
  if (files.every((file) => file.uri === undefined)) {
    edit.yieldTo = [vscode.DocumentDropOrPasteEditKind.Text];
  }
  return [edit];
}

// Drop files into a saved wiki as links to them, copying them into its file root if needed.
async function dropFiles(
  document: vscode.TextDocument,
  _position: vscode.Position,
  dataTransfer: vscode.DataTransfer,
  token: vscode.CancellationToken,
): Promise<vscode.DocumentDropEdit | undefined> {
  const attachment = await attachFiles(document, await attachableFiles(dataTransfer), token);
  if (attachment === undefined) {
    return undefined;
  }
  const edit = new vscode.DocumentDropEdit(attachment.text, ATTACH_FILES_TITLE, ATTACH_FILES_KIND);
  edit.additionalEdit = attachment.additionalEdit;
  return edit;
}

// This formatting option asks the language server to run a wiki's scripts as it's formatted. Keep
// this in sync with [group:run_scripts_option].
const RUN_SCRIPTS_OPTION = 'mull.runScripts';

// Format a wiki, running its scripts as well if scripts may run, which they can't in Restricted
// Mode. The language client sends only the standard formatting options, so the request is sent
// directly to add the option which runs scripts.
async function formatRunningScripts(
  document: vscode.TextDocument,
  options: vscode.FormattingOptions,
  token: vscode.CancellationToken,
  next: ProvideDocumentFormattingEditsSignature,
): Promise<vscode.TextEdit[] | null | undefined> {
  if (client === undefined || !vscode.workspace.isTrusted) {
    return next(document, options, token);
  }
  const edits = await client.sendRequest(
    DocumentFormattingRequest.type,
    {
      textDocument: client.code2ProtocolConverter.asTextDocumentIdentifier(document),
      options: {
        tabSize: options.tabSize,
        insertSpaces: options.insertSpaces,
        [RUN_SCRIPTS_OPTION]: true,
      },
    },
    token,
  );
  return client.protocol2CodeConverter.asTextEdits(edits, token);
}

// This is where an editor's cursor is in a page, as its title and the number of characters before
// the cursor in the page, counting only those which aren't whitespace, along with the whitespace
// right before the cursor. Formatting rarely changes anything but whitespace within a page, so these
// find the same place in the page after formatting, wherever the page moves.
interface PagePosition {
  readonly editor: vscode.TextEditor;
  readonly title: string;
  readonly charactersBefore: number;
  readonly whitespaceBefore: number;
}

// Find where each main editor showing a wiki has its cursor in a page, if it's in one.
async function pagePositions(document: vscode.TextDocument): Promise<PagePosition[]> {
  const pages = await wikiPages(document);
  const text = document.getText();
  const positions: PagePosition[] = [];
  for (const editor of vscode.window.visibleTextEditors) {
    const cursor = editor.selection.active;
    const page =
      isMainWikiEditor(editor) && editor.document === document ? pageAt(pages, cursor) : undefined;
    if (page !== undefined) {
      const before = text.slice(document.offsetAt(page.range.start), document.offsetAt(cursor));
      positions.push({
        editor,
        title: page.name,
        charactersBefore: before.replaceAll(/\s/gu, '').length,
        whitespaceBefore: before.length - before.trimEnd().length,
      });
    }
  }
  return positions;
}

// Move each editor's cursor to where it was in its page, wherever the page is now. A page which no
// longer exists leaves its cursor where it is.
async function restorePagePositions(
  document: vscode.TextDocument,
  positions: readonly PagePosition[],
): Promise<void> {
  const pages = await wikiPages(document);
  const text = document.getText();
  for (const position of positions) {
    const page = pages.find((candidate) => candidate.name === position.title);
    if (page !== undefined) {
      // Pass the same number of characters which aren't whitespace, then as much of the same
      // whitespace as remains, without leaving the page.
      const pageEnd = document.offsetAt(page.range.end);
      let offset = document.offsetAt(page.range.start);
      for (
        let remaining = position.charactersBefore;
        remaining > 0 && offset < pageEnd;
        offset += 1
      ) {
        if (/\S/u.test(text.charAt(offset))) {
          remaining -= 1;
        }
      }
      for (
        let remaining = position.whitespaceBefore;
        remaining > 0 && offset < pageEnd && /\s/u.test(text.charAt(offset));
        remaining -= 1
      ) {
        offset += 1;
      }
      const cursor = document.positionAt(offset);
      position.editor.selection = new vscode.Selection(cursor, cursor);
      position.editor.revealRange(
        new vscode.Range(cursor, cursor),
        vscode.TextEditorRevealType.InCenterIfOutsideViewport,
      );
    }
  }
}

// Format a wiki, running its scripts if they may run, and keep each editor's cursor where it was in
// its page, even if formatting moves the page. The editor keeps a cursor in place only if the text
// around it doesn't change, so the cursors are moved back once the formatting edits are applied. A
// later change to the wiki, as when the edits are discarded because it changed while being
// formatted, cancels this.
async function formatKeepingPagePositions(
  document: vscode.TextDocument,
  options: vscode.FormattingOptions,
  token: vscode.CancellationToken,
  next: ProvideDocumentFormattingEditsSignature,
): Promise<vscode.TextEdit[] | null | undefined> {
  // Note the cursors, then format.
  const positions = await pagePositions(document);
  const edits = await formatRunningScripts(document, options, token, next);
  if (edits === null || edits === undefined || edits.length === 0 || positions.length === 0) {
    return edits;
  }

  // Find the text the edits produce, applying them from the end so earlier offsets stay valid.
  let formattedText = document.getText();
  for (const edit of edits.toSorted((a, b) => b.range.start.compareTo(a.range.start))) {
    formattedText =
      formattedText.slice(0, document.offsetAt(edit.range.start)) +
      edit.newText +
      formattedText.slice(document.offsetAt(edit.range.end));
  }

  // Restore the cursors after the next change, if it applies the edits.
  const subscription = vscode.workspace.onDidChangeTextDocument(async (event) => {
    if (event.document !== document || event.contentChanges.length === 0) {
      return;
    }
    subscription.dispose();
    if (document.getText() === formattedText) {
      await restorePagePositions(document, positions);
    }
  });
  return edits;
}

// Start a Mull language server for local and untitled Mull documents.
export async function activate(context: vscode.ExtensionContext): Promise<void> {
  // Expose the navigation commands that the language server's responses refer to.
  context.subscriptions.push(vscode.commands.registerCommand(REVEAL_RANGE_COMMAND, revealRange));
  context.subscriptions.push(
    vscode.commands.registerCommand(REVEAL_IN_EXPLORER_COMMAND, revealInExplorer),
  );

  // Expose the command that checks the wiki being edited again.
  context.subscriptions.push(vscode.commands.registerCommand(CHECK_WIKI_COMMAND, checkActiveWiki));

  // Paste or drop files into saved wikis as links to them, copying them in if needed.
  context.subscriptions.push(
    vscode.languages.registerDocumentPasteEditProvider(
      { scheme: 'file', language: 'mull' },
      { provideDocumentPasteEdits: pasteFiles },
      { providedPasteEditKinds: [ATTACH_FILES_KIND], pasteMimeTypes: ['files', 'text/uri-list'] },
    ),
    vscode.languages.registerDocumentDropEditProvider(
      { scheme: 'file', language: 'mull' },
      { provideDocumentDropEdits: dropFiles },
      { providedDropEditKinds: [ATTACH_FILES_KIND], dropMimeTypes: ['files', 'text/uri-list'] },
    ),
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

  // Dim the fences around blocks, following edits and the visible editors.
  const fenceDimming = new FenceDimming();
  context.subscriptions.push(
    fenceDimming,
    vscode.workspace.onDidChangeTextDocument((event) => {
      if (event.document.languageId === 'mull') {
        fenceDimming.dimVisible();
      }
    }),
    vscode.window.onDidChangeVisibleTextEditors(() => {
      fenceDimming.dimVisible();
    }),
  );
  fenceDimming.dimVisible();

  // Hide the diagnostics where the user is typing, following edits and the cursor.
  const typingDiagnostics = new TypingDiagnostics();
  context.subscriptions.push(
    vscode.workspace.onDidChangeTextDocument((event) => {
      typingDiagnostics.recordEdit(event);
    }),
    vscode.window.onDidChangeTextEditorSelection((event) => {
      typingDiagnostics.recordSelection(event.textEditor);
    }),
    vscode.window.onDidChangeActiveTextEditor((editor) => {
      typingDiagnostics.recordActiveEditor(editor);
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
    // Let hovers use HTML, which sizes image previews.
    markdown: { supportHtml: true },
    // Run scripts when formatting where they may run, keep cursors in place within their pages
    // when formatting moves the pages, and hide the diagnostics where the user is typing.
    middleware: {
      provideDocumentFormattingEdits: formatKeepingPagePositions,
      handleDiagnostics: typingDiagnostics.handleDiagnostics,
    },
  };
  client = new LanguageClient('mull', 'Mull', serverOptions, clientOptions);
  await client.start();

  // Find the pages of the wikis already shown, which needs the language server, and
  // start the active wiki's trail.
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
