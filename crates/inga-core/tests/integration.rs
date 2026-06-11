//! End-to-end tests: source text in, diagnostics/output/formatting out.

use inga_core::diag::Severity;
use inga_core::{check_source, fmt, interp};

fn run(src: &str) -> String {
    let checked = check_source(src);
    let errors: Vec<String> = checked
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.message.clone())
        .collect();
    assert!(errors.is_empty(), "unexpected check errors: {errors:?}\nsource:\n{src}");
    interp::run_captured(&checked.program, "main").expect("runtime error")
}

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
fn hello_world() {
    let out = run(r#"
main :: () {
    println("hello, ${1 + 1} worlds")
}
"#);
    assert_eq!(out, "hello, 2 worlds\n");
}

#[test]
fn string_interpolation_nests() {
    let out = run(r#"
main :: () {
    inner = "in${"ner"}"
    println("outer ${inner} ${1 + 2}")
}
"#);
    assert_eq!(out, "outer inner 3\n");
}

#[test]
fn pipes_insert_first_argument() {
    let out = run(r#"
add :: (Int a, Int b) -> Int {
    a + b
}

main :: () {
    println(10 |> add(5))
}
"#);
    assert_eq!(out, "15\n");
}

#[test]
fn match_options_and_literals() {
    let out = run(r#"
describe :: (n) {
    match n {
        0 -> "zero"
        1 -> "one"
        _ -> "many"
    }
}

main :: () {
    println(describe(0), describe(1), describe(7))
    println(match Some("x") { Some(v) -> v, None -> "-" })
}
"#);
    assert_eq!(out, "zero one many\nx\n");
}

#[test]
fn lambdas_and_map() {
    let out = run(r#"
main :: () {
    println(map([1, 2, 3], (x) -> x * x))
    println(map(Some(4), (x) -> x + 1))
    println(map(None, (x) -> x + 1))
}
"#);
    assert_eq!(out, "[1, 4, 9]\nSome(5)\nNone\n");
}

#[test]
fn durations_and_schedules() {
    let out = run(r#"
use std/schedule

main :: () {
    d = 2.minutes + 30.seconds
    println(d)
    println(schedule.exponential(100.millis) |> schedule.upTo(3))
}
"#);
    assert_eq!(out, "150.seconds\nschedule.exponential(100.millis) |> schedule.upTo(3)\n");
}

// ---- errors as effects ---------------------------------------------------------

#[test]
fn fail_and_catch() {
    let out = run(r#"
struct Boom = { String why }

risky :: (Bool go) {
    if go {
        fail Boom("kaboom")
    }
    "safe"
}

main :: () {
    a = risky(false) |> catch { Boom(why) -> why }
    b = risky(true) |> catch { Boom(why) -> why }
    println(a, b)
}
"#);
    assert_eq!(out, "safe kaboom\n");
}

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
fn catch_all_with_binding() {
    let out = run(r#"
struct Boom = { Int code }

main :: () {
    n = { fail Boom(7) } |> catch { e -> e.code }
    println(n)
}
"#);
    assert_eq!(out, "7\n");
}

#[test]
fn or_fail_converts_none() {
    let out = run(r#"
struct Missing = { String key }

lookup :: (String key) {
    None |> orFail(Missing(key))
}

main :: () {
    println(lookup("a") |> catch { Missing(key) -> "missing ${key}" })
}
"#);
    assert_eq!(out, "missing a\n");
}

#[test]
fn fail_accepts_any_value() {
    let out = run(r#"
risky :: (Int n) -> Int ! String, Int {
    if n == 0 {
        fail "zero"
    }
    if n < 0 {
        fail n
    }
    n
}

main :: () {
    a = risky(5) |> catch { String s -> -1, Int m -> m }
    b = risky(0) |> catch { String s -> -1, Int m -> m }
    c = risky(-3) |> catch { String s -> -1, Int m -> m }
    println(a, b, c)
}
"#);
    assert_eq!(out, "5 -1 -3\n");
}

#[test]
fn catch_literal_arms_match_by_value() {
    let out = run(r#"
risky :: (Int n) -> String ! Int {
    if n > 0 {
        fail n
    }
    "ok"
}

main :: () {
    a = risky(404) |> catch { 404 -> "not found", Int m -> "code ${m}" }
    b = risky(500) |> catch { 404 -> "not found", Int m -> "code ${m}" }
    println(a)
    println(b)
}
"#);
    assert_eq!(out, "not found\ncode 500\n");
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
fn enums_construct_and_match() {
    let out = run(r#"
enum Shape = Circle { Float radius } | Rect { Float w, Float h } | Dot

area :: (Shape s) -> Float {
    match s {
        Circle(r)  -> 3.0 * r * r
        Rect(w, h) -> w * h
        Dot        -> 0.0
    }
}

main :: () {
    println(area(Circle(2.0)), area(Rect(3.0, 4.0)), area(Dot))
    println(show(Dot), show(Circle(1.5)))
    println(Dot == Dot, Circle(1.0) == Dot)
}
"#);
    assert_eq!(out, "12.0 12.0 0.0\nDot Circle(radius: 1.5)\ntrue false\n");
}

#[test]
fn failed_enums_catch_by_variant_with_full_coverage() {
    let out = run(r#"
enum Signal = Go | Stop { String why }

drive :: (Bool ok) -> Int ! Signal {
    if ok {
        fail Go
    }
    fail Stop("red light")
}

main :: () {
    a = drive(true) |> catch { Go -> 1, Stop(why) -> 0 }
    b = drive(false) |> catch { Go -> 1, Stop(why) -> 0 }
    c = drive(false) |> catch { Signal s -> 2 }
    println(a, b, c)
}
"#);
    assert_eq!(out, "1 0 2\n");
}

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
fn services_are_inferred_and_provided() {
    let out = run(r#"
service Greeter {
    greet :: (String name) -> String
}

shouty :: Greeter {
    greet :: (name) {
        "HELLO ${name}"
    }
}

welcome :: (name) {
    Greeter greeter
    greeter.greet(name)
}

main :: () {
    provide shouty {
        println(welcome("wing"))
    }
}
"#);
    assert_eq!(out, "HELLO wing\n");
}

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
fn provide_scopes_dynamically_and_instances_are_fresh() {
    let out = run(r#"
service Store {
    bump :: () -> Int
}

counter :: Store {
    state = MutMap()

    bump :: () {
        n = state.get("n") |> getOrElse(0)
        state.set("n", n + 1)
        n
    }
}

main :: () {
    provide counter {
        Store s
        println(s.bump(), s.bump())
        provide counter {
            Store fresh
            println(fresh.bump())
        }
    }
}
"#);
    assert_eq!(out, "0 1\n0\n");
}

// ---- retry / laziness ----------------------------------------------------------------

#[test]
fn retry_reevaluates_until_success() {
    let out = run(r#"
use std/schedule

struct Flaky = { Int n }

service Counter {
    next :: () -> Int
}

mem :: Counter {
    state = MutMap()

    next :: () {
        n = state.get("n") |> getOrElse(0)
        state.set("n", n + 1)
        n
    }
}

attempt :: () {
    Counter c
    n = c.next()
    if n < 2 {
        fail Flaky(n)
    }
    n
}

main :: () {
    provide mem {
        // retry does not clear the error row — a retried action can still
        // fail — so main still has to catch.
        n = attempt()
            |> retry(schedule.fixed(1.millis) |> schedule.upTo(5))
            |> catch { Flaky -> -1 }
        println(n)
    }
}
"#);
    assert_eq!(out, "2\n");
}

#[test]
fn ignore_failure_swallows_errors() {
    let out = run(r#"
struct Boom = { Int x }

main :: () {
    { fail Boom(1) } |> ignoreFailure
    println("survived")
}
"#);
    assert_eq!(out, "survived\n");
}

#[test]
fn lazy_params_defer_evaluation() {
    let out = run(r#"
struct Boom = { Int x }

pick :: (Bool first, lazy Int a, lazy Int b) -> Int {
    if first {
        a
    } else {
        b
    }
}

boom :: () -> Int ! Boom {
    fail Boom(1)
}

main :: () {
    n = pick(true, 10, boom()) |> catch { Boom -> -1 }
    println(n)
}
"#);
    assert_eq!(out, "10\n");
}

#[test]
fn now_millis_is_monotonic() {
    let out = run(r#"
main :: () {
    a = nowMillis()
    b = nowMillis()
    println(b >= a && a >= 0)
}
"#);
    assert_eq!(out, "true\n");
}

// ---- encode / decode -------------------------------------------------------------------

#[test]
fn encode_decode_roundtrip() {
    let out = run(r#"
struct User = { Int id, String name }

main :: () {
    raw = encode(User(7, "Ada"))
    println(raw)
    user = decode(raw, User) |> catch { DecodeError e -> User(0, e.message) }
    println(user.name)
}
"#);
    assert_eq!(out, "{\"id\":7,\"name\":\"Ada\"}\nAda\n");
}

#[test]
fn decode_failure_is_typed() {
    let out = run(r#"
struct User = { Int id, String name }

main :: () {
    user = decode("not json", User) |> catch { DecodeError(msg) -> User(-1, "bad") }
    println(user.id, user.name)
}
"#);
    assert_eq!(out, "-1 bad\n");
}

// ---- type errors ----------------------------------------------------------------------

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
fn user_service_example_runs() {
    let path = format!("{}/../../examples/user_service.inga", env!("CARGO_MANIFEST_DIR"));
    let src = std::fs::read_to_string(path).unwrap();
    let checked = check_source(&src);
    let errors: Vec<&str> = checked
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.message.as_str())
        .collect();
    assert!(errors.is_empty(), "example has errors: {errors:?}");
    let out = interp::run_captured(&checked.program, "main").unwrap();
    assert_eq!(
        out,
        "[info] cache refreshed for 42\n\
         fetched: Wing <wing@anara.com>\n\
         cached:  Wing\n\
         fallback for user 7: anonymous\n"
    );
}

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
fn provide_scopes_left_to_right_and_inline() {
    let out = run(r#"
service Logger {
    log :: (String m)
}
service Db {
    get :: () -> String
}

quiet :: Logger {
    log :: (m) {
        println("[q] ${m}")
    }
}

db :: Db {
    banner = {
        Logger logger
        logger.log("connect")
        "up"
    }
    get :: () {
        banner
    }
}

main :: () {
    provide quiet, db
    Db d
    println(d.get())
}
"#);
    assert_eq!(out, "[q] connect\nup\n");
}

#[test]
fn arena_scopes_check_and_run() {
    let out = run(r#"
main :: () {
    n = provide Arena(64.kb) { len("in the region ${21 * 2}") }
    println(n)
}
"#);
    assert_eq!(out, "16\n");
}

#[test]
fn arena_result_must_not_escape() {
    let errors = check_errors(r#"
main :: () {
    s = provide Arena(64.kb) { "escapes ${1}" }
    println(s)
}
"#);
    assert!(
        errors.iter().any(|e| e.contains("must not escape")),
        "got: {errors:?}"
    );
}

#[test]
fn size_suffixes_are_ints() {
    let out = run(r#"
main :: () {
    println(2.kb, 1.mb)
}
"#);
    assert_eq!(out, "2048 1048576\n");
}

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
fn modules_import_pub_and_hide_private() {
    use std::path::Path;
    let lib = r#"
pub yell :: (String s) -> String {
    "${s}${bang()}"
}

bang :: () -> String {
    "!"
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
    let (checked, _mods) = inga_core::check_loaded(loaded);
    let errors: Vec<&str> = checked
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.message.as_str())
        .collect();
    assert!(errors.is_empty(), "unexpected: {errors:?}");
    let out = interp::run_captured(&checked.program, "main").unwrap();
    assert_eq!(out, "hey!\n");

    // A plain `use lib` binds the qualified alias instead.
    let qualified = "\nuse lib\n\nmain :: () {\n    println(lib.yell(\"yo\"))\n}\n";
    let loaded = inga_core::modules::load_program_with(
        Path::new("/virtual/main.inga"),
        qualified.to_string(),
        &mut |p| (p == Path::new("/virtual/lib.inga")).then(|| lib.to_string()),
    );
    let (checked, _mods) = inga_core::check_loaded(loaded);
    let errors: Vec<&str> = checked
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.message.as_str())
        .collect();
    assert!(errors.is_empty(), "qualified call failed: {errors:?}");
    let out = interp::run_captured(&checked.program, "main").unwrap();
    assert_eq!(out, "yo!\n");

    // ...and a bare name through a plain import is rejected with a hint.
    let bare = main_src.replace("use lib { yell }", "use lib");
    let loaded = inga_core::modules::load_program_with(
        Path::new("/virtual/main.inga"),
        bare,
        &mut |p| (p == Path::new("/virtual/lib.inga")).then(|| lib.to_string()),
    );
    let (checked, _mods) = inga_core::check_loaded(loaded);
    assert!(
        checked
            .diagnostics
            .iter()
            .any(|d| d.message.contains("call it as `lib.yell`")),
        "got: {:?}",
        checked.diagnostics
    );

    // Private names do not cross the module boundary.
    let bad = main_src.replace("use lib { yell }", "use lib { bang }").replace("yell(\"hey\")", "bang()");
    let loaded = inga_core::modules::load_program_with(
        Path::new("/virtual/main.inga"),
        bad,
        &mut |p| (p == Path::new("/virtual/lib.inga")).then(|| lib.to_string()),
    );
    let (checked, _mods) = inga_core::check_loaded(loaded);
    assert!(
        checked.diagnostics.iter().any(|d| d.message.contains("private to module `lib`")),
        "got: {:?}",
        checked.diagnostics
    );
}

#[test]
fn pipe_feeds_first_argument_of_builtins_too() {
    let out = run(r#"
main :: () {
    n = [1, 2, 3]
        |> map((x) -> x * 10)
        |> len
    println(n)
    println([4, 5] |> map((x) -> x + 1))
}
"#);
    assert_eq!(out, "3\n[5, 6]\n");
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
