//! Tests for the `adaptive-fetch` plugin shipped in `plugins/adaptive-fetch/`,
//! built from the crate in that directory by `build.rs`.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use crate::scripting::wasm::WasmScriptRunner;
use crate::scripting::{
    ContentChange, Event, EventPayload, FeedInfo, FetchSchedule, ScriptRunner, ScriptServices,
    ScriptSource, ServiceCall, ServiceReply,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const MANIFEST: &str = include_str!("../../plugins/adaptive-fetch/manifest.toml");
const WASM: &[u8] = include_bytes!(concat!(
    env!("OUT_DIR"),
    "/plugins-wasm/adaptive-fetch.wasm"
));

const MIN_CADENCE: u64 = 60;
const DAY: u64 = 86_400;

/// Answers the plugin's calls from memory: a store, and feeds with URLs.
#[derive(Default)]
struct FakeServices {
    store: Mutex<HashMap<String, String>>,
    feeds: HashMap<i64, String>,
    /// How many calls of each kind the plugin made.
    calls: Mutex<HashMap<&'static str, usize>>,
}

impl FakeServices {
    fn calls(&self, kind: &str) -> usize {
        self.calls.lock().unwrap().get(kind).copied().unwrap_or(0)
    }

    fn stored(&self, key: &str) -> Option<String> {
        self.store.lock().unwrap().get(key).cloned()
    }
}

impl ScriptServices for FakeServices {
    fn call(&self, plugin: &str, call: ServiceCall) -> Result<ServiceReply, String> {
        assert_eq!(plugin, "adaptive-fetch");
        let mut store = self.store.lock().unwrap();
        let mut calls = self.calls.lock().unwrap();
        match call {
            ServiceCall::StoreGet { key } => {
                *calls.entry("get").or_default() += 1;
                Ok(ServiceReply::Value(store.get(&key).cloned()))
            }
            ServiceCall::StoreSet { key, value } => {
                *calls.entry("set").or_default() += 1;
                match value {
                    Some(value) => store.insert(key, value),
                    None => store.remove(&key),
                };
                Ok(ServiceReply::Done)
            }
            ServiceCall::GetFeed { feed_id } => {
                *calls.entry("feed").or_default() += 1;
                Ok(ServiceReply::Feed(self.feeds.get(&feed_id).map(|url| {
                    FeedInfo {
                        id: feed_id,
                        url: Some(url.clone()),
                        title: "feed".into(),
                    }
                })))
            }
            other => Err(format!("unexpected call {other:?}")),
        }
    }
}

/// The plugin, loaded with `config`, answering its calls with `services`.
fn plugin(config: Value, services: Arc<FakeServices>) -> WasmScriptRunner {
    load(config, Some(services)).unwrap()
}

/// The plugin, loaded with `config`, answering its calls with `services`, if
/// given.
fn load(config: Value, services: Option<Arc<FakeServices>>) -> Result<WasmScriptRunner, String> {
    let source = ScriptSource {
        name: "adaptive-fetch".to_string(),
        config: config.to_string(),
        ..ScriptSource::new(WASM.to_vec())
    };
    let services = services.map(|s| s as Arc<dyn ScriptServices>);
    WasmScriptRunner::from_sources_with(&[source], services).map_err(|e| e.to_string())
}

/// The plugin with its default config, and its services.
fn default_plugin() -> (WasmScriptRunner, Arc<FakeServices>) {
    let manifest = crate::plugins::PluginManifest::parse(MANIFEST).unwrap();
    let services = Arc::new(FakeServices::default());
    (
        plugin(Value::Object(manifest.config), services.clone()),
        services,
    )
}

/// A fetch of feed 1 that found it `change`d or not, with a `hint`, on a
/// feed with `interval`.
fn fetch(change: ContentChange, hint: u64, interval: u64) -> FetchSchedule {
    fetch_feed(1, change, hint, interval)
}

fn fetch_feed(feed_id: i64, change: ContentChange, hint: u64, interval: u64) -> FetchSchedule {
    FetchSchedule {
        feed_id,
        status: 304,
        change,
        hint_secs: hint,
        interval_secs: interval,
        min_cadence_secs: MIN_CADENCE,
        wait_secs: hint.max(MIN_CADENCE),
    }
}

/// The wait the plugin chose for `schedule`, if it chose one.
fn wait(runner: &WasmScriptRunner, schedule: FetchSchedule) -> Option<u64> {
    runner.dispatch_schedule(schedule).unwrap().map(|d| {
        assert_eq!(d.plugin, "adaptive-fetch");
        d.wait_secs
    })
}

/// `n` fetches of feed 1 finding it `change`d or not, with a 0s hint on a
/// feed with `interval`, returning each wait chosen.
fn waits(
    runner: &WasmScriptRunner,
    change: ContentChange,
    interval: u64,
    n: usize,
) -> Vec<Option<u64>> {
    (0..n)
        .map(|_| wait(runner, fetch(change, 0, interval)))
        .collect()
}

#[test]
fn the_manifest_is_valid() {
    let manifest = crate::plugins::PluginManifest::parse(MANIFEST).unwrap();
    assert_eq!(manifest.name, "adaptive-fetch");
    let names: Vec<_> = manifest.settings.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["feeds", "exclude"]);
    crate::plugins::settings::check_config(&manifest.settings, &manifest.config).unwrap();
}

