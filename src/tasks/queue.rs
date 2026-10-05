//! The task queue: the sending side, which leaves out feed refreshes that
//! are already queued, and the receiving side, which workers take commands
//! from.
//!
//! The queue the server runs has two lanes. Asset caching
//! ([`TaskManagerCommand::CacheEntryAssets`] and
//! [`TaskManagerCommand::CacheFeedFavicon`]) goes in a lane of its own with
//! no bound, since a refresh that brings in hundreds of new entries queues
//! a command for each of them, and in a bounded queue shared with
//! everything else they would either be dropped once it filled or keep
//! feed refreshes out. Workers take from the asset lane only when the main
//! lane is empty. See [`queue`].

use crate::tasks::command::TaskManagerCommand;
use async_channel::{Receiver, RecvError, SendError, Sender, TrySendError};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// What became of a command handed to [`TaskSender::send`] or
/// [`TaskSender::try_send`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enqueue {
    /// The command was added to the queue.
    Queued,
    /// The command is a feed refresh that the queue already holds, or
    /// holds a manual refresh of the same feed, which does everything it
    /// would; it was dropped.
    AlreadyQueued,
}

/// Create the server's task queue: a main lane that holds up to
/// `capacity` commands, and an asset lane with no bound (see the module
/// documentation).
///
/// # Examples
///
/// ```
/// use kiki_rss::tasks::{queue, TaskManagerCommand};
///
/// # tokio_test_block_on(async {
/// let (tx, rx) = queue(1);
/// tx.try_send(TaskManagerCommand::OptimizeFts).unwrap();
/// // The main lane is full, but asset caching has a lane of its own...
/// for entry_id in 0..100 {
///     tx.try_send(TaskManagerCommand::CacheEntryAssets { entry_id }).unwrap();
/// }
/// // ...which is only served once the main lane is empty.
/// assert!(matches!(rx.recv().await.unwrap(), TaskManagerCommand::OptimizeFts));
/// assert!(matches!(
///     rx.recv().await.unwrap(),
///     TaskManagerCommand::CacheEntryAssets { entry_id: 0 }
/// ));
/// # });
/// # fn tokio_test_block_on<F: std::future::Future>(f: F) -> F::Output {
/// #     tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(f)
/// # }
/// ```
pub fn queue(capacity: usize) -> (TaskSender, TaskReceiver) {
    let (tx, rx) = async_channel::bounded(capacity);
    let (assets_tx, assets_rx) = async_channel::unbounded();
    let mut sender = TaskSender::from(tx);
    sender.assets = Some(assets_tx);
    (
        sender,
        TaskReceiver {
            rx,
            assets: Some(assets_rx),
        },
    )
}

/// Whether `command` goes in the asset lane.
fn is_asset_caching(command: &TaskManagerCommand) -> bool {
    matches!(
        command,
        TaskManagerCommand::CacheEntryAssets { .. } | TaskManagerCommand::CacheFeedFavicon { .. }
    )
}

/// The sending side of the task queue: an [`async_channel::Sender`] that
/// drops a [`TaskManagerCommand::RefreshFeed`] when the queue already
/// holds one that does the same.
///
/// A feed is refreshed once however many times it is asked for while it
/// waits, so a burst of "refresh everything" requests, or a scheduler tick
/// while the workers are behind, cannot fill the queue with copies of the
/// same refreshes and keep everything else out. A manual refresh is still
/// queued behind a scheduled one, since it fetches a feed that is not yet
/// due; a scheduled refresh behind a manual one is dropped.
///
/// Workers call [`TaskSender::dequeued`] with each command they take off
/// the queue, so a refresh asked for while the same feed is being
/// refreshed is queued again.
///
/// One made with [`From`] has a single lane, which asset caching shares
/// with everything else; [`queue`] makes one with an asset lane.
///
/// # Examples
///
/// ```
/// use kiki_rss::tasks::{Enqueue, TaskManagerCommand, TaskSender};
///
/// let (tx, rx) = async_channel::bounded(8);
/// let tx = TaskSender::from(tx);
/// let refresh = TaskManagerCommand::RefreshFeed { feed_id: 1, manual: false };
/// assert_eq!(tx.try_send(refresh.clone()).unwrap(), Enqueue::Queued);
/// assert_eq!(tx.try_send(refresh.clone()).unwrap(), Enqueue::AlreadyQueued);
///
/// let taken = rx.try_recv().unwrap();
/// tx.dequeued(&taken);
/// assert_eq!(tx.try_send(refresh).unwrap(), Enqueue::Queued);
/// ```
#[derive(Clone, Debug)]
pub struct TaskSender {
    tx: Sender<TaskManagerCommand>,
    /// The asset lane, if there is one.
    assets: Option<Sender<TaskManagerCommand>>,
    /// The `(feed_id, manual)` of each refresh in the queue.
    refreshes: Arc<Mutex<HashSet<(i64, bool)>>>,
}

