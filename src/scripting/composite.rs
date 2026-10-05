//! Running plugins written for different engines together.
//!
//! Plugins load, and their handlers run, in the order of their directory names, whatever
//! engine each is written for. [`CompositeRunner`] keeps that order across engines by
//! splitting the plugins into runs of consecutive plugins with the same engine, building a
//! runner for each run, and dispatching every event to the runs in turn. For
//! `entry.ingest` and `fetch.schedule`, each run starts from what the run before it
//! returned, just as each handler does within a run.
//!
//! With plugins of one engine only, as when every plugin is written in Lua, there is a
//! single run, and the composite behaves exactly as that engine's runner. Lua plugins on
//! either side of a WebAssembly plugin are in different runs, and so in different VMs even
//! when they ask for the same permissions.

use super::lua::{LuaScriptRunner, ScriptError};
use super::{
    Event, EventPayload, EventSet, FeedEntry, FetchSchedule, ScanSummary, ScheduleDecision,
    ScriptRunner, ScriptServices, ScriptSource,
};
use std::sync::Arc;
use thiserror::Error;
use tracing::warn;

/// Why plugins could not be loaded.
#[derive(Debug, Error)]
pub enum LoadError {
    /// A Lua plugin failed to load.
    #[error(transparent)]
    Lua(#[from] ScriptError),

    /// A WebAssembly plugin failed to load.
    #[cfg(feature = "wasm-plugins")]
    #[error(transparent)]
    Wasm(#[from] super::wasm::WasmError),

    /// A plugin is written for an engine this build of Kiki cannot run.
    #[error("plugin '{0}' is a WebAssembly plugin, which this build of Kiki cannot run")]
    Unsupported(String),
}

/// The runner of one run of consecutive plugins with the same engine.
enum Segment {
    Lua(LuaScriptRunner),
    #[cfg(feature = "wasm-plugins")]
    Wasm(Box<super::wasm::WasmScriptRunner>),
}

impl Segment {
    fn runner(&self) -> &dyn ScriptRunner {
        match self {
            Segment::Lua(r) => r,
            #[cfg(feature = "wasm-plugins")]
            Segment::Wasm(r) => &**r,
        }
    }

    fn subscriptions(&self) -> EventSet {
        match self {
            Segment::Lua(r) => r.subscriptions(),
            #[cfg(feature = "wasm-plugins")]
            Segment::Wasm(r) => r.subscriptions(),
        }
    }

    fn has_scan(&self, scan_id: u64) -> bool {
        match self {
            Segment::Lua(r) => r.has_scan(scan_id),
            #[cfg(feature = "wasm-plugins")]
            Segment::Wasm(r) => r.has_scan(scan_id),
        }
    }
}

/// Runs Lua and WebAssembly plugins together, in plugin order. See the [module
/// documentation](self).
pub struct CompositeRunner {
    segments: Vec<Segment>,
}

impl CompositeRunner {
    /// Build a runner from plugins of any engine, in the order they load in, answering the
    /// calls they make to the server with `services`.
    ///
    /// # Errors
    ///
    /// Returns an error if any plugin fails to load, as each engine's runner does. Nothing
    /// runs then: the caller keeps the runner it had.
    ///
    /// # Examples
    ///
    /// ```
    /// use kiki_rss::scripting::composite::CompositeRunner;
    /// use kiki_rss::scripting::{Event, ScriptRunner, ScriptSource};
    ///
    /// let source = ScriptSource::new(r#"kiki.on("entry.ingest", function(e) return e end)"#);
    /// let runner = CompositeRunner::from_sources_with(&[source], None).unwrap();
    /// assert!(runner.handles(Event::EntryIngest));
    /// ```
    pub fn from_sources_with(
        sources: &[ScriptSource],
        services: Option<Arc<dyn ScriptServices>>,
    ) -> Result<Self, LoadError> {
        let mut segments = Vec::new();
        let mut rest = sources;
        while let Some(first) = rest.first() {
            let wasm = first.component.is_some();
            let len = rest
                .iter()
                .position(|s| s.component.is_some() != wasm)
                .unwrap_or(rest.len());
            let (run, after) = rest.split_at(len);
            rest = after;
            segments.push(build_segment(run, wasm, services.clone())?);
        }
        Ok(Self { segments })
    }

    /// The events at least one plugin has a handler for.
    pub fn subscriptions(&self) -> EventSet {
        let mut set = EventSet::default();
        for segment in &self.segments {
            set = set.union(segment.subscriptions());
        }
        set
    }
}

fn build_segment(
    run: &[ScriptSource],
    wasm: bool,
    services: Option<Arc<dyn ScriptServices>>,
) -> Result<Segment, LoadError> {
    if !wasm {
        return Ok(Segment::Lua(LuaScriptRunner::from_sources_with(
            run, services,
        )?));
    }
    #[cfg(feature = "wasm-plugins")]
    {
        Ok(Segment::Wasm(Box::new(
            super::wasm::WasmScriptRunner::from_sources_with(run, services)?,
        )))
    }
    #[cfg(not(feature = "wasm-plugins"))]
    {
        let _ = services;
        let name = run.first().map(|s| s.name.clone()).unwrap_or_default();
        Err(LoadError::Unsupported(name))
    }
}

impl ScriptRunner for CompositeRunner {
    fn handles(&self, event: Event) -> bool {
        self.segments
            .iter()
            .any(|s| s.subscriptions().contains(event))
    }

    fn dispatch_transform_entry(&self, entry: FeedEntry) -> anyhow::Result<Option<FeedEntry>> {
        let mut current = entry;
        for segment in &self.segments {
            let runner = segment.runner();
            if !runner.handles(Event::EntryIngest) {
                continue;
            }
            match runner.dispatch_transform_entry(current.clone()) {
                Ok(Some(entry)) => current = entry,
                Ok(None) => return Ok(None),
                // Never drop an entry because a run of plugins broke: it passes through
                // that run unmodified.
                Err(e) => warn!(error = %e, "entry.ingest dispatch failed; passing the entry on"),
            }
        }
        Ok(Some(current))
    }

    fn dispatch_schedule(
        &self,
        mut schedule: FetchSchedule,
    ) -> anyhow::Result<Option<ScheduleDecision>> {
        let mut decision = None;
        for segment in &self.segments {
            let runner = segment.runner();
            if !runner.handles(Event::FetchSchedule) {
                continue;
            }
            match runner.dispatch_schedule(schedule.clone()) {
                Ok(Some(d)) => {
                    schedule.wait_secs = d.wait_secs;
                    decision = Some(d);
                }
                Ok(None) => {}
                Err(e) => warn!(error = %e, "fetch.schedule dispatch failed; keeping the wait"),
            }
        }
        Ok(decision)
    }

    fn dispatch_observe(&self, event: Event, payload: EventPayload) {
        for segment in &self.segments {
            let runner = segment.runner();
            // Timers are started by handlers, so every run is asked to check its own.
            if event == Event::Timer || runner.handles(event) {
                runner.dispatch_observe(event, payload.clone());
            }
        }
    }

    fn dispatch_scan(
        &self,
        scan_id: u64,
        entries: Vec<FeedEntry>,
    ) -> anyhow::Result<Option<Vec<Option<FeedEntry>>>> {
        match self.segments.iter().find(|s| s.has_scan(scan_id)) {
            Some(segment) => segment.runner().dispatch_scan(scan_id, entries),
            None => Ok(None),
        }
    }

    fn finish_scan(&self, scan_id: u64, summary: Option<ScanSummary>) {
        if let Some(segment) = self.segments.iter().find(|s| s.has_scan(scan_id)) {
            segment.runner().finish_scan(scan_id, summary);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn entry() -> FeedEntry {
        FeedEntry {
            id: None,
            feed_id: 1,
            syndication_format: "rss".to_string(),
            guid: "guid".to_string(),
            published_at: None,
            title: "title".to_string(),
            url: None,
            content: None,
            authors: Vec::new(),
            categories: Vec::new(),
            tags: Vec::new(),
            cache_assets: true,
        }
    }

    fn lua(name: &str, text: &str) -> ScriptSource {
        ScriptSource {
            name: name.to_string(),
            ..ScriptSource::new(text)
        }
    }

    #[test]
    fn lua_plugins_run_as_one_segment() {
        let runner = CompositeRunner::from_sources_with(
            &[
                lua(
                    "a",
                    r#"kiki.on("entry.ingest", function(e) e.title = e.title .. "a" return e end)"#,
                ),
                lua(
                    "b",
                    r#"kiki.on("entry.ingest", function(e) e.title = e.title .. "b" return e end)"#,
                ),
            ],
            None,
        )
        .unwrap();
        assert_eq!(runner.segments.len(), 1);
        let out = runner.dispatch_transform_entry(entry()).unwrap().unwrap();
        assert_eq!(out.title, "titleab");
    }

    #[test]
    fn no_plugins_make_no_segments() {
        let runner = CompositeRunner::from_sources_with(&[], None).unwrap();
        assert!(runner.segments.is_empty());
        assert_eq!(runner.subscriptions(), EventSet::default());
        let out = runner.dispatch_transform_entry(entry()).unwrap().unwrap();
        assert_eq!(out.title, entry().title);
    }

    #[cfg(not(feature = "wasm-plugins"))]
    #[test]
    fn wasm_plugins_are_refused_without_the_feature() {
        let err = CompositeRunner::from_sources_with(&[ScriptSource::wasm(vec![])], None)
            .err()
            .unwrap();
        assert!(matches!(err, LoadError::Unsupported(_)));
    }
}
