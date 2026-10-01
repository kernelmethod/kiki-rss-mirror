use anyhow::Result;
use clap::{Parser, Subcommand};
use kiki_rss::cli;

// On glibc, mimalloc: it returns freed memory to the OS, unlike glibc under
// tokio's thread pool, whose per-thread arenas grow and rarely shrink —
// though only when a thread calls into it again, which is why kiki's
// runtimes collect as their threads park (see kiki_rss::memory). Its
// secure mode (guard pages, encrypted free lists, randomized allocation)
// hardens the heap against the untrusted feeds and plugins kiki parses and
// runs.
//
// The static musl build keeps musl's own malloc. It holds a quarter of the
// memory mimalloc does across Kiki's processes (about 30 MiB rather than
// 120 MiB while fetching feeds in the background), since it hands freed
// memory straight back to the OS, and under load from `tools/stress` its
// global lock cost no measurable throughput or latency. It also keeps its
// metadata out of band and checks it, short of mimalloc's secure mode.
#[cfg(not(target_env = "musl"))]
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

    /// Import or export feeds as OPML
    Opml(cli::opml::OpmlArgs),

    /// List plugins and read or change their config
    Plugin(cli::plugin::PluginArgs),

    /// Generate a static HTML page for the API documentation
    #[cfg(feature = "api-docs")]
    Docs(cli::docs::DocsArgs),

    /// Manage the kiki systemd user service
    #[cfg(feature = "systemd")]
    Systemd(cli::systemd::SystemdArgs),

    /// Starts the Kiki web UI
    #[cfg(feature = "web-ui")]
    Web(cli::web::WebArgs),

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
            Commands::Opml(args) => args.run(),
            Commands::Plugin(args) => args.run(),
            #[cfg(feature = "api-docs")]
            Commands::Docs(args) => args.run(),
            #[cfg(feature = "systemd")]
            Commands::Systemd(args) => args.run(),
            #[cfg(feature = "web-ui")]
            Commands::Web(args) => args.run(),
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