#[test]
fn the_plugin_handles_fetch_schedule() {
    let (runner, _) = default_plugin();
    assert!(runner.handles(Event::FetchSchedule));
    // Only needed for feeds named by URL.
    assert!(!runner.handles(Event::FetchSuccess));
}

#[test]
fn unchanged_fetches_double_the_wait_up_to_the_interval() {
    let (runner, _) = default_plugin();
    let expected: Vec<Option<u64>> = [
        120, 240, 480, 960, 1920, 3840, 7680, 15_360, 30_720, 61_440, DAY, DAY, DAY,
    ]
    .into_iter()
    .map(Some)
    .collect();
    assert_eq!(waits(&runner, ContentChange::Unchanged, DAY, 13), expected);
}

#[test]
fn changed_fetches_halve_the_wait_down_to_the_hint() {
    let (runner, _) = default_plugin();
    // Held at level 11, the first that reaches the interval.
    waits(&runner, ContentChange::Unchanged, DAY, 15);
    assert_eq!(
        waits(&runner, ContentChange::Changed, DAY, 3),
        [Some(61_440), Some(30_720), Some(15_360)]
    );
    let rest = waits(&runner, ContentChange::Changed, DAY, 20);
    assert_eq!(rest.last(), Some(&None));
}

#[test]
fn unknown_change_keeps_the_level() {
    let (runner, _) = default_plugin();
    waits(&runner, ContentChange::Unchanged, DAY, 3);
    assert_eq!(
        waits(&runner, ContentChange::Unknown, DAY, 2),
        [Some(480), Some(480)]
    );
    // A feed's first fetch is unknown, and leaves it at level zero.
    assert_eq!(
        wait(&runner, fetch_feed(2, ContentChange::Unknown, 0, DAY)),
        None
    );
}

#[test]
fn a_level_beyond_a_shorter_interval_is_brought_back() {
    let (runner, _) = default_plugin();
    waits(&runner, ContentChange::Unchanged, DAY, 11);
    // The feed's interval was lowered to 1h: 1m * 2^6 reaches it, so a
    // changed fetch comes down from level 6, not 11.
    assert_eq!(
        wait(&runner, fetch(ContentChange::Changed, 0, 3600)),
        Some(1920)
    );
}

#[test]
fn the_hint_is_the_starting_point_when_above_the_floor() {
    let (runner, _) = default_plugin();
    // A 10m hint on a 1h feed: 20m, 40m, then the interval.
    let waits: Vec<_> = (0..4)
        .map(|_| wait(&runner, fetch(ContentChange::Unchanged, 600, 3600)))
        .collect();
    assert_eq!(waits, [Some(1200), Some(2400), Some(3600), Some(3600)]);
}

#[test]
fn the_polling_floor_is_the_starting_point_when_above_the_hint() {
    let (runner, _) = default_plugin();
    let mut schedule = fetch(ContentChange::Unchanged, 0, DAY);
    schedule.min_cadence_secs = 300;
    schedule.wait_secs = 300;
    let waits: Vec<_> = (0..3).map(|_| wait(&runner, schedule.clone())).collect();
    assert_eq!(waits, [Some(600), Some(1200), Some(2400)]);
}

