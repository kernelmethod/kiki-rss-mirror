use crate::db::tags::{is_reserved_tag_name, SystemTag};
use crate::db::Db;
use crate::fetcher::{AtomEntry, AtomFeedIngestData, RssEntry};
use crate::metrics::Metrics;
use crate::scripting::{FeedEntry, ScriptRunner};
use crate::tasks::command::TaskManagerCommand;
use crate::tasks::parsing::{insert_atom_entry_data, insert_rss_entry_data, upsert_atom_feed_data};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use std::collections::BTreeSet;
use tracing::{debug, error, warn};

/// The entries a refresh wrote.
#[derive(Debug, Default)]
pub(super) struct StoredEntries {
    /// Ids of every entry written, new or updated.
    pub(super) ids: Vec<i64>,
    /// Ids of the entries whose assets should be cached: those no script
    /// set `cache_assets` to `false` on.
    pub(super) cache_assets: Vec<i64>,
}

impl StoredEntries {
    fn with_capacity(n: usize) -> Self {
        StoredEntries {
            ids: Vec::with_capacity(n),
            cache_assets: Vec::with_capacity(n),
        }
    }

    fn push(&mut self, entry_id: i64, entry: &FeedEntry) {
        self.ids.push(entry_id);
        if entry.cache_assets {
            self.cache_assets.push(entry_id);
        }
    }
}

/// The most entries written in one transaction by a refresh. Between
/// batches the writer is free for other work, such as marking an entry
/// read, so a long feed cannot hold it for its whole refresh.
const ENTRIES_PER_TRANSACTION: usize = 100;

/// How long a refresh pauses between batches of entries, so that work
/// already waiting for the writer gets it: the writer's pool does not queue
/// its waiters, and would otherwise hand the writer straight back to the
/// refresh.
const WRITER_HANDOFF: std::time::Duration = std::time::Duration::from_millis(1);

/// Store a parsed Atom feed: its feed-level data, then each entry after
/// it has been through the script chain.
///
/// Returns the entries that were written.
///
/// The entries may have come from the isolated fetcher, so nothing in them
/// is trusted to say which feed they belong to: `feed_id` and the
/// syndication format are re-stamped from the caller's own values before
/// scripts or the database see them.
///
/// Scripts run first, outside any transaction; the entries are then written
/// by [`store_entries`].
pub(super) fn process_atom_feed(
    feed_id: i64,
    site_url: Option<&str>,
    feed_data: AtomFeedIngestData,
    entries: Vec<AtomEntry>,
    db: &Db,
    script_runner: Option<&dyn ScriptRunner>,
    metrics: &Metrics,
) -> Result<StoredEntries> {
    debug!("Parsed Atom feed {} with {} items", feed_id, entries.len());

    let parsed_count = entries.len();
    let entries: Vec<_> = entries
        .into_iter()
        .filter_map(
            |AtomEntry {
                 entry: mut feed_entry,
                 data,
             }| {
                feed_entry.feed_id = feed_id;
                feed_entry.syndication_format = "atom".to_string();
                run_scripts(feed_id, feed_entry, script_runner, metrics).map(|e| (e, data))
            },
        )
        .collect();

    let stored = store_entries(
        db,
        feed_id,
        "atom",
        parsed_count,
        entries,
        |tx| {
            crate::db::favicons::set_site_url(tx, feed_id, site_url)?;
            upsert_atom_feed_data(tx, feed_id, &feed_data)
        },
        |tx, entry_id, data, _| insert_atom_entry_data(tx, entry_id, data),
    )?;
    for _ in &stored.ids {
        metrics.record_feed_entry_upserted("atom");
    }
    Ok(stored)
}

