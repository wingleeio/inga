# The Inga Language

*Version 0.1 — design and reference*

Inga is what you get if the ideas of **Effect.ts** — typed errors, dependency
injection through the type system, declarative retry/schedule policies —
were a language of their own instead of a library, with **Koka**'s direct
style instead of wrapper values. You write ordinary code; the compiler tracks
what can fail and what it needs.

```inga
error UserNotFound = { Int id }

getUserById :: (Int id) -> User ! UserNotFound uses Database, Cache, Logger {
    match cached(id) {
        Some(user) -> user
        None       -> fetchAndCache(id)
    }
}
```

Every function has three inferred facets beyond its value type:

| Facet | Syntax | Effect.ts analogue |
|---|---|---|
| return type | `-> User` | `Effect<A, _, _>` success channel |
| error row | `! UserNotFound, DbError` | `Effect<_, E, _>` error channel |
| capability row | `uses Database, Cache` | `Effect<_, _, R>` requirements |

All three are **inferred** and may be **annotated**; an annotation is a
contract the compiler verifies (it is an error to fail with an undeclared
error or use an undeclared capability). `main` must have *empty* rows: every
error handled, every capability provided. That single rule is the whole
safety story — a program that compiles cannot crash with an unhandled typed
error or reach for a missing dependency.

## 1. Why direct style (the Koka influence)

Effect.ts programs build lazy `Effect` values and `pipe` them through
combinators because TypeScript can't track effects natively. A language can.
In Inga, `db.findUser(id)` *runs* — and its error and capability rows flow
into the enclosing function's inferred rows, the way Koka's effect types do.
Two conveniences keep the Effect.ts feel:

- **`|>` pipe** — `x |> f(a)` is `f(x, a)`; `x |> f` is `f(x)`. Pipelines
  read top-to-bottom like Effect pipelines, and a newline before `|>`
  continues the expression.
- **By-name combinators** — `retry`, `ignoreFailure` take their first
  argument *unevaluated* (re-evaluated per attempt / failure-swallowed), so
  `db.findUser(id) |> retry(schedule)` works without wrapping a lambda. User
  functions opt in with `lazy` parameters: `pick :: (Bool c, lazy Int a, lazy Int b)`.

`catch` is syntax, not a function: it intercepts the error channel of the
expression to its left and *subtracts* the handled error names from the row.

## 2. Declarations

```inga
error CacheMiss = { String key }            // an error type (fields optional, types optional)
type  User      = { Int id, String name }   // a record type
service Cache {                              // a capability interface
    get :: (String key) -> String ! CacheMiss
    set :: (String key, String value, Duration ttl)
}
memoryCache :: Cache {                       // an implementation
    store = MutMap()                         //   instance state, evaluated per `provide`
    get :: (key) { store.get(key) |> orFail(CacheMiss(key)) }
    set :: (key, value, ttl) { store.set(key, value) }
}
fetchAndCache :: (id) { ... }                // a function
```

- Type-before-name everywhere: `(String id)` in parameters, `{ Int id }` in
  fields, `Cache cache` for capability bindings. Omitted types are inferred.
- `Name?` is an option type, `[Name]` a list type.
- Constructors are positional in field order: `User(42, "Wing")`,
  `UserNotFound(id)`. A bare type name is a *type tag* for `decode(raw, User)`.

## 3. Errors

`fail UserNotFound(id)` raises; the error becomes part of the function's
inferred `!` row. Handling is pattern-shaped:

```inga
expr |> catch {
    CacheMiss      -> None                       // by error name
    DecodeError(e) -> { logger.warn(e.message); None }  // bind the whole error
    UserNotFound { id } -> retryUser(id)         // destructure fields
    other          -> fallback(other)            // catch-all (clears the row)
}
```

A `catch` arm for an error the expression cannot raise is an
*unreachable-arm warning*. Helpers: `orFail(option, err)` unwraps or fails;
`ignoreFailure(action)` swallows the error channel and returns `Unit`;
`retry(action, schedule)` re-runs the action per a `Schedule`
(`Schedule.exponential(100.millis) |> upTo(3)`, `Schedule.fixed(...)`).
`retry` deliberately does **not** clear the row — a retried action can still
fail.

## 4. Dependencies (capabilities)

A `service` is an interface; naming one in statement position binds it from
the environment and adds it to the `uses` row:

```inga
cached :: (id) {
    Cache cache          // ← acquires the capability, infers `uses Cache`
    Logger logger
    cache.get("user:${id}") |> ...
}
```

`provide impl1, impl2 { body }` instantiates the implementations (running
their field initializers — each `provide` gets fresh instances) and satisfies
those services for the dynamic extent of the body, *subtracting* them from
the body's `uses` row. Capabilities compose transitively: callers of `cached`
inherit `uses Cache, Logger` without writing anything.

This is Effect.ts `Layer`/`Context` reduced to two keywords. There are no
globals and no implicit singletons; tests provide fakes the same way `main`
provides real implementations.

## 5. Type system

- **Value types**: unification-based inference (Hindley–Milner machinery),
  whole-program, monomorphic user functions in v0.1; builtins and
  constructors instantiate polymorphically. Primitives: `Int`, `Float`,
  `Bool`, `String`, `Unit`, `Duration`, `Schedule`; composites: `T?`, `[T]`,
  records (`type`), errors, services, functions, `MutMap`.
