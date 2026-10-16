//! WebAssembly-based [`ScriptRunner`] implementation.
//!
//! A WebAssembly plugin is a [component] targeting the `plugin` world of
//! `sdk/rust/kiki-plugin/wit/kiki-plugin.wit`: it imports the `host` interface, through
//! which it calls the server, and exports an `init` function and one function per event. A core
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
//! name, so the server can check its permissions.
//!
//! # Safety controls
//!
//! Every call into a plugin is bounded by:
//!
//! * The plugin's [`TimeBudget`], enforced with Wasmtime's epoch interruption: a thread
//!   advances the engine's epoch every [`EPOCH_TICK`], and at each tick a running plugin
//!   checks its deadline. The budget is of CPU time, so that a busy machine does not make
//!   plugins run out of time, with a ceiling in wall-clock time (see [`wall_limit`]).
//!   Time spent on the server's behalf is given back, up to [`MAX_CALL_ALLOWANCE`] per
//!   call. Instantiating a plugin and calling its `init` run under [`LOAD_BUDGET`],
//!   together.
//! * [`WASM_MEMORY_LIMIT_BYTES`] of linear memory per plugin, and limits on tables and
//!   instances, enforced with [`StoreLimits`].
//! * [`MAX_WASM_STACK`] bytes of stack.
//!
//! A call that traps — runs out of time or memory, or hits `unreachable`, as a Rust panic
//! does — fails the call: the entry passes through unmodified, the wait is kept, or the
//! failure is logged. Since a trap can leave the plugin's memory in
//! any state, the plugin is then started afresh: instantiated again, and its `init` called
//! again, which starts its timers again. Its scans end. The restart waits for the next
//! event the plugin handles, and each dispatch restarts at most one plugin, so the time a
//! request to the script host can take grows by at most one [`LOAD_BUDGET`]; see
//! [`crate::process::script_host::ScriptHost`]. A plugin that traps more than
//! [`MAX_TRAPS`] times in [`TRAP_WINDOW`] is disabled until plugins next reload.

use super::regex::MAX_LIVE_REGEXES;
use super::{
    parse_script_config, ContentChange, DeleteFilter, EntryChanges, Event, EventPayload, EventSet,
    FeedEntry, FetchSchedule, PluginRun, ScanOptions, ScanSummary, ScheduleDecision, ScriptRunner,
    ScriptServices, ScriptSource, ServiceCall, ServiceReply, TimeBudget, WasmComponent,
    MAX_TIMER_INTERVAL, SCAN_SLICE, TIMER_TICK,
};
use crate::db::tags::{is_reserved_tag_name, SystemTag};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use thiserror::Error;
use tracing::{debug, error, warn};
use wasmtime::component::{Component, HasSelf, Linker, Resource, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreContextMut, StoreLimits, StoreLimitsBuilder};

mod bindings {
    wasmtime::component::bindgen!({
        path: "sdk/rust/kiki-plugin/wit",
        world: "plugin",
        imports: { default: trappable },
        with: {
            "kiki:plugin/regex.regex": super::HostRegex,
            "kiki:plugin/regex.regex-set": super::HostRegexSet,
        },
    });
}

use bindings::kiki::plugin::host::Host;
use bindings::kiki::plugin::html as html_host;
use bindings::kiki::plugin::regex as regex_host;
use bindings::kiki::plugin::types as wit;
use bindings::Plugin as Bindings;

/// Linear memory each plugin may use.
pub const WASM_MEMORY_LIMIT_BYTES: usize = 16 * 1024 * 1024;

/// Stack each plugin may use.
pub const MAX_WASM_STACK: usize = 512 * 1024;

/// How often the engine's epoch advances, and so how often a running plugin checks its
/// time budget.
pub const EPOCH_TICK: Duration = Duration::from_millis(5);

/// The time budget of loading a plugin: instantiating it and calling its `init`, together.
/// See [`super::WASM_LOAD_BUDGET`].
pub const LOAD_BUDGET: Duration = super::WASM_LOAD_BUDGET;

/// How much of the time a call spends waiting on the server does not count against its
/// time budget.
pub const MAX_CALL_ALLOWANCE: Duration = Duration::from_secs(1);

/// How much of the time a call spends waiting on the network, as for `fetch-page`, does
/// not count against its time budget, on top of [`MAX_CALL_ALLOWANCE`]: about long enough
/// to fetch one page that takes as long as the fetcher allows
/// ([`crate::fetcher::page::PAGE_FETCH_TIMEOUT`]), and a quick one besides.
pub const MAX_NETWORK_ALLOWANCE: Duration = Duration::from_secs(30);

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
            // Compile to native code with Cranelift, for the host: the JIT, not Wasmtime's
            // Pulley interpreter, and not whatever `Auto` may come to pick.
            config.strategy(wasmtime::Strategy::Cranelift);
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
            .and_then(|mut encoder| encoder.encode())
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
    #[cfg(test)]
    return compile_once(plugin, &component.bytes);
    #[cfg(not(test))]
    compile_bytes(plugin, &component.bytes)
}

/// Compile `bytes` as [`compile_bytes`] does, once per test process.
///
/// Tests build runners from components' bytes, as the server does, so [`compile`] would
/// compile them anew for every test; Cranelift takes seconds over a plugin such as
/// `sanitize`, and the plugins' tests would spend most of their time compiling the same
/// few components. Tests running at once wait for the first to compile it. Failures are
/// not kept, so that each is reported with its own plugin's name.
#[cfg(test)]
fn compile_once(plugin: &str, bytes: &[u8]) -> Result<Component, WasmError> {
    type Slot = Arc<Mutex<Option<Component>>>;
    static COMPILED: OnceLock<Mutex<HashMap<[u8; 32], Slot>>> = OnceLock::new();
    let slot = COMPILED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(*blake3::hash(bytes).as_bytes())
        .or_default()
        .clone();
    let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(compiled) = slot.as_ref() {
        return Ok(compiled.clone());
    }
    let compiled = compile_bytes(plugin, bytes)?;
    *slot = Some(compiled.clone());
    Ok(compiled)
}

