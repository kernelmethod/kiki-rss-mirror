#!/bin/sh
# Rebuild plugins/filter-wasm/plugin.wasm, the filter plugin ported to Rust,
# after changing it, the SDK in sdk/rust/kiki-plugin, or wit/kiki-plugin.wit.
# The tests in src/plugins/filter_tests.rs run it alongside the Lua filter.
#
# Needs the wasm32-wasip2 Rust target: rustup target add wasm32-wasip2
set -eu
cd "$(dirname "$0")/../plugins/filter-wasm"
cargo build --release --target wasm32-wasip2
cp target/wasm32-wasip2/release/kiki_filter.wasm plugin.wasm
echo "wrote plugins/filter-wasm/plugin.wasm ($(wc -c < plugin.wasm) bytes)"
