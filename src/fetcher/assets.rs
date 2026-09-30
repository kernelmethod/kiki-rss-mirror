//! Downloading assets and looking for favicons, with no access to Kiki's
//! state.
//!
//! Caching an entry's images or a feed's favicon handles as much untrusted
//! input as fetching the feed itself: an HTTP exchange with an arbitrary
//! server, TLS, decompression, and HTML parsing — of the entry's content to
//! find its images, and of a website's home page to find its icons. As
//! for feeds, all of that lives here, as functions of their inputs alone,
//! so that it can run in the sandboxed feed fetcher process. What they
//! return is plain data: the server hashes, stores and indexes it
//! ([`crate::tasks::assets`]), after checking it again.

use super::svg::{sanitize_svg, SVG_CONTENT_TYPE};
use super::ProxiedClient;
use crate::config::ProxySettings;
use crate::http::{read_body_capped, CappedBody};
use lol_html::html_content::Element;
use lol_html::{element, HtmlRewriter, Settings};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::time::Duration;

/// Per-asset download cap. Enclosures can be large (podcasts); this is a
/// safety valve to prevent a single rogue asset from filling the cache.
pub const MAX_ASSET_BYTES: u64 = 32 * 1024 * 1024;

/// How much of a website's home page is read looking for icon links. They
/// belong in the `<head>`, so a prefix of the page is enough.
pub const MAX_PAGE_BYTES: usize = 512 * 1024;

/// Time limits on the requests that cache assets and look for favicons.
///
/// A feed fetch has a single overall timeout, but an asset may be a
/// [`MAX_ASSET_BYTES`] enclosure that a slow link cannot download in the
/// same time. So the limits are split: a server must accept the connection
/// within `connect`, and must never go `read` without sending anything,
/// which catches a server that has stalled; `total` is a looser cap on the
/// whole request, which catches one that sends just often enough to keep
/// `read` from firing. Without them, a server that never answers would tie
/// up a worker, and every task queued behind it, indefinitely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssetTimeouts {
    /// Longest wait to connect to a server (and through a proxy, if any).
    pub connect: Duration,
    /// Longest wait for any data at all, while waiting for the response or
    /// reading its body.
    pub read: Duration,
    /// Longest a request may take from start to finish, body included.
    pub total: Duration,
}

impl AssetTimeouts {
    /// The limits Kiki uses: 15 seconds to connect, 30 seconds without
    /// data, and 5 minutes in all, which is enough to download a
    /// [`MAX_ASSET_BYTES`] enclosure at about 1 Mbit/s.
    pub const DEFAULT: AssetTimeouts = AssetTimeouts {
        connect: Duration::from_secs(15),
        read: Duration::from_secs(30),
        total: Duration::from_secs(5 * 60),
    };
}

/// The client configuration for caching assets and looking for favicons:
/// feed fetches' ([`super::client_builder`]), with the limits in
/// `timeouts`. Build it with [`ProxiedClient`], so the proxy settings apply.
///
/// # Examples
///
/// ```
/// use kiki_rss::fetcher::assets::{asset_client_builder, AssetTimeouts};
/// use kiki_rss::fetcher::ProxiedClient;
///
/// let clients = ProxiedClient::new(|| asset_client_builder(AssetTimeouts::DEFAULT)).unwrap();
/// ```
pub fn asset_client_builder(timeouts: AssetTimeouts) -> reqwest::ClientBuilder {
    super::client_builder()
        .connect_timeout(timeouts.connect)
        .read_timeout(timeouts.read)
        .timeout(timeouts.total)
}

/// Kind of asset reference discovered in an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssetKind {
    InlineImg,
    Enclosure,
    /// The favicon of the website a feed belongs to.
    Favicon,
}

impl AssetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AssetKind::InlineImg => "inline_img",
            AssetKind::Enclosure => "enclosure",
            AssetKind::Favicon => "favicon",
        }
    }
}

