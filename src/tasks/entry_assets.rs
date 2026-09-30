use crate::config::ProxySettings;
use crate::db::Db;
use crate::fetcher::Fetcher;
use crate::tasks::assets::{self, AssetCache};
use anyhow::Result;
use reqwest::Url;
use tracing::{debug, warn};

/// Download and cache the external assets referenced by an entry.
///
/// Runs in an async worker: looks up the entry's post-script `content` HTML
/// plus its feed URL and any RSS enclosure, has `fetcher` find the
/// `<img src>` URLs in the HTML, and delegates each to
/// [`assets::cache_asset`]. Individual asset failures are logged and
/// skipped.
///
/// # Errors
///
/// Returns database errors. A fetcher that cannot serve a request is
/// logged like any other failed asset.
pub(crate) async fn cache_entry_assets(
    fetcher: &Fetcher,
    proxy: &ProxySettings,
    db: &Db,
    data_dir: &std::path::Path,
    settings: &crate::config::AssetCacheSettings,
    entry_id: i64,
) -> Result<()> {
    if !settings.enabled {
        return Ok(());
    }
    let cache = AssetCache {
        fetcher,
        proxy,
        db,
        data_dir,
        max_bytes: settings.max_bytes,
    };

    #[derive(Debug)]
    struct EntryCtx {
        content: Option<String>,
        base: Option<String>,
        enclosure_url: Option<String>,
    }

    let ctx = db.read_blocking(|conn| {
        conn.query_row(
            "SELECT e.content, f.url, e.url, red.enclosure_url
                 FROM entries e
                 LEFT JOIN feeds f ON f.id = e.feed_id
                 LEFT JOIN rss_entry_data red ON red.entry_id = e.id
                 WHERE e.id = ?1",
            [entry_id],
            |row| {
                let content: Option<String> = row.get(0)?;
                let feed_url: Option<String> = row.get(1)?;
                let entry_url: Option<String> = row.get(2)?;
                let enclosure_url: Option<String> = row.get(3)?;
                // Prefer the entry URL as the resolution base; fall back
                // to the feed URL so relative URLs still work when the
                // entry URL is empty.
                let base = entry_url.filter(|s| !s.is_empty()).or(feed_url);
                Ok(EntryCtx {
                    content,
                    base,
                    enclosure_url,
                })
            },
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
    })??;
    let Some(ctx) = ctx else {
        return Ok(());
    };

    let base = match ctx.base.as_deref().and_then(|s| Url::parse(s).ok()) {
        Some(u) => u,
        None => {
            debug!(
                "entry {} has no resolvable base URL; skipping asset cache",
                entry_id
            );
            return Ok(());
        }
    };

    // Inline images from the entry's HTML content, found by the fetcher so
    // that the HTML is never parsed here. What it sends back is checked
    // again: only `http(s)` URLs, each once.
    if let Some(content) = ctx.content {
        // A failure here still leaves the enclosure to cache below.
        let found = fetcher
            .extract_images(content, &base)
            .await
            .unwrap_or_else(|e| {
                warn!("finding the images in entry {} failed: {}", entry_id, e);
                Vec::new()
            });
        let mut urls: Vec<Url> = Vec::new();
        for url in found
            .iter()
            .filter_map(|u| Url::parse(u).ok())
            .filter(|u| matches!(u.scheme(), "http" | "https"))
        {
            if !urls.contains(&url) {
                urls.push(url);
            }
        }
        for url in urls {
            if let Err(e) =
                assets::cache_asset(&cache, &url, entry_id, assets::AssetKind::InlineImg).await
            {
                warn!("cache_asset failed for {}: {:?}", url, e);
            }
        }
    }

    // RSS enclosure, when present.
    if let Some(enc) = ctx.enclosure_url.as_deref() {
        if let Some(url) = Url::parse(enc)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https"))
        {
            if let Err(e) =
                assets::cache_asset(&cache, &url, entry_id, assets::AssetKind::Enclosure).await
            {
                warn!("cache_asset failed for enclosure {}: {:?}", url, e);
            }
        }
    }

    Ok(())
}
