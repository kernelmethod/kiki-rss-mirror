//! The server's shared, reloadable view of the config file.
use super::{ConfigError, Overrides, Settings};
use arc_swap::ArcSwap;
use std::fs;
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Shared handle to a [`ConfigStore`].
pub type ConfigHandle = Arc<ConfigStore>;

/// Written at the top of the config file on every save.
const FILE_HEADER: &str = "# Kiki settings. Keys here override the built-in defaults.\n\
                           # This file is rewritten by Kiki whenever settings change.\n\n";

/// Holds the current [`Settings`] and is the only writer of the config file.
///
/// Reads are lock-free: [`ConfigStore::current`] hands out an
/// `Arc<Settings>` snapshot, so a feed refresh sees one consistent set of
/// settings even if they change mid-fetch. Writes and reloads are
/// serialized by an internal lock.
pub struct ConfigStore {
    path: PathBuf,
    current: ArcSwap<Settings>,
    write_lock: Mutex<()>,
}

impl ConfigStore {
    /// Loads the config file at `path`. A missing file means "no
    /// overrides", i.e. the built-in defaults.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read, is not
    /// valid TOML, or holds an invalid setting.
    ///
    /// # Examples
    ///
    /// ```
    /// use kiki_rss::config::{ConfigStore, Settings};
    ///
    /// let dir = std::env::temp_dir().join("kiki-config-doctest");
    /// std::fs::create_dir_all(&dir).unwrap();
    /// let store = ConfigStore::open(dir.join("no-such-file.toml")).unwrap();
    /// assert_eq!(*store.current(), Settings::default());
    /// ```
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, ConfigError> {
        let path = path.into();
        let settings = read_overrides(&path)?.resolve()?;
        Ok(ConfigStore {
            path,
            current: ArcSwap::from_pointee(settings),
            write_lock: Mutex::new(()),
        })
    }

    /// Path of the config file this store reads and writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns a snapshot of the current settings.
    pub fn current(&self) -> Arc<Settings> {
        self.current.load_full()
    }

    /// Reads the overrides currently on disk.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or parsed.
    pub fn overrides(&self) -> Result<Overrides, ConfigError> {
        read_overrides(&self.path)
    }

    /// Re-reads the config file and, if it is valid, makes it current.
    ///
    /// Returns whether the effective settings changed. On error the
    /// previous settings stay in force.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read, is not valid TOML, or
    /// holds an invalid setting.
    pub fn reload(&self) -> Result<bool, ConfigError> {
        let _guard = self.lock();
        let settings = read_overrides(&self.path)?.resolve()?;
        let changed = **self.current.load() != settings;
        if changed {
            self.current.store(Arc::new(settings));
        }
        Ok(changed)
    }

