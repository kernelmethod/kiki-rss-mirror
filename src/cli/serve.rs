use crate::sandbox::{self, SandboxConfig};
use crate::server;
use anyhow::{Context, Result};
use clap::Args;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

#[derive(Args)]
pub struct ServeArgs {
    /// Listen on a TCP port
    #[arg(short, long, conflicts_with = "socket_path")]
    port: Option<u16>,

    /// IP address to bind the TCP listener to. Only meaningful with --port.
    #[arg(
        short = 'b',
        long,
        default_value_t = IpAddr::V4(Ipv4Addr::LOCALHOST),
        requires = "port",
        conflicts_with = "socket_path",
    )]
    bind: IpAddr,

    /// Path to the Unix domain socket [default: ./kiki.sock]
    #[arg(short = 'u', long = "uds", conflicts_with = "port")]
    socket_path: Option<PathBuf>,

    /// Disable the OS-level sandbox (Landlock + seccomp-bpf on Linux).
    ///
    /// Only use this if the sandbox is demonstrably causing a failure —
    /// running without it exposes the full filesystem and syscall surface
    /// to any post-exploitation code path.
    #[arg(long)]
    no_sandbox: bool,

    /// Run the seccomp filter in log-only mode instead of killing on
    /// violation. Useful when tightening the denylist or diagnosing an
    /// unexpected SIGSYS in production. Landlock is unaffected.
    #[arg(long)]
    seccomp_log_only: bool,
}

impl ServeArgs {
    pub fn run(&self) -> Result<()> {
        tracing_subscriber::fmt::init();

        let db_path = PathBuf::from("./kiki.db");
        let default_socket = PathBuf::from("./kiki.sock");

        // Resolve the Unix socket path the server will bind to (if any).
        // The sandbox needs this so it can grant write access to the
        // socket's parent directory.
        let uds_path: Option<PathBuf> = match (self.port, &self.socket_path) {
            (Some(_), _) => None,
            (None, Some(path)) => Some(path.clone()),
            (None, None) => Some(default_socket.clone()),
        };

        if !self.no_sandbox {
            let config = build_sandbox_config(&db_path, uds_path.as_deref(), self);
            sandbox::apply(&config).context("failed to install sandbox")?;
        } else {
            tracing::warn!(
                "sandbox disabled via --no-sandbox; process runs with full filesystem \
                 and syscall access"
            );
        }

        let mut builder = server::ServerBuilder::new(&db_path).autofetch();
        builder = match (self.port, &uds_path) {
            (Some(port), _) => builder.bind_addr(SocketAddr::new(self.bind, port)),
            (None, Some(path)) => builder.socket_path(path),
            (None, None) => builder.socket_path(&default_socket),
        };
        let server = builder.build();

        std::thread::spawn(|| server.run())
            .join()
            .map_err(|_| anyhow::anyhow!("panic in server thread"))?
    }
}

/// Build a [`SandboxConfig`] from the CLI arguments and the paths the
/// server will use.
///
/// The sandbox needs read-write access to:
///   * the directory containing the SQLite database (which also contains
///     the `assets/` cache tree), and
///   * the parent directory of the Unix socket, if the server is
///     listening on a UDS (so the socket file can be created/unlinked).
fn build_sandbox_config(
    db_path: &std::path::Path,
    socket_path: Option<&std::path::Path>,
    args: &ServeArgs,
) -> SandboxConfig {
    SandboxConfig {
        data_dir: parent_or_cwd(db_path),
        socket_dir: socket_path.map(parent_or_cwd),
        log_only: args.seccomp_log_only,
    }
}

/// Return the parent directory of `p`, treating a relative path with no
/// parent component (e.g. `kiki.db`) as referring to the current
/// directory.
fn parent_or_cwd(p: &std::path::Path) -> PathBuf {
    p.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}