/// Compile `component`, the code of the plugin named `plugin`, and keep it, so that
/// sources can refer to it by hash alone.
///
/// The script host calls this for each component the server sends it, before the sources
/// that use it; see [`crate::process::script_host::ScriptHost::reload`].
///
/// # Errors
///
/// Returns [`WasmError::Compile`] if the component cannot be compiled.
pub fn put_component(plugin: &str, component: &WasmComponent) -> Result<(), WasmError> {
    let compiled = compile_bytes(plugin, &component.bytes)?;
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

/// How much CPU time the calling thread has used, from the clock that measures plugins'
/// time budgets.
///
/// Where the platform has no clock of a thread's CPU time, this is the time since the
/// first call, so that budgets are measured in wall-clock time.
fn thread_cpu_time() -> Duration {
    #[cfg(unix)]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid, writable timespec, and the clock id is a constant
        // every Unix Kiki builds for defines.
        if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) } == 0 {
            return Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32);
        }
    }
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed()
}

/// The time budget of the call into a plugin in progress, if any.
///
/// The budget is charged in CPU time of the thread running the call, so that a plugin is
/// not stopped because the machine is busy and its thread waits for a CPU. So that a call
/// still cannot run for ever, it is also stopped once it has taken [`wall_limit`] in
/// wall-clock time. Time the call spends on the server's behalf, waiting on it or doing
/// its work on this thread, is given back to it, up to [`MAX_CALL_ALLOWANCE`], and time
/// it spends waiting on the network up to [`MAX_NETWORK_ALLOWANCE`] more.
///
/// Reading the CPU time is a system call, which in the sandboxed script host also runs
/// through its seccomp filter, and a plugin checks its budget on every call to the host,
/// such as for each regex it matches. A thread cannot use CPU time faster than the clock
/// runs, though, so a call that has run for less than its budget by the clock cannot have
/// used it up: the CPU time is not read at all for calls that take less than
/// [`CPU_BASELINE_AFTER`], as most do. A call that takes longer reads it once at its
/// first check after that, and takes as its start the reading less the time since it
/// started, as though it had been on a CPU all along: time it spent waiting for one
/// before then counts, and none after.
#[derive(Default)]
struct CallBudget {
    /// The call's budget, and when it started, while a call with a budget runs.
    running: Option<(Duration, Instant)>,
    /// The reading of [`thread_cpu_time`] taken as the call's start, once there is one.
    cpu_start: std::cell::Cell<Option<Duration>>,
    /// The time given back to the call, which neither its budget nor its ceiling counts.
    credit: Duration,
    allowance: Duration,
    network_allowance: Duration,
}

/// How long a call with a CPU time budget runs before it first reads the CPU time; see
/// [`CallBudget`].
pub const CPU_BASELINE_AFTER: Duration = Duration::from_millis(1);

/// How many times its budget a call may take in wall-clock time.
pub const WALL_CLOCK_FACTOR: u32 = 5;

/// The most more than its budget a call may take in wall-clock time.
pub const MAX_WALL_CLOCK_SLACK: Duration = Duration::from_secs(1);

/// How long, in wall-clock time, a call with a CPU time budget of `limit` may take:
/// [`WALL_CLOCK_FACTOR`] times its budget, but no more than [`MAX_WALL_CLOCK_SLACK`] more
/// than it.
pub fn wall_limit(limit: Duration) -> Duration {
    (limit * WALL_CLOCK_FACTOR).min(limit + MAX_WALL_CLOCK_SLACK)
}

impl CallBudget {
    fn start(&mut self, limit: Option<Duration>) {
        self.running = limit.map(|l| (l, Instant::now()));
        self.cpu_start.set(None);
        self.credit = Duration::ZERO;
        self.allowance = MAX_CALL_ALLOWANCE;
        self.network_allowance = MAX_NETWORK_ALLOWANCE;
    }

    fn stop(&mut self) {
        self.running = None;
    }

    fn expired(&self) -> bool {
        let Some((limit, started)) = self.running else {
            return false;
        };
        let elapsed = started.elapsed();
        if elapsed >= wall_limit(limit) + self.credit {
            return true;
        }
        let budget = limit + self.credit;
        let cpu_start = match self.cpu_start.get() {
            Some(cpu_start) => cpu_start,
            // Too soon to have used the budget, or to be worth reading the CPU time.
            None if elapsed < CPU_BASELINE_AFTER.min(budget) => return false,
            None => {
                let cpu_start = thread_cpu_time().saturating_sub(elapsed);
                self.cpu_start.set(Some(cpu_start));
                cpu_start
            }
        };
        // The thread's CPU time grows no faster than the clock.
        elapsed >= budget && thread_cpu_time().saturating_sub(cpu_start) >= budget
    }

    /// Give back `waited`, time the call spent on the server's behalf, up to what is left
    /// of [`MAX_CALL_ALLOWANCE`]; or, if the server spent it waiting on the network, up to
    /// what is left of [`MAX_NETWORK_ALLOWANCE`] first.
    fn give_back(&mut self, waited: Duration, network: bool) {
        let mut waited = waited;
        if network {
            let credit = waited.min(self.network_allowance);
            self.network_allowance -= credit;
            self.credit += credit;
            waited -= credit;
        }
        let credit = waited.min(self.allowance);
        self.allowance -= credit;
        self.credit += credit;
    }
}

