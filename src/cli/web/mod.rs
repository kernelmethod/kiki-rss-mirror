mod sanitize;
mod settings;

use crate::cli::serve::ServeArgs;
use crate::db::tags::{SystemTag, TagKind, SYSTEM_TAG_PREFIX};
use crate::plugins::settings::{Setting, SettingType};
use crate::routes::v1::entries::entry_assets::ListEntryAssetsResponse;
use crate::routes::v1::entries::entry_tags::GetEntryTagsResponse;
use crate::routes::v1::entries::get_entry::GetEntryResponse;
use crate::routes::v1::entries::search_entries::SearchEntriesResponse;
use crate::routes::v1::entries::ListEntriesResponseEntry;
use crate::routes::v1::feeds::list_feeds::ListFeedsResponse;
use crate::routes::v1::plugins::list_plugins::{ListPluginsResponse, PluginResponse};
use crate::routes::v1::plugins::plugin_config::PluginConfigResponse;
use crate::routes::v1::tags::list_tags::{ListTagsResponse, TagResponse};
use crate::routes::v1::tags::tag_entries::AddTagEntriesRequest;
use crate::sandbox::{self, SandboxConfig};
use anyhow::{anyhow, bail, Context, Result};
use axum::{
    extract::{Form, Path as UrlPath, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post, put},
    Router,
};
use clap::Args;
use quick_xml::escape::escape;
use regex::Regex;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::ExitStatus;
use std::sync::LazyLock;
use tokio::net::TcpListener;
use tokio::signal;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// The layout every page is rendered into; see [`render_page`].
const PAGE_HTML: &str = include_str!("page.html");

/// Matches the `{{name}}` placeholders in [`PAGE_HTML`].
static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used)]
    Regex::new(r"\{\{(\w+)\}\}").expect("placeholder regex is valid")
});

/// The script every page runs, inlined into [`PAGE_HTML`]. It powers the
/// save buttons shown with each entry, the filter menus on lists of entries,
/// the search box and its popup on narrow screens, and the buttons that mark
/// entries as read or delete a tag.
const PAGE_JS: &str = include_str!("page.js");

/// The `script-src` directive that lets pages run [`PAGE_JS`] and nothing
/// else: the script is allowed by its SHA-256 hash, so neither inline
/// script that slips through from a feed, nor a script served from the
/// asset cache, can run.
static SCRIPT_SRC: LazyLock<String> = LazyLock::new(|| {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(PAGE_JS));
    format!("script-src 'sha256-{hash}'")
});

/// `Content-Security-Policy` sent with every page.
///
/// Pages carry titles and content from feeds. They are escaped or
/// sanitized, but as a second line of defence the pages may not run any
/// script but [`PAGE_JS`], connect anywhere but the web UI itself, load
/// anything but images from the web UI's own asset cache, or submit forms.
static CONTENT_SECURITY_POLICY: LazyLock<String> = LazyLock::new(|| {
    format!(
        "default-src 'none'; img-src 'self'; style-src 'unsafe-inline'; {}; \
         connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        *SCRIPT_SRC
    )
});

/// `Content-Security-Policy` sent with pages that hold forms, such as a
/// plugin's config page. It is [`CONTENT_SECURITY_POLICY`], except that
/// forms may be submitted to the web UI itself. These pages show nothing
/// from feeds.
static FORM_CONTENT_SECURITY_POLICY: LazyLock<String> = LazyLock::new(|| {
    format!(
        "default-src 'none'; img-src 'self'; style-src 'unsafe-inline'; {}; \
         connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        *SCRIPT_SRC
    )
});

/// `Content-Security-Policy` sent with cached assets. They were downloaded
/// from feeds, and are served from the web UI's origin, so one opened on
/// its own — an SVG, say — must not be able to run script there either.
const ASSET_CONTENT_SECURITY_POLICY: &str = "default-src 'none'; img-src 'self'; \
    style-src 'unsafe-inline'; sandbox";

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
/// that profile nor loosen it. The web UI gets a sandbox of its own, which
/// grants it no filesystem access and no network access but its listener
/// and the server's socket; `--no-sandbox` lifts both.
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
    /// unsuccessfully, if the web UI cannot bind to its address, or if its
    /// sandbox cannot be installed.
    pub fn run(&self) -> Result<()> {
        tracing_subscriber::fmt::init();

        // Everything that needs the filesystem, a new listening socket, or
        // `execve` happens here, before the sandbox forbids it. That has to
        // be before the tokio runtime too: Landlock restricts only the
        // thread that installs it and the threads that thread starts later,
        // and a multi-threaded runtime starts its workers as it is built.

        // Resolve the socket here and hand it to the child, rather than
        // letting each process resolve it on its own, so the web UI is
        // certain to connect to the server it started.
        let socket_path = self.serve.socket_path()?;
        let api = api_client(&socket_path)?;

        let listener = std::net::TcpListener::bind(self.listen)
            .with_context(|| format!("unable to bind the web UI to {}", self.listen))?;
        listener.set_nonblocking(true)?;
        tracing::info!("web UI listening on http://{}", listener.local_addr()?);

        let server = self.spawn_server(&socket_path)?;

        // Counted before the sandbox hides the cgroup files that bound it.
        let workers = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);

        self.apply_sandbox()?;

        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .enable_all()
            .build()?
            .block_on(run_async(listener, api, server))
    }

    /// Install the web UI's sandbox, unless `--no-sandbox` was given.
    ///
    /// The web UI has no use for the filesystem, and none for the network
    /// beyond the listener it already has and the server's Unix socket, so
    /// the [`SandboxProfile::WebUi`](crate::sandbox::SandboxProfile::WebUi)
    /// profile denies it both. It must go up after the server has been
    /// spawned, since it denies `execve`; the server is outside it, and
    /// installs its own.
    fn apply_sandbox(&self) -> Result<()> {
        if self.serve.no_sandbox() {
            tracing::warn!(
                "sandbox disabled via --no-sandbox; the web UI runs with full filesystem \
                 and syscall access"
            );
            return Ok(());
        }
        let config = SandboxConfig::web_ui(self.serve.seccomp_log_only());
        sandbox::apply(&config).context("failed to install the web UI sandbox")
    }

    /// Start `kiki serve` as a child process listening on `socket_path`,
    /// passing on the server flags this command was given.
    ///
    /// The child gets its own process group, so a Ctrl+C at the terminal
    /// reaches only this process, which then stops the server itself. That
    /// keeps shutdown in one order no matter how it was asked for.
    fn spawn_server(&self, socket_path: &Path) -> Result<ServerProcess> {
        let exe = std::env::current_exe().context("locating the kiki executable")?;
        let child = std::process::Command::new(exe)
            .arg("serve")
            .args(self.serve.to_argv(socket_path))
            .process_group(0)
            .spawn()
            .context("failed to start the Kiki server")?;
        tracing::info!(pid = child.id(), "started Kiki server");
        Ok(ServerProcess(child))
    }
}

/// Serve the web UI on `listener` alongside the Kiki `server`, until one of
/// them stops.
async fn run_async(
    listener: std::net::TcpListener,
    api: reqwest::Client,
    mut server: ServerProcess,
) -> Result<()> {
    let listener = TcpListener::from_std(listener)?;
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

/// The `kiki serve` child of `kiki web`.
///
/// A plain [`std::process::Child`] rather than a tokio one, because it is
/// spawned before the sandbox goes up, and so before there is a runtime to
/// spawn it on. It is killed if dropped while still running, so an error
/// in the web UI never leaves an orphaned server behind.
struct ServerProcess(std::process::Child);

impl ServerProcess {
    /// Wait for the server to exit, without blocking the runtime.
    ///
    /// Only this handle ever reaps the child, so until this returns its pid
    /// cannot be reused, and [`stop_server`] can safely signal it.
    async fn wait(&mut self) -> Result<ExitStatus> {
        // Listen for SIGCHLD before checking, so an exit between the check
        // and the wait still wakes us.
        let mut sigchld = signal::unix::signal(signal::unix::SignalKind::child())?;
        loop {
            if let Some(status) = self.0.try_wait()? {
                return Ok(status);
            }
            if sigchld.recv().await.is_none() {
                bail!("SIGCHLD stream closed while waiting on the Kiki server");
            }
        }
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        if let Ok(None) = self.0.try_wait() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

/// Build a client that sends every request to the Kiki API over the Unix
/// socket at `socket_path`.
fn api_client(socket_path: &Path) -> Result<reqwest::Client> {
    crate::http::unix_socket_client(socket_path, None)
        .context("failed to build the Kiki API client")
}

/// Serve the web UI on `listener` until `cancel` fires, talking to the Kiki
/// API through `api`.
async fn serve_ui(
    listener: TcpListener,
    api: reqwest::Client,
    cancel: CancellationToken,
) -> Result<()> {
    let app = Router::new()
        .route("/", get(index))
        .route("/entries/{id}", get(entry_page))
        .route("/entries/read", post(mark_entries_read))
        .route("/entries/{id}/saved", put(save_entry).delete(unsave_entry))
        .route("/feeds", get(feeds_page))
        .route("/feeds/{id}", get(feed_page))
        .route("/tags", get(tags_page))
        .route("/tags/{id}", get(tag_page).delete(delete_tag))
        .route("/search", get(search_page))
        .route("/plugins", get(plugins_page))
        .route("/plugins/{name}", get(plugin_page))
        .route("/plugins/{name}/config", post(update_plugin_config))
        .route("/assets/{hash}", get(asset))
        .with_state(api);
    axum::serve(listener, app)
        .with_graceful_shutdown(cancel.cancelled_owned())
        .await
        .context("error encountered while running the web UI")
}

/// Number of entries, or feeds, shown on each page of a list.
const PAGE_SIZE: u32 = 25;

/// Query parameters accepted by the index, feed, search and entry pages.
#[derive(Deserialize)]
struct PageParams {
    /// The page of entries to show, or to link back to, counting from 1
    /// (default: 1).
    page: Option<u32>,
    /// On an entry page, the feed whose page to link back to, rather than
    /// the index.
    feed: Option<i64>,
    /// On an entry page, the tag whose page to link back to, rather than
    /// the index.
    tag: Option<i64>,
    /// Also list entries tagged `system:read`, which are left out by
    /// default; on an entry page, whether the list it links back to does.
    show_read: Option<bool>,
    /// On the search page, what to search for; on an entry page, the search
    /// whose results to link back to, rather than the index.
    q: Option<String>,
    /// How search results are sorted: `newest` first, or by best match
    /// (the default).
    sort: Option<String>,
}

impl PageParams {
    fn page(&self) -> u32 {
        self.page.unwrap_or(1).max(1)
    }

    /// The list of entries an entry page links back to.
    fn listing(&self) -> Listing {
        Listing {
            feed: self.feed,
            tag: self.tag,
            search: self.search(),
            page: self.page(),
            show_read: self.show_read(),
        }
    }

    /// The search given by the `q` and `sort` parameters, or `None` if `q`
    /// is missing or blank.
    fn search(&self) -> Option<Search> {
        let query = self.q.as_deref()?.trim();
        (!query.is_empty()).then(|| Search {
            query: query.to_owned(),
            newest: self.sort.as_deref() == Some("newest"),
        })
    }

    fn show_read(&self) -> bool {
        self.show_read.unwrap_or(false)
    }
}

/// A page of a list of entries: of the index, of a feed's page, of a tag's
/// page, or of search results. Entry pages link back to the listing they
/// were opened from.
#[derive(Clone)]
struct Listing {
    /// The feed whose entries are listed, or `None` for the index.
    feed: Option<i64>,
    /// The tag whose entries are listed, or `None` for the index.
    tag: Option<i64>,
    /// The search whose results are listed, or `None` for the index. At
    /// most one of `feed`, `tag` and `search` is set.
    search: Option<Search>,
    /// The page of the list, counting from 1.
    page: u32,
    /// Whether entries tagged `system:read` are listed. Search results
    /// always list them, whatever this says.
    show_read: bool,
}

/// A full-text search of the entries, as typed into the search box.
#[derive(Clone)]
struct Search {
    /// What was searched for, trimmed and never empty. See [`fts_query`]
    /// for how it is understood.
    query: String,
    /// Whether results are sorted newest first, rather than by best match.
    newest: bool,
}

impl Search {
    /// The query parameters that give this search, already encoded.
    fn query_params(&self) -> Vec<String> {
        let mut query = vec![format!("q={}", encode_path_segment(&self.query))];
        if self.newest {
            query.push("sort=newest".to_owned());
        }
        query
    }
}

impl Listing {
    /// The path of the list, without a page.
    fn path(&self) -> String {
        match (self.feed, self.tag, &self.search) {
            (Some(id), _, _) => format!("/feeds/{id}"),
            (None, Some(id), _) => format!("/tags/{id}"),
            (None, None, Some(_)) => "/search".to_owned(),
            (None, None, None) => "/".to_owned(),
        }
    }

    /// The URL of this page of the list, escaped for use in an attribute.
    fn href(&self) -> String {
        let page = (self.page > 1).then_some(self.page as usize);
        self.list_href(page, self.show_read)
    }

    /// The URL of page `page` of the list, escaped for use in an attribute.
    fn page_href(&self, page: usize) -> String {
        self.list_href(Some(page), self.show_read)
    }

    /// The URL of the first page of these search results, sorted newest
    /// first if `newest` or by best match if not, escaped for use in an
    /// attribute.
    fn sort_href(&self, newest: bool) -> String {
        let listing = Listing {
            search: self
                .search
                .clone()
                .map(|search| Search { newest, ..search }),
            ..self.clone()
        };
        listing.list_href(None, listing.show_read)
    }

    /// The URL of the first page of the list with read entries shown or
    /// not, the other way from this one, escaped for use in an attribute.
    fn toggle_read_href(&self) -> String {
        self.list_href(None, !self.show_read)
    }

    /// The URL of page `page` of the list, or of its first page if `None`,
    /// listing read entries if `show_read`, escaped for use in an attribute.
    fn list_href(&self, page: Option<usize>, show_read: bool) -> String {
        let mut query = Vec::new();
        if let Some(search) = &self.search {
            query.extend(search.query_params());
        }
        if let Some(page) = page {
            query.push(format!("page={page}"));
        }
        if show_read && self.search.is_none() {
            query.push("show_read=true".to_owned());
        }
        with_query(self.path(), &query)
    }

    /// The URL of entry `id`'s page, linking back to this page of the list,
    /// escaped for use in an attribute.
    fn entry_href(&self, id: i64) -> String {
        let mut query = Vec::new();
        if let Some(feed) = self.feed {
            query.push(format!("feed={feed}"));
        }
        if let Some(tag) = self.tag {
            query.push(format!("tag={tag}"));
        }
        if let Some(search) = &self.search {
            query.extend(search.query_params());
        }
        if self.page > 1 {
            query.push(format!("page={}", self.page));
        }
        if self.show_read && self.search.is_none() {
            query.push("show_read=true".to_owned());
        }
        with_query(format!("/entries/{id}"), &query)
    }
}

/// `path` with the query parameters `query` (already encoded) appended,
/// escaped for use in an attribute.
fn with_query(path: String, query: &[String]) -> String {
    if query.is_empty() {
        path
    } else {
        format!("{path}?{}", query.join("&amp;"))
    }
}

/// Fill the layout in [`PAGE_HTML`] with `title` (plain text, which is
/// escaped) and `content` (HTML), and wrap it in a response with `status`.
fn render_page(status: StatusCode, title: &str, content: &str) -> Response {
    render_page_with_csp(status, title, content, &CONTENT_SECURITY_POLICY)
}

/// [`render_page`], for a page of search results: the search box in the
/// layout is filled in with `query`, what was searched for.
fn render_search_page(status: StatusCode, title: &str, query: &str, content: &str) -> Response {
    render_layout(status, title, query, content, &CONTENT_SECURITY_POLICY)
}

/// [`render_page`], for a page with forms: the page is sent with
/// [`FORM_CONTENT_SECURITY_POLICY`], so that its forms can be submitted.
fn render_form_page(status: StatusCode, title: &str, content: &str) -> Response {
    render_page_with_csp(status, title, content, &FORM_CONTENT_SECURITY_POLICY)
}

/// [`render_page`], sent with the `Content-Security-Policy` `csp`.
fn render_page_with_csp(status: StatusCode, title: &str, content: &str, csp: &str) -> Response {
    render_layout(status, title, "", content, csp)
}

/// Fill the layout in [`PAGE_HTML`] with `title` and `query` (plain text,
/// which is escaped; `query` goes in the search box) and `content` (HTML),
/// and wrap it in a response with `status`, sent with the
/// `Content-Security-Policy` `csp`.
fn render_layout(
    status: StatusCode,
    title: &str,
    query: &str,
    content: &str,
    csp: &str,
) -> Response {
    // Fill every placeholder in one pass, so that a placeholder appearing in
    // a feed's title or content is left alone.
    let html = PLACEHOLDER.replace_all(PAGE_HTML, |caps: &regex::Captures| match &caps[1] {
        "title" => escape(title).into_owned(),
        "query" => escape(query).into_owned(),
        "version" => env!("CARGO_PKG_VERSION").to_owned(),
        "content" => content.to_owned(),
        "script" => PAGE_JS.to_owned(),
        _ => caps[0].to_owned(),
    });
    (
        status,
        [
            (header::CONTENT_SECURITY_POLICY, csp),
            // Following a link out of the reader shouldn't tell the site
            // what the reader was.
            (header::REFERRER_POLICY, "no-referrer"),
            // Nor should merely showing a link: browsers may look up the
            // hosts of links on a page before any are followed.
            (header::X_DNS_PREFETCH_CONTROL, "off"),
        ],
        Html(html.into_owned()),
    )
        .into_response()
}

/// Render the page for when the Kiki server cannot be reached — it may
/// still be starting — as a 502, rather than an error the browser renders
/// on its own.
fn server_unavailable(e: &anyhow::Error) -> Response {
    tracing::warn!("failed to reach the Kiki server: {e:#}");
    render_page(
        StatusCode::BAD_GATEWAY,
        "Kiki",
        "<p>The Kiki server is unavailable.</p>",
    )
}

/// Render the index page: the total number of entries, and one page of
/// them, newest first, each with the feed it came from, and links to the
/// neighbouring pages. Read entries are left out unless the `show_read`
/// query parameter is true.
async fn index(State(api): State<reqwest::Client>, Query(params): Query<PageParams>) -> Response {
    let listing = Listing {
        feed: None,
        tag: None,
        search: None,
        ..params.listing()
    };
    match fetch_entries(&api, &listing, None).await {
        Ok(entries) => {
            let (feeds, tags) = tokio::join!(
                fetch_feed_titles(&api, entries.entries.iter().filter_map(|e| e.feed_id)),
                fetch_entry_tags(&api, entries.entries.iter().map(|e| e.id)),
            );
            render_page(
                StatusCode::OK,
                "Kiki",
                &render_entries(entries.count, &entries.entries, &feeds, &tags, &listing),
            )
        }
        Err(e) => server_unavailable(&e),
    }
}

/// Render the page for entry `id`: a summary of the entry built from what
/// its feed says about it, with a link through to the entry itself.
///
/// The page links back to the list of entries it was opened from: the
/// index, a feed's page, or a tag's page.
async fn entry_page(
    State(api): State<reqwest::Client>,
    UrlPath(id): UrlPath<i64>,
    Query(params): Query<PageParams>,
) -> Response {
    let entry = match fetch_entry(&api, id).await {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            return render_page(
                StatusCode::NOT_FOUND,
                "Entry not found - Kiki",
                &format!(
                    "<p>Entry not found.</p>\n{}",
                    render_back_link(&params.listing())
                ),
            )
        }
        Err(e) => return server_unavailable(&e),
    };

    let feed_title = async {
        match entry.feed_id {
            Some(feed_id) => fetch_feed_titles(&api, [feed_id]).await.remove(&feed_id),
            None => None,
        }
    };
    let (feed_title, cached, mut tags) = tokio::join!(
        feed_title,
        fetch_cached_assets(&api, id),
        fetch_entry_tags(&api, [id])
    );
    let tags = tags.remove(&id).unwrap_or_default();
    render_page(
        StatusCode::OK,
        &format!("{} - Kiki", display_title(&entry.title)),
        &render_entry_page(
            &entry,
            feed_title.as_deref(),
            &tags,
            &cached,
            &params.listing(),
        ),
    )
}

/// Save entry `id`, giving it the `system:saved` tag. Called by the save
/// buttons' script; see [`set_entry_saved`].
async fn save_entry(
    State(api): State<reqwest::Client>,
    UrlPath(id): UrlPath<i64>,
    headers: HeaderMap,
) -> Response {
    set_entry_saved(&api, id, &headers, true).await
}

/// Unsave entry `id`, removing its `system:saved` tag. Called by the save
/// buttons' script; see [`set_entry_saved`].
async fn unsave_entry(
    State(api): State<reqwest::Client>,
    UrlPath(id): UrlPath<i64>,
    headers: HeaderMap,
) -> Response {
    set_entry_saved(&api, id, &headers, false).await
}

/// Add (`saved == true`) or remove the `system:saved` tag on entry `id`,
/// responding with `204 No Content` once it is done.
///
/// Responds with `404 Not Found` if there is no such entry, `502 Bad
/// Gateway` if the Kiki server cannot make the change, and `403 Forbidden`
/// to requests from other sites, going by `headers`; see
/// [`is_same_origin`].
async fn set_entry_saved(
    api: &reqwest::Client,
    id: i64,
    headers: &HeaderMap,
    saved: bool,
) -> Response {
    if !is_same_origin(headers) {
        return (
            StatusCode::FORBIDDEN,
            "Entries may only be saved from the web UI's own pages.",
        )
            .into_response();
    }

    let url = format!("{API_BASE}/v1/entries/id/{id}/system-tags/saved");
    let req = if saved { api.put(url) } else { api.delete(url) };
    match req.send().await.map(|resp| resp.status()) {
        Ok(StatusCode::OK) => StatusCode::NO_CONTENT.into_response(),
        Ok(StatusCode::NOT_FOUND) => (StatusCode::NOT_FOUND, "Entry not found.").into_response(),
        Ok(status) => {
            tracing::warn!(%status, entry_id = id, saved, "failed to update saved entry");
            (StatusCode::BAD_GATEWAY, "The entry could not be updated.").into_response()
        }
        Err(e) => {
            tracing::warn!("failed to reach the Kiki server: {e:#}");
            (StatusCode::BAD_GATEWAY, "The Kiki server is unavailable.").into_response()
        }
    }
}

/// Query parameters accepted by [`mark_entries_read`].
#[derive(Deserialize)]
struct MarkReadParams {
    /// Only mark this feed's entries as read, rather than every entry.
    feed: Option<i64>,
}

/// Mark every entry as read, or only those from the feed given in
/// `params`, by giving them the `system:read` tag in one request to the
/// Kiki API, once [`fetch_system_tag_id`] has looked up the tag. Called by the "Mark all as read" buttons' script, which
/// reloads the page afterwards.
///
/// Responds with `204 No Content` once it is done, `502 Bad Gateway` if
/// the Kiki server cannot make the change, and `403 Forbidden` to requests
/// from other sites, going by `headers`; see [`is_same_origin`].
async fn mark_entries_read(
    State(api): State<reqwest::Client>,
    Query(params): Query<MarkReadParams>,
    headers: HeaderMap,
) -> Response {
    if !is_same_origin(&headers) {
        return (
            StatusCode::FORBIDDEN,
            "Entries may only be marked as read from the web UI's own pages.",
        )
            .into_response();
    }

    let result = async {
        let tag_id = fetch_system_tag_id(&api, SystemTag::Read).await?;
        let request = AddTagEntriesRequest {
            up_to_id: None,
            feed_id: params.feed,
        };
        api.post(format!("{API_BASE}/v1/tags/id/{tag_id}/entries"))
            .json(&request)
            .send()
            .await?
            .error_for_status()?;
        anyhow::Ok(())
    }
    .await;
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::warn!(
                feed_id = params.feed,
                "failed to mark entries as read: {e:#}"
            );
            (
                StatusCode::BAD_GATEWAY,
                "The entries could not be marked as read.",
            )
                .into_response()
        }
    }
}

