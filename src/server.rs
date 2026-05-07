use crate::{
    db::migrations,
    routes,
    scripting::ScriptRunnerHandle,
    tasks::{self, TaskManagerCommand},
};
use anyhow::{bail, Context, Error, Result};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::OpenFlags;
use std::{
    fs,
    net::SocketAddr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::{TcpListener, UnixListener},
    signal,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, span, Level};

pub struct SharedAppState {
    /// An [`async_channel::Sender`] instance that may be used to send commands
    /// to worker tasks used to fetch and process feeds.
    pub task_manager_tx: async_channel::Sender<TaskManagerCommand>,

    /// A [`tokio::sync::watch::Sender`] used to signal all workers to reload
    /// their configuration (e.g. Lua script runners).
    pub reload_tx: tokio::sync::watch::Sender<()>,

    /// A [`r2d2::Pool`] instance that intermediates connections to the
    /// SQLite database.
    pub conn_pool: r2d2::Pool<SqliteConnectionManager>,

    /// A [`CancellationToken`] that can be used to trigger a graceful
    /// server shutdown.
    pub cancel_token: CancellationToken,

    /// Per-server Prometheus metrics recorder.
    pub metrics: Arc<crate::metrics::Metrics>,

    /// Root directory for on-disk state. Used to locate the cached asset
    /// filesystem under `{data_dir}/assets/`.
    pub data_dir: PathBuf,

    /// Shared handle to the currently-installed scripting engine. Empty when the `lua`
    /// feature is disabled or when no scripts have been loaded.
    pub script_runner: ScriptRunnerHandle,
}

pub type AppState = Arc<SharedAppState>;

/// Specifies how the server should listen for connections.
pub enum ListenAddr {
    /// Listen on a Unix domain socket at the given path.
    Uds(PathBuf),
    /// Listen on a TCP socket address.
    Tcp(SocketAddr),
}

pub struct ServerBuilder<'a> {
    db_path: &'a Path,
    listen_addr: Option<ListenAddr>,
    autofetch: bool,
    single_threaded: bool,
    worker_count: Option<usize>,
}

impl<'a> ServerBuilder<'a> {
    pub fn new(db_path: &'a Path) -> Self {
        ServerBuilder {
            db_path,
            listen_addr: None,
            autofetch: false,
            single_threaded: false,
            worker_count: None,
        }
    }

    pub fn socket_path(mut self, p: &'a Path) -> Self {
        self.listen_addr = Some(ListenAddr::Uds(p.to_path_buf()));
        self
    }

