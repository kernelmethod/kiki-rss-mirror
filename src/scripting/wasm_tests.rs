//! Tests of the WebAssembly engine, against the plugin in `tests/wasm-fixture`, whose
//! config chooses what it does. Rebuild it with `tools/build-wasm-fixture.sh`.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use super::*;
use crate::scripting::composite::CompositeRunner;
use crate::scripting::FeedInfo;
use serde_json::json;

const FIXTURE: &[u8] = include_bytes!("../../tests/wasm-fixture/fixture.wasm");

/// Services that record the calls plugins make, and answer them as a server would.
#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<(String, ServiceCall)>>,
    store: Mutex<HashMap<String, String>>,
}

impl Recorder {
    /// What the fixture stored under `seen:<what>`, if anything.
    fn seen(&self, what: &str) -> Option<String> {
        self.store
            .lock()
            .unwrap()
            .get(&format!("seen:{what}"))
            .map(|v| serde_json::from_str::<String>(v).unwrap())
    }
}

impl ScriptServices for Recorder {
    fn call(&self, plugin: &str, call: ServiceCall) -> Result<ServiceReply, String> {
        self.calls
            .lock()
            .unwrap()
            .push((plugin.to_string(), call.clone()));
        Ok(match call {
            ServiceCall::StoreGet { key } => {
                ServiceReply::Value(self.store.lock().unwrap().get(&key).cloned())
            }
            ServiceCall::StoreSet { key, value } => {
                let mut store = self.store.lock().unwrap();
                match value {
                    Some(value) => store.insert(key, value),
                    None => store.remove(&key),
                };
                ServiceReply::Done
            }
            ServiceCall::SetEntryTag { .. } => ServiceReply::Changed(true),
            ServiceCall::StartScan { .. } => ServiceReply::ScanStarted(7),
            ServiceCall::GetFeed { feed_id } => ServiceReply::Feed(Some(FeedInfo {
                id: feed_id,
                url: Some("https://example.com/feed".to_string()),
                title: "Example".to_string(),
            })),
            ServiceCall::DeleteEntries { filter } => {
                if filter.keep_tagged != ["system:saved"] {
                    return Err(format!("unexpected filter {filter:?}"));
                }
                ServiceReply::Deleted(3)
            }
        })
    }
}

fn source(name: &str, config: serde_json::Value) -> ScriptSource {
    ScriptSource {
        name: name.to_string(),
        config: config.to_string(),
        ..ScriptSource::wasm(FIXTURE.to_vec())
    }
}

fn runner(sources: &[ScriptSource]) -> (WasmScriptRunner, Arc<Recorder>) {
    let recorder = Arc::new(Recorder::default());
    let runner = WasmScriptRunner::from_sources_with(
        sources,
        Some(recorder.clone() as Arc<dyn ScriptServices>),
    )
    .unwrap();
    (runner, recorder)
}

fn entry(title: &str) -> FeedEntry {
    FeedEntry {
        id: None,
        feed_id: 1,
        syndication_format: "rss".to_string(),
        guid: "guid".to_string(),
        published_at: Some(1_700_000_000),
        title: title.to_string(),
        url: Some("https://example.com/1".to_string()),
        content: Some("<p>hi</p>".to_string()),
        authors: vec!["Ada".to_string()],
        categories: vec!["news".to_string()],
        tags: Vec::new(),
        cache_assets: true,
    }
}

#[test]
fn ingest_handlers_run_in_order() {
    let (runner, _) = runner(&[
        source("a", json!({ "suffix": " a", "tag": "one" })),
        source("b", json!({ "suffix": " b", "tag": "two" })),
    ]);
    assert!(runner.handles(Event::EntryIngest));
    let out = runner
        .dispatch_transform_entry(entry("title"))
        .unwrap()
        .unwrap();
    assert_eq!(out.title, "title a b");
    assert_eq!(out.tags, ["one", "two"]);
    assert_eq!(out.content, entry("").content);
}

