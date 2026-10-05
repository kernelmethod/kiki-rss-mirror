//! The parts of the `kiki` Lua API that reach the server, `kiki.store`, `kiki.entries` and
//! `kiki.feeds`, and the plugin's timers, `kiki.every`.
//!
//! Each plugin sees its own `kiki` table, which holds these functions bound to the plugin's
//! name and falls back to the shared `kiki` table (`kiki.on`, `kiki.log`, `kiki.regex`,
//! `kiki.html`)
//! for everything else. Every call goes through [`ScriptServices`], so it works the same
//! whether the VM runs in the server or in the sandboxed script host.

use super::config::{from_lua_value, to_lua_value};
use crate::scripting::{
    DeleteFilter, ScanOptions, ScriptServices, ServiceCall, ServiceReply, TimeBudget, TIMER_TICK,
};
use mlua::prelude::*;
use mlua::RegistryKey;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The callbacks of a scan a plugin started.
pub(super) struct ScanCallbacks {
    /// Called with each entry.
    pub handler: RegistryKey,
    /// Called once the scan has gone through every entry, if the plugin gave one.
    pub on_done: Option<RegistryKey>,
    /// The time budget of the plugin that started the scan, for each call of `handler` and
    /// `on_done`.
    pub budget: TimeBudget,
}

/// The scans plugins have started, keyed by scan id.
pub(super) type Scans = Arc<Mutex<HashMap<u64, ScanCallbacks>>>;

/// A timer a plugin started with `kiki.every(secs, handler)`.
pub(super) struct Timer {
    /// Called each time the timer is due.
    pub handler: RegistryKey,
    /// The name of the plugin that started the timer.
    pub plugin: Arc<str>,
    /// The time budget of the plugin that started the timer, for each call of `handler`.
    pub budget: TimeBudget,
    /// How long the timer waits between calls.
    pub every: Duration,
    /// When the timer is next due.
    pub next: Instant,
}

/// The timers plugins have started, in the order they were started.
pub(super) type Timers = Arc<Mutex<Vec<Timer>>>;

/// The longest a timer may wait between calls: a year. Longer waits would outlast any
/// server, since timers start over whenever plugins reload.
pub const MAX_TIMER_INTERVAL: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// How much of the time a handler spends waiting on the server, in calls to `kiki.store`,
/// `kiki.entries` and `kiki.feeds`, does not count against its time budget. Past this,
/// waiting counts as usual, so a handler cannot run for ever by calling the server in a
/// loop.
pub const MAX_CALL_ALLOWANCE: Duration = Duration::from_secs(1);

/// The time budget of the handler call in progress, if any.
///
/// The budget is wall-clock time, but time spent waiting on the server in a service call
/// is given back, up to [`MAX_CALL_ALLOWANCE`] per handler call: how long the database or
/// the channel to the server takes is out of the handler's hands.
#[derive(Default)]
pub(super) struct Budget {
    state: Mutex<Option<BudgetState>>,
}

struct BudgetState {
    deadline: Instant,
    allowance: Duration,
}

impl Budget {
    /// Starts a budget of `limit` for a handler call.
    pub fn start(&self, limit: Duration) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = Some(BudgetState {
            deadline: Instant::now() + limit,
            allowance: MAX_CALL_ALLOWANCE,
        });
    }

    /// Ends the handler call's budget.
    pub fn stop(&self) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Whether the handler call has used up its budget.
    pub fn expired(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|s| Instant::now() >= s.deadline)
    }

    /// Gives back `waited`, time spent waiting on the server, as far as the allowance
    /// goes.
    fn give_back(&self, waited: Duration) {
        if let Some(s) = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            let credit = waited.min(s.allowance);
            s.allowance -= credit;
            s.deadline += credit;
        }
    }
}

/// What the per-plugin API functions share with the runner.
#[derive(Clone)]
pub(super) struct ApiContext {
    /// Answers the plugins' calls. `None` where there is no server to call, as in tests;
    /// the functions then raise an error.
    pub services: Option<Arc<dyn ScriptServices>>,
    pub scans: Scans,
    pub timers: Timers,
    /// The budget of the handler call in progress.
    pub budget: Arc<Budget>,
    /// Set while plugins' top-level chunks run, when scans cannot start yet: the runner
    /// they would be dispatched to is not installed until every plugin has loaded.
    pub loading: Arc<AtomicBool>,
}

