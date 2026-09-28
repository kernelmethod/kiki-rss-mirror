//! Discovery of plugins installed in Kiki's home directory.
//!
//! A plugin is a directory inside [`PLUGINS_DIR_NAME`] (`plugins/` in Kiki's
//! home directory, next to the database) with a manifest,
//! [`MANIFEST_FILE_NAME`], at its root:
//!
//! ```text
//! plugins/
//! └── hide-sponsored/
//!     ├── manifest.toml
//!     ├── main.lua
//!     └── lib/
//!         └── rules.lua
//! ```
//!
//! The manifest names the plugin, gives its version, and declares the
//! scripting engine its code is written for. Its `[config]` table holds the
//! plugin's default config:
//!
//! ```toml
//! name = "hide-sponsored"
//! version = "1.0.0"
//! engine = "lua"
//! entrypoint = "main.lua"
//! description = "Hide sponsored posts"
//!
//! [config]
//! patterns = ["sponsored"]
//! ```
//!
//! See [`PluginManifest`] for every field. [`discover`] scans the plugins
//! directory and returns every plugin whose manifest is valid, together with
//! an error for each directory that could not be loaded; one broken plugin
//! never keeps the others from loading.
//!
//! The server discovers plugins once, when it starts: installing, removing
//! or editing a plugin takes effect after a restart. Plugins are loaded in
//! the order of their directory names, so prefixing directory names with a
//! number (`10-filter`, `20-tag`) controls the order their handlers run in.

use crate::scripting::{ScriptModule, ScriptSource};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;
use thiserror::Error;

/// Name of the directory, inside Kiki's home directory, that holds plugins.
pub const PLUGINS_DIR_NAME: &str = "plugins";

/// Name of the manifest file at the root of every plugin directory.
pub const MANIFEST_FILE_NAME: &str = "manifest.toml";

/// Name of the optional file, at the root of a plugin directory, that
/// overrides the default config given in the plugin's manifest.
pub const CONFIG_FILE_NAME: &str = "config.json";

/// Largest manifest or config file that will be read.
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024;

/// Largest total size of the source files of one plugin.
///
/// Every plugin's source is shipped to the script host in one message, so
/// this also keeps the message well inside the host's frame limit.
pub const MAX_PLUGIN_SOURCE_BYTES: u64 = 1024 * 1024;

/// How deep inside a plugin directory source files are looked for.
const MAX_SOURCE_DEPTH: usize = 8;

/// Longest allowed plugin name.
const MAX_NAME_LEN: usize = 64;

static NAME_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used)]
    regex::Regex::new(r"^[a-z0-9][a-z0-9_-]*$").expect("plugin name regex is valid")
});

static VERSION_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used)]
    regex::Regex::new(
        r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$",
    )
    .expect("plugin version regex is valid")
});

/// Returns the plugins directory inside Kiki's home directory `home`.
///
/// # Examples
///
/// ```
/// use kiki_rss::plugins::plugins_dir;
/// use std::path::Path;
///
/// assert_eq!(plugins_dir(Path::new("/var/lib/kiki")), Path::new("/var/lib/kiki/plugins"));
/// ```
pub fn plugins_dir(home: &Path) -> PathBuf {
    home.join(PLUGINS_DIR_NAME)
}

/// The scripting engine a plugin's code is written for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PluginEngine {
    /// Lua 5.4. See the scripting guide for the API available to Lua code.
    Lua,
}

impl PluginEngine {
    /// The engine's name, as written in a manifest.
    pub fn name(self) -> &'static str {
        match self {
            Self::Lua => "lua",
        }
    }

    /// The file extension of the engine's source files, without the dot.
    pub fn source_extension(self) -> &'static str {
        match self {
            Self::Lua => "lua",
        }
    }

    /// The entrypoint a plugin has when its manifest does not name one.
    pub fn default_entrypoint(self) -> &'static str {
        match self {
            Self::Lua => "main.lua",
        }
    }

    /// Whether this build of Kiki can run plugins written for the engine.
    pub fn is_supported(self) -> bool {
        match self {
            Self::Lua => cfg!(feature = "lua"),
        }
    }
}

/// The contents of a plugin's manifest, [`MANIFEST_FILE_NAME`].
///
/// Fields Kiki does not know are ignored, so that plugins may carry extra
/// metadata of their own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PluginManifest {
    /// The plugin's name: lowercase letters, digits, `-` and `_`, starting
    /// with a letter or digit, and at most 64 characters long. Must be
    /// unique among installed plugins.
    pub name: String,

    /// The plugin's version, in `MAJOR.MINOR.PATCH` form with an optional
    /// pre-release and build suffix, as in [Semantic Versioning].
    ///
    /// [Semantic Versioning]: https://semver.org
    pub version: String,

    /// The scripting engine the plugin's code is written for.
    pub engine: PluginEngine,

    /// Path of the file whose code runs when the plugin is loaded, relative
    /// to the plugin directory. Defaults to `main.lua` for Lua plugins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<String>,

    /// A one-line description of what the plugin does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// The plugin's authors.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,

    /// The plugin's license, preferably as an SPDX expression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,

    /// Where to find out more about the plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,

    /// Whether the plugin is loaded. Set to `false` to keep a plugin
    /// installed without running it.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// The plugin's default config, the manifest's `[config]` table, handed
    /// to its entrypoint as its argument. Keys in the plugin's
    /// [`CONFIG_FILE_NAME`], if it has one, override these.
    ///
    /// The table is held as JSON, the form configs take everywhere else; see
    /// [`toml_to_json`] for how TOML values are converted.
    #[serde(default, deserialize_with = "deserialize_config")]
    #[schema(value_type = Object)]
    pub config: serde_json::Map<String, serde_json::Value>,
}

