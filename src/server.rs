use crate::{
    fetcher::{self, FetchManagerCommand},
    routes,
};
use anyhow::{Context, Error, Result};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::OpenFlags;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{atomic, Arc},
};
use tokio::{net::UnixListener, signal, sync::mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{span, Level};

pub struct SharedAppState {
    /// An [`mpsc::Sender`] instance that may be used to send commands
    /// to workers threads used to fetch and process feeds.
    pub fetcher_tx: mpsc::Sender<FetchManagerCommand>,

    /// A [`r2d2::Pool`] instance that intermediates connections to the
    /// SQLite database.
    pub conn_pool: r2d2::Pool<SqliteConnectionManager>,
}

pub type AppState = Arc<SharedAppState>;

pub struct ServerBuilder<'a> {
    db_path: &'a Path,
    socket_path: Option<&'a Path>,
}

impl<'a> ServerBuilder<'a> {
    pub fn new(db_path: &'a Path) -> Self {
        ServerBuilder {
            db_path,
            socket_path: None,
        }
    }

    pub fn socket_path(mut self, p: &'a Path) -> Self {
        self.socket_path = Some(p);
        self
    }

    pub fn build(&self) -> Server {
        let db_path = PathBuf::from(self.db_path);
        let socket_path = match self.socket_path {
            Some(p) => PathBuf::from(p),
            None => PathBuf::from("kiki.sock"),
        };

        Server {
            db_path,
            socket_path,
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
    db_path: PathBuf,
    socket_path: PathBuf,
    cancel_token: CancellationToken,
}

impl Server {
    pub fn run(self) -> Result<()> {
        // Create a pool of connections that can be shared between all of
        // the threads that we spawn.
        let manager = SqliteConnectionManager::file(&self.db_path)
            .with_flags(OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .with_init(|c| c.execute_batch("PRAGMA foreign_keys=ON;"));
        let pool = r2d2::Pool::new(manager).with_context(|| {
            format!(
                "Unable to open connection pool to database at {:?}",
                &self.db_path
            )
        })?;

        // We create two separate runtimes, one for the feed-fetchers and
        // one for the web service workers.
        //
        // This ensures that feed fetcher threads don't consume all of
        // the resources being used by the server threads.
        let fetcher_runtime = tokio::runtime::Builder::new_multi_thread()
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

        // Create a channel so that web service workers can send tasks
        // to the feed fetchers
        let (tx, rx) = mpsc::channel(1024);

        // Create Unix socket for the server listener
        if self.socket_path.exists() {
            fs::remove_file(&self.socket_path).with_context(|| {
                format!(
                    "Unable to delete existing socket file from {:?}",
                    &self.socket_path
                )
            })?;
        }

        fetcher_runtime.spawn(fetcher::manager(
            rx,
            pool.clone(),
            self.cancel_token.clone(),
        ));
        web_runtime.spawn(server(
            self.socket_path,
            tx.clone(),
            pool.clone(),
            self.cancel_token.clone(),
        ));
        let cancel_task = web_runtime.spawn(shutdown_signal(self.cancel_token.clone()));

        web_runtime.block_on(cancel_task)?;

        Ok(())
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }
}

async fn shutdown_signal(token: CancellationToken) {
    let handler = || {
        token.cancel();
    };

    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => { handler(); }
        _ = terminate => { handler(); }
        _ = token.cancelled() => {}
    }
}

/// Parent function for the web worker threads.
async fn server(
    socket_path: PathBuf,
    tx: mpsc::Sender<FetchManagerCommand>,
    pool: r2d2::Pool<SqliteConnectionManager>,
    cancel_token: CancellationToken,
) -> Result<()> {
    let shared_state = Arc::new(SharedAppState {
        fetcher_tx: tx,
        conn_pool: pool,
    });
    let app = routes::create_router().with_state(shared_state);

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("Unable to bind to Unix socket at {:?}", &socket_path))?;

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
mod test {
    use crate::test::TestBuilder;
    use anyhow::Result;

    /// Ensure that we can start and stop the server without a panic.
    #[test]
    fn test_start_stop_server() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        tc.server_token.unwrap().cancel();
        tc.server_handle
            .unwrap()
            .join()
            .expect("panic in server thread")?;

        Ok(())
    }
}
