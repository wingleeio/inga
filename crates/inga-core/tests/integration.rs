//! Checker and formatter tests: source text in, diagnostics/formatting out.\n//! (Execution tests compile and run native binaries — see inga-cli/tests/exec.rs.)

use inga_core::diag::Severity;
use inga_core::{check_source, fmt};

fn check_errors(src: &str) -> Vec<String> {
    check_source(src)
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.message.clone())
        .collect()
}

fn check_warnings(src: &str) -> Vec<String> {
    check_source(src)
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Warning)
        .map(|d| d.message.clone())
        .collect()
}

// ---- basics -----------------------------------------------------------------

#[test]
fn uncaught_error_in_main_is_rejected() {
    let errors = check_errors(r#"
struct Boom = { String why }

main :: () {
    fail Boom("x")
}
"#);
    assert!(
        errors.iter().any(|e| e.contains("`main` does not handle the error `Boom`")),
        "got: {errors:?}"
    );
}

#[test]
fn declared_error_row_must_cover_inferred() {
    let errors = check_errors(r#"
struct A = { Int x }
struct B = { Int x }

f :: (Bool go) -> Int ! A {
    if go {
        fail B(1)
    }
    fail A(2)
}

main :: () {
    f(true) |> catch { A -> 0, B -> 1 }
}
"#);
    assert!(
        errors.iter().any(|e| e.contains("can fail with `B`") && e.contains("does not declare")),
        "got: {errors:?}"
    );
}

#[test]
fn unreachable_catch_arm_warns() {
    let warnings = check_warnings(r#"
struct A = { Int x }
struct B = { Int x }

f :: () {
    fail A(1)
}

main :: () {
    f() |> catch { A -> 0, B -> 1 }
}
"#);
    assert!(
        warnings.iter().any(|w| w.contains("cannot fail with `B`")),
        "got: {warnings:?}"
    );
}

#[test]
fn main_must_handle_primitive_failures() {
    let errors = check_errors(r#"
main :: () {
    fail "boom"
}
"#);
    assert!(
        errors.iter().any(|e| e.contains("`main` does not handle the error `String`")),
        "got: {errors:?}"
    );
}

// ---- enums ---------------------------------------------------------------------

#[test]
fn partial_variant_coverage_keeps_enum_in_row() {
    let errors = check_errors(r#"
enum Signal = Go | Stop { String why }

drive :: () {
    fail Go
}

main :: () {
    drive() |> catch { Go -> 1 }
}
"#);
    assert!(
        errors.iter().any(|e| e.contains("`main` does not handle the error `Signal`")),
        "got: {errors:?}"
    );
}

#[test]
fn duplicate_variant_names_are_rejected() {
    let errors = check_errors(r#"
enum A = Go | Halt
enum B = Halt | Wait

main :: () {
    println(1)
}
"#);
    assert!(
        errors.iter().any(|e| e.contains("variant name `Halt` is already taken")),
        "got: {errors:?}"
    );
}

// ---- capabilities ----------------------------------------------------------------

#[test]
fn missing_capability_in_main_is_rejected() {
    let errors = check_errors(r#"
service Greeter {
    greet :: (String name) -> String
}

main :: () {
    Greeter greeter
    println(greeter.greet("x"))
}
"#);
    assert!(
        errors.iter().any(|e| e.contains("`main` requires the service `Greeter`")),
        "got: {errors:?}"
    );
}

#[test]
fn declared_uses_row_must_cover_inferred() {
    let errors = check_errors(r#"
service A {
    go :: () -> Int
}
service B {
    go :: () -> Int
}

f :: () -> Int uses A {
    A a
    B b
    a.go() + b.go()
}

aImpl :: A {
    go :: () {
        1
    }
}
bImpl :: B {
    go :: () {
        2
    }
}

main :: () {
    provide aImpl, bImpl {
        println(f())
    }
}
"#);
    assert!(
        errors.iter().any(|e| e.contains("uses `B`") && e.contains("does not declare")),
        "got: {errors:?}"
    );
}

#[test]
fn type_mismatch_is_reported() {
    let errors = check_errors(r#"
add :: (Int a, Int b) -> Int {
    a + b
}

main :: () {
    println(add(1, "two"))
}
"#);
    assert!(
        errors.iter().any(|e| e.contains("expected Int, found String")),
        "got: {errors:?}"
    );
}

#[test]
fn unknown_names_are_reported() {
    let errors = check_errors("main :: () {\n    println(nope)\n}\n");
    assert!(errors.iter().any(|e| e.contains("unknown name `nope`")), "got: {errors:?}");
}

#[test]
fn return_annotation_is_checked() {
    let errors = check_errors(r#"
f :: () -> Int {
    "string"
}

main :: () {
    println(f())
}
"#);
    assert!(
        errors.iter().any(|e| e.contains("expected Int, found String")),
        "got: {errors:?}"
    );
}

// ---- formatter --------------------------------------------------------------------------

#[test]
fn formatter_is_idempotent_on_examples() {
    for example in ["user_service.inga", "hello.inga", "retry.inga"] {
        let path = format!("{}/../../examples/{example}", env!("CARGO_MANIFEST_DIR"));
        let src = std::fs::read_to_string(&path).unwrap();
        let once = fmt::format(&src).expect("example should format");
        let twice = fmt::format(&once).expect("formatted output should re-format");
        assert_eq!(once, twice, "formatter not idempotent on {example}");
        assert_eq!(once, src, "{example} is not canonically formatted");
    }
}

#[test]
fn formatter_preserves_comments() {
    let src = "// leading comment\nmain :: () {\n    println(1) // trailing\n}\n";
    let formatted = fmt::format(src).unwrap();
    assert!(formatted.contains("// leading comment"), "got:\n{formatted}");
    assert!(formatted.contains("// trailing"), "got:\n{formatted}");
}

#[test]
fn formatter_leaves_broken_code_alone() {
    assert!(fmt::format("main :: () {").is_err());
}

#[test]
fn formatter_canonicalizes_layout() {
    let messy = "main::(){\nx=1+2\nprintln(x)\n}\n";
    let formatted = fmt::format(messy).unwrap();
    assert_eq!(formatted, "main :: () {\n    x = 1 + 2\n    println(x)\n}\n");
}

// ---- the flagship example -----------------------------------------------------------------

#[test]
fn user_service_signatures_are_inferred() {
    let path = format!("{}/../../examples/user_service.inga", env!("CARGO_MANIFEST_DIR"));
    let src = std::fs::read_to_string(path).unwrap();
    let checked = check_source(&src);
    let hover = |name: &str| {
        checked
            .info
            .hovers
            .iter()
            .find(|(_, text)| text.starts_with(&format!("{name} ::")))
            .map(|(_, text)| text.clone())
            .unwrap_or_else(|| panic!("no hover for {name}"))
    };
    // `cached` is fully unannotated in the source; everything below is inferred.
    assert_eq!(hover("cached"), "cached :: (Int id) -> User? uses Cache, Logger");
    assert_eq!(
        hover("fetchAndCache"),
        "fetchAndCache :: (Int id) -> User ! UserNotFound uses Cache, Database, Logger"
    );
    assert_eq!(hover("main"), "main :: () -> Unit");
}

#[test]
fn builtins_hover_with_their_signature() {
    let path = format!("{}/../../examples/user_service.inga", env!("CARGO_MANIFEST_DIR"));
    let src = std::fs::read_to_string(path).unwrap();
    let checked = check_source(&src);
    let has = |prefix: &str| checked.info.hovers.iter().any(|(_, t)| t.starts_with(prefix));
    // The example calls retry/orFail/decode/schedule.exponential; hovering
    // them shows the builtin's signature + doc, not just "(builtin)".
    assert!(has("retry(lazy action, schedule) -> a"), "retry hover missing");
    assert!(has("orFail(option, error) -> a"), "orFail hover missing");
    assert!(has("decode(raw, StructName) -> a ! DecodeError"), "decode hover missing");
    assert!(has("schedule.exponential(base) -> Schedule"), "schedule hover missing");
    assert!(
        !checked.info.hovers.iter().any(|(_, t)| t.ends_with("(builtin)")),
        "no builtin should fall back to the bare `(builtin)` hover"
    );
}

// ---- provide v2 / arenas / modules ---------------------------------------------

#[test]
fn gfx_requires_use() {
    let errors = check_errors(r#"
main :: () {
    graphics.mouseX()
}
"#);
    assert!(errors.iter().any(|e| e.contains("add `use std/graphics`")), "got: {errors:?}");
}

#[test]
fn use_lines_hover_and_resolve_cross_module() {
    use std::path::Path;
    let lib = r#"
pub yell :: (String s) -> String {
    "${s}!"
}
"#;
    let main_src = r#"
use lib { yell }

main :: () {
    println(yell("hey"))
}
"#;
    let loaded = inga_core::modules::load_program_with(
        Path::new("/virtual/main.inga"),
        main_src.to_string(),
        &mut |p| (p == Path::new("/virtual/lib.inga")).then(|| lib.to_string()),
    );
    let (checked, mods) = inga_core::check_loaded(loaded);
    let entry_end = mods[0].end;
    // The use path hovers with the module's exports.
    assert!(
        checked.info.hovers.iter().any(|(_, t)| t.starts_with("module lib") && t.contains("yell")),
        "no module hover: {:?}",
        checked.info.hovers
    );
    // The selected name hovers with its signature.
    assert!(
        checked.info.hovers.iter().any(|(_, t)| t.starts_with("yell ::")),
        "no signature hover for the selected name"
    );
    // And go-to-definition refs cross the module boundary: a use inside the
    // entry resolves to a definition span inside lib's range.
    assert!(
        checked
            .info
            .refs
            .iter()
            .any(|(u, d)| u.start <= entry_end && d.start > entry_end),
        "no cross-module ref: {:?}",
        checked.info.refs
    );
}

// ---- function types --------------------------------------------------------------

#[test]
fn function_type_rows_are_contracts() {
    let errors = check_errors(r#"
struct Boom = { Int code }

pure :: ((Int) -> Int f, Int x) -> Int {
    f(x)
}

main :: () {
    pure((n) -> {
        if n > 5 {
            fail Boom(n)
        }
        n
    }, 3)
}
"#);
    assert!(
        errors
            .iter()
            .any(|e| e.contains("can fail with `Boom`") && e.contains("does not declare")),
        "got: {errors:?}"
    );
}

#[test]
fn type_parameters_are_opaque() {
    let errors = check_errors(r#"
add :: (a x, a y) -> a {
    x + y
}

main :: () {
    println(add(1, 2))
}
"#);
    assert!(
        errors.iter().any(|e| e.contains("not defined for the type parameter")),
        "got: {errors:?}"
    );
}

// ---- tuples / record update / exhaustiveness -------------------------------------

#[test]
fn match_must_be_exhaustive() {
    let errors = check_errors(r#"
enum Signal = Go | Stop { String why }

main :: () {
    s = Go
    n = match s {
        Go -> 1
    }
    println(n)
}
"#);
    assert!(
        errors.iter().any(|e| e.contains("not exhaustive") && e.contains("`Stop`")),
        "got: {errors:?}"
    );
}

#[test]
fn await_requires_a_task() {
    let errs = check_errors("main :: () {\n    println(await(3))\n}\n");
    assert!(errs.iter().any(|m| m.contains("Task")), "got: {errs:?}");
}

#[test]
fn arena_scopes_reject_uncopyable_values() {
    let errs = check_errors(
        "main :: () {\n    f = {\n        provide Arena(16.kb)\n        (Int x) -> x + 1\n    }\n    println(f(1))\n}\n",
    );
    assert!(
        errs.iter().any(|m| m.contains("cannot be copied")),
        "got: {errs:?}"
    );
}

#[test]
fn asserts_count_toward_the_error_row() {
    // Unhandled in main -> the usual "main must handle" error.
    let errs = check_errors("main :: () {\n    assert(true)\n}\n");
    assert!(
        errs.iter().any(|m| m.contains("AssertFailed")),
        "got: {errs:?}"
    );
}
