# The Inga Language

*Version 0.3 — design and reference*

Inga is what you get if the ideas of **Effect.ts** — typed errors, dependency
injection through the type system, declarative retry/schedule policies —
were a language of their own instead of a library, with **Koka**'s direct
style instead of wrapper values. You write ordinary code; the compiler tracks
what can fail and what it needs.

```inga
struct UserNotFound = { Int id }

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
| error row | `! UserNotFound, String` | `Effect<_, E, _>` error channel |
| capability row | `uses Database, Cache` | `Effect<_, _, R>` requirements |

All three are **inferred** and may be **annotated**; an annotation is a
contract the compiler verifies (it is an error to fail with an undeclared
type or use an undeclared capability). `main` must have *empty* rows: every
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
struct CacheMiss = { String key }            // a struct (fields optional, types optional)
struct User      = { Int id, String name }
enum   Shape     = Circle { Float radius }   // a sum type: variants separated by `|`,
       | Rect { Float w, Float h } | Dot     //   each with optional struct-style fields
service Cache {                              // a capability interface
                                             //   (`shared service` additionally allows
                                             //    instances to cross fibers — §6.5)
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
  fields, `Cache cache` for capability bindings, `String msg` in patterns.
  Omitted types are inferred.
- `Name?` is an option type, `[Name]` a list type, `(Int, String)` a
  tuple type, `(Int) -> Bool` a function type (for callbacks), and the two
  builtin generic types are written the way hover renders them:
  `MutMap<String, Int>`, `Fiber<Int ! Boom>`, `Outcome<a ! E>`. A plain
  arrow type is a *pure* contract;
  `(Int) -> User ! DbError uses Logger` accepts effectful callbacks, and a
  function with effects the annotation doesn't declare is rejected. An
  unannotated callback parameter is simply inferred.
- **Lowercase type names in a signature are type parameters** (universals):
  `first :: ([a] xs) -> a?` works for every element type. They are
  instantiated fresh at each call site and rigid in the body — code that
  would constrain `a` (arithmetic, comparison) is rejected, which is what
  makes the signature a real promise.
- Constructors are positional in field order: `User(42, "Wing")`,
  `Circle(2.0)`; a fieldless variant is a value (`Dot`). A bare struct name
  is a *type tag* for `decode(raw, User)`.
- Tuples are positional: `t = (1, "one")`, `t.0`, and `(n, s)` in patterns.
  **Record update** copies a struct with overrides:
  `User { ..u, name: "new" }`.
- `match` over an enum, `Bool`, or option must be **exhaustive**: cover
  every variant (or both literals / `Some` and `None`) or end with a
  catch-all arm.

## 3. Errors

`fail` raises **any value** — a struct, an enum, or a primitive — and the
*type* of the failed value joins the function's inferred `!` row:
`fail UserNotFound(id)` adds `UserNotFound`; `fail "bad input"` adds
`String`. Handling is pattern-shaped, matching the failed value itself:

```inga
expr |> catch {
    CacheMiss        -> None                     // by struct name
    DecodeError e    -> { logger.warn(e.message); None } // typed-bind: binds the whole value
    UserNotFound { id } -> retryUser(id)         // destructure named fields
    DbError(cause)   -> retryLater(cause)        // destructure positionally
    String reason    -> { logger.warn(reason); None }    // failed primitives, by type
    404              -> None                     // literals match one value (row keeps `Int`)
    other            -> fallback(other)          // catch-all (clears the row)
}
```

**Naming convention:** error types end in `Error` — `HttpError`,
`DecodeError`, `TimeoutError`, `AssertionError`, `InterruptedError`; name
yours the same way (`PageGoneError`, not `PageGone`).

An arm clears its type from the row when it matches *every* value of that
type: struct arms and typed-bind arms subtract their tag; literal arms never
subtract; enum **variant** arms subtract the enum only once all variants are
covered — partially-caught enums stay in the row. A `catch` arm for a type
the expression cannot fail with is an *unreachable-arm warning*. Helpers:
`orFail(option, err)` unwraps or fails;
`ignoreFailure(action)` swallows the error channel and returns `Unit`;
`retry(action, schedule)` re-runs the action per a `Schedule`
(`schedule.exponential(100.millis) |> schedule.upTo(3)`, `schedule.fixed(...)` — from
`use std/schedule`).
`retry` deliberately does **not** clear the row — a retried action can still
fail. `tap(value, f)` runs a side effect on the value mid-pipe and passes it
along untouched; `tapError(action, f)` runs a side effect on a *failure* and
re-raises it — both are observation points (logging, metrics), neither
transforms nor clears anything. `then(value, f)` is the transforming
sibling — `x |> then((u) -> u.name)` maps the value itself mid-pipe
(`map` is for the *elements* of a list), with `f`'s rows merging like any
call. `assert(cond)` and `assertEq(actual, expected)` fail with the builtin
struct `AssertionError { message }` — ordinary typed errors, catchable
anywhere, and the backbone of `inga test` (§9).

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

`provide` instantiates implementations (running their field initializers —
each `provide` gets fresh instances) and satisfies those services for a
dynamic extent, *subtracting* them from that extent's `uses` row. It has
two forms:

```inga
main :: () {
    provide prettyLogger, db        // braceless: the rest of this block
    Db handle
    ...
    provide fakeDb { runTests() }   // braced: just this body
}
```

Items provide **left to right**: a later implementation's field
initializers run with the earlier services already available, so an impl
whose setup logs can be written `provide prettyLogger, db`. An item may
also be a configured builtin resource — `provide Arena(256.kb)` switches
the scope's allocator (see §6). Capabilities compose transitively: callers
of `cached` inherit `uses Cache, Logger` without writing anything.

This is Effect.ts `Layer`/`Context` reduced to two keywords. There are no
globals and no implicit singletons; tests provide fakes the same way `main`
provides real implementations.

## 5. Type system

- **Value types**: unification-based inference (Hindley–Milner machinery),
  whole-program, monomorphic user functions in v0.1; builtins and
  constructors instantiate polymorphically. Primitives: `Int`, `Float`,
  `Bool`, `String`, `Unit`, `Duration`, `Schedule`; composites: `T?`, `[T]`,
  structs, enums, services, functions, `MutMap`.
- **Effect rows**: finite sets of type/service *names*, computed as a
  monotone fixpoint over the call graph. `catch` and `provide` subtract;
  calls union. Service method rows are the union of all implementations'
  inferred rows plus the interface's declared `!` annotations (per-impl
  precision is future work). Higher-order calls conservatively assume a
  function-typed argument may be invoked.
- **Annotations are contracts**: inferred ⊆ declared is enforced; the
  declared row is what callers see.
- Field access on an unannotated value uses *unique-field inference*: if
  exactly one declared type has the field, the receiver unifies with it.
  Calling `get`/`set`/`delete`/`size` on an otherwise-unconstrained value
  defaults the receiver to `MutMap` (they are map vocabulary).
- **Functions are values**: a top-level function passed where a callback is
  expected (`map(xs, double)`) closes over its evidence like a lambda.
- **Builtins** (a deliberate, small prelude — no imports needed):
  `println print show encode decode len map filter fold at concat reverse
  range` (lists), `split slice indexOf trim parseInt toFloat floor`
  (strings/numbers), `getOrElse orFail` (options), `retry ignoreFailure
  tap tapError then sleep` (effects), `assert assertEq` (tests),
  `MutMap Some env nowMillis nowMicros random`. Concurrency is **not** in the
  prelude — it lives in `std/fiber` (§6.5). Editors show each builtin's
  signature on hover.

## 6. Execution: how Inga runs

Inga is **compiled, always**. `crates/inga-codegen` lowers the checked AST
to textual LLVM IR; `clang -O2` (which embeds LLVM — no other toolchain
dependency) compiles and links it against a small Rust runtime staticlib
(`crates/inga-rt`: allocator, strings, maps, tasks, clock, GL). `inga build`
produces a binary; `inga run` is the same pipeline to a temp binary,
executed immediately. There is no interpreter — one backend, one semantics.
Because Inga's effects are deliberately *static* — error rows and capability
rows are name-sets known at compile time — **the effect system compiles
away**:

- **Values are native.** Every value is one `i64`; `Int`/`Bool`/`Duration`
  are raw machine integers — no boxing, no tags, because types are static.
- **Errors are return values.** A function with a non-empty `!` row returns
  `{ i64 value, i64 err }` in two registers (Rust's `Result` shape); `err`
  points to the failed value boxed with its type tag. `fail` is an alloc +
  branch; `catch` compares the tag. Functions with empty rows pay nothing —
  the checker proved they can't fail.
- **Structs are field tuples; enums are tagged boxes.** A struct is
  `{ fields... }` with no header; an enum value is `{ variant_id, fields... }`
  — or a raw variant id when every variant of the enum is fieldless, making
  C-like enums free.
- **Capabilities are evidence** (Koka's strategy). A `uses` row becomes
  hidden leading parameters, one instance pointer per service; `provide`
  allocates `{ method fn-ptrs..., fields... }`; `Cache cache` is just a
  parameter reference; method calls are indirect calls — the same machine
  code as Rust `dyn` dispatch.
- The optimizer exploits staticness: `len("n=${n}")` folds to
  `2 + digits(n)` with no string materialized; `map.get(k) |> getOrElse(d)`
  fuses into a direct probe with no `Option` box.
- **Self-tail calls become loops** (a branch back to the function head),
  so the idiomatic accumulator-recursion style is iteration —
  `sumTo(10_000_000)` runs in constant stack.
- **Runtime type descriptors** make data-generic operations native: the
  compiler serializes each type's shape into a compact string; a small
  interpreter in the runtime walks value + descriptor to implement `show`,
  structural `==`, JSON `encode`/`decode`, deep copy (arena copy-out), and
  freezing (task captures) — one mechanism instead of per-type glue.

Measured result (bench/README.md): the compiled benchmarks beat Node/V8 on
all five workloads — 2–3× on calls, dispatch, and strings, ~290× on
typed-error control flow.

**Memory.** Compiled Inga uses Perceus-style ARC plus optional region
arenas:

- Every heap object carries one header word: a **non-atomic refcount**
  (each thread owns its values — see tasks below), or a marker for string
  constants and arena objects (dup/release are no-ops on those). The compiler emits
  type-directed drop glue per struct/enum/list/option, inserts a `dup`
  where a value is stored into something longer-lived, and registers every
  fresh heap value in a per-function pool that is released on every return
  path — allocation-heavy code reclaims memory at function granularity,
  and small dead objects recycle through segregated free lists at bump
  speed.
- `provide Arena(256.kb)` pushes a **region**: allocations in its dynamic
  extent are bump-allocated from the region (overflow chains chunks) and
  freed wholesale when the scope ends, failures included. The scope's
  result is **deep-copied out** past the region as it dies, so an arena
  scope can produce any plain-data value; only results containing function
  values or mutable maps (shared by reference — a copy would dangle) are
  rejected. Error boxes are allocated from the RC heap so a `fail` can
  cross an arena boundary. A per-frame arena in a game loop means the
  whole render path does no refcount work (see `games/balatro.inga`).
- Known leaks, by design (all bounded by program shape, not input size):
  closures and service instances free their record but not their captures;
  MutMap contents; values captured at a fork (frozen, §6.5); results of
  generic functions (uniform representation has no per-instance drop glue).

### 6.5 Concurrency: `std/fiber`

Concurrency lives in a standard module, imported and used like
`std/graphics` — qualified-only, so `fiber.` is the visual marker that
concurrency is happening on a line. The unit is the **fiber**. Two types
are global builtins in the type grammar (writable as hover renders them):
`Fiber<a ! E>` — a running fiber whose error row rides inside the type —
and `Outcome<a ! E>` — a settled result, `Ok(value)` or `Failed(error)`.

```inga
use std/fiber

