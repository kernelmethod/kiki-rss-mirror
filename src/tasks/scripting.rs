#[cfg(feature = "lua")]
use crate::metrics::Metrics;
use crate::scripting::ScriptRunner;
#[cfg(feature = "lua")]
use crate::scripting::{ScriptRunnerHandle, ScriptSource};
#[cfg(feature = "lua")]
use std::sync::Arc;
#[cfg(feature = "lua")]
use tracing::{error, warn};

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

/// Build a [`ScriptRunner`] from the Lua plugins in `discovery` and install it in
/// `handle`.
///
/// Plugins are loaded once, when the server starts; picking up a new or changed plugin
/// takes a restart.
///
/// When `host` carries a sandboxed script host, the sources are shipped to that child
/// process and `handle` receives a [`SubprocessScriptRunner`] that forwards to it.
/// Otherwise the VM is built in this process, which is the path the library tests and
/// `--no-script-isolation` take.
///
/// Plugins that cannot be loaded are logged and skipped (see
/// [`crate::plugins::load_sources`]). Logs and clears the handle if the plugins fail to
/// compile.
///
/// [`SubprocessScriptRunner`]: crate::process::script_host::SubprocessScriptRunner
#[cfg(feature = "lua")]
pub fn load_script_runner(
    discovery: &crate::plugins::Discovery,
    metrics: &Metrics,
    handle: &ScriptRunnerHandle,
    host: &crate::process::ScriptHostHandle,
) {
    let sources = load_lua_sources(discovery);

    // A runner with no handlers behaves exactly like no runner at all —
    // every dispatch site skips a `None` — so with no plugins installed,
    // install nothing. For the isolated host that also spares every
    // ingested entry two IPC round trips that could only ever be no-ops.
    let empty = sources.is_empty();

    #[cfg(all(unix, feature = "lua"))]
    if let Some(host) = host {
        use crate::process::script_host::SubprocessScriptRunner;

        match host.reload(sources) {
            Ok(loaded) => {
                handle.set(if empty {
                    None
                } else {
                    Some(Arc::new(SubprocessScriptRunner::new(host.clone()))
                        as Arc<dyn ScriptRunner>)
                });
                // Only after the swap, so that the gauge reaching a value
                // means the plugins have taken effect.
                metrics.set_scripts_loaded(loaded as f64);
            }
            Err(e) if host.is_alive() => {
                // The host answered, it just could not compile what we
                // sent. Plugins stay off until the operator fixes them and
                // restarts the server.
                warn!("script host failed to compile Lua scripts: {}", e);
                metrics.record_script_compile_error();
                metrics.set_scripts_loaded(0.0);
                handle.set(None);
            }
            Err(e) => {
                // The channel itself is gone. Nothing can bring it back:
                // the server denied itself `execve` when it sandboxed.
                error!(
                    "script host is gone ({}); scripting is disabled until the server restarts",
                    e
                );
                metrics.set_scripts_loaded(0.0);
                handle.set(None);
            }
        }
        return;
    }

    // No isolated host: compile into a VM in this process.
    let _ = host;
    let count = sources.len() as f64;
    match crate::scripting::lua::LuaScriptRunner::from_sources(&sources) {
        Ok(runner) => {
            handle.set(if empty {
                None
            } else {
                Some(Arc::new(runner) as Arc<dyn ScriptRunner>)
            });
            // As above, only after the swap.
            metrics.set_scripts_loaded(count);
        }
        Err(e) => {
            warn!("failed to compile Lua scripts: {}", e);
            metrics.record_script_compile_error();
            metrics.set_scripts_loaded(0.0);
            handle.set(None);
        }
    }
}

/// Read the source of every enabled Lua plugin in `discovery`.
#[cfg(feature = "lua")]
pub(super) fn load_lua_sources(discovery: &crate::plugins::Discovery) -> Vec<ScriptSource> {
    crate::plugins::load_sources(discovery, crate::plugins::PluginEngine::Lua)
}
