//! Host names the web UI and the API answer to; see [`HostPattern`].

use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::Ipv6Addr;
use std::str::FromStr;

/// A host the web UI or the API may be reached at, as given to `kiki web
/// --allowed-host` or listed in
/// [`WebUiSettings::allowed_hosts`](super::WebUiSettings::allowed_hosts) or
/// [`ApiSettings::allowed_hosts`](super::ApiSettings::allowed_hosts).
///
/// In the config file it is written as a string, in the forms
/// [`HostPattern::from_str`] accepts.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum HostPattern {
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
pub enum HostPatternError {
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
    /// ```
    /// use kiki_rss::config::HostPattern;
    ///
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

impl HostPattern {
    /// Whether `host`, a host name or IP address as [`normalize`] leaves
    /// it, matches this pattern.
    ///
    /// ```
    /// use kiki_rss::config::HostPattern;
    ///
    /// let pattern: HostPattern = "*.example.com".parse().unwrap();
    /// assert!(pattern.matches("kiki.example.com"));
    /// assert!(!pattern.matches("example.com"));
    /// ```
    pub fn matches(&self, host: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(allowed) => allowed == host,
            Self::Subdomains(parent) => host
                .strip_suffix(parent.as_str())
                .is_some_and(|sub| sub.len() > 1 && sub.ends_with('.')),
        }
    }
}

impl TryFrom<String> for HostPattern {
    type Error = HostPatternError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<HostPattern> for String {
    fn from(pattern: HostPattern) -> Self {
        pattern.to_string()
    }
}

impl fmt::Display for HostPattern {
    /// Writes the pattern back in the form [`HostPattern::from_str`]
    /// parses, IPv6 addresses without brackets.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any => f.write_str("*"),
            Self::Subdomains(parent) => write!(f, "*.{parent}"),
            Self::Exact(host) => f.write_str(host),
        }
    }
}

/// Lowercase `host` and drop a trailing dot, which names the same host.
pub(crate) fn normalize(host: &str) -> String {
    host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase()
}

/// Host names always answered to, whatever the patterns. They name the
/// local machine, so no other site can have a browser send them.
pub(crate) const LOCALHOST: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

/// Whether a request whose `Host` header (or HTTP/2 `:authority`) is
/// `authority` names one of the [`LOCALHOST`] names or a host matching one
/// of `patterns`. The port, if any, is ignored: a site rebinding its domain
/// has to use the listener's port anyway. A malformed `authority` is never
/// allowed.
pub(crate) fn allows(patterns: &[HostPattern], authority: &str) -> bool {
    let Some(host) = host_of(authority) else {
        return false;
    };
    LOCALHOST.contains(&host.as_str()) || patterns.iter().any(|p| p.matches(&host))
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

/// The host and port `req` is for: its `Host` header, or for HTTP/2 its
/// `:authority`, without any userinfo. `None` if it names neither, or the
/// header is not text.
pub(crate) fn request_authority<B>(req: &axum::http::Request<B>) -> Option<&str> {
    let authority = match req.headers().get(axum::http::header::HOST) {
        Some(host) => host.to_str().ok(),
        None => req.uri().authority().map(|a| a.as_str()),
    };
    // An HTTP/2 `:authority` may carry userinfo, which is no part of the host.
    authority.map(|a| a.rsplit_once('@').map_or(a, |(_, host)| host))
}
