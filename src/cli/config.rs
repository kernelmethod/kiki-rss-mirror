//! The `kiki config` subcommands, for reading and changing Kiki's settings
//! in `kiki.toml`.
//!
//! Changes are saved through a [`ConfigStore`], as the settings API saves
//! them, so they are checked the same way and never overwrite a change the
//! server is saving at the same moment. A running server notices the file
//! change and applies it; nothing needs to reach the server itself.
use crate::cli::paths::{self, Env};
use crate::cli::plugin::read_input;
use crate::config::{self, ConfigStore, Overrides, Settings};
use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Subcommand};
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Arguments for the `config` subcommand.
#[derive(Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Print settings as TOML
    ///
    /// By default these are the settings in effect: the built-in defaults
    /// with the overrides in kiki.toml applied. $KIKI_PROXY and
    /// $KIKI_NO_PROXY, which override the proxy settings when the server
    /// runs, are not applied. A setting that is unset prints nothing.
    Get(GetArgs),

    /// Change settings
    ///
    /// Either sets one setting, `kiki config set SECTION.KEY VALUE`, or
    /// every setting in a TOML file laid out as kiki.toml is, `kiki config
    /// set --file FILE`. The settings are checked before anything is saved.
    /// A running server applies them as soon as it notices the change.
    Set(SetArgs),

    /// Remove settings' overrides, restoring their defaults
    Unset(UnsetArgs),
}

#[derive(Args)]
struct GetArgs {
    /// A section, such as `feed_fetch`, or a single setting, such as
    /// `feed_fetch.timeout_seconds`. Every setting if left out
    #[arg(value_name = "SECTION[.KEY]")]
    selector: Option<Selector>,

    /// Print the built-in defaults instead
    #[arg(long, conflicts_with = "overrides")]
    defaults: bool,

    /// Print only the overrides in kiki.toml
    #[arg(long)]
    overrides: bool,
}

#[derive(Args)]
struct SetArgs {
    /// The setting to change, such as `feed_fetch.timeout_seconds`
    #[arg(
        value_name = "SECTION.KEY",
        required_unless_present = "file",
        conflicts_with = "file",
        requires = "value"
    )]
    key: Option<Key>,

    /// The new value, in TOML: `3600`, `false`, `'["kiki.lan"]'`. A value
    /// that is not valid TOML is taken as a string, so `read-only` needs no
    /// quotes
    #[arg(value_name = "VALUE")]
    value: Option<String>,

    /// Set every setting in this TOML file, or `-` to read standard input.
    /// Each setting it gives replaces that setting's override; other
    /// overrides are kept unless `--replace` is given
    #[arg(short, long, value_name = "FILE")]
    file: Option<PathBuf>,

    /// With `--file`, remove every override the file leaves out, restoring
    /// those settings' defaults. An empty file removes them all
    #[arg(long, requires = "file", conflicts_with = "key")]
    replace: bool,
}

#[derive(Args)]
struct UnsetArgs {
    /// The settings to restore to their defaults, such as
    /// `feed_fetch.timeout_seconds`
    #[arg(value_name = "SECTION.KEY", required = true)]
    keys: Vec<Key>,
}

/// One setting, `section.key`, as given on the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Key {
    section: String,
    key: String,
}

impl Key {
    /// Splits `s` at its first `.` into a section and a key, both
    /// non-empty, without checking that they name a setting.
    fn split(s: &str) -> Result<Self> {
        match s.split_once('.') {
            Some((section, key)) if !section.is_empty() && !key.is_empty() => Ok(Key {
                section: section.to_string(),
                key: key.to_string(),
            }),
            _ => bail!("{s:?} is not of the form SECTION.KEY, as in feed_fetch.timeout_seconds"),
        }
    }

    /// Whether `kiki web` reads this setting only as it starts.
    fn read_by_web_ui_at_start(&self) -> bool {
        matches!(self.section.as_str(), "http" | "web_ui")
            || (self.section == "api" && self.key == "anonymous_access")
    }
}

impl FromStr for Key {
    type Err = anyhow::Error;

    /// Parses `section.key`, which must name a setting.
    fn from_str(s: &str) -> Result<Self> {
        let key = Key::split(s)?;
        if !Settings::keys()?.contains(&(key.section.clone(), key.key.clone())) {
            bail!("there is no setting {s:?}; `kiki config get` lists them all");
        }
        Ok(key)
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.section, self.key)
    }
}

