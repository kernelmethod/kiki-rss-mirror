# Writing WebAssembly plugins

A plugin can be a [WebAssembly component](https://component-model.bytecodealliance.org/)
instead of a Lua script, written in Rust or any other language that compiles
to one. A WebAssembly plugin handles the same [events](writing-plugins.md#events),
makes the same calls to the server, and is installed, configured, given
permissions and limited in the same way as a Lua plugin. Lua and WebAssembly
plugins can be installed side by side, and their handlers run in the order of
their plugins' directory names, whichever engine each is written for.

This chapter describes what is different about WebAssembly plugins. Read
[Writing plugins](writing-plugins.md) first, for plugins in general.

WebAssembly plugins need a build of Kiki with the `wasm-plugins` feature, which
is on by default. A build without it lists WebAssembly plugins as unsupported
and skips them.

## The plugin directory

A WebAssembly plugin's directory holds its manifest and one file of code,
the compiled component:

```text
plugins/user/hide-matching/
├── manifest.toml
└── plugin.wasm
```

Its manifest says `engine = "wasm"`. `entrypoint`, if given, names the
component, and defaults to `plugin.wasm`; it may be at most 7 MiB. Everything
else in the manifest, its [config](writing-plugins.md#plugin-config) and
[settings](writing-plugins.md#describing-settings), its `time_budget_ms` and its
[permissions](writing-plugins.md#permissions), means what it means for a Lua
plugin:

```toml
name = "hide-matching"
version = "1.0.0"
engine = "wasm"
description = "Hide entries whose title or content matches a regular expression"

[config]
rules = []
```

## The interface

The interface between Kiki and a plugin is defined in
[WIT](https://component-model.bytecodealliance.org/design/wit.html), in
[`wit/kiki-plugin.wit`](https://github.com/kernelmethod/kiki-rss/blob/main/wit/kiki-plugin.wit)
in Kiki's source. A plugin is a component targeting its `plugin` world, which:

- imports `kiki:plugin/host`, the counterpart of the `kiki` table Lua
  plugins get: `log`, `store-get` and `store-set`, `tag-entry` and
  `untag-entry`, `start-scan`, `delete-entries`, `get-feed`, and `every`;
- imports `kiki:plugin/regex`, the counterpart of `kiki.regex`: see
  [Regular expressions](#regular-expressions);
- exports `init`, which Kiki calls with the plugin's config, as a JSON
  object, when the plugin loads. It returns the events the plugin handles,
  and only those are delivered to it; an error fails the load, as an error
  in a Lua plugin's top level does;
- exports a function for each event: `on-entry-ingest`, `on-fetch-schedule`,
  `on-plugin-load` and so on, and `on-timer`, `on-scan-entry` and
  `on-scan-done` for timers and scans.

Where a Lua plugin passes a function, a WebAssembly plugin gets an id:
`every(secs)` returns a timer's id, which is passed to `on-timer` each time
the timer is due, and `start-scan(options)` returns a scan's id, which is
passed with each of its entries to `on-scan-entry`, and with its summary to
`on-scan-done`. Values in the plugin's store are JSON text.

## Writing a plugin in Rust

The `kiki-plugin` crate, in
[`sdk/rust/kiki-plugin`](https://github.com/kernelmethod/kiki-rss/tree/main/sdk/rust/kiki-plugin)
in Kiki's source, generates the bindings and wraps them in a `Plugin` trait:
implement the handlers your plugin needs, list the events they handle, and
export it with `export_plugin!`. Make a library crate whose `Cargo.toml` has

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
kiki-plugin = { git = "https://github.com/kernelmethod/kiki-rss" }
serde = { version = "1", features = ["derive"] }

[profile.release]
opt-level = "s"
lto = true
strip = true
panic = "abort"
```

and in `src/lib.rs`, a plugin that hides entries whose title matches one of
the patterns in its config:

```rust,ignore
use kiki_plugin::regex::Regex;
use kiki_plugin::{export_plugin, parse_config, Entry, EventKind, Plugin};
use serde::Deserialize;

#[derive(Deserialize)]
struct Config {
    patterns: Vec<String>,
}

struct HideMatching {
    patterns: Vec<Regex>,
}

impl Plugin for HideMatching {
    const EVENTS: &'static [EventKind] = &[EventKind::EntryIngest];

    fn new(config: &str) -> Result<Self, String> {
        let config: Config = parse_config(config)?;
        let patterns = config
            .patterns
            .iter()
            .map(|p| Regex::compile(p, ""))
            .collect::<Result<_, _>>()?;
        Ok(HideMatching { patterns })
    }

    fn on_entry_ingest(&mut self, mut entry: Entry) -> Option<Entry> {
        if self.patterns.iter().any(|re| re.is_match(&entry.title)) {
            entry.tags.push("system:hidden".to_string());
        }
        Some(entry)
    }
}

export_plugin!(HideMatching);
```

Build it for the `wasm32-wasip2` target, and install the result as the
plugin's `plugin.wasm`:

```sh
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2
cp target/wasm32-wasip2/release/hide_matching.wasm \
    ~/.local/share/kiki/plugins/user/hide-matching/plugin.wasm
```

A fuller version of this plugin, with a manifest describing its settings, is
in [`examples/wasm/hide-matching`](https://github.com/kernelmethod/kiki-rss/tree/main/examples/wasm/hide-matching)
in Kiki's source.

`Plugin::new` is called once when the plugin loads, and every handler is
called on the value it returns, so a plugin keeps what it needs, such as
compiled regexes, in its own fields. A handler that panics traps, which is
handled as described [below](#resource-limits). Other languages can use the
WIT file with their own bindings generator, such as `wit-bindgen`'s for C.

## Regular expressions

`kiki_plugin::regex`, the `regex` interface, gives plugins the regular
expressions Lua plugins get from [`kiki.regex`](writing-plugins.md): the same
syntax and flags, compiled and matched by Kiki itself, as native code. That is
faster than a regex library built into the plugin, and leaves the plugin much
smaller: the `regex` crate alone adds about a megabyte to a plugin.

- `Regex::compile(pattern, flags)` compiles a pattern, with flags such as
  `"i"` for case-insensitive; `is_match` and `find` match it.
- `RegexSet::compile(patterns)` compiles a list of patterns and flags, whose
  `matches` returns which of them match a string, in one call to Kiki rather
  than one per pattern.

A plugin may have 128 distinct patterns alive at once. Compiling a pattern
it has alive already, with the same flags, shares it, so a pattern in a
`Regex` and a `RegexSet` counts once; dropping the last one holding a
pattern frees its place.

Plugins can bundle a regex library instead, if they need something the
interface lacks. Prefer matching each pattern alone to the `regex` crate's
`RegexSet`, though: a set of patterns, one of which has no literal text to
look for, can be a hundred times slower on text that is not ASCII.

## What a plugin can reach

A plugin runs in a sandbox of its own: it shares no memory with Kiki or with
any other plugin, and can reach nothing but what the `host` interface
offers. Of [WASI](https://wasi.dev/), the system interface WebAssembly
toolchains build on, Kiki provides only randomness (`wasi:random`) and the
clocks (`wasi:clocks`' `now` and `resolution`), which libraries use without
being asked: Rust's `HashMap` seeds its hasher from it, for instance. There
are no files, no network, no environment and no standard streams. A plugin
built for `wasm32-wasip2` still loads, but calling any of them, as `std::fs`,
`std::env` or `println!` do, traps. Write to Kiki's log with `log` instead.

## Resource limits

A WebAssembly plugin's handlers run under the same [time budget](writing-plugins.md#resource-limits)
as a Lua plugin's: 100 ms per call, unless its manifest sets
`time_budget_ms`, with time spent waiting on the server not counted, up to a
second per call. Loading a plugin, which runs its `init`, may take up to 5
seconds. Each plugin may use up to
16 MiB of memory and 512 KiB of stack.

Kiki compiles each plugin to native code when it loads, once for each
distinct `plugin.wasm`, so changing only a plugin's config reloads it without
compiling it again.

When a handler traps, whether it ran out of time or memory, panicked, or
called into WASI that Kiki doesn't provide, it fails as a Lua handler that
raises an error does: the entry passes through it unmodified, the wait
before a feed's next fetch is left as it was, or the failure is logged. A
trap can leave the plugin's memory in any state, so the plugin is then
started afresh the next time it is needed: Kiki loads it again and calls
`init`, which starts its timers again, while the scans it had started end.
Timers that were due when the plugin trapped aren't called. Kiki restarts at
most one plugin per event, so a plugin may miss an event or two if several
trap at once. A plugin that traps more
than five times in ten minutes is disabled until plugins next reload.

## Security

Compiling plugins to native code means the script host, the process plugins
run in, has to be able to write code to memory and then run it, which Kiki's
other processes are not allowed to do. Systemd's `MemoryDenyWriteExecute=`
would stop it, and is inherited by every process of a service, so the units
Kiki ships don't set it; if you wrote your own, remove it, or WebAssembly
plugins will fail to compile. See the [threat model](threat-model.md#a-misbehaving-plugin)
for what this means.
