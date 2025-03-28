use crate::serve::fetcher::FetchManagerCommand;
use anyhow::{Context, Result};
use axum::{
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use std::{fs, path::PathBuf, time::Duration};
use tokio::net::UnixListener;
use tokio::signal;
use tokio::sync::mpsc;
use tower_http::{timeout::TimeoutLayer, trace::TraceLayer};

/// Parent function for the server threads.
pub async fn server(_tx: mpsc::Sender<FetchManagerCommand>) -> Result<()> {
    let app = Router::new()
        .route("/", get(root))
        .route("/feed", post(add_feed))
        .layer((
            TraceLayer::new_for_http(),
            TimeoutLayer::new(Duration::from_secs(10)),
        ));

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

async fn root() -> (StatusCode, &'static str) {
    (StatusCode::NOT_FOUND, "Page not found")
}

#[derive(serde::Serialize)]
struct AddFeedResult {}

#[axum::debug_handler]
async fn add_feed() -> (StatusCode, Json<AddFeedResult>) {
    let result = AddFeedResult {};

    (StatusCode::CREATED, Json(result))
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
