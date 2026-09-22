// Load the editor API and its Language Server Protocol client.
const vscode = require("vscode");
const { LanguageClient } = require("vscode-languageclient/node");

// This private command reveals a source range for text links embedded in hover previews.
// [ref:reveal_range_command]
const REVEAL_RANGE_COMMAND = "mull.revealRange";

// Retain the active client so it can be stopped when the extension is deactivated.
let client;

// Reveal the source range supplied by a trusted language-server hover.
async function revealRange(uriString, startLine, startCharacter, endLine, endCharacter) {
  // Open either a file-backed or untitled document and select the complete title.
  const document = await vscode.workspace.openTextDocument(vscode.Uri.parse(uriString));
  const editor = await vscode.window.showTextDocument(document);
  const range = new vscode.Range(startLine, startCharacter, endLine, endCharacter);
  editor.selection = new vscode.Selection(range.start, range.end);
  editor.revealRange(range, vscode.TextEditorRevealType.InCenterIfOutsideViewport);
}

// Start a Mull language server for local and untitled Mull documents.
async function activate(context) {
  // Expose only the navigation command embedded in Mull's hover Markdown.
  context.subscriptions.push(
    vscode.commands.registerCommand(REVEAL_RANGE_COMMAND, revealRange),
  );

  // Resolve the configured executable before constructing the server process.
  const executablePath = vscode.workspace
    .getConfiguration("mull")
    .get("executablePath", "mull");
  const serverOptions = {
    command: executablePath,
    args: ["language-server"],
  };

  // Connect Mull documents to the server and complete the LSP handshake.
  const clientOptions = {
    documentSelector: [
      { scheme: "file", language: "mull" },
      { scheme: "untitled", language: "mull" },
    ],
    markdown: {
      isTrusted: {
        enabledCommands: [REVEAL_RANGE_COMMAND],
      },
    },
  };
  client = new LanguageClient("mull", "Mull", serverOptions, clientOptions);
  await client.start();
}

// Shut down the language client and its server process with the extension.
async function deactivate() {
  await client?.dispose();
  client = undefined;
}

// Expose the lifecycle hooks expected by the VS Code extension host.
module.exports = { activate, deactivate };
