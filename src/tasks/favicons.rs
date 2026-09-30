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
//!
//! Fetching the home page, parsing it, and downloading the icon all happen
//! in the feed fetcher (see [`crate::fetcher::assets`]); this module decides
//! where to look, and stores what is found.
use crate::config::ProxySettings;
use crate::db::favicons;
use crate::db::Pool;
use crate::fetcher::assets::{resolve_http_url, AssetKind, PageProblem, PageSpec};
use crate::fetcher::Fetcher;
use crate::tasks::assets::{self, AssetCache};
use anyhow::{Context, Result};
use reqwest::Url;
use tracing::{debug, warn};

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
/// filesystem errors are returned, as is `fetcher` being unable to serve a
/// request at all, so that a fetcher outage is not recorded as a feed
/// having no favicon.
pub(crate) async fn cache_feed_favicon(
    fetcher: &Fetcher,
    proxy: &ProxySettings,
    pool: &Pool,
    data_dir: &std::path::Path,
    settings: &crate::config::AssetCacheSettings,
    feed_id: i64,
) -> Result<()> {
    if !settings.enabled {
        return Ok(());
    }
    let cache = AssetCache {
        fetcher,
        proxy,
        pool,
        data_dir,
        max_bytes: settings.max_bytes,
    };

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
        let (page_icons, landed_on) = discover_page_icons(fetcher, proxy, home).await?;
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
        let stored = assets::store_asset(&cache, url, AssetKind::Favicon, move |conn, asset_id| {
            favicons::record_check(conn, feed_id, Some(asset_id))
        })
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

/// Have `fetcher` fetch the web page `page` and return the icons it links
/// to, best first, and the URL it was finally served from if a redirect
/// moved it to another origin. A page that cannot be read yields no icons.
///
/// What comes back is checked again: only `http(s)` URLs are kept, and no
/// more of them than could be tried.
///
/// # Errors
///
/// Fails only if `fetcher` could not serve the request at all.
async fn discover_page_icons(
    fetcher: &Fetcher,
    proxy: &ProxySettings,
    page: &Url,
) -> Result<(Vec<Url>, Option<Url>)> {
    let found = fetcher
        .find_page_icons(PageSpec {
            url: page.to_string(),
            proxy: proxy.clone(),
        })
        .await
        .with_context(|| format!("looking for icons on {page}"))?;
    match &found.problem {
        None => {}
        Some(PageProblem::Unreachable(e)) => {
            warn!("failed to fetch {} looking for a favicon: {}", page, e)
        }
        Some(PageProblem::Status(status)) => {
            debug!("{} returned {} looking for a favicon", page, status)
        }
        Some(PageProblem::ReadFailed(e)) => {
            debug!("failed reading {} looking for a favicon: {}", page, e)
        }
    }
    let http = |u: &String| {
        Url::parse(u)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https"))
    };
    let icons = found
        .icons
        .iter()
        .filter_map(http)
        .take(MAX_CANDIDATES)
        .collect();
    Ok((icons, found.landed_on.as_ref().and_then(http)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

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
