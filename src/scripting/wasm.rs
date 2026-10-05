//! WebAssembly-based [`ScriptRunner`] implementation.
//!
//! A WebAssembly plugin is a [component] targeting the `plugin` world of
//! `wit/kiki-plugin.wit`: it imports the `host` interface, Kiki's counterpart of the `kiki`
//! table Lua plugins get, and exports an `init` function and one function per event. A core
//! module carrying the world's type information, as `wit-bindgen` builds for
//! `wasm32-unknown-unknown`, is accepted too, and turned into a component when it is
//! compiled.
//!
//! [component]: https://component-model.bytecodealliance.org/
//!
//! Of WASI, Kiki provides only randomness and clocks (see the `wasm_wasi` module): no
//! files, network, environment or standard streams. Toolchains that import more, such as
//! Rust's for `wasm32-wasip2`, are accommodated by defining every import Kiki does not
//! know as a function that traps: a plugin built with them loads, and fails only if it
//! calls one.
//!
//! # Architecture
//!
//! Components are compiled to native code with [Wasmtime] and Cranelift, once per distinct
//! component: compiled components are kept in a cache keyed by the BLAKE3 hash of their
//! bytes, so a reload that changes only a plugin's config recompiles nothing, and the
//! script host can be sent a component's bytes once and then refer to it by hash (see
//! [`put_component`]). Compiling to native code needs memory that is first writable and then
//! executable, so the script host does not refuse it as the other Kiki processes do; see
//! [`crate::sandbox`].
//!
//! [Wasmtime]: https://wasmtime.dev/
//!
//! Each plugin gets a [`Store`] and component instance of its own, so plugins share no
//! memory, whatever permissions they ask for. A plugin's `init` export is called with its
//! config when it loads, and returns the events the plugin handles; only those are
//! delivered to it. Its calls to the server go through [`ScriptServices`], tagged with its
//! name, so the server checks its permissions just as it does a Lua plugin's.
//!
//! # Safety controls
//!
//! Every call into a plugin is bounded by:
//!
//! * The plugin's [`TimeBudget`], enforced with Wasmtime's epoch interruption: a thread
//!   advances the engine's epoch every [`EPOCH_TICK`], and at each tick a running plugin
//!   checks its deadline. As for Lua, time spent waiting on the server is given back, up to
//!   [`MAX_CALL_ALLOWANCE`] per call. `init` runs under [`LOAD_BUDGET`].
//! * [`WASM_MEMORY_LIMIT_BYTES`] of linear memory per plugin, and limits on tables and
//!   instances, enforced with [`StoreLimits`].
//! * [`MAX_WASM_STACK`] bytes of stack.
//!
//! A call that traps — runs out of time or memory, or hits `unreachable`, as a Rust panic
//! does — fails as a Lua handler's error does: the entry passes through unmodified, the
//! wait is kept, or the failure is logged. Since a trap can leave the plugin's memory in
//! any state, the plugin is then started afresh: instantiated again, and its `init`
//! called again, which starts its timers again. Its scans end. A plugin that traps more
//! than [`MAX_TRAPS`] times in [`TRAP_WINDOW`] is disabled until plugins next reload.

use super::lua::MAX_TIMER_INTERVAL;
use super::{
    parse_script_config, restore_read_only, ContentChange, DeleteFilter, Event, EventPayload,
    EventSet, FeedEntry, FetchSchedule, ScanOptions, ScanSummary, ScheduleDecision, ScriptRunner,
    ScriptServices, ScriptSource, ServiceCall, ServiceReply, TimeBudget, WasmComponent, TIMER_TICK,
};
use crate::db::tags::{is_reserved_tag_name, SystemTag};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use thiserror::Error;
use tracing::{debug, error, warn};
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, Store, StoreContextMut, StoreLimits, StoreLimitsBuilder};

mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "plugin",
        imports: { default: trappable },
    });
}

use bindings::kiki::plugin::host::Host;
use bindings::kiki::plugin::types as wit;
use bindings::Plugin as Bindings;

/// Linear memory each plugin may use.
pub const WASM_MEMORY_LIMIT_BYTES: usize = super::lua::SCRIPT_MEMORY_LIMIT_BYTES;

/// Stack each plugin may use.
pub const MAX_WASM_STACK: usize = 512 * 1024;

/// How often the engine's epoch advances, and so how often a running plugin checks its
/// time budget.
pub const EPOCH_TICK: Duration = Duration::from_millis(5);

/// The time budget of a plugin's `init`, which runs when plugins load.
pub const LOAD_BUDGET: Duration = Duration::from_secs(5);