#[test]
fn ingest_handlers_can_drop_entries() {
    let (runner, _) = runner(&[
        source("a", json!({ "drop_title": "spam" })),
        source("b", json!({ "suffix": "!" })),
    ]);
    assert!(runner
        .dispatch_transform_entry(entry("spam"))
        .unwrap()
        .is_none());
    let kept = runner
        .dispatch_transform_entry(entry("ham"))
        .unwrap()
        .unwrap();
    assert_eq!(kept.title, "ham!");
}

#[test]
fn read_only_fields_are_restored() {
    let (runner, _) = runner(&[source("a", json!({ "change_guid": true }))]);
    let out = runner
        .dispatch_transform_entry(entry("t"))
        .unwrap()
        .unwrap();
    assert_eq!(out.guid, "guid");
    assert_eq!(out.authors, ["Ada"]);
}

/// A handler that traps lets the entry through unmodified, and the plugin is started
/// afresh, so the next entry is handled again.
#[test]
fn a_trap_passes_the_entry_through_and_restarts_the_plugin() {
    let (runner, recorder) = runner(&[source("a", json!({ "trap_title": "boom", "suffix": "!" }))]);
    runner.dispatch_observe(Event::PluginLoad, EventPayload::PluginLoad);
    runner.dispatch_transform_entry(entry("one")).unwrap();
    runner.dispatch_observe(Event::PluginLoad, EventPayload::PluginLoad);
    assert_eq!(recorder.seen("plugin.load").unwrap(), "made 1");

    let out = runner
        .dispatch_transform_entry(entry("boom"))
        .unwrap()
        .unwrap();
    assert_eq!(out.title, "boom");

    let out = runner
        .dispatch_transform_entry(entry("two"))
        .unwrap()
        .unwrap();
    assert_eq!(out.title, "two!");
    // A fresh instance: the count made before the trap is gone.
    runner.dispatch_observe(Event::PluginLoad, EventPayload::PluginLoad);
    assert_eq!(recorder.seen("plugin.load").unwrap(), "made 1");
}

#[test]
fn a_handler_that_runs_too_long_is_stopped() {
    let mut slow = source("a", json!({ "spin_title": "spin", "suffix": "!" }));
    slow.time_budget = TimeBudget::Millis(50);
    let (runner, _) = runner(&[slow]);
    let start = Instant::now();
    let out = runner
        .dispatch_transform_entry(entry("spin"))
        .unwrap()
        .unwrap();
    assert_eq!(out.title, "spin");
    assert!(start.elapsed() < Duration::from_secs(5));
    let out = runner
        .dispatch_transform_entry(entry("next"))
        .unwrap()
        .unwrap();
    assert_eq!(out.title, "next!");
}

#[test]
fn a_handler_that_uses_too_much_memory_fails() {
    let (runner, _) = runner(&[source("a", json!({ "hog_title": "hog", "suffix": "!" }))]);
    let out = runner
        .dispatch_transform_entry(entry("hog"))
        .unwrap()
        .unwrap();
    assert_eq!(out.title, "hog");
    let out = runner
        .dispatch_transform_entry(entry("next"))
        .unwrap()
        .unwrap();
    assert_eq!(out.title, "next!");
}

#[test]
fn a_plugin_that_keeps_trapping_is_disabled() {
    let (runner, _) = runner(&[source("a", json!({ "trap_title": "boom" }))]);
    for _ in 0..=MAX_TRAPS {
        runner.dispatch_transform_entry(entry("boom")).unwrap();
    }
    assert!(!runner.handles(Event::EntryIngest));
    assert_eq!(runner.subscriptions(), EventSet::default());
    let out = runner
        .dispatch_transform_entry(entry("t"))
        .unwrap()
        .unwrap();
    assert_eq!(out.title, "t");
}

#[test]
fn init_errors_fail_the_load() {
    let err = WasmScriptRunner::from_sources_with(
        &[source("a", json!({ "init_error": "bad config" }))],
        None,
    )
    .err()
    .unwrap();
    assert!(matches!(err, WasmError::Init { .. }), "{err}");
    assert!(err.to_string().contains("bad config"), "{err}");

    let err = WasmScriptRunner::from_sources_with(
        &[source("a", json!({ "no_such_option": true }))],
        None,
    )
    .err()
    .unwrap();
    assert!(err.to_string().contains("invalid config"), "{err}");
}