/// Raster image MIME types we're willing to cache and serve as they are.
/// Deliberately excludes `image/svg+xml`: SVG is XML and can embed
/// executable script, which would run when a browser navigates directly to
/// the asset URL. SVG images are cached too, but only after
/// [`sanitize_svg`] has rebuilt them (see [`is_allowed_content_type`]).
const SAFE_IMAGE_TYPES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/avif",
    "image/bmp",
    "image/heic",
    "image/heif",
    "image/tiff",
    "image/x-icon",
    "image/vnd.microsoft.icon",
];

/// Parse a `Content-Type` header value into a normalized `type/subtype` form.
///
/// Strips media-type parameters (e.g. `; charset=utf-8`), trims whitespace,
/// and lowercases. Returns `None` when the input is empty or doesn't match
/// `type/subtype`.
pub fn normalize_content_type(raw: &str) -> Option<String> {
    let main = raw.split(';').next()?.trim();
    let (ty, subty) = main.split_once('/')?;
    let ty = ty.trim();
    let subty = subty.trim();
    if ty.is_empty()
        || subty.is_empty()
        || ty.chars().any(char::is_whitespace)
        || subty.chars().any(char::is_whitespace)
    {
        return None;
    }
    Some(format!(
        "{}/{}",
        ty.to_ascii_lowercase(),
        subty.to_ascii_lowercase()
    ))
}

/// Whether a normalized MIME type is acceptable to cache for the given kind.
///
/// Inline images and favicons must match one of the safe raster image
/// types, or be SVG, which is accepted only because [`fetch_asset`]
/// sanitizes it. Enclosures additionally accept audio and video types plus a
/// handful of common podcast-adjacent `application/*` types, but not SVG.
pub fn is_allowed_content_type(normalized: &str, kind: AssetKind) -> bool {
    if SAFE_IMAGE_TYPES.contains(&normalized) {
        return true;
    }
    match kind {
        AssetKind::InlineImg | AssetKind::Favicon => normalized == SVG_CONTENT_TYPE,
        AssetKind::Enclosure => {
            normalized.starts_with("audio/")
                || normalized.starts_with("video/")
                || matches!(normalized, "application/ogg" | "application/pdf")
        }
    }
}

/// Resolve `raw` against `base`, returning it only if the result is an
/// `http(s)` URL.
pub(crate) fn resolve_http_url(raw: &str, base: &Url) -> Option<Url> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let resolved = match Url::parse(trimmed) {
        Ok(u) => u,
        Err(url::ParseError::RelativeUrlWithoutBase) => base.join(trimmed).ok()?,
        Err(_) => return None,
    };
    match resolved.scheme() {
        "http" | "https" => Some(resolved),
        _ => None,
    }
}

/// Parse `content` as HTML and return every resolved `<img src>` URL whose
/// scheme is `http(s)`. Relative URLs are resolved against `base`. Duplicates
/// within a single entry are collapsed preserving first-seen order.
pub fn extract_asset_urls(content: &str, base: &Url) -> Vec<Url> {
    let out: RefCell<Vec<Url>> = RefCell::new(Vec::new());

    let collect = |el: &mut Element| {
        if let Some(src) = el.get_attribute("src") {
            if let Some(url) = resolve_http_url(&src, base) {
                let mut v = out.borrow_mut();
                if !v.iter().any(|u| u == &url) {
                    v.push(url);
                }
            }
        }
        Ok(())
    };

    {
        // lol_html rewriter is streaming but we don't actually rewrite — we
        // use it purely as a safe HTML parser. A no-op output sink is fine.
        let mut rewriter = HtmlRewriter::new(
            Settings {
                element_content_handlers: vec![element!("img", collect)],
                ..Settings::default()
            },
            |_: &[u8]| {},
        );

        if rewriter.write(content.as_bytes()).is_ok() {
            let _ = rewriter.end();
        }
        // Dropping `rewriter` here releases the borrow on `out`.
    }

    out.into_inner()
}

