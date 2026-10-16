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
//! [proxy]
//! url = "http://proxy.example:3128"   # or "socks5h://127.0.0.1:9050" for Tor
//! no_proxy = "localhost, .internal.example"
//!
//! [http]
//! allowed_hosts = ["kiki.lan", "*.home.example"]
//!
//! [api]
//! anonymous_access = "read-only"
//!
//! [plugins]
//! allow_network = true
//! ```
//!
//! The proxy can also be set with environment variables, which take
//! precedence over the file; see [`ProxySettings`].
//!
//! The file is owned by Kiki: the settings API and `kiki config` rewrite it
//! through a [`ConfigStore`], so comments and formatting are not
//! preserved. The store re-reads the file before every change, holding a
//! lock that stores in other processes wait on, so no change is saved over
//! another. Edits made to the file from outside the server are picked up by
//! [`watch::spawn_watcher`].

pub mod hosts;
mod store;
pub mod watch;

pub use hosts::{HostPattern, HostPatternError};
pub(crate) use store::read_overrides;
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
    #[serde(default)]
    pub proxy: ProxySettings,
    #[serde(default)]
    pub http: HttpSettings,
    #[serde(default)]
    pub web_ui: WebUiSettings,
    #[serde(default)]
    pub api: ApiSettings,
    #[serde(default)]
    pub plugins: PluginSettings,
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
    /// URL of the proxy all outbound requests go through, optionally with
    /// `user:password@` credentials. The scheme picks the kind of proxy:
    ///
    /// * `http://` or `https://`: an HTTP proxy.
    /// * `socks5h://`: a SOCKS5 proxy that also looks up host names, so
    ///   no DNS query for a feed or asset leaves this machine. Use this one
    ///   for Tor (`socks5h://127.0.0.1:9050`).
    /// * `socks5://`: a SOCKS5 proxy, with host names looked up locally.
    ///   The lookups go out directly, so they reveal which hosts Kiki
    ///   fetches from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// Hosts that bypass [`url`](Self::url): a comma-separated list of
    /// domains (matching their subdomains too), IP addresses, CIDR ranges,
    /// or `*` for every host. Has no effect without `url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_proxy: Option<String>,
}

/// Settings shared by Kiki's HTTP listeners: the web UI that `kiki web`
/// serves, and the API, with any reverse proxy in front of it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpSettings {
    /// Hosts the web UI and the API answer to besides `localhost`,
    /// `127.0.0.1` and `::1`: exact host names or IP addresses,
    /// `*.example.com` for every subdomain of `example.com`, or `*` for any
    /// host at all. Requests naming any other host in their `Host` header
    /// are refused, so that a site cannot reach Kiki by pointing its own
    /// domain at this machine or at a reverse proxy in front of it (DNS
    /// rebinding).
    ///
    /// The two are usually reached at the same name, so they share this
    /// list. [`WebUiSettings::allowed_hosts`] and
    /// [`ApiSettings::allowed_hosts`] replace it for one of them. Empty, the
    /// default, the web UI answers only to the localhost names, and the API
    /// to any host, since it listens only on a Unix socket unless a proxy
    /// is put in front of it.
    ///
    /// # Examples
    ///
    /// ```
    /// use kiki_rss::config::{HostPattern, Overrides};
    ///
    /// let o = Overrides::parse("[http]\nallowed_hosts = [\"kiki.lan\", \"*\"]\n").unwrap();
    /// assert_eq!(
    ///     o.resolve().unwrap().http.allowed_hosts,
    ///     [HostPattern::Exact("kiki.lan".into()), HostPattern::Any]
    /// );
    /// ```
    #[serde(default)]
    pub allowed_hosts: Vec<HostPattern>,
}

