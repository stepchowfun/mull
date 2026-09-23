// Load the editor API and its Language Server Protocol client.
const { execFile } = require("node:child_process");
const { promisify } = require("node:util");
const vscode = require("vscode");
const { LanguageClient } = require("vscode-languageclient/node");

// Make executable probes compatible with the extension's asynchronous startup.
const execFileAsync = promisify(execFile);

// This private command reveals a source range for text links embedded in hover previews.
// [group:reveal_range_command]
const REVEAL_RANGE_COMMAND = "mull.revealRange";

// Link users to Mull's platform-specific installation instructions.
const INSTALLATION_URL =
  "https://github.com/stepchowfun/mull#installation-instructions";
const INSTALLATION_ACTION = "View installation instructions";
const CONFIGURATION_ACTION = "Configure executable path";

// Retain the active client so it can be stopped when the extension is deactivated.
let client;

// Help the user install Mull or select an existing executable.
async function reportMissingMull(outputChannel, error) {
  // Record both an actionable explanation and the underlying launch failure.
  outputChannel.error(
    `Mull could not be found. Install Mull or configure mull.executablePath. ${INSTALLATION_URL}`,
  );
  outputChannel.debug(String(error));

  // Offer direct access to the two ways to resolve the problem [tag:missing_mull_actions].
  const action = await vscode.window.showErrorMessage(
    "Mull could not be found.",
    INSTALLATION_ACTION,
    CONFIGURATION_ACTION,
  );

  // Perform the selected recovery action.
  if (action === INSTALLATION_ACTION) {
    await vscode.env.openExternal(vscode.Uri.parse(INSTALLATION_URL));
  } else if (action === CONFIGURATION_ACTION) {
    await vscode.commands.executeCommand(
      "workbench.action.openSettings",
      "mull.executablePath",
    );
  } else if (action === undefined) {
    // Leave Mull inactive when the user dismisses the notification.
    return;
  } else {
    // VS Code returns a configured action or undefined [ref:missing_mull_actions].
    throw new Error(`Unexpected missing-Mull action: ${action}`);
  }
}

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

  // Detect a missing executable before the language client emits its own error.
  const outputChannel = vscode.window.createOutputChannel("Mull", { log: true });
  context.subscriptions.push(outputChannel);
  try {
    await execFileAsync(executablePath, ["--version"]);
  } catch (error) {
    if (error.code === "ENOENT") {
      void reportMissingMull(outputChannel, error).catch((reportingError) => {
        outputChannel.error("Unable to show Mull installation help.", reportingError);
      });
      return;
    }

    throw error;
  }

  // Connect Mull documents to the server and complete the LSP handshake.
  const serverOptions = {
    command: executablePath,
    args: ["language-server"],
  };
  const clientOptions = {
    outputChannel,
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
