use crate::cli::serve::ServeArgs;
use crate::routes::v1::entries::{ListEntriesResponse, ListEntriesResponseEntry};
use anyhow::{anyhow, bail, Context, Result};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use clap::Args;
use quick_xml::escape::escape;
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::Path;
use std::process::ExitStatus;
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::signal;
use tokio_util::sync::CancellationToken;

/// The page served at `/`. Its `{{content}}` placeholder is filled in per
/// request, and `{{version}}` with Kiki's version; see [`index`].
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

/// Number of entries shown on each page of the index.
const PAGE_SIZE: u32 = 25;

/// Query parameters accepted by the index page.
#[derive(Deserialize)]
struct IndexParams {
    /// The page of entries to show, counting from 1 (default: 1).
    page: Option<u32>,
}

/// Render the index page: the total number of entries, and one page of
/// them, newest first, with links to the neighbouring pages.
///
/// If the server cannot be reached — it may still be starting — the page
/// says so and the response is a 502, rather than an error the browser
/// renders on its own.
async fn index(State(api): State<reqwest::Client>, Query(params): Query<IndexParams>) -> Response {
    let page = params.page.unwrap_or(1).max(1);
    let (status, content) = match fetch_entries(&api, page).await {
        Ok(entries) => (StatusCode::OK, render_entries(&entries, page)),
        Err(e) => {
            tracing::warn!("failed to reach the Kiki server: {e:#}");
            (
                StatusCode::BAD_GATEWAY,
                "<p>The Kiki server is unavailable.</p>".to_owned(),
            )
        }
    };
    let html = INDEX_HTML
        .replace("{{version}}", env!("CARGO_PKG_VERSION"))
        .replace("{{content}}", &content);
    (status, Html(html)).into_response()
}