/// A regex a plugin compiled with the `regex` interface.
pub struct HostRegex(Arc<regex::bytes::Regex>);

/// A set of regexes a plugin compiled with the `regex` interface: each matched alone, for
/// the reason [`super::regex`] gives.
pub struct HostRegexSet(Vec<Arc<regex::bytes::Regex>>);

/// The regexes a plugin has compiled, by pattern and flags, so that compiling one again,
/// alone or in a set, shares the one already alive.
#[derive(Default)]
struct Regexes(HashMap<(String, String), std::sync::Weak<regex::bytes::Regex>>);

impl Regexes {
    /// The regex for `pattern` and `flags`: the one alive already, or a new one, unless
    /// the plugin has [`MAX_LIVE_REGEXES`] alive.
    fn get(&mut self, pattern: String, flags: String) -> Result<Arc<regex::bytes::Regex>, String> {
        let key = (pattern, flags);
        if let Some(re) = self.0.get(&key).and_then(std::sync::Weak::upgrade) {
            return Ok(re);
        }
        self.0.retain(|_, re| re.strong_count() > 0);
        if self.0.len() >= MAX_LIVE_REGEXES {
            return Err(format!(
                "too many regexes (at most {MAX_LIVE_REGEXES} may be alive at once)"
            ));
        }
        let re = Arc::new(super::regex::compile(&key.0, &key.1)?);
        self.0.insert(key, Arc::downgrade(&re));
        Ok(re)
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
    /// The regexes and regex sets the plugin holds.
    table: ResourceTable,
    regexes: Regexes,
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
        let network = call.is_slow();
        let start = Instant::now();
        let result = services.call(&self.plugin, call);
        self.budget.give_back(start.elapsed(), network);
        Ok(result.map_err(|e| format!("{name}: {e}")))
    }
}

impl State {
    /// Traps if the call into the plugin has used up its time budget, so that a plugin
    /// matching in a loop is stopped, though matching runs outside the plugin.
    fn check_budget(&self, name: &str) -> wasmtime::Result<()> {
        if self.budget.expired() {
            return Err(wasmtime::Error::msg(format!(
                "{name}: the handler has used up its time budget"
            )));
        }
        Ok(())
    }
}

impl regex_host::Host for State {}

impl regex_host::HostRegex for State {
    fn compile(
        &mut self,
        pattern: String,
        flags: String,
    ) -> wasmtime::Result<Result<Resource<HostRegex>, String>> {
        self.check_budget("regex.compile")?;
        match self.regexes.get(pattern, flags) {
            Ok(re) => Ok(Ok(self.table.push(HostRegex(re))?)),
            Err(e) => Ok(Err(e)),
        }
    }

    fn is_match(&mut self, re: Resource<HostRegex>, haystack: String) -> wasmtime::Result<bool> {
        self.check_budget("regex.is-match")?;
        Ok(self.table.get(&re)?.0.is_match(haystack.as_bytes()))
    }

    fn find(
        &mut self,
        re: Resource<HostRegex>,
        haystack: String,
        start: u32,
    ) -> wasmtime::Result<Option<(u32, u32)>> {
        self.check_budget("regex.find")?;
        let start = start as usize;
        if start > haystack.len() {
            return Ok(None);
        }
        // A plugin's haystack is in its 32-bit memory, so offsets fit in a u32.
        Ok(self
            .table
            .get(&re)?
            .0
            .find_at(haystack.as_bytes(), start)
            .map(|m| (m.start() as u32, m.end() as u32)))
    }

    fn drop(&mut self, re: Resource<HostRegex>) -> wasmtime::Result<()> {
        self.table.delete(re)?;
        Ok(())
    }
}

impl regex_host::HostRegexSet for State {
    fn compile(
        &mut self,
        patterns: Vec<(String, String)>,
    ) -> wasmtime::Result<Result<Resource<HostRegexSet>, String>> {
        self.check_budget("regex-set.compile")?;
        let mut set = Vec::with_capacity(patterns.len());
        for (i, (pattern, flags)) in patterns.into_iter().enumerate() {
            match self.regexes.get(pattern, flags) {
                Ok(re) => set.push(re),
                Err(e) => return Ok(Err(format!("pattern {i}: {e}"))),
            }
        }
        Ok(Ok(self.table.push(HostRegexSet(set))?))
    }

    fn matches(
        &mut self,
        set: Resource<HostRegexSet>,
        haystack: String,
    ) -> wasmtime::Result<Vec<u32>> {
        self.check_budget("regex-set.matches")?;
        let haystack = haystack.as_bytes();
        Ok(self
            .table
            .get(&set)?
            .0
            .iter()
            .enumerate()
            .filter(|(_, re)| re.is_match(haystack))
            .map(|(i, _)| i as u32)
            .collect())
    }

    fn drop(&mut self, set: Resource<HostRegexSet>) -> wasmtime::Result<()> {
        self.table.delete(set)?;
        Ok(())
    }
}

impl State {
    /// The result of an `html` call: a trap if the time budget ran out during it, or else
    /// the result or its error message.
    fn html_result<T>(
        name: &str,
        result: Result<T, super::html::HtmlError>,
    ) -> wasmtime::Result<Result<T, String>> {
        match result {
            Ok(value) => Ok(Ok(value)),
            Err(super::html::HtmlError::Failed(message)) => Ok(Err(format!("{name}: {message}"))),
            Err(super::html::HtmlError::Expired) => Err(wasmtime::Error::msg(format!(
                "{name}: the handler has used up its time budget"
            ))),
        }
    }
}

