# The Inga Language

*Version 0.1 — design and reference*

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
- `Name?` is an option type, `[Name]` a list type.
- Constructors are positional in field order: `User(42, "Wing")`,
  `Circle(2.0)`; a fieldless variant is a value (`Dot`). A bare struct name
  is a *type tag* for `decode(raw, User)`.

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

An arm clears its type from the row when it matches *every* value of that
type: struct arms and typed-bind arms subtract their tag; literal arms never
subtract; enum **variant** arms subtract the enum only once all variants are
covered — partially-caught enums stay in the row. A `catch` arm for a type
the expression cannot fail with is an *unreachable-arm warning*. Helpers:
`orFail(option, err)` unwraps or fails;
`ignoreFailure(action)` swallows the error channel and returns `Unit`;
`retry(action, schedule)` re-runs the action per a `Schedule`
(`schedule.exponential(100.millis) |> upTo(3)`, `schedule.fixed(...)` — from
`use std/schedule`).
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

Measured result (bench/README.md): the compiled benchmarks beat Node/V8 on
all five workloads — about 2× on calls, dispatch, and strings, ~860× on
typed-error control flow — and run ~300× faster than the interpreter.

**Memory.** Compiled Inga uses Perceus-style ARC plus optional region
arenas:

- Every heap object carries one header word: a **non-atomic refcount**
  (compiled Inga is single-threaded), or a marker for string constants and
  arena objects (dup/release are no-ops on those). The compiler emits
  type-directed drop glue per struct/enum/list/option, inserts a `dup`
  where a value is stored into something longer-lived, and registers every
  fresh heap value in a per-function pool that is released on every return
  path — allocation-heavy code reclaims memory at function granularity,
  and small dead objects recycle through segregated free lists at bump
  speed.
- `provide Arena(256.kb)` pushes a **region**: allocations in its dynamic
  extent are bump-allocated from the region (overflow chains chunks) and
  freed wholesale when the scope ends, failures included. The checker
  rejects an arena scope whose result is heap-shaped (it would escape the
  freed region); error boxes are allocated from the RC heap so a `fail`
  can cross an arena boundary.
- Known leaks, by design (all bounded by program shape, not input size):
  closures and service instances free their record but not their captures;
  MutMap contents; error boxes. Values stored into an arena keep their
  region alive only as long as the scope — don't stash arena values in
  longer-lived structures.

**Current limits of the backend:** `encode`/`decode` (runtime JSON) and
showing structs still require the interpreter. Full
delimited-continuation effects (resumable handlers, generators, async)
remain out of scope; handlers are syntax (`catch`, `provide`) rather than
first-class values precisely so that evidence passing stays sufficient.

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
  span space, so inference, the interpreter, the LLVM backend, and
  diagnostics (mapped back to file + line) all operate on one merged
  program.

Future packages keep this shape: a manifest (`inga.toml`) with
content-addressed, lockfile-pinned deps; capabilities make library APIs
honest — a package that needs the network *says so in its types*
(`uses Http`), and the application root decides what to provide.

## 8. Graphics bindings

The `std/graphics` module (imported with `use std/graphics`) provides
GL-backed 2D graphics, implemented on OpenGL through miniquad/macroquad in
both the interpreter (cargo feature `gfx`, enabled by the CLI) and the
native runtime:

```
graphics.run(width, height, title, frame)   // runtime-owned loop; frame: () -> a, once per frame
graphics.clear(r, g, b)                     // 0–255 channels everywhere
graphics.rect(x, y, w, h, r, g, b, a)       graphics.rectLines(x, y, w, h, thick, r, g, b, a)
graphics.circle(x, y, radius, r, g, b, a)   graphics.text(s, x, y, size, r, g, b)
graphics.textWidth(s, size) -> Int          graphics.mouseX() / graphics.mouseY() -> Int
graphics.mousePressed() -> Bool
graphics.shaderNew(fragGlsl) -> Int         // compile GLSL ES; uniforms iTime, iRes
graphics.shaderUse(handle)             graphics.shaderOff()
```

Inverting the loop (`graphics.run` calls the closure, rather than the program
recursing) keeps stacks bounded, and the frame closure captures capability
evidence like any closure — services work normally inside frames. Helpers:
`range(n) -> [Int]` and `random(n) -> Int`. Setting `INGA_GFX_SHOT=<path>`
renders 30 frames, writes a PNG of the framebuffer, and exits (CI smoke
tests). See `games/balatro.inga` for a complete game.

## 9. Tooling (all in this repo)

| Tool | Where | Notes |
|---|---|---|
| `inga run / check` | `crates/inga-cli` | caret diagnostics, warnings |
| `inga fmt` | `crates/inga-core/src/fmt.rs` | canonical style, idempotent, comment-preserving, `--check` mode |
| `inga highlight` | `crates/inga-cli` | ANSI terminal highlighting from the real lexer (lossless) |
| `inga lsp` | `crates/inga-lsp` | diagnostics, hover (inferred signatures with rows), go-to-definition, completion, formatting, semantic tokens |
| VS Code extension | `editors/vscode` | TextMate grammar + LSP client |

## 10. Grammar sketch

```
program   := decl*
decl      := 'use' name ('/' name)* ('{' name,* '}')?
           | 'pub'? 'struct' Upper '=' '{' field,* '}'
           | 'pub'? 'enum' Upper '=' variant ('|' variant)*
           | 'pub'? 'service' Upper '{' (name '::' sig)* '}'
           | name '::' Upper '{' (name '=' expr | name '::' sig block)* '}'   -- impl
           | name '::' sig block                                              -- func
variant   := Upper ('{' field,* '}')?
sig       := '(' param,* ')' ('->' type)? ('!' Upper,+)? ('uses' Upper,+)?
param     := 'lazy'? type? name
type      := Upper | lower | '[' type ']' | type '?'
field     := type? name
block     := '{' stmt* '}'
stmt      := Upper name                      -- capability bind
           | type? name '=' expr             -- binding
           | 'provide' item,*                -- braceless: scopes over the rest of the block
           | expr
item      := name ('(' expr,* ')')?          -- impl, or a resource like Arena(256.kb)
expr      := pipe; pipe := or ('|>' (call | 'catch' arms))*
           | match | if | fail | provide | lambda | literals…
arms      := '{' (pattern '->' expr)+ '}'
pattern   := '_' | name | literal | Upper name              -- typed bind
           | Upper ('(' pattern,* ')' | '{' name,* '}')?
```

Statements end at newlines; expressions continue across a newline before
`|>`, binary operators (except `-` and `!`), and `.` chains. String literals
interpolate with `${expr}`. Comments are `//` and `/* */`.