/// Store a parsed RSS feed, each item after it has been through the
/// script chain.
///
/// Returns the entries that were written. As with
/// [`process_atom_feed`], `feed_id` and the syndication format are
/// re-stamped on every entry rather than trusted, and the entries are
/// written by [`store_entries`] once the scripts have run.
pub(super) fn process_rss_feed(
    feed_id: i64,
    site_url: Option<&str>,
    entries: Vec<RssEntry>,
    db: &Db,
    script_runner: Option<&dyn ScriptRunner>,
    metrics: &Metrics,
) -> Result<StoredEntries> {
    debug!("Parsed RSS feed {} with {} items", feed_id, entries.len());

    let parsed_count = entries.len();
    let entries: Vec<_> = entries
        .into_iter()
        .filter_map(
            |RssEntry {
                 entry: mut feed_entry,
                 data,
             }| {
                feed_entry.feed_id = feed_id;
                feed_entry.syndication_format = "rss".to_string();
                run_scripts(feed_id, feed_entry, script_runner, metrics).map(|e| (e, data))
            },
        )
        .collect();

    let stored = store_entries(
        db,
        feed_id,
        "rss",
        parsed_count,
        entries,
        |tx| crate::db::favicons::set_site_url(tx, feed_id, site_url),
        |tx, entry_id, data, entry| {
            insert_rss_entry_data(tx, entry_id, data, entry.content.as_deref())
        },
    )?;
    for _ in &stored.ids {
        metrics.record_feed_entry_upserted("rss");
    }
    Ok(stored)
}

/// Write a refreshed feed's `entries`, each with its format-specific data,
/// and then mark the feed's stored entries that it no longer lists as
/// dropped.
///
/// The entries are written in batches of [`ENTRIES_PER_TRANSACTION`], each
/// in a `BEGIN IMMEDIATE` transaction of its own, which takes the write lock
/// up front and waits for it (up to the busy timeout) instead of failing
/// with "database is locked" when it would have to upgrade a read. The
/// first batch also records the feed's syndication format and runs
/// `feed_data` for the rest of its feed-level data; the last also marks
/// dropped entries, once every entry the feed lists has been stored. A feed
/// with no entries is one batch.
///
/// If a batch fails, the batches before it stay written: each entry is
/// stored whole or not at all, and the next refresh stores the rest. No
/// entries are marked dropped by a refresh that fails.
fn store_entries<D>(
    db: &Db,
    feed_id: i64,
    format: &'static str,
    parsed_count: usize,
    entries: Vec<(FeedEntry, D)>,
    feed_data: impl FnOnce(&rusqlite::Transaction) -> Result<()>,
    entry_data: impl Fn(&rusqlite::Transaction, i64, &D, &FeedEntry) -> Result<()>,
) -> Result<StoredEntries> {
    let seen_guids: Vec<String> = entries.iter().map(|(e, _)| e.guid.clone()).collect();
    let batches = entries.len().div_ceil(ENTRIES_PER_TRANSACTION).max(1);
    let mut stored = StoredEntries::with_capacity(entries.len());
    let mut feed_data = Some(feed_data);
    let mut entries = entries.into_iter();

    for batch in 0..batches {
        if batch > 0 {
            std::thread::sleep(WRITER_HANDOFF);
        }
        let last = batch + 1 == batches;
        db.write_blocking(|conn| -> Result<()> {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let Some(feed_data) = feed_data.take() {
                tx.execute(
                    "UPDATE feeds SET syndication_format = ?2 WHERE id = ?1",
                    rusqlite::params![feed_id, format],
                )?;
                feed_data(&tx)?;
            }
            for (feed_entry, data) in entries.by_ref().take(ENTRIES_PER_TRANSACTION) {
                let (entry_id, is_new) = upsert_entry(&tx, feed_id, format, &feed_entry)?;
                entry_data(&tx, entry_id, &data, &feed_entry)?;
                if is_new && feed_entry.cache_assets {
                    crate::db::pending_assets::add(&tx, entry_id)?;
                }
                if !feed_entry.tags.is_empty() {
                    sync_entry_tags(&tx, entry_id, &feed_entry.tags, is_new)?;
                }
                stored.push(entry_id, &feed_entry);
            }
            if last {
                mark_dropped_entries(&tx, feed_id, parsed_count, &seen_guids)?;
            }
            tx.commit()?;
            Ok(())
        })??;
    }
    Ok(stored)
}

