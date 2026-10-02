use super::layout::ASSET_CONTENT_SECURITY_POLICY;
use super::API_BASE;
use axum::{
    extract::{Path as UrlPath, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};

/// Serve the cached asset with the blake3 hash `hash`, fetched from the
/// Kiki API. Entry pages show images and link attachments from here, so
/// that the browser never loads anything from the sites the feeds link to.
///
/// Images, audio and video are served for the browser to show; anything
/// else is served as a download, rather than rendered from the web UI's
/// origin.
pub(super) async fn asset(
    State(api): State<reqwest::Client>,
    UrlPath(hash): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    // Checked here as well as by the API, so that nothing but a hash is
    // ever put into the API's URL.
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return (StatusCode::NOT_FOUND, "asset not found").into_response();
    }

    let mut req = api.get(format!("{API_BASE}/v1/assets/{hash}"));
    if let Some(etag) = headers.get(header::IF_NONE_MATCH) {
        req = req.header(header::IF_NONE_MATCH, etag);
    }
    let resp = match req.send().await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!("failed to reach the Kiki server: {e:#}");
            return (StatusCode::BAD_GATEWAY, "The Kiki server is unavailable.").into_response();
        }
    };

    let status = resp.status();
    if status == StatusCode::NOT_FOUND {
        return (StatusCode::NOT_FOUND, "asset not found").into_response();
    }
    if status != StatusCode::OK && status != StatusCode::NOT_MODIFIED {
        tracing::warn!(%status, hash, "failed to fetch cached asset");
        return (StatusCode::BAD_GATEWAY, "failed to fetch asset").into_response();
    }

    let mut out = HeaderMap::new();
    for name in [header::CONTENT_TYPE, header::ETAG, header::CACHE_CONTROL] {
        if let Some(value) = resp.headers().get(&name) {
            out.insert(name, value.clone());
        }
    }
    let shown_inline = out
        .get(header::CONTENT_TYPE)
        .and_then(|t| t.to_str().ok())
        .is_some_and(|t| {
            let t = t.trim_start().to_ascii_lowercase();
            ["image/", "audio/", "video/"]
                .iter()
                .any(|prefix| t.starts_with(prefix))
        });
    out.insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_static(if shown_inline { "inline" } else { "attachment" }),
    );
    out.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    out.insert(
        header::X_DNS_PREFETCH_CONTROL,
        header::HeaderValue::from_static("off"),
    );
    out.insert(
        header::CONTENT_SECURITY_POLICY,
        header::HeaderValue::from_static(ASSET_CONTENT_SECURITY_POLICY),
    );
    match resp.bytes().await {
        Ok(body) => (status, out, body).into_response(),
        Err(e) => {
            tracing::warn!(hash, "failed to read cached asset: {e:#}");
            (StatusCode::BAD_GATEWAY, "failed to fetch asset").into_response()
        }
    }
}
