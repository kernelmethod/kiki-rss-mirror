//! Plugins bundled with Kiki and installed by `kiki init`.
//!
//! The build script packs each default plugin's directory from `plugins/` in
//! Kiki's source into a zstd-compressed tarball, [`DEFAULT_PLUGINS_TAR_ZST`],
//! embedded in the binary. [`sync_default_plugins`] unpacks it into the
//! system plugins directory (see [`crate::plugins::PluginSource::System`]),
//! where the plugins are discovered like any other plugin, so they can be
//! configured, disabled in their manifest, edited or deleted.
//!
//! `kiki init --check` syncs them on every run, so a new release of Kiki
//! brings new default plugins, and updates to the ones already installed,
//! to an existing home directory. A default plugin that has been edited or
//! deleted is left as it is.
//!
//! Only built with the `default-plugins` feature.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// A zstd-compressed tarball of the plugins installed by default: currently
/// `adaptive-fetch`, `auto-tag`, `filter`, `privacy`, `retention` and
/// `sanitize`.
///
/// Each plugin's directory is stored under its own name at the root of the
/// archive, and holds only regular files.
pub static DEFAULT_PLUGINS_TAR_ZST: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/default-plugins.tar.zst"));

/// A file in [`DEFAULT_PLUGINS_TAR_ZST`].
struct BundledFile {
    /// The plugin the file belongs to: the first component of its path.
    plugin: String,
    /// The file's path, relative to the plugins directory.
    path: PathBuf,
    contents: Vec<u8>,
}

impl BundledFile {
    /// The file's path relative to its plugin's directory.
    fn path_in_plugin(&self) -> &Path {
        let mut components = self.path.components();
        components.next();
        components.as_path()
    }
}

/// Joins `path`'s components with `/`, the form paths take in a
/// [`FileHashes`] map on every platform.
fn slash_path(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Reads every file in [`DEFAULT_PLUGINS_TAR_ZST`], in archive order.
fn bundled_files() -> Result<Vec<BundledFile>> {
    let decoder = zstd::Decoder::new(DEFAULT_PLUGINS_TAR_ZST)
        .context("unable to decompress the default plugins")?;
    let mut archive = tar::Archive::new(decoder);
    let mut files = Vec::new();
    for entry in archive
        .entries()
        .context("unable to read the default plugins")?
    {
        let mut entry = entry.context("unable to read the default plugins")?;
        let path = entry.path()?.into_owned();
        if entry.header().entry_type() != tar::EntryType::Regular {
            bail!("bundled plugin entry {path:?} is not a regular file");
        }

        let mut components = path.components();
        let plugin = match components.next() {
            Some(Component::Normal(name)) if components.clone().next().is_some() => {
                name.to_string_lossy().into_owned()
            }
            _ => bail!("bundled plugin file {path:?} is not inside a plugin directory"),
        };
        if !components.all(|c| matches!(c, Component::Normal(_))) {
            bail!("bundled plugin file {path:?} escapes its directory");
        }

        let mut contents = Vec::new();
        entry
            .read_to_end(&mut contents)
            .with_context(|| format!("unable to read bundled plugin file {path:?}"))?;
        files.push(BundledFile {
            plugin,
            path,
            contents,
        });
    }
    Ok(files)
}

/// Name of the file, inside the system plugins directory, that records the
/// default plugins [`sync_default_plugins`] has installed and the files each
/// was installed with. Its name starts with a dot, so
/// [`crate::plugins::discover`] ignores it.
pub const RECORD_FILE_NAME: &str = ".default-plugins.toml";

/// How deep inside a plugin directory files are compared. Matches the depth
/// the build script bundles files from.
const MAX_DEPTH: usize = 8;

/// The files of one plugin: each file's `/`-separated path, relative to the
/// plugin's directory, mapped to the BLAKE3 hash of its contents.
type FileHashes = BTreeMap<String, String>;

/// The contents of [`RECORD_FILE_NAME`]: for each default plugin that was
/// installed, the files it was last installed or updated with.
type Record = BTreeMap<String, FileHashes>;

/// What [`sync_default_plugins`] did with one default plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    /// The plugin wasn't in the plugins directory, and was installed.
    Installed,
    /// The plugin was an older bundled version, unmodified since it was
    /// installed, and was replaced with the bundled one.
    Updated,
    /// The plugin already matches the bundled version.
    UpToDate,
    /// The plugin's directory differs from both the bundled version and the
    /// version that was installed, so it has been edited (or was never a
    /// default plugin), and was left alone.
    Modified,
    /// The plugin was installed once but its directory has since been
    /// deleted, and was not reinstalled.
    Removed,
}

