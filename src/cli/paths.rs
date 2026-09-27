//! Resolution of the filesystem locations Kiki uses.
//!
//! Every subcommand resolves its paths through this module, so `kiki init`,
//! `kiki serve`, `kiki migrate`, and `kiki systemd` all agree on where Kiki
//! lives without having to be told twice.
//!
//! Kiki keeps two kinds of state, and they belong in two different places:
//!
//! * **Persistent state** — the SQLite database, the settings file
//!   ([`crate::config::CONFIG_FILE_NAME`]), and the cached asset tree —
//!   lives in a *data directory*, defaulting to the platform's per-user data
//!   location (`$XDG_DATA_HOME/kiki` on Linux, `~/Library/Application
//!   Support/kiki` on macOS). This is the directory [`kiki init`] creates.
//! * **Runtime state** — the Unix domain socket the API is served on — lives
//!   in the per-user *runtime directory* (`$XDG_RUNTIME_DIR/kiki`, i.e.
//!   `/run/user/$UID/kiki`, on Linux). The XDG base directory specification
//!   names sockets specifically as what that directory is for: it is a tmpfs
//!   cleared on logout, is owned by a single user with mode `0700`, and is
//!   never a network mount. A default socket path there is therefore unique
//!   to one user, unreachable by others, and never leaves a stale file behind
//!   across a reboot.
//!
//! Each has an environment variable that names it outright:
//!
//! * `$KIKI_HOME` *is* Kiki's home directory. When set, the database lives
//!   directly inside it, for every subcommand, in preference to the platform
//!   data directory.
//! * `$KIKI_RUNTIME_DIR` *is* Kiki's runtime directory. When set, the socket
//!   lives directly inside it, in preference to `$XDG_RUNTIME_DIR/kiki`. No
//!   `kiki/` subdirectory is appended — the variable names Kiki's own
//!   directory, where `$XDG_RUNTIME_DIR` names one shared with every other
//!   application on the system.
//!
//! Neither is required, and a directory Kiki was pointed at keeps the socket
//! too: with `$KIKI_HOME` set — or when serving from a directory that
//! already holds a database — the socket sits with the data it serves unless
//! `$KIKI_RUNTIME_DIR` says otherwise. So `$KIKI_HOME` alone still moves the
//! whole instance to one place, and `$KIKI_RUNTIME_DIR` alone still splits
//! the socket back out. The socket can also be pinned to an exact path with
//! `--uds` or `$KIKI_SOCKET`, which beat both.
//!
//! [`kiki init`]: crate::cli::init

use anyhow::{bail, Context, Result};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Environment variable naming Kiki's home directory. When set, both the
/// database and the Unix socket default to living directly inside it.
pub const KIKI_HOME_ENV: &str = "KIKI_HOME";

/// Environment variable naming Kiki's runtime directory. When set, the Unix
/// socket defaults to living directly inside it.
pub const KIKI_RUNTIME_DIR_ENV: &str = "KIKI_RUNTIME_DIR";

/// Environment variable pinning the Unix socket path.
pub const KIKI_SOCKET_ENV: &str = "KIKI_SOCKET";

/// Name of the SQLite database file inside the data directory.
pub const DB_FILE_NAME: &str = "kiki.db";

/// Name of the Unix domain socket file.
pub const SOCKET_FILE_NAME: &str = "kiki.sock";

/// Subdirectory created inside the platform runtime directory.
///
/// `$XDG_RUNTIME_DIR` is shared with every other application, so Kiki takes
/// a subdirectory of its own. `$KIKI_RUNTIME_DIR` names Kiki's directory
/// directly and gets no subdirectory appended.
pub const RUNTIME_SUBDIR: &str = "kiki";

/// Subdirectory created inside the platform data directory.
pub const DATA_SUBDIR: &str = "kiki";

/// Maximum length, in bytes, of a Unix domain socket path.
///
/// A socket address is a `sockaddr_un`, whose `sun_path` field is a fixed
/// 108-byte buffer on Linux and 104 bytes on the BSDs and macOS. The path
/// must be NUL-terminated within it, leaving one byte less than the buffer
/// size. Exceeding this fails at `bind(2)` time with a singularly unhelpful
/// `EINVAL`, so [`validate_socket_path`] checks it up front instead.
#[cfg(target_os = "linux")]
pub const MAX_SOCKET_PATH_LEN: usize = 107;
#[cfg(not(target_os = "linux"))]
pub const MAX_SOCKET_PATH_LEN: usize = 103;

