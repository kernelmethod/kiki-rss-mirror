use crate::metrics::Metrics;
use crate::scripting::ScriptRunner;
use crate::scripting::{ScriptRunnerHandle, ScriptSource};
use std::sync::Arc;
use tracing::warn;

/// Dispatch `fetch.error` to the scripting engine, if one is installed.
pub(super) fn fire_fetch_error(
    runner: Option<&dyn ScriptRunner>,
    feed_id: i64,
    kind: &'static str,
    status: Option<u16>,
    message: String,
    retry_after: Option<i64>,
) {
    if let Some(r) = runner {
        r.dispatch_observe(
            crate::scripting::Event::FetchError,
            crate::scripting::EventPayload::FetchError {
                feed_id,
                kind: kind.into(),
                status,
                message,
                retry_after,
            },
        );
    }
}

/// Dispatch `fetch.success` to the scripting engine, if one is installed.
pub(super) fn fire_fetch_success(
    runner: Option<&dyn ScriptRunner>,
    feed_id: i64,
    status: u16,
    url: String,
    content_length: Option<u64>,
) {
    if let Some(r) = runner {
        r.dispatch_observe(
            crate::scripting::Event::FetchSuccess,
            crate::scripting::EventPayload::FetchSuccess {
                feed_id,
                status,
                url,
                content_length,
            },
        );
    }
}

/// Error returned when plugins could not be loaded into a script runner.
#[derive(Debug, thiserror::Error)]
pub enum LoadPluginsError {
    /// A plugin's code failed to compile or its top-level chunk failed to run. The
    /// plugins that were running before, if any, keep running.
    #[error("failed to load plugins: {0}")]
    Compile(String),

    /// The sandboxed script host is gone, so no plugins can run until the server
    /// restarts.
    #[error("the script host is gone: {0}")]
    HostDead(String),
}

/// Build a [`ScriptRunner`] from the Lua plugins in `discovery` and install it in
/// `handle`, replacing the runner it held.
///
/// Called when the server starts, and again whenever plugins are reloaded (see
/// [`crate::plugins::runtime`]). Once the new runner is installed, the plugins'
/// `plugin.load` handlers run. Returns the number of plugins loaded.
///
/// When `host` carries a sandboxed script host, the sources are shipped to that child
/// process and `handle` receives a [`SubprocessScriptRunner`] that forwards to it.
/// Otherwise the VM is built in this process, which is the path the library tests and
/// `--no-script-isolation` take, and the calls plugins make through the `kiki` API are
/// answered by `services`. (The host answers them with the services set on it.)
///
/// Plugins that cannot be loaded are logged and skipped (see
/// [`crate::plugins::load_sources`]).
///
/// # Errors
///
/// Returns [`LoadPluginsError::Compile`] if the plugins fail to compile. `handle` is then
/// left as it was: the plugins that were running keep running, as when an edit to the
/// config file is invalid. Returns [`LoadPluginsError::HostDead`], and clears `handle`,
/// if the script host has gone away.
///
/// [`SubprocessScriptRunner`]: crate::process::script_host::SubprocessScriptRunner
pub fn load_script_runner(
    discovery: &crate::plugins::Discovery,
    metrics: &Metrics,
    handle: &ScriptRunnerHandle,
    host: &crate::process::ScriptHostHandle,
    services: Arc<dyn crate::scripting::ScriptServices>,
) -> Result<usize, LoadPluginsError> {
    let sources = load_lua_sources(discovery);
    let count = sources.len();

    // A runner with no handlers behaves exactly like no runner at all —
    // every dispatch site skips a `None` — so with no plugins installed,
    // install nothing. For the isolated host that also spares every
    // ingested entry two IPC round trips that could only ever be no-ops.
    let empty = sources.is_empty();

    #[cfg(unix)]
    if let Some(host) = host {
        use crate::process::script_host::SubprocessScriptRunner;

        return match host.reload(sources) {
            Ok(loaded) => {
                handle.set(if empty {
                    None
                } else {
                    Some(Arc::new(SubprocessScriptRunner::new(host.clone()))
                        as Arc<dyn ScriptRunner>)
                });
                // Only after the swap, so that the gauge reaching a value
                // means the plugins have taken effect.
                metrics.set_plugins_loaded(loaded as f64);
                fire_plugin_load(handle);
                Ok(loaded)
            }
            Err(e) if host.is_alive() => {
                // The host answered, it just could not compile what we
                // sent, and kept running what it had.
                warn!("script host failed to compile Lua scripts: {}", e);
                metrics.record_plugin_load_error();
                Err(LoadPluginsError::Compile(e.to_string()))
            }
            Err(e) => {
                // The channel itself is gone. Nothing can bring it back:
                // the server denied itself `execve` when it sandboxed.
                tracing::error!(
                    "script host is gone ({}); scripting is disabled until the server restarts",
                    e
                );
                metrics.set_plugins_loaded(0.0);
                handle.set(None);
                Err(LoadPluginsError::HostDead(e.to_string()))
            }
        };
    }

    // No isolated host: compile into a VM in this process.
    let _ = host;
    match crate::scripting::lua::LuaScriptRunner::from_sources_with(&sources, Some(services)) {
        Ok(runner) => {
            handle.set(if empty {
                None
            } else {
                Some(Arc::new(runner) as Arc<dyn ScriptRunner>)
            });
            // As above, only after the swap.
            metrics.set_plugins_loaded(count as f64);
            fire_plugin_load(handle);
            Ok(count)
        }
        Err(e) => {
            warn!("failed to compile Lua scripts: {}", e);
            metrics.record_plugin_load_error();
            Err(LoadPluginsError::Compile(e.to_string()))
        }
    }
}

/// Dispatch `plugin.load` to the runner in `handle`, if there is one.
fn fire_plugin_load(handle: &ScriptRunnerHandle) {
    if let Some(runner) = handle.current() {
        runner.dispatch_observe(
            crate::scripting::Event::PluginLoad,
            crate::scripting::EventPayload::PluginLoad,
        );
    }
}

/// Read the source of every enabled Lua plugin in `discovery`.
pub(super) fn load_lua_sources(discovery: &crate::plugins::Discovery) -> Vec<ScriptSource> {
    crate::plugins::load_sources(discovery, crate::plugins::PluginEngine::Lua)
}
