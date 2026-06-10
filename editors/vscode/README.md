# Inga for VS Code

Syntax highlighting (TextMate grammar + LSP semantic tokens), diagnostics,
hover with inferred signatures (including `!` error rows and `uses` capability
rows), go-to-definition, completion, and formatting for the
[Inga](../../README.md) language.

## Setup

1. Build and install the `inga` CLI so the extension can spawn the language
   server (`inga lsp`):

   ```sh
   cargo install --path crates/inga-cli
   ```

   Or set `inga.serverPath` in VS Code settings to a built binary
   (`target/release/inga`).

2. Install the extension's dependency and load it:

   ```sh
   cd editors/vscode
   npm install
   ```

   Then either press F5 in VS Code with this folder open ("Run Extension"),
   or package it with `npx vsce package` and install the generated `.vsix`
   via "Extensions: Install from VSIX…".

Open any `.inga` file — try `examples/user_service.inga`.
