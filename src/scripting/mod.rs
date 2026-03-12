//! Feed entry scripting support.
//!
//! This module provides the [`ScriptRunner`] trait for processing feed entries through
//! user-supplied scripts, and the [`FeedEntry`] type that crosses the scripting boundary.
//!
//! The [`lua`] sub-module provides a concrete Lua-based implementation ([`lua::LuaScriptRunner`])
//! when the `lua` feature is enabled.
//!
//! # Script contract
//!
//! Each script is a Lua chunk that **must return a function**. The returned function receives a
//! single argument — the entry table — and must return either a (possibly modified) table or
//! `nil`.
//!
//! ```lua
//! -- Minimal valid script: pass-through
//! return function(entry)
//!     return entry
//! end
//! ```
//!
//! # Entry table fields
//!
//! | Field               | Lua type          | Mutable |
//! |---------------------|-------------------|---------|
//! | `feed_id`           | integer           | No      |
//! | `syndication_format`| string            | No      |
//! | `guid`              | string            | No      |
//! | `published_at`      | integer or nil    | Yes     |
//! | `title`             | string            | Yes     |
//! | `url`               | string or nil     | Yes     |
//! | `content`           | string or nil     | Yes     |
//! | `tags`              | array of strings  | Yes     |
//!
//! `feed_id`, `syndication_format`, and `guid` are identity fields. They are present for scripts
//! to read, but any modifications are ignored when converting back to [`FeedEntry`].
//!
//! # Script chaining
//!
//! When multiple scripts are associated with a feed they run in sequence — the output of one
//! becomes the input to the next. If any script returns `nil` the entry is immediately filtered
//! out and subsequent scripts are not called.
//!
//! # Error handling
//!
//! If a script fails to load or throws a runtime error, the error is logged as a warning and the
//! entry passes through **unmodified**. A broken script will never silently drop entries.
//!
//! # Sandboxing
//!
//! The Lua VM is created with a restricted standard library. Only `string`, `table`, `math`,
//! `os` (with dangerous functions removed), `tostring`, `tonumber`, `type`, `pairs`, `ipairs`,
//! `select`, and `unpack` are available. Filesystem access, process execution, and module
//! loading are blocked.

#[cfg(feature = "lua")]
pub mod lua;

/// Represents a feed entry at the scripting boundary.
///
/// This struct mirrors the fields that scripts can see and modify. The identity fields
/// (`feed_id`, `syndication_format`, `guid`) are read-only from the script's perspective —
/// any changes made to them in a script are ignored when converting back from the scripting
/// layer.
#[derive(Debug, Clone)]
pub struct FeedEntry {
    /// ID of the feed this entry belongs to.
    pub feed_id: i64,
    /// Syndication format: `"rss"` or `"atom"`.
    pub syndication_format: String,
    /// Unique identifier for the entry.
    pub guid: String,
    /// Unix timestamp of the publication date, if known.
    pub published_at: Option<i64>,
    /// Entry title.
    pub title: String,
    /// Entry URL/link, if present.
    pub url: Option<String>,
    /// Entry body/description (HTML), if present.
    pub content: Option<String>,
    /// Tag names to attach to this entry. Scripts can add or remove tags; duplicates are
    /// deduplicated on the Rust side. Starts empty when the entry is first extracted.
    pub tags: Vec<String>,
}

/// Processes a feed entry through a sequence of user-supplied scripts.
///
/// Implementations may filter entries (returning `Ok(None)`) or modify them
/// (returning `Ok(Some(modified_entry))`).
///
/// Errors during script execution should be logged internally and the entry should pass
/// through **unmodified** rather than being silently dropped — only unrecoverable failures
/// should surface as `Err`.
pub trait ScriptRunner: Send + Sync {
    /// Pass `entry` through the configured scripts.
    ///
    /// # Return values
    ///
    /// - `Ok(Some(entry))` — the entry should be retained, possibly with modifications.
    /// - `Ok(None)` — a script filtered the entry out; it should not be inserted.
    /// - `Err(_)` — an unrecoverable failure; the caller decides how to proceed.
    fn process_entry(&self, entry: FeedEntry) -> anyhow::Result<Option<FeedEntry>>;
}