fn default_enabled() -> bool {
    true
}

/// Reads a manifest's `[config]` table, converting it to JSON.
fn deserialize_config<'de, D>(
    deserializer: D,
) -> Result<serde_json::Map<String, serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let table = toml::Table::deserialize(deserializer)?;
    match toml_to_json(&toml::Value::Table(table)) {
        Ok(serde_json::Value::Object(map)) => Ok(map),
        Ok(_) => Err(serde::de::Error::custom("config must be a table")),
        Err(e) => Err(serde::de::Error::custom(e)),
    }
}

/// Error returned when a TOML value has no JSON equivalent.
#[derive(Debug, Error)]
#[error("config value {0} is not a finite number")]
pub struct NonFiniteFloat(pub f64);

/// Converts a TOML value into the equivalent JSON value.
///
/// Tables become objects, arrays become arrays, and strings, integers,
/// floats and booleans map across unchanged. Dates and times, which JSON
/// lacks, become strings in their TOML (RFC 3339) form.
///
/// # Errors
///
/// Returns [`NonFiniteFloat`] for `inf` and `nan`, which JSON cannot
/// represent.
///
/// # Examples
///
/// ```
/// use kiki_rss::plugins::toml_to_json;
///
/// let value: toml::Value = toml::from_str("n = 1\nat = 2024-01-02").unwrap();
/// assert_eq!(
///     toml_to_json(&value).unwrap(),
///     serde_json::json!({"n": 1, "at": "2024-01-02"}),
/// );
/// assert!(toml_to_json(&toml::Value::Float(f64::NAN)).is_err());
/// ```
pub fn toml_to_json(value: &toml::Value) -> Result<serde_json::Value, NonFiniteFloat> {
    use serde_json::Value as Json;
    Ok(match value {
        toml::Value::String(s) => Json::String(s.clone()),
        toml::Value::Integer(i) => Json::from(*i),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Json::Number)
            .ok_or(NonFiniteFloat(*f))?,
        toml::Value::Boolean(b) => Json::Bool(*b),
        toml::Value::Datetime(d) => Json::String(d.to_string()),
        toml::Value::Array(items) => {
            Json::Array(items.iter().map(toml_to_json).collect::<Result<_, _>>()?)
        }
        toml::Value::Table(table) => Json::Object(
            table
                .iter()
                .map(|(k, v)| Ok((k.clone(), toml_to_json(v)?)))
                .collect::<Result<_, _>>()?,
        ),
    })
}

impl PluginManifest {
    /// The plugin's entrypoint: the one its manifest names, or the engine's
    /// default.
    pub fn entrypoint(&self) -> &str {
        self.entrypoint
            .as_deref()
            .unwrap_or_else(|| self.engine.default_entrypoint())
    }

    /// Parses and validates a manifest.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::InvalidManifest`] if `text` is not a manifest,
    /// and [`PluginError::InvalidName`], [`PluginError::InvalidVersion`] or
    /// [`PluginError::InvalidEntrypoint`] if one of its fields is invalid.
    ///
    /// # Examples
    ///
    /// ```
    /// use kiki_rss::plugins::{PluginEngine, PluginManifest};
    ///
    /// let manifest = PluginManifest::parse(r#"
    ///     name = "hello"
    ///     version = "0.1.0"
    ///     engine = "lua"
    ///
    ///     [config]
    ///     greeting = "hi"
    /// "#).unwrap();
    /// assert_eq!(manifest.engine, PluginEngine::Lua);
    /// assert_eq!(manifest.entrypoint(), "main.lua");
    /// assert!(manifest.enabled);
    /// assert_eq!(manifest.config["greeting"], "hi");
    ///
    /// assert!(PluginManifest::parse(r#"name = "Hello!""#).is_err());
    /// ```
    pub fn parse(text: &str) -> Result<Self, PluginError> {
        let manifest: Self = toml::from_str(text).map_err(PluginError::InvalidManifest)?;
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<(), PluginError> {
        if self.name.len() > MAX_NAME_LEN || !NAME_RE.is_match(&self.name) {
            return Err(PluginError::InvalidName(self.name.clone()));
        }
        if !VERSION_RE.is_match(&self.version) {
            return Err(PluginError::InvalidVersion(self.version.clone()));
        }
        let entrypoint = self.entrypoint();
        let extension = format!(".{}", self.engine.source_extension());
        if !is_plain_relative_path(Path::new(entrypoint)) || !entrypoint.ends_with(&extension) {
            return Err(PluginError::InvalidEntrypoint(entrypoint.to_string()));
        }
        Ok(())
    }
}

/// Whether `path` is relative and made only of normal components, so that
/// it cannot reach outside the directory it is resolved against.
fn is_plain_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}