    pub fn bind_addr(mut self, addr: SocketAddr) -> Self {
        self.listen_addr = Some(ListenAddr::Tcp(addr));
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

    pub fn build(self) -> Server {
        let listen_addr = self
            .listen_addr
            .unwrap_or_else(|| ListenAddr::Uds(PathBuf::from("kiki.sock")));

        // Default the data directory to the directory containing the
        // database file. Callers that want an explicit location can extend
        // the builder later.
        let data_dir = self
            .db_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        Server {
            db_path: PathBuf::from(self.db_path),
            data_dir,
            listen_addr,
            autofetch: self.autofetch,
            single_threaded: self.single_threaded,
            worker_count: self.worker_count,
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

    /// How the server listens for connections.
    listen_addr: ListenAddr,

    /// Whether or not to automatically fetch feed contents.
    autofetch: bool,

    /// Use single-threaded tokio runtimes instead of multi-threaded ones.
    single_threaded: bool,

    /// Override the number of feed-fetcher worker tasks.
    worker_count: Option<usize>,

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
                .enable_all()
                .build()?
        } else {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
        };
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

        // Create a pool of connections that can be shared between all of
        // the threads that we spawn.
        let manager = SqliteConnectionManager::file(&self.db_path)
            .with_flags(OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .with_init(|c| {
                c.execute_batch(
                    "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;",
                )?;
                {
                    use rusqlite::functions::FunctionFlags;
                    c.create_scalar_function(
                        "regexp",
                        2,
                        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
                        |ctx| {
                            let pattern = ctx.get_raw(0).as_str()?;
                            let text = ctx.get_raw(1).as_str().unwrap_or("");
                            let re = regex::Regex::new(pattern)
                                .map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))?;
                            Ok(re.is_match(text))
                        },
                    )?;
                }
                Ok(())
            });
        let pool = r2d2::Pool::new(manager).with_context(|| {
            format!(
                "Unable to open connection pool to database at {:?}",
                &self.db_path
            )
        })?;

        // Check for pending migrations before starting the server
        {
            let conn = pool
                .get()
                .with_context(|| "failed to get connection for migration check")?;
            let pending = migrations::pending_migrations(&conn)?;
            if !pending.is_empty() {
                let names: Vec<&str> = pending.iter().map(|m| m.name).collect();
                bail!(
                    "Database has {} pending migration(s): {}. Run `kiki migrate` first.",
                    pending.len(),
                    names.join(", ")
                );
            }
        }

        // Create a multi-producer, multi-consumer channel so that web
        // service workers can send tasks to the feed-fetcher workers.
        let (tx, rx) = async_channel::bounded(1024);

        // Watch channel for broadcasting script-reload signals.
        let (reload_tx, reload_rx) = tokio::sync::watch::channel(());

        // Shared scripting engine handle; workers and HTTP handlers dispatch events
        // through it.
        let script_runner = ScriptRunnerHandle::empty();

        // Build the initial runner and spawn the reloader task (lua feature only).
        #[cfg(feature = "lua")]
        {
            tasks::reload_script_runner(&pool, &metrics, &script_runner);
            tokio::spawn(tasks::run_script_reloader(
                pool.clone(),
                metrics.clone(),
                script_runner.clone(),
                reload_rx,
                self.cancel_token.clone(),
            ));
        }
        #[cfg(not(feature = "lua"))]
        {
            // `reload_rx` would otherwise be unused.
            let _ = reload_rx;
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

        let _worker_handles = tasks::spawn_workers(
            rx,
            tx.clone(),
            pool.clone(),
            self.cancel_token.clone(),
            num_workers,
            metrics.clone(),
            self.data_dir.clone(),
            script_runner.clone(),
        );

        tokio::spawn(metrics_sampler_loop(
            tx.clone(),
            pool.clone(),
            self.cancel_token.clone(),
            metrics.clone(),
        ));

        if self.autofetch {
            tokio::spawn(check_feeds_loop(
                tx.clone(),
                pool.clone(),
                self.cancel_token.clone(),
                metrics.clone(),
            ));
            tokio::spawn(cleanup_loop(
                tx.clone(),
                self.cancel_token.clone(),
                metrics.clone(),
            ));
            tokio::spawn(periodic_command_loop(
                tx.clone(),
                pool.clone(),
                self.cancel_token.clone(),
                Duration::from_secs(86400),
                TaskManagerCommand::WalCheckpointAnalyze,
                crate::db::task_queue::TASK_WAL_CHECKPOINT_ANALYZE,
                metrics.clone(),
            ));
            tokio::spawn(periodic_command_loop(
                tx.clone(),
                pool.clone(),
                self.cancel_token.clone(),
                Duration::from_secs(86400),
                TaskManagerCommand::IncrementalVacuum,
                crate::db::task_queue::TASK_INCREMENTAL_VACUUM,
                metrics.clone(),
            ));
            tokio::spawn(periodic_command_loop(
                tx.clone(),
                pool.clone(),
                self.cancel_token.clone(),
                Duration::from_secs(604800),
                TaskManagerCommand::OptimizeFts,
                crate::db::task_queue::TASK_FTS_OPTIMIZE,
                metrics.clone(),
            ));
        }

        match self.listen_addr {
            ListenAddr::Uds(socket_path) => {
                if socket_path.exists() {
                    fs::remove_file(&socket_path).with_context(|| {
                        format!(
                            "Unable to delete existing socket file from {:?}",
                            &socket_path
                        )
                    })?;
                }
                tokio::spawn(uds_server(
                    socket_path,
                    tx.clone(),
                    reload_tx.clone(),
                    pool.clone(),
                    self.cancel_token.clone(),
                    metrics.clone(),
                    self.data_dir.clone(),
                    script_runner.clone(),
                ));
            }
            ListenAddr::Tcp(addr) => {
                tokio::spawn(tcp_server(
                    addr,
                    tx.clone(),
                    reload_tx.clone(),
                    pool.clone(),
                    self.cancel_token.clone(),
                    metrics.clone(),
                    self.data_dir.clone(),
                    script_runner.clone(),
                ));
            }
        }

        self.cancel_token.cancelled().await;

        Ok(())
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }
}

/// Periodically sample observable process state into the metrics recorder.
///
/// Covers DB pool utilization, task queue depth, and domain totals that are
/// cheap to read (feed/entry counts, feeds with fetch errors).
async fn metrics_sampler_loop(
    task_manager_tx: async_channel::Sender<TaskManagerCommand>,
    pool: r2d2::Pool<SqliteConnectionManager>,
    cancel_token: CancellationToken,
    metrics: Arc<crate::metrics::Metrics>,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    let mut iterations: u64 = 0;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let state = pool.state();
                metrics.set_db_pool_state(
                    state.connections as f64,
                    state.idle_connections as f64,
                );
                metrics.set_task_queue_depth(task_manager_tx.len() as f64);

                // Sample domain totals less frequently to avoid running
                // COUNT(*) against the database every 5s. 30s cadence.
                if iterations.is_multiple_of(6) {
                    let pool = pool.clone();
                    let metrics = metrics.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        let conn = match pool.get() {
                            Ok(c) => c,
                            Err(_) => return,
                        };
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
    task_manager_tx: async_channel::Sender<TaskManagerCommand>,
    pool: r2d2::Pool<SqliteConnectionManager>,
    cancel_token: CancellationToken,
    metrics: Arc<crate::metrics::Metrics>,
) -> Result<()> {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                if let Err(e) = check_feeds(&task_manager_tx, &pool, &metrics) {
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
    task_manager_tx: async_channel::Sender<TaskManagerCommand>,
    cancel_token: CancellationToken,
    metrics: Arc<crate::metrics::Metrics>,
) -> Result<()> {
    let mut interval = tokio::time::interval(Duration::from_secs(3600));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                match task_manager_tx.try_send(TaskManagerCommand::CleanupAll) {
                    Ok(()) => metrics.record_task_enqueued("cleanup_all"),
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
    task_manager_tx: async_channel::Sender<TaskManagerCommand>,
    pool: r2d2::Pool<SqliteConnectionManager>,
    cancel_token: CancellationToken,
    period: Duration,
    cmd: TaskManagerCommand,
    task_type: &'static str,
    metrics: Arc<crate::metrics::Metrics>,
) -> Result<()> {
    let last_run_at = {
        let conn = match pool.get() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(
                    "Failed to get DB connection for task_queue {}: {:?}",
                    task_type,
                    e
                );
                return Ok(());
            }
        };
        match crate::db::task_queue::ensure_task(&conn, task_type) {
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
                    Ok(()) => metrics.record_task_enqueued(task_type),
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

fn check_feeds(
    task_manager_tx: &async_channel::Sender<TaskManagerCommand>,
    pool: &r2d2::Pool<SqliteConnectionManager>,
    metrics: &crate::metrics::Metrics,
) -> Result<()> {
    debug!("Sending RefreshFeed commands for all feeds");
    let conn = pool.get()?;

    // Query all feed IDs
    let mut stmt = conn.prepare("SELECT id FROM feeds")?;

    let feed_ids = stmt.query_map([], |row| {
        let id: i64 = row.get(0)?;
        Ok(id)
    })?;

    // Send a RefreshFeed command for each feed
    for feed_id in feed_ids {
        let feed_id = feed_id?;
        match task_manager_tx.try_send(TaskManagerCommand::RefreshFeed(feed_id)) {
            Ok(()) => metrics.record_task_enqueued("refresh_feed"),
            Err(e) => tracing::error!(
                "Failed to send RefreshFeed command for feed {}: {:?}",
                feed_id,
                e
            ),
        }
    }

    Ok(())
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

    #[cfg(unix)]
    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::error!("failed to install signal handler: {:?}", e);
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => { handler(); }
        _ = terminate => { handler(); }
        _ = token.cancelled() => {}
    }
}

/// Parent function for the Unix domain socket web worker threads.
#[allow(clippy::too_many_arguments)]
async fn uds_server(
    socket_path: PathBuf,
    tx: async_channel::Sender<TaskManagerCommand>,
    reload_tx: tokio::sync::watch::Sender<()>,
    pool: r2d2::Pool<SqliteConnectionManager>,
    cancel_token: CancellationToken,
    metrics: Arc<crate::metrics::Metrics>,
    data_dir: PathBuf,
    script_runner: ScriptRunnerHandle,
) -> Result<()> {
    let shared_state = Arc::new(SharedAppState {
        task_manager_tx: tx,
        reload_tx,
        conn_pool: pool,
        cancel_token: cancel_token.clone(),
        metrics: metrics.clone(),
        data_dir,
        script_runner,
    });
    let app = routes::create_router(metrics, shared_state.clone()).with_state(shared_state);

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("Unable to bind to Unix socket at {:?}", &socket_path))?;

    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o660)).with_context(|| {
        format!(
            "Unable to set permissions on Unix socket at {:?}",
            &socket_path
        )
    })?;

    let absolute_path = fs::canonicalize(&socket_path).unwrap_or(socket_path.clone());
    tracing::info!("Listening on {}", absolute_path.display());

    span!(Level::TRACE, "web-worker");
    axum::serve(listener, app)
        .with_graceful_shutdown(web_shutdown_signal(socket_path, cancel_token.clone()))
        .await
        .with_context(|| "Error encountered while running server")
}

/// Parent function for the TCP web worker threads.
#[allow(clippy::too_many_arguments)]
async fn tcp_server(
    addr: SocketAddr,
    tx: async_channel::Sender<TaskManagerCommand>,
    reload_tx: tokio::sync::watch::Sender<()>,
    pool: r2d2::Pool<SqliteConnectionManager>,
    cancel_token: CancellationToken,
    metrics: Arc<crate::metrics::Metrics>,
    data_dir: PathBuf,
    script_runner: ScriptRunnerHandle,
) -> Result<()> {
    let shared_state = Arc::new(SharedAppState {
        task_manager_tx: tx,
        reload_tx,
        conn_pool: pool,
        cancel_token: cancel_token.clone(),
        metrics: metrics.clone(),
        data_dir,
        script_runner,
    });
    let app = routes::create_router(metrics, shared_state.clone()).with_state(shared_state);

    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("Unable to bind to TCP address {}", addr))?;