/// Hashes `contents` for a [`FileHashes`] map.
fn hash(contents: &[u8]) -> String {
    blake3::hash(contents).to_hex().to_string()
}

/// Reads [`RECORD_FILE_NAME`] from `plugins_dir`: empty if there is none.
fn read_record(plugins_dir: &Path) -> Result<Record> {
    let path = plugins_dir.join(RECORD_FILE_NAME);
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            toml::from_str(&text).with_context(|| format!("unable to parse {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Record::new()),
        Err(e) => Err(e).with_context(|| format!("unable to read {}", path.display())),
    }
}

/// Replaces [`RECORD_FILE_NAME`] in `plugins_dir` with `record`.
fn write_record(plugins_dir: &Path, record: &Record) -> Result<()> {
    let path = plugins_dir.join(RECORD_FILE_NAME);
    let text = format!(
        "# Written by `kiki init`: the default plugins it installed, and the\n\
         # BLAKE3 hash of each file they were installed with. A plugin whose\n\
         # files still match is updated when Kiki bundles a new version of it.\n\
         # Remove a plugin's table to have a deleted plugin reinstalled.\n\n{}",
        toml::to_string(record).context("unable to serialize the default plugins record")?
    );
    let tmp = plugins_dir.join(format!("{RECORD_FILE_NAME}.tmp"));
    std::fs::write(&tmp, text).with_context(|| format!("unable to write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("unable to write {}", path.display()))
}

/// Hashes the files in the plugin directory `dir`, skipping hidden files and
/// editor backups (`*~`) as the build script does. Anything that is neither
/// a regular file nor a directory, such as a symlink, gets an empty hash, so
/// it never matches a bundled file.
fn hash_dir(base: &Path, dir: &Path, depth: usize, hashes: &mut FileHashes) -> Result<()> {
    if depth > MAX_DEPTH {
        return Ok(());
    }
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("unable to read {}", dir.display()))?
    {
        let entry = entry.with_context(|| format!("unable to read {}", dir.display()))?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if file_name.starts_with('.') || file_name.ends_with('~') {
            continue;
        }

        let path = entry.path();
        let relative = slash_path(
            path.strip_prefix(base)
                .context("plugin file is outside its directory")?,
        );
        let file_type = entry
            .file_type()
            .with_context(|| format!("unable to read {}", path.display()))?;
        if file_type.is_dir() {
            hash_dir(base, &path, depth + 1, hashes)?;
        } else if file_type.is_file() {
            let contents = std::fs::read(&path)
                .with_context(|| format!("unable to read {}", path.display()))?;
            hashes.insert(relative, hash(&contents));
        } else {
            hashes.insert(relative, String::new());
        }
    }
    Ok(())
}

/// Writes `files` into a new directory `dir`, which must not exist.
fn write_plugin(dir: &Path, files: &[&BundledFile]) -> Result<()> {
    std::fs::create_dir(dir).with_context(|| format!("unable to create {}", dir.display()))?;
    for file in files {
        let path = dir.join(file.path_in_plugin());
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("unable to create {}", parent.display()))?;
        }
        std::fs::write(&path, &file.contents)
            .with_context(|| format!("unable to write {}", path.display()))?;
    }
    Ok(())
}