impl ApiContext {
    fn call(&self, plugin: &str, name: &str, call: ServiceCall) -> LuaResult<ServiceReply> {
        let services = self.services.as_ref().ok_or_else(|| {
            LuaError::RuntimeError(format!("kiki.{name}: not available in this runner"))
        })?;
        // The timeout hook only runs every so many instructions, and a loop
        // of calls runs few of them while taking a long time, so the budget
        // is checked here too.
        if self.budget.expired() {
            return Err(LuaError::RuntimeError(format!(
                "kiki.{name}: the handler has used up its time budget"
            )));
        }
        let start = Instant::now();
        let result = services.call(plugin, call);
        self.budget.give_back(start.elapsed());
        result.map_err(|e| LuaError::RuntimeError(format!("kiki.{name}: {e}")))
    }
}

fn unexpected(name: &str, reply: ServiceReply) -> LuaError {
    LuaError::RuntimeError(format!("kiki.{name}: unexpected reply {reply:?}"))
}

/// Builds the `kiki` table for the plugin named `plugin`, whose handlers have time budget
/// `budget`.
pub(super) fn plugin_kiki_table(
    lua: &Lua,
    plugin: &str,
    budget: TimeBudget,
    ctx: &ApiContext,
) -> LuaResult<LuaTable> {
    let kiki = lua.create_table()?;
    let meta = lua.create_table()?;
    meta.set("__index", lua.globals().get::<LuaTable>("kiki")?)?;
    kiki.set_metatable(Some(meta));

    kiki.raw_set("store", store_table(lua, plugin, ctx)?)?;
    kiki.raw_set("entries", entries_table(lua, plugin, budget, ctx)?)?;
    kiki.raw_set("feeds", feeds_table(lua, plugin, ctx)?)?;
    kiki.raw_set("every", every_function(lua, plugin, budget, ctx)?)?;
    Ok(kiki)
}

/// Builds `kiki.every(secs, handler)` for the plugin named `plugin`, which starts a timer
/// calling `handler` every `secs` seconds, the first time `secs` seconds from now.
///
/// Timers run on the server's [`TIMER_TICK`], so `secs` must be at least that long, and a
/// timer may run up to a tick late. They last until plugins next reload.
fn every_function(
    lua: &Lua,
    plugin: &str,
    budget: TimeBudget,
    ctx: &ApiContext,
) -> LuaResult<LuaFunction> {
    let plugin: Arc<str> = Arc::from(plugin);
    let timers = ctx.timers.clone();
    lua.create_function(move |lua, (secs, handler): (LuaValue, LuaFunction)| {
        let err = |m: String| LuaError::RuntimeError(format!("kiki.every: {m}"));
        let secs = match secs {
            LuaValue::Integer(i) => i as f64,
            LuaValue::Number(n) => n,
            other => {
                return Err(err(format!(
                    "expected a number of seconds, not a {}",
                    other.type_name()
                )))
            }
        };
        let (min, max) = (TIMER_TICK.as_secs_f64(), MAX_TIMER_INTERVAL.as_secs_f64());
        // Written so that NaN fails it too.
        if !(secs >= min && secs <= max) {
            return Err(err(format!(
                "the interval must be between {min} and {max} seconds, not {secs}"
            )));
        }
        let every = Duration::from_secs_f64(secs);
        let timer = Timer {
            handler: lua.create_registry_value(handler)?,
            plugin: plugin.clone(),
            budget,
            every,
            next: Instant::now() + every,
        };
        timers.lock().unwrap_or_else(|e| e.into_inner()).push(timer);
        Ok(())
    })
}

