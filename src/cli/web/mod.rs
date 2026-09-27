use crate::cli::serve::ServeArgs;
use crate::routes::v1::health::HealthResponse;
use anyhow::{anyhow, bail, Context, Result};
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use clap::Args;
use quick_xml::escape::escape;
use std::net::SocketAddr;
use std::path::Path;
use std::process::ExitStatus;
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::signal;
use tokio_util::sync::CancellationToken;

/// The page served at `/`. Its `{{content}}` placeholder is filled in per
/// request; see [`index`].
const INDEX_HTML: &str = include_str!("index.html");

/// Base URL for requests to the Kiki API. The client sends every request
/// over the server's Unix socket, so the host is never resolved and only
/// fills the `Host` header.
const API_BASE: &str = "http://kiki";

/// Arguments for the `kiki web` subcommand.
///
/// `kiki web` runs two things side by side: the Kiki server, listening on
/// its Unix socket exactly as `kiki serve` would, and a small HTTP server
/// for the web UI. The Kiki server runs as a separate `kiki serve` child
/// process rather than in this one: its sandbox applies to the whole
/// process and denies `execve`, and the web UI should neither live under
/// that profile nor loosen it.
///
/// The browser never talks to the Kiki API. The web UI is the API's only
/// client: it calls the server over the Unix socket and renders what it
/// gets back, so the socket stays the API's only way in.
#[derive(Args)]
pub struct WebArgs {
    /// Address the web UI listens on
    #[arg(
        short = 'l',
        long = "listen",
        value_name = "ADDR",
        default_value = "127.0.0.1:8080"
    )]
    listen: SocketAddr,

    #[command(flatten)]
    serve: ServeArgs,
}

impl WebArgs {
    /// Run the `web` subcommand.
    ///
    /// Returns once the web UI and the Kiki server have both shut down,
    /// either because this process was asked to stop (Ctrl+C or
    /// `SIGTERM`) or because the Kiki server exited on its own.
    ///
    /// # Errors
    ///
    /// Returns an error if the Kiki server cannot be started or exits
    /// unsuccessfully, or if the web UI cannot bind to its address.
    pub fn run(&self) -> Result<()> {
        tracing_subscriber::fmt::init();

        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(self.run_async())
    }

    async fn run_async(&self) -> Result<()> {
        // Resolve the socket here and hand it to the child, rather than
        // letting each process resolve it on its own, so the web UI is
        // certain to connect to the server it started.
        let socket_path = self.serve.socket_path()?;
        let api = api_client(&socket_path)?;

        let listener = TcpListener::bind(self.listen)
            .await
            .with_context(|| format!("unable to bind the web UI to {}", self.listen))?;
        tracing::info!("web UI listening on http://{}", listener.local_addr()?);

        let mut server = self.spawn_server(&socket_path)?;

        let cancel = CancellationToken::new();
        let web = tokio::spawn(serve_ui(listener, api, cancel.clone()));

        let status = tokio::select! {
            status = server.wait() => {
                let status = status.context("failed to wait on the Kiki server")?;
                tracing::warn!(%status, "Kiki server exited; stopping the web UI");
                Some(status)
            }
            _ = shutdown_signal() => None,
        };

        cancel.cancel();
        let status = match status {
            Some(status) => status,
            None => stop_server(&mut server).await?,
        };
        web.await.map_err(|_| anyhow!("panic in web UI task"))??;

        if !status.success() {
            bail!("Kiki server exited with {status}");
        }
        Ok(())
    }

    /// Start `kiki serve` as a child process listening on `socket_path`,
    /// passing on the server flags this command was given.
    ///
    /// The child gets its own process group, so a Ctrl+C at the terminal
    /// reaches only this process, which then stops the server itself. That
    /// keeps shutdown in one order no matter how it was asked for.
    fn spawn_server(&self, socket_path: &Path) -> Result<Child> {
        let exe = std::env::current_exe().context("locating the kiki executable")?;
        let child = Command::new(exe)
            .arg("serve")
            .args(self.serve.to_argv(socket_path))
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .context("failed to start the Kiki server")?;
        tracing::info!(pid = child.id(), "started Kiki server");
        Ok(child)
    }
}

/// Build a client that sends every request to the Kiki API over the Unix
/// socket at `socket_path`.
fn api_client(socket_path: &Path) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .unix_socket(socket_path)
        .build()
        .context("failed to build the Kiki API client")
}

