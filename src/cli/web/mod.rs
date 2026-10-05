mod api;
mod assets;
mod entries;
mod feeds;
mod hosts;
mod layout;
mod listing;
mod login;
#[cfg(test)]
mod login_tests;
mod plugins;
mod sanitize;
mod server;
mod settings;
mod tags;
#[cfg(test)]
mod tests;

use crate::cli::paths::{self, Env};
use crate::cli::serve::ServeArgs;
use crate::config::{self, AnonymousAccess, ConfigStore, HostPattern};
use crate::sandbox::{self, SandboxConfig};
use anyhow::{anyhow, bail, Context, Result};
use clap::Args;
use hosts::AllowedHosts;
use login::Gate;
use server::{api_client, serve_ui_with, shutdown_signal, stop_server, ServerProcess};
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

    /// Also answer requests for HOST, besides `localhost`, `127.0.0.1` and
    /// `::1`, and any listed in `kiki.toml`'s `web_ui.allowed_hosts`. May
    /// be given more than once. `*.example.com` allows every
    /// subdomain of `example.com`, and `*` allows any host at all.
    ///
    /// The web UI refuses requests whose `Host` header names any other
    /// host, so that a site cannot reach it by pointing its own domain at
    /// this machine (DNS rebinding). When the web UI is reached by another
    /// name, such as `kiki.lan` or a LAN address, give that name here.
    #[arg(long = "allowed-host", value_name = "HOST")]
    allowed_hosts: Vec<HostPattern>,

    /// Require logging in with an API token, created with `kiki token
    /// create`, before using the web UI. Each person can then do only what
    /// their token's scopes allow. Also set by `kiki.toml`'s
    /// `web_ui.require_login`.
    #[arg(long = "require-login")]
    require_login: bool,

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
        let settings = configured_settings()?;
        let allowed_hosts = self.allowed_hosts(&settings.web_ui.allowed_hosts);
        let anonymous = settings.api.anonymous_access;
        let gate = if self.require_login || settings.web_ui.require_login {
            tracing::info!("the web UI requires logging in with an API token");
            Gate::login_required(api)
        } else if anonymous == AnonymousAccess::TokenRequired {
            // Without a token, the web UI could show nothing at all.
            tracing::info!(
                "the web UI requires logging in with an API token, since api.anonymous_access \
                 is \"token-required\""
            );
            Gate::login_required(api)
        } else {
            if !self.listen.ip().is_loopback() {
                let what = match anonymous {
                    AnonymousAccess::ReadOnly => "read everything",
                    _ => "do anything",
                };
                tracing::warn!(
                    "the web UI listens on {} and does not require logging in, so anyone who \
                     can reach it can {what}; see --require-login",
                    self.listen
                );
            }
            Gate::anonymous(api, anonymous.scopes())
        };

        let server = self.spawn_server(&socket_path)?;

        // Counted before the sandbox hides the cgroup files that bound it.
        let workers = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);

        self.apply_sandbox()?;

        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .enable_all()
            .build()?
            .block_on(run_async(listener, gate, allowed_hosts, server))
    }

    /// The hosts the web UI answers to: those given to `--allowed-host`,
    /// and `configured`, from the config file's `web_ui.allowed_hosts`.
    /// Warns when that leaves the web UI open to DNS rebinding, or when it
    /// listens beyond loopback but can only be reached as `localhost`.
    fn allowed_hosts(&self, configured: &[HostPattern]) -> AllowedHosts {
        let mut patterns = self.allowed_hosts.clone();
        patterns.extend_from_slice(configured);
        let allowed = AllowedHosts::new(patterns);
        if allowed.allows_any() {
            tracing::warn!(
                "the web UI answers to any host name ('*' in --allowed-host or \
                 web_ui.allowed_hosts), so other sites may reach it through DNS rebinding"
            );
        } else if allowed.only_localhost() && !self.listen.ip().is_loopback() {
            tracing::warn!(
                "the web UI listens on {} but only answers to localhost; add the name or \
                 address it is reached at with --allowed-host or web_ui.allowed_hosts",
                self.listen
            );
        }
        allowed
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

/// The settings in `kiki.toml` in the data directory, where the `kiki
/// serve` child reads its own settings. Read once, before the sandbox
/// hides the file, for the `web_ui` settings and `api.anonymous_access`.
///
/// # Errors
///
/// Returns an error if the data directory cannot be found, or if the
/// config file cannot be read or holds an invalid setting.
fn configured_settings() -> Result<config::Settings> {
    let data_dir = paths::resolve_data_dir(&Env::from_process())?;
    let path = data_dir.path.join(config::CONFIG_FILE_NAME);
    let store =
        ConfigStore::open(&path).with_context(|| format!("failed to load config file {path:?}"))?;
    Ok(store.current().as_ref().clone())
}

/// Serve the web UI on `listener` alongside the Kiki `server`, until one of
/// them stops.
async fn run_async(
    listener: std::net::TcpListener,
    gate: Gate,
    allowed_hosts: AllowedHosts,
    mut server: ServerProcess,
) -> Result<()> {
    let listener = TcpListener::from_std(listener)?;
    let cancel = CancellationToken::new();
    let web = tokio::spawn(serve_ui_with(listener, gate, allowed_hosts, cancel.clone()));

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