/// The set of ambient locations that path resolution consults.
///
/// Keeping these in a struct rather than reading the process environment
/// inline makes [`resolve_data_dir`] and [`resolve_socket_path`] pure
/// functions of their inputs, which is what lets them be tested without
/// mutating process-global state.
#[derive(Debug, Default, Clone)]
pub struct Env {
    /// Value of `$KIKI_HOME`, if set and non-empty.
    pub kiki_home: Option<PathBuf>,

    /// Value of `$KIKI_RUNTIME_DIR`, if set and non-empty. This is the bare
    /// variable; use [`default_runtime_dir`] to get it with the platform
    /// runtime directory as a fallback.
    pub kiki_runtime_dir: Option<PathBuf>,

    /// Value of `$KIKI_SOCKET`, if set and non-empty.
    pub kiki_socket: Option<PathBuf>,

    /// The platform runtime directory, if one exists and is usable. See
    /// [`runtime_dir_is_usable`]. This is the bare platform location, with
    /// no `kiki/` subdirectory applied.
    pub runtime_dir: Option<PathBuf>,

    /// The platform per-user data directory for Kiki, e.g.
    /// `~/.local/share/kiki`. This is the bare platform location; use
    /// [`default_data_dir`] to get it with `$KIKI_HOME` applied.
    pub platform_data_dir: Option<PathBuf>,

    /// The process's current working directory.
    pub current_dir: Option<PathBuf>,
}

impl Env {
    /// Build an [`Env`] from the current process environment.
    ///
    /// The *platform* runtime directory is validated here rather than at
    /// resolution time, so a hostile or misconfigured `$XDG_RUNTIME_DIR` is
    /// simply absent from the resulting `Env` and resolution falls through
    /// to the data directory. `$KIKI_RUNTIME_DIR` is not validated: like
    /// `$KIKI_HOME` and `$KIKI_SOCKET` it is an instruction, and silently
    /// ignoring it would put the socket somewhere the operator did not ask
    /// for. A directory Kiki has to create for it is made `0700`; see
    /// [`crate::cli::serve`].
    pub fn from_process() -> Self {
        let runtime_dir = dirs::runtime_dir().filter(|d| {
            let usable = runtime_dir_is_usable(d);
            if !usable {
                tracing::warn!(
                    path = %d.display(),
                    "ignoring runtime directory: it is not a directory owned by \
                     this user with mode 0700"
                );
            }
            usable
        });

        Self {
            kiki_home: non_empty_var(KIKI_HOME_ENV),
            kiki_runtime_dir: non_empty_var(KIKI_RUNTIME_DIR_ENV),
            kiki_socket: non_empty_var(KIKI_SOCKET_ENV),
            runtime_dir,
            platform_data_dir: dirs::data_dir().map(|d| d.join(DATA_SUBDIR)),
            current_dir: std::env::current_dir().ok(),
        }
    }
}

