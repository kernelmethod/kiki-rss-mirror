//! The parts of the `kiki` Lua API that reach the server: `kiki.store` and `kiki.entries`.
//!
//! Each plugin sees its own `kiki` table, which holds these functions bound to the plugin's
//! name and falls back to the shared `kiki` table (`kiki.on`, `kiki.log`, `kiki.regex`)
//! for everything else. Every call goes through [`ScriptServices`], so it works the same
//! whether the VM runs in the server or in the sandboxed script host.

use super::config::{from_lua_value, to_lua_value};
use crate::scripting::{ScanOptions, ScriptServices, ServiceCall, ServiceReply};
use mlua::prelude::*;
use mlua::RegistryKey;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// The handlers of the scans plugins have started, keyed by scan id.
pub(super) type Scans = Arc<Mutex<HashMap<u64, RegistryKey>>>;

/// What the per-plugin API functions share with the runner.
#[derive(Clone)]
pub(super) struct ApiContext {
    /// Answers the plugins' calls. `None` where there is no server to call, as in tests;
    /// the functions then raise an error.
    pub services: Option<Arc<dyn ScriptServices>>,
    pub scans: Scans,
    /// Set while plugins' top-level chunks run, when scans cannot start yet: the runner
    /// they would be dispatched to is not installed until every plugin has loaded.
    pub loading: Arc<AtomicBool>,
}

impl ApiContext {
    fn call(&self, plugin: &str, name: &str, call: ServiceCall) -> LuaResult<ServiceReply> {
        let services = self.services.as_ref().ok_or_else(|| {
            LuaError::RuntimeError(format!("kiki.{name}: not available in this runner"))
        })?;
        services
            .call(plugin, call)
            .map_err(|e| LuaError::RuntimeError(format!("kiki.{name}: {e}")))
    }
}

fn unexpected(name: &str, reply: ServiceReply) -> LuaError {
    LuaError::RuntimeError(format!("kiki.{name}: unexpected reply {reply:?}"))
}

/// Builds the `kiki` table for the plugin named `plugin`.
pub(super) fn plugin_kiki_table(lua: &Lua, plugin: &str, ctx: &ApiContext) -> LuaResult<LuaTable> {
    let kiki = lua.create_table()?;
    let meta = lua.create_table()?;
    meta.set("__index", lua.globals().get::<LuaTable>("kiki")?)?;
    kiki.set_metatable(Some(meta));

    kiki.raw_set("store", store_table(lua, plugin, ctx)?)?;
    kiki.raw_set("entries", entries_table(lua, plugin, ctx)?)?;
    Ok(kiki)
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

fn entries_table(lua: &Lua, plugin: &str, ctx: &ApiContext) -> LuaResult<LuaTable> {
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
            let (options, handler) = scan_args(args)?;
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
            let key = lua.create_registry_value(handler)?;
            c.scans
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id, key);
            Ok(id)
        })?,
    )?;

    Ok(entries)
}

/// Parses the arguments of `kiki.entries.scan([options,] handler)`.
fn scan_args(args: LuaMultiValue) -> LuaResult<(ScanOptions, LuaFunction)> {
    let err = |m: &str| LuaError::RuntimeError(format!("kiki.entries.scan: {m}"));
    let mut args = args.into_iter();
    let (options, handler) = match (args.next(), args.next()) {
        (Some(LuaValue::Function(f)), None) => (None, f),
        (Some(LuaValue::Nil), Some(LuaValue::Function(f))) => (None, f),
        (Some(LuaValue::Table(t)), Some(LuaValue::Function(f))) => (Some(t), f),
        _ => return Err(err("expected ([options,] handler)")),
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
    Ok((parsed, handler))
}
