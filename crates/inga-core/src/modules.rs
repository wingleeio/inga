//! Module loading. `use cards` imports the sibling file `cards.inga`;
//! paths are folder-aware (`use lib/colors` is `lib/colors.inga` relative
//! to the importing file) and the standard library lives under `std/`
//! (`use std/graphics`, `use std/schedule` — compiler-implemented, nothing
//! to load). A plain `use` binds the path's last segment as a qualified
//! alias (`graphics.rect(...)`, `cards.rankName(c)`); `use m { a, b }`
//! imports only the listed `pub` names, unqualified. Importing an enum
//! name also grants its variants.
//!
//! All modules merge into one program in a single global span space — each
//! module's tokens are lexed at a disjoint base offset, so diagnostics and
//! hover info map back to (file, local offset) via [`ModuleSrc`]. Top-level
//! names are program-unique (whole-program compilation, v0.x).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::ast::{Decl, Program};
use crate::diag::Diagnostic;
use crate::span::Span;
use crate::{lexer, parser};

/// Std modules, by full path. Imported like file modules but implemented
/// by the compiler/runtime.
pub const STD_MODULES: [&str; 7] =
    ["std/graphics", "std/schedule", "std/fiber", "std/http", "std/json", "std/fs", "std/process"];

/// One `use` in a module, resolved.
#[derive(Debug, Clone)]
pub struct ImportInfo {
    /// Qualified alias — the path's last segment (`graphics`, `cards`).
    pub alias: String,
    /// Target module key: `std/...` for std modules, else the imported
    /// file's canonical path.
    pub target: String,
    /// `use m { a, b }`: only these names, unqualified (no alias binding).
    pub names: Option<Vec<String>>,
    pub span: Span,
}

impl ImportInfo {
    pub fn is_std(&self) -> bool {
        self.target.starts_with("std/")
    }
}

#[derive(Debug, Clone)]
pub struct ModuleSrc {
    /// Module name (file stem), used in diagnostics.
    pub name: String,
    /// Identity: the canonical path (or the raw path if it can't resolve).
    pub key: String,
    pub path: PathBuf,
    pub src: String,
    /// Global offset of this module's first byte.
    pub base: u32,
    /// Global offset one past this module's last byte.
    pub end: u32,
    pub imports: Vec<ImportInfo>,
}

impl ModuleSrc {
    pub fn contains(&self, span: Span) -> bool {
        span.start >= self.base && span.start <= self.end
    }
}

pub struct Loaded {
    pub program: Program,
    pub modules: Vec<ModuleSrc>,
    pub diagnostics: Vec<Diagnostic>,
}

fn canonical_key(path: &Path) -> String {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf()).display().to_string()
}

/// Load the entry file and, transitively, every file module it imports.
/// The entry module always comes first (base 0).
pub fn load_program(entry: &Path) -> std::io::Result<Loaded> {
    let src = std::fs::read_to_string(entry)?;
    Ok(load_program_with(entry, src, &mut |path| std::fs::read_to_string(path).ok()))
}

