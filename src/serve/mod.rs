mod fetcher;
mod routes;
mod server;

use anyhow::{Context, Result};
use clap::Args;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::OpenFlags;
use std::{fs, path::PathBuf, sync::atomic};
use tokio::sync::mpsc;

#[derive(Args)]
pub struct ServeArgs {}

impl ServeArgs {
    pub fn run(&self) -> Result<()> {
        tracing_subscriber::fmt::init();

        // Create a pool of connections that can be shared between all of
        // the threads that we spawn.
        let path = PathBuf::from("./kiki.db");
        let manager = SqliteConnectionManager::file(&path)
            .with_flags(OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .with_init(|c| c.execute_batch("PRAGMA foreign_keys=ON;"));
        let pool = r2d2::Pool::new(manager)
            .with_context(|| format!("Unable to open connection pool to database at {:?}", path))?;

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

        // Create Unix socket for the server listener
        let socket_path = PathBuf::from("kiki.sock");
        if socket_path.exists() {
            let _ = fs::remove_file(&socket_path).with_context(|| {
                format!(
                    "Unable to delete existing socket file from {:?}",
                    &socket_path
                )
            })?;
        }

        fetcher_runtime.spawn(fetcher::manager(rx, pool.clone()));
        server_runtime
            .block_on(async { server::server(&socket_path, tx.clone(), pool.clone()).await })
            .with_context(|| "Failed to spawn server tasks")?;

        Ok(())
    }
}
