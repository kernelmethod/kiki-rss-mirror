use crate::http::USER_AGENT;
use axum::{
    routing::get,
    http::StatusCode,
    Router
};
use anyhow::{Context, Result};
use clap::Args;
use rss::Channel;
use std::{fs, path::PathBuf};
use std::time::Duration;
use tower_http::{timeout::TimeoutLayer, trace::TraceLayer};
use tokio::net::UnixListener;
use tokio::signal;

#[derive(Args)]
pub struct ServerArgs {}

impl ServerArgs {
    pub fn run(&self) -> Result<()> {
        tracing_subscriber::fmt::init();
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(self.server())
    }

    async fn server(&self) -> Result<()> {
        let app = Router::new()
            .route("/", get(root))
            .layer((
                TraceLayer::new_for_http(),
                TimeoutLayer::new(Duration::from_secs(10))
            ));

        let socket_path = PathBuf::from("kiki.sock");
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path)
                .with_context(|| format!("Unable to delete existing socket file from {:?}", &socket_path))?;
        }

        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("Unable to bind to Unix socket at {:?}", &socket_path))?;

        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal(socket_path.clone()))
            .await
            .with_context(|| "Error encountered while running server")
    }
}

async fn root() -> (StatusCode, &'static str) {
    (StatusCode::NOT_FOUND, "Page not found")
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

pub async fn server() -> Result<()> {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://kernelmethod.org/notes/index.xml")
        .header("User-Agent", USER_AGENT)
        .send()
        .await?;

    let content = resp.bytes().await?;
    let chan = Channel::read_from(&content[..])?;

    println!("{chan:#?}");
    Ok(())
}