/// How much of the time a call spends waiting on the server does not count against its
/// time budget, as for Lua.
pub const MAX_CALL_ALLOWANCE: Duration = Duration::from_secs(1);

/// How many traps a plugin may hit in [`TRAP_WINDOW`] before it is disabled until plugins
/// next reload.
pub const MAX_TRAPS: usize = 5;

/// See [`MAX_TRAPS`].
pub const TRAP_WINDOW: Duration = Duration::from_secs(10 * 60);

/// Most tables, and most elements in a table, a plugin may have.
const MAX_TABLES: usize = 16;
const MAX_TABLE_ELEMENTS: usize = 64 * 1024;

/// Most component and core instances one plugin's component may create.
const MAX_INSTANCES: usize = 64;

/// Errors that keep WebAssembly plugins from loading.
#[derive(Debug, Error)]
pub enum WasmError {
    /// The WebAssembly engine could not be set up.
    #[error("the WebAssembly engine could not be started: {0}")]
    Engine(String),

    /// A component could not be compiled.
    #[error("failed to compile WebAssembly plugin '{plugin}': {message}")]
    Compile { plugin: String, message: String },

    /// A source refers to a component by hash that was never compiled.
    #[error("WebAssembly plugin '{plugin}': component {hash} was not sent to the script host")]
    MissingComponent { plugin: String, hash: String },

    /// A component could not be instantiated, such as because it does not target the
    /// `plugin` world.
    #[error("failed to instantiate WebAssembly plugin '{plugin}': {message}")]
    Instantiate { plugin: String, message: String },

    /// A plugin's `init` failed, trapped or returned an error.
    #[error("WebAssembly plugin '{plugin}' failed to initialize: {message}")]
    Init { plugin: String, message: String },

    /// A plugin's config is not a JSON object.
    #[error(transparent)]
    InvalidConfig(#[from] super::ScriptConfigError),
}

/// The engine every plugin in this process is compiled for and runs in, and the thread
/// that advances its epoch.
fn engine() -> Result<&'static Engine, WasmError> {
    static ENGINE: OnceLock<Result<Engine, String>> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            let mut config = Config::new();
            config.wasm_component_model(true);
            config.epoch_interruption(true);
            config.max_wasm_stack(MAX_WASM_STACK);
            let engine = Engine::new(&config).map_err(|e| format!("{e:?}"))?;
            let ticker = engine.weak();
            std::thread::Builder::new()
                .name("kiki-wasm-epoch".to_string())
                .spawn(move || {
                    while let Some(engine) = ticker.upgrade() {
                        engine.increment_epoch();
                        drop(engine);
                        std::thread::sleep(EPOCH_TICK);
                    }
                })
                .map_err(|e| format!("starting the epoch thread: {e}"))?;
            Ok(engine)
        })
        .as_ref()
        .map_err(|e| WasmError::Engine(e.clone()))
}

/// Compiled components, by the hash of their bytes.
fn cache() -> &'static Mutex<HashMap<[u8; 32], Component>> {
    static CACHE: OnceLock<Mutex<HashMap<[u8; 32], Component>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Whether `bytes` are a core module, rather than a component: the two share the `\0asm`
/// magic, and differ in the version and layer that follow it.
fn is_core_module(bytes: &[u8]) -> bool {
    bytes.get(4..8) == Some(&[1, 0, 0, 0])
}

/// Compile `bytes`, a component or a core module carrying the `plugin` world's type
/// information, for the plugin named `plugin`.
fn compile_bytes(plugin: &str, bytes: &[u8]) -> Result<Component, WasmError> {
    let err = |message: String| WasmError::Compile {
        plugin: plugin.to_string(),
        message,
    };
    let engine = engine()?;
    let componentized;
    let bytes = if is_core_module(bytes) {
        componentized = wit_component::ComponentEncoder::default()
            .validate(true)
            .module(bytes)
            .and_then(|encoder| encoder.encode())
            .map_err(|e| err(format!("turning the core module into a component: {e:#}")))?;
        &componentized[..]
    } else {
        bytes
    };
    Component::new(engine, bytes).map_err(|e| err(format!("{e:?}")))
}

/// Compile `component` for the plugin named `plugin`, or find it compiled already.
///
/// A component whose bytes were left out is looked up by its hash; see [`put_component`].
/// Only components given to [`put_component`] are kept, so that a runner built in the
/// server, which always has the bytes, keeps nothing alive once it is dropped.
fn compile(plugin: &str, component: &WasmComponent) -> Result<Component, WasmError> {
    if let Some(compiled) = cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&component.hash)
    {
        return Ok(compiled.clone());
    }
    if component.bytes.is_empty() {
        return Err(WasmError::MissingComponent {
            plugin: plugin.to_string(),
            hash: component.hash_hex(),
        });
    }
    compile_bytes(plugin, &component.bytes)
}

