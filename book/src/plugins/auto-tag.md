# Tagging entries automatically

The `auto-tag` plugin adds tags to entries as they arrive. Each of its rules
adds a tag:

- to the entries whose title, URL, content, authors, categories or GUID
  match a regular expression;
- to every entry from a list of feeds; or,
- given both, to the entries from those feeds that match.

`auto-tag` is [installed by default](index.md) with no rules. Give it some:

```bash
kiki plugin config set auto-tag <<'EOF'
[[rules]]                # tag everything from feed 3, and from this URL
tag = "security"
feeds = [3, "https://example.com/feed.xml"]

[[rules]]                # tag entries about Rust, from any feed
tag = "rust"
fields = ["title", "categories"]
pattern = '\brust\b'
flags = "i"
EOF
```

A rule's tag may also be `system:saved`, `system:read` or `system:hidden`.

Whenever its rules change, the plugin also applies them to the entries
already downloaded. It never removes tags.

The plugin is written in Rust, as a [WebAssembly plugin](../writing-plugins.md);
see [`plugins/auto-tag/src/lib.rs`](https://github.com/kernelmethod/kiki-rss/blob/main/plugins/auto-tag/src/lib.rs)
for every setting. Versions before 2.0.0 were written in Lua, and took the same
configuration.
