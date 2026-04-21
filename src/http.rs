use reqwest::RequestBuilder;
use serde::{Deserialize, Serialize};
use std::fmt;

pub const USER_AGENT: &str = concat!(
    "github.com/kernelmethod/kiki-rss ",
    env!("CARGO_PKG_VERSION")
);

/// Per-feed authentication scheme applied when fetching feed content.
///
/// Values round-trip through the `feeds.auth_type` column via
/// [`FeedAuthType::from_db`] / [`FeedAuthType::as_db`]. `None` and `"none"`
/// both deserialize to [`FeedAuthType::None`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize, utoipa::ToSchema)]
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
