use crate::fetcher::{AtomEntry, AtomFeedIngestData, RssEntry};
use crate::metrics::Metrics;
use crate::scripting::{FeedEntry, ScriptRunner};
use crate::tasks::command::TaskManagerCommand;
use crate::tasks::parsing::{insert_atom_entry_data, insert_rss_entry_data, upsert_atom_feed_data};
use anyhow::Result;
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use std::time::Instant;
use tracing::{debug, info, warn};

/// Store a parsed Atom feed: its feed-level data, then each entry after
/// it has been through the script chain.
///
/// Returns the ids of the entries that were written.
///
/// The entries may have come from the isolated fetcher, so nothing in them
/// is trusted to say which feed they belong to: `feed_id` and the
/// syndication format are re-stamped from the caller's own values before
/// scripts or the database see them.
pub(super) fn process_atom_feed(
    feed_id: i64,
    feed_data: AtomFeedIngestData,
    entries: Vec<AtomEntry>,
    mut conn: PooledConnection<SqliteConnectionManager>,
    script_runner: Option<&dyn ScriptRunner>,
    metrics: &Metrics,
) -> Result<Vec<i64>> {
    info!(
        "Successfully fetched Atom feed {} with {} items",
        feed_id,
        entries.len()
    );

    {
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE feeds SET syndication_format = 'atom' WHERE id = ?1",
            [feed_id],
        )?;
        upsert_atom_feed_data(&tx, feed_id, &feed_data)?;
        tx.commit()?;
    }

    let parsed_count = entries.len();
    let mut inserted_entry_ids: Vec<i64> = Vec::new();
    let mut seen_guids: Vec<String> = Vec::new();
    for AtomEntry {
        entry: mut feed_entry,
        data: ingest,
    } in entries
    {
        feed_entry.feed_id = feed_id;
        feed_entry.syndication_format = "atom".to_string();

        let feed_entry = if let Some(runner) = script_runner {
            runner.dispatch_observe(
                crate::scripting::Event::EntryParsed,
                crate::scripting::EventPayload::Entry(feed_entry.clone()),
            );
            let original = feed_entry.clone();
            let script_start = Instant::now();
            match runner.dispatch_transform_entry(feed_entry) {
                Ok(Some(e)) => {
                    metrics.record_script_execution(script_start.elapsed().as_secs_f64(), "ok");
                    e
                }
                Ok(None) => {
                    metrics
                        .record_script_execution(script_start.elapsed().as_secs_f64(), "filtered");
                    debug!("atom entry filtered by script for feed {}", feed_id);
                    continue;
                }
                Err(e) => {
                    metrics.record_script_execution(script_start.elapsed().as_secs_f64(), "error");
                    warn!(
                        "script error processing atom entry for feed {}: {}; inserting unmodified",
                        feed_id, e
                    );
                    original
                }
            }
        } else {
            feed_entry
        };

        let tx = conn.transaction()?;
        let entry_id = upsert_entry(&tx, feed_id, "atom", &feed_entry)?;
        insert_atom_entry_data(&tx, entry_id, &ingest)?;
        tx.commit()?;
        metrics.record_feed_entry_upserted("atom");
        inserted_entry_ids.push(entry_id);

        if !feed_entry.tags.is_empty() {
            sync_entry_tags(&conn, feed_id, &feed_entry.guid, &feed_entry.tags)?;
        }
        seen_guids.push(feed_entry.guid);
    }

    mark_dropped_entries(&conn, feed_id, parsed_count, &seen_guids)?;
    Ok(inserted_entry_ids)
}

/// Store a parsed RSS feed, each item after it has been through the
/// script chain.
///
/// Returns the ids of the entries that were written. As with
/// [`process_atom_feed`], `feed_id` and the syndication format are
/// re-stamped on every entry rather than trusted.
pub(super) fn process_rss_feed(
    feed_id: i64,
    entries: Vec<RssEntry>,
    mut conn: PooledConnection<SqliteConnectionManager>,
    script_runner: Option<&dyn ScriptRunner>,
    metrics: &Metrics,
) -> Result<Vec<i64>> {
    info!(
        "Successfully fetched RSS feed {} with {} items",
        feed_id,
        entries.len()
    );

    conn.execute(
        "UPDATE feeds SET syndication_format = 'rss' WHERE id = ?1",
        [feed_id],
    )?;

    let parsed_count = entries.len();
    let mut inserted_entry_ids: Vec<i64> = Vec::new();
    let mut seen_guids: Vec<String> = Vec::new();
    for RssEntry {
        entry: mut feed_entry,
        data: ingest,
    } in entries
    {
        feed_entry.feed_id = feed_id;
        feed_entry.syndication_format = "rss".to_string();

        let feed_entry = if let Some(runner) = script_runner {
            runner.dispatch_observe(
                crate::scripting::Event::EntryParsed,
                crate::scripting::EventPayload::Entry(feed_entry.clone()),
            );
            let original = feed_entry.clone();
            let script_start = Instant::now();
            match runner.dispatch_transform_entry(feed_entry) {
                Ok(Some(e)) => {
                    metrics.record_script_execution(script_start.elapsed().as_secs_f64(), "ok");
                    e
                }
                Ok(None) => {
                    metrics
                        .record_script_execution(script_start.elapsed().as_secs_f64(), "filtered");
                    debug!("rss entry filtered by script for feed {}", feed_id);
                    continue;
                }
                Err(e) => {
                    metrics.record_script_execution(script_start.elapsed().as_secs_f64(), "error");
                    warn!(
                        "script error processing rss entry for feed {}: {}; inserting unmodified",
                        feed_id, e
                    );
                    original
                }
            }
        } else {
            feed_entry
        };

        let tx = conn.transaction()?;
        let entry_id = upsert_entry(&tx, feed_id, "rss", &feed_entry)?;
        insert_rss_entry_data(&tx, entry_id, &ingest, feed_entry.content.as_deref())?;
        tx.commit()?;
        metrics.record_feed_entry_upserted("rss");
        inserted_entry_ids.push(entry_id);

        if !feed_entry.tags.is_empty() {
            sync_entry_tags(&conn, feed_id, &feed_entry.guid, &feed_entry.tags)?;
        }
        seen_guids.push(feed_entry.guid);
    }

    mark_dropped_entries(&conn, feed_id, parsed_count, &seen_guids)?;
    Ok(inserted_entry_ids)
}

