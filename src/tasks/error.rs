use serde::{Deserialize, Serialize};
use std::fmt;

/// Structured representation of feed fetch errors.
///
/// Serialized to JSON for storage in the database `last_fetch_error` column
/// and included in API responses.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(tag = "type")]
pub enum FetchError {
    /// The response body could not be parsed as RSS or Atom.
    #[serde(rename = "invalid_feed")]
    InvalidFeed { url: String },
    /// The server returned a non-success HTTP status code.
    #[serde(rename = "http_status")]
    HttpStatus { url: String, status: u16 },
    /// The maximum number of redirects was exceeded.
    #[serde(rename = "too_many_redirects")]
    TooManyRedirects { url: String },
    /// A network or other unexpected error occurred.
    #[serde(rename = "other")]
    Other { message: String },
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::InvalidFeed { url } => {
                write!(
                    f,
                    "Response from {} was not detected as a valid RSS or Atom feed",
                    url
                )
            }
            FetchError::HttpStatus { url, status } => {
                write!(f, "Received HTTP status {} while fetching {}", status, url)
            }
            FetchError::TooManyRedirects { url } => {
                write!(f, "Exceeded maximum redirects while fetching {}", url)
            }
            FetchError::Other { message } => write!(f, "{}", message),
        }
    }
}

impl FetchError {
    /// Classify a fetch failure as transient (worth retrying with backoff) or
    /// permanent (wait the full backoff cap before the next attempt).
    ///
    /// Transient: 408 Request Timeout, 429 Too Many Requests, any 5xx,
    /// network/timeout errors (`Other`).
    /// Permanent: other 4xx statuses, malformed feed bodies, redirect loops.
    pub fn is_transient(&self) -> bool {
        match self {
            FetchError::HttpStatus { status, .. } => {
                matches!(*status, 408 | 429) || (500..=599).contains(status)
            }
            FetchError::Other { .. } => true,
            FetchError::InvalidFeed { .. } | FetchError::TooManyRedirects { .. } => false,
        }
    }
}
