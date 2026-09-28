use crate::db::tags::{is_reserved_tag_name, SystemTag};
use crate::fetcher::{AtomEntry, AtomFeedIngestData, RssEntry};
use crate::metrics::Metrics;
use crate::scripting::{FeedEntry, ScriptRunner};
use crate::tasks::command::TaskManagerCommand;
use crate::tasks::parsing::{insert_atom_entry_data, insert_rss_entry_data, upsert_atom_feed_data};
use anyhow::Result;
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Connection, TransactionBehavior};
use std::time::Instant;
use tracing::{debug, warn};

/// Store a parsed Atom feed: its feed-level data, then each entry after
/// it has been through the script chain.
///
/// Returns the ids of the entries that were written.
///
/// The entries may have come from the isolated fetcher, so nothing in them
/// is trusted to say which feed they belong to: `feed_id` and the
/// syndication format are re-stamped from the caller's own values before
/// scripts or the database see them.
///
/// Scripts run first, outside any transaction; everything is then written
/// in a single `BEGIN IMMEDIATE` transaction, so a refresh takes the write
/// lock once rather than once per entry, and waits for it (up to the busy
/// timeout) instead of failing with "database is locked" when it would
/// have to upgrade a read.
pub(super) fn process_atom_feed(
    feed_id: i64,
    feed_data: AtomFeedIngestData,
    entries: Vec<AtomEntry>,
    mut conn: PooledConnection<SqliteConnectionManager>,
    script_runner: Option<&dyn ScriptRunner>,
    metrics: &Metrics,
) -> Result<Vec<i64>> {
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

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "UPDATE feeds SET syndication_format = 'atom' WHERE id = ?1",
        [feed_id],
    )?;
    upsert_atom_feed_data(&tx, feed_id, &feed_data)?;

    let mut inserted_entry_ids: Vec<i64> = Vec::with_capacity(entries.len());
    let mut seen_guids: Vec<String> = Vec::with_capacity(entries.len());
    for (feed_entry, ingest) in entries {
        let (entry_id, is_new) = upsert_entry(&tx, feed_id, "atom", &feed_entry)?;
        insert_atom_entry_data(&tx, entry_id, &ingest)?;
        if !feed_entry.tags.is_empty() {
            sync_entry_tags(&tx, entry_id, &feed_entry.tags, is_new)?;
        }
        inserted_entry_ids.push(entry_id);
        seen_guids.push(feed_entry.guid);
    }

    mark_dropped_entries(&tx, feed_id, parsed_count, &seen_guids)?;
    tx.commit()?;
    for _ in &inserted_entry_ids {
        metrics.record_feed_entry_upserted("atom");
    }
    Ok(inserted_entry_ids)
}

/// Store a parsed RSS feed, each item after it has been through the
/// script chain.
///
/// Returns the ids of the entries that were written. As with
/// [`process_atom_feed`], `feed_id` and the syndication format are
/// re-stamped on every entry rather than trusted, and everything is
/// written in one immediate transaction once the scripts have run.
pub(super) fn process_rss_feed(
    feed_id: i64,
    entries: Vec<RssEntry>,
    mut conn: PooledConnection<SqliteConnectionManager>,
    script_runner: Option<&dyn ScriptRunner>,
    metrics: &Metrics,
) -> Result<Vec<i64>> {
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

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "UPDATE feeds SET syndication_format = 'rss' WHERE id = ?1",
        [feed_id],
    )?;

    let mut inserted_entry_ids: Vec<i64> = Vec::with_capacity(entries.len());
    let mut seen_guids: Vec<String> = Vec::with_capacity(entries.len());
    for (feed_entry, ingest) in entries {
        let (entry_id, is_new) = upsert_entry(&tx, feed_id, "rss", &feed_entry)?;
        insert_rss_entry_data(&tx, entry_id, &ingest, feed_entry.content.as_deref())?;
        if !feed_entry.tags.is_empty() {
            sync_entry_tags(&tx, entry_id, &feed_entry.tags, is_new)?;
        }
        inserted_entry_ids.push(entry_id);
        seen_guids.push(feed_entry.guid);
    }

    mark_dropped_entries(&tx, feed_id, parsed_count, &seen_guids)?;
    tx.commit()?;
    for _ in &inserted_entry_ids {
        metrics.record_feed_entry_upserted("rss");
    }
    Ok(inserted_entry_ids)
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
    let format = feed_entry.syndication_format.clone();
    runner.dispatch_observe(
        crate::scripting::Event::EntryParsed,
        crate::scripting::EventPayload::Entry(feed_entry.clone()),
    );
    let original = feed_entry.clone();
    let script_start = Instant::now();
    match runner.dispatch_transform_entry(feed_entry) {
        Ok(Some(e)) => {
            metrics.record_script_execution(script_start.elapsed().as_secs_f64(), "ok");
            Some(e)
        }
        Ok(None) => {
            metrics.record_script_execution(script_start.elapsed().as_secs_f64(), "filtered");
            debug!("{} entry filtered by script for feed {}", format, feed_id);
            None
        }
        Err(e) => {
            metrics.record_script_execution(script_start.elapsed().as_secs_f64(), "error");
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
/// Otherwise, for each other (user) tag name in `tags`:
/// - ensures the tag row exists in `tags` (`INSERT OR IGNORE`)
/// - looks up its `id`
///
/// Then removes any user-tag `entry_tags` rows for this entry whose `tag_id` is
/// not in the script-provided set, and inserts new associations (`INSERT OR
/// IGNORE`).
fn sync_entry_tags(conn: &Connection, entry_id: i64, tags: &[String], is_new: bool) -> Result<()> {
    let has_user_tags = tags.iter().any(|name| !is_reserved_tag_name(name));

    // Upsert each tag and collect its id.
    let mut tag_ids: Vec<i64> = Vec::with_capacity(tags.len());
    for name in tags {
        if is_reserved_tag_name(name) {
            match SystemTag::ALL.into_iter().find(|tag| tag.name() == name) {
                Some(tag) if is_new => {
                    conn.execute(
                        "INSERT OR IGNORE INTO entry_tags (entry_id, tag_id) VALUES (?1, ?2)",
                        rusqlite::params![entry_id, tag.id(conn)?],
                    )?;
                }
                Some(_) => {}
                None => warn!(
                    "ignoring tag {:?} set by a script on entry {}: it is not a system tag, and names starting with \"system:\" are reserved for system tags",
                    name, entry_id
                ),
            }
            continue;
        }
        conn.execute("INSERT OR IGNORE INTO tags (name) VALUES (?1)", [name])?;
        let id: i64 = conn.query_row(
            "SELECT id FROM tags WHERE name = ?1",
            [name.as_str()],
            |row| row.get(0),
        )?;
        tag_ids.push(id);
    }

    if !has_user_tags {
        return Ok(());
    }

    // Remove stale user-tag entry_tags rows (those not in the script-provided
    // set).
    const USER_TAGS: &str = "tag_id IN (SELECT id FROM tags WHERE kind = 'user')";
    if tag_ids.is_empty() {
        conn.execute(
            &format!("DELETE FROM entry_tags WHERE entry_id = ?1 AND {USER_TAGS}"),
            [entry_id],
        )?;
    } else {
        let placeholders = tag_ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 2))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "DELETE FROM entry_tags WHERE entry_id = ?1 AND {USER_TAGS} \
             AND tag_id NOT IN ({placeholders})"
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
}