/// Errors that keep a plugin from being discovered or loaded.
#[derive(Debug, Error)]
pub enum PluginError {
    /// A file could not be read.
    #[error("unable to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The plugin directory has no manifest.
    #[error("no {MANIFEST_FILE_NAME} found")]
    MissingManifest,

    /// The manifest is not valid TOML, or is missing a required field.
    #[error("invalid {MANIFEST_FILE_NAME}: {0}")]
    InvalidManifest(#[source] toml::de::Error),

    /// The plugin's name is not allowed.
    #[error(
        "invalid plugin name {0:?}: names must be at most {MAX_NAME_LEN} characters of \
         lowercase letters, digits, '-' and '_', starting with a letter or digit"
    )]
    InvalidName(String),

    /// The plugin's version is not in `MAJOR.MINOR.PATCH` form.
    #[error("invalid plugin version {0:?}: versions must be of the form MAJOR.MINOR.PATCH")]
    InvalidVersion(String),

    /// The entrypoint is not a relative path to a source file inside the
    /// plugin directory.
    #[error(
        "invalid entrypoint {0:?}: it must be a relative path to a source file inside \
         the plugin directory"
    )]
    InvalidEntrypoint(String),

    /// The entrypoint named by the manifest does not exist.
    #[error("entrypoint {0:?} not found")]
    MissingEntrypoint(String),

    /// The plugin's [`CONFIG_FILE_NAME`] is not a JSON object.
    #[error("invalid {CONFIG_FILE_NAME}: {0}")]
    InvalidConfig(#[source] crate::scripting::ScriptConfigError),

    /// A file is larger than Kiki will read.
    #[error("{path} is too large: the limit is {limit} bytes")]
    TooLarge { path: PathBuf, limit: u64 },

    /// A new config is too large to be saved to [`CONFIG_FILE_NAME`].
    #[error("config is too large: saved as JSON, it must be at most {limit} bytes")]
    ConfigTooLarge { limit: u64 },

    /// A source file is not valid UTF-8.
    #[error("{0} is not valid UTF-8")]
    NotUtf8(PathBuf),

    /// Another plugin with the same name was discovered first.
    #[error("another plugin named {name:?} is installed at {other}")]
    DuplicateName { name: String, other: PathBuf },
}

/// A plugin discovered in the plugins directory.
#[derive(Debug, Clone)]
pub struct Plugin {
    /// The plugin's directory.
    pub dir: PathBuf,
    /// The plugin's manifest.
    pub manifest: PluginManifest,
    /// The plugin's config: the manifest's default config, with the keys of
    /// its [`CONFIG_FILE_NAME`] applied over it.
    pub config: serde_json::Map<String, serde_json::Value>,
}

impl Plugin {
    /// Loads the plugin in directory `dir`, reading and validating its
    /// manifest and config.
    ///
    /// Source files are not read until [`Self::load_source`].
    ///
    /// # Errors
    ///
    /// Returns an error if the manifest or config is missing, unreadable or
    /// invalid, or if the manifest's entrypoint does not exist.
    pub fn load(dir: &Path) -> Result<Self, PluginError> {
        let manifest_path = dir.join(MANIFEST_FILE_NAME);
        let manifest = match read_small_file(&manifest_path)? {
            Some(text) => PluginManifest::parse(&text)?,
            None => return Err(PluginError::MissingManifest),
        };

        let config = apply_config_overrides(&manifest.config, read_config_file(dir)?);

        if !dir.join(manifest.entrypoint()).is_file() {
            return Err(PluginError::MissingEntrypoint(
                manifest.entrypoint().to_string(),
            ));
        }

        Ok(Self {
            dir: dir.to_path_buf(),
            manifest,
            config,
        })
    }

    /// Path of the plugin's [`CONFIG_FILE_NAME`], whether or not it exists.
    pub fn config_path(&self) -> PathBuf {
        self.dir.join(CONFIG_FILE_NAME)
    }

    /// Reads the config overrides currently in the plugin's
    /// [`CONFIG_FILE_NAME`]. A missing file holds no overrides.
    ///
    /// Unlike [`Self::config`], which is fixed when the plugin is loaded,
    /// this reflects the file as it is now.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read, is too large, or is not
    /// a JSON object.
    pub fn read_config_overrides(
        &self,
    ) -> Result<serde_json::Map<String, serde_json::Value>, PluginError> {
        read_config_file(&self.dir)
    }

    /// Applies `edit` to the config overrides in the plugin's
    /// [`CONFIG_FILE_NAME`] and saves the result. Returns the new overrides.
    ///
    /// The file is re-read first, so changes made to it by hand are kept.
    /// Updates are serialized, so concurrent edits do not lose each other's
    /// changes. See [`Self::replace_config_overrides`] for how the file is
    /// saved.
    ///
    /// # Errors
    ///
    /// Returns an error, and leaves the file untouched, if the file on disk
    /// cannot be read or is not a JSON object, if `edit` fails, or if the
    /// result cannot be saved.
    pub fn update_config_overrides<F>(
        &self,
        edit: F,
    ) -> Result<serde_json::Map<String, serde_json::Value>, PluginError>
    where
        F: FnOnce(&mut serde_json::Map<String, serde_json::Value>) -> Result<(), PluginError>,
    {
        let _guard = lock_config_writes();
        let mut overrides = self.read_config_overrides()?;
        edit(&mut overrides)?;
        write_config_file(&self.config_path(), &overrides)?;
        Ok(overrides)
    }

