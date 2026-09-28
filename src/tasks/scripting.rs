#[cfg(feature = "lua")]
use crate::metrics::Metrics;
use crate::scripting::ScriptRunner;
#[cfg(feature = "lua")]
use crate::scripting::{ScriptRunnerHandle, ScriptSource};
#[cfg(feature = "lua")]
use std::path::{Path, PathBuf};
#[cfg(feature = "lua")]
use std::sync::Arc;
#[cfg(feature = "lua")]
use tokio_util::sync::CancellationToken;
#[cfg(feature = "lua")]
use tracing::{debug, error, info, warn};

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

/// Build a fresh [`ScriptRunner`] from the Lua plugins in `plugins_dir` and install it in
/// `handle`, replacing any previous runner.
///
/// When `host` carries a sandboxed script host, the sources are shipped to that child
/// process and `handle` receives a [`SubprocessScriptRunner`] that forwards to it; the
/// VM is rebuilt inside the child, so no respawn is needed. Otherwise the VM is built
/// in this process, which is the path the library tests and `--no-script-isolation`
/// take.
///
/// Plugins that cannot be loaded are logged and skipped (see
/// [`crate::plugins::load_sources`]). Logs and clears the handle if the plugins directory
/// cannot be read or the plugins fail to compile.
///
/// [`SubprocessScriptRunner`]: crate::process::script_host::SubprocessScriptRunner
#[cfg(feature = "lua")]
pub fn reload_script_runner(
    plugins_dir: &Path,
    metrics: &Metrics,
    handle: &ScriptRunnerHandle,
    host: &crate::process::ScriptHostHandle,
) {
    let sources = match load_lua_sources(plugins_dir) {
        Ok(s) => s,
        Err(e) => {
            error!(
                "failed to load plugins from {}: {}",
                plugins_dir.display(),
                e
            );
            handle.set(None);
            return;
        }
    };

    // A runner with no handlers behaves exactly like no runner at all —
    // every dispatch site skips a `None` — so with no plugins installed,
    // install nothing. For the isolated host that also spares every
    // ingested entry two IPC round trips that could only ever be no-ops.
    // The host is still told, so it drops any VM left over from plugins
    // that have since been removed.
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
                // means the reload has taken effect.
                metrics.set_scripts_loaded(loaded as f64);
            }
            Err(e) if host.is_alive() => {
                // The host answered, it just could not compile what we
                // sent. Scripts stay off until the operator fixes them,
                // and a later reload will be picked up normally.
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

/// Listen on `reload_rx` and rebuild the script runner each time a reload signal arrives.
///
/// Exits cleanly on cancellation or when the watch channel is closed.
#[cfg(feature = "lua")]
pub async fn run_script_reloader(
    plugins_dir: PathBuf,
    metrics: Arc<Metrics>,
    handle: ScriptRunnerHandle,
    host: crate::process::ScriptHostHandle,
    mut reload_rx: tokio::sync::watch::Receiver<()>,
    token: CancellationToken,
) {
    loop {
        tokio::select! {
            res = reload_rx.changed() => {
                if res.is_err() {
                    // Sender dropped — no more reloads possible.
                    return;
                }
                debug!("Reloading plugins from {}", plugins_dir.display());
                let plugins_dir = plugins_dir.clone();
                let metrics = metrics.clone();
                let handle = handle.clone();
                let host = host.clone();
                // Compilation can be CPU-heavy — and, with an isolated host, is a
                // blocking round trip to another process. Either way, keep it off
                // the async runtime.
                let _ = tokio::task::spawn_blocking(move || {
                    reload_script_runner(&plugins_dir, &metrics, &handle, &host);
                })
                .await;
                info!("Plugins reloaded");
            }
            _ = token.cancelled() => return,
        }
    }
}

/// Read the source of every enabled Lua plugin in `plugins_dir`.
///
/// # Errors
///
/// Returns an error if `plugins_dir` exists but cannot be listed.
#[cfg(feature = "lua")]
pub(super) fn load_lua_sources(
    plugins_dir: &Path,
) -> Result<Vec<ScriptSource>, crate::plugins::PluginError> {
    crate::plugins::load_sources(plugins_dir, crate::plugins::PluginEngine::Lua)
}