main :: () {
    provide Runtime(4)                    // the fiber runtime: 4 workers
    a = crunch("medium", 100000) |> fiber.fork
    b = crunch("large", 10000000) |> fiber.fork
    println(fiber.join(a).total + fiber.join(b).total)
}
```

**The `Fibers` capability.** Every scheduling operation carries
`uses Fibers`, satisfied only by the builtin resource `provide Runtime(n)`
(in the same grammar slot as `Arena(256.kb)`; `n` = OS workers). So a
library that parallelizes says so in its types, `main` without a `Runtime`
gets a teaching diagnostic, and a function whose rows lack `Fibers` is
proven non-forking. The semantic promise: *programs behave identically
under any `n ≥ 1` except for speed and the interleaving of observable side
effects* — with one caveat: a fiber that computes without yielding can
starve siblings under small `n` (cooperative scheduling; interruption and
timers are observed at park points).

**Core operations** (`lazy` = by-name, so `expr |> fiber.fork` works):

```
fiber.fork      (lazy action) -> Fiber<a ! E>      uses Fibers   start now, return immediately
fiber.join      structural — see below             uses Fibers   park, re-raise the error channel
fiber.poll      (Fiber<a ! E>) -> a? ! E           uses Fibers   non-blocking probe (frame loops)
fiber.interrupt (Fiber<a ! E>)                     uses Fibers   request cooperative cancellation
fiber.settle    (lazy action) -> Outcome<a ! E>    row-free      the error channel as data
fiber.unsettle  (Outcome<a ! E>) -> a ! E          row-free      put it back in the channel
fiber.par       (lazy a, lazy b, …) -> (a, b, …)   uses Fibers   fork all + join
fiber.parMap    ([a], (a) -> b ! E) -> [b] ! E     uses Fibers   one fiber per element
fiber.race      (lazy a, lazy a) -> a ! Ea, Eb     uses Fibers   first completion wins, loser interrupted
fiber.within    (lazy a, Duration) -> a ! E, TimeoutError             race against a deadline
fiber.partition ([Outcome<a ! E>]) -> ([a], [Outcome<a ! E>])    split successes from failures
```

**Structural `join`.** `join` accepts a fiber, a tuple of fibers, or a
list of fibers, and returns the same shape with the fibers stripped; the
joined error row is the union of the element rows. On failure the **first
error in shape order** wins (deterministic under any `Runtime(n)`): the
remaining fibers in the shape are interrupted and the error re-raises.
`InterruptedError` and `TimeoutError` are fieldless builtin structs — ordinary
catchable errors.

**Error placement** — one sentence to teach: *errors live in the fiber's
type until the join; the join puts them back in the channel; then it's
ordinary `catch`.* Decide per branch at the fork (inline `catch` inside
the forked expression), sweep up the remainder at the join. The joined
row forgets which branch contributed an error, so when branches differ in
recovery policy, handle per branch — or use `settle` when the failure
itself is data you will act on:

```inga
outcomes = urls |> fiber.parMap((u) -> fetch(u) |> fiber.settle)
map(outcomes, (o) -> match o {
    Ok(body)          -> store(body)
    Failed(TimeoutError)   -> queueRetry()
    Failed(HttpError e) -> logger.warn("${e.status}")
})
```

`Failed` arms reuse `catch`'s pattern language (typed binds,
destructuring), and `match` over an `Outcome` is exhaustiveness-checked
against the row in its type. `settle`/`unsettle` carry no `Fibers` row —
they are error-channel operators, useful in sequential code too.

**Capture and sharing.** Captured values are frozen before the fiber
starts (marked static recursively by type descriptor; arena captures are
copied out first); function values, maps, fibers, and outcomes are
rejected as captures. Capability evidence crosses **iff the service is
declared `shared`** — `shared service Adder { … }` is the contract, and
the checker enforces scalar-only instance state (`Int`/`Float`/`Bool`/
`Duration`) at every implementation, so adding a `MutMap` field errors at
the impl, not at a distant fork site. Non-shared services are provided
fresh inside the forked expression.

**Supervision (the no-leak rule).** A fiber whose handle is dropped —
including when its forking function returns without joining — is
interrupted: handles are ordinary RC values whose drop glue is the
abandon. Returning or storing the handle keeps it alive, like any value.
There is no daemon escape hatch; if one is ever needed it arrives as a
capability.

**Phase 1 implementation note.** The runtime currently backs each fiber
with one OS thread (the worker count of `Runtime(n)` is honored when the
M:N scheduler lands, co-designed with the IO layer); the API and every
rule above are final, and the §6.5 promise means programs written today
keep their meaning then. `sleep` and `retry` backoff keep **empty rows**:
they park when a scheduler can park them and block a worker otherwise —
an unobservable difference, by design. Deferred to the scheduler phase:
`fiber.recover` (its fallback-thunk execution context needs the scheduler;
catch-inside-fork covers it), fiber-heap page recycling (fiber results
currently leak one reference, like the other documented leaks), and
preemption points in non-`Fibers` code.

**Current limits of the backend:** full delimited-continuation effects
(resumable handlers, generators, async) remain out of scope; handlers are
syntax (`catch`, `provide`) rather than first-class values precisely so
that evidence passing stays sufficient.

## 7. Modules

Modules are files; paths are folder-aware and relative to the importing
file. A plain `use` binds the path's last segment as a **qualified
alias**; `use m { a, b }` imports only the listed `pub` names,
unqualified — imports never dump a module's whole namespace into yours:

```inga
// geometry.inga
pub enum Shape = Circle { Float radius } | Dot
pub area :: (Shape s) -> Float { ... }
tau :: () -> Float { 6.28318 }            // private

