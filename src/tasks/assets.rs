//! Feed asset caching — URL extraction and download helpers.
//!
//! Given an entry's post-script `content` HTML and its originating feed URL,
//! [`extract_asset_urls`] pulls out every `<img src>` reference, resolves
//! relative URLs against the entry/feed URL, and filters out non-http(s)
//! schemes. [`cache_asset`] then downloads each referenced asset, hashes it,
//! writes the bytes under `{data_dir}/assets/<shard>/<blake3>`, and records
//! the mapping in the database.
//!
//! Eviction is enforced inline: after each successful insertion, if the total
//! cache size exceeds the configured cap, the least-recently-accessed rows
//! are dropped via [`crate::db::assets::evict_to`] and their files unlinked.
use crate::http::{read_body_capped, CappedBody};
use anyhow::{Context, Result};
use lol_html::html_content::Element;
use lol_html::{element, HtmlRewriter, Settings};
use reqwest::Url;
use std::cell::RefCell;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

/// Per-asset download cap. Enclosures can be large (podcasts); this is a
/// safety valve to prevent a single rogue asset from filling the cache.
pub const MAX_ASSET_BYTES: u64 = 32 * 1024 * 1024;

/// Kind of asset reference discovered in an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetKind {
    InlineImg,
    Enclosure,
}

impl AssetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AssetKind::InlineImg => "inline_img",
            AssetKind::Enclosure => "enclosure",
        }
    }
}