/// Compile `component` and keep it, so that sources can refer to it by hash alone.
///
/// The script host calls this for each component the server sends it, before the sources
/// that use it; see [`crate::process::script_host::ScriptHost::reload`].
///
/// # Errors
///
/// Returns [`WasmError::Compile`] if the component cannot be compiled.
pub fn put_component(component: &WasmComponent) -> Result<(), WasmError> {
    let compiled = compile_bytes("(uploaded)", &component.bytes)?;
    cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(component.hash, compiled);
    Ok(())
}

/// Forget the compiled components whose hashes are not in `keep`.
pub fn retain_components(keep: &HashSet<[u8; 32]>) {
    cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|hash, _| keep.contains(hash));
}

/// A timer a plugin started with `every`.
struct Timer {
    id: u32,
    every: Duration,
    next: Instant,
}

/// The time budget of the call into a plugin in progress, if any. See the Lua engine's
/// budget, which this mirrors.
#[derive(Default)]
struct CallBudget {
    deadline: Option<Instant>,
    allowance: Duration,
}

impl CallBudget {
    fn start(&mut self, limit: Option<Duration>) {
        self.deadline = limit.map(|l| Instant::now() + l);
        self.allowance = MAX_CALL_ALLOWANCE;
    }

    fn stop(&mut self) {
        self.deadline = None;
    }

    fn expired(&self) -> bool {
        self.deadline.is_some_and(|d| Instant::now() >= d)
    }

    fn give_back(&mut self, waited: Duration) {
        if let Some(deadline) = self.deadline.as_mut() {
            let credit = waited.min(self.allowance);
            self.allowance -= credit;
            *deadline += credit;
        }
    }
}

/// What a plugin's [`Store`] holds: what its calls to the host need.
struct State {
    plugin: Arc<str>,
    services: Option<Arc<dyn ScriptServices>>,
    limits: StoreLimits,
    budget: CallBudget,
    /// Set while `init` runs, when scans cannot start.
    loading: bool,
    timers: Vec<Timer>,
    next_timer: u32,
    /// The scans the plugin has started that have not finished.
    scans: HashSet<u64>,
}

impl State {
    /// Make `call` to the server on the plugin's behalf.
    ///
    /// Fails the call, rather than trapping, when the server refuses it; traps when the
    /// call has used up its time budget, so that a plugin calling the server in a loop is
    /// still stopped.
    fn service(
        &mut self,
        name: &str,
        call: ServiceCall,
    ) -> wasmtime::Result<Result<ServiceReply, String>> {
        let Some(services) = self.services.clone() else {
            return Ok(Err(format!("{name}: not available in this runner")));
        };
        if self.budget.expired() {
            return Err(wasmtime::Error::msg(format!(
                "{name}: the handler has used up its time budget"
            )));
        }
        let start = Instant::now();
        let result = services.call(&self.plugin, call);
        self.budget.give_back(start.elapsed());
        Ok(result.map_err(|e| format!("{name}: {e}")))
    }
}

fn unexpected<T>(name: &str, reply: ServiceReply) -> wasmtime::Result<Result<T, String>> {
    Ok(Err(format!("{name}: unexpected reply {reply:?}")))
}

impl wit::Host for State {}

impl Host for State {
    fn log(&mut self, level: wit::Level, message: String) -> wasmtime::Result<()> {
        let plugin = &*self.plugin;
        match level {
            wit::Level::Debug => tracing::debug!(target: "kiki::wasm", plugin, "{message}"),
            wit::Level::Info => tracing::info!(target: "kiki::wasm", plugin, "{message}"),
            wit::Level::Warn => tracing::warn!(target: "kiki::wasm", plugin, "{message}"),
            wit::Level::Error => tracing::error!(target: "kiki::wasm", plugin, "{message}"),
        }
        Ok(())
    }

    fn store_get(&mut self, key: String) -> wasmtime::Result<Result<Option<String>, String>> {
        match self.service("store-get", ServiceCall::StoreGet { key })? {
            Ok(ServiceReply::Value(value)) => Ok(Ok(value)),
            Ok(other) => unexpected("store-get", other),
            Err(e) => Ok(Err(e)),
        }
    }

