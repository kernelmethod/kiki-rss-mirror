use reqwest::{RequestBuilder, Response};
use serde::{Deserialize, Serialize};
use std::fmt;

pub const USER_AGENT: &str = concat!(
    "github.com/kernelmethod/kiki-rss ",
    env!("CARGO_PKG_VERSION")
);

/// Outcome of reading a response body under a size cap.
#[derive(Debug)]
pub enum CappedBody {
    /// The body was fully read and fits within the cap.
    Complete(Vec<u8>),
    /// The body exceeded the cap and was abandoned part-way through.
    /// Carries the number of bytes seen before giving up, which is at
    /// least `cap + 1` unless `Content-Length` announced the overage up
    /// front (in which case nothing was read and this is the declared
    /// length).
    TooLarge { seen: u64 },
}

/// Read a response body into memory, refusing to buffer more than `cap`
/// bytes.
///
/// Unlike [`Response::bytes`], this streams chunk by chunk and bails out
/// as soon as the running total exceeds `cap`, so a server advertising a
/// small body (or no `Content-Length` at all) and then sending gigabytes
/// cannot drive the process out of memory. A `Content-Length` over the cap
/// short-circuits before any body bytes are read.
///
/// # Errors
///
/// Returns the underlying [`reqwest::Error`] if the connection fails
/// mid-body. Exceeding the cap is not an error — it comes back as
/// [`CappedBody::TooLarge`] so callers can decide how to record it.
///
/// # Examples
///
/// ```no_run
/// # use kiki_rss::http::{read_body_capped, CappedBody};
/// # async fn run(resp: reqwest::Response) -> Result<(), reqwest::Error> {
/// match read_body_capped(resp, 1024 * 1024).await? {
///     CappedBody::Complete(bytes) => println!("read {} bytes", bytes.len()),
///     CappedBody::TooLarge { seen } => println!("over cap at {} bytes", seen),
/// }
/// # Ok(())
/// # }
/// ```
pub async fn read_body_capped(mut resp: Response, cap: u64) -> Result<CappedBody, reqwest::Error> {
    // Trust a declared over-cap length enough to skip the download, but
    // never trust an under-cap one — the running total below is what
    // actually enforces the limit.
    if let Some(declared) = resp.content_length() {
        if declared > cap {
            return Ok(CappedBody::TooLarge { seen: declared });
        }
    }

    let mut buf: Vec<u8> = Vec::new();
    let mut seen: u64 = 0;
    while let Some(chunk) = resp.chunk().await? {
        seen += chunk.len() as u64;
        if seen > cap {
            return Ok(CappedBody::TooLarge { seen });
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(CappedBody::Complete(buf))
}

/// Per-feed authentication scheme applied when fetching feed content.
///
/// Values round-trip through the `feeds.auth_type` column via
/// [`FeedAuthType::from_db`] / [`FeedAuthType::as_db`]. `None` and `"none"`
/// both deserialize to [`FeedAuthType::None`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize, utoipa::ToSchema)]
#[cfg_attr(feature = "mcp", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum FeedAuthType {
    /// No authentication is applied to the fetch request.
    #[default]
    None,
    /// HTTP Basic authentication ([RFC 7617]).
    Basic,
    /// HTTP Bearer token authentication ([RFC 6750]).
    Bearer,
}

impl FeedAuthType {
    /// Parse a value read from the `feeds.auth_type` column.
    ///
    /// `None` and the string `"none"` both map to [`FeedAuthType::None`];
    /// unrecognized strings produce an error.
    pub fn from_db(value: Option<&str>) -> Result<Self, FeedAuthError> {
        match value {
            None => Ok(FeedAuthType::None),
            Some(s) => match s.to_ascii_lowercase().as_str() {
                "" | "none" => Ok(FeedAuthType::None),
                "basic" => Ok(FeedAuthType::Basic),
                "bearer" => Ok(FeedAuthType::Bearer),
                other => Err(FeedAuthError::UnknownType(other.to_string())),
            },
        }
    }

    /// Encode this value for storage in the `feeds.auth_type` column.
    ///
    /// [`FeedAuthType::None`] is stored as SQL `NULL` (represented here as
    /// `None`) so existing rows without authentication remain unchanged.
    pub fn as_db(&self) -> Option<&'static str> {
        match self {
            FeedAuthType::None => None,
            FeedAuthType::Basic => Some("basic"),
            FeedAuthType::Bearer => Some("bearer"),
        }
    }
}