/// Serve the web UI on `listener` until `cancel` fires, talking to the Kiki
/// API through `api`.
async fn serve_ui(
    listener: TcpListener,
    api: reqwest::Client,
    cancel: CancellationToken,
) -> Result<()> {
    let app = Router::new().route("/", get(index)).with_state(api);
    axum::serve(listener, app)
        .with_graceful_shutdown(cancel.cancelled_owned())
        .await
        .context("error encountered while running the web UI")
}

/// Render the index page from the Kiki server's health report.
///
/// If the server cannot be reached — it may still be starting — the page
/// says so and the response is a 502, rather than an error the browser
/// renders on its own.
async fn index(State(api): State<reqwest::Client>) -> Response {
    let (status, content) = match fetch_health(&api).await {
        Ok(health) => (
            StatusCode::OK,
            format!(
                "<p>Server status: {}</p>\n  <p>{} feeds, {} entries</p>",
                escape(health.status.as_str()),
                health.feed_count,
                health.entry_count,
            ),
        ),
        Err(e) => {
            tracing::warn!("failed to reach the Kiki server: {e:#}");
            (
                StatusCode::BAD_GATEWAY,
                "<p>The Kiki server is unavailable.</p>".to_owned(),
            )
        }
    };
    (status, Html(INDEX_HTML.replace("{{content}}", &content))).into_response()
}

/// Fetch `/v1/health` from the Kiki API.
async fn fetch_health(api: &reqwest::Client) -> Result<HealthResponse> {
    Ok(api
        .get(format!("{API_BASE}/v1/health"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// Ask the Kiki server to shut down gracefully with `SIGTERM`, and wait
/// for it to exit.
async fn stop_server(server: &mut Child) -> Result<ExitStatus> {
    if let Some(pid) = server.id() {
        let pid = libc::pid_t::try_from(pid).context("Kiki server pid out of range")?;
        // SAFETY: kill(2) has no memory-safety preconditions. The child has
        // not been reaped yet (`id()` returned `Some`), so the pid still
        // names it and cannot have been reused.
        if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
            tracing::warn!(
                error = %std::io::Error::last_os_error(),
                "failed to signal the Kiki server"
            );
        }
    }
    server
        .wait()
        .await
        .context("failed to wait on the Kiki server")
}

/// Resolve on Ctrl+C or `SIGTERM`.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = signal::ctrl_c().await {
            tracing::error!("failed to install Ctrl+C handler: {e:?}");
            std::future::pending::<()>().await;
        }
    };

    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::error!("failed to install signal handler: {e:?}");
                std::future::pending::<()>().await;
            }
        }
    };

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::test::TestBuilder;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        web: WebArgs,
    }

    fn parse(argv: &[&str]) -> WebArgs {
        TestCli::parse_from(std::iter::once("kiki").chain(argv.iter().copied())).web
    }

    #[test]
    fn the_web_ui_listens_on_localhost_by_default() {
        assert_eq!(parse(&[]).listen, "127.0.0.1:8080".parse().unwrap());
    }

    /// Server flags are accepted alongside the web UI's own, and reach the
    /// `kiki serve` child.
    #[test]
    fn server_flags_are_passed_through() {
        let args = parse(&["--listen", "127.0.0.1:9000", "--no-sandbox"]);
        assert_eq!(args.listen, "127.0.0.1:9000".parse().unwrap());
        assert_eq!(
            args.serve.to_argv(Path::new("/tmp/k.sock")),
            ["--uds", "/tmp/k.sock", "--no-sandbox"]
        );
    }

    /// Serve the web UI on an ephemeral port with `api` as its API client,
    /// and fetch the index page from it.
    async fn get_index(api: reqwest::Client) -> Result<(StatusCode, String)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let cancel = CancellationToken::new();
        let task = tokio::spawn(serve_ui(listener, api, cancel.clone()));

        let resp = reqwest::get(format!("http://{addr}/")).await?;
        let status = resp.status();
        let body = resp.text().await?;

        cancel.cancel();
        task.await??;
        Ok((status, body))
    }

    /// The index page is rendered from what the Kiki server reports over
    /// its socket.
    #[tokio::test]
    async fn the_index_page_shows_the_server_health() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let (status, body) = get_index(tc.client()?).await?;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("0 feeds, 0 entries"), "{body}");
        assert!(!body.contains("{{content}}"), "{body}");
        Ok(())
    }

    #[tokio::test]
    async fn an_unreachable_server_is_reported_on_the_page() -> Result<()> {
        let td = tempdir::TempDir::new("kiki_")?;
        let api = api_client(&td.path().join("missing.sock"))?;
        let (status, body) = get_index(api).await?;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(body.contains("unavailable"), "{body}");
        Ok(())
    }
}