    fn store_set(
        &mut self,
        key: String,
        value: Option<String>,
    ) -> wasmtime::Result<Result<(), String>> {
        if let Some(value) = &value {
            if let Err(e) = serde_json::from_str::<serde_json::Value>(value) {
                return Ok(Err(format!("store-set: the value is not JSON: {e}")));
            }
        }
        match self.service("store-set", ServiceCall::StoreSet { key, value })? {
            Ok(ServiceReply::Done) => Ok(Ok(())),
            Ok(other) => unexpected("store-set", other),
            Err(e) => Ok(Err(e)),
        }
    }

    fn tag_entry(&mut self, id: i64, name: String) -> wasmtime::Result<Result<bool, String>> {
        set_entry_tag(self, "tag-entry", id, name, true)
    }

    fn untag_entry(&mut self, id: i64, name: String) -> wasmtime::Result<Result<bool, String>> {
        set_entry_tag(self, "untag-entry", id, name, false)
    }

    fn start_scan(&mut self, options: wit::ScanOptions) -> wasmtime::Result<Result<u64, String>> {
        if self.loading {
            return Ok(Err(
                "start-scan: scans cannot start while plugins are loading; \
                 start them from on-plugin-load"
                    .to_string(),
            ));
        }
        let options = ScanOptions {
            feed_id: options.feed_id,
            since: options.since,
            include_hidden: options.include_hidden,
        };
        match self.service("start-scan", ServiceCall::StartScan { options })? {
            Ok(ServiceReply::ScanStarted(id)) => {
                // The scan cannot reach the plugin before this: its batches are dispatched
                // through the runner, which is busy until this call returns.
                self.scans.insert(id);
                Ok(Ok(id))
            }
            Ok(other) => unexpected("start-scan", other),
            Err(e) => Ok(Err(e)),
        }
    }

    fn delete_entries(
        &mut self,
        filter: wit::DeleteFilter,
    ) -> wasmtime::Result<Result<u64, String>> {
        let keep_tagged = match filter.keep_tagged {
            Some(names) => names,
            None => DeleteFilter::default().keep_tagged,
        };
        for name in &keep_tagged {
            if is_reserved_tag_name(name) && !SystemTag::ALL.iter().any(|t| t.name() == name) {
                return Ok(Err(format!(
                    "delete-entries: {name:?} in 'keep-tagged' is not a system tag"
                )));
            }
        }
        let filter = DeleteFilter {
            dropped_before: filter.dropped_before,
            feed_id: filter.feed_id,
            published_before: filter.published_before,
            keep_tagged,
        };
        match self.service("delete-entries", ServiceCall::DeleteEntries { filter })? {
            Ok(ServiceReply::Deleted(count)) => Ok(Ok(count)),
            Ok(other) => unexpected("delete-entries", other),
            Err(e) => Ok(Err(e)),
        }
    }

    fn get_feed(&mut self, id: i64) -> wasmtime::Result<Result<Option<wit::Feed>, String>> {
        match self.service("get-feed", ServiceCall::GetFeed { feed_id: id })? {
            Ok(ServiceReply::Feed(feed)) => Ok(Ok(feed.map(|f| wit::Feed {
                id: f.id,
                url: f.url,
                title: f.title,
            }))),
            Ok(other) => unexpected("get-feed", other),
            Err(e) => Ok(Err(e)),
        }
    }

    fn every(&mut self, secs: u64) -> wasmtime::Result<Result<u32, String>> {
        let (min, max) = (TIMER_TICK.as_secs(), MAX_TIMER_INTERVAL.as_secs());
        if !(min..=max).contains(&secs) {
            return Ok(Err(format!(
                "every: the interval must be between {min} and {max} seconds, not {secs}"
            )));
        }
        let every = Duration::from_secs(secs);
        let id = self.next_timer;
        self.next_timer = self.next_timer.wrapping_add(1);
        self.timers.push(Timer {
            id,
            every,
            next: Instant::now() + every,
        });
        Ok(Ok(id))
    }
}

fn set_entry_tag(
    state: &mut State,
    name: &str,
    entry_id: i64,
    tag: String,
    present: bool,
) -> wasmtime::Result<Result<bool, String>> {
    let call = ServiceCall::SetEntryTag {
        entry_id,
        tag,
        present,
    };
    match state.service(name, call)? {
        Ok(ServiceReply::Changed(changed)) => Ok(Ok(changed)),
        Ok(other) => unexpected(name, other),
        Err(e) => Ok(Err(e)),
    }
}

/// A plugin's running instance.
struct Live {
    store: Store<State>,
    bindings: Bindings,
}

