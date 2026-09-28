//! Lua-based [`ScriptRunner`] implementation.
//!
//! This module is compiled only when the `lua` feature is enabled.
//!
//! # Architecture
//!
//! A single [`LuaScriptRunner`] is built at server startup and shared across every worker
//! that needs to dispatch events. Internally the runner owns one Lua VM guarded by a
//! [`std::sync::Mutex`], plus a registry mapping each [`Event`] to the list of handlers
//! registered for it.
//!
//! Handlers are registered at script load time via the `kiki.on(event, handler)` function
//! exposed in Lua. Scripts must not return anything from their top-level chunk — a return
//! value is treated as an error.
//!
//! # Safety controls
//!
//! Every handler call is bounded by:
//!
//! * A 100 ms time budget (see [`SCRIPT_TIMEOUT_MS`]), enforced via [`Lua::set_hook`] firing
//!   every [`HOOK_EVERY_N`] bytecode instructions.
//! * A per-VM memory cap (see [`SCRIPT_MEMORY_LIMIT_BYTES`]), applied once at construction
//!   via [`Lua::set_memory_limit`].
//!
//! Timeouts surface as execution errors; for `entry.ingest` handlers the entry passes
//! through unmodified, for observe handlers the failure is dropped. Regexes compiled through
//! `kiki.regex` live outside the Lua allocator and have limits of their own; see
//! the `regex_api` module.

mod api;
mod config;
mod regex_api;

use super::{
    parse_script_config, Event, EventPayload, FeedEntry, ScanSummary, ScriptRunner, ScriptServices,
    ScriptSource,
};
use api::{ApiContext, Budget};
use mlua::prelude::*;
use mlua::HookTriggers;
use mlua::RegistryKey;
use mlua::VmState;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thiserror::Error;
use tracing::warn;

/// Per-handler execution time budget.
pub const SCRIPT_TIMEOUT_MS: u64 = 100;

/// How long one [`ScriptRunner::dispatch_scan`] call may keep the VM busy before handing
/// back the entries it has not reached, so that events queued behind a scan are not held up
/// for long. Checked between entries, so a dispatch runs at most this plus one handler's
/// budget.
pub const SCAN_SLICE: Duration = Duration::from_millis(50);

/// Frequency (in Lua VM instructions) at which the timeout hook fires.
const HOOK_EVERY_N: u32 = 1000;

/// Memory cap for the scripting VM.
pub const SCRIPT_MEMORY_LIMIT_BYTES: usize = 16 * 1024 * 1024;

/// Errors that can occur during script loading or execution.
#[derive(Debug, Error)]
pub enum ScriptError {
    /// A script failed to compile, or to set up the sandbox.
    #[error("script load error: {0}")]
    ScriptLoadError(#[source] LuaError),

    /// A runtime error occurred while executing a Lua script.
    #[error("script execution error: {0}")]
    ScriptExecutionError(#[source] LuaError),

    /// A script's top-level chunk returned a value; scripts must register handlers
    /// via `kiki.on(...)` and must not return anything.
    #[error(
        "script returned a value of type {0}; scripts must register handlers via `kiki.on(event, handler)` and must not return a value"
    )]
    InvalidReturnType(String),