// main.inga
use std/graphics                          // std library: graphics.rect(...)
use geometry                              // qualified: geometry.area(...)
use geometry { Shape, area }              // or unqualified, these names only
main :: () { println(area(Circle(2.0))) }
```

- `pub` may prefix any top-level declaration (struct, enum, service,
  implementation, function). Private cross-module references are errors;
  a bare name reachable only qualified gets a hint
  (``call it as `geometry.area` or import it with `use geometry { area }` ``).
- Importing an enum name also grants its variants (`Shape` brings
  `Circle`/`Dot`).
- Imports are not re-exported; diamonds and cycles load once. Top-level
  names are program-unique (whole-program compilation, monomorphic v0.x).
- **The standard library lives under `std/`** and is imported like any
  module, but qualified-only: `use std/graphics` (GL bindings, §8) and
  `use std/schedule` (retry schedules). Module names are lowercase —
  uppercase identifiers are types; `graphics.rect(...)` can never be
  mistaken for a constructor or a service.
- Internally, every module lexes at a disjoint base offset into one global
  span space, so inference, the LLVM backend, and
  diagnostics (mapped back to file + line) all operate on one merged
  program.

Future packages keep this shape: a manifest (`inga.toml`) with
content-addressed, lockfile-pinned deps; capabilities make library APIs
honest — a package that needs the network *says so in its types*
(`uses Http`), and the application root decides what to provide.

## 8. Graphics bindings

The `std/graphics` module (imported with `use std/graphics`) provides
GL-backed 2D graphics, implemented on OpenGL through miniquad/macroquad in
the native runtime:

```
graphics.run(width, height, title, frame)   // runtime-owned loop; frame: () -> a, once per frame
graphics.clear(r, g, b)                     // 0–255 channels everywhere
graphics.rect(x, y, w, h, r, g, b, a)       graphics.rectLines(x, y, w, h, thick, r, g, b, a)
graphics.circle(x, y, radius, r, g, b, a)   graphics.text(s, x, y, size, r, g, b)
graphics.textWidth(s, size) -> Int          graphics.mouseX() / graphics.mouseY() -> Int
graphics.mousePressed() -> Bool
graphics.shaderNew(fragGlsl) -> Int         // compile GLSL ES; uniforms iTime, iRes
graphics.shaderUse(handle)             graphics.shaderOff()
graphics.imageNew(pngBytes) -> Int          // decode image bytes (e.g. an http body); -1 on failure
graphics.image(handle, x, y, w, h)          // draw scaled; nearest-filtered (crisp pixels)
```

Inverting the loop (`graphics.run` calls the closure, rather than the program
recursing) keeps stacks bounded, and the frame closure captures capability
evidence like any closure — services work normally inside frames. Windows
are **resizable for free**: the frame draws in logical coordinates (the
size passed to `run`), rendered to an offscreen target and scaled to the
real window with letterboxing; `mouseX`/`mouseY` map back into logical
space, so hit-testing never changes. Helpers:
`range(n) -> [Int]` and `random(n) -> Int`. Setting `INGA_GFX_SHOT=<path>`
renders 30 frames, writes a PNG of the framebuffer, and exits (CI smoke
tests). See `games/balatro.inga` for a complete game.

## 8.5 HTTP client: `std/http`

```inga
use std/http

