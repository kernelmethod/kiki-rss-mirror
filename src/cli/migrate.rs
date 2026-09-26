use crate::cli::paths::{self, Env};
use crate::db::{migrations, ConnectionBuilder};
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