/// Look up the ID of the system tag `tag` through the Kiki API.
async fn fetch_system_tag_id(api: &reqwest::Client, tag: SystemTag) -> Result<i64> {
    let tags: ListTagsResponse = api
        .get(format!("{API_BASE}/v1/tags?kind=system"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    tags.tags
        .into_iter()
        .find(|t| t.name == tag.name())
        .map(|t| t.id)
        .ok_or_else(|| anyhow!("the Kiki server has no {tag} tag"))
}

/// Render the list of feeds: the total number of feeds, and one page of
/// them, each linked to its page.
async fn feeds_page(
    State(api): State<reqwest::Client>,
    Query(params): Query<PageParams>,
) -> Response {
    let page = params.page();
    match fetch_feeds(&api, page).await {
        Ok(feeds) => render_page(StatusCode::OK, "Feeds - Kiki", &render_feeds(&feeds, page)),
        Err(e) => server_unavailable(&e),
    }
}

/// Render the page for feed `id`: what the feed says about itself, and one
/// page of the entries retrieved from it, newest first. Read entries are
/// left out unless the `show_read` query parameter is true.
async fn feed_page(
    State(api): State<reqwest::Client>,
    UrlPath(id): UrlPath<i64>,
    Query(params): Query<PageParams>,
) -> Response {
    let listing = Listing {
        feed: Some(id),
        tag: None,
        search: None,
        ..params.listing()
    };
    let not_found = || {
        render_page(
            StatusCode::NOT_FOUND,
            "Feed not found - Kiki",
            "<p>Feed not found.</p>\n<p><a href=\"/feeds\">&larr; Back to feeds</a></p>\n",
        )
    };

    let (feed, entries) = tokio::join!(fetch_feed(&api, id), fetch_entries(&api, &listing, None));
    let feed = match feed {
        Ok(Some(feed)) => feed,
        Ok(None) => return not_found(),
        Err(e) => return server_unavailable(&e),
    };
    let entries = match entries {
        Ok(entries) => entries,
        Err(e) => return server_unavailable(&e),
    };

    let tags = fetch_entry_tags(&api, entries.entries.iter().map(|e| e.id)).await;
    render_page(
        StatusCode::OK,
        &format!("{} - Kiki", display_feed_title(&feed.title)),
        &render_feed_page(&feed, &entries, &tags, &listing),
    )
}

/// Render the list of tags: the total number of tags, and one page of them,
/// each linked to its page.
async fn tags_page(
    State(api): State<reqwest::Client>,
    Query(params): Query<PageParams>,
) -> Response {
    let page = params.page();
    match fetch_tags(&api, page).await {
        Ok(tags) => render_page(StatusCode::OK, "Tags - Kiki", &render_tag_list(&tags, page)),
        Err(e) => server_unavailable(&e),
    }
}

/// Render the page for tag `id`: one page of the entries with the tag,
/// newest first. Read entries are left out unless the `show_read` query
/// parameter is true, or the tag is `system:read`.
async fn tag_page(
    State(api): State<reqwest::Client>,
    UrlPath(id): UrlPath<i64>,
    Query(params): Query<PageParams>,
) -> Response {
    let listing = Listing {
        feed: None,
        tag: Some(id),
        search: None,
        ..params.listing()
    };
    let tag = match fetch_tag(&api, id).await {
        Ok(Some(tag)) => tag,
        Ok(None) => {
            return render_page(
                StatusCode::NOT_FOUND,
                "Tag not found - Kiki",
                "<p>Tag not found.</p>\n<p><a href=\"/tags\">&larr; Back to tags</a></p>\n",
            )
        }
        Err(e) => return server_unavailable(&e),
    };
    let entries = match fetch_entries(&api, &listing, Some(&tag.name)).await {
        Ok(entries) => entries,
        Err(e) => return server_unavailable(&e),
    };

    let (feeds, tags) = tokio::join!(
        fetch_feed_titles(&api, entries.entries.iter().filter_map(|e| e.feed_id)),
        fetch_entry_tags(&api, entries.entries.iter().map(|e| e.id)),
    );
    render_page(
        StatusCode::OK,
        &format!("{} - Kiki", tag.name),
        &render_tag_page(&tag, &entries, &feeds, &tags, &listing),
    )
}

/// Render the search page: one page of the entries matching the `q` query
/// parameter, best match first, or newest first if the `sort` parameter is
/// `newest`. Read entries are listed too, but hidden ones are not. With no
/// `q`, the page only asks what to search for.
async fn search_page(
    State(api): State<reqwest::Client>,
    Query(params): Query<PageParams>,
) -> Response {
    let listing = Listing {
        feed: None,
        tag: None,
        ..params.listing()
    };
    let Some(search) = &listing.search else {
        return render_page(
            StatusCode::OK,
            "Search - Kiki",
            &format!("<h2>Search</h2>\n{}", render_search_help()),
        );
    };
    let entries = match fetch_entries(&api, &listing, None).await {
        Ok(entries) => entries,
        Err(e) => return server_unavailable(&e),
    };

    let (feeds, tags) = tokio::join!(
        fetch_feed_titles(&api, entries.entries.iter().filter_map(|e| e.feed_id)),
        fetch_entry_tags(&api, entries.entries.iter().map(|e| e.id)),
    );
    render_search_page(
        StatusCode::OK,
        &format!("{} - Search - Kiki", search.query),
        &search.query,
        &format!(
            "<h2>Search results for &ldquo;{}&rdquo;</h2>\n{}{}",
            escape(&search.query),
            render_entries(entries.count, &entries.entries, &feeds, &tags, &listing),
            render_search_help(),
        ),
    )
}

/// Render the note on the search page explaining how searches are
/// understood; see [`fts_query`].
fn render_search_help() -> &'static str {
    "<p class=\"hint\">Entries match when every word appears in their title, \
     content or URL. Put words in &quot;quotes&quot; to match them as a phrase, \
     and end a word with * to match any word starting with it.</p>\n"
}

/// Turn `input`, as typed into the search box, into an FTS5 query for the
/// Kiki API, or `None` if it has no words to search for.
///
/// Rather than letting FTS5's own query syntax through — where a stray
/// quote, a hyphen or an apostrophe is a syntax error — every word becomes
/// its own quoted string, so that entries must contain all of them. Text in
/// double quotes is kept together as a phrase, and a `*` after a word or
/// phrase makes it match as a prefix. Words with no letters or digits are
/// dropped, since FTS5 would find nothing in them to match.
fn fts_query(input: &str) -> Option<String> {
    let mut terms = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut term = String::new();
        if c == '"' {
            chars.next();
            term.extend(chars.by_ref().take_while(|&c| c != '"'));
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() || c == '"' {
                    break;
                }
                term.push(c);
                chars.next();
            }
        }
        let mut prefix = chars.next_if_eq(&'*').is_some();
        if let Some(stripped) = term.strip_suffix('*') {
            term = stripped.to_owned();
            prefix = true;
        }
        if term.chars().any(char::is_alphanumeric) {
            let star = if prefix { "*" } else { "" };
            terms.push(format!("\"{}\"{star}", term.replace('"', "\"\"")));
        }
    }
    (!terms.is_empty()).then(|| terms.join(" "))
}

/// Delete user tag `id`, responding with `204 No Content` once it is done.
/// Called by the delete button on the tag's page, whose script then goes
/// back to the list of tags.
///
/// System tags cannot be deleted: the Kiki API refuses to, and this passes
/// its `403 Forbidden` on. Also responds with `404 Not Found` if there is
/// no such tag, `502 Bad Gateway` if the Kiki server cannot delete it, and
/// `403 Forbidden` to requests from other sites, going by `headers`; see
/// [`is_same_origin`].
async fn delete_tag(
    State(api): State<reqwest::Client>,
    UrlPath(id): UrlPath<i64>,
    headers: HeaderMap,
) -> Response {
    if !is_same_origin(&headers) {
        return (
            StatusCode::FORBIDDEN,
            "Tags may only be deleted from the web UI's own pages.",
        )
            .into_response();
    }

    let resp = api
        .delete(format!("{API_BASE}/v1/tags/id/{id}"))
        .send()
        .await;
    match resp.map(|resp| resp.status()) {
        Ok(StatusCode::NO_CONTENT) => StatusCode::NO_CONTENT.into_response(),
        Ok(StatusCode::NOT_FOUND) => (StatusCode::NOT_FOUND, "Tag not found.").into_response(),
        Ok(StatusCode::FORBIDDEN) => {
            (StatusCode::FORBIDDEN, "System tags cannot be deleted.").into_response()
        }
        Ok(status) => {
            tracing::warn!(%status, tag_id = id, "failed to delete tag");
            (StatusCode::BAD_GATEWAY, "The tag could not be deleted.").into_response()
        }
        Err(e) => {
            tracing::warn!("failed to reach the Kiki server: {e:#}");
            (StatusCode::BAD_GATEWAY, "The Kiki server is unavailable.").into_response()
        }
    }
}

/// Render the list of installed plugins, and of the directories in the
/// plugins directory that could not be loaded as plugins.
async fn plugins_page(State(api): State<reqwest::Client>) -> Response {
    match fetch_plugins(&api).await {
        Ok(plugins) => render_page(StatusCode::OK, "Plugins - Kiki", &render_plugins(&plugins)),
        Err(e) => server_unavailable(&e),
    }
}

/// Render the page for plugin `name`: what its manifest says about it, and
/// its config, with a form to change each setting.
async fn plugin_page(
    State(api): State<reqwest::Client>,
    UrlPath(name): UrlPath<String>,
) -> Response {
    render_plugin_config_page(&api, &name, StatusCode::OK, None).await
}

/// What a form on a plugin's page asks to do to its config.
#[derive(Clone, Copy)]
enum ConfigAction {
    /// Override setting `key` with the value of the form's fields for it;
    /// see [`settings::parse_input`].
    Save,
    /// Override setting `key` with `value`, written as JSON.
    Set,
    /// Remove the override of setting `key`, restoring its default.
    Reset,
    /// Remove every override, restoring every setting to its default.
    ResetAll,
}

impl ConfigAction {
    /// The action named `name` by a form's `action` field.
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "save" => Self::Save,
            "set" => Self::Set,
            "reset" => Self::Reset,
            "reset_all" => Self::ResetAll,
            _ => return None,
        })
    }
}

/// Change plugin `name`'s config as a form on its page asks, then send the
/// browser back to the page.
///
/// The form's `action` field says what to do (see [`ConfigAction`]) and its
/// `key` field names the setting to change. A value that does not match its
/// setting or is not valid JSON, or a config too large for the server to
/// save, is reported on the plugin's page, and nothing is changed. Forms
/// submitted from other sites are refused with `403 Forbidden`; see
/// [`is_same_origin`].
async fn update_plugin_config(
    State(api): State<reqwest::Client>,
    UrlPath(name): UrlPath<String>,
    headers: HeaderMap,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    if !is_same_origin(&headers) {
        return (
            StatusCode::FORBIDDEN,
            "Forms may only be submitted from the web UI's own pages.",
        )
            .into_response();
    }

    let form = settings::FormValues::new(pairs);
    let key = form.first("key").unwrap_or_default().to_owned();
    let Some(action) = form.first("action").and_then(ConfigAction::from_name) else {
        return (StatusCode::UNPROCESSABLE_ENTITY, "Unknown form action.").into_response();
    };
    let invalid = |error: String| {
        let api = api.clone();
        let name = name.clone();
        async move {
            render_plugin_config_page(&api, &name, StatusCode::UNPROCESSABLE_ENTITY, Some(&error))
                .await
        }
    };
    if key.is_empty() && !matches!(action, ConfigAction::ResetAll) {
        return invalid("Give the setting a name.".into()).await;
    }

    let config_url = plugin_api_url(&name, &["config"]);
    let req = match action {
        ConfigAction::Save => {
            let config =
                match fetch_optional::<PluginConfigResponse>(&api, config_url.clone()).await {
                    Ok(Some(config)) => config,
                    Ok(None) => return plugin_not_found(),
                    Err(e) => return server_unavailable(&e),
                };
            let setting = setting_for(&config, &key);
            let value = match settings::parse_input(&setting, &key, &form) {
                Ok(value) => value,
                Err(e) => {
                    return invalid(format!("{} was not saved: {e}.", setting.label())).await;
                }
            };
            let mut changes = Map::new();
            changes.insert(key, value);
            api.patch(config_url).json(&changes)
        }
        ConfigAction::Set => {
            let text = form.first("value").unwrap_or_default();
            let value: Value = match serde_json::from_str(text) {
                Ok(value) => value,
                Err(e) => {
                    return invalid(format!(
                        "The value for {key} is not valid JSON ({e}). Strings must be in \
                         double quotes."
                    ))
                    .await;
                }
            };
            let mut changes = Map::new();
            changes.insert(key, value);
            api.patch(config_url).json(&changes)
        }
        ConfigAction::Reset => api.delete(plugin_api_url(&name, &["config", &key])),
        ConfigAction::ResetAll => api.delete(config_url),
    };

    let resp = match req.send().await {
        Ok(resp) => resp,
        Err(e) => return server_unavailable(&e.into()),
    };
    match resp.status() {
        StatusCode::OK => {
            let reload_error = resp
                .json::<PluginConfigResponse>()
                .await
                .ok()
                .and_then(|c| c.reload_error);
            match reload_error {
                None => Redirect::to(&format!("/plugins/{}", encode_path_segment(&name)))
                    .into_response(),
                Some(e) => {
                    let error = format!(
                        "The config was saved, but the plugins failed to load with it: {e}"
                    );
                    render_plugin_config_page(
                        &api,
                        &name,
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Some(&error),
                    )
                    .await
                }
            }
        }
        StatusCode::NOT_FOUND => plugin_not_found(),
        StatusCode::UNPROCESSABLE_ENTITY => {
            let error = resp.text().await.unwrap_or_default();
            let error = error.trim();
            invalid(if error.is_empty() {
                "The config was not saved.".to_owned()
            } else {
                format!("The config was not saved. {error}.")
            })
            .await
        }
        StatusCode::PAYLOAD_TOO_LARGE => {
            render_plugin_config_page(
                &api,
                &name,
                StatusCode::PAYLOAD_TOO_LARGE,
                Some("The config is too large to save."),
            )
            .await
        }
        status => {
            tracing::warn!(%status, plugin = name, "failed to update plugin config");
            render_plugin_config_page(
                &api,
                &name,
                StatusCode::BAD_GATEWAY,
                Some("The config could not be saved."),
            )
            .await
        }
    }
}

/// Whether a form or request was sent from one of the web UI's own pages,
/// going by the headers the browser sent with it, `headers`.
///
/// The web UI has no login, so any site open in the same browser could
/// otherwise submit a form to it and change a plugin's config, or save an
/// entry. Browsers say
/// where a form came from in `Sec-Fetch-Site` or, failing that, `Origin`;
/// a request with neither did not come from a browser that would submit a
/// form for another site, and is let through.
fn is_same_origin(headers: &HeaderMap) -> bool {
    if let Some(site) = headers.get("sec-fetch-site") {
        return site == "same-origin";
    }
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    origin
        .to_str()
        .ok()
        .and_then(|o| {
            o.strip_prefix("http://")
                .or_else(|| o.strip_prefix("https://"))
        })
        .is_some_and(|authority| Some(authority) == host)
}