- **Effect rows**: finite sets of error/service *names*, computed as a
  monotone fixpoint over the call graph. `catch` and `provide` subtract;
  calls union. Service method rows are the union of all implementations'
  inferred rows plus the interface's declared `!` annotations (per-impl
  precision is future work). Higher-order calls conservatively assume a
  function-typed argument may be invoked.
- **Annotations are contracts**: inferred ⊆ declared is enforced; the
  declared row is what callers see.
- Field access on an unannotated value uses *unique-field inference*: if
  exactly one declared type has the field, the receiver unifies with it.

## 6. Execution: how Inga runs

Inga has two execution modes sharing one front end (lexer → parser → type +
effect inference):

**`inga run` — the reference interpreter.** A tree-walking interpreter in
Rust: errors are a `Result` channel, capabilities a dynamically scoped stack
of provided instances, `retry`/`lazy` re-evaluate thunked AST. It covers the
whole language and is the semantics of record.

**`inga build` — the LLVM backend (v0.2).** `crates/inga-codegen` lowers the
checked AST to textual LLVM IR; `clang -O2` (which embeds LLVM — no other
toolchain dependency) compiles and links it against a small Rust runtime
staticlib (`crates/inga-rt`: bump allocator, strings, hash map, clock).
Because Inga's effects are deliberately *static* — error rows and capability
rows are name-sets known at compile time — **the effect system compiles
away**:

- **Values are native.** Every value is one `i64`; `Int`/`Bool`/`Duration`
  are raw machine integers — no boxing, no tags, because types are static.
- **Errors are return values.** A function with a non-empty `!` row returns
  `{ i64 value, i64 err }` in two registers (Rust's `Result` shape). `fail`
  is a store + branch; `catch` compares an error type id. Functions with
  empty rows pay nothing — the checker proved they can't fail.
- **Capabilities are evidence** (Koka's strategy). A `uses` row becomes
  hidden leading parameters, one instance pointer per service; `provide`
  allocates `{ method fn-ptrs..., fields... }`; `Cache cache` is just a
  parameter reference; method calls are indirect calls — the same machine
  code as Rust `dyn` dispatch.
- The optimizer exploits staticness: `len("n=${n}")` folds to
  `2 + digits(n)` with no string materialized; `map.get(k) |> getOrElse(d)`
  fuses into a direct probe with no `Option` box.

Measured result (bench/README.md): the compiled benchmarks beat Node/V8 on
all five workloads — about 2× on calls, dispatch, and strings, ~860× on
typed-error control flow — and run ~300× faster than the interpreter.

**Current limits of the backend:** `encode`/`decode` (runtime JSON) and
showing structs still require the interpreter, and compiled programs use a
bump allocator that never frees (no GC yet — acceptable for short-lived
processes, on the roadmap). Full delimited-continuation effects (resumable
handlers, generators, async) remain out of scope; handlers are syntax
(`catch`, `provide`) rather than first-class values precisely so that
evidence passing stays sufficient.

## 7. Packages (future)

Out of scope for v0.1 (programs are single files), but the design intent,
so syntax doesn't paint us into a corner:

- A manifest (`inga.toml`) with content-addressed, lockfile-pinned deps.
- Modules are files; `use http/client` style imports; capabilities make
  library APIs honest — a package that needs the network *says so in its
  types* (`uses Http`), and the application root decides what to provide.
  Dependency injection and package dependencies are the same idea at two
  scales.

## 8. Tooling (all in this repo)

| Tool | Where | Notes |
|---|---|---|
| `inga run / check` | `crates/inga-cli` | caret diagnostics, warnings |
| `inga fmt` | `crates/inga-core/src/fmt.rs` | canonical style, idempotent, comment-preserving, `--check` mode |
| `inga highlight` | `crates/inga-cli` | ANSI terminal highlighting from the real lexer (lossless) |
| `inga lsp` | `crates/inga-lsp` | diagnostics, hover (inferred signatures with rows), go-to-definition, completion, formatting, semantic tokens |
| VS Code extension | `editors/vscode` | TextMate grammar + LSP client |

## 9. Grammar sketch

```
program   := decl*
decl      := 'error' Upper '=' '{' field,* '}'
           | 'type'  Upper '=' '{' field,* '}'
           | 'service' Upper '{' (name '::' sig)* '}'
           | name '::' Upper '{' (name '=' expr | name '::' sig block)* '}'   -- impl
           | name '::' sig block                                              -- func
sig       := '(' param,* ')' ('->' type)? ('!' Upper,+)? ('uses' Upper,+)?
param     := 'lazy'? type? name
type      := Upper | lower | '[' type ']' | type '?'
field     := type? name
block     := '{' stmt* '}'
stmt      := Upper name                      -- capability bind
           | type? name '=' expr             -- binding
           | expr
expr      := pipe; pipe := or ('|>' (call | 'catch' arms))*
           | match | if | fail | provide | lambda | literals…
arms      := '{' (pattern '->' expr)+ '}'
pattern   := '_' | name | literal | Upper ('(' pattern,* ')' | '{' name,* '}')?
```

Statements end at newlines; expressions continue across a newline before
`|>`, binary operators (except `-` and `!`), and `.` chains. String literals
interpolate with `${expr}`. Comments are `//` and `/* */`.