    /// A script's config is not a JSON object.
    #[error(transparent)]
    InvalidConfig(#[from] super::ScriptConfigError),
}

impl IntoLua for FeedEntry {
    fn into_lua(self, lua: &Lua) -> LuaResult<LuaValue> {
        let table = lua.create_table()?;
        table.set("id", self.id)?;
        table.set("feed_id", self.feed_id)?;
        table.set("syndication_format", self.syndication_format)?;
        table.set("guid", self.guid)?;
        table.set("published_at", self.published_at)?;
        table.set("title", self.title)?;
        table.set("url", self.url)?;
        table.set("content", self.content)?;
        table.set("authors", lua.create_sequence_from(self.authors)?)?;
        table.set("categories", lua.create_sequence_from(self.categories)?)?;

        let tags = lua.create_table()?;
        for (i, tag) in self.tags.iter().enumerate() {
            tags.set(i + 1, tag.as_str())?;
        }
        table.set("tags", tags)?;

        Ok(LuaValue::Table(table))
    }
}

impl FromLua for FeedEntry {
    fn from_lua(value: LuaValue, _lua: &Lua) -> LuaResult<Self> {
        let table = match value {
            LuaValue::Table(t) => t,
            other => {
                return Err(LuaError::FromLuaConversionError {
                    from: other.type_name(),
                    to: "FeedEntry".to_string(),
                    message: Some("expected a table".to_string()),
                })
            }
        };

        let feed_id: i64 = table.get("feed_id")?;
        let syndication_format: String = table.get("syndication_format")?;
        let guid: String = table.get("guid")?;

        let published_at: Option<i64> = table.get("published_at")?;
        let title: String = table.get("title")?;
        let url: Option<String> = table.get("url")?;
        let content: Option<String> = table.get("content")?;

        let tags_table: LuaTable = table.get("tags")?;
        let mut tags = Vec::new();
        for pair in tags_table.sequence_values::<String>() {
            tags.push(pair?);
        }

        Ok(FeedEntry {
            // Read-only: restored from the entry the handler was given.
            id: None,
            feed_id,
            syndication_format,
            guid,
            published_at,
            title,
            url,
            content,
            // Read-only: restored from the entry the handler was given.
            authors: Vec::new(),
            categories: Vec::new(),
            tags,
        })
    }
}

/// Convert an [`EventPayload`] into the Lua table passed to a handler.
fn payload_to_lua(lua: &Lua, payload: EventPayload) -> LuaResult<LuaValue> {
    match payload {
        EventPayload::Entry(entry) => entry.into_lua(lua),
        EventPayload::FetchSuccess {
            feed_id,
            status,
            url,
            content_length,
        } => {
            let t = lua.create_table()?;
            t.set("feed_id", feed_id)?;
            t.set("status", status)?;
            t.set("url", url)?;
            t.set("content_length", content_length)?;
            Ok(LuaValue::Table(t))
        }
        EventPayload::FetchError {
            feed_id,
            kind,
            status,
            message,
            retry_after,
        } => {
            let t = lua.create_table()?;
            t.set("feed_id", feed_id)?;
            t.set("kind", kind.as_ref())?;
            t.set("status", status)?;
            t.set("message", message)?;
            t.set("retry_after", retry_after)?;
            Ok(LuaValue::Table(t))
        }
        EventPayload::Feed { id, url, title } => {
            let t = lua.create_table()?;
            t.set("id", id)?;
            t.set("url", url)?;
            t.set("title", title)?;
            Ok(LuaValue::Table(t))
        }
        EventPayload::PluginLoad => Ok(LuaValue::Nil),
    }
}

/// Copies the fields scripts may not change from `original` onto `modified`, the entry a
/// handler returned.
fn restore_read_only(modified: &mut FeedEntry, original: &FeedEntry) {
    modified.id = original.id;
    modified.feed_id = original.feed_id;
    modified.syndication_format = original.syndication_format.clone();
    modified.guid = original.guid.clone();
    modified.authors = original.authors.clone();
    modified.categories = original.categories.clone();
}

/// Runs user-supplied Lua handlers in response to server events.
///
/// See the module-level docs for the architecture, sandboxing, and script contract.
pub struct LuaScriptRunner {
    // Wrapping `Lua` in a `Mutex` gives us atomicity across the
    // `set_hook → call → remove_hook` sequence — mlua's internal locking only protects
    // individual calls, not sequences.
    lua: Mutex<Lua>,
    // Populated at load time by the `kiki.on` closure; read-only thereafter.
    handlers: Arc<Mutex<HashMap<Event, Vec<RegistryKey>>>>,
    // The handlers of the scans plugins have started with `kiki.entries.scan`.
    scans: api::Scans,
    // The time budget of the handler call in progress, which service calls give time back
    // to.
    budget: Arc<Budget>,
}

impl LuaScriptRunner {
    /// Build a runner from a slice of Lua script source strings, each with an empty config.
    ///
    /// See [`Self::from_sources`].
    ///
    /// # Errors
    ///
    /// As for [`Self::from_sources`].
    pub fn new(script_sources: &[String]) -> Result<Self, ScriptError> {
        let sources: Vec<ScriptSource> = script_sources.iter().map(ScriptSource::new).collect();
        Self::from_sources(&sources)
    }

    /// Build a runner from scripts and their configs.
    ///
    /// The VM is sandboxed and memory-capped before any script is loaded. Each script's
    /// top-level chunk is called with its config, converted to a Lua table, as its only
    /// argument, and registers handlers via `kiki.on`.
    ///
    /// # Errors
    ///
    /// Returns [`ScriptError::InvalidConfig`] if a config is not a JSON object,
    /// [`ScriptError::ScriptLoadError`] for VM-setup, compile, or top-level runtime failures,
    /// and [`ScriptError::InvalidReturnType`] if a chunk returns a value.
    pub fn from_sources(script_sources: &[ScriptSource]) -> Result<Self, ScriptError> {
        Self::from_sources_with(script_sources, None)
    }

