//! Feed entry scripting support.
//!
//! This module provides the [`ScriptRunner`] trait for dispatching server events to
//! user-supplied scripts, and the [`FeedEntry`] and [`EventPayload`] types that cross the
//! scripting boundary.
//!
//! The [`lua`] sub-module provides a concrete Lua-based implementation ([`lua::LuaScriptRunner`])
//! when the `lua` feature is enabled.
//!
//! # Events
//!
//! Scripts subscribe to server events by calling `kiki.on(event_name, handler)` in their
//! top-level chunk. The full set of events is described by the [`Event`] enum. A script's
//! top-level chunk must not return a value — returning anything is rejected at load time.
//!
//! # Script contract
//!
//! ```lua
//! kiki.on("entry.ingest", function(entry)
//!     entry.title = "[kiki] " .. entry.title
//!     return entry
//! end)
//! ```
//!
//! # Entry table fields (for `entry.*` events)
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
//! `feed_id`, `syndication_format`, and `guid` are identity fields. They are present for
//! scripts to read, but any modifications are ignored when converting back to [`FeedEntry`].
//!
//! # Handler chaining and return values
//!
//! `entry.ingest` handlers run in registration order; the output of one becomes the input
//! of the next. A handler may return `nil` to drop the entry; subsequent handlers are not
//! called. Handlers for observe-only events (everything except `entry.ingest`) are called
//! for their side effects; their return values are discarded.
//!
//! # Error handling
//!
//! If a handler fails to load, times out, or throws a runtime error, the error is logged as
//! a warning. For `entry.ingest` the entry passes through the failing handler **unmodified**;
//! for observe events the failure is simply dropped. A broken script never silently drops
//! entries.
//!
//! # Sandboxing
//!
//! Two layers, in different address spaces.
//!
//! Inside the VM: a restricted standard library. Only `string`, `table`, `math`, `os` (with
//! dangerous functions removed), `tostring`, `tonumber`, `type`, `pairs`, `ipairs`, `select`,
//! and `unpack` are available, plus the `kiki` table exposing `on`, `log`, and `regex`. Filesystem
//! access, process execution, and module loading are blocked. Scripts run under a
//! per-invocation time budget and a VM-wide memory limit (see the `lua` sub-module for the
//! concrete values).
//!
//! Around the VM: by default `kiki serve` does not host the VM at all. It runs in a separate,
//! more tightly sandboxed process that holds no database handle, no filesystem access, and no
//! sockets beyond the one it talks to the server over — so a VM escape lands somewhere with
//! nothing worth having. See [`crate::process::script_host`]. The trait below is the boundary
//! that makes this substitutable: [`lua::LuaScriptRunner`] runs the VM here,
//! [`crate::process::script_host::SubprocessScriptRunner`] forwards to the child, and callers
//! cannot tell the difference.

#[cfg(feature = "lua")]
pub mod lua;

use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::sync::{Arc, RwLock};

/// Represents a feed entry at the scripting boundary.
///
/// This struct mirrors the fields that scripts can see and modify. The identity fields
/// (`feed_id`, `syndication_format`, `guid`) are read-only from the script's perspective —
/// any changes made to them in a script are ignored when converting back from the scripting
/// layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// The set of server events that scripts may subscribe to via `kiki.on(name, handler)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Event {
    /// Fires per-entry during feed refresh, after parsing and before insertion. Handlers
    /// may transform or filter the entry by returning a modified table or `nil`.
    EntryIngest,
    /// Fires per-entry during feed refresh, immediately before `EntryIngest`. Observe-only;
    /// sees the entry exactly as parsed from the feed source.
    EntryParsed,
    /// Fires when a feed fetch completes successfully (2xx response).
    FetchSuccess,
    /// Fires when a feed fetch fails (HTTP error, timeout, network error, too many redirects).
    FetchError,
    /// Fires when a new feed is added via the HTTP API.
    FeedAdded,
    /// Fires when a feed is removed via the HTTP API.
    FeedRemoved,
}

