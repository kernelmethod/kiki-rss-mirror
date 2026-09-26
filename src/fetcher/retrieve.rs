//! The HTTP half of a feed fetch.

use super::{parse_off_thread, FetchReply, FetchSpec, FetchedBody, ResponseHeaders, MAX_REDIRECTS};
use crate::http::{read_body_capped, CappedBody, FeedAuthType};
use crate::tasks::same_origin;
use reqwest::Url;
use std::time::Duration;
use tracing::debug;

/// Fetch the feed described by `spec`, following redirects by hand, and
/// parse the body if the server answered `200 OK`.
///
/// Redirects are followed here rather than by reqwest so that credentials
/// are only ever sent to the feed's own origin, and so that a permanent
/// redirect can be reported back for the stored URL to be updated.
///
/// Never fails: every way a fetch can end is a [`FetchReply`] variant.
pub async fn retrieve(client: &reqwest::Client, spec: &FetchSpec) -> FetchReply {
    let feed_id = spec.feed_id;
    let feed_url = spec.url.as_str();
    let timeout = Duration::from_secs(spec.timeout_secs);

    let mut current_url = feed_url.to_string();
    let mut permanent_redirect = false;
    let mut redirects: u64 = 0;

    let resp = 'redirect: {
        for _ in 0..=MAX_REDIRECTS {
            // Only send conditional headers on the first request, and only
            // when the caller allows them.
            let mut request = client.get(&current_url).timeout(timeout);
            if current_url == feed_url && spec.send_conditionals {
                if let Some(etag) = &spec.etag {
                    request = request.header("If-None-Match", etag);
                }
                if let Some(last_modified) = &spec.last_modified {
                    request = request.header("If-Modified-Since", last_modified);
                }
            }
            // Only forward credentials to the feed's configured origin. If a
            // redirect took us cross-origin we intentionally drop them to
            // avoid leaking secrets to an unrelated host.
            if same_origin(feed_url, &current_url) {
                request = spec.auth.apply(request);
            } else if spec.auth.auth_type != FeedAuthType::None {
                debug!(
                    "Feed {}: dropping auth on cross-origin redirect to {}",
                    feed_id, current_url
                );
            }

            let resp = match request.send().await {
                Ok(r) => r,
                Err(e) => {
                    return FetchReply::Network {
                        message: format!("{}", e),
                        timeout: e.is_timeout(),
                        redirects,
                    };
                }
            };

            if resp.status().is_redirection() && resp.status() != reqwest::StatusCode::NOT_MODIFIED
            {
                let Some(location) = resp.headers().get("location").and_then(|h| h.to_str().ok())
                else {
                    return FetchReply::Failed {
                        message: "Redirect response missing Location header".to_string(),
                    };
                };

                // 301 Moved Permanently and 308 Permanent Redirect both
                // indicate a permanent move.
                if resp.status() == reqwest::StatusCode::MOVED_PERMANENTLY
                    || resp.status() == reqwest::StatusCode::PERMANENT_REDIRECT
                {
                    permanent_redirect = true;
                }

                // Resolve the Location against the current URL to handle
                // relative redirects.
                match Url::parse(&current_url).and_then(|base| base.join(location)) {
                    Ok(next) => current_url = next.to_string(),
                    Err(e) => {
                        return FetchReply::Failed {
                            message: format!("{}", e),
                        }
                    }
                }
                redirects += 1;
                continue;
            }

            break 'redirect resp;
        }

        return FetchReply::TooManyRedirects { redirects };
    };

    match resp.status() {
        reqwest::StatusCode::NOT_MODIFIED => {
            return FetchReply::NotModified {
                headers: ResponseHeaders::capture(resp.headers()),
                redirects,
            };
        }
        reqwest::StatusCode::OK => {}
        status => {
            return FetchReply::HttpStatus {
                status: status.as_u16(),
                headers: ResponseHeaders::capture(resp.headers()),
                redirects,
            };
        }
    }

    let headers = ResponseHeaders::capture(resp.headers());

    // The read is capped: a feed server that streams an unbounded body
    // would otherwise buffer straight into an OOM, and the request
    // timeout is no defence against a fast sender.
    let content = match read_body_capped(resp, spec.max_feed_bytes).await {
        Ok(CappedBody::Complete(bytes)) => bytes,
        Ok(CappedBody::TooLarge { seen }) => {
            return FetchReply::BodyTooLarge {
                final_url: current_url,
                seen,
                redirects,
            };
        }
        Err(e) => {
            return FetchReply::Failed {
                message: format!("{}", e),
            }
        }
    };

    let body_len = content.len() as u64;
    let body_hash = blake3::hash(&content).to_hex().to_string();
    let parsed = parse_off_thread(feed_id, content).await;

    FetchReply::Body(Box::new(FetchedBody {
        final_url: current_url,
        permanent_redirect,
        redirects,
        headers,
        body_len,
        body_hash,
        parsed,
    }))
}
