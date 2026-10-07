//! Adaptive fetching: back off from feeds that keep not changing.
//!
//! Kiki's `adaptive-fetch` plugin, built as WebAssembly by Kiki's `build.rs` (see
//! `plugins/Cargo.toml`) and installed as `plugins/adaptive-fetch/plugin.wasm`. Versions
//! before 2.0.0 were written in Lua; this one takes the same config, keeps the same
//! levels in its store, and chooses the same waits.
//!
//! Some servers send a freshness hint far shorter than the feed's fetch interval
//! (`Cache-Control: max-age=0` is common), which on its own has the feed fetched at the
//! minimum polling cadence even if it changes once a week. This plugin keeps one small
//! counter per feed, its level:
//!
//! * a fetch that finds the feed unchanged (a 304, or a 200 whose body is the same as the
//!   last one) raises the level by one;
//! * a fetch that finds it changed lowers the level by one;
//! * a fetch that cannot tell (the feed's first) leaves it alone.
//!
//! The wait the hint asks for is then stretched to `max(hint, min_cadence) * 2^level`,
//! never past the feed's own fetch interval. Raising on no change and lowering on change
//! settles the wait near the feed's real update period: about where half the fetches find
//! something new.
//!
//! Kiki only asks (with the `fetch.schedule` event) about feeds whose hint is shorter than
//! their interval; a feed without one already waits its full interval, and its level is
//! left as it was.
//!
//! Config:
//!
//! * `feeds`: the feeds to back off from, each given by its id or by the URL it is fetched
//!   from. Empty, every feed.
//! * `exclude`: feeds never to back off from, given the same way. Their levels are
//!   dropped.
//!
//! Levels are kept in the plugin's store, under `level:<feed id>`, so they outlive
//! restarts; a feed at level zero has no key.

use kiki_plugin::{
    export_plugin, host, log, ContentChange, EventKind, FeedEvent, FetchSchedule, FetchSuccess,
    Level, Plugin,
};
use serde_json::{Map, Value};
use std::collections::{hash_map, HashMap, HashSet};

/// A list of feeds from the config.
#[derive(Default)]
struct FeedSet {
    ids: HashSet<i64>,
    urls: HashSet<String>,
}

impl FeedSet {
    /// Reads config key `key`, a list of feed ids and URLs.
    fn from_config(config: &Map<String, Value>, key: &str) -> Result<Self, String> {
        let list = match config.get(key) {
            None | Some(Value::Null) => return Ok(FeedSet::default()),
            // An empty Lua table, as configs written for the Lua plugin could hold.
            Some(Value::Object(map)) if map.is_empty() => return Ok(FeedSet::default()),
            Some(Value::Array(list)) => list,
            Some(_) => {
                return Err(format!(
                    "adaptive-fetch: '{key}' must be a list of feed ids or URLs"
                ))
            }
        };
        let mut set = FeedSet::default();
        for (i, feed) in list.iter().enumerate() {
            match (feed.as_str(), as_integer(feed)) {
                (Some(url), _) if !url.is_empty() => {
                    set.urls.insert(url.to_string());
                }
                (None, Some(id)) => {
                    set.ids.insert(id);
                }
                _ => {
                    return Err(format!(
                        "adaptive-fetch: {key}[{}] must be a feed id or URL",
                        i + 1
                    ))
                }
            }
        }
        Ok(set)
    }

    fn is_empty(&self) -> bool {
        self.ids.is_empty() && self.urls.is_empty()
    }
}

/// `value` as an integer: an integer, or a float with an integer's value.
fn as_integer(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| {
        let f = value.as_f64()?;
        // i64::MAX as f64 rounds up to 2^63, which is out of range.
        (f.fract() == 0.0 && f >= i64::MIN as f64 && f < i64::MAX as f64).then_some(f as i64)
    })
}

/// The lowest level at which `base` doubled that many times reaches `interval`. Levels
/// above it change nothing, so the level is held there to let a feed that starts changing
/// come back down quickly.
fn max_level(base: u64, interval: u64) -> u64 {
    let (mut level, mut wait) = (0, base);
    while wait < interval {
        wait = wait.saturating_mul(2);
        level += 1;
    }
    level
}

/// `base` doubled `level` times, but no more than `interval`.
fn stretch(base: u64, level: u64, interval: u64) -> u64 {
    let mut wait = base;
    for _ in 0..level {
        wait = wait.saturating_mul(2);
        if wait >= interval {
            return interval;
        }
    }
    wait
}

fn store_key(feed_id: i64) -> String {
    format!("level:{feed_id}")
}

struct AdaptiveFetch {
    only: FeedSet,
    exclude: FeedSet,
    feed_urls: FeedUrls,
    /// Each feed's level, as read from or written to the store, by feed id. The plugin is
    /// the only writer of its store, so once read a level is kept here, and the store is
    /// only written when it changes.
    levels: HashMap<i64, u64>,
}

/// The URL of each feed looked up so far, by feed id; `None` for a feed with no URL, or
/// that does not exist.
#[derive(Default)]
struct FeedUrls(HashMap<i64, Option<String>>);