/// Run one entry through the script chain.
///
/// Returns the entry to store, or `None` if a script filtered it out. An
/// entry whose scripts fail is stored unmodified.
fn run_scripts(
    feed_id: i64,
    feed_entry: FeedEntry,
    script_runner: Option<&dyn ScriptRunner>,
    metrics: &Metrics,
) -> Option<FeedEntry> {
    let Some(runner) = script_runner else {
        return Some(feed_entry);
    };
    // Checked first to spare cloning the entry for events nothing handles.
    if runner.handles(crate::scripting::Event::EntryParsed) {
        runner.dispatch_observe(
            crate::scripting::Event::EntryParsed,
            crate::scripting::EventPayload::Entry(feed_entry.clone()),
        );
    }
    if !runner.handles(crate::scripting::Event::EntryIngest) {
        return Some(feed_entry);
    }
    let format = feed_entry.syndication_format.clone();
    let original = feed_entry.clone();
    let mut runs = Vec::new();
    let result = runner.dispatch_transform_entry_timed(feed_entry, &mut runs);
    for run in &runs {
        metrics.record_plugin_run(&run.plugin, run.seconds);
    }
    match result {
        Ok(Some(e)) => {
            metrics.record_plugin_execution("ok");
            Some(e)
        }
        Ok(None) => {
            metrics.record_plugin_execution("filtered");
            debug!("{} entry filtered by script for feed {}", format, feed_id);
            None
        }
        Err(e) => {
            metrics.record_plugin_execution("error");
            warn!(
                "script error processing {} entry for feed {}: {}; inserting unmodified",
                format, feed_id, e
            );
            Some(original)
        }
    }
}

/// Insert an entry, or update the existing row for the same
/// `(feed_id, guid)` in place, and return its id and whether it was newly
/// inserted.
///
/// Updating in place keeps the entry's id stable across refreshes, and
/// with it everything keyed on that id (tags, cached assets, the search
/// index). The entry is in the feed again, so `dropped_at` is cleared.
///
/// A new entry takes the id after the highest ever used, recorded in
/// `entry_id_high_water`, rather than SQLite's default of one past the
/// highest in use, so that ids only ever increase even after the newest
/// entries are deleted. It is stamped with the current time as its
/// `ingested_at`, which an update leaves alone.
fn upsert_entry(
    tx: &rusqlite::Transaction,
    feed_id: i64,
    syndication_format: &str,
    entry: &FeedEntry,
) -> Result<(i64, bool)> {
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM entries WHERE feed_id = ?1 AND guid = ?2)",
        rusqlite::params![feed_id, entry.guid],
        |row| row.get(0),
    )?;
    let entry_id = tx.query_row(
        "INSERT INTO entries (
            id,
            feed_id,
            syndication_format,
            guid,
            published_at,
            title,
            url,
            content,
            ingested_at
        ) VALUES (
            (SELECT MAX(hw.id, COALESCE((SELECT MAX(id) FROM entries), 0)) + 1
             FROM entry_id_high_water hw),
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, unixepoch()
        )
        ON CONFLICT(feed_id, guid) DO UPDATE SET
            syndication_format = excluded.syndication_format,
            published_at = excluded.published_at,
            title = excluded.title,
            url = excluded.url,
            content = excluded.content,
            dropped_at = NULL
        RETURNING id",
        rusqlite::params![
            feed_id,
            syndication_format,
            entry.guid,
            entry.published_at,
            entry.title,
            entry.url,
            entry.content
        ],
        |row| row.get(0),
    )?;
    Ok((entry_id, !exists))
}

