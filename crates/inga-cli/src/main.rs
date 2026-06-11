//! The `inga` command-line tool.

use std::path::Path;
use std::process::ExitCode;

use inga_core::diag::{Diagnostic, Severity};
use inga_core::span::LineIndex;
use inga_core::token::{StrPart, Token, TokenKind};
use inga_core::modules::{load_program, ModuleSrc};
use inga_core::{check_loaded, fmt as inga_fmt, interp, lexer};

const USAGE: &str = "\
inga — the Inga language

Usage:
  inga run <file.inga>          type-check and run in the interpreter
  inga build <file.inga> [-o out] [--emit-ir]
                                compile to a native binary via LLVM (clang)
  inga check <file.inga>...     type-check and report diagnostics
  inga test [file.inga...]      run test* functions (default: ./*.inga)
  inga fmt [--check] <file>...  format in place (--check: diff exit code only)
  inga highlight <file.inga>    print the file with ANSI syntax colors
  inga lsp                      run the language server over stdio
  inga help                     show this message
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("run") => cmd_run(&args[1..]),
        Some("build") => cmd_build(&args[1..]),
        Some("check") => cmd_check(&args[1..]),
        Some("test") => cmd_test(&args[1..]),
        Some("fmt") => cmd_fmt(&args[1..]),
        Some("highlight") => cmd_highlight(&args[1..]),
        Some("lsp") => {
            inga_lsp::run_server();
            ExitCode::SUCCESS
        }
        Some("help") | Some("--help") | Some("-h") | None => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("unknown command `{other}`\n");
            eprint!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

fn read_file(path: &str) -> Result<String, ExitCode> {
    std::fs::read_to_string(path).map_err(|e| {
        eprintln!("error: cannot read `{path}`: {e}");
        ExitCode::FAILURE
    })
}

fn cmd_run(args: &[String]) -> ExitCode {
    let Some(path) = args.first() else {
        eprintln!("usage: inga run <file.inga>");
        return ExitCode::FAILURE;
    };
    let loaded = match load_program(Path::new(path)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: cannot read `{path}`: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (checked, mods) = check_loaded(loaded);
    if print_diagnostics_modules(&mods, &checked.diagnostics) {
        return ExitCode::FAILURE;
    }
    // The tree-walker's depth is bounded by the host stack; run it on a
    // thread with a large one so deep recursion behaves like the native
    // backend (which eliminates self-tail calls outright).
    let program = &checked.program;
    let result = std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(1 << 29)
            .spawn_scoped(scope, move || interp::run(program, "main"))
            .expect("spawn interpreter thread")
            .join()
            .expect("interpreter thread panicked")
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            match err.span {
                Some(span) => {
                    let diag = Diagnostic::error(span, format!("runtime error: {}", err.message));
                    print_diagnostics_modules(&mods, &[diag]);
                }
                None => eprintln!("runtime error: {}", err.message),
            }
            ExitCode::FAILURE
        }
    }
}

/// Compile to a native binary: emit LLVM IR, link against the runtime
/// staticlib with clang (clang embeds LLVM, so there is no other dependency).
fn cmd_build(args: &[String]) -> ExitCode {
    let mut input: Option<&str> = None;
    let mut output: Option<String> = None;
    let mut emit_ir = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-o" => {
                i += 1;
                output = args.get(i).cloned();
            }
            "--emit-ir" => emit_ir = true,
            other => input = Some(other),
        }
        i += 1;
    }
    let Some(path) = input else {
        eprintln!("usage: inga build <file.inga> [-o out] [--emit-ir]");
        return ExitCode::FAILURE;
    };
    let loaded = match load_program(Path::new(path)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: cannot read `{path}`: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (checked, mods) = check_loaded(loaded);
    if print_diagnostics_modules(&mods, &checked.diagnostics) {
        return ExitCode::FAILURE;
    }
    let ir = match inga_codegen::compile(&checked.program, &checked.info) {
        Ok(ir) => ir,
        Err(diagnostics) => {
            print_diagnostics_modules(&mods, &diagnostics);
            return ExitCode::FAILURE;
        }
    };

    let stem = Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("out");
    let out_path = output.unwrap_or_else(|| stem.to_string());
    let ll_path = format!("{out_path}.ll");
    if let Err(e) = std::fs::write(&ll_path, &ir) {
        eprintln!("error: cannot write `{ll_path}`: {e}");
        return ExitCode::FAILURE;
    }

    // The runtime staticlib is built next to this binary by cargo.
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_default();
    let rt_lib = match std::env::var("INGA_RT_LIB") {
        Ok(p) => std::path::PathBuf::from(p),
        Err(_) => exe_dir.join("libinga_rt.a"),
    };
    if !rt_lib.exists() {
        // Dev convenience: inside the repo, build it on demand (a plain
        // `cargo run -p inga-cli` doesn't build inga-rt, which is not a
        // dependency — the staticlib is only an artifact of its own build).
        eprintln!("runtime library missing; running `cargo build -p inga-rt`...");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let mut build_rt = std::process::Command::new(cargo);
        build_rt.args(["build", "-p", "inga-rt"]);
        if exe_dir.file_name().is_some_and(|n| n == "release") {
            build_rt.arg("--release");
        }
        let _ = build_rt.status();
    }
    if !rt_lib.exists() {
        eprintln!(
            "error: runtime library not found at {} (build it with `cargo build -p inga-rt` from the Inga repo, or set INGA_RT_LIB to a built libinga_rt.a)",
            rt_lib.display()
        );
        return ExitCode::FAILURE;
    }

    let mut clang = std::process::Command::new("clang");
    clang
        .arg("-O2")
        .arg("-Wno-override-module")
        .arg(&ll_path)
        .arg(&rt_lib)
        .arg("-o")
        .arg(&out_path);
    // The runtime's GL window layer (miniquad) needs the system frameworks.
    if cfg!(target_os = "macos") {
        for framework in ["Cocoa", "OpenGL", "QuartzCore", "Metal", "MetalKit"] {
            clang.arg("-framework").arg(framework);
        }
    }
    let status = clang.status();
    match status {
        Ok(s) if s.success() => {
            if !emit_ir {
                if std::env::var("INGA_KEEP_LL").is_err() {
                    let _ = std::fs::remove_file(&ll_path);
                }
            } else {
                println!("{ll_path}: LLVM IR");
            }
            println!("{out_path}: native binary");
            ExitCode::SUCCESS
        }
        Ok(s) => {
            eprintln!("error: clang failed with {s} (IR kept at {ll_path})");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("error: cannot run clang: {e} (install the Xcode command line tools or LLVM)");
            ExitCode::FAILURE
        }
    }
}

/// Run every zero-parameter `test*` function of each file (interpreter).
/// A test passes when it returns; any unhandled failure — usually
/// `AssertFailed` from `assert`/`assertEq` — fails it.
fn cmd_test(args: &[String]) -> ExitCode {
    let files: Vec<String> = if args.is_empty() {
        let mut found: Vec<String> = std::fs::read_dir(".")
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| e.path().to_str().map(str::to_string))
                    .filter(|p| p.ends_with(".inga"))
                    .collect()
            })
            .unwrap_or_default();
        found.sort();
        found
    } else {
        args.to_vec()
    };
    if files.is_empty() {
        eprintln!("no .inga files found (usage: inga test [file.inga...])");
        return ExitCode::FAILURE;
    }

    // Same big interpreter stack as `inga run`.
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(1 << 29)
            .spawn_scoped(scope, move || run_tests(&files))
            .expect("spawn test thread")
            .join()
            .expect("test thread panicked")
    })
}

fn run_tests(files: &[String]) -> ExitCode {
    let (mut passed, mut failed) = (0usize, 0usize);
    for path in files {
        let loaded = match load_program(Path::new(path)) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("error: cannot read `{path}`: {e}");
                failed += 1;
                continue;
            }
        };
        let (checked, mods) = check_loaded(loaded);
        if print_diagnostics_modules(&mods, &checked.diagnostics) {
            failed += 1;
            continue;
        }
        // Only the entry file's own tests run — not its imports'.
        let entry = std::fs::canonicalize(path).unwrap_or_else(|_| Path::new(path).to_path_buf());
        let root = mods
            .iter()
            .find(|m| std::fs::canonicalize(&m.path).ok().as_deref() == Some(&entry));
        let tests: Vec<&str> = checked
            .program
            .decls
            .iter()
            .filter_map(|d| match d {
                inga_core::ast::Decl::Func(func)
                    if func.name.starts_with("test")
                        && func.name.len() > 4
                        && func.sig.params.is_empty()
                        && root.is_none_or(|m| {
                            func.name_span.start >= m.base && func.name_span.start < m.end
                        }) =>
                {
                    Some(func.name.as_str())
                }
                _ => None,
            })
            .collect();
        if tests.is_empty() {
            continue;
        }
        println!("{path}");
        for name in tests {
            match interp::run_captured(&checked.program, name) {
                Ok(_) => {
                    println!("  \u{2713} {name}");
                    passed += 1;
                }
                Err(err) => {
                    let message =
                        err.message.strip_prefix("unhandled error: ").unwrap_or(&err.message);
                    match err.span {
                        Some(span) => {
                            println!("  \u{2717} {name}");
                            print_diagnostics_modules(
                                &mods,
                                &[Diagnostic::error(span, message.to_string())],
                            );
                        }
                        None => println!("  \u{2717} {name} \u{2014} {message}"),
                    }
                    failed += 1;
                }
            }
        }
    }
    println!();
    println!("{passed} passed, {failed} failed");
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn cmd_check(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: inga check <file.inga>...");
        return ExitCode::FAILURE;
    }
    let mut failed = false;
    for path in args {
        let src = match read_file(path) {
            Ok(s) => s,
            Err(_) => {
                failed = true;
                continue;
            }
        };
        // A library module (no `main`) is checked as part of the program
        // that imports it; only this file's diagnostics are reported.
        let entry = inga_core::modules::resolve_entry_for(Path::new(path), &src);
        let entry_path = entry.as_deref().unwrap_or(Path::new(path));
        let loaded = match load_program(entry_path) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("error: cannot read `{path}`: {e}");
                failed = true;
                continue;
            }
        };
        let (checked, mods) = check_loaded(loaded);
        // Checking a whole program reports everything; checking a library
        // module (redirected to its entry) reports only that file.
        let diags: Vec<Diagnostic> = if entry.is_some() {
            let this =
                std::fs::canonicalize(path).unwrap_or_else(|_| Path::new(path).to_path_buf());
            checked
                .diagnostics
                .iter()
                .filter(|d| {
                    mods.iter()
                        .find(|m| m.contains(d.span))
                        .map(|m| {
                            std::fs::canonicalize(&m.path).unwrap_or_else(|_| m.path.clone())
                                == this
                        })
                        .unwrap_or(true)
                })
                .cloned()
                .collect()
        } else {
            checked.diagnostics.clone()
        };
        if print_diagnostics_modules(&mods, &diags) {
            failed = true;
        } else if checked.diagnostics.is_empty() {
            println!("{path}: ok");
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn cmd_fmt(args: &[String]) -> ExitCode {
    let check_only = args.first().map(String::as_str) == Some("--check");
    let files = if check_only { &args[1..] } else { args };
    if files.is_empty() {
        eprintln!("usage: inga fmt [--check] <file.inga>...");
        return ExitCode::FAILURE;
    }
    let mut failed = false;
    for path in files {
        let src = match read_file(path) {
            Ok(s) => s,
            Err(_) => {
                failed = true;
                continue;
            }
        };
        match inga_fmt::format(&src) {
            Ok(formatted) => {
                if formatted == src {
                    continue;
                }
                if check_only {
                    println!("{path}: needs formatting");
                    failed = true;
                } else if let Err(e) = std::fs::write(Path::new(path), &formatted) {
                    eprintln!("error: cannot write `{path}`: {e}");
                    failed = true;
                } else {
                    println!("{path}: formatted");
                }
            }
            Err(diagnostics) => {
                print_diagnostics(path, &src, &diagnostics);
                eprintln!("{path}: not formatted (fix parse errors first)");
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

// ---- diagnostics rendering -------------------------------------------------

/// Prints diagnostics with source context. Returns true if any were errors.
/// Render diagnostics whose spans live in the merged multi-module space.
fn print_diagnostics_modules(modules: &[ModuleSrc], diagnostics: &[Diagnostic]) -> bool {
    let mut has_errors = false;
    for diag in diagnostics {
        let module = modules
            .iter()
            .find(|m| m.contains(diag.span))
            .or_else(|| modules.first());
        let Some(module) = module else { continue };
        let mut local = diag.clone();
        local.span = inga_core::span::Span::new(
            diag.span.start.saturating_sub(module.base),
            diag.span.end.saturating_sub(module.base),
        );
        let path = module.path.display().to_string();
        if print_diagnostics(&path, &module.src, std::slice::from_ref(&local)) {
            has_errors = true;
        }
    }
    has_errors
}

fn print_diagnostics(path: &str, src: &str, diagnostics: &[Diagnostic]) -> bool {
    let lines = LineIndex::new(src);
    let use_color = std::io::IsTerminal::is_terminal(&std::io::stderr());
    let mut has_errors = false;
    for diag in diagnostics {
        let (severity, color) = match diag.severity {
            Severity::Error => {
                has_errors = true;
                ("error", "\x1b[31;1m")
            }
            Severity::Warning => ("warning", "\x1b[33;1m"),
        };
        let (line, col) = lines.line_col(diag.span.start);
        let (c0, c1, bold) = if use_color { (color, "\x1b[0m", "\x1b[1m") } else { ("", "", "") };
        eprintln!(
            "{c0}{severity}{c1}{bold}: {}{c1}\n  --> {path}:{}:{}",
            diag.message,
            line + 1,
            col + 1
        );
        // Source line with a caret underline.
        let line_start = lines.line_start(line) as usize;
        let line_end = src[line_start..].find('\n').map(|i| line_start + i).unwrap_or(src.len());
        let text = &src[line_start..line_end];
        let line_no = format!("{}", line + 1);
        let pad = " ".repeat(line_no.len());
        eprintln!("{pad} |");
        eprintln!("{line_no} | {text}");
        let caret_start = (diag.span.start as usize).saturating_sub(line_start);
        let caret_len =
            ((diag.span.end as usize).min(line_end) as i64 - diag.span.start as i64).max(1) as usize;
        let underline = " ".repeat(caret_start) + &"^".repeat(caret_len);
        eprintln!("{pad} | {c0}{underline}{c1}");
        eprintln!();
    }
    has_errors
}

// ---- terminal highlighter ----------------------------------------------------

fn cmd_highlight(args: &[String]) -> ExitCode {
    let Some(path) = args.first() else {
        eprintln!("usage: inga highlight <file.inga>");
        return ExitCode::FAILURE;
    };
    let src = match read_file(path) {
        Ok(s) => s,
        Err(code) => return code,
    };
    let mut diagnostics = Vec::new();
    let tokens = lexer::lex(&src, &mut diagnostics);
    print!("{}", highlight_ansi(&src, &tokens));
    ExitCode::SUCCESS
}

const RESET: &str = "\x1b[0m";
const KEYWORD: &str = "\x1b[35m"; // magenta
const TYPE: &str = "\x1b[33m"; // yellow — uppercase identifiers
const STRING: &str = "\x1b[32m"; // green
const NUMBER: &str = "\x1b[36m"; // cyan
const COMMENT: &str = "\x1b[90m"; // bright black
const OPERATOR: &str = "\x1b[34m"; // blue — ::, |>, ->, !

/// Re-emit source with ANSI colors. Token spans index the original source, so
/// whitespace and layout are preserved exactly.
fn highlight_ansi(src: &str, tokens: &[Token]) -> String {
    let mut out = String::with_capacity(src.len() * 2);
    let mut cursor = 0usize;
    emit_tokens(src, tokens, &mut cursor, &mut out);
    out.push_str(&src[cursor.min(src.len())..]);
    out
}

fn emit_tokens(src: &str, tokens: &[Token], cursor: &mut usize, out: &mut String) {
    for token in tokens {
        if token.kind == TokenKind::Eof {
            continue;
        }
        let start = token.span.start as usize;
        let end = (token.span.end as usize).min(src.len());
        if start < *cursor || end < start {
            continue;
        }
        out.push_str(&src[*cursor..start]);
        let text = &src[start..end];
        match &token.kind {
            TokenKind::Comment(_) => {
                out.push_str(COMMENT);
                out.push_str(text);
                out.push_str(RESET);
            }
            TokenKind::Str(parts) => {
                emit_string(src, text, start, parts, out);
            }
            TokenKind::Int(_) | TokenKind::Float(_) => {
                out.push_str(NUMBER);
                out.push_str(text);
                out.push_str(RESET);
            }
            TokenKind::Ident(name) => {
                if name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                    out.push_str(TYPE);
                    out.push_str(text);
                    out.push_str(RESET);
                } else {
                    out.push_str(text);
                }
            }
            TokenKind::KwTrue | TokenKind::KwFalse => {
                out.push_str(NUMBER);
                out.push_str(text);
                out.push_str(RESET);
            }
            k if is_keyword(k) => {
                out.push_str(KEYWORD);
                out.push_str(text);
                out.push_str(RESET);
            }
            TokenKind::ColonColon | TokenKind::Arrow | TokenKind::PipeOp | TokenKind::Bang => {
                out.push_str(OPERATOR);
                out.push_str(text);
                out.push_str(RESET);
            }
            _ => out.push_str(text),
        }
        *cursor = end;
    }
}

/// Strings: green text, with `${...}` holes highlighted recursively.
fn emit_string(src: &str, text: &str, start: usize, parts: &[StrPart], out: &mut String) {
    let exprs: Vec<&Vec<Token>> = parts
        .iter()
        .filter_map(|p| match p {
            StrPart::Expr(tokens) => Some(tokens),
            _ => None,
        })
        .collect();
    if exprs.is_empty() {
        out.push_str(STRING);
        out.push_str(text);
        out.push_str(RESET);
        return;
    }
    let end = start + text.len();
    let mut cursor = start;
    for tokens in exprs {
        let Some(first) = tokens.iter().find(|t| t.kind != TokenKind::Eof) else { continue };
        let Some(last) = tokens.iter().rev().find(|t| t.kind != TokenKind::Eof) else { continue };
        let expr_start = first.span.start as usize;
        let expr_end = (last.span.end as usize).min(src.len());
        if expr_start < cursor {
            continue;
        }
        // String text up to the `${`, then the hole contents.
        let pre = &src[cursor..expr_start];
        let dollar = pre.rfind("${").map(|i| cursor + i).unwrap_or(expr_start);
        out.push_str(STRING);
        out.push_str(&src[cursor..dollar]);
        out.push_str(RESET);
        out.push_str(KEYWORD);
        out.push_str(&src[dollar..expr_start]);
        out.push_str(RESET);
        let mut inner_cursor = expr_start;
        emit_tokens(src, tokens, &mut inner_cursor, out);
        out.push_str(&src[inner_cursor..expr_end]);
        // Closing `}`.
        let close = src[expr_end..end.min(src.len())]
            .find('}')
            .map(|i| expr_end + i + 1)
            .unwrap_or(expr_end);
        out.push_str(KEYWORD);
        out.push_str(&src[expr_end..close]);
        out.push_str(RESET);
        cursor = close;
    }
    out.push_str(STRING);
    out.push_str(&src[cursor..end.min(src.len())]);
    out.push_str(RESET);
}

fn is_keyword(kind: &TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::KwStruct
            | TokenKind::KwEnum
            | TokenKind::KwService
            | TokenKind::KwMatch
            | TokenKind::KwCatch
            | TokenKind::KwFail
            | TokenKind::KwProvide
            | TokenKind::KwUses
            | TokenKind::KwLazy
            | TokenKind::KwIf
            | TokenKind::KwElse
    )
}
