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

#[derive(Args)]
pub struct InitArgs {
    /// The directory that Kiki's files should be set up in
    #[arg(conflicts_with = "auto")]
    directory: Option<PathBuf>,

    /// Use Kiki's default directory ($KIKI_HOME, or the platform data
    /// directory)
    #[arg(long)]
    auto: bool,

    /// Do nothing if Kiki has already been configured
    #[arg(short, long, conflicts_with = "force")]
    check: bool,

    /// Force Kiki to overwrite existing files. This option is destructive!
    #[arg(long, conflicts_with = "check")]
    force: bool,
}

impl InitArgs {
    /// Create an `InitArgs` equivalent to `kiki init --auto --check`.
    ///
    /// This initializes Kiki's default directory if it hasn't been set up
    /// yet, and is a no-op otherwise.
    pub(crate) fn auto_with_check() -> Self {
        Self {
            directory: None,
            auto: true,
            check: true,
            force: false,
        }
    }

    /// Resolve the target directory from the provided arguments.
    ///
    /// A bare `kiki init` with `$KIKI_HOME` set needs no `--auto`: the
    /// environment has already named the directory, and asking for the flag
    /// as well would be ceremony.
    ///
    /// # Errors
    ///
    /// Returns an error if no directory was given, `--auto` was not passed,
    /// and `$KIKI_HOME` is unset.
    fn resolve_directory(&self) -> Result<PathBuf> {
        self.resolve_directory_in(&Env::from_process())
    }

    /// [`resolve_directory`](Self::resolve_directory) against an explicit
    /// environment.
    fn resolve_directory_in(&self, env: &Env) -> Result<PathBuf> {
        if self.auto {
            return paths::default_data_dir(env);
        }

        if let Some(ref dir) = self.directory {
            return Ok(dir.clone());
        }

        if env.kiki_home.is_some() {
            return paths::default_data_dir(env);
        }

        bail!(
            "please provide a directory, set $KIKI_HOME, or use --auto for the platform \
             default"
        )
    }

    /// Run the `init` subcommand
    pub fn run(&self) -> Result<()> {
        let directory = self.resolve_directory()?;
        let db_path = Path::new(&directory).join(paths::DB_FILE_NAME);
        if db_path.exists() {
            if self.check {
                // Kiki has already been configured
                return Ok(());
            }

            if !self.force {
                bail!("A database has already been set up at {:#?}", &db_path);
            }

            fs::remove_file(&db_path)
                .with_context(|| format!("unable to delete database file at {:#?}", db_path))?;
        }

        fs::create_dir_all(&directory)
            .with_context(|| format!("unable to create directory {directory:?}"))?;
        restrict_permissions(&directory, 0o750)?;

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

    /// Test the `--check` flag for `kiki init`.
    #[test]
    fn test_check() -> Result<()> {
        let td = TempDir::new("kiki_")?;
        let path = PathBuf::from(td.path());
        assert!((InitArgs {
            directory: Some(path.clone()),
            auto: false,
            check: false,
            force: false
        })
        .run()
        .is_ok());
        assert!((InitArgs {
            directory: Some(path.clone()),
            auto: false,
            check: false,
            force: false
        })
        .run()
        .is_err());
        assert!((InitArgs {
            directory: Some(path.clone()),
            auto: false,
            check: true,
            force: false
        })
        .run()
        .is_ok());

        Ok(())
    }

    /// Test the `--force` flag for `kiki init`
    #[test]
    fn test_force() -> Result<()> {
        let td = TempDir::new("kiki_")?;
        let path = PathBuf::from(td.path());
        assert!((InitArgs {
            directory: Some(path.clone()),
            auto: false,
            check: false,
            force: false
        })
        .run()
        .is_ok());
        assert!((InitArgs {
            directory: Some(path.clone()),
            auto: false,
            check: false,
            force: false
        })
        .run()
        .is_err());
        assert!((InitArgs {
            directory: Some(path.clone()),
            auto: false,
            check: false,
            force: true
        })
        .run()
        .is_ok());

        Ok(())
    }

    /// An `InitArgs` with every flag at its default.
    fn auto_args() -> InitArgs {
        InitArgs {
            directory: None,
            auto: true,
            check: false,
            force: false,
        }
    }

    /// Test that `--auto` resolves to a path under the platform data directory.
    #[test]
    fn test_auto() -> Result<()> {
        let platform = PathBuf::from("/home/rey/.local/share/kiki");
        let env = Env {
            platform_data_dir: Some(platform.clone()),
            ..Env::default()
        };
        assert_eq!(auto_args().resolve_directory_in(&env)?, platform);
        Ok(())
    }

    /// `$KIKI_HOME` displaces the platform data directory under `--auto`.
    #[test]
    fn test_auto_prefers_kiki_home() -> Result<()> {
        let env = Env {
            kiki_home: Some(PathBuf::from("/srv/kiki")),
            platform_data_dir: Some(PathBuf::from("/home/rey/.local/share/kiki")),
            ..Env::default()
        };
        assert_eq!(
            auto_args().resolve_directory_in(&env)?,
            Path::new("/srv/kiki")
        );
        Ok(())
    }

    /// With `$KIKI_HOME` set, a bare `kiki init` needs no `--auto`: the
    /// environment has already named the directory.
    #[test]
    fn test_kiki_home_needs_no_auto() -> Result<()> {
        let env = Env {
            kiki_home: Some(PathBuf::from("/srv/kiki")),
            ..Env::default()
        };
        let args = InitArgs {
            auto: false,
            ..auto_args()
        };
        assert_eq!(args.resolve_directory_in(&env)?, Path::new("/srv/kiki"));
        Ok(())
    }

    /// Test that providing neither a directory, nor `--auto`, nor
    /// `$KIKI_HOME` returns an error.
    #[test]
    fn test_no_directory_no_auto() {
        let args = InitArgs {
            auto: false,
            ..auto_args()
        };
        assert!(args.resolve_directory_in(&Env::default()).is_err());
    }
}
