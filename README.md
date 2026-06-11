# Inga

**Typed errors. Inferred dependencies. Direct style.**
Inga is what Effect.ts would look like as its own language, with Koka's
direct style: you write ordinary code, and the compiler infers — and
enforces — what can fail (`!` row) and what it needs (`uses` row). Data is
structs and enums; `fail` raises *any* value, and the `!` row names the
types of the values a function can fail with.

```inga
use std/schedule

struct UserNotFound = { Int id }
struct DbError      = { String cause }
struct CacheMiss    = { String key }

// Fully annotated — but every annotation here is optional and inferred:
getUserById :: (Int id) -> User ! UserNotFound uses Database, Cache, Logger {
    match cached(id) {
        Some(user) -> user
        None       -> fetchAndCache(id)
    }
}

fetchAndCache :: (id) {
    Database db          // ← binds the capability, infers `uses Database`
    Cache cache
    Logger logger

    user = db.findUser(id)
        |> retry(schedule.exponential(100.millis) |> schedule.upTo(3))
        |> orFail(UserNotFound(id))
        |> catch {
            DbError(cause) -> {
                logger.warn("db down after retries: ${cause}")
                fail UserNotFound(id)
            }
        }

    cache.set("user:${id}", encode(user), 5.minutes) |> ignoreFailure
    logger.info("cache refreshed for ${id}")
    user
}
```

Hover `fetchAndCache` in an editor and the language server shows what the
compiler inferred:

```
fetchAndCache :: (Int id) -> User ! UserNotFound uses Cache, Database, Logger
```

`main` must have empty rows — every error caught, every service provided —
so a program that compiles cannot hit an unhandled typed error or a missing
dependency:

```inga
main :: () {
    provide consoleLogger, memoryCache, fakeDb {
        user = getUserById(42) |> catch { UserNotFound -> User(0, "anonymous", "n/a") }
        println("fetched: ${user.name}")
    }
}
```

## Try it

```sh
cargo run -p inga-cli -- run examples/user_service.inga
```

```
[info] cache refreshed for 42
fetched: Wing <wing@anara.com>
cached:  Wing
fallback for user 7: anonymous
```

The flaky fake database refuses the first two connections — `retry` recovers
— and the second lookup hits the cache. Delete the `catch` in `main` and the
compiler answers:

```
error: `main` does not handle the error `UserNotFound`; add a `catch` for it
```

## The toolchain

One binary, everything included:

| Command | What it does |
|---|---|
| `inga run file.inga` | type-check, compile, and run (a temp native binary) |
| `inga build file.inga [-o out]` | **compile to a native binary** (LLVM IR → clang -O2) |
| `inga check files...` | diagnostics with source carets |
| `inga test [files...]` | run `test*` functions; `assert`/`assertEq` failures point at the line |
| `inga fmt [--check] files...` | canonical formatter (idempotent, keeps comments) |
| `inga highlight file.inga` | ANSI syntax highlighting in the terminal |
| `inga lsp` | language server: hover with inferred `!`/`uses` rows, diagnostics, go-to-definition, completion, formatting, semantic tokens |

Editor support lives in [`editors/vscode`](editors/vscode) (TextMate grammar
+ LSP client).

## Concurrency without a manual

`spawn(action)` runs the action on its own OS thread; `await(task)` takes
the result. The effect system is the whole safety story — and it does the
bookkeeping for you:

```inga
a = crunch("medium", 100000) |> spawn   // crunch :: ... -> Report ! TooBig uses Adder
b = crunch("large", 10000000) |> spawn  // both in flight; ~Nx on N cores, zero locks
println(await(a) |> catch { TooBig -> fallback })
println((await(b) |> catch { TooBig -> fallback }).total)
```

The action's errors travel in the task's type (`Task<Report ! TooBig>`) and
**re-raise at the `await`** — catch them where you collect the result, or
the compiler reminds you the same way it guards `main`. The action can use
services in scope too, as long as every implementation is shareable across
threads (scalar-only state); anything stateful gets a checker error telling
you to provide a fresh one inside the spawn. Each task gets its own heap
(allocation stays lock-free); captured values are frozen before the thread
starts so refcounts never race. Try it:
`inga build examples/tasks.inga -o tasks && ./tasks`.

## Tests are built in

```sh
inga test games/logic_test.inga
```