fn span_to_wit(span: super::html::Span) -> html_host::Span {
    html_host::Span {
        start: span.start,
        len: span.len,
    }
}

/// What `select` returns, as plugins see it; also the elements `strip` returns.
fn elements_to_wit(selected: super::html::Selected) -> html_host::Elements {
    use super::html::Namespace;
    html_host::Elements {
        elements: selected
            .elements
            .iter()
            .map(|el| html_host::Element {
                tag_name: span_to_wit(el.tag_name),
                namespace: match el.namespace {
                    Namespace::Html => html_host::Namespace::Html,
                    Namespace::Svg => html_host::Namespace::Svg,
                    Namespace::MathMl => html_host::Namespace::Mathml,
                },
                attributes: span_to_wit(el.attributes),
            })
            .collect(),
        attributes: selected
            .attributes
            .iter()
            .map(|a| html_host::Attribute {
                name: span_to_wit(a.name),
                value: span_to_wit(a.value),
            })
            .collect(),
        text: selected.text,
    }
}

impl html_host::Host for State {
    fn select(
        &mut self,
        html: String,
        selector: String,
        max_bytes: u32,
    ) -> wasmtime::Result<Result<html_host::Elements, String>> {
        self.check_budget("html.select")?;
        let budget = &self.budget;
        let selected = super::html::select(&html, &selector, max_bytes, &|| budget.expired());
        Self::html_result("html.select", selected.map(elements_to_wit))
    }

    fn strip(
        &mut self,
        html: String,
        rules: html_host::StripRules,
        max_bytes: u32,
    ) -> wasmtime::Result<Result<html_host::Stripped, String>> {
        self.check_budget("html.strip")?;
        let rules = super::html::StripRules {
            drop: rules.drop.into_iter().collect(),
            keep: rules.keep.into_iter().collect(),
            drop_foreign: rules.drop_foreign,
            remove_comments: rules.remove_comments,
        };
        let budget = &self.budget;
        let stripped = super::html::strip(&html, &rules, max_bytes, &|| budget.expired());
        Self::html_result(
            "html.strip",
            stripped.map(|stripped| html_host::Stripped {
                html: stripped.html,
                elements: elements_to_wit(stripped.elements),
            }),
        )
    }

    fn rewrite(
        &mut self,
        html: String,
        selector: String,
        edits: Vec<html_host::Edit>,
        remove_comments: bool,
        max_bytes: u32,
    ) -> wasmtime::Result<Result<String, String>> {
        use super::html::{Edit, EditOp, Place};
        use html_host::EditOp as Op;
        self.check_budget("html.rewrite")?;
        let place = |place: html_host::Place| match place {
            html_host::Place::Before => Place::Before,
            html_host::Place::After => Place::After,
            html_host::Place::Prepend => Place::Prepend,
            html_host::Place::Append => Place::Append,
            html_host::Place::Inner => Place::Inner,
            html_host::Place::Replace => Place::Replace,
        };
        let edits = edits
            .into_iter()
            .map(|edit| Edit {
                element: edit.element,
                op: match edit.op {
                    Op::Remove => EditOp::Remove,
                    Op::Unwrap => EditOp::Unwrap,
                    Op::SetAttribute((name, value)) => EditOp::SetAttribute(name, value),
                    Op::RemoveAttribute(name) => EditOp::RemoveAttribute(name),
                    Op::SetTagName(name) => EditOp::SetTagName(name),
                    Op::InsertText((at, content)) => EditOp::Insert(place(at), content, false),
                    Op::InsertHtml((at, content)) => EditOp::Insert(place(at), content, true),
                },
            })
            .collect();
        let budget = &self.budget;
        let out =
            super::html::rewrite(&html, &selector, edits, remove_comments, max_bytes, &|| {
                budget.expired()
            });
        Self::html_result("html.rewrite", out)
    }

    fn unescape(&mut self, s: String, max_bytes: u32) -> wasmtime::Result<Result<String, String>> {
        self.check_budget("html.unescape")?;
        Self::html_result("html.unescape", super::html::unescape(&s, max_bytes))
    }

    fn inner(
        &mut self,
        html: String,
        selector: String,
        max_bytes: u32,
    ) -> wasmtime::Result<Result<Option<String>, String>> {
        self.check_budget("html.inner")?;
        let budget = &self.budget;
        let content = super::html::inner(&html, &selector, max_bytes, &|| budget.expired());
        Self::html_result("html.inner", content)
    }