impl From<Sender<TaskManagerCommand>> for TaskSender {
    fn from(tx: Sender<TaskManagerCommand>) -> Self {
        Self {
            tx,
            assets: None,
            refreshes: Arc::default(),
        }
    }
}

impl TaskSender {
    /// The lane `command` goes in.
    fn lane(&self, command: &TaskManagerCommand) -> &Sender<TaskManagerCommand> {
        match &self.assets {
            Some(assets) if is_asset_caching(command) => assets,
            _ => &self.tx,
        }
    }

    /// Queue `command`, waiting for room if the queue is full.
    ///
    /// # Errors
    ///
    /// Fails, handing `command` back, if the queue has been closed.
    pub async fn send(
        &self,
        command: TaskManagerCommand,
    ) -> Result<Enqueue, SendError<TaskManagerCommand>> {
        let Some(key) = self.claim(&command) else {
            return Ok(Enqueue::AlreadyQueued);
        };
        self.lane(&command)
            .send(command)
            .await
            .inspect_err(|_| self.release(key))?;
        Ok(Enqueue::Queued)
    }

    /// Queue `command` if there is room for it now. There is always room
    /// in the asset lane.
    ///
    /// # Errors
    ///
    /// Fails, handing `command` back, if the queue is full or has been
    /// closed.
    pub fn try_send(
        &self,
        command: TaskManagerCommand,
    ) -> Result<Enqueue, TrySendError<TaskManagerCommand>> {
        let Some(key) = self.claim(&command) else {
            return Ok(Enqueue::AlreadyQueued);
        };
        self.lane(&command)
            .try_send(command)
            .inspect_err(|_| self.release(key))?;
        Ok(Enqueue::Queued)
    }

    /// Note that a worker has taken `command` off the queue, so that the
    /// same refresh can be queued again.
    pub fn dequeued(&self, command: &TaskManagerCommand) {
        if let Some(key) = refresh_key(command) {
            self.release(Some(key));
        }
    }

    /// Close the queue, both lanes, so that nothing more can be sent on it
    /// and workers stop once they have taken what is in it.
    pub fn close(&self) -> bool {
        let assets = self.assets.as_ref().is_some_and(Sender::close);
        self.tx.close() || assets
    }

    /// The number of commands in the queue, in both lanes.
    pub fn len(&self) -> usize {
        self.tx.len() + self.asset_queue_len().unwrap_or(0)
    }

    /// Whether the queue is empty, in both lanes.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The most commands the main lane holds, or `None` if it is
    /// unbounded. The asset lane has no bound.
    pub fn capacity(&self) -> Option<usize> {
        self.tx.capacity()
    }

    /// The number of commands in the asset lane, or `None` if there is no
    /// asset lane.
    pub fn asset_queue_len(&self) -> Option<usize> {
        self.assets.as_ref().map(Sender::len)
    }

    /// Record `command` as queued if it is a refresh. Returns `None` if an
    /// equivalent refresh is already queued, and otherwise the key to
    /// release should sending it fail (`Some(None)` for anything but a
    /// refresh).
    fn claim(&self, command: &TaskManagerCommand) -> Option<Option<(i64, bool)>> {
        let Some((feed_id, manual)) = refresh_key(command) else {
            return Some(None);
        };
        let mut queued = self.refreshes.lock().unwrap_or_else(|e| e.into_inner());
        if queued.contains(&(feed_id, true)) || !queued.insert((feed_id, manual)) {
            return None;
        }
        Some(Some((feed_id, manual)))
    }

    fn release(&self, key: Option<(i64, bool)>) {
        if let Some(key) = key {
            self.refreshes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
        }
    }
}

