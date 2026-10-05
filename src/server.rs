use crate::{
    config::{self, ConfigHandle, ConfigStore},
    db::migrations,
    plugins, routes,
    scripting::ScriptRunnerHandle,
    tasks::{self, TaskManagerCommand},
};
use anyhow::{bail, Context, Error, Result};
use std::{
    fs, io,
    os::unix::{
        fs::{FileTypeExt, PermissionsExt},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{net::UnixListener, signal};
use tokio_util::sync::CancellationToken;
use tracing::{debug, span, Level};

pub struct SharedAppState {
    /// An [`async_channel::Sender`] instance that may be used to send commands
    /// to worker tasks used to fetch and process feeds.
    pub task_manager_tx: crate::tasks::TaskSender,

    /// The SQLite database; see [`crate::db::Db`].
    pub db: crate::db::Db,

    /// A [`CancellationToken`] that can be used to trigger a graceful
    /// server shutdown.
    pub cancel_token: CancellationToken,

    /// Per-server Prometheus metrics recorder.
    pub metrics: Arc<crate::metrics::Metrics>,

    /// Root directory for on-disk state. Used to locate the cached asset
    /// filesystem under `{data_dir}/assets/`.
    pub data_dir: PathBuf,

    /// The plugins the server is running, and reloading them. See
    /// [`crate::plugins::runtime`].
    pub plugins: Arc<plugins::runtime::PluginRuntime>,

    /// Shared handle to the currently-installed scripting engine. Empty when no scripts
    /// have been loaded.
    pub script_runner: ScriptRunnerHandle,

    /// The server's settings, backed by the config file. The settings
    /// routes write through it; everything else reads snapshots from it.
    pub config: ConfigHandle,

    /// Whether the server's other parts are still running, for the health
    /// check.
    pub liveness: Liveness,
}

pub type AppState = Arc<SharedAppState>;

pub struct ServerBuilder<'a> {
    db_path: &'a Path,
    socket_path: Option<PathBuf>,
    listener: Option<std::os::unix::net::UnixListener>,
    tcp_listener: Option<std::net::TcpListener>,
    config_path: Option<PathBuf>,
    plugins_dir: Option<PathBuf>,
    autofetch: bool,
    single_threaded: bool,
    worker_count: Option<usize>,
    script_host: crate::process::ScriptHostHandle,
    feed_fetcher: crate::process::FeedFetcherHandle,
    notifier: Option<Arc<crate::notify::Notifier>>,
}

impl<'a> ServerBuilder<'a> {
    pub fn new(db_path: &'a Path) -> Self {
        ServerBuilder {
            db_path,
            socket_path: None,
            listener: None,
            tcp_listener: None,
            config_path: None,
            plugins_dir: None,
            autofetch: false,
            single_threaded: false,
            worker_count: None,
            script_host: None,
            feed_fetcher: None,
            notifier: None,
        }
    }

    pub fn socket_path(mut self, p: &'a Path) -> Self {
        self.socket_path = Some(p.to_path_buf());
        self
    }

    /// Serve the API on `listener`, already bound to the socket path with
    /// [`bind_socket`], instead of binding it when the server starts.
    ///
    /// `kiki serve` binds its socket before it installs its seccomp
    /// filter, so the filter need not allow creating or binding sockets.
    /// The socket path should still be set with [`Self::socket_path`]:
    /// the server removes the socket file there when it stops.
    pub fn listener(mut self, listener: std::os::unix::net::UnixListener) -> Self {
        self.listener = Some(listener);
        self
    }

    /// Also serve the API on `listener`, a TCP listener already bound by the
    /// caller, where every request but those to public routes must carry
    /// an API token; see [`crate::auth`].
    ///
    /// As with [`Self::listener`], `kiki serve` binds it before installing
    /// its seccomp filter. It must be in non-blocking mode.
    pub fn tcp_listener(mut self, listener: std::net::TcpListener) -> Self {
        self.tcp_listener = Some(listener);
        self
    }

    /// Read and write settings at `p` instead of the default,
    /// [`config::CONFIG_FILE_NAME`] in the data directory.
    pub fn config_path(mut self, p: &Path) -> Self {
        self.config_path = Some(p.to_path_buf());
        self
    }

    /// Discover plugins in `p` instead of the default,
    /// [`plugins::PLUGINS_DIR_NAME`] in the data directory.
    pub fn plugins_dir(mut self, p: &Path) -> Self {
        self.plugins_dir = Some(p.to_path_buf());
        self
    }

    pub fn autofetch(mut self) -> Self {
        self.autofetch = true;
        self
    }

    /// Use single-threaded tokio runtimes instead of multi-threaded ones.
    pub fn single_threaded(mut self) -> Self {
        self.single_threaded = true;
        self
    }

    /// Set the number of feed-fetcher worker tasks.
    pub fn worker_count(mut self, n: usize) -> Self {
        self.worker_count = Some(n);
        self
    }

    /// Dispatch script events to an already-spawned, sandboxed script
    /// host instead of a Lua VM in this process.
    ///
    /// The host must be spawned by the caller, before it installs its own
    /// sandbox — see [`crate::process`]. Passing `None` keeps the
    /// in-process VM, which is what the library-level tests use.
    pub fn script_host(mut self, host: crate::process::ScriptHostHandle) -> Self {
        self.script_host = host;
        self
    }

    /// Retrieve and parse feeds in an already-spawned, sandboxed feed
    /// fetcher process instead of in this one.
    ///
    /// As with [`Self::script_host`], the fetcher must be spawned by the
    /// caller before it installs its seccomp filter. Passing `None` fetches
    /// in-process, which is what the library-level tests use.
    pub fn feed_fetcher(mut self, fetcher: crate::process::FeedFetcherHandle) -> Self {
        self.feed_fetcher = fetcher;
        self
    }

    /// Tell a service manager such as systemd when the server is ready,
    /// that it is still alive, and when it stops; see [`crate::notify`].
    pub fn notifier(mut self, notifier: Option<crate::notify::Notifier>) -> Self {
        self.notifier = notifier.map(Arc::new);
        self
    }

    pub fn build(self) -> Server {
        let socket_path = self
            .socket_path
            .unwrap_or_else(|| PathBuf::from("kiki.sock"));

        // Default the data directory to the directory containing the
        // database file. Callers that want an explicit location can extend
        // the builder later.
        let data_dir = self
            .db_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        let config_path = self
            .config_path
            .unwrap_or_else(|| data_dir.join(config::CONFIG_FILE_NAME));

        let plugins_dir = self
            .plugins_dir
            .unwrap_or_else(|| plugins::plugins_dir(&data_dir));

        Server {
            db_path: PathBuf::from(self.db_path),
            config_path,
            plugins_dir,
            data_dir,
            socket_path,
            listener: self.listener,
            tcp_listener: self.tcp_listener,
            autofetch: self.autofetch,
            single_threaded: self.single_threaded,
            worker_count: self.worker_count,
            script_host: self.script_host,
            feed_fetcher: self.feed_fetcher,
            notifier: self.notifier,
            cancel_token: CancellationToken::new(),
        }
    }
}

#[derive(Debug, Default)]
pub struct ServerError {
    fetch_error: Option<Error>,
    web_error: Option<Error>,
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "ServerError:")?;
        if let Some(e) = &self.fetch_error {
            writeln!(f, "\tfetch_error={:#?}", e)?;
        }
        if let Some(e) = &self.web_error {
            writeln!(f, "\tweb_error={:#?}", e)?;
        }
        Ok(())
    }
}

impl std::error::Error for ServerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

pub struct Server {
    /// Path to the database used by the server.
    db_path: PathBuf,

    /// Root directory for on-disk state (cached assets live beneath this).
    data_dir: PathBuf,