#[test]
fn bad_components_fail_to_compile() {
    let err =
        WasmScriptRunner::from_sources_with(&[ScriptSource::wasm(b"not wasm".to_vec())], None)
            .err()
            .unwrap();
    assert!(matches!(err, WasmError::Compile { .. }), "{err}");

    // A component that doesn't target the plugin world.
    let other = wat::parse_str("(component)").unwrap();
    let err = WasmScriptRunner::from_sources_with(&[ScriptSource::wasm(other)], None)
        .err()
        .unwrap();
    assert!(matches!(err, WasmError::Instantiate { .. }), "{err}");

    // A core module is made into a component, which here doesn't target the world either.
    let module = wat::parse_str("(module)").unwrap();
    let err = WasmScriptRunner::from_sources_with(&[ScriptSource::wasm(module)], None)
        .err()
        .unwrap();
    assert!(
        matches!(
            err,
            WasmError::Compile { .. } | WasmError::Instantiate { .. }
        ),
        "{err}"
    );
}

/// A component compiled for one runner is not kept once the runner is gone, unless it
/// was put in the cache for the script host.
#[test]
fn only_components_put_are_kept() {
    let mut bytes = FIXTURE.to_vec();
    // A custom section, so that the bytes, and so the hash, are this test's own.
    bytes.extend_from_slice(&[0, 5, 4, b'k', b'e', b'e', b'p']);
    let source = ScriptSource {
        config: json!({}).to_string(),
        ..ScriptSource::wasm(bytes)
    };
    runner(std::slice::from_ref(&source));
    let mut by_hash = source;
    by_hash.component.as_mut().unwrap().bytes.clear();
    let err = WasmScriptRunner::from_sources_with(&[by_hash], None)
        .err()
        .unwrap();
    assert!(matches!(err, WasmError::MissingComponent { .. }), "{err}");
}

#[test]
fn components_are_found_by_hash_once_compiled() {
    let component = WasmComponent::new(FIXTURE.to_vec());
    put_component("a", &component).unwrap();
    let mut source = source("a", json!({ "suffix": "!" }));
    source.component.as_mut().unwrap().bytes.clear();
    let (runner, _) = runner(&[source.clone()]);
    let out = runner
        .dispatch_transform_entry(entry("t"))
        .unwrap()
        .unwrap();
    assert_eq!(out.title, "t!");

    let unknown = WasmComponent {
        hash: [0xab; 32],
        bytes: Vec::new(),
    };
    source.component = Some(unknown);
    let err = WasmScriptRunner::from_sources_with(&[source], None)
        .err()
        .unwrap();
    assert!(matches!(err, WasmError::MissingComponent { .. }), "{err}");
}

#[test]
fn observe_events_reach_the_plugin() {
    let (runner, recorder) = runner(&[source("a", json!({}))]);
    runner.dispatch_observe(Event::EntryParsed, EventPayload::Entry(entry("parsed")));
    runner.dispatch_observe(
        Event::FetchSuccess,
        EventPayload::FetchSuccess {
            feed_id: 3,
            status: 200,
            url: "https://example.com/feed".to_string(),
            content_length: Some(10),
        },
    );
    runner.dispatch_observe(
        Event::FetchError,
        EventPayload::FetchError {
            feed_id: 4,
            kind: "timeout".into(),
            status: None,
            message: "took too long".to_string(),
            retry_after: None,
        },
    );
    let feed = EventPayload::Feed {
        id: 5,
        url: "https://example.com/5".to_string(),
        title: "Five".to_string(),
    };
    runner.dispatch_observe(Event::FeedAdded, feed.clone());
    runner.dispatch_observe(Event::FeedRemoved, feed);
    assert_eq!(recorder.seen("entry.parsed").unwrap(), "parsed");
    assert_eq!(
        recorder.seen("fetch.success").unwrap(),
        "3 200 https://example.com/feed"
    );
    assert_eq!(
        recorder.seen("fetch.error").unwrap(),
        "4 timeout took too long"
    );
    assert_eq!(
        recorder.seen("feed.added").unwrap(),
        "5 https://example.com/5"
    );
    assert_eq!(
        recorder.seen("feed.removed").unwrap(),
        "5 https://example.com/5"
    );
    // Every call is made on the plugin's behalf.
    assert!(recorder.calls.lock().unwrap().iter().all(|(p, _)| p == "a"));
}