    fn inner_each(
        &mut self,
        html: String,
        selectors: Vec<String>,
    ) -> wasmtime::Result<Result<Vec<Option<html_host::Span>>, String>> {
        self.check_budget("html.inner-each")?;
        let budget = &self.budget;
        let spans = super::html::inner_each(&html, &selectors, &|| budget.expired());
        Self::html_result(
            "html.inner-each",
            spans.map(|spans| spans.into_iter().map(|s| s.map(span_to_wit)).collect()),
        )
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
                // The scan cannot reach the plugin before this: its batches are handed to
                // the plugin, which is locked until this call returns.
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

    fn get_entry(
        &mut self,
        feed_id: i64,
        guid: String,
    ) -> wasmtime::Result<Result<Option<wit::Entry>, String>> {
        match self.service("get-entry", ServiceCall::GetEntry { feed_id, guid })? {
            Ok(ServiceReply::Entry(entry)) => Ok(Ok(entry.map(|mut e| entry_to_wit(&mut e)))),
            Ok(other) => unexpected("get-entry", other),
            Err(e) => Ok(Err(e)),
        }
    }

    fn fetch_page(&mut self, url: String) -> wasmtime::Result<Result<wit::Page, String>> {
        match self.service("fetch-page", ServiceCall::FetchPage { url })? {
            Ok(ServiceReply::Page(page)) => Ok(Ok(wit::Page {
                url: page.url,
                content_type: page.content_type,
                body: page.body,
                truncated: page.truncated,
            })),
            Ok(other) => unexpected("fetch-page", other),
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
    /// `None` after a trap, until the plugin is restarted, and for good once it has been
    /// disabled for trapping too often.
    live: Option<Live>,
    /// Whether the plugin trapped, and is to be started afresh when it is next needed.
    restart_pending: bool,
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
        table: ResourceTable::new(),
        regexes: Regexes::default(),
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

    // Instantiating runs the component's start functions. They and `init` share one load
    // budget, so that loading a plugin takes at most that long, however the time splits.
    store.data_mut().budget.start(Some(LOAD_BUDGET));
    let bindings = Bindings::instantiate(&mut store, component, linker).map_err(|e| {
        WasmError::Instantiate {
            plugin: name.to_string(),
            message: format!("{e:?}"),
        }
    })?;
    let mut live = Live { store, bindings };
    let init_err = |message: String| WasmError::Init {
        plugin: name.to_string(),
        message,
    };
    let kinds = continue_call(&mut live, LOAD_BUDGET, |b, s| b.call_init(s, config))
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
    finish_call(live, limit, f)
}

/// As [`call`], under the budget of `limit` that is already running.
fn continue_call<R>(
    live: &mut Live,
    limit: Duration,
    f: impl FnOnce(&Bindings, &mut Store<State>) -> wasmtime::Result<R>,
) -> Result<R, String> {
    finish_call(live, Some(limit), f)
}

fn finish_call<R>(
    live: &mut Live,
    limit: Option<Duration>,
    f: impl FnOnce(&Bindings, &mut Store<State>) -> wasmtime::Result<R>,
) -> Result<R, String> {
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
        wit::EventKind::EntryEnrich => Event::EntryEnrich,
        wit::EventKind::EntryTransform => Event::EntryTransform,
        wit::EventKind::EntryIngest => Event::EntryIngest,
        wit::EventKind::FetchSuccess => Event::FetchSuccess,
        wit::EventKind::FetchError => Event::FetchError,
        wit::EventKind::FeedAdded => Event::FeedAdded,
        wit::EventKind::FeedRemoved => Event::FeedRemoved,
        wit::EventKind::PluginLoad => Event::PluginLoad,
        wit::EventKind::FetchSchedule => Event::FetchSchedule,
    }
}

/// Move `entry` into the WIT type plugins are called with, leaving its strings and lists
/// empty, to be put back with [`entry_from_wit`].
fn entry_to_wit(entry: &mut FeedEntry) -> wit::Entry {
    wit::Entry {
        id: entry.id,
        feed_id: entry.feed_id,
        syndication_format: std::mem::take(&mut entry.syndication_format),
        guid: std::mem::take(&mut entry.guid),
        published_at: entry.published_at,
        title: std::mem::take(&mut entry.title),
        url: entry.url.take(),
        content: entry.content.take(),
        authors: std::mem::take(&mut entry.authors),
        categories: std::mem::take(&mut entry.categories),
        tags: std::mem::take(&mut entry.tags),
        cache_assets: entry.cache_assets,
    }
}

/// Move `wit` back into `entry`, undoing [`entry_to_wit`].
fn entry_from_wit(entry: &mut FeedEntry, wit: wit::Entry) {
    entry.syndication_format = wit.syndication_format;
    entry.guid = wit.guid;
    entry.published_at = wit.published_at;
    entry.title = wit.title;
    entry.url = wit.url;
    entry.content = wit.content;
    entry.authors = wit.authors;
    entry.categories = wit.categories;
    entry.tags = wit.tags;
    entry.cache_assets = wit.cache_assets;
}

/// Take into `current` the fields of `returned`, the entry a handler returned, that
/// plugins may change, and return which of them differ. The rest of `returned` is
/// ignored: those fields are read-only.
fn merge_returned(current: &mut wit::Entry, returned: wit::Entry) -> EntryChanges {
    let mut changes = EntryChanges::default();
    if current.published_at != returned.published_at {
        current.published_at = returned.published_at;
        changes.insert(EntryChanges::PUBLISHED_AT);
    }
    if current.title != returned.title {
        current.title = returned.title;
        changes.insert(EntryChanges::TITLE);
    }
    if current.url != returned.url {
        current.url = returned.url;
        changes.insert(EntryChanges::URL);
    }
    if current.content != returned.content {
        current.content = returned.content;
        changes.insert(EntryChanges::CONTENT);
    }
    if current.tags != returned.tags {
        current.tags = returned.tags;
        changes.insert(EntryChanges::TAGS);
    }
    if current.cache_assets != returned.cache_assets {
        current.cache_assets = returned.cache_assets;
        changes.insert(EntryChanges::CACHE_ASSETS);
    }
    changes
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

/// An observe-only event's payload, as plugins are called with it.
enum WitPayload {
    Entry(wit::Entry),
    FetchSuccess(wit::FetchSuccess),
    FetchError(wit::FetchError),
    Feed(wit::FeedEvent),
    None,
}

impl WitPayload {
    /// `payload`, the payload of `event`, as plugins are called with it, or `None` if it
    /// is not a payload of `event`.
    fn new(event: Event, payload: EventPayload) -> Option<Self> {
        Some(match (event, payload) {
            (Event::EntryParsed, EventPayload::Entry(mut entry)) => {
                WitPayload::Entry(entry_to_wit(&mut entry))
            }
            (
                Event::FetchSuccess,
                EventPayload::FetchSuccess {
                    feed_id,
                    status,
                    url,
                    content_length,
                },
            ) => WitPayload::FetchSuccess(wit::FetchSuccess {
                feed_id,
                status,
                url,
                content_length,
            }),
            (
                Event::FetchError,
                EventPayload::FetchError {
                    feed_id,
                    kind,
                    status,
                    message,
                    retry_after,
                },
            ) => WitPayload::FetchError(wit::FetchError {
                feed_id,
                kind: kind.into_owned(),
                status,
                message,
                retry_after,
            }),
            (Event::FeedAdded | Event::FeedRemoved, EventPayload::Feed { id, url, title }) => {
                WitPayload::Feed(wit::FeedEvent { id, url, title })
            }
            (Event::PluginLoad, EventPayload::PluginLoad) => WitPayload::None,
            _ => return None,
        })
    }
}

/// What one dispatch shares between the plugins it calls.
struct Dispatch<'a> {
    linker: &'a Linker<State>,
    services: &'a Option<Arc<dyn ScriptServices>>,
    /// Whether a plugin has been restarted during this dispatch: each restarts at most one.
    restarted: bool,
}

impl WasmPlugin {
    /// Call into the plugin with `f`, under its time budget, restarting it first if it
    /// trapped earlier and this dispatch has not restarted a plugin yet.
    ///
    /// On failure, logs `what` failed, and marks the plugin to be started afresh, or
    /// disables it if it has failed too often. Returns `None` if the call failed or the
    /// plugin is not running.
    fn call<R>(
        &mut self,
        dispatch: &mut Dispatch<'_>,
        what: &str,
        f: impl FnOnce(&Bindings, &mut Store<State>) -> wasmtime::Result<R>,
    ) -> Option<R> {
        if !self.ensure_live(dispatch) {
            return None;
        }
        let live = self.live.as_mut()?;
        match call(live, self.budget.limit(), f) {
            Ok(result) => Some(result),
            Err(e) => {
                warn!(plugin = %self.name, error = %e, "{what} failed");
                self.trapped();
                None
            }
        }
    }

    /// Note a trap: the instance is dropped, and the plugin is to be started afresh,
    /// unless it has trapped too often.
    fn trapped(&mut self) {
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
        self.restart_pending = self.traps.len() <= MAX_TRAPS;
        if !self.restart_pending {
            error!(
                plugin = %self.name,
                "plugin failed {} times in {} minutes; disabling it until plugins reload",
                self.traps.len(),
                TRAP_WINDOW.as_secs() / 60
            );
        }
    }

    /// Make sure the plugin is running, restarting it if it trapped and `dispatch` has not
    /// restarted a plugin yet. Returns whether it is running.
    fn ensure_live(&mut self, dispatch: &mut Dispatch<'_>) -> bool {
        if self.live.is_some() {
            return true;
        }
        if !self.restart_pending || dispatch.restarted {
            return false;
        }
        dispatch.restarted = true;
        self.restart_pending = false;
        match instantiate(
            dispatch.linker,
            &self.component,
            &self.name,
            &self.config,
            dispatch.services.clone(),
        ) {
            Ok((live, events)) => {
                debug!(plugin = %self.name, "plugin restarted");
                self.live = Some(live);
                self.events = events;
                true
            }
            Err(e) => {
                error!(
                    plugin = %self.name,
                    error = %e,
                    "plugin could not be restarted; disabling it until plugins reload"
                );
                false
            }
        }
    }

    fn has_scan(&self, scan_id: u64) -> bool {
        self.live
            .as_ref()
            .is_some_and(|l| l.store.data().scans.contains(&scan_id))
    }

    /// The events the plugin handles, and [`Event::Timer`] if it has started a timer.
    fn subscriptions(&self) -> EventSet {
        match &self.live {
            Some(live) => {
                let mut set = self.events;
                if !live.store.data().timers.is_empty() {
                    set.insert(Event::Timer);
                }
                set
            }
            // A plugin that is to be restarted may start timers again, so the timer tick,
            // like its events, restarts it.
            None if self.restart_pending => {
                let mut set = self.events;
                set.insert(Event::Timer);
                set
            }
            None => EventSet::default(),
        }
    }
}

/// A plugin, behind a lock of its own, so that different plugins run at the same time.
struct PluginSlot {
    plugin: Mutex<WasmPlugin>,
    /// The [`EventSet::to_bits`] of [`WasmPlugin::subscriptions`], as of when the plugin
    /// was last unlocked, so that what plugins handle can be told without waiting for them.
    subscribed: AtomicU16,
}

/// A locked plugin. Unlocking it records what it handles now in its slot.
struct PluginGuard<'a> {
    slot: &'a PluginSlot,
    plugin: MutexGuard<'a, WasmPlugin>,
}

impl PluginSlot {
    fn new(plugin: WasmPlugin) -> Self {
        let subscribed = AtomicU16::new(plugin.subscriptions().to_bits());
        PluginSlot {
            plugin: Mutex::new(plugin),
            subscribed,
        }
    }

