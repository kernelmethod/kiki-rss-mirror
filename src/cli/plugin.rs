//! The `kiki plugin` subcommands, for listing installed plugins and reading
//! and changing their config.
//!
//! Like the rest of the CLI, these work directly on Kiki's home directory:
//! plugins are discovered in its plugins directory (see [`crate::plugins`])
//! and config overrides are kept in its database (see
//! [`crate::db::plugins`]), so they run whether or not the server is up.
//! After saving a config, `config set` hands it to the running server, if
//! there is one, which reloads its plugins with it.
//!
//! Configs are read and written as TOML, the format of the `[config]` table
//! in a plugin's manifest.
use crate::cli::paths::{self, Env};
use crate::db::plugins::{self as db, ConfigOverrides};
use crate::db::ConnectionBuilder;
use crate::plugins::{self, apply_config_overrides, toml_to_json, Discovery, Plugin};
use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Subcommand};
use serde_json::{Map, Value};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Arguments for the `plugin` subcommand.
#[derive(Args)]
pub struct PluginArgs {
    #[command(subcommand)]
    command: PluginCommand,
}

#[derive(Subcommand)]
enum PluginCommand {
    /// List the plugins installed in the plugins directory
    ///
    /// Directories in the plugins directory that cannot be loaded as plugins
    /// are reported on standard error.
    #[command(visible_alias = "list")]
    Ls,

    /// Read or change a plugin's config
    Config(ConfigArgs),
}

#[derive(Args)]
struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Print a plugin's config as TOML
    ///
    /// By default this is the config the plugin will be loaded with the next
    /// time the server starts: the defaults from its manifest with its
    /// overrides applied.
    Get(GetArgs),

    /// Set a plugin's config overrides from TOML
    ///
    /// Each top-level key of the TOML document replaces the whole of that
    /// key's override; nested tables are not merged. Other overrides are
    /// kept unless `--replace` is given. If the server is running, it
    /// reloads its plugins with the new config; otherwise the config takes
    /// effect when the server starts.
    Set(SetArgs),
}

#[derive(Args)]
struct GetArgs {
    /// Name of the plugin
    name: String,

    /// Print only the defaults from the plugin's manifest
    #[arg(long, conflicts_with = "overrides")]
    defaults: bool,

    /// Print only the plugin's config overrides
    #[arg(long)]
    overrides: bool,
}

#[derive(Args)]
struct SetArgs {
    /// Name of the plugin
    name: String,

    /// TOML file holding the overrides to set, or `-` to read from standard
    /// input
    #[arg(default_value = "-")]
    file: PathBuf,

    /// Replace every override with the keys of the TOML document, rather
    /// than keeping the overrides it leaves out. An empty document removes
    /// every override.
    #[arg(long)]
    replace: bool,
}

impl PluginArgs {
    /// Run the `plugin` subcommand.
    ///
    /// # Errors
    ///
    /// Returns an error if no data directory can be resolved, the plugins
    /// directory cannot be read, the named plugin is not installed, the
    /// database cannot be read or written, or the TOML input cannot be read
    /// or parsed.
    pub fn run(&self) -> Result<()> {
        // The same home directory `kiki serve` would use
        let home = paths::resolve_data_dir(&Env::from_process())?.path;
        let mut stdout = std::io::stdout().lock();

        match &self.command {
            PluginCommand::Ls => list(&home, &mut stdout, &mut std::io::stderr().lock()),
            PluginCommand::Config(args) => match &args.command {
                ConfigCommand::Get(args) => args.get(&home, &mut stdout),
                ConfigCommand::Set(args) => {
                    let input = read_input(&args.file)?;
                    let overrides = args.set(&home, &input)?;
                    eprintln!("Saved the config of plugin {:?}.", args.name);
                    match apply_to_server(&home, &args.name, &overrides) {
                        Ok(None) => eprintln!("The server reloaded its plugins with it."),
                        Ok(Some(e)) => eprintln!(
                            "The server failed to load the plugins with it, and keeps running \
                             them with the config they had: {e}"
                        ),
                        Err(e) => {
                            tracing::debug!("could not reach the server: {e:#}");
                            eprintln!("It takes effect when the server starts.");
                        }
                    }
                    Ok(())
                }
            },
        }
    }
}

