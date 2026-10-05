# Plan: WebAssembly plugins

Status: proposal. Nothing here is implemented yet.

## Goal

Let a plugin be a WebAssembly component instead of a Lua script, so that
plugins can be written in Rust (and other languages that compile to
components: C, Zig, TinyGo), with the same events, host calls, permissions,
config, settings, reload behaviour and failure semantics Lua plugins have
today. A Lua plugin and a WASM plugin should be indistinguishable to the
operator except for the `engine` column.

### Non-goals (for the first release)

- Replacing Lua, or porting the bundled plugins. They stay Lua; at most one is
  ported as an example and a parity test.
- Guests that need a large runtime (componentize-py, ComponentizeJS). Their
  components run to tens of MiB; see [Size limits](#size-limits).
- Giving plugins the network, the filesystem, or anything else Lua plugins
  can't do. WASM is a second way to write the same plugins, not a way to
  widen what a plugin may do.
- `kiki.regex` and `kiki.html` as host calls. Lua needs them because it has
  no regex engine or HTML parser; a Rust guest compiles in `regex` and
  `lol_html` itself.

## What the current design constrains

These facts about the code decide most of the plan:

1. **Plugins run in the script host child, not the server.**
   `src/process/script_host.rs` spawns `kiki __script-host`, which holds no
   database and no filesystem (Landlock with an empty ruleset), and can't
   make sockets (`DENIED_SCRIPT_HOST` in `src/sandbox/linux.rs`). The
   server talks to it over a lockstep postcard protocol
   (`src/process/ipc.rs`). WASM must run in the same child, or in a sibling
   with the same profile.
2. **No writable+executable memory, anywhere.** `apply_mdwe` installs
   `PR_SET_MDWE` in every sandboxed process, `dist/kiki.service` sets
   `MemoryDenyWriteExecute=yes`, and so does the homelab deployment
   (`hosts/nixdev/kiki-web.nix`). A JIT (Wasmtime's Cranelift-to-native,
   Wasmer, V8) can't run there. Loading precompiled native code doesn't
   work either: it needs `mprotect(PROT_EXEC)` on memory that wasn't
   executable before, which MDWE refuses, and the child can't open files
   anyway. **The runtime must interpret.**
3. **Permissions are enforced by the server, by plugin name.** Every
   `ServiceCall` goes up the socket as `FromHost::Call { plugin, call }` and
   `src/plugins/services.rs` decides. A WASM engine gets this for free as
   long as it tags each call with the plugin's name.
4. **Handlers run in plugin order across all plugins.** `entry.ingest` is a
   chain: plugins load in directory-name order and each handler's output is
   the next one's input. Mixing engines must keep one global order.
5. **The manifest already has an `engine` field.** `PluginEngine` in
   `src/plugins/mod.rs` has one variant, `Lua`, plus `source_extension`,
   `default_entrypoint` and `is_supported`, so a new engine was planned for.
6. **`ScriptSource` is text.** The entrypoint is a `String` and modules are
   Lua files, and `MAX_PLUGIN_SOURCE_BYTES` is 1 MiB. The IPC frame limit
   (`MAX_FRAME_BYTES`) is 8 MiB for a whole `Reload`.
7. **Binary size matters.** The release profile uses fat LTO and one codegen
   unit to save 14%, and releases are static musl binaries.

## Decisions

### Runtime: Wasmtime with the Pulley interpreter (to confirm in Phase 0)

| Option | For | Against |
|---|---|---|
| **Wasmtime + Pulley** (recommended) | Component Model and WIT, so typed bindings in every guest language. Epoch interruption, fuel, `ResourceLimiter`. Same runtime most of the ecosystem targets. Pulley compiles to interpreter bytecode, which is data, so no executable memory is needed. | Largest binary-size cost: Cranelift is still in the binary to compile to Pulley. Interpreted, so slower than native, though fast enough for RSS entries. |
| **wasmi** | Pure Rust, small, interpreter by design, fuel metering built in, fast startup. | Core modules only, so we would design our own ABI (postcard or JSON over linear memory) and ship SDKs that hide it. Every guest language then needs hand-written glue. |
| wasm3 / WAMR | Fast interpreters. | C. The script host doc says the Lua VM is "the largest remaining body of C in the server", so adding more C goes the wrong way. |
| Any JIT | Speed. | Ruled out by MDWE (constraint 2). |

The Component Model is the main reason to recommend Wasmtime. With it, the
plugin API is one `.wit` file, and guests get generated bindings from it,
including for records, options and results. With wasmi, the ABI and every SDK
would be ours to design and maintain. **Phase 0 decides this with
measurements.** If Wasmtime+Pulley costs too much binary size or memory,
fall back to wasmi with a postcard ABI. The rest of this plan stays the same
apart from the WIT section.

### Where it runs: in the existing script host, behind a composite runner

Run WASM in the same `__script-host` child as Lua. A second child would mean
a cross-process hop between plugins in every mixed `entry.ingest` chain, a
second channel to keep alive, and a second fixed memory cost. With one child,
one request still serves the whole chain.

Inside the child, `run_child` currently builds a `LuaScriptRunner`. It will
build a `CompositeRunner` instead. That runner holds the plugins in load
order, each backed by one engine. Each engine gives the composite the list of
`(plugin_index, handler)` pairs it has for an event. The composite merges
them by plugin index and runs them in that order, carrying the entry from one
handler to the next. `LuaScriptRunner`'s per-VM grouping by permissions stays
as it is, inside the Lua engine.

### Handler model: exported functions plus `init`

Lua plugins register closures with `kiki.on`. A component can't pass
closures across the boundary, so:

- The component exports `init(config) -> result<list<event-kind>, string>`.
  It runs where Lua's top-level chunk runs, and returns the events the plugin
  handles. The host uses that list for `EventSet`, so events no plugin
  handles are still never sent over IPC.
- The component exports one function per event (`on-entry-ingest`,
  `on-fetch-schedule`, …). The SDK gives each one a default no-op body, so a
  plugin implements only what it needs.
- Scans and timers return ids instead of taking closures:
  `start-scan(options) -> u64` and `every(secs) -> u32`, with exported
  `on-scan-entry(scan, entry)`, `on-scan-done(scan, summary)` and
  `on-timer(id)` callbacks.

### WASI: the minimum guests need, all denied or virtual

Rust's `wasm32-wasip2` target imports WASI interfaces even for a plain
`std` program (for example to print a panic message). Link `wasmtime-wasi`
with an empty context: no preopened directories, no sockets, no environment
or arguments, wall and monotonic clocks allowed, random allowed. Send stdout
and stderr to `tracing` at `debug`/`warn`, with a rate limit, tagged with the
plugin. Check in Phase 0 that `getrandom` and `clock_gettime` pass the script
host's seccomp filter (they aren't on any deny list today).

