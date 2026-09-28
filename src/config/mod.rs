//! Kiki's runtime settings.
//!
//! Settings are resolved in two layers:
//!
//! 1. **Built-in defaults**, from [`Settings::default`].
//! 2. **Overrides** read from a TOML file, `kiki.toml` in the data
//!    directory ([`CONFIG_FILE_NAME`]). The file holds only the keys that
//!    differ from the defaults, so a default changed in a later release
//!    still reaches every install that never set that key.
//!
//! The file is laid out in the same sections as [`Settings`]:
//!
//! ```toml
//! [feed_fetch]
//! max_feed_bytes = 4096
//!
//! [asset_cache]
//! enabled = false
//!
//! [retention]
//! max_age_days = 30
//! ```
//!
//! The file is owned by Kiki: the settings API rewrites it through a
//! [`ConfigStore`], which re-reads it from disk before every change, so
//! comments and formatting are not preserved. Edits made to the file from
//! outside the server are picked up by [`watch::spawn_watcher`].

mod store;
pub mod watch;

pub use store::{ConfigHandle, ConfigStore};

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;

/// Name of the config file inside the data directory.
pub const CONFIG_FILE_NAME: &str = "kiki.toml";

/// Errors raised while loading, validating, or saving the config.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Reading or writing the config file failed.
    #[error("failed to access config file {path:?}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The config file is not valid TOML.
    #[error("failed to parse config file {path:?}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    /// A setting has the wrong type, an out-of-range value, or an unknown
    /// name. The message is suitable for returning to an API client.
    #[error("invalid setting: {0}")]
    Invalid(String),

    /// The config file on disk holds an invalid setting, so it cannot be
    /// updated until it is fixed. Distinct from [`ConfigError::Invalid`] so
    /// that an API client can tell a problem with the file apart from a
    /// problem with its own request.
    #[error("config file {path:?} holds an invalid setting: {reason}")]
    InvalidFile { path: PathBuf, reason: String },

    /// The overrides could not be serialized back to TOML.
    #[error("failed to serialize config")]
    Serialize(#[from] toml::ser::Error),
}

/// The effective settings: built-in defaults with the config file's
/// overrides applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub feed_fetch: FeedFetchSettings,
    pub asset_cache: AssetCacheSettings,
    pub retention: RetentionSettings,
}

/// Settings governing how and how often feeds are fetched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedFetchSettings {
    /// HTTP request timeout for a feed fetch, in seconds.
    pub timeout_seconds: u64,

    /// Absolute floor on how often any single feed can be polled, in
    /// seconds. Caps the effect of a very low `max-age` or `Retry-After`
    /// so a misbehaving server cannot trigger hyperpolling.
    pub min_polling_cadence_seconds: u64,

    /// Fetch interval, in seconds, given to newly added feeds. It becomes
    /// the feed's `min_fetch_interval_seconds`: the longest the scheduler
    /// waits between fetches, and the wait used when the server sends no
    /// freshness hint. Feeds already added keep their own interval, which
    /// can be changed per feed.
    pub default_fetch_interval_seconds: u64,

    /// Cap on exponential backoff for transient errors, and the wait
    /// applied to permanent errors, in seconds.
    pub max_backoff_seconds: u64,

    /// How often, in seconds, to bypass conditional-request headers and
    /// force a full `GET` on a feed. Catches servers that keep serving the
    /// same `ETag`/`Last-Modified` while the body has changed.
    pub force_refresh_after_seconds: u64,

    /// Largest feed response body, in bytes, that will be read into memory.
    /// Responses exceeding this are abandoned mid-read and recorded as
    /// [`crate::tasks::FetchError::BodyTooLarge`].
    pub max_feed_bytes: u64,
}

/// Settings for the on-disk cache of images and enclosures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetCacheSettings {
    /// Whether entry assets are downloaded and cached at all.
    pub enabled: bool,

    /// Total size, in bytes, the cache is evicted down to.
    pub max_bytes: i64,
}

/// Settings for deleting old entries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionSettings {
    /// Entries are deleted once their feed has stopped listing them for
    /// more than this many days; entries still in their feed are never
    /// deleted. `None` keeps entries forever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_days: Option<i64>,
}

/// Default for [`FeedFetchSettings::max_feed_bytes`]: 32 MiB.
///
/// Feeds are text and even very long archive feeds sit far below this.
pub const DEFAULT_MAX_FEED_BYTES: u64 = 32 * 1024 * 1024;

/// Default for [`FeedFetchSettings::default_fetch_interval_seconds`]: 3 hours.
pub const DEFAULT_FETCH_INTERVAL_SECONDS: u64 = 3 * 60 * 60;