    /// Applies `edit` to the overrides on disk, validates the result, saves
    /// it, and makes it current. Returns the new settings.
    ///
    /// The file is re-read first rather than rebuilt from memory, so a
    /// change made to it outside the server is kept. The save replaces the
    /// file atomically: a crash leaves either the old file or the new one.
    ///
    /// # Errors
    ///
    /// Returns an error — and leaves the file and the current settings
    /// untouched — if the file on disk cannot be read or parsed
    /// ([`ConfigError::Parse`]) or already holds an invalid setting
    /// ([`ConfigError::InvalidFile`]), if `edit` fails, if the result is
    /// invalid ([`ConfigError::Invalid`]), or if the save fails.
    ///
    /// Refusing to touch an invalid file means an operator's broken edit is
    /// never overwritten, but also that every update fails until it is
    /// fixed.
    pub fn update<F>(&self, edit: F) -> Result<Arc<Settings>, ConfigError>
    where
        F: FnOnce(&mut Overrides) -> Result<(), ConfigError>,
    {
        let _guard = self.lock();
        let mut overrides = read_overrides(&self.path)?;
        if let Err(ConfigError::Invalid(reason)) = overrides.resolve() {
            return Err(ConfigError::InvalidFile {
                path: self.path.clone(),
                reason,
            });
        }
        edit(&mut overrides)?;
        let settings = Arc::new(overrides.resolve()?);

        let body = overrides.to_toml_string()?;
        write_atomically(&self.path, &format!("{FILE_HEADER}{body}")).map_err(|source| {
            ConfigError::Io {
                path: self.path.clone(),
                source,
            }
        })?;

        self.current.store(settings.clone());
        Ok(settings)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        // The guarded data is `()`, so a panic while holding the lock
        // cannot have left anything half-updated.
        self.write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Reads and parses the overrides at `path`; a missing file is empty.
fn read_overrides(path: &Path) -> Result<Overrides, ConfigError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Overrides::default()),
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    Overrides::parse(&text).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

/// Replaces the file at `path` with `contents` via a temporary file in the
/// same directory and a `rename(2)`, so readers never see a partial file.
///
/// The replacement keeps the permissions of the file it replaces.
fn write_atomically(path: &Path, contents: &str) -> io::Result<()> {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "config path has no file name")
    })?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(".tmp");
    let tmp_path = dir.join(tmp_name);

    let existing = fs::metadata(path).ok().map(|m| m.permissions());

    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        // Create the temporary file no more permissive than the original,
        // so the new contents are never more widely readable, even briefly.
        // The umask may strip bits from `mode`; they are restored below.
        #[cfg(unix)]
        let mode = existing.as_ref().map_or(0o666, |p| p.mode() & 0o777);
        #[cfg(unix)]
        options.mode(mode);
        let mut file = options.open(&tmp_path)?;
        // A leftover temporary file from a crash keeps its old mode, so
        // set it explicitly.
        #[cfg(unix)]
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.write_all(contents.as_bytes())?;
        if let Some(perms) = existing {
            file.set_permissions(perms)?;
        }
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp_path, path)?;
        // Persist the rename itself. Windows cannot open a directory as a
        // file, and journals the rename's metadata on its own.
        #[cfg(unix)]
        fs::File::open(dir)?.sync_all()?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store() -> (TempDir, ConfigStore) {
        let td = TempDir::with_prefix("kiki_config").unwrap();
        let store = ConfigStore::open(td.path().join("kiki.toml")).unwrap();
        (td, store)
    }

    #[test]
    fn missing_file_means_defaults() {
        let (_td, store) = store();
        assert_eq!(*store.current(), Settings::default());
        assert!(!store.path().exists(), "opening must not create the file");
    }

    #[test]
    fn update_saves_and_applies() {
        let (_td, store) = store();
        let s = store
            .update(|o| o.set("feed_fetch", "max_feed_bytes", 4096u64))
            .unwrap();
        assert_eq!(s.feed_fetch.max_feed_bytes, 4096);
        assert_eq!(store.current().feed_fetch.max_feed_bytes, 4096);

        // Durable: a fresh store sees it too.
        let reopened = ConfigStore::open(store.path()).unwrap();
        assert_eq!(reopened.current().feed_fetch.max_feed_bytes, 4096);
    }

    #[test]
    fn only_overridden_keys_are_written() {
        let (_td, store) = store();
        store
            .update(|o| o.set("asset_cache", "enabled", false))
            .unwrap();
        let text = fs::read_to_string(store.path()).unwrap();
        assert!(text.contains("enabled = false"));
        assert!(!text.contains("max_feed_bytes"), "{text}");
        assert!(!text.contains("feed_fetch"), "{text}");
    }

    #[test]
    fn invalid_update_changes_nothing() {
        let (_td, store) = store();
        store
            .update(|o| o.set("feed_fetch", "max_feed_bytes", 4096u64))
            .unwrap();
        let before = fs::read_to_string(store.path()).unwrap();

        let err = store
            .update(|o| o.set("feed_fetch", "max_feed_bytes", 0u64))
            .unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(_)));
        assert_eq!(fs::read_to_string(store.path()).unwrap(), before);
        assert_eq!(store.current().feed_fetch.max_feed_bytes, 4096);
    }

    #[test]
    fn update_keeps_changes_made_on_disk() {
        let (_td, store) = store();
        fs::write(store.path(), "[retention]\nmax_age_days = 7\n").unwrap();

        store
            .update(|o| o.set("feed_fetch", "max_feed_bytes", 4096u64))
            .unwrap();
        let s = store.current();
        assert_eq!(s.retention.max_age_days, Some(7));
        assert_eq!(s.feed_fetch.max_feed_bytes, 4096);
    }

    #[test]
    fn update_refuses_to_overwrite_a_file_with_an_invalid_setting() {
        let (_td, store) = store();
        let bad = "[retention]\nmax_age_days = 0\n";
        fs::write(store.path(), bad).unwrap();
        let err = store
            .update(|o| o.set("asset_cache", "enabled", false))
            .unwrap_err();
        assert!(matches!(err, ConfigError::InvalidFile { .. }), "{err:?}");
        assert_eq!(fs::read_to_string(store.path()).unwrap(), bad);
    }

    #[test]
    fn update_refuses_to_overwrite_an_unparseable_file() {
        let (_td, store) = store();
        fs::write(store.path(), "[feed_fetch\n").unwrap();
        let err = store
            .update(|o| o.set("asset_cache", "enabled", false))
            .unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
        assert_eq!(fs::read_to_string(store.path()).unwrap(), "[feed_fetch\n");
    }

    #[test]
    fn reload_picks_up_valid_edits_and_ignores_invalid_ones() {
        let (_td, store) = store();

        fs::write(store.path(), "[feed_fetch]\ntimeout_seconds = 5\n").unwrap();
        assert!(store.reload().unwrap());
        assert_eq!(store.current().feed_fetch.timeout_seconds, 5);
        assert!(!store.reload().unwrap(), "no change the second time");

        fs::write(store.path(), "[feed_fetch]\ntimeout_seconds = 0\n").unwrap();
        assert!(store.reload().is_err());
        assert_eq!(
            store.current().feed_fetch.timeout_seconds,
            5,
            "last good settings stay in force"
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_preserves_file_permissions() {
        let (_td, store) = store();
        fs::write(store.path(), "").unwrap();
        fs::set_permissions(store.path(), fs::Permissions::from_mode(0o640)).unwrap();

        store
            .update(|o| o.set("asset_cache", "enabled", false))
            .unwrap();
        let mode = fs::metadata(store.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o640);
    }

    #[test]
    fn open_rejects_an_invalid_file() {
        let td = TempDir::with_prefix("kiki_config").unwrap();
        let path = td.path().join("kiki.toml");
        fs::write(&path, "[feed_fetch]\nbogus = 1\n").unwrap();
        assert!(matches!(
            ConfigStore::open(&path),
            Err(ConfigError::Invalid(_))
        ));
    }
}