    /// Path of the config file holding settings overrides.
    config_path: PathBuf,

    /// Directory plugins are discovered in.
    plugins_dir: PathBuf,

    /// Path of the Unix domain socket the server listens on.
    socket_path: PathBuf,

    /// The socket, if the caller bound it already; see
    /// [`ServerBuilder::listener`].
    listener: Option<std::os::unix::net::UnixListener>,

    /// A TCP listener to serve the API on as well, if the caller bound one;
    /// see [`ServerBuilder::tcp_listener`].
    tcp_listener: Option<std::net::TcpListener>,

    /// Whether or not to automatically fetch feed contents.
    autofetch: bool,

    /// Use single-threaded tokio runtimes instead of multi-threaded ones.
    single_threaded: bool,

    /// Override the number of feed-fetcher worker tasks.
    worker_count: Option<usize>,

    /// Sandboxed script host to dispatch script events to, if one was
    /// spawned. `None` runs Lua in this process.
    script_host: crate::process::ScriptHostHandle,

    /// Sandboxed feed fetcher to retrieve and parse feeds in, if one was
    /// spawned. `None` fetches in this process.
    feed_fetcher: crate::process::FeedFetcherHandle,

    /// Where to tell a service manager how the server is doing, if one
    /// asked to be told.
    notifier: Option<Arc<crate::notify::Notifier>>,

    /// A [`CancellationToken`] used to indicate that the server should
    /// be killed.
    cancel_token: CancellationToken,
}

impl Server {
    /// Run the server synchronously, creating a Tokio runtime and installing
    /// signal handlers. This is the entry point used by the CLI.
    pub fn run(self) -> Result<()> {
        let rt = if self.single_threaded {
            tokio::runtime::Builder::new_current_thread()
        } else {
            tokio::runtime::Builder::new_multi_thread()
        }
        .enable_all()
        .build()?;
        let cancel = self.cancel_token.clone();
        rt.block_on(async {
            tokio::spawn(shutdown_signal(cancel));
            self.run_async().await
        })
    }

    /// Run the server within an existing Tokio runtime.
    ///
    /// This method does **not** install signal handlers — the caller is
    /// responsible for triggering graceful shutdown via
    /// [`Server::cancel_token()`].
    pub async fn run_async(self) -> Result<()> {
        // Build a per-server metrics recorder. Every instrumented code path
        // in this server instance writes samples into the returned `Metrics`
        // value, which is rendered by the `/metrics` handler.
        let metrics = Arc::new(crate::metrics::Metrics::new()?);
        tokio::spawn(crate::metrics::run_upkeep(
            metrics.clone(),
            self.cancel_token.clone(),
        ));

        // One writer connection and a pool of readers, shared by all of the
        // threads that we spawn.
        let db = crate::db::Db::open(
            &self.db_path,
            crate::db::DbOptions {
                metrics: Some(metrics.clone()),
                ..Default::default()
            },
        )
        .with_context(|| format!("Unable to open the database at {:?}", self.db_path))?;

        // Check for pending migrations before starting the server
        {
            let pending = db
                .read(|conn| migrations::pending_migrations(conn))
                .await
                .with_context(|| "failed to get connection for migration check")??;
            if !pending.is_empty() {
                let names: Vec<&str> = pending.iter().map(|m| m.name).collect();
                bail!(
                    "Database has {} pending migration(s): {}. Run `kiki migrate` first.",
                    pending.len(),
                    names.join(", ")
                );
            }
        }

        // Load settings. An invalid config file is fatal here, at startup,
        // where the operator is watching; later edits that fail to parse
        // are logged and ignored by the watcher instead.
        let config: ConfigHandle = Arc::new(
            ConfigStore::open(&self.config_path)
                .with_context(|| format!("failed to load config file {:?}", self.config_path))?,
        );
        // The config file's proxy was validated as it loaded; this catches
        // a bad `$KIKI_PROXY`, which otherwise fails only at fetch time.
        let proxy = config.current().effective_proxy();
        proxy.validate().with_context(|| {
            format!(
                "invalid proxy settings from ${} or ${}",
                config::PROXY_ENV,
                config::NO_PROXY_ENV
            )
        })?;
        if let Some(host) = proxy
            .url
            .as_deref()
            .and_then(|u| url::Url::parse(u.trim()).ok())
            .and_then(|u| u.host_str().map(str::to_owned))
        {
            tracing::info!(%host, "sending outbound requests through a proxy");
        }
        if let Err(e) = config::watch::spawn_watcher(config.clone(), self.cancel_token.clone()) {
            tracing::warn!(
                "not watching config file {:?} for changes; edits will need a restart: {:#}",
                self.config_path,
                e
            );
        }

        // Create a multi-producer, multi-consumer queue so that web
        // service workers can send tasks to the feed-fetcher workers.
        // Asset caching gets a lane of its own; see `crate::tasks::queue`.
        let (tx, rx) = crate::tasks::queue(1024);

        // Shared scripting engine handle; workers and HTTP handlers dispatch events
        // through it.
        let script_runner = ScriptRunnerHandle::empty();

        // Plugins live in a directory Kiki creates, so that there is always
        // somewhere to install one.
        if let Err(e) = fs::create_dir_all(&self.plugins_dir) {
            tracing::warn!(
                "failed to create plugins directory {:?}: {}",
                self.plugins_dir,
                e
            );
        }

        // Plugins are reloaded while the server runs, whenever the plugins
        // directory changes or a plugin's config is changed through the API.
        let plugins = Arc::new(
            plugins::runtime::PluginRuntime::start(
                self.plugins_dir.clone(),
                db.clone(),
                metrics.clone(),
                script_runner.clone(),
                self.script_host.clone(),
                self.cancel_token.clone(),
            )
            .with_context(|| "failed to load plugins")?,
        );
        if let Err(e) = plugins::runtime::spawn_watcher(plugins.clone(), self.cancel_token.clone())
        {
            tracing::warn!(
                "not watching plugins directory {:?} for changes; edits to plugins \
                 take effect when a plugin's config changes or the server restarts: {:#}",
                self.plugins_dir,
                e
            );
        }

        let num_workers = self.worker_count.unwrap_or_else(tasks::worker_count);
        debug!("Spawning {} task-manager workers", num_workers);
        metrics.set_workers_total(num_workers as f64);

        // Ensure the asset cache directory exists before workers start
        // writing into it.
        let assets_dir = self.data_dir.join("assets");
        if let Err(e) = fs::create_dir_all(&assets_dir) {
            tracing::warn!(
                "failed to create asset cache directory {:?}: {}",
                assets_dir,
                e
            );
        }

        let fetcher = match &self.feed_fetcher {
            #[cfg(unix)]
            Some(host) => crate::fetcher::Fetcher::Isolated(host.clone()),
            #[cfg(not(unix))]
            Some(()) => crate::fetcher::Fetcher::in_process()?,
            None => crate::fetcher::Fetcher::in_process()?,
        };

        let fetcher_lost = fetcher_lost(self.feed_fetcher.clone());

        let worker_handles = tasks::spawn_workers(
            rx,
            tx.clone(),
            db.clone(),
            self.cancel_token.clone(),
            num_workers,
            metrics.clone(),
            self.data_dir.clone(),
            config.clone(),
            script_runner.clone(),
            fetcher,
        );
        let (workers_live, workers_exited) = watch_workers(worker_handles);
        let liveness = Liveness {
            feed_fetcher: self.feed_fetcher.clone(),
            script_host: self.script_host.clone(),
            workers: workers_live,
        };

        if let Some(interval) = self.notifier.as_ref().and_then(|n| n.watchdog_interval()) {
            tokio::spawn(watchdog_loop(
                self.notifier.clone(),
                interval,
                liveness.clone(),
                db.clone(),
                self.cancel_token.clone(),
            ));
        }

        tokio::spawn(metrics_sampler_loop(
            tx.clone(),
            db.clone(),
            self.cancel_token.clone(),
            metrics.clone(),
        ));

        if self.autofetch {
            tokio::spawn(check_feeds_loop(
                tx.clone(),
                db.clone(),
                self.cancel_token.clone(),
                metrics.clone(),
            ));
            tokio::spawn(cleanup_loop(
                tx.clone(),
                self.cancel_token.clone(),
                metrics.clone(),
            ));
            tokio::spawn(retry_entry_assets_loop(
                tx.clone(),
                db.clone(),
                self.cancel_token.clone(),
                metrics.clone(),
            ));
            tokio::spawn(periodic_command_loop(
                tx.clone(),
                db.clone(),
                self.cancel_token.clone(),
                Duration::from_secs(86400),
                TaskManagerCommand::WalCheckpointAnalyze,
                crate::db::task_queue::TASK_WAL_CHECKPOINT_ANALYZE,
                metrics.clone(),
            ));
            tokio::spawn(periodic_command_loop(
                tx.clone(),
                db.clone(),
                self.cancel_token.clone(),
                Duration::from_secs(86400),
                TaskManagerCommand::IncrementalVacuum,
                crate::db::task_queue::TASK_INCREMENTAL_VACUUM,
                metrics.clone(),
            ));
            tokio::spawn(periodic_command_loop(
                tx.clone(),
                db.clone(),
                self.cancel_token.clone(),
                Duration::from_secs(604800),
                TaskManagerCommand::OptimizeFts,
                crate::db::task_queue::TASK_FTS_OPTIMIZE,
                metrics.clone(),
            ));
            tokio::spawn(periodic_command_loop(
                tx.clone(),
                db.clone(),
                self.cancel_token.clone(),
                Duration::from_secs(86400),
                TaskManagerCommand::IntegrityCheck,
                crate::db::task_queue::TASK_INTEGRITY_CHECK,
                metrics.clone(),
            ));
        }

        let listener = match self.listener {
            Some(listener) => listener,
            None => bind_socket(&self.socket_path)?,
        };
        tokio::spawn(uds_server(
            listener,
            self.tcp_listener,
            self.socket_path,
            tx.clone(),
            db.clone(),
            self.cancel_token.clone(),
            metrics.clone(),
            self.data_dir.clone(),
            plugins,
            script_runner.clone(),
            config,
            liveness,
            self.notifier.clone(),
        ));

        // Without workers nothing queued is ever run — feeds included — so
        // if they all exit while the server is still meant to be up, stop
        // with an error rather than carry on without them.
        tokio::select! {
            biased;
            _ = self.cancel_token.cancelled() => {
                if let Some(n) = &self.notifier {
                    n.stopping();
                }
                Ok(())
            }
            _ = workers_exited => {
                self.cancel_token.cancel();
                bail!("all task workers exited; stopping the server")
            }
            // Likewise without the fetcher, which cannot be restarted from
            // inside the sandbox: exit with an error, so that a supervisor
            // such as systemd restarts the server and the fetcher with it.
            _ = fetcher_lost => {
                self.cancel_token.cancel();
                bail!("the feed fetcher process exited; stopping the server so it can be restarted")
            }
        }
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }
}

