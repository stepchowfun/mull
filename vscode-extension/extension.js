// Load the editor API and its Language Server Protocol client.
const vscode = require("vscode");
const { LanguageClient } = require("vscode-languageclient/node");

// Retain the active client so it can be stopped when the extension is deactivated.
let client;

// Start a Mull language server for local and untitled Mull documents.
async function activate() {
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
