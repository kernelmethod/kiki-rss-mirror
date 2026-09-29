#[derive(Debug, Clone)]
pub enum TaskManagerCommand {
    /// Refresh a feed.
    RefreshFeed {
        feed_id: i64,
        /// Set for a refresh a user asked for, which fetches even if the
        /// feed's `next_fetch_at` is still in the future. The scheduler
        /// only queues feeds that are already due and leaves it unset.
        manual: bool,
    },
    /// Run retention cleanup for the given feed.
    CleanupFeed(i64),
    /// Run retention cleanup across all feeds.
    CleanupAll,
    /// Download and cache the external assets referenced by an entry
    /// (inline images plus any enclosure).
    CacheEntryAssets { entry_id: i64 },
    /// Find and cache the favicon of the website a feed belongs to, if
    /// Kiki has not looked recently.
    CacheFeedFavicon { feed_id: i64 },
    /// Merge FTS5 index segments to improve search performance.
    OptimizeFts,
    /// Checkpoint the WAL file and refresh query-planner statistics.
    WalCheckpointAnalyze,
    /// Reclaim free pages via `PRAGMA incremental_vacuum`.
    IncrementalVacuum,
}