/// Warn, once per process, that the proportional memory of `role`'s
/// processes could not be read, so its series is missing from the metrics.
#[cfg(target_os = "linux")]
fn warn_pss_unreadable(role: crate::process::stats::Role) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        tracing::warn!(
            process = role.as_str(),
            "cannot read the proportional memory (PSS) of kiki's {} process from \
             /proc/<pid>/smaps_rollup; kiki_process_proportional_memory_bytes will be \
             missing for it",
            role.as_str()
        );
    });
}

/// Periodically sample observable process state into the metrics recorder.
///
/// Covers DB pool utilization, task queue depth, domain totals that are
/// cheap to read (feed/entry counts, feeds with fetch errors, database size),
/// and the CPU and memory used by kiki's processes.
async fn metrics_sampler_loop(
    task_manager_tx: crate::tasks::TaskSender,
    db: crate::db::Db,
    cancel_token: CancellationToken,
    metrics: Arc<crate::metrics::Metrics>,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    let mut iterations: u64 = 0;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let (connections, idle) = db.connections();
                metrics.set_db_pool_state(connections as f64, idle as f64);
                metrics.set_task_queue_depth(task_manager_tx.len() as f64);

                // Sample domain totals less frequently to avoid running
                // COUNT(*) against the database every 5s. 30s cadence.
                if iterations.is_multiple_of(6) {
                    let db = db.clone();
                    let metrics = metrics.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        #[cfg(target_os = "linux")]
                        match crate::process::stats::sample() {
                            Ok(usage) => {
                                use crate::process::stats::Role;
                                for role in Role::ALL {
                                    let Some(u) = usage.get(&role) else {
                                        metrics.set_process_usage(
                                            role.as_str(),
                                            crate::metrics::ProcessUsage::default(),
                                        );
                                        continue;
                                    };
                                    // The server cannot read its parent's PSS.
                                    if u.proportional_bytes.is_none() && role != Role::Web {
                                        warn_pss_unreadable(role);
                                    }
                                    metrics.set_process_usage(
                                        role.as_str(),
                                        crate::metrics::ProcessUsage {
                                            cpu_seconds: u.cpu_seconds,
                                            resident_bytes: u.resident_bytes as f64,
                                            proportional_bytes: u.proportional_bytes.map(|b| b as f64),
                                            swap_bytes: u.swap_bytes.map(|b| b as f64),
                                            peak_resident_bytes: u.peak_resident_bytes.map(|b| b as f64),
                                            processes: u.processes as f64,
                                        },
                                    );
                                }
                            }
                            Err(e) => debug!("failed to sample process usage: {e}"),
                        }

                        let _ = db.read_blocking(|conn| {
                        if let Ok(n) = conn
                            .query_row::<i64, _, _>("SELECT COUNT(*) FROM feeds", [], |row| row.get(0))
                        {
                            metrics.set_feeds_total(n as f64);
                        }
                        if let Ok(n) = conn.query_row::<i64, _, _>(
                            "SELECT COUNT(*) FROM entries",
                            [],
                            |row| row.get(0),
                        ) {
                            metrics.set_entries_total(n as f64);
                        }
                        if let Ok(n) = conn.query_row::<i64, _, _>(
                            "SELECT COUNT(*) FROM feeds WHERE last_fetch_error IS NOT NULL",
                            [],
                            |row| row.get(0),
                        ) {
                            metrics.set_feeds_with_fetch_error(n as f64);
                        }
                        if let Ok(n) = crate::db::size_bytes(conn) {
                            metrics.set_db_size_bytes(n as f64);
                        }
                        });
                    })
                    .await;
                }

                iterations = iterations.wrapping_add(1);
            }
            _ = cancel_token.cancelled() => break,
        }
    }
}

async fn check_feeds_loop(
    task_manager_tx: crate::tasks::TaskSender,
    db: crate::db::Db,
    cancel_token: CancellationToken,
    metrics: Arc<crate::metrics::Metrics>,
) -> Result<()> {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                if let Err(e) = check_feeds(&task_manager_tx, &db, &metrics).await {
                    tracing::error!("Error checking feeds: {:?}", e);
                }
            }
            _ = cancel_token.cancelled() => {
                break;
            }
        }
    }

    Ok(())
}