/// Everything needed to download one asset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetSpec {
    /// The asset's URL; `http(s)` only.
    pub url: String,

    /// What the asset is, which decides the content types accepted.
    pub kind: AssetKind,

    /// The proxy to download through, with environment overrides already
    /// applied by the server.
    #[serde(with = "super::proxy_wire")]
    pub proxy: ProxySettings,
}

/// How an asset download ended.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AssetReply {
    /// A successful response of an allowed type, read in full.
    Fetched(Box<FetchedAsset>),

    /// No response was received (connection failure, TLS error, timeout),
    /// or the client could not be configured for the proxy.
    Network { message: String },

    /// Any status other than a success.
    HttpStatus { status: u16 },

    /// The response's `Content-Type` was missing, or not one cached for
    /// the asset's kind.
    DisallowedType { content_type: Option<String> },

    /// The body exceeded [`MAX_ASSET_BYTES`].
    TooLarge { seen: u64 },

    /// An SVG image that could not be made safe to serve: it was not a
    /// well-formed SVG document, or it was over [`super::svg::MAX_SVG_BYTES`].
    UnsafeSvg,

    /// The body could not be read to the end.
    Failed { message: String },
}

/// A downloaded asset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchedAsset {
    /// The normalized `Content-Type`; see [`normalize_content_type`].
    pub content_type: String,

    pub etag: Option<String>,

    pub last_modified: Option<String>,

    /// Encoded as one length and the raw bytes, not byte by byte.
    #[serde(with = "serde_bytes")]
    pub bytes: Vec<u8>,
}

/// Download the asset described by `spec`, with the client for its proxy
/// settings.
pub async fn fetch_asset(clients: &ProxiedClient, spec: &AssetSpec) -> AssetReply {
    let Some(url) = Url::parse(&spec.url)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https"))
    else {
        return AssetReply::Failed {
            message: "not an http(s) URL".to_string(),
        };
    };
    let client = match clients.get(&spec.proxy) {
        Ok(c) => c,
        Err(_) => {
            return AssetReply::Network {
                message: "could not configure the HTTP client for the proxy".to_string(),
            }
        }
    };

    let resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => {
            return AssetReply::Network {
                message: e.to_string(),
            }
        }
    };
    if !resp.status().is_success() {
        return AssetReply::HttpStatus {
            status: resp.status().as_u16(),
        };
    }

    let raw_content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let content_type = match raw_content_type.and_then(normalize_content_type) {
        Some(ct) if is_allowed_content_type(&ct, spec.kind) => ct,
        _ => {
            return AssetReply::DisallowedType {
                content_type: raw_content_type.map(str::to_string),
            }
        }
    };
    let header = |name| {
        resp.headers()
            .get(name)
            .and_then(|v: &reqwest::header::HeaderValue| v.to_str().ok())
            .map(str::to_string)
    };
    let etag = header(reqwest::header::ETAG);
    let last_modified = header(reqwest::header::LAST_MODIFIED);

    // Streamed under the cap: checking `Content-Length` up front and then
    // calling `bytes()` would still buffer the whole body for a server
    // that lies about (or omits) the header.
    match read_body_capped(resp, MAX_ASSET_BYTES).await {
        Ok(CappedBody::Complete(bytes)) => {
            // What is served is the rebuilt SVG, never the original.
            let bytes = if content_type == SVG_CONTENT_TYPE {
                match sanitize_svg(&bytes) {
                    Some(clean) => clean,
                    None => return AssetReply::UnsafeSvg,
                }
            } else {
                bytes
            };
            AssetReply::Fetched(Box::new(FetchedAsset {
                content_type,
                etag,
                last_modified,
                bytes,
            }))
        }
        Ok(CappedBody::TooLarge { seen }) => AssetReply::TooLarge { seen },
        Err(e) => AssetReply::Failed {
            message: e.to_string(),
        },
    }
}