/// Default for [`AssetCacheSettings::max_bytes`]: 1 GiB.
pub const DEFAULT_ASSET_CACHE_MAX_BYTES: i64 = 1024 * 1024 * 1024;

/// Largest accepted [`RetentionSettings::max_age_days`]: 100 years.
///
/// Anything longer is indistinguishable from keeping entries forever, and
/// an unbounded value would overflow when converted to seconds.
pub const MAX_RETENTION_DAYS: i64 = 100 * 365;

impl Default for Settings {
    fn default() -> Self {
        Settings {
            feed_fetch: FeedFetchSettings {
                timeout_seconds: 15,
                min_polling_cadence_seconds: 60,
                default_fetch_interval_seconds: DEFAULT_FETCH_INTERVAL_SECONDS,
                max_backoff_seconds: 24 * 60 * 60,
                force_refresh_after_seconds: 7 * 24 * 60 * 60,
                max_feed_bytes: DEFAULT_MAX_FEED_BYTES,
            },
            asset_cache: AssetCacheSettings {
                enabled: true,
                max_bytes: DEFAULT_ASSET_CACHE_MAX_BYTES,
            },
            retention: RetentionSettings::default(),
        }
    }
}

impl Settings {
    /// Checks the values that serde's types alone cannot rule out.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Invalid`] naming the first offending setting.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |msg: &str| Err(ConfigError::Invalid(msg.to_string()));

        if self.feed_fetch.timeout_seconds == 0 {
            return invalid("feed_fetch.timeout_seconds must be greater than zero");
        }
        // Feed intervals are stored as SQLite INTEGERs and must be positive.
        if self.feed_fetch.default_fetch_interval_seconds == 0 {
            return invalid("feed_fetch.default_fetch_interval_seconds must be greater than zero");
        }
        if i64::try_from(self.feed_fetch.default_fetch_interval_seconds).is_err() {
            return Err(ConfigError::Invalid(format!(
                "feed_fetch.default_fetch_interval_seconds must be at most {}",
                i64::MAX
            )));
        }
        // A cap of zero would reject every feed.
        if self.feed_fetch.max_feed_bytes == 0 {
            return invalid("feed_fetch.max_feed_bytes must be greater than zero");
        }
        if self.asset_cache.max_bytes < 0 {
            return invalid("asset_cache.max_bytes must be non-negative");
        }
        // Zero days would delete every entry on the next cleanup.
        if self.retention.max_age_days.is_some_and(|d| d < 1) {
            return invalid("retention.max_age_days must be at least 1");
        }
        if self
            .retention
            .max_age_days
            .is_some_and(|d| d > MAX_RETENTION_DAYS)
        {
            return Err(ConfigError::Invalid(format!(
                "retention.max_age_days must be at most {MAX_RETENTION_DAYS}"
            )));
        }
        Ok(())
    }
}

/// The contents of the config file: a sparse set of overrides on top of
/// [`Settings::default`].
///
/// Kept as a raw TOML table rather than a typed struct so that keys can be
/// set and removed by name (as a CLI will), and so that an absent key is
/// distinguishable from one explicitly set to its default.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overrides(toml::Table);

impl Overrides {
    /// Parses overrides from TOML text.
    ///
    /// Only the syntax is checked here; [`Overrides::resolve`] checks the
    /// keys and values.
    ///
    /// # Errors
    ///
    /// Returns the TOML parse error.
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        text.parse::<toml::Table>().map(Overrides)
    }

    /// Serializes the overrides as TOML text.
    ///
    /// # Errors
    ///
    /// Returns an error if a value cannot be represented in TOML.
    pub fn to_toml_string(&self) -> Result<String, ConfigError> {
        Ok(toml::to_string(&self.0)?)
    }

    /// Returns the override for `section.key`, if one is set.
    pub fn get(&self, section: &str, key: &str) -> Option<&toml::Value> {
        self.0.get(section)?.as_table()?.get(key)
    }

    /// Sets `section.key` to `value`, creating the section if needed.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Invalid`] if `value` has no TOML
    /// representation (e.g. a `u64` above `i64::MAX`), or if `section`
    /// exists but is not a table.
    ///
    /// # Examples
    ///
    /// ```
    /// use kiki_rss::config::Overrides;
    ///
    /// let mut o = Overrides::default();
    /// o.set("feed_fetch", "max_feed_bytes", 4096u64).unwrap();
    /// assert_eq!(o.resolve().unwrap().feed_fetch.max_feed_bytes, 4096);
    /// ```
    pub fn set<T: Serialize>(
        &mut self,
        section: &str,
        key: &str,
        value: T,
    ) -> Result<(), ConfigError> {
        let value = toml::Value::try_from(value)
            .map_err(|e| ConfigError::Invalid(format!("{section}.{key}: {e}")))?;
        let table = self
            .0
            .entry(section)
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .ok_or_else(|| ConfigError::Invalid(format!("{section} must be a table")))?;
        table.insert(key.to_string(), value);
        Ok(())
    }

    /// Removes the override for `section.key`, restoring the built-in
    /// default. A section left empty is removed too.
    pub fn unset(&mut self, section: &str, key: &str) {
        let Some(table) = self.0.get_mut(section).and_then(|v| v.as_table_mut()) else {
            return;
        };
        table.remove(key);
        if table.is_empty() {
            self.0.remove(section);
        }
    }

    /// Applies the overrides to the built-in defaults and validates the
    /// result.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Invalid`] for an unknown key, a value of the
    /// wrong type, or a value that fails [`Settings::validate`].
    pub fn resolve(&self) -> Result<Settings, ConfigError> {
        let mut merged = toml::Table::try_from(Settings::default())?;
        merge(&mut merged, &self.0);
        let settings: Settings = toml::Value::Table(merged)
            .try_into()
            .map_err(|e: toml::de::Error| ConfigError::Invalid(e.message().to_string()))?;
        settings.validate()?;
        Ok(settings)
    }
}