/// Mark the feed's stored entries that this refresh did not store as
/// dropped, starting their retention clock.
///
/// `parsed_count` is how many entries the feed document held before
/// scripts ran. A document with none is more likely a publisher glitch
/// than a feed that really emptied, so it marks nothing; entries that
/// scripts filtered out are marked like any other.
fn mark_dropped_entries(
    conn: &Connection,
    feed_id: i64,
    parsed_count: usize,
    seen_guids: &[String],
) -> Result<()> {
    if parsed_count == 0 {
        debug!(
            "feed {} listed no entries; not marking any dropped",
            feed_id
        );
        return Ok(());
    }
    let marked = crate::db::retention::mark_dropped(conn, feed_id, seen_guids)?;
    if marked > 0 {
        debug!("marked {} entries of feed {} as dropped", marked, feed_id);
    }
    Ok(())
}

/// Enqueue a [`TaskManagerCommand::CacheEntryAssets`] for each of the given
/// entry IDs.
///
/// Asset caching has a lane of its own in the server's queue, which has no
/// bound, so this only fails once the queue has been closed. A failure is
/// logged as an error, and [`crate::db::pending_assets`] queues the entry
/// again later.
pub(super) fn enqueue_asset_caching(
    task_tx: &crate::tasks::TaskSender,
    metrics: &Metrics,
    entry_ids: &[i64],
) {
    for &entry_id in entry_ids {
        match task_tx.try_send(TaskManagerCommand::CacheEntryAssets { entry_id }) {
            Ok(_) => metrics.record_task_enqueued("cache_entry_assets"),
            Err(e) => error!(
                "failed to queue CacheEntryAssets for entry {}: {:?}",
                entry_id, e
            ),
        }
    }
}

/// Enqueue a [`TaskManagerCommand::CacheFeedFavicon`] for feed `feed_id`.
/// Like [`enqueue_asset_caching`], it only fails once the queue has been
/// closed, which is logged as an error; the favicon is looked for again on
/// the feed's next refresh.
pub(super) fn enqueue_favicon_caching(
    task_tx: &crate::tasks::TaskSender,
    metrics: &Metrics,
    feed_id: i64,
) {
    match task_tx.try_send(TaskManagerCommand::CacheFeedFavicon { feed_id }) {
        Ok(_) => metrics.record_task_enqueued("cache_feed_favicon"),
        Err(e) => error!(
            "failed to queue CacheFeedFavicon for feed {}: {:?}",
            feed_id, e
        ),
    }
}

