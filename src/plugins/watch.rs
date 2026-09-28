//! Reloading plugins when the plugins directory changes on disk.
//!
//! The whole plugins directory is watched recursively, so installing,
//! removing, or editing a plugin all trigger a reload, the same one
//! `POST /v1/plugins/reload` queues.
use crate::config::watch::is_change;
use notify::{RecursiveMode, Watcher};
use std::path::Path;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// How long the plugins directory must go without further events before
/// plugins are reloaded, so that copying a plugin in triggers one reload
/// rather than one per file.
const DEBOUNCE: Duration = Duration::from_millis(500);

/// Starts watching `plugins_dir`, sending on `reload_tx` whenever anything
/// inside it changes, until `cancel` fires.
///
/// Must be called from within a Tokio runtime.
///
/// # Errors
///
/// Returns an error if the platform's file watcher cannot be created or
/// `plugins_dir` cannot be watched, for instance because it does not exist.
pub fn spawn_watcher(
    plugins_dir: &Path,
    reload_tx: tokio::sync::watch::Sender<()>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<()>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let event = match res {
            Ok(event) => event,
            Err(e) => {
                warn!("plugin watcher error: {:?}", e);
                return;
            }
        };
        // Reloading reads every plugin's files, so reads must not count as
        // changes or each reload would trigger the next.
        if event.need_rescan() || is_change(&event.kind) {
            let _ = tx.send(());
        }
    })?;
    watcher.watch(plugins_dir, RecursiveMode::Recursive)?;

    tokio::spawn(async move {
        // Dropping the watcher stops the watch, so it lives in this task.
        let _watcher = watcher;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                event = rx.recv() => {
                    if event.is_none() {
                        break;
                    }
                    // Wait for the burst to go quiet.
                    while let Ok(Some(())) = tokio::time::timeout(DEBOUNCE, rx.recv()).await {}
                    debug!("plugins directory changed; queueing a reload");
                    if reload_tx.send(()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn changes_queue_a_reload() {
        let td = TempDir::with_prefix("kiki_plugins").unwrap();
        let (tx, mut rx) = tokio::sync::watch::channel(());
        rx.mark_unchanged();
        let cancel = CancellationToken::new();
        spawn_watcher(td.path(), tx, cancel.clone()).unwrap();

        let dir = td.path().join("a");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("main.lua"), "-- a").unwrap();

        tokio::time::timeout(Duration::from_secs(10), rx.changed())
            .await
            .expect("no reload was queued")
            .unwrap();

        cancel.cancel();
    }
}