/// Periodically sends a [`TaskManagerCommand::CleanupAll`] command to
/// trigger retention cleanup. Runs every hour.
async fn cleanup_loop(
    task_manager_tx: crate::tasks::TaskSender,
    cancel_token: CancellationToken,
    metrics: Arc<crate::metrics::Metrics>,
) -> Result<()> {
    let mut interval = tokio::time::interval(Duration::from_secs(3600));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                match task_manager_tx.try_send(TaskManagerCommand::CleanupAll) {
                    Ok(_) => metrics.record_task_enqueued("cleanup_all"),
                    Err(e) => tracing::warn!("Failed to queue periodic cleanup: {:?}", e),
                }
            }
            _ = cancel_token.cancelled() => {
                break;
            }
        }
    }

    Ok(())
}

/// How often [`retry_entry_assets_loop`] looks for asset caching to retry.
const ENTRY_ASSETS_RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// The most entries [`retry_entry_assets_loop`] queues at once, so that a
/// backlog is worked off a batch at a time rather than filling the queue.
const ENTRY_ASSETS_RETRY_BATCH: usize = 128;

/// Periodically queue [`TaskManagerCommand::CacheEntryAssets`] again for
/// new entries whose assets were never cached: those whose task was
/// dropped from a full queue, failed, or was lost to a restart. See
/// [`crate::db::pending_assets`].
async fn retry_entry_assets_loop(
    task_manager_tx: crate::tasks::TaskSender,
    db: crate::db::Db,
    cancel_token: CancellationToken,
    metrics: Arc<crate::metrics::Metrics>,
) {
    let mut interval = tokio::time::interval(ENTRY_ASSETS_RETRY_INTERVAL);
    loop {
        tokio::select! {
            _ = interval.tick() => {
                if let Err(e) = retry_entry_assets(&task_manager_tx, &db, &metrics).await {
                    tracing::warn!("Failed to retry caching entry assets: {:?}", e);
                }
            }
            _ = cancel_token.cancelled() => break,
        }
    }
}

/// Queue one batch of the entries whose asset caching has fallen due.
async fn retry_entry_assets(
    task_manager_tx: &crate::tasks::TaskSender,
    db: &crate::db::Db,
    metrics: &crate::metrics::Metrics,
) -> Result<()> {
    let room = match task_manager_tx.asset_queue_len() {
        // The asset lane has no bound, and anything already in it is ahead
        // of what is retried, so keep it to a batch at a time.
        Some(queued) => ENTRY_ASSETS_RETRY_BATCH.saturating_sub(queued),
        // Leave room in a queue shared with feed refreshes for them.
        None => task_manager_tx
            .capacity()
            .map_or(ENTRY_ASSETS_RETRY_BATCH, |cap| {
                (cap / 2).saturating_sub(task_manager_tx.len())
            })
            .min(ENTRY_ASSETS_RETRY_BATCH),
    };
    if room == 0 {
        return Ok(());
    }
    let due = db
        .write(move |conn| crate::db::pending_assets::take_due(conn, room))
        .await??;
    if !due.abandoned.is_empty() {
        tracing::warn!(
            "Gave up caching the assets of {} entries after {} attempts: {:?}",
            due.abandoned.len(),
            crate::db::pending_assets::MAX_ATTEMPTS,
            due.abandoned
        );
    }
    if !due.retry.is_empty() {
        tracing::info!(
            "Retrying asset caching for {} entries whose caching was lost or failed",
            due.retry.len()
        );
    }
    for entry_id in due.retry {
        // Should it not be queued after all, the entry is simply due
        // again later.
        match task_manager_tx.try_send(TaskManagerCommand::CacheEntryAssets { entry_id }) {
            Ok(_) => metrics.record_task_enqueued("cache_entry_assets"),
            Err(e) => tracing::error!(
                "Failed to requeue asset caching for entry {}: {:?}",
                entry_id,
                e
            ),
        }
    }
    Ok(())
}

/// Wait until the channel to the isolated feed fetcher fails. Never
/// returns when feeds are fetched in this process.
#[cfg(unix)]
async fn fetcher_lost(handle: crate::process::FeedFetcherHandle) {
    match handle {
        Some(host) => host.closed().await,
        None => std::future::pending().await,
    }
}

/// See the `unix` variant.
#[cfg(not(unix))]
async fn fetcher_lost(_: crate::process::FeedFetcherHandle) {
    std::future::pending().await
}

/// Keep count of the task workers in `handles` that are still running,
/// logging each as it finishes, and whether it panicked.
///
/// Returns the count, and a future that completes once every worker has
/// finished.
fn watch_workers(
    handles: Vec<tokio::task::JoinHandle<()>>,
) -> (
    tokio::sync::watch::Receiver<usize>,
    impl std::future::Future<Output = ()>,
) {
    let (tx, rx) = tokio::sync::watch::channel(handles.len());
    let tx = Arc::new(tx);
    for (worker_id, handle) in handles.into_iter().enumerate() {
        let tx = Arc::clone(&tx);
        tokio::spawn(async move {
            match handle.await {
                Ok(()) => debug!("task worker {worker_id} exited"),
                Err(e) => tracing::error!("task worker {worker_id} failed: {e}"),
            }
            tx.send_modify(|live| *live = live.saturating_sub(1));
        });
    }
    let mut all = rx.clone();
    (rx, async move {
        // Only fails once every sender is gone, and the last one to go
        // has counted the last worker out first.
        let _ = all.wait_for(|live| *live == 0).await;
    })
}

/// The state of one of the server's parts, as the health check reports it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ComponentState {
    /// Running in a process of its own, and reachable.
    Ok,
    /// Its process has gone, and will not be back until the server
    /// restarts.
    Gone,
    /// Not isolated in a process of its own: its work is done in the
    /// server process, if at all.
    InProcess,
}

/// Whether the parts of a running server that can fail independently of
/// it are still running, for the health check and the watchdog.
#[derive(Clone)]
pub struct Liveness {
    // Without isolated children, the handles are always `None`, and the
    // accessors below report `InProcess` without looking at them.
    #[cfg_attr(not(unix), allow(dead_code))]
    feed_fetcher: crate::process::FeedFetcherHandle,
    #[cfg_attr(not(unix), allow(dead_code))]
    script_host: crate::process::ScriptHostHandle,
    workers: tokio::sync::watch::Receiver<usize>,
}

impl Liveness {
    /// The state of the isolated feed fetcher.
    pub fn feed_fetcher(&self) -> ComponentState {
        #[cfg(unix)]
        if let Some(host) = &self.feed_fetcher {
            return if host.is_alive() {
                ComponentState::Ok
            } else {
                ComponentState::Gone
            };
        }
        ComponentState::InProcess
    }

    /// The state of the isolated script host.
    pub fn script_host(&self) -> ComponentState {
        #[cfg(unix)]
        if let Some(host) = &self.script_host {
            return if host.is_alive() {
                ComponentState::Ok
            } else {
                ComponentState::Gone
            };
        }
        ComponentState::InProcess
    }

    /// How many task workers are still running.
    pub fn workers(&self) -> usize {
        *self.workers.borrow()
    }

    /// Whether the server can still do its job: fetch feeds, and run the
    /// tasks that do. The script host is not needed for that: without it,
    /// feeds are stored as they come.
    pub fn is_serviceable(&self) -> bool {
        self.feed_fetcher() != ComponentState::Gone && self.workers() > 0
    }
}

/// Ping the service manager's watchdog every `interval` for as long as the
/// server is serviceable and the database answers, so that a server that
/// hangs, or loses what it needs to do its job, is restarted.
async fn watchdog_loop(
    notifier: Option<Arc<crate::notify::Notifier>>,
    interval: Duration,
    liveness: Liveness,
    db: crate::db::Db,
    cancel_token: CancellationToken,
) {
    let Some(notifier) = notifier else { return };
    let mut tick = tokio::time::interval(interval);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = cancel_token.cancelled() => return,
        }
        if !liveness.is_serviceable() {
            tracing::warn!("not pinging the watchdog: the server cannot fetch feeds");
            continue;
        }
        let answered = tokio::time::timeout(
            interval,
            db.read(|conn| conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))),
        )
        .await;
        match answered {
            Ok(Ok(Ok(_))) => notifier.watchdog(),
            Ok(Ok(Err(e))) => tracing::warn!("not pinging the watchdog: database error: {e}"),
            Ok(Err(e)) => tracing::warn!("not pinging the watchdog: database error: {e}"),
            Err(_) => tracing::warn!("not pinging the watchdog: the database did not answer"),
        }
    }
}

