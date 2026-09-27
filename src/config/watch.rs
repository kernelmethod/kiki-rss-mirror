//! Reloading the config file when it changes on disk.
//!
//! The watch is on the file's *directory*, not the file. Every save —
//! Kiki's own, and most editors' — writes a new file and renames it over the
//! old one, which would silently orphan a watch on the old file's inode. A
//! directory watch also sees the file being created after startup.
use super::ConfigHandle;
use notify::event::{AccessKind, AccessMode};
use notify::{EventKind, RecursiveMode, Watcher};
use std::path::Path;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// How long the file must go without further events before it is
/// reloaded, so that a burst of events from one save triggers one reload.
const DEBOUNCE: Duration = Duration::from_millis(200);

/// Starts watching the config file and reloads `store` whenever it changes,
/// until `cancel` fires.
///
/// An invalid edit is logged and ignored: the last good settings stay in
/// force. Saves made through `store` itself also trigger a reload, which is
/// a no-op since the settings already match.
///
/// Must be called from within a Tokio runtime.
///
/// # Errors
///
/// Returns an error if the platform's file watcher cannot be created or
/// the config directory cannot be watched.
pub fn spawn_watcher(store: ConfigHandle, cancel: CancellationToken) -> anyhow::Result<()> {
    let path = store.path().to_path_buf();
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => Path::new(".").to_path_buf(),
    };
    let Some(file_name) = path.file_name().map(|n| n.to_os_string()) else {
        anyhow::bail!("config path {:?} has no file name", path);
    };

    let (tx, mut rx) = mpsc::unbounded_channel::<()>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let event = match res {
            Ok(event) => event,
            Err(e) => {
                warn!("config watcher error: {:?}", e);
                return;
            }
        };
        // A rescan means events were dropped (e.g. the inotify queue
        // overflowed under SQLite write load in the same directory) and
        // carries no paths, so it cannot be filtered by name. Reloading is
        // idempotent, so treat it as a possible change.
        let about_config = event
            .paths
            .iter()
            .any(|p| p.file_name() == Some(file_name.as_os_str()));
        if event.need_rescan() || (is_change(&event.kind) && about_config) {
            let _ = tx.send(());
        }
    })?;
    watcher.watch(&dir, RecursiveMode::NonRecursive)?;

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
                    reload(&store).await;
                }
            }
        }
    });
    Ok(())
}

/// Whether `kind` can change the file's contents.
///
/// Reads must be excluded: on Linux, reading the file raises access
/// events, so reacting to them would make every reload trigger the next.
fn is_change(kind: &EventKind) -> bool {
    match kind {
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        EventKind::Access(_) => false,
        _ => true,
    }
}

async fn reload(store: &ConfigHandle) {
    let store = store.clone();
    let path = store.path().to_path_buf();
    match tokio::task::spawn_blocking(move || store.reload()).await {
        Ok(Ok(true)) if !path.exists() => warn!(
            path = %path.display(),
            "config file removed; settings reset to the built-in defaults"
        ),
        Ok(Ok(true)) => info!(path = %path.display(), "reloaded config"),
        Ok(Ok(false)) => debug!("config file changed on disk but settings are unchanged"),
        Ok(Err(e)) => error!(
            "ignoring invalid config file; previous settings stay in force: {:#}",
            anyhow::Error::from(e)
        ),
        Err(e) => error!("config reload task failed: {:?}", e),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::config::ConfigStore;
    use std::sync::Arc;
    use std::time::Instant;
    use tempdir::TempDir;

    async fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    /// Regression: reloading reads the file, and treating that read as a
    /// change made the watcher reload in a loop.
    #[test]
    fn reads_are_not_changes() {
        use notify::event::{CreateKind, ModifyKind, RenameMode};

        assert!(!is_change(&EventKind::Access(AccessKind::Open(
            AccessMode::Any
        ))));
        assert!(!is_change(&EventKind::Access(AccessKind::Read)));
        assert!(!is_change(&EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));

        assert!(is_change(&EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));
        assert!(is_change(&EventKind::Modify(ModifyKind::Any)));
        assert!(is_change(&EventKind::Modify(ModifyKind::Name(
            RenameMode::To
        ))));
        assert!(is_change(&EventKind::Create(CreateKind::File)));
    }

    #[tokio::test]
    async fn edits_on_disk_are_picked_up() {
        let td = TempDir::new("kiki_watch").unwrap();
        let store = Arc::new(ConfigStore::open(td.path().join("kiki.toml")).unwrap());
        let cancel = CancellationToken::new();
        spawn_watcher(store.clone(), cancel.clone()).unwrap();

        // Created after the watch started, and replaced by rename as a
        // second edit, to cover both of the cases a file watch would miss.
        std::fs::write(store.path(), "[feed_fetch]\ntimeout_seconds = 5\n").unwrap();
        assert!(
            wait_for(|| store.current().feed_fetch.timeout_seconds == 5).await,
            "new file was not picked up"
        );

        let tmp = td.path().join("replacement");
        std::fs::write(&tmp, "[feed_fetch]\ntimeout_seconds = 9\n").unwrap();
        std::fs::rename(&tmp, store.path()).unwrap();
        assert!(
            wait_for(|| store.current().feed_fetch.timeout_seconds == 9).await,
            "renamed-over file was not picked up"
        );

        cancel.cancel();
    }
}
