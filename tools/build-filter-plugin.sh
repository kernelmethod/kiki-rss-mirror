#!/bin/sh
# Rebuild plugins/filter/plugin.wasm, the filter plugin Kiki installs by
# default, from its source in plugins/filter-src, after changing it, the SDK
# in sdk/rust/kiki-plugin, or wit/kiki-plugin.wit.
#
# Needs the wasm32-wasip2 Rust target: rustup target add wasm32-wasip2
set -eu
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root/plugins/filter-src"
cargo build --release --target wasm32-wasip2
cp target/wasm32-wasip2/release/kiki_filter.wasm "$root/plugins/filter/plugin.wasm"
echo "wrote plugins/filter/plugin.wasm ($(wc -c < "$root/plugins/filter/plugin.wasm") bytes)"