/// Settings for the web UI that `kiki web` serves.
///
/// `kiki web` reads these once, as it starts, so a change takes effect
/// when it is restarted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebUiSettings {
    /// Hosts the web UI answers to besides `localhost`, `127.0.0.1` and
    /// `::1`, on top of any given to `kiki web --allowed-host`, in the
    /// forms [`HttpSettings::allowed_hosts`] takes. Unset, the default,
    /// the web UI answers to `http.allowed_hosts`; set, to these instead.
    /// See [`Settings::web_ui_allowed_hosts`].
    ///
    /// # Examples
    ///
    /// ```
    /// use kiki_rss::config::{HostPattern, Overrides};
    ///
    /// let o = Overrides::parse("[web_ui]\nallowed_hosts = [\"kiki.lan\", \"*\"]\n").unwrap();
    /// assert_eq!(
    ///     o.resolve().unwrap().web_ui.allowed_hosts,
    ///     Some(vec![HostPattern::Exact("kiki.lan".into()), HostPattern::Any])
    /// );
    /// ```
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_hosts: Option<Vec<HostPattern>>,

    /// Whether the web UI requires logging in with an API token, created
    /// with `kiki token create`, as `kiki web --require-login` does. Each
    /// person can then do only what their token's scopes allow.
    ///
    /// # Examples
    ///
    /// ```
    /// use kiki_rss::config::Overrides;
    ///
    /// let o = Overrides::parse("[web_ui]\nrequire_login = true\n").unwrap();
    /// assert!(o.resolve().unwrap().web_ui.require_login);
    /// ```
    #[serde(default)]
    pub require_login: bool,
}

/// Settings for the API.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiSettings {
    /// What a request that presents no API token may do. Requests that do
    /// present one are held to its scopes either way. A change takes
    /// effect with the next request.
    ///
    /// # Examples
    ///
    /// ```
    /// use kiki_rss::config::{AnonymousAccess, Overrides};
    ///
    /// let o = Overrides::parse("[api]\nanonymous_access = \"read-only\"\n").unwrap();
    /// assert_eq!(o.resolve().unwrap().api.anonymous_access, AnonymousAccess::ReadOnly);
    /// ```
    #[serde(default)]
    pub anonymous_access: AnonymousAccess,

    /// Hosts the API answers to besides `localhost`, `127.0.0.1` and
    /// `::1`, in the forms [`HttpSettings::allowed_hosts`] takes. Unset,
    /// the default, the API answers to `http.allowed_hosts`; set, to these
    /// instead. See [`Settings::api_allowed_hosts`]. With an empty list,
    /// here or inherited, the check is off and the API answers whatever
    /// host a request names. Otherwise requests naming any other host in
    /// their `Host` header are refused, so that a site cannot reach a
    /// reverse proxy in front of the API by pointing its own domain at the
    /// proxy (DNS rebinding). The proxy has to pass the client's `Host`
    /// through. A change takes effect with the next request.
    ///
    /// # Examples
    ///
    /// ```
    /// use kiki_rss::config::{HostPattern, Overrides};
    ///
    /// let o = Overrides::parse("[api]\nallowed_hosts = [\"kiki.example.com\"]\n").unwrap();
    /// assert_eq!(
    ///     o.resolve().unwrap().api.allowed_hosts,
    ///     Some(vec![HostPattern::Exact("kiki.example.com".into())])
    /// );
    /// ```
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_hosts: Option<Vec<HostPattern>>,
}

/// Settings for plugins.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginSettings {
    /// Whether plugins whose manifest asks for the
    /// [`network`](crate::plugins::Permission::Network) permission may fetch web
    /// pages. Off by default: a plugin fetching pages reaches servers no feed
    /// was added for, at URLs that may come from feeds. While it is off, their
    /// `fetch-page` calls fail. A change takes effect with the next call, with
    /// no restart.
    ///
    /// # Examples
    ///
    /// ```
    /// use kiki_rss::config::Overrides;
    ///
    /// let o = Overrides::parse("[plugins]\nallow_network = true\n").unwrap();
    /// assert!(o.resolve().unwrap().plugins.allow_network);
    /// assert!(!Overrides::parse("").unwrap().resolve().unwrap().plugins.allow_network);
    /// ```
    #[serde(default)]
    pub allow_network: bool,
}