/// A loaded plugin.
struct WasmPlugin {
    name: Arc<str>,
    budget: TimeBudget,
    config: String,
    component: Component,
    /// The events the plugin's `init` said it handles.
    events: EventSet,
    /// `None` once the plugin has been disabled for trapping too often.
    live: Option<Live>,
    /// When the plugin last trapped, oldest first, within [`TRAP_WINDOW`].
    traps: VecDeque<Instant>,
}

/// Instantiate `component` as the plugin `name` and call its `init` with `config`.
fn instantiate(
    linker: &Linker<State>,
    component: &Component,
    name: &Arc<str>,
    config: &str,
    services: Option<Arc<dyn ScriptServices>>,
) -> Result<(Live, EventSet), WasmError> {
    let engine = engine()?;
    let state = State {
        plugin: name.clone(),
        services,
        limits: StoreLimitsBuilder::new()
            .memory_size(WASM_MEMORY_LIMIT_BYTES)
            .table_elements(MAX_TABLE_ELEMENTS)
            .tables(MAX_TABLES)
            .instances(MAX_INSTANCES)
            .memories(MAX_INSTANCES)
            .build(),
        budget: CallBudget::default(),
        loading: true,
        timers: Vec::new(),
        next_timer: 0,
        scans: HashSet::new(),
    };
    let mut store = Store::new(engine, state);
    store.limiter(|state| &mut state.limits);
    store.set_epoch_deadline(1);
    store.epoch_deadline_callback(|ctx: StoreContextMut<'_, State>| {
        if ctx.data().budget.expired() {
            Ok(wasmtime::UpdateDeadline::Interrupt)
        } else {
            Ok(wasmtime::UpdateDeadline::Continue(1))
        }
    });

    // Instantiating runs the component's start functions, which get the load budget too.
    store.data_mut().budget.start(Some(LOAD_BUDGET));
    let bindings = Bindings::instantiate(&mut store, component, linker).map_err(|e| {
        WasmError::Instantiate {
            plugin: name.to_string(),
            message: format!("{e:?}"),
        }
    })?;
    store.data_mut().budget.stop();
    let mut live = Live { store, bindings };
    let init_err = |message: String| WasmError::Init {
        plugin: name.to_string(),
        message,
    };
    let kinds = call(&mut live, Some(LOAD_BUDGET), |b, s| b.call_init(s, config))
        .map_err(init_err)?
        .map_err(init_err)?;
    live.store.data_mut().loading = false;

    let mut events = EventSet::default();
    for kind in kinds {
        events.insert(event_from_kind(kind));
    }
    Ok((live, events))
}

/// Call into `live` with `f`, under a time budget of `limit`.
///
/// Returns the trap or error as a message on failure, which leaves the instance unfit to
/// call again.
fn call<R>(
    live: &mut Live,
    limit: Option<Duration>,
    f: impl FnOnce(&Bindings, &mut Store<State>) -> wasmtime::Result<R>,
) -> Result<R, String> {
    live.store.data_mut().budget.start(limit);
    live.store.set_epoch_deadline(1);
    let result = f(&live.bindings, &mut live.store);
    let expired = live.store.data().budget.expired();
    live.store.data_mut().budget.stop();
    result.map_err(|e| match limit {
        Some(limit) if expired => format!(
            "exceeded {} time budget",
            TimeBudget::Millis(limit.as_millis() as u64)
        ),
        _ => format!("{e:?}"),
    })
}

fn event_from_kind(kind: wit::EventKind) -> Event {
    match kind {
        wit::EventKind::EntryParsed => Event::EntryParsed,
        wit::EventKind::EntryIngest => Event::EntryIngest,
        wit::EventKind::FetchSuccess => Event::FetchSuccess,
        wit::EventKind::FetchError => Event::FetchError,
        wit::EventKind::FeedAdded => Event::FeedAdded,
        wit::EventKind::FeedRemoved => Event::FeedRemoved,
        wit::EventKind::PluginLoad => Event::PluginLoad,
        wit::EventKind::FetchSchedule => Event::FetchSchedule,
    }
}

fn entry_to_wit(entry: FeedEntry) -> wit::Entry {
    wit::Entry {
        id: entry.id,
        feed_id: entry.feed_id,
        syndication_format: entry.syndication_format,
        guid: entry.guid,
        published_at: entry.published_at,
        title: entry.title,
        url: entry.url,
        content: entry.content,
        authors: entry.authors,
        categories: entry.categories,
        tags: entry.tags,
        cache_assets: entry.cache_assets,
    }
}