/// Render the page for plugin `name` with `status`, showing `error` above
/// its config if there is one.
async fn render_plugin_config_page(
    api: &reqwest::Client,
    name: &str,
    status: StatusCode,
    error: Option<&str>,
) -> Response {
    let (plugin, config) = tokio::join!(
        fetch_optional::<PluginResponse>(api, plugin_api_url(name, &[])),
        fetch_optional::<PluginConfigResponse>(api, plugin_api_url(name, &["config"])),
    );
    match (plugin, config) {
        (Ok(Some(plugin)), Ok(Some(config))) => render_form_page(
            status,
            &format!("{} - Plugins - Kiki", plugin.name),
            &render_plugin_page(&plugin, &config, error),
        ),
        (Err(e), _) | (_, Err(e)) => server_unavailable(&e),
        _ => plugin_not_found(),
    }
}

/// Render the page for a plugin that is not installed.
fn plugin_not_found() -> Response {
    render_page(
        StatusCode::NOT_FOUND,
        "Plugin not found - Kiki",
        "<p>Plugin not found.</p>\n<p><a href=\"/plugins\">&larr; Back to plugins</a></p>\n",
    )
}

/// Serve the cached asset with the blake3 hash `hash`, fetched from the
/// Kiki API. Entry pages show images and link attachments from here, so
/// that the browser never loads anything from the sites the feeds link to.
///
/// Images, audio and video are served for the browser to show; anything
/// else is served as a download, rather than rendered from the web UI's
/// origin.
async fn asset(
    State(api): State<reqwest::Client>,
    UrlPath(hash): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    // Checked here as well as by the API, so that nothing but a hash is
    // ever put into the API's URL.
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return (StatusCode::NOT_FOUND, "asset not found").into_response();
    }

    let mut req = api.get(format!("{API_BASE}/v1/assets/{hash}"));
    if let Some(etag) = headers.get(header::IF_NONE_MATCH) {
        req = req.header(header::IF_NONE_MATCH, etag);
    }
    let resp = match req.send().await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!("failed to reach the Kiki server: {e:#}");
            return (StatusCode::BAD_GATEWAY, "The Kiki server is unavailable.").into_response();
        }
    };

    let status = resp.status();
    if status == StatusCode::NOT_FOUND {
        return (StatusCode::NOT_FOUND, "asset not found").into_response();
    }
    if status != StatusCode::OK && status != StatusCode::NOT_MODIFIED {
        tracing::warn!(%status, hash, "failed to fetch cached asset");
        return (StatusCode::BAD_GATEWAY, "failed to fetch asset").into_response();
    }

    let mut out = HeaderMap::new();
    for name in [header::CONTENT_TYPE, header::ETAG, header::CACHE_CONTROL] {
        if let Some(value) = resp.headers().get(&name) {
            out.insert(name, value.clone());
        }
    }
    let shown_inline = out
        .get(header::CONTENT_TYPE)
        .and_then(|t| t.to_str().ok())
        .is_some_and(|t| {
            let t = t.trim_start().to_ascii_lowercase();
            ["image/", "audio/", "video/"]
                .iter()
                .any(|prefix| t.starts_with(prefix))
        });
    out.insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_static(if shown_inline { "inline" } else { "attachment" }),
    );
    out.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    out.insert(
        header::X_DNS_PREFETCH_CONTROL,
        header::HeaderValue::from_static("off"),
    );
    out.insert(
        header::CONTENT_SECURITY_POLICY,
        header::HeaderValue::from_static(ASSET_CONTENT_SECURITY_POLICY),
    );
    match resp.bytes().await {
        Ok(body) => (status, out, body).into_response(),
        Err(e) => {
            tracing::warn!(hash, "failed to read cached asset: {e:#}");
            (StatusCode::BAD_GATEWAY, "failed to fetch asset").into_response()
        }
    }
}

