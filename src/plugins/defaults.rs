//! Plugins bundled with Kiki and installed by `kiki init`.
//!
//! The build script packs each default plugin's directory from `plugins/` in
//! Kiki's source into a zstd-compressed tarball, [`DEFAULT_PLUGINS_TAR_ZST`],
//! embedded in the binary. [`install_default_plugins`] unpacks it into a plugins directory,
//! where the plugins are discovered like any other plugin, so they can be
//! configured, disabled in their manifest, edited or deleted.
//!
//! Only built with the `default-plugins` feature.

use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// A zstd-compressed tarball of the plugins installed by default: currently
/// only `filter`.
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

/// Installs every plugin in [`DEFAULT_PLUGINS_TAR_ZST`] into `plugins_dir`,
/// creating it if needed.
///
/// A plugin whose directory already exists is left alone, so a plugin the
/// user has edited is never overwritten.
///
/// Returns the names of the plugins that were installed.
///
/// # Examples
///
/// ```
/// use kiki_rss::plugins::defaults::install_default_plugins;
///
/// let dir = tempfile::TempDir::new().unwrap();
/// assert_eq!(install_default_plugins(dir.path()).unwrap(), ["filter"]);
/// // Already installed plugins are skipped.
/// assert!(install_default_plugins(dir.path()).unwrap().is_empty());
/// ```
///
/// # Errors
///
/// Returns an error if the bundled tarball is malformed, or if a directory
/// or file cannot be created or written.
pub fn install_default_plugins(plugins_dir: &Path) -> Result<Vec<String>> {
    std::fs::create_dir_all(plugins_dir)
        .with_context(|| format!("unable to create {}", plugins_dir.display()))?;

    // Read the whole archive first, so a malformed one installs nothing.
    let files = bundled_files()?;

    let mut installed: Vec<String> = Vec::new();
    let mut skipped = HashSet::new();
    for file in files {
        if skipped.contains(&file.plugin) {
            continue;
        }
        if !installed.contains(&file.plugin) {
            let dir = plugins_dir.join(&file.plugin);
            if dir.symlink_metadata().is_ok() {
                skipped.insert(file.plugin);
                continue;
            }
            std::fs::create_dir(&dir)
                .with_context(|| format!("unable to create {}", dir.display()))?;
            installed.push(file.plugin.clone());
        }

        let path = plugins_dir.join(&file.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("unable to create {}", parent.display()))?;
        }
        std::fs::write(&path, &file.contents)
            .with_context(|| format!("unable to write {}", path.display()))?;
    }
    Ok(installed)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::plugins::discover;
    use tempfile::TempDir;

    #[test]
    fn the_filter_plugin_is_bundled() {
        let files = bundled_files().unwrap();
        let paths: Vec<_> = files.iter().map(|f| f.path.as_path()).collect();
        assert_eq!(
            paths,
            [
                Path::new("filter/main.lua"),
                Path::new("filter/manifest.toml")
            ]
        );
        assert!(files.iter().all(|f| f.plugin == "filter"));
        assert_eq!(
            files[0].contents,
            include_bytes!("../../plugins/filter/main.lua")
        );
    }

    #[test]
    fn installed_plugins_are_discovered() {
        let td = TempDir::new().unwrap();
        let plugins_dir = td.path().join("plugins");
        let installed = install_default_plugins(&plugins_dir).unwrap();
        assert_eq!(installed, ["filter"]);

        let discovery = discover(&plugins_dir).unwrap();
        assert!(discovery.errors.is_empty(), "{:?}", discovery.errors);
        for plugin in &discovery.plugins {
            assert_eq!(plugin.manifest.name, plugin.dir_name());
        }
        let names: Vec<_> = discovery
            .plugins
            .iter()
            .map(|p| p.manifest.name.as_str())
            .collect();
        assert_eq!(names, installed);
    }

    #[test]
    fn existing_plugins_are_not_overwritten() {
        let td = TempDir::new().unwrap();
        let main = td.path().join("filter").join("main.lua");
        std::fs::create_dir(td.path().join("filter")).unwrap();
        std::fs::write(&main, "-- edited").unwrap();

        assert!(install_default_plugins(td.path()).unwrap().is_empty());
        assert_eq!(std::fs::read_to_string(&main).unwrap(), "-- edited");
        assert!(!td.path().join("filter").join("manifest.toml").exists());
    }
}
