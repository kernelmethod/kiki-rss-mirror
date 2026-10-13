//! Fetching web pages for plugins, with no access to Kiki's state.
//!
//! A plugin with the `network` permission may ask for a web page with the
//! host's `fetch-page`, such as the article a feed entry links to (see
//! [`crate::plugins::services`]). The page comes from a server Kiki has no
//! reason to trust, at a URL that may have come from a feed, so the work
//! is the same kind as fetching a feed, and lives here for the same
//! reason: so that it can run in the sandboxed feed fetcher. Nothing is
//! parsed: the page goes back to the plugin as text, which it parses
//! itself, in the script host.

use super::assets::normalize_content_type;
use super::{ProxiedClient, MAX_REDIRECTS};
use crate::config::ProxySettings;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// The most of a page that is read. A longer page is cut short, and
/// [`FetchedPage::truncated`] says so: HTML parses as well cut short, and
/// an article's text comes before most of what makes a page long.
pub const MAX_PAGE_FETCH_BYTES: usize = 2 * 1024 * 1024;

/// Longest a page fetch may take, from the first request to the end of the
/// body, redirects included.
pub const PAGE_FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// Everything needed to fetch one page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageFetchSpec {
    /// The page's URL; `http(s)` only.
    pub url: String,

    /// The proxy to fetch through, with environment overrides already
    /// applied by the server.
    #[serde(with = "super::proxy_wire")]
    pub proxy: ProxySettings,
}

/// A page, fetched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchedPage {
    /// The URL the page was served from, after any redirects.
    pub url: String,
    /// The normalized `Content-Type`: `text/html` or
    /// `application/xhtml+xml`.
    pub content_type: String,
    /// The page, decoded to UTF-8 from the character encoding it declares.
    pub body: String,
    /// Whether the page was longer than [`MAX_PAGE_FETCH_BYTES`], and was
    /// cut short.
    pub truncated: bool,
}

/// Fetch the page in `spec` with the client for its proxy settings (the
/// one for assets, [`super::assets::asset_client_builder`]), following up
/// to [`MAX_REDIRECTS`] redirects to other `http(s)` URLs, within
/// [`PAGE_FETCH_TIMEOUT`].
///
/// # Errors
///
/// Fails with a message for the plugin if the URL is not `http(s)`, the
/// server cannot be reached, it answers with anything but a success, or
/// what it sends is not an HTML page.
pub async fn fetch_page(
    clients: &ProxiedClient,
    spec: &PageFetchSpec,
) -> Result<FetchedPage, String> {
    match tokio::time::timeout(PAGE_FETCH_TIMEOUT, fetch(clients, spec)).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "the page took longer than {}s to fetch",
            PAGE_FETCH_TIMEOUT.as_secs()
        )),
    }
}

async fn fetch(clients: &ProxiedClient, spec: &PageFetchSpec) -> Result<FetchedPage, String> {
    let mut url = http_url(&spec.url).ok_or("not an http(s) URL")?;
    let client = clients
        .get(&spec.proxy)
        .map_err(|_| "could not configure the HTTP client for the proxy".to_string())?;

    let mut redirects = 0;
    let mut resp = loop {
        let resp = client
            .get(url.clone())
            .header(
                reqwest::header::ACCEPT,
                "text/html,application/xhtml+xml;q=0.9",
            )
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_redirection() {
            break resp;
        }
        if redirects == MAX_REDIRECTS {
            return Err(format!("more than {MAX_REDIRECTS} redirects"));
        }
        redirects += 1;
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| format!("a {} redirect with no Location", resp.status().as_u16()))?;
        url = url
            .join(location.trim())
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .ok_or("redirected to a URL that is not http(s)")?;
    };

    if !resp.status().is_success() {
        return Err(format!("the server answered {}", resp.status()));
    }
    let raw_content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let content_type = raw_content_type
        .as_deref()
        .and_then(normalize_content_type)
        .filter(|t| t == "text/html" || t == "application/xhtml+xml")
        .ok_or_else(|| match &raw_content_type {
            Some(t) => format!("not an HTML page: its content type is {t:?}"),
            None => "not an HTML page: it has no content type".to_string(),
        })?;

    let mut body: Vec<u8> = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_PAGE_FETCH_BYTES {
            body.truncate(MAX_PAGE_FETCH_BYTES);
            truncated = true;
            break;
        }
    }

    Ok(FetchedPage {
        url: resp.url().to_string(),
        body: decode(&body, raw_content_type.as_deref()),
        content_type,
        truncated,
    })
}

/// `raw` parsed as an `http(s)` URL, or `None`.
fn http_url(raw: &str) -> Option<Url> {
    Url::parse(raw.trim())
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https"))
}

/// How much of a page is searched for a `<meta>` declaring its encoding, as
/// browsers do.
const META_PRESCAN_BYTES: usize = 1024;

/// `body` decoded to UTF-8: from the encoding its byte order mark names, or
/// else the `charset` of `content_type`, or else a `charset` declared near
/// its start, as in `<meta charset="...">`, or else as UTF-8. Bytes that do
/// not decode become U+FFFD.
///
/// # Examples
///
/// ```
/// use kiki_rss::fetcher::page::decode;
///
/// assert_eq!(decode(b"caf\xe9", Some("text/html; charset=iso-8859-1")), "café");
/// assert_eq!(decode(b"<meta charset=windows-1252>\x93", None), "<meta charset=windows-1252>\u{201c}");
/// assert_eq!(decode("café".as_bytes(), None), "café");
/// ```
pub fn decode(body: &[u8], content_type: Option<&str>) -> String {
    let declared = content_type
        .and_then(charset_param)
        .or_else(|| meta_charset(body.get(..META_PRESCAN_BYTES).unwrap_or(body)))
        .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    // `decode` follows a byte order mark over the declared encoding.
    let (text, _, _) = declared.decode(body);
    text.into_owned()
}