/// Fetch the assets — images and enclosures — cached for entry `id` from
/// the Kiki API, mapping each asset's original URL to the web UI URL that
/// serves the cached copy.
///
/// If the list cannot be fetched it is logged and treated as empty, so the
/// entry is still shown, linking to its images and attachments where they
/// were found.
async fn fetch_cached_assets(api: &reqwest::Client, id: i64) -> HashMap<String, String> {
    let assets: Result<ListEntryAssetsResponse> = async {
        Ok(api
            .get(format!("{API_BASE}/v1/entries/id/{id}/assets"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }
    .await;

    match assets {
        Ok(assets) => assets
            .assets
            .into_iter()
            .map(|a| (a.original_url, format!("/assets/{}", a.blake3)))
            .collect(),
        Err(e) => {
            tracing::warn!(entry_id = id, "failed to fetch cached assets: {e:#}");
            HashMap::new()
        }
    }
}

/// A page of a list of entries, as fetched by [`fetch_entries`].
struct EntryPage {
    /// Number of entries in the whole list.
    count: usize,
    /// The entries on the page, newest first.
    entries: Vec<ListEntriesResponseEntry>,
}

/// Fetch the entries on this page of `listing` with `/v1/entries/search`:
/// the entries of its feed, or with the tag named `tag`, or of every feed,
/// newest first; or the entries matching its search, sorted as it asks.
/// Hidden entries are left out, and so are read ones unless the listing
/// shows them or is of search results — except on the pages of the `system:hidden` and
/// `system:read` tags themselves, which would otherwise always be empty.
///
/// A search with no words to search for (see [`fts_query`]) matches no
/// entries, and the Kiki API is not asked.
async fn fetch_entries(
    api: &reqwest::Client,
    listing: &Listing,
    tag: Option<&str>,
) -> Result<EntryPage> {
    let (query, sort) = match &listing.search {
        Some(search) => match fts_query(&search.query) {
            Some(query) => (
                Some(query),
                if search.newest {
                    "published_at"
                } else {
                    "relevance"
                },
            ),
            None => {
                return Ok(EntryPage {
                    count: 0,
                    entries: Vec::new(),
                })
            }
        },
        None => (None, "published_at"),
    };
    let offset = u64::from(listing.page - 1) * u64::from(PAGE_SIZE);
    let mut excluded = vec![SystemTag::Hidden.name()];
    if !listing.show_read && listing.search.is_none() {
        excluded.push(SystemTag::Read.name());
    }
    excluded.retain(|&name| Some(name) != tag);
    let exclude = match excluded.as_slice() {
        [] => None,
        [name] => Some(serde_json::json!({ "not": name })),
        names => Some(serde_json::json!({ "not": { "or": names } })),
    };
    let tags = match (tag, exclude) {
        (Some(tag), Some(exclude)) => serde_json::json!({ "and": [tag, exclude] }),
        (Some(tag), None) => serde_json::json!(tag),
        (None, Some(exclude)) => exclude,
        (None, None) => Value::Null,
    };
    let resp: SearchEntriesResponse = api
        .post(format!("{API_BASE}/v1/entries/search"))
        .json(&serde_json::json!({
            "tags": tags,
            "feed_id": listing.feed,
            "query": query,
            "sort": sort,
            "offset": offset,
            "limit": PAGE_SIZE,
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(EntryPage {
        count: resp.count,
        entries: resp.entries.into_iter().map(|e| e.entry).collect(),
    })
}

/// Fetch entry `id` from the Kiki API, or `None` if there is no such entry.
async fn fetch_entry(api: &reqwest::Client, id: i64) -> Result<Option<GetEntryResponse>> {
    let resp = api
        .get(format!("{API_BASE}/v1/entries/id/{id}"))
        .send()
        .await?;
    if resp.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    Ok(Some(resp.error_for_status()?.json().await?))
}

/// Fetch page `page` (counting from 1) of `/v1/feeds` from the Kiki API.
async fn fetch_feeds(api: &reqwest::Client, page: u32) -> Result<ListFeedsResponse> {
    let offset = u64::from(page - 1) * u64::from(PAGE_SIZE);
    Ok(api
        .get(format!(
            "{API_BASE}/v1/feeds?offset={offset}&limit={PAGE_SIZE}"
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// Fetch feed `id` from the Kiki API, or `None` if there is no such feed.
async fn fetch_feed(api: &reqwest::Client, id: i64) -> Result<Option<Feed>> {
    let resp = api
        .get(format!("{API_BASE}/v1/feeds/id/{id}"))
        .send()
        .await?;
    if resp.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    Ok(Some(resp.error_for_status()?.json().await?))
}

/// Fetch page `page` (counting from 1) of `/v1/tags` from the Kiki API.
async fn fetch_tags(api: &reqwest::Client, page: u32) -> Result<ListTagsResponse> {
    let offset = u64::from(page - 1) * u64::from(PAGE_SIZE);
    Ok(api
        .get(format!(
            "{API_BASE}/v1/tags?offset={offset}&limit={PAGE_SIZE}"
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// Fetch tag `id` from the Kiki API, or `None` if there is no such tag.
async fn fetch_tag(api: &reqwest::Client, id: i64) -> Result<Option<TagResponse>> {
    fetch_optional(api, format!("{API_BASE}/v1/tags/id/{id}")).await
}

/// Fetch `/v1/plugins` from the Kiki API.
async fn fetch_plugins(api: &reqwest::Client) -> Result<ListPluginsResponse> {
    Ok(api
        .get(format!("{API_BASE}/v1/plugins"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// Fetch `url` from the Kiki API, or `None` if it is not found.
async fn fetch_optional<T: DeserializeOwned>(
    api: &reqwest::Client,
    url: String,
) -> Result<Option<T>> {
    let resp = api.get(url).send().await?;
    if resp.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    Ok(Some(resp.error_for_status()?.json().await?))
}

/// The Kiki API URL of plugin `name`, followed by the path segments in
/// `rest`, each percent-encoded.
fn plugin_api_url(name: &str, rest: &[&str]) -> String {
    let mut url = format!("{API_BASE}/v1/plugins/name/{}", encode_path_segment(name));
    for segment in rest {
        url.push('/');
        url.push_str(&encode_path_segment(segment));
    }
    url
}

/// Percent-encode `s` for use as one segment of a URL's path, leaving only
/// ASCII letters, digits, `-`, `.`, `_` and `~` as they are. The result
/// needs no further escaping to go in HTML.
fn encode_path_segment(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// The part of a `/v1/feeds/id/{id}` response the web UI uses.
#[derive(Deserialize)]
struct Feed {
    title: String,
    url: String,
    description: Option<String>,
    /// When the feed was last checked, in RFC 3339.
    last_checked: Option<String>,
    /// The API URL of the feed's cached favicon.
    #[serde(default)]
    favicon_url: Option<String>,
}

/// Fetch the titles of the feeds in `feed_ids` from the Kiki API, keyed by
/// feed ID.
///
/// Each feed is fetched once, however often it appears in `feed_ids`. A
/// feed that cannot be fetched is logged and left out, so its entries are
/// shown without a feed rather than not at all.
async fn fetch_feed_titles(
    api: &reqwest::Client,
    feed_ids: impl IntoIterator<Item = i64>,
) -> HashMap<i64, String> {
    let mut tasks = JoinSet::new();
    for id in feed_ids.into_iter().collect::<BTreeSet<_>>() {
        let api = api.clone();
        tasks.spawn(async move {
            let feed: Result<Feed> = async {
                Ok(api
                    .get(format!("{API_BASE}/v1/feeds/id/{id}"))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?)
            }
            .await;
            (id, feed)
        });
    }

    let mut titles = HashMap::new();
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok((id, Ok(feed))) => {
                titles.insert(id, feed.title);
            }
            Ok((id, Err(e))) => tracing::warn!(feed_id = id, "failed to fetch feed: {e:#}"),
            Err(e) => tracing::warn!("feed fetch task failed: {e}"),
        }
    }
    titles
}

/// Fetch the tags of the entries in `entry_ids` from the Kiki API, keyed by
/// entry ID. Both user tags and system tags are included.
///
/// An entry whose tags cannot be fetched is logged and left out, so it is
/// shown without tags rather than not at all.
async fn fetch_entry_tags(
    api: &reqwest::Client,
    entry_ids: impl IntoIterator<Item = i64>,
) -> HashMap<i64, Vec<TagResponse>> {
    let mut tasks = JoinSet::new();
    for id in entry_ids.into_iter().collect::<BTreeSet<_>>() {
        let api = api.clone();
        tasks.spawn(async move {
            let tags: Result<GetEntryTagsResponse> = async {
                Ok(api
                    .get(format!("{API_BASE}/v1/entries/id/{id}/tags"))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?)
            }
            .await;
            (id, tags)
        });
    }

    let mut tags = HashMap::new();
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok((id, Ok(resp))) => {
                tags.insert(id, resp.tags);
            }
            Ok((id, Err(e))) => tracing::warn!(entry_id = id, "failed to fetch entry tags: {e:#}"),
            Err(e) => tracing::warn!("entry tags fetch task failed: {e}"),
        }
    }
    tags
}

/// Render the entry count (`count`, of all the entries in the list), the
/// "Mark all as read" button and the filter menu — or, for search results,
/// the links that sort them — the entries on this page of `listing`, and the
/// page links.
/// `feeds` maps feed
/// IDs to the titles of the feeds; entries from feeds not in it are shown
/// without their feed. `tags` maps entry IDs to the entries' tags; entries
/// not in it are shown without tags.
fn render_entries(
    count: usize,
    entries: &[ListEntriesResponseEntry],
    feeds: &HashMap<i64, String>,
    tags: &HashMap<i64, Vec<TagResponse>>,
    listing: &Listing,
) -> String {
    let searching = listing.search.is_some();
    let unread = if listing.show_read { "" } else { "unread " };
    let (noun, actions) = if searching {
        (
            if count == 1 { "result" } else { "results" },
            render_sort(listing),
        )
    } else {
        (
            if count == 1 { "entry" } else { "entries" },
            // Entries can be marked as read in bulk by feed, but not by tag.
            if count == 0 || listing.tag.is_some() {
                render_filter(listing)
            } else {
                render_mark_read_button(listing.feed) + &render_filter(listing)
            },
        )
    };
    let mut html = format!(
        "<div class=\"list-header\">\n<p class=\"count\">{count} {}{noun}</p>\n\
         <div class=\"list-actions\">{actions}</div>\n</div>\n",
        if searching { "" } else { unread },
    );

    if entries.is_empty() {
        html.push_str(match (count, searching, listing.show_read) {
            (0, true, _) => "<p>No entries match your search.</p>\n",
            (0, false, true) => "<p>No entries yet.</p>\n",
            (0, false, false) => "<p>No unread entries.</p>\n",
            _ => "<p>No entries on this page.</p>\n",
        });
    } else {
        html.push_str("<ol class=\"entries\">\n");
        for entry in entries {
            let feed = entry.feed_id.and_then(|id| feeds.get(&id));
            let tags = tags.get(&entry.id).map_or(&[][..], Vec::as_slice);
            html.push_str("<li>");
            html.push_str(&render_entry(
                entry,
                feed.map(String::as_str),
                tags,
                listing,
            ));
            html.push_str("</li>\n");
        }
        html.push_str("</ol>\n");
    }

    html.push_str(&render_pagination(
        count,
        listing.page,
        |page| listing.page_href(page),
        match &listing.search {
            Some(search) if !search.newest => ("&larr; Previous", "Next &rarr;"),
            _ => ("&larr; Newer", "Older &rarr;"),
        },
    ));
    html
}

/// Render the links that sort a list of search results by best match or
/// newest first, the way they are sorted now shown without a link.
fn render_sort(listing: &Listing) -> String {
    let newest = listing.search.as_ref().is_some_and(|s| s.newest);
    let option = |label: &str, sorts_newest: bool| {
        if sorts_newest == newest {
            format!("<strong aria-current=\"true\">{label}</strong>")
        } else {
            format!(
                "<a href=\"{}\">{label}</a>",
                listing.sort_href(sorts_newest)
            )
        }
    };
    format!(
        "<p class=\"sort\">Sort by {} &middot; {}</p>",
        option("best match", false),
        option("newest", true),
    )
}

/// Render the filter menu for a list of entries: a checkbox that shows read
/// entries, or hides them again. Ticking it reloads the first page of
/// `listing` with the other setting; the script in `page.js` follows the
/// checkbox's `data-href`, so that the page needs no form, and the page's
/// `Content-Security-Policy` can go on forbidding them.
fn render_filter(listing: &Listing) -> String {
    let href = listing.toggle_read_href();
    format!(
        "<details class=\"filter\">\n<summary>Filter</summary>\n<div class=\"menu\">\n\
         <label><input type=\"checkbox\" class=\"filter-toggle\" data-href=\"{href}\"{}> Show read entries</label>\n\
         <noscript><a href=\"{href}\">{}</a></noscript>\n\
         </div>\n</details>\n",
        if listing.show_read { " checked" } else { "" },
        if listing.show_read { "Hide read entries" } else { "Show read entries" },
    )
}

/// Render a single entry in a list: its title, linked to the entry's page,
/// and its save button, and below them its publication date, the title of
/// `feed`, the feed it came from, and its `tags`. The entry's page links
/// back to `listing`.
fn render_entry(
    entry: &ListEntriesResponseEntry,
    feed: Option<&str>,
    tags: &[TagResponse],
    listing: &Listing,
) -> String {
    let href = listing.entry_href(entry.id);
    let meta = render_meta(
        entry.published_at.as_deref(),
        feed,
        entry.feed_favicon_url.as_deref(),
        None,
    );
    format!(
        "<a href=\"{href}\">{}</a> {}{meta}{}",
        escape(display_title(&entry.title)),
        render_save_button(entry.id, tags),
        render_tags(tags)
    )
}

/// Render the button that marks every entry as read, or only those from
/// `feed`. The button does nothing on its own: the script in `page.js`
/// sends the request to [`mark_entries_read`].
fn render_mark_read_button(feed: Option<i64>) -> String {
    let (data, label) = match feed {
        Some(id) => (format!(" data-feed=\"{id}\""), "this feed&rsquo;s entries"),
        None => (String::new(), "every entry"),
    };
    format!(
        "<button type=\"button\" class=\"mark-read\"{data} \
         title=\"Mark {label} as read\">Mark all as read</button>"
    )
}

/// The bookmark drawn on save buttons; filled in when the entry is saved.
const SAVE_ICON: &str = "<svg viewBox=\"0 0 24 24\" aria-hidden=\"true\" focusable=\"false\">\
    <path d=\"M6 3h12v18l-6-4.5L6 21z\"/></svg>";

/// Render the button that saves entry `id`, or unsaves it if its `tags`
/// include `system:saved`. The button does nothing on its own: the script
/// in `page.js` sends the change to [`save_entry`] or [`unsave_entry`].
fn render_save_button(id: i64, tags: &[TagResponse]) -> String {
    let saved = tags
        .iter()
        .any(|tag| tag.kind == TagKind::System && tag.name == SystemTag::Saved.name());
    format!(
        "<button type=\"button\" class=\"save\" data-entry=\"{id}\" aria-pressed=\"{saved}\" \
         aria-label=\"Save\" title=\"{}\">{SAVE_ICON}</button>",
        if saved { "Unsave" } else { "Save" }
    )
}

/// Render `tags`, the tags attached to an entry, as a list, or nothing if
/// there are none.
///
/// System tags come first, shown without their `system:` prefix and styled
/// apart from user tags; each group is sorted by name.
fn render_tags(tags: &[TagResponse]) -> String {
    if tags.is_empty() {
        return String::new();
    }
    let mut tags: Vec<&TagResponse> = tags.iter().collect();
    tags.sort_by(|a, b| {
        (a.kind != TagKind::System, &a.name).cmp(&(b.kind != TagKind::System, &b.name))
    });

    let items: Vec<String> = tags
        .into_iter()
        .map(|tag| format!("<li {}>{}</li>", tag_attrs(tag), escape(tag_label(tag))))
        .collect();
    format!(
        "<ul class=\"tags\" aria-label=\"Tags\">{}</ul>",
        items.concat()
    )
}

/// The name `tag` is shown under: system tags lose their `system:` prefix.
fn tag_label(tag: &TagResponse) -> &str {
    match tag.kind {
        TagKind::System => tag
            .name
            .strip_prefix(SYSTEM_TAG_PREFIX)
            .unwrap_or(&tag.name),
        TagKind::User => &tag.name,
    }
}

/// The attributes of the element `tag` is shown in: its class, and for a
/// system tag, its full name as a tooltip.
fn tag_attrs(tag: &TagResponse) -> String {
    match tag.kind {
        TagKind::System => format!("class=\"tag system\" title=\"{}\"", escape(&tag.name)),
        TagKind::User => "class=\"tag\"".to_owned(),
    }
}

/// Render the page for `entry`: its title, date, feed (`feed`), author,
/// categories and `tags`, its content from the feed, and links to the entry
/// itself and to anything else the feed links it to. The page links back to
/// `listing`.
///
/// Images in the content, and the entry's attachment, are taken from the
/// asset cache: `cached` maps an asset's original URL to the URL of its
/// cached copy. Images that are not cached are shown as links instead, and
/// an attachment that is not cached is linked where the feed says it is.
fn render_entry_page(
    entry: &GetEntryResponse,
    feed: Option<&str>,
    tags: &[TagResponse],
    cached: &HashMap<String, String>,
    listing: &Listing,
) -> String {
    let authors: Vec<&str> = match (&entry.rss, &entry.atom) {
        (Some(rss), _) => rss.author.as_deref().into_iter().collect(),
        (None, Some(atom)) => atom.authors.iter().map(String::as_str).collect(),
        (None, None) => Vec::new(),
    };
    let categories: Vec<&str> = match (&entry.rss, &entry.atom) {
        (Some(rss), _) => rss.categories.iter().map(|c| c.category.as_str()).collect(),
        (None, Some(atom)) => atom
            .categories
            .iter()
            .map(|c| c.label.as_deref().unwrap_or(&c.term))
            .collect(),
        (None, None) => Vec::new(),
    };
    let authors = authors.join(", ");

    let mut html = format!(
        "<article class=\"entry\">\n<div class=\"title-row\"><h2>{}</h2>{}</div>\n{}\n",
        escape(display_title(&entry.title)),
        render_save_button(entry.id, tags),
        render_meta(
            entry.published_at.as_deref(),
            feed,
            entry.feed_favicon_url.as_deref(),
            (!authors.trim().is_empty()).then_some(authors.as_str()),
        ),
    );
    if !categories.is_empty() {
        let categories: Vec<_> = categories.into_iter().map(escape).collect();
        html.push_str(&format!(
            "<span class=\"meta categories\">Filed under {}</span>\n",
            categories.join(", ")
        ));
    }
    html.push_str(&render_tags(tags));

    // Links and images in the content are resolved against the entry's own
    // URL, where the content was written to appear.
    let base = url::Url::parse(&entry.url).ok();
    let content = entry
        .content
        .as_deref()
        .or_else(|| {
            entry
                .rss
                .as_ref()
                .and_then(|rss| rss.description.as_deref())
        })
        .filter(|c| !c.trim().is_empty());
    match content
        .map(|c| sanitize::sanitize_html(c, base.as_ref(), |url| cached.get(url.as_str()).cloned()))
    {
        Some(Ok(content)) => {
            html.push_str(&format!("<div class=\"content\">\n{content}\n</div>\n"));
        }
        Some(Err(e)) => {
            tracing::warn!(entry_id = entry.id, "failed to sanitize entry content: {e}");
            html.push_str(
                "<p class=\"content\"><em>This entry's summary could not be shown.</em></p>\n",
            );
        }
        None => html.push_str(
            "<p class=\"content\"><em>The feed gives no summary of this entry.</em></p>\n",
        ),
    }

    let mut links = Vec::new();
    if let Some(url) = safe_link(&entry.url) {
        links.push(format!(
            "<a href=\"{}\" rel=\"noopener noreferrer\">Read the full entry &rarr;</a>",
            escape(url)
        ));
    }
    if let Some(rss) = &entry.rss {
        if let Some(url) = rss.comments.as_deref().and_then(safe_link) {
            links.push(format!(
                "<a href=\"{}\" rel=\"noopener noreferrer\">Comments</a>",
                escape(url)
            ));
        }
        if let Some(url) = rss.enclosure_url.as_deref().and_then(safe_link) {
            // The cache keys an enclosure by its URL as `url` writes it.
            let url = url::Url::parse(url)
                .ok()
                .and_then(|u| cached.get(u.as_str()))
                .map_or(url, String::as_str);
            let kind = rss
                .enclosure_mime_type
                .as_deref()
                .map(|t| format!(" ({})", escape(t)))
                .unwrap_or_default();
            links.push(format!(
                "<a href=\"{}\" rel=\"noopener noreferrer\">Attachment{kind}</a>",
                escape(url)
            ));
        }
    }
    if !links.is_empty() {
        html.push_str(&format!(
            "<p class=\"links\">{}</p>\n",
            links.join(" &middot; ")
        ));
    }

    html.push_str("</article>\n");
    html.push_str(&render_back_link(listing));
    html
}

/// Render the link back to `listing`.
fn render_back_link(listing: &Listing) -> String {
    let label = match (listing.feed, listing.tag, &listing.search) {
        (Some(_), _, _) => "Back to feed",
        (None, Some(_), _) => "Back to tag",
        (None, None, Some(_)) => "Back to search results",
        (None, None, None) => "Back to entries",
    };
    format!("<p><a href=\"{}\">&larr; {label}</a></p>\n", listing.href())
}

/// Render the feed count, the feeds on page `page`, each linked to its
/// page, and the page links.
fn render_feeds(resp: &ListFeedsResponse, page: u32) -> String {
    let mut html = format!(
        "<h2>Feeds</h2>\n<p class=\"count\">{} {}</p>\n",
        resp.count,
        if resp.count == 1 { "feed" } else { "feeds" }
    );

    if resp.feeds.is_empty() {
        html.push_str(if resp.count == 0 {
            "<p>No feeds have been added yet.</p>\n"
        } else {
            "<p>No feeds on this page.</p>\n"
        });
    } else {
        html.push_str("<ol class=\"feeds\">\n");
        for feed in &resp.feeds {
            let meta = render_feed_meta(
                &feed.url,
                &url_domain(&feed.url),
                feed.last_checked.as_deref(),
                false,
            );
            html.push_str(&format!(
                "<li>{}<a href=\"/feeds/{}\">{}</a> <span class=\"entry-count\">({} {})</span>{meta}</li>\n",
                render_favicon(feed.favicon_url.as_deref()),
                feed.id,
                escape(display_feed_title(&feed.title)),
                feed.unread_count,
                "unread"
            ));
        }
        html.push_str("</ol>\n");
    }

    html.push_str(&render_pagination(
        resp.count,
        page,
        |page| format!("/feeds?page={page}"),
        ("&larr; Previous", "Next &rarr;"),
    ));
    html
}

/// Render the tag count, the tags on page `page`, each linked to its page,
/// and the page links.
fn render_tag_list(resp: &ListTagsResponse, page: u32) -> String {
    let mut html = format!(
        "<h2>Tags</h2>\n<p class=\"count\">{} {}</p>\n",
        resp.count,
        if resp.count == 1 { "tag" } else { "tags" }
    );

    if resp.tags.is_empty() {
        html.push_str("<p>No tags on this page.</p>\n");
    } else {
        let items: Vec<String> = resp
            .tags
            .iter()
            .map(|tag| {
                format!(
                    "<li><a href=\"/tags/{}\" {}>{}</a></li>",
                    tag.id,
                    tag_attrs(tag),
                    escape(tag_label(tag))
                )
            })
            .collect();
        html.push_str(&format!(
            "<ul class=\"tags tag-list\">{}</ul>\n",
            items.concat()
        ));
    }

    html.push_str(&render_pagination(
        resp.count,
        page,
        |page| format!("/tags?page={page}"),
        ("&larr; Previous", "Next &rarr;"),
    ));
    html
}

/// Render the page for `tag`: its name, a button that deletes it if it is a
/// user tag, and the entries on this page of `listing`, each with the title
/// of its feed from `feeds` and its tags from `tags`.
fn render_tag_page(
    tag: &TagResponse,
    entries: &EntryPage,
    feeds: &HashMap<i64, String>,
    tags: &HashMap<i64, Vec<TagResponse>>,
    listing: &Listing,
) -> String {
    let mut html = format!(
        "<div class=\"title-row\">\n<h2>Entries tagged <span {}>{}</span></h2>\n{}</div>\n",
        tag_attrs(tag),
        escape(tag_label(tag)),
        render_delete_tag_button(tag),
    );
    html.push_str(&render_entries(
        entries.count,
        &entries.entries,
        feeds,
        tags,
        listing,
    ));
    html.push_str("<p><a href=\"/tags\">&larr; Back to tags</a></p>\n");
    html
}

/// Render the button that deletes `tag`, or nothing for a system tag, which
/// cannot be deleted. The button does nothing on its own: the script in
/// `page.js` asks to confirm, then sends the request to [`delete_tag`].
fn render_delete_tag_button(tag: &TagResponse) -> String {
    match tag.kind {
        TagKind::System => String::new(),
        TagKind::User => format!(
            "<button type=\"button\" class=\"delete-tag\" data-tag=\"{}\" \
             data-name=\"{}\">Delete tag</button>\n",
            tag.id,
            escape(&tag.name)
        ),
    }
}

/// Render the plugin count, each plugin in `resp` with what its manifest
/// says about it, and the directories that could not be loaded as plugins.
fn render_plugins(resp: &ListPluginsResponse) -> String {
    let mut html = format!(
        "<h2>Plugins</h2>\n<p class=\"count\">{} {}</p>\n",
        resp.count,
        if resp.count == 1 { "plugin" } else { "plugins" }
    );

    if resp.plugins.is_empty() {
        html.push_str("<p>No plugins are installed.</p>\n");
    } else {
        html.push_str("<ol class=\"plugins\">\n");
        for plugin in &resp.plugins {
            html.push_str("<li>");
            html.push_str(&render_plugin(plugin));
            html.push_str("</li>\n");
        }
        html.push_str("</ol>\n");
    }

    if !resp.errors.is_empty() {
        html.push_str("<h3>Could not be loaded</h3>\n<ol class=\"plugins\">\n");
        for error in &resp.errors {
            html.push_str(&format!(
                "<li><strong>{}</strong><span class=\"meta\">{}</span></li>\n",
                escape(&error.directory),
                escape(&error.error)
            ));
        }
        html.push_str("</ol>\n");
    }

    html.push_str(
        "<p class=\"meta\">Plugins are reloaded whenever the plugins directory or a \
         plugin's config changes.</p>\n",
    );
    html
}

/// Render a single plugin in the list: its name, linked to its page, and
/// version, then [`render_plugin_details`].
fn render_plugin(plugin: &PluginResponse) -> String {
    format!(
        "<a href=\"/plugins/{}\"><strong>{}</strong></a> <span class=\"version\">v{}</span>{}",
        encode_path_segment(&plugin.name),
        escape(&plugin.name),
        escape(&plugin.version),
        render_plugin_details(plugin)
    )
}

/// Render a plugin's description, and a line with its engine, whether it
/// runs, its authors, license and homepage.
fn render_plugin_details(plugin: &PluginResponse) -> String {
    let mut html = String::new();
    if let Some(description) = plugin
        .description
        .as_deref()
        .filter(|d| !d.trim().is_empty())
    {
        html.push_str(&format!(
            "<p class=\"description\">{}</p>",
            escape(description)
        ));
    }

    let mut parts = vec![plugin.engine.name().to_owned()];
    parts.push(
        if !plugin.engine_supported {
            "engine not supported by this build"
        } else if plugin.enabled {
            "enabled"
        } else {
            "disabled"
        }
        .to_owned(),
    );
    if !plugin.authors.is_empty() {
        let authors: Vec<_> = plugin.authors.iter().map(escape).collect();
        parts.push(format!("by {}", authors.join(", ")));
    }
    if let Some(license) = &plugin.license {
        parts.push(escape(license).into_owned());
    }
    if let Some(url) = plugin.homepage.as_deref().and_then(safe_link) {
        parts.push(format!(
            "<a href=\"{}\" rel=\"noopener noreferrer\">Homepage</a>",
            escape(url)
        ));
    }
    html.push_str(&format!(
        "<span class=\"meta\">{}</span>",
        parts.join(" &middot; ")
    ));
    html
}

/// Render the page for `plugin`: its name, version and
/// [`render_plugin_details`], then its config, `config`, with a form for
/// each setting, a form to add one and a form to reset them all. `error`,
/// if there is one, is shown above the config.
///
/// The settings the plugin's manifest describes come first, in the order
/// it gives them, then the others, by name.
fn render_plugin_page(
    plugin: &PluginResponse,
    config: &PluginConfigResponse,
    error: Option<&str>,
) -> String {
    let action = format!("/plugins/{}/config", encode_path_segment(&plugin.name));
    let mut html = format!(
        "<article class=\"plugin\">\n<h2>{} <small class=\"version\">v{}</small></h2>\n{}\n",
        escape(&plugin.name),
        escape(&plugin.version),
        render_plugin_details(plugin)
    );
    if let Some(error) = error {
        html.push_str(&format!("<p class=\"error\">{}</p>\n", escape(error)));
    }
    if config.reload_failed {
        html.push_str(
            "<p class=\"notice\">The plugin is not running with this config: the plugins \
             failed to load with it, so they keep running with the config they had. The \
             server log says why.</p>\n",
        );
    }

    html.push_str("<h3>Config</h3>\n");
    // Settings the plugin is running with but that are no longer set are
    // listed too, until the plugins reload without them.
    let described: Vec<&str> = config.settings.iter().map(|s| s.name.as_str()).collect();
    let others: BTreeSet<&str> = config
        .config
        .keys()
        .chain(config.active.keys())
        .map(String::as_str)
        .filter(|k| !described.contains(k))
        .collect();
    let keys: Vec<&str> = described.into_iter().chain(others).collect();
    if keys.is_empty() {
        html.push_str("<p>This plugin has no settings.</p>\n");
    }
    for (i, key) in keys.into_iter().enumerate() {
        html.push_str(&render_config_setting(&action, key, config, i));
    }

    let add_form = format!(
        "<form method=\"post\" action=\"{action}\" class=\"config-form\">\
         <input type=\"hidden\" name=\"action\" value=\"set\">\
         <input type=\"text\" name=\"key\" placeholder=\"name\" aria-label=\"Name\" required>\
         <input type=\"text\" name=\"value\" placeholder=\"value, as JSON\" aria-label=\"Value\" required>\
         <button type=\"submit\">Add</button></form>\n\
         <p class=\"meta\">Values are JSON: strings go in double quotes, as in \
         <code>\"hello\"</code>; numbers, <code>true</code>, <code>false</code>, \
         <code>null</code>, lists and objects are written as they are.</p>\n"
    );
    if config.settings.is_empty() {
        html.push_str(&format!("<h3>Add a setting</h3>\n{add_form}"));
    } else {
        // Plugins that describe their settings rarely need others.
        html.push_str(&format!(
            "<details class=\"add-setting\"><summary>Add a setting the plugin does not \
             describe</summary>\n{add_form}</details>\n"
        ));
    }
    if !config.overrides.is_empty() {
        html.push_str(&format!(
            "<form method=\"post\" action=\"{action}\" class=\"config-form\">\
             <button type=\"submit\" name=\"action\" value=\"reset_all\">\
             Reset every setting to its default</button></form>\n"
        ));
    }
    html.push_str("<p class=\"meta\">Changes take effect at once.</p>\n");
    html.push_str("</article>\n<p><a href=\"/plugins\">&larr; Back to plugins</a></p>\n");
    html
}

/// The setting that describes config key `key` of `config`: the one the
/// plugin's manifest gives, or else one guessed from the key's default, or
/// its value if it has no default. See [`guess_setting_type`].
fn setting_for(config: &PluginConfigResponse, key: &str) -> Setting {
    if let Some(setting) = config.settings.iter().find(|s| s.name == key) {
        return setting.clone();
    }
    let example = config
        .defaults
        .get(key)
        .or_else(|| config.config.get(key))
        .or_else(|| config.active.get(key));
    Setting {
        name: key.to_owned(),
        label: None,
        description: None,
        required: false,
        kind: example.map(guess_setting_type).unwrap_or(SettingType::Json),
    }
}

/// The type of setting that `example`, a value of it, suggests: booleans,
/// numbers and strings are edited as such, lists of strings or of integers
/// one item per line, and anything else as JSON.
fn guess_setting_type(example: &Value) -> SettingType {
    let list_of = |items: SettingType| SettingType::List {
        items: Box::new(items),
    };
    match example {
        Value::Bool(_) => SettingType::Boolean,
        Value::Number(n) if n.is_i64() => SettingType::Integer {
            min: None,
            max: None,
        },
        Value::Number(_) => SettingType::Number {
            min: None,
            max: None,
        },
        Value::String(s) => SettingType::String {
            multiline: s.contains('\n'),
        },
        Value::Array(items) if !items.is_empty() && items.iter().all(Value::is_string) => {
            list_of(SettingType::String { multiline: false })
        }
        Value::Array(items) if !items.is_empty() && items.iter().all(Value::is_i64) => {
            list_of(SettingType::Integer {
                min: None,
                max: None,
            })
        }
        _ => SettingType::Json,
    }
}

/// Render the section of the plugin page for setting `key` of `config`,
/// the `index`th on the page: its label and description, a form, submitted
/// to `action`, to change its value or restore its default, and where its
/// value comes from.
///
/// The form has fields that fit the setting (see [`setting_for`]), with a
/// second form to edit the value as JSON; a value that does not fit them is
/// only shown as JSON.
fn render_config_setting(
    action: &str,
    key: &str,
    config: &PluginConfigResponse,
    index: usize,
) -> String {
    let setting = setting_for(config, key);
    let default = config.defaults.get(key);
    let overridden = config.overrides.contains_key(key);
    let value = config.config.get(key);
    let active = config.active.get(key);
    let shown = value.or(active);
    let key_html = escape(key);
    let id = format!("setting-{index}");

    let mut html = format!("<section class=\"setting\" id=\"{id}\">\n<h4>");
    if setting.label() != key {
        html.push_str(&format!("{} ", escape(setting.label())));
    }
    html.push_str(&format!("<code>{key_html}</code></h4>\n"));
    if let Some(description) = setting.description.as_deref() {
        html.push_str(&format!(
            "<p class=\"description\">{}</p>\n",
            escape(description)
        ));
    }

    let reset = match (overridden, default.is_some()) {
        (true, true) => {
            "<button type=\"submit\" name=\"action\" value=\"reset\">Reset to default</button>"
        }
        (true, false) => "<button type=\"submit\" name=\"action\" value=\"reset\">Remove</button>",
        (false, _) => "",
    };
    let form = |fields: &str, save: &str| {
        format!(
            "<form method=\"post\" action=\"{action}\" class=\"setting-form\">\
             <input type=\"hidden\" name=\"key\" value=\"{key_html}\">{fields}\
             <div class=\"buttons\"><button type=\"submit\" name=\"action\" value=\"{save}\">\
             Save</button>{reset}</div></form>\n"
        )
    };

    // Lists and objects get room to be written out over several lines.
    let json_field = if shown.is_some_and(|v| v.is_array() || v.is_object()) {
        format!(
            "<textarea name=\"value\" rows=\"4\" class=\"json\" aria-label=\"Value of {key_html}, as JSON\">{}</textarea>",
            escape(shown.map(to_json_pretty).unwrap_or_default())
        )
    } else {
        format!(
            "<input type=\"text\" name=\"value\" value=\"{}\" class=\"json\" aria-label=\"Value of {key_html}, as JSON\">",
            escape(shown.map(to_json).unwrap_or_default())
        )
    };
    let typed = match setting.kind {
        SettingType::Json => None,
        _ => settings::render_input(&setting, shown, &id),
    };
    let mut notes = Vec::new();
    match typed {
        Some(fields) => {
            html.push_str(&form(&fields, "save"));
            html.push_str(&format!(
                "<details class=\"as-json\"><summary>Edit as JSON</summary>\n{}</details>\n",
                form(&json_field, "set")
            ));
        }
        None => {
            html.push_str(&form(&json_field, "set"));
            if !matches!(setting.kind, SettingType::Json) {
                notes.push(
                    "shown as JSON, since the value does not fit the setting's fields".to_owned(),
                );
            }
        }
    }

    notes.insert(
        0,
        match (overridden, default) {
            (true, Some(default)) => {
                let default = to_json(default);
                if default.len() <= 80 {
                    format!("overrides the default, <code>{}</code>", escape(default))
                } else {
                    "overrides the default".to_owned()
                }
            }
            (true, None) => "set here; the manifest has no default".to_owned(),
            (false, Some(_)) => "default".to_owned(),
            (false, None) => "no longer set".to_owned(),
        },
    );
    if value.is_some_and(Value::is_null) {
        notes.push("set to <code>null</code>, hiding the default".to_owned());
    }
    if value != active {
        notes.push(match active {
            Some(active) => format!(
                "running with <code>{}</code>, since the plugins failed to load with this value",
                escape(to_json(active))
            ),
            None => "not set in the config the plugin is running with".to_owned(),
        });
    }
    html.push_str(&format!(
        "<span class=\"meta\">{}</span>\n</section>\n",
        notes.join(" &middot; ")
    ));
    html
}

/// `value` as compact JSON.
fn to_json(value: &Value) -> String {
    value.to_string()
}

/// `value` as JSON spread over several lines.
fn to_json_pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// Render the page for `feed`: its title, URL, description and when it was
/// last checked, then `entries`, the entries on this page of `listing`, with
/// their tags from `tags`, keyed by entry ID.
fn render_feed_page(
    feed: &Feed,
    entries: &EntryPage,
    tags: &HashMap<i64, Vec<TagResponse>>,
    listing: &Listing,
) -> String {
    let mut html = format!(
        "<header class=\"feed-header\">\n<h2>{}{}</h2>\n{}\n",
        render_favicon(feed.favicon_url.as_deref()),
        escape(display_feed_title(&feed.title)),
        render_feed_meta(
            &feed.url,
            &url_domain(&feed.url),
            feed.last_checked.as_deref(),
            true,
        ),
    );
    // Feed descriptions are shown as plain text; they come from the feed.
    if let Some(description) = feed.description.as_deref().filter(|d| !d.trim().is_empty()) {
        html.push_str(&format!(
            "<p class=\"description\">{}</p>\n",
            escape(description)
        ));
    }
    html.push_str("</header>\n");

    // Every entry here comes from this feed, so none is labelled with it.
    html.push_str(&render_entries(
        entries.count,
        &entries.entries,
        &HashMap::new(),
        tags,
        listing,
    ));
    html.push_str("<p><a href=\"/feeds\">&larr; Back to feeds</a></p>\n");
    html
}

/// The icon on the button that copies a feed's URL.
const COPY_ICON: &str = "<svg viewBox=\"0 0 24 24\" aria-hidden=\"true\"><rect x=\"9\" y=\"9\" width=\"11\" height=\"11\" rx=\"2\"/><path d=\"M5 15V6a2 2 0 0 1 2-2h9\"/></svg>";

/// Render the line under a feed's title: its URL (`url`), shown as `label`
/// but not linked, and when it was last checked (`last_checked`, in
/// RFC 3339). When `label` isn't the whole URL, the URL is its tooltip. If
/// `copyable`, a button next to the URL copies the whole URL to the clipboard.
fn render_feed_meta(url: &str, label: &str, last_checked: Option<&str>, copyable: bool) -> String {
    let mut url_html = if label == url {
        format!("<span class=\"url\">{}</span>", escape(url))
    } else {
        format!(
            "<span class=\"url\" title=\"{}\">{}</span>",
            escape(url),
            escape(label)
        )
    };
    if copyable {
        url_html.push_str(&format!(
            "<button type=\"button\" class=\"copy-url\" data-url=\"{}\" title=\"Copy feed URL\" aria-label=\"Copy feed URL\">{COPY_ICON}</button>",
            escape(url)
        ));
    }
    let mut parts = vec![url_html];
    match last_checked.and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()) {
        Some(t) => parts.push(format!(
            "last checked <time datetime=\"{}\">{}</time>",
            t.to_rfc3339(),
            t.format("%Y-%m-%d %H:%M UTC")
        )),
        None => parts.push("not checked yet".to_owned()),
    }
    format!("<span class=\"meta\">{}</span>", parts.join(" &middot; "))
}

/// Render the line under an entry's title: its publication date
/// (`published_at`, in RFC 3339), the title of `feed`, the feed it came
/// from, and `author`, leaving out whichever are unknown.
fn render_meta(
    published_at: Option<&str>,
    feed: Option<&str>,
    favicon: Option<&str>,
    author: Option<&str>,
) -> String {
    let mut parts = Vec::new();
    // Show just the date; the API reports times in RFC 3339.
    if let Some(t) = published_at.and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()) {
        parts.push(format!(
            "<time datetime=\"{}\">{}</time>",
            t.to_rfc3339(),
            t.format("%Y-%m-%d")
        ));
    }
    if let Some(feed) = feed {
        parts.push(format!(
            "<span class=\"feed\">{}{}</span>",
            render_favicon(favicon),
            escape(display_feed_title(feed))
        ));
    }
    if let Some(author) = author {
        parts.push(format!("by {}", escape(author)));
    }

    if parts.is_empty() {
        String::new()
    } else {
        format!("<span class=\"meta\">{}</span>", parts.join(" &middot; "))
    }
}

/// Render the favicon the API serves at `api_url` (a feed's `favicon_url`)
/// as a small decorative image, loaded through the web UI's asset proxy, or
/// nothing if there is none.
fn render_favicon(api_url: Option<&str>) -> String {
    let Some(hash) = api_url.and_then(|u| u.strip_prefix("/v1/assets/")) else {
        return String::new();
    };
    // Only ever a hash goes into the page, whatever the API said.
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return String::new();
    }
    format!(
        "<img class=\"favicon\" src=\"/assets/{hash}\" alt=\"\" width=\"16\" height=\"16\" loading=\"lazy\">"
    )
}

/// `title`, or a placeholder if it is blank.
fn display_title(title: &str) -> &str {
    if title.trim().is_empty() {
        "(untitled)"
    } else {
        title
    }
}

/// `title`, a feed's title, or a placeholder if it is blank.
fn display_feed_title(title: &str) -> &str {
    if title.trim().is_empty() {
        "(untitled feed)"
    } else {
        title
    }
}

/// Return the domain of `url`, or all of `url` if it has none (or doesn't
/// parse), so that there is always something to show.
fn url_domain(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| url.to_owned())
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

/// Render the "page X of Y" line for page `page` of a list of `count`
/// items, with links to the pages before and after it, labelled with
/// `labels`. `href` gives the URL of a page of the list, escaped for use in
/// an attribute.
fn render_pagination(
    count: usize,
    page: u32,
    href: impl Fn(usize) -> String,
    labels: (&str, &str),
) -> String {
    let (prev_label, next_label) = labels;
    let pages = count.div_ceil(PAGE_SIZE as usize).max(1);
    let page_usize = page as usize;

    let mut links = Vec::new();
    if page > 1 {
        // A page past the end links back to the last page, not to the
        // (equally empty) page before it.
        let prev = page_usize.min(pages + 1) - 1;
        links.push(format!(
            "<a href=\"{}\" rel=\"prev\">{prev_label}</a>",
            href(prev)
        ));
    }
    links.push(format!("Page {page} of {pages}"));
    if page_usize < pages {
        links.push(format!(
            "<a href=\"{}\" rel=\"next\">{next_label}</a>",
            href(page_usize + 1)
        ));
    }
    format!(
        "<nav class=\"pagination\">{}</nav>",
        links.join(" &middot; ")
    )
}

/// Ask the Kiki server to shut down gracefully with `SIGTERM`, and wait
/// for it to exit.
async fn stop_server(server: &mut ServerProcess) -> Result<ExitStatus> {
    if server.0.try_wait()?.is_none() {
        let pid = libc::pid_t::try_from(server.0.id()).context("Kiki server pid out of range")?;
        // SAFETY: kill(2) has no memory-safety preconditions. The child has
        // not been reaped yet (`try_wait` returned `None`, and only
        // `ServerProcess` reaps it), so the pid still names it and cannot
        // have been reused.
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

    /// Favicons are shown from the web UI's asset proxy, and only when the
    /// API gave a well-formed asset URL.
    #[test]
    fn favicons_are_rendered_from_the_asset_proxy() {
        let hash = "ab".repeat(32);
        let api_url = format!("/v1/assets/{hash}");
        let html = render_meta(None, Some("Feed"), Some(&api_url), None);
        let img = format!(r#"<img class="favicon" src="/assets/{hash}" alt="""#);
        assert!(html.contains(&img), "{html}");
        for bad in [
            "https://evil.example/x.png".to_owned(),
            "/v1/assets/../../etc".to_owned(),
            format!("{api_url}\"><script>"),
        ] {
            assert_eq!(render_favicon(Some(&bad)), "", "{bad}");
        }
        assert_eq!(render_favicon(None), "");
        // Without a feed name there is nothing to put the icon beside.
        let html = render_meta(None, None, Some(&api_url), None);
        assert!(!html.contains("favicon"), "{html}");
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
        assert!(body.contains("0 unread entries"), "{body}");
        assert!(body.contains("No unread entries."), "{body}");
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
        assert!(body.contains("30 unread entries"), "{body}");
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

    /// Entries a plugin, or the user, hid are left out of the list.
    #[tokio::test]
    async fn the_index_page_leaves_out_hidden_entries() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        insert_entries(&tc, 3)?;
        tag_entry(&tc, 2, "system:hidden")?;

        let (status, body) = get_index(tc.client()?).await?;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("2 unread entries"), "{body}");
        assert_eq!(listed_titles(&body), ["Entry 3", "Entry 1"]);

        // Its own page still shows it, tagged hidden.
        let (status, body) = get_page(tc.client()?, "/entries/2").await?;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains(r#"<li class="tag system" title="system:hidden">hidden</li>"#),
            "{body}"
        );
        Ok(())
    }

    /// Read entries are left out of the index, unless the filter menu's
    /// checkbox asks for them.
    #[tokio::test]
    async fn the_index_page_hides_read_entries_by_default() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        insert_entries(&tc, 3)?;
        tag_entry(&tc, 2, "system:read")?;

        let (status, body) = get_index(tc.client()?).await?;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("2 unread entries"), "{body}");
        assert_eq!(listed_titles(&body), ["Entry 3", "Entry 1"]);
        assert!(
            body.contains(
                r#"<input type="checkbox" class="filter-toggle" data-href="/?show_read=true"> Show read entries</label>"#
            ),
            "{body}"
        );

        let (status, body) = get_page(tc.client()?, "/?show_read=true").await?;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("3 entries"), "{body}");
        assert_eq!(listed_titles(&body), ["Entry 3", "Entry 2", "Entry 1"]);
        // Unticking the checkbox hides them again.
        assert!(
            body.contains(
                r#"<input type="checkbox" class="filter-toggle" data-href="/" checked> Show read entries</label>"#
            ),
            "{body}"
        );
        Ok(())
    }

    /// With read entries shown, the page links, entry links, and the links
    /// back from entry pages keep showing them.
    #[tokio::test]
    async fn showing_read_entries_carries_through_links() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        insert_entries(&tc, 30)?;
        for id in 1..=30 {
            tag_entry(&tc, id, "system:read")?;
        }

        let (_, body) = get_index(tc.client()?).await?;
        assert!(body.contains("No unread entries."), "{body}");

        let (_, body) = get_page(tc.client()?, "/?show_read=true").await?;
        assert!(body.contains("Page 1 of 2"), "{body}");
        assert!(
            body.contains(r#"href="/?page=2&amp;show_read=true" rel="next""#),
            "{body}"
        );

        let (_, body) = get_page(tc.client()?, "/?page=2&show_read=true").await?;
        assert!(
            body.contains(r#"href="/?page=1&amp;show_read=true" rel="prev""#),
            "{body}"
        );
        assert!(
            body.contains(r#"href="/entries/5?page=2&amp;show_read=true""#),
            "{body}"
        );
        // The checkbox goes back to the first page of unread entries.
        assert!(body.contains(r#"data-href="/" checked"#), "{body}");

        let (_, body) = get_page(tc.client()?, "/entries/5?page=2&show_read=true").await?;
        assert!(
            body.contains(r#"<a href="/?page=2&amp;show_read=true">"#),
            "{body}"
        );
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

    /// Each entry on the index names the feed it came from, and links to
    /// its own page rather than straight to the entry's URL.
    #[tokio::test]
    async fn the_index_page_shows_each_entrys_feed() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let conn = tc.database_conn()?;
        conn.execute(
            "INSERT INTO feeds (id, title, url)
             VALUES (1, 'Feed <One>', 'http://example.com/1.xml'),
                    (2, 'Feed Two', 'http://example.com/2.xml')",
            [],
        )?;
        conn.execute(
            "INSERT INTO entries (id, feed_id, syndication_format, guid, published_at, title, url)
             VALUES (1, 1, 'rss', 'a', 1, 'From one', 'http://example.com/a'),
                    (2, 2, 'rss', 'b', 2, 'From two', 'http://example.com/b'),
                    (3, NULL, 'rss', 'c', 3, 'Orphan', 'http://example.com/c')",
            [],
        )?;

        let (status, body) = get_index(tc.client()?).await?;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains(r#"<a href="/entries/1">From one</a>"#),
            "{body}"
        );
        assert!(
            body.contains(r#"<span class="feed">Feed &lt;One&gt;</span>"#),
            "{body}"
        );
        assert!(
            body.contains(r#"<span class="feed">Feed Two</span>"#),
            "{body}"
        );
        assert!(!body.contains("http://example.com/a"), "{body}");
        assert_eq!(body.matches(r#"class="feed""#).count(), 2, "{body}");
        Ok(())
    }

    /// Attach the tag named `name`, creating it as a user tag if there is
    /// no such tag, to entry `entry_id`.
    fn tag_entry(tc: &crate::test::TestConfig, entry_id: i64, name: &str) -> Result<()> {
        let conn = tc.database_conn()?;
        conn.execute("INSERT OR IGNORE INTO tags (name) VALUES (?1)", [name])?;
        conn.execute(
            "INSERT INTO entry_tags (entry_id, tag_id)
             SELECT ?1, id FROM tags WHERE name = ?2",
            rusqlite::params![entry_id, name],
        )?;
        Ok(())
    }

    /// Entries on the index show their tags, system tags first and without
    /// their prefix, with user tag names escaped.
    #[tokio::test]
    async fn the_index_page_shows_each_entrys_tags() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        insert_entries(&tc, 2)?;
        tag_entry(&tc, 1, "<b>news</b>")?;
        tag_entry(&tc, 1, "system:read")?;
        tag_entry(&tc, 1, "system:saved")?;

        // Entry 1 is read, so only shows when read entries are.
        let (status, body) = get_page(tc.client()?, "/?show_read=true").await?;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains(
                r#"<ul class="tags" aria-label="Tags"><li class="tag system" title="system:read">read</li><li class="tag system" title="system:saved">saved</li><li class="tag">&lt;b&gt;news&lt;/b&gt;</li></ul>"#
            ),
            "{body}"
        );
        assert!(!body.contains("<b>news"), "{body}");
        // Entry 2 has no tags, so only entry 1 gets a list.
        assert_eq!(body.matches(r#"class="tags""#).count(), 1, "{body}");
        // The tags don't disturb the list of entries.
        assert_eq!(listed_titles(&body), ["Entry 2", "Entry 1"]);
        Ok(())
    }

    /// An entry's page, and its feed's page, show the entry's tags.
    #[tokio::test]
    async fn entry_and_feed_pages_show_tags() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let conn = tc.database_conn()?;
        conn.execute(
            "INSERT INTO feeds (id, title, url) VALUES (1, 'Feed', 'http://example.com/feed.xml')",
            [],
        )?;
        conn.execute(
            "INSERT INTO entries (id, feed_id, syndication_format, guid, published_at, title, url)
             VALUES (3, 1, 'rss', 'a', 1, 'Tagged', 'http://example.com/a')",
            [],
        )?;
        tag_entry(&tc, 3, "tech")?;
        tag_entry(&tc, 3, "system:saved")?;
        let expected = r#"<ul class="tags" aria-label="Tags"><li class="tag system" title="system:saved">saved</li><li class="tag">tech</li></ul>"#;

        let (status, body) = get_page(tc.client()?, "/entries/3").await?;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(expected), "{body}");

        let (status, body) = get_page(tc.client()?, "/feeds/1").await?;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(expected), "{body}");
        Ok(())
    }

    /// Every page links to the list of tags, which links to each tag's page.
    #[tokio::test]
    async fn the_tags_page_lists_every_tag() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        insert_entries(&tc, 1)?;
        tag_entry(&tc, 1, "<b>news</b>")?;

        let (_, body) = get_index(tc.client()?).await?;
        assert!(body.contains(r#"<a href="/tags">Tags</a>"#), "{body}");

        let (status, body) = get_page(tc.client()?, "/tags").await?;
        assert_eq!(status, StatusCode::OK);
        let id: i64 = tc.database_conn()?.query_row(
            "SELECT id FROM tags WHERE name = '<b>news</b>'",
            [],
            |row| row.get(0),
        )?;
        assert!(
            body.contains(&format!(
                r#"<li><a href="/tags/{id}" class="tag">&lt;b&gt;news&lt;/b&gt;</a></li>"#
            )),
            "{body}"
        );
        let saved: i64 = tc.database_conn()?.query_row(
            "SELECT id FROM tags WHERE name = 'system:saved'",
            [],
            |row| row.get(0),
        )?;
        assert!(
            body.contains(&format!(
                r#"<li><a href="/tags/{saved}" class="tag system" title="system:saved">saved</a></li>"#
            )),
            "{body}"
        );
        Ok(())
    }

    /// A tag's page lists the unread entries with the tag, and its entries
    /// link back to it.
    #[tokio::test]
    async fn a_tags_page_lists_its_entries() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        insert_entries(&tc, 4)?;
        tag_entry(&tc, 1, "tech")?;
        tag_entry(&tc, 2, "tech")?;
        tag_entry(&tc, 3, "tech")?;
        tag_entry(&tc, 3, "system:read")?;
        let id: i64 = tc.database_conn()?.query_row(
            "SELECT id FROM tags WHERE name = 'tech'",
            [],
            |row| row.get(0),
        )?;

        let (status, body) = get_page(tc.client()?, &format!("/tags/{id}")).await?;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("<title>tech - Kiki</title>"), "{body}");
        assert_eq!(listed_titles(&body), ["Entry 2", "Entry 1"]);
        assert!(
            body.contains(&format!(r#"href="/entries/2?tag={id}""#)),
            "{body}"
        );
        // Entries can't be marked as read by tag.
        assert!(!body.contains(r#"class="mark-read""#), "{body}");

        let (_, body) = get_page(tc.client()?, &format!("/tags/{id}?show_read=true")).await?;
        assert_eq!(listed_titles(&body), ["Entry 3", "Entry 2", "Entry 1"]);

        let (_, body) = get_page(tc.client()?, &format!("/entries/2?tag={id}")).await?;
        assert!(
            body.contains(&format!(r#"<a href="/tags/{id}">&larr; Back to tag</a>"#)),
            "{body}"
        );

        let (status, _) = get_page(tc.client()?, "/tags/999").await?;
        assert_eq!(status, StatusCode::NOT_FOUND);
        Ok(())
    }

    /// A user tag's page has a button that deletes the tag; a system tag's
    /// page has none.
    #[tokio::test]
    async fn only_user_tag_pages_have_a_delete_button() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        insert_entries(&tc, 1)?;
        tag_entry(&tc, 1, "<b>news</b>")?;
        let conn = tc.database_conn()?;
        let id: i64 = conn.query_row(
            "SELECT id FROM tags WHERE name = '<b>news</b>'",
            [],
            |row| row.get(0),
        )?;

        let (status, body) = get_page(tc.client()?, &format!("/tags/{id}")).await?;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains(&format!(
                r#"<button type="button" class="delete-tag" data-tag="{id}" data-name="&lt;b&gt;news&lt;/b&gt;">Delete tag</button>"#
            )),
            "{body}"
        );

        for tag in SystemTag::ALL {
            let id = tag.id(&conn)?;
            let (status, body) = get_page(tc.client()?, &format!("/tags/{id}")).await?;
            assert_eq!(status, StatusCode::OK);
            assert!(!body.contains(r#"class="delete-tag""#), "{body}");
        }
        Ok(())
    }

    /// `DELETE /tags/{id}` deletes a user tag, but not a system tag;
    /// requests from other sites are refused.
    #[tokio::test]
    async fn user_tags_can_be_deleted() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        insert_entries(&tc, 1)?;
        tag_entry(&tc, 1, "tech")?;
        let conn = tc.database_conn()?;
        let tag_exists = |name: &str| -> Result<bool> {
            Ok(conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM tags WHERE name = ?1)",
                [name],
                |row| row.get(0),
            )?)
        };
        let id: i64 = conn.query_row("SELECT id FROM tags WHERE name = 'tech'", [], |row| {
            row.get(0)
        })?;
        let path = format!("/tags/{id}");
        let delete = reqwest::Method::DELETE;

        for headers in [
            &[("Sec-Fetch-Site", "cross-site")][..],
            &[("Origin", "http://evil.example")],
        ] {
            let status = send_request(tc.client()?, delete.clone(), &path, headers).await?;
            assert_eq!(status, StatusCode::FORBIDDEN, "{headers:?}");
        }
        assert!(tag_exists("tech")?);

        let same_origin = [("Sec-Fetch-Site", "same-origin")];
        for tag in SystemTag::ALL {
            let path = format!("/tags/{}", tag.id(&conn)?);
            let status = send_request(tc.client()?, delete.clone(), &path, &same_origin).await?;
            assert_eq!(status, StatusCode::FORBIDDEN, "{tag}");
            assert!(tag_exists(tag.name())?);
        }

        let status = send_request(tc.client()?, delete.clone(), &path, &same_origin).await?;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(!tag_exists("tech")?);

        let status = send_request(tc.client()?, delete, &path, &same_origin).await?;
        assert_eq!(status, StatusCode::NOT_FOUND);
        Ok(())
    }

    /// The pages of the `system:read` and `system:hidden` tags list the
    /// entries that other lists leave out.
    #[tokio::test]
    async fn read_and_hidden_tag_pages_list_their_entries() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        insert_entries(&tc, 2)?;
        tag_entry(&tc, 1, "system:read")?;
        tag_entry(&tc, 2, "system:hidden")?;
        let tag_id = |name: &str| -> Result<i64> {
            Ok(tc.database_conn()?.query_row(
                "SELECT id FROM tags WHERE name = ?1",
                [name],
                |row| row.get(0),
            )?)
        };

        let read = tag_id("system:read")?;
        let (_, body) = get_page(tc.client()?, &format!("/tags/{read}")).await?;
        assert_eq!(listed_titles(&body), ["Entry 1"]);

        let hidden = tag_id("system:hidden")?;
        let (_, body) = get_page(tc.client()?, &format!("/tags/{hidden}")).await?;
        assert_eq!(listed_titles(&body), ["Entry 2"]);
        Ok(())
    }

    /// Every entry, in a list or on its own page, has a save button that
    /// shows whether the entry is saved.
    #[tokio::test]
    async fn entries_have_save_buttons() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let conn = tc.database_conn()?;
        conn.execute(
            "INSERT INTO feeds (id, title, url) VALUES (1, 'Feed', 'http://example.com/feed.xml')",
            [],
        )?;
        conn.execute(
            "INSERT INTO entries (id, feed_id, syndication_format, guid, published_at, title, url)
             VALUES (1, 1, 'rss', 'a', 1, 'Saved', 'http://example.com/a'),
                    (2, 1, 'rss', 'b', 2, 'Unsaved', 'http://example.com/b')",
            [],
        )?;
        tag_entry(&tc, 1, "system:saved")?;
        let saved = r#"<button type="button" class="save" data-entry="1" aria-pressed="true" aria-label="Save" title="Unsave">"#;
        let unsaved = r#"<button type="button" class="save" data-entry="2" aria-pressed="false" aria-label="Save" title="Save">"#;

        for path in ["/", "/feeds/1"] {
            let (status, body) = get_page(tc.client()?, path).await?;
            assert_eq!(status, StatusCode::OK);
            assert!(body.contains(saved), "{path}: {body}");
            assert!(body.contains(unsaved), "{path}: {body}");
            assert_eq!(listed_titles(&body), ["Unsaved", "Saved"], "{path}");
        }

        let (_, body) = get_page(tc.client()?, "/entries/1").await?;
        assert!(body.contains(saved), "{body}");
        let (_, body) = get_page(tc.client()?, "/entries/2").await?;
        assert!(body.contains(unsaved), "{body}");
        Ok(())
    }

    /// Send a `method` request for `path` to the web UI, with `headers`, and
    /// return the response's status.
    async fn send_request(
        api: reqwest::Client,
        method: reqwest::Method,
        path: &str,
        headers: &[(&str, &str)],
    ) -> Result<StatusCode> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let cancel = CancellationToken::new();
        let task = tokio::spawn(serve_ui(listener, api, cancel.clone()));

        let mut req = reqwest::Client::new().request(method, format!("http://{addr}{path}"));
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        let status = req.send().await?.status();

        cancel.cancel();
        task.await??;
        Ok(status)
    }

    /// Whether entry `id` has the `system:saved` tag.
    fn is_saved(tc: &crate::test::TestConfig, id: i64) -> Result<bool> {
        Ok(tc.database_conn()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM entry_tags et JOIN tags t ON t.id = et.tag_id
             WHERE et.entry_id = ?1 AND t.name = 'system:saved')",
            [id],
            |row| row.get(0),
        )?)
    }

    /// `PUT /entries/{id}/saved` saves an entry and `DELETE` unsaves it,
    /// each any number of times; an unknown entry is not found.
    #[tokio::test]
    async fn entries_can_be_saved_and_unsaved() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        insert_entries(&tc, 1)?;
        let same_origin = [("Sec-Fetch-Site", "same-origin")];

        for _ in 0..2 {
            let status = send_request(
                tc.client()?,
                reqwest::Method::PUT,
                "/entries/1/saved",
                &same_origin,
            )
            .await?;
            assert_eq!(status, StatusCode::NO_CONTENT);
            assert!(is_saved(&tc, 1)?);
        }
        for _ in 0..2 {
            let status = send_request(
                tc.client()?,
                reqwest::Method::DELETE,
                "/entries/1/saved",
                &same_origin,
            )
            .await?;
            assert_eq!(status, StatusCode::NO_CONTENT);
            assert!(!is_saved(&tc, 1)?);
        }

        for method in [reqwest::Method::PUT, reqwest::Method::DELETE] {
            let status =
                send_request(tc.client()?, method, "/entries/99/saved", &same_origin).await?;
            assert_eq!(status, StatusCode::NOT_FOUND);
        }
        Ok(())
    }

    /// Requests to save or unsave an entry from other sites are refused.
    #[tokio::test]
    async fn cross_site_saves_are_refused() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        insert_entries(&tc, 2)?;
        tag_entry(&tc, 2, "system:saved")?;

        for headers in [
            &[("Sec-Fetch-Site", "cross-site")][..],
            &[("Sec-Fetch-Site", "same-site")],
            &[("Origin", "http://evil.example")],
        ] {
            for (method, id) in [(reqwest::Method::PUT, 1), (reqwest::Method::DELETE, 2)] {
                let path = format!("/entries/{id}/saved");
                let status = send_request(tc.client()?, method, &path, headers).await?;
                assert_eq!(status, StatusCode::FORBIDDEN, "{path} {headers:?}");
            }
        }
        assert!(!is_saved(&tc, 1)?);
        assert!(is_saved(&tc, 2)?);
        Ok(())
    }

    /// IDs of the entries with the `system:read` tag.
    fn read_entries(tc: &crate::test::TestConfig) -> Result<Vec<i64>> {
        let conn = tc.database_conn()?;
        let mut stmt = conn.prepare(
            "SELECT et.entry_id FROM entry_tags et JOIN tags t ON t.id = et.tag_id
             WHERE t.name = 'system:read' ORDER BY et.entry_id",
        )?;
        let ids = stmt
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(ids)
    }

    /// The index and feed pages have a "Mark all as read" button, which
    /// marks only the feed's entries on a feed's page. The index leaves it
    /// out when there are no entries.
    #[tokio::test]
    async fn entry_lists_have_a_mark_all_as_read_button() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let (_, body) = get_index(tc.client()?).await?;
        assert!(!body.contains("class=\"mark-read\""), "{body}");

        insert_entries(&tc, 1)?;
        let (_, body) = get_index(tc.client()?).await?;
        assert!(
            body.contains(
                r#"<button type="button" class="mark-read" title="Mark every entry as read">"#
            ),
            "{body}"
        );

        let conn = tc.database_conn()?;
        conn.execute(
            "INSERT INTO feeds (id, title, url) VALUES (3, 'Feed', 'http://example.com/feed.xml')",
            [],
        )?;
        conn.execute("UPDATE entries SET feed_id = 3", [])?;
        let (_, body) = get_page(tc.client()?, "/feeds/3").await?;
        assert!(
            body.contains(r#"<button type="button" class="mark-read" data-feed="3""#),
            "{body}"
        );
        Ok(())
    }

    /// `POST /entries/read` marks every entry as read, or with `?feed=`,
    /// only that feed's entries; requests from other sites are refused.
    #[tokio::test]
    async fn entries_can_be_marked_as_read() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        insert_entries(&tc, 3)?;
        let conn = tc.database_conn()?;
        conn.execute(
            "INSERT INTO feeds (id, title, url) VALUES (1, 'Feed', 'http://example.com/feed.xml')",
            [],
        )?;
        conn.execute("UPDATE entries SET feed_id = 1 WHERE id = 2", [])?;
        let post = reqwest::Method::POST;

        for headers in [
            &[("Sec-Fetch-Site", "cross-site")][..],
            &[("Origin", "http://evil.example")],
        ] {
            let status = send_request(tc.client()?, post.clone(), "/entries/read", headers).await?;
            assert_eq!(status, StatusCode::FORBIDDEN, "{headers:?}");
        }
        assert!(read_entries(&tc)?.is_empty());

        let same_origin = [("Sec-Fetch-Site", "same-origin")];
        let status = send_request(
            tc.client()?,
            post.clone(),
            "/entries/read?feed=1",
            &same_origin,
        )
        .await?;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(read_entries(&tc)?, [2]);

        let status = send_request(tc.client()?, post, "/entries/read", &same_origin).await?;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(read_entries(&tc)?, [1, 2, 3]);
        Ok(())
    }

    /// What is typed into the search box becomes an FTS5 query that
    /// cannot be a syntax error: every word is quoted, phrases are kept
    /// together, and a trailing `*` matches a prefix.
    #[test]
    fn search_input_becomes_a_safe_fts_query() {
        let cases = [
            ("rust", Some(r#""rust""#)),
            ("  rust   release ", Some(r#""rust" "release""#)),
            (r#""rust release" notes"#, Some(r#""rust release" "notes""#)),
            ("rel* \"rust rel\"*", Some(r#""rel"* "rust rel"*"#)),
            ("don't", Some(r#""don't""#)),
            ("rust OR go NOT c", Some(r#""rust" "OR" "go" "NOT" "c""#)),
            (r#"unclosed "quote"#, Some(r#""unclosed" "quote""#)),
            (r#"a"b"#, Some(r#""a" "b""#)),
            ("c++ -x", Some(r#""c++" "-x""#)),
            ("", None),
            ("  \"\" * - ", None),
        ];
        for (input, expected) in cases {
            assert_eq!(fts_query(input).as_deref(), expected, "{input}");
        }
    }

    /// Insert the entries the search tests look for: two about Rust, one
    /// matching far better than the other but published earlier, one that
    /// does not mention it, and one that does but is hidden. The weaker
    /// match is read, which does not keep it out of search results.
    fn insert_search_entries(tc: &crate::test::TestConfig) -> Result<()> {
        let conn = tc.database_conn()?;
        for (id, title, content) in [
            (1, "Rust Rust Rust", "All about rust, and more rust."),
            (2, "Weekly notes", "Don't panic: a little rust this week."),
            (3, "Gardening", "Tomatoes."),
            (4, "Hidden rust", "Rust."),
        ] {
            conn.execute(
                "INSERT INTO entries (id, syndication_format, guid, published_at, title, url, content)
                 VALUES (?1, 'rss', ?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    id,
                    1_700_000_000 + id * 86_400,
                    title,
                    format!("http://example.com/{id}"),
                    content,
                ],
            )?;
        }
        tag_entry(tc, 2, "system:read")?;
        tag_entry(tc, 4, "system:hidden")?;
        Ok(())
    }

    /// Every page has the search box, empty unless it shows search results.
    #[tokio::test]
    async fn every_page_has_a_search_box() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        for path in ["/", "/feeds", "/tags", "/plugins"] {
            let (_, body) = get_page(tc.client()?, path).await?;
            assert!(
                body.contains(r#"<input type="search" name="q" value="""#),
                "{path}: {body}"
            );
        }
        Ok(())
    }

    /// On narrow screens the nav has a button that opens the search box in
    /// a popup instead; the popup's box is filled in with the search too.
    #[tokio::test]
    async fn the_search_box_has_a_popup_for_narrow_screens() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let (_, body) = get_page(tc.client()?, "/search?q=rust").await?;
        assert!(
            body.contains(r#"<button type="button" class="search-open""#),
            "{body}"
        );
        assert!(body.contains(r#"<dialog class="search-dialog""#), "{body}");
        assert_eq!(
            body.matches(r#"name="q" value="rust""#).count(),
            2,
            "{body}"
        );
        Ok(())
    }

    /// The search page lists the entries matching the search, best match
    /// first, read ones included and hidden ones left out.
    #[tokio::test]
    async fn the_search_page_lists_matching_entries() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        insert_search_entries(&tc)?;

        let (status, body) = get_page(tc.client()?, "/search?q=rust").await?;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("<title>rust - Search - Kiki</title>"),
            "{body}"
        );
        assert!(body.contains("2 results"), "{body}");
        assert!(body.contains(r#"value="rust""#), "{body}");
        assert_eq!(listed_titles(&body), ["Rust Rust Rust", "Weekly notes"]);
        assert!(
            body.contains(r#"<a href="/search?q=rust&amp;sort=newest">newest</a>"#),
            "{body}"
        );
        assert!(!body.contains(r#"class="mark-read""#), "{body}");
        assert!(!body.contains(r#"class="filter-toggle""#), "{body}");

        let (_, body) = get_page(tc.client()?, "/search?q=rust&sort=newest").await?;
        assert_eq!(listed_titles(&body), ["Weekly notes", "Rust Rust Rust"]);
        assert!(
            body.contains(r#"<a href="/search?q=rust">best match</a>"#),
            "{body}"
        );

        let (_, body) = get_page(tc.client()?, "/search?q=tomatoes%20rust").await?;
        assert!(body.contains("0 results"), "{body}");
        assert!(body.contains("No entries match your search."), "{body}");
        Ok(())
    }

    /// Searches that FTS5 would reject as syntax errors are searched for
    /// word by word, rather than failing.
    #[tokio::test]
    async fn searches_with_stray_punctuation_still_work() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        insert_search_entries(&tc)?;

        let (status, body) = get_page(tc.client()?, "/search?q=don%27t%20%22panic").await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(listed_titles(&body), ["Weekly notes"]);

        let (status, body) = get_page(tc.client()?, "/search?q=-%20%2A").await?;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("No entries match your search."), "{body}");
        Ok(())
    }

    /// Without anything to search for, the search page says how searching
    /// works rather than listing entries.
    #[tokio::test]
    async fn an_empty_search_lists_nothing() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        insert_search_entries(&tc)?;

        for path in ["/search", "/search?q=%20%20"] {
            let (status, body) = get_page(tc.client()?, path).await?;
            assert_eq!(status, StatusCode::OK);
            assert!(body.contains("<h2>Search</h2>"), "{body}");
            assert!(listed_titles(&body).is_empty(), "{body}");
        }
        Ok(())
    }

    /// What was searched for is escaped wherever the page shows it.
    #[tokio::test]
    async fn searches_are_rendered_safely() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let (_, body) = get_page(tc.client()?, "/search?q=%22%3E%3Cb%3Ex").await?;
        assert!(!body.contains("<b>x"), "{body}");
        assert!(body.contains(r#"value="&quot;&gt;&lt;b&gt;x""#), "{body}");
        assert!(
            body.contains("Search results for &ldquo;&quot;&gt;&lt;b&gt;x&rdquo;"),
            "{body}"
        );
        Ok(())
    }

    /// Search results link to entry pages that link back to them, sorted
    /// and paged as they were.
    #[tokio::test]
    async fn entry_pages_link_back_to_search_results() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        insert_search_entries(&tc)?;

        let (_, body) = get_page(tc.client()?, "/search?q=rust%20all&sort=newest").await?;
        let href = r#"href="/entries/1?q=rust%20all&amp;sort=newest""#;
        assert!(body.contains(href), "{body}");

        let (status, body) =
            get_page(tc.client()?, "/entries/1?q=rust%20all&sort=newest&page=2").await?;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains(
                r#"<a href="/search?q=rust%20all&amp;sort=newest&amp;page=2">&larr; Back to search results</a>"#
            ),
            "{body}"
        );
        Ok(())
    }

    /// On later pages of the index, entries link to pages that link back.
    #[tokio::test]
    async fn entry_pages_link_back_to_the_index_page() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        insert_entries(&tc, 30)?;

        let (_, body) = get_page(tc.client()?, "/?page=2").await?;
        assert!(body.contains(r#"href="/entries/5?page=2""#), "{body}");

        let (status, body) = get_page(tc.client()?, "/entries/5?page=2").await?;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#"<a href="/?page=2">"#), "{body}");
        Ok(())
    }

    /// An entry's page summarizes it from the feed's data: its title, date,
    /// feed, author, categories and content, with a link through to it.
    #[tokio::test]
    async fn the_entry_page_summarizes_the_entry() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let conn = tc.database_conn()?;
        conn.execute(
            "INSERT INTO feeds (id, title, url) VALUES (1, 'Example Feed', 'http://example.com/feed.xml')",
            [],
        )?;
        conn.execute(
            "INSERT INTO entries (id, feed_id, syndication_format, guid, published_at, title, url, content)
             VALUES (7, 1, 'rss', 'a', 1700000000, 'An <Entry>', 'http://example.com/posts/a',
                     '<p onclick=\"x()\">Hello <a href=\"/about\">there</a></p><script>alert(1)</script>')",
            [],
        )?;
        conn.execute(
            "INSERT INTO rss_entry_data (entry_id, author, comments)
             VALUES (7, 'Ann Author', 'http://example.com/posts/a#comments')",
            [],
        )?;
        conn.execute(
            "INSERT INTO rss_categories (entry_id, category) VALUES (7, 'news')",
            [],
        )?;

        let (status, body) = get_page(tc.client()?, "/entries/7").await?;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("<title>An &lt;Entry&gt; - Kiki</title>"),
            "{body}"
        );
        assert!(body.contains("<h2>An &lt;Entry&gt;</h2>"), "{body}");
        assert!(body.contains("2023-11-14"), "{body}");
        assert!(
            body.contains(r#"<span class="feed">Example Feed</span>"#),
            "{body}"
        );
        assert!(body.contains("by Ann Author"), "{body}");
        assert!(body.contains("Filed under news"), "{body}");
        assert!(
            body.contains(
                r#"<p>Hello <a href="http://example.com/about" rel="noopener noreferrer nofollow">there</a></p>"#
            ),
            "{body}"
        );
        // The page's only script is its own.
        assert_eq!(body.matches("<script>").count(), 1, "{body}");
        assert!(!body.contains("alert(1)"), "{body}");
        assert!(!body.contains("onclick"), "{body}");
        assert!(
            body.contains(r#"<a href="http://example.com/posts/a" rel="noopener noreferrer">Read the full entry"#),
            "{body}"
        );
        assert!(
            body.contains(r#"<a href="http://example.com/posts/a#comments" rel="noopener noreferrer">Comments</a>"#),
            "{body}"
        );
        assert!(
            body.contains(r#"<a href="/">&larr; Back to entries</a>"#),
            "{body}"
        );
        Ok(())
    }

    /// Cache `bytes` as the asset at `original_url`, of type `content_type`,
    /// for entry `entry_id` as an asset of `kind`, and return its hash.
    fn cache_asset(
        tc: &crate::test::TestConfig,
        entry_id: i64,
        bytes: &[u8],
        original_url: &str,
        content_type: &str,
        kind: &str,
    ) -> Result<String> {
        let conn = tc.database_conn()?;
        let hash = blake3::hash(bytes).to_hex().to_string();
        let path = crate::tasks::assets::asset_path(tc.config_dir(), &hash);
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, bytes)?;
        let asset_id = crate::db::assets::insert_asset(
            &conn,
            &hash,
            original_url,
            Some(content_type),
            bytes.len() as i64,
            None,
            None,
        )?;
        crate::db::assets::link_entry_asset(&conn, entry_id, asset_id, kind)?;
        Ok(hash)
    }

    /// An entry's attachment links to its cached copy when there is one, and
    /// to where the feed says it is when there isn't.
    #[tokio::test]
    async fn attachments_link_to_the_asset_cache() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let conn = tc.database_conn()?;
        conn.execute(
            "INSERT INTO entries (id, syndication_format, guid, published_at, title, url)
             VALUES (1, 'rss', 'a', 1, 'Cached', 'http://example.com/a'),
                    (2, 'rss', 'b', 2, 'Uncached', 'http://example.com/b')",
            [],
        )?;
        conn.execute(
            "INSERT INTO rss_entry_data (entry_id, enclosure_url, enclosure_mime_type)
             VALUES (1, 'HTTP://Example.com/episode.mp3', 'audio/mpeg'),
                    (2, 'http://example.com/other.mp3', 'audio/mpeg')",
            [],
        )?;
        let hash = cache_asset(
            &tc,
            1,
            b"episode",
            "http://example.com/episode.mp3",
            "audio/mpeg",
            "enclosure",
        )?;

        let (_, body) = get_page(tc.client()?, "/entries/1").await?;
        assert!(
            body.contains(&format!(
                r#"<a href="/assets/{hash}" rel="noopener noreferrer">Attachment (audio/mpeg)</a>"#
            )),
            "{body}"
        );
        assert!(!body.contains("episode.mp3"), "{body}");

        let (_, body) = get_page(tc.client()?, "/entries/2").await?;
        assert!(
            body.contains(
                r#"<a href="http://example.com/other.mp3" rel="noopener noreferrer">Attachment"#
            ),
            "{body}"
        );
        Ok(())
    }

    /// Cached media is shown in the browser; anything else is downloaded
    /// rather than rendered from the web UI's origin.
    #[tokio::test]
    async fn only_media_assets_are_shown_inline() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        tc.database_conn()?.execute(
            "INSERT INTO entries (id, syndication_format, guid, published_at, title, url)
             VALUES (1, 'rss', 'a', 1, 'Entry', 'http://example.com/a')",
            [],
        )?;
        let cases = [
            ("image/png", "inline"),
            ("audio/mpeg", "inline"),
            ("video/mp4", "inline"),
            ("text/html", "attachment"),
            ("application/pdf", "attachment"),
        ];
        let mut hashes = Vec::new();
        for (i, (content_type, _)) in cases.iter().enumerate() {
            hashes.push(cache_asset(
                &tc,
                1,
                format!("asset {i}").as_bytes(),
                &format!("http://example.com/{i}"),
                content_type,
                "enclosure",
            )?);
        }

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let cancel = CancellationToken::new();
        let task = tokio::spawn(serve_ui(listener, tc.client()?, cancel.clone()));

        for ((content_type, disposition), hash) in cases.iter().zip(&hashes) {
            let resp = reqwest::get(format!("http://{addr}/assets/{hash}")).await?;
            assert_eq!(
                resp.headers()[header::CONTENT_DISPOSITION],
                *disposition,
                "{content_type}"
            );
        }

        cancel.cancel();
        task.await??;
        Ok(())
    }

    /// Cached images are shown from the web UI's asset route, which serves
    /// the bytes from the cache; images that aren't cached become links.
    #[tokio::test]
    async fn entry_images_are_served_from_the_asset_cache() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let conn = tc.database_conn()?;
        conn.execute(
            "INSERT INTO entries (id, syndication_format, guid, published_at, title, url, content)
             VALUES (1, 'rss', 'a', 1, 'Pictures', 'http://example.com/posts/a',
                     '<img src=\"cached.png\" alt=\"Cached\"><img src=\"http://example.com/missing.png\">')",
            [],
        )?;
        let bytes = b"not really a png";
        let hash = cache_asset(
            &tc,
            1,
            bytes,
            "http://example.com/posts/cached.png",
            "image/png",
            "inline_img",
        )?;

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let cancel = CancellationToken::new();
        let task = tokio::spawn(serve_ui(listener, tc.client()?, cancel.clone()));

        let resp = reqwest::get(format!("http://{addr}/entries/1")).await?;
        let csp = resp.headers()[header::CONTENT_SECURITY_POLICY].to_str()?;
        assert!(csp.contains("img-src 'self';"), "{csp}");
        let body = resp.text().await?;
        assert!(
            body.contains(&format!(
                r#"<img src="/assets/{hash}" alt="Cached" loading="lazy">"#
            )),
            "{body}"
        );
        assert!(
            body.contains(r#"<a href="http://example.com/missing.png" rel="noopener noreferrer nofollow">[Image]</a>"#),
            "{body}"
        );

        let resp = reqwest::get(format!("http://{addr}/assets/{hash}")).await?;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "image/png");
        assert_eq!(resp.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert!(resp.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()?
            .ends_with("sandbox"));
        assert_eq!(resp.headers()[header::X_DNS_PREFETCH_CONTROL], "off");
        let etag = resp.headers()[header::ETAG].clone();
        assert_eq!(resp.bytes().await?.as_ref(), bytes);

        let resp = reqwest::Client::new()
            .get(format!("http://{addr}/assets/{hash}"))
            .header(header::IF_NONE_MATCH, etag)
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);

        for path in [
            format!("/assets/{}", "0".repeat(64)),
            "/assets/..%2Fentries".to_owned(),
        ] {
            let resp = reqwest::get(format!("http://{addr}{path}")).await?;
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{path}");
        }

        cancel.cancel();
        task.await??;
        Ok(())
    }

    /// Pages forbid any script but their own, in case anything from a feed
    /// slips through, and ask the browser not to look up the hosts they link
    /// to.
    #[tokio::test]
    async fn pages_only_run_their_own_script() -> Result<()> {
        use base64::Engine;
        use sha2::{Digest, Sha256};
        let tc = TestBuilder::all().build()?;
        insert_entries(&tc, 1)?;
        tc.database_conn()?.execute(
            "INSERT INTO feeds (id, title, url) VALUES (1, 'Feed', 'http://example.com/1.xml')",
            [],
        )?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let cancel = CancellationToken::new();
        let task = tokio::spawn(serve_ui(listener, tc.client()?, cancel.clone()));

        for path in ["/", "/entries/1", "/feeds", "/feeds/1", "/plugins"] {
            let resp = reqwest::get(format!("http://{addr}{path}")).await?;
            let csp = resp.headers()[header::CONTENT_SECURITY_POLICY]
                .to_str()?
                .to_owned();
            assert!(csp.starts_with("default-src 'none';"), "{path}: {csp}");
            assert!(csp.contains("connect-src 'self';"), "{path}: {csp}");
            assert_eq!(
                resp.headers()[header::X_DNS_PREFETCH_CONTROL],
                "off",
                "{path}"
            );

            // The only script allowed is the one inlined into the page.
            let body = resp.text().await?;
            let script = body
                .split_once("<script>")
                .and_then(|(_, rest)| rest.split_once("</script>"))
                .map(|(script, _)| script)
                .context("page has no script")?;
            let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(script));
            let script_src: Vec<&str> = csp
                .split(';')
                .map(str::trim)
                .filter(|d| d.starts_with("script-src"))
                .collect();
            assert_eq!(
                script_src,
                [format!("script-src 'sha256-{hash}'")],
                "{path}: {csp}"
            );
        }

        cancel.cancel();
        task.await??;
        Ok(())
    }

    /// Placeholders in a feed's text are shown as they are, not filled in.
    #[tokio::test]
    async fn placeholders_in_entries_are_not_filled_in() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        tc.database_conn()?.execute(
            "INSERT INTO entries (id, syndication_format, guid, published_at, title, url, content)
             VALUES (1, 'rss', 'a', 1, '{{content}}', 'http://example.com/a', '{{version}}')",
            [],
        )?;

        let (_, body) = get_page(tc.client()?, "/entries/1").await?;
        assert!(body.contains("<h2>{{content}}</h2>"), "{body}");
        assert!(body.contains("{{version}}"), "{body}");
        Ok(())
    }

    /// Every page links to the index, to the list of feeds and to the list
    /// of plugins.
    #[tokio::test]
    async fn pages_link_to_the_site_sections() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        insert_entries(&tc, 1)?;
        for path in ["/", "/entries/1", "/feeds", "/tags", "/plugins"] {
            let (_, body) = get_page(tc.client()?, path).await?;
            assert!(
                body.contains(r#"<nav class="site-nav"><a href="/feeds">Feeds</a><a href="/tags">Tags</a><a href="/plugins">Plugins</a>"#),
                "{path}: {body}"
            );
        }
        Ok(())
    }

    /// The list of feeds links each feed to its page. Titles and URLs come
    /// from feeds, so they are escaped, and the URLs aren't linked. Only
    /// each URL's domain is shown, with the full URL as its tooltip.
    #[tokio::test]
    async fn the_feeds_page_lists_every_feed() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        tc.database_conn()?.execute(
            "INSERT INTO feeds (id, title, url, last_checked)
             VALUES (1, 'Feed <One>', 'http://example.com/1.xml?a=\"><b>', 1700000000),
                    (2, '', 'http://example.com/2.xml', NULL)",
            [],
        )?;

        let (status, body) = get_page(tc.client()?, "/feeds").await?;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("<title>Feeds - Kiki</title>"), "{body}");
        assert!(body.contains("2 feeds"), "{body}");
        assert!(
            body.contains(r#"<a href="/feeds/1">Feed &lt;One&gt;</a>"#),
            "{body}"
        );
        assert!(
            body.contains(r#"<a href="/feeds/2">(untitled feed)</a>"#),
            "{body}"
        );
        assert!(!body.contains(r#""><b>"#), "{body}");
        assert!(!body.contains(r#"href="http://example.com"#), "{body}");
        assert!(
            body.contains(
                r#"<span class="url" title="http://example.com/2.xml">example.com</span>"#
            ),
            "{body}"
        );
        assert!(!body.contains(">http://example.com/2.xml<"), "{body}");
        assert!(body.contains("last checked"), "{body}");
        assert!(body.contains("not checked yet"), "{body}");
        assert!(body.contains("Page 1 of 1"), "{body}");
        Ok(())
    }

    #[tokio::test]
    async fn the_feeds_page_says_when_there_are_no_feeds() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let (status, body) = get_page(tc.client()?, "/feeds").await?;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("0 feeds"), "{body}");
        assert!(body.contains("No feeds have been added yet."), "{body}");
        Ok(())
    }

    #[tokio::test]
    async fn the_feeds_page_pages_through_the_feeds() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let conn = tc.database_conn()?;
        for i in 1..=30 {
            conn.execute(
                "INSERT INTO feeds (id, title, url) VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    i,
                    format!("Feed {i}"),
                    format!("http://example.com/{i}.xml")
                ],
            )?;
        }

        let (_, body) = get_page(tc.client()?, "/feeds").await?;
        assert!(body.contains("30 feeds"), "{body}");
        assert!(body.contains("Page 1 of 2"), "{body}");
        assert!(
            body.contains(r#"href="/feeds?page=2" rel="next""#),
            "{body}"
        );
        assert_eq!(body.matches(r#"<li><a href="/feeds/"#).count(), 25);

        let (_, body) = get_page(tc.client()?, "/feeds?page=2").await?;
        assert!(
            body.contains(r#"href="/feeds?page=1" rel="prev""#),
            "{body}"
        );
        assert_eq!(body.matches(r#"<li><a href="/feeds/"#).count(), 5);
        Ok(())
    }

    /// A feed's page lists only that feed's entries, newest first, and its
    /// entries link to pages that link back to it.
    #[tokio::test]
    async fn the_feed_page_lists_the_feeds_entries() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let conn = tc.database_conn()?;
        conn.execute(
            "INSERT INTO feeds (id, title, url, description)
             VALUES (1, 'Feed <One>', 'http://example.com/1.xml', 'About <b>one</b>'),
                    (2, 'Feed Two', 'http://example.com/2.xml', NULL)",
            [],
        )?;
        for i in 1..=30 {
            conn.execute(
                "INSERT INTO entries (feed_id, syndication_format, guid, published_at, title, url)
                 VALUES (?1, 'rss', ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    if i % 2 == 0 { 1 } else { 2 },
                    format!("guid-{i}"),
                    1_700_000_000 + i * 86_400,
                    format!("Entry {i}"),
                    format!("http://example.com/{i}"),
                ],
            )?;
        }

        let (status, body) = get_page(tc.client()?, "/feeds/1").await?;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("<title>Feed &lt;One&gt; - Kiki</title>"),
            "{body}"
        );
        assert!(body.contains("<h2>Feed &lt;One&gt;</h2>"), "{body}");
        assert!(body.contains("About &lt;b&gt;one&lt;/b&gt;"), "{body}");
        assert!(body.contains("15 unread entries"), "{body}");
        let expected: Vec<_> = (1..=15).rev().map(|i| format!("Entry {}", i * 2)).collect();
        assert_eq!(listed_titles(&body), expected);
        // Every entry is from this feed, so none is labelled with it.
        assert!(!body.contains(r#"class="feed""#), "{body}");
        assert!(body.contains(r#"href="/entries/30?feed=1""#), "{body}");
        assert!(
            body.contains(r#"<a href="/feeds">&larr; Back to feeds</a>"#),
            "{body}"
        );

        let (status, body) = get_page(tc.client()?, "/entries/30?feed=1").await?;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains(r#"<a href="/feeds/1">&larr; Back to feed</a>"#),
            "{body}"
        );
        Ok(())
    }

    /// A feed's page leaves out its read entries too, unless asked for them.
    #[tokio::test]
    async fn the_feed_page_hides_read_entries_by_default() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let conn = tc.database_conn()?;
        conn.execute(
            "INSERT INTO feeds (id, title, url) VALUES (1, 'Feed', 'http://example.com/1.xml')",
            [],
        )?;
        insert_entries(&tc, 3)?;
        conn.execute("UPDATE entries SET feed_id = 1", [])?;
        tag_entry(&tc, 3, "system:read")?;

        let (_, body) = get_page(tc.client()?, "/feeds/1").await?;
        assert!(body.contains("2 unread entries"), "{body}");
        assert_eq!(listed_titles(&body), ["Entry 2", "Entry 1"]);
        assert!(
            body.contains(r#"data-href="/feeds/1?show_read=true""#),
            "{body}"
        );

        let (_, body) = get_page(tc.client()?, "/feeds/1?show_read=true").await?;
        assert!(body.contains("3 entries"), "{body}");
        assert_eq!(listed_titles(&body), ["Entry 3", "Entry 2", "Entry 1"]);
        assert!(
            body.contains(r#"href="/entries/3?feed=1&amp;show_read=true""#),
            "{body}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn the_feed_page_pages_through_the_entries() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let conn = tc.database_conn()?;
        conn.execute(
            "INSERT INTO feeds (id, title, url) VALUES (1, 'Feed', 'http://example.com/1.xml')",
            [],
        )?;
        insert_entries(&tc, 30)?;
        conn.execute("UPDATE entries SET feed_id = 1", [])?;

        let (_, body) = get_page(tc.client()?, "/feeds/1").await?;
        assert!(body.contains("Page 1 of 2"), "{body}");
        assert!(
            body.contains(r#"href="/feeds/1?page=2" rel="next""#),
            "{body}"
        );

        let (_, body) = get_page(tc.client()?, "/feeds/1?page=2").await?;
        assert!(body.contains("Page 2 of 2"), "{body}");
        assert!(
            body.contains(r#"href="/feeds/1?page=1" rel="prev""#),
            "{body}"
        );
        let expected: Vec<_> = (1..=5).rev().map(|i| format!("Entry {i}")).collect();
        assert_eq!(listed_titles(&body), expected);
        assert!(
            body.contains(r#"href="/entries/5?feed=1&amp;page=2""#),
            "{body}"
        );

        let (_, body) = get_page(tc.client()?, "/entries/5?feed=1&page=2").await?;
        assert!(
            body.contains(r#"<a href="/feeds/1?page=2">&larr; Back to feed</a>"#),
            "{body}"
        );
        Ok(())
    }

    /// The list of plugins shows each installed plugin, and each directory
    /// that could not be loaded as one and why.
    #[tokio::test]
    async fn the_plugins_page_lists_every_plugin() -> Result<()> {
        let tc = TestBuilder::default().init_database().build()?;
        tc.install_lua_plugin("passthrough", "", serde_json::json!({}))?;
        std::fs::create_dir_all(tc.plugins_dir().join("broken"))?;
        let tc = tc.init_server()?;

        let (status, body) = get_page(tc.client()?, "/plugins").await?;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("<title>Plugins - Kiki</title>"), "{body}");
        assert!(body.contains("1 plugin<"), "{body}");
        assert!(
            body.contains(r#"<a href="/plugins/passthrough"><strong>passthrough</strong></a> <span class="version">v1.0.0</span>"#),
            "{body}"
        );
        assert!(body.contains("Could not be loaded"), "{body}");
        assert!(body.contains("<strong>broken</strong>"), "{body}");
        assert!(body.contains("manifest.toml"), "{body}");
        Ok(())
    }

    #[tokio::test]
    async fn the_plugins_page_says_when_there_are_no_plugins() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let (status, body) = get_page(tc.client()?, "/plugins").await?;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("0 plugins"), "{body}");
        assert!(body.contains("No plugins are installed."), "{body}");
        assert!(!body.contains("Could not be loaded"), "{body}");
        Ok(())
    }

    /// Serve the web UI on an ephemeral port with `api` as its API client,
    /// and submit `form` to `path` from it, sending `headers` as well. The
    /// client follows redirects.
    async fn post_form(
        api: reqwest::Client,
        path: &str,
        form: &[(&str, &str)],
        headers: &[(&str, &str)],
    ) -> Result<(StatusCode, String)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let cancel = CancellationToken::new();
        let task = tokio::spawn(serve_ui(listener, api, cancel.clone()));

        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form)
            .finish();
        let mut req = reqwest::Client::new()
            .post(format!("http://{addr}{path}"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body);
        for (name, value) in headers {
            req = req.header(*name, value.replace("{addr}", &addr.to_string()));
        }
        let resp = req.send().await?;
        let status = resp.status();
        let body = resp.text().await?;

        cancel.cancel();
        task.await??;
        Ok((status, body))
    }

    /// A test context with one plugin, `hello`, whose defaults are
    /// `{"greeting": "hi", "count": 1, "tags": ["a"]}`, and the server running.
    fn hello_plugin() -> Result<crate::test::TestConfig> {
        let tc = TestBuilder::default().init_database().build()?;
        tc.install_lua_plugin(
            "hello",
            "",
            serde_json::json!({"greeting": "hi", "count": 1, "tags": ["a"]}),
        )?;
        tc.init_server()
    }

    /// The overrides stored in the database for `hello`.
    fn stored_overrides(tc: &crate::test::TestConfig) -> Result<Value> {
        Ok(Value::Object(crate::db::plugins::get_config_overrides(
            &tc.database_conn()?,
            "hello",
        )?))
    }

    /// A plugin's page shows its config, one form per setting, and allows
    /// forms to be submitted to the web UI.
    #[tokio::test]
    async fn the_plugin_page_shows_the_config() -> Result<()> {
        let tc = hello_plugin()?;

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let cancel = CancellationToken::new();
        let task = tokio::spawn(serve_ui(listener, tc.client()?, cancel.clone()));
        let resp = reqwest::get(format!("http://{addr}/plugins/hello")).await?;
        let csp = resp.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()?
            .to_owned();
        let status = resp.status();
        let body = resp.text().await?;
        cancel.cancel();
        task.await??;

        assert_eq!(status, StatusCode::OK);
        assert!(csp.contains("form-action 'self'"), "{csp}");
        assert!(csp.contains(SCRIPT_SRC.as_str()), "{csp}");
        assert!(
            body.contains("<title>hello - Plugins - Kiki</title>"),
            "{body}"
        );
        assert!(body.contains("<h2>hello <small"), "{body}");
        assert!(
            body.contains(r#"<form method="post" action="/plugins/hello/config""#),
            "{body}"
        );
        assert!(
            body.contains(r#"<input type="hidden" name="key" value="greeting"><input type="text" name="value" value="&quot;hi&quot;""#),
            "{body}"
        );
        assert!(body.contains(r#"name="value" value="1""#), "{body}");
        assert!(body.contains("<textarea name=\"value\""), "{body}");
        // Settings the manifest does not describe get fields that fit their
        // defaults, and can be edited as JSON too.
        assert!(
            body.contains(r#"<input type="text" name="v" id="setting-1-v" value="hi""#),
            "{body}"
        );
        assert!(
            body.contains(r#"<input type="number" step="1" name="v" id="setting-0-v" value="1""#),
            "{body}"
        );
        assert!(
            body.contains(
                r#"<textarea name="v" id="setting-2-v" rows="3" aria-label="tags">a</textarea>"#
            ),
            "{body}"
        );
        assert!(body.contains("<summary>Edit as JSON</summary>"), "{body}");
        assert!(!body.contains("Reset to default"), "{body}");
        assert!(!body.contains("reset_all"), "{body}");
        assert!(!body.contains(r#"class="notice""#), "{body}");
        Ok(())
    }

    /// Saving a setting overrides it, and the page then says so; resetting
    /// it restores the default.
    #[tokio::test]
    async fn settings_can_be_changed_from_the_plugin_page() -> Result<()> {
        let tc = hello_plugin()?;
        let path = "/plugins/hello/config";

        let (status, body) = post_form(
            tc.client()?,
            path,
            &[
                ("action", "set"),
                ("key", "greeting"),
                ("value", "\"hello\""),
            ],
            &[],
        )
        .await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            stored_overrides(&tc)?,
            serde_json::json!({"greeting": "hello"})
        );
        assert!(body.contains("<h2>hello <small"), "{body}");
        assert!(body.contains(r#"value="&quot;hello&quot;""#), "{body}");
        assert!(
            body.contains("overrides the default, <code>&quot;hi&quot;</code>"),
            "{body}"
        );
        // The plugins reloaded, so the plugin runs with the new value.
        assert!(!body.contains("running with"), "{body}");
        assert!(!body.contains(r#"class="notice""#), "{body}");
        assert!(body.contains("Reset to default"), "{body}");

        // Settings the manifest has no default for can be added, too.
        let (status, _) = post_form(
            tc.client()?,
            path,
            &[
                ("action", "set"),
                ("key", "extra/key"),
                ("value", "{\"a\": [1, 2]}"),
            ],
            &[],
        )
        .await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            stored_overrides(&tc)?,
            serde_json::json!({"greeting": "hello", "extra/key": {"a": [1, 2]}})
        );

        // Keys are sent to the API as one path segment.
        let (status, body) = post_form(
            tc.client()?,
            path,
            &[("action", "reset"), ("key", "extra/key"), ("value", "")],
            &[],
        )
        .await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            stored_overrides(&tc)?,
            serde_json::json!({"greeting": "hello"})
        );
        assert!(body.contains("reset_all"), "{body}");

        let (status, body) = post_form(tc.client()?, path, &[("action", "reset_all")], &[]).await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(stored_overrides(&tc)?, serde_json::json!({}));
        assert!(!body.contains(r#"class="notice""#), "{body}");
        Ok(())
    }

    /// A plugin, `rules`, whose manifest describes its settings: a
    /// boolean, a choice and a list of objects. The server is running.
    fn rules_plugin() -> Result<crate::test::TestConfig> {
        let tc = TestBuilder::default().init_database().build()?;
        let dir = tc.plugins_dir().join("rules");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join("manifest.toml"),
            r#"
            name = "rules"
            version = "1.0.0"
            engine = "lua"

            [config]
            on = true
            mode = "fast"
            rules = [{ pattern = "a", fields = ["title"] }]
            toggles = [{ enabled = false }]

            [[settings]]
            name = "on"
            type = "boolean"
            label = "Enabled"
            description = "Whether to do anything."

            [[settings]]
            name = "mode"
            type = "choice"
            choices = ["fast", "slow"]

            [[settings]]
            name = "rules"
            type = "list"
            label = "Rules"
            [settings.items]
            type = "object"
            [[settings.items.fields]]
            name = "pattern"
            type = "string"
            required = true
            [[settings.items.fields]]
            name = "fields"
            type = "list"
            items = { type = "choice", choices = ["title", "content"] }
            [[settings.items.fields]]
            name = "feeds"
            type = "list"
            items = { type = "integer" }

            [[settings]]
            name = "toggles"
            type = "list"
            [settings.items]
            type = "object"
            [[settings.items.fields]]
            name = "enabled"
            type = "boolean"
            [[settings.items.fields]]
            name = "note"
            type = "string"
            "#,
        )?;
        std::fs::write(dir.join("main.lua"), "")?;
        tc.init_server()
    }

    fn stored_rules(tc: &crate::test::TestConfig) -> Result<Value> {
        Ok(Value::Object(crate::db::plugins::get_config_overrides(
            &tc.database_conn()?,
            "rules",
        )?))
    }

    /// Settings the manifest describes get fields that fit them, in the
    /// order the manifest gives.
    #[tokio::test]
    async fn described_settings_get_fitting_fields() -> Result<()> {
        let tc = rules_plugin()?;
        let (status, body) = get_page(tc.client()?, "/plugins/rules").await?;
        assert_eq!(status, StatusCode::OK);

        assert!(body.contains("<h4>Enabled <code>on</code></h4>"), "{body}");
        assert!(body.contains("Whether to do anything."), "{body}");
        assert!(
            body.contains(
                r#"<input type="checkbox" name="v" id="setting-0-v" value="true" checked> Enabled"#
            ),
            "{body}"
        );
        assert!(
            body.contains(
                r#"<option value="fast" selected>fast</option><option value="slow">slow</option>"#
            ),
            "{body}"
        );
        assert!(
            !body.contains("(not set)</option><option value=\"fast\" selected"),
            "{body}"
        );
        // One fieldset per rule, and a blank one to add a rule.
        assert!(body.contains(r#"name="v#count" value="2""#), "{body}");
        assert!(
            body.contains(r#"name="v.0.pattern" id="setting-2-v.0.pattern" value="a""#),
            "{body}"
        );
        assert!(
            body.contains(r#"name="v.0.fields" value="title" checked"#),
            "{body}"
        );
        assert!(
            body.contains(r#"name="v.0.fields" value="content">"#),
            "{body}"
        );
        assert!(body.contains(r#"name="remove" value="v.0""#), "{body}");
        assert!(
            body.contains(r#"name="v.1.pattern" id="setting-2-v.1.pattern" value="""#),
            "{body}"
        );
        assert!(body.contains(r#"<legend>Add to Rules</legend>"#), "{body}");
        assert!(body.find("<code>on</code>") < body.find("<code>mode</code>"));
        assert!(body.find("<code>mode</code>") < body.find("<code>rules</code>"));
        assert!(
            body.contains("Add a setting the plugin does not describe"),
            "{body}"
        );
        Ok(())
    }

    /// Saving a described setting reads its value from its fields.
    #[tokio::test]
    async fn described_settings_are_saved_from_their_fields() -> Result<()> {
        let tc = rules_plugin()?;
        let path = "/plugins/rules/config";

        // An unticked box is false.
        let (status, body) = post_form(
            tc.client()?,
            path,
            &[("action", "save"), ("key", "on")],
            &[],
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(stored_rules(&tc)?, serde_json::json!({"on": false}));

        let (status, _) = post_form(
            tc.client()?,
            path,
            &[("action", "save"), ("key", "mode"), ("v", "slow")],
            &[],
        )
        .await?;
        assert_eq!(status, StatusCode::OK);

        // Edit the first rule, remove it, keep the second, and add a third
        // from the blank fieldset; a blank one adds nothing.
        let (status, body) = post_form(
            tc.client()?,
            path,
            &[
                ("action", "save"),
                ("key", "rules"),
                ("v#count", "4"),
                ("v.0.pattern", "gone"),
                ("remove", "v.0"),
                ("v.1.pattern", "b"),
                ("v.1.fields", "title"),
                ("v.1.fields", "content"),
                ("v.1.feeds", "1\r\n\r\n2\r\n"),
                ("v.2.pattern", "c"),
                ("v.3.pattern", ""),
                ("v.3.feeds", ""),
            ],
            &[],
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            stored_rules(&tc)?,
            serde_json::json!({
                "on": false,
                "mode": "slow",
                "rules": [
                    {"pattern": "b", "fields": ["title", "content"], "feeds": [1, 2]},
                    {"pattern": "c"},
                ],
            })
        );
        assert!(
            body.contains(r#"name="v.1.pattern" id="setting-2-v.1.pattern" value="c""#),
            "{body}"
        );

        // Removing every item leaves an empty list.
        let (status, _) = post_form(
            tc.client()?,
            path,
            &[
                ("action", "save"),
                ("key", "rules"),
                ("v#count", "1"),
                ("v.0.pattern", "b"),
                ("remove", "v.0"),
            ],
            &[],
        )
        .await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            stored_rules(&tc)?.get("rules"),
            Some(&serde_json::json!([]))
        );
        Ok(())
    }

    /// An item already in a list is kept when all its fields are left empty
    /// or unticked; only the blank fieldset for a new item is dropped.
    #[tokio::test]
    async fn existing_items_are_kept_when_left_blank() -> Result<()> {
        let tc = rules_plugin()?;
        let (_, body) = get_page(tc.client()?, "/plugins/rules").await?;
        assert!(body.contains(r#"name="v.0#present" value="1""#), "{body}");
        assert!(!body.contains(r#"name="v.1#present""#), "{body}");

        // What the browser submits for the page as it is: the item's box is
        // unticked, and its note and the blank fieldset are empty.
        let (status, body) = post_form(
            tc.client()?,
            "/plugins/rules/config",
            &[
                ("action", "save"),
                ("key", "toggles"),
                ("v#count", "2"),
                ("v.0#present", "1"),
                ("v.0.note", ""),
                ("v.1.note", ""),
            ],
            &[],
        )
        .await?;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            stored_rules(&tc)?,
            serde_json::json!({"toggles": [{"enabled": false}]})
        );
        Ok(())
    }

    /// Fields that do not hold a value of their setting's type are reported,
    /// and nothing is saved; so are JSON values the API refuses.
    #[tokio::test]
    async fn described_settings_are_checked() -> Result<()> {
        let tc = rules_plugin()?;
        let path = "/plugins/rules/config";

        for (form, error) in [
            (
                &[("v#count", "1"), ("v.0.fields", "title")][..],
                "Rules was not saved: rules[0].pattern: is required.",
            ),
            (
                &[("v#count", "1"), ("v.0.pattern", "a"), ("v.0.feeds", "x")],
                "Rules was not saved: rules[0].feeds[0]: &quot;x&quot; is not a whole number.",
            ),
            (
                &[
                    ("v#count", "1"),
                    ("v.0.pattern", "a"),
                    ("v.0.fields", "url"),
                ],
                "Rules was not saved: rules[0].fields[0]: expected one of",
            ),
        ] {
            let mut pairs = vec![("action", "save"), ("key", "rules")];
            pairs.extend_from_slice(form);
            let (status, body) = post_form(tc.client()?, path, &pairs, &[]).await?;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
            assert!(body.contains(error), "{error}: {body}");
        }

        let (status, body) = post_form(
            tc.client()?,
            path,
            &[("action", "set"), ("key", "mode"), ("value", "\"medium\"")],
            &[],
        )
        .await?;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            body.contains("The config was not saved. Invalid config: mode: expected one of"),
            "{body}"
        );
        assert_eq!(stored_rules(&tc)?, serde_json::json!({}));
        Ok(())
    }

    /// A value that does not fit its setting's fields, set some other way,
    /// is shown as JSON.
    #[tokio::test]
    async fn values_that_do_not_fit_are_shown_as_json() -> Result<()> {
        let tc = rules_plugin()?;
        let overrides = serde_json::json!({"rules": [{"pattern": "a", "fields": "title"}]});
        crate::db::plugins::set_config_overrides(
            &tc.database_conn()?,
            "rules",
            overrides.as_object().unwrap_or(&Map::new()),
        )?;
        let (status, body) = get_page(tc.client()?, "/plugins/rules").await?;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !body.contains(r#"<div class="items" id="setting-2-v">"#),
            "{body}"
        );
        assert!(
            body.contains("shown as JSON, since the value does not fit the setting's fields"),
            "{body}"
        );
        Ok(())
    }

    /// A value that is not JSON is reported on the page, and not saved.
    #[tokio::test]
    async fn invalid_settings_are_reported() -> Result<()> {
        let tc = hello_plugin()?;
        let path = "/plugins/hello/config";

        let (status, body) = post_form(
            tc.client()?,
            path,
            &[("action", "set"), ("key", "greeting"), ("value", "<hello>")],
            &[],
        )
        .await?;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            body.contains(r#"<p class="error">The value for greeting is not valid JSON"#),
            "{body}"
        );
        assert!(!body.contains("<hello>"), "{body}");

        let (status, body) = post_form(
            tc.client()?,
            path,
            &[("action", "set"), ("key", ""), ("value", "1")],
            &[],
        )
        .await?;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body.contains("Give the setting a name."), "{body}");

        assert_eq!(stored_overrides(&tc)?, serde_json::json!({}));
        Ok(())
    }

    /// A setting the plugin fails to load with is saved, and the page says
    /// the plugin keeps running with its old config.
    #[tokio::test]
    async fn settings_that_fail_to_load_are_reported() -> Result<()> {
        let tc = TestBuilder::default().init_database().build()?;
        tc.install_lua_plugin(
            "hello",
            "local config = ...\nif config.fail then error('refusing to load') end",
            serde_json::json!({}),
        )?;
        let tc = tc.init_server()?;

        let (status, body) = post_form(
            tc.client()?,
            "/plugins/hello/config",
            &[("action", "set"), ("key", "fail"), ("value", "true")],
            &[],
        )
        .await?;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body.contains("refusing to load"), "{body}");
        assert!(body.contains(r#"class="notice""#), "{body}");
        assert_eq!(stored_overrides(&tc)?, serde_json::json!({"fail": true}));
        Ok(())
    }

    /// Forms submitted from other sites are refused; forms from the web
    /// UI's own pages are not.
    #[tokio::test]
    async fn cross_site_forms_are_refused() -> Result<()> {
        let tc = hello_plugin()?;
        let path = "/plugins/hello/config";
        let form = [("action", "set"), ("key", "count"), ("value", "2")];

        for headers in [
            &[("Sec-Fetch-Site", "cross-site")][..],
            &[("Sec-Fetch-Site", "same-site")],
            &[("Origin", "http://evil.example")],
            &[("Origin", "null")],
        ] {
            let (status, _) = post_form(tc.client()?, path, &form, headers).await?;
            assert_eq!(status, StatusCode::FORBIDDEN, "{headers:?}");
        }
        assert_eq!(stored_overrides(&tc)?, serde_json::json!({}));

        for headers in [
            &[("Sec-Fetch-Site", "same-origin")][..],
            &[("Origin", "http://{addr}")],
        ] {
            let (status, _) = post_form(tc.client()?, path, &form, headers).await?;
            assert_eq!(status, StatusCode::OK, "{headers:?}");
        }
        assert_eq!(stored_overrides(&tc)?, serde_json::json!({"count": 2}));
        Ok(())
    }

    #[tokio::test]
    async fn a_missing_plugin_is_reported() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let (status, body) = get_page(tc.client()?, "/plugins/missing").await?;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("Plugin not found."), "{body}");

        let (status, body) = post_form(
            tc.client()?,
            "/plugins/missing/config",
            &[("action", "set"), ("key", "a"), ("value", "1")],
            &[],
        )
        .await?;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("Plugin not found."), "{body}");
        Ok(())
    }

    #[tokio::test]
    async fn a_missing_feed_is_reported() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let (status, body) = get_page(tc.client()?, "/feeds/42").await?;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("Feed not found."), "{body}");
        Ok(())
    }

    #[tokio::test]
    async fn a_missing_entry_is_reported() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let (status, body) = get_page(tc.client()?, "/entries/42").await?;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("Entry not found."), "{body}");
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