    tracing::info!("Listening on http://{}", addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(cancel_token.cancelled_owned())
        .await
        .with_context(|| "Error encountered while running TCP server")
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
    use rusqlite::OpenFlags;
    use std::path::Path;

    fn make_pool(path: &Path) -> Result<r2d2::Pool<SqliteConnectionManager>> {
        let manager = SqliteConnectionManager::file(path)
            .with_flags(OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .with_init(|c| c.execute_batch("PRAGMA foreign_keys=ON;"));
        Ok(r2d2::Pool::new(manager)?)
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

    #[tokio::test(flavor = "multi_thread")]
    async fn periodic_command_loop_fires_immediately_when_overdue() -> Result<()> {
        let tc = TestBuilder::default().init_database().build()?;
        let pool = make_pool(&tc.database_path())?;

        // Anchor an ancient last_run_at so the task is obviously overdue.
        {
            let conn = pool.get()?;
            conn.execute(
                "INSERT INTO task_queue (task_type, last_run_at) VALUES ('test_overdue', 0)",
                [],
            )?;
        }

        let (tx, rx) = async_channel::bounded(4);
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
            let conn = pool.get()?;
            conn.execute(
                "INSERT INTO task_queue (task_type, last_run_at)
                 VALUES ('test_recent', unixepoch())",
                [],
            )?;
        }

        let (tx, rx) = async_channel::bounded(4);
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
            let conn = pool.get()?;
            let n: i64 = conn.query_row(
                "SELECT count(*) FROM task_queue WHERE task_type = 'test_fresh'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(n, 0);
        }

        let before = chrono::Utc::now().timestamp();
        let (tx, _rx) = async_channel::bounded(4);
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

        // Give the loop a moment to call ensure_task.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let after = chrono::Utc::now().timestamp();

        let ts: i64 = {
            let conn = pool.get()?;
            conn.query_row(
                "SELECT last_run_at FROM task_queue WHERE task_type = 'test_fresh'",
                [],
                |r| r.get(0),
            )?
        };
        assert!(ts >= before && ts <= after, "unexpected last_run_at {ts}");

        token.cancel();
        handle.await.ok();
        Ok(())
    }
}