/// Compute the delay before the first tick of a persisted periodic task.
///
/// If `last_run_at` is in the past by at least `period`, the task is
/// overdue and the delay is zero. If it's more recent, the delay is the
/// remainder of the period. A `last_run_at` that sits in the future
/// (clock skew) is treated as if it were the current time, yielding a
/// full-period delay.
fn compute_initial_delay(period: Duration, last_run_at: i64, now: i64) -> Duration {
    let elapsed = (now - last_run_at).max(0) as u64;
    Duration::from_secs(period.as_secs().saturating_sub(elapsed))
}

/// Periodically queue a [`TaskManagerCommand`] for execution by a worker.
///
/// Uses the persisted `last_run_at` from `task_queue` so that the schedule
/// resumes across server restarts. If the task is already overdue, the
/// first tick fires immediately; otherwise it waits for the remainder of
/// the period.
#[allow(clippy::too_many_arguments)]
async fn periodic_command_loop(
    task_manager_tx: crate::tasks::TaskSender,
    db: crate::db::Db,
    cancel_token: CancellationToken,
    period: Duration,
    cmd: TaskManagerCommand,
    task_type: &'static str,
    metrics: Arc<crate::metrics::Metrics>,
) -> Result<()> {
    let last_run_at = {
        let ensured = db
            .write(move |conn| crate::db::task_queue::ensure_task(conn, task_type))
            .await;
        let ensured = match ensured {
            Ok(ensured) => ensured,
            Err(e) => {
                tracing::error!(
                    "Failed to get DB connection for task_queue {}: {:?}",
                    task_type,
                    e
                );
                return Ok(());
            }
        };
        match ensured {
            Ok(ts) => ts,
            Err(e) => {
                tracing::error!("Failed to initialize task_queue for {}: {:?}", task_type, e);
                return Ok(());
            }
        }
    };

    let initial_delay = compute_initial_delay(period, last_run_at, chrono::Utc::now().timestamp());

    let start = tokio::time::Instant::now() + initial_delay;
    let mut interval = tokio::time::interval_at(start, period);

    loop {
        tokio::select! {
            _ = interval.tick() => {
                match task_manager_tx.try_send(cmd.clone()) {
                    Ok(_) => metrics.record_task_enqueued(task_type),
                    Err(e) => tracing::warn!("Failed to queue {} task: {:?}", task_type, e),
                }
            }
            _ = cancel_token.cancelled() => {
                break;
            }
        }
    }

    Ok(())
}

/// Queue a [`TaskManagerCommand::RefreshFeed`] for every feed that is due:
/// one that has never been scheduled or whose `next_fetch_at` has passed.
///
/// This is the same test `refresh_feed` applies before fetching, so feeds
/// that are not due are left out rather than queued only to be skipped.
async fn check_feeds(
    task_manager_tx: &crate::tasks::TaskSender,
    db: &crate::db::Db,
    metrics: &crate::metrics::Metrics,
) -> Result<()> {
    let now_ts = chrono::Utc::now().timestamp();
    let feed_ids: Vec<i64> = db
        .read(move |conn| {
            conn.prepare("SELECT id FROM feeds WHERE next_fetch_at IS NULL OR next_fetch_at <= ?1")?
                .query_map([now_ts], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()
        })
        .await??;
    if !feed_ids.is_empty() {
        debug!(
            "Sending RefreshFeed commands for {} due feeds",
            feed_ids.len()
        );
    }

    for feed_id in feed_ids {
        let cmd = TaskManagerCommand::RefreshFeed {
            feed_id,
            manual: false,
        };
        match task_manager_tx.try_send(cmd) {
            Ok(crate::tasks::Enqueue::Queued) => metrics.record_task_enqueued("refresh_feed"),
            Ok(crate::tasks::Enqueue::AlreadyQueued) => {}
            Err(e) => tracing::error!(
                "Failed to send RefreshFeed command for feed {}: {:?}",
                feed_id,
                e
            ),
        }
    }

    Ok(())
}

/// Resolve when the process is asked to terminate: on `SIGTERM` on Unix,
/// and on `CTRL_BREAK_EVENT` on Windows, which is how `kiki web` stops the
/// server it started. Ctrl+C is left to [`signal::ctrl_c`].
///
/// Never resolves if the handler cannot be installed, which is logged.
pub(crate) async fn terminate_signal() {
    #[cfg(unix)]
    let sig = signal::unix::signal(signal::unix::SignalKind::terminate());
    #[cfg(windows)]
    let sig = signal::windows::ctrl_break();

    match sig {
        Ok(mut sig) => {
            sig.recv().await;
        }
        Err(e) => {
            tracing::error!("failed to install signal handler: {:?}", e);
            std::future::pending::<()>().await;
        }
    }
}

async fn shutdown_signal(token: CancellationToken) {
    let handler = || {
        token.cancel();
    };

    let ctrl_c = async {
        if let Err(e) = signal::ctrl_c().await {
            tracing::error!("failed to install Ctrl+C handler: {:?}", e);
        }
    };

    let terminate = terminate_signal();

    tokio::select! {
        _ = ctrl_c => { handler(); }
        _ = terminate => { handler(); }
        _ = token.cancelled() => {}
    }
}

/// Claim `socket_path` for this server, clearing a stale socket file left
/// behind by a previous run.
///
/// A socket file outlives the process that bound it, so `bind(2)` reports
/// `EADDRINUSE` whether the path belongs to a live server or is merely
/// stale. Deleting it unconditionally resolves that in the worst way: a
/// second server started against the same path would silently steal it from
/// the first, leaving the first running but unreachable. Probing with
/// `connect(2)` tells the two cases apart:
///
/// * the connection succeeds — another server is listening, and this one
///   must not displace it;
/// * `ECONNREFUSED` — nothing is listening, so the file is stale and safe to
///   unlink;
/// * the path does not exist — there is nothing to do.
///
/// This is what makes the per-user default socket path a single-instance
/// guard: the second `kiki serve` fails with a clear message instead of
/// quietly taking over.
///
/// # Errors
///
/// Returns an error if another server is already listening on `socket_path`,
/// if the path exists but is not a socket, or if a stale socket file cannot
/// be inspected or removed.
///
/// # Examples
///
/// ```ignore
/// // A path nothing has ever bound is claimed without doing anything.
/// claim_socket_path(Path::new("/run/user/1000/kiki/kiki.sock"))?;
/// ```
fn claim_socket_path(socket_path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(socket_path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(e).with_context(|| {
                format!("Unable to inspect existing socket file at {socket_path:?}")
            })
        }
    };

    if !metadata.file_type().is_socket() {
        bail!(
            "{} already exists and is not a socket; refusing to remove it. Pass \
             --uds to listen somewhere else.",
            socket_path.display()
        );
    }

    match UnixStream::connect(socket_path) {
        Ok(_) => bail!(
            "another Kiki server is already listening on {}. Stop it first, or pass \
             --uds to listen somewhere else.",
            socket_path.display()
        ),
        // Nothing is accepting connections, so the file is a leftover. The
        // NotFound case is a race with another process cleaning up the same
        // stale socket, which leaves us with exactly what we wanted anyway.
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
            debug!("removing stale socket file at {:?}", socket_path);
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("Unable to probe existing socket at {socket_path:?}"))
        }
    }

    fs::remove_file(socket_path)
        .with_context(|| format!("Unable to delete stale socket file at {socket_path:?}"))
}

