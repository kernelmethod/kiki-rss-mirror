//! The `Host` check that guards the web UI against DNS rebinding.
//!
//! The web UI has no login, and by default listens only on loopback. That
//! alone does not keep other sites out: a site whose domain's DNS record
//! is switched to `127.0.0.1` after the page loads can then send requests
//! to the web UI that the browser counts as same-origin, so they pass
//! [`is_same_origin`](super::plugins::is_same_origin) and can read pages
//! too. Such a request still names the attacker's domain in its `Host`
//! header, though, so the web UI only answers requests whose `Host` is one
//! it was told to expect.

use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::net::Ipv6Addr;
use std::str::FromStr;
use std::sync::Arc;

/// Host names the web UI always answers to. They name the local machine,
/// so no other site can have a browser send them.
const LOCALHOST: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

/// A host the web UI may be reached at, as given to `--allowed-host`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum HostPattern {
    /// `*`: any host at all, which turns the check off.
    Any,
    /// `*.example.com`: any subdomain of `example.com`, but not
    /// `example.com` itself.
    Subdomains(String),
    /// `example.com`, `192.168.1.5` or `::1`: that host only.
    Exact(String),
}

/// Errors from parsing a [`HostPattern`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(super) enum HostPatternError {
    /// The pattern named no host.
    #[error("no host given")]
    Empty,
    /// The pattern was not a host name or IP address, e.g. it held a port
    /// or a URL scheme.
    #[error("{0:?} is not a host name or IP address; give the host alone, without a port")]
    Invalid(String),
}

impl FromStr for HostPattern {
    type Err = HostPatternError;

    /// Parse `*`, `*.<host>` or `<host>`, where `<host>` is a host name or
    /// IP address without a port. IPv6 addresses may be given with or
    /// without brackets. Case and a trailing dot are ignored.
    ///
    /// ```ignore
    /// assert_eq!("*".parse(), Ok(HostPattern::Any));
    /// assert_eq!("Kiki.LAN".parse(), Ok(HostPattern::Exact("kiki.lan".into())));
    /// assert!("kiki.lan:8080".parse::<HostPattern>().is_err());
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`HostPatternError::Empty`] for an empty pattern, and
    /// [`HostPatternError::Invalid`] for one that is not a host.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s == "*" {
            return Ok(Self::Any);
        }
        let (subdomains, host) = match s.strip_prefix("*.") {
            Some(rest) => (true, rest),
            None => (false, s),
        };
        let host = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        let host = normalize(host);
        if host.is_empty() {
            return Err(HostPatternError::Empty);
        }
        let is_name = host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
        let is_ipv6 = !subdomains && host.parse::<Ipv6Addr>().is_ok();
        if !is_name && !is_ipv6 {
            return Err(HostPatternError::Invalid(s.to_owned()));
        }
        Ok(if subdomains {
            Self::Subdomains(host)
        } else {
            Self::Exact(host)
        })
    }
}

/// The hosts the web UI answers to: the [`LOCALHOST`] names, and whatever
/// was given to `--allowed-host`.
#[derive(Clone, Debug, Default)]
pub(super) struct AllowedHosts {
    patterns: Vec<HostPattern>,
}

impl AllowedHosts {
    /// Allow the [`LOCALHOST`] names and the hosts matching `patterns`.
    pub(super) fn new(patterns: Vec<HostPattern>) -> Self {
        Self { patterns }
    }

    /// Whether every host is allowed, because `*` was given.
    pub(super) fn allows_any(&self) -> bool {
        self.patterns.contains(&HostPattern::Any)
    }

    /// Whether only the [`LOCALHOST`] names are allowed.
    pub(super) fn only_localhost(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Whether a request whose `Host` header (or HTTP/2 `:authority`) is
    /// `authority` may be answered. The port, if any, is ignored: a site
    /// rebinding its domain has to use the web UI's port anyway.
    pub(super) fn allows(&self, authority: &str) -> bool {
        let Some(host) = host_of(authority) else {
            return false;
        };
        LOCALHOST.contains(&host.as_str())
            || self.patterns.iter().any(|pattern| match pattern {
                HostPattern::Any => true,
                HostPattern::Exact(allowed) => *allowed == host,
                HostPattern::Subdomains(parent) => host
                    .strip_suffix(parent.as_str())
                    .is_some_and(|sub| sub.len() > 1 && sub.ends_with('.')),
            })
    }
}

/// Lowercase `host` and drop a trailing dot, which names the same host.
fn normalize(host: &str) -> String {
    host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase()
}

/// The host in `authority`, a `Host` header's value: `host`, `host:port`,
/// `[v6]` or `[v6]:port`, normalized as [`normalize`] does. `None` if it
/// is malformed.
fn host_of(authority: &str) -> Option<String> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (v6, rest) = rest.split_once(']')?;
        v6.parse::<Ipv6Addr>().ok()?;
        (v6, rest.strip_prefix(':'))
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    if port.is_some_and(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let host = normalize(host);
    (!host.is_empty()).then_some(host)
}

/// Middleware that refuses, with `400 Bad Request`, any request that does
/// not name one of the `allowed` hosts in its `Host` header, or for
/// HTTP/2 its `:authority`. A request that names no host is refused too.
pub(super) async fn check_host(
    State(allowed): State<Arc<AllowedHosts>>,
    req: Request,
    next: Next,
) -> Response {
    let authority = match req.headers().get(header::HOST) {
        Some(host) => host.to_str().ok(),
        None => req.uri().authority().map(|a| a.as_str()),
    };
    // An HTTP/2 `:authority` may carry userinfo, which is no part of the host.
    let authority = authority.map(|a| a.rsplit_once('@').map_or(a, |(_, host)| host));
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
