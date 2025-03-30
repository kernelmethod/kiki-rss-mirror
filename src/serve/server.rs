use crate::{routes, serve::fetcher::FetchManagerCommand};
use anyhow::{Context, Result};
use axum::Router;
use r2d2_sqlite::SqliteConnectionManager;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
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

/// Parent function for the server threads.
pub async fn server(
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
        .with_context(|| format!("Unable to bind to Unix socket at {:?}", &socket_path))?;

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