/// Read an environment variable, treating an unset variable and one set to
/// the empty string alike.
fn non_empty_var(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Report whether `dir` is safe to place a socket in.
///
/// The XDG base directory specification requires the runtime directory to be
/// owned by the user with an access mode of `0700`; anything else means some
/// other user could reach — or have planted — the socket. A symlink is
/// rejected outright rather than followed.
pub fn runtime_dir_is_usable(dir: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(dir) else {
        return false;
    };

    // SAFETY: geteuid is always safe to call and has no failure modes.
    let euid = unsafe { libc::geteuid() };

    metadata.is_dir() && metadata.uid() == euid && metadata.mode() & 0o777 == 0o700
}

/// The directory Kiki treats as its home when nothing more specific says
/// otherwise: `$KIKI_HOME` if set, else the platform per-user data directory
/// (`~/.local/share/kiki` and friends).
///
/// This is the location `kiki init` creates and `kiki systemd` installs a
/// unit against. It deliberately does *not* consider the current directory —
/// those commands name a well-known location, and picking one up from
/// wherever the shell happens to be sitting would be a surprise.
///
/// # Errors
///
/// Returns an error if `$KIKI_HOME` is unset and the platform data directory
/// cannot be determined, which in practice means `$HOME` is unset too.
///
/// # Examples
///
/// ```
/// use kiki_rss::cli::paths::{default_data_dir, Env};
/// use std::path::{Path, PathBuf};
///
/// let env = Env {
///     kiki_home: Some(PathBuf::from("/srv/kiki")),
///     platform_data_dir: Some(PathBuf::from("/home/rey/.local/share/kiki")),
///     ..Env::default()
/// };
/// assert_eq!(default_data_dir(&env)?, Path::new("/srv/kiki"));
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn default_data_dir(env: &Env) -> Result<PathBuf> {
    if let Some(home) = &env.kiki_home {
        return Ok(home.clone());
    }

    env.platform_data_dir.clone().context(
        "unable to determine where Kiki should live: $KIKI_HOME is unset and the \
         platform data directory could not be determined",
    )
}

/// The directory Kiki puts its socket in when nothing more specific says
/// otherwise: `$KIKI_RUNTIME_DIR` if set, else `$XDG_RUNTIME_DIR/kiki`.
///
/// Returns `None` when neither is available — no `$KIKI_RUNTIME_DIR`, and no
/// usable platform runtime directory (a non-Linux platform, or a login
/// session without one). [`resolve_socket_path`] then falls back to the data
/// directory, which always exists.
///
/// # Examples
///
/// ```
/// use kiki_rss::cli::paths::{default_runtime_dir, Env};
/// use std::path::{Path, PathBuf};
///
/// // $KIKI_RUNTIME_DIR is Kiki's runtime directory as given, with no
/// // `kiki/` subdirectory appended...
/// let env = Env {
///     kiki_runtime_dir: Some(PathBuf::from("/run/kiki")),
///     runtime_dir: Some(PathBuf::from("/run/user/1000")),
///     ..Env::default()
/// };
/// assert_eq!(default_runtime_dir(&env), Some(PathBuf::from("/run/kiki")));
///
/// // ...where $XDG_RUNTIME_DIR is shared, so Kiki takes a subdirectory.
/// let env = Env {
///     runtime_dir: Some(PathBuf::from("/run/user/1000")),
///     ..Env::default()
/// };
/// assert_eq!(
///     default_runtime_dir(&env),
///     Some(PathBuf::from("/run/user/1000/kiki")),
/// );
/// ```
pub fn default_runtime_dir(env: &Env) -> Option<PathBuf> {
    if let Some(dir) = &env.kiki_runtime_dir {
        return Some(dir.clone());
    }

    env.runtime_dir.as_ref().map(|d| d.join(RUNTIME_SUBDIR))
}

/// Where a resolved data directory came from.
///
/// The socket's default location depends on this: a directory somebody
/// *named* holds the socket too, while the platform default leaves the
/// socket to the runtime directory. See [`resolve_socket_path`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataDirSource {
    /// Named by `$KIKI_HOME`.
    KikiHome,

    /// The current directory, which already contains a database.
    CurrentDir,

    /// The platform per-user data directory — nobody named anything.
    Platform,
}

/// A resolved data directory, and the reason it was chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDir {
    /// The directory holding `kiki.db` and the cached asset tree.
    pub path: PathBuf,

    /// How [`resolve_data_dir`] arrived at `path`.
    pub source: DataDirSource,
}

impl DataDir {
    /// Whether this location was named by the user rather than fallen back
    /// to.
    fn is_named(&self) -> bool {
        !matches!(self.source, DataDirSource::Platform)
    }
}

