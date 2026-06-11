//! Module loading: `use name` imports the sibling file `name.inga`; `use
//! Gfx` enables a std module (compiler-implemented, nothing to load). All
//! modules are merged into one program in a single global span space — each
//! module's tokens are lexed at a disjoint base offset, so diagnostics and
//! hover info can be mapped back to (file, local offset) via [`ModuleSrc`].
//!
//! Exports: `pub` declarations are visible to importing modules; everything
//! else is module-private. Top-level names are program-unique (a duplicate
//! across modules is a duplicate-declaration error).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::ast::{Decl, Program};
use crate::diag::Diagnostic;
use crate::span::Span;
use crate::{lexer, parser};

/// Std modules implemented by the compiler/runtime; `use` enables them.
pub const STD_MODULES: [&str; 1] = ["Gfx"];

#[derive(Debug, Clone)]
pub struct ModuleSrc {
    /// Module name (file stem; the entry file is also its stem).
    pub name: String,
    pub path: PathBuf,
    pub src: String,
    /// Global offset of this module's first byte.
    pub base: u32,
    /// Global offset one past this module's last byte.
    pub end: u32,
    /// Module names this module imports (file and std modules).
    pub imports: Vec<String>,
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
    let mut loaded: HashMap<String, usize> = HashMap::new();
    let mut base = 0u32;

    // Work queue of (module name, path, source). Imports found while
    // parsing are appended; diamonds/cycles load once.
    let entry_name = module_name(entry);
    let mut queue: Vec<(String, PathBuf, String)> =
        vec![(entry_name.clone(), entry.to_path_buf(), entry_src)];
    loaded.insert(entry_name, 0);

    while !queue.is_empty() {
        let (name, path, src) = queue.remove(0);
        let tokens = lexer::lex_from(&src, base, &mut diagnostics);
        let module_program = parser::parse(tokens, &mut diagnostics);
        let end = base + src.len() as u32;

        let mut imports = Vec::new();
        for decl in &module_program.decls {
            if let Decl::Use(u) = decl {
                imports.push(u.name.clone());
                if STD_MODULES.contains(&u.name.as_str()) {
                    continue;
                }
                if loaded.contains_key(&u.name) {
                    continue;
                }
                let import_path = path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(format!("{}.inga", u.name));
                match read(&import_path) {
                    Some(text) => {
                        loaded.insert(u.name.clone(), loaded.len());
                        queue.push((u.name.clone(), import_path, text));
                    }
                    None => {
                        diagnostics.push(Diagnostic::error(
                            u.name_span,
                            format!(
                                "cannot find module `{}` (looked for {})",
                                u.name,
                                import_path.display()
                            ),
                        ));
                    }
                }
            }
        }

        decls.extend(module_program.decls);
        modules.push(ModuleSrc { name, path, src, base, end, imports });
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
    let target = module_name(path);
    let dir = path.parent()?;
    let mut candidates: Vec<(bool, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(dir).ok()? {
        let sibling = entry.ok()?.path();
        if sibling.extension().and_then(|e| e.to_str()) != Some("inga") || sibling == *path {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&sibling) else { continue };
        // BFS over `use` names from the sibling, reading files in `dir`.
        let mut seen = vec![module_name(&sibling)];
        let mut queue: Vec<String> = use_names(&text);
        let mut imports_target = false;
        while let Some(name) = queue.pop() {
            if seen.contains(&name) {
                continue;
            }
            seen.push(name.clone());
            if name == target {
                imports_target = true;
                break;
            }
            if let Ok(t) = std::fs::read_to_string(dir.join(format!("{name}.inga"))) {
                queue.extend(use_names(&t));
            }
        }
        if imports_target {
            candidates.push((defines_main(&text), sibling));
        }
    }
    candidates.sort_by_key(|(has_main, _)| !*has_main);
    candidates.into_iter().next().map(|(_, p)| p)
}

fn use_names(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.trim_start().strip_prefix("use "))
        .map(|n| n.trim().to_string())
        .filter(|n| !STD_MODULES.contains(&n.as_str()))
        .collect()
}