impl fmt::Display for FeedAuthType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FeedAuthType::None => f.write_str("none"),
            FeedAuthType::Basic => f.write_str("basic"),
            FeedAuthType::Bearer => f.write_str("bearer"),
        }
    }
}

/// Credentials used to authenticate a feed fetch.
///
/// Instances are constructed from the `feeds` row that backs a given feed
/// and applied to a request via [`FeedAuth::apply`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FeedAuth {
    pub auth_type: FeedAuthType,
    pub username: Option<String>,
    pub password: Option<String>,
    pub bearer_token: Option<String>,
}

impl FeedAuth {
    /// Validate that the provided fields match the declared `auth_type`.
    ///
    /// Returns an error for obviously nonsensical combinations (e.g. basic
    /// auth with no username, or bearer auth with no token) so we fail fast
    /// at input boundaries rather than silently fetching without credentials.
    pub fn validate(&self) -> Result<(), FeedAuthError> {
        match self.auth_type {
            FeedAuthType::None => Ok(()),
            FeedAuthType::Basic => {
                if self.username.as_deref().is_none_or(str::is_empty) {
                    return Err(FeedAuthError::MissingField {
                        auth_type: FeedAuthType::Basic,
                        field: "auth_username",
                    });
                }
                Ok(())
            }
            FeedAuthType::Bearer => {
                if self.bearer_token.as_deref().is_none_or(str::is_empty) {
                    return Err(FeedAuthError::MissingField {
                        auth_type: FeedAuthType::Bearer,
                        field: "auth_bearer_token",
                    });
                }
                Ok(())
            }
        }
    }

    /// Apply this authentication to an in-progress request.
    ///
    /// A [`FeedAuthType::None`] value (or missing credentials) is a no-op;
    /// callers should still validate with [`FeedAuth::validate`] before
    /// persisting if they want to catch misconfiguration up front.
    pub fn apply(&self, request: RequestBuilder) -> RequestBuilder {
        match self.auth_type {
            FeedAuthType::None => request,
            FeedAuthType::Basic => match self.username.as_deref() {
                Some(user) if !user.is_empty() => {
                    request.basic_auth(user, self.password.as_deref())
                }
                _ => request,
            },
            FeedAuthType::Bearer => match self.bearer_token.as_deref() {
                Some(token) if !token.is_empty() => request.bearer_auth(token),
                _ => request,
            },
        }
    }
}

/// Errors produced while parsing or validating per-feed authentication input.
#[derive(Debug, thiserror::Error)]
pub enum FeedAuthError {
    #[error("unknown auth_type '{0}'; expected one of: none, basic, bearer")]
    UnknownType(String),

    #[error("auth_type '{auth_type}' requires '{field}'")]
    MissingField {
        auth_type: FeedAuthType,
        field: &'static str,
    },
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Build a `reqwest::Response` with a known-length body, which is the
    /// shape `content_length()` can answer for up front.
    fn sized_response(body: Vec<u8>) -> Response {
        Response::from(::http::Response::new(body))
    }

    /// Destructure a [`CappedBody`] into the complete-body case, so tests
    /// can assert with `assert_eq!` instead of panicking on the wrong arm.
    fn complete(body: CappedBody) -> Option<Vec<u8>> {
        match body {
            CappedBody::Complete(b) => Some(b),
            CappedBody::TooLarge { .. } => None,
        }
    }

    /// Destructure a [`CappedBody`] into the over-cap case, yielding the
    /// number of bytes seen.
    fn too_large(body: CappedBody) -> Option<u64> {
        match body {
            CappedBody::TooLarge { seen } => Some(seen),
            CappedBody::Complete(_) => None,
        }
    }

