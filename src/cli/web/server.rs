use super::assets::asset;
use super::entries::{
    add_entry_system_tag, entry_page, index, mark_entries_read, remove_entry_system_tag,
    search_page,
};
use super::feeds::{feed_page, feeds_page};
use super::hosts::{check_host, AllowedHosts};
use super::login::{self, log_in, log_out, show_login, Gate};
use super::plugins::{plugin_page, plugins_page, update_plugin_config};
use super::settings_page::settings_page;
use super::tags::{delete_tag, tag_page, tags_page};
use anyhow::{bail, Context, Result};
use axum::{
    middleware,
    routing::{get, post, put},
    Router,
};
use std::path::Path;
use std::process::ExitStatus;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::signal;
use tokio_util::sync::CancellationToken;

/// The `kiki serve` child of `kiki web`.
///
/// A plain [`std::process::Child`] rather than a tokio one, because it is
/// spawned before the sandbox goes up, and so before there is a runtime to
/// spawn it on. It is killed if dropped while still running, so an error
/// in the web UI never leaves an orphaned server behind.
pub(super) struct ServerProcess(pub(super) std::process::Child);

impl ServerProcess {
    /// Wait for the server to exit, without blocking the runtime.
    ///
    /// Only this handle ever reaps the child, so until this returns its pid
    /// cannot be reused, and [`stop_server`] can safely signal it.
    #[cfg(unix)]
    pub(super) async fn wait(&mut self) -> Result<ExitStatus> {
        // Listen for SIGCHLD before checking, so an exit between the check
        // and the wait still wakes us.
        let mut sigchld = signal::unix::signal(signal::unix::SignalKind::child())?;
        loop {
            if let Some(status) = self.0.try_wait()? {
                return Ok(status);
            }
            if sigchld.recv().await.is_none() {
                bail!("SIGCHLD stream closed while waiting on the Kiki server");
            }
        }
    }

    /// See the `unix` variant. Windows has no `SIGCHLD`, so this polls.
    #[cfg(windows)]
    pub(super) async fn wait(&mut self) -> Result<ExitStatus> {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
        loop {
            tick.tick().await;
            if let Some(status) = self.0.try_wait()? {
                return Ok(status);
            }
        }
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        if let Ok(None) = self.0.try_wait() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

/// Build a client that sends every request to the Kiki API over the Unix
/// socket at `socket_path`.
pub(super) fn api_client(socket_path: &Path) -> Result<reqwest::Client> {
    crate::http::unix_socket_client(socket_path, None)
        .context("failed to build the Kiki API client")
}

/// Serve the web UI on `listener` until `cancel` fires, talking to the Kiki
/// API through `api` with the full access of its socket, and answering
/// only requests for `allowed_hosts`.
#[cfg(test)]
pub(super) async fn serve_ui(
    listener: TcpListener,
    api: reqwest::Client,
    allowed_hosts: AllowedHosts,
    cancel: CancellationToken,
) -> Result<()> {
    serve_ui_with(listener, Gate::open(api), allowed_hosts, cancel).await
}

/// Serve the web UI on `listener` until `cancel` fires, letting in those
/// `gate` lets in, and answering only requests for `allowed_hosts`.
pub(super) async fn serve_ui_with(
    listener: TcpListener,
    gate: Gate,
    allowed_hosts: AllowedHosts,
    cancel: CancellationToken,
) -> Result<()> {
    let gate = Arc::new(gate);
    let app = Router::new()
        .route("/", get(index))
        .route("/login", get(show_login).post(log_in))
        .route("/logout", post(log_out))
        .route("/entries/{id}", get(entry_page))
        .route("/entries/read", post(mark_entries_read))
        .route(
            "/entries/{id}/system-tags/{name}",
            put(add_entry_system_tag).delete(remove_entry_system_tag),
        )
        .route("/feeds", get(feeds_page))
        .route("/feeds/{id}", get(feed_page))
        .route("/tags", get(tags_page))
        .route("/tags/{id}", get(tag_page).delete(delete_tag))
        .route("/search", get(search_page))
        .route("/settings", get(settings_page))
        .route("/plugins", get(plugins_page))
        .route("/plugins/{name}", get(plugin_page))
        .route("/plugins/{name}/config", post(update_plugin_config))
        .route("/assets/{hash}", get(asset))
        .with_state(gate.clone())
        .layer(middleware::from_fn_with_state(gate, login::gate))
        .layer(middleware::from_fn_with_state(
            Arc::new(allowed_hosts),
            check_host,
        ));
    axum::serve(listener, app)
        .with_graceful_shutdown(cancel.cancelled_owned())
        .await
        .context("error encountered while running the web UI")
}

/// Ask the Kiki server to shut down gracefully, and wait for it to exit.
///
/// The request is a `SIGTERM` on Unix and a Ctrl+Break on Windows; see
/// [`crate::server::terminate_signal`].
pub(super) async fn stop_server(server: &mut ServerProcess) -> Result<ExitStatus> {
    if server.0.try_wait()?.is_none() {
        request_termination(&mut server.0)?;
    }
    server
        .wait()
        .await
        .context("failed to wait on the Kiki server")
}

/// Send `child` a `SIGTERM`.
#[cfg(unix)]
pub(super) fn request_termination(child: &mut std::process::Child) -> Result<()> {
    let pid = libc::pid_t::try_from(child.id()).context("Kiki server pid out of range")?;
    // SAFETY: kill(2) has no memory-safety preconditions. The child has
    // not been reaped yet (the caller's `try_wait` returned `None`, and
    // only `ServerProcess` reaps it), so the pid still names it and cannot
    // have been reused.
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        tracing::warn!(
            error = %std::io::Error::last_os_error(),
            "failed to signal the Kiki server"
        );
    }
    Ok(())
}

/// Send Ctrl+Break to `child`'s process group, which [`spawn_server`]
/// made it the leader of, so the group id is its pid. That only works
/// while the two share a console; without one, kill it instead.
///
/// [`spawn_server`]: WebArgs::spawn_server
#[cfg(windows)]
pub(super) fn request_termination(child: &mut std::process::Child) -> Result<()> {
    use windows_sys::Win32::System::Console::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};

    // SAFETY: GenerateConsoleCtrlEvent takes no pointers. The child has
    // not been reaped, so its process group id cannot have been reused.
    if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.id()) } == 0 {
        tracing::warn!(
            error = %std::io::Error::last_os_error(),
            "failed to send Ctrl+Break to the Kiki server; killing it instead"
        );
        child.kill().context("failed to kill the Kiki server")?;
    }
    Ok(())
}

/// Resolve on Ctrl+C, or on whatever [`crate::server::terminate_signal`]
/// waits for.
pub(super) async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = signal::ctrl_c().await {
            tracing::error!("failed to install Ctrl+C handler: {e:?}");
            std::future::pending::<()>().await;
        }
    };

    tokio::select! {
        _ = ctrl_c => {}
        _ = crate::server::terminate_signal() => {}
    }
}
