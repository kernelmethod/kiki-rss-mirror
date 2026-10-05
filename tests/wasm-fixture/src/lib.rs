//! A plugin for the tests of Kiki's WebAssembly engine (`src/scripting/wasm_tests.rs`).
//!
//! What it does is chosen by its config; see [`Config`]. It reports what it sees by
//! storing values under `seen:<what>` with `store-set`, which the tests record.

use kiki_plugin::{
    export_plugin, host, parse_config, DeleteFilter, Entry, EventKind, FeedEvent, FetchError,
    FetchSchedule, FetchSuccess, Level, Plugin, ScanOptions, ScanSummary,
};
use serde::Deserialize;

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Config {
    /// Append this to the title of each ingested entry.
    suffix: Option<String>,
    /// Add this tag to each ingested entry.
    tag: Option<String>,
    /// Drop ingested entries with this title.
    drop_title: Option<String>,
    /// Panic on ingesting an entry with this title.
    trap_title: Option<String>,
    /// Loop for ever on ingesting an entry with this title.
    spin_title: Option<String>,
    /// Allocate 64 MiB on ingesting an entry with this title.
    hog_title: Option<String>,
    /// Change the guid, which is read-only, of each ingested entry.
    change_guid: bool,
    /// Return this wait from `fetch.schedule`.
    wait_secs: Option<u64>,
    /// Start a timer of this many seconds when made.
    every: Option<u64>,
    /// Fail to load with this message.
    init_error: Option<String>,
    /// Start a scan from `plugin.load`.
    scan: bool,
    /// Add this system tag to each scanned entry.
    scan_tag: Option<String>,
    /// Delete entries from `plugin.load`.
    delete: bool,
    /// Store a value from `plugin.load`, and read it back.
    store: bool,
    /// Look up feed 1 from `plugin.load`.
    feed: bool,
    /// Use the randomness and clocks Kiki provides from `plugin.load`.
    wasi: bool,
    /// Print to standard output, which Kiki doesn't provide, on ingesting an entry
    /// with this title.
    print_title: Option<String>,
}

struct Fixture {
    config: Config,
    made: u64,
}

fn seen(what: &str, value: impl Into<String>) {
    let _ = host::store_set(&format!("seen:{what}"), Some(&format!("{:?}", value.into())));
}

impl Plugin for Fixture {
    const EVENTS: &'static [EventKind] = &[
        EventKind::EntryParsed,
        EventKind::EntryIngest,
        EventKind::FetchSuccess,
        EventKind::FetchError,
        EventKind::FeedAdded,
        EventKind::FeedRemoved,
        EventKind::PluginLoad,
        EventKind::FetchSchedule,
    ];

    fn new(config: &str) -> Result<Self, String> {
        let config: Config = parse_config(config)?;
        if let Some(message) = &config.init_error {
            return Err(message.clone());
        }
        if let Some(secs) = config.every {
            host::every(secs)?;
        }
        if config.scan {
            // Scans cannot start while loading; record the refusal.
            if let Err(e) = host::start_scan(ScanOptions {
                feed_id: None,
                since: None,
                include_hidden: false,
            }) {
                seen("init-scan", e);
            }
        }
        kiki_plugin::log(Level::Info, "fixture loaded");
        Ok(Fixture { config, made: 0 })
    }

    fn on_entry_parsed(&mut self, entry: Entry) {
        seen("entry.parsed", entry.title);
    }

    fn on_entry_ingest(&mut self, mut entry: Entry) -> Option<Entry> {
        let title = Some(entry.title.clone());
        if title == self.config.trap_title {
            panic!("trap requested");
        }
        if title == self.config.spin_title {
            let mut n: u64 = 0;
            loop {
                n = std::hint::black_box(n.wrapping_add(1));
            }
        }
        if title == self.config.hog_title {
            let hog = vec![1u8; 64 * 1024 * 1024];
            std::hint::black_box(&hog);
        }
        if title == self.config.print_title {
            println!("hello");
        }
        if title == self.config.drop_title {
            return None;
        }
        self.made += 1;
        if let Some(suffix) = &self.config.suffix {
            entry.title.push_str(suffix);
        }
        if let Some(tag) = &self.config.tag {
            entry.tags.push(tag.clone());
        }
        if self.config.change_guid {
            entry.guid = "changed".to_string();
            entry.authors.push("someone else".to_string());
        }
        Some(entry)
    }

    fn on_fetch_success(&mut self, event: FetchSuccess) {
        seen("fetch.success", format!("{} {} {}", event.feed_id, event.status, event.url));
    }

    fn on_fetch_error(&mut self, event: FetchError) {
        seen("fetch.error", format!("{} {} {}", event.feed_id, event.kind, event.message));
    }

    fn on_feed_added(&mut self, feed: FeedEvent) {
        seen("feed.added", format!("{} {}", feed.id, feed.url));
    }

    fn on_feed_removed(&mut self, feed: FeedEvent) {
        seen("feed.removed", format!("{} {}", feed.id, feed.url));
    }

    fn on_plugin_load(&mut self) {
        seen("plugin.load", format!("made {}", self.made));
        if self.config.scan {
            match host::start_scan(ScanOptions {
                feed_id: Some(1),
                since: None,
                include_hidden: true,
            }) {
                Ok(id) => seen("scan", id.to_string()),
                Err(e) => seen("scan", e),
            }
        }
        if self.config.delete {
            let result = host::delete_entries(&DeleteFilter {
                dropped_before: 100,
                feed_id: None,
                published_before: None,
                keep_tagged: None,
            });
            seen("delete", format!("{result:?}"));
            let bad = host::delete_entries(&DeleteFilter {
                dropped_before: 100,
                feed_id: None,
                published_before: None,
                keep_tagged: Some(vec!["system:nope".to_string()]),
            });
            seen("delete-bad", format!("{bad:?}"));
        }
        if self.config.store {
            let set = host::set("answer", &42u32);
            let got: Result<Option<u32>, String> = host::get("answer");
            seen("store", format!("{set:?} {got:?}"));
        }
        if self.config.feed {
            seen("feed", format!("{:?}", host::get_feed(1)));
        }
        if self.config.wasi {
            // A randomly seeded hasher, and both clocks.
            let mut map = std::collections::HashMap::new();
            map.insert("a", 1);
            let wall = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let start = std::time::Instant::now();
            let elapsed = start.elapsed();
            seen(
                "wasi",
                format!("{} {} {}", map["a"], wall > 1_700_000_000, elapsed.as_secs() < 1),
            );
        }
    }

    fn on_fetch_schedule(&mut self, schedule: FetchSchedule) -> Option<u64> {
        self.config.wait_secs.map(|w| w.max(schedule.wait_secs))
    }

    fn on_timer(&mut self, id: u32) {
        seen("timer", id.to_string());
    }

    fn on_scan_entry(&mut self, scan: u64, mut entry: Entry) -> Option<Entry> {
        let tag = self.config.scan_tag.clone()?;
        let _ = scan;
        entry.tags.push(tag);
        entry.title = "ignored".to_string();
        Some(entry)
    }

    fn on_scan_done(&mut self, scan: u64, summary: ScanSummary) {
        seen(
            "scan-done",
            format!("{scan} {} {}", summary.scanned, summary.updated),
        );
    }
}

export_plugin!(Fixture);