/// Insert an entry, or update the existing row for the same
/// `(feed_id, guid)` in place, and return its id.
///
/// Updating in place keeps the entry's id stable across refreshes, and
/// with it everything keyed on that id (tags, cached assets, the search
/// index). The entry is in the feed again, so `dropped_at` is cleared.
fn upsert_entry(
    tx: &rusqlite::Transaction,
    feed_id: i64,
    syndication_format: &str,
    entry: &FeedEntry,
) -> Result<i64> {
    let entry_id = tx.query_row(
        "INSERT INTO entries (
            feed_id,
            syndication_format,
            guid,
            published_at,
            title,
            url,
            content
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
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
    Ok(entry_id)
}

/// Mark the feed's stored entries that this refresh did not store as
/// dropped, starting their retention clock.
///
/// `parsed_count` is how many entries the feed document held before
/// scripts ran. A document with none is more likely a publisher glitch
/// than a feed that really emptied, so it marks nothing; entries that
/// scripts filtered out are marked like any other.
fn mark_dropped_entries(
    conn: &PooledConnection<SqliteConnectionManager>,
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
/// entry IDs. Best-effort: a full or closed queue is logged and ignored.
pub(super) fn enqueue_asset_caching(
    task_tx: &async_channel::Sender<TaskManagerCommand>,
    metrics: &Metrics,
    entry_ids: &[i64],
) {
    for &entry_id in entry_ids {
        match task_tx.try_send(TaskManagerCommand::CacheEntryAssets { entry_id }) {
            Ok(()) => metrics.record_task_enqueued("cache_entry_assets"),
            Err(e) => debug!(
                "failed to queue CacheEntryAssets for entry {}: {:?}",
                entry_id, e
            ),
        }
    }
}

/// Resolve and sync the script-provided tags for a newly inserted database entry.
///
/// For each tag name in `tags`:
/// - ensures the tag row exists in `tags` (`INSERT OR IGNORE`)
/// - looks up its `id`
///
/// Then removes any `entry_tags` rows for this entry whose `tag_id` is not in the
/// script-provided set, and inserts new associations (`INSERT OR IGNORE`).
fn sync_entry_tags(
    conn: &PooledConnection<SqliteConnectionManager>,
    feed_id: i64,
    guid: &str,
    tags: &[String],
) -> Result<()> {
    let entry_id: i64 = conn.query_row(
        "SELECT id FROM entries WHERE feed_id = ?1 AND guid = ?2",
        rusqlite::params![feed_id, guid],
        |row| row.get(0),
    )?;

    // Upsert each tag and collect its id.
    let mut tag_ids: Vec<i64> = Vec::with_capacity(tags.len());
    for name in tags {
        conn.execute("INSERT OR IGNORE INTO tags (name) VALUES (?1)", [name])?;
        let id: i64 = conn.query_row(
            "SELECT id FROM tags WHERE name = ?1",
            [name.as_str()],
            |row| row.get(0),
        )?;
        tag_ids.push(id);
    }

    // Remove stale entry_tags rows (those not in the script-provided set).
    if tag_ids.is_empty() {
        conn.execute("DELETE FROM entry_tags WHERE entry_id = ?1", [entry_id])?;
    } else {
        let placeholders = tag_ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 2))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "DELETE FROM entry_tags WHERE entry_id = ?1 AND tag_id NOT IN ({placeholders})"
        );
        let params: Vec<rusqlite::types::Value> =
            std::iter::once(rusqlite::types::Value::Integer(entry_id))
                .chain(
                    tag_ids
                        .iter()
                        .map(|&id| rusqlite::types::Value::Integer(id)),
                )
                .collect();
        conn.execute(&sql, rusqlite::params_from_iter(params))?;
    }

    // Insert new tag associations.
    for tag_id in tag_ids {
        conn.execute(
            "INSERT OR IGNORE INTO entry_tags (entry_id, tag_id) VALUES (?1, ?2)",
            rusqlite::params![entry_id, tag_id],
        )?;
    }

    Ok(())
}