    /// Replaces the plugin's [`CONFIG_FILE_NAME`] with `overrides`, whatever
    /// the file held before (even if it was invalid).
    ///
    /// The file is replaced atomically. Empty overrides remove the file, so
    /// the plugin is left with the defaults from its manifest.
    ///
    /// The new config takes effect the next time the server starts.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::ConfigTooLarge`] if the overrides would be
    /// larger than [`MAX_MANIFEST_BYTES`] once saved, since such a file
    /// would keep the plugin from loading, and [`PluginError::Io`] if the
    /// file cannot be written.
    ///
    /// # Examples
    ///
    /// ```
    /// use kiki_rss::plugins::{discover, Plugin};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let plugin = dir.path().join("hello");
    /// std::fs::create_dir(&plugin).unwrap();
    /// std::fs::write(
    ///     plugin.join("manifest.toml"),
    ///     "name = 'hello'\nversion = '1.0.0'\nengine = 'lua'\n[config]\ngreeting = 'hi'\n",
    /// ).unwrap();
    /// std::fs::write(plugin.join("main.lua"), "").unwrap();
    ///
    /// let plugin = Plugin::load(&plugin).unwrap();
    /// let overrides = serde_json::json!({"greeting": "hello"});
    /// plugin.replace_config_overrides(overrides.as_object().unwrap()).unwrap();
    ///
    /// // The change is picked up the next time the plugin is loaded.
    /// assert_eq!(plugin.config["greeting"], "hi");
    /// assert_eq!(Plugin::load(&plugin.dir).unwrap().config["greeting"], "hello");
    /// ```
    pub fn replace_config_overrides(
        &self,
        overrides: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), PluginError> {
        let _guard = lock_config_writes();
        write_config_file(&self.config_path(), overrides)
    }

    /// The name of the plugin's directory.
    pub fn dir_name(&self) -> String {
        self.dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Reads the plugin's source files, ready to hand to a script runner.
    ///
    /// Every file in the plugin directory (and its subdirectories) with the
    /// engine's source extension is read, and becomes a module named after
    /// its path, as described in [`module_name`]. Symbolic links inside the
    /// plugin directory are skipped.
    ///
    /// # Errors
    ///
    /// Returns an error if a source file cannot be read or is not UTF-8, or
    /// if the sources are larger than [`MAX_PLUGIN_SOURCE_BYTES`] in total.
    pub fn load_source(&self) -> Result<ScriptSource, PluginError> {
        let extension = self.manifest.engine.source_extension();
        let mut files = Vec::new();
        collect_source_files(&self.dir, &self.dir, extension, 0, &mut files)?;
        files.sort();

        let entrypoint = Path::new(self.manifest.entrypoint());
        let mut total: u64 = 0;
        let mut text = None;
        let mut modules = Vec::with_capacity(files.len());
        for relative in files {
            let path = self.dir.join(&relative);
            let source = read_source_file(&path, &mut total)?;
            if relative == entrypoint {
                text = Some(source);
            } else if let Some(name) = module_name(&relative, extension) {
                modules.push(ScriptModule { name, text: source });
            }
        }

        // The entrypoint can be a symbolic link, which the walk skips.
        let text = match text {
            Some(text) => text,
            None => read_source_file(&self.dir.join(entrypoint), &mut total)?,
        };

        Ok(ScriptSource {
            name: self.manifest.name.clone(),
            text,
            config: serde_json::Value::Object(self.config.clone()).to_string(),
            modules,
        })
    }
}

/// Returns `defaults` with the keys of `overrides` applied over it: the
/// config a plugin with those defaults and overrides is loaded with.
///
/// Only top-level keys are replaced; objects are not merged. A key
/// overridden with `null` is kept, as `null`.
///
/// # Examples
///
/// ```
/// use kiki_rss::plugins::apply_config_overrides;
/// use serde_json::json;
///
/// let defaults = json!({"a": 1, "b": {"x": 1}});
/// let overrides = json!({"b": {"y": 2}, "c": 3});
/// assert_eq!(
///     apply_config_overrides(defaults.as_object().unwrap(), overrides.as_object().unwrap().clone()),
///     *json!({"a": 1, "b": {"y": 2}, "c": 3}).as_object().unwrap(),
/// );
/// ```
pub fn apply_config_overrides(
    defaults: &serde_json::Map<String, serde_json::Value>,
    overrides: serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut config = defaults.clone();
    config.extend(overrides);
    config
}

/// Serializes writes to plugins' config files.
fn lock_config_writes() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // The guarded data is `()`, so a panic while holding the lock cannot
    // have left anything half-updated.
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Reads the config overrides in plugin directory `dir`; a missing
/// [`CONFIG_FILE_NAME`] holds none.
fn read_config_file(dir: &Path) -> Result<serde_json::Map<String, serde_json::Value>, PluginError> {
    match read_small_file(&dir.join(CONFIG_FILE_NAME))? {
        Some(text) => {
            crate::scripting::parse_script_config(&text).map_err(PluginError::InvalidConfig)
        }
        None => Ok(serde_json::Map::new()),
    }
}

/// Saves `overrides` to the config file at `path`, or removes the file if
/// there are none.
fn write_config_file(
    path: &Path,
    overrides: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), PluginError> {
    let io_err = |source| PluginError::Io {
        path: path.to_path_buf(),
        source,
    };
    if overrides.is_empty() {
        return match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err(e)),
        };
    }

    // Serializing a map of JSON values cannot fail.
    let mut text = serde_json::to_string_pretty(overrides).unwrap_or_default();
    text.push('\n');
    if text.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(PluginError::ConfigTooLarge {
            limit: MAX_MANIFEST_BYTES,
        });
    }
    crate::config::write_atomically(path, &text).map_err(io_err)
}