/// Image MIME types we're willing to cache and serve. Deliberately excludes
/// `image/svg+xml`: SVG is XML and can embed executable script, which would
/// run when a browser navigates directly to the asset URL.
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
/// Inline images must match one of the safe raster image types. Enclosures
/// additionally accept audio and video types plus a handful of common
/// podcast-adjacent `application/*` types.
pub fn is_allowed_content_type(normalized: &str, kind: AssetKind) -> bool {
    if SAFE_IMAGE_TYPES.contains(&normalized) {
        return true;
    }
    match kind {
        AssetKind::InlineImg => false,
        AssetKind::Enclosure => {
            normalized.starts_with("audio/")
                || normalized.starts_with("video/")
                || matches!(normalized, "application/ogg" | "application/pdf")
        }
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

fn resolve_http_url(raw: &str, base: &Url) -> Option<Url> {
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

/// Path to the cached bytes for the given blake3 hash under `data_dir`.
///
/// Caller does not need to ensure the parent exists; [`write_asset_file`]
/// will create it.
pub fn asset_path(data_dir: &Path, blake3: &str) -> PathBuf {
    let shard = blake3.get(..2).unwrap_or("");
    data_dir.join("assets").join(shard).join(blake3)
}

/// Write `bytes` atomically to the asset path for `blake3`.
pub fn write_asset_file(data_dir: &Path, blake3: &str, bytes: &[u8]) -> Result<PathBuf> {
    let final_path = asset_path(data_dir, blake3);
    let parent = final_path
        .parent()
        .context("asset path has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create asset directory {:?}", parent))?;
    let tmp_path = parent.join(format!("{}.tmp", blake3));
    {
        let mut f = fs::File::create(&tmp_path)
            .with_context(|| format!("failed to create temp file {:?}", tmp_path))?;
        f.write_all(bytes)
            .with_context(|| format!("failed to write asset bytes to {:?}", tmp_path))?;
        f.sync_all().ok();
    }
    fs::rename(&tmp_path, &final_path)
        .with_context(|| format!("failed to rename {:?} -> {:?}", tmp_path, final_path))?;
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

/// Fetch, hash, store, and index a single asset.
///
/// Short-circuits to just linking the entry if an asset with the same
/// `original_url` already exists in the index.
///
/// After storing, evicts the oldest assets until the cache is back under
/// `max_cache_bytes`.
///
/// Returns `Ok(())` even when the fetch fails — individual asset failures
/// shouldn't block the rest of the entry. Errors are logged at WARN.
pub async fn cache_asset(
    client: &reqwest::Client,
    pool: &r2d2::Pool<r2d2_sqlite::SqliteConnectionManager>,
    data_dir: &Path,
    max_cache_bytes: i64,
    asset_url: &Url,
    entry_id: i64,
    kind: AssetKind,
) -> Result<()> {
    // Fast path: we've already fetched this exact URL before. Just ensure the
    // entry is linked to the existing asset row.
    {
        let conn = pool.get()?;
        if let Some(existing) = crate::db::assets::lookup_by_url(&conn, asset_url.as_str())? {
            crate::db::assets::link_entry_asset(&conn, entry_id, existing.id, kind.as_str())?;
            return Ok(());
        }
    }

    // Download. Enforce the per-asset size cap as we stream.
    let resp = match client.get(asset_url.clone()).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!("asset fetch failed for {}: {}", asset_url, e);
            return Ok(());
        }
    };

    if !resp.status().is_success() {
        debug!(
            "asset fetch non-success for {}: {}",
            asset_url,
            resp.status()
        );
        return Ok(());
    }

    let raw_content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let content_type = match raw_content_type.and_then(normalize_content_type) {
        Some(ct) if is_allowed_content_type(&ct, kind) => ct,
        _ => {
            warn!(
                "asset {} has disallowed or missing content-type {:?}, skipping",
                asset_url, raw_content_type
            );
            return Ok(());
        }
    };
    let etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let last_modified = resp
        .headers()
        .get(reqwest::header::LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Streamed under the cap: checking `Content-Length` up front and then
    // calling `bytes()` would still buffer the whole body for a server
    // that lies about (or omits) the header.
    let bytes = match read_body_capped(resp, MAX_ASSET_BYTES).await {
        Ok(CappedBody::Complete(b)) => b,
        Ok(CappedBody::TooLarge { seen }) => {
            warn!(
                "asset {} body of {} bytes exceeds cap {}, skipping",
                asset_url, seen, MAX_ASSET_BYTES
            );
            return Ok(());
        }
        Err(e) => {
            warn!("asset body read failed for {}: {}", asset_url, e);
            return Ok(());
        }
    };

    let hash = blake3::hash(&bytes).to_hex().to_string();
    let size = bytes.len() as i64;

    // Check again by hash — a different URL may have yielded the same bytes.
    let url_str = asset_url.as_str().to_string();
    let data_dir = data_dir.to_path_buf();
    let pool = pool.clone();
    let kind_str = kind.as_str().to_string();

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
        crate::db::assets::link_entry_asset(&conn, entry_id, asset_id, &kind_str)?;

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

    Ok(())
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
    fn malformed_html_does_not_crash() {
        let html = r#"<img src="http://a.test/x.png" <unclosed <img src="http://b.test/y.png">"#;
        let _ = extract_asset_urls(html, &base());
    }

    #[test]
    fn asset_path_uses_shard() {
        let p = asset_path(Path::new("/tmp/data"), "abcdef1234");
        assert_eq!(p, PathBuf::from("/tmp/data/assets/ab/abcdef1234"));
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
    fn inline_img_allowlist_rejects_html_and_svg() {
        assert!(is_allowed_content_type("image/png", AssetKind::InlineImg));
        assert!(is_allowed_content_type("image/jpeg", AssetKind::InlineImg));
        assert!(is_allowed_content_type("image/webp", AssetKind::InlineImg));
        assert!(!is_allowed_content_type("text/html", AssetKind::InlineImg));
        assert!(!is_allowed_content_type(
            "application/javascript",
            AssetKind::InlineImg
        ));
        assert!(!is_allowed_content_type(
            "image/svg+xml",
            AssetKind::InlineImg
        ));
        assert!(!is_allowed_content_type("audio/mpeg", AssetKind::InlineImg));
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
}
