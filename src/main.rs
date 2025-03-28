pub mod http;
pub mod init;
pub mod server;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about, long_about = None)]
pub struct Cli {
    /// The subcommand that should be run. See [Commands].
    #[command(subcommand)]
    pub command: Commands,
}

/// Subcommands that are available for Kiki.
#[derive(Subcommand)]
pub enum Commands {
    /// Starts the Kiki server
    Server {},

    /// Sets up a new Kiki database and configuration files
    Init {
        /// The directory that Kiki's files should be set up in
        directory: PathBuf,

        /// Do nothing if Kiki has already been configured
        #[arg(short, long, conflicts_with = "force")]
        check: bool,

        /// Force Kiki to overwrite existing files. This option is destructive!
        #[arg(long, conflicts_with = "check")]
        force: bool,
    },
}

impl Commands {
    /// Run the selected subcommand.
    pub fn run(&self) -> Result<()> {
        match self {
            Commands::Init {
                directory,
                check,
                force,
            } => init::init(&directory, *check, *force),
            Commands::Server {} => tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(server::server()),
        }
    }
}

pub fn main() -> Result<()> {
    let cli = Cli::parse();

    cli.command.run()
}
