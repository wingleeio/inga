//! Zed extension for Inga: wires the `inga lsp` language server.
//! Syntax highlighting comes from the tree-sitter grammar declared in
//! extension.toml (tree-sitter-inga in this repository).

use zed_extension_api::{self as zed, Result};

struct IngaExtension;

impl zed::Extension for IngaExtension {
    fn new() -> Self {
        IngaExtension
    }

    fn language_server_command(
        &mut self,
        _language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let command = worktree.which("inga").ok_or_else(|| {
            "could not find `inga` on PATH — install it with `cargo install --path crates/inga-cli`"
                .to_string()
        })?;
        Ok(zed::Command { command, args: vec!["lsp".to_string()], env: Default::default() })
    }
}

zed::register_extension!(IngaExtension);
