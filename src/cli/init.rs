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

/// Returns the platform-default data directory for Kiki.
///
/// - Linux: `$XDG_DATA_HOME/kiki/` (defaults to `~/.local/share/kiki/`)
/// - macOS: `~/Library/Application Support/kiki/`
/// - Windows: `%APPDATA%\kiki\`
pub(crate) fn default_directory() -> Result<PathBuf> {
    let base = dirs::data_dir().context("unable to determine platform data directory")?;
    Ok(base.join("kiki"))
}

#[derive(Args)]
pub struct InitArgs {
    /// The directory that Kiki's files should be set up in
    #[arg(conflicts_with = "auto")]
    directory: Option<PathBuf>,

    /// Use the platform-default data directory
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
    /// This initializes the platform-default data directory if it hasn't been
    /// set up yet, and is a no-op otherwise.
    pub(crate) fn auto_with_check() -> Self {
        Self {
            directory: None,
            auto: true,
            check: true,
            force: false,
        }
    }

    /// Resolve the target directory from the provided arguments.
    fn resolve_directory(&self) -> Result<PathBuf> {
        if self.auto {
            default_directory()
        } else if let Some(ref dir) = self.directory {
            Ok(dir.clone())
        } else {
            bail!("please provide a directory or use --auto for the platform default")
        }
    }

    /// Run the `init` subcommand
    pub fn run(&self) -> Result<()> {
        let directory = self.resolve_directory()?;
        let db_path = Path::new(&directory).join("kiki.db");
        if db_path.exists() {
            if self.check {
                // Kiki has already been configured
                return Ok(());
            }

            if !self.force {
                bail!("A database has already been set up at {:#?}", &db_path);
            }

            fs::remove_file(&db_path)
                .with_context(|| format!("unable to delete database file at {:#?}", &db_path))?;
        }

        fs::create_dir_all(&directory)
            .with_context(|| format!("unable to create directory {directory:?}"))?;
        restrict_permissions(&directory, 0o750)?;

        ConnectionBuilder::default()
            .at_path(&db_path)
            .create()
            .build()
            .with_context(|| format!("failed to create database in {:?}", &db_path))?;
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

    /// Test that `--auto` resolves to a path under the platform data directory.
    #[test]
    fn test_auto() -> Result<()> {
        let args = InitArgs {
            directory: None,
            auto: true,
            check: false,
            force: false,
        };
        let resolved = args.resolve_directory()?;
        let data_dir = dirs::data_dir()
            .ok_or_else(|| anyhow::anyhow!("platform should have a data directory"))?;
        assert_eq!(resolved, data_dir.join("kiki"));
        Ok(())
    }

    /// Test that providing neither a directory nor `--auto` returns an error.
    #[test]
    fn test_no_directory_no_auto() {
        let args = InitArgs {
            directory: None,
            auto: false,
            check: false,
            force: false,
        };
        assert!(args.resolve_directory().is_err());
    }
}