fn feeds_table(lua: &Lua, plugin: &str, ctx: &ApiContext) -> LuaResult<LuaTable> {
    let feeds = lua.create_table()?;

    let (p, c) = (plugin.to_string(), ctx.clone());
    feeds.set(
        "get",
        lua.create_function(move |lua, feed_id: i64| {
            match c.call(&p, "feeds.get", ServiceCall::GetFeed { feed_id })? {
                ServiceReply::Feed(None) => Ok(LuaValue::Nil),
                ServiceReply::Feed(Some(feed)) => {
                    let table = lua.create_table()?;
                    table.set("id", feed.id)?;
                    table.set("url", feed.url)?;
                    table.set("title", feed.title)?;
                    Ok(LuaValue::Table(table))
                }
                other => Err(unexpected("feeds.get", other)),
            }
        })?,
    )?;

    Ok(feeds)
}

fn store_table(lua: &Lua, plugin: &str, ctx: &ApiContext) -> LuaResult<LuaTable> {
    let store = lua.create_table()?;

    let (p, c) = (plugin.to_string(), ctx.clone());
    store.set(
        "get",
        lua.create_function(move |lua, key: String| {
            match c.call(&p, "store.get", ServiceCall::StoreGet { key })? {
                ServiceReply::Value(None) => Ok(LuaValue::Nil),
                ServiceReply::Value(Some(text)) => {
                    let value = serde_json::from_str(&text).map_err(|e| {
                        LuaError::RuntimeError(format!("kiki.store.get: invalid value: {e}"))
                    })?;
                    to_lua_value(lua, &value)
                }
                other => Err(unexpected("store.get", other)),
            }
        })?,
    )?;

    let (p, c) = (plugin.to_string(), ctx.clone());
    store.set(
        "set",
        lua.create_function(move |_, (key, value): (String, LuaValue)| {
            let value = match value {
                LuaValue::Nil => None,
                value => Some(
                    from_lua_value(&value)
                        .map_err(|e| LuaError::RuntimeError(format!("kiki.store.set: {e}")))?
                        .to_string(),
                ),
            };
            match c.call(&p, "store.set", ServiceCall::StoreSet { key, value })? {
                ServiceReply::Done => Ok(()),
                other => Err(unexpected("store.set", other)),
            }
        })?,
    )?;

    Ok(store)
}

fn entries_table(
    lua: &Lua,
    plugin: &str,
    budget: TimeBudget,
    ctx: &ApiContext,
) -> LuaResult<LuaTable> {
    let entries = lua.create_table()?;

    for (name, present) in [("tag", true), ("untag", false)] {
        let (p, c) = (plugin.to_string(), ctx.clone());
        let full = format!("entries.{name}");
        entries.set(
            name,
            lua.create_function(move |_, (entry_id, tag): (i64, String)| {
                let call = ServiceCall::SetEntryTag {
                    entry_id,
                    tag,
                    present,
                };
                match c.call(&p, &full, call)? {
                    ServiceReply::Changed(changed) => Ok(changed),
                    other => Err(unexpected(&full, other)),
                }
            })?,
        )?;
    }

    let (p, c) = (plugin.to_string(), ctx.clone());
    entries.set(
        "scan",
        lua.create_function(move |lua, args: LuaMultiValue| {
            let (options, handler, on_done) = scan_args(args)?;
            if c.loading.load(Ordering::SeqCst) {
                return Err(LuaError::RuntimeError(
                    "kiki.entries.scan: scans cannot start while plugins are loading; \
                     start them from a plugin.load handler"
                        .to_string(),
                ));
            }
            let id = match c.call(&p, "entries.scan", ServiceCall::StartScan { options })? {
                ServiceReply::ScanStarted(id) => id,
                other => return Err(unexpected("entries.scan", other)),
            };
            // The scan cannot reach the handler before it is registered: its batches are
            // dispatched through this runner, which is busy until this handler returns.
            let callbacks = ScanCallbacks {
                handler: lua.create_registry_value(handler)?,
                on_done: on_done.map(|f| lua.create_registry_value(f)).transpose()?,
                budget,
            };
            c.scans
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id, callbacks);
            Ok(id)
        })?,
    )?;

    let (p, c) = (plugin.to_string(), ctx.clone());
    entries.set(
        "delete_where",
        lua.create_function(move |_, filter: LuaValue| {
            let filter = delete_filter(filter)?;
            match c.call(
                &p,
                "entries.delete_where",
                ServiceCall::DeleteEntries { filter },
            )? {
                ServiceReply::Deleted(count) => Ok(count),
                other => Err(unexpected("entries.delete_where", other)),
            }
        })?,
    )?;

    Ok(entries)
}

