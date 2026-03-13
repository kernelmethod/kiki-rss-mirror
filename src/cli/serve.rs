use crate::server;
use anyhow::Result;
use clap::Args;
use std::path::PathBuf;

#[derive(Args)]
pub struct ServeArgs {
    /// Listen on a localhost TCP port
    #[arg(short, long, conflicts_with = "socket_path")]
    port: Option<u16>,

    /// Path to the Unix domain socket [default: ./kiki.sock]
    #[arg(short = 'u', long = "uds", conflicts_with = "port")]
    socket_path: Option<PathBuf>,
}

impl ServeArgs {
    pub fn run(&self) -> Result<()> {
        tracing_subscriber::fmt::init();

        let db_path = PathBuf::from("./kiki.db");
        let socket_path = self
            .socket_path
            .clone()
            .unwrap_or_else(|| PathBuf::from("./kiki.sock"));
        let mut builder = server::ServerBuilder::new(&db_path);
        builder = builder.autofetch();
        if let Some(port) = self.port {
            builder = builder.port(port);
        } else {
            builder = builder.socket_path(&socket_path);
        }
        let server = builder.build();

        std::thread::spawn(|| server.run())
            .join()
            .map_err(|_| anyhow::anyhow!("panic in server thread"))?
    }
}
