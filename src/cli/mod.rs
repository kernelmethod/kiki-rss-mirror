pub mod init;
pub mod migrate;
pub mod opml;
pub mod paths;
pub mod plugin;
pub mod serve;

#[cfg(feature = "api-docs")]
pub mod docs;

#[cfg(feature = "systemd")]
pub mod systemd;

#[cfg(feature = "web-ui")]
pub mod web;

#[cfg(unix)]
pub mod child;

/// Install the global logger, writing to `writer`.
///
/// What is logged is set by `RUST_LOG`, in [`tracing_subscriber::EnvFilter`]'s
/// syntax (`warn`, or `kiki_rss::process=debug,info`, say), and is
/// everything at `INFO` and above when it is unset or names nothing valid.
pub(crate) fn init_logging<W>(writer: W)
where
    W: for<'w> tracing_subscriber::fmt::MakeWriter<'w> + Send + Sync + 'static,
{
    use tracing_subscriber::filter::{EnvFilter, LevelFilter};

    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .init();
}
