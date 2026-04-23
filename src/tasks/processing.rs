use crate::metrics::Metrics;
use crate::scripting::ScriptRunner;
use crate::tasks::command::TaskManagerCommand;
use crate::tasks::parsing::{
    atom_entry_to_parts, extract_atom_feed_data, insert_atom_entry_data, insert_rss_entry_data,
    rss_item_to_parts, upsert_atom_feed_data,
};
use anyhow::Result;
use r2d2::PooledConnection;
use r2d2_sqlite::SqliteConnectionManager;
use std::time::Instant;
use tracing::{debug, info, warn};

pub(super) fn process_atom_feed(
    feed_id: i64,
    feed: atom_syndication::Feed,
    mut conn: PooledConnection<SqliteConnectionManager>,
    script_runner: Option<&dyn ScriptRunner>,
    metrics: &Metrics,
) -> Result<Vec<i64>> {
    info!(
        "Successfully fetched Atom feed {} with {} items",
        feed_id,
        feed.entries.len()
    );

    let feed_data = extract_atom_feed_data(&feed);

    {
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE feeds SET syndication_format = 'atom' WHERE id = ?1",
            [feed_id],
        )?;
        upsert_atom_feed_data(&tx, feed_id, &feed_data)?;
        tx.commit()?;
    }

    let mut inserted_entry_ids: Vec<i64> = Vec::new();
    for entry in feed.entries.into_iter() {
        let (feed_entry, ingest) = atom_entry_to_parts(feed_id, entry);

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
        tx.execute(
            "INSERT OR REPLACE INTO entries (
                feed_id,
                syndication_format,
                guid,
                published_at,
                title,
                url,
                content
            ) VALUES (?1, 'atom', ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                feed_id,
                feed_entry.guid,
                feed_entry.published_at,
                feed_entry.title,
                feed_entry.url,
                feed_entry.content
            ],
        )?;

        let entry_id: i64 = tx.query_row(
            "SELECT id FROM entries WHERE feed_id = ?1 AND guid = ?2",
            rusqlite::params![feed_id, feed_entry.guid],
            |row| row.get(0),
        )?;

        insert_atom_entry_data(&tx, entry_id, &ingest)?;
        tx.commit()?;
        metrics.record_feed_entry_upserted("atom");
        inserted_entry_ids.push(entry_id);

        if !feed_entry.tags.is_empty() {
            sync_entry_tags(&conn, feed_id, &feed_entry.guid, &feed_entry.tags)?;
        }
    }

    Ok(inserted_entry_ids)
}

pub(super) fn process_rss_feed(
    feed_id: i64,
    channel: rss::Channel,
    mut conn: PooledConnection<SqliteConnectionManager>,
    script_runner: Option<&dyn ScriptRunner>,
    metrics: &Metrics,
) -> Result<Vec<i64>> {
    info!(
        "Successfully fetched RSS feed {} with {} items",
        feed_id,
        channel.items.len()
    );

    conn.execute(
        "UPDATE feeds SET syndication_format = 'rss' WHERE id = ?1",
        [feed_id],
    )?;

    let mut inserted_entry_ids: Vec<i64> = Vec::new();
    for item in channel.items.into_iter() {
        let (feed_entry, ingest) = rss_item_to_parts(feed_id, item);

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
        tx.execute(
            "INSERT OR REPLACE INTO entries (
                feed_id,
                syndication_format,
                guid,
                published_at,
                title,
                url,
                content
            ) VALUES (?1, 'rss', ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                feed_id,
                feed_entry.guid,
                feed_entry.published_at,
                feed_entry.title,
                feed_entry.url,
                feed_entry.content
            ],
        )?;

        let entry_id: i64 = tx.query_row(
            "SELECT id FROM entries WHERE feed_id = ?1 AND guid = ?2",
            rusqlite::params![feed_id, feed_entry.guid],
            |row| row.get(0),
        )?;

        insert_rss_entry_data(&tx, entry_id, &ingest)?;
        tx.commit()?;
        metrics.record_feed_entry_upserted("rss");
        inserted_entry_ids.push(entry_id);

        if !feed_entry.tags.is_empty() {
            sync_entry_tags(&conn, feed_id, &feed_entry.guid, &feed_entry.tags)?;
        }
    }

    Ok(inserted_entry_ids)
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