/// The entry a handler returned, with the fields it may not change taken from `original`.
fn entry_from_wit(entry: wit::Entry, original: &FeedEntry) -> FeedEntry {
    let mut modified = FeedEntry {
        id: entry.id,
        feed_id: entry.feed_id,
        syndication_format: entry.syndication_format,
        guid: entry.guid,
        published_at: entry.published_at,
        title: entry.title,
        url: entry.url,
        content: entry.content,
        authors: entry.authors,
        categories: entry.categories,
        tags: entry.tags,
        cache_assets: entry.cache_assets,
    };
    restore_read_only(&mut modified, original);
    modified
}

fn schedule_to_wit(schedule: &FetchSchedule) -> wit::FetchSchedule {
    wit::FetchSchedule {
        feed_id: schedule.feed_id,
        status: schedule.status,
        change: match schedule.change {
            ContentChange::Changed => wit::ContentChange::Changed,
            ContentChange::Unchanged => wit::ContentChange::Unchanged,
            ContentChange::Unknown => wit::ContentChange::Unknown,
        },
        hint_secs: schedule.hint_secs,
        interval_secs: schedule.interval_secs,
        min_cadence_secs: schedule.min_cadence_secs,
        wait_secs: schedule.wait_secs,
    }
}

impl WasmPlugin {
    /// Call into the plugin with `f`, under its time budget, if it is running.
    ///
    /// On failure, logs `what` failed, and starts the plugin afresh, or disables it if it
    /// has failed too often. Returns `None` if the call failed or the plugin is disabled.
    fn call<R>(
        &mut self,
        linker: &Linker<State>,
        services: &Option<Arc<dyn ScriptServices>>,
        what: &str,
        f: impl FnOnce(&Bindings, &mut Store<State>) -> wasmtime::Result<R>,
    ) -> Option<R> {
        let live = self.live.as_mut()?;
        match call(live, self.budget.limit(), f) {
            Ok(result) => Some(result),
            Err(e) => {
                warn!(plugin = %self.name, error = %e, "{what} failed");
                self.restart(linker, services);
                None
            }
        }
    }

    /// Start the plugin afresh after a trap, unless it has trapped too often.
    fn restart(&mut self, linker: &Linker<State>, services: &Option<Arc<dyn ScriptServices>>) {
        self.live = None;
        let now = Instant::now();
        self.traps.push_back(now);
        while self
            .traps
            .front()
            .is_some_and(|t| now.duration_since(*t) > TRAP_WINDOW)
        {
            self.traps.pop_front();
        }
        if self.traps.len() > MAX_TRAPS {
            error!(
                plugin = %self.name,
                "plugin failed {} times in {} minutes; disabling it until plugins reload",
                self.traps.len(),
                TRAP_WINDOW.as_secs() / 60
            );
            return;
        }
        match instantiate(
            linker,
            &self.component,
            &self.name,
            &self.config,
            services.clone(),
        ) {
            Ok((live, events)) => {
                debug!(plugin = %self.name, "plugin restarted");
                self.live = Some(live);
                self.events = events;
            }
            Err(e) => error!(
                plugin = %self.name,
                error = %e,
                "plugin could not be restarted; disabling it until plugins reload"
            ),
        }
    }

    fn handles(&self, event: Event) -> bool {
        self.live.is_some() && self.events.contains(event)
    }

    fn has_scan(&self, scan_id: u64) -> bool {
        self.live
            .as_ref()
            .is_some_and(|l| l.store.data().scans.contains(&scan_id))
    }
}

/// Runs WebAssembly plugins in response to server events.
///
/// See the [module documentation](self).
pub struct WasmScriptRunner {
    /// The plugins, in the order they load in. Behind a mutex since a store is used by one
    /// call at a time, and to run one call at a time, as the Lua engine does.
    plugins: Mutex<Vec<WasmPlugin>>,
    linker: Linker<State>,
    services: Option<Arc<dyn ScriptServices>>,
}

