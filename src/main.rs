pub mod init;
pub mod server;
pub mod http;

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
    Server {},

    /// Sets up a new Kiki database and configuration files
    Init {}
}

impl Commands {
    /// Run the selected subcommand.
    pub fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        match self {
            Commands::Init {} => {
                init::init()
            }
            Commands::Server {} => {
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(server::server())
            }
        }
    }
}

pub fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    cli.command.run()
}
