//! The sending side of the task queue, which leaves out feed refreshes
//! that are already queued.

use crate::tasks::command::TaskManagerCommand;
use async_channel::{SendError, Sender, TrySendError};
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
    /// The `(feed_id, manual)` of each refresh in the queue.
    refreshes: Arc<Mutex<HashSet<(i64, bool)>>>,
}

impl From<Sender<TaskManagerCommand>> for TaskSender {
    fn from(tx: Sender<TaskManagerCommand>) -> Self {
        Self {
            tx,
            refreshes: Arc::default(),
        }
    }
}

impl TaskSender {
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
        self.tx
            .send(command)
            .await
            .inspect_err(|_| self.release(key))?;
        Ok(Enqueue::Queued)
    }

    /// Queue `command` if there is room for it now.
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
        self.tx
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

    /// Close the queue, so that nothing more can be sent on it and workers
    /// stop once they have taken what is in it.
    pub fn close(&self) -> bool {
        self.tx.close()
    }

    /// The number of commands in the queue.
    pub fn len(&self) -> usize {
        self.tx.len()
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.tx.is_empty()
    }

    /// The most commands the queue holds, or `None` if it is unbounded.
    pub fn capacity(&self) -> Option<usize> {
        self.tx.capacity()
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
                tx.try_send(TaskManagerCommand::CleanupFeed(1)).unwrap(),
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
            tx.try_send(TaskManagerCommand::CleanupAll).unwrap(),
            Enqueue::Queued
        );
        assert!(matches!(
            tx.try_send(refresh(1, false)),
            Err(TrySendError::Full(_))
        ));
        rx.try_recv().unwrap();
        assert_eq!(tx.try_send(refresh(1, false)).unwrap(), Enqueue::Queued);
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