/// What `kiki config get` prints: a whole section, or one setting.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Selector {
    Section(String),
    Key(Key),
}

impl FromStr for Selector {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        if s.contains('.') {
            return s.parse().map(Selector::Key);
        }
        if !Settings::keys()?.iter().any(|(section, _)| section == s) {
            bail!("there is no section {s:?}; `kiki config get` lists them all");
        }
        Ok(Selector::Section(s.to_string()))
    }
}

/// Parses `s` as a TOML value, or takes it as a string if it is not one.
fn parse_value(s: &str) -> toml::Value {
    format!("value = {s}")
        .parse::<toml::Table>()
        .ok()
        .filter(|t| t.len() == 1)
        .and_then(|mut t| t.remove("value"))
        .unwrap_or_else(|| toml::Value::String(s.to_string()))
}

impl ConfigArgs {
    /// Run the `config` subcommand.
    ///
    /// # Errors
    ///
    /// Returns an error if no data directory can be resolved or it does not
    /// exist, if a setting named does not exist, if the config file cannot
    /// be read or written, or if it, or the change, holds an invalid
    /// setting.
    pub fn run(&self) -> Result<()> {
        let home = paths::resolve_data_dir(&Env::from_process())?.path;
        if !home.is_dir() {
            bail!(
                "no Kiki data directory at {}; run `kiki init` to create one, or set \
                 $KIKI_HOME to point Kiki somewhere else",
                home.display(),
            );
        }
        let path = home.join(config::CONFIG_FILE_NAME);
        let mut stdout = std::io::stdout().lock();
        let mut stderr = std::io::stderr().lock();

        match &self.command {
            ConfigCommand::Get(args) => args.get(&path, &mut stdout),
            ConfigCommand::Set(args) => {
                check_owner(&home)?;
                let input = args.file.as_deref().map(read_input).transpose()?;
                args.set(&path, input.as_deref(), &mut stderr)
            }
            ConfigCommand::Unset(args) => {
                check_owner(&home)?;
                args.unset(&path, &mut stderr)
            }
        }
    }
}