## The WIT interface (sketch)

This file lives at `wit/kiki-plugin.wit` and is the source of truth for the
API. Field names and types mirror `FeedEntry`, `FetchSchedule`,
`ScanOptions`, `DeleteFilter` and `ServiceCall` in `src/scripting/mod.rs`.

```wit
package kiki:plugin@0.1.0;

interface types {
    enum level { debug, info, warn, error }

    enum event-kind {
        entry-parsed, entry-ingest, fetch-success, fetch-error,
        feed-added, feed-removed, plugin-load, fetch-schedule,
    }

    record entry {
        id: option<s64>,
        feed-id: s64,
        syndication-format: string,
        guid: string,
        published-at: option<s64>,
        title: string,
        url: option<string>,
        content: option<string>,
        authors: list<string>,
        categories: list<string>,
        tags: list<string>,
        cache-assets: bool,
    }

    record fetch-success { feed-id: s64, status: u16, url: string, content-length: option<u64> }
    record fetch-error   { feed-id: s64, kind: string, status: option<u16>, message: string, retry-after: option<u64> }
    record feed          { id: s64, url: option<string>, title: string }
    enum content-change  { changed, unchanged, unknown }
    record fetch-schedule {
        feed-id: s64, status: u16, change: content-change, hint-secs: u64,
        interval-secs: u64, min-cadence-secs: u64, wait-secs: u64,
    }
    record scan-options  { feed-id: option<s64>, since: option<s64>, include-hidden: bool }
    record scan-summary  { scanned: u64, updated: u64 }
    record delete-filter {
        dropped-before: s64, feed-id: option<s64>,
        published-before: option<s64>, keep-tagged: option<list<string>>,
    }
}

/// What Lua plugins reach through the `kiki` global.
interface host {
    use types.{level, feed, scan-options, delete-filter};

    log: func(level: level, message: string);

    /// Values are JSON text, with the same limits as `kiki.store`.
    store-get: func(key: string) -> result<option<string>, string>;
    store-set: func(key: string, value: option<string>) -> result<_, string>;

    tag-entry: func(id: s64, tag: string) -> result<bool, string>;
    untag-entry: func(id: s64, tag: string) -> result<bool, string>;
    start-scan: func(options: scan-options) -> result<u64, string>;
    /// Needs the `entries.delete` permission.
    delete-entries: func(filter: delete-filter) -> result<u64, string>;

    get-feed: func(id: s64) -> result<option<feed>, string>;

    /// Same rules as `kiki.every`: 60 s to a year.
    every: func(secs: u64) -> result<u32, string>;
}

world plugin {
    use types.{event-kind, entry, fetch-success, fetch-error, feed, fetch-schedule, scan-summary};
    import host;

    /// Runs when plugins load; `config` is the plugin's config as JSON.
    export init: func(config: string) -> result<list<event-kind>, string>;

    export on-entry-parsed: func(entry: entry);
    /// `none` drops the entry.
    export on-entry-ingest: func(entry: entry) -> option<entry>;
    export on-fetch-success: func(event: fetch-success);
    export on-fetch-error: func(event: fetch-error);
    export on-feed-added: func(feed: feed);
    export on-feed-removed: func(feed: feed);
    export on-plugin-load: func();
    /// `none` leaves the wait as it is.
    export on-fetch-schedule: func(schedule: fetch-schedule) -> option<u64>;
    export on-timer: func(id: u32);
    export on-scan-entry: func(scan: u64, entry: entry) -> option<entry>;
    export on-scan-done: func(scan: u64, summary: scan-summary);
}
```