/// Resolve the directory holding Kiki's database and cached assets.
///
/// In precedence order:
///
/// 1. `$KIKI_HOME`.
/// 2. the current directory, if it already contains a `kiki.db`. A database
///    sitting right there is as clear a statement of intent as `$KIKI_HOME`,
///    so `cd` into it and `kiki serve` finds it.
/// 3. [`default_data_dir`], i.e. the platform per-user data directory.
///
/// # Errors
///
/// Returns an error if every candidate is exhausted, which happens only when
/// `$KIKI_HOME` is unset, no database sits in the current directory, and the
/// platform data directory cannot be determined.
///
/// # Examples
///
/// ```
/// use kiki_rss::cli::paths::{resolve_data_dir, DataDirSource, Env};
/// use std::path::{Path, PathBuf};
///
/// let env = Env {
///     kiki_home: Some(PathBuf::from("/srv/kiki")),
///     ..Env::default()
/// };
/// let data_dir = resolve_data_dir(&env)?;
/// assert_eq!(data_dir.path, Path::new("/srv/kiki"));
/// assert_eq!(data_dir.source, DataDirSource::KikiHome);
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn resolve_data_dir(env: &Env) -> Result<DataDir> {
    if let Some(home) = &env.kiki_home {
        return Ok(DataDir {
            path: home.clone(),
            source: DataDirSource::KikiHome,
        });
    }

    if let Some(cwd) = &env.current_dir {
        if cwd.join(DB_FILE_NAME).exists() {
            return Ok(DataDir {
                path: cwd.clone(),
                source: DataDirSource::CurrentDir,
            });
        }
    }

    // $KIKI_HOME is already ruled out above, so this is the platform
    // directory or nothing.
    let path = default_data_dir(env).context(
        "unable to determine a data directory: $KIKI_HOME is unset and there is no \
         kiki.db in the current directory",
    )?;

    Ok(DataDir {
        path,
        source: DataDirSource::Platform,
    })
}

/// Resolve the path of the Unix domain socket the server listens on.
///
/// In precedence order:
///
/// 1. `explicit` — the `--uds` flag.
/// 2. `$KIKI_SOCKET`.
/// 3. `$KIKI_RUNTIME_DIR/kiki.sock`. Naming the runtime directory is a
///    statement about the socket specifically, so it beats the data
///    directory rule below — `$KIKI_HOME` being set as well does not take
///    the socket back.
/// 4. `<data_dir>/kiki.sock`, when the data directory was *named* — by
///    `$KIKI_HOME`, or by running from a directory that already holds a
///    database. Pointing Kiki at a directory points all of it there, so the
///    socket sits with the data it serves.
/// 5. `$XDG_RUNTIME_DIR/kiki/kiki.sock`, when nobody named a directory. This
///    is the preferred default: see the module docs for why the runtime
///    directory is the right home for a socket.
/// 6. `<data_dir>/kiki.sock`, for platforms and sessions with no runtime
///    directory.
///
/// # Examples
///
/// ```
/// use kiki_rss::cli::paths::{resolve_socket_path, DataDir, DataDirSource, Env};
/// use std::path::{Path, PathBuf};
///
/// let env = Env {
///     runtime_dir: Some(PathBuf::from("/run/user/1000")),
///     ..Env::default()
/// };
///
/// // Nobody named a directory, so the socket goes to the runtime directory.
/// let platform = DataDir {
///     path: PathBuf::from("/home/rey/.local/share/kiki"),
///     source: DataDirSource::Platform,
/// };
/// assert_eq!(
///     resolve_socket_path(None, &platform, &env),
///     Path::new("/run/user/1000/kiki/kiki.sock"),
/// );
///
/// // $KIKI_HOME named one, so the socket follows it.
/// let home = DataDir {
///     path: PathBuf::from("/srv/kiki"),
///     source: DataDirSource::KikiHome,
/// };
/// assert_eq!(
///     resolve_socket_path(None, &home, &env),
///     Path::new("/srv/kiki/kiki.sock"),
/// );
///
/// // ...unless $KIKI_RUNTIME_DIR splits the socket back out.
/// let env = Env {
///     kiki_runtime_dir: Some(PathBuf::from("/run/kiki")),
///     ..env
/// };
/// assert_eq!(
///     resolve_socket_path(None, &home, &env),
///     Path::new("/run/kiki/kiki.sock"),
/// );
/// ```
pub fn resolve_socket_path(explicit: Option<&Path>, data_dir: &DataDir, env: &Env) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }

    if let Some(path) = &env.kiki_socket {
        return path.clone();
    }

    // The runtime directory takes the socket when it was named outright, and
    // when nobody named a data directory for it to sit in instead. A named
    // data directory otherwise keeps it, and is the last resort for a
    // platform or session with no runtime directory at all.
    if env.kiki_runtime_dir.is_some() || !data_dir.is_named() {
        if let Some(runtime) = default_runtime_dir(env) {
            return runtime.join(SOCKET_FILE_NAME);
        }
    }

    data_dir.path.join(SOCKET_FILE_NAME)
}

