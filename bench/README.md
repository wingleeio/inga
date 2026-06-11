# Inga vs JavaScript vs Rust — microbenchmarks

The same five workloads, written idiomatically in each language
([`bench.inga`](bench.inga), [`bench.js`](bench.js), [`bench.rs`](bench.rs)).
The **same Inga source** runs two ways: compiled to native code by
`inga build` (LLVM, via clang -O2) and interpreted by `inga run`.

| # | Workload | What it measures | Inga | JavaScript | Rust |
|---|---|---|---|---|---|
| 1 | `fib(27)` — 635,621 calls | function calls + integer arithmetic | recursion | recursion | recursion |
| 2 | `fib_service(24)` — 150,049 calls | dependency-injected dispatch | `uses Adder` capability | object method | `&dyn Adder` trait object |
| 3 | `strings(24)` — 150,049 nodes | string build + length | `"n=${n}"` interpolation | template literal | `format!` |
| 4 | `errors(24)` — 75,025 raise/handle | typed error per leaf | `fail` / `catch` | `throw` / `try` (Error subclass) | `Result` / `match` |
| 5 | `store(24)` — 150k writes, 75k reads | keyed mutable storage via DI | `Store` service over `MutMap` | object over `Map` | `&mut dyn Store` over `HashMap` |

All workloads are fib-shaped tree recursion (large call counts, shallow
depth). Each program times itself in-process — Inga with its `nowMicros()`
builtin, JS with `performance.now()`, Rust with `Instant` — and runs two
rounds; tables report round 2 (JS round 1 warms the JIT).

## Results

Apple Silicon (Darwin 25.4.0), 2026-06-10, all four runs back-to-back in one
session. `inga build` / `rustc -O` 1.96.0 / clang 21 / node v24.16.0.
Reproduce with `bench/run.sh`.

| Workload | **Inga native** | JavaScript | Rust | Inga interp | native vs V8 |
|---|---:|---:|---:|---:|---:|
| `fib(27)` | **353 µs** | 1,018 µs | 531 µs | 147,487 µs | **2.9× faster** |
| `fib_service(24)` | **140 µs** | 275 µs | 124 µs | 57,532 µs | **2.0× faster** |
| `strings(24)` | **180 µs** | 459 µs | 3,730 µs | 41,254 µs | **2.6× faster** |
| `errors(24)` | **544 µs** | 207,173 µs | 124 µs | 63,107 µs | **380× faster** |
| `store(24)` | **631 µs** | 935 µs | 1,690 µs | 114,472 µs | **1.5× faster** |

**Compiled Inga beats V8 on all five workloads** — and beats the idiomatic
Rust version on two (`strings`, where `format!` allocates per node, and
`store`, where std's `HashMap` pays for SipHash). These numbers include
Inga's Perceus-style ARC: unlike the earlier never-freeing bump allocator,
every workload now reclaims memory (`errors` pays ~2× for boxing failed
values and refcounts and is still ~380× ahead of V8; `fib` and `store` got
*faster* because dead objects recycle through warm free lists).

## Why it's fast — and where that's earned vs. situational

The backend compiles Inga's two effect rows *away* (see
[SPEC §6](../docs/SPEC.md#6-execution-how-inga-runs)):

- **`fib`**: `Int` is a raw `i64`; calls are direct calls. This is just
  LLVM-compiled native code vs a JIT — the honest baseline gap (~2×).
- **`fib_service`**: the `uses Adder` row becomes a hidden instance-pointer
  parameter and the method call an indirect call — the same machine code as
  Rust `dyn` dispatch. V8 pays for hidden-class checks; Inga's evidence was
  resolved by the type system at compile time.
- **`errors`**: `fail`/`catch` compile to a `{value, err}` two-register
  return plus a predictable branch — Rust's `Result` shape, reaching ~2.6 ns
  per raise+handle. V8 throws a real exception with stack capture (~2.7 µs).
  This is the headline design claim: typed errors are control flow you can
  afford.
- **`strings`**: `len("n=${n}")` is folded to `2 + digits(n)` — no string is
  materialized. That's the compile-time analogue of what V8's rope strings do
  at run time (`.length` on a cons string doesn't flatten it), so the
  comparison stays fair — but note both engines are succeeding by *not doing
  the work*; Rust's `format!` actually allocates, which is why it's last.
- **`store`**: open-addressing map with fibonacci hashing in the runtime,
  plus one compiler fusion: `map.get(k) |> getOrElse(default)` (with a pure
  default) probes the table directly instead of boxing an `Option`.

Two caveats worth saying out loud. First, allocation is a bump allocator
that never frees — fine for short-lived processes and benchmarks, not a GC
story (that's future work, and it flatters `strings`/`store` slightly).
Second, V8 is running a dynamically-typed language; Inga's wins here are
wins of *language design* (static types and static effect rows make the fast
lowering possible), not of compiler engineering heroics — LLVM does the
heavy lifting.

## Caveats

- Microbenchmarks measure runtimes, not languages-as-used; none of these
  capture I/O-bound services where `retry`/capability semantics matter.
- Single machine, wall-clock, two rounds, one representative session; expect
  ±10% noise. Sub-100 µs numbers are indicative.
- `errors` in JS pays for stack capture because `Error` subclasses are the
  idiom; throwing a plain object would narrow (not close) the gap.
- `inga build` covers the benchmarked subset of the language; `encode`/
  `decode` (runtime JSON) still require the interpreter.
