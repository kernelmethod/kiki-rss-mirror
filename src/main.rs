pub mod http;
pub mod init;
pub mod serve;

use anyhow::Result;
use clap::{Parser, Subcommand};

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
    Serve(serve::ServeArgs),

    /// Sets up a new Kiki database and configuration files
    Init(init::InitArgs),
}

impl Commands {
    /// Run the selected subcommand.
    pub fn run(&self) -> Result<()> {
        match self {
            Commands::Init(args) => args.run(),
            Commands::Serve(args) => args.run(),
        }
    }
}

pub fn main() -> Result<()> {
    let cli = Cli::parse();

    cli.command.run()
}
