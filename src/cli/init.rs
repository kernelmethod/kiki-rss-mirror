use crate::cli::paths::{self, Env};
use crate::db::ConnectionBuilder;
use anyhow::{bail, Context, Result};
use clap::Args;
use std::fs;
use std::path::{Path, PathBuf};

/// Set restrictive permissions on a path so that only the owner and group can
/// access it. On non-Unix platforms this is a no-op.
fn restrict_permissions(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(mode);
        fs::set_permissions(path, perms)
            .with_context(|| format!("unable to set permissions on {path:?}"))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

/// Returns the directory Kiki treats as its home.
///
/// `$KIKI_HOME` if set, otherwise the platform data directory:
///
/// - Linux: `$XDG_DATA_HOME/kiki/` (defaults to `~/.local/share/kiki/`)
/// - macOS: `~/Library/Application Support/kiki/`
/// - Windows: `%APPDATA%\kiki\`
///
/// # Errors
///
/// Returns an error if neither is available. See
/// [`paths::default_data_dir`].
pub(crate) fn default_directory() -> Result<PathBuf> {
    paths::default_data_dir(&Env::from_process())
}

/// Arguments for the `kiki init` subcommand.
///
/// `kiki init` takes no directory. It always sets up
/// [`default_directory`] — `$KIKI_HOME` when that is set, and the platform
/// data directory otherwise — so there is only ever one answer to where
/// Kiki lives, and every other subcommand resolves it the same way. To
/// initialize somewhere else, name it with `$KIKI_HOME`.
#[derive(Args)]
pub struct InitArgs {
    /// Do nothing if Kiki has already been configured
    #[arg(short, long, conflicts_with = "force")]
    check: bool,

    /// Force Kiki to overwrite existing files. This option is destructive!
    #[arg(long, conflicts_with = "check")]
    force: bool,
}

impl InitArgs {
    /// Create an `InitArgs` equivalent to `kiki init --check`.
    ///
    /// This initializes Kiki's directory if it hasn't been set up yet, and
    /// is a no-op otherwise.
    pub(crate) fn with_check() -> Self {
        Self {
            check: true,
            force: false,
        }
    }

    /// Run the `init` subcommand.
    ///
    /// # Errors
    ///
    /// Returns an error if the target directory cannot be determined, if a
    /// database is already present and neither `--check` nor `--force` was
    /// passed, or if the directory or database cannot be created.
    pub fn run(&self) -> Result<()> {
        self.run_in(&default_directory()?)
    }

    /// [`run`](Self::run) against an explicit directory.
    fn run_in(&self, directory: &Path) -> Result<()> {
        let db_path = directory.join(paths::DB_FILE_NAME);
        if db_path.exists() {
            if self.check {
                // Kiki has already been configured
                return Ok(());
            }

            if !self.force {
                bail!("A database has already been set up at {:#?}", db_path);
            }

            fs::remove_file(&db_path)
                .with_context(|| format!("unable to delete database file at {:#?}", db_path))?;
        }

        fs::create_dir_all(directory)
            .with_context(|| format!("unable to create directory {directory:?}"))?;
        restrict_permissions(directory, 0o750)?;

        ConnectionBuilder::default()
            .at_path(&db_path)
            .create()
            .build()
            .with_context(|| format!("failed to create database in {:?}", db_path))?;
        restrict_permissions(&db_path, 0o660)?;

        println!("Initialized Kiki in {}", directory.display());

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use anyhow::Result;
    use tempdir::TempDir;

    /// An `InitArgs` with every flag at its default.
    fn args() -> InitArgs {
        InitArgs {
            check: false,
            force: false,
        }
    }

    /// Test the `--check` flag for `kiki init`.
    #[test]
    fn test_check() -> Result<()> {
        let td = TempDir::new("kiki_")?;
        let path = td.path();
        assert!(args().run_in(path).is_ok());
        assert!(args().run_in(path).is_err());
        assert!((InitArgs {
            check: true,
            ..args()
        })
        .run_in(path)
        .is_ok());

        Ok(())
    }

    /// Test the `--force` flag for `kiki init`
    #[test]
    fn test_force() -> Result<()> {
        let td = TempDir::new("kiki_")?;
        let path = td.path();
        assert!(args().run_in(path).is_ok());
        assert!(args().run_in(path).is_err());
        assert!((InitArgs {
            force: true,
            ..args()
        })
        .run_in(path)
        .is_ok());

        Ok(())
    }

    /// `with_check` is `kiki init --check`, so it leaves an existing
    /// database alone rather than failing on it.
    #[test]
    fn test_with_check_is_idempotent() -> Result<()> {
        let td = TempDir::new("kiki_")?;
        let path = td.path();
        assert!(InitArgs::with_check().run_in(path).is_ok());
        assert!(InitArgs::with_check().run_in(path).is_ok());

        Ok(())
    }

    /// `kiki init` creates the directory it was pointed at, rather than
    /// requiring it to exist already.
    #[test]
    fn test_creates_a_missing_directory() -> Result<()> {
        let td = TempDir::new("kiki_")?;
        let path = td.path().join("nested").join("home");
        assert!(args().run_in(&path).is_ok());
        assert!(path.join(paths::DB_FILE_NAME).exists());

        Ok(())
    }
}