/// Bind the API's Unix domain socket at `socket_path`, clearing a stale
/// socket file first, and make it readable and
/// writable by its owner and group alone.
///
/// The listener is put in non-blocking mode, ready for the server's
/// runtime to take over with [`ServerBuilder::listener`].
///
/// # Errors
///
/// Returns an error if the path cannot be claimed — another server is
/// listening on it, say, or it is not a socket — or if the socket cannot
/// be bound or its permissions set.
///
/// # Examples
///
/// ```no_run
/// # fn main() -> anyhow::Result<()> {
/// use std::path::Path;
/// let socket = Path::new("/run/user/1000/kiki/kiki.sock");
/// let listener = kiki_rss::server::bind_socket(socket)?;
/// let server = kiki_rss::server::ServerBuilder::new(Path::new("kiki.db"))
///     .socket_path(socket)
///     .listener(listener)
///     .build();
/// # Ok(())
/// # }
/// ```
pub fn bind_socket(socket_path: &Path) -> Result<std::os::unix::net::UnixListener> {
    claim_socket_path(socket_path)?;
    let listener = std::os::unix::net::UnixListener::bind(socket_path)
        .with_context(|| format!("Unable to bind to Unix socket at {:?}", socket_path))?;

    fs::set_permissions(socket_path, fs::Permissions::from_mode(0o660)).with_context(|| {
        format!(
            "Unable to set permissions on Unix socket at {:?}",
            socket_path
        )
    })?;
    listener
        .set_nonblocking(true)
        .with_context(|| format!("Unable to configure Unix socket at {:?}", socket_path))?;
    Ok(listener)
}

/// Parent function for the Unix domain socket web worker threads.
#[allow(clippy::too_many_arguments)]
async fn uds_server(
    listener: std::os::unix::net::UnixListener,
    tcp_listener: Option<std::net::TcpListener>,
    socket_path: PathBuf,
    tx: crate::tasks::TaskSender,
    db: crate::db::Db,
    cancel_token: CancellationToken,
    metrics: Arc<crate::metrics::Metrics>,
    data_dir: PathBuf,
    plugins: Arc<plugins::runtime::PluginRuntime>,
    script_runner: ScriptRunnerHandle,
    config: ConfigHandle,
    liveness: Liveness,
    notifier: Option<Arc<crate::notify::Notifier>>,
) -> Result<()> {
    let shared_state = Arc::new(SharedAppState {
        task_manager_tx: tx,
        db,
        cancel_token: cancel_token.clone(),
        metrics: metrics.clone(),
        data_dir,
        plugins,
        script_runner,
        config,
        liveness,
    });
    // Every route checks the request's token, if it has one, and whether
    // it needs one; see `crate::auth`. The listener a request came in on
    // decides the latter.
    let app = routes::create_router(metrics)
        .layer(axum::middleware::from_fn_with_state(
            shared_state.clone(),
            crate::auth::authorize,
        ))
        .with_state(shared_state);

    if let Some(tcp_listener) = tcp_listener {
        let tcp_listener = tokio::net::TcpListener::from_std(tcp_listener)
            .with_context(|| "Unable to listen on the API's TCP address")?;
        let addr = tcp_listener.local_addr()?;
        tracing::info!("Listening on http://{addr} (API tokens required)");
        let tcp_app = app
            .clone()
            .layer(axum::Extension(crate::auth::Transport::Network));
        let cancel = cancel_token.clone();
        tokio::spawn(async move {
            let served = axum::serve(tcp_listener, tcp_app)
                .with_graceful_shutdown(cancel.clone().cancelled_owned())
                .await;
            if let Err(e) = served {
                tracing::error!("error serving the API on {addr}: {e}");
                cancel.cancel();
            }
        });
    }
    let app = app.layer(axum::Extension(crate::auth::Transport::Socket));

    let listener = UnixListener::from_std(listener)
        .with_context(|| format!("Unable to listen on Unix socket at {:?}", socket_path))?;

    let absolute_path = fs::canonicalize(&socket_path).unwrap_or(socket_path.clone());
    tracing::info!("Listening on {}", absolute_path.display());
    if let Some(n) = &notifier {
        n.ready();
    }

    span!(Level::TRACE, "web-worker");
    axum::serve(listener, app)
        .with_graceful_shutdown(web_shutdown_signal(socket_path, cancel_token.clone()))
        .await
        .with_context(|| "Error encountered while running server")
}

