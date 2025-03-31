use crate::server;
use anyhow::Result;
use clap::Args;
use std::path::PathBuf;

#[derive(Args)]
pub struct ServeArgs {}

impl ServeArgs {
    pub fn run(&self) -> Result<()> {
        tracing_subscriber::fmt::init();

        let db_path = PathBuf::from("./kiki.db");
        let socket_path = PathBuf::from("./kiki.sock");
        let server = server::ServerBuilder::new(&db_path)
            .socket_path(&socket_path)
            .build();

        std::thread::spawn(|| server.run())
            .join()
            .expect("panic in server thread")
    }
}
