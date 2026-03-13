use crate::db::{migrations, ConnectionBuilder};
use anyhow::{Context, Result};
use clap::Args;
use std::path::PathBuf;

#[derive(Args)]
pub struct MigrateArgs {
    /// Path to the database file to migrate
    #[arg(default_value = "kiki.db")]
    database: PathBuf,

    /// Show pending migrations without applying them
    #[arg(long)]
    dry_run: bool,
}

impl MigrateArgs {
    /// Run the `migrate` subcommand
    pub fn run(&self) -> Result<()> {
        if self.dry_run {
            let conn = ConnectionBuilder::default()
                .at_path(&self.database)
                .read_write()
                .build()
                .with_context(|| format!("failed to open database at {:?}", &self.database))?;

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
                .at_path(&self.database)
                .read_write()
                .build()
                .with_context(|| format!("failed to open database at {:?}", &self.database))?;

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
