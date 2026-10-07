//! Write [Kiki](https://github.com/kernelmethod/kiki-rss) plugins in Rust.
//!
//! A Kiki plugin is a WebAssembly component targeting the `plugin` world of
//! `wit/kiki-plugin.wit`. This crate generates the bindings for it and wraps them in the
//! [`Plugin`] trait and the [`plugin`] attribute: implement [`Plugin`] to make the plugin
//! from its config, and put `#[plugin]` on an `impl` block holding its handlers, each a
//! method marked with the event it handles.
//!
//! ```ignore
//! use kiki_plugin::{plugin, Entry, Plugin};
//!
//! struct Shout;
//!
//! impl Plugin for Shout {
//!     fn new(_config: &str) -> Result<Self, String> {
//!         Ok(Shout)
//!     }
//! }
//!
//! #[plugin]
//! impl Shout {
//!     #[on(entry.ingest)]
//!     fn shout(&mut self, mut entry: Entry) -> Option<Entry> {
//!         entry.title = entry.title.to_uppercase();
//!         Some(entry)
//!     }
//! }
//! ```
//!
//! Build it as a `cdylib` for `wasm32-wasip2`:
//!
//! ```text
//! cargo build --release --target wasm32-wasip2
//! ```
//!
//! and install `target/wasm32-wasip2/release/<name>.wasm` as the plugin's `plugin.wasm`,
//! next to a `manifest.toml` with `engine = "wasm"`.
//!
//! Of WASI, Kiki gives plugins only randomness and clocks, so `HashMap`'s default hasher,
//! `std::time::SystemTime::now` and `std::time::Instant` work. Files, the network, the
//! environment and the standard streams are not available: `std::fs`, `std::env` and
//! `println!` trap. Use [`log`] to write to Kiki's log.

#![warn(missing_docs)]

pub use kiki_plugin_macros::plugin;
use std::any::Any;
use std::cell::RefCell;

/// The bindings generated from `kiki-plugin.wit`.
#[doc(hidden)]
pub mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "plugin",
        pub_export_macro: true,
        export_macro_name: "export",
        default_bindings_module: "::kiki_plugin::bindings",
    });
}

pub use bindings::kiki::plugin::types::{
    ContentChange, DeleteFilter, Entry, EventKind, Feed, FeedEvent, FetchError, FetchSchedule,
    FetchSuccess, Level, ScanOptions, ScanSummary,
};

/// Calls to the server: the counterparts of the Lua API's `kiki.log`, `kiki.store`,
/// `kiki.entries`, `kiki.feeds` and `kiki.every`.
pub mod host {
    pub use crate::bindings::kiki::plugin::host::*;

    #[cfg(feature = "serde")]
    /// Reads the value stored under `key`, deserialized from JSON.
    ///
    /// # Errors
    ///
    /// Fails if the server refuses the call, or the value doesn't deserialize as `T`.
    pub fn get<T: serde::de::DeserializeOwned>(key: &str) -> Result<Option<T>, String> {
        match store_get(key)? {
            None => Ok(None),
            Some(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|e| format!("store value {key:?}: {e}")),
        }
    }

    #[cfg(feature = "serde")]
    /// Stores `value`, serialized as JSON, under `key`.
    ///
    /// # Errors
    ///
    /// Fails if `value` can't be serialized, or the server refuses the call.
    pub fn set<T: serde::Serialize>(key: &str, value: &T) -> Result<(), String> {
        let text = serde_json::to_string(value).map_err(|e| e.to_string())?;
        store_set(key, Some(&text))
    }
}