/// Refuses to change the settings in Kiki's home directory `home` unless
/// this process runs as the user that owns it.
///
/// Saving replaces the config file with one this process creates, and
/// creates the lock file beside it if it is missing, so both would belong
/// to this user. Run as root, that would leave the server, running as its
/// own user, unable to read its settings or save them.
fn check_owner(home: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let owner = std::fs::metadata(home)
            .with_context(|| format!("unable to read {home:?}"))?
            .uid();
        // SAFETY: geteuid is always safe to call and has no failure modes.
        let euid = unsafe { libc::geteuid() };
        if owner != euid {
            bail!(
                "{} belongs to another user (uid {owner}); change the settings as the user \
                 Kiki runs as, as in `sudo -u kiki env KIKI_HOME={} kiki config ...`, so that \
                 the server can still read them",
                home.display(),
                home.display(),
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = home;
    }
    Ok(())
}

impl GetArgs {
    /// Write the requested settings, from the config file at `path`, to
    /// `out` as TOML.
    fn get(&self, path: &Path, out: &mut impl Write) -> Result<()> {
        let table = if self.defaults {
            toml::Table::try_from(Settings::default())?
        } else if self.overrides {
            config::read_overrides(path)?.as_table().clone()
        } else {
            let store = ConfigStore::open(path)
                .with_context(|| format!("failed to load config file {path:?}"))?;
            toml::Table::try_from(store.current().as_ref())?
        };

        match &self.selector {
            None => out.write_all(toml::to_string(&table)?.as_bytes())?,
            Some(Selector::Section(section)) => {
                if let Some(keys) = table.get(section) {
                    let mut only = toml::Table::new();
                    only.insert(section.clone(), keys.clone());
                    out.write_all(toml::to_string(&only)?.as_bytes())?;
                }
            }
            Some(Selector::Key(key)) => {
                let value = table
                    .get(&key.section)
                    .and_then(toml::Value::as_table)
                    .and_then(|keys| keys.get(&key.key));
                if let Some(value) = value {
                    writeln!(out, "{value}")?;
                }
            }
        }
        Ok(())
    }
}

impl SetArgs {
    /// Save the change to the config file at `path`, reporting what
    /// happened to `out`. `input` is the TOML read from `--file`, if it was
    /// given.
    fn set(&self, path: &Path, input: Option<&str>, out: &mut impl Write) -> Result<()> {
        let store = ConfigStore::open(path)
            .with_context(|| format!("failed to load config file {path:?}"))?;

        let (keys, changed) = match (input, &self.key, &self.value) {
            (Some(input), _, _) => {
                let file = self.file.as_deref().unwrap_or(Path::new("-"));
                let changes = Overrides::parse(input)
                    .with_context(|| format!("failed to parse TOML from {file:?}"))?;
                let keys = keys_of(&changes)?;
                let (_, changed) = store
                    .update_reporting(|o| {
                        if self.replace {
                            *o = changes;
                        } else {
                            o.extend(&changes);
                        }
                        Ok(())
                    })
                    .with_context(|| format!("failed to set the settings in {file:?}"))?;
                if changed {
                    writeln!(out, "Saved the settings to {}.", path.display())?;
                } else {
                    writeln!(out, "No change: {} already holds them.", path.display())?;
                }
                (keys, changed)
            }
            (None, Some(key), Some(value)) => {
                let value = parse_value(value);
                let (_, changed) = store
                    .update_reporting(|o| o.set(&key.section, &key.key, &value))
                    .with_context(|| format!("failed to set {key}"))?;
                if changed {
                    writeln!(out, "Set {key} = {value}.")?;
                } else {
                    writeln!(out, "No change: {key} is already set to {value}.")?;
                }
                (vec![key.clone()], changed)
            }
            _ => bail!("give either SECTION.KEY and VALUE, or --file"),
        };

        if changed {
            web_ui_restart_hint(&keys, out)?;
        }
        Ok(())
    }
}

/// The settings `overrides` sets, as [`Key`]s, unchecked.
///
/// # Errors
///
/// Returns an error if a top-level value is not a table of settings.
fn keys_of(overrides: &Overrides) -> Result<Vec<Key>> {
    let mut keys = Vec::new();
    for (section, value) in overrides.as_table() {
        let table = value
            .as_table()
            .ok_or_else(|| anyhow!("{section} must be a table of settings, as in [{section}]"))?;
        keys.extend(table.keys().map(|key| Key {
            section: section.clone(),
            key: key.clone(),
        }));
    }
    Ok(keys)
}

/// Tells `out` to restart `kiki web` if any of `keys` is read by it only
/// as it starts.
fn web_ui_restart_hint(keys: &[Key], out: &mut impl Write) -> Result<()> {
    if keys.iter().any(Key::read_by_web_ui_at_start) {
        writeln!(
            out,
            "kiki web reads [http], [web_ui] and api.anonymous_access as it starts: \
             restart it for the change to reach the web UI."
        )?;
    }
    Ok(())
}

impl UnsetArgs {
    /// Remove the overrides from the config file at `path`, reporting
    /// what happened to `out`.
    fn unset(&self, path: &Path, out: &mut impl Write) -> Result<()> {
        let store = ConfigStore::open(path)
            .with_context(|| format!("failed to load config file {path:?}"))?;
        let mut removed = Vec::new();
        store.update_reporting(|o| {
            for key in &self.keys {
                if o.get(&key.section, &key.key).is_some() {
                    o.unset(&key.section, &key.key);
                    removed.push(key.clone());
                }
            }
            Ok(())
        })?;

        for key in &self.keys {
            if removed.contains(key) {
                writeln!(out, "Unset {key}; it is back to its default.")?;
            } else {
                writeln!(out, "No change: {key} was not set.")?;
            }
        }
        web_ui_restart_hint(&removed, out)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::fs;
    use tempfile::TempDir;

    #[derive(Parser)]
    struct TestCli {
        #[command(subcommand)]
        command: ConfigCommand,
    }

    fn parse(argv: &[&str]) -> Result<ConfigCommand, clap::Error> {
        TestCli::try_parse_from(std::iter::once("kiki-config").chain(argv.iter().copied()))
            .map(|cli| cli.command)
    }

    /// A temporary home directory and the config file in it.
    fn home() -> (TempDir, PathBuf) {
        let td = TempDir::with_prefix("kiki_config_cli").unwrap();
        let path = td.path().join(config::CONFIG_FILE_NAME);
        (td, path)
    }

    /// Runs `argv` against the config file at `path`, returning what it
    /// writes to standard output (for `get`) or its report (otherwise).
    fn run(path: &Path, argv: &[&str], input: Option<&str>) -> Result<String> {
        let mut out = Vec::new();
        match parse(argv)? {
            ConfigCommand::Get(args) => args.get(path, &mut out)?,
            ConfigCommand::Set(args) => args.set(path, input, &mut out)?,
            ConfigCommand::Unset(args) => args.unset(path, &mut out)?,
        }
        Ok(String::from_utf8(out).unwrap())
    }

    fn settings(path: &Path) -> Settings {
        ConfigStore::open(path).unwrap().current().as_ref().clone()
    }

    #[test]
    fn values_are_toml_or_else_strings() {
        assert_eq!(parse_value("3600"), toml::Value::Integer(3600));
        assert_eq!(parse_value("false"), toml::Value::Boolean(false));
        assert_eq!(parse_value("\"read-only\""), "read-only".into());
        assert_eq!(parse_value("read-only"), "read-only".into());
        assert_eq!(
            parse_value("socks5h://127.0.0.1:9050"),
            "socks5h://127.0.0.1:9050".into()
        );
        assert_eq!(
            parse_value(r#"["kiki.lan", "*.home.example"]"#),
            toml::Value::Array(vec!["kiki.lan".into(), "*.home.example".into()])
        );
        // Not a single value, so taken whole as a string.
        assert_eq!(parse_value("1\nother = 2"), "1\nother = 2".into());
    }

    #[test]
    fn keys_must_name_a_setting() {
        assert!(parse(&["get", "feed_fetch.timeout_seconds"]).is_ok());
        assert!(parse(&["get", "feed_fetch"]).is_ok());
        assert!(parse(&["get", "proxy.url"]).is_ok());
        assert!(parse(&["get", "feed_fetch.timeout"]).is_err());
        assert!(parse(&["get", "feeds"]).is_err());
        assert!(parse(&["set", "feed_fetch", "1"]).is_err());
        assert!(parse(&["set", ".timeout_seconds", "1"]).is_err());
        assert!(parse(&["unset", "feed_fetch.nope"]).is_err());
    }

    #[test]
    fn set_takes_a_key_and_value_or_a_file() {
        assert!(parse(&["set"]).is_err());
        assert!(parse(&["set", "asset_cache.enabled"]).is_err());
        assert!(parse(&["set", "asset_cache.enabled", "false", "--file", "x"]).is_err());
        assert!(parse(&["set", "asset_cache.enabled", "false", "--replace"]).is_err());
        assert!(parse(&["set", "--file", "-", "--replace"]).is_ok());
    }

    #[test]
    fn set_saves_one_setting_and_reports_whether_it_changed() {
        let (_td, path) = home();
        let report = run(&path, &["set", "api.anonymous_access", "read-only"], None).unwrap();
        assert!(
            report.starts_with("Set api.anonymous_access = \"read-only\"."),
            "{report}"
        );
        assert!(report.contains("restart it"), "{report}");
        assert_eq!(
            settings(&path).api.anonymous_access,
            config::AnonymousAccess::ReadOnly
        );

        let report = run(&path, &["set", "api.anonymous_access", "read-only"], None).unwrap();
        assert!(report.starts_with("No change"), "{report}");
        assert!(!report.contains("restart it"), "{report}");
    }

    #[test]
    fn set_refuses_an_invalid_value_and_saves_nothing() {
        let (_td, path) = home();
        let err = run(&path, &["set", "feed_fetch.timeout_seconds", "0"], None).unwrap_err();
        assert!(format!("{err:#}").contains("greater than zero"), "{err:#}");
        let err = run(&path, &["set", "asset_cache.enabled", "yes"], None).unwrap_err();
        assert!(format!("{err:#}").contains("asset_cache"), "{err:#}");
        assert!(!path.exists());
    }

    #[test]
    fn set_from_a_file_keeps_other_overrides_unless_replacing() {
        let (_td, path) = home();
        run(&path, &["set", "asset_cache.enabled", "false"], None).unwrap();

        let input = "[feed_fetch]\ntimeout_seconds = 30\n";
        run(&path, &["set", "--file", "-"], Some(input)).unwrap();
        let s = settings(&path);
        assert_eq!(s.feed_fetch.timeout_seconds, 30);
        assert!(!s.asset_cache.enabled);

        let report = run(&path, &["set", "--file", "-"], Some(input)).unwrap();
        assert!(report.starts_with("No change"), "{report}");

        run(&path, &["set", "--file", "-", "--replace"], Some(input)).unwrap();
        let s = settings(&path);
        assert_eq!(s.feed_fetch.timeout_seconds, 30);
        assert!(
            s.asset_cache.enabled,
            "--replace drops overrides the file leaves out"
        );

        let err = run(
            &path,
            &["set", "--file", "-"],
            Some("[feed_fetch]\nbogus = 1\n"),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("bogus"), "{err:#}");
        let err = run(
            &path,
            &["set", "--file", "-"],
            Some("timeout_seconds = 1\n"),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("table"), "{err:#}");
    }

    #[test]
    fn unset_restores_defaults() {
        let (_td, path) = home();
        run(&path, &["set", "feed_fetch.timeout_seconds", "30"], None).unwrap();
        let report = run(
            &path,
            &["unset", "feed_fetch.timeout_seconds", "asset_cache.enabled"],
            None,
        )
        .unwrap();
        assert_eq!(
            report,
            "Unset feed_fetch.timeout_seconds; it is back to its default.\n\
             No change: asset_cache.enabled was not set.\n"
        );
        assert_eq!(settings(&path), Settings::default());
    }

    #[test]
    fn get_prints_settings_overrides_or_defaults() {
        let (_td, path) = home();
        run(&path, &["set", "feed_fetch.timeout_seconds", "30"], None).unwrap();
        run(
            &path,
            &["set", "http.allowed_hosts", r#"["kiki.lan"]"#],
            None,
        )
        .unwrap();

        assert_eq!(
            run(&path, &["get", "feed_fetch.timeout_seconds"], None).unwrap(),
            "30\n"
        );
        assert_eq!(
            run(
                &path,
                &["get", "feed_fetch.timeout_seconds", "--defaults"],
                None
            )
            .unwrap(),
            "15\n"
        );
        assert_eq!(
            run(&path, &["get", "http.allowed_hosts"], None).unwrap(),
            "[\"kiki.lan\"]\n"
        );
        assert_eq!(
            run(&path, &["get", "api.anonymous_access"], None).unwrap(),
            "\"full\"\n"
        );
        // Unset, or not overridden: nothing.
        assert_eq!(run(&path, &["get", "proxy.url"], None).unwrap(), "");
        assert_eq!(
            run(&path, &["get", "asset_cache.enabled", "--overrides"], None).unwrap(),
            ""
        );

        assert_eq!(
            run(&path, &["get", "--overrides"], None).unwrap(),
            "[feed_fetch]\ntimeout_seconds = 30\n\n[http]\nallowed_hosts = [\"kiki.lan\"]\n"
        );
        let section = run(&path, &["get", "feed_fetch"], None).unwrap();
        assert!(section.starts_with("[feed_fetch]\n"), "{section}");
        assert!(section.contains("timeout_seconds = 30\n"), "{section}");
        assert!(!section.contains("[asset_cache]"), "{section}");

        // Everything round-trips through `set --file`.
        let all = run(&path, &["get"], None).unwrap();
        let (_td2, other) = home();
        run(&other, &["set", "--file", "-"], Some(&all)).unwrap();
        assert_eq!(settings(&other), settings(&path));
    }

    #[test]
    fn get_prints_overrides_even_from_an_invalid_file() {
        let (_td, path) = home();
        fs::write(&path, "[feed_fetch]\ntimeout_seconds = 0\n").unwrap();
        assert!(run(&path, &["get"], None).is_err());
        assert_eq!(
            run(&path, &["get", "--overrides"], None).unwrap(),
            "[feed_fetch]\ntimeout_seconds = 0\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn changes_are_refused_in_another_users_home() {
        // Owned by root; tests that run as root have nothing to check.
        // SAFETY: geteuid is always safe to call and has no failure modes.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        assert!(check_owner(Path::new("/")).is_err());
        let (td, _) = home();
        assert!(check_owner(td.path()).is_ok());
    }
}