impl WasmScriptRunner {
    /// Build a runner from WebAssembly plugins and their configs, answering the calls they
    /// make to the server with `services`.
    ///
    /// Every source must carry a [`WasmComponent`]. Each plugin is compiled (or found
    /// compiled already), instantiated, and its `init` called with its config.
    ///
    /// # Errors
    ///
    /// Returns an error if a config is not a JSON object, or a plugin fails to compile,
    /// instantiate or initialize.
    pub fn from_sources_with(
        sources: &[ScriptSource],
        services: Option<Arc<dyn ScriptServices>>,
    ) -> Result<Self, WasmError> {
        let engine = engine()?;
        let mut linker = Linker::new(engine);
        Bindings::add_to_linker::<State, HasSelf<State>>(&mut linker, |state| state)
            .map_err(|e| WasmError::Engine(format!("{e:?}")))?;

        let mut wasi_defined = HashSet::new();
        let mut plugins = Vec::with_capacity(sources.len());
        for source in sources {
            parse_script_config(&source.config)?;
            let name: Arc<str> = Arc::from(source.name.as_str());
            let component = match &source.component {
                Some(component) => compile(&source.name, component)?,
                None => {
                    return Err(WasmError::Compile {
                        plugin: source.name.clone(),
                        message: "the plugin has no WebAssembly component".to_string(),
                    })
                }
            };
            // Toolchains such as Rust's for `wasm32-wasip2` import WASI whether or not a
            // plugin uses it. Kiki defines the harmless parts; the rest trap if called.
            wasi::define(&mut linker, engine, &component, &mut wasi_defined).map_err(|e| {
                WasmError::Instantiate {
                    plugin: source.name.clone(),
                    message: format!("{e:?}"),
                }
            })?;
            linker
                .define_unknown_imports_as_traps(&component)
                .map_err(|e| WasmError::Instantiate {
                    plugin: source.name.clone(),
                    message: format!("{e:?}"),
                })?;
            let (live, events) =
                instantiate(&linker, &component, &name, &source.config, services.clone())?;
            plugins.push(WasmPlugin {
                name,
                budget: source.time_budget,
                config: source.config.clone(),
                component,
                events,
                live: Some(live),
                traps: VecDeque::new(),
            });
        }
        Ok(Self {
            plugins: Mutex::new(plugins),
            linker,
            services,
        })
    }

    fn plugins(&self) -> std::sync::MutexGuard<'_, Vec<WasmPlugin>> {
        self.plugins.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The events at least one plugin handles, and [`Event::Timer`] if a plugin has
    /// started a timer.
    pub fn subscriptions(&self) -> EventSet {
        let mut set = EventSet::default();
        for plugin in self.plugins().iter() {
            let Some(live) = &plugin.live else { continue };
            set = set.union(plugin.events);
            if !live.store.data().timers.is_empty() {
                set.insert(Event::Timer);
            }
        }
        set
    }

    /// Whether a plugin in this runner started the scan `scan_id`, which has not finished.
    pub fn has_scan(&self, scan_id: u64) -> bool {
        self.plugins().iter().any(|p| p.has_scan(scan_id))
    }

    /// Calls `on-timer` for each timer due by `now`, and schedules its next call, as the
    /// Lua engine does.
    fn run_timers(&self, now: Instant) {
        let mut plugins = self.plugins();
        for plugin in plugins.iter_mut() {
            let due: Vec<u32> = match plugin.live.as_mut() {
                None => continue,
                Some(live) => live
                    .store
                    .data_mut()
                    .timers
                    .iter_mut()
                    .filter(|t| t.next <= now)
                    .map(|timer| {
                        timer.next += timer.every;
                        if timer.next <= now {
                            timer.next = now + timer.every;
                        }
                        timer.id
                    })
                    .collect(),
            };
            for id in due {
                plugin.call(&self.linker, &self.services, "timer handler", |b, s| {
                    b.call_on_timer(s, id)
                });
            }
        }
    }
}

impl ScriptRunner for WasmScriptRunner {
    fn handles(&self, event: Event) -> bool {
        self.subscriptions().contains(event)
    }

    fn dispatch_transform_entry(&self, entry: FeedEntry) -> anyhow::Result<Option<FeedEntry>> {
        let mut current = entry;
        for plugin in self.plugins().iter_mut() {
            if !plugin.handles(Event::EntryIngest) {
                continue;
            }
            let input = entry_to_wit(current.clone());
            let returned = plugin.call(
                &self.linker,
                &self.services,
                "entry.ingest handler",
                |b, s| b.call_on_entry_ingest(s, &input),
            );
            match returned {
                // A failed handler passes the entry through unmodified.
                None => {}
                Some(None) => return Ok(None),
                Some(Some(entry)) => current = entry_from_wit(entry, &current),
            }
        }
        Ok(Some(current))
    }

    fn dispatch_schedule(
        &self,
        mut schedule: FetchSchedule,
    ) -> anyhow::Result<Option<ScheduleDecision>> {
        let mut decision = None;
        for plugin in self.plugins().iter_mut() {
            if !plugin.handles(Event::FetchSchedule) {
                continue;
            }
            let input = schedule_to_wit(&schedule);
            let returned = plugin.call(
                &self.linker,
                &self.services,
                "fetch.schedule handler",
                |b, s| b.call_on_fetch_schedule(s, input),
            );
            if let Some(Some(wait_secs)) = returned {
                schedule.wait_secs = wait_secs;
                decision = Some(ScheduleDecision {
                    wait_secs,
                    plugin: plugin.name.to_string(),
                });
            }
        }
        Ok(decision)
    }

