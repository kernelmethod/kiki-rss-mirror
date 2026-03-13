use crate::{
    db::migrations,
    routes,
    tasks::{self, TaskManagerCommand},
};
use anyhow::{bail, Context, Error, Result};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::OpenFlags;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{atomic, Arc},
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
}

pub type AppState = Arc<SharedAppState>;

/// Specifies how the server should listen for connections.
pub enum ListenAddr {
    /// Listen on a Unix domain socket at the given path.
    Uds(PathBuf),
    /// Listen on a localhost TCP port.
    Tcp(u16),
}

pub struct ServerBuilder<'a> {
    db_path: &'a Path,
    listen_addr: Option<ListenAddr>,
    autofetch: bool,
}

impl<'a> ServerBuilder<'a> {
    pub fn new(db_path: &'a Path) -> Self {
        ServerBuilder {
            db_path,
            listen_addr: None,
            autofetch: false,
        }
    }

    pub fn socket_path(mut self, p: &'a Path) -> Self {
        self.listen_addr = Some(ListenAddr::Uds(p.to_path_buf()));
        self
    }

    pub fn port(mut self, port: u16) -> Self {
        self.listen_addr = Some(ListenAddr::Tcp(port));
        self
    }

    pub fn autofetch(mut self) -> Self {
        self.autofetch = true;
        self
    }

    pub fn build(self) -> Server {
        let listen_addr = self
            .listen_addr
            .unwrap_or_else(|| ListenAddr::Uds(PathBuf::from("kiki.sock")));

        Server {
            db_path: PathBuf::from(self.db_path),
            listen_addr,
            autofetch: self.autofetch,
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

    /// How the server listens for connections.
    listen_addr: ListenAddr,

    /// Whether or not to automatically fetch feed contents.
    autofetch: bool,

    /// A [`CancellationToken`] used to indicate that the server should
    /// be killed.
    cancel_token: CancellationToken,
}

impl Server {
    pub fn run(self) -> Result<()> {
        // Create a pool of connections that can be shared between all of
        // the threads that we spawn.
        let manager = SqliteConnectionManager::file(&self.db_path)
            .with_flags(OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .with_init(|c| {
                c.execute_batch(
                    "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;",
                )
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

        // We create two separate runtimes, one for the feed-fetchers and
        // one for the web service workers.
        //
        // This ensures that feed fetcher threads don't consume all of
        // the resources being used by the server threads.
        let task_manager_runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name_fn(|| {
                static ATOMIC_ID: atomic::AtomicUsize = atomic::AtomicUsize::new(0);
                let id = ATOMIC_ID.fetch_add(1, atomic::Ordering::SeqCst);
                format!("feed-fetcher-{}", id)
            })
            .build()
            .with_context(|| "failed to build Tokio runtime for feed fetchers")?;
        let web_runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name_fn(|| {
                static ATOMIC_ID: atomic::AtomicUsize = atomic::AtomicUsize::new(0);
                let id = ATOMIC_ID.fetch_add(1, atomic::Ordering::SeqCst);
                format!("server-worker-{}", id)
            })
            .build()
            .with_context(|| "failed to build Tokio runtime for web service workers")?;

        // Create a multi-producer, multi-consumer channel so that web
        // service workers can send tasks to the feed-fetcher workers.
        let (tx, rx) = async_channel::bounded(1024);

        // Watch channel for broadcasting script-reload signals to all workers.
        let (reload_tx, _) = tokio::sync::watch::channel(());

        let num_workers = tasks::worker_count();
        debug!("Spawning {} task-manager workers", num_workers);

        let _worker_handles = {
            let _guard = task_manager_runtime.enter();
            tasks::spawn_workers(
                rx,
                tx.clone(),
                pool.clone(),
                self.cancel_token.clone(),
                reload_tx.clone(),
                num_workers,
            )
        };

        if self.autofetch {
            web_runtime.spawn(check_feeds_loop(
                tx.clone(),
                pool.clone(),
                self.cancel_token.clone(),
            ));
            web_runtime.spawn(cleanup_loop(tx.clone(), self.cancel_token.clone()));
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
                web_runtime.spawn(uds_server(
                    socket_path,
                    tx.clone(),
                    reload_tx.clone(),
                    pool.clone(),
                    self.cancel_token.clone(),
                ));
            }
            ListenAddr::Tcp(port) => {
                web_runtime.spawn(tcp_server(
                    port,
                    tx.clone(),
                    reload_tx.clone(),
                    pool.clone(),
                    self.cancel_token.clone(),
                ));
            }
        }
        let cancel_task = web_runtime.spawn(shutdown_signal(self.cancel_token.clone()));

        web_runtime.block_on(cancel_task)?;

        Ok(())
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }
}

async fn check_feeds_loop(
    task_manager_tx: async_channel::Sender<TaskManagerCommand>,
    pool: r2d2::Pool<SqliteConnectionManager>,
    cancel_token: CancellationToken,
) -> Result<()> {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                if let Err(e) = check_feeds(&task_manager_tx, &pool) {
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
) -> Result<()> {
    let mut interval = tokio::time::interval(Duration::from_secs(3600));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                if let Err(e) = task_manager_tx.try_send(TaskManagerCommand::CleanupAll) {
                    tracing::warn!("Failed to queue periodic cleanup: {:?}", e);
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
        if let Err(e) = task_manager_tx.try_send(TaskManagerCommand::RefreshFeed(feed_id)) {
            tracing::error!(
                "Failed to send RefreshFeed command for feed {}: {:?}",
                feed_id,
                e
            );
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
async fn uds_server(
    socket_path: PathBuf,
    tx: async_channel::Sender<TaskManagerCommand>,
    reload_tx: tokio::sync::watch::Sender<()>,
    pool: r2d2::Pool<SqliteConnectionManager>,
    cancel_token: CancellationToken,
) -> Result<()> {
    let shared_state = Arc::new(SharedAppState {
        task_manager_tx: tx,
        reload_tx,
        conn_pool: pool,
        cancel_token: cancel_token.clone(),
    });
    let app = routes::create_router().with_state(shared_state);

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
async fn tcp_server(
    port: u16,
    tx: async_channel::Sender<TaskManagerCommand>,
    reload_tx: tokio::sync::watch::Sender<()>,
    pool: r2d2::Pool<SqliteConnectionManager>,
    cancel_token: CancellationToken,
) -> Result<()> {
    let shared_state = Arc::new(SharedAppState {
        task_manager_tx: tx,
        reload_tx,
        conn_pool: pool,
        cancel_token: cancel_token.clone(),
    });
    let app = routes::create_router().with_state(shared_state);

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("Unable to bind to TCP port {}", port))?;

    tracing::info!("Listening on http://127.0.0.1:{}", port);

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
    use crate::test::TestBuilder;
    use anyhow::Result;

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
}
