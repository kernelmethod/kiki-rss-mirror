mod fetcher;
mod routes;
mod server;

use anyhow::{Context, Result};
use clap::Args;
use std::sync::atomic;
use tokio::sync::mpsc;

#[derive(Args)]
pub struct ServeArgs {}

impl ServeArgs {
    pub fn run(&self) -> Result<()> {
        tracing_subscriber::fmt::init();

        // We create two separate runtimes, one for the feed-fetchers and
        // one for the web service workers.
        //
        // This ensures that feed fetcher threads don't consume all of
        // the resources being used by the server threads.
        let fetcher_runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name_fn(|| {
                static ATOMIC_ID: atomic::AtomicUsize = atomic::AtomicUsize::new(0);
                let id = ATOMIC_ID.fetch_add(1, atomic::Ordering::SeqCst);
                format!("feed-fetcher-{}", id)
            })
            .build()
            .with_context(|| "Failed to build Tokio runtime for feed fetchers")?;
        let server_runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name_fn(|| {
                static ATOMIC_ID: atomic::AtomicUsize = atomic::AtomicUsize::new(0);
                let id = ATOMIC_ID.fetch_add(1, atomic::Ordering::SeqCst);
                format!("server-worker-{}", id)
            })
            .build()
            .with_context(|| "Failed to build Tokio runtime for web service workers")?;

        // Create a channel so that web service workers can send tasks
        // to the feed fetchers
        let (tx, rx) = mpsc::channel(1024);

        fetcher_runtime.spawn(fetcher::manager(rx));
        server_runtime
            .block_on(async { server::server(tx.clone()).await })
            .with_context(|| "Failed to spawn server tasks")?;

        Ok(())
    }
}
