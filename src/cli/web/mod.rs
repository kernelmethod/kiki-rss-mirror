mod api;
mod assets;
mod entries;
mod feeds;
mod layout;
mod listing;
mod plugins;
mod sanitize;
mod server;
mod settings;
mod tags;
#[cfg(test)]
mod tests;

use crate::cli::serve::ServeArgs;
use crate::sandbox::{self, SandboxConfig};
use anyhow::{anyhow, bail, Context, Result};
use clap::Args;
use server::{api_client, serve_ui, shutdown_signal, stop_server, ServerProcess};
use std::net::SocketAddr;
use std::path::Path;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// Base URL for requests to the Kiki API. The client sends every request
/// over the server's Unix socket, so the host is never resolved and only
/// fills the `Host` header.
const API_BASE: &str = "http://kiki";

/// Arguments for the `kiki web` subcommand.
///
/// `kiki web` runs two things side by side: the Kiki server, listening on
/// its Unix socket exactly as `kiki serve` would, and a small HTTP server
/// for the web UI. The Kiki server runs as a separate `kiki serve` child
/// process rather than in this one: its sandbox applies to the whole
/// process and denies `execve`, and the web UI should neither live under
/// that profile nor loosen it. The web UI gets a sandbox of its own, which
/// grants it no filesystem access and no network access but its listener
/// and the server's socket; `--no-sandbox` lifts both.
///
/// The browser never talks to the Kiki API. The web UI is the API's only
/// client: it calls the server over the Unix socket and renders what it
/// gets back, so the socket stays the API's only way in.
#[derive(Args)]
pub struct WebArgs {
    /// Address the web UI listens on
    #[arg(
        short = 'l',
        long = "listen",
        value_name = "ADDR",
        default_value = "127.0.0.1:8080"
    )]
    listen: SocketAddr,

    #[command(flatten)]
    serve: ServeArgs,
}

impl WebArgs {
    /// Run the `web` subcommand.
    ///
    /// Returns once the web UI and the Kiki server have both shut down,
    /// either because this process was asked to stop (Ctrl+C or
    /// `SIGTERM`) or because the Kiki server exited on its own.
    ///
    /// # Errors
    ///
    /// Returns an error if the Kiki server cannot be started or exits
    /// unsuccessfully, if the web UI cannot bind to its address, or if its
    /// sandbox cannot be installed.
    pub fn run(&self) -> Result<()> {
        crate::cli::init_logging(std::io::stdout);

        // Everything that needs the filesystem, a new listening socket, or
        // `execve` happens here, before the sandbox forbids it. That has to
        // be before the tokio runtime too: Landlock restricts only the
        // thread that installs it and the threads that thread starts later,
        // and a multi-threaded runtime starts its workers as it is built.

        // Resolve the socket here and hand it to the child, rather than
        // letting each process resolve it on its own, so the web UI is
        // certain to connect to the server it started.
        let socket_path = self.serve.socket_path()?;
        let api = api_client(&socket_path)?;

        let listener = std::net::TcpListener::bind(self.listen)
            .with_context(|| format!("unable to bind the web UI to {}", self.listen))?;
        listener.set_nonblocking(true)?;
        tracing::info!("web UI listening on http://{}", listener.local_addr()?);

        let server = self.spawn_server(&socket_path)?;

        // Counted before the sandbox hides the cgroup files that bound it.
        let workers = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);

        self.apply_sandbox()?;

        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .enable_all()
            .build()?
            .block_on(run_async(listener, api, server))
    }

    /// Install the web UI's sandbox, unless `--no-sandbox` was given.
    ///
    /// The web UI has no use for the filesystem, and none for the network
    /// beyond the listener it already has and the server's Unix socket, so
    /// the [`SandboxProfile::WebUi`](crate::sandbox::SandboxProfile::WebUi)
    /// profile denies it both. It must go up after the server has been
    /// spawned, since it denies `execve`; the server is outside it, and
    /// installs its own.
    fn apply_sandbox(&self) -> Result<()> {
        if self.serve.no_sandbox() {
            tracing::warn!(
                "sandbox disabled via --no-sandbox; the web UI runs with full filesystem \
                 and syscall access"
            );
            return Ok(());
        }
        let config = SandboxConfig::web_ui(self.serve.seccomp_log_only());
        sandbox::apply(&config).context("failed to install the web UI sandbox")
    }

    /// Start `kiki serve` as a child process listening on `socket_path`,
    /// passing on the server flags this command was given.
    ///
    /// The child gets its own process group, so a Ctrl+C at the terminal
    /// reaches only this process, which then stops the server itself. That
    /// keeps shutdown in one order no matter how it was asked for. (On
    /// Windows, a new process group also ignores Ctrl+C, but still hears the
    /// Ctrl+Break [`stop_server`] sends it.)
    ///
    /// On Linux the child is also sent `SIGTERM` if this process dies
    /// without stopping it, e.g. when it is killed with `SIGKILL`, so the
    /// server shuts down rather than living on, orphaned, holding the
    /// database and the socket. The signal follows the thread that spawned
    /// the child, so this must run on the main thread, which lives as long
    /// as the process does.
    fn spawn_server(&self, socket_path: &Path) -> Result<ServerProcess> {
        let exe = std::env::current_exe().context("locating the kiki executable")?;
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("serve").args(self.serve.to_argv(socket_path));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt;
            let parent = std::process::id();
            // SAFETY: the closure runs between fork and exec, where only
            // async-signal-safe calls are permitted; `prctl` and `getppid`
            // are plain syscalls that neither allocate nor take a lock.
            unsafe {
                cmd.pre_exec(move || {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    // Checked after the `prctl`, in case this process died
                    // before it took effect.
                    if libc::getppid() as u32 != parent {
                        return Err(std::io::Error::other("the web UI has exited"));
                    }
                    Ok(())
                });
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP);
        }
        let child = cmd.spawn().context("failed to start the Kiki server")?;
        tracing::info!(pid = child.id(), "started Kiki server");
        Ok(ServerProcess(child))
    }
}

/// Serve the web UI on `listener` alongside the Kiki `server`, until one of
/// them stops.
async fn run_async(
    listener: std::net::TcpListener,
    api: reqwest::Client,
    mut server: ServerProcess,
) -> Result<()> {
    let listener = TcpListener::from_std(listener)?;
    let cancel = CancellationToken::new();
    let web = tokio::spawn(serve_ui(listener, api, cancel.clone()));

    let status = tokio::select! {
        status = server.wait() => {
            let status = status.context("failed to wait on the Kiki server")?;
            tracing::warn!(%status, "Kiki server exited; stopping the web UI");
            Some(status)
        }
        _ = shutdown_signal() => None,
    };

    cancel.cancel();
    let status = match status {
        Some(status) => status,
        None => stop_server(&mut server).await?,
    };
    web.await.map_err(|_| anyhow!("panic in web UI task"))??;

    if !status.success() {
        bail!("Kiki server exited with {status}");
    }
    Ok(())
}
