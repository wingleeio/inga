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
fn arena_scopes_copy_their_value_out() {
    let out = run(
        "struct Stats = { Int count, [Int] kept }\n\nsummarize :: ([Int] xs) -> Stats {\n    provide Arena(64.kb)\n    evens = filter(xs, (x) -> x % 2 == 0)\n    Stats(len(evens), evens)\n}\n\nmain :: () {\n    println(summarize(range(6)))\n}\n",
    );
    assert_eq!(out, "Stats(count: 3, kept: [0, 2, 4])\n");
}

#[test]
fn asserts_fail_with_assert_failed() {
    let out = run(
        "main :: () {\n    assertEq(2 + 2, 4) |> catch { AssertionError(m) -> println(m) }\n    assertEq(\"a\", \"b\") |> catch { AssertionError(m) -> println(\"caught:\", m) }\n    assert(false) |> catch { AssertionError(m) -> println(\"caught:\", m) }\n}\n",
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
        "use std/fiber\n\nservice Stats {\n    counts :: () -> MutMap<String, Int>\n}\n\nmemStats :: Stats {\n    m = MutMap()\n\n    counts :: () {\n        m\n    }\n}\n\nbump :: (String k) -> Int uses Stats {\n    Stats stats\n    n = stats.counts().get(k) |> getOrElse(0)\n    stats.counts().set(k, n + 1)\n    n + 1\n}\n\nslowDouble :: (Int n) -> Int {\n    n * 2\n}\n\nstartDouble :: (Int n) -> Fiber<Int> uses Fibers {\n    slowDouble(n) |> fiber.fork\n}\n\nmain :: () {\n    provide Runtime(1), memStats\n    bump(\"a\")\n    println(bump(\"a\"), fiber.join(startDouble(21)))\n}\n",
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

#[test]
fn fibers_fork_join_round_trip() {
    let out = run(
        "use std/fiber\n\ndouble :: (Int n) -> Int {\n    n * 2\n}\n\nmain :: () {\n    provide Runtime(2)\n    t = map([1, 2, 3], double) |> fiber.fork\n    u = \"ready\" |> fiber.fork\n    println(fiber.join(t), fiber.join(u))\n}\n",
    );
    assert_eq!(out, "[2, 4, 6] ready\n");
}

#[test]
fn fiber_errors_reraise_at_join() {
    let out = run(
        "use std/fiber\n\nstruct Boom = { Int n }\n\nrisky :: () -> Int ! Boom {\n    fail Boom(7)\n}\n\nmain :: () {\n    provide Runtime(1)\n    t = risky() |> fiber.fork\n    println(fiber.join(t) |> catch { Boom(n) -> n * 10 })\n}\n",
    );
    assert_eq!(out, "70\n");

    // Left unhandled, the row reaches main like any other error.
    let errs = check_errors(
        "use std/fiber\n\nstruct Boom = { Int n }\n\nrisky :: () -> Int ! Boom {\n    fail Boom(1)\n}\n\nmain :: () {\n    provide Runtime(1)\n    println(fiber.join(risky() |> fiber.fork))\n}\n",
    );
    assert!(
        errs.iter().any(|m| m.contains("`main` does not handle the error `Boom`")),
        "got: {errs:?}"
    );

    // No Runtime provided -> the Fibers capability diagnostic teaches it.
    let errs = check_errors(
        "use std/fiber\n\nmain :: () {\n    println(fiber.join((1 + 1) |> fiber.fork))\n}\n",
    );
    assert!(errs.iter().any(|m| m.contains("provide Runtime")), "got: {errs:?}");
}

#[test]
fn structural_join_tuples_and_lists() {
    let out = run(
        "use std/fiber\n\nsq :: (Int n) -> Int {\n    n * n\n}\n\nmain :: () {\n    provide Runtime(2)\n    pair = fiber.join((sq(3) |> fiber.fork, sq(4) |> fiber.fork))\n    println(pair.0 + pair.1)\n    println(fiber.join([sq(2) |> fiber.fork, sq(5) |> fiber.fork]))\n    both = fiber.par(sq(6), sq(7))\n    println(both.0, both.1)\n}\n",
    );
    assert_eq!(out, "25\n[4, 25]\n36 49\n");
}

#[test]
fn settle_outcome_and_partition() {
    let out = run(
        "use std/fiber\n\nstruct TooBig = { Int n }\n\ncheck :: (Int n) -> Int ! TooBig {\n    if n > 10 {\n        fail TooBig(n)\n    }\n    n * 2\n}\n\nmain :: () {\n    provide Runtime(1)\n    outcomes = map([1, 50, 3], (n) -> check(n) |> fiber.settle)\n    parts = fiber.partition(outcomes)\n    println(len(parts.0), len(parts.1))\n    map(outcomes, (o) -> match o {\n        Ok(v) -> println(\"ok\", v)\n        Failed(TooBig(n)) -> println(\"big\", n)\n    })\n    println(check(4) |> fiber.settle |> fiber.unsettle |> catch { TooBig -> -1 })\n}\n",
    );
    assert_eq!(out, "2 1\nok 2\nbig 50\nok 6\n8\n");

    // settle is row-free: no Runtime needed for sequential batches.
    let out = run(
        "use std/fiber\n\nstruct Nope = { Int n }\n\nf :: (Int n) -> Int ! Nope {\n    if n < 0 {\n        fail Nope(n)\n    }\n    n\n}\n\nmain :: () {\n    o = f(-1) |> fiber.settle\n    match o {\n        Ok(v) -> println(v)\n        Failed(Nope(n)) -> println(\"nope\", n)\n    }\n}\n",
    );
    assert_eq!(out, "nope -1\n");
}

#[test]
fn parmap_race_and_within() {
    let out = run(
        "use std/fiber\n\nslow :: () -> Int {\n    sleep(2.seconds)\n    99\n}\n\nmain :: () {\n    provide Runtime(4)\n    println(fiber.parMap([1, 2, 3], (n) -> n * 10))\n    fast = fiber.within(40 + 2, 1.seconds) |> catch { TimeoutError -> -1 }\n    println(fast)\n    timed = fiber.within(slow(), 50.millis) |> catch { TimeoutError -> -1 }\n    println(timed)\n    won = fiber.race(7, slow())\n    println(won)\n}\n",
    );
    assert_eq!(out, "[10, 20, 30]\n42\n-1\n7\n");
}

#[test]
fn shared_services_cross_fibers() {
    let out = run(
        "use std/fiber\n\nshared service Adder {\n    add :: (Int a, Int b) -> Int\n}\n\nplainAdder :: Adder {\n    add :: (a, b) {\n        a + b\n    }\n}\n\ndouble :: (Int n) -> Int uses Adder {\n    Adder adder\n    adder.add(n, n)\n}\n\nmain :: () {\n    provide Runtime(2), plainAdder\n    println(fiber.join(double(21) |> fiber.fork))\n}\n",
    );
    assert_eq!(out, "42\n");

    // Unshared services are rejected with guidance...
    let errs = check_errors(
        "use std/fiber\n\nservice Store {\n    put :: (Int k, Int v)\n}\n\nmemStore :: Store {\n    m = MutMap()\n\n    put :: (k, v) {\n        m.set(k, v)\n    }\n}\n\nuseStore :: () uses Store {\n    Store store\n    store.put(1, 2)\n}\n\nmain :: () {\n    provide Runtime(1), memStore\n    fiber.join(useStore() |> fiber.fork)\n}\n",
    );
    assert!(errs.iter().any(|m| m.contains("only `shared` services")), "got: {errs:?}");

    // ...and a shared declaration is enforced at every impl.
    let errs = check_errors(
        "shared service Store {\n    put :: (Int k, Int v)\n}\n\nmemStore :: Store {\n    m = MutMap()\n\n    put :: (k, v) {\n        m.set(k, v)\n    }\n}\n\nmain :: () {\n    provide memStore\n    Store store\n    store.put(1, 1)\n}\n",
    );
    assert!(
        errs.iter().any(|m| m.contains("shared services may carry only scalar state")),
        "got: {errs:?}"
    );
}

#[test]
fn catch_after_fork_is_guided() {
    let errs = check_errors(
        "use std/fiber\n\nstruct Boom = { Int n }\n\nrisky :: () -> Int ! Boom {\n    fail Boom(1)\n}\n\nmain :: () {\n    provide Runtime(1)\n    t = risky() |> fiber.fork |> catch { Boom -> 0 }\n    fiber.join(t) |> catch { Boom -> -1 }\n}\n",
    );
    assert!(
        errs.iter().any(|m| m.contains("surface at `fiber.join`")),
        "got: {errs:?}"
    );
}

#[test]
fn tap_and_tap_error_observe_without_transforming() {
    let out = run(
        "struct Boom = { Int code }\n\nrisky :: (Int n) -> Int ! Boom {\n    if n > 10 {\n        fail Boom(n)\n    }\n    n * 2\n}\n\nmain :: () {\n    total = [1, 2, 3]\n        |> tap((xs) -> println(\"saw\", len(xs)))\n        |> fold(0, (a, b) -> a + b)\n    println(total)\n    ok = risky(3) |> tapError((e) -> println(\"failed\", e.code)) |> catch { Boom -> -1 }\n    bad = risky(99) |> tapError((e) -> println(\"failed\", e.code)) |> catch { Boom(c) -> c }\n    println(ok, bad)\n}\n",
    );
    assert_eq!(out, "saw 3\n6\nfailed 99\n6 99\n");

    // The row is preserved: tapError alone does not satisfy main.
    let errs = check_errors(
        "struct Boom = { Int code }\n\nrisky :: () -> Int ! Boom {\n    fail Boom(1)\n}\n\nmain :: () {\n    println(risky() |> tapError((e) -> println(e.code)))\n}\n",
    );
    assert!(
        errs.iter().any(|m| m.contains("`main` does not handle the error `Boom`")),
        "got: {errs:?}"
    );
}

#[test]
fn http_get_post_status_and_streaming() {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() || line.is_empty() {
                continue;
            }
            let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
            let is_post = line.starts_with("POST");
            let mut len = 0usize;
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h == "\r\n" || h == "\n" || h.is_empty() {
                    break;
                }
                if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; len];
            if len > 0 {
                let _ = reader.read_exact(&mut body);
            }
            let mut out = stream;
            let respond = |out: &mut std::net::TcpStream, status: &str, body: &[u8]| {
                let _ = write!(
                    out,
                    "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                );
                let _ = out.write_all(body);
            };
            match (is_post, path.as_str()) {
                (false, "/hello") => respond(&mut out, "200 OK", b"hi there"),
                (true, "/echo") => {
                    let mut e = b"echo:".to_vec();
                    e.extend_from_slice(&body);
                    respond(&mut out, "200 OK", &e);
                }
                (false, "/stream") => {
                    let _ = write!(
                        out,
                        "HTTP/1.1 200 OK\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n"
                    );
                    for chunk in [&b"alpha "[..], b"beta ", b"gamma"] {
                        let _ = write!(out, "{:x}\r\n", chunk.len());
                        let _ = out.write_all(chunk);
                        let _ = write!(out, "\r\n");
                        let _ = out.flush();
                    }
                    let _ = write!(out, "0\r\n\r\n");
                }
                _ => respond(&mut out, "404 Not Found", b"nope"),
            }
        }
    });

    let src = format!(
        "use std/http\n\nreadAll :: (HttpStream s, String acc) -> String ! HttpError uses Http {{\n    match http.read(s) {{\n        Some(chunk) -> readAll(s, acc + chunk)\n        None -> acc\n    }}\n}}\n\nmain :: () {{\n    provide Http\n    ok = http.get(\"http://127.0.0.1:{port}/hello\") |> catch {{ HttpError -> HttpResponse(0, \"\") }}\n    println(ok.status, ok.body)\n    missing = http.get(\"http://127.0.0.1:{port}/missing\") |> catch {{ HttpError -> HttpResponse(0, \"\") }}\n    println(missing.status)\n    echoed = http.post(\"http://127.0.0.1:{port}/echo\", \"ping\") |> catch {{ HttpError -> HttpResponse(0, \"\") }}\n    println(echoed.body)\n    streamed = {{\n        s = http.openStream(\"http://127.0.0.1:{port}/stream\")\n        text = readAll(s, \"\")\n        http.close(s)\n        text\n    }} |> catch {{ HttpError -> \"failed\" }}\n    println(streamed)\n    down = http.get(\"http://127.0.0.1:1/x\") |> catch {{ HttpError(status, m) -> HttpResponse(status, \"transport\") }}\n    println(down.status, down.body)\n}}\n"
    );
    let out = run(&src);
    assert_eq!(out, "200 hi there\n404\necho:ping\nalpha beta gamma\n0 transport\n");

    // Requests require the capability: no `provide Http` -> teaching error.
    let errs = check_errors(
        "use std/http\n\nmain :: () {\n    println(http.get(\"http://x\").status |> catch { HttpError -> 0 })\n}\n",
    );
    assert!(
        errs.iter().any(|m| m.contains("`main` requires the service `Http`")),
        "got: {errs:?}"
    );
}

