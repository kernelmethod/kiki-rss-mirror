//! A plugin for the tests of Kiki's WebAssembly engine (`src/scripting/wasm_tests.rs`).
//!
//! What it does is chosen by its config; see [`Config`]. It reports what it sees by
//! storing values under `seen:<what>` with `store-set`, which the tests record.

use kiki_plugin::{plugin, 
    host, parse_config, DeleteFilter, Entry, FeedEvent, FetchError,
    FetchSchedule, FetchSuccess, Level, Plugin, ScanOptions, ScanSummary,
};
use kiki_plugin::regex::{Regex, RegexSet};
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
    /// Start a timer of each of these many seconds when made.
    every: Vec<u64>,
    /// Panic when the timer with this id is due.
    trap_timer: Option<u32>,
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
    /// Compile this pattern and flags with the server's `regex`, and match each ingested
    /// entry's title with it.
    regex: Option<(String, String)>,
    /// Compile these patterns and flags as a set, and match each ingested entry's title
    /// with it.
    regex_set: Vec<(String, String)>,
    /// On ingesting an entry, compile this many regexes and keep them all alive, then
    /// drop them and compile one more.
    regex_count: Option<usize>,
}

struct Fixture {
    config: Config,
    made: u64,
}

fn seen(what: &str, value: impl Into<String>) {
    let _ = host::store_set(&format!("seen:{what}"), Some(&format!("{:?}", value.into())));
}

impl Plugin for Fixture {
    fn new(config: &str) -> Result<Self, String> {
        let config: Config = parse_config(config)?;
        if let Some(message) = &config.init_error {
            return Err(message.clone());
        }
        for secs in &config.every {
            host::every(*secs)?;
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
}

#[plugin]
impl Fixture {
    #[on(entry.parsed)]
    fn entry_parsed(&mut self, entry: Entry) {
        seen("entry.parsed", entry.title);
    }

    #[on(entry.ingest)]
    fn entry_ingest(&mut self, mut entry: Entry) -> Option<Entry> {
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
        if let Some((pattern, flags)) = &self.config.regex {
            match Regex::compile(pattern, flags) {
                Ok(re) => seen(
                    "regex",
                    format!(
                        "{} {:?}",
                        re.is_match(&entry.title),
                        re.find(&entry.title, 0)
                    ),
                ),
                Err(e) => seen("regex", e),
            }
        }
        if !self.config.regex_set.is_empty() {
            match RegexSet::compile(&self.config.regex_set) {
                Ok(set) => seen("regex-set", format!("{:?}", set.matches(&entry.title))),
                Err(e) => seen("regex-set", e),
            }
        }
        if let Some(count) = self.config.regex_count {
            let mut held = Vec::new();
            let mut result = "ok".to_string();
            for i in 0..count {
                match Regex::compile(&format!("x{i}"), "") {
                    Ok(re) => held.push(re),
                    Err(e) => {
                        result = format!("{i}: {e}");
                        break;
                    }
                }
            }
            seen("regex-count", result);
            drop(held);
            let after = Regex::compile("y", "").map_or_else(|e| e, |_| "ok".to_string());
            seen("regex-after-drop", after);
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

    #[on(fetch.success)]
    fn fetch_success(&mut self, event: FetchSuccess) {
        seen("fetch.success", format!("{} {} {}", event.feed_id, event.status, event.url));
    }

    #[on(fetch.error)]
    fn fetch_error(&mut self, event: FetchError) {
        seen("fetch.error", format!("{} {} {}", event.feed_id, event.kind, event.message));
    }

    #[on(feed.added)]
    fn feed_added(&mut self, feed: FeedEvent) {
        seen("feed.added", format!("{} {}", feed.id, feed.url));
    }

    #[on(feed.removed)]
    fn feed_removed(&mut self, feed: FeedEvent) {
        seen("feed.removed", format!("{} {}", feed.id, feed.url));
    }

    #[on(plugin.load)]
    fn plugin_load(&mut self) {
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

    #[on(fetch.schedule)]
    fn fetch_schedule(&mut self, schedule: FetchSchedule) -> Option<u64> {
        self.config.wait_secs.map(|w| w.max(schedule.wait_secs))
    }

    #[on(timer)]
    fn timer(&mut self, id: u32) {
        if Some(id) == self.config.trap_timer {
            panic!("trap requested");
        }
        seen("timer", id.to_string());
    }

    #[on(scan.entry)]
    fn scan_entry(&mut self, scan: u64, mut entry: Entry) -> Option<Entry> {
        let tag = self.config.scan_tag.clone()?;
        let _ = scan;
        entry.tags.push(tag);
        entry.title = "ignored".to_string();
        Some(entry)
    }

    #[on(scan.done)]
    fn scan_done(&mut self, scan: u64, summary: ScanSummary) {
        seen(
            "scan-done",
            format!("{scan} {} {}", summary.scanned, summary.updated),
        );
    }
}
