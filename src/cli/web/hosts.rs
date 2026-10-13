//! The `Host` check that guards the web UI against DNS rebinding.
//!
//! The web UI has no login unless asked for one (see [`super::login`]),
//! and by default listens only on loopback. That alone does not keep other
//! sites out: a site whose domain's DNS record
//! is switched to `127.0.0.1` after the page loads can then send requests
//! to the web UI that the browser counts as same-origin, so they pass
//! [`is_same_origin`](super::plugins::is_same_origin) and can read pages
//! too. Such a request still names the attacker's domain in its `Host`
//! header, though, so the web UI only answers requests whose `Host` is one
//! it was told to expect.

use crate::config::hosts;
use crate::config::HostPattern;
use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::Arc;

/// The hosts the web UI answers to: the [`LOCALHOST`](hosts::LOCALHOST) names, and whatever
/// was given to `--allowed-host`.
#[derive(Clone, Debug, Default)]
pub(super) struct AllowedHosts {
    patterns: Vec<HostPattern>,
}

impl AllowedHosts {
    /// Allow the [`LOCALHOST`](hosts::LOCALHOST) names and the hosts matching `patterns`.
    pub(super) fn new(patterns: Vec<HostPattern>) -> Self {
        Self { patterns }
    }

    /// Whether every host is allowed, because `*` was given.
    pub(super) fn allows_any(&self) -> bool {
        self.patterns.contains(&HostPattern::Any)
    }

    /// Whether only the [`LOCALHOST`](hosts::LOCALHOST) names are allowed.
    pub(super) fn only_localhost(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Whether a request whose `Host` header (or HTTP/2 `:authority`) is
    /// `authority` may be answered. The port, if any, is ignored: a site
    /// rebinding its domain has to use the web UI's port anyway.
    pub(super) fn allows(&self, authority: &str) -> bool {
        hosts::allows(&self.patterns, authority)
    }
}

/// Middleware that refuses, with `400 Bad Request`, any request that does
/// not name one of the `allowed` hosts in its `Host` header, or for
/// HTTP/2 its `:authority`. A request that names no host is refused too.
pub(super) async fn check_host(
    State(allowed): State<Arc<AllowedHosts>>,
    req: Request,
    next: Next,
) -> Response {
    let authority = hosts::request_authority(&req);
    if authority.is_some_and(|a| allowed.allows(a)) {
        return next.run(req).await;
    }
    tracing::warn!(
        host = authority.unwrap_or("<none>"),
        "refused a request for a host the web UI is not allowed to answer to; \
         see --allowed-host"
    );
    (
        StatusCode::BAD_REQUEST,
        "The web UI does not answer to this host name. Run `kiki web` with \
         `--allowed-host <HOST>` to allow it.",
    )
        .into_response()
}