/// The name that the source file at `relative` (a path inside a plugin
/// directory) is loaded under, if it has one.
///
/// Directory separators become dots and the extension is dropped, so
/// `lib/rules.lua` is `lib.rules`. A file named `init` stands for its
/// directory: `lib/init.lua` is `lib`. Files whose path components are not
/// valid module name parts (letters, digits and `_`, or `-`) have no name.
///
/// # Examples
///
/// ```
/// use kiki_rss::plugins::module_name;
/// use std::path::Path;
///
/// assert_eq!(module_name(Path::new("lib/rules.lua"), "lua").as_deref(), Some("lib.rules"));
/// assert_eq!(module_name(Path::new("lib/init.lua"), "lua").as_deref(), Some("lib"));
/// assert_eq!(module_name(Path::new("a.b.lua"), "lua"), None);
/// ```
pub fn module_name(relative: &Path, extension: &str) -> Option<String> {
    if relative.extension()? != extension {
        return None;
    }
    let stem = relative.with_extension("");
    let mut parts = Vec::new();
    for component in stem.components() {
        let Component::Normal(part) = component else {
            return None;
        };
        let part = part.to_str()?;
        let valid = !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !valid {
            return None;
        }
        parts.push(part);
    }
    if parts.len() > 1 && parts.last() == Some(&"init") {
        parts.pop();
    }
    Some(parts.join("."))
}

/// Collects the paths, relative to `root`, of the files under `dir` with the
/// given extension, skipping hidden entries and symbolic links.
fn collect_source_files(
    root: &Path,
    dir: &Path,
    extension: &str,
    depth: usize,
    out: &mut Vec<PathBuf>,
) -> Result<(), PluginError> {
    if depth > MAX_SOURCE_DEPTH {
        return Ok(());
    }
    let entries = std::fs::read_dir(dir).map_err(|source| PluginError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| PluginError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| PluginError::Io {
            path: path.clone(),
            source,
        })?;
        if file_type.is_dir() {
            collect_source_files(root, &path, extension, depth + 1, out)?;
        } else if file_type.is_file() && path.extension().is_some_and(|e| e == extension) {
            if let Ok(relative) = path.strip_prefix(root) {
                out.push(relative.to_path_buf());
            }
        }
    }
    Ok(())
}

/// Reads the source file at `path`, adding its size to `total` and failing
/// if that takes it over [`MAX_PLUGIN_SOURCE_BYTES`].
fn read_source_file(path: &Path, total: &mut u64) -> Result<String, PluginError> {
    let io_err = |source| PluginError::Io {
        path: path.to_path_buf(),
        source,
    };
    let len = std::fs::metadata(path).map_err(io_err)?.len();
    *total = total.saturating_add(len);
    if *total > MAX_PLUGIN_SOURCE_BYTES {
        return Err(PluginError::TooLarge {
            path: path.to_path_buf(),
            limit: MAX_PLUGIN_SOURCE_BYTES,
        });
    }
    let bytes = std::fs::read(path).map_err(io_err)?;
    String::from_utf8(bytes).map_err(|_| PluginError::NotUtf8(path.to_path_buf()))
}

/// Reads a manifest or config file, returning `None` if it does not exist.
fn read_small_file(path: &Path) -> Result<Option<String>, PluginError> {
    let io_err = |source| PluginError::Io {
        path: path.to_path_buf(),
        source,
    };
    let len = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_err(e)),
    };
    if len > MAX_MANIFEST_BYTES {
        return Err(PluginError::TooLarge {
            path: path.to_path_buf(),
            limit: MAX_MANIFEST_BYTES,
        });
    }
    let bytes = std::fs::read(path).map_err(io_err)?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| PluginError::NotUtf8(path.to_path_buf()))
}

/// A plugin directory that could not be loaded.
#[derive(Debug)]
pub struct DiscoveryError {
    /// The plugin directory.
    pub dir: PathBuf,
    /// Why it could not be loaded.
    pub error: PluginError,
}