async fn web_shutdown_signal(socket_path: PathBuf, cancel_token: CancellationToken) {
    let handler = || {
        let _ = fs::remove_file(socket_path);
    };

    tokio::select! {
        _ = cancel_token.cancelled() => { handler(); },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod test {
    use super::*;
    use crate::tasks::TaskManagerCommand;
    use crate::test::TestBuilder;
    use anyhow::Result;
    use std::path::Path;

    fn make_pool(path: &Path) -> Result<crate::db::Db> {
        crate::db::Db::open(path, Default::default())
    }

    /// Nothing at the path means nothing to clean up.
    #[test]
    fn claim_socket_path_accepts_an_unused_path() -> Result<()> {
        let td = tempfile::TempDir::with_prefix("kiki_")?;
        claim_socket_path(&td.path().join("kiki.sock"))?;
        Ok(())
    }

    /// A socket file whose server is gone is stale, and gets cleared.
    #[test]
    fn claim_socket_path_removes_a_stale_socket() -> Result<()> {
        let td = tempfile::TempDir::with_prefix("kiki_")?;
        let path = td.path().join("kiki.sock");

        // Dropping the listener closes the socket but leaves its file behind,
        // which is exactly the state a killed server leaves the path in.
        let listener = std::os::unix::net::UnixListener::bind(&path)?;
        drop(listener);
        assert!(path.exists());

        claim_socket_path(&path)?;
        assert!(!path.exists(), "stale socket file should have been removed");

        Ok(())
    }

    /// A socket someone is still listening on belongs to a live server, and
    /// must not be stolen from it.
    #[test]
    fn claim_socket_path_refuses_a_live_socket() -> Result<()> {
        let td = tempfile::TempDir::with_prefix("kiki_")?;
        let path = td.path().join("kiki.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path)?;

        assert!(claim_socket_path(&path).is_err());
        assert!(
            path.exists(),
            "a live server's socket must be left in place"
        );

        Ok(())
    }

    /// Whatever a non-socket file at the path is, it is not ours to delete.
    #[test]
    fn claim_socket_path_refuses_to_remove_a_regular_file() -> Result<()> {
        let td = tempfile::TempDir::with_prefix("kiki_")?;
        let path = td.path().join("kiki.sock");
        fs::write(&path, b"not a socket")?;

        assert!(claim_socket_path(&path).is_err());
        assert!(path.exists());

        Ok(())
    }

    /// Ensure that we can start and stop the server without a panic.
    #[test]
    fn test_start_stop_server() -> Result<()> {
        let mut tc = TestBuilder::all().build()?;
        tc.server_token.take().unwrap().cancel();
        tc.server_handle
            .take()
            .unwrap()
            .join()
            .expect("panic in server thread")?;

        Ok(())
    }

    /// Losing the feed fetcher stops the server with an error, rather than
    /// leaving it up with nothing to fetch feeds.
    #[cfg(unix)]
    #[test]
    fn losing_the_feed_fetcher_stops_the_server() -> Result<()> {
        use crate::process::feed_fetcher::FeedFetcherHost;

        let tc = TestBuilder::default().init_database().build()?;
        let socket = tc.config_dir().join("fetcher-test.sock");
        let (host, far_end) = FeedFetcherHost::with_far_end();
        let server = ServerBuilder::new(&tc.database_path())
            .socket_path(&socket)
            .worker_count(1)
            .feed_fetcher(Some(Arc::new(host)))
            .build();
        let token = server.cancel_token();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done_tx.send(server.run());
        });

        // Up and running while the fetcher is.
        let early = done_rx.recv_timeout(Duration::from_millis(300));
        assert!(early.is_err(), "the server stopped early: {early:?}");

        drop(far_end);
        let result = done_rx.recv_timeout(Duration::from_secs(5));
        token.cancel();
        let err = result
            .expect("the server should stop once the fetcher is gone")
            .expect_err("losing the fetcher should be an error");
        assert!(format!("{err:#}").contains("feed fetcher"), "{err:#}");
        Ok(())
    }

    /// Under a service manager, the server says when it is ready, pings
    /// the watchdog while it is healthy, and says when it stops; and the
    /// health check reports on its parts.
    #[cfg(unix)]
    #[test]
    fn the_service_manager_is_kept_informed() -> Result<()> {
        use crate::process::feed_fetcher::FeedFetcherHost;
        use std::os::unix::net::UnixDatagram;

        let tc = TestBuilder::default().init_database().build()?;
        let notify_path = tc.config_dir().join("notify");
        let manager = UnixDatagram::bind(&notify_path)?;
        manager.set_read_timeout(Some(Duration::from_secs(5)))?;
        let notifier =
            crate::notify::Notifier::for_socket(&notify_path, Some(Duration::from_millis(50)))?;
        let socket = tc.config_dir().join("notify-test.sock");
        // Kept, so that the fetcher stays up.
        let (host, _far_end) = FeedFetcherHost::with_far_end();
        let server = ServerBuilder::new(&tc.database_path())
            .socket_path(&socket)
            .worker_count(2)
            .feed_fetcher(Some(Arc::new(host)))
            .notifier(Some(notifier))
            .build();
        let token = server.cancel_token();
        let handle = std::thread::spawn(move || server.run());

        let mut buf = [0u8; 256];
        let mut recv = || -> Result<String> {
            let n = manager.recv(&mut buf)?;
            Ok(String::from_utf8_lossy(buf.get(..n).unwrap_or_default()).into_owned())
        };
        let mut seen = Vec::new();
        while !(seen.iter().any(|m: &String| m.starts_with("READY=1"))
            && seen.iter().any(|m| m == "WATCHDOG=1"))
        {
            seen.push(recv()?);
        }

        let rt = tokio::runtime::Runtime::new()?;
        let health: crate::routes::v1::health::HealthResponse = rt.block_on(async {
            let client = reqwest::Client::builder().unix_socket(socket).build()?;
            let resp = client.get("http://localhost/v1/health").send().await?;
            anyhow::ensure!(resp.status() == reqwest::StatusCode::OK);
            Ok::<_, anyhow::Error>(resp.json().await?)
        })?;
        assert_eq!(health.status, "ok");
        assert_eq!(health.feed_fetcher, ComponentState::Ok);
        assert_eq!(health.script_host, ComponentState::InProcess);
        assert_eq!(health.workers, 2);
        assert!(health.database_writable);

        token.cancel();
        while recv()? != "STOPPING=1" {}
        handle.join().expect("panic in server thread")?;
        Ok(())
    }

    /// Losing the fetcher, or every worker, leaves the server unable to do
    /// its job; losing the script host does not.
    #[cfg(unix)]
    #[tokio::test]
    async fn liveness_reflects_the_server_parts() {
        use crate::process::feed_fetcher::FeedFetcherHost;

        let (host, far_end) = FeedFetcherHost::with_far_end();
        let host = Arc::new(host);
        let (workers_tx, workers) = tokio::sync::watch::channel(1usize);
        let liveness = Liveness {
            feed_fetcher: Some(Arc::clone(&host)),
            script_host: None,
            workers,
        };
        assert_eq!(liveness.feed_fetcher(), ComponentState::Ok);
        assert!(liveness.is_serviceable());

        workers_tx.send_replace(0);
        assert!(!liveness.is_serviceable());
        workers_tx.send_replace(1);

        drop(far_end);
        host.closed().await;
        assert_eq!(liveness.feed_fetcher(), ComponentState::Gone);
        assert!(!liveness.is_serviceable());

        let in_process = Liveness {
            feed_fetcher: None,
            script_host: None,
            workers: tokio::sync::watch::channel(1usize).1,
        };
        assert_eq!(in_process.feed_fetcher(), ComponentState::InProcess);
        assert!(in_process.is_serviceable());
    }

    #[test]
    fn compute_initial_delay_fresh_install_waits_full_period() {
        let now = 1_000_000;
        let delay = compute_initial_delay(Duration::from_secs(3600), now, now);
        assert_eq!(delay, Duration::from_secs(3600));
    }

    #[test]
    fn compute_initial_delay_recent_run_waits_remainder() {
        let now = 1_000_000;
        let delay = compute_initial_delay(Duration::from_secs(3600), now - 600, now);
        assert_eq!(delay, Duration::from_secs(3000));
    }

    #[test]
    fn compute_initial_delay_overdue_returns_zero() {
        let now = 1_000_000;
        let delay = compute_initial_delay(Duration::from_secs(3600), now - 10_000, now);
        assert_eq!(delay, Duration::from_secs(0));
    }

    #[test]
    fn compute_initial_delay_future_last_run_returns_full_period() {
        // Clock skew: persisted timestamp is ahead of `now`. `(now - last).max(0)`
        // clamps elapsed to zero, so the delay is the full period.
        let now = 1_000_000;
        let delay = compute_initial_delay(Duration::from_secs(3600), now + 500, now);
        assert_eq!(delay, Duration::from_secs(3600));
    }

    /// A feed whose refresh is still waiting in the queue is not queued
    /// again by the next tick, so workers that fall behind do not see the
    /// queue fill with copies of the same refreshes; once a worker takes
    /// it, the feed can be queued again.
    #[tokio::test]
    async fn check_feeds_does_not_queue_a_refresh_twice() -> Result<()> {
        let tc = TestBuilder::default().init_database().build()?;
        let pool = make_pool(&tc.database_path())?;
        let conn = pool.connect();
        conn.execute(
            "INSERT INTO feeds (title, url) VALUES ('feed', 'https://example.com/a.xml')",
            [],
        )?;
        drop(conn);

        let (tx, rx) = async_channel::bounded(16);
        let tx = crate::tasks::TaskSender::from(tx);
        let metrics = crate::metrics::Metrics::new()?;
        check_feeds(&tx, &pool, &metrics).await?;
        check_feeds(&tx, &pool, &metrics).await?;
        assert_eq!(rx.len(), 1);

        tx.dequeued(&rx.try_recv()?);
        check_feeds(&tx, &pool, &metrics).await?;
        assert_eq!(rx.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn check_feeds_queues_only_due_feeds() -> Result<()> {
        let tc = TestBuilder::default().init_database().build()?;
        let pool = make_pool(&tc.database_path())?;
        let now = chrono::Utc::now().timestamp();

        let conn = pool.connect();
        let insert = |url: &str, next_fetch_at: Option<i64>| -> Result<i64> {
            conn.execute(
                "INSERT INTO feeds (title, url, next_fetch_at) VALUES ('feed', ?1, ?2)",
                rusqlite::params![url, next_fetch_at],
            )?;
            Ok(conn.last_insert_rowid())
        };
        let never = insert("https://example.com/never.xml", None)?;
        let due = insert("https://example.com/due.xml", Some(now - 10))?;
        insert("https://example.com/later.xml", Some(now + 3600))?;
        drop(conn);

        let (tx, rx) = async_channel::bounded(16);
        let tx = crate::tasks::TaskSender::from(tx);
        check_feeds(&tx, &pool, &crate::metrics::Metrics::new()?).await?;

        let mut queued = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            let TaskManagerCommand::RefreshFeed { feed_id, manual } = cmd else {
                anyhow::bail!("unexpected command {cmd:?}");
            };
            assert!(!manual, "scheduled refreshes are not manual");
            queued.push(feed_id);
        }
        queued.sort();
        assert_eq!(queued, vec![never, due]);
        Ok(())
    }

    /// Entries whose asset caching fell due are queued again, and counted
    /// as another attempt.
    #[tokio::test(flavor = "multi_thread")]
    async fn pending_entry_assets_are_queued_again_once_due() -> Result<()> {
        let tc = TestBuilder::default().init_database().build()?;
        let pool = make_pool(&tc.database_path())?;
        let conn = tc.database_conn()?;
        conn.execute(
            "INSERT INTO entries (feed_id, syndication_format, guid, published_at, title, url)
             VALUES (NULL, 'rss', 'g', 0, 't', 'https://example.com/')",
            [],
        )?;
        let entry_id = conn.last_insert_rowid();
        crate::db::pending_assets::add(&conn, entry_id)?;
        let metrics = crate::metrics::Metrics::new()?;
        let (tx, rx) = async_channel::bounded(16);
        let tx = crate::tasks::TaskSender::from(tx);

        // Not yet due: the task queued alongside it may still be running.
        retry_entry_assets(&tx, &pool, &metrics).await?;
        assert!(rx.try_recv().is_err());

        conn.execute(
            "UPDATE pending_entry_assets SET next_attempt_at = unixepoch() - 1",
            [],
        )?;
        retry_entry_assets(&tx, &pool, &metrics).await?;
        match rx.try_recv()? {
            TaskManagerCommand::CacheEntryAssets { entry_id: queued } => {
                assert_eq!(queued, entry_id)
            }
            other => anyhow::bail!("unexpected command {other:?}"),
        }
        let attempts: i64 = conn.query_row(
            "SELECT attempts FROM pending_entry_assets WHERE entry_id = ?1",
            [entry_id],
            |r| r.get(0),
        )?;
        assert_eq!(attempts, 1);
        Ok(())
    }

    /// With asset caching in a lane of its own, retries are held back while
    /// a batch's worth is already waiting there, rather than by how full
    /// the main lane is.
    #[tokio::test(flavor = "multi_thread")]
    async fn retried_asset_caching_waits_for_the_asset_lane() -> Result<()> {
        let tc = TestBuilder::default().init_database().build()?;
        let pool = make_pool(&tc.database_path())?;
        let conn = tc.database_conn()?;
        conn.execute(
            "INSERT INTO entries (feed_id, syndication_format, guid, published_at, title, url)
             VALUES (NULL, 'rss', 'g', 0, 't', 'https://example.com/')",
            [],
        )?;
        let entry_id = conn.last_insert_rowid();
        crate::db::pending_assets::add(&conn, entry_id)?;
        conn.execute(
            "UPDATE pending_entry_assets SET next_attempt_at = unixepoch() - 1",
            [],
        )?;
        let metrics = crate::metrics::Metrics::new()?;
        let (tx, rx) = crate::tasks::queue(1);
        // A full main lane does not hold retries back...
        tx.try_send(TaskManagerCommand::CleanupAll)?;
        // ...but a batch already waiting in the asset lane does.
        for queued in 0..ENTRY_ASSETS_RETRY_BATCH as i64 {
            tx.try_send(TaskManagerCommand::CacheEntryAssets {
                entry_id: -1 - queued,
            })?;
        }
        retry_entry_assets(&tx, &pool, &metrics).await?;
        assert_eq!(tx.asset_queue_len(), Some(ENTRY_ASSETS_RETRY_BATCH));

        // Once that is taken, the due entry is queued.
        rx.recv().await?;
        for _ in 0..ENTRY_ASSETS_RETRY_BATCH {
            rx.recv().await?;
        }
        retry_entry_assets(&tx, &pool, &metrics).await?;
        match rx.recv().await? {
            TaskManagerCommand::CacheEntryAssets { entry_id: queued } => {
                assert_eq!(queued, entry_id)
            }
            other => anyhow::bail!("unexpected command {other:?}"),
        }
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn periodic_command_loop_fires_immediately_when_overdue() -> Result<()> {
        let tc = TestBuilder::default().init_database().build()?;
        let pool = make_pool(&tc.database_path())?;

        // Anchor an ancient last_run_at so the task is obviously overdue.
        {
            let conn = pool.connect();
            conn.execute(
                "INSERT INTO task_queue (task_type, last_run_at) VALUES ('test_overdue', 0)",
                [],
            )?;
        }

        let (tx, rx) = async_channel::bounded(4);
        let tx = crate::tasks::TaskSender::from(tx);
        let token = CancellationToken::new();
        let handle = tokio::spawn(periodic_command_loop(
            tx,
            pool.clone(),
            token.clone(),
            Duration::from_secs(60),
            TaskManagerCommand::OptimizeFts,
            "test_overdue",
            Arc::new(crate::metrics::Metrics::new()?),
        ));

        let got = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await;
        assert!(got.is_ok(), "overdue task should fire immediately");

        token.cancel();
        handle.await.ok();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn periodic_command_loop_waits_when_not_overdue() -> Result<()> {
        let tc = TestBuilder::default().init_database().build()?;
        let pool = make_pool(&tc.database_path())?;

        // Anchor last_run_at to now so the next tick is ~60s away.
        {
            let conn = pool.connect();
            conn.execute(
                "INSERT INTO task_queue (task_type, last_run_at)
                 VALUES ('test_recent', unixepoch())",
                [],
            )?;
        }

        let (tx, rx) = async_channel::bounded(4);
        let tx = crate::tasks::TaskSender::from(tx);
        let token = CancellationToken::new();
        let handle = tokio::spawn(periodic_command_loop(
            tx,
            pool.clone(),
            token.clone(),
            Duration::from_secs(60),
            TaskManagerCommand::OptimizeFts,
            "test_recent",
            Arc::new(crate::metrics::Metrics::new()?),
        ));

        let got = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
        assert!(
            got.is_err(),
            "recent task should not fire within 300ms, got {:?}",
            got
        );

        token.cancel();
        handle.await.ok();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn periodic_command_loop_creates_task_queue_row_on_first_run() -> Result<()> {
        let tc = TestBuilder::default().init_database().build()?;
        let pool = make_pool(&tc.database_path())?;

        // Confirm no row exists yet.
        {
            let conn = pool.connect();
            let n: i64 = conn.query_row(
                "SELECT count(*) FROM task_queue WHERE task_type = 'test_fresh'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(n, 0);
        }

        let before = chrono::Utc::now().timestamp();
        let (tx, _rx) = async_channel::bounded(4);
        let tx = crate::tasks::TaskSender::from(tx);
        let token = CancellationToken::new();
        let handle = tokio::spawn(periodic_command_loop(
            tx,
            pool.clone(),
            token.clone(),
            Duration::from_secs(60),
            TaskManagerCommand::OptimizeFts,
            "test_fresh",
            Arc::new(crate::metrics::Metrics::new()?),
        ));

        // Wait for the loop to call ensure_task.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let ts: i64 = loop {
            let row = pool.connect().query_row(
                "SELECT last_run_at FROM task_queue WHERE task_type = 'test_fresh'",
                [],
                |r| r.get(0),
            );
            match row {
                Ok(ts) => break ts,
                Err(rusqlite::Error::QueryReturnedNoRows)
                    if std::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Err(e) => return Err(e.into()),
            }
        };
        let after = chrono::Utc::now().timestamp();
        assert!(ts >= before && ts <= after, "unexpected last_run_at {ts}");

        token.cancel();
        handle.await.ok();
        Ok(())
    }
}