    /// Build a runner from scripts and their configs, answering the calls they make
    /// through `kiki.store` and `kiki.entries` with `services`.
    ///
    /// See [`Self::from_sources`]; without `services`, those calls raise an error.
    ///
    /// # Errors
    ///
    /// As for [`Self::from_sources`].
    pub fn from_sources_with(
        script_sources: &[ScriptSource],
        services: Option<Arc<dyn ScriptServices>>,
    ) -> Result<Self, ScriptError> {
        let lua = Lua::new_with(
            LuaStdLib::STRING | LuaStdLib::TABLE | LuaStdLib::MATH | LuaStdLib::OS,
            LuaOptions::default(),
        )
        .map_err(ScriptError::ScriptLoadError)?;

        lua.set_memory_limit(SCRIPT_MEMORY_LIMIT_BYTES)
            .map_err(ScriptError::ScriptLoadError)?;

        // Remove dangerous OS functions, leaving only os.time, os.clock, os.date, os.difftime.
        {
            let os: LuaTable = lua
                .globals()
                .get("os")
                .map_err(ScriptError::ScriptLoadError)?;
            for func in &["execute", "exit", "getenv", "remove", "rename", "tmpname"] {
                os.set(*func, LuaValue::Nil)
                    .map_err(ScriptError::ScriptLoadError)?;
            }
        }

        // Remove other dangerous globals.
        for global in &["require", "dofile", "loadfile", "debug", "io", "package"] {
            lua.globals()
                .set(*global, LuaValue::Nil)
                .map_err(ScriptError::ScriptLoadError)?;
        }

        let handlers: Arc<Mutex<HashMap<Event, Vec<RegistryKey>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let kiki = lua.create_table().map_err(ScriptError::ScriptLoadError)?;

        {
            let handlers_for_on = Arc::clone(&handlers);
            let on_fn = lua
                .create_function(move |lua, (event_name, handler): (String, LuaFunction)| {
                    let event = Event::from_name(&event_name).ok_or_else(|| {
                        LuaError::RuntimeError(format!("unknown event: {}", event_name))
                    })?;
                    let key = lua.create_registry_value(handler)?;
                    handlers_for_on
                        .lock()
                        .map_err(|_| {
                            LuaError::RuntimeError("scripting state poisoned".to_string())
                        })?
                        .entry(event)
                        .or_default()
                        .push(key);
                    Ok(())
                })
                .map_err(ScriptError::ScriptLoadError)?;
            kiki.set("on", on_fn)
                .map_err(ScriptError::ScriptLoadError)?;
        }

        {
            let log_fn = lua
                .create_function(|_, (level, message): (String, String)| {
                    match level.as_str() {
                        "debug" => tracing::debug!(target: "kiki::lua", "{}", message),
                        "info" => tracing::info!(target: "kiki::lua", "{}", message),
                        "warn" => tracing::warn!(target: "kiki::lua", "{}", message),
                        "error" => tracing::error!(target: "kiki::lua", "{}", message),
                        other => {
                            return Err(LuaError::RuntimeError(format!(
                                "kiki.log: unknown level '{}'; expected debug|info|warn|error",
                                other
                            )));
                        }
                    }
                    Ok(())
                })
                .map_err(ScriptError::ScriptLoadError)?;
            kiki.set("log", log_fn)
                .map_err(ScriptError::ScriptLoadError)?;
        }

        regex_api::install(&lua, &kiki).map_err(ScriptError::ScriptLoadError)?;

        lua.globals()
            .set("kiki", kiki)
            .map_err(ScriptError::ScriptLoadError)?;

        // Load each plugin, passing it its config. Chunks register handlers via
        // `kiki.on(...)` side effects and must not return a value — any return (including a
        // function) is treated as an error to catch accidentally-copied legacy scripts at
        // load time rather than silently.
        let ctx = ApiContext {
            services,
            scans: Arc::new(Mutex::new(HashMap::new())),
            budget: Arc::new(Budget::default()),
            loading: Arc::new(AtomicBool::new(true)),
        };
        for source in script_sources {
            load_plugin(&lua, source, &ctx)?;
        }
        ctx.loading.store(false, Ordering::SeqCst);

        Ok(Self {
            lua: Mutex::new(lua),
            handlers,
            scans: ctx.scans,
            budget: ctx.budget,
        })
    }

    /// Resolve the stored registry keys for `event` into live [`LuaFunction`]s.
    ///
    /// The handlers mutex is acquired only for the short snapshot; we then drop it so that
    /// subsequent handler execution (which can call `kiki.on` recursively, in theory) does
    /// not deadlock.
    fn resolve_handlers(&self, lua: &Lua, event: Event) -> Vec<LuaFunction> {
        let guard = self
            .handlers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let keys = match guard.get(&event) {
            Some(v) => v,
            None => return Vec::new(),
        };
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            match lua.registry_value::<LuaFunction>(key) {
                Ok(f) => out.push(f),
                Err(e) => warn!(error = %e, "failed to resolve handler from Lua registry"),
            }
        }
        out
    }
}

/// Run a plugin's entrypoint, passing it its config.
///
/// Each plugin runs in an environment of its own, so the globals one plugin defines are
/// not seen by the others, and its `require` loads only its own modules.
fn load_plugin(lua: &Lua, source: &ScriptSource, ctx: &ApiContext) -> Result<(), ScriptError> {
    let config = parse_script_config(&source.config)?;
    let config = config::to_lua_table(lua, &config).map_err(ScriptError::ScriptLoadError)?;
    let env = plugin_env(lua, source, ctx).map_err(ScriptError::ScriptLoadError)?;
    let value: LuaValue = lua
        .load(source.text.as_str())
        .set_name(format!("@{}", source.name))
        .set_environment(env)
        .call(config)
        .map_err(ScriptError::ScriptLoadError)?;
    match value {
        LuaValue::Nil => Ok(()),
        other => Err(ScriptError::InvalidReturnType(
            other.type_name().to_string(),
        )),
    }
}