/// Write a table of the plugins installed in `home` to `out`, and the
/// directories that could not be loaded as plugins to `err`.
fn list(home: &Path, out: &mut impl Write, err: &mut impl Write) -> Result<()> {
    let discovery = discover(home)?;

    let rows: Vec<[String; 5]> = discovery
        .plugins
        .iter()
        .map(|p| {
            let m = &p.manifest;
            let status = if !m.engine.is_supported() {
                "unsupported"
            } else if m.enabled {
                "enabled"
            } else {
                "disabled"
            };
            [
                m.name.clone(),
                m.version.clone(),
                m.engine.name().to_string(),
                status.to_string(),
                m.description.clone().unwrap_or_default(),
            ]
        })
        .collect();

    if rows.is_empty() {
        writeln!(out, "No plugins installed.")?;
    } else {
        let header = ["NAME", "VERSION", "ENGINE", "STATUS", "DESCRIPTION"].map(String::from);
        let mut widths = [0; 5];
        for row in std::iter::once(&header).chain(&rows) {
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(cell.chars().count());
            }
        }
        for row in std::iter::once(&header).chain(&rows) {
            let mut line = String::new();
            for (cell, width) in row.iter().zip(widths) {
                line.push_str(&format!("{cell:width$}  "));
            }
            writeln!(out, "{}", line.trim_end())?;
        }
    }

    for e in &discovery.errors {
        writeln!(err, "warning: skipped {:?}: {}", e.dir, e.error)?;
    }
    Ok(())
}

impl GetArgs {
    /// Write the requested config of the plugin as TOML to `out`.
    fn get(&self, home: &Path, out: &mut impl Write) -> Result<()> {
        let plugin = find_plugin(home, &self.name)?;
        let config = if self.defaults {
            plugin.manifest.config
        } else {
            let conn = ConnectionBuilder::default()
                .at_path(&database(home))
                .build()
                .with_context(|| format!("failed to open database in {home:?}"))?;
            let overrides = db::get_config_overrides(&conn, &self.name)
                .with_context(|| format!("failed to read the config of plugin {:?}", self.name))?;
            if self.overrides {
                overrides
            } else {
                apply_config_overrides(&plugin.manifest.config, overrides)
            }
        };

        out.write_all(config_to_toml(&config)?.as_bytes())?;
        Ok(())
    }
}

impl SetArgs {
    /// Parse `input` as TOML and save it as the plugin's config overrides,
    /// returning the overrides saved.
    fn set(&self, home: &Path, input: &str) -> Result<ConfigOverrides> {
        let changes = toml_to_config(input)
            .with_context(|| format!("failed to parse TOML from {:?}", self.file))?;
        let plugin = find_plugin(home, &self.name)?;
        plugins::settings::check_config(&plugin.manifest.settings, &changes)
            .with_context(|| format!("invalid config for plugin {:?}", self.name))?;

        let mut conn = ConnectionBuilder::default()
            .at_path(&database(home))
            .read_write()
            .build()
            .with_context(|| format!("failed to open database in {home:?}"))?;

        let result = if self.replace {
            db::set_config_overrides(&conn, &self.name, &changes).map(|()| changes)
        } else {
            db::update_config_overrides(&mut conn, &self.name, |overrides| {
                overrides.extend(changes)
            })
        };
        result.with_context(|| format!("failed to save the config of plugin {:?}", self.name))
    }
}

/// Hands the config overrides `overrides` of plugin `name` to the server
/// running on Kiki's home directory `home`, which reloads its plugins with
/// them.
///
/// Returns why the plugins failed to load with the new config, if they did.
///
/// # Errors
///
/// Returns an error if no server can be reached, or it does not accept the
/// config.
fn apply_to_server(home: &Path, name: &str, overrides: &ConfigOverrides) -> Result<Option<String>> {
    use crate::routes::v1::plugins::plugin_config::PluginConfigResponse;

    let env = Env::from_process();
    let data_dir = paths::resolve_data_dir(&env)?;
    anyhow::ensure!(
        data_dir.path == home,
        "the server's home directory is not {home:?}"
    );
    let socket = paths::resolve_socket_path(None, &data_dir, &env);
    if !socket.exists() {
        bail!("no server socket at {socket:?}");
    }

    let url = format!(
        "http://kiki/v1/plugins/name/{}/config",
        url::form_urlencoded::byte_serialize(name.as_bytes()).collect::<String>()
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let client = reqwest::Client::builder()
                .unix_socket(socket.as_path())
                .timeout(std::time::Duration::from_secs(30))
                .build()?;
            let resp = client.put(url).json(overrides).send().await?;
            let resp = resp.error_for_status()?;
            Ok(resp.json::<PluginConfigResponse>().await?.reload_error)
        })
}

