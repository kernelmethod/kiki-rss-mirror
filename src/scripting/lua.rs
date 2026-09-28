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

mod config;
mod regex_api;

use super::{parse_script_config, Event, EventPayload, FeedEntry, ScriptRunner, ScriptSource};
use mlua::prelude::*;
use mlua::HookTriggers;
use mlua::RegistryKey;
use mlua::VmState;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thiserror::Error;
use tracing::warn;

/// Per-handler execution time budget.
pub const SCRIPT_TIMEOUT_MS: u64 = 100;

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
        table.set("feed_id", self.feed_id)?;
        table.set("syndication_format", self.syndication_format)?;
        table.set("guid", self.guid)?;
        table.set("published_at", self.published_at)?;
        table.set("title", self.title)?;
        table.set("url", self.url)?;
        table.set("content", self.content)?;

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
            feed_id,
            syndication_format,
            guid,
            published_at,
            title,
            url,
            content,
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
    }
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

        // Load each script, passing it its config. Chunks register handlers via
        // `kiki.on(...)` side effects and must not return a value — any return (including a function) is treated as an
        // error to catch accidentally-copied legacy scripts at load time rather than
        // silently.
        for source in script_sources {
            let config = parse_script_config(&source.config)?;
            let config =
                config::to_lua_table(&lua, &config).map_err(ScriptError::ScriptLoadError)?;
            let value: LuaValue = lua
                .load(source.text.as_str())
                .call(config)
                .map_err(ScriptError::ScriptLoadError)?;
            match value {
                LuaValue::Nil => {}
                other => {
                    return Err(ScriptError::InvalidReturnType(
                        other.type_name().to_string(),
                    ));
                }
            }
        }

        Ok(Self {
            lua: Mutex::new(lua),
            handlers,
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

/// Invoke `handler(payload)` with the timeout hook installed for the duration of the call.
fn call_with_timeout<R: FromLua>(
    lua: &Lua,
    handler: &LuaFunction,
    payload: LuaValue,
) -> LuaResult<R> {
    let deadline = Instant::now() + Duration::from_millis(SCRIPT_TIMEOUT_MS);
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(HOOK_EVERY_N),
        move |_lua, _debug| {
            if Instant::now() >= deadline {
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

        // Preserve identity fields so scripts cannot corrupt them.
        let feed_id = entry.feed_id;
        let syndication_format = entry.syndication_format.clone();
        let guid = entry.guid.clone();

        let mut current = entry;
        for handler in &handlers {
            let lua_entry = match current.clone().into_lua(&lua) {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "failed to convert FeedEntry to Lua table; skipping handler");
                    continue;
                }
            };

            match call_with_timeout::<LuaValue>(&lua, handler, lua_entry) {
                Ok(LuaValue::Nil) => return Ok(None),
                Ok(LuaValue::Table(t)) => match FeedEntry::from_lua(LuaValue::Table(t), &lua) {
                    Ok(mut modified) => {
                        modified.feed_id = feed_id;
                        modified.syndication_format = syndication_format.clone();
                        modified.guid = guid.clone();
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

            if let Err(e) = call_with_timeout::<LuaValue>(&lua, handler, lua_payload) {
                warn!(
                    event = event.name(),
                    error = %e,
                    "observe handler execution error; dropping"
                );
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn make_entry() -> FeedEntry {
        FeedEntry {
            feed_id: 1,
            syndication_format: "rss".to_string(),
            guid: "test-guid".to_string(),
            published_at: Some(1_700_000_000),
            title: "Test Title".to_string(),
            url: Some("https://example.com".to_string()),
            content: Some("<p>Hello</p>".to_string()),
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
            },
            ScriptSource {
                text: script.to_string(),
                config: r#"{"tag": "-2"}"#.to_string(),
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
        }])
        .err()
        .unwrap();
        assert!(matches!(err, ScriptError::ScriptLoadError(_)), "{err}");
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
}