/// The receiving side of the task queue, shared by the workers.
///
/// One made with [`From`] has a single lane; [`queue`] makes one with an
/// asset lane.
#[derive(Clone, Debug)]
pub struct TaskReceiver {
    rx: Receiver<TaskManagerCommand>,
    /// The asset lane, if there is one.
    assets: Option<Receiver<TaskManagerCommand>>,
}

impl From<Receiver<TaskManagerCommand>> for TaskReceiver {
    fn from(rx: Receiver<TaskManagerCommand>) -> Self {
        Self { rx, assets: None }
    }
}

impl TaskReceiver {
    /// Take the next command, waiting for one if the queue is empty. The
    /// asset lane is served only while the main lane is empty.
    ///
    /// Cancelling the returned future loses no command.
    ///
    /// # Errors
    ///
    /// Fails once the queue is closed and both lanes are empty.
    pub async fn recv(&self) -> Result<TaskManagerCommand, RecvError> {
        let Some(assets) = &self.assets else {
            return self.rx.recv().await;
        };
        tokio::select! {
            biased;
            command = self.rx.recv() => match command {
                Ok(command) => Ok(command),
                // Closed and empty: what is left in the asset lane is
                // still to be done.
                Err(RecvError) => assets.recv().await,
            },
            Ok(command) = assets.recv() => Ok(command),
        }
    }
}

