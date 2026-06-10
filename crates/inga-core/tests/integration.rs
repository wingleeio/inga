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
main :: () {
    d = 2.minutes + 30.seconds
    println(d)
    println(Schedule.exponential(100.millis) |> upTo(3))
}
"#);
    assert_eq!(out, "150.seconds\nSchedule.exponential(100.millis) |> upTo(3)\n");
}

// ---- errors as effects ---------------------------------------------------------

#[test]
fn fail_and_catch() {
    let out = run(r#"
error Boom = { String why }

risky :: (Bool go) {
    if go {
        fail Boom("kaboom")
    }
    "safe"
}

main :: () {
    a = risky(false) |> catch { Boom(e) -> e.why }
    b = risky(true) |> catch { Boom(e) -> e.why }
    println(a, b)
}
"#);
    assert_eq!(out, "safe kaboom\n");
}

#[test]
fn uncaught_error_in_main_is_rejected() {
    let errors = check_errors(r#"
error Boom = { String why }

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
error A = { Int x }
error B = { Int x }

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
error A = { Int x }
error B = { Int x }

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
error Boom = { Int code }

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
error Missing = { String key }

lookup :: (String key) {
    None |> orFail(Missing(key))
}

main :: () {
    println(lookup("a") |> catch { Missing(e) -> "missing ${e.key}" })
}
"#);
    assert_eq!(out, "missing a\n");
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
error Flaky = { Int n }

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
            |> retry(Schedule.fixed(1.millis) |> upTo(5))
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
error Boom = { Int x }

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
error Boom = { Int x }

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

// ---- encode / decode -------------------------------------------------------------------

#[test]
fn encode_decode_roundtrip() {
    let out = run(r#"
type User = { Int id, String name }

main :: () {
    raw = encode(User(7, "Ada"))
    println(raw)
    user = decode(raw, User) |> catch { DecodeError(e) -> User(0, e.message) }
    println(user.name)
}
"#);
    assert_eq!(out, "{\"id\":7,\"name\":\"Ada\"}\nAda\n");
}

#[test]
fn decode_failure_is_typed() {
    let out = run(r#"
type User = { Int id, String name }

main :: () {
    user = decode("not json", User) |> catch { DecodeError(e) -> User(-1, "bad") }
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