impl FeedUrls {
    /// The URL of feed `feed_id`, looked up once per feed. A failed lookup is logged and
    /// not remembered, so that the next fetch tries again.
    fn get(&mut self, feed_id: i64) -> Option<&str> {
        let url = match self.0.entry(feed_id) {
            hash_map::Entry::Occupied(url) => url.into_mut(),
            hash_map::Entry::Vacant(slot) => match host::get_feed(feed_id) {
                Ok(feed) => slot.insert(feed.and_then(|feed| feed.url)),
                Err(e) => {
                    log(
                        Level::Warn,
                        &format!("adaptive-fetch: looking up feed {feed_id}: {e}"),
                    );
                    return None;
                }
            },
        };
        url.as_deref()
    }

    fn forget(&mut self, feed_id: i64) {
        self.0.remove(&feed_id);
    }
}

impl FeedSet {
    /// Whether feed `feed_id` is in the set, by id or by URL.
    fn contains(&self, feed_id: i64, urls: &mut FeedUrls) -> bool {
        self.ids.contains(&feed_id)
            || (!self.urls.is_empty() && urls.get(feed_id).is_some_and(|u| self.urls.contains(u)))
    }
}

impl AdaptiveFetch {
    /// Whether the plugin backs off from feed `feed_id`.
    fn applies(&mut self, feed_id: i64) -> bool {
        if self.exclude.contains(feed_id, &mut self.feed_urls) {
            return false;
        }
        self.only.is_empty() || self.only.contains(feed_id, &mut self.feed_urls)
    }

    /// The level of feed `feed_id`, read from the store the first time. `None` if it
    /// cannot be read.
    fn level_of(&mut self, feed_id: i64) -> Option<u64> {
        if let Some(&level) = self.levels.get(&feed_id) {
            return Some(level);
        }
        let stored = match host::get::<Value>(&store_key(feed_id)) {
            Ok(stored) => stored,
            Err(e) => {
                log(
                    Level::Warn,
                    &format!("adaptive-fetch: reading the level of feed {feed_id}: {e}"),
                );
                return None;
            }
        };
        // Anything but a whole, positive number is level zero.
        let level = stored
            .as_ref()
            .and_then(as_integer)
            .and_then(|level| u64::try_from(level).ok())
            .unwrap_or(0);
        self.levels.insert(feed_id, level);
        Some(level)
    }

    fn set_level(&mut self, feed_id: i64, level: u64) {
        if self.level_of(feed_id) == Some(level) {
            return;
        }
        let key = store_key(feed_id);
        let result = if level > 0 {
            host::set(&key, &level)
        } else {
            host::store_set(&key, None)
        };
        match result {
            Ok(()) => {
                self.levels.insert(feed_id, level);
            }
            Err(e) => {
                // Read again next time, to find what is stored.
                self.levels.remove(&feed_id);
                log(
                    Level::Warn,
                    &format!("adaptive-fetch: storing the level of feed {feed_id}: {e}"),
                );
            }
        }
    }
}

impl Plugin for AdaptiveFetch {
    const EVENTS: &'static [EventKind] = &[
        EventKind::FetchSchedule,
        EventKind::FeedRemoved,
        EventKind::FetchSuccess,
    ];

    fn new(config: &str) -> Result<Self, String> {
        let config: Map<String, Value> = serde_json::from_str(config)
            .map_err(|e| format!("adaptive-fetch: invalid config: {e}"))?;
        Ok(AdaptiveFetch {
            only: FeedSet::from_config(&config, "feeds")?,
            exclude: FeedSet::from_config(&config, "exclude")?,
            feed_urls: FeedUrls::default(),
            levels: HashMap::new(),
        })
    }

    // A feed's URL changes when it is permanently redirected. Only watched for when feeds
    // are named by URL, since every handler costs each fetch a little.
    fn events(&self) -> Vec<EventKind> {
        let mut events = vec![EventKind::FetchSchedule, EventKind::FeedRemoved];
        if !self.only.urls.is_empty() || !self.exclude.urls.is_empty() {
            events.push(EventKind::FetchSuccess);
        }
        events
    }

    fn on_fetch_schedule(&mut self, fetch: FetchSchedule) -> Option<u64> {
        let feed_id = fetch.feed_id;
        if !self.applies(feed_id) {
            self.set_level(feed_id, 0);
            return None;
        }

        let interval = fetch.interval_secs;
        let base = fetch.hint_secs.max(fetch.min_cadence_secs).max(1);
        if base >= interval {
            return None;
        }
        let top = max_level(base, interval);
        // A level stored when the feed's interval was longer is brought back.
        let mut level = self.level_of(feed_id)?.min(top);
        match fetch.change {
            ContentChange::Unchanged => level = (level + 1).min(top),
            ContentChange::Changed => level = level.saturating_sub(1),
            ContentChange::Unknown => {}
        }
        self.set_level(feed_id, level);

        if level == 0 {
            return None;
        }
        // Never shorten a wait another plugin has already lengthened.
        Some(stretch(base, level, interval).max(fetch.wait_secs))
    }

    // A removed feed's id may be given to a new feed.
    fn on_feed_removed(&mut self, feed: FeedEvent) {
        self.feed_urls.forget(feed.id);
        self.levels.insert(feed.id, 0);
        if let Err(e) = host::store_set(&store_key(feed.id), None) {
            self.levels.remove(&feed.id);
            log(
                Level::Warn,
                &format!(
                    "adaptive-fetch: forgetting the level of feed {}: {e}",
                    feed.id
                ),
            );
        }
    }

    fn on_fetch_success(&mut self, fetch: FetchSuccess) {
        self.feed_urls.forget(fetch.feed_id);
    }
}

export_plugin!(AdaptiveFetch);
