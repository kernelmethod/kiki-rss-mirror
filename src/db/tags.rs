//! User tags and system tags.
//!
//! Every tag has a [`TagKind`]. **User** tags are created, renamed, and
//! deleted by the user (directly through the API, or indirectly by scripts
//! and OPML imports). **System** tags are a fixed set of tags that Kiki
//! seeds into the database to record per-entry metadata such as whether an
//! entry has been read; see [`SystemTag`].
//!
//! System tags share the `tags` table (and hence the tag name namespace)
//! with user tags, so they can be used anywhere a tag can, e.g. in entry
//! search filters. To keep the two apart, system tag names all start with
//! [`SYSTEM_TAG_PREFIX`], and user tags may not use that prefix.
//!
//! ```
//! use kiki_rss::db::tags::{is_reserved_tag_name, SystemTag};
//!
//! assert_eq!(SystemTag::Read.name(), "system:read");
//! assert!(is_reserved_tag_name("system:read"));
//! assert!(!is_reserved_tag_name("news"));
//! ```
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ValueRef};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Prefix shared by the names of all system tags. User tags may not start
/// with it (compared case-insensitively).
pub const SYSTEM_TAG_PREFIX: &str = "system:";

/// Whether a tag is managed by the user or by Kiki itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum TagKind {
    /// A tag created and managed by the user.
    User,
    /// A built-in tag recording entry metadata (see [`SystemTag`]).
    System,
}

impl TagKind {
    /// The value stored in the `tags.kind` column.
    pub fn as_str(self) -> &'static str {
        match self {
            TagKind::User => "user",
            TagKind::System => "system",
        }
    }
}

impl FromSql for TagKind {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "user" => Ok(TagKind::User),
            "system" => Ok(TagKind::System),
            other => Err(FromSqlError::Other(
                format!("unknown tag kind {other:?}").into(),
            )),
        }
    }
}

/// The built-in system tags.
///
/// These are seeded into every database and cannot be created, renamed, or
/// deleted through the API. They can be added to or removed from entries
/// with `PUT`/`DELETE /v1/entries/id/{id}/system-tags/{name}`, where
/// `{name}` is the tag's [short name](SystemTag::short_name).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemTag {
    /// The entry has been read.
    Read,
    /// The entry has been saved for later.
    Saved,
    /// The entry has been hidden from view.
    Hidden,
}

impl SystemTag {
    /// Every system tag.
    pub const ALL: [SystemTag; 3] = [SystemTag::Read, SystemTag::Saved, SystemTag::Hidden];

    /// The tag's name without the [`SYSTEM_TAG_PREFIX`], e.g. `"read"`.
    pub fn short_name(self) -> &'static str {
        match self {
            SystemTag::Read => "read",
            SystemTag::Saved => "saved",
            SystemTag::Hidden => "hidden",
        }
    }

    /// The tag's full name as stored in the database, e.g. `"system:read"`.
    pub fn name(self) -> &'static str {
        match self {
            SystemTag::Read => "system:read",
            SystemTag::Saved => "system:saved",
            SystemTag::Hidden => "system:hidden",
        }
    }

    /// Look up the tag's ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails, including
    /// [`rusqlite::Error::QueryReturnedNoRows`] if the tag has not been
    /// seeded into the database.
    pub fn id(self, conn: &Connection) -> rusqlite::Result<i64> {
        conn.query_row(
            "SELECT id FROM tags WHERE name = ?1 AND kind = 'system'",
            [self.name()],
            |row| row.get(0),
        )
    }
}

impl fmt::Display for SystemTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Error returned when parsing an unknown system tag name.
#[derive(Debug, thiserror::Error)]
#[error("unknown system tag {0:?}")]
pub struct UnknownSystemTag(pub String);

impl FromStr for SystemTag {
    type Err = UnknownSystemTag;

    /// Parse a system tag from either its short name (`"read"`) or its full
    /// name (`"system:read"`).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let short = s.strip_prefix(SYSTEM_TAG_PREFIX).unwrap_or(s);
        SystemTag::ALL
            .into_iter()
            .find(|tag| tag.short_name() == short)
            .ok_or_else(|| UnknownSystemTag(s.to_string()))
    }
}

/// Whether `name` is reserved for system tags, and so cannot be used as the
/// name of a user tag.
pub fn is_reserved_tag_name(name: &str) -> bool {
    name.get(..SYSTEM_TAG_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(SYSTEM_TAG_PREFIX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ConnectionBuilder;
    use anyhow::Result;

    #[test]
    fn reserved_names() {
        assert!(is_reserved_tag_name("system:read"));
        assert!(is_reserved_tag_name("System:Anything"));
        assert!(is_reserved_tag_name("system:"));
        assert!(!is_reserved_tag_name("system"));
        assert!(!is_reserved_tag_name("my system:tag"));
        assert!(!is_reserved_tag_name("é"));
    }

    #[test]
    fn parse_system_tags() {
        for tag in SystemTag::ALL {
            assert_eq!(tag.short_name().parse::<SystemTag>().ok(), Some(tag));
            assert_eq!(tag.name().parse::<SystemTag>().ok(), Some(tag));
            assert_eq!(
                tag.name(),
                format!("{SYSTEM_TAG_PREFIX}{}", tag.short_name())
            );
        }
        assert!("starred".parse::<SystemTag>().is_err());
        assert!("system:starred".parse::<SystemTag>().is_err());
    }

    /// Every [`SystemTag`] is seeded into a new database, and nothing else
    /// is marked as a system tag.
    #[test]
    fn system_tags_are_seeded() -> Result<()> {
        let conn = ConnectionBuilder::default().in_memory().create().build()?;
        for tag in SystemTag::ALL {
            tag.id(&conn)?;
        }
        let count: usize =
            conn.query_row("SELECT COUNT(*) FROM tags WHERE kind = 'system'", [], |r| {
                r.get(0)
            })?;
        assert_eq!(count, SystemTag::ALL.len());
        Ok(())
    }

    /// System tags cannot be deleted, even with SQL run directly against
    /// the database, while user tags can.
    #[test]
    fn system_tags_cannot_be_deleted() -> Result<()> {
        let conn = ConnectionBuilder::default().in_memory().create().build()?;
        for tag in SystemTag::ALL {
            let id = tag.id(&conn)?;
            let result = conn.execute("DELETE FROM tags WHERE id = ?1", [id]);
            assert!(
                result
                    .as_ref()
                    .is_err_and(|e| e.to_string().contains("system tags cannot be deleted")),
                "deleting {tag}: {result:?}"
            );
            tag.id(&conn)?;
        }
        assert!(conn.execute("DELETE FROM tags", []).is_err());

        conn.execute("INSERT INTO tags (name) VALUES ('news')", [])?;
        assert_eq!(conn.execute("DELETE FROM tags WHERE kind = 'user'", [])?, 1);
        Ok(())
    }
}