impl Event {
    /// Returns the public string name of the event (e.g. `"entry.ingest"`).
    pub fn name(self) -> &'static str {
        match self {
            Self::EntryIngest => "entry.ingest",
            Self::EntryParsed => "entry.parsed",
            Self::FetchSuccess => "fetch.success",
            Self::FetchError => "fetch.error",
            Self::FeedAdded => "feed.added",
            Self::FeedRemoved => "feed.removed",
        }
    }

    /// Parses an event name as used by `kiki.on(name, ...)` into its enum value.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "entry.ingest" => Some(Self::EntryIngest),
            "entry.parsed" => Some(Self::EntryParsed),
            "fetch.success" => Some(Self::FetchSuccess),
            "fetch.error" => Some(Self::FetchError),
            "feed.added" => Some(Self::FeedAdded),
            "feed.removed" => Some(Self::FeedRemoved),
            _ => None,
        }
    }
}

/// Payload variants carried alongside an [`Event`] when dispatched to scripts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventPayload {
    /// Used for [`Event::EntryIngest`] and [`Event::EntryParsed`].
    Entry(FeedEntry),
    /// Used for [`Event::FetchSuccess`].
    FetchSuccess {
        feed_id: i64,
        status: u16,
        url: String,
        content_length: Option<u64>,
    },
    /// Used for [`Event::FetchError`].
    FetchError {
        feed_id: i64,
        /// One of `"http"`, `"timeout"`, `"network"`, `"too_many_redirects"`, `"parse"`.
        ///
        /// A [`Cow`] rather than a `&'static str` so the payload survives a
        /// round trip through the script host's IPC channel, where it is
        /// rebuilt from owned data.
        kind: Cow<'static, str>,
        status: Option<u16>,
        message: String,
        retry_after: Option<i64>,
    },
    /// Used for [`Event::FeedAdded`] and [`Event::FeedRemoved`].
    Feed { id: i64, url: String, title: String },
}

/// Dispatches server events to user-supplied scripts.
///
/// Implementations hold a live scripting VM plus the handlers registered by scripts at load
/// time, and route [`Event`]s to the appropriate handlers.
///
/// Transform events (currently only [`Event::EntryIngest`]) use
/// [`Self::dispatch_transform_entry`] and may return `Ok(None)` to filter the entry out.
/// All other events are observe-only and dispatched via [`Self::dispatch_observe`].
pub trait ScriptRunner: Send + Sync {
    /// Pass `entry` through each `entry.ingest` handler in registration order.
    ///
    /// # Return values
    ///
    /// - `Ok(Some(entry))` — the entry should be retained, possibly with modifications.
    /// - `Ok(None)` — a handler filtered the entry out; it should not be inserted.
    /// - `Err(_)` — an unrecoverable failure; the caller decides how to proceed.
    fn dispatch_transform_entry(&self, entry: FeedEntry) -> anyhow::Result<Option<FeedEntry>>;

    /// Fire an observe-only event to every handler registered for it.
    ///
    /// Errors inside individual handlers are logged but do not surface to the caller —
    /// observe events are fire-and-forget.
    fn dispatch_observe(&self, event: Event, payload: EventPayload);
}

/// Shared, reload-safe access to the currently-installed [`ScriptRunner`].
///
/// The runner is built once at server startup and replaced wholesale whenever scripts
/// change. Workers and HTTP handlers consume a runner by calling [`Self::current`], which
/// returns a cheap clone of the shared [`Arc`]; they then dispatch events on that snapshot
/// without blocking other readers.
///
/// The handle is cloneable and tracks the same underlying slot across clones.
#[derive(Clone, Default)]
pub struct ScriptRunnerHandle {
    inner: Arc<RwLock<Option<Arc<dyn ScriptRunner>>>>,
}

impl ScriptRunnerHandle {
    /// Construct a handle holding no runner.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Return a clone of the currently-installed runner, if any.
    ///
    /// The returned [`Arc`] is stable for the caller even if the handle is updated
    /// concurrently — events dispatched to the snapshot see a consistent runner.
    pub fn current(&self) -> Option<Arc<dyn ScriptRunner>> {
        match self.inner.read() {
            Ok(g) => g.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Install (or clear) the runner. Any subsequent [`Self::current`] calls observe the
    /// new value.
    pub fn set(&self, runner: Option<Arc<dyn ScriptRunner>>) {
        match self.inner.write() {
            Ok(mut g) => *g = runner,
            Err(poisoned) => *poisoned.into_inner() = runner,
        }
    }
}
