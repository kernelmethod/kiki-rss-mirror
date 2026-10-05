//! Feed entry scripting support.
//!
//! This module provides the [`ScriptRunner`] trait for dispatching server events to
//! user-supplied scripts, and the [`FeedEntry`] and [`EventPayload`] types that cross the
//! scripting boundary.
//!
//! The [`lua`] sub-module provides a concrete Lua-based implementation ([`lua::LuaScriptRunner`]).
//!
//! # Events
//!
//! Scripts are shipped as plugins: directories in Kiki's home with a manifest, discovered
//! by [`crate::plugins`]. Each plugin's entrypoint subscribes to server events by calling
//! `kiki.on(event_name, handler)` in its top-level chunk. The full set of events is
//! described by the [`Event`] enum. A script's top-level chunk must not return a value —
//! returning anything is rejected at load time.
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
//! | `authors`           | array of strings  | No      |
//! | `categories`        | array of strings  | No      |
//! | `tags`              | array of strings  | Yes     |
//!
//! `feed_id`, `syndication_format`, and `guid` are identity fields, and `authors` and
//! `categories` describe the entry as the feed published it. They are present for scripts to
//! read, but any modifications are ignored when converting back to [`FeedEntry`].
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
//! and `unpack` are available, plus the `kiki` table exposing `on`, `log`, `regex`, and `html`. Filesystem
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

pub mod lua;

use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::sync::{Arc, RwLock};

/// A plugin's source code together with its configuration.
///
/// Each plugin's entrypoint is called with its config as its only argument, so a
/// plugin reads it with `local config = ...`. That keeps one plugin's config out of reach
/// of the others that share its VM.
///
/// A plugin's other source files travel with it as [`ScriptModule`]s, which its code
/// loads with `require`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptSource {
    /// The name of the plugin the source belongs to, used in error messages and stack
    /// traces.
    pub name: String,
    /// The source code of the plugin's entrypoint.
    pub text: String,
    /// The script's config, as the text of a JSON object (see [`parse_script_config`]).
    ///
    /// Kept as text rather than as a parsed value because the script host's IPC codec is
    /// not self-describing, and so cannot carry a [`serde_json::Value`].
    pub config: String,
    /// The plugin's other source files, which its code can `require`.
    pub modules: Vec<ScriptModule>,
}

/// A source file of a plugin other than its entrypoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptModule {
    /// The name the module is loaded under, such as `lib.rules` for `lib/rules.lua`. See
    /// [`crate::plugins::module_name`].
    pub name: String,
    /// The module's source code.
    pub text: String,
}

impl ScriptSource {
    /// The config a script has when none has been set: an empty JSON object.
    pub const EMPTY_CONFIG: &'static str = "{}";

    /// A single-file script named `script`, with an empty config.
    ///
    /// # Examples
    ///
    /// ```
    /// use kiki_rss::scripting::ScriptSource;
    ///
    /// let source = ScriptSource::new("local config = ...");
    /// assert_eq!(source.config, "{}");
    /// assert!(source.modules.is_empty());
    /// ```
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            name: "script".to_string(),
            text: text.into(),
            config: Self::EMPTY_CONFIG.to_string(),
            modules: Vec::new(),
        }
    }
}

/// Error returned when a script's config is not a JSON object.
#[derive(Debug, thiserror::Error)]
pub enum ScriptConfigError {
    /// The config is not valid JSON.
    #[error("script config is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),

    /// The config is valid JSON, but not an object.
    #[error("script config must be a JSON object")]
    NotAnObject,
}

/// Parses a script's config, which must be a JSON object.
///
/// # Errors
///
/// Returns [`ScriptConfigError::Json`] if `text` is not valid JSON, and
/// [`ScriptConfigError::NotAnObject`] if it is some other JSON value.
///
/// # Examples
///
/// ```
/// use kiki_rss::scripting::parse_script_config;
///
/// let config = parse_script_config(r#"{"rules": []}"#).unwrap();
/// assert!(config.contains_key("rules"));
/// assert!(parse_script_config("[]").is_err());
/// ```
pub fn parse_script_config(
    text: &str,
) -> Result<serde_json::Map<String, serde_json::Value>, ScriptConfigError> {
    match serde_json::from_str(text)? {
        serde_json::Value::Object(map) => Ok(map),
        _ => Err(ScriptConfigError::NotAnObject),
    }
}