/// What a request without an API token may do; see
/// [`ApiSettings::anonymous_access`].
///
/// Whatever the setting, a request without a token may still reach the
/// routes that need no scope, such as `/v1/health` and `/v1/access`, the
/// latter of which reports this setting.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum AnonymousAccess {
    /// Anything, administration included, as with a token holding every
    /// scope.
    #[default]
    Full,
    /// Only reading, as with a token holding just the `read` scope.
    ReadOnly,
    /// Nothing beyond the routes that need no scope: everything else
    /// requires a token.
    TokenRequired,
}

impl AnonymousAccess {
    /// The scopes a request without a token holds.
    ///
    /// # Examples
    ///
    /// ```
    /// use kiki_rss::auth::{Scope, Scopes};
    /// use kiki_rss::config::AnonymousAccess;
    ///
    /// assert_eq!(AnonymousAccess::Full.scopes(), Scopes::all());
    /// assert_eq!(AnonymousAccess::ReadOnly.scopes(), Scopes::of(Scope::Read));
    /// assert!(AnonymousAccess::TokenRequired.scopes().is_empty());
    /// ```
    pub fn scopes(self) -> crate::auth::Scopes {
        use crate::auth::{Scope, Scopes};
        match self {
            AnonymousAccess::Full => Scopes::all(),
            AnonymousAccess::ReadOnly => Scopes::of(Scope::Read),
            AnonymousAccess::TokenRequired => Scopes::NONE,
        }
    }
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
    /// scheme other than those listed on [`url`](Self::url), or has no
    /// host. The message
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
        if !matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h") {
            return invalid(format!(
                "must use the http, https, socks5 or socks5h scheme, not {:?}",
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

/// Default for [`FeedFetchSettings::default_fetch_interval_seconds`]: 24 hours.
pub const DEFAULT_FETCH_INTERVAL_SECONDS: u64 = 24 * 60 * 60;

/// Default for [`AssetCacheSettings::max_bytes`]: 1 GiB.
pub const DEFAULT_ASSET_CACHE_MAX_BYTES: i64 = 1024 * 1024 * 1024;

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
            proxy: ProxySettings::default(),
            http: HttpSettings::default(),
            web_ui: WebUiSettings::default(),
            api: ApiSettings::default(),
            plugins: PluginSettings::default(),
        }
    }
}

impl Settings {
    /// The hosts the web UI answers to besides the localhost names and
    /// those given to `kiki web --allowed-host`:
    /// [`WebUiSettings::allowed_hosts`] if it is set, and otherwise the
    /// shared [`HttpSettings::allowed_hosts`].
    ///
    /// # Examples
    ///
    /// ```
    /// use kiki_rss::config::{HostPattern, Overrides};
    ///
    /// let o = Overrides::parse("[http]\nallowed_hosts = [\"kiki.lan\"]\n").unwrap();
    /// assert_eq!(
    ///     o.resolve().unwrap().web_ui_allowed_hosts(),
    ///     [HostPattern::Exact("kiki.lan".into())]
    /// );
    /// ```
    pub fn web_ui_allowed_hosts(&self) -> &[HostPattern] {
        self.web_ui
            .allowed_hosts
            .as_deref()
            .unwrap_or(&self.http.allowed_hosts)
    }

