use anyhow::{bail, Result};
use clap::Args;

#[derive(Args)]
pub struct MigrateArgs {}

impl MigrateArgs {
    /// Run the `migrate` subcommand
    pub fn run(&self) -> Result<()> {
        bail!("the `migrate` subcommand has not been implemented")
    }
}
