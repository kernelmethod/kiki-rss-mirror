//! Finding and caching the favicons of the websites feeds belong to.
//!
//! [`cache_feed_favicon`] looks for a feed's favicon in these places, in
//! order, and caches the first one that downloads as a safe raster image
//! (see [`assets::is_allowed_content_type`]; SVG icons are never cached):
//!
//! 1. The Atom feed's `<icon>`.
//! 2. The `<link rel="icon">` (and `apple-touch-icon`) elements on the
//!    website's home page: the feed's `site_url`, or the root of the feed's
//!    own URL if the feed does not give one.
//! 3. `/favicon.ico` at the root of the website.
//!
//! The icon is stored in the asset cache like entry images are, and
//! recorded in `feed_favicons` (see [`crate::db::favicons`]), along with
//! when Kiki looked, so that it only looks again once
//! [`FaviconCheck::is_due`](crate::db::favicons::FaviconCheck::is_due).
use crate::db::favicons;
use crate::db::Pool;
use crate::tasks::assets::{self, resolve_http_url, AssetKind};
use anyhow::Result;
use lol_html::{element, HtmlRewriter, Settings};
use reqwest::Url;
use std::cell::RefCell;
use tracing::{debug, warn};

/// How much of a website's home page is read looking for icon links. They
/// belong in the `<head>`, so a prefix of the page is enough.
pub const MAX_PAGE_BYTES: usize = 512 * 1024;

/// The most icon URLs tried for one feed, so that a page listing many icons
/// cannot make Kiki download all of them.
pub const MAX_CANDIDATES: usize = 6;

/// Resolve `raw`, the website link a feed gives, against the feed's own URL
/// `feed_url`, returning it only if it is an absolute `http(s)` URL.
pub(crate) fn resolve_site_url(raw: &str, feed_url: &str) -> Option<Url> {
    match Url::parse(feed_url) {
        Ok(base) => resolve_http_url(raw, &base),
        Err(_) => Url::parse(raw.trim())
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https")),
    }
}