main :: () {
    provide Http
    resp = http.get(url) |> catch { HttpError(status, m) -> ... }
    if resp.status == 200 { store(resp.body) }
}
```

Every operation carries `uses Http`, satisfied by `provide Http` — the
network shows up in your rows, which is the package-honesty story made
real. The service is `shared`, so requests cross fiber boundaries:
`http.get(url) |> fiber.within(2.seconds)` and `retry` compose directly
(a request parks one fiber; deadlines and backoff come from the existing
combinators, not from client options).

```
http.get        (String url) -> HttpResponse ! HttpError            uses Http
http.post       (String url, String body) -> HttpResponse ! HttpError
http.send       (method, url, body, [(String, String)] headers) -> HttpResponse ! HttpError
http.openStream (String url) -> HttpStream ! HttpError              GET, streamed body
http.read       (HttpStream) -> String? ! HttpError                 next chunk; None at end
http.close      (HttpStream)
```

`HttpResponse { Int status, String body }` — like fetch, a non-2xx
status is **data**, not a failure; only transport/TLS/connect errors
raise `HttpError { Int status, String message }` (status 0 = transport).
Streaming is pull-based — a tail-recursive loop is the iteration:

```inga
readAll :: (HttpStream s, String acc) -> String ! HttpError uses Http {
    match http.read(s) {
        Some(chunk) -> readAll(s, acc + chunk)
        None -> acc
    }
}
```

`HttpStream { Int handle, Int status }` reports the response status at
open. Bodies decode with the existing `decode(resp.body, User)`. The
client is blocking (rustls underneath) — exactly right for
thread-per-fiber, and the M:N reactor adopts these calls in phase 2.

## 9. Tooling (all in this repo)

| Tool | Where | Notes |
|---|---|---|
| `inga run / build / check` | `crates/inga-cli` | compile via LLVM/clang; caret diagnostics, warnings |
| `inga test` | `crates/inga-cli` | runs every zero-parameter `test*` function; `assert`/`assertEq` failures point at the failing line; exit code for CI |
| `inga fmt` | `crates/inga-core/src/fmt.rs` | canonical style, idempotent, comment-preserving, `--check` mode |
| `inga highlight` | `crates/inga-cli` | ANSI terminal highlighting from the real lexer (lossless) |
| `inga lsp` | `crates/inga-lsp` | diagnostics, hover (inferred signatures with rows), go-to-definition, completion with auto-import (sibling `pub` names and std modules insert/extend the `use` line) and `.`-member completion (module members, struct fields, service methods, map ops, tuple slots, Int suffixes — typed via the checker), arm completion in `catch`/`match` (the caught row's error types and variants; the scrutinee's variants, Some/None, true/false, Ok/Failed), quick fixes on unknown names, formatting, semantic tokens |
| VS Code extension | `editors/vscode` | TextMate grammar + LSP client |

## 10. Grammar sketch

```
program   := decl*
decl      := 'use' name ('/' name)* ('{' name,* '}')?
           | 'pub'? 'struct' Upper '=' '{' field,* '}'
           | 'pub'? 'enum' Upper '=' variant ('|' variant)*
           | 'pub'? 'shared'? 'service' Upper '{' (name '::' sig)* '}'
           | name '::' Upper '{' (name '=' expr | name '::' sig block)* '}'   -- impl
           | name '::' sig block                                              -- func
