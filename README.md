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
| `inga run file.inga` | type-check + run (tree-walking interpreter) |
| `inga check files...` | diagnostics with source carets |
| `inga fmt [--check] files...` | canonical formatter (idempotent, keeps comments) |
| `inga highlight file.inga` | ANSI syntax highlighting in the terminal |
| `inga lsp` | language server: hover with inferred `!`/`uses` rows, diagnostics, go-to-definition, completion, formatting, semantic tokens |

Editor support lives in [`editors/vscode`](editors/vscode) (TextMate grammar
+ LSP client).

## Repository layout

```
crates/inga-core   lexer, parser, type & effect inference, interpreter, formatter
crates/inga-cli    the `inga` binary
crates/inga-lsp    language server (lsp-server / lsp-types)
editors/vscode     VS Code extension + TextMate grammar
examples/          hello.inga, retry.inga, user_service.inga
docs/SPEC.md       language design: semantics, effect rows, execution strategy
```

## How it runs

v0.1 interprets the typed AST. Because Inga's effects are static — error and
capability rows are compile-time name-sets — the compilation story is
conventional: errors lower to tagged-union returns, capabilities to
Koka-style evidence passing. The planned backend ladder is bytecode VM →
Cranelift native code (LLVM only if release-grade optimization ever warrants
the toolchain cost). The reasoning is laid out in
[docs/SPEC.md §6](docs/SPEC.md#6-execution-how-inga-runs).

## Status

v0.1 — a complete, tested vertical slice: language, inference, runtime,
formatter, LSP, editor tooling (`cargo test` covers all of it). Not yet:
modules/packages, per-implementation capability precision, resumable
handlers, native codegen.