/// Like [`load_program`], with a source override (the LSP supplies open
/// documents from memory and falls back to disk).
pub fn load_program_with(
    entry: &Path,
    entry_src: String,
    read: &mut dyn FnMut(&Path) -> Option<String>,
) -> Loaded {
    let mut diagnostics = Vec::new();
    let mut modules: Vec<ModuleSrc> = Vec::new();
    let mut decls = Vec::new();
    let mut loaded: HashMap<String, ()> = HashMap::new();
    let mut base = 0u32;

    let entry_key = canonical_key(entry);
    let mut queue: Vec<(String, PathBuf, String)> =
        vec![(entry_key.clone(), entry.to_path_buf(), entry_src)];
    loaded.insert(entry_key, ());

    while !queue.is_empty() {
        let (key, path, src) = queue.remove(0);
        let tokens = lexer::lex_from(&src, base, &mut diagnostics);
        let module_program = parser::parse(tokens, &mut diagnostics);
        let end = base + src.len() as u32;

        let mut imports = Vec::new();
        for decl in &module_program.decls {
            if let Decl::Use(u) = decl {
                let alias = u.path.last().cloned().unwrap_or_default();
                let joined = u.path.join("/");
                let names =
                    u.names.as_ref().map(|ns| ns.iter().map(|(n, _)| n.clone()).collect());
                if u.path.first().map(String::as_str) == Some("std") {
                    if !STD_MODULES.contains(&joined.as_str()) {
                        diagnostics.push(Diagnostic::error(
                            u.path_span,
                            format!(
                                "unknown std module `{joined}` (available: {})",
                                STD_MODULES.join(", ")
                            ),
                        ));
                        continue;
                    }
                    if u.names.is_some() {
                        diagnostics.push(Diagnostic::error(
                            u.path_span,
                            format!("std modules are imported whole: `use {joined}` (then `{alias}.…`)"),
                        ));
                    }
                    imports.push(ImportInfo {
                        alias,
                        target: joined,
                        names: None,
                        span: u.path_span,
                    });
                    continue;
                }
                let import_path = path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(format!("{joined}.inga"));
                match read(&import_path) {
                    Some(text) => {
                        let target_key = canonical_key(&import_path);
                        if loaded.insert(target_key.clone(), ()).is_none() {
                            queue.push((target_key.clone(), import_path, text));
                        }
                        imports.push(ImportInfo {
                            alias,
                            target: target_key,
                            names,
                            span: u.path_span,
                        });
                    }
                    None => {
                        diagnostics.push(Diagnostic::error(
                            u.path_span,
                            format!(
                                "cannot find module `{joined}` (looked for {})",
                                import_path.display()
                            ),
                        ));
                    }
                }
            }
        }

        decls.extend(module_program.decls);
        modules.push(ModuleSrc {
            name: module_name(&path),
            key,
            path,
            src,
            base,
            end,
            imports,
        });
        base = end + 1; // keep module ranges disjoint
    }

    Loaded { program: Program { decls }, modules, diagnostics }
}

pub fn module_name(path: &Path) -> String {
    path.file_stem().and_then(|s| s.to_str()).unwrap_or("main").to_string()
}

/// Library modules are checked in the context of their program: if `path`
/// does not define `main` and a sibling `.inga` file (transitively) imports
/// it, return that sibling as the entry to load instead. Prefers siblings
/// that define `main`.
pub fn resolve_entry_for(path: &Path, src: &str) -> Option<PathBuf> {
    let defines_main = |text: &str| {
        text.lines().any(|l| {
            let l = l.trim_start();
            l.starts_with("main ::") || l.starts_with("pub main ::")
        })
    };
    if defines_main(src) {
        return None;
    }
    let target = canonical_key(path);
    let dir = path.parent()?;
    let mut candidates: Vec<(bool, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(dir).ok()? {
        let sibling = entry.ok()?.path();
        if sibling.extension().and_then(|e| e.to_str()) != Some("inga") || sibling == *path {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&sibling) else { continue };
        // BFS over `use` paths from the sibling.
        let mut seen: Vec<String> = vec![canonical_key(&sibling)];
        let mut queue: Vec<PathBuf> = use_paths(&text, dir);
        let mut imports_target = false;
        while let Some(p) = queue.pop() {
            let key = canonical_key(&p);
            if seen.contains(&key) {
                continue;
            }
            seen.push(key.clone());
            if key == target {
                imports_target = true;
                break;
            }
            if let (Some(parent), Ok(t)) = (p.parent(), std::fs::read_to_string(&p)) {
                queue.extend(use_paths(&t, parent));
            }
        }
        if imports_target {
            candidates.push((defines_main(&text), sibling));
        }
    }
    candidates.sort_by_key(|(has_main, _)| !*has_main);
    candidates.into_iter().next().map(|(_, p)| p)
}

/// File paths a module's `use` lines refer to (std imports excluded).
fn use_paths(text: &str, dir: &Path) -> Vec<PathBuf> {
    text.lines()
        .filter_map(|l| l.trim_start().strip_prefix("use "))
        .map(|rest| {
            rest.split(|c: char| c == '{' || c.is_whitespace())
                .next()
                .unwrap_or("")
                .trim()
                .to_string()
        })
        .filter(|p| !p.is_empty() && !p.starts_with("std/") && *p != "std")
        .map(|p| dir.join(format!("{p}.inga")))
        .collect()
}