variant   := Upper ('{' field,* '}')?
sig       := '(' param,* ')' ('->' type)? ('!' Upper,+)? ('uses' Upper,+)?
param     := 'lazy'? type? name
type      := Upper | lower | '[' type ']' | type '?' | '(' type,+ ')'   -- paren / tuple
           | Upper '<' type,+ ('!' Upper,+)? '>'   -- MutMap<K, V> / Fiber<T ! E> / Outcome<T ! E>
           | '(' type,* ')' '->' type ('!' Upper,+)? ('uses' Upper,+)?  -- function type
field     := type? name
block     := '{' stmt* '}'
stmt      := Upper name                      -- capability bind
           | type? name '=' expr             -- binding
           | 'provide' item,*                -- braceless: scopes over the rest of the block
           | expr
item      := name ('(' expr,* ')')?          -- impl, or a resource like Arena(256.kb)
expr      := pipe; pipe := or ('|>' (call | 'catch' arms))*
           | match | if | fail | provide | lambda
           | '(' expr ',' expr,* ')'            -- tuple ('.0' indexes)
           | Upper '{' '..' expr (',' name ':' expr)* '}'   -- record update
           | literals…
arms      := '{' (pattern '->' expr)+ '}'
pattern   := '_' | name | literal | Upper name              -- typed bind
           | '(' pattern,* ')'                              -- tuple
           | Upper ('(' pattern,* ')' | '{' name,* '}')?
```

Statements end at newlines; expressions continue across a newline before
`|>`, binary operators (except `-` and `!`), and `.` chains. String literals
interpolate with `${expr}`; `"""` opens a **multiline string** (raw quotes
and newlines, interpolation and escapes still active) with Swift-style
dedent — the indentation of the closing `"""` is stripped from every line,
and a newline right after the opener is dropped. Comments are `//` and
`/* */`.
