# Inga

**Typed errors. Inferred dependencies. Direct style.**
Inga is what Effect.ts would look like as its own language, with Koka's
direct style: you write ordinary code, and the compiler infers — and
enforces — what can fail (`!` row) and what it needs (`uses` row).

```inga
error UserNotFound = { Int id }
error DbError      = { String cause }
error CacheMiss    = { String key }

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
        |> retry(Schedule.exponential(100.millis) |> upTo(3))
        |> orFail(UserNotFound(id))
        |> catch {
            DbError(e) -> {
                logger.warn("db down after retries: ${e.cause}")
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
| `inga run file.inga` | type-check + run (reference interpreter, full language) |
| `inga build file.inga [-o out]` | **compile to a native binary** (LLVM IR → clang -O2) |
| `inga check files...` | diagnostics with source carets |
| `inga fmt [--check] files...` | canonical formatter (idempotent, keeps comments) |
| `inga highlight file.inga` | ANSI syntax highlighting in the terminal |
| `inga lsp` | language server: hover with inferred `!`/`uses` rows, diagnostics, go-to-definition, completion, formatting, semantic tokens |

Editor support lives in [`editors/vscode`](editors/vscode) (TextMate grammar
+ LSP client).

## Graphics, and a game

Inga has GL-backed graphics bindings — the `Gfx` module (window, rects,
circles, text, mouse), implemented on OpenGL via miniquad/macroquad in the
native runtime and available in both backends. The frame loop is owned by the
runtime (`Gfx.run(w, h, title, frame)` calls your closure once per frame), so
games don't need unbounded recursion — and the frame closure captures
capability evidence like any other Inga closure.

The proof is [`games/balatro.inga`](games/balatro.inga): **INGA-LATRO**, a
Balatro-style roguelike deckbuilder in ~600 lines of pure Inga — poker-hand
scoring, escalating blinds and antes, money, and a joker shop:

```sh
inga build games/balatro.inga -o ingalatro && ./ingalatro
```

![INGA-LATRO](games/screenshot.png)

## Repository layout

```
crates/inga-core      lexer, parser, type & effect inference, interpreter, formatter
crates/inga-codegen   LLVM backend (emits .ll; clang compiles and links)
crates/inga-rt        native runtime staticlib (allocator, strings, maps, clock)
crates/inga-cli       the `inga` binary
crates/inga-lsp       language server (lsp-server / lsp-types)
editors/vscode        VS Code extension + TextMate grammar
examples/             hello.inga, retry.inga, user_service.inga
games/                balatro.inga — a Balatro-style deckbuilder on the Gfx module
bench/                the same workloads in Inga, JavaScript, and Rust (see bench/README.md)
docs/SPEC.md          language design: semantics, effect rows, execution strategy
```

`bench/run.sh` runs five identical workloads as native Inga, interpreted
Inga, node, and `rustc -O` — compiled Inga wins every one against V8
([results](bench/README.md)).

## How it runs

Two backends, one front end. `inga run` interprets the typed AST (reference
semantics, full language). `inga build` compiles to native code through LLVM
— and because Inga's effects are static, **the effect system compiles
away**: error rows become Rust-style `{value, err}` two-register returns,
capability rows become Koka-style evidence parameters, and a capability
method call is the same machine code as a Rust `dyn` call. Details in
[docs/SPEC.md §6](docs/SPEC.md#6-execution-how-inga-runs).

The result ([benchmarks](bench/README.md)): **compiled Inga beats Node/V8 on
all five benchmark workloads** — ~2× on raw calls, DI dispatch, and string
interpolation, ~860× on typed-error control flow — and beats idiomatic Rust
on two of them.

## Status

v0.2 — a complete, tested vertical slice: language, inference, interpreter,
**native LLVM backend**, formatter, LSP, editor tooling (`cargo test` covers
all of it). Not yet: modules/packages, GC for compiled programs (bump
allocator today), `encode`/`decode` in compiled mode, per-implementation
capability precision, resumable handlers.