/// Represents a feed entry at the scripting boundary.
///
/// This struct mirrors the fields that scripts can see and modify. The identity fields
/// (`feed_id`, `syndication_format`, `guid`) are read-only from the script's perspective —
/// any changes made to them in a script are ignored when converting back from the scripting
/// layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedEntry {
    /// ID of the stored entry, for entries handed to a scan (see [`ServiceCall::StartScan`]).
    /// `None` for entries being ingested, which are not stored yet. Read-only for scripts.
    pub id: Option<i64>,
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
    /// Names of the entry's authors: an RSS item's `<author>`, or an Atom entry's
    /// `<author>` names. Read-only for scripts.
    pub authors: Vec<String>,
    /// The entry's categories: an RSS item's `<category>` values, or an Atom entry's
    /// `<category>` terms. Read-only for scripts.
    pub categories: Vec<String>,
    /// Tag names to attach to this entry. Scripts can add or remove tags; duplicates are
    /// deduplicated on the Rust side. Starts empty when the entry is first extracted.
    ///
    /// System tag names (such as `system:hidden`) are applied only when the entry is first
    /// stored, and never removed; see `sync_entry_tags` in [`crate::tasks`].
    pub tags: Vec<String>,
    /// Whether the images and enclosure the entry links to are downloaded into the
    /// asset cache. `true` unless a script sets it to `false`; has no effect when the
    /// asset cache is disabled.
    pub cache_assets: bool,
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
    /// Fires once plugins have loaded: when the server starts, and whenever plugins are
    /// reloaded because a plugin or its config changed.
    PluginLoad,
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
            Self::PluginLoad => "plugin.load",
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
            "plugin.load" => Some(Self::PluginLoad),
            _ => None,
        }
    }

    /// This event's bit in an [`EventSet`].
    fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

/// A set of [`Event`]s, such as those some handler is registered for.
///
/// Packed into one byte, so the script host can report its subscriptions
/// on every response for next to nothing.
///
/// # Examples
///
/// ```
/// use kiki_rss::scripting::{Event, EventSet};
///
/// let mut set = EventSet::default();
/// set.insert(Event::EntryIngest);
/// assert!(set.contains(Event::EntryIngest));
/// assert!(!set.contains(Event::EntryParsed));
/// assert!(EventSet::ALL.contains(Event::EntryParsed));
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventSet(u8);

impl EventSet {
    /// Every event.
    pub const ALL: EventSet = EventSet(u8::MAX);

    /// Whether `event` is in the set.
    pub fn contains(self, event: Event) -> bool {
        self.0 & event.bit() != 0
    }

    /// Add `event` to the set.
    pub fn insert(&mut self, event: Event) {
        self.0 |= event.bit();
    }

    /// The set as a byte, for storing in an atomic.
    pub fn to_bits(self) -> u8 {
        self.0
    }

    /// The set a byte from [`Self::to_bits`] stands for.
    pub fn from_bits(bits: u8) -> Self {
        EventSet(bits)
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
    /// Used for [`Event::PluginLoad`]. Handlers are called with no argument.
    PluginLoad,
}

/// Which stored entries a scan visits. See [`ServiceCall::StartScan`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanOptions {
    /// Only visit the entries of this feed.
    pub feed_id: Option<i64>,
    /// Only visit entries published at or after this Unix timestamp.
    pub since: Option<i64>,
    /// Also visit entries tagged `system:hidden`, which are skipped by default.
    pub include_hidden: bool,
}

/// How a scan went, handed to its `on_done` callback when it finishes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanSummary {
    /// How many entries were passed to the scan's handler.
    pub scanned: u64,
    /// How many of them gained a system tag the handler added.
    pub updated: u64,
}

