//! The format of API tokens, and the hashes of them that are stored.
//!
//! A token reads `kiki_<id>_<secret>`: the id of its row in the
//! `api_tokens` table, and 32 random bytes in unpadded base64url. Only
//! the SHA-256 hash of the secret is stored. The secret is random enough
//! that a slow hash would add nothing, and the id lets a token be checked
//! against a single row.

use base64::Engine;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// What every token starts with, so that tokens are easy to recognize,
/// such as by secret scanners.
pub const TOKEN_PREFIX: &str = "kiki_";

/// Random bytes in a token's secret.
const SECRET_BYTES: usize = 32;

/// A token's secret: the part after its id.
pub struct Secret(String);

impl Secret {
    /// A new secret, from the operating system's random number generator.
    ///
    /// # Errors
    ///
    /// Fails if the random number generator cannot be read.
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut bytes = [0u8; SECRET_BYTES];
        getrandom::fill(&mut bytes)?;
        Ok(Secret(
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes),
        ))
    }

    /// The hash of the secret that is stored in place of it.
    pub fn hash(&self) -> [u8; 32] {
        Sha256::digest(self.0.as_bytes()).into()
    }

    /// Whether `hash` is the hash of this secret, compared in constant
    /// time.
    pub fn matches(&self, hash: &[u8]) -> bool {
        self.hash().ct_eq(hash).into()
    }

    /// The whole token for this secret, for the token with id `id`.
    pub fn token(&self, id: i64) -> String {
        format!("{TOKEN_PREFIX}{id}_{}", self.0)
    }
}

/// A token as presented by a client, split into its id and secret.
pub struct PresentedToken {
    /// The id of the token's row.
    pub id: i64,
    /// The token's secret.
    pub secret: Secret,
}

impl PresentedToken {
    /// Split `token` into its id and secret, or `None` if it is not shaped
    /// like a Kiki token.
    ///
    /// # Examples
    ///
    /// ```
    /// use kiki_rss::auth::PresentedToken;
    ///
    /// assert_eq!(PresentedToken::parse("kiki_12_abc_def").map(|t| t.id), Some(12));
    /// assert!(PresentedToken::parse("kiki_x_abc").is_none());
    /// assert!(PresentedToken::parse("12_abc").is_none());
    /// ```
    pub fn parse(token: &str) -> Option<Self> {
        let (id, secret) = token.strip_prefix(TOKEN_PREFIX)?.split_once('_')?;
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) || secret.is_empty() {
            return None;
        }
        Some(PresentedToken {
            id: id.parse().ok()?,
            secret: Secret(secret.to_owned()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() -> Result<(), getrandom::Error> {
        let secret = Secret::generate()?;
        let hash = secret.hash();
        let token = secret.token(7);
        assert!(token.starts_with("kiki_7_"));
        assert_eq!(token.len(), "kiki_7_".len() + 43);

        let presented = PresentedToken::parse(&token).ok_or(getrandom::Error::UNSUPPORTED)?;
        assert_eq!(presented.id, 7);
        assert!(presented.secret.matches(&hash));
        assert!(!Secret::generate()?.matches(&hash));
        Ok(())
    }

    #[test]
    fn rejects_malformed_tokens() {
        for token in [
            "",
            "kiki_",
            "kiki_1",
            "kiki_1_",
            "kiki__abc",
            "kiki_-1_abc",
            "x_1_abc",
        ] {
            assert!(PresentedToken::parse(token).is_none(), "{token:?}");
        }
    }
}
