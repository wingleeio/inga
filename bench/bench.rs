// Rust side of the cross-language benchmark. Mirrors bench.inga.
// Compile and run with: rustc -O bench/bench.rs -o /tmp/bench-rs && /tmp/bench-rs

use std::collections::HashMap;
use std::hint::black_box;
use std::time::Instant;

struct Boom {
    n: i64,
}

// Dependency-injection analogue: behavior behind a trait object.
trait Adder {
    fn add(&self, a: i64, b: i64) -> i64;
}

struct PlainAdder;

impl Adder for PlainAdder {
    fn add(&self, a: i64, b: i64) -> i64 {
        a + b
    }
}

trait Store {
    fn put(&mut self, k: i64, v: i64);
    fn take(&mut self, k: i64) -> i64;
}

struct MemStore {
    m: HashMap<i64, i64>,
}

impl Store for MemStore {
    fn put(&mut self, k: i64, v: i64) {
        self.m.insert(k, v);
    }
    fn take(&mut self, k: i64) -> i64 {
        self.m.get(&k).copied().unwrap_or(0)
    }
}

// 1. Raw calls + integer arithmetic.
fn fib(n: i64) -> i64 {
    if n < 2 {
        n
    } else {
        fib(n - 1) + fib(n - 2)
    }
}

// 2. Every addition goes through the trait object (dynamic dispatch).
fn fib_service(n: i64, adder: &dyn Adder) -> i64 {
    if n < 2 {
        n
    } else {
        adder.add(fib_service(n - 1, adder), fib_service(n - 2, adder))
    }
}

// 3. One string format + length per node.
fn fib_strings(n: i64) -> i64 {
    if n < 2 {
        format!("n={n}").len() as i64
    } else {
        fib_strings(n - 1) + fib_strings(n - 2)
    }
}

// 4. Every leaf returns a typed error that the caller handles.
fn boom(n: i64) -> Result<i64, Boom> {
    Err(Boom { n })
}

fn fib_errors(n: i64) -> i64 {
    if n < 2 {
        match boom(n) {
            Ok(v) => v,
            Err(e) => e.n,
        }
    } else {
        fib_errors(n - 1) + fib_errors(n - 2)
    }
}

// 5. A map write per node and a read per leaf, through the trait object.
fn fib_store(n: i64, store: &mut dyn Store) -> i64 {
    store.put(n, n);
    if n < 2 {
        store.take(n)
    } else {
        fib_store(n - 1, store) + fib_store(n - 2, store)
    }
}

fn bench(name: &str, work: impl FnOnce() -> i64) {
    let t0 = Instant::now();
    let result = black_box(work());
    let elapsed = t0.elapsed();
    println!("{name} {:.2} ms (result {result})", elapsed.as_secs_f64() * 1000.0);
}

fn round(i: u32) {
    println!("--- round {i} ---");
    bench("fib(27)         ", || fib(black_box(27)));
    bench("fib_service(24) ", || fib_service(black_box(24), &PlainAdder));
    bench("strings(24)     ", || fib_strings(black_box(24)));
    bench("errors(24)      ", || fib_errors(black_box(24)));
    bench("store(24)       ", || {
        let mut store = MemStore { m: HashMap::new() };
        fib_store(black_box(24), &mut store)
    });
}

fn main() {
    round(1);
    round(2);
}