/// A request from a plugin to the server, made through the `kiki` Lua API.
///
/// Plugins may run in a sandboxed process with no database access (see
/// [`crate::process::script_host`]), so everything they ask of the server goes through
/// [`ScriptServices`], which the server answers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServiceCall {
    /// Read the value stored under `key` in the plugin's store. Answered with
    /// [`ServiceReply::Value`].
    StoreGet { key: String },
    /// Store `value`, as JSON text, under `key` in the plugin's store, or remove the key
    /// when `value` is `None`. Answered with [`ServiceReply::Done`].
    StoreSet { key: String, value: Option<String> },
    /// Add the tag named `tag` to the stored entry `entry_id`, or remove it when `present`
    /// is false. Answered with [`ServiceReply::Changed`].
    SetEntryTag {
        entry_id: i64,
        tag: String,
        present: bool,
    },
    /// Start visiting the stored entries described by `options` in the background, handing
    /// them to the plugin in batches with [`ScriptRunner::dispatch_scan`]. Answered with
    /// [`ServiceReply::ScanStarted`], carrying the scan's id.
    StartScan { options: ScanOptions },
    /// Look up the feed with id `feed_id`. Answered with [`ServiceReply::Feed`].
    GetFeed { feed_id: i64 },
}

/// A feed, as plugins see it through `kiki.feeds.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedInfo {
    /// The feed's id: the `feed_id` of its entries.
    pub id: i64,
    /// The URL the feed is fetched from, if it has one.
    pub url: Option<String>,
    /// The feed's title.
    pub title: String,
}

/// The server's answer to a [`ServiceCall`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServiceReply {
    /// A stored value, as JSON text, or `None` if there is none.
    Value(Option<String>),
    /// Whether the call changed anything.
    Changed(bool),
    /// A scan started, with this id.
    ScanStarted(u64),
    /// A feed, or `None` if there is no feed with the id asked for.
    Feed(Option<FeedInfo>),
    /// The call succeeded and has nothing to report.
    Done,
}

/// Answers the [`ServiceCall`]s plugins make.
pub trait ScriptServices: Send + Sync {
    /// Answer `call`, made by the plugin named `plugin`.
    ///
    /// Called while the plugin's handler is running, so implementations must not dispatch
    /// events to scripts themselves: the script runner is busy.
    ///
    /// # Errors
    ///
    /// Returns a message, raised as a Lua error in the calling plugin, if the call fails.
    fn call(&self, plugin: &str, call: ServiceCall) -> Result<ServiceReply, String>;
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
    /// Whether any handler may be registered for `event`.
    ///
    /// Dispatching an event no handler is registered for does nothing, so callers check
    /// this first to skip building the payload. A `false` is a promise that dispatching
    /// now would do nothing; a `true` promises nothing. The default is always `true`.
    fn handles(&self, event: Event) -> bool {
        let _ = event;
        true
    }

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

    /// Pass `entries`, in order, to the handler of the scan `scan_id`, started by a plugin
    /// with [`ServiceCall::StartScan`].
    ///
    /// Returns, for each entry handled, what the handler returned: the entry, possibly
    /// modified, or `None` if the handler returned `nil` or failed. So that a scan never
    /// holds up other events for long, the runner may stop after handling only some of
    /// `entries` (but at least one); the results then cover that prefix, and the caller
    /// passes the rest again. Returns `Ok(None)` if the runner has
    /// no scan `scan_id`, because the scan was finished or the plugins were reloaded since
    /// it started; the scan should then stop.
    ///
    /// # Errors
    ///
    /// Returns an error if the entries could not be dispatched at all.
    fn dispatch_scan(
        &self,
        scan_id: u64,
        entries: Vec<FeedEntry>,
    ) -> anyhow::Result<Option<Vec<Option<FeedEntry>>>>;

    /// Forget the scan `scan_id`, releasing its handler. When `summary` is given, the scan
    /// went through every entry it was asked to, and the scan's `on_done` callback, if it
    /// has one, is called with the summary first. Scans the runner does not know are
    /// ignored.
    fn finish_scan(&self, scan_id: u64, summary: Option<ScanSummary>);
}

/// Shared access to the currently-installed [`ScriptRunner`].
///
/// The runner is built once at server startup, from the plugins discovered then. Workers and HTTP handlers consume a runner by calling [`Self::current`], which
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