/// Everything needed to look for the icons a web page links to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageSpec {
    /// The page's URL; `http(s)` only.
    pub url: String,

    /// The proxy to fetch through, as for [`AssetSpec::proxy`].
    #[serde(with = "super::proxy_wire")]
    pub proxy: ProxySettings,
}

/// The icons a web page links to.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PageIcons {
    /// The icons' URLs, best first; see [`extract_icon_links`].
    pub icons: Vec<String>,

    /// The URL the page was finally served from, if a redirect moved it to
    /// another origin.
    pub landed_on: Option<String>,

    /// What went wrong reading the page, if anything. Only for logging.
    pub problem: Option<PageProblem>,
}

/// Why a page could not be (fully) searched for icons.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PageProblem {
    /// No response was received, or the client could not be configured.
    Unreachable(String),

    /// Any status other than a success.
    Status(u16),

    /// The body could not be read to the end; icons linked from what was
    /// read are still returned.
    ReadFailed(String),
}

/// Fetch the web page in `spec` and return the icons it links to, best
/// first. Any failure yields no icons.
pub async fn find_page_icons(clients: &ProxiedClient, spec: &PageSpec) -> PageIcons {
    let failed = |error: String| PageIcons {
        problem: Some(PageProblem::Unreachable(error)),
        ..PageIcons::default()
    };
    let Some(page) = Url::parse(&spec.url)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https"))
    else {
        return failed("not an http(s) URL".to_string());
    };
    let client = match clients.get(&spec.proxy) {
        Ok(c) => c,
        Err(_) => return failed("could not configure the HTTP client for the proxy".into()),
    };

    let mut resp = match client
        .get(page.clone())
        .header(
            reqwest::header::ACCEPT,
            "text/html,application/xhtml+xml;q=0.9",
        )
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return failed(e.to_string()),
    };
    let final_url = resp.url().clone();
    let landed_on = (final_url.origin() != page.origin()).then(|| final_url.to_string());

    if !resp.status().is_success() {
        return PageIcons {
            landed_on,
            problem: Some(PageProblem::Status(resp.status().as_u16())),
            ..PageIcons::default()
        };
    }
    let is_html = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(normalize_content_type)
        .is_some_and(|t| t == "text/html" || t == "application/xhtml+xml");
    if !is_html {
        return PageIcons {
            landed_on,
            ..PageIcons::default()
        };
    }

    // Only the start of the page is needed; stop reading once there is
    // enough of it.
    let mut body: Vec<u8> = Vec::new();
    let mut problem = None;
    while body.len() < MAX_PAGE_BYTES {
        match resp.chunk().await {
            Ok(Some(chunk)) => body.extend_from_slice(&chunk),
            Ok(None) => break,
            Err(e) => {
                problem = Some(PageProblem::ReadFailed(e.to_string()));
                break;
            }
        }
    }
    body.truncate(MAX_PAGE_BYTES);

    PageIcons {
        icons: extract_icon_links(&body, &final_url)
            .into_iter()
            .map(String::from)
            .collect(),
        landed_on,
        problem,
    }
}

/// An icon linked from a page, with what is needed to rank it.
struct IconLink {
    url: Url,
    /// 0 for `rel="icon"`, 1 for Apple touch icons, which are larger than
    /// needed but better than nothing.
    rel_rank: u8,
    size_rank: u32,
}

