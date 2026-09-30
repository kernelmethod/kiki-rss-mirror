//! Feed asset caching — storing and indexing downloaded assets.
//!
//! Given an entry's post-script `content` HTML and its originating feed URL,
//! the feed fetcher finds every `<img src>` reference
//! ([`Fetcher::extract_images`]), and [`cache_asset`] then has it download
//! each one ([`Fetcher::fetch_asset`]). The downloading and the HTML
//! parsing happen in the sandboxed feed fetcher (see
//! [`crate::fetcher::assets`]); what is left here is what needs the
//! database and the asset cache: checking what came back, hashing it,
//! writing the bytes under `{data_dir}/assets/<shard>/<blake3>`, and
//! recording the mapping in the database.
//!
//! Eviction is enforced inline: after each successful insertion, if the total
//! cache size exceeds the configured cap, the least-recently-accessed rows
//! are dropped via [`crate::db::assets::evict_to`] and their files unlinked.
use crate::config::ProxySettings;
use crate::fetcher::assets::{AssetReply, AssetSpec};
use crate::fetcher::Fetcher;
use anyhow::{Context, Result};
use reqwest::Url;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{debug, warn};

pub use crate::fetcher::assets::{
    is_allowed_content_type, normalize_content_type, AssetKind, MAX_ASSET_BYTES,
};

/// Where assets are downloaded through and stored.
pub struct AssetCache<'a> {
    /// Downloads assets; normally the sandboxed feed fetcher.
    pub fetcher: &'a Fetcher,
    /// The proxy to download through.
    pub proxy: &'a ProxySettings,
    pub pool: &'a crate::db::Pool,
    /// Kiki's data directory, holding the `assets/` tree.
    pub data_dir: &'a Path,
    /// Evict the least recently used assets once the cache is larger.
    pub max_bytes: i64,
}

/// Path to the cached bytes for the given blake3 hash under `data_dir`.
///
/// Caller does not need to ensure the parent exists; [`write_asset_file`]
/// will create it.
pub fn asset_path(data_dir: &Path, blake3: &str) -> PathBuf {
    let shard = blake3.get(..2).unwrap_or("");
    data_dir.join("assets").join(shard).join(blake3)
}

/// Write `bytes` atomically to the asset path for `blake3`.
///
/// Safe to call from several tasks at once for the same hash: each writes
/// its own temporary file, and since the bytes are the same whichever
/// rename lands last leaves the same file behind.
pub fn write_asset_file(data_dir: &Path, blake3: &str, bytes: &[u8]) -> Result<PathBuf> {
    static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    let final_path = asset_path(data_dir, blake3);
    let parent = final_path
        .parent()
        .context("asset path has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create asset directory {:?}", parent))?;
    let tmp_path = parent.join(format!(
        "{}.{}.{}.tmp",
        blake3,
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut f = fs::File::create_new(&tmp_path)
            .with_context(|| format!("failed to create temp file {:?}", tmp_path))?;
        f.write_all(bytes)
            .with_context(|| format!("failed to write asset bytes to {:?}", tmp_path))?;
        f.sync_all().ok();
        drop(f);
        fs::rename(&tmp_path, &final_path)
            .with_context(|| format!("failed to rename {:?} -> {:?}", tmp_path, final_path))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result?;
    Ok(final_path)
}

/// Remove a cached asset's file from disk. Missing files are not an error.
pub fn unlink_asset_file(data_dir: &Path, blake3: &str) {
    let path = asset_path(data_dir, blake3);
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!("failed to remove asset file {:?}: {}", path, e),
    }
}

/// Fetch, hash, store, and index a single asset, and link it to entry
/// `entry_id` as an asset of kind `kind`.
///
/// Short-circuits to just linking the entry if an asset with the same
/// `original_url` already exists in the index.
///
/// After storing, evicts the oldest assets until the cache is back under
/// [`AssetCache::max_bytes`].
///
/// Returns `Ok(())` even when the download fails — individual asset
/// failures shouldn't block the rest of the entry. Those are logged at
/// WARN; database, filesystem and fetcher errors are returned.
pub async fn cache_asset(
    cache: &AssetCache<'_>,
    asset_url: &Url,
    entry_id: i64,
    kind: AssetKind,
) -> Result<()> {
    store_asset(cache, asset_url, kind, move |conn, asset_id| {
        crate::db::assets::link_entry_asset(conn, entry_id, asset_id, kind.as_str())
    })
    .await?;
    Ok(())
}