/// The database in Kiki's home directory `home`.
fn database(home: &Path) -> PathBuf {
    home.join(paths::DB_FILE_NAME)
}

/// Discover the plugins installed in Kiki's home directory `home`.
fn discover(home: &Path) -> Result<Discovery> {
    let dir = plugins::plugins_dir(home);
    plugins::discover(&dir).with_context(|| format!("failed to read plugins directory {dir:?}"))
}

/// Find the installed plugin named `name`.
fn find_plugin(home: &Path, name: &str) -> Result<Plugin> {
    let dir = plugins::plugins_dir(home);
    discover(home)?
        .plugins
        .into_iter()
        .find(|p| p.manifest.name == name)
        .ok_or_else(|| anyhow!("no plugin named {name:?} is installed in {dir:?}"))
}

/// Parse a TOML document into a config, converting its values to JSON as a
/// manifest's `[config]` table is.
fn toml_to_config(input: &str) -> Result<ConfigOverrides> {
    let table: toml::Table = toml::from_str(input)?;
    match toml_to_json(&toml::Value::Table(table))? {
        Value::Object(map) => Ok(map),
        _ => bail!("TOML document is not a table"),
    }
}

/// Format a config as a TOML document.
///
/// TOML has no null, so top-level keys set to `null` (which a plugin sees as
/// absent) are left out, each noted in a comment at the top of the
/// document.
///
/// # Errors
///
/// Returns an error if the config holds a `null` below its top level, or an
/// integer too large for TOML.
fn config_to_toml(config: &Map<String, Value>) -> Result<String> {
    let mut doc = String::new();
    let mut table = toml::Table::new();
    for (key, value) in config {
        if value.is_null() {
            doc.push_str(&format!("# {key:?} is set to null\n"));
        } else {
            let value = json_to_toml(value).with_context(|| format!("in config key {key:?}"))?;
            table.insert(key.clone(), value);
        }
    }
    doc.push_str(&toml::to_string(&table)?);
    Ok(doc)
}

/// Convert a JSON value into the equivalent TOML value; the inverse of
/// [`toml_to_json`], except that dates and times stay strings.
fn json_to_toml(value: &Value) -> Result<toml::Value> {
    Ok(match value {
        Value::Null => bail!("null has no TOML equivalent"),
        Value::Bool(b) => toml::Value::Boolean(*b),
        Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => toml::Value::Integer(i),
            (None, Some(f)) if !n.is_u64() => toml::Value::Float(f),
            _ => bail!("{n} is too large for a TOML integer"),
        },
        Value::String(s) => toml::Value::String(s.clone()),
        Value::Array(items) => {
            toml::Value::Array(items.iter().map(json_to_toml).collect::<Result<_>>()?)
        }
        Value::Object(map) => toml::Value::Table(
            map.iter()
                .map(|(k, v)| Ok((k.clone(), json_to_toml(v)?)))
                .collect::<Result<_>>()?,
        ),
    })
}