/// Parse `html` and return the URLs of the icons it links to with
/// `<link rel="icon">` (including `shortcut icon`) or
/// `<link rel="apple-touch-icon">`, best first, resolving them against
/// `page_url` or the page's `<base href>`.
///
/// `rel="icon"` comes before touch icons, and the smallest icon at least 32
/// pixels wide is preferred. Icons declared as SVG, which are sanitized
/// before they are cached, rank last among those of the same `rel`, as a
/// fallback for when a raster icon cannot be had.
pub(crate) fn extract_icon_links(html: &[u8], page_url: &Url) -> Vec<Url> {
    let base = RefCell::new(page_url.clone());
    let icons: RefCell<Vec<IconLink>> = RefCell::new(Vec::new());

    {
        let mut rewriter = HtmlRewriter::new(
            Settings {
                element_content_handlers: vec![
                    element!("base[href]", |el| {
                        if let Some(href) = el.get_attribute("href") {
                            let resolved = resolve_http_url(&href, &base.borrow());
                            if let Some(resolved) = resolved {
                                *base.borrow_mut() = resolved;
                            }
                        }
                        Ok(())
                    }),
                    element!("link[rel][href]", |el| {
                        let rel = el.get_attribute("rel").unwrap_or_default();
                        let rels: Vec<String> = rel
                            .split_ascii_whitespace()
                            .map(str::to_ascii_lowercase)
                            .collect();
                        let rel_rank = if rels.iter().any(|r| r == "icon") {
                            0
                        } else if rels
                            .iter()
                            .any(|r| r == "apple-touch-icon" || r == "apple-touch-icon-precomposed")
                        {
                            1
                        } else {
                            return Ok(());
                        };
                        let declared_svg = el
                            .get_attribute("type")
                            .and_then(|t| normalize_content_type(&t))
                            .is_some_and(|t| t == SVG_CONTENT_TYPE);
                        let href = el.get_attribute("href").unwrap_or_default();
                        let Some(url) = resolve_http_url(&href, &base.borrow()) else {
                            return Ok(());
                        };
                        let is_svg =
                            declared_svg || url.path().to_ascii_lowercase().ends_with(".svg");
                        let size_rank = if is_svg {
                            u32::MAX
                        } else {
                            size_rank(el.get_attribute("sizes").as_deref())
                        };
                        icons.borrow_mut().push(IconLink {
                            url,
                            rel_rank,
                            size_rank,
                        });
                        Ok(())
                    }),
                ],
                ..Settings::default()
            },
            |_: &[u8]| {},
        );
        if rewriter.write(html).is_ok() {
            let _ = rewriter.end();
        }
    }

    let mut icons = icons.into_inner();
    // Stable, so that equally good icons keep the page's order.
    icons.sort_by_key(|i| (i.rel_rank, i.size_rank));
    icons.into_iter().map(|i| i.url).collect()
}