    fn lock(&self) -> PluginGuard<'_> {
        PluginGuard {
            slot: self,
            plugin: self.plugin.lock().unwrap_or_else(|e| e.into_inner()),
        }
    }

    /// As [`Self::lock`], unless another call holds the plugin.
    fn try_lock(&self) -> Option<PluginGuard<'_>> {
        let plugin = match self.plugin.try_lock() {
            Ok(plugin) => plugin,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        Some(PluginGuard { slot: self, plugin })
    }

    /// What the plugin handled when it was last unlocked.
    fn subscriptions(&self) -> EventSet {
        EventSet::from_bits(self.subscribed.load(Ordering::SeqCst))
    }
}

impl std::ops::Deref for PluginGuard<'_> {
    type Target = WasmPlugin;
    fn deref(&self) -> &WasmPlugin {
        &self.plugin
    }
}

impl std::ops::DerefMut for PluginGuard<'_> {
    fn deref_mut(&mut self) -> &mut WasmPlugin {
        &mut self.plugin
    }
}

impl Drop for PluginGuard<'_> {
    fn drop(&mut self) {
        self.slot
            .subscribed
            .store(self.plugin.subscriptions().to_bits(), Ordering::SeqCst);
    }
}

/// Runs WebAssembly plugins in response to server events.
///
/// Each plugin is behind a lock of its own, so that one plugin's handler runs one call at
/// a time, while different plugins' handlers run at the same time: an entry can be in one
/// plugin of the `entry.ingest` chain while another entry is in the next. See the
/// [module documentation](self).
pub struct WasmScriptRunner {
    /// The plugins, in the order they load in.
    plugins: Vec<PluginSlot>,
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
            let component = compile(&source.name, &source.component)?;
            // Toolchains such as Rust's for `wasm32-wasip2` import WASI whether or not a
            // plugin uses it. Kiki defines the harmless parts; the rest trap if called.
            wasi::define(&mut linker, engine, &component, &mut wasi_defined).map_err(|e| {
                WasmError::Instantiate {
                    plugin: source.name.clone(),
                    message: format!("{e:?}"),
                }
            })?;
            let (live, events) =
                instantiate(&linker, &component, &name, &source.config, services.clone())?;
            plugins.push(PluginSlot::new(WasmPlugin {
                name,
                budget: source.time_budget,
                config: source.config.clone(),
                component,
                events,
                live: Some(live),
                restart_pending: false,
                traps: VecDeque::new(),
            }));
        }
        Ok(Self {
            plugins,
            linker,
            services,
        })
    }

    fn dispatch(&self) -> Dispatch<'_> {
        Dispatch {
            linker: &self.linker,
            services: &self.services,
            restarted: false,
        }
    }

    /// The plugins that handled `event` when they were last unlocked, with their indexes
    /// in load order, locked in turn. Each is checked again once locked, since it may have
    /// changed while it was busy. (For [`Event::Timer`], a plugin handles the event if it
    /// has timers; see [`WasmPlugin::subscriptions`].)
    fn locked_for(&self, event: Event) -> impl Iterator<Item = (usize, PluginGuard<'_>)> {
        self.plugins
            .iter()
            .enumerate()
            .filter(move |(_, slot)| slot.subscriptions().contains(event))
            .map(|(i, slot)| (i, slot.lock()))
            .filter(move |(_, plugin)| plugin.subscriptions().contains(event))
    }

    /// The events at least one plugin handles, and [`Event::Timer`] if a plugin has
    /// started a timer.
    ///
    /// Answered without waiting for plugins that are busy, from what they handled when
    /// they were last unlocked.
    pub fn subscriptions(&self) -> EventSet {
        self.plugins.iter().fold(EventSet::default(), |set, slot| {
            set.union(slot.subscriptions())
        })
    }

    /// Whether a plugin in this runner started the scan `scan_id`, which has not finished.
    pub fn has_scan(&self, scan_id: u64) -> bool {
        self.scan_owner(scan_id).is_some()
    }

    /// The plugin that started the scan `scan_id`, locked. Plugins that are free are looked
    /// at first, so that a busy plugin holds up only its own scans.
    fn scan_owner(&self, scan_id: u64) -> Option<PluginGuard<'_>> {
        let mut busy = Vec::new();
        for slot in &self.plugins {
            match slot.try_lock() {
                Some(plugin) if plugin.has_scan(scan_id) => return Some(plugin),
                Some(_) => {}
                None => busy.push(slot),
            }
        }
        busy.into_iter()
            .map(PluginSlot::lock)
            .find(|plugin| plugin.has_scan(scan_id))
    }

    /// Calls `on-timer` for each timer due by `now`, and schedules its next call.
    fn run_timers(&self, now: Instant) {
        let mut dispatch = self.dispatch();
        for (_, mut plugin) in self.locked_for(Event::Timer) {
            // A restarted plugin starts its timers afresh, so none is due yet.
            plugin.ensure_live(&mut dispatch);
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
                // A timer handler that traps ends the instance, and with it the timers
                // still due: the ids belong to the old instance.
                if plugin.live.is_none() {
                    break;
                }
                plugin.call(&mut dispatch, Event::Timer.handler_name(), |b, s| {
                    b.call_on_timer(s, id)
                });
            }
        }
    }

    /// Pass `entry` through each `entry.enrich` handler, then each `entry.transform` handler
    /// and then each `entry.ingest` handler, each in registration order, changing it in place, and calling `on_run` with the index in load order and the name of each
    /// plugin whose handler ran, and how long it took, in seconds.
    ///
    /// Returns which fields the handlers changed, or `None` if a handler dropped the entry.
    /// A handler that fails leaves the entry as it was.
    pub(crate) fn transform_entry(
        &self,
        entry: &mut FeedEntry,
        mut on_run: impl FnMut(usize, &Arc<str>, f64),
    ) -> Option<EntryChanges> {
        let mut dispatch = self.dispatch();
        let mut changes = EntryChanges::default();
        // Moved into the WIT type once, and back at the end, rather than copied for each
        // plugin.
        let mut current = entry_to_wit(entry);
        let chain = [
            Event::EntryEnrich,
            Event::EntryTransform,
            Event::EntryIngest,
        ]
        .into_iter()
        .flat_map(|event| self.locked_for(event).map(move |(i, p)| (event, i, p)));
        for (event, index, mut plugin) in chain {
            let start = Instant::now();
            let returned = plugin.call(&mut dispatch, event.handler_name(), |b, s| match event {
                Event::EntryEnrich => b.call_on_entry_enrich(s, &current),
                Event::EntryTransform => b.call_on_entry_transform(s, &current),
                _ => b.call_on_entry_ingest(s, &current),
            });
            on_run(index, &plugin.name, start.elapsed().as_secs_f64());
            match returned {
                // A failed handler passes the entry through unmodified.
                None => {}
                Some(None) => {
                    entry_from_wit(entry, current);
                    return None;
                }
                Some(Some(returned)) => changes.insert(merge_returned(&mut current, returned)),
            }
        }
        entry_from_wit(entry, current);
        Some(changes)
    }
}