/// Sync the script-provided tags for the entry `entry_id`.
///
/// System tags (such as `system:hidden`) in `tags` are applied only when
/// `is_new`, i.e. when the entry is first stored: from then on the entry's
/// system tags record the user's own actions (reading, saving, hiding or
/// unhiding it), which a later refresh must not undo. Scripts never remove
/// system tags. A name with the system tag prefix that is not a known system
/// tag is skipped with a warning.
///
/// If `tags` holds only system tags, the entry's user tags are left alone,
/// so that a script that only hides entries does not also untag them.
/// Otherwise the entry's user tags are made to match the other (user) tag
/// names in `tags`: user tags not among them are removed, and those missing
/// are added, creating the tag first if it does not exist.
///
/// This runs for every tagged entry in a feed on every refresh, and usually
/// finds the tags already in place, so it reads the entry's current user
/// tags first and writes only the difference. An entry whose tags are
/// unchanged costs one indexed read and no writes.
fn sync_entry_tags(conn: &Connection, entry_id: i64, tags: &[String], is_new: bool) -> Result<()> {
    let mut wanted: BTreeSet<&str> = BTreeSet::new();
    for name in tags {
        if !is_reserved_tag_name(name) {
            wanted.insert(name);
            continue;
        }
        match SystemTag::ALL.into_iter().find(|tag| tag.name() == name) {
            Some(tag) if is_new => {
                conn.prepare_cached(
                    "INSERT OR IGNORE INTO entry_tags (entry_id, tag_id) VALUES (?1, ?2)",
                )?
                .execute(rusqlite::params![entry_id, tag.id(conn)?])?;
            }
            Some(_) => {}
            None => warn!(
                "ignoring tag {:?} set by a script on entry {}: it is not a system tag, and names starting with \"system:\" are reserved for system tags",
                name, entry_id
            ),
        }
    }

    if wanted.is_empty() {
        return Ok(());
    }

    // The entry's current user tags. Both tables are reached through an
    // index, so this reads only the entry's own tags, however many tags
    // there are in all.
    let current = conn
        .prepare_cached(
            "SELECT t.id, t.name FROM entry_tags et JOIN tags t ON t.id = et.tag_id
             WHERE et.entry_id = ?1 AND t.kind = 'user'",
        )?
        .query_map([entry_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    // Remove the user tags the script no longer sets; whatever is left in
    // `wanted` afterwards is missing from the entry.
    for (tag_id, name) in current {
        if !wanted.remove(name.as_str()) {
            conn.prepare_cached("DELETE FROM entry_tags WHERE entry_id = ?1 AND tag_id = ?2")?
                .execute([entry_id, tag_id])?;
        }
    }

    for name in wanted {
        let existing: Option<i64> = conn
            .prepare_cached("SELECT id FROM tags WHERE name = ?1")?
            .query_row([name], |row| row.get(0))
            .optional()?;
        let tag_id = match existing {
            Some(id) => id,
            None => conn
                .prepare_cached("INSERT INTO tags (name) VALUES (?1) RETURNING id")?
                .query_row([name], |row| row.get(0))?,
        };
        conn.prepare_cached("INSERT OR IGNORE INTO entry_tags (entry_id, tag_id) VALUES (?1, ?2)")?
            .execute([entry_id, tag_id])?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tags::SystemTag;
    use crate::db::ConnectionBuilder;

    fn entry_tag_names(conn: &Connection, entry_id: i64) -> Result<Vec<String>> {
        let mut stmt = conn.prepare(
            "SELECT t.name FROM tags t JOIN entry_tags et ON et.tag_id = t.id
             WHERE et.entry_id = ?1 ORDER BY t.name",
        )?;
        let names = stmt
            .query_map([entry_id], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(names)
    }

    /// Script-provided tags replace the entry's user tags, but leave its
    /// system tags alone. Scripts apply system tags only to new entries.
    #[test]
    fn sync_entry_tags_preserves_system_tags() -> Result<()> {
        let conn = ConnectionBuilder::default().in_memory().create().build()?;
        conn.execute(
            "INSERT INTO entries (syndication_format, guid, published_at, title, url)
             VALUES ('rss', 'g', 0, 't', 'u')",
            [],
        )?;
        let entry_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO entry_tags (entry_id, tag_id) VALUES (?1, ?2)",
            [entry_id, SystemTag::Read.id(&conn)?],
        )?;

        sync_entry_tags(&conn, entry_id, &["a".into(), "b".into()], false)?;
        assert_eq!(entry_tag_names(&conn, entry_id)?, ["a", "b", "system:read"]);

        // The entry is not new, so its system tags are the user's.
        sync_entry_tags(
            &conn,
            entry_id,
            &["b".into(), "system:hidden".into(), "System:new".into()],
            false,
        )?;
        assert_eq!(entry_tag_names(&conn, entry_id)?, ["b", "system:read"]);

        // On a new entry, known system tags are applied and others ignored.
        sync_entry_tags(
            &conn,
            entry_id,
            &["b".into(), "system:hidden".into(), "system:new".into()],
            true,
        )?;
        assert_eq!(
            entry_tag_names(&conn, entry_id)?,
            ["b", "system:hidden", "system:read"]
        );

        // Scripts never remove system tags.
        sync_entry_tags(&conn, entry_id, &["c".into()], false)?;
        assert_eq!(
            entry_tag_names(&conn, entry_id)?,
            ["c", "system:hidden", "system:read"]
        );

        let reserved: i64 = conn.query_row(
            "SELECT COUNT(*) FROM tags WHERE kind = 'user' AND name LIKE 'system:%'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(reserved, 0);

        Ok(())
    }

    /// Tags holding only system tags, as from a script that just hides
    /// entries, leave the entry's user tags alone.
    #[test]
    fn sync_entry_tags_with_only_system_tags_keeps_user_tags() -> Result<()> {
        let conn = ConnectionBuilder::default().in_memory().create().build()?;
        conn.execute(
            "INSERT INTO entries (syndication_format, guid, published_at, title, url)
             VALUES ('rss', 'g', 0, 't', 'u')",
            [],
        )?;
        let entry_id = conn.last_insert_rowid();
        sync_entry_tags(&conn, entry_id, &["a".into()], true)?;

        sync_entry_tags(&conn, entry_id, &["system:hidden".into()], false)?;
        assert_eq!(entry_tag_names(&conn, entry_id)?, ["a"]);

        sync_entry_tags(&conn, entry_id, &["system:hidden".into()], true)?;
        assert_eq!(entry_tag_names(&conn, entry_id)?, ["a", "system:hidden"]);

        Ok(())
    }

    /// A refresh that finds the entry's tags already in place writes
    /// nothing; one that changes them writes only the difference, reusing
    /// tags that already exist.
    #[test]
    fn sync_entry_tags_writes_only_changes() -> Result<()> {
        let conn = ConnectionBuilder::default().in_memory().create().build()?;
        conn.execute(
            "INSERT INTO entries (syndication_format, guid, published_at, title, url)
             VALUES ('rss', 'g', 0, 't', 'u')",
            [],
        )?;
        let entry_id = conn.last_insert_rowid();
        conn.execute("INSERT INTO tags (name) VALUES ('c')", [])?;
        let c_id = conn.last_insert_rowid();

        sync_entry_tags(&conn, entry_id, &["a".into(), "b".into(), "a".into()], true)?;
        assert_eq!(entry_tag_names(&conn, entry_id)?, ["a", "b"]);

        let before = conn.total_changes();
        sync_entry_tags(&conn, entry_id, &["b".into(), "a".into()], false)?;
        assert_eq!(conn.total_changes(), before);

        // Drop `a`, keep `b`, add the existing tag `c`: one delete and one
        // insert, and no new tag.
        let before = conn.total_changes();
        sync_entry_tags(&conn, entry_id, &["b".into(), "c".into()], false)?;
        assert_eq!(conn.total_changes(), before + 2);
        assert_eq!(entry_tag_names(&conn, entry_id)?, ["b", "c"]);
        let c_ids: i64 = conn.query_row(
            "SELECT COUNT(*) FROM tags WHERE name = 'c' AND id = ?1",
            [c_id],
            |row| row.get(0),
        )?;
        assert_eq!(c_ids, 1);

        Ok(())
    }

    /// Syncing an entry's tags reads only that entry's tags, never the
    /// whole tags table, however many tags there are.
    #[test]
    fn sync_entry_tags_does_not_scan_tags() -> Result<()> {
        let conn = ConnectionBuilder::default().in_memory().create().build()?;
        conn.execute(
            "INSERT INTO entries (syndication_format, guid, published_at, title, url)
             VALUES ('rss', 'g', 0, 't', 'u')",
            [],
        )?;
        let entry_id = conn.last_insert_rowid();
        for i in 0..100 {
            conn.execute("INSERT INTO tags (name) VALUES (?1)", [format!("t{i}")])?;
        }
        let metrics = std::sync::Arc::new(crate::metrics::Metrics::new()?);
        crate::db::profile::install(&conn, metrics.clone())?;

        sync_entry_tags(&conn, entry_id, &["a".into(), "t1".into()], true)?;
        sync_entry_tags(&conn, entry_id, &["a".into(), "t1".into()], false)?;
        sync_entry_tags(&conn, entry_id, &["b".into()], false)?;
        assert_eq!(entry_tag_names(&conn, entry_id)?, ["b"]);

        let fullscan_steps: f64 = metrics
            .render()
            .lines()
            .filter(|l| l.starts_with("kiki_db_statement_fullscan_steps_total{"))
            .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
            .sum();
        assert_eq!(fullscan_steps, 0.0);

        Ok(())
    }

    fn feed_entry(guid: &str, title: &str) -> FeedEntry {
        FeedEntry {
            id: None,
            feed_id: 1,
            syndication_format: "rss".into(),
            guid: guid.into(),
            published_at: Some(1700000000),
            title: title.into(),
            url: Some("http://example.com/".into()),
            content: None,
            authors: Vec::new(),
            categories: Vec::new(),
            tags: Vec::new(),
            cache_assets: true,
        }
    }

    /// New entries never reuse the id of a deleted one, even the newest,
    /// and are stamped with the time they were stored, which updates to the
    /// entry leave alone.
    #[test]
    fn upsert_entry_ids_only_increase() -> Result<()> {
        let mut conn = ConnectionBuilder::default().in_memory().create().build()?;
        conn.execute(
            "INSERT INTO feeds (id, title, url) VALUES (1, 'f', 'u')",
            [],
        )?;
        let upsert = |conn: &mut Connection, guid: &str, title: &str| -> Result<(i64, bool)> {
            let tx = conn.transaction()?;
            let stored = upsert_entry(&tx, 1, "rss", &feed_entry(guid, title))?;
            tx.commit()?;
            Ok(stored)
        };

        assert_eq!(upsert(&mut conn, "a", "a")?, (1, true));
        assert_eq!(upsert(&mut conn, "b", "b")?, (2, true));
        conn.execute("DELETE FROM entries WHERE id = 2", [])?;
        assert_eq!(upsert(&mut conn, "c", "c")?, (3, true));

        // An update keeps the entry's id and when it was ingested
        conn.execute("UPDATE entries SET ingested_at = 5 WHERE id = 1", [])?;
        assert_eq!(upsert(&mut conn, "a", "a, edited")?, (1, false));
        let (title, ingested_at): (String, i64) = conn.query_row(
            "SELECT title, ingested_at FROM entries WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!((title.as_str(), ingested_at), ("a, edited", 5));

        let ingested_at: i64 =
            conn.query_row("SELECT ingested_at FROM entries WHERE id = 3", [], |row| {
                row.get(0)
            })?;
        assert!((chrono::Utc::now().timestamp() - ingested_at).abs() < 60);
        Ok(())
    }

    /// Storing an entry again unchanged, as every refresh of its feed does,
    /// leaves the full-text index alone; a changed title is reindexed.
    #[test]
    fn upsert_entry_reindexes_only_changes() -> Result<()> {
        let mut conn = ConnectionBuilder::default().in_memory().create().build()?;
        conn.execute(
            "INSERT INTO feeds (id, title, url) VALUES (1, 'f', 'u')",
            [],
        )?;
        let upsert = |conn: &mut Connection, title: &str| -> Result<u64> {
            let before = conn.total_changes();
            let tx = conn.transaction()?;
            upsert_entry(&tx, 1, "rss", &feed_entry("a", title))?;
            tx.commit()?;
            Ok(conn.total_changes() - before)
        };
        let matches = |conn: &Connection, word: &str| -> Result<i64> {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM entries_fts WHERE entries_fts MATCH ?1",
                [word],
                |row| row.get(0),
            )?)
        };

        upsert(&mut conn, "apples")?;
        // Only the entry's own row changes: no trigger writes to the index.
        assert_eq!(upsert(&mut conn, "apples")?, 1);
        assert_eq!(matches(&conn, "apples")?, 1);

        assert!(upsert(&mut conn, "oranges")? > 1);
        assert_eq!(matches(&conn, "apples")?, 0);
        assert_eq!(matches(&conn, "oranges")?, 1);
        Ok(())
    }
}
