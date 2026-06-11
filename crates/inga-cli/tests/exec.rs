//! End-to-end execution tests: each program is compiled to a native binary
//! by the `inga` CLI and run; stdout is asserted. Checker-only assertions
//! (`check_errors`) call into inga-core directly.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use inga_core::check_source;
use inga_core::diag::Severity;

static NEXT: AtomicU32 = AtomicU32::new(0);

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("inga-exec-tests-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_temp(src: &str) -> PathBuf {
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = temp_dir().join(format!("t{n}.inga"));
    std::fs::write(&path, src).unwrap();
    path
}

fn run_file(path: &std::path::Path) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_inga")).arg("run").arg(path).output().unwrap();
    assert!(
        out.status.success(),
        "inga run {} failed:\n{}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn run(src: &str) -> String {
    let path = write_temp(src);
    let out = Command::new(env!("CARGO_BIN_EXE_inga")).arg("run").arg(&path).output().unwrap();
    assert!(
        out.status.success(),
        "inga run failed:\n--- stderr ---\n{}\n--- source ---\n{src}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn check_errors(src: &str) -> Vec<String> {
    check_source(src)
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.message.clone())
        .collect()
}

#[allow(dead_code)]
fn check_warnings(src: &str) -> Vec<String> {
    check_source(src)
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Warning)
        .map(|d| d.message.clone())
        .collect()
}

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
fn arena_results_are_copied_out() {
    // Heap-shaped results no longer need to stay inside the scope — they
    // are deep-copied past the region as it is freed.
    let out = run(r#"
main :: () {
    s = provide Arena(64.kb) { "escapes ${1}" }
    println(s)
}
"#);
    assert_eq!(out, "escapes 1\n");
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
fn function_types_annotate_callbacks() {
    let out = run(r#"
struct Boom = { Int code }

twice :: ((Int) -> Int f, Int x) -> Int {
    f(f(x))
}

attempt :: ((Int) -> Int ! Boom f, Int x) -> Int {
    f(x) |> catch { Boom(code) -> -code }
}

pick :: (((Int) -> Int)? maybe, Int x) -> Int {
    match maybe {
        Some(f) -> f(x)
        None    -> x
    }
}

main :: () {
    println(twice((n) -> n * 3, 2))
    println(attempt((n) -> {
        if n > 5 {
            fail Boom(n)
        }
        n * 10
    }, 9))
    println(pick(Some((n) -> n + 1), 41), pick(None, 7))
    (Int) -> Int g = (n) -> n - 1
    println(g(100))
}
"#);
    assert_eq!(out, "18\n-9\n42 7\n99\n");
}

#[test]
fn stdlib_lists_strings_conversions() {
    let out = run(r#"
main :: () {
    xs = [5, 1, 4, 2, 3]
    println(filter(xs, (x) -> x >= 3), fold(xs, 0, (acc, x) -> acc + x))
    println(at(xs, 2) |> getOrElse(-1), at(xs, 9) |> getOrElse(-1))
    println(concat([1, 2], [3]), reverse([1, 2, 3]))
    println(split("a,bb,ccc", ","), slice("hello world", 6, 11))
    println(indexOf("hello", "ll"), indexOf("hello", "zz"), trim("  pad  "))
    println(parseInt("42") |> getOrElse(0), parseInt("nope") |> getOrElse(-1))
    println(toFloat(7) / 2.0, floor(3.9))
}
"#);
    assert_eq!(
        out,
        "[5, 4, 3] 15\n4 -1\n[1, 2, 3] [3, 2, 1]\n[\"a\", \"bb\", \"ccc\"] world\n2 -1 pad\n42 -1\n3.5 3\n"
    );
}

// ---- generics ---------------------------------------------------------------------

#[test]
fn generic_functions_instantiate_per_call_site() {
    let out = run(r#"
myMap :: ([a] xs, (a) -> b f) -> [b] {
    fold(xs, [], (acc, x) -> concat(acc, [f(x)]))
}

first :: ([a] xs, a fallback) -> a {
    at(xs, 0) |> getOrElse(fallback)
}

main :: () {
    println(myMap([1, 2, 3], (n) -> n * n))
    println(myMap(["a", "bb"], (s) -> len(s)))
    println(first([7, 8], 0), first(["x"], "?"))
}
"#);
    assert_eq!(out, "[1, 4, 9]\n[1, 2]\n7 x\n");
}

#[test]
fn tuples_construct_index_match() {
    let out = run(r#"
minMax :: ([Int] xs) -> (Int, Int) {
    fold(xs, (9999, -9999), (acc, x) -> {
        lo = if x < acc.0 { x } else { acc.0 }
        hi = if x > acc.1 { x } else { acc.1 }
        (lo, hi)
    })
}

main :: () {
    pair = (1, "two")
    println(pair.0, pair.1, pair)
    bounds = minMax([5, 2, 9, 4])
    match bounds {
        (2, hi) -> println("lo two, hi ${hi}")
        (lo, hi) -> println("${lo}..${hi}")
    }
    println((1, 2) == (1, 2), (1, 2) == (1, 3))
}
"#);
    assert_eq!(out, "1 two (1, \"two\")\nlo two, hi 9\ntrue false\n");
}

#[test]
fn record_update_copies_and_overrides() {
    let out = run(r#"
struct User = { Int id, String name, Int score }

main :: () {
    u = User(7, "Ada", 10)
    promoted = User { ..u, score: u.score + 5, name: "Ada L" }
    println(promoted, u.score)
}
"#);
    assert_eq!(out, "User(id: 7, name: \"Ada L\", score: 15) 10\n");
}

#[test]
fn tasks_spawn_await_round_trip() {
    let out = run(
        "double :: (Int n) -> Int {\n    n * 2\n}\n\nmain :: () {\n    xs = [1, 2, 3]\n    t = spawn(map(xs, double))\n    u = spawn(\"ready\")\n    println(await(t), await(u))\n}\n",
    );
    assert_eq!(out, "[2, 4, 6] ready\n");
}

#[test]
fn task_errors_reraise_at_await() {
    // A failing action's error travels in the Task type and surfaces at
    // the await, where the normal catch machinery applies.
    let out = run(
        "struct Boom = { Int n }\n\nrisky :: () -> Int ! Boom {\n    fail Boom(7)\n}\n\nmain :: () {\n    t = risky() |> spawn\n    println(await(t) |> catch { Boom(n) -> n * 10 })\n}\n",
    );
    assert_eq!(out, "70\n");

    // Left unhandled, it reaches `main`'s row like any other error.
    let errs = check_errors(
        "struct Boom = { Int n }\n\nrisky :: () -> Int ! Boom {\n    fail Boom(1)\n}\n\nmain :: () {\n    t = spawn(risky())\n    println(await(t))\n}\n",
    );
    assert!(
        errs.iter().any(|m| m.contains("`main` does not handle the error `Boom`")),
        "got: {errs:?}"
    );

    // Tasks aliased through a list union their error rows.
    let errs = check_errors(
        "struct A = { Int n }\nstruct B = { Int n }\n\nfa :: () -> Int ! A {\n    fail A(1)\n}\n\nfb :: () -> Int ! B {\n    fail B(2)\n}\n\nmain :: () {\n    ts = [spawn(fa()), spawn(fb())]\n    map(ts, (t) -> await(t) |> catch { A -> 0 })\n}\n",
    );
    assert!(
        errs.iter().any(|m| m.contains("`main` does not handle the error `B`")),
        "got: {errs:?}"
    );
}

#[test]
fn spawned_tasks_share_only_stateless_services() {
    // A scalar-state service crosses into the task; spawn captures the
    // evidence in scope like a call would.
    let out = run(
        "service Adder {\n    add :: (Int a, Int b) -> Int\n}\n\nplainAdder :: Adder {\n    add :: (a, b) {\n        a + b\n    }\n}\n\ndouble :: (Int n) -> Int uses Adder {\n    Adder adder\n    adder.add(n, n)\n}\n\nmain :: () {\n    provide plainAdder\n    t = double(21) |> spawn\n    println(await(t))\n}\n",
    );
    assert_eq!(out, "42\n");

    // A MutMap-backed implementation is rejected with guidance.
    let errs = check_errors(
        "service Store {\n    put :: (Int k, Int v)\n}\n\nmemStore :: Store {\n    m = MutMap()\n\n    put :: (k, v) {\n        m.set(k, v)\n    }\n}\n\nuseStore :: () uses Store {\n    Store store\n    store.put(1, 2)\n}\n\nmain :: () {\n    provide memStore\n    t = spawn(useStore())\n    await(t)\n}\n",
    );
    assert!(
        errs.iter().any(|m| m.contains("shareable") && m.contains("memStore")),
        "got: {errs:?}"
    );

    // Handling everything inside the spawn still works, of course.
    let out = run(
        "struct Boom = { Int n }\n\nrisky :: () -> Int ! Boom {\n    fail Boom(7)\n}\n\nmain :: () {\n    t = spawn(risky() |> catch { Boom(n) -> n })\n    println(await(t))\n}\n",
    );
    assert_eq!(out, "7\n");
}

#[test]
fn arena_scopes_copy_their_value_out() {
    let out = run(
        "struct Stats = { Int count, [Int] kept }\n\nsummarize :: ([Int] xs) -> Stats {\n    provide Arena(64.kb)\n    evens = filter(xs, (x) -> x % 2 == 0)\n    Stats(len(evens), evens)\n}\n\nmain :: () {\n    println(summarize(range(6)))\n}\n",
    );
    assert_eq!(out, "Stats(count: 3, kept: [0, 2, 4])\n");
}

#[test]
fn asserts_fail_with_assert_failed() {
    let out = run(
        "main :: () {\n    assertEq(2 + 2, 4) |> catch { AssertFailed(m) -> println(m) }\n    assertEq(\"a\", \"b\") |> catch { AssertFailed(m) -> println(\"caught:\", m) }\n    assert(false) |> catch { AssertFailed(m) -> println(\"caught:\", m) }\n}\n",
    );
    assert_eq!(
        out,
        "caught: assertEq failed: \"a\" != \"b\"\ncaught: assert failed: condition was false\n"
    );
}

#[test]
fn mutmap_and_task_have_surface_types() {
    // The forms hover renders are writable: MutMap<K, V> and Task<T>.
    let out = run(
        "service Stats {\n    counts :: () -> MutMap<String, Int>\n}\n\nmemStats :: Stats {\n    m = MutMap()\n\n    counts :: () {\n        m\n    }\n}\n\nbump :: (String k) -> Int uses Stats {\n    Stats stats\n    n = stats.counts().get(k) |> getOrElse(0)\n    stats.counts().set(k, n + 1)\n    n + 1\n}\n\nslowDouble :: (Int n) -> Int {\n    n * 2\n}\n\nstartDouble :: (Int n) -> Task<Int> {\n    slowDouble(n) |> spawn\n}\n\nmain :: () {\n    provide memStats\n    bump(\"a\")\n    println(bump(\"a\"), await(startDouble(21)))\n}\n",
    );
    assert_eq!(out, "2 42\n");

    // Other names take no type arguments.
    let errs = check_errors(
        "struct User = { Int id }\n\nf :: (User<Int> u) -> Int {\n    1\n}\n\nmain :: () {\n    println(f(User(1)))\n}\n",
    );
    assert!(
        errs.iter().any(|m| m.contains("does not take type arguments")),
        "got: {errs:?}"
    );
}

// ---- the flagship example -----------------------------------------------------------------

#[test]
fn user_service_example_runs() {
    let path = format!("{}/../../examples/user_service.inga", env!("CARGO_MANIFEST_DIR"));
    let out = run_file(std::path::Path::new(&path));
    assert_eq!(
        out,
        "[info] cache refreshed for 42\nfetched: Wing <wing@anara.com>\ncached:  Wing\nfallback for user 7: anonymous\n"
    );
}

#[test]
fn modules_import_pub_and_hide_private() {
    let dir = temp_dir().join("mods");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("lib.inga"),
        "pub yell :: (String s) -> String {\n    \"${s}!\"\n}\n\nwhisper :: (String s) -> String {\n    s\n}\n",
    )
    .unwrap();

    // Selective import: the listed name is usable unqualified.
    std::fs::write(
        dir.join("main.inga"),
        "use lib { yell }\n\nmain :: () {\n    println(yell(\"hey\"))\n}\n",
    )
    .unwrap();
    assert_eq!(run_file(&dir.join("main.inga")), "hey!\n");

    // A plain `use lib` binds the qualified alias instead.
    std::fs::write(
        dir.join("main.inga"),
        "use lib\n\nmain :: () {\n    println(lib.yell(\"yo\"))\n}\n",
    )
    .unwrap();
    assert_eq!(run_file(&dir.join("main.inga")), "yo!\n");

    // Private names stay private, qualified or not.
    std::fs::write(
        dir.join("main.inga"),
        "use lib\n\nmain :: () {\n    println(lib.whisper(\"shh\"))\n}\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_inga"))
        .arg("run")
        .arg(dir.join("main.inga"))
        .output()
        .unwrap();
    assert!(!out.status.success(), "private access must be rejected");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("private"), "got: {stderr}");
}
