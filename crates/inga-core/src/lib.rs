//! inga-core: the Inga language implementation.
//!
//! Pipeline: source → [`lexer`] → tokens → [`parser`] → AST ([`ast`]) →
//! [`check`] (type + effect inference) → [`interp`] (evaluation).
//! [`fmt`] pretty-prints the AST back to canonical source.

pub mod ast;
pub mod check;
pub mod diag;
pub mod fmt;
pub mod interp;
pub mod lexer;
pub mod modules;
pub mod parser;
pub mod span;
pub mod token;
pub mod types;

use diag::Diagnostic;

/// Parse and type-check a source file, returning the AST, hover/type info,
/// and any diagnostics. This is the shared front half used by the CLI and LSP.
pub struct Checked {
    pub program: ast::Program,
    pub info: check::CheckInfo,
    pub diagnostics: Vec<Diagnostic>,
}

/// Check a single self-contained source (no file imports resolved; `use`
/// of a std module still works). Tests and simple tools use this.
pub fn check_source(src: &str) -> Checked {
    let mut diagnostics = Vec::new();
    let tokens = lexer::lex(src, &mut diagnostics);
    let program = parser::parse(tokens, &mut diagnostics);
    let single = modules::ModuleSrc {
        name: "main".to_string(),
        key: "main.inga".to_string(),
        path: std::path::PathBuf::from("main.inga"),
        src: src.to_string(),
        base: 0,
        end: src.len() as u32,
        imports: program
            .decls
            .iter()
            .filter_map(|d| match d {
                ast::Decl::Use(u) => Some(modules::ImportInfo {
                    alias: u.path.last().cloned().unwrap_or_default(),
                    target: u.path.join("/"),
                    names: u
                        .names
                        .as_ref()
                        .map(|ns| ns.iter().map(|(n, _)| n.clone()).collect()),
                    span: u.path_span,
                }),
                _ => None,
            })
            .collect(),
    };
    let info = check::check(&program, &[single], &mut diagnostics);
    Checked { program, info, diagnostics }
}

/// Check a multi-module program produced by [`modules::load_program`].
/// Returns the merged program (interp/codegen consume it) plus diagnostics
/// in the global span space.
pub fn check_loaded(loaded: modules::Loaded) -> (Checked, Vec<modules::ModuleSrc>) {
    let mut diagnostics = loaded.diagnostics;
    let info = check::check(&loaded.program, &loaded.modules, &mut diagnostics);
    (Checked { program: loaded.program, info, diagnostics }, loaded.modules)
}