/// Read the whole of `path`, treating `-` as standard input.
fn read_input(path: &Path) -> Result<String> {
    if path == Path::new("-") {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("failed to read TOML from standard input")?;
        Ok(buf)
    } else {
        std::fs::read_to_string(path).with_context(|| format!("failed to read {path:?}"))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    /// A home directory with a database and one plugin, `hello`, whose
    /// defaults are `a = 1` and `b = [1, 2]`, and whose setting `a` is an
    /// integer.
    fn home() -> TempDir {
        let home = TempDir::with_prefix("kiki-plugin").unwrap();
        ConnectionBuilder::default()
            .at_path(&database(home.path()))
            .create()
            .build()
            .unwrap();

        let dir = plugins::plugins_dir(home.path()).join("hello");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(plugins::MANIFEST_FILE_NAME),
            "name = 'hello'\nversion = '1.0.0'\nengine = 'lua'\n\
             description = 'Says hello'\n[config]\na = 1\nb = [1, 2]\n\
             [[settings]]\nname = 'a'\ntype = 'integer'\n",
        )
        .unwrap();
        std::fs::write(dir.join("main.lua"), "").unwrap();
        home
    }

    fn get(home: &TempDir, defaults: bool, overrides: bool) -> String {
        let mut out = Vec::new();
        GetArgs {
            name: "hello".into(),
            defaults,
            overrides,
        }
        .get(home.path(), &mut out)
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    fn set(home: &TempDir, input: &str, replace: bool) -> Result<ConfigOverrides> {
        SetArgs {
            name: "hello".into(),
            file: "-".into(),
            replace,
        }
        .set(home.path(), input)
    }

    #[test]
    fn test_list() {
        let home = home();
        std::fs::create_dir(plugins::plugins_dir(home.path()).join("broken")).unwrap();

        let (mut out, mut err) = (Vec::new(), Vec::new());
        list(home.path(), &mut out, &mut err).unwrap();
        let out = String::from_utf8(out).unwrap();
        let mut lines = out
            .lines()
            .map(|l| l.split_whitespace().collect::<Vec<_>>());
        assert_eq!(
            lines.next().unwrap(),
            ["NAME", "VERSION", "ENGINE", "STATUS", "DESCRIPTION"]
        );
        let status = if cfg!(feature = "lua") {
            "enabled"
        } else {
            "unsupported"
        };
        assert_eq!(
            lines.next().unwrap(),
            ["hello", "1.0.0", "lua", status, "Says", "hello"]
        );
        assert!(lines.next().is_none());
        assert!(String::from_utf8(err).unwrap().contains("broken"));
    }

    #[test]
    fn test_list_empty() {
        let home = TempDir::with_prefix("kiki-plugin").unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        list(home.path(), &mut out, &mut err).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "No plugins installed.\n");
        assert!(err.is_empty());
    }

    #[test]
    fn test_get_defaults() {
        let home = home();
        assert_eq!(get(&home, false, false), "a = 1\nb = [1, 2]\n");
        assert_eq!(get(&home, true, false), "a = 1\nb = [1, 2]\n");
        assert_eq!(get(&home, false, true), "");
    }

    #[test]
    fn test_set_merges_overrides() -> Result<()> {
        let home = home();
        set(&home, "b = 'x'\n[c]\nd = true\n", false)?;
        let overrides = set(&home, "e = 2.5", false)?;
        assert_eq!(
            Value::Object(overrides),
            json!({"b": "x", "c": {"d": true}, "e": 2.5})
        );

        assert_eq!(
            get(&home, false, false),
            "a = 1\nb = \"x\"\ne = 2.5\n\n[c]\nd = true\n"
        );
        assert_eq!(get(&home, true, false), "a = 1\nb = [1, 2]\n");
        assert_eq!(
            get(&home, false, true),
            "b = \"x\"\ne = 2.5\n\n[c]\nd = true\n"
        );
        Ok(())
    }

    #[test]
    fn test_set_replace() -> Result<()> {
        let home = home();
        set(&home, "a = 2\nb = 3", false)?;
        let overrides = set(&home, "b = 4", true)?;
        assert_eq!(Value::Object(overrides), json!({"b": 4}));
        assert_eq!(get(&home, false, false), "a = 1\nb = 4\n");

        // An empty document with `--replace` restores every default
        set(&home, "", true)?;
        assert_eq!(get(&home, false, true), "");
        Ok(())
    }

    #[test]
    fn test_set_rejects_bad_input() {
        let home = home();
        assert!(set(&home, "not toml", false).is_err());
        assert!(set(&home, "a = nan", false).is_err());

        let err = set(&home, "a = 'x'", false).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "invalid config for plugin \"hello\": a: expected an integer"
        );
        assert_eq!(get(&home, false, true), "");

        let missing = SetArgs {
            name: "missing".into(),
            file: "-".into(),
            replace: false,
        };
        let err = missing.set(home.path(), "a = 1").unwrap_err();
        assert!(err.to_string().contains("no plugin named \"missing\""));
    }

    #[test]
    fn test_get_null_overrides() -> Result<()> {
        let home = home();
        let conn = ConnectionBuilder::default()
            .at_path(&database(home.path()))
            .read_write()
            .build()?;
        let overrides = json!({"a": null});
        db::set_config_overrides(&conn, "hello", overrides.as_object().unwrap())?;

        assert_eq!(
            get(&home, false, false),
            "# \"a\" is set to null\nb = [1, 2]\n"
        );
        Ok(())
    }

    #[test]
    fn test_json_to_toml() {
        assert!(json_to_toml(&json!([1, null])).is_err());
        assert!(json_to_toml(&json!(u64::MAX)).is_err());
        assert_eq!(
            json_to_toml(&json!({"x": [1, -2.5, "s"]})).unwrap(),
            toml::from_str::<toml::Value>("x = [1, -2.5, 's']").unwrap()
        );
    }
}