    /// The hosts the API answers to besides the localhost names:
    /// [`ApiSettings::allowed_hosts`] if it is set, and otherwise the
    /// shared [`HttpSettings::allowed_hosts`]. Empty means the API answers
    /// any host.
    ///
    /// # Examples
    ///
    /// ```
    /// use kiki_rss::config::{HostPattern, Overrides};
    ///
    /// let lan = [HostPattern::Exact("kiki.lan".into())];
    /// let o = Overrides::parse("[http]\nallowed_hosts = [\"kiki.lan\"]\n").unwrap();
    /// assert_eq!(o.resolve().unwrap().api_allowed_hosts(), lan);
    ///
    /// let o = Overrides::parse(
    ///     "[http]\nallowed_hosts = [\"kiki.lan\"]\n[api]\nallowed_hosts = []\n",
    /// )
    /// .unwrap();
    /// assert!(o.resolve().unwrap().api_allowed_hosts().is_empty());
    /// ```
    pub fn api_allowed_hosts(&self) -> &[HostPattern] {
        self.api
            .allowed_hosts
            .as_deref()
            .unwrap_or(&self.http.allowed_hosts)
    }

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
        self.proxy.validate()
    }

    /// The proxy settings in effect: the config file's, with the
    /// environment's overrides applied (see [`ProxySettings::with_env`]).
    pub fn effective_proxy(&self) -> ProxySettings {
        self.proxy.clone().with_env(|name| std::env::var(name).ok())
    }

    /// Every setting, as `(section, key)`, sorted by section and then key.
    ///
    /// # Errors
    ///
    /// Returns an error if the settings cannot be represented in TOML,
    /// which they always can.
    ///
    /// # Examples
    ///
    /// ```
    /// use kiki_rss::config::Settings;
    ///
    /// let keys = Settings::keys().unwrap();
    /// assert!(keys.contains(&("asset_cache".into(), "enabled".into())));
    /// // Settings that are unset by default are listed too.
    /// assert!(keys.contains(&("proxy".into(), "url".into())));
    /// ```
    pub fn keys() -> Result<Vec<(String, String)>, ConfigError> {
        // Defaults with every optional setting given a value, since TOML
        // leaves unset ones out. Every struct is taken apart in full, so a
        // new setting fails to compile here until it is given one.
        let mut s = Settings::default();
        let Settings {
            feed_fetch:
                FeedFetchSettings {
                    timeout_seconds: _,
                    min_polling_cadence_seconds: _,
                    default_fetch_interval_seconds: _,
                    max_backoff_seconds: _,
                    force_refresh_after_seconds: _,
                    max_feed_bytes: _,
                },
            asset_cache:
                AssetCacheSettings {
                    enabled: _,
                    max_bytes: _,
                },
            proxy: ProxySettings { url, no_proxy },
            http: HttpSettings { allowed_hosts: _ },
            web_ui:
                WebUiSettings {
                    allowed_hosts: web_ui_hosts,
                    require_login: _,
                },
            api:
                ApiSettings {
                    anonymous_access: _,
                    allowed_hosts: api_hosts,
                },
            plugins: PluginSettings { allow_network: _ },
        } = &mut s;
        *url = Some(String::new());
        *no_proxy = Some(String::new());
        *web_ui_hosts = Some(Vec::new());
        *api_hosts = Some(Vec::new());

        let table = toml::Table::try_from(s)?;
        Ok(table
            .iter()
            .filter_map(|(section, keys)| Some((section, keys.as_table()?)))
            .flat_map(|(section, keys)| keys.keys().map(|key| (section.clone(), key.clone())))
            .collect())
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

    /// The overrides as a TOML table of sections.
    pub fn as_table(&self) -> &toml::Table {
        &self.0
    }

    /// Sets every key `other` sets, keeping the overrides it leaves out.
    /// Each key's value replaces the whole of what was there.
    ///
    /// # Examples
    ///
    /// ```
    /// use kiki_rss::config::Overrides;
    ///
    /// let mut o = Overrides::parse("[asset_cache]\nenabled = false\nmax_bytes = 10\n").unwrap();
    /// o.extend(&Overrides::parse("[asset_cache]\nmax_bytes = 20\n").unwrap());
    /// let s = o.resolve().unwrap();
    /// assert!(!s.asset_cache.enabled);
    /// assert_eq!(s.asset_cache.max_bytes, 20);
    /// ```
    pub fn extend(&mut self, other: &Overrides) {
        merge(&mut self.0, &other.0);
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
        merge(&mut merged, &self.without_retired_keys());
        let settings: Settings = toml::Value::Table(merged)
            .try_into()
            .map_err(|e: toml::de::Error| ConfigError::Invalid(e.message().to_string()))?;
        settings.validate()?;
        Ok(settings)
    }
}