fn schedule(wait_secs: u64) -> FetchSchedule {
    FetchSchedule {
        feed_id: 1,
        status: 200,
        change: ContentChange::Unchanged,
        hint_secs: 60,
        interval_secs: 3600,
        min_cadence_secs: 60,
        wait_secs,
    }
}

#[test]
fn schedule_handlers_chain() {
    let (runner, _) = runner(&[
        source("a", json!({ "wait_secs": 600 })),
        source("b", json!({})),
        source("c", json!({ "wait_secs": 300 })),
    ]);
    let decision = runner.dispatch_schedule(schedule(120)).unwrap().unwrap();
    // c sees a's wait, and keeps the longer.
    assert_eq!(decision.wait_secs, 600);
    assert_eq!(decision.plugin, "c");

    let (runner, _) = runner_without_schedule();
    assert!(runner.dispatch_schedule(schedule(120)).unwrap().is_none());
}

fn runner_without_schedule() -> (WasmScriptRunner, Arc<Recorder>) {
    runner(&[source("a", json!({}))])
}

#[test]
fn calls_to_the_server() {
    let (runner, recorder) = runner(&[source(
        "a",
        json!({ "store": true, "feed": true, "delete": true }),
    )]);
    runner.dispatch_observe(Event::PluginLoad, EventPayload::PluginLoad);
    assert_eq!(recorder.seen("store").unwrap(), "Ok(()) Ok(Some(42))");
    assert_eq!(
        recorder.seen("feed").unwrap(),
        r#"Ok(Some(Feed { id: 1, url: Some("https://example.com/feed"), title: "Example" }))"#
    );
    assert_eq!(recorder.seen("delete").unwrap(), "Ok(3)");
    assert!(
        recorder
            .seen("delete-bad")
            .unwrap()
            .contains("is not a system tag"),
        "{:?}",
        recorder.seen("delete-bad")
    );
}

/// Plugins get randomness and clocks, which common libraries use without being asked.
#[test]
fn plugins_get_randomness_and_clocks() {
    let (runner, recorder) = runner(&[source("a", json!({ "wasi": true }))]);
    runner.dispatch_observe(Event::PluginLoad, EventPayload::PluginLoad);
    assert_eq!(recorder.seen("wasi").unwrap(), "1 true true");
}

/// The rest of WASI, such as standard output, traps if called, without keeping the
/// plugin from loading.
#[test]
fn the_rest_of_wasi_traps() {
    let (runner, _) = runner(&[source(
        "a",
        json!({ "print_title": "print", "suffix": "!" }),
    )]);
    let out = runner
        .dispatch_transform_entry(entry("print"))
        .unwrap()
        .unwrap();
    assert_eq!(out.title, "print");
    let out = runner
        .dispatch_transform_entry(entry("t"))
        .unwrap()
        .unwrap();
    assert_eq!(out.title, "t!");
}

#[test]
fn calls_fail_without_services() {
    let runner =
        WasmScriptRunner::from_sources_with(&[source("a", json!({ "store": true }))], None)
            .unwrap();
    // The fixture's store calls fail, and it carries on.
    runner.dispatch_observe(Event::PluginLoad, EventPayload::PluginLoad);
    let out = runner
        .dispatch_transform_entry(entry("t"))
        .unwrap()
        .unwrap();
    assert_eq!(out.title, "t");
}