/// Regular expressions, compiled and matched by the server: the counterpart of the Lua
/// API's `kiki.regex`, with the same syntax, flags and limits.
///
/// Matching runs as native code in the server, so it is faster than a regex library built
/// into the plugin, and leaves the plugin smaller: the `regex` crate adds about a
/// megabyte to a WebAssembly plugin. Flags are letters: `i` (case-insensitive), `m`
/// (multi-line), `s` (`.` matches a newline), `x` (ignore whitespace) and `U` (swap
/// greed). A plugin may have 128 distinct patterns, with their flags, alive at once, alone
/// or in a [`RegexSet`](regex::RegexSet): compiling one it has alive already shares it,
/// and dropping the last regex or set holding a pattern frees its place. A set matches
/// each of its patterns in one call, saving a call to the server per pattern.
///
/// ```ignore
/// use kiki_plugin::regex::{Regex, RegexSet};
///
/// let re = Regex::compile(r"\bkiki\b", "i")?;
/// assert!(re.is_match("Hello, Kiki!"));
/// assert_eq!(re.find("a kiki", 0), Some((2, 6)));
///
/// let set = RegexSet::compile(&[("rust".into(), "i".into()), ("^go".into(), "".into())])?;
/// assert_eq!(set.matches("Rust and go"), [0]);
/// ```
pub mod regex {
    pub use crate::bindings::kiki::plugin::regex::{Regex, RegexSet};
}

/// Write `message` to the server log at `level`.
pub fn log(level: Level, message: &str) {
    host::log(level, message);
}

/// Parse a plugin's config, the JSON object [`Plugin::new`] is given.
///
/// # Errors
///
/// Fails, with a message fit for [`Plugin::new`] to return, if the config doesn't
/// deserialize as `T`.
#[cfg(feature = "serde")]
pub fn parse_config<T: serde::de::DeserializeOwned>(config: &str) -> Result<T, String> {
    serde_json::from_str(config).map_err(|e| format!("invalid config: {e}"))
}

/// A Kiki plugin: how to make it from its config.
///
/// One value of the type is made, with [`Plugin::new`], when the plugin loads, and every
/// handler is called on it. The handlers are the methods marked `#[on(<event>)]` in the
/// `impl` block of the type marked [`#[plugin]`](plugin).
///
/// A handler that panics traps: Kiki treats it as having failed (an entry passes through
/// it unchanged), then starts the plugin afresh, calling [`Plugin::new`] again.
pub trait Plugin: Sized + 'static {
    /// Make the plugin, from its config: a JSON object, its manifest's `[config]` table
    /// with its overrides applied.
    ///
    /// # Errors
    ///
    /// An error fails the load, as a Lua plugin's error at its top level does.
    fn new(config: &str) -> Result<Self, String>;

    /// Whether this instance of the plugin, made from its config, wants `event`, one of
    /// the events it has a handler for: by default, every one. Return `false` for events
    /// the config has no use for, since Kiki calls into a plugin for every event it
    /// handles. Timers and scans the plugin started are delivered regardless.
    fn wants(&self, event: EventKind) -> bool {
        let _ = event;
        true
    }
}

thread_local! {
    static INSTANCE: RefCell<Option<Box<dyn Any>>> = const { RefCell::new(None) };
}

/// Make the plugin `P` from `config`, for its `init` export, and return which of
/// `handled`, the events it has handlers for, it wants.
#[doc(hidden)]
pub fn __init<P: Plugin>(config: String, handled: &[EventKind]) -> Result<Vec<EventKind>, String> {
    let plugin = P::new(&config)?;
    let events = handled
        .iter()
        .copied()
        .filter(|&e| plugin.wants(e))
        .collect();
    INSTANCE.with(|i| *i.borrow_mut() = Some(Box::new(plugin)));
    Ok(events)
}

/// Call `f` with the plugin `P`, for its other exports.
#[doc(hidden)]
pub fn __with<P: Plugin, R>(f: impl FnOnce(&mut P) -> R) -> R {
    INSTANCE.with(|i| {
        let mut instance = i.borrow_mut();
        let plugin = instance
            .as_mut()
            .and_then(|p| p.downcast_mut::<P>())
            .expect("kiki-plugin: a handler was called before init");
        f(plugin)
    })
}