**Versioning.** The package is versioned (`kiki:plugin@0.1.0`). Adding an
event adds an export, which older components lack, so each API change is a
new minor version of the package. The host keeps bindings for every version
it supports (`bindgen!` once per version), and picks one by matching the
component's exports. A component built against an unsupported version fails
to load and is reported under `errors`, like an invalid manifest. Kiki
changes its supported versions only in a minor release, and the changelog
says so.

## Manifest and discovery changes (`src/plugins/mod.rs`)

```toml
name = "hide-sponsored"
version = "1.0.0"
engine = "wasm"
entrypoint = "plugin.wasm"   # default
time_budget_ms = 100
permissions = []
memory_limit_mib = 16        # new, WASM only; see Resource limits

[config]
# unchanged
```

- `PluginEngine::Wasm`. It has `name() = "wasm"`, `source_extension() =
  "wasm"` and `default_entrypoint() = "plugin.wasm"`. `is_supported()` is
  `cfg!(feature = "wasm-plugins")`, so a build without the feature lists
  WASM plugins under `errors` with a clear reason instead of failing to parse
  them.
- `ScriptSource` gets a `code` field holding
  `enum PluginCode { Lua { text, modules }, Wasm { bytes: ByteBuf, hash: [u8; 32] } }`.
  The postcard codec isn't self-describing, but both ends are the same
  binary, so this is safe (see the note at the top of `ipc.rs`).
  `serde_bytes` and `blake3` are already dependencies.
- `load_source` for a WASM plugin reads only the entrypoint. There are no
  modules, since a component is self-contained, and other files in the
  directory are ignored.
- `load_sources(discovery, engine)` is called per engine today. It becomes
  one call that returns every enabled plugin in load order, so the
  composite can keep the order.