/// Build the environment a plugin's code runs in: a table that falls back to the VM's
/// globals, with a `require` that loads the plugin's own modules and a `kiki` table whose
/// `store` and `entries` act for the plugin.
///
/// `require(name)` runs the module named `name` the first time it is called, in the same
/// environment, and returns what the module returned (or `true` if it returned nothing).
/// Later calls return the same value without running the module again.
fn plugin_env(lua: &Lua, source: &ScriptSource, ctx: &ApiContext) -> LuaResult<LuaTable> {
    let env = lua.create_table()?;
    let meta = lua.create_table()?;
    meta.set("__index", lua.globals())?;
    env.set_metatable(Some(meta));
    env.raw_set("kiki", api::plugin_kiki_table(lua, &source.name, ctx)?)?;

    let plugin = source.name.clone();
    let modules: HashMap<String, String> = source
        .modules
        .iter()
        .map(|m| (m.name.clone(), m.text.clone()))
        .collect();
    let loaded = lua.create_table()?;
    let loading: Mutex<HashSet<String>> = Mutex::new(HashSet::new());
    let module_env = env.clone();

    let require = lua.create_function(move |lua, name: String| {
        let cached: LuaValue = loaded.raw_get(name.as_str())?;
        if !cached.is_nil() {
            return Ok(cached);
        }
        let text = modules.get(&name).ok_or_else(|| {
            LuaError::RuntimeError(format!("module '{name}' not found in plugin '{plugin}'"))
        })?;
        let first = loading
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(name.clone());
        if !first {
            return Err(LuaError::RuntimeError(format!(
                "module '{name}' in plugin '{plugin}' requires itself"
            )));
        }
        let result = lua
            .load(text.as_str())
            .set_name(format!("@{plugin}:{name}"))
            .set_environment(module_env.clone())
            .call::<LuaValue>(name.as_str());
        loading
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&name);
        let value = match result? {
            LuaValue::Nil => LuaValue::Boolean(true),
            value => value,
        };
        loaded.raw_set(name.as_str(), value.clone())?;
        Ok(value)
    })?;
    env.raw_set("require", require)?;
    Ok(env)
}

/// Invoke `handler(payload)` with the timeout hook installed for the duration of the call,
/// and `budget` running. Time the handler spends waiting on the server is given back to it
/// (see [`Budget`]).
fn call_with_timeout<R: FromLua>(
    lua: &Lua,
    budget: &Arc<Budget>,
    handler: &LuaFunction,
    payload: LuaValue,
) -> LuaResult<R> {
    budget.start(Duration::from_millis(SCRIPT_TIMEOUT_MS));
    let hook_budget = budget.clone();
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(HOOK_EVERY_N),
        move |_lua, _debug| {
            if hook_budget.expired() {
                Err(LuaError::RuntimeError(format!(
                    "script exceeded {SCRIPT_TIMEOUT_MS}ms time budget"
                )))
            } else {
                Ok(VmState::Continue)
            }
        },
    );
    let result = handler.call::<R>(payload);
    lua.remove_hook();
    budget.stop();
    result
}

impl ScriptRunner for LuaScriptRunner {
    fn dispatch_transform_entry(&self, entry: FeedEntry) -> anyhow::Result<Option<FeedEntry>> {
        let lua = self
            .lua
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let handlers = self.resolve_handlers(&lua, Event::EntryIngest);
        if handlers.is_empty() {
            return Ok(Some(entry));
        }

        // Keep the identity and read-only fields so scripts cannot corrupt them.
        let original = entry.clone();

        let mut current = entry;
        for handler in &handlers {
            let lua_entry = match current.clone().into_lua(&lua) {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "failed to convert FeedEntry to Lua table; skipping handler");
                    continue;
                }
            };

