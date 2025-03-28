use crate::http::USER_AGENT;
use anyhow::{Context, Result};
use axum::{http::StatusCode, routing::get, Router};
use clap::Args;
use rss::Channel;
use std::{fs, path::PathBuf, thread, time::Duration};
use tokio::net::UnixListener;
use tokio::signal;
use tower_http::{timeout::TimeoutLayer, trace::TraceLayer};

#[derive(Args)]
pub struct ServeArgs {}

impl ServeArgs {
    pub fn run(&self) -> Result<()> {
        tracing_subscriber::fmt::init();
        let fetcher_handle = thread::Builder::new()
            .name("fetcher".to_string())
            .spawn(|| fetcher())
            .with_context(|| "Failed to spawn fetcher thread")?;
        let server_handle = thread::Builder::new()
            .name("server".to_string())
            .spawn(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(server())
            })
            .with_context(|| "Failed to spawn server thread")?;

        let _ = fetcher_handle.join().unwrap();
        let _ = server_handle.join().unwrap();

        Ok(())
    }
}

/// Parent function for the fetcher threads.
fn fetcher() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
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
        })
}

/// Parent function for the server threads.
async fn server() -> Result<()> {
    let app = Router::new().route("/", get(root)).layer((
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
