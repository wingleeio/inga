#!/usr/bin/env bash
# Cross-language benchmark runner: Inga vs JavaScript vs Rust.
# Usage: bench/run.sh   (from the repository root)
set -euo pipefail
cd "$(dirname "$0")/.."

echo "building inga (release)..."
cargo build --release -p inga-cli --quiet

echo "compiling bench.rs..."
rustc -O bench/bench.rs -o target/bench-rs

echo
echo "=== INGA (tree-walking interpreter, release build) ==="
./target/release/inga run bench/bench.inga

echo
echo "=== JAVASCRIPT (node $(node --version)) ==="
node bench/bench.js

echo
echo "=== RUST (rustc -O) ==="
./target/bench-rs