/// Recursively overlays `overrides` onto `base`. Tables are merged key by
/// key; any other value replaces what was there.
fn merge(base: &mut toml::Table, overrides: &toml::Table) {
    for (key, value) in overrides {
        match (base.get_mut(key), value) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge(b, o),
            _ => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn empty_overrides_resolve_to_defaults() {
        assert_eq!(Overrides::default().resolve().unwrap(), Settings::default());
    }

    #[test]
    fn defaults_are_valid() {
        Settings::default().validate().unwrap();
    }

    #[test]
    fn overrides_apply_per_key() {
        let o = Overrides::parse(
            "[feed_fetch]\nmax_feed_bytes = 4096\n\n[retention]\nmax_age_days = 30\n",
        )
        .unwrap();
        let s = o.resolve().unwrap();
        assert_eq!(s.feed_fetch.max_feed_bytes, 4096);
        assert_eq!(s.retention.max_age_days, Some(30));
        // Keys not overridden keep their defaults.
        assert_eq!(
            s.feed_fetch.timeout_seconds,
            Settings::default().feed_fetch.timeout_seconds
        );
        assert_eq!(s.asset_cache, Settings::default().asset_cache);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let o = Overrides::parse("[feed_fetch]\nmax_feed_byte = 4096\n").unwrap();
        assert!(matches!(o.resolve(), Err(ConfigError::Invalid(_))));

        let o = Overrides::parse("[nonsense]\nx = 1\n").unwrap();
        assert!(matches!(o.resolve(), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn wrong_types_are_rejected() {
        let o = Overrides::parse("[asset_cache]\nenabled = \"yes\"\n").unwrap();
        assert!(matches!(o.resolve(), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn out_of_range_values_are_rejected() {
        for text in [
            "[feed_fetch]\nmax_feed_bytes = 0\n",
            "[feed_fetch]\ntimeout_seconds = 0\n",
            "[feed_fetch]\ndefault_fetch_interval_seconds = 0\n",
            "[feed_fetch]\nmax_feed_bytes = -1\n",
            "[asset_cache]\nmax_bytes = -1\n",
            "[retention]\nmax_age_days = 0\n",
            "[retention]\nmax_age_days = 36501\n",
            // Would overflow `max_age_days * 86400`.
            "[retention]\nmax_age_days = 200000000000000\n",
        ] {
            let o = Overrides::parse(text).unwrap();
            assert!(
                matches!(o.resolve(), Err(ConfigError::Invalid(_))),
                "{text:?} should be rejected"
            );
        }
    }

    #[test]
    fn set_and_unset_round_trip_through_toml() {
        let mut o = Overrides::default();
        o.set("feed_fetch", "max_feed_bytes", 4096u64).unwrap();
        o.set("asset_cache", "enabled", false).unwrap();

        let reparsed = Overrides::parse(&o.to_toml_string().unwrap()).unwrap();
        assert_eq!(reparsed, o);

        o.unset("asset_cache", "enabled");
        assert!(o.get("asset_cache", "enabled").is_none());
        assert!(
            !o.to_toml_string().unwrap().contains("asset_cache"),
            "an emptied section should be dropped"
        );
    }

    #[test]
    fn values_beyond_toml_range_are_rejected() {
        let mut o = Overrides::default();
        assert!(matches!(
            o.set("feed_fetch", "max_feed_bytes", u64::MAX),
            Err(ConfigError::Invalid(_))
        ));
    }
}
