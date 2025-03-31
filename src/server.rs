use crate::{
    fetcher::{self, FetchManagerCommand},
    routes,
};
use anyhow::{Context, Result};
use axum::Router;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::OpenFlags;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{atomic, Arc},
    thread::{self, JoinHandle},
    time::Duration,
};
use tokio::net::UnixListener;
use tokio::signal;
use tokio::sync::mpsc;
use tower_http::{timeout::TimeoutLayer, trace::TraceLayer};
use tracing::{span, Level};

pub struct SharedAppState {
    pub fetcher_tx: mpsc::Sender<FetchManagerCommand>,
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
        }
    }
}

pub struct Server {
    db_path: PathBuf,
    socket_path: PathBuf,
}

impl Server {
    pub fn run(self) -> Result<JoinHandle<Result<()>>> {
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
            .with_context(|| "Failed to build Tokio runtime for feed fetchers")?;
        let server_runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name_fn(|| {
                static ATOMIC_ID: atomic::AtomicUsize = atomic::AtomicUsize::new(0);
                let id = ATOMIC_ID.fetch_add(1, atomic::Ordering::SeqCst);
                format!("server-worker-{}", id)
            })
            .build()
            .with_context(|| "Failed to build Tokio runtime for web service workers")?;

        // Create a channel so that web service workers can send tasks
        // to the feed fetchers
        let (tx, rx) = mpsc::channel(1024);

        // Create Unix socket for the server listener
        if self.socket_path.exists() {
            let _ = fs::remove_file(&self.socket_path).with_context(|| {
                format!(
                    "Unable to delete existing socket file from {:?}",
                    &self.socket_path
                )
            })?;
        }

        let handle = thread::spawn(move || {
            fetcher_runtime.spawn(fetcher::manager(rx, pool.clone()));
            server_runtime
                .block_on(async { server(&self.socket_path, tx.clone(), pool.clone()).await })
                .with_context(|| "Failed to spawn server tasks")?;
            Ok(())
        });

        Ok(handle)
    }
}

/// Parent function for the web worker threads.
async fn server(
    socket_path: &Path,
    tx: mpsc::Sender<FetchManagerCommand>,
    pool: r2d2::Pool<SqliteConnectionManager>,
) -> Result<()> {
    let shared_state = Arc::new(SharedAppState {
        fetcher_tx: tx,
        conn_pool: pool,
    });
    let app = Router::new()
        .nest("/feeds", routes::feeds::create_router())
        .with_state(shared_state)
        .layer((
            TraceLayer::new_for_http(),
            TimeoutLayer::new(Duration::from_secs(10)),
        ));

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("Unable to bind to Unix socket at {:?}", socket_path))?;

    span!(Level::TRACE, "web-worker");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(socket_path.into()))
        .await
        .with_context(|| "Error encountered while running server")
}

async fn shutdown_signal(socket_path: PathBuf) {
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

    let handler = || {
        let _ = fs::remove_file(socket_path);
    };

    tokio::select! {
        _ = ctrl_c => { handler(); },
        _ = terminate => { handler(); },
    }
}