    #[tokio::test]
    async fn capped_read_returns_body_under_cap() {
        let read = read_body_capped(sized_response(b"hello".to_vec()), 1024)
            .await
            .unwrap();
        assert_eq!(complete(read), Some(b"hello".to_vec()));
    }

    #[tokio::test]
    async fn capped_read_accepts_body_exactly_at_cap() {
        let read = read_body_capped(sized_response(vec![b'x'; 64]), 64)
            .await
            .unwrap();
        assert_eq!(complete(read).map(|b| b.len()), Some(64));
    }

    #[tokio::test]
    async fn capped_read_rejects_declared_overage_without_reading() {
        let read = read_body_capped(sized_response(vec![b'x'; 100]), 10)
            .await
            .unwrap();
        assert_eq!(too_large(read), Some(100));
    }

    /// The important case: a server that never declares a length and just
    /// keeps sending. `Content-Length` cannot save us here — only the
    /// running total in the read loop can.
    #[tokio::test]
    async fn capped_read_stops_streaming_body_with_no_content_length() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(_) => return,
            };
            let mut discard = [0u8; 1024];
            let _ = stream.read(&mut discard);
            // Chunked encoding, so no Content-Length is ever sent.
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n");
            let chunk = vec![b'x'; 4096];
            // Far more than the cap applied below. Writes start failing
            // once the client hangs up mid-body, which is the point.
            for _ in 0..64 {
                if stream
                    .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                    .is_err()
                    || stream.write_all(&chunk).is_err()
                    || stream.write_all(b"\r\n").is_err()
                {
                    return;
                }
            }
            let _ = stream.write_all(b"0\r\n\r\n");
        });

        let resp = reqwest::Client::new()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.content_length(),
            None,
            "test precondition: chunked response must not declare a length"
        );

        let seen = too_large(read_body_capped(resp, 8192).await.unwrap());
        let seen = seen.unwrap_or_default();
        assert!(
            seen > 8192,
            "expected to stop just past the cap, saw {seen}"
        );
        assert!(
            seen < 8192 * 8,
            "read far past the cap before bailing out: {seen}"
        );

        let _ = server.join();
    }

    #[test]
    fn from_db_parses_known_values() {
        assert_eq!(FeedAuthType::from_db(None).unwrap(), FeedAuthType::None);
        assert_eq!(
            FeedAuthType::from_db(Some("none")).unwrap(),
            FeedAuthType::None
        );
        assert_eq!(
            FeedAuthType::from_db(Some("basic")).unwrap(),
            FeedAuthType::Basic
        );
        assert_eq!(
            FeedAuthType::from_db(Some("Bearer")).unwrap(),
            FeedAuthType::Bearer
        );
    }

    #[test]
    fn from_db_rejects_unknown() {
        assert!(matches!(
            FeedAuthType::from_db(Some("oauth2")),
            Err(FeedAuthError::UnknownType(_))
        ));
    }

    #[test]
    fn as_db_roundtrip() {
        for variant in [
            FeedAuthType::None,
            FeedAuthType::Basic,
            FeedAuthType::Bearer,
        ] {
            assert_eq!(FeedAuthType::from_db(variant.as_db()).unwrap(), variant);
        }
    }

    #[test]
    fn validate_none_always_ok() {
        let a = FeedAuth::default();
        assert!(a.validate().is_ok());
    }

    #[test]
    fn validate_basic_requires_username() {
        let a = FeedAuth {
            auth_type: FeedAuthType::Basic,
            username: None,
            password: Some("hunter2".into()),
            bearer_token: None,
        };
        assert!(a.validate().is_err());

        let a = FeedAuth {
            auth_type: FeedAuthType::Basic,
            username: Some("alice".into()),
            password: None,
            bearer_token: None,
        };
        assert!(a.validate().is_ok());
    }

    #[test]
    fn validate_bearer_requires_token() {
        let a = FeedAuth {
            auth_type: FeedAuthType::Bearer,
            username: None,
            password: None,
            bearer_token: None,
        };
        assert!(a.validate().is_err());

        let a = FeedAuth {
            auth_type: FeedAuthType::Bearer,
            username: None,
            password: None,
            bearer_token: Some("tok".into()),
        };
        assert!(a.validate().is_ok());
    }
}