    fn dispatch_observe(&self, event: Event, payload: EventPayload) {
        if event == Event::Timer {
            self.run_timers(Instant::now());
            return;
        }
        let what = format!("{} handler", event.name());
        for plugin in self.plugins().iter_mut() {
            if !plugin.handles(event) {
                continue;
            }
            let (linker, services) = (&self.linker, &self.services);
            match (&payload, event) {
                (EventPayload::Entry(entry), Event::EntryParsed) => {
                    let entry = entry_to_wit(entry.clone());
                    plugin.call(linker, services, &what, |b, s| {
                        b.call_on_entry_parsed(s, &entry)
                    });
                }
                (
                    EventPayload::FetchSuccess {
                        feed_id,
                        status,
                        url,
                        content_length,
                    },
                    _,
                ) => {
                    let event = wit::FetchSuccess {
                        feed_id: *feed_id,
                        status: *status,
                        url: url.clone(),
                        content_length: *content_length,
                    };
                    plugin.call(linker, services, &what, |b, s| {
                        b.call_on_fetch_success(s, &event)
                    });
                }
                (
                    EventPayload::FetchError {
                        feed_id,
                        kind,
                        status,
                        message,
                        retry_after,
                    },
                    _,
                ) => {
                    let event = wit::FetchError {
                        feed_id: *feed_id,
                        kind: kind.to_string(),
                        status: *status,
                        message: message.clone(),
                        retry_after: *retry_after,
                    };
                    plugin.call(linker, services, &what, |b, s| {
                        b.call_on_fetch_error(s, &event)
                    });
                }
                (EventPayload::Feed { id, url, title }, Event::FeedAdded | Event::FeedRemoved) => {
                    let feed = wit::FeedEvent {
                        id: *id,
                        url: url.clone(),
                        title: title.clone(),
                    };
                    plugin.call(linker, services, &what, |b, s| {
                        if event == Event::FeedAdded {
                            b.call_on_feed_added(s, &feed)
                        } else {
                            b.call_on_feed_removed(s, &feed)
                        }
                    });
                }
                (EventPayload::PluginLoad, Event::PluginLoad) => {
                    plugin.call(linker, services, &what, |b, s| b.call_on_plugin_load(s));
                }
                _ => warn!(
                    event = event.name(),
                    "event payload does not match the event; dropping it"
                ),
            }
        }
    }

    fn dispatch_scan(
        &self,
        scan_id: u64,
        entries: Vec<FeedEntry>,
    ) -> anyhow::Result<Option<Vec<Option<FeedEntry>>>> {
        let mut plugins = self.plugins();
        let Some(plugin) = plugins.iter_mut().find(|p| p.has_scan(scan_id)) else {
            return Ok(None);
        };
        let start = Instant::now();
        let mut results = Vec::with_capacity(entries.len());
        for entry in entries {
            // At least one entry is always handled, so the scan makes progress.
            if !results.is_empty() && start.elapsed() >= super::lua::SCAN_SLICE {
                break;
            }
            if !plugin.has_scan(scan_id) {
                // The plugin was restarted, which ended its scans.
                return Ok(None);
            }
            let input = entry_to_wit(entry.clone());
            let returned = plugin.call(&self.linker, &self.services, "scan handler", |b, s| {
                b.call_on_scan_entry(s, scan_id, &input)
            });
            results.push(returned.flatten().map(|e| entry_from_wit(e, &entry)));
        }
        Ok(Some(results))
    }

    fn finish_scan(&self, scan_id: u64, summary: Option<ScanSummary>) {
        let mut plugins = self.plugins();
        let Some(plugin) = plugins.iter_mut().find(|p| p.has_scan(scan_id)) else {
            return;
        };
        if let Some(live) = plugin.live.as_mut() {
            live.store.data_mut().scans.remove(&scan_id);
        }
        if let Some(summary) = summary {
            let summary = wit::ScanSummary {
                scanned: summary.scanned,
                updated: summary.updated,
            };
            plugin.call(&self.linker, &self.services, "scan on-done", |b, s| {
                b.call_on_scan_done(s, scan_id, summary)
            });
        }
    }
}

#[path = "wasm_wasi.rs"]
mod wasi;

#[cfg(test)]
#[path = "wasm_tests.rs"]
mod tests;
