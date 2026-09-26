use anyhow::Result;
use clap::{Parser, Subcommand};
use kiki_rss::cli;

#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

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
    Systemd(cli::systemd::SystemdArgs),

    /// Internal: run the sandboxed feed fetcher. Spawned by `serve`.
    #[cfg(unix)]
    #[command(name = kiki_rss::process::feed_fetcher::SUBCOMMAND, hide = true)]
    FeedFetcher(cli::child::ChildArgs),

    /// Internal: run the sandboxed Lua script host. Spawned by `serve`.
    #[cfg(all(unix, feature = "lua"))]
    #[command(name = kiki_rss::process::script_host::SUBCOMMAND, hide = true)]
    ScriptHost(cli::child::ChildArgs),

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
            Commands::Systemd(args) => args.run(),
            #[cfg(unix)]
            Commands::FeedFetcher(args) => {
                use kiki_rss::process::feed_fetcher as f;
                args.run(f::SUBCOMMAND, f::HOST_FD_ENV, f::run_child)
            }
            #[cfg(all(unix, feature = "lua"))]
            Commands::ScriptHost(args) => {
                use kiki_rss::process::script_host as s;
                args.run(s::SUBCOMMAND, s::HOST_FD_ENV, s::run_child)
            }
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