/// Check that `path` can actually be used as a Unix socket address.
///
/// # Errors
///
/// Returns an error if the path is longer than [`MAX_SOCKET_PATH_LEN`] bytes
/// or contains an interior NUL byte, either of which `bind(2)` would reject
/// with an error that says nothing about the real cause.
pub fn validate_socket_path(path: &Path) -> Result<()> {
    let bytes = path.as_os_str().as_bytes();

    if bytes.contains(&0) {
        bail!("socket path {path:?} contains a NUL byte");
    }

    if bytes.len() > MAX_SOCKET_PATH_LEN {
        bail!(
            "socket path {:?} is {} bytes long, but a Unix socket address holds at \
             most {} on this platform; pass a shorter --uds path",
            path,
            bytes.len(),
            MAX_SOCKET_PATH_LEN,
        );
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod test {
    use super::*;
    use tempdir::TempDir;

    /// An `Env` with every location unset.
    fn empty_env() -> Env {
        Env::default()
    }

    #[test]
    fn data_dir_prefers_kiki_home_over_platform_dir() -> Result<()> {
        let env = Env {
            kiki_home: Some(PathBuf::from("/srv/kiki")),
            platform_data_dir: Some(PathBuf::from("/home/rey/.local/share/kiki")),
            ..empty_env()
        };
        let data_dir = resolve_data_dir(&env)?;

        assert_eq!(data_dir.path, Path::new("/srv/kiki"));
        assert_eq!(data_dir.source, DataDirSource::KikiHome);
        Ok(())
    }

    /// A directory that already holds a `kiki.db` keeps being used, so that
    /// `cd`-ing into one and running `kiki serve` finds it.
    #[test]
    fn data_dir_falls_back_to_cwd_holding_a_database() -> Result<()> {
        let td = TempDir::new("kiki_")?;
        std::fs::write(td.path().join(DB_FILE_NAME), b"")?;

        let env = Env {
            current_dir: Some(td.path().to_path_buf()),
            platform_data_dir: Some(PathBuf::from("/home/rey/.local/share/kiki")),
            ..empty_env()
        };
        let data_dir = resolve_data_dir(&env)?;

        assert_eq!(data_dir.path, td.path());
        assert_eq!(data_dir.source, DataDirSource::CurrentDir);
        Ok(())
    }

    /// ...but a directory with no database in it does not hijack the
    /// platform default.
    #[test]
    fn data_dir_ignores_cwd_without_a_database() -> Result<()> {
        let td = TempDir::new("kiki_")?;
        let platform = PathBuf::from("/home/rey/.local/share/kiki");

        let env = Env {
            current_dir: Some(td.path().to_path_buf()),
            platform_data_dir: Some(platform.clone()),
            ..empty_env()
        };
        let data_dir = resolve_data_dir(&env)?;

        assert_eq!(data_dir.path, platform);
        assert_eq!(data_dir.source, DataDirSource::Platform);
        Ok(())
    }

    #[test]
    fn default_data_dir_prefers_kiki_home() -> Result<()> {
        let env = Env {
            kiki_home: Some(PathBuf::from("/srv/kiki")),
            platform_data_dir: Some(PathBuf::from("/home/rey/.local/share/kiki")),
            ..empty_env()
        };
        assert_eq!(default_data_dir(&env)?, Path::new("/srv/kiki"));
        Ok(())
    }

    /// Unlike [`resolve_data_dir`], the default location ignores the current
    /// directory: `init` and `systemd install` name a well-known place, not
    /// wherever the shell happens to be.
    #[test]
    fn default_data_dir_ignores_the_current_directory() -> Result<()> {
        let td = TempDir::new("kiki_")?;
        std::fs::write(td.path().join(DB_FILE_NAME), b"")?;
        let platform = PathBuf::from("/home/rey/.local/share/kiki");

        let env = Env {
            current_dir: Some(td.path().to_path_buf()),
            platform_data_dir: Some(platform.clone()),
            ..empty_env()
        };
        assert_eq!(default_data_dir(&env)?, platform);
        Ok(())
    }

    #[test]
    fn default_data_dir_errors_when_nothing_is_resolvable() {
        assert!(default_data_dir(&empty_env()).is_err());
    }

    #[test]
    fn data_dir_errors_when_nothing_is_resolvable() {
        assert!(resolve_data_dir(&empty_env()).is_err());
    }

    /// A data directory nobody named — the platform default.
    fn platform_dir(path: &str) -> DataDir {
        DataDir {
            path: PathBuf::from(path),
            source: DataDirSource::Platform,
        }
    }

    /// A data directory named by `$KIKI_HOME`.
    fn named_dir(path: &str) -> DataDir {
        DataDir {
            path: PathBuf::from(path),
            source: DataDirSource::KikiHome,
        }
    }

    #[test]
    fn socket_prefers_explicit_flag() {
        let env = Env {
            kiki_socket: Some(PathBuf::from("/run/env.sock")),
            runtime_dir: Some(PathBuf::from("/run/user/1000")),
            ..empty_env()
        };
        let explicit = PathBuf::from("/tmp/explicit.sock");
        assert_eq!(
            resolve_socket_path(Some(&explicit), &named_dir("/srv/kiki"), &env),
            explicit
        );
    }

    #[test]
    fn socket_prefers_env_var_over_the_data_dir() {
        let env = Env {
            kiki_socket: Some(PathBuf::from("/run/env.sock")),
            runtime_dir: Some(PathBuf::from("/run/user/1000")),
            ..empty_env()
        };
        assert_eq!(
            resolve_socket_path(None, &named_dir("/srv/kiki"), &env),
            Path::new("/run/env.sock")
        );
    }

    /// Pointing Kiki at a directory points all of it there: the socket
    /// follows the data rather than staying in the runtime directory.
    #[test]
    fn socket_follows_a_named_data_dir_over_the_runtime_dir() {
        let env = Env {
            runtime_dir: Some(PathBuf::from("/run/user/1000")),
            ..empty_env()
        };
        assert_eq!(
            resolve_socket_path(None, &named_dir("/srv/kiki"), &env),
            Path::new("/srv/kiki/kiki.sock")
        );
    }

    /// The same holds for a directory named by having a database in it, so
    /// serving from one puts the socket back in `./kiki.sock`.
    #[test]
    fn socket_follows_the_current_dir_when_it_holds_the_database() {
        let env = Env {
            runtime_dir: Some(PathBuf::from("/run/user/1000")),
            ..empty_env()
        };
        let data_dir = DataDir {
            path: PathBuf::from("/home/rey/project"),
            source: DataDirSource::CurrentDir,
        };
        assert_eq!(
            resolve_socket_path(None, &data_dir, &env),
            Path::new("/home/rey/project/kiki.sock")
        );
    }

    /// `$KIKI_RUNTIME_DIR` names Kiki's runtime directory outright, so the
    /// socket goes directly inside it with no `kiki/` subdirectory.
    #[test]
    fn socket_uses_kiki_runtime_dir_verbatim() {
        let env = Env {
            kiki_runtime_dir: Some(PathBuf::from("/run/kiki")),
            runtime_dir: Some(PathBuf::from("/run/user/1000")),
            ..empty_env()
        };
        assert_eq!(
            resolve_socket_path(None, &platform_dir("/data"), &env),
            Path::new("/run/kiki/kiki.sock")
        );
    }

    /// Naming the runtime directory is a statement about the socket, so it
    /// beats the rule that a named data directory keeps the socket.
    #[test]
    fn kiki_runtime_dir_beats_a_named_data_dir() {
        let env = Env {
            kiki_runtime_dir: Some(PathBuf::from("/run/kiki")),
            ..empty_env()
        };
        assert_eq!(
            resolve_socket_path(None, &named_dir("/srv/kiki"), &env),
            Path::new("/run/kiki/kiki.sock")
        );
    }

    /// ...but `--uds` and `$KIKI_SOCKET` name the socket file itself, so
    /// they beat `$KIKI_RUNTIME_DIR` in turn.
    #[test]
    fn an_explicit_socket_beats_kiki_runtime_dir() {
        let env = Env {
            kiki_runtime_dir: Some(PathBuf::from("/run/kiki")),
            kiki_socket: Some(PathBuf::from("/run/env.sock")),
            ..empty_env()
        };
        assert_eq!(
            resolve_socket_path(None, &platform_dir("/data"), &env),
            Path::new("/run/env.sock")
        );

        let explicit = PathBuf::from("/tmp/explicit.sock");
        assert_eq!(
            resolve_socket_path(Some(&explicit), &platform_dir("/data"), &env),
            explicit
        );
    }

    /// An empty `$KIKI_RUNTIME_DIR` is treated as unset, not as a relative
    /// path resolving to `./kiki.sock`.
    #[test]
    fn empty_kiki_runtime_dir_is_ignored() {
        // `non_empty_var` filters it out on the way in, so an `Env` built
        // from the process never carries one. Confirm resolution agrees.
        let env = Env {
            kiki_runtime_dir: None,
            runtime_dir: Some(PathBuf::from("/run/user/1000")),
            ..empty_env()
        };
        assert_eq!(
            resolve_socket_path(None, &platform_dir("/data"), &env),
            Path::new("/run/user/1000/kiki/kiki.sock")
        );
    }

    #[test]
    fn default_runtime_dir_prefers_kiki_runtime_dir() {
        let env = Env {
            kiki_runtime_dir: Some(PathBuf::from("/run/kiki")),
            runtime_dir: Some(PathBuf::from("/run/user/1000")),
            ..empty_env()
        };
        assert_eq!(default_runtime_dir(&env), Some(PathBuf::from("/run/kiki")));
    }

    #[test]
    fn default_runtime_dir_subdivides_the_platform_dir() {
        let env = Env {
            runtime_dir: Some(PathBuf::from("/run/user/1000")),
            ..empty_env()
        };
        assert_eq!(
            default_runtime_dir(&env),
            Some(PathBuf::from("/run/user/1000/kiki"))
        );
    }

    #[test]
    fn default_runtime_dir_is_none_when_nothing_is_available() {
        assert_eq!(default_runtime_dir(&empty_env()), None);
    }

    /// With nobody naming anything, the runtime directory wins.
    #[test]
    fn socket_defaults_to_runtime_dir() {
        let env = Env {
            runtime_dir: Some(PathBuf::from("/run/user/1000")),
            ..empty_env()
        };
        assert_eq!(
            resolve_socket_path(None, &platform_dir("/data"), &env),
            Path::new("/run/user/1000/kiki/kiki.sock")
        );
    }

    #[test]
    fn socket_falls_back_to_data_dir_without_runtime_dir() {
        assert_eq!(
            resolve_socket_path(None, &platform_dir("/data"), &empty_env()),
            Path::new("/data/kiki.sock")
        );
    }

    #[test]
    fn runtime_dir_must_be_private_to_this_user() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let td = TempDir::new("kiki_")?;
        let dir = td.path().join("runtime");
        std::fs::create_dir(&dir)?;

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        assert!(runtime_dir_is_usable(&dir));

        // Group- or world-accessible means another user could reach the
        // socket, so the directory is rejected.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))?;
        assert!(!runtime_dir_is_usable(&dir));

        Ok(())
    }

    #[test]
    fn runtime_dir_must_exist_and_be_a_directory() -> Result<()> {
        let td = TempDir::new("kiki_")?;
        assert!(!runtime_dir_is_usable(&td.path().join("missing")));

        let file = td.path().join("file");
        std::fs::write(&file, b"")?;
        assert!(!runtime_dir_is_usable(&file));

        Ok(())
    }

    #[test]
    fn socket_path_length_is_validated() {
        assert!(validate_socket_path(Path::new("/run/user/1000/kiki/kiki.sock")).is_ok());

        let long = PathBuf::from(format!("/{}/kiki.sock", "a".repeat(MAX_SOCKET_PATH_LEN)));
        assert!(validate_socket_path(&long).is_err());
    }
}
