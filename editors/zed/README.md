# Inga for Zed

Tree-sitter syntax highlighting (functions, methods, types, constructors,
operators, interpolation) plus the Inga language server: diagnostics, hover
with inferred `!`/`uses` rows, go-to-definition, completion, and formatting.

Zed highlights via tree-sitter, not TextMate or LSP semantic tokens — the
grammar lives in [`tree-sitter-inga`](../../tree-sitter-inga) at the root of
this repository, and this extension pins it by commit in `extension.toml`.

## Install (dev extension)

1. Install the `inga` CLI so the language server is on your PATH:

   ```sh
   cargo install --path crates/inga-cli
   ```

2. You need the Rust wasm target once (Zed compiles dev extensions locally):

   ```sh
   rustup target add wasm32-wasip1
   ```

3. In Zed: `zed: extensions` → **Install Dev Extension** → select this
   directory (`editors/zed`). Zed builds the extension, fetches the pinned
   grammar from GitHub, and compiles it.

Open any `.inga` file. If the language server doesn't attach, check that
`which inga` works in the shell Zed inherits its PATH from.