/// Fetch page `page` (counting from 1) of `/v1/entries` from the Kiki API.
async fn fetch_entries(api: &reqwest::Client, page: u32) -> Result<ListEntriesResponse> {
    let offset = u64::from(page - 1) * u64::from(PAGE_SIZE);
    Ok(api
        .get(format!(
            "{API_BASE}/v1/entries?offset={offset}&limit={PAGE_SIZE}"
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// Render the entry count, the entries on page `page`, and the page links.
fn render_entries(resp: &ListEntriesResponse, page: u32) -> String {
    let mut html = format!(
        "<p class=\"count\">{} {}</p>\n",
        resp.count,
        if resp.count == 1 { "entry" } else { "entries" }
    );

    if resp.entries.is_empty() {
        html.push_str(if resp.count == 0 {
            "<p>No entries yet.</p>\n"
        } else {
            "<p>No entries on this page.</p>\n"
        });
    } else {
        html.push_str("<ol class=\"entries\">\n");
        for entry in &resp.entries {
            html.push_str("<li>");
            html.push_str(&render_entry(entry));
            html.push_str("</li>\n");
        }
        html.push_str("</ol>\n");
    }

    html.push_str(&render_pagination(resp.count, page));
    html
}

/// Render a single entry: its title, linked to the entry when its URL is
/// safe to link to, and its publication date.
fn render_entry(entry: &ListEntriesResponseEntry) -> String {
    let title = if entry.title.trim().is_empty() {
        "(untitled)"
    } else {
        entry.title.as_str()
    };
    let title = match safe_link(&entry.url) {
        Some(url) => format!("<a href=\"{}\">{}</a>", escape(url), escape(title)),
        None => escape(title).into_owned(),
    };

    // Show just the date; the API reports times in RFC 3339.
    let date = entry
        .published_at
        .as_deref()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| {
            format!(
                " <time datetime=\"{}\">{}</time>",
                t.to_rfc3339(),
                t.format("%Y-%m-%d")
            )
        })
        .unwrap_or_default();

    format!("{title}{date}")
}

/// Return `url` if it is an `http` or `https` URL.
///
/// Entry URLs come from the feeds, so they are untrusted. Linking to
/// anything else — a `javascript:` URL in particular — would let a feed run
/// script in the web UI.
fn safe_link(url: &str) -> Option<&str> {
    let parsed = url::Url::parse(url).ok()?;
    matches!(parsed.scheme(), "http" | "https").then_some(url)
}

/// Render the "page X of Y" line with links to the newer and older pages.
fn render_pagination(count: usize, page: u32) -> String {
    let pages = count.div_ceil(PAGE_SIZE as usize).max(1);
    let page_usize = page as usize;

    let mut links = Vec::new();
    if page > 1 {
        // A page past the end links back to the last page, not to the
        // (equally empty) page before it.
        let prev = page_usize.min(pages + 1) - 1;
        links.push(format!(
            "<a href=\"/?page={prev}\" rel=\"prev\">&larr; Newer</a>"
        ));
    }
    links.push(format!("Page {page} of {pages}"));
    if page_usize < pages {
        links.push(format!(
            "<a href=\"/?page={}\" rel=\"next\">Older &rarr;</a>",
            page_usize + 1
        ));
    }
    format!(
        "<nav class=\"pagination\">{}</nav>",
        links.join(" &middot; ")
    )
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
    /// and fetch `path` from it.
    async fn get_page(api: reqwest::Client, path: &str) -> Result<(StatusCode, String)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let cancel = CancellationToken::new();
        let task = tokio::spawn(serve_ui(listener, api, cancel.clone()));

        let resp = reqwest::get(format!("http://{addr}{path}")).await?;
        let status = resp.status();
        let body = resp.text().await?;

        cancel.cancel();
        task.await??;
        Ok((status, body))
    }

    async fn get_index(api: reqwest::Client) -> Result<(StatusCode, String)> {
        get_page(api, "/").await
    }

    /// Insert `n` entries titled "Entry 1" through "Entry n", each
    /// published a day after the one before.
    fn insert_entries(tc: &crate::test::TestConfig, n: i64) -> Result<()> {
        let conn = tc.database_conn()?;
        for i in 1..=n {
            conn.execute(
                "INSERT INTO entries (syndication_format, guid, published_at, title, url)
                 VALUES ('rss', ?1, ?2, ?3, ?4)",
                rusqlite::params![
                    format!("guid-{i}"),
                    1_700_000_000 + i * 86_400,
                    format!("Entry {i}"),
                    format!("http://example.com/{i}"),
                ],
            )?;
        }
        Ok(())
    }

    /// Titles of the entries listed on `body`, in order.
    fn listed_titles(body: &str) -> Vec<String> {
        let re = regex::Regex::new(r#"<li><a href="[^"]*">([^<]*)</a>"#).unwrap();
        re.captures_iter(body).map(|c| c[1].to_owned()).collect()
    }

    #[tokio::test]
    async fn the_index_page_says_when_there_are_no_entries() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let (status, body) = get_index(tc.client()?).await?;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("0 entries"), "{body}");
        assert!(body.contains("No entries yet."), "{body}");
        assert!(body.contains("Page 1 of 1"), "{body}");
        assert!(!body.contains("{{content}}"), "{body}");
        Ok(())
    }

    /// The header shows Kiki's version.
    #[tokio::test]
    async fn the_index_page_shows_the_version() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let (_, body) = get_index(tc.client()?).await?;

        let version = format!(
            r#"<small class="version">v{}</small>"#,
            env!("CARGO_PKG_VERSION")
        );
        assert!(body.contains(&version), "{body}");
        assert!(!body.contains("{{version}}"), "{body}");
        Ok(())
    }

    /// The index lists the newest entries first, with the total count, and
    /// pages through the rest.
    #[tokio::test]
    async fn the_index_page_lists_entries_newest_first() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        insert_entries(&tc, 30)?;

        let (status, body) = get_index(tc.client()?).await?;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("30 entries"), "{body}");
        assert!(body.contains("Page 1 of 2"), "{body}");
        assert!(body.contains(r#"href="/?page=2""#), "{body}");
        assert!(!body.contains(r#"rel="prev""#), "{body}");
        let expected: Vec<_> = (6..=30).rev().map(|i| format!("Entry {i}")).collect();
        assert_eq!(listed_titles(&body), expected);

        let (status, body) = get_page(tc.client()?, "/?page=2").await?;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Page 2 of 2"), "{body}");
        assert!(body.contains(r#"href="/?page=1""#), "{body}");
        assert!(!body.contains(r#"rel="next""#), "{body}");
        let expected: Vec<_> = (1..=5).rev().map(|i| format!("Entry {i}")).collect();
        assert_eq!(listed_titles(&body), expected);
        Ok(())
    }

    #[tokio::test]
    async fn a_page_past_the_end_links_back_to_the_last_page() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        insert_entries(&tc, 3)?;

        let (status, body) = get_page(tc.client()?, "/?page=9").await?;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("No entries on this page."), "{body}");
        assert!(body.contains(r#"href="/?page=1" rel="prev""#), "{body}");
        Ok(())
    }

    /// Entry titles and URLs come from feeds, so they are escaped, and
    /// only `http(s)` URLs are linked.
    #[tokio::test]
    async fn entries_are_rendered_safely() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let conn = tc.database_conn()?;
        conn.execute(
            "INSERT INTO entries (syndication_format, guid, published_at, title, url)
             VALUES ('rss', 'a', 1, '<script>alert(1)</script>', 'javascript:alert(1)'),
                    ('rss', 'b', 2, 'Quotes', 'http://example.com/?a=\"><b>')",
            [],
        )?;

        let (_, body) = get_index(tc.client()?).await?;
        assert!(!body.contains("<script>alert"), "{body}");
        assert!(
            body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "{body}"
        );
        assert!(!body.contains("javascript:"), "{body}");
        assert!(!body.contains(r#""><b>"#), "{body}");
        Ok(())
    }

    #[tokio::test]
    async fn an_unreachable_server_is_reported_on_the_page() -> Result<()> {
        let td = tempfile::TempDir::with_prefix("kiki_")?;
        let api = api_client(&td.path().join("missing.sock"))?;
        let (status, body) = get_index(api).await?;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(body.contains("unavailable"), "{body}");
        Ok(())
    }
}