- `MAX_PLUGIN_SOURCE_BYTES` (1 MiB) stays for Lua. A new
  `MAX_WASM_COMPONENT_BYTES` starts at 8 MiB; see [Size limits](#size-limits).
- `kiki plugin ls`, `GET /v1/plugins` and the web UI already show `engine`,
  and `PluginEngine` already derives `ToSchema`. Check the OpenAPI output
  and the web UI's plugin list after the change.

### Size limits

A release-built Rust component with `regex` and `serde_json` is around
0.3–2 MiB. Cap a component at 8 MiB to start.

`HostRequest::Reload` sends every plugin in one frame, and frames are capped
at 8 MiB, so a few components could overflow it. Split loading in two:

1. `HostRequest::PutComponent { hash, bytes }`, one frame per component the
   child hasn't seen yet. The child keeps compiled components in a cache
   keyed by BLAKE3 hash, which also means a reload that changes only config
   doesn't recompile anything.
2. `HostRequest::Reload { sources }`, where WASM sources carry only the hash.
   The child evicts cached components no source refers to.

## Execution inside the child (`src/scripting/wasm.rs`, new)

One `wasmtime::Engine` per child, configured with:

- `Config::target("pulley64")`, or `pulley32` on 32-bit hosts.
- `signals_based_traps(false)`, so no signal handlers are needed.
- Small memory reservations and guard sizes, since this is a few
  plugins on a small box, not a server full of tenants.
- `epoch_interruption(true)`.
- `parallel_compilation(false)`, so compilation doesn't start a thread
  pool inside the sandbox.
- Component Model on, and no WASM proposals beyond what `wasm32-wasip2`
  emits.

Each plugin gets its own `Store` and component instance. That is stronger
isolation than Lua's VM sharing, at the cost of a little memory per plugin.
The `Store` data holds the plugin name, its permissions, the
`ScriptServices` callback for host calls, its timers and scans, and a
`StoreLimits`.

### Resource limits

| Limit | Lua today | WASM |
|---|---|---|
| Time per handler call | Debug hook every 1000 instructions checks the plugin's `TimeBudget` | A ticker thread in the child bumps the engine epoch every ~5 ms. Each call sets `epoch_deadline` to 1 tick, and the epoch callback compares elapsed time against the plugin's `TimeBudget` and either extends the deadline or traps. Time spent waiting on host calls is subtracted, up to 1 s per call, as for Lua. |
| Memory | 16 MiB per VM, shared by plugins with the same permissions | `StoreLimits`: 16 MiB of linear memory per plugin by default, or the manifest's `memory_limit_mib` (capped at 64 MiB). Also one memory, a few tables and a bounded table size. |
| Stack | Lua's C stack limit | `max_wasm_stack` (e.g. 512 KiB). |
| Wedged child | `IPC_TIMEOUT` (10 s) kills the host | Unchanged. |
| Compile time | n/a | Compiling happens in the child, during `PutComponent`. It must finish inside `IPC_TIMEOUT`; Phase 0 measures it, and if it gets close, the compile timeout gets its own setting. |

### Failure semantics

These match the Lua engine:

- `entry.ingest`: a trap, timeout, out-of-memory or bad return value lets
  the entry through that handler **unmodified**, and the chain continues.
  `restore_read_only` still applies to what the handler returns.
- `fetch.schedule`: the wait is left as it was, and the next handler runs.
- Observe events: the failure is logged and dropped.
- `init` failing: on startup no plugins run, and on a reload the old set
  keeps running, as for a Lua compile error.

WASM adds one case. After a trap, a component instance can't safely be
entered again. The engine therefore drops the instance, instantiates the
cached component again, and re-runs `init`, which re-creates the plugin's
timers. The plugin's scans are cancelled. To stop a crash loop, a plugin
that traps more than N times in M minutes is disabled until the next reload,
and that shows up in `GET /v1/plugins` and in a metric.

### Sandbox

No new syscalls should be needed, and MDWE stays as it is. Phase 0 checks
this by running the WASM engine inside the real script host sandbox (not
`--no-sandbox`). The existing test in `src/sandbox/linux.rs` that maps W+X
memory stays as a regression guard. If Wasmtime reserves large virtual
ranges for linear memory, check that this doesn't distort the per-process
memory figures in `src/process/stats.rs`.

## Phases

Each phase is one or a few PRs, and each must pass `cargo fmt`,
`cargo clippy --all-targets --all-features -- -D warnings` and `cargo test`.

### Phase 0: Spike (throwaway branch)

Load a trivial component that implements `on-entry-ingest` in the sandboxed
script host using Wasmtime+Pulley, and separately using wasmi with a core
module. Measure:

- Size of the release static musl binary, with and without each runtime.
- Time to compile a ~1 MiB component, and the child's RSS afterwards.
- `entry.ingest` latency for a typical entry (one regex over the title and
  content), compared with the same logic in Lua.
- That it runs under MDWE, Landlock and seccomp without changes.

**Exit:** pick the runtime, and agree whether `wasm-plugins` is a default
feature. Suggested bar: under ~6 MiB added to the binary and per-entry
latency within 3× of Lua. Write the numbers into this document.

### Phase 1: Plumbing, with no runtime yet

- `PluginEngine::Wasm` and `PluginCode`, the manifest default entrypoint,
  `MAX_WASM_COMPONENT_BYTES`, and `memory_limit_mib` validation.
- One ordered `load_sources`.
- `CompositeRunner` in the child, wrapping only `LuaScriptRunner`. This is a
  pure refactor and all existing tests must pass unchanged.
- The `PutComponent` IPC message and the hash cache, with a stub engine.
- The `wasm-plugins` Cargo feature, off by default, with
  `is_supported() == false` without it.
- Tests: manifest parsing and validation, size cap, a WASM plugin listed
  under `errors` when the feature is off, and the IPC round trip
  (`ipc.rs` already has codec tests to extend).

### Phase 2: Engine with observe and transform events

- `src/scripting/wasm.rs`: engine config, per-plugin stores, `init`, every
  event export, and the time, memory and stack limits.
- `log` as the only host import, plus the WASI shim.
- Failure semantics, including re-instantiating after a trap and the
  crash-loop limiter.
- Mixed ordering in `CompositeRunner`.
- Tests use component fixtures written in the WAT text format and compiled
  with the `wat` crate (a dev-dependency), so `cargo test` needs no
  `wasm32` toolchain. They cover:
  - transform chaining and drop (`none`),
  - read-only fields restored,
  - a timeout passing the entry through,
  - out-of-memory,
  - a trap, then re-instantiation,
  - `fetch.schedule` returns,
  - Lua→WASM→Lua ordering in one chain,
  - `EventSet` coming from `init`.

### Phase 3: Host calls

- `store-*`, `tag-entry`/`untag-entry`, `get-feed`, `start-scan` with
  `on-scan-entry`/`on-scan-done`, `delete-entries`, and `every` with
  `on-timer`. Each maps one-to-one to an existing `ServiceCall`, so the
  server side doesn't change.
- Wire scans into `dispatch_scan`/`finish_scan`, and timers into the
  child's timer tick.
- Tests: mirror the Lua tests in `src/plugins/*_tests.rs` and
  `src/tasks/tests` for the same calls, including the permission denial
  for `delete-entries` without `entries.delete`, and time spent in host
  calls not counting against the budget.

### Phase 4: SDK, example and docs

- `wit/kiki-plugin.wit` in the repository, and a small Rust guest SDK at
  `sdk/rust/kiki-plugin`. The SDK wraps `wit-bindgen` with a `Plugin` trait
  whose event methods have default bodies, an `export!` macro, typed config
  through `serde`, and a `store` helper that does JSON for you.
- `examples/wasm/filter`: the bundled `filter` plugin ported to Rust. CI
  builds it for `wasm32-wasip2` and runs it through the same cases as
  `src/plugins/filter_tests.rs`. That is the parity test, and it shows
  `regex` working inside the guest.
- The flake gains the `wasm32-wasip2` target and `wasm-tools` in the dev
  shell, and a `checks` entry that builds the example.
- Guide: a new `book/src/writing-wasm-plugins.md` linked from
  `SUMMARY.md` and from the top of `src/docs/scripting.md`. It covers the
  toolchain, the SDK, a walkthrough, limits, and how the API maps to the
  Lua one. Update `book/src/threat-model.md` with the WASM engine.
  `settings.md` doesn't change unless a server setting is added.

### Phase 5: Hardening and release

- Metrics: add an `engine` label to `kiki_plugin_execution_duration_seconds`
  and `kiki_plugin_executions_total`, and add
  `kiki_plugin_wasm_compile_seconds` and `kiki_plugin_wasm_traps_total`.
  Add a panel to `hosts/nixdev/grafana/kiki.json` in homelab.
- A fuzz target that loads arbitrary bytes as a component inside the
  child. It must fail cleanly and never kill the host.
- Turn on `wasm-plugins` by default if Phase 0's numbers allow it.
- homelab: no config change is needed, since `MemoryDenyWriteExecute` stays
  on. Optionally deploy the example plugin on nixdev for a release cycle
  before announcing the feature.

## Risks and open questions

- **Binary size and build time.** Wasmtime is a large dependency and will
  make fat-LTO release builds noticeably slower. A feature flag limits the
  cost to builds that want it. Phase 0 measures it.
- **Pulley maturity and speed.** Pulley is newer than Wasmtime's native
  backends. If it turns out too slow or too new, wasmi is the fallback.
- **Guest languages.** Rust is first-class. TinyGo, C and Zig should work
  with no extra work on our side. Python and JS need larger component size
  limits, which we would revisit only if someone asks.
- **API drift between engines.** Every new `kiki.*` call now has to land in
  both Lua and WIT. Proposal: a test that lists `ServiceCall` variants and
  fails if one has no WIT counterpart.
- **Per-plugin memory limit.** Should `memory_limit_mib` exist, or should
  WASM plugins share the fixed 16 MiB Lua uses? It is proposed here because
  Rust guests with `regex` tables can use more memory than equivalent Lua.
- **Compile in the child or the server?** Compiling in the child keeps
  Cranelift's attack surface sandboxed. That is the proposal. The cost is
  that a large component's compile time counts against the IPC timeout.