            match call_with_timeout::<LuaValue>(&lua, &self.budget, handler, lua_entry) {
                Ok(LuaValue::Nil) => return Ok(None),
                Ok(LuaValue::Table(t)) => match FeedEntry::from_lua(LuaValue::Table(t), &lua) {
                    Ok(mut modified) => {
                        restore_read_only(&mut modified, &original);
                        current = modified;
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            "failed to convert Lua table back to FeedEntry; passing entry through unmodified"
                        );
                    }
                },
                Ok(other) => {
                    warn!(
                        return_type = other.type_name(),
                        "entry.ingest handler returned invalid type; passing entry through unmodified"
                    );
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        "entry.ingest handler execution error; passing entry through unmodified"
                    );
                }
            }
        }

        Ok(Some(current))
    }

    fn dispatch_observe(&self, event: Event, payload: EventPayload) {
        let lua = self
            .lua
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let handlers = self.resolve_handlers(&lua, event);
        if handlers.is_empty() {
            return;
        }

        for handler in &handlers {
            let lua_payload = match payload_to_lua(&lua, payload.clone()) {
                Ok(v) => v,
                Err(e) => {
                    warn!(
                        event = event.name(),
                        error = %e,
                        "failed to convert event payload to Lua; dropping handler invocation"
                    );
                    continue;
                }
            };

            if let Err(e) = call_with_timeout::<LuaValue>(&lua, &self.budget, handler, lua_payload)
            {
                warn!(
                    event = event.name(),
                    error = %e,
                    "observe handler execution error; dropping"
                );
            }
        }
    }

    fn dispatch_scan(
        &self,
        scan_id: u64,
        entries: Vec<FeedEntry>,
    ) -> anyhow::Result<Option<Vec<Option<FeedEntry>>>> {
        let lua = self
            .lua
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let handler: LuaFunction = {
            let scans = self.scans.lock().unwrap_or_else(|e| e.into_inner());
            match scans.get(&scan_id) {
                Some(callbacks) => lua.registry_value(&callbacks.handler)?,
                None => return Ok(None),
            }
        };

        let start = Instant::now();
        let mut results = Vec::with_capacity(entries.len());
        for entry in entries {
            // At least one entry is always handled, so the scan makes progress.
            if !results.is_empty() && start.elapsed() >= SCAN_SLICE {
                break;
            }
            let lua_entry = entry.clone().into_lua(&lua)?;
            let result = match call_with_timeout::<LuaValue>(
                &lua,
                &self.budget,
                &handler,
                lua_entry,
            ) {
                Ok(LuaValue::Nil) => None,
                Ok(LuaValue::Table(t)) => match FeedEntry::from_lua(LuaValue::Table(t), &lua) {
                    Ok(mut modified) => {
                        restore_read_only(&mut modified, &entry);
                        Some(modified)
                    }
                    Err(e) => {
                        warn!(error = %e, "scan handler returned an invalid entry; skipping it");
                        None
                    }
                },
                Ok(other) => {
                    warn!(
                        return_type = other.type_name(),
                        "scan handler returned invalid type; skipping the entry"
                    );
                    None
                }
                Err(e) => {
                    warn!(error = %e, "scan handler execution error; skipping the entry");
                    None
                }
            };
            results.push(result);
        }
        Ok(Some(results))
    }

    fn finish_scan(&self, scan_id: u64, summary: Option<ScanSummary>) {
        let callbacks = self
            .scans
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&scan_id);
        let Some(callbacks) = callbacks else {
            return;
        };
        let lua = self
            .lua
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let (Some(summary), Some(on_done)) = (summary, &callbacks.on_done) {
            let result = lua.registry_value::<LuaFunction>(on_done).and_then(|f| {
                let t = lua.create_table()?;
                t.set("scanned", summary.scanned)?;
                t.set("updated", summary.updated)?;
                call_with_timeout::<LuaValue>(&lua, &self.budget, &f, LuaValue::Table(t))
            });
            if let Err(e) = result {
                warn!(error = %e, "scan on_done callback failed");
            }
        }
        let _ = lua.remove_registry_value(callbacks.handler);
        if let Some(on_done) = callbacks.on_done {
            let _ = lua.remove_registry_value(on_done);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn make_entry() -> FeedEntry {
        FeedEntry {
            id: None,
            feed_id: 1,
            syndication_format: "rss".to_string(),
            guid: "test-guid".to_string(),
            published_at: Some(1_700_000_000),
            title: "Test Title".to_string(),
            url: Some("https://example.com".to_string()),
            content: Some("<p>Hello</p>".to_string()),
            authors: vec!["Ada".to_string()],
            categories: vec!["news".to_string()],
            tags: vec![],
        }
    }

    #[test]
    fn config_is_passed_to_the_chunk() {
        let runner = LuaScriptRunner::from_sources(&[ScriptSource {
            text: r#"
                local config = ...
                kiki.on("entry.ingest", function(entry)
                    entry.title = config.prefix .. entry.title .. config.suffixes[2]
                        .. tostring(config.count) .. tostring(config.missing)
                    return entry
                end)
            "#
            .to_string(),
            config: r#"{"prefix": "[x] ", "suffixes": ["a", "b"], "count": 3, "missing": null}"#
                .to_string(),
            ..ScriptSource::new("")
        }])
        .unwrap();
        let out = runner
            .dispatch_transform_entry(make_entry())
            .unwrap()
            .unwrap();
        assert_eq!(out.title, "[x] Test Titleb3nil");
    }

    #[test]
    fn each_script_gets_its_own_config() {
        let script = r#"
            local config = ...
            kiki.on("entry.ingest", function(entry)
                entry.title = entry.title .. config.tag
                return entry
            end)
        "#;
        let runner = LuaScriptRunner::from_sources(&[
            ScriptSource {
                text: script.to_string(),
                config: r#"{"tag": "-1"}"#.to_string(),
                ..ScriptSource::new("")
            },
            ScriptSource {
                text: script.to_string(),
                config: r#"{"tag": "-2"}"#.to_string(),
                ..ScriptSource::new("")
            },
        ])
        .unwrap();
        let out = runner
            .dispatch_transform_entry(make_entry())
            .unwrap()
            .unwrap();
        assert_eq!(out.title, "Test Title-1-2");
    }

    #[test]
    fn scripts_without_config_get_an_empty_table() {
        let runner = LuaScriptRunner::new(&[r#"
            local config = ...
            assert(type(config) == "table" and next(config) == nil)
        "#
        .to_string()])
        .unwrap();
        drop(runner);
    }

    #[test]
    fn config_that_is_not_an_object_is_rejected() {
        for config in ["[]", "1", "not json"] {
            let err = LuaScriptRunner::from_sources(&[ScriptSource {
                text: String::new(),
                config: config.to_string(),
                ..ScriptSource::new("")
            }])
            .err()
            .unwrap();
            assert!(
                matches!(err, ScriptError::InvalidConfig(_)),
                "{config:?}: {err}"
            );
        }
    }

    #[test]
    fn a_bad_regex_in_config_fails_the_load() {
        let err = LuaScriptRunner::from_sources(&[ScriptSource {
            text: "local config = ...; kiki.regex(config.pattern)".to_string(),
            config: r#"{"pattern": "("}"#.to_string(),
            ..ScriptSource::new("")
        }])
        .err()
        .unwrap();
        assert!(matches!(err, ScriptError::ScriptLoadError(_)), "{err}");
    }

    /// A plugin with the given entrypoint and modules, and an empty config.
    fn plugin(name: &str, text: &str, modules: &[(&str, &str)]) -> ScriptSource {
        ScriptSource {
            name: name.to_string(),
            modules: modules
                .iter()
                .map(|(name, text)| crate::scripting::ScriptModule {
                    name: name.to_string(),
                    text: text.to_string(),
                })
                .collect(),
            ..ScriptSource::new(text)
        }
    }

    #[test]
    fn plugins_can_require_their_modules() {
        let runner = LuaScriptRunner::from_sources(&[plugin(
            "a",
            r#"
                local prefix = require("lib.prefix")
                assert(require("lib.prefix") == prefix, "modules are cached")
                assert(require("side_effect") == true)
                kiki.on("entry.ingest", function(entry)
                    entry.title = prefix.apply(entry.title)
                    return entry
                end)
            "#,
            &[
                (
                    "lib.prefix",
                    "local count = 0; return { apply = function(t) return '[a] ' .. t end }",
                ),
                ("side_effect", "kiki.log('debug', 'loaded')"),
            ],
        )])
        .unwrap();
        let out = runner
            .dispatch_transform_entry(make_entry())
            .unwrap()
            .unwrap();
        assert_eq!(out.title, "[a] Test Title");
    }

    #[test]
    fn plugins_cannot_require_each_others_modules() {
        let err = LuaScriptRunner::from_sources(&[
            plugin("a", "", &[("shared", "return 1")]),
            plugin("b", "require('shared')", &[]),
        ])
        .err()
        .unwrap();
        assert!(
            err.to_string()
                .contains("module 'shared' not found in plugin 'b'"),
            "{err}"
        );
    }

    #[test]
    fn circular_requires_fail() {
        let err = LuaScriptRunner::from_sources(&[plugin(
            "a",
            "require('x')",
            &[("x", "require('y')"), ("y", "require('x')")],
        )])
        .err()
        .unwrap();
        assert!(err.to_string().contains("requires itself"), "{err}");
    }

    #[test]
    fn plugins_have_their_own_globals() {
        let runner = LuaScriptRunner::from_sources(&[
            plugin("a", "counter = 'a'", &[]),
            plugin(
                "b",
                r#"
                    assert(counter == nil, "globals of plugin a leaked into plugin b")
                    kiki.on("entry.ingest", function(entry)
                        entry.title = tostring(counter)
                        return entry
                    end)
                "#,
                &[],
            ),
        ])
        .unwrap();
        let out = runner
            .dispatch_transform_entry(make_entry())
            .unwrap()
            .unwrap();
        assert_eq!(out.title, "nil");
    }

    #[test]
    fn errors_name_the_plugin() {
        let err = LuaScriptRunner::from_sources(&[plugin("broken-plugin", "error('boom')", &[])])
            .err()
            .unwrap();
        assert!(err.to_string().contains("broken-plugin"), "{err}");
    }

    #[test]
    fn passthrough_handler_returns_entry_unmodified() {
        let runner = LuaScriptRunner::new(&[
            r#"kiki.on("entry.ingest", function(entry) return entry end)"#.to_string(),
        ])
        .unwrap();
        let entry = make_entry();
        let out = runner
            .dispatch_transform_entry(entry.clone())
            .unwrap()
            .unwrap();
        assert_eq!(out.title, entry.title);
        assert_eq!(out.guid, entry.guid);
        assert_eq!(out.feed_id, entry.feed_id);
    }

    #[test]
    fn filtering_handler_returns_none() {
        let runner = LuaScriptRunner::new(&[
            r#"kiki.on("entry.ingest", function(entry) return nil end)"#.to_string(),
        ])
        .unwrap();
        let result = runner.dispatch_transform_entry(make_entry()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn modifying_handler_changes_title() {
        let runner = LuaScriptRunner::new(&[r#"
            kiki.on("entry.ingest", function(entry)
                entry.title = "[NEWS] " .. entry.title
                return entry
            end)
        "#
        .to_string()])
        .unwrap();
        let result = runner
            .dispatch_transform_entry(make_entry())
            .unwrap()
            .unwrap();
        assert_eq!(result.title, "[NEWS] Test Title");
    }

    #[test]
    fn handler_chaining_runs_in_order() {
        let runner = LuaScriptRunner::new(&[
            r#"kiki.on("entry.ingest", function(entry)
                entry.title = "A:" .. entry.title
                return entry
            end)"#
                .to_string(),
            r#"kiki.on("entry.ingest", function(entry)
                entry.title = "B:" .. entry.title
                return entry
            end)"#
                .to_string(),
        ])
        .unwrap();
        let result = runner
            .dispatch_transform_entry(make_entry())
            .unwrap()
            .unwrap();
        assert_eq!(result.title, "B:A:Test Title");
    }

    #[test]
    fn tagging_handler_populates_tags() {
        let runner = LuaScriptRunner::new(&[r#"
            kiki.on("entry.ingest", function(entry)
                table.insert(entry.tags, "rust")
                return entry
            end)
        "#
        .to_string()])
        .unwrap();
        let result = runner
            .dispatch_transform_entry(make_entry())
            .unwrap()
            .unwrap();
        assert_eq!(result.tags, vec!["rust"]);
    }

    #[test]
    fn tag_removal_works() {
        let mut entry = make_entry();
        entry.tags = vec!["keep".to_string(), "remove".to_string()];
        let runner = LuaScriptRunner::new(&[r#"
            kiki.on("entry.ingest", function(entry)
                local filtered = {}
                for _, tag in ipairs(entry.tags) do
                    if tag ~= "remove" then
                        table.insert(filtered, tag)
                    end
                end
                entry.tags = filtered
                return entry
            end)
        "#
        .to_string()])
        .unwrap();
        let result = runner.dispatch_transform_entry(entry).unwrap().unwrap();
        assert_eq!(result.tags, vec!["keep"]);
    }

    #[test]
    fn erroring_handler_does_not_filter_entry() {
        let runner = LuaScriptRunner::new(&[
            r#"kiki.on("entry.ingest", function(entry) error("oops") end)"#.to_string(),
        ])
        .unwrap();
        let result = runner.dispatch_transform_entry(make_entry()).unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn identity_fields_cannot_be_changed_by_handler() {
        let runner = LuaScriptRunner::new(&[r#"
            kiki.on("entry.ingest", function(entry)
                entry.feed_id = 999
                entry.guid = "hacked"
                entry.syndication_format = "atom"
                return entry
            end)
        "#
        .to_string()])
        .unwrap();
        let result = runner
            .dispatch_transform_entry(make_entry())
            .unwrap()
            .unwrap();
        assert_eq!(result.feed_id, 1);
        assert_eq!(result.guid, "test-guid");
        assert_eq!(result.syndication_format, "rss");
    }

    #[test]
    fn first_nil_in_chain_stops_processing() {
        let runner = LuaScriptRunner::new(&[
            r#"kiki.on("entry.ingest", function(entry) return nil end)"#.to_string(),
            // This handler would panic if called, but it should never be reached.
            r#"kiki.on("entry.ingest", function(entry) error("should not run") end)"#.to_string(),
        ])
        .unwrap();
        let result = runner.dispatch_transform_entry(make_entry()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn kiki_on_registers_entry_ingest_handler() {
        let runner = LuaScriptRunner::new(&[r#"
            kiki.on("entry.ingest", function(entry)
                entry.title = "[EVENT] " .. entry.title
                return entry
            end)
        "#
        .to_string()])
        .unwrap();
        let result = runner
            .dispatch_transform_entry(make_entry())
            .unwrap()
            .unwrap();
        assert_eq!(result.title, "[EVENT] Test Title");
    }

    #[test]
    fn kiki_on_unknown_event_fails_to_load() {
        let result = LuaScriptRunner::new(&[r#"
            kiki.on("not.a.real.event", function() end)
        "#
        .to_string()]);
        assert!(
            matches!(result, Err(ScriptError::ScriptLoadError(_))),
            "expected ScriptLoadError for unknown event name"
        );
    }

    #[test]
    fn returning_a_value_from_top_level_is_rejected() {
        let result =
            LuaScriptRunner::new(&[r#"return function(entry) return entry end"#.to_string()]);
        assert!(
            matches!(result, Err(ScriptError::InvalidReturnType(_))),
            "expected InvalidReturnType when a script returns a value from the top-level chunk"
        );
    }

    #[test]
    fn multiple_handlers_for_same_event_run_in_registration_order() {
        let runner = LuaScriptRunner::new(&[r#"
            kiki.on("entry.ingest", function(entry)
                entry.title = "X:" .. entry.title
                return entry
            end)
            kiki.on("entry.ingest", function(entry)
                entry.title = "Y:" .. entry.title
                return entry
            end)
        "#
        .to_string()])
        .unwrap();
        let result = runner
            .dispatch_transform_entry(make_entry())
            .unwrap()
            .unwrap();
        assert_eq!(result.title, "Y:X:Test Title");
    }

    #[test]
    fn observe_handler_sees_feed_payload() {
        // We can't easily capture side effects from a pure Lua handler without touching
        // tracing, so assert via the one side effect available: the handler can raise an
        // error which we then verify doesn't crash the dispatch.
        let runner = LuaScriptRunner::new(&[r#"
            kiki.on("feed.added", function(feed)
                if feed.id ~= 42 then error("bad id") end
                if feed.url ~= "https://example.com/rss" then error("bad url") end
                if feed.title ~= "Example" then error("bad title") end
            end)
        "#
        .to_string()])
        .unwrap();
        // Correct payload: no error → dispatch completes silently.
        runner.dispatch_observe(
            Event::FeedAdded,
            EventPayload::Feed {
                id: 42,
                url: "https://example.com/rss".to_string(),
                title: "Example".to_string(),
            },
        );
        // Wrong payload: handler errors, but dispatch still returns without panicking.
        runner.dispatch_observe(
            Event::FeedAdded,
            EventPayload::Feed {
                id: 99,
                url: "x".to_string(),
                title: "y".to_string(),
            },
        );
    }

    #[test]
    fn timeout_terminates_infinite_loop() {
        let runner = LuaScriptRunner::new(&[r#"
            kiki.on("entry.ingest", function(entry) while true do end end)
        "#
        .to_string()])
        .unwrap();
        let start = Instant::now();
        // The handler never returns an entry; the timeout fires and dispatch logs a warning
        // and falls through to returning the entry unchanged.
        let result = runner.dispatch_transform_entry(make_entry()).unwrap();
        let elapsed = start.elapsed();
        assert!(
            result.is_some(),
            "timed-out handler should pass entry through"
        );
        // Give a generous upper bound — the hook fires every 1000 instructions so it may
        // overshoot the 100ms budget slightly, but nowhere near seconds.
        assert!(
            elapsed < Duration::from_secs(2),
            "timeout did not fire within 2s, elapsed={:?}",
            elapsed
        );
    }

    #[test]
    fn memory_limit_is_applied() {
        // Allocate more than the configured limit and confirm the handler errors (dispatch
        // still returns the entry unchanged).
        let runner = LuaScriptRunner::new(&[format!(
            r#"kiki.on("entry.ingest", function(entry)
                local t = {{}}
                for i = 1, {} do t[i] = string.rep("x", 1024) end
                return entry
            end)"#,
            SCRIPT_MEMORY_LIMIT_BYTES / 512 // definitely exceeds the cap
        )])
        .unwrap();
        let result = runner.dispatch_transform_entry(make_entry()).unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn handlers_can_use_regexes_compiled_at_load_time() {
        let runner = LuaScriptRunner::new(&[r#"
            local promo = kiki.regex([[\b(sponsored|giveaway)\b]], "i")
            kiki.on("entry.ingest", function(entry)
                if promo:is_match(entry.title) then
                    table.insert(entry.tags, "promo")
                end
                return entry
            end)
        "#
        .to_string()])
        .unwrap();

        let mut entry = make_entry();
        entry.title = "A Sponsored post".to_string();
        let result = runner.dispatch_transform_entry(entry).unwrap().unwrap();
        assert_eq!(result.tags, vec!["promo"]);

        let result = runner
            .dispatch_transform_entry(make_entry())
            .unwrap()
            .unwrap();
        assert!(result.tags.is_empty());
    }

    #[test]
    fn an_invalid_regex_fails_the_script_at_load_time() {
        let result = LuaScriptRunner::new(&[r#"local re = kiki.regex("(")"#.to_string()]);
        assert!(matches!(result, Err(ScriptError::ScriptLoadError(_))));
    }

    #[test]
    fn kiki_log_does_not_error_on_valid_level() {
        let runner = LuaScriptRunner::new(&[r#"
            kiki.on("entry.ingest", function(entry)
                kiki.log("info", "hello from lua")
                return entry
            end)
        "#
        .to_string()])
        .unwrap();
        let result = runner.dispatch_transform_entry(make_entry()).unwrap();
        assert!(result.is_some());
    }

    /// Answers every scan request with scan 7, and makes every store read
    /// take `delay`.
    struct SlowServices {
        delay: Duration,
    }

    impl ScriptServices for SlowServices {
        fn call(
            &self,
            _plugin: &str,
            call: crate::scripting::ServiceCall,
        ) -> Result<crate::scripting::ServiceReply, String> {
            use crate::scripting::{ServiceCall, ServiceReply};
            match call {
                ServiceCall::StartScan { .. } => Ok(ServiceReply::ScanStarted(7)),
                ServiceCall::StoreGet { .. } => {
                    std::thread::sleep(self.delay);
                    Ok(ServiceReply::Value(None))
                }
                other => Err(format!("unexpected {other:?}")),
            }
        }
    }

    /// A runner whose plugin, on `plugin.load`, starts scan 7 with `handler`.
    fn scanning_runner(handler: &str, delay: Duration) -> LuaScriptRunner {
        let text =
            format!(r#"kiki.on("plugin.load", function() kiki.entries.scan({handler}) end)"#);
        let runner = LuaScriptRunner::from_sources_with(
            &[ScriptSource::new(text)],
            Some(Arc::new(SlowServices { delay })),
        )
        .unwrap();
        runner.dispatch_observe(Event::PluginLoad, EventPayload::PluginLoad);
        runner
    }

    #[test]
    fn scan_dispatches_are_time_sliced() {
        // Each entry takes about 20ms, so a 50ms slice ends well before 10.
        let runner = scanning_runner(
            r#"function(entry)
                local start = os.clock()
                while os.clock() - start < 0.02 do end
                return entry
            end"#,
            Duration::ZERO,
        );
        let entries = vec![make_entry(); 10];
        let results = runner.dispatch_scan(7, entries).unwrap().unwrap();
        assert!(
            !results.is_empty() && results.len() < 10,
            "handled {} entries",
            results.len()
        );
        assert!(runner
            .dispatch_scan(8, vec![make_entry()])
            .unwrap()
            .is_none());
    }

    #[test]
    fn time_spent_waiting_on_the_server_is_not_counted() {
        // A store read takes longer than the whole budget, but the handler
        // still finishes.
        let runner = scanning_runner(
            r#"function(entry)
                kiki.store.get("k")
                entry.title = "done"
                return entry
            end"#,
            Duration::from_millis(3 * SCRIPT_TIMEOUT_MS),
        );
        let results = runner
            .dispatch_scan(7, vec![make_entry()])
            .unwrap()
            .unwrap();
        assert_eq!(results.first().unwrap().as_ref().unwrap().title, "done");
    }

    #[test]
    fn waiting_on_the_server_only_goes_so_far() {
        // Past the allowance, waiting counts again, so calling the server in
        // a loop cannot keep a handler running for ever.
        let runner = scanning_runner(
            r#"function(entry)
                while true do kiki.store.get("k") end
            end"#,
            Duration::from_millis(50),
        );
        let start = Instant::now();
        let results = runner
            .dispatch_scan(7, vec![make_entry()])
            .unwrap()
            .unwrap();
        assert!(results.first().unwrap().is_none());
        assert!(start.elapsed() < api::MAX_CALL_ALLOWANCE * 3);
    }
}