/// Fetch, hash, store, and index a single asset, then call `link` with the
/// id of its `feed_assets` row to record what it belongs to.
///
/// Short-circuits to just calling `link` if an asset with the same
/// `original_url` already exists in the index. Otherwise `link` is called
/// before the cache is evicted back under [`AssetCache::max_bytes`], so
/// that it never sees an asset row that has already been removed.
///
/// What the fetcher sends back is checked again before it is stored: its
/// content type must be allowed for `kind`, and it must fit under
/// [`MAX_ASSET_BYTES`].
///
/// Returns `Ok(true)` once `link` has been called, and `Ok(false)` when the
/// asset could not be fetched, was too large, or was of a type not allowed
/// for `kind`; those failures are logged rather than returned. Database and
/// filesystem errors, errors from `link`, and a fetcher that could not
/// serve the request at all, are returned.
pub async fn store_asset<F>(
    cache: &AssetCache<'_>,
    asset_url: &Url,
    kind: AssetKind,
    link: F,
) -> Result<bool>
where
    F: FnOnce(&rusqlite::Connection, i64) -> Result<()> + Send + 'static,
{
    // Fast path: we've already fetched this exact URL before. Just link the
    // existing asset row.
    {
        let conn = cache.pool.get()?;
        if let Some(existing) = crate::db::assets::lookup_by_url(&conn, asset_url.as_str())? {
            link(&conn, existing.id)?;
            return Ok(true);
        }
    }

    let reply = cache
        .fetcher
        .fetch_asset(AssetSpec {
            url: asset_url.to_string(),
            kind,
            proxy: cache.proxy.clone(),
        })
        .await
        .with_context(|| format!("fetching asset {asset_url}"))?;
    let asset = match reply {
        AssetReply::Fetched(asset) => asset,
        AssetReply::Network { message } => {
            warn!("asset fetch failed for {}: {}", asset_url, message);
            return Ok(false);
        }
        AssetReply::HttpStatus { status } => {
            debug!("asset fetch non-success for {}: {}", asset_url, status);
            return Ok(false);
        }
        AssetReply::DisallowedType { content_type } => {
            warn!(
                "asset {} has disallowed or missing content-type {:?}, skipping",
                asset_url, content_type
            );
            return Ok(false);
        }
        AssetReply::TooLarge { seen } => {
            warn!(
                "asset {} body of {} bytes exceeds cap {}, skipping",
                asset_url, seen, MAX_ASSET_BYTES
            );
            return Ok(false);
        }
        AssetReply::Failed { message } => {
            warn!("asset body read failed for {}: {}", asset_url, message);
            return Ok(false);
        }
    };

    // The fetcher checked both of these already; a fetcher that has been
    // compromised would not have, so check them again.
    let content_type = match normalize_content_type(&asset.content_type) {
        Some(ct) if is_allowed_content_type(&ct, kind) => ct,
        _ => {
            warn!(
                "fetcher returned asset {} with disallowed content-type {:?}, skipping",
                asset_url, asset.content_type
            );
            return Ok(false);
        }
    };
    if asset.bytes.len() as u64 > MAX_ASSET_BYTES {
        warn!(
            "fetcher returned asset {} of {} bytes, over cap {}, skipping",
            asset_url,
            asset.bytes.len(),
            MAX_ASSET_BYTES
        );
        return Ok(false);
    }
    let bytes = asset.bytes;
    let etag = asset.etag;
    let last_modified = asset.last_modified;

    let hash = blake3::hash(&bytes).to_hex().to_string();
    let size = bytes.len() as i64;

    // Check again by hash — a different URL may have yielded the same bytes.
    let url_str = asset_url.as_str().to_string();
    let data_dir = cache.data_dir.to_path_buf();
    let pool = cache.pool.clone();
    let max_cache_bytes = cache.max_bytes;

    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut conn = pool.get()?;
        let asset_id = if let Some(existing) = crate::db::assets::lookup_by_hash(&conn, &hash)? {
            existing.id
        } else {
            write_asset_file(&data_dir, &hash, &bytes)?;
            crate::db::assets::insert_asset(
                &conn,
                &hash,
                &url_str,
                Some(content_type.as_str()),
                size,
                etag.as_deref(),
                last_modified.as_deref(),
            )?
        };
        link(&conn, asset_id)?;

        // Enforce cache cap inline. Cheap: one SUM and, in the common case
        // where we're under the cap, no deletes.
        if crate::db::assets::total_cache_size(&conn).unwrap_or(0) > max_cache_bytes {
            match crate::db::assets::evict_to(&mut conn, max_cache_bytes) {
                Ok(deleted) => {
                    for h in deleted {
                        unlink_asset_file(&data_dir, &h);
                    }
                }
                Err(e) => warn!("asset eviction failed: {}", e),
            }
        }
        Ok(())
    })
    .await??;

    Ok(true)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn asset_path_uses_shard() {
        let p = asset_path(Path::new("/tmp/data"), "abcdef1234");
        assert_eq!(p, PathBuf::from("/tmp/data/assets/ab/abcdef1234"));
    }

    /// Several writers storing the same bytes at once all succeed, and
    /// leave just the finished file behind.
    #[test]
    fn concurrent_writes_of_the_same_asset_succeed() {
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let bytes = b"the same bytes, every time".to_vec();
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let writers = 16;
        let barrier = Arc::new(Barrier::new(writers));

        let handles: Vec<_> = (0..writers)
            .map(|_| {
                let data_dir = dir.path().to_path_buf();
                let (hash, bytes, barrier) = (hash.clone(), bytes.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    write_asset_file(&data_dir, &hash, &bytes)
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }

        let path = asset_path(dir.path(), &hash);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        let files: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(files, vec![std::ffi::OsString::from(&hash)]);
    }
}
