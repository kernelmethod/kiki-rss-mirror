#[derive(Debug, Clone)]
pub enum TaskManagerCommand {
    RefreshFeed(i64),
    /// Run retention cleanup for the given feed.
    CleanupFeed(i64),
    /// Run retention cleanup across all feeds.
    CleanupAll,
    /// Download and cache the external assets referenced by an entry
    /// (inline images plus any enclosure).
    CacheEntryAssets {
        entry_id: i64,
    },
    /// Find and cache the favicon of the website a feed belongs to, if
    /// Kiki has not looked recently.
    CacheFeedFavicon {
        feed_id: i64,
    },
    /// Merge FTS5 index segments to improve search performance.
    OptimizeFts,
    /// Checkpoint the WAL file and refresh query-planner statistics.
    WalCheckpointAnalyze,
    /// Reclaim free pages via `PRAGMA incremental_vacuum`.
    IncrementalVacuum,
}
