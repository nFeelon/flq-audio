#!/usr/bin/env bash
# Build the flq wasm decoder into web/flq_wasm.wasm
set -euo pipefail
cd "$(dirname "$0")"
cargo build --release --target wasm32-unknown-unknown
mkdir -p ../web
cp target/wasm32-unknown-unknown/release/flq_wasm.wasm ../web/flq_wasm.wasm
echo "wrote web/flq_wasm.wasm"
