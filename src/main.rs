use anyhow::Result;
use clap::{Parser, Subcommand};
use kiki_rss::cli;

#[derive(Parser)]
#[command(about, long_about = None)]
pub struct Cli {
    /// The subcommand that should be run. See [Commands].
    #[command(subcommand)]
    pub command: Commands,
}

/// Subcommands that are available for Kiki.
#[derive(Subcommand)]
pub enum Commands {
    /// Sets up a new Kiki database and configuration files
    Init(cli::init::InitArgs),

    /// Starts the Kiki server
    Serve(cli::serve::ServeArgs),

    /// Migrate the Kiki database schema to the latest version
    Migrate(cli::migrate::MigrateArgs),

    /// Generate a static HTML page for the API documentation
    #[cfg(feature = "api-docs")]
    Docs(cli::docs::DocsArgs),

    /// Manage the kiki systemd user service
    #[cfg(feature = "systemd")]
    Service(cli::service::ServiceArgs),

    /// Internal: run the sandboxed Lua script host. Spawned by `serve`.
    #[cfg(all(unix, feature = "lua"))]
    #[command(name = kiki_rss::process::script_host::SUBCOMMAND, hide = true)]
    ScriptHost(cli::script_host::ScriptHostArgs),

    /// Print the version of Kiki
    Version,
}

impl Commands {
    /// Run the selected subcommand.
    pub fn run(&self) -> Result<()> {
        match self {
            Commands::Init(args) => args.run(),
            Commands::Migrate(args) => args.run(),
            Commands::Serve(args) => args.run(),
            #[cfg(feature = "api-docs")]
            Commands::Docs(args) => args.run(),
            #[cfg(feature = "systemd")]
            Commands::Service(args) => args.run(),
            #[cfg(all(unix, feature = "lua"))]
            Commands::ScriptHost(args) => args.run(),
            Commands::Version => {
                println!("kiki {}", env!("CARGO_PKG_VERSION"));
                Ok(())
            }
        }
    }
}

pub fn main() -> Result<()> {
    // Ensure files created by kiki are not accessible to other users.
    // SAFETY: umask is always safe to call and has no failure modes.
    #[cfg(unix)]
    unsafe {
        libc::umask(0o007);
    }

    let cli = Cli::parse();

    cli.command.run()
}