impl ScriptRunner for WasmScriptRunner {
    fn handles(&self, event: Event) -> bool {
        self.subscriptions().contains(event)
    }

    fn dispatch_transform_entry(&self, mut entry: FeedEntry) -> anyhow::Result<Option<FeedEntry>> {
        let keep = self.dispatch_transform_entry_timed(&mut entry, &mut Vec::new())?;
        Ok(keep.then_some(entry))
    }

    fn dispatch_transform_entry_timed(
        &self,
        entry: &mut FeedEntry,
        runs: &mut Vec<PluginRun>,
    ) -> anyhow::Result<bool> {
        let changes = self.transform_entry(entry, |_, plugin, seconds| {
            runs.push(PluginRun {
                plugin: plugin.clone(),
                seconds,
            })
        });
        Ok(changes.is_some())
    }

    fn dispatch_parsed(&self, entry: &FeedEntry) {
        if self.handles(Event::EntryParsed) {
            self.dispatch_observe(Event::EntryParsed, EventPayload::Entry(entry.clone()));
        }
    }

    fn dispatch_schedule(
        &self,
        mut schedule: FetchSchedule,
    ) -> anyhow::Result<Option<ScheduleDecision>> {
        let mut decision = None;
        let mut dispatch = self.dispatch();
        for (_, mut plugin) in self.locked_for(Event::FetchSchedule) {
            let input = schedule_to_wit(&schedule);
            let returned = plugin.call(
                &mut dispatch,
                Event::FetchSchedule.handler_name(),
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
        if !self.handles(event) {
            return;
        }
        // Converted once, for every plugin.
        let Some(payload) = WitPayload::new(event, payload) else {
            warn!(
                event = event.name(),
                "event payload does not match the event; dropping it"
            );
            return;
        };
        let what = event.handler_name();
        let mut dispatch = self.dispatch();
        for (_, mut plugin) in self.locked_for(event) {
            let d = &mut dispatch;
            match &payload {
                WitPayload::Entry(entry) => {
                    plugin.call(d, what, |b, s| b.call_on_entry_parsed(s, entry));
                }
                WitPayload::FetchSuccess(success) => {
                    plugin.call(d, what, |b, s| b.call_on_fetch_success(s, success));
                }
                WitPayload::FetchError(error) => {
                    plugin.call(d, what, |b, s| b.call_on_fetch_error(s, error));
                }
                WitPayload::Feed(feed) => {
                    plugin.call(d, what, |b, s| {
                        if event == Event::FeedAdded {
                            b.call_on_feed_added(s, feed)
                        } else {
                            b.call_on_feed_removed(s, feed)
                        }
                    });
                }
                WitPayload::None => {
                    plugin.call(d, what, |b, s| b.call_on_plugin_load(s));
                }
            }
        }
    }

    fn dispatch_scan(
        &self,
        scan_id: u64,
        entries: Vec<FeedEntry>,
    ) -> anyhow::Result<Option<Vec<Option<FeedEntry>>>> {
        let Some(mut plugin) = self.scan_owner(scan_id) else {
            return Ok(None);
        };
        let mut dispatch = self.dispatch();
        let start = Instant::now();
        let mut results = Vec::with_capacity(entries.len());
        for mut entry in entries {
            // At least one entry is always handled, so the scan makes progress.
            if !results.is_empty() && start.elapsed() >= SCAN_SLICE {
                break;
            }
            if !plugin.has_scan(scan_id) {
                // The plugin was restarted, which ended its scans.
                return Ok(None);
            }
            let mut input = entry_to_wit(&mut entry);
            let returned = plugin.call(&mut dispatch, "scan handler", |b, s| {
                b.call_on_scan_entry(s, scan_id, &input)
            });
            results.push(returned.flatten().map(|returned| {
                merge_returned(&mut input, returned);
                entry_from_wit(&mut entry, input);
                entry
            }));
        }
        Ok(Some(results))
    }

    fn finish_scan(&self, scan_id: u64, summary: Option<ScanSummary>) {
        let Some(mut plugin) = self.scan_owner(scan_id) else {
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
            plugin.call(&mut self.dispatch(), "scan on-done", |b, s| {
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