/// The `charset` parameter of a `Content-Type` header value.
fn charset_param(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|param| {
        let (name, value) = param.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches(['"', '\'']).to_string())
    })
}

/// The encoding named by the first `charset=` in `prefix`, the start of a
/// page: in a `<meta charset>`, or in the `content` of a `<meta
/// http-equiv="Content-Type">`.
fn meta_charset(prefix: &[u8]) -> Option<String> {
    let lower = prefix.to_ascii_lowercase();
    let at = lower.windows(8).position(|w| w == b"charset=")? + 8;
    let rest = lower.get(at..)?;
    let rest = rest
        .strip_prefix(b"\"")
        .or_else(|| rest.strip_prefix(b"'"))
        .unwrap_or(rest);
    let len = rest
        .iter()
        .position(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'.')))
        .unwrap_or(rest.len());
    let label = std::str::from_utf8(rest.get(..len)?).ok()?;
    (!label.is_empty()).then(|| label.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn charsets_come_from_the_header_then_the_page() {
        assert_eq!(
            charset_param("text/html; Charset=\"Shift_JIS\"").as_deref(),
            Some("Shift_JIS")
        );
        assert_eq!(charset_param("text/html"), None);
        assert_eq!(
            meta_charset(
                br#"<meta http-equiv="Content-Type" content="text/html; charset=koi8-r">"#
            )
            .as_deref(),
            Some("koi8-r")
        );
        assert_eq!(
            meta_charset(b"<meta charset='utf-8'>").as_deref(),
            Some("utf-8")
        );
        assert_eq!(meta_charset(b"<p>no charset</p>"), None);

        // The header wins over the page.
        let page = b"<meta charset=windows-1252>\xe9";
        assert_eq!(
            decode(page, Some("text/html; charset=utf-8")),
            "<meta charset=windows-1252>\u{FFFD}"
        );
        // A byte order mark wins over both.
        let page = b"\xef\xbb\xbf<meta charset=windows-1252>\xc3\xa9";
        assert_eq!(
            decode(page, Some("text/html; charset=iso-8859-1")),
            "<meta charset=windows-1252>é"
        );
        // An unknown label falls back to UTF-8.
        assert_eq!(
            decode("é".as_bytes(), Some("text/html; charset=nonsense")),
            "é"
        );
    }

    /// Serve a little website, returning its base URL.
    async fn start_site() -> String {
        use axum::http::header::{CONTENT_TYPE, LOCATION};
        use axum::http::StatusCode;
        use axum::routing::get;
        use axum::Router;
        let app = Router::new()
            .route(
                "/article",
                get(|| async {
                    (
                        [(CONTENT_TYPE, "text/html; charset=windows-1252")],
                        b"<p>\x93Hi\x94</p>".to_vec(),
                    )
                }),
            )
            .route(
                "/moved",
                get(|| async { (StatusCode::MOVED_PERMANENTLY, [(LOCATION, "article")]) }),
            )
            .route(
                "/loop",
                get(|| async { (StatusCode::FOUND, [(LOCATION, "/loop")]) }),
            )
            .route(
                "/elsewhere",
                get(|| async { (StatusCode::FOUND, [(LOCATION, "file:///etc/passwd")]) }),
            )
            .route(
                "/image",
                get(|| async { ([(CONTENT_TYPE, "image/png")], vec![0u8; 16]) }),
            )
            .route(
                "/huge",
                get(|| async {
                    (
                        [(CONTENT_TYPE, "text/html")],
                        "x".repeat(MAX_PAGE_FETCH_BYTES + 10),
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.ok() });
        base
    }

    async fn get(url: String) -> Result<FetchedPage, String> {
        let clients = ProxiedClient::new(|| {
            super::super::assets::asset_client_builder(super::super::assets::AssetTimeouts::DEFAULT)
        })
        .unwrap();
        fetch_page(
            &clients,
            &PageFetchSpec {
                url,
                proxy: ProxySettings::default(),
            },
        )
        .await
    }

    #[tokio::test]
    async fn pages_are_fetched_through_redirects_and_decoded() {
        let base = start_site().await;
        let page = get(format!("{base}/moved")).await.unwrap();
        assert_eq!(
            page,
            FetchedPage {
                url: format!("{base}/article"),
                content_type: "text/html".to_string(),
                body: "<p>\u{201c}Hi\u{201d}</p>".to_string(),
                truncated: false,
            }
        );

        let page = get(format!("{base}/huge")).await.unwrap();
        assert!(page.truncated);
        assert_eq!(page.body.len(), MAX_PAGE_FETCH_BYTES);
    }

    #[tokio::test]
    async fn what_is_not_an_html_page_is_refused() {
        let base = start_site().await;
        let err = get(format!("{base}/image")).await.unwrap_err();
        assert!(err.contains("not an HTML page"), "{err}");
        let err = get(format!("{base}/missing")).await.unwrap_err();
        assert!(err.contains("404"), "{err}");
        let err = get(format!("{base}/loop")).await.unwrap_err();
        assert!(err.contains("redirects"), "{err}");
        let err = get(format!("{base}/elsewhere")).await.unwrap_err();
        assert!(err.contains("not http(s)"), "{err}");
        let err = get("file:///etc/passwd".to_string()).await.unwrap_err();
        assert!(err.contains("not an http(s) URL"), "{err}");
    }

    #[test]
    fn only_http_urls_are_fetched() {
        assert!(http_url("https://example.com/a").is_some());
        assert!(http_url(" http://example.com/a ").is_some());
        assert!(http_url("file:///etc/passwd").is_none());
        assert!(http_url("/relative").is_none());
    }
}
