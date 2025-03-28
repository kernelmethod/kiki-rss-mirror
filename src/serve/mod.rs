mod fetcher;
mod server;

use anyhow::{Context, Result};
use clap::Args;
use std::{sync::atomic, thread};

#[derive(Args)]
pub struct ServeArgs {}

impl ServeArgs {
    pub fn run(&self) -> Result<()> {
        tracing_subscriber::fmt::init();
        let fetcher_handle = thread::Builder::new()
            .name("fetcher".to_string())
            .spawn(|| fetcher::fetcher())
            .with_context(|| "Failed to spawn fetcher thread")?;
        let server_handle = thread::Builder::new()
            .name("server".to_string())
            .spawn(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .thread_name_fn(|| {
                        static ATOMIC_ID: atomic::AtomicUsize = atomic::AtomicUsize::new(0);
                        let id = ATOMIC_ID.fetch_add(1, atomic::Ordering::SeqCst);
                        format!("server-{}", id)
                    })
                    .build()
                    .unwrap()
                    .block_on(server::server())
            })
            .with_context(|| "Failed to spawn server thread")?;

        let _ = fetcher_handle.join().unwrap();
        let _ = server_handle.join().unwrap();

        Ok(())
    }
}


