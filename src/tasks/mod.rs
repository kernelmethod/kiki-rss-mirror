mod adaptive;
pub mod assets;
mod backoff;
mod cache;
mod command;
mod entry_assets;
mod error;
mod error_recording;
mod favicons;
mod fetch;
mod maintenance;
mod parsing;
mod processing;
mod scripting;
mod worker;

pub(crate) use backoff::same_origin;
pub use command::TaskManagerCommand;
pub use error::FetchError;
pub use scripting::{load_script_runner, LoadPluginsError};
pub use worker::{spawn_workers, worker_count};

// Re-exported for use from tests (which reach them via `crate::tasks::*`).
// The #[allow] keeps the `cargo build` / clippy on the lib target green —
// non-test code inside the crate does not consume these symbols through
// this path (it imports them directly from the submodules instead).
#[allow(unused_imports)]
pub(crate) use entry_assets::cache_entry_assets;
#[allow(unused_imports)]
pub(crate) use favicons::cache_feed_favicon;
/// [`fetch::refresh_feed`] with an in-process fetcher around `client` and
/// the default settings, which is how the tests drive it.
#[cfg(test)]
pub(crate) async fn refresh_feed(
    client: &reqwest::Client,
    feed_id: i64,
    pool: crate::db::Db,
    script_runner: Option<&dyn crate::scripting::ScriptRunner>,
    metrics: &crate::metrics::Metrics,
    task_tx: &async_channel::Sender<TaskManagerCommand>,
) -> anyhow::Result<()> {
    let settings = crate::config::Settings::default();
    refresh_feed_with_settings(
        client,
        feed_id,
        pool,
        &settings,
        script_runner,
        metrics,
        task_tx,
    )
    .await
}

/// [`refresh_feed`] with explicit settings, for tests that tune them.
#[cfg(test)]
pub(crate) async fn refresh_feed_with_settings(
    client: &reqwest::Client,
    feed_id: i64,
    pool: crate::db::Db,
    settings: &crate::config::Settings,
    script_runner: Option<&dyn crate::scripting::ScriptRunner>,
    metrics: &crate::metrics::Metrics,
    task_tx: &async_channel::Sender<TaskManagerCommand>,
) -> anyhow::Result<()> {
    refresh_feed_inner(
        client,
        feed_id,
        false,
        pool,
        settings,
        script_runner,
        metrics,
        task_tx,
    )
    .await
}

/// [`refresh_feed`] as a refresh a user asked for, which fetches even when
/// the feed is not yet due.
#[cfg(test)]
pub(crate) async fn refresh_feed_manual(
    client: &reqwest::Client,
    feed_id: i64,
    pool: crate::db::Db,
    metrics: &crate::metrics::Metrics,
    task_tx: &async_channel::Sender<TaskManagerCommand>,
) -> anyhow::Result<()> {
    let settings = crate::config::Settings::default();
    refresh_feed_inner(
        client, feed_id, true, pool, &settings, None, metrics, task_tx,
    )
    .await
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn refresh_feed_inner(
    client: &reqwest::Client,
    feed_id: i64,
    manual: bool,
    pool: crate::db::Db,
    settings: &crate::config::Settings,
    script_runner: Option<&dyn crate::scripting::ScriptRunner>,
    metrics: &crate::metrics::Metrics,
    task_tx: &async_channel::Sender<TaskManagerCommand>,
) -> anyhow::Result<()> {
    let fetcher = test_fetcher(client);
    fetch::refresh_feed(
        &fetcher,
        feed_id,
        manual,
        pool,
        settings,
        script_runner,
        metrics,
        task_tx,
    )
    .await
}
/// An in-process [`crate::fetcher::Fetcher`] that fetches feeds, and
/// downloads assets, with `client` for as long as no proxy is asked for.
#[cfg(test)]
pub(crate) fn test_fetcher(client: &reqwest::Client) -> crate::fetcher::Fetcher {
    use crate::fetcher::assets::{asset_client_builder, AssetTimeouts};
    use crate::fetcher::{client_builder, Fetcher, ProxiedClient};
    Fetcher::InProcess {
        feeds: ProxiedClient::with_client(client.clone(), client_builder),
        assets: ProxiedClient::with_client(client.clone(), || {
            asset_client_builder(AssetTimeouts::DEFAULT)
        }),
    }
}

#[allow(unused_imports)]
pub(crate) use maintenance::run_maintenance;

#[cfg(test)]
mod tests;
