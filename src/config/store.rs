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

/// Holds the current [`Settings`] and writes the config file.
///
/// Reads are lock-free: [`ConfigStore::current`] hands out an
/// `Arc<Settings>` snapshot, so a feed refresh sees one consistent set of
/// settings even if they change mid-fetch. Writes and reloads are
/// serialized by an internal lock, and writes also by a lock on a file
/// beside the config file, `.kiki.toml.lock` for `kiki.toml`, so that
/// every process saving through a `ConfigStore` — the server, and `kiki
/// config set` — waits for the others rather than overwriting their
/// changes.
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
        self.update_reporting(edit).map(|(settings, _)| settings)
    }

    /// Like [`ConfigStore::update`], but also returns whether `edit`
    /// changed the overrides. When it did not, the file is not rewritten.
    ///
    /// # Errors
    ///
    /// As for [`ConfigStore::update`].
    ///
    /// # Examples
    ///
    /// ```
    /// use kiki_rss::config::ConfigStore;
    ///
    /// let dir = tempfile::TempDir::new().unwrap();
    /// let store = ConfigStore::open(dir.path().join("kiki.toml")).unwrap();
    /// let set = |o: &mut kiki_rss::config::Overrides| o.set("asset_cache", "enabled", false);
    /// let (settings, changed) = store.update_reporting(set).unwrap();
    /// assert!(!settings.asset_cache.enabled && changed);
    /// let (_, changed) = store.update_reporting(set).unwrap();
    /// assert!(!changed);
    /// ```
    pub fn update_reporting<F>(&self, edit: F) -> Result<(Arc<Settings>, bool), ConfigError>
    where
        F: FnOnce(&mut Overrides) -> Result<(), ConfigError>,
    {
        let _guard = self.lock();
        // Held until the new file is in place, so that no other process
        // saves between the read below and the write.
        let _file_lock = lock_file(&self.path)?;
        let before = read_overrides(&self.path)?;
        if let Err(ConfigError::Invalid(reason)) = before.resolve() {
            return Err(ConfigError::InvalidFile {
                path: self.path.clone(),
                reason,
            });
        }
        let mut overrides = before.clone();
        edit(&mut overrides)?;
        let settings = Arc::new(overrides.resolve()?);

        let changed = overrides != before;
        if changed {
            let body = overrides.to_toml_string()?;
            write_atomically(&self.path, &format!("{FILE_HEADER}{body}")).map_err(|source| {
                ConfigError::Io {
                    path: self.path.clone(),
                    source,
                }
            })?;
        }

        self.current.store(settings.clone());
        Ok((settings, changed))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        // The guarded data is `()`, so a panic while holding the lock
        // cannot have left anything half-updated.
        self.write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The file whose lock serializes saves of the config file at `path`
/// across processes: `.kiki.toml.lock` beside `kiki.toml`.
///
/// It cannot be the config file itself, since every save renames a new
/// file over it: a process waiting on the old file's lock would get it
/// once the save is done, and then lock a file no longer in place.
fn lock_path(path: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(path.file_name().unwrap_or(path.as_os_str()));
    name.push(".lock");
    path.with_file_name(name)
}

/// Takes the lock on [`lock_path`] for the config file at `path`, waiting
/// for any other process holding it, and creating the lock file if it is
/// missing. The lock is released when the returned file is closed.
fn lock_file(path: &Path) -> Result<fs::File, ConfigError> {
    let lock_path = lock_path(path);
    let io_error = |source| ConfigError::Io {
        path: lock_path.clone(),
        source,
    };
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(io_error)?;
    file.lock().map_err(io_error)?;
    Ok(file)
}

/// Reads and parses the overrides at `path`; a missing file is empty.
///
/// # Errors
///
/// Returns an error if the file cannot be read or is not valid TOML.
pub(crate) fn read_overrides(path: &Path) -> Result<Overrides, ConfigError> {
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

/// The mode of a config file created by a save: read and write for its
/// owner and group, and nothing for anyone else.
#[cfg(unix)]
const NEW_FILE_MODE: u32 = 0o660;

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
        // A new file gets what Kiki's umask (see `main`) leaves of 0o666:
        // the mode is set explicitly below, which the umask does not
        // touch, and the file can hold a proxy password.
        #[cfg(unix)]
        let mode = existing
            .as_ref()
            .map_or(NEW_FILE_MODE, |p| p.mode() & 0o777);
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
        fs::write(store.path(), "[asset_cache]\nenabled = false\n").unwrap();

        store
            .update(|o| o.set("feed_fetch", "max_feed_bytes", 4096u64))
            .unwrap();
        let s = store.current();
        assert!(!s.asset_cache.enabled);
        assert_eq!(s.feed_fetch.max_feed_bytes, 4096);
    }

    #[test]
    fn update_refuses_to_overwrite_a_file_with_an_invalid_setting() {
        let (_td, store) = store();
        let bad = "[asset_cache]\nmax_bytes = -1\n";
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
    fn an_update_that_changes_nothing_leaves_the_file_alone() {
        let (_td, store) = store();
        let text = "# tuned for the slow mirror\n[feed_fetch]\ntimeout_seconds = 30\n";
        fs::write(store.path(), text).unwrap();

        let (settings, changed) = store
            .update_reporting(|o| o.set("feed_fetch", "timeout_seconds", 30u64))
            .unwrap();
        assert!(!changed);
        assert_eq!(settings.feed_fetch.timeout_seconds, 30);
        assert_eq!(fs::read_to_string(store.path()).unwrap(), text);

        let (_, changed) = store
            .update_reporting(|o| o.set("feed_fetch", "timeout_seconds", 31u64))
            .unwrap();
        assert!(changed);
    }

    /// Increments `feed_fetch.timeout_seconds`, reading it from the
    /// overrides the update sees.
    fn increment(o: &mut Overrides) -> Result<(), ConfigError> {
        let n = o
            .get("feed_fetch", "timeout_seconds")
            .and_then(toml::Value::as_integer)
            .unwrap_or(0);
        o.set("feed_fetch", "timeout_seconds", n + 1)
    }

    /// Two stores on one file stand for two processes, such as the server
    /// and `kiki config set`: their in-memory locks are separate, so only
    /// the file lock keeps one from saving over the other's change.
    #[test]
    fn stores_on_the_same_file_never_lose_each_others_updates() {
        let (_td, store) = store();
        let other = ConfigStore::open(store.path()).unwrap();
        const ROUNDS: i64 = 50;

        std::thread::scope(|s| {
            for store in [&store, &other] {
                s.spawn(move || {
                    for _ in 0..ROUNDS {
                        store.update(increment).unwrap();
                    }
                });
            }
        });

        let reopened = ConfigStore::open(store.path()).unwrap();
        assert_eq!(
            reopened.current().feed_fetch.timeout_seconds,
            2 * ROUNDS as u64
        );
    }

    #[test]
    fn an_update_waits_for_another_process_holding_the_lock() {
        let (_td, store) = store();
        let lock = lock_file(store.path()).unwrap();

        std::thread::scope(|s| {
            let (tx, rx) = std::sync::mpsc::channel();
            s.spawn(|| {
                let tx = tx;
                store.update(increment).unwrap();
                tx.send(()).unwrap();
            });
            assert!(
                rx.recv_timeout(std::time::Duration::from_millis(200))
                    .is_err(),
                "the update must wait for the lock"
            );

            // The other process saves while it holds the lock...
            fs::write(store.path(), "[feed_fetch]\ntimeout_seconds = 10\n").unwrap();
            drop(lock);
            rx.recv().unwrap();
        });

        // ...and the update builds on that save rather than replacing it.
        assert_eq!(store.current().feed_fetch.timeout_seconds, 11);
    }

    #[test]
    fn the_lock_file_sits_beside_the_config_file() {
        assert_eq!(
            lock_path(Path::new("/var/lib/kiki/kiki.toml")),
            Path::new("/var/lib/kiki/.kiki.toml.lock")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_new_file_is_not_accessible_to_other_users() {
        let (_td, store) = store();
        store
            .update(|o| o.set("asset_cache", "enabled", false))
            .unwrap();
        let mode = fs::metadata(store.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o660);
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
