//! API tokens: creating, listing, revoking and checking them.
//!
//! See [`crate::auth`] for what tokens grant and how they are presented.

use crate::auth::{PresentedToken, Scopes, Secret};
use rusqlite::{Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

/// The longest name a token may have, in characters.
pub const MAX_NAME_CHARS: usize = 64;

/// Errors from creating or checking tokens.
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    /// Another token already has the name.
    #[error("a token named {0:?} already exists")]
    NameTaken(String),

    /// The name is empty, too long, or holds control characters.
    #[error("token names must be 1 to {MAX_NAME_CHARS} characters, with no control characters")]
    InvalidName,

    /// A token must grant at least one scope.
    #[error("a token must have at least one scope")]
    NoScopes,

    /// The random number generator could not be read.
    #[error("unable to generate a token: {0}")]
    Random(getrandom::Error),

    /// The database failed.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

/// A token, as stored; its secret is never stored, and cannot be shown
/// again once it has been created.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, utoipa::ToSchema)]
pub struct Token {
    /// The token's id, which also appears in the token itself.
    pub id: i64,
    /// A name to tell the token apart by, unique among tokens.
    pub name: String,
    /// What the token may do.
    pub scopes: Scopes,
    /// Unix timestamp of when the token was created.
    pub created_at: i64,
    /// Unix timestamp after which the token is refused, if it expires.
    pub expires_at: Option<i64>,
    /// Unix timestamp of when the token was last used, to the minute, if
    /// it has been.
    pub last_used_at: Option<i64>,
}

impl Token {
    /// Whether the token has expired as of the Unix timestamp `now`.
    pub fn expired_at(&self, now: i64) -> bool {
        self.expires_at.is_some_and(|t| t <= now)
    }

    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(Token {
            id: row.get("id")?,
            name: row.get("name")?,
            scopes: Scopes::from_bits(row.get("scopes")?),
            created_at: row.get("created_at")?,
            expires_at: row.get("expires_at")?,
            last_used_at: row.get("last_used_at")?,
        })
    }
}

const COLUMNS: &str = "id, name, scopes, created_at, expires_at, last_used_at";

/// Create a token named `name` granting `scopes`, expiring at the Unix
/// timestamp `expires_at` if given, and return it along with the token
/// itself, which is not stored and cannot be retrieved later.
///
/// # Errors
///
/// Fails with [`TokenError::NameTaken`] if another token has the name,
/// [`TokenError::InvalidName`] if the name is not allowed,
/// [`TokenError::NoScopes`] if `scopes` is empty, and otherwise if the
/// database or random number generator fails.
///
/// # Examples
///
/// ```
/// use kiki_rss::db::{tokens, ConnectionBuilder};
///
/// let conn = ConnectionBuilder::default().in_memory().create().build()?;
/// let (token, secret) = tokens::create(&conn, "phone", "reader".parse()?, None)?;
/// assert_eq!(tokens::authenticate(&conn, &secret, 0)?, tokens::Authentication::Valid(token));
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn create(
    conn: &Connection,
    name: &str,
    scopes: Scopes,
    expires_at: Option<i64>,
) -> Result<(Token, String), TokenError> {
    let name = name.trim();
    if name.is_empty()
        || name.chars().count() > MAX_NAME_CHARS
        || name.chars().any(char::is_control)
    {
        return Err(TokenError::InvalidName);
    }
    if scopes.is_empty() {
        return Err(TokenError::NoScopes);
    }
    let secret = Secret::generate().map_err(TokenError::Random)?;
    let token = conn
        .query_row(
            &format!(
                "INSERT INTO api_tokens (name, secret_hash, scopes, expires_at)
                 VALUES (?1, ?2, ?3, ?4)
                 RETURNING {COLUMNS}"
            ),
            rusqlite::params![name, secret.hash(), scopes.bits(), expires_at],
            Token::from_row,
        )
        .map_err(|e| match e {
            rusqlite::Error::SqliteFailure(f, _)
                if f.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
            {
                TokenError::NameTaken(name.to_owned())
            }
            e => TokenError::Sqlite(e),
        })?;
    let full = secret.token(token.id);
    Ok((token, full))
}

/// Every token, oldest first.
///
/// # Errors
///
/// Fails if the database does.
pub fn list(conn: &Connection) -> rusqlite::Result<Vec<Token>> {
    conn.prepare(&format!("SELECT {COLUMNS} FROM api_tokens ORDER BY id"))?
        .query_map([], Token::from_row)?
        .collect()
}

/// The token with id `id`, if there is one.
///
/// # Errors
///
/// Fails if the database does.
pub fn get(conn: &Connection, id: i64) -> rusqlite::Result<Option<Token>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM api_tokens WHERE id = ?1"),
        [id],
        Token::from_row,
    )
    .optional()
}

/// The token named `name`, if there is one.
///
/// # Errors
///
/// Fails if the database does.
pub fn get_by_name(conn: &Connection, name: &str) -> rusqlite::Result<Option<Token>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM api_tokens WHERE name = ?1"),
        [name.trim()],
        Token::from_row,
    )
    .optional()
}

/// Revoke the token with id `id`, deleting it. Returns whether there was
/// such a token.
///
/// # Errors
///
/// Fails if the database does.
pub fn revoke(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    Ok(conn.execute("DELETE FROM api_tokens WHERE id = ?1", [id])? > 0)
}

