use crate::tasks::assets;
use anyhow::Result;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use reqwest::Url;
use tracing::{debug, warn};

/// Download and cache the external assets referenced by an entry.
///
/// Runs in an async worker: looks up the entry's post-script `content` HTML
/// plus its feed URL and any RSS enclosure, extracts `<img src>` URLs, and
/// delegates each to [`assets::cache_asset`]. Individual asset failures are
/// logged and skipped.
pub(crate) async fn cache_entry_assets(
    client: &reqwest::Client,
    pool: &Pool<SqliteConnectionManager>,
    data_dir: &std::path::Path,
    cache: &crate::config::AssetCacheSettings,
    entry_id: i64,
) -> Result<()> {
    if !cache.enabled {
        return Ok(());
    }

    #[derive(Debug)]
    struct EntryCtx {
        content: Option<String>,
        base: Option<String>,
        enclosure_url: Option<String>,
    }

    let ctx = {
        let conn = pool.get()?;
        let row = conn
            .query_row(
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
            })?;
        match row {
            Some(r) => r,
            None => return Ok(()),
        }
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

    // Inline images from the entry's HTML content.
    if let Some(content) = ctx.content.as_deref() {
        for url in assets::extract_asset_urls(content, &base) {
            if let Err(e) = assets::cache_asset(
                client,
                pool,
                data_dir,
                cache.max_bytes,
                &url,
                entry_id,
                assets::AssetKind::InlineImg,
            )
            .await
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
            if let Err(e) = assets::cache_asset(
                client,
                pool,
                data_dir,
                cache.max_bytes,
                &url,
                entry_id,
                assets::AssetKind::Enclosure,
            )
            .await
            {
                warn!("cache_asset failed for enclosure {}: {:?}", url, e);
            }
        }
    }

    Ok(())
}
