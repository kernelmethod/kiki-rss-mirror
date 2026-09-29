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
//!
//! [proxy]
//! url = "http://proxy.example:3128"
//! no_proxy = "localhost, .internal.example"
//! ```
//!
//! The proxy can also be set with environment variables, which take
//! precedence over the file; see [`ProxySettings`].
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
    #[serde(default)]
    pub proxy: ProxySettings,
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
    /// more than this many days; entries still in their feed, and entries
    /// tagged `system:saved`, are never deleted. `None` keeps entries forever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_days: Option<i64>,
}

/// The proxy used for outbound HTTP(S): feed fetches and asset downloads.
///
/// Each key can also be set from the environment, which takes precedence
/// over the config file key by key: [`PROXY_ENV`] (`KIKI_PROXY`) for
/// [`url`](Self::url) and [`NO_PROXY_ENV`] (`KIKI_NO_PROXY`) for
/// [`no_proxy`](Self::no_proxy). [`ProxySettings::with_env`] applies them.
///
/// When no proxy URL is set either way, the conventional `HTTPS_PROXY`,
/// `HTTP_PROXY`, `ALL_PROXY` and `NO_PROXY` variables are honored instead.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxySettings {
    /// URL of the proxy all outbound requests go through, `http://` or
    /// `https://`, optionally with `user:password@` credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// Hosts that bypass [`url`](Self::url): a comma-separated list of
    /// domains (matching their subdomains too), IP addresses, CIDR ranges,
    /// or `*` for every host. Has no effect without `url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_proxy: Option<String>,
}

/// Environment variable overriding [`ProxySettings::url`].
pub const PROXY_ENV: &str = "KIKI_PROXY";

/// Environment variable overriding [`ProxySettings::no_proxy`].
pub const NO_PROXY_ENV: &str = "KIKI_NO_PROXY";

impl ProxySettings {
    /// Returns these settings with any key set in the environment
    /// replaced by the environment's value. `var` looks a variable up;
    /// unset and empty variables are both ignored.
    ///
    /// # Examples
    ///
    /// ```
    /// use kiki_rss::config::ProxySettings;
    ///
    /// let file = ProxySettings {
    ///     url: Some("http://file-proxy:3128".into()),
    ///     no_proxy: Some("localhost".into()),
    /// };
    /// let env = |name: &str| (name == "KIKI_PROXY").then(|| "http://env-proxy:8080".to_string());
    /// let effective = file.with_env(env);
    /// assert_eq!(effective.url.as_deref(), Some("http://env-proxy:8080"));
    /// assert_eq!(effective.no_proxy.as_deref(), Some("localhost"));
    /// ```
    pub fn with_env(mut self, var: impl Fn(&str) -> Option<String>) -> Self {
        let var = |name| var(name).filter(|v| !v.trim().is_empty());
        if let Some(url) = var(PROXY_ENV) {
            self.url = Some(url);
        }
        if let Some(no_proxy) = var(NO_PROXY_ENV) {
            self.no_proxy = Some(no_proxy);
        }
        self
    }

    /// Checks that [`url`](Self::url), if set, is a usable proxy URL.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Invalid`] if the URL does not parse, has a
    /// scheme other than `http` or `https`, or has no host. The message
    /// never repeats the URL, which may hold credentials.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let Some(raw) = &self.url else {
            return Ok(());
        };
        let invalid = |why: String| Err(ConfigError::Invalid(format!("proxy.url {why}")));
        let url = match url::Url::parse(raw.trim()) {
            Ok(url) => url,
            Err(e) => return invalid(format!("is not a valid URL: {e}")),
        };
        if !matches!(url.scheme(), "http" | "https") {
            return invalid(format!(
                "must use the http or https scheme, not {:?}",
                url.scheme()
            ));
        }
        if url.host_str().is_none_or(str::is_empty) {
            return invalid("must name a host".to_string());
        }
        Ok(())
    }
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
            proxy: ProxySettings::default(),
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
        self.proxy.validate()
    }

    /// The proxy settings in effect: the config file's, with the
    /// environment's overrides applied (see [`ProxySettings::with_env`]).
    pub fn effective_proxy(&self) -> ProxySettings {
        self.proxy.clone().with_env(|name| std::env::var(name).ok())
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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
    fn proxy_settings_are_read_from_the_file() {
        let o = Overrides::parse(
            "[proxy]\nurl = \"http://user:pw@proxy.example:3128\"\nno_proxy = \"localhost\"\n",
        )
        .unwrap();
        let s = o.resolve().unwrap();
        assert_eq!(
            s.proxy.url.as_deref(),
            Some("http://user:pw@proxy.example:3128")
        );
        assert_eq!(s.proxy.no_proxy.as_deref(), Some("localhost"));
        assert_eq!(Settings::default().proxy, ProxySettings::default());
    }

    #[test]
    fn invalid_proxy_urls_are_rejected_without_echoing_them() {
        for url in [
            "not a url",
            "socks5://secret@proxy.example:1080",
            "ftp://proxy.example",
            "http://",
        ] {
            let mut o = Overrides::default();
            o.set("proxy", "url", url).unwrap();
            match o.resolve() {
                Err(ConfigError::Invalid(msg)) => {
                    assert!(msg.starts_with("proxy.url"), "{msg}");
                    assert!(!msg.contains("secret"), "{msg}");
                }
                other => panic!("{url:?} should be rejected, got {other:?}"),
            }
        }
        let o = Overrides::parse("[proxy]\nurl_typo = \"http://p\"\n").unwrap();
        assert!(matches!(o.resolve(), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn proxy_environment_overrides_the_file_key_by_key() {
        let file = ProxySettings {
            url: Some("http://file:1".into()),
            no_proxy: Some("file.example".into()),
        };
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                vars.iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            }
        };

        assert_eq!(file.clone().with_env(env(&[])), file);
        // Empty variables count as unset.
        assert_eq!(
            file.clone()
                .with_env(env(&[(PROXY_ENV, ""), (NO_PROXY_ENV, " ")])),
            file
        );

        let s = file.clone().with_env(env(&[(PROXY_ENV, "http://env:2")]));
        assert_eq!(s.url.as_deref(), Some("http://env:2"));
        assert_eq!(s.no_proxy.as_deref(), Some("file.example"));

        let s = ProxySettings::default().with_env(env(&[
            (PROXY_ENV, "http://env:2"),
            (NO_PROXY_ENV, "env.example"),
        ]));
        assert_eq!(s.url.as_deref(), Some("http://env:2"));
        assert_eq!(s.no_proxy.as_deref(), Some("env.example"));
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