/// The outcome of checking a token with [`authenticate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Authentication {
    /// The token is valid.
    Valid(Token),
    /// The token was valid, but has expired.
    Expired,
    /// The token is malformed, unknown, revoked, or its secret is wrong.
    Invalid,
}

/// Check `presented`, a token as a client sent it, as of the Unix
/// timestamp `now`.
///
/// # Errors
///
/// Fails if the database does.
pub fn authenticate(
    conn: &Connection,
    presented: &str,
    now: i64,
) -> rusqlite::Result<Authentication> {
    let Some(presented) = PresentedToken::parse(presented) else {
        return Ok(Authentication::Invalid);
    };
    let row = conn
        .query_row(
            &format!("SELECT {COLUMNS}, secret_hash FROM api_tokens WHERE id = ?1"),
            [presented.id],
            |row| Ok((Token::from_row(row)?, row.get::<_, Vec<u8>>("secret_hash")?)),
        )
        .optional()?;
    Ok(match row {
        Some((token, hash)) if presented.secret.matches(&hash) => {
            if token.expired_at(now) {
                Authentication::Expired
            } else {
                Authentication::Valid(token)
            }
        }
        _ => Authentication::Invalid,
    })
}

/// How stale a token's `last_used_at` may get before [`touch`] updates it,
/// in seconds. Updating it on every request would make every request a
/// write.
pub const LAST_USED_RESOLUTION: i64 = 60;

/// Whether [`touch`] would update `token`'s last use as of `now`.
pub fn needs_touch(token: &Token, now: i64) -> bool {
    token
        .last_used_at
        .is_none_or(|t| now - t >= LAST_USED_RESOLUTION)
}

/// Record that the token with id `id` was used at the Unix timestamp
/// `now`, unless its last use was recorded within the past
/// [`LAST_USED_RESOLUTION`] seconds.
///
/// # Errors
///
/// Fails if the database does.
pub fn touch(conn: &Connection, id: i64, now: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE api_tokens SET last_used_at = ?2
         WHERE id = ?1 AND (last_used_at IS NULL OR ?2 - last_used_at >= ?3)",
        rusqlite::params![id, now, LAST_USED_RESOLUTION],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Scope;
    use crate::db::ConnectionBuilder;
    use anyhow::Result;

    fn conn() -> Result<Connection> {
        ConnectionBuilder::default().in_memory().create().build()
    }

    #[test]
    fn create_and_authenticate() -> Result<()> {
        let conn = conn()?;
        let (token, secret) = create(&conn, " phone ", "reader".parse()?, Some(100))?;
        assert_eq!(token.name, "phone");
        assert!(token.scopes.contains(Scope::State));
        assert!(secret.starts_with(&format!("kiki_{}_", token.id)));

        assert_eq!(
            authenticate(&conn, &secret, 99)?,
            Authentication::Valid(token.clone())
        );
        assert_eq!(authenticate(&conn, &secret, 100)?, Authentication::Expired);

        // A wrong secret for a real id, and nonsense.
        let forged = format!("kiki_{}_AAAA", token.id);
        assert_eq!(authenticate(&conn, &forged, 0)?, Authentication::Invalid);
        assert_eq!(authenticate(&conn, "hunter2", 0)?, Authentication::Invalid);
        Ok(())
    }

    #[test]
    fn revoked_tokens_are_refused_and_ids_not_reused() -> Result<()> {
        let conn = conn()?;
        let (first, secret) = create(&conn, "a", Scopes::all(), None)?;
        assert!(revoke(&conn, first.id)?);
        assert!(!revoke(&conn, first.id)?);
        assert_eq!(authenticate(&conn, &secret, 0)?, Authentication::Invalid);

        let (second, _) = create(&conn, "a", Scopes::all(), None)?;
        assert_ne!(second.id, first.id);
        assert_eq!(list(&conn)?, vec![second]);
        Ok(())
    }

    #[test]
    fn rejects_bad_names_and_empty_scopes() -> Result<()> {
        let conn = conn()?;
        create(&conn, "dup", Scopes::all(), None)?;
        assert!(matches!(
            create(&conn, "dup", Scopes::all(), None),
            Err(TokenError::NameTaken(n)) if n == "dup"
        ));
        assert!(matches!(
            create(&conn, "  ", Scopes::all(), None),
            Err(TokenError::InvalidName)
        ));
        assert!(matches!(
            create(&conn, "a\nb", Scopes::all(), None),
            Err(TokenError::InvalidName)
        ));
        assert!(matches!(
            create(&conn, &"x".repeat(MAX_NAME_CHARS + 1), Scopes::all(), None),
            Err(TokenError::InvalidName)
        ));
        assert!(matches!(
            create(&conn, "none", Scopes::NONE, None),
            Err(TokenError::NoScopes)
        ));
        Ok(())
    }

    #[test]
    fn touch_is_throttled() -> Result<()> {
        let conn = conn()?;
        let (token, _) = create(&conn, "t", Scopes::all(), None)?;
        assert!(needs_touch(&token, 1000));
        touch(&conn, token.id, 1000)?;
        touch(&conn, token.id, 1030)?;
        let token = get(&conn, token.id)?.ok_or_else(|| anyhow::anyhow!("missing"))?;
        assert_eq!(token.last_used_at, Some(1000));
        assert!(!needs_touch(&token, 1030));
        assert!(needs_touch(&token, 1060));
        touch(&conn, token.id, 1060)?;
        assert_eq!(
            get(&conn, token.id)?.and_then(|t| t.last_used_at),
            Some(1060)
        );
        Ok(())
    }
}
