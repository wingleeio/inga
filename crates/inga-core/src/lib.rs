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

pub fn check_source(src: &str) -> Checked {
    let mut diagnostics = Vec::new();
    let tokens = lexer::lex(src, &mut diagnostics);
    let program = parser::parse(tokens, &mut diagnostics);
    let info = check::check(&program, &mut diagnostics);
    Checked { program, info, diagnostics }
}