/// Find, download and cache the favicon of the website feed `feed_id`
/// belongs to, unless Kiki has looked recently or the asset cache is
/// disabled.
///
/// Failing to find a favicon is not an error: it is recorded, and Kiki
/// looks again after [`favicons::RECHECK_MISSING_SECS`]. Database and
/// filesystem errors are returned.
pub(crate) async fn cache_feed_favicon(
    client: &reqwest::Client,
    pool: &Pool,
    data_dir: &std::path::Path,
    cache: &crate::config::AssetCacheSettings,
    feed_id: i64,
) -> Result<()> {
    if !cache.enabled {
        return Ok(());
    }

    let (feed_url, site_url, atom_icon) = {
        let conn = pool.get()?;
        if !is_due(&conn, feed_id)? {
            return Ok(());
        }
        let row = conn
            .query_row(
                "SELECT f.url, f.site_url, ai.uri
                 FROM feeds f
                 LEFT JOIN atom_feed_icons ai ON ai.feed_id = f.id
                 WHERE f.id = ?1",
                [feed_id],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        match row {
            Some(r) => r,
            None => return Ok(()),
        }
    };

    let feed_url = feed_url.as_deref().and_then(|u| Url::parse(u).ok());
    // The feed's website, or failing that the root of the site serving the
    // feed. `file://` feeds have no website unless they name one.
    let home = site_url
        .as_deref()
        .and_then(|u| Url::parse(u).ok())
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .or_else(|| {
            feed_url
                .as_ref()
                .filter(|u| matches!(u.scheme(), "http" | "https"))
                .and_then(|u| u.join("/").ok())
        });

    let mut candidates: Vec<Url> = Vec::new();
    if let (Some(icon), Some(base)) = (atom_icon.as_deref(), feed_url.as_ref()) {
        candidates.extend(resolve_http_url(icon, base));
    }
    if let Some(home) = &home {
        let (page_icons, landed_on) = discover_page_icons(client, home).await;
        candidates.extend(page_icons);
        candidates.extend(home.join("/favicon.ico").ok());
        // The home page may have redirected to another host (say, from the
        // bare domain to `www.`), which may be the one with the icon.
        if let Some(landed_on) = landed_on {
            candidates.extend(landed_on.join("/favicon.ico").ok());
        }
    }
    let mut seen = Vec::new();
    candidates.retain(|u| {
        let new = !seen.contains(u);
        if new {
            seen.push(u.clone());
        }
        new
    });
    candidates.truncate(MAX_CANDIDATES);

    for url in &candidates {
        let stored = assets::store_asset(
            client,
            pool,
            data_dir,
            cache.max_bytes,
            url,
            AssetKind::Favicon,
            move |conn, asset_id| favicons::record_check(conn, feed_id, Some(asset_id)),
        )
        .await?;
        if stored {
            debug!("cached favicon {} for feed {}", url, feed_id);
            return Ok(());
        }
    }

    debug!(
        "no favicon found for feed {} (tried {} URLs)",
        feed_id,
        candidates.len()
    );
    favicons::record_check(&*pool.get()?, feed_id, None)?;
    Ok(())
}

/// Whether it is time to look for feed `feed_id`'s favicon: Kiki has never
/// looked, or looked long enough ago.
pub(crate) fn is_due(conn: &rusqlite::Connection, feed_id: i64) -> Result<bool> {
    let now = chrono::Utc::now().timestamp();
    Ok(favicons::last_check(conn, feed_id)?.is_none_or(|check| check.is_due(now)))
}

/// Fetch the web page `page` and return the icons it links to, best first,
/// and the URL it was finally served from if a redirect moved it to another
/// origin. Any failure yields no icons.
async fn discover_page_icons(client: &reqwest::Client, page: &Url) -> (Vec<Url>, Option<Url>) {
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
        Err(e) => {
            warn!("failed to fetch {} looking for a favicon: {}", page, e);
            return (Vec::new(), None);
        }
    };
    let final_url = resp.url().clone();
    let landed_on = (final_url.origin() != page.origin()).then(|| final_url.clone());

    if !resp.status().is_success() {
        debug!("{} returned {} looking for a favicon", page, resp.status());
        return (Vec::new(), landed_on);
    }
    let is_html = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(assets::normalize_content_type)
        .is_some_and(|t| t == "text/html" || t == "application/xhtml+xml");
    if !is_html {
        return (Vec::new(), landed_on);
    }

    // Only the start of the page is needed; stop reading once there is
    // enough of it.
    let mut body: Vec<u8> = Vec::new();
    while body.len() < MAX_PAGE_BYTES {
        match resp.chunk().await {
            Ok(Some(chunk)) => body.extend_from_slice(&chunk),
            Ok(None) => break,
            Err(e) => {
                debug!("failed reading {} looking for a favicon: {}", page, e);
                break;
            }
        }
    }
    body.truncate(MAX_PAGE_BYTES);

    (extract_icon_links(&body, &final_url), landed_on)
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
/// Icons declared as SVG are left out, since they would not be cached.
/// Among the rest, `rel="icon"` comes before touch icons, and the smallest
/// icon at least 32 pixels wide is preferred.
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
                        let is_svg = el
                            .get_attribute("type")
                            .and_then(|t| assets::normalize_content_type(&t))
                            .is_some_and(|t| t == "image/svg+xml");
                        if is_svg {
                            return Ok(());
                        }
                        let href = el.get_attribute("href").unwrap_or_default();
                        let Some(url) = resolve_http_url(&href, &base.borrow()) else {
                            return Ok(());
                        };
                        if url.path().to_ascii_lowercase().ends_with(".svg") {
                            return Ok(());
                        }
                        let size_rank = size_rank(el.get_attribute("sizes").as_deref());
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
    fn skips_svg_icons() {
        assert_eq!(
            icons(
                r#"<link rel="icon" type="image/svg+xml" href="/icon">
                <link rel="icon" href="/icon.SVG">
                <link rel="mask-icon" href="/mask.png">
                <link rel="icon" href="/icon.png">"#
            ),
            ["https://example.com/icon.png"]
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
    fn malformed_html_does_not_crash() {
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
    fn resolves_site_urls() {
        let feed = "https://example.com/feeds/all.xml";
        assert_eq!(
            resolve_site_url("/", feed).unwrap().as_str(),
            "https://example.com/"
        );
        assert_eq!(
            resolve_site_url("https://other.test/x", feed)
                .unwrap()
                .as_str(),
            "https://other.test/x"
        );
        assert!(resolve_site_url("ftp://example.com/", feed).is_none());
        assert!(resolve_site_url("/", "not a url").is_none());
        assert_eq!(
            resolve_site_url("http://x.test/", "not a url")
                .unwrap()
                .as_str(),
            "http://x.test/"
        );
    }
}