#[test]
fn then_transforms_the_value_mid_pipe() {
    let out = run(
        "struct User = { Int id, String name }\nstruct LookupError = { Int id }\n\nfindUser :: (Int id) -> User ! LookupError {\n    if id > 100 {\n        fail LookupError(id)\n    }\n    User(id, \"user-${id}\")\n}\n\nmain :: () {\n    greeting = findUser(7)\n        |> then((u) -> u.name)\n        |> tap((n) -> println(\"saw:\", n))\n        |> then((n) -> \"hello, ${n}!\")\n        |> catch { LookupError(id) -> \"no user ${id}\" }\n    println(greeting)\n    missing = findUser(999)\n        |> then((u) -> u.name)\n        |> catch { LookupError(id) -> \"no user ${id}\" }\n    println(missing)\n    println(21 |> then((n) -> n * 2))\n}\n",
    );
    assert_eq!(out, "saw: user-7\nhello, user-7!\nno user 999\n42\n");

    // The function's own rows merge like any call.
    let errs = check_errors(
        "struct ParseError = { String s }\n\nmain :: () {\n    println(\"x\" |> then((s) -> {\n        fail ParseError(s)\n        1\n    }))\n}\n",
    );
    assert!(
        errs.iter().any(|m| m.contains("`main` does not handle the error `ParseError`")),
        "got: {errs:?}"
    );
}

