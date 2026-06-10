// Inga VS Code extension: wires the `inga lsp` language server.
const vscode = require("vscode");
const { LanguageClient, TransportKind } = require("vscode-languageclient/node");

let client;

function activate(context) {
  const serverPath = vscode.workspace.getConfiguration("inga").get("serverPath", "inga");

  const serverOptions = {
    command: serverPath,
    args: ["lsp"],
    transport: TransportKind.stdio,
  };

  const clientOptions = {
    documentSelector: [{ scheme: "file", language: "inga" }],
  };

  client = new LanguageClient("inga", "Inga Language Server", serverOptions, clientOptions);
  client.start();
  context.subscriptions.push({ dispose: () => client && client.stop() });
}

function deactivate() {
  return client ? client.stop() : undefined;
}

module.exports = { activate, deactivate };
