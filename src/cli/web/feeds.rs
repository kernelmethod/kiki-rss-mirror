use super::api::{fetch_entries, fetch_feed, fetch_feeds, EntryPage, Feed};
use super::entries::render_entries;
use super::layout::{render_page, server_unavailable};
use super::listing::{render_pagination, Listing, PageParams};
use super::login::Api;
use crate::routes::v1::feeds::list_feeds::ListFeedsResponse;
use axum::{
    extract::{Path as UrlPath, Query},
    http::StatusCode,
    response::Response,
};
use quick_xml::escape::escape;

/// Render the list of feeds: the total number of feeds, and one page of
/// them, each linked to its page.
pub(super) async fn feeds_page(api: Api, Query(params): Query<PageParams>) -> Response {
    let page = params.page();
    match fetch_feeds(&api, page).await {
        Ok(feeds) => render_page(StatusCode::OK, "Feeds - Kiki", &render_feeds(&feeds, page)),
        Err(e) => server_unavailable(&e),
    }
}

/// Render the page for feed `id`: what the feed says about itself, and one
/// page of the entries retrieved from it, newest first. Read entries are
/// left out unless the `show_read` query parameter is true.
pub(super) async fn feed_page(
    api: Api,
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

    render_page(
        StatusCode::OK,
        &format!("{} - Kiki", display_feed_title(&feed.title)),
        &render_feed_page(&feed, &entries, &listing),
    )
}

/// Render the feed count, the feeds on page `page`, each linked to its
/// page, and the page links.
pub(super) fn render_feeds(resp: &ListFeedsResponse, page: u32) -> String {
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

/// Render the page for `feed`: its title, URL, description and when it was
/// last checked, then `entries`, the entries on this page of `listing`.
pub(super) fn render_feed_page(feed: &Feed, entries: &EntryPage, listing: &Listing) -> String {
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
        false,
        listing,
    ));
    html.push_str("<p><a href=\"/feeds\">&larr; Back to feeds</a></p>\n");
    html
}

/// The icon on the button that copies a feed's URL.
pub(super) const COPY_ICON: &str = "<svg viewBox=\"0 0 24 24\" aria-hidden=\"true\"><rect x=\"9\" y=\"9\" width=\"11\" height=\"11\" rx=\"2\"/><path d=\"M5 15V6a2 2 0 0 1 2-2h9\"/></svg>";

/// Render the line under a feed's title: its URL (`url`), shown as `label`
/// but not linked, and when it was last checked (`last_checked`, in
/// RFC 3339). When `label` isn't the whole URL, the URL is its tooltip. If
/// `copyable`, a button next to the URL copies the whole URL to the clipboard.
pub(super) fn render_feed_meta(
    url: &str,
    label: &str,
    last_checked: Option<&str>,
    copyable: bool,
) -> String {
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
pub(super) fn render_meta(
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
pub(super) fn render_favicon(api_url: Option<&str>) -> String {
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
pub(super) fn display_title(title: &str) -> &str {
    if title.trim().is_empty() {
        "(untitled)"
    } else {
        title
    }
}

/// `title`, a feed's title, or a placeholder if it is blank.
pub(super) fn display_feed_title(title: &str) -> &str {
    if title.trim().is_empty() {
        "(untitled feed)"
    } else {
        title
    }
}

/// Return the domain of `url`, or all of `url` if it has none (or doesn't
/// parse), so that there is always something to show.
pub(super) fn url_domain(url: &str) -> String {
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
pub(super) fn safe_link(url: &str) -> Option<&str> {
    let parsed = url::Url::parse(url).ok()?;
    matches!(parsed.scheme(), "http" | "https").then_some(url)
}
