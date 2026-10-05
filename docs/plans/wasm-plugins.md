# Plan: WebAssembly plugins

Status: implemented on `claude/wasm-plugin-support-plan-f791i9`, apart from
the follow-ups listed at the end. The user guide's
[Writing WebAssembly plugins](../../book/src/writing-wasm-plugins.md) chapter
describes the result for plugin authors. This document records the design
and the decisions behind it.

## Goal

A plugin can be a WebAssembly component instead of a Lua script. It has the
same events, host calls, permissions, config, settings, reload behaviour and
failure semantics as a Lua plugin. Lua and WebAssembly plugins run together,
in plugin order.

## Decisions

### Runtime: Wasmtime with Cranelift, compiled to native code

The first draft of this plan proposed an interpreter (Wasmtime's Pulley, or
wasmi), so that every Kiki process could keep refusing writable and
executable memory. We chose the JIT instead, for speed and maturity. That
meant giving up that protection where the JIT runs:

- `apply_mdwe` (`src/sandbox/linux.rs`) skips `PR_SET_MDWE` for the script
  host profile when the `wasm-plugins` feature is on. Every other process
  still refuses such memory. The server starts the script host before it
  installs `PR_SET_MDWE` itself, so the host doesn't inherit it.
- systemd's `MemoryDenyWriteExecute=` is inherited by every process of a
  service and can't be lifted, so it was removed from `dist/kiki.service`,
  `dist/kiki-user.service`, the unit `kiki systemd` generates, and
  `nix/module.nix`. Deployments with their own units (the homelab's
  `hosts/nixdev/kiki-web.nix`) must drop it too. If the script host finds
  the memory refused anyway, it logs a warning saying so.
- `book/src/threat-model.md` describes the trade.

Measured on this branch: the example `hide-matching` plugin (Rust, `regex`,
about 1 MiB) handles an entry in about 15 µs.

### Interface: the Component Model, from one WIT file

`wit/kiki-plugin.wit` is the source of truth. The host reads it with
`wasmtime::component::bindgen!`, and the Rust SDK with `wit_bindgen::generate!`.
A plugin exports `init(config) -> result<list<event-kind>, string>` and one
function per event. Scans and timers return ids instead of taking closures
(`start-scan` and `on-scan-entry`/`on-scan-done`, `every` and `on-timer`).
Host imports map one-to-one onto the existing `ServiceCall`s, so the server
side, including its permission checks, didn't change.

The package is versioned (`kiki:plugin@0.1.0`). An incompatible change
needs a new version, with the host keeping bindings for each version it
supports.

### Where it runs: the existing script host, behind a composite runner

`CompositeRunner` (`src/scripting/composite.rs`) splits plugins into runs of
consecutive plugins with the same engine, builds a `LuaScriptRunner` or
`WasmScriptRunner` for each run, and dispatches to the runs in order. Both
the script host and the in-process path (`--no-script-isolation`, tests)
use it. With only Lua plugins it is one run, and behaves exactly as before.

### WASI: randomness and clocks only

`wasmtime-wasi` needs tokio ≥ 1.51, but Kiki pins tokio below 1.45, so it
isn't used. Instead, `src/scripting/wasm_wasi.rs` defines `wasi:random/*`
and `wasi:clocks/{wall,monotonic}-clock`'s `now`/`resolution`, for whichever
WASI 0.2.x a component imports. Libraries call these without being asked:
`HashMap`'s default hasher, which `regex` uses, seeds itself from them.
Every other import is defined as a function that traps (and resources as
stubs), so a component built for `wasm32-wasip2` loads, and traps only if it
touches files, the network, the environment or the standard streams.
Wasmtime's own `Linker::define_unknown_imports_as_traps` isn't used: in
Wasmtime 45 it refuses interfaces the linker already partly defines.

### Rust version

CI builds with the nixpkgs toolchain, rustc 1.93, so Wasmtime is pinned to
45, the newest release that supports it (46 and later need 1.94 or newer),
with the matching `wit-component` 0.248. Moving to a newer Wasmtime means
updating nixpkgs first.

### Components across the IPC channel

