#!/usr/bin/env bash
# Cross-language benchmark runner: Inga (native + interpreter) vs JavaScript vs Rust.
# Usage: bench/run.sh   (from the repository root)
set -euo pipefail
cd "$(dirname "$0")/.."

echo "building inga toolchain (release)..."
cargo build --release --workspace --quiet

echo "compiling bench.inga to native code (inga build)..."
./target/release/inga build bench/bench.inga -o target/bench-inga

echo "compiling bench.rs..."
rustc -O bench/bench.rs -o target/bench-rs

echo
echo "=== INGA (native, inga build → LLVM via clang -O2) ==="
./target/bench-inga

echo
echo "=== INGA (tree-walking interpreter, inga run) ==="
./target/release/inga run bench/bench.inga

echo
echo "=== JAVASCRIPT (node $(node --version)) ==="
node bench/bench.js

echo
echo "=== RUST (rustc -O) ==="
./target/bench-rs