/// The result of scanning the plugins directory.
#[derive(Debug, Default)]
pub struct Discovery {
    /// The plugins that were found, in load order.
    pub plugins: Vec<Plugin>,
    /// The directories that could not be loaded as plugins.
    pub errors: Vec<DiscoveryError>,
}

/// Scans `plugins_dir` for plugins.
///
/// Every directory (or symbolic link to a directory) directly inside
/// `plugins_dir` whose name does not start with `.` is a plugin; other files
/// are ignored. Plugins are returned in the order of their directory names.
/// A directory that cannot be loaded, or whose plugin has the same name as
/// one found before it, is reported in [`Discovery::errors`] and skipped.
///
/// A missing `plugins_dir` holds no plugins.
///
/// # Errors
///
/// Returns an error only if `plugins_dir` exists but cannot be listed.
///
/// # Examples
///
/// ```
/// use kiki_rss::plugins::discover;
///
/// let dir = tempfile::tempdir().unwrap();
/// let plugin = dir.path().join("hello");
/// std::fs::create_dir(&plugin).unwrap();
/// std::fs::write(
///     plugin.join("manifest.toml"),
///     "name = 'hello'\nversion = '1.0.0'\nengine = 'lua'\n",
/// ).unwrap();
/// std::fs::write(plugin.join("main.lua"), "kiki.log('info', 'hello')").unwrap();
///
/// let found = discover(dir.path()).unwrap();
/// assert_eq!(found.plugins.len(), 1);
/// assert_eq!(found.plugins[0].manifest.name, "hello");
/// assert!(found.errors.is_empty());
/// ```
pub fn discover(plugins_dir: &Path) -> Result<Discovery, PluginError> {
    let io_err = |source| PluginError::Io {
        path: plugins_dir.to_path_buf(),
        source,
    };
    let entries = match std::fs::read_dir(plugins_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Discovery::default()),
        Err(e) => return Err(io_err(e)),
    };

    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_err)?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            dirs.push(path);
        }
    }
    dirs.sort();

    let mut discovery = Discovery::default();
    let mut names = HashSet::new();
    for dir in dirs {
        match Plugin::load(&dir) {
            Ok(plugin) if !names.insert(plugin.manifest.name.clone()) => {
                let other = discovery
                    .plugins
                    .iter()
                    .find(|p| p.manifest.name == plugin.manifest.name)
                    .map(|p| p.dir.clone())
                    .unwrap_or_default();
                discovery.errors.push(DiscoveryError {
                    dir,
                    error: PluginError::DuplicateName {
                        name: plugin.manifest.name,
                        other,
                    },
                });
            }
            Ok(plugin) => discovery.plugins.push(plugin),
            Err(error) => discovery.errors.push(DiscoveryError { dir, error }),
        }
    }
    Ok(discovery)
}

/// Reads the source of every enabled plugin in `discovery` whose engine is
/// `engine`, logging each plugin that cannot be loaded.
pub fn load_sources(discovery: &Discovery, engine: PluginEngine) -> Vec<ScriptSource> {
    let mut sources = Vec::new();
    for plugin in &discovery.plugins {
        let name = &plugin.manifest.name;
        if !plugin.manifest.enabled {
            tracing::debug!(plugin = %name, "skipping disabled plugin");
            continue;
        }
        if plugin.manifest.engine != engine {
            continue;
        }
        match plugin.load_source() {
            Ok(source) => {
                tracing::debug!(
                    plugin = %name,
                    version = %plugin.manifest.version,
                    "loaded plugin"
                );
                sources.push(source);
            }
            Err(e) => {
                tracing::warn!(dir = %plugin.dir.display(), "skipping plugin {name}: {e}");
            }
        }
    }
    sources
}

