# Filtering entries

The `filter` plugin hides entries, by tagging them `system:hidden`, when
their title, URL, content, authors, categories or GUID match regular
expressions, or when they fail to match any of a feed's "include" rules.

Hidden entries are left out of the [web UI](../web-ui.md), and of
`GET /v1/entries` and `GET /v1/feeds/id/{id}/entries` unless those are
passed `include_hidden=true`. Nothing is deleted.

`filter` is [installed by default](index.md) with no rules. Give it some:

```bash
kiki plugin config set filter <<'EOF'
[[exclude]]
fields = ["title"]
pattern = '\b(sponsored|webinar)\b'
flags = "i"

[[include]]              # for feed 3, hide everything not about Rust
fields = ["title", "categories"]
pattern = "(?i)rust"
feeds = [3]
EOF
```

- **`[[exclude]]`** rules hide the entries that match them.
- **`[[include]]`** rules apply to the feeds they list: an entry from one of
  those feeds is hidden unless it matches at least one of the feed's include
  rules.

Whenever its rules change, the filter also applies them to the entries
already downloaded. It never unhides entries.

See [`plugins/filter/main.lua`](https://github.com/kernelmethod/kiki-rss/blob/main/plugins/filter/main.lua) for every
setting.

## The WebAssembly version

[`plugins/filter-wasm`](https://github.com/kernelmethod/kiki-rss/tree/main/plugins/filter-wasm)
is the same plugin ported to Rust and built as a
[WebAssembly component](../writing-wasm-plugins.md). It takes the same
settings, keeps its record of the rules it has applied in the same place,
and hides the same entries, so it can replace the Lua version in place:
copy its `manifest.toml` and `plugin.wasm` into the `filter` plugin
directory, in place of `manifest.toml` and `main.lua`. Rebuild
`plugin.wasm` with `tools/build-filter-wasm.sh`, and compare the two with
`cargo run --profile profiling --example filter_bench`.