#[test]
fn scans() {
    let (runner, recorder) = runner(&[source(
        "a",
        json!({ "scan": true, "scan_tag": "system:hidden" }),
    )]);
    assert!(recorder
        .seen("init-scan")
        .unwrap()
        .contains("cannot start while plugins are loading"));
    assert!(!runner.has_scan(7));
    runner.dispatch_observe(Event::PluginLoad, EventPayload::PluginLoad);
    assert_eq!(recorder.seen("scan").unwrap(), "7");
    assert!(runner.has_scan(7));

    let mut stored = entry("stored");
    stored.id = Some(10);
    let results = runner.dispatch_scan(7, vec![stored]).unwrap().unwrap();
    let scanned = results[0].as_ref().unwrap();
    assert_eq!(scanned.tags, ["system:hidden"]);
    assert_eq!(scanned.id, Some(10));
    assert!(runner.dispatch_scan(8, vec![entry("x")]).unwrap().is_none());

    runner.finish_scan(
        7,
        Some(ScanSummary {
            scanned: 1,
            updated: 1,
        }),
    );
    assert_eq!(recorder.seen("scan-done").unwrap(), "7 1 1");
    assert!(!runner.has_scan(7));
    assert!(runner.dispatch_scan(7, vec![entry("x")]).unwrap().is_none());
}

#[test]
fn timers() {
    let (runner, recorder) = runner(&[source("a", json!({ "every": [60] }))]);
    assert!(runner.handles(Event::Timer));
    runner.run_timers(Instant::now());
    assert!(recorder.seen("timer").is_none());
    runner.run_timers(Instant::now() + Duration::from_secs(61));
    assert_eq!(recorder.seen("timer").unwrap(), "0");

    let err = WasmScriptRunner::from_sources_with(&[source("a", json!({ "every": [1] }))], None)
        .err()
        .unwrap();
    assert!(err.to_string().contains("between 60"), "{err}");
}

/// Lua and WebAssembly plugins run together, in plugin order.
#[test]
fn mixed_engines_run_in_plugin_order() {
    let lua = |name: &str, suffix: &str| ScriptSource {
        name: name.to_string(),
        ..ScriptSource::new(format!(
            r#"kiki.on("entry.ingest", function(e) e.title = e.title .. "{suffix}" return e end)"#
        ))
    };
    let runner = CompositeRunner::from_sources_with(
        &[
            lua("1", " lua1"),
            source("2", json!({ "suffix": " wasm2" })),
            lua("3", " lua3"),
            source("4", json!({ "suffix": " wasm4", "wait_secs": 900 })),
        ],
        None,
    )
    .unwrap();
    let out = runner
        .dispatch_transform_entry(entry("t"))
        .unwrap()
        .unwrap();
    assert_eq!(out.title, "t lua1 wasm2 lua3 wasm4");
    assert!(runner.handles(Event::FetchSchedule));
    assert_eq!(
        runner
            .dispatch_schedule(schedule(60))
            .unwrap()
            .unwrap()
            .wait_secs,
        900
    );

    let dropper = CompositeRunner::from_sources_with(
        &[source("1", json!({ "drop_title": "t" })), lua("2", " lua2")],
        None,
    )
    .unwrap();
    assert!(dropper
        .dispatch_transform_entry(entry("t"))
        .unwrap()
        .is_none());
}

/// When a timer handler traps, the timers still due were the old instance's: they are not
/// called on the restarted one, whose own timers start afresh.
#[test]
fn timers_of_a_trapped_instance_are_dropped() {
    let (runner, recorder) = runner(&[source("a", json!({ "every": [60, 60], "trap_timer": 0 }))]);
    let later = Instant::now() + Duration::from_secs(61);
    runner.run_timers(later);
    // Timer 0 trapped, and timer 1 was not called.
    assert!(recorder.seen("timer").is_none());
    // The plugin is restarted on the next tick, with timers not yet due.
    assert!(runner.handles(Event::Timer));
    runner.run_timers(later);
    assert!(recorder.seen("timer").is_none());
    assert!(runner.handles(Event::Timer));
}

