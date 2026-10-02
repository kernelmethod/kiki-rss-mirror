//! The plugins a running server has loaded, and reloading them while it runs.
//!
//! [`PluginRuntime`] holds the [`Discovery`] the server is running with and
//! rebuilds the script runner from the plugins directory and the database
//! whenever [`PluginRuntime::reload`] is called. The server reloads its
//! plugins:
//!
//! - when a file in the plugins directory changes (see [`spawn_watcher`]),
//!   and
//! - when a plugin's config overrides are changed through the API, which
//!   `kiki plugin config set` also uses when the server is running.
//!
//! A reload whose plugins fail to compile changes nothing: the plugins that
//! were running keep running, just as an invalid edit to the config file
//! leaves the last good settings in force.
//!
//! Once plugins have loaded, their `plugin.load` handlers run. That is where
//! a plugin that needs to apply itself to the entries already stored starts a
//! scan of them (see [`crate::plugins::services`]).

use super::services::ServerServices;
use super::{discover, Discovery, PluginError};
use crate::db::plugins::PluginConfigError;
use crate::metrics::Metrics;
use crate::scripting::{ScriptRunnerHandle, ScriptServices, ScriptSource};
use arc_swap::ArcSwap;
use notify::{RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// How long the plugins directory must go without further changes before
/// plugins are reloaded, so that copying a plugin in, or an editor's save,
/// triggers one reload rather than one per file.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// Errors that keep plugins from being reloaded. In every case the plugins
/// that were running keep running.
#[derive(Debug, Error)]
pub enum ReloadError {
    /// The plugins directory could not be listed.
    #[error("failed to scan for plugins: {0}")]
    Scan(#[from] PluginError),

    /// The plugins' config overrides could not be read from the database.
    #[error("failed to load plugin configs: {0}")]
    Config(#[from] PluginConfigError),

    /// No database connection could be had.
    #[error("failed to get a database connection: {0}")]
    Db(#[from] crate::db::DbError),

    /// The plugins failed to compile, or their top-level chunks failed.
    #[error("{0}")]
    Load(String),
}

/// What a successful reload did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReloadOutcome {
    /// How many plugins are loaded now.
    pub loaded: usize,
    /// Whether the plugins' code or config changed, so that the script
    /// runner was rebuilt. A reload that finds nothing changed leaves the
    /// running plugins as they are.
    pub changed: bool,
}

/// The plugins a running server has loaded. See the [module
/// documentation](self).
pub struct PluginRuntime {
    dir: PathBuf,
    discovery: ArcSwap<Discovery>,
    /// The sources the script runner was last built from, to tell whether a
    /// reload changes anything. `None` until plugins first load.
    sources: Mutex<Option<Vec<ScriptSource>>>,
    db: crate::db::Db,
    metrics: Arc<Metrics>,
    script_runner: ScriptRunnerHandle,
    script_host: crate::process::ScriptHostHandle,
    services: Arc<ServerServices>,
}

impl PluginRuntime {
    /// Discovers the plugins in `dir`, applies their config overrides from
    /// the database, and loads them into `script_runner`.
    ///
    /// Plugins that cannot be discovered, and plugins that fail to compile,
    /// are logged, and the server runs without them until they are fixed
    /// and reloaded. The calls plugins make are answered until `cancel`
    /// fires.
    ///
    /// # Errors
    ///
    /// Returns an error if the config overrides cannot be read: running a
    /// plugin with its defaults instead of the config it was given could
    /// quietly change what it does.
    pub fn start(
        dir: PathBuf,
        db: crate::db::Db,
        metrics: Arc<Metrics>,
        script_runner: ScriptRunnerHandle,
        script_host: crate::process::ScriptHostHandle,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Self, ReloadError> {
        let discovery = ArcSwap::from_pointee(Discovery::default());
        let services = Arc::new(ServerServices::new(
            db.clone(),
            script_runner.clone(),
            cancel,
        ));
        #[cfg(unix)]
        if let Some(host) = &script_host {
            host.set_services(services.clone());
        }
        let runtime = Self {
            dir,
            discovery,
            sources: Mutex::new(None),
            db,
            metrics,
            script_runner,
            script_host,
            services,
        };
        match runtime.reload() {
            Ok(_) | Err(ReloadError::Scan(_) | ReloadError::Load(_)) => Ok(runtime),
            Err(e) => Err(e),
        }
    }

    /// The directory plugins are discovered in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The plugins the server is running with.
    pub fn current(&self) -> Arc<Discovery> {
        self.discovery.load_full()
    }

    /// The database the plugins' config overrides are read from.
    pub fn db(&self) -> &crate::db::Db {
        &self.db
    }

    /// Discovers the plugins again, applies their config overrides, and
    /// loads them, replacing the plugins that were running.
    ///
    /// Blocks while plugins are read and compiled, so async callers should
    /// run it on the blocking thread pool. Concurrent reloads run one at a
    /// time.
    ///
    /// # Errors
    ///
    /// Returns an error if the plugins cannot be discovered, their config
    /// overrides cannot be read, or they fail to load. The plugins that
    /// were running then keep running, with the config they had.
    pub fn reload(&self) -> Result<ReloadOutcome, ReloadError> {
        let mut last_sources = self.sources.lock().unwrap_or_else(|e| e.into_inner());

        let mut discovery = discover(&self.dir).inspect_err(|e| error!("{e}"))?;
        for e in &discovery.errors {
            warn!(dir = %e.dir.display(), "skipping plugin: {}", e.error);
        }
        let overrides = self
            .db
            .read_blocking(|conn| crate::db::plugins::all_config_overrides(conn))??;
        discovery.apply_config_overrides(overrides);

        let sources = super::load_sources(&discovery, super::PluginEngine::Lua);
        let loaded = sources.len();
        let changed = last_sources.as_ref() != Some(&sources);
        let discovery = Arc::new(discovery);
        if changed {
            // Set before the plugins load, since the calls they make while
            // loading are only answered for plugins that are loaded.
            let names = sources.iter().map(|s| s.name.clone()).collect();
            let previous = self.services.set_loaded(names);
            if let Err(e) = self.load(&discovery) {
                // The plugins that were running keep running, and keep
                // being reported, so that what the API reports as running
                // is what is running. With nothing loaded before, as when
                // the server starts, list the ones that were found,
                // although they are not running.
                self.services.set_loaded((*previous).clone());
                if last_sources.is_none() {
                    self.discovery.store(discovery);
                }
                return Err(e);
            }
            *last_sources = Some(sources);
        }
        self.discovery.store(discovery);
        Ok(ReloadOutcome { loaded, changed })
    }

    fn load(&self, discovery: &Discovery) -> Result<(), ReloadError> {
        crate::tasks::load_script_runner(
            discovery,
            &self.metrics,
            &self.script_runner,
            &self.script_host,
            self.services.clone() as Arc<dyn ScriptServices>,
        )
        .map(|_| ())
        .map_err(|e| ReloadError::Load(e.to_string()))
    }

    /// [`Self::reload`] on the blocking thread pool, logging the outcome.
    ///
    /// # Errors
    ///
    /// As for [`Self::reload`].
    pub async fn reload_async(self: &Arc<Self>) -> Result<ReloadOutcome, ReloadError> {
        let runtime = self.clone();
        let result = tokio::task::spawn_blocking(move || runtime.reload())
            .await
            .unwrap_or_else(|e| Err(ReloadError::Load(format!("reload task failed: {e}"))));
        match &result {
            Ok(ReloadOutcome {
                loaded,
                changed: true,
            }) => info!("reloaded plugins; {loaded} loaded"),
            Ok(_) => debug!("plugins are unchanged; nothing to reload"),
            Err(e) => error!("failed to reload plugins; the previous plugins keep running: {e}"),
        }
        result
    }
}

/// Starts watching the plugins directory of `runtime`, reloading its
/// plugins whenever a file in it changes, until `cancel` fires.
///
/// Hidden files, such as editors' swap files, are ignored. Must be called
/// from within a Tokio runtime.
///
/// # Errors
///
/// Returns an error if the platform's file watcher cannot be created or the
/// plugins directory cannot be watched.
pub fn spawn_watcher(runtime: Arc<PluginRuntime>, cancel: CancellationToken) -> anyhow::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<()>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let event = match res {
            Ok(event) => event,
            Err(e) => {
                warn!("plugins watcher error: {:?}", e);
                return;
            }
        };
        // As in the config watcher, a rescan carries no paths, so treat it
        // as a possible change.
        let relevant = event.paths.iter().any(|p| !is_ignored(p));
        if event.need_rescan() || (crate::config::watch::is_change(&event.kind) && relevant) {
            let _ = tx.send(());
        }
    })?;
    watcher.watch(runtime.dir(), RecursiveMode::Recursive)?;

    tokio::spawn(async move {
        // Dropping the watcher stops the watch, so it lives in this task.
        let _watcher = watcher;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                event = rx.recv() => {
                    if event.is_none() {
                        break;
                    }
                    while let Ok(Some(())) = tokio::time::timeout(DEBOUNCE, rx.recv()).await {}
                    debug!("plugins directory changed; reloading plugins");
                    let _ = runtime.reload_async().await;
                }
            }
        }
    });
    Ok(())
}

/// Whether a change to `path` is not a change to a plugin: the file is
/// hidden, or an editor's backup file.
fn is_ignored(path: &Path) -> bool {
    path.file_name()
        .map(|n| n.to_string_lossy())
        .is_some_and(|n| n.starts_with('.') || n.ends_with('~'))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn hidden_and_backup_files_are_ignored() {
        assert!(is_ignored(Path::new("/p/filter/.main.lua.swp")));
        assert!(is_ignored(Path::new("/p/filter/main.lua~")));
        assert!(!is_ignored(Path::new("/p/filter/main.lua")));
        assert!(!is_ignored(Path::new("/p/filter")));
    }
}
