//! Retention: delete entries some days after their feed stops listing them.
//!
//! Kiki's `retention` plugin, built as WebAssembly by Kiki's `build.rs` (see
//! `plugins/Cargo.toml`) and installed as `plugins/retention/plugin.wasm`. Versions
//! before 2.0.0 were written in Lua; this one takes the same config and deletes the same
//! entries.
//!
//! Kiki keeps an entry for as long as its feed lists it, and notes when a refresh finds
//! that the feed no longer does. This plugin deletes entries whose feed stopped listing
//! them more than `max_age_days` days ago, with the host's `delete-entries`, which never
//! deletes an entry its feed still lists: that entry would be fetched again, as new and
//! unread.
//!
//! It cleans up when plugins load, and then every hour.
//!
//! Config:
//!
//! * `max_age_days`: how many days to keep an entry after its feed stops listing it. 0
//!   keeps every entry forever.
//! * `keep_tags`: never delete entries tagged with any of these tags. Defaults to
//!   `["system:saved"]`; `[]` keeps none.

use kiki_plugin::{host, log, plugin, DeleteFilter, EventKind, Level, Plugin};
use serde_json::{Map, Value};

/// The time, from WASI's wall clock, which Kiki gives plugins. The standard library has
/// no clock on `wasm32-unknown-unknown`, the target Kiki's own plugins are built for.
mod clock {
    wit_bindgen::generate!({
        inline: "
            package wasi:clocks@0.2.0;

            interface wall-clock {
                record datetime {
                    seconds: u64,
                    nanoseconds: u32,
                }
                now: func() -> datetime;
            }

            world retention-clock {
                import wall-clock;
            }
        ",
        world: "retention-clock",
    });

    /// The current Unix time, in seconds.
    pub fn now() -> i64 {
        let now = wasi::clocks::wall_clock::now();
        i64::try_from(now.seconds).unwrap_or(i64::MAX)
    }
}

const DAY: i64 = 24 * 60 * 60;
const MAX_DAYS: i64 = 36500;
const HOUR: u64 = 60 * 60;

struct Retention {
    days: i64,
    keep_tags: Vec<String>,
}

impl Retention {
    fn from_config(config: &Map<String, Value>) -> Result<Self, String> {
        let days = match config.get("max_age_days") {
            None | Some(Value::Null) => Some(0),
            Some(days) => days.as_i64().filter(|days| (0..=MAX_DAYS).contains(days)),
        }
        .ok_or_else(|| {
            format!("retention: 'max_age_days' must be a whole number of days from 0 to {MAX_DAYS}")
        })?;

        let keep_tags = match config.get("keep_tags") {
            None | Some(Value::Null) => vec![Value::from("system:saved")],
            // An empty Lua table, as configs written for the Lua plugin could hold.
            Some(Value::Object(map)) if map.is_empty() => vec![],
            Some(Value::Array(tags)) => tags.clone(),
            Some(_) => return Err("retention: 'keep_tags' must be a list of tag names".into()),
        };
        let keep_tags = keep_tags
            .into_iter()
            .enumerate()
            .map(|(i, tag)| match tag {
                Value::String(tag) if !tag.is_empty() => Ok(tag),
                _ => Err(format!(
                    "retention: 'keep_tags' entry {} must be a non-empty tag name",
                    i + 1
                )),
            })
            .collect::<Result<_, _>>()?;

        Ok(Retention { days, keep_tags })
    }

    fn clean_up(&self) {
        let filter = DeleteFilter {
            dropped_before: clock::now().saturating_sub(self.days * DAY),
            feed_id: None,
            published_before: None,
            keep_tagged: Some(self.keep_tags.clone()),
        };
        match host::delete_entries(&filter) {
            Ok(0) => {}
            Ok(deleted) => log(
                Level::Info,
                &format!("retention: deleted {deleted} entries"),
            ),
            Err(e) => log(Level::Error, &format!("retention: deleting entries: {e}")),
        }
    }
}

impl Plugin for Retention {
    fn new(config: &str) -> Result<Self, String> {
        let config: Map<String, Value> =
            serde_json::from_str(config).map_err(|e| format!("retention: invalid config: {e}"))?;
        let retention = Retention::from_config(&config)?;
        if retention.days > 0 {
            host::every(HOUR).map_err(|e| format!("retention: {e}"))?;
        }
        Ok(retention)
    }

    // With nothing to delete, the plugin handles nothing.
    fn wants(&self, _event: EventKind) -> bool {
        self.days > 0
    }
}

#[plugin]
impl Retention {
    #[on(plugin.load)]
    fn clean_up_on_load(&mut self) {
        self.clean_up();
    }

    #[on(timer)]
    fn clean_up_hourly(&mut self, _id: u32) {
        self.clean_up();
    }
}