#[test]
fn a_longer_wait_from_another_plugin_is_kept() {
    let (runner, _) = default_plugin();
    let mut schedule = fetch(ContentChange::Unchanged, 0, DAY);
    schedule.wait_secs = 5000;
    assert_eq!(wait(&runner, schedule), Some(5000));
}

#[test]
fn levels_are_stored_per_feed_and_written_only_when_they_change() {
    let (runner, services) = default_plugin();
    waits(&runner, ContentChange::Unchanged, 3600, 8);
    wait(&runner, fetch_feed(2, ContentChange::Unchanged, 0, DAY));

    // 1m * 2^6 reaches the 1h interval, so feed 1 stopped at level 6.
    assert_eq!(services.stored("level:1").as_deref(), Some("6"));
    assert_eq!(services.stored("level:2").as_deref(), Some("1"));
    // Each feed's level is read once, and written once per change.
    assert_eq!(services.calls("get"), 2);
    assert_eq!(services.calls("set"), 7);

    // Back to zero, the key is removed.
    waits(&runner, ContentChange::Changed, 3600, 6);
    assert_eq!(services.stored("level:1"), None);
}

#[test]
fn stored_levels_are_picked_up() {
    let services = Arc::new(FakeServices::default());
    services
        .store
        .lock()
        .unwrap()
        .insert("level:1".into(), "4".into());
    let runner = plugin(json!({}), services.clone());
    // From level 4, unchanged: level 5.
    assert_eq!(
        wait(&runner, fetch(ContentChange::Unchanged, 0, DAY)),
        Some(1920)
    );
}

#[test]
fn removed_feeds_lose_their_level() {
    let (runner, services) = default_plugin();
    waits(&runner, ContentChange::Unchanged, DAY, 3);
    runner.dispatch_observe(
        Event::FeedRemoved,
        EventPayload::Feed {
            id: 1,
            url: "http://example.com/".into(),
            title: "feed".into(),
        },
    );
    assert_eq!(services.stored("level:1"), None);
    // A new feed given the same id starts again.
    assert_eq!(
        wait(&runner, fetch(ContentChange::Unchanged, 0, DAY)),
        Some(120)
    );
}

#[test]
fn excluded_feeds_are_left_alone_and_lose_their_level() {
    let services = Arc::new(FakeServices {
        feeds: HashMap::from([(2, "https://example.com/two.xml".to_string())]),
        ..Default::default()
    });
    services
        .store
        .lock()
        .unwrap()
        .insert("level:1".into(), "4".into());
    let runner = plugin(
        json!({ "exclude": [1, "https://example.com/two.xml"] }),
        services.clone(),
    );
    assert_eq!(
        waits(&runner, ContentChange::Unchanged, DAY, 2),
        [None, None]
    );
    assert_eq!(services.stored("level:1"), None);
    assert_eq!(
        wait(&runner, fetch_feed(2, ContentChange::Unchanged, 0, DAY)),
        None
    );
    assert_eq!(
        wait(&runner, fetch_feed(3, ContentChange::Unchanged, 0, DAY)),
        Some(120)
    );
    // Feed URLs are looked up once each, and forgotten when a feed is
    // fetched with a 200, in case it moved.
    assert!(runner.handles(Event::FetchSuccess));
    let feed_lookups = services.calls("feed");
    wait(&runner, fetch_feed(3, ContentChange::Unchanged, 0, DAY));
    assert_eq!(services.calls("feed"), feed_lookups);
}

#[test]
fn only_the_listed_feeds_are_backed_off_from() {
    let services = Arc::new(FakeServices::default());
    let runner = plugin(json!({ "feeds": [2] }), services);
    assert_eq!(
        waits(&runner, ContentChange::Unchanged, DAY, 2),
        [None, None]
    );
    assert_eq!(
        wait(&runner, fetch_feed(2, ContentChange::Unchanged, 0, DAY)),
        Some(120)
    );
}

#[test]
fn invalid_feed_lists_fail_the_load() {
    let err = load(json!({ "exclude": [1.5] }), None).err().unwrap();
    assert!(err.contains("exclude[1]"), "{err}");
}