/// Parses the argument of `kiki.entries.delete_where(filter)`.
fn delete_filter(filter: LuaValue) -> LuaResult<DeleteFilter> {
    let err = |m: &str| LuaError::RuntimeError(format!("kiki.entries.delete_where: {m}"));
    let LuaValue::Table(filter) = filter else {
        return Err(err("expected a table of filters"));
    };
    let mut parsed = DeleteFilter::default();
    let mut dropped_before = None;
    for pair in filter.pairs::<String, LuaValue>() {
        let (key, value) = pair.map_err(|_| err("filter names must be strings"))?;
        let int = |v: &LuaValue| match v {
            LuaValue::Integer(i) => Ok(*i),
            LuaValue::Number(n) if n.fract() == 0.0 => Ok(*n as i64),
            _ => Err(err(&format!("filter '{key}' must be an integer"))),
        };
        match key.as_str() {
            "dropped_before" => dropped_before = Some(int(&value)?),
            "feed_id" => parsed.feed_id = Some(int(&value)?),
            "published_before" => parsed.published_before = Some(int(&value)?),
            "include_saved" => {
                parsed.include_saved = value
                    .as_boolean()
                    .ok_or_else(|| err("filter 'include_saved' must be a boolean"))?
            }
            other => {
                return Err(err(&format!(
                    "unknown filter '{other}'; expected dropped_before, feed_id, \
                     published_before or include_saved"
                )))
            }
        }
    }
    parsed.dropped_before = dropped_before.ok_or_else(|| {
        err("'dropped_before' is required: only entries their feed has stopped listing are deleted")
    })?;
    Ok(parsed)
}

/// Parses the arguments of `kiki.entries.scan([options,] handler [, on_done])`.
fn scan_args(args: LuaMultiValue) -> LuaResult<(ScanOptions, LuaFunction, Option<LuaFunction>)> {
    let err = |m: &str| LuaError::RuntimeError(format!("kiki.entries.scan: {m}"));
    let mut args: Vec<LuaValue> = args.into_iter().collect();
    // Trailing nils are as good as absent.
    while matches!(args.last(), Some(LuaValue::Nil)) {
        args.pop();
    }
    let options = match args.first() {
        Some(LuaValue::Table(t)) => Some(t.clone()),
        Some(LuaValue::Nil) => None,
        Some(LuaValue::Function(_)) => {
            args.insert(0, LuaValue::Nil);
            None
        }
        _ => return Err(err("expected ([options,] handler [, on_done])")),
    };
    let (handler, on_done) = match args.get(1..).unwrap_or_default() {
        [LuaValue::Function(h)] => (h.clone(), None),
        [LuaValue::Function(h), LuaValue::Function(d)] => (h.clone(), Some(d.clone())),
        _ => return Err(err("expected ([options,] handler [, on_done])")),
    };
    let mut parsed = ScanOptions::default();
    if let Some(options) = options {
        for pair in options.pairs::<String, LuaValue>() {
            let (key, value) = pair.map_err(|_| err("option names must be strings"))?;
            let int = |v: &LuaValue| match v {
                LuaValue::Integer(i) => Ok(*i),
                LuaValue::Number(n) if n.fract() == 0.0 => Ok(*n as i64),
                _ => Err(err(&format!("option '{key}' must be an integer"))),
            };
            match key.as_str() {
                "feed_id" => parsed.feed_id = Some(int(&value)?),
                "since" => parsed.since = Some(int(&value)?),
                "include_hidden" => {
                    parsed.include_hidden = value
                        .as_boolean()
                        .ok_or_else(|| err("option 'include_hidden' must be a boolean"))?
                }
                other => {
                    return Err(err(&format!(
                        "unknown option '{other}'; expected feed_id, since or include_hidden"
                    )))
                }
            }
        }
    }
    Ok((parsed, handler, on_done))
}