/// A plugin that trapped is restarted when it is next needed, and a dispatch restarts at
/// most one plugin, so that it takes at most one load budget longer.
#[test]
fn restarts_wait_for_the_next_dispatch_one_at_a_time() {
    let (runner, _) = runner(&[
        source("a", json!({ "trap_title": "boom", "suffix": "a" })),
        source("b", json!({ "trap_title": "boom", "suffix": "b" })),
    ]);
    let out = runner
        .dispatch_transform_entry(entry("boom"))
        .unwrap()
        .unwrap();
    assert_eq!(out.title, "boom");
    // Both trapped; this dispatch restarts a, and passes the entry through b.
    let out = runner
        .dispatch_transform_entry(entry("t"))
        .unwrap()
        .unwrap();
    assert_eq!(out.title, "ta");
    let out = runner
        .dispatch_transform_entry(entry("t"))
        .unwrap()
        .unwrap();
    assert_eq!(out.title, "tab");
}

/// Compile errors in the script host name the plugin.
#[test]
fn compile_errors_name_the_plugin() {
    let err = put_component("broken", &WasmComponent::new(b"not wasm".to_vec()))
        .err()
        .unwrap();
    assert!(err.to_string().contains("'broken'"), "{err}");
}

/// Plugins are compiled to native code with Cranelift, not interpreted.
#[test]
fn plugins_are_compiled_to_native_code() {
    let engine = engine().unwrap();
    assert!(!engine.is_pulley());
}

#[test]
fn plugins_match_with_the_servers_regexes() {
    let (plugins, recorder) = runner(&[source(
        "a",
        json!({
            "regex": [r"\bkiki\b", "i"],
            "regex_set": [["rust", "i"], ["^go", ""], ["kiki", ""]],
        }),
    )]);
    plugins
        .dispatch_transform_entry(entry("Rust and Kiki"))
        .unwrap();
    assert_eq!(recorder.seen("regex").unwrap(), "true Some((9, 13))");
    assert_eq!(recorder.seen("regex-set").unwrap(), "[0]");

    // Errors read as kiki.regex's do, and fail the call rather than trapping.
    let (plugins, recorder) = runner(&[source(
        "a",
        json!({"regex": ["(", ""], "regex_set": [["a", ""], ["b", "q"]]}),
    )]);
    let out = plugins
        .dispatch_transform_entry(entry("t"))
        .unwrap()
        .unwrap();
    assert_eq!(out.title, "t");
    assert!(recorder
        .seen("regex")
        .unwrap()
        .starts_with("invalid pattern: "));
    assert!(recorder
        .seen("regex-set")
        .unwrap()
        .starts_with("pattern 1: unknown flag 'q'"));
}

#[test]
fn plugins_may_keep_only_so_many_regexes_alive() {
    let (plugins, recorder) = runner(&[source(
        "a",
        json!({"regex_count": crate::scripting::regex::MAX_LIVE_REGEXES + 1}),
    )]);
    plugins.dispatch_transform_entry(entry("t")).unwrap();
    let count = recorder.seen("regex-count").unwrap();
    assert!(
        count.starts_with(&format!("{}: too many regexes", MAX_LIVE_REGEXES)),
        "{count}"
    );
    // Dropping them frees their places.
    assert_eq!(recorder.seen("regex-after-drop").unwrap(), "ok");

    let (plugins, recorder) = runner(&[source("a", json!({"regex_count": MAX_LIVE_REGEXES}))]);
    plugins.dispatch_transform_entry(entry("t")).unwrap();
    assert_eq!(recorder.seen("regex-count").unwrap(), "ok");
}

/// A plugin compiling a pattern it has alive already, as the filter's per-field sets do,
/// shares it, and does not use up its places.
#[test]
fn regexes_alive_already_are_shared() {
    let mut regexes = Regexes::default();
    let first = regexes.get("kiki".into(), "i".into()).unwrap();
    for _ in 0..2 * MAX_LIVE_REGEXES {
        let again = regexes.get("kiki".into(), "i".into()).unwrap();
        assert!(Arc::ptr_eq(&first, &again));
    }
    // Different flags make a different regex.
    let other = regexes.get("kiki".into(), String::new()).unwrap();
    assert!(!Arc::ptr_eq(&first, &other));
    // Once dropped, a regex no longer counts.
    drop((first, other));
    for i in 0..MAX_LIVE_REGEXES {
        regexes.get(format!("x{i}"), String::new()).unwrap();
    }
}