A `Reload` frame is capped at 8 MiB, so components travel separately.
`HostRequest::PutComponent` carries one component (each is capped at
`MAX_WASM_COMPONENT_BYTES`, 7 MiB). The child compiles it and caches it by
BLAKE3 hash. `Reload` then names each component by hash only. The server
remembers which hashes the child has, so a reload that changes only config
sends and compiles nothing. A successful reload evicts unused components on
both sides. `PutComponent` also carries the plugin's name, for its compile
errors.

The server waits `IPC_TIMEOUT` (10 s) for each frame from the script host,
and kills it for good when the wait runs out: the server can't start a new
one once its own sandbox denies `execve`. Loading WebAssembly plugins is
bounded but can take longer than that, so the wait grows with the work
(`request_timeout` in `src/process/script_host.rs`):

- `PutComponent`: 10 s more per MiB of component, since compiling can't be
  interrupted but takes time in proportion to size;
- `Reload`: one `WASM_LOAD_BUDGET` (5 s) more per WebAssembly plugin, which
  instantiating and `init` share;
- anything else, while WebAssembly plugins are loaded: one
  `WASM_LOAD_BUDGET` more, for restarting a plugin that trapped.

### Limits and failures

These match the Lua engine where they can:

- Time: the plugin's `TimeBudget`, enforced with epoch interruption (a
  thread ticks every 5 ms). Time spent waiting on host calls is given back,
  up to 1 s per call. Instantiating a plugin and its `init` share 5 s.
- Resources: 16 MiB of linear memory and 512 KiB of stack per plugin, plus
  limits on tables and instances.
- Isolation: each plugin has its own `Store`, so plugins share no memory.
- Failures: a trap passes the entry through unmodified, keeps the fetch
  schedule's wait, or is logged, as for Lua.
- After a trap, the plugin is instantiated afresh and `init` runs again,
  which restarts its timers and ends its scans; timers of the old instance
  that were still due are dropped. The restart waits for the next dispatch
  the plugin is part of, and a dispatch restarts at most one plugin, so a
  request takes at most one load budget longer. More than 5 traps in 10
  minutes disables the plugin until the next reload.

A per-plugin `memory_limit_mib` was considered and left out: 16 MiB has been
enough for the plugins written so far.

## What changed, by file

| Area | Files |
|---|---|
| Interface | `wit/kiki-plugin.wit` |
| Engine | `src/scripting/wasm.rs`, `wasm_wasi.rs`, `wasm_tests.rs` |
| Mixed engines | `src/scripting/composite.rs`; `ScriptSource::component`, `WasmComponent`, `EventSet::union` in `src/scripting/mod.rs` |
| Manifests and discovery | `PluginEngine::Wasm`, `load_wasm_source`, `load_sources` across engines in `src/plugins/mod.rs` |
| Script host | `PutComponent` in `src/process/ipc.rs`; component cache and composite runner in `src/process/script_host.rs` |
| Sandbox | `src/sandbox/linux.rs`, `src/sandbox.rs`; systemd units and the NixOS module |
| SDK and examples | `sdk/rust/kiki-plugin`, `examples/wasm/hide-matching`, `tests/wasm-fixture` (rebuilt with `tools/build-wasm-fixture.sh`) |
| Docs | `book/src/writing-wasm-plugins.md`, `src/docs/scripting.md`, `book/src/threat-model.md` |
| Build | `wasm-plugins` Cargo feature (default on); the Nix source filter includes `wit/` and the fixture |

Tests: `src/scripting/wasm_tests.rs` runs the fixture through every event,
host call, limit and failure path, and mixes it with Lua plugins.
`tests/sandbox.rs`'s `an_isolated_wasm_plugin_transforms_an_ingested_entry`
runs a WebAssembly plugin and a Lua plugin together in the real sandboxed
script host.

## Follow-ups

- Metrics: an `engine` label on the plugin execution metrics, and counters
  for compile time and traps.
- A CI job that builds the SDK, the example and the fixture for
  `wasm32-wasip2`, and checks the committed `fixture.wasm` is current.
- A fuzz target that feeds arbitrary bytes to the component compiler in the
  script host.
- Publishing the SDK crate. Its WIT path is relative to this repository
  today.
- The `kiki web` plugin page shows the engine, but has no WebAssembly-specific
  information (such as the component's size or hash).