Every zero-parameter `test*` function is a test; `assert(cond)` and
`assertEq(actual, expected)` are ordinary typed errors (`! AssertFailed`),
so a failing assertion prints with the usual caret pointing at the line.
INGA-LATRO's poker evaluator is tested this way.

## Graphics, and a game

Inga has GL-backed graphics bindings — the `std/graphics` module (window, rects,
circles, text, mouse, **GLSL fragment shaders**), implemented on OpenGL via
miniquad/macroquad in the native runtime and available in both backends. The
frame loop is owned by the runtime (`graphics.run(w, h, title, frame)` calls your
closure once per frame), so games don't need unbounded recursion — and the
frame closure captures capability evidence like any other Inga closure.

The proof is [`games/balatro.inga`](games/balatro.inga): **INGA-LATRO**, a
Balatro-style roguelike deckbuilder in ~1,000 lines of pure Inga, split
across six modules (`use game`, `util`, `cards`, `jokers`, `poker`, `state`) —
poker-hand scoring, escalating blinds and antes, fifteen jokers and a
rerollable shop, animated card deals/hovers/score popups, and the signature
swirling paint background written as a GLSL shader *inside the Inga source*
and compiled at runtime via `graphics.shaderNew`. All game state lives in
one `Game` service ([`games/game.inga`](games/game.inga)) provided once in
`main` — every function just says `uses Game` (inferred), so nothing
threads state through arguments, and the
[`inga test` tests](games/logic_test.inga) provide their own fresh instance
per test. The frame closure opens with `provide Arena(256.kb)`, so
everything built while drawing is freed wholesale at frame end — the render
path does no refcount work:

```sh
inga build games/balatro.inga -o ingalatro && ./ingalatro
```

![INGA-LATRO](games/screenshot.png)

## Repository layout

```
crates/inga-core      lexer, parser, type & effect inference, formatter
crates/inga-codegen   LLVM backend (emits .ll; clang compiles and links)
crates/inga-rt        native runtime staticlib (allocator, strings, maps, clock)
crates/inga-cli       the `inga` binary
crates/inga-lsp       language server (lsp-server / lsp-types)
editors/vscode        VS Code extension + TextMate grammar
editors/zed           Zed extension (tree-sitter highlighting + LSP)
tree-sitter-inga      tree-sitter grammar (used by the Zed extension)
examples/             hello.inga, retry.inga, shapes.inga, arena.inga, tasks.inga, modules.inga (+ geometry.inga), user_service.inga
games/                balatro.inga (+ game, util, cards, jokers, poker, state, logic_test) — a Balatro-style deckbuilder
bench/                the same workloads in Inga, JavaScript, and Rust (see bench/README.md)
docs/SPEC.md          language design: semantics, effect rows, execution strategy
```

`bench/run.sh` runs five identical workloads as Inga, node, and `rustc -O`
— Inga wins every one against V8 ([results](bench/README.md)).

## How it runs

Inga is compiled, always: `inga build` produces a binary through LLVM, and
`inga run` is the same pipeline to a temp binary, executed immediately —
one backend, one semantics. Because Inga's effects are static, **the effect
system compiles away**: error rows become Rust-style `{value, err}` two-register returns,
capability rows become Koka-style evidence parameters, and a capability
method call is the same machine code as a Rust `dyn` call. Memory is
**Perceus-style ARC** (non-atomic refcounts + compiler-emitted drop glue,
small objects recycled through free lists, one heap per thread so `spawn`
never locks) with opt-in region arenas: `provide Arena(256.kb)` makes a
scope allocate from a region freed wholesale at scope end, deep-copying the
scope's result out as it dies. Details in
[docs/SPEC.md §6](docs/SPEC.md#6-execution-how-inga-runs).

The result ([benchmarks](bench/README.md)): **compiled Inga beats Node/V8 on
all five benchmark workloads** — 2–3× on raw calls, DI dispatch, and string
interpolation, ~290× on typed-error control flow — and beats idiomatic Rust
on two of them.

## Status

v0.3 — a complete, tested vertical slice: language (structs/enums/tuples,
record update, generics, exhaustive `match`, typed errors over any value),
inference, a **native-only LLVM backend** (`show`/`==`/`encode`/`decode`/
functions-as-values/tasks all compile; the reference interpreter served its
purpose and was removed), Perceus-style ARC + arenas with copy-out,
`spawn`/`await` tasks, a built-in test runner, formatter, LSP, editor
tooling (`cargo test` covers all of it). Not yet: a package manager,
per-implementation capability precision, resumable handlers.