/// The `(feed_id, manual)` of `command` if it is a feed refresh.
fn refresh_key(command: &TaskManagerCommand) -> Option<(i64, bool)> {
    match *command {
        TaskManagerCommand::RefreshFeed { feed_id, manual } => Some((feed_id, manual)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn refresh(feed_id: i64, manual: bool) -> TaskManagerCommand {
        TaskManagerCommand::RefreshFeed { feed_id, manual }
    }

    #[test]
    fn a_queued_refresh_is_not_queued_again() {
        let (tx, rx) = async_channel::bounded(8);
        let tx = TaskSender::from(tx);
        assert_eq!(tx.try_send(refresh(1, false)).unwrap(), Enqueue::Queued);
        assert_eq!(
            tx.try_send(refresh(1, false)).unwrap(),
            Enqueue::AlreadyQueued
        );
        assert_eq!(tx.try_send(refresh(2, false)).unwrap(), Enqueue::Queued);
        assert_eq!(rx.len(), 2);
    }

    #[test]
    fn a_manual_refresh_is_queued_behind_a_scheduled_one_but_not_the_reverse() {
        let (tx, rx) = async_channel::bounded(8);
        let tx = TaskSender::from(tx);
        assert_eq!(tx.try_send(refresh(1, false)).unwrap(), Enqueue::Queued);
        assert_eq!(tx.try_send(refresh(1, true)).unwrap(), Enqueue::Queued);
        assert_eq!(
            tx.try_send(refresh(1, true)).unwrap(),
            Enqueue::AlreadyQueued
        );
        assert_eq!(
            tx.try_send(refresh(1, false)).unwrap(),
            Enqueue::AlreadyQueued
        );
        assert_eq!(rx.len(), 2);

        // Once the manual refresh is taken, a scheduled one can be queued.
        tx.dequeued(&rx.try_recv().unwrap());
        tx.dequeued(&rx.try_recv().unwrap());
        assert_eq!(tx.try_send(refresh(1, false)).unwrap(), Enqueue::Queued);
    }

    #[test]
    fn other_commands_are_always_queued() {
        let (tx, rx) = async_channel::bounded(8);
        let tx = TaskSender::from(tx);
        for _ in 0..3 {
            assert_eq!(
                tx.try_send(TaskManagerCommand::WalCheckpointAnalyze)
                    .unwrap(),
                Enqueue::Queued
            );
        }
        assert_eq!(rx.len(), 3);
    }

    #[test]
    fn a_refresh_that_could_not_be_queued_can_be_tried_again() {
        let (tx, rx) = async_channel::bounded(1);
        let tx = TaskSender::from(tx);
        assert_eq!(
            tx.try_send(TaskManagerCommand::OptimizeFts).unwrap(),
            Enqueue::Queued
        );
        assert!(matches!(
            tx.try_send(refresh(1, false)),
            Err(TrySendError::Full(_))
        ));
        rx.try_recv().unwrap();
        assert_eq!(tx.try_send(refresh(1, false)).unwrap(), Enqueue::Queued);
    }

    fn assets(entry_id: i64) -> TaskManagerCommand {
        TaskManagerCommand::CacheEntryAssets { entry_id }
    }

    /// A burst of new entries' asset caching fits however full the main
    /// lane is, and leaves room in it for refreshes.
    #[test]
    fn asset_caching_neither_fills_nor_is_dropped_from_the_queue() {
        let (tx, _rx) = queue(2);
        for entry_id in 0..10_000 {
            assert_eq!(tx.try_send(assets(entry_id)).unwrap(), Enqueue::Queued);
        }
        assert_eq!(
            tx.try_send(TaskManagerCommand::CacheFeedFavicon { feed_id: 1 })
                .unwrap(),
            Enqueue::Queued
        );
        assert_eq!(tx.try_send(refresh(1, false)).unwrap(), Enqueue::Queued);
        assert_eq!(tx.try_send(refresh(2, false)).unwrap(), Enqueue::Queued);
        assert!(matches!(
            tx.try_send(refresh(3, false)),
            Err(TrySendError::Full(_))
        ));
        assert_eq!(tx.asset_queue_len(), Some(10_001));
        assert_eq!(tx.len(), 10_003);
    }

    /// Workers take asset caching only when nothing else is waiting.
    #[tokio::test]
    async fn the_main_lane_is_served_first() {
        let (tx, rx) = queue(8);
        tx.try_send(assets(1)).unwrap();
        tx.try_send(refresh(1, false)).unwrap();
        tx.try_send(assets(2)).unwrap();
        tx.try_send(TaskManagerCommand::OptimizeFts).unwrap();

        let mut order = Vec::new();
        for _ in 0..4 {
            order.push(rx.recv().await.unwrap().name());
        }
        assert_eq!(
            order,
            [
                "refresh_feed",
                "optimize_fts",
                "cache_entry_assets",
                "cache_entry_assets"
            ]
        );
    }

    /// A worker waiting on an empty queue wakes for asset caching.
    #[tokio::test]
    async fn a_waiting_worker_takes_asset_caching() {
        let (tx, rx) = queue(8);
        let waiting = tokio::spawn(async move { rx.recv().await });
        tokio::task::yield_now().await;
        tx.send(assets(7)).await.unwrap();
        assert!(matches!(
            waiting.await.unwrap().unwrap(),
            TaskManagerCommand::CacheEntryAssets { entry_id: 7 }
        ));
    }

    /// Closing the queue lets workers finish both lanes, then stop.
    #[tokio::test]
    async fn a_closed_queue_is_drained_from_both_lanes() {
        let (tx, rx) = queue(8);
        tx.try_send(refresh(1, false)).unwrap();
        tx.try_send(assets(1)).unwrap();
        assert!(tx.close());
        assert!(tx.try_send(assets(2)).is_err());

        assert!(matches!(
            rx.recv().await.unwrap(),
            TaskManagerCommand::RefreshFeed { .. }
        ));
        assert!(matches!(
            rx.recv().await.unwrap(),
            TaskManagerCommand::CacheEntryAssets { entry_id: 1 }
        ));
        assert!(rx.recv().await.is_err());
    }

    /// A single-lane queue, as tests make, keeps asset caching in the one
    /// lane.
    #[test]
    fn a_single_lane_queue_takes_asset_caching_in_its_lane() {
        let (tx, rx) = async_channel::bounded(1);
        let tx = TaskSender::from(tx);
        tx.try_send(assets(1)).unwrap();
        assert!(matches!(tx.try_send(assets(2)), Err(TrySendError::Full(_))));
        assert_eq!(tx.asset_queue_len(), None);
        assert_eq!(rx.len(), 1);
    }

    #[tokio::test]
    async fn send_waits_for_room_and_drops_duplicates() {
        let (tx, rx) = async_channel::bounded(1);
        let tx = TaskSender::from(tx);
        assert_eq!(tx.send(refresh(1, true)).await.unwrap(), Enqueue::Queued);
        // Already queued: returns at once even though the queue is full.
        assert_eq!(
            tx.send(refresh(1, true)).await.unwrap(),
            Enqueue::AlreadyQueued
        );
        assert_eq!(rx.len(), 1);
    }
}