/// Settings Kiki no longer has, as `(section, key)`, with what replaced
/// each. A config file that still sets one is accepted, and the key
/// ignored with a warning, rather than refused. A section left empty is
/// ignored too.
const RETIRED_KEYS: &[(&str, &str, &str)] = &[
    (
        "feed_fetch",
        "adaptive_fetch",
        "adaptive fetching is now the adaptive-fetch plugin; set its \
         `feeds` or `exclude`, or disable it in its manifest",
    ),
    (
        RETIRED_RETENTION.0,
        RETIRED_RETENTION.1,
        "deleting old entries is now the retention plugin; set its \
         `max_age_days`. The server moves this setting there when it starts",
    ),
];

/// The retired `retention.max_age_days`, as `(section, key)`, which the
/// server moves into the retention plugin's config when it starts; see
/// [`crate::plugins::migrate_retention_setting`].
pub const RETIRED_RETENTION: (&str, &str) = ("retention", "max_age_days");

impl Overrides {
    /// The overrides without any of the [`RETIRED_KEYS`], warning about
    /// each one that was set.
    ///
    /// # Examples
    ///
    /// ```
    /// use kiki_rss::config::{Overrides, Settings};
    ///
    /// let o = Overrides::parse("[feed_fetch]\nadaptive_fetch = false\n").unwrap();
    /// assert_eq!(o.resolve().unwrap(), Settings::default());
    /// let o = Overrides::parse("[retention]\nmax_age_days = 30\n").unwrap();
    /// assert_eq!(o.resolve().unwrap(), Settings::default());
    /// ```
    fn without_retired_keys(&self) -> toml::Table {
        let mut table = self.0.clone();
        for (section, key, replacement) in RETIRED_KEYS {
            let Some(t) = table.get_mut(*section).and_then(toml::Value::as_table_mut) else {
                continue;
            };
            if t.remove(*key).is_some() {
                tracing::warn!("ignoring the retired setting {section}.{key}: {replacement}");
            }
            if t.is_empty() {
                table.remove(*section);
            }
        }
        table
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
            "[feed_fetch]\nmax_feed_bytes = 4096\n\n[asset_cache]\nenabled = false\n",
        )
        .unwrap();
        let s = o.resolve().unwrap();
        assert_eq!(s.feed_fetch.max_feed_bytes, 4096);
        assert!(!s.asset_cache.enabled);
        // Keys not overridden keep their defaults.
        assert_eq!(
            s.feed_fetch.timeout_seconds,
            Settings::default().feed_fetch.timeout_seconds
        );
        assert_eq!(
            s.asset_cache.max_bytes,
            Settings::default().asset_cache.max_bytes
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let o = Overrides::parse("[feed_fetch]\nmax_feed_byte = 4096\n").unwrap();
        assert!(matches!(o.resolve(), Err(ConfigError::Invalid(_))));

        let o = Overrides::parse("[nonsense]\nx = 1\n").unwrap();
        assert!(matches!(o.resolve(), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn retired_keys_are_ignored_but_their_sections_other_keys_are_not() {
        for text in [
            "[retention]\nmax_age_days = 30\n",
            // Retired keys are ignored whatever their value.
            "[retention]\nmax_age_days = 0\n",
            "[feed_fetch]\nadaptive_fetch = false\n",
        ] {
            let o = Overrides::parse(text).unwrap();
            assert_eq!(o.resolve().unwrap(), Settings::default(), "{text:?}");
        }
        let o = Overrides::parse("[retention]\nmax_age_days = 30\nother = 1\n").unwrap();
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
    fn socks5_proxies_are_accepted() {
        for url in [
            "socks5://127.0.0.1:1080",
            "socks5h://127.0.0.1:9050",
            "socks5h://user:pw@proxy.example:1080",
        ] {
            let mut o = Overrides::default();
            o.set("proxy", "url", url).unwrap();
            let s = o.resolve().unwrap();
            assert_eq!(s.proxy.url.as_deref(), Some(url));
        }
    }

    #[test]
    fn invalid_proxy_urls_are_rejected_without_echoing_them() {
        for url in [
            "not a url",
            "socks4://secret@proxy.example:1080",
            "ftp://secret@proxy.example",
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
    fn allowed_hosts_are_read_from_the_file() {
        let defaults = Settings::default();
        assert!(defaults.http.allowed_hosts.is_empty());
        assert_eq!(defaults.web_ui.allowed_hosts, None);
        assert_eq!(defaults.api.allowed_hosts, None);

        let all = [
            HostPattern::Exact("kiki.lan".into()),
            HostPattern::Subdomains("example.com".into()),
            HostPattern::Exact("::1".into()),
            HostPattern::Any,
        ];
        for section in ["http", "web_ui", "api"] {
            let o = Overrides::parse(&format!(
                "[{section}]\nallowed_hosts = [\"Kiki.LAN\", \"*.example.com\", \"[::1]\", \"*\"]\n"
            ))
            .unwrap();
            let settings = o.resolve().unwrap();
            let shared = section == "http";
            assert_eq!(
                settings.web_ui_allowed_hosts() == all,
                shared || section == "web_ui",
                "{section}"
            );
            assert_eq!(
                settings.api_allowed_hosts() == all,
                shared || section == "api",
                "{section}"
            );

            // Each survives being written back out, an empty list too.
            for hosts in [vec!["kiki.lan", "*.example.com"], vec![]] {
                let mut o = Overrides::default();
                o.set(section, "allowed_hosts", hosts).unwrap();
                let reparsed = Overrides::parse(&o.to_toml_string().unwrap()).unwrap();
                assert_eq!(reparsed.resolve().unwrap(), o.resolve().unwrap());
            }

            for text in [
                "allowed_hosts = [\"kiki.lan:8080\"]\n",
                "allowed_hosts = [\"\"]\n",
                "allowed_hosts = \"kiki.lan\"\n",
                "allowed_host = [\"kiki.lan\"]\n",
            ] {
                let o = Overrides::parse(&format!("[{section}]\n{text}")).unwrap();
                assert!(
                    matches!(o.resolve(), Err(ConfigError::Invalid(_))),
                    "{section} {text:?} should be rejected"
                );
            }
        }
    }

    #[test]
    fn web_ui_and_api_hosts_replace_the_shared_ones() {
        let shared = "[http]\nallowed_hosts = [\"kiki.lan\"]\n";
        let lan = [HostPattern::Exact("kiki.lan".into())];

        let settings = Overrides::parse(shared).unwrap().resolve().unwrap();
        assert_eq!(settings.web_ui_allowed_hosts(), lan);
        assert_eq!(settings.api_allowed_hosts(), lan);

        // An override replaces the shared list rather than adding to it,
        // and an empty one turns the check off.
        let settings = Overrides::parse(&format!(
            "{shared}[web_ui]\nallowed_hosts = [\"*.lan\"]\n[api]\nallowed_hosts = []\n"
        ))
        .unwrap()
        .resolve()
        .unwrap();
        assert_eq!(
            settings.web_ui_allowed_hosts(),
            [HostPattern::Subdomains("lan".into())]
        );
        assert!(settings.api_allowed_hosts().is_empty());
    }

    #[test]
    fn anonymous_access_is_read_from_the_file() {
        assert_eq!(
            Settings::default().api.anonymous_access,
            AnonymousAccess::Full
        );
        for (value, expected) in [
            ("full", AnonymousAccess::Full),
            ("read-only", AnonymousAccess::ReadOnly),
            ("token-required", AnonymousAccess::TokenRequired),
        ] {
            let mut o = Overrides::default();
            o.set("api", "anonymous_access", value).unwrap();
            let reparsed = Overrides::parse(&o.to_toml_string().unwrap()).unwrap();
            assert_eq!(reparsed.resolve().unwrap().api.anonymous_access, expected);
        }
        for text in [
            "[api]\nanonymous_access = \"none\"\n",
            "[api]\nanonymous_access = true\n",
            "[api]\nanonymous = \"full\"\n",
        ] {
            let o = Overrides::parse(text).unwrap();
            assert!(
                matches!(o.resolve(), Err(ConfigError::Invalid(_))),
                "{text:?} should be rejected"
            );
        }
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
