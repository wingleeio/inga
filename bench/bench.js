// JavaScript side of the cross-language benchmark. Mirrors bench.inga.
// Run with: node bench/bench.js

"use strict";

class Boom extends Error {
  constructor(n) {
    super("boom");
    this.n = n;
  }
}

// Dependency-injection analogue: behavior behind an object interface.
const plainAdder = {
  add(a, b) {
    return a + b;
  },
};

const memStore = {
  m: new Map(),
  put(k, v) {
    this.m.set(k, v);
  },
  take(k) {
    const v = this.m.get(k);
    return v === undefined ? 0 : v;
  },
};

// 1. Raw calls + integer arithmetic.
function fib(n) {
  if (n < 2) return n;
  return fib(n - 1) + fib(n - 2);
}

// 2. Every addition goes through the injected interface.
function fibService(n, adder) {
  if (n < 2) return n;
  return adder.add(fibService(n - 1, adder), fibService(n - 2, adder));
}

// 3. One string interpolation + length per node.
function fibStrings(n) {
  if (n < 2) return `n=${n}`.length;
  return fibStrings(n - 1) + fibStrings(n - 2);
}

// 4. Every leaf throws a typed error that the caller catches.
function boom(n) {
  throw new Boom(n);
}

function fibErrors(n) {
  if (n < 2) {
    try {
      return boom(n);
    } catch (e) {
      return e.n;
    }
  }
  return fibErrors(n - 1) + fibErrors(n - 2);
}

// 5. A map write per node and a read per leaf, through the interface.
function fibStore(n, store) {
  store.put(n, n);
  if (n < 2) return store.take(n);
  return fibStore(n - 1, store) + fibStore(n - 2, store);
}

function bench(name, work) {
  const t0 = performance.now();
  const result = work();
  const t1 = performance.now();
  console.log(`${name} ${Math.round((t1 - t0) * 1000)} us (result ${result})`);
}

function round(i) {
  console.log(`--- round ${i} ---`);
  bench("fib(27)         ", () => fib(27));
  bench("fib_service(24) ", () => fibService(24, plainAdder));
  bench("strings(24)     ", () => fibStrings(24));
  bench("errors(24)      ", () => fibErrors(24));
  bench("store(24)       ", () => fibStore(24, memStore));
}

// Two rounds: the first warms the JIT, the second is the representative one.
round(1);
round(2);
