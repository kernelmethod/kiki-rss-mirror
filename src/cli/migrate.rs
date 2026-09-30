use crate::cli::paths::{self, Env};
use crate::db::{integrity, migrations, ConnectionBuilder};
use anyhow::{Context, Result};
use clap::Args;
use std::path::PathBuf;

#[derive(Args)]
pub struct MigrateArgs {
    /// Path to the database file to migrate [default: the database
    /// `kiki serve` would open]
    database: Option<PathBuf>,

    /// Show pending migrations without applying them
    #[arg(long)]
    dry_run: bool,

    /// Apply migrations without first backing up the database.
    ///
    /// By default, a copy of the database is written next to it before any
    /// migration runs, named `<database>.pre-<migration>.<time>.bak`: a
    /// migration that fails is rolled back, but one that succeeds and
    /// turns out to be wrong has no way back without it. The copy needs
    /// about as much free space as the database.
    #[arg(long)]
    no_backup: bool,
}

impl MigrateArgs {
    /// Resolve which database to migrate.
    ///
    /// With no path given this is the database `kiki serve` would open, so
    /// that a bare `kiki migrate` always acts on the running server's
    /// database rather than on whatever happens to be in the current
    /// directory.
    ///
    /// # Errors
    ///
    /// Returns an error if no path was given and no data directory could be
    /// resolved. See [`paths::resolve_data_dir`].
    fn resolve_database(&self) -> Result<PathBuf> {
        match &self.database {
            Some(path) => Ok(path.clone()),
            None => Ok(paths::resolve_data_dir(&Env::from_process())?
                .path
                .join(paths::DB_FILE_NAME)),
        }
    }

    /// Run the `migrate` subcommand
    pub fn run(&self) -> Result<()> {
        let database = self.resolve_database()?;
        if self.dry_run {
            let conn = ConnectionBuilder::default()
                .at_path(&database)
                .read_write()
                .build()
                .with_context(|| format!("failed to open database at {database:?}"))?;

            let pending = migrations::pending_migrations(&conn)?;
            if pending.is_empty() {
                println!("Database is up to date. No pending migrations.");
            } else {
                println!("Pending migrations:");
                for migration in &pending {
                    println!("  - {}", migration.name);
                }
            }
        } else {
            let mut conn = ConnectionBuilder::default()
                .at_path(&database)
                .read_write()
                .build()
                .with_context(|| format!("failed to open database at {database:?}"))?;

            if !self.no_backup {
                if let Some(first) = migrations::pending_migrations(&conn)?.first() {
                    let dest = integrity::backup_path(
                        &database,
                        first.name,
                        chrono::Utc::now().timestamp(),
                    );
                    integrity::backup(&conn, &dest).context(
                        "not migrating without a backup; pass --no-backup to migrate anyway",
                    )?;
                    println!("Backed up the database to {}", dest.display());
                }
            }

            let count = migrations::run_pending_migrations(&mut conn)?;
            if count == 0 {
                println!("Database is up to date. No migrations applied.");
            } else {
                println!("Applied {} migration(s) successfully.", count);
            }
        }

        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// A database one migration behind, and the directory it lives in.
    fn outdated_database() -> Result<(tempfile::TempDir, PathBuf)> {
        let td = tempfile::TempDir::with_prefix("kiki_")?;
        let path = td.path().join("kiki.db");
        let conn = ConnectionBuilder::default()
            .at_path(&path)
            .create()
            .build()?;
        conn.execute_batch(
            "DROP TABLE pending_entry_assets;
             DELETE FROM migrations WHERE name = '0009_pending_entry_assets';",
        )?;
        Ok((td, path))
    }

    fn backups(dir: &std::path::Path) -> Result<Vec<PathBuf>> {
        Ok(std::fs::read_dir(dir)?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "bak"))
            .collect())
    }

    fn has_table(path: &std::path::Path) -> Result<bool> {
        Ok(Connection::open(path)?.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'pending_entry_assets')",
            [],
            |r| r.get(0),
        )?)
    }

    #[test]
    fn migrating_backs_up_the_database_first() -> Result<()> {
        let (td, path) = outdated_database()?;
        MigrateArgs {
            database: Some(path.clone()),
            dry_run: false,
            no_backup: false,
        }
        .run()?;

        assert!(has_table(&path)?);
        let backups = backups(td.path())?;
        assert_eq!(backups.len(), 1, "{backups:?}");
        let name = backups
            .first()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy();
        assert!(
            name.starts_with("kiki.db.pre-0009_pending_entry_assets."),
            "{name}"
        );
        // The backup is the database as it was before the migration.
        assert!(!has_table(backups.first().unwrap())?);
        Ok(())
    }

    #[test]
    fn no_backup_is_taken_when_asked_or_when_up_to_date() -> Result<()> {
        let (td, path) = outdated_database()?;
        MigrateArgs {
            database: Some(path.clone()),
            dry_run: false,
            no_backup: true,
        }
        .run()?;
        assert!(has_table(&path)?);
        assert!(backups(td.path())?.is_empty());

        MigrateArgs {
            database: Some(path),
            dry_run: false,
            no_backup: false,
        }
        .run()?;
        assert!(backups(td.path())?.is_empty());
        Ok(())
    }
}
