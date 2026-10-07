#!/bin/sh
# Rebuild tests/wasm-fixture/fixture.wasm, the WebAssembly plugin the tests
# in src/scripting/wasm_tests.rs run, after changing it, the SDK in
# sdk/rust/kiki-plugin, including its interface, wit/kiki-plugin.wit.
#
# Needs the wasm32-wasip2 Rust target: rustup target add wasm32-wasip2
set -eu
cd "$(dirname "$0")/../tests/wasm-fixture"
cargo build --release --target wasm32-wasip2
cp target/wasm32-wasip2/release/kiki_wasm_fixture.wasm fixture.wasm
echo "wrote tests/wasm-fixture/fixture.wasm ($(wc -c < fixture.wasm) bytes)"
