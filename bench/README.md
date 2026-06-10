# Inga vs JavaScript vs Rust — microbenchmarks

The same five workloads, written idiomatically in each language
([`bench.inga`](bench.inga), [`bench.js`](bench.js), [`bench.rs`](bench.rs)):

| # | Workload | What it measures | Inga | JavaScript | Rust |
|---|---|---|---|---|---|
| 1 | `fib(27)` — 635,621 calls | function calls + integer arithmetic | recursion | recursion | recursion |
| 2 | `fib_service(24)` — 150,049 calls | dependency-injected dispatch | `uses Adder` capability | object method | `&dyn Adder` trait object |
| 3 | `strings(24)` — 150,049 nodes | string build + length | `"n=${n}"` interpolation | template literal | `format!` |
| 4 | `errors(24)` — 75,025 raise/handle | typed error per leaf | `fail` / `catch` | `throw` / `try` (Error subclass) | `Result` / `match` |
| 5 | `store(24)` — 150k writes, 75k reads | keyed mutable storage via DI | `Store` service over `MutMap` | object over `Map` | `&mut dyn Store` over `HashMap` |

All workloads are fib-shaped tree recursion (large call counts, shallow
depth). Each program times itself in-process — Inga with its `nowMillis()`
builtin, JS with `performance.now()`, Rust with `Instant` — and runs two
rounds; the table reports round 2 (JS round 1 warms the JIT).

## Results

Apple Silicon (Darwin 25.4.0), 2026-06-10. Inga `--release`, node v24.16.0,
`rustc -O` 1.96.0. Reproduce with `bench/run.sh`.

| Workload | Inga | JavaScript | Rust | Inga / JS | Inga / Rust |
|---|---:|---:|---:|---:|---:|
| `fib(27)` | 144 ms | 1.03 ms | 0.53 ms | 140× | 272× |
| `fib_service(24)` | 64 ms | 0.27 ms | 0.13 ms | 237× | 492× |
| `strings(24)` | 40 ms | 0.44 ms | 3.71 ms | 91× | **11×** |
| `errors(24)` | 66 ms | 210 ms | 0.11 ms | **0.31×** | 600× |
| `store(24)` | 114 ms | 0.90 ms | 1.62 ms | 127× | 70× |

## Reading the numbers

**The headline ratio is expected.** Inga v0.1 is a tree-walking interpreter
(see [SPEC §6](../docs/SPEC.md#6-execution-how-inga-runs)); 100–500× versus a
JIT or native code on call-dense workloads is the normal cost of that
architecture, and it's the bytecode-VM → Cranelift roadmap's job to close it.
The absolute per-operation costs are still small: ~230 ns per function call,
~430 ns per capability-dispatched call, ~880 ns per `fail`+`catch`.

**The interesting upset: typed errors.** Inga beats JavaScript 3× on the
error workload. Inga's `fail` is a value traveling the return path — exactly
like Rust's `Result`, which is why Rust does it in 1.5 ns — while idiomatic
JS `throw new Error(...)` captures a stack trace on every throw. The design
claim this benchmark supports: when errors are part of the type system you
can afford to use them for ordinary control flow ("user not found"), not
just disasters.

**Service dispatch costs ~2× a plain call in the interpreter** (compare
per-node cost of workloads 1 and 2: ~230 ns vs ~430 ns). That overhead is a
dynamic lookup of the provided instance plus a method-scope setup; under the
planned evidence-passing compilation it becomes an indexed load plus an
indirect call — the same machine code as Rust's `dyn` dispatch (0.13 ms
here).

**Rust's `strings` losing to JS** is the usual microbenchmark caveat in the
other direction: `format!` heap-allocates a fresh `String` per node while V8
fast-paths small-integer-to-string and short-lived strings. Idiomatic ≠
optimal in every language; we benchmarked idiomatic.

## Caveats

- Microbenchmarks measure runtimes, not languages-as-used; none of these
  capture I/O-bound services where Inga's `retry`/capability semantics matter.
- Single machine, wall-clock, two rounds. Expect ±10% run-to-run noise;
  sub-millisecond JS/Rust numbers are indicative only. Ratios are computed
  from one representative run.
- `errors` in JS pays for stack capture because `Error` subclasses are the
  idiom. Throwing a plain object would be faster — and unidiomatic.