#[test]
fn forking_inside_an_arena_scope_is_safe() {
    // Regression: a fork's thunk environment must come from the global
    // heap. When the fork happens inside `provide Arena`, a
    // region-allocated env dies with the scope while the fiber still
    // reads it. MallocScribble poisons freed memory so the old bug
    // crashes deterministically instead of by timing.
    let src = "use std/fiber\n\nslowSum :: (Int n, Int acc) -> Int {\n    if n == 0 {\n        acc\n    } else {\n        slowSum(n - 1, acc + n)\n    }\n}\n\nkick :: (Int n, pending) uses Fibers {\n    provide Arena(64.kb)\n    note = \"${n}-${n * 2}\"\n    pending.set(n, slowSum(n * 100000, 0) |> fiber.fork)\n    println(\"kicked\", note)\n}\n\nmain :: () {\n    provide Runtime(4)\n    pending = MutMap()\n    kick(7, pending)\n    sleep(30.millis)\n    match pending.get(7) {\n        Some(f) -> println(fiber.join(f))\n        None -> println(\"missing\")\n    }\n    nums = {\n        provide Arena(64.kb)\n        fiber.parMap([1, 2, 3], (k) -> slowSum(k * 1000, 0))\n    }\n    println(len(nums))\n}\n";
    let path = write_temp(src);
    let out = Command::new(env!("CARGO_BIN_EXE_inga")).arg("run").arg(&path)
        .env("MallocScribble", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "fork-in-arena crashed: {}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("kicked 7-14"), "got: {stdout}");
    assert!(stdout.contains("245000350000"), "got: {stdout}");
    assert!(stdout.contains("\n3\n"), "got: {stdout}");
}
