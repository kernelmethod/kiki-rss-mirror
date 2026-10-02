use anyhow::Result;
use clap::{Parser, Subcommand};
use kiki_rss::cli;

/// The most malloc arenas glibc may create.
///
/// Kiki uses the C library's malloc. musl's, in the static build, hands
/// freed memory straight back to the OS. glibc's gives each thread that
/// contends for the heap an arena of its own, up to eight per core, and
/// rarely shrinks them; with tokio's blocking pool running every database
/// call, the server collects dozens. Under load from `tools/stress`,
/// capping them at two cut the memory across Kiki's processes by a third
/// (from 133 to 86 MiB), and at one by little more, with no measurable
/// cost in throughput. See `kiki_rss::docs::memory` for the measurements.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const MALLOC_ARENA_MAX: libc::c_int = 2;

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

    /// Internal: run the feed fetcher's worker. Spawned by the feed fetcher.
    #[cfg(unix)]
    #[command(name = kiki_rss::process::feed_fetcher::WORKER_SUBCOMMAND, hide = true)]
    FeedWorker(cli::child::ChildArgs),

    /// Internal: run the feed fetcher's parser. Spawned by the feed fetcher.
    #[cfg(unix)]
    #[command(name = kiki_rss::process::feed_fetcher::PARSER_SUBCOMMAND, hide = true)]
    FeedParser(cli::child::ChildArgs),

    /// Internal: run the feed fetcher's resolver. Spawned by the feed
    /// fetcher.
    #[cfg(unix)]
    #[command(name = kiki_rss::process::feed_fetcher::RESOLVER_SUBCOMMAND, hide = true)]
    FeedResolver(cli::child::ChildArgs),

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
            #[cfg(unix)]
            Commands::FeedWorker(args) => {
                use kiki_rss::process::feed_fetcher as f;
                args.run(f::WORKER_SUBCOMMAND, f::WORKER_FD_ENV, f::run_worker)
            }
            #[cfg(unix)]
            Commands::FeedParser(args) => {
                use kiki_rss::process::feed_fetcher as f;
                args.run(f::PARSER_SUBCOMMAND, f::PARSER_FD_ENV, f::run_parser)
            }
            #[cfg(unix)]
            Commands::FeedResolver(args) => {
                use kiki_rss::process::feed_fetcher as f;
                args.run(f::RESOLVER_SUBCOMMAND, f::RESOLVER_FD_ENV, f::run_resolver)
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
    // Before any other thread starts, so that none gets an arena of its own.
    // SAFETY: mallopt takes no pointers; it only changes glibc's malloc
    // parameters, and no other thread is allocating yet.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, MALLOC_ARENA_MAX);
    }

    // Ensure files created by kiki are not accessible to other users.
    // SAFETY: umask is always safe to call and has no failure modes.
    #[cfg(unix)]
    unsafe {
        libc::umask(0o007);
    }

    let cli = Cli::parse();

    cli.command.run()
}
