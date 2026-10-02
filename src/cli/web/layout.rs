use axum::{
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
};
use quick_xml::escape::escape;
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

/// The layout every page is rendered into; see [`render_page`].
pub(super) const PAGE_HTML: &str = include_str!("page.html");

/// Matches the `{{name}}` placeholders in [`PAGE_HTML`].
pub(super) static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used)]
    Regex::new(r"\{\{(\w+)\}\}").expect("placeholder regex is valid")
});

/// The script every page runs, inlined into [`PAGE_HTML`]. It powers the
/// save buttons shown with each entry, the filter menus on lists of entries,
/// the search box and its popup on narrow screens, and the buttons that mark
/// entries as read or delete a tag.
pub(super) const PAGE_JS: &str = include_str!("page.js");

/// The `script-src` directive that lets pages run [`PAGE_JS`] and nothing
/// else: the script is allowed by its SHA-256 hash, so neither inline
/// script that slips through from a feed, nor a script served from the
/// asset cache, can run.
pub(super) static SCRIPT_SRC: LazyLock<String> = LazyLock::new(|| {
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
pub(super) static CONTENT_SECURITY_POLICY: LazyLock<String> = LazyLock::new(|| {
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
pub(super) static FORM_CONTENT_SECURITY_POLICY: LazyLock<String> = LazyLock::new(|| {
    format!(
        "default-src 'none'; img-src 'self'; style-src 'unsafe-inline'; {}; \
         connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        *SCRIPT_SRC
    )
});

/// `Content-Security-Policy` sent with cached assets. They were downloaded
/// from feeds, and are served from the web UI's origin, so one opened on
/// its own — an SVG, say — must not be able to run script there either.
pub(super) const ASSET_CONTENT_SECURITY_POLICY: &str = "default-src 'none'; img-src 'self'; \
    style-src 'unsafe-inline'; sandbox";

/// Fill the layout in [`PAGE_HTML`] with `title` (plain text, which is
/// escaped) and `content` (HTML), and wrap it in a response with `status`.
pub(super) fn render_page(status: StatusCode, title: &str, content: &str) -> Response {
    render_page_with_csp(status, title, content, &CONTENT_SECURITY_POLICY)
}

/// [`render_page`], for a page of search results: the search box in the
/// layout is filled in with `query`, what was searched for.
pub(super) fn render_search_page(
    status: StatusCode,
    title: &str,
    query: &str,
    content: &str,
) -> Response {
    render_layout(status, title, query, content, &CONTENT_SECURITY_POLICY)
}

/// [`render_page`], for a page with forms: the page is sent with
/// [`FORM_CONTENT_SECURITY_POLICY`], so that its forms can be submitted.
pub(super) fn render_form_page(status: StatusCode, title: &str, content: &str) -> Response {
    render_page_with_csp(status, title, content, &FORM_CONTENT_SECURITY_POLICY)
}

/// [`render_page`], sent with the `Content-Security-Policy` `csp`.
pub(super) fn render_page_with_csp(
    status: StatusCode,
    title: &str,
    content: &str,
    csp: &str,
) -> Response {
    render_layout(status, title, "", content, csp)
}

/// Fill the layout in [`PAGE_HTML`] with `title` and `query` (plain text,
/// which is escaped; `query` goes in the search box) and `content` (HTML),
/// and wrap it in a response with `status`, sent with the
/// `Content-Security-Policy` `csp`.
pub(super) fn render_layout(
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
pub(super) fn server_unavailable(e: &anyhow::Error) -> Response {
    tracing::warn!("failed to reach the Kiki server: {e:#}");
    render_page(
        StatusCode::BAD_GATEWAY,
        "Kiki",
        "<p>The Kiki server is unavailable.</p>",
    )
}

/// `value` as compact JSON.
pub(super) fn to_json(value: &Value) -> String {
    value.to_string()
}

/// `value` as JSON spread over several lines.
pub(super) fn to_json_pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}
