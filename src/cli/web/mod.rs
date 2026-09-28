mod sanitize;

use crate::cli::serve::ServeArgs;
use crate::routes::v1::entries::entry_assets::ListEntryAssetsResponse;
use crate::routes::v1::entries::get_entry::GetEntryResponse;
use crate::routes::v1::entries::{ListEntriesResponse, ListEntriesResponseEntry};
use crate::routes::v1::feeds::feed_entries::FeedEntriesResponse;
use crate::routes::v1::feeds::list_feeds::ListFeedsResponse;
use crate::routes::v1::plugins::list_plugins::{ListPluginsResponse, PluginResponse};
use anyhow::{anyhow, bail, Context, Result};
use axum::{
    extract::{Path as UrlPath, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use clap::Args;
use quick_xml::escape::escape;
use regex::Regex;
use serde::Deserialize;
use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::path::Path;
use std::process::ExitStatus;
use std::sync::LazyLock;
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
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

/// `Content-Security-Policy` sent with every page.
///
/// Pages carry titles and content from feeds. They are escaped or
/// sanitized, but as a second line of defence the pages may not run any
/// script, load anything but images from the web UI's own asset cache, or
/// submit forms.
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; img-src 'self'; \
    style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

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
    let app = Router::new()
        .route("/", get(index))
        .route("/entries/{id}", get(entry_page))
        .route("/feeds", get(feeds_page))
        .route("/feeds/{id}", get(feed_page))
        .route("/plugins", get(plugins_page))
        .route("/assets/{hash}", get(asset))
        .with_state(api);
    axum::serve(listener, app)
        .with_graceful_shutdown(cancel.cancelled_owned())
        .await
        .context("error encountered while running the web UI")
}

/// Number of entries, or feeds, shown on each page of a list.
const PAGE_SIZE: u32 = 25;

/// Query parameters accepted by the index, feed and entry pages.
#[derive(Deserialize)]
struct PageParams {
    /// The page of entries to show, or to link back to, counting from 1
    /// (default: 1).
    page: Option<u32>,
    /// On an entry page, the feed whose page to link back to, rather than
    /// the index.
    feed: Option<i64>,
}

impl PageParams {
    fn page(&self) -> u32 {
        self.page.unwrap_or(1).max(1)
    }

    /// The list of entries an entry page links back to.
    fn listing(&self) -> Listing {
        Listing {
            feed: self.feed,
            page: self.page(),
        }
    }
}

/// A page of a list of entries: of the index, or of a feed's page. Entry
/// pages link back to the listing they were opened from.
#[derive(Clone, Copy)]
struct Listing {
    /// The feed whose entries are listed, or `None` for the index.
    feed: Option<i64>,
    /// The page of the list, counting from 1.
    page: u32,
}

impl Listing {
    /// The path of the list, without a page.
    fn path(&self) -> String {
        match self.feed {
            Some(id) => format!("/feeds/{id}"),
            None => "/".to_owned(),
        }
    }

    /// The URL of this page of the list.
    fn href(&self) -> String {
        if self.page > 1 {
            format!("{}?page={}", self.path(), self.page)
        } else {
            self.path()
        }
    }

    /// The URL of entry `id`'s page, linking back to this page of the list,
    /// escaped for use in an attribute.
    fn entry_href(&self, id: i64) -> String {
        let mut query = Vec::new();
        if let Some(feed) = self.feed {
            query.push(format!("feed={feed}"));
        }
        if self.page > 1 {
            query.push(format!("page={}", self.page));
        }
        if query.is_empty() {
            format!("/entries/{id}")
        } else {
            format!("/entries/{id}?{}", query.join("&amp;"))
        }
    }
}

/// Fill the layout in [`PAGE_HTML`] with `title` (plain text, which is
/// escaped) and `content` (HTML), and wrap it in a response with `status`.
fn render_page(status: StatusCode, title: &str, content: &str) -> Response {
    // Fill every placeholder in one pass, so that a placeholder appearing in
    // a feed's title or content is left alone.
    let html = PLACEHOLDER.replace_all(PAGE_HTML, |caps: &regex::Captures| match &caps[1] {
        "title" => escape(title).into_owned(),
        "version" => env!("CARGO_PKG_VERSION").to_owned(),
        "content" => content.to_owned(),
        _ => caps[0].to_owned(),
    });
    (
        status,
        [
            (header::CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY),
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
/// neighbouring pages.
async fn index(State(api): State<reqwest::Client>, Query(params): Query<PageParams>) -> Response {
    let page = params.page();
    match fetch_entries(&api, page).await {
        Ok(entries) => {
            let feeds =
                fetch_feed_titles(&api, entries.entries.iter().filter_map(|e| e.feed_id)).await;
            let listing = Listing { feed: None, page };
            render_page(
                StatusCode::OK,
                "Kiki",
                &render_entries(entries.count, &entries.entries, &feeds, listing),
            )
        }
        Err(e) => server_unavailable(&e),
    }
}

/// Render the page for entry `id`: a summary of the entry built from what
/// its feed says about it, with a link through to the entry itself.
///
/// The page links back to the list of entries it was opened from: the
/// index, or a feed's page.
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
                    render_back_link(params.listing())
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
    let (feed_title, cached) = tokio::join!(feed_title, fetch_cached_assets(&api, id));
    render_page(
        StatusCode::OK,
        &format!("{} - Kiki", display_title(&entry.title)),
        &render_entry_page(&entry, feed_title.as_deref(), &cached, params.listing()),
    )
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
/// page of the entries retrieved from it, newest first.
async fn feed_page(
    State(api): State<reqwest::Client>,
    UrlPath(id): UrlPath<i64>,
    Query(params): Query<PageParams>,
) -> Response {
    let listing = Listing {
        feed: Some(id),
        page: params.page(),
    };
    let not_found = || {
        render_page(
            StatusCode::NOT_FOUND,
            "Feed not found - Kiki",
            "<p>Feed not found.</p>\n<p><a href=\"/feeds\">&larr; Back to feeds</a></p>\n",
        )
    };

    let (feed, entries) = tokio::join!(
        fetch_feed(&api, id),
        fetch_feed_entries(&api, id, listing.page)
    );
    let feed = match feed {
        Ok(Some(feed)) => feed,
        Ok(None) => return not_found(),
        Err(e) => return server_unavailable(&e),
    };
    let entries = match entries {
        Ok(Some(entries)) => entries,
        // The feed was deleted between the two requests.
        Ok(None) => return not_found(),
        Err(e) => return server_unavailable(&e),
    };

    render_page(
        StatusCode::OK,
        &format!("{} - Kiki", display_feed_title(&feed.title)),
        &render_feed_page(&feed, &entries, listing),
    )
}

/// Render the list of installed plugins, and of the directories in the
/// plugins directory that could not be loaded as plugins.
async fn plugins_page(State(api): State<reqwest::Client>) -> Response {
    match fetch_plugins(&api).await {
        Ok(plugins) => render_page(StatusCode::OK, "Plugins - Kiki", &render_plugins(&plugins)),
        Err(e) => server_unavailable(&e),
    }
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

/// Fetch page `page` (counting from 1) of the entries of feed `id` from the
/// Kiki API, or `None` if there is no such feed.
async fn fetch_feed_entries(
    api: &reqwest::Client,
    id: i64,
    page: u32,
) -> Result<Option<FeedEntriesResponse>> {
    let offset = u64::from(page - 1) * u64::from(PAGE_SIZE);
    let resp = api
        .get(format!(
            "{API_BASE}/v1/feeds/id/{id}/entries?offset={offset}&limit={PAGE_SIZE}"
        ))
        .send()
        .await?;
    if resp.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    Ok(Some(resp.error_for_status()?.json().await?))
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

/// The part of a `/v1/feeds/id/{id}` response the web UI uses.
#[derive(Deserialize)]
struct Feed {
    title: String,
    url: String,
    description: Option<String>,
    /// When the feed was last checked, in RFC 3339.
    last_checked: Option<String>,
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

/// Render the entry count (`count`, of all the entries in the list), the
/// entries on this page of `listing`, and the page links. `feeds` maps feed
/// IDs to the titles of the feeds; entries from feeds not in it are shown
/// without their feed.
fn render_entries(
    count: usize,
    entries: &[ListEntriesResponseEntry],
    feeds: &HashMap<i64, String>,
    listing: Listing,
) -> String {
    let mut html = format!(
        "<p class=\"count\">{} {}</p>\n",
        count,
        if count == 1 { "entry" } else { "entries" }
    );

    if entries.is_empty() {
        html.push_str(if count == 0 {
            "<p>No entries yet.</p>\n"
        } else {
            "<p>No entries on this page.</p>\n"
        });
    } else {
        html.push_str("<ol class=\"entries\">\n");
        for entry in entries {
            let feed = entry.feed_id.and_then(|id| feeds.get(&id));
            html.push_str("<li>");
            html.push_str(&render_entry(entry, feed.map(String::as_str), listing));
            html.push_str("</li>\n");
        }
        html.push_str("</ol>\n");
    }

    html.push_str(&render_pagination(
        count,
        listing.page,
        &listing.path(),
        ("&larr; Newer", "Older &rarr;"),
    ));
    html
}

/// Render a single entry in a list: its title, linked to the entry's page,
/// and below it its publication date and the title of `feed`, the feed it
/// came from. The entry's page links back to `listing`.
fn render_entry(entry: &ListEntriesResponseEntry, feed: Option<&str>, listing: Listing) -> String {
    let href = listing.entry_href(entry.id);
    let meta = render_meta(entry.published_at.as_deref(), feed, None);
    format!(
        "<a href=\"{href}\">{}</a>{meta}",
        escape(display_title(&entry.title))
    )
}

/// Render the page for `entry`: its title, date, feed (`feed`), author and
/// categories, its content from the feed, and links to the entry itself and
/// to anything else the feed links it to. The page links back to `listing`.
///
/// Images in the content, and the entry's attachment, are taken from the
/// asset cache: `cached` maps an asset's original URL to the URL of its
/// cached copy. Images that are not cached are shown as links instead, and
/// an attachment that is not cached is linked where the feed says it is.
fn render_entry_page(
    entry: &GetEntryResponse,
    feed: Option<&str>,
    cached: &HashMap<String, String>,
    listing: Listing,
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
        "<article class=\"entry\">\n<h2>{}</h2>\n{}\n",
        escape(display_title(&entry.title)),
        render_meta(
            entry.published_at.as_deref(),
            feed,
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
fn render_back_link(listing: Listing) -> String {
    let label = if listing.feed.is_some() {
        "Back to feed"
    } else {
        "Back to entries"
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
            );
            html.push_str(&format!(
                "<li><a href=\"/feeds/{}\">{}</a>{meta}</li>\n",
                feed.id,
                escape(display_feed_title(&feed.title))
            ));
        }
        html.push_str("</ol>\n");
    }

    html.push_str(&render_pagination(
        resp.count,
        page,
        "/feeds",
        ("&larr; Previous", "Next &rarr;"),
    ));
    html
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
        "<p class=\"meta\">Plugins installed or changed take effect after the server restarts.</p>\n",
    );
    html
}

/// Render a single plugin: its name and version, its description, and a
/// line with its engine, whether it runs, its authors, license and
/// homepage.
fn render_plugin(plugin: &PluginResponse) -> String {
    let mut html = format!(
        "<strong>{}</strong> <span class=\"version\">v{}</span>",
        escape(&plugin.name),
        escape(&plugin.version)
    );
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

/// Render the page for `feed`: its title, URL, description and when it was
/// last checked, then `entries`, the entries on this page of `listing`.
fn render_feed_page(feed: &Feed, entries: &FeedEntriesResponse, listing: Listing) -> String {
    let mut html = format!(
        "<header class=\"feed-header\">\n<h2>{}</h2>\n{}\n",
        escape(display_feed_title(&feed.title)),
        render_feed_meta(&feed.url, &feed.url, feed.last_checked.as_deref()),
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
        listing,
    ));
    html.push_str("<p><a href=\"/feeds\">&larr; Back to feeds</a></p>\n");
    html
}

/// Render the line under a feed's title: its URL (`url`), shown as `label`
/// but not linked, and when it was last checked (`last_checked`, in
/// RFC 3339). When `label` isn't the whole URL, the URL is its tooltip.
fn render_feed_meta(url: &str, label: &str, last_checked: Option<&str>) -> String {
    let mut parts = vec![if label == url {
        format!("<span class=\"url\">{}</span>", escape(url))
    } else {
        format!(
            "<span class=\"url\" title=\"{}\">{}</span>",
            escape(url),
            escape(label)
        )
    }];
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
fn render_meta(published_at: Option<&str>, feed: Option<&str>, author: Option<&str>) -> String {
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
            "<span class=\"feed\">{}</span>",
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
/// items at `path`, with links to the pages before and after it, labelled
/// with `labels`.
fn render_pagination(count: usize, page: u32, path: &str, labels: (&str, &str)) -> String {
    let (prev_label, next_label) = labels;
    let pages = count.div_ceil(PAGE_SIZE as usize).max(1);
    let page_usize = page as usize;

    let mut links = Vec::new();
    if page > 1 {
        // A page past the end links back to the last page, not to the
        // (equally empty) page before it.
        let prev = page_usize.min(pages + 1) - 1;
        links.push(format!(
            "<a href=\"{path}?page={prev}\" rel=\"prev\">{prev_label}</a>"
        ));
    }
    links.push(format!("Page {page} of {pages}"));
    if page_usize < pages {
        links.push(format!(
            "<a href=\"{path}?page={}\" rel=\"next\">{next_label}</a>",
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
        assert!(!body.contains("<script>"), "{body}");
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

    /// Pages forbid script, in case anything from a feed slips through, and
    /// ask the browser not to look up the hosts they link to.
    #[tokio::test]
    async fn pages_forbid_script() -> Result<()> {
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
            let csp = resp.headers()[header::CONTENT_SECURITY_POLICY].to_str()?;
            assert!(csp.starts_with("default-src 'none';"), "{path}: {csp}");
            assert!(!csp.contains("script-src"), "{path}: {csp}");
            assert_eq!(
                resp.headers()[header::X_DNS_PREFETCH_CONTROL],
                "off",
                "{path}"
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
        for path in ["/", "/entries/1", "/feeds", "/plugins"] {
            let (_, body) = get_page(tc.client()?, path).await?;
            assert!(
                body.contains(r#"<nav class="site-nav"><a href="/">Entries</a><a href="/feeds">Feeds</a><a href="/plugins">Plugins</a></nav>"#),
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
        assert!(body.contains("15 entries"), "{body}");
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
            body.contains(r#"<strong>passthrough</strong> <span class="version">v1.0.0</span>"#),
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