/// Removes `path` if it exists.
fn remove_dir_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("unable to remove {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// Replaces the plugin directory `dir` with `files`, staging the new version
/// next to it so the plugin is never left half written.
fn replace_plugin(plugins_dir: &Path, name: &str, files: &[&BundledFile]) -> Result<()> {
    let dir = plugins_dir.join(name);
    let staged = plugins_dir.join(format!(".{name}.new"));
    let old = plugins_dir.join(format!(".{name}.old"));
    remove_dir_if_exists(&staged)?;
    remove_dir_if_exists(&old)?;

    write_plugin(&staged, files)?;
    std::fs::rename(&dir, &old).with_context(|| format!("unable to move {}", dir.display()))?;
    std::fs::rename(&staged, &dir)
        .with_context(|| format!("unable to move {} into place", dir.display()))?;
    remove_dir_if_exists(&old)
}

/// Installs every plugin in [`DEFAULT_PLUGINS_TAR_ZST`] into `plugins_dir`,
/// creating it if needed, and updates the ones installed by an earlier
/// version of Kiki. `plugins_dir` is normally the system plugins directory,
/// [`crate::plugins::PluginSource::System`]'s directory inside the plugins
/// directory.
///
/// Which plugins were installed, and with which files, is kept in
/// [`RECORD_FILE_NAME`] in `plugins_dir`. For each bundled plugin:
///
/// - if its directory doesn't exist, it is installed, unless the record
///   shows it was installed before, in which case it was deleted on purpose
///   and stays deleted;
/// - if its directory matches the files the record says it was installed
///   with, it hasn't been edited since, and is replaced with the bundled
///   version;
/// - otherwise it has been edited, or is someone else's plugin of the same
///   name, and is left alone.
///
/// Config overrides are kept in the database, not the plugin's directory,
/// so an update keeps them.
///
/// Returns each bundled plugin's name with what was done with it.
///
/// # Examples
///
/// ```
/// use kiki_rss::plugins::defaults::{sync_default_plugins, SyncOutcome};
///
/// let dir = tempfile::TempDir::new().unwrap();
/// let outcomes = sync_default_plugins(dir.path()).unwrap();
/// assert_eq!(
///     outcomes,
///     [
///         ("adaptive-fetch".to_string(), SyncOutcome::Installed),
///         ("auto-tag".to_string(), SyncOutcome::Installed),
///         ("filter".to_string(), SyncOutcome::Installed),
///         ("privacy".to_string(), SyncOutcome::Installed),
///         ("retention".to_string(), SyncOutcome::Installed),
///         ("sanitize".to_string(), SyncOutcome::Installed),
///     ]
/// );
/// // Syncing again finds them up to date.
/// let outcomes = sync_default_plugins(dir.path()).unwrap();
/// assert!(outcomes.iter().all(|(_, o)| *o == SyncOutcome::UpToDate));
/// ```
///
/// # Errors
///
/// Returns an error if the bundled tarball or the record is malformed, or if
/// a directory or file cannot be read, created or written.
pub fn sync_default_plugins(plugins_dir: &Path) -> Result<Vec<(String, SyncOutcome)>> {
    std::fs::create_dir_all(plugins_dir)
        .with_context(|| format!("unable to create {}", plugins_dir.display()))?;

    // Read the whole archive first, so a malformed one installs nothing.
    let files = bundled_files()?;
    let mut plugins: BTreeMap<&str, Vec<&BundledFile>> = BTreeMap::new();
    for file in &files {
        plugins.entry(file.plugin.as_str()).or_default().push(file);
    }

    let mut record = read_record(plugins_dir)?;
    let mut outcomes = Vec::new();
    for (name, files) in plugins {
        let bundled: FileHashes = files
            .iter()
            .map(|f| (slash_path(f.path_in_plugin()), hash(&f.contents)))
            .collect();

        let dir = plugins_dir.join(name);
        let outcome = if dir.symlink_metadata().is_err() {
            if record.contains_key(name) {
                SyncOutcome::Removed
            } else {
                write_plugin(&dir, &files)?;
                SyncOutcome::Installed
            }
        } else if !dir.is_dir() {
            SyncOutcome::Modified
        } else {
            let mut current = FileHashes::new();
            hash_dir(&dir, &dir, 0, &mut current)?;
            if current == bundled {
                SyncOutcome::UpToDate
            } else if record.get(name) == Some(&current) {
                replace_plugin(plugins_dir, name, &files)?;
                SyncOutcome::Updated
            } else {
                SyncOutcome::Modified
            }
        };

        if matches!(
            outcome,
            SyncOutcome::Installed | SyncOutcome::Updated | SyncOutcome::UpToDate
        ) {
            record.insert(name.to_string(), bundled);
            // Record as we go, so an error on a later plugin doesn't lose
            // what was done with this one.
            write_record(plugins_dir, &record)?;
        }
        outcomes.push((name.to_string(), outcome));
    }
    Ok(outcomes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::plugins::{discover, PluginSource};
    use tempfile::TempDir;

    #[test]
    fn the_default_plugins_are_bundled() {
        let files = bundled_files().unwrap();
        let paths: Vec<_> = files.iter().map(|f| f.path.as_path()).collect();
        assert_eq!(
            paths,
            [
                Path::new("adaptive-fetch/main.lua"),
                Path::new("adaptive-fetch/manifest.toml"),
                Path::new("auto-tag/main.lua"),
                Path::new("auto-tag/manifest.toml"),
                Path::new("filter/manifest.toml"),
                Path::new("filter/plugin.wasm"),
                Path::new("privacy/main.lua"),
                Path::new("privacy/manifest.toml"),
                Path::new("retention/main.lua"),
                Path::new("retention/manifest.toml"),
                Path::new("sanitize/manifest.toml"),
                Path::new("sanitize/plugin.wasm"),
            ]
        );
        let plugins: Vec<_> = files.iter().map(|f| f.plugin.as_str()).collect();
        assert_eq!(
            plugins,
            [
                "adaptive-fetch",
                "adaptive-fetch",
                "auto-tag",
                "auto-tag",
                "filter",
                "filter",
                "privacy",
                "privacy",
                "retention",
                "retention",
                "sanitize",
                "sanitize"
            ]
        );
        assert_eq!(
            files[0].contents,
            include_bytes!("../../plugins/adaptive-fetch/main.lua")
        );
        assert_eq!(
            files[2].contents,
            include_bytes!("../../plugins/auto-tag/main.lua")
        );
        assert_eq!(
            files[5].contents,
            include_bytes!(concat!(env!("OUT_DIR"), "/plugins-wasm/filter.wasm"))
        );
        assert_eq!(
            files[6].contents,
            include_bytes!("../../plugins/privacy/main.lua")
        );
        assert_eq!(
            files[8].contents,
            include_bytes!("../../plugins/retention/main.lua")
        );
        assert_eq!(
            files[11].contents,
            include_bytes!(concat!(env!("OUT_DIR"), "/plugins-wasm/sanitize.wasm"))
        );
    }

    /// Syncs `dir`, returning the outcome for `filter`.
    fn sync_filter(dir: &Path) -> SyncOutcome {
        let outcomes = sync_default_plugins(dir).unwrap();
        let names: Vec<_> = outcomes.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "adaptive-fetch",
                "auto-tag",
                "filter",
                "privacy",
                "retention",
                "sanitize"
            ]
        );
        outcomes[2].1
    }

    /// Makes the recorded version of `filter` look older than the bundled
    /// one, as though it was installed by an earlier release, by editing
    /// both the plugin and its record the same way.
    fn install_older_filter(dir: &Path) -> std::path::PathBuf {
        assert_eq!(sync_filter(dir), SyncOutcome::Installed);
        let main = dir.join("filter").join("plugin.wasm");
        std::fs::write(&main, "-- an older release").unwrap();
        let mut record = read_record(dir).unwrap();
        record
            .get_mut("filter")
            .unwrap()
            .insert("plugin.wasm".into(), hash(b"-- an older release"));
        write_record(dir, &record).unwrap();
        main
    }

    #[test]
    fn installed_plugins_are_discovered() {
        let td = TempDir::new().unwrap();
        let plugins_dir = td.path().join("plugins");
        let outcomes = sync_default_plugins(&PluginSource::System.dir(&plugins_dir)).unwrap();
        let installed: Vec<_> = outcomes
            .iter()
            .inspect(|(_, outcome)| assert_eq!(*outcome, SyncOutcome::Installed))
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(
            installed,
            [
                "adaptive-fetch",
                "auto-tag",
                "filter",
                "privacy",
                "retention",
                "sanitize"
            ]
        );

        let discovery = discover(&plugins_dir).unwrap();
        assert!(discovery.errors.is_empty(), "{:?}", discovery.errors);
        for plugin in &discovery.plugins {
            assert_eq!(plugin.manifest.name, plugin.dir_name());
            assert_eq!(plugin.source, PluginSource::System);
        }
        let names: Vec<_> = discovery
            .plugins
            .iter()
            .map(|p| p.manifest.name.as_str())
            .collect();
        assert_eq!(names, installed);
    }

    #[test]
    fn unmodified_plugins_are_updated() {
        let td = TempDir::new().unwrap();
        let main = install_older_filter(td.path());
        // An extra file the older version shipped is removed with it.
        std::fs::write(td.path().join("filter").join("old.lua"), "").unwrap();
        let mut record = read_record(td.path()).unwrap();
        record
            .get_mut("filter")
            .unwrap()
            .insert("old.lua".into(), hash(b""));
        write_record(td.path(), &record).unwrap();

        assert_eq!(sync_filter(td.path()), SyncOutcome::Updated);
        assert_eq!(
            std::fs::read(&main).unwrap(),
            include_bytes!(concat!(env!("OUT_DIR"), "/plugins-wasm/filter.wasm"))
        );
        assert!(!td.path().join("filter").join("old.lua").exists());
        assert_eq!(sync_filter(td.path()), SyncOutcome::UpToDate);
        // No staging directories are left behind.
        let entries: Vec<_> = std::fs::read_dir(td.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        let mut entries = entries;
        entries.sort();
        assert_eq!(
            entries,
            [
                RECORD_FILE_NAME,
                "adaptive-fetch",
                "auto-tag",
                "filter",
                "privacy",
                "retention",
                "sanitize"
            ]
        );
    }

    /// Before 3.1.0, the filter was written in Lua. Installed and left as
    /// it was, it is replaced with the WebAssembly version, and its Lua
    /// source removed.
    #[test]
    fn the_lua_filter_is_replaced() {
        let td = TempDir::new().unwrap();
        assert_eq!(sync_filter(td.path()), SyncOutcome::Installed);
        let dir = td.path().join("filter");
        let lua = b"-- the filter, in Lua";
        let manifest = b"name = \"filter\"\nversion = \"3.0.0\"\nengine = \"lua\"\n";
        std::fs::remove_file(dir.join("plugin.wasm")).unwrap();
        std::fs::write(dir.join("main.lua"), lua).unwrap();
        std::fs::write(dir.join("manifest.toml"), manifest).unwrap();
        let mut record = read_record(td.path()).unwrap();
        let files = record.get_mut("filter").unwrap();
        files.clear();
        files.insert("main.lua".into(), hash(lua));
        files.insert("manifest.toml".into(), hash(manifest));
        write_record(td.path(), &record).unwrap();

        assert_eq!(sync_filter(td.path()), SyncOutcome::Updated);
        assert!(!dir.join("main.lua").exists());
        assert_eq!(
            std::fs::read(dir.join("plugin.wasm")).unwrap(),
            include_bytes!(concat!(env!("OUT_DIR"), "/plugins-wasm/filter.wasm"))
        );
        assert_eq!(
            std::fs::read(dir.join("manifest.toml")).unwrap(),
            include_bytes!("../../plugins/filter/manifest.toml")
        );
    }

    #[test]
    fn edited_plugins_are_not_updated() {
        let td = TempDir::new().unwrap();
        let main = install_older_filter(td.path());
        std::fs::write(&main, "-- edited").unwrap();

        assert_eq!(sync_filter(td.path()), SyncOutcome::Modified);
        assert_eq!(std::fs::read_to_string(&main).unwrap(), "-- edited");
    }

    #[test]
    fn existing_plugins_are_not_overwritten() {
        let td = TempDir::new().unwrap();
        let main = td.path().join("filter").join("main.lua");
        std::fs::create_dir(td.path().join("filter")).unwrap();
        std::fs::write(&main, "-- edited").unwrap();

        assert_eq!(sync_filter(td.path()), SyncOutcome::Modified);
        assert_eq!(std::fs::read_to_string(&main).unwrap(), "-- edited");
        assert!(!td.path().join("filter").join("manifest.toml").exists());
        assert!(!read_record(td.path()).unwrap().contains_key("filter"));
    }

    /// A copy of the bundled plugin made by hand, before Kiki recorded what
    /// it installed, is adopted, so it gets later updates.
    #[test]
    fn identical_unrecorded_plugins_are_adopted() {
        let td = TempDir::new().unwrap();
        assert_eq!(sync_filter(td.path()), SyncOutcome::Installed);
        std::fs::remove_file(td.path().join(RECORD_FILE_NAME)).unwrap();

        assert_eq!(sync_filter(td.path()), SyncOutcome::UpToDate);
        assert!(read_record(td.path()).unwrap().contains_key("filter"));
    }

    #[test]
    fn deleted_plugins_are_not_reinstalled() {
        let td = TempDir::new().unwrap();
        assert_eq!(sync_filter(td.path()), SyncOutcome::Installed);
        std::fs::remove_dir_all(td.path().join("filter")).unwrap();

        assert_eq!(sync_filter(td.path()), SyncOutcome::Removed);
        assert!(!td.path().join("filter").exists());
    }
}
