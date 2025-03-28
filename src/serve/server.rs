use crate::serve::{fetcher::FetchManagerCommand, routes};
use anyhow::{Context, Result};
use std::{fs, path::PathBuf};
use tokio::net::UnixListener;
use tokio::signal;
use tokio::sync::mpsc;

/// Parent function for the server threads.
pub async fn server(_tx: mpsc::Sender<FetchManagerCommand>) -> Result<()> {
    let app = routes::create_router();

    let socket_path = PathBuf::from("kiki.sock");
    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path).with_context(|| {
            format!(
                "Unable to delete existing socket file from {:?}",
                &socket_path
            )
        })?;
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("Unable to bind to Unix socket at {:?}", &socket_path))?;

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(socket_path.clone()))
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
