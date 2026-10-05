//! Write [Kiki](https://github.com/kernelmethod/kiki-rss) plugins in Rust.
//!
//! A Kiki plugin is a WebAssembly component targeting the `plugin` world of
//! `wit/kiki-plugin.wit`. This crate generates the bindings for it and wraps them in the
//! [`Plugin`] trait: implement the handlers your plugin needs, and export it with
//! [`export_plugin!`].
//!
//! ```ignore
//! use kiki_plugin::{export_plugin, Entry, EventKind, Plugin};
//!
//! struct Shout;
//!
//! impl Plugin for Shout {
//!     const EVENTS: &'static [EventKind] = &[EventKind::EntryIngest];
//!
//!     fn new(_config: &str) -> Result<Self, String> {
//!         Ok(Shout)
//!     }
//!
//!     fn on_entry_ingest(&mut self, mut entry: Entry) -> Option<Entry> {
//!         entry.title = entry.title.to_uppercase();
//!         Some(entry)
//!     }
//! }
//!
//! export_plugin!(Shout);
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

use std::any::Any;
use std::cell::RefCell;

/// The bindings generated from `kiki-plugin.wit`.
#[doc(hidden)]
pub mod bindings {
    wit_bindgen::generate!({
        path: "../../../wit",
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

/// A Kiki plugin.
///
/// One value of the type is made, with [`Plugin::new`], when the plugin loads, and every
/// handler is called on it. Only the events in [`Plugin::EVENTS`] are delivered, so each
/// handler the plugin implements should be listed there; the default handlers do
/// nothing.
///
/// A handler that panics traps: Kiki treats it as having failed (an entry passes through
/// it unchanged), then starts the plugin afresh, calling [`Plugin::new`] again.
pub trait Plugin: Sized + 'static {
    /// The events the plugin handles. Timers started with [`host::every`] call
    /// [`Plugin::on_timer`] whether or not they are listed.
    const EVENTS: &'static [EventKind];

    /// Make the plugin, from its config: a JSON object, its manifest's `[config]` table
    /// with its overrides applied.
    ///
    /// # Errors
    ///
    /// An error fails the load, as a Lua plugin's error at its top level does.
    fn new(config: &str) -> Result<Self, String>;

    /// `entry.parsed`: a newly parsed entry, before any changes.
    fn on_entry_parsed(&mut self, entry: Entry) {
        let _ = entry;
    }

    /// `entry.ingest`: change an entry, or drop it by returning `None`.
    fn on_entry_ingest(&mut self, entry: Entry) -> Option<Entry> {
        Some(entry)
    }

    /// `fetch.success`: a feed was fetched.
    fn on_fetch_success(&mut self, event: FetchSuccess) {
        let _ = event;
    }

    /// `fetch.error`: a feed could not be fetched.
    fn on_fetch_error(&mut self, event: FetchError) {
        let _ = event;
    }

    /// `feed.added`: a feed was added.
    fn on_feed_added(&mut self, feed: FeedEvent) {
        let _ = feed;
    }

    /// `feed.removed`: a feed was removed.
    fn on_feed_removed(&mut self, feed: FeedEvent) {
        let _ = feed;
    }

    /// `plugin.load`: plugins have loaded. Where to start scans.
    fn on_plugin_load(&mut self) {}

    /// `fetch.schedule`: return a longer wait before the feed's next fetch, in seconds,
    /// or `None` to leave it.
    fn on_fetch_schedule(&mut self, schedule: FetchSchedule) -> Option<u64> {
        let _ = schedule;
        None
    }

    /// A timer started with [`host::every`] is due.
    fn on_timer(&mut self, id: u32) {
        let _ = id;
    }

    /// An entry from the scan `scan`, started with [`host::start_scan`]. System tags added
    /// to its `tags` are applied to it; `None` leaves it alone.
    fn on_scan_entry(&mut self, scan: u64, entry: Entry) -> Option<Entry> {
        let _ = (scan, entry);
        None
    }

    /// The scan `scan` has gone through every entry.
    fn on_scan_done(&mut self, scan: u64, summary: ScanSummary) {
        let _ = (scan, summary);
    }
}

thread_local! {
    static INSTANCE: RefCell<Option<Box<dyn Any>>> = const { RefCell::new(None) };
}

/// Make the plugin `P` from `config`, for its `init` export.
#[doc(hidden)]
pub fn __init<P: Plugin>(config: String) -> Result<Vec<EventKind>, String> {
    let plugin = P::new(&config)?;
    INSTANCE.with(|i| *i.borrow_mut() = Some(Box::new(plugin)));
    Ok(P::EVENTS.to_vec())
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

/// Export `$plugin`, a type implementing [`Plugin`], as the component's plugin.
#[macro_export]
macro_rules! export_plugin {
    ($plugin:ty) => {
        const _: () = {
            struct KikiPluginExports;

            impl $crate::bindings::Guest for KikiPluginExports {
                fn init(
                    config: ::std::string::String,
                ) -> ::std::result::Result<
                    ::std::vec::Vec<$crate::EventKind>,
                    ::std::string::String,
                > {
                    $crate::__init::<$plugin>(config)
                }
                fn on_entry_parsed(entry: $crate::Entry) {
                    $crate::__with::<$plugin, _>(|p| $crate::Plugin::on_entry_parsed(p, entry))
                }
                fn on_entry_ingest(entry: $crate::Entry) -> ::std::option::Option<$crate::Entry> {
                    $crate::__with::<$plugin, _>(|p| $crate::Plugin::on_entry_ingest(p, entry))
                }
                fn on_fetch_success(event: $crate::FetchSuccess) {
                    $crate::__with::<$plugin, _>(|p| $crate::Plugin::on_fetch_success(p, event))
                }
                fn on_fetch_error(event: $crate::FetchError) {
                    $crate::__with::<$plugin, _>(|p| $crate::Plugin::on_fetch_error(p, event))
                }
                fn on_feed_added(feed: $crate::FeedEvent) {
                    $crate::__with::<$plugin, _>(|p| $crate::Plugin::on_feed_added(p, feed))
                }
                fn on_feed_removed(feed: $crate::FeedEvent) {
                    $crate::__with::<$plugin, _>(|p| $crate::Plugin::on_feed_removed(p, feed))
                }
                fn on_plugin_load() {
                    $crate::__with::<$plugin, _>(|p| $crate::Plugin::on_plugin_load(p))
                }
                fn on_fetch_schedule(
                    schedule: $crate::FetchSchedule,
                ) -> ::std::option::Option<u64> {
                    $crate::__with::<$plugin, _>(|p| {
                        $crate::Plugin::on_fetch_schedule(p, schedule)
                    })
                }
                fn on_timer(id: u32) {
                    $crate::__with::<$plugin, _>(|p| $crate::Plugin::on_timer(p, id))
                }
                fn on_scan_entry(
                    scan: u64,
                    entry: $crate::Entry,
                ) -> ::std::option::Option<$crate::Entry> {
                    $crate::__with::<$plugin, _>(|p| $crate::Plugin::on_scan_entry(p, scan, entry))
                }
                fn on_scan_done(scan: u64, summary: $crate::ScanSummary) {
                    $crate::__with::<$plugin, _>(|p| $crate::Plugin::on_scan_done(p, scan, summary))
                }
            }

            $crate::bindings::export!(KikiPluginExports with_types_in $crate::bindings);
        };
    };
}