/// Installs a plugin into `plugins_dir`, in a directory named after it:
/// writes its manifest and its entrypoint, containing `text`.
///
/// The manifest's config must be representable in TOML, so it may not
/// contain `null`.
///
/// This is a convenience for tests; plugins are
/// normally installed by copying their directory into the plugins directory.
///
/// # Errors
///
/// Returns an error if the plugin directory already exists, or if a file
/// cannot be written.
pub fn install(
    plugins_dir: &Path,
    manifest: &PluginManifest,
    text: &str,
) -> anyhow::Result<PathBuf> {
    use anyhow::Context;

    manifest.validate()?;
    let dir = plugins_dir.join(&manifest.name);
    std::fs::create_dir_all(plugins_dir)
        .with_context(|| format!("unable to create {}", plugins_dir.display()))?;
    std::fs::create_dir(&dir).with_context(|| format!("unable to create {}", dir.display()))?;

    let manifest_toml = toml::to_string_pretty(manifest)?;
    std::fs::write(dir.join(MANIFEST_FILE_NAME), manifest_toml)
        .with_context(|| format!("unable to write the manifest in {}", dir.display()))?;
    std::fs::write(dir.join(manifest.entrypoint()), text)
        .with_context(|| format!("unable to write the entrypoint in {}", dir.display()))?;
    Ok(dir)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn manifest(name: &str) -> PluginManifest {
        PluginManifest {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            engine: PluginEngine::Lua,
            entrypoint: None,
            description: None,
            authors: vec![],
            license: None,
            homepage: None,
            enabled: true,
            config: Default::default(),
        }
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn manifest_requires_name_version_and_engine() {
        for text in [
            "version = '1.0.0'\nengine = 'lua'",
            "name = 'a'\nengine = 'lua'",
            "name = 'a'\nversion = '1.0.0'",
            "name = 'a'\nversion = '1.0.0'\nengine = 'python'",
            "name = 'a'\nversion = '1.0.0'\nengine = 'lua'\nconfig = 1",
            "not toml",
        ] {
            assert!(
                matches!(
                    PluginManifest::parse(text),
                    Err(PluginError::InvalidManifest(_))
                ),
                "{text}"
            );
        }
    }

    #[test]
    fn manifest_validates_names() {
        for name in ["hello", "hello-world", "hello_world", "2fa", "a"] {
            let text = format!("name = '{name}'\nversion = '1.0.0'\nengine = 'lua'");
            assert!(PluginManifest::parse(&text).is_ok(), "{name}");
        }
        let long = "a".repeat(MAX_NAME_LEN + 1);
        for name in ["", "Hello", "-a", "_a", "a b", "a/b", "..", long.as_str()] {
            let text = format!("name = '{name}'\nversion = '1.0.0'\nengine = 'lua'");
            assert!(
                matches!(
                    PluginManifest::parse(&text),
                    Err(PluginError::InvalidName(_))
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn manifest_validates_versions() {
        for version in [
            "0.0.0",
            "1.2.3",
            "10.20.30",
            "1.0.0-alpha.1",
            "1.0.0+build.5",
        ] {
            let text = format!("name = 'a'\nversion = '{version}'\nengine = 'lua'");
            assert!(PluginManifest::parse(&text).is_ok(), "{version}");
        }
        for version in ["", "1", "1.0", "v1.0.0", "01.0.0", "1.0.0.0", "1.0.0-"] {
            let text = format!("name = 'a'\nversion = '{version}'\nengine = 'lua'");
            assert!(
                matches!(
                    PluginManifest::parse(&text),
                    Err(PluginError::InvalidVersion(_))
                ),
                "{version}"
            );
        }
    }

    #[test]
    fn manifest_entrypoint_must_stay_inside_the_plugin() {
        for entrypoint in [
            "../main.lua",
            "/etc/main.lua",
            "lib/../main.lua",
            "main.py",
            "",
        ] {
            let text = format!(
                "name = 'a'\nversion = '1.0.0'\nengine = 'lua'\nentrypoint = '{entrypoint}'"
            );
            assert!(
                matches!(
                    PluginManifest::parse(&text),
                    Err(PluginError::InvalidEntrypoint(_))
                ),
                "{entrypoint}"
            );
        }
        let text = "name = 'a'\nversion = '1.0.0'\nengine = 'lua'\nentrypoint = 'src/init.lua'";
        assert_eq!(
            PluginManifest::parse(text).unwrap().entrypoint(),
            "src/init.lua"
        );
    }

    #[test]
    fn manifest_config_is_converted_to_json() {
        let text = r#"
            name = "a"
            version = "1.0.0"
            engine = "lua"

            [config]
            count = 3
            ratio = 0.5
            on = true
            since = 2024-01-02T03:04:05Z
            patterns = ["x", "y"]

            [[config.rules]]
            field = "title"
            pattern = '\bsponsored\b'
        "#;
        let manifest = PluginManifest::parse(text).unwrap();
        assert_eq!(
            serde_json::Value::Object(manifest.config),
            serde_json::json!({
                "count": 3,
                "ratio": 0.5,
                "on": true,
                "since": "2024-01-02T03:04:05Z",
                "patterns": ["x", "y"],
                "rules": [{"field": "title", "pattern": "\\bsponsored\\b"}],
            })
        );

        let text = "name = 'a'\nversion = '1.0.0'\nengine = 'lua'\n[config]\nx = nan";
        assert!(matches!(
            PluginManifest::parse(text),
            Err(PluginError::InvalidManifest(_))
        ));
    }

    #[test]
    fn install_round_trips_the_manifest() {
        let td = TempDir::new().unwrap();
        let mut m = manifest("a");
        m.description = Some("A plugin".to_string());
        m.config = serde_json::from_str(r#"{"x": 1, "nested": {"y": [1, 2]}, "z": "s"}"#).unwrap();
        install(td.path(), &m, "").unwrap();

        let plugin = Plugin::load(&td.path().join("a")).unwrap();
        assert_eq!(plugin.manifest, m);
    }

    #[test]
    fn manifest_ignores_unknown_fields() {
        let text = "name = 'a'\nversion = '1.0.0'\nengine = 'lua'\nkeywords = ['x']";
        assert!(PluginManifest::parse(text).is_ok());
    }

    #[test]
    fn a_missing_plugins_dir_has_no_plugins() {
        let td = TempDir::new().unwrap();
        let found = discover(&td.path().join("plugins")).unwrap();
        assert!(found.plugins.is_empty());
        assert!(found.errors.is_empty());
    }

    #[test]
    fn plugins_are_discovered_in_directory_order() {
        let td = TempDir::new().unwrap();
        for (dir, name) in [("20-b", "b"), ("10-c", "c"), ("30-a", "a")] {
            write(
                &td.path().join(dir).join(MANIFEST_FILE_NAME),
                &toml::to_string(&manifest(name)).unwrap(),
            );
            write(&td.path().join(dir).join("main.lua"), "");
        }
        // Hidden directories and plain files are not plugins.
        write(&td.path().join(".hidden").join(MANIFEST_FILE_NAME), "{}");
        write(&td.path().join("README.md"), "");

        let found = discover(td.path()).unwrap();
        let names: Vec<_> = found
            .plugins
            .iter()
            .map(|p| p.manifest.name.as_str())
            .collect();
        assert_eq!(names, ["c", "b", "a"]);
        assert!(found.errors.is_empty(), "{:?}", found.errors);
    }

    #[test]
    fn broken_plugins_are_reported_and_skipped() {
        let td = TempDir::new().unwrap();
        // No manifest.
        std::fs::create_dir(td.path().join("empty")).unwrap();
        // No entrypoint.
        write(
            &td.path().join("no-main").join(MANIFEST_FILE_NAME),
            &toml::to_string(&manifest("no-main")).unwrap(),
        );
        // Duplicate name.
        for dir in ["a1", "a2"] {
            write(
                &td.path().join(dir).join(MANIFEST_FILE_NAME),
                &toml::to_string(&manifest("a")).unwrap(),
            );
            write(&td.path().join(dir).join("main.lua"), "");
        }

        let found = discover(td.path()).unwrap();
        assert_eq!(found.plugins.len(), 1);
        assert_eq!(found.plugins[0].dir, td.path().join("a1"));

        let errors: Vec<_> = found
            .errors
            .iter()
            .map(|e| (e.dir.file_name().unwrap().to_str().unwrap(), &e.error))
            .collect();
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(matches!(
            errors[0],
            ("a2", PluginError::DuplicateName { .. })
        ));
        assert!(matches!(errors[1], ("empty", PluginError::MissingManifest)));
        assert!(matches!(
            errors[2],
            ("no-main", PluginError::MissingEntrypoint(_))
        ));
    }

    #[test]
    fn config_file_overrides_manifest_defaults() {
        let td = TempDir::new().unwrap();
        let mut m = manifest("a");
        m.config = serde_json::from_str(r#"{"x": 1, "y": 2}"#).unwrap();
        install(td.path(), &m, "").unwrap();
        write(
            &td.path().join("a").join(CONFIG_FILE_NAME),
            r#"{"y": 3, "z": 4}"#,
        );

        let plugin = Plugin::load(&td.path().join("a")).unwrap();
        assert_eq!(
            serde_json::Value::Object(plugin.config),
            serde_json::json!({"x": 1, "y": 3, "z": 4})
        );

        write(&td.path().join("a").join(CONFIG_FILE_NAME), "[]");
        assert!(matches!(
            Plugin::load(&td.path().join("a")),
            Err(PluginError::InvalidConfig(_))
        ));
    }

    #[test]
    fn source_includes_modules() {
        let td = TempDir::new().unwrap();
        let dir = install(td.path(), &manifest("a"), "-- main").unwrap();
        write(&dir.join("util.lua"), "-- util");
        write(&dir.join("lib/rules.lua"), "-- rules");
        write(&dir.join("lib/init.lua"), "-- lib");
        write(&dir.join("notes.txt"), "not lua");
        write(&dir.join(".hidden.lua"), "-- hidden");

        let source = Plugin::load(&dir).unwrap().load_source().unwrap();
        assert_eq!(source.name, "a");
        assert_eq!(source.text, "-- main");
        assert_eq!(source.config, "{}");
        let modules: Vec<_> = source
            .modules
            .iter()
            .map(|m| (m.name.as_str(), m.text.as_str()))
            .collect();
        assert_eq!(
            modules,
            [
                ("lib", "-- lib"),
                ("lib.rules", "-- rules"),
                ("util", "-- util")
            ]
        );
    }

    #[test]
    fn oversized_plugins_are_rejected() {
        let td = TempDir::new().unwrap();
        let dir = install(td.path(), &manifest("a"), "").unwrap();
        let big = "-".repeat(MAX_PLUGIN_SOURCE_BYTES as usize + 1);
        write(&dir.join("big.lua"), &big);

        let err = Plugin::load(&dir).unwrap().load_source().unwrap_err();
        assert!(matches!(err, PluginError::TooLarge { .. }), "{err}");
    }

    #[test]
    fn load_sources_skips_disabled_plugins() {
        let td = TempDir::new().unwrap();
        install(td.path(), &manifest("on"), "-- on").unwrap();
        let mut off = manifest("off");
        off.enabled = false;
        install(td.path(), &off, "-- off").unwrap();

        let sources = load_sources(&discover(td.path()).unwrap(), PluginEngine::Lua);
        let names: Vec<_> = sources.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["on"]);
    }

    #[test]
    fn install_refuses_to_overwrite() {
        let td = TempDir::new().unwrap();
        install(td.path(), &manifest("a"), "").unwrap();
        assert!(install(td.path(), &manifest("a"), "").is_err());
    }
}