/// Rank an icon by its `sizes` attribute, lower being better: the smallest
/// icon at least 32 pixels wide is best, then larger ones, then smaller
/// ones, largest first. An icon without `sizes` is assumed to be the usual
/// 32 pixels; `sizes="any"` (almost always an SVG) ranks last.
fn size_rank(sizes: Option<&str>) -> u32 {
    const PREFERRED: u32 = 32;
    const WORST: u32 = u32::MAX;
    let Some(sizes) = sizes.map(str::trim).filter(|s| !s.is_empty()) else {
        return PREFERRED;
    };
    if sizes.eq_ignore_ascii_case("any") {
        return WORST;
    }
    // Several sizes may be listed, for an `.ico` holding more than one
    // image; the largest decides how well it will scale down.
    let width = sizes
        .split_ascii_whitespace()
        .filter_map(|s| {
            let s = s.to_ascii_lowercase();
            let (w, _) = s.split_once('x')?;
            w.parse::<u32>().ok()
        })
        .max();
    match width {
        Some(w) if w >= PREFERRED => w,
        // Below the preferred size: after every larger icon, bigger first.
        Some(w) => 1_000_000 - w,
        None => PREFERRED,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("http://example.com/feed").unwrap()
    }

    #[test]
    fn extract_absolute_img_urls() {
        let html = r#"<p>hi</p><img src="http://a.test/1.png"><img src="https://b.test/2.jpg">"#;
        let urls = extract_asset_urls(html, &base());
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0].as_str(), "http://a.test/1.png");
        assert_eq!(urls[1].as_str(), "https://b.test/2.jpg");
    }

    #[test]
    fn relative_img_resolved_against_base() {
        let html = r#"<img src="/img.png"><img src="sub/x.gif">"#;
        let urls = extract_asset_urls(html, &base());
        assert_eq!(urls[0].as_str(), "http://example.com/img.png");
        assert_eq!(urls[1].as_str(), "http://example.com/sub/x.gif");
    }

    #[test]
    fn data_and_non_http_schemes_dropped() {
        let html = r#"<img src="data:image/png;base64,AAAA"><img src="javascript:alert(1)"><img src="ftp://x/x.png">"#;
        let urls = extract_asset_urls(html, &base());
        assert!(urls.is_empty());
    }

    #[test]
    fn duplicate_img_urls_deduped() {
        let html = r#"<img src="http://a.test/x.png"><img src="http://a.test/x.png">"#;
        let urls = extract_asset_urls(html, &base());
        assert_eq!(urls.len(), 1);
    }

    #[test]
    fn malformed_img_html_does_not_crash() {
        let html = r#"<img src="http://a.test/x.png" <unclosed <img src="http://b.test/y.png">"#;
        let _ = extract_asset_urls(html, &base());
    }

    #[test]
    fn normalize_content_type_strips_params_and_lowercases() {
        assert_eq!(
            normalize_content_type("image/PNG").as_deref(),
            Some("image/png")
        );
        assert_eq!(
            normalize_content_type("image/jpeg; charset=utf-8").as_deref(),
            Some("image/jpeg")
        );
        assert_eq!(
            normalize_content_type("  Image/Jpeg ;boundary=x  ").as_deref(),
            Some("image/jpeg")
        );
    }

    #[test]
    fn normalize_content_type_rejects_garbage() {
        assert!(normalize_content_type("").is_none());
        assert!(normalize_content_type("notatype").is_none());
        assert!(normalize_content_type("image/").is_none());
        assert!(normalize_content_type("/png").is_none());
        // Internal whitespace inside a token is rejected.
        assert!(normalize_content_type("image/pn g").is_none());
    }

    #[test]
    fn inline_img_allowlist_rejects_html_but_not_sanitized_svg() {
        assert!(is_allowed_content_type("image/png", AssetKind::InlineImg));
        assert!(is_allowed_content_type("image/jpeg", AssetKind::InlineImg));
        assert!(is_allowed_content_type("image/webp", AssetKind::InlineImg));
        assert!(!is_allowed_content_type("text/html", AssetKind::InlineImg));
        assert!(!is_allowed_content_type(
            "application/javascript",
            AssetKind::InlineImg
        ));
        assert!(is_allowed_content_type(
            "image/svg+xml",
            AssetKind::InlineImg
        ));
        assert!(!is_allowed_content_type("audio/mpeg", AssetKind::InlineImg));
    }

    #[test]
    fn favicon_allowlist_is_images_only() {
        assert!(is_allowed_content_type("image/x-icon", AssetKind::Favicon));
        assert!(is_allowed_content_type(
            "image/vnd.microsoft.icon",
            AssetKind::Favicon
        ));
        assert!(is_allowed_content_type("image/png", AssetKind::Favicon));
        assert!(is_allowed_content_type("image/svg+xml", AssetKind::Favicon));
        assert!(!is_allowed_content_type("text/html", AssetKind::Favicon));
        assert!(!is_allowed_content_type("audio/mpeg", AssetKind::Favicon));
    }

    #[test]
    fn enclosure_allowlist_accepts_media_types() {
        assert!(is_allowed_content_type("audio/mpeg", AssetKind::Enclosure));
        assert!(is_allowed_content_type("audio/ogg", AssetKind::Enclosure));
        assert!(is_allowed_content_type("video/mp4", AssetKind::Enclosure));
        assert!(is_allowed_content_type("image/png", AssetKind::Enclosure));
        assert!(is_allowed_content_type(
            "application/pdf",
            AssetKind::Enclosure
        ));
        assert!(!is_allowed_content_type("text/html", AssetKind::Enclosure));
        assert!(!is_allowed_content_type(
            "image/svg+xml",
            AssetKind::Enclosure
        ));
    }

    fn page() -> Url {
        Url::parse("https://example.com/blog/").unwrap()
    }

    fn icons(html: &str) -> Vec<String> {
        extract_icon_links(html.as_bytes(), &page())
            .into_iter()
            .map(|u| u.to_string())
            .collect()
    }

    #[test]
    fn finds_icon_and_shortcut_icon() {
        assert_eq!(
            icons(
                r#"<head><link rel="stylesheet" href="/s.css">
                <link rel="shortcut icon" href="/favicon.png"></head>"#
            ),
            ["https://example.com/favicon.png"]
        );
        assert_eq!(
            icons(r#"<link rel="ICON" href="img/i.ico">"#),
            ["https://example.com/blog/img/i.ico"]
        );
    }

    #[test]
    fn prefers_icon_over_touch_icon() {
        assert_eq!(
            icons(
                r#"<link rel="apple-touch-icon" href="/touch.png">
                <link rel="icon" href="/icon.png">"#
            ),
            [
                "https://example.com/icon.png",
                "https://example.com/touch.png"
            ]
        );
    }

    #[test]
    fn prefers_smallest_icon_of_at_least_32px() {
        assert_eq!(
            icons(
                r#"<link rel="icon" sizes="16x16" href="/16.png">
                <link rel="icon" sizes="192x192" href="/192.png">
                <link rel="icon" sizes="32x32" href="/32.png">
                <link rel="icon" sizes="any" href="/any.png">"#
            ),
            [
                "https://example.com/32.png",
                "https://example.com/192.png",
                "https://example.com/16.png",
                "https://example.com/any.png",
            ]
        );
    }

    #[test]
    fn ranks_svg_icons_last() {
        assert_eq!(
            icons(
                r#"<link rel="icon" type="image/svg+xml" href="/icon">
                <link rel="icon" href="/icon.SVG">
                <link rel="mask-icon" href="/mask.png">
                <link rel="apple-touch-icon" href="/touch.png">
                <link rel="icon" href="/icon.png">"#
            ),
            [
                "https://example.com/icon.png",
                "https://example.com/icon",
                "https://example.com/icon.SVG",
                "https://example.com/touch.png",
            ]
        );
    }

    #[test]
    fn honors_base_href_and_drops_non_http() {
        assert_eq!(
            icons(
                r#"<base href="https://cdn.example.net/assets/">
                <link rel="icon" href="fav.ico">
                <link rel="icon" href="data:image/png;base64,AAAA">
                <link rel="icon" href="javascript:alert(1)">"#
            ),
            ["https://cdn.example.net/assets/fav.ico"]
        );
    }

    #[test]
    fn malformed_icon_html_does_not_crash() {
        let _ = icons(r#"<link rel="icon" href="/a.png" <link rel=icon href=/b.png"#);
    }

    #[test]
    fn size_rank_orders_sizes() {
        assert!(size_rank(Some("32x32")) < size_rank(Some("64x64")));
        assert!(size_rank(Some("512x512")) < size_rank(Some("16x16")));
        assert!(size_rank(Some("16x16")) < size_rank(Some("any")));
        assert_eq!(size_rank(None), size_rank(Some("32X32")));
        assert_eq!(size_rank(Some("16x16 48x48")), 48);
        assert_eq!(size_rank(Some("garbage")), size_rank(None));
    }

    #[test]
    fn asset_bodies_are_encoded_as_raw_bytes() {
        let reply = AssetReply::Fetched(Box::new(FetchedAsset {
            content_type: "image/png".into(),
            etag: None,
            last_modified: None,
            bytes: vec![0xff; 4096],
        }));
        let encoded = crate::process::ipc::encode(&reply).unwrap();
        assert!(encoded.len() <= 4096 + 32, "{} bytes", encoded.len());
    }
}
