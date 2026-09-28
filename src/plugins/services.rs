//! The server's answers to the calls plugins make through the `kiki` Lua
//! API: their key-value stores (`kiki.store`), tagging stored entries
//! (`kiki.entries.tag` and `untag`), and scans of stored entries
//! (`kiki.entries.scan`).
//!
//! # Scans
//!
//! A scan visits the stored entries a plugin asked for, oldest first,
//! handing them to the plugin's scan handler in batches, on a thread of its
//! own so that feed refreshes carry on alongside it. Of what the handler
//! returns, only the system tags it adds (such as `system:hidden`) are
//! applied, just as they would be to a new entry; to change anything else
//! the handler calls `kiki.entries.tag` or `untag` itself.
//!
//! A plugin runs at most one scan at a time: starting another cancels the
//! first. Reloading the plugins ends every scan, since the handlers they
//! were running belong to the plugins that were unloaded, and the server
//! shutting down ends them too.

use crate::db::plugins::{store_get, store_set};
use crate::db::tags::{is_reserved_tag_name, SystemTag};
use crate::scripting::{
    FeedEntry, ScanOptions, ScanSummary, ScriptRunnerHandle, ScriptServices, ServiceCall,
    ServiceReply,
};
use arc_swap::ArcSwap;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// How many entries a scan reads from the database at a time. Each batch's
/// tags are written in one short transaction, so feed refreshes are not held
/// up behind a long scan.
const READ_BATCH: usize = 200;

/// Most entries a scan hands to the plugin per dispatch. The runner hands
/// back the ones it did not reach once a dispatch has taken
/// [`SCAN_SLICE`](crate::scripting::lua::SCAN_SLICE), so events queued
/// behind a scan wait for one slice at most, not a whole batch.
const DISPATCH_BATCH: usize = 25;

/// Longest user tag name a plugin may set.
const MAX_TAG_NAME_BYTES: usize = 255;

/// Most user tags there may be for a plugin to create another. Plugins can
/// tag entries with existing user tags past this, but not add new ones.
pub const MAX_USER_TAGS: i64 = 10_000;

type Pool = r2d2::Pool<SqliteConnectionManager>;

/// Answers the calls plugins make. See the [module documentation](self).
pub struct ServerServices {
    pool: Pool,
    /// The names of the plugins loaded into the script runner.
    loaded: ArcSwap<HashSet<String>>,
    scans: Arc<ScanState>,
}

/// What the scan threads share.
struct ScanState {
    pool: Pool,
    runner: ScriptRunnerHandle,
    cancel: CancellationToken,
    next_id: AtomicU64,
    /// The running scan of each plugin, with the token that cancels it.
    running: Mutex<HashMap<String, (u64, CancellationToken)>>,
}

impl ServerServices {
    /// Answers calls from plugins, dispatching scans to the runner in
    /// `runner`. Scans stop when `cancel` fires. No calls are answered
    /// until [`Self::set_loaded`] names the plugins that are loaded.
    pub fn new(pool: Pool, runner: ScriptRunnerHandle, cancel: CancellationToken) -> Self {
        Self {
            scans: Arc::new(ScanState {
                pool: pool.clone(),
                runner,
                cancel,
                next_id: AtomicU64::new(1),
                running: Mutex::new(HashMap::new()),
            }),
            pool,
            loaded: ArcSwap::from_pointee(HashSet::new()),
        }
    }

    /// Answers calls only from the plugins named in `names`, the plugins
    /// being loaded into the script runner, and returns the names it
    /// answered calls from before.
    pub fn set_loaded(&self, names: HashSet<String>) -> Arc<HashSet<String>> {
        self.loaded.swap(Arc::new(names))
    }

    fn conn(&self) -> Result<r2d2::PooledConnection<SqliteConnectionManager>, String> {
        self.pool
            .get()
            .map_err(|e| format!("database unavailable: {e}"))
    }
}

impl ScriptServices for ServerServices {
    fn call(&self, plugin: &str, call: ServiceCall) -> Result<ServiceReply, String> {
        // The name comes from the script host, which is not trusted to
        // make it up: only loaded plugins get stores and scans.
        if !self.loaded.load().contains(plugin) {
            return Err(format!("no plugin named {plugin:?} is loaded"));
        }

        match call {
            ServiceCall::StoreGet { key } => {
                let value = store_get(&*self.conn()?, plugin, &key).map_err(|e| e.to_string())?;
                Ok(ServiceReply::Value(value.map(|v| v.to_string())))
            }
            ServiceCall::StoreSet { key, value } => {
                let value = value
                    .map(|text| serde_json::from_str(&text))
                    .transpose()
                    .map_err(|e| format!("invalid value: {e}"))?;
                store_set(&*self.conn()?, plugin, &key, value.as_ref())
                    .map_err(|e| e.to_string())?;
                Ok(ServiceReply::Done)
            }
            ServiceCall::SetEntryTag {
                entry_id,
                tag,
                present,
            } => {
                let changed = set_entry_tag(&*self.conn()?, entry_id, &tag, present)?;
                Ok(ServiceReply::Changed(changed))
            }
            ServiceCall::StartScan { options } => {
                Ok(ServiceReply::ScanStarted(self.scans.start(plugin, options)))
            }
        }
    }
}

/// Adds the tag named `tag` to the entry `entry_id`, or removes it when
/// `present` is false, and returns whether that changed anything.
///
/// A user tag that does not exist yet is created. Names starting with
/// `system:` must name a system tag.
fn set_entry_tag(
    conn: &Connection,
    entry_id: i64,
    tag: &str,
    present: bool,
) -> Result<bool, String> {
    let db = |e: rusqlite::Error| format!("database error: {e}");
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM entries WHERE id = ?1)",
            [entry_id],
            |row| row.get(0),
        )
        .map_err(db)?;
    if !exists {
        return Err(format!("no entry with id {entry_id}"));
    }

    let tag_id = if is_reserved_tag_name(tag) {
        let system = SystemTag::ALL
            .into_iter()
            .find(|t| t.name() == tag)
            .ok_or_else(|| format!("{tag:?} is not a system tag"))?;
        Some(system.id(conn).map_err(db)?)
    } else {
        if tag.trim().is_empty() || tag.len() > MAX_TAG_NAME_BYTES {
            return Err(format!(
                "tag names must be between 1 and {MAX_TAG_NAME_BYTES} bytes long and not blank"
            ));
        }
        if present {
            create_user_tag(conn, tag)?;
        }
        conn.query_row("SELECT id FROM tags WHERE name = ?1", [tag], |row| {
            row.get(0)
        })
        .optional()
        .map_err(db)?
    };
    let Some(tag_id) = tag_id else {
        // Removing a user tag that does not exist.
        return Ok(false);
    };

    let sql = if present {
        "INSERT OR IGNORE INTO entry_tags (entry_id, tag_id) VALUES (?1, ?2)"
    } else {
        "DELETE FROM entry_tags WHERE entry_id = ?1 AND tag_id = ?2"
    };
    Ok(conn.execute(sql, [entry_id, tag_id]).map_err(db)? > 0)
}

/// Creates the user tag named `tag`, unless it exists already.
///
/// # Errors
///
/// Returns an error if the tag does not exist and there are already
/// [`MAX_USER_TAGS`] user tags.
fn create_user_tag(conn: &Connection, tag: &str) -> Result<(), String> {
    let db = |e: rusqlite::Error| format!("database error: {e}");
    let (exists, count): (bool, i64) = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM tags WHERE name = ?1),
                    (SELECT COUNT(*) FROM tags WHERE kind = 'user')",
            [tag],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(db)?;
    if !exists && count >= MAX_USER_TAGS {
        return Err(format!(
            "there are already {MAX_USER_TAGS} user tags, so plugins cannot create more"
        ));
    }
    conn.execute("INSERT OR IGNORE INTO tags (name) VALUES (?1)", [tag])
        .map_err(db)?;
    Ok(())
}

impl ScanState {
    /// Starts a scan for `plugin`, cancelling the one it was running, and
    /// returns its id.
    fn start(self: &Arc<Self>, plugin: &str, options: ScanOptions) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let token = self.cancel.child_token();
        let previous = self
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(plugin.to_string(), (id, token.clone()));
        if let Some((previous, token)) = previous {
            debug!(plugin, "scan {id} replaces scan {previous}");
            token.cancel();
        }

        let state = self.clone();
        let plugin = plugin.to_string();
        let spawned = std::thread::Builder::new()
            .name(format!("kiki-scan-{id}"))
            .spawn({
                let plugin = plugin.clone();
                move || state.run(&plugin, id, &options, &token)
            });
        if let Err(e) = spawned {
            warn!(plugin, "could not start a scan: {e}");
            self.finished(&plugin, id);
        }
        id
    }

    /// Runs the scan `id` to the end, then forgets it, calling its
    /// `on_done` callback if it went through every entry.
    fn run(&self, plugin: &str, id: u64, options: &ScanOptions, cancel: &CancellationToken) {
        let mut summary = ScanSummary::default();
        let result = scan(&self.pool, &self.runner, id, options, cancel, |s, u| {
            summary.scanned += s;
            summary.updated += u;
        });
        let completed = match result {
            Ok(true) => {
                info!(
                    plugin,
                    scanned = summary.scanned,
                    updated = summary.updated,
                    "scan {id} of stored entries finished"
                );
                true
            }
            Ok(false) => {
                debug!(plugin, "scan {id} ended before it finished");
                false
            }
            Err(e) => {
                warn!(plugin, "scan {id} of stored entries failed: {e:#}");
                false
            }
        };
        if let Some(runner) = self.runner.current() {
            runner.finish_scan(id, completed.then_some(summary));
        }
        self.finished(plugin, id);
    }

    fn finished(&self, plugin: &str, id: u64) {
        let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        if running
            .get(plugin)
            .is_some_and(|(running_id, _)| *running_id == id)
        {
            running.remove(plugin);
        }
    }
}

/// Runs the scan `scan_id` over the entries `options` describes, calling
/// `progress` with the number of entries scanned and updated after each
/// batch. Returns whether it went through every entry: it stops early,
/// without error, when `cancel` fires or the runner no longer knows the
/// scan.
///
/// # Errors
///
/// Returns an error if the database cannot be read or written, or the
/// entries cannot be dispatched. The batches written before then stay
/// written.
pub(crate) fn scan(
    pool: &Pool,
    runner: &ScriptRunnerHandle,
    scan_id: u64,
    options: &ScanOptions,
    cancel: &CancellationToken,
    mut progress: impl FnMut(u64, u64),
) -> anyhow::Result<bool> {
    let hidden = SystemTag::Hidden.id(&*pool.get()?)?;
    let mut after_id = 0;
    loop {
        if cancel.is_cancelled() {
            return Ok(false);
        }
        let batch = load_batch(&*pool.get()?, options, hidden, after_id)?;
        let Some((last_id, _)) = batch.last() else {
            return Ok(true);
        };
        after_id = *last_id;

        let mut scanned = 0;
        let mut to_tag: Vec<(i64, Vec<SystemTag>)> = Vec::new();
        let mut pending: std::collections::VecDeque<FeedEntry> =
            batch.into_iter().map(|(_, e)| e).collect();
        let mut stopped = false;
        while !pending.is_empty() {
            // Read the runner for each dispatch: after a reload, the scan
            // belongs to a runner that is gone, and the new one says so.
            let Some(runner) = runner.current() else {
                stopped = true;
                break;
            };
            let take = pending.len().min(DISPATCH_BATCH);
            let chunk: Vec<FeedEntry> = pending.drain(..take).collect();
            let ids: Vec<i64> = chunk.iter().filter_map(|e| e.id).collect();
            let Some(results) = runner.dispatch_scan(scan_id, chunk.clone())? else {
                stopped = true;
                break;
            };
            anyhow::ensure!(
                !results.is_empty() && results.len() <= ids.len(),
                "the runner handled {} of {} entries",
                results.len(),
                ids.len()
            );
            // Put back the entries the runner did not reach.
            for entry in chunk.into_iter().skip(results.len()).rev() {
                pending.push_front(entry);
            }
            scanned += results.len() as u64;
            for (entry_id, result) in ids.into_iter().zip(results) {
                let tags = result.map(|e| system_tags(&e.tags)).unwrap_or_default();
                if !tags.is_empty() {
                    to_tag.push((entry_id, tags));
                }
            }
            if cancel.is_cancelled() {
                stopped = true;
                break;
            }
        }

        // Whatever the handler already returned is applied, even if the
        // scan stops here.
        let updated = if to_tag.is_empty() {
            0
        } else {
            apply_system_tags(&mut *pool.get()?, &to_tag)?
        };
        progress(scanned, updated);
        if stopped {
            return Ok(false);
        }
    }
}

/// The known system tags in `tags`.
fn system_tags(tags: &[String]) -> Vec<SystemTag> {
    SystemTag::ALL
        .into_iter()
        .filter(|tag| tags.iter().any(|t| t == tag.name()))
        .collect()
}

/// Adds the system tags in `to_tag` to their entries, in one transaction,
/// and returns how many entries gained a tag. Entries deleted since they
/// were read are skipped.
fn apply_system_tags(
    conn: &mut Connection,
    to_tag: &[(i64, Vec<SystemTag>)],
) -> anyhow::Result<u64> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut updated = 0;
    {
        let mut insert = tx.prepare(
            "INSERT OR IGNORE INTO entry_tags (entry_id, tag_id)
             SELECT id, ?2 FROM entries WHERE id = ?1",
        )?;
        for (entry_id, tags) in to_tag {
            let mut changed = false;
            for tag in tags {
                changed |= insert.execute(rusqlite::params![entry_id, tag.id(&tx)?])? > 0;
            }
            updated += u64::from(changed);
        }
    }
    tx.commit()?;
    Ok(updated)
}

/// Reads the next batch of entries `options` describes with ids above
/// `after_id`, in id order, as the entry tables plugins see. `hidden` is
/// the id of the `system:hidden` tag.
///
/// Entries whose feed has been deleted are skipped: plugins always see an
/// entry's feed.
fn load_batch(
    conn: &Connection,
    options: &ScanOptions,
    hidden: i64,
    after_id: i64,
) -> rusqlite::Result<Vec<(i64, FeedEntry)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, feed_id, syndication_format, guid, published_at, title, url, content
         FROM entries e
         WHERE id > ?1 AND feed_id IS NOT NULL
           AND (?2 IS NULL OR feed_id = ?2)
           AND (?3 IS NULL OR published_at >= ?3)
           AND (?4 OR NOT EXISTS (
               SELECT 1 FROM entry_tags et WHERE et.entry_id = e.id AND et.tag_id = ?5
           ))
         ORDER BY id
         LIMIT ?6",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![
            after_id,
            options.feed_id,
            options.since,
            options.include_hidden,
            hidden,
            READ_BATCH as i64
        ],
        |row| {
            let id: i64 = row.get(0)?;
            Ok((
                id,
                FeedEntry {
                    id: Some(id),
                    feed_id: row.get(1)?,
                    syndication_format: row.get(2)?,
                    guid: row.get(3)?,
                    published_at: row.get(4)?,
                    title: row.get(5)?,
                    url: row.get(6)?,
                    content: row.get(7)?,
                    authors: Vec::new(),
                    categories: Vec::new(),
                    tags: Vec::new(),
                },
            ))
        },
    )?;
    let mut batch = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    for (entry_id, entry) in &mut batch {
        if entry.syndication_format == "atom" {
            entry.authors = strings(
                conn,
                "SELECT author FROM atom_entry_authors WHERE entry_id = ?1 ORDER BY id",
                *entry_id,
            )?;
            entry.categories = strings(
                conn,
                "SELECT c.category FROM atom_entry_categories ec
                 JOIN atom_categories c ON c.id = ec.category_id
                 WHERE ec.entry_id = ?1 ORDER BY ec.rowid",
                *entry_id,
            )?;
        } else {
            entry.authors = conn
                .prepare_cached("SELECT author FROM rss_entry_data WHERE entry_id = ?1")?
                .query_row([*entry_id], |row| row.get::<_, Option<String>>(0))
                .optional()?
                .flatten()
                .into_iter()
                .collect();
            entry.categories = strings(
                conn,
                "SELECT category FROM rss_categories WHERE entry_id = ?1 ORDER BY rowid",
                *entry_id,
            )?;
        }
    }
    Ok(batch)
}

/// Runs `sql`, a query of one text column taking an entry id, for `entry_id`.
fn strings(conn: &Connection, sql: &str, entry_id: i64) -> rusqlite::Result<Vec<String>> {
    conn.prepare_cached(sql)?
        .query_map([entry_id], |row| row.get(0))?
        .collect()
}

#[cfg(all(test, feature = "lua"))]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::scripting::lua::LuaScriptRunner;
    use crate::scripting::{Event, EventPayload, ScriptRunner, ScriptSource};
    use rusqlite::params;
    use std::time::{Duration, Instant};

    /// A database in a temporary directory, with a pool on it. The
    /// directory is kept for the rest of the test process.
    fn pool() -> Pool {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("kiki.db");
        crate::db::ConnectionBuilder::default()
            .at_path(&path)
            .create()
            .build()
            .unwrap();
        std::mem::forget(td);
        r2d2::Pool::new(SqliteConnectionManager::file(path)).unwrap()
    }

    fn insert_feed(conn: &Connection) -> i64 {
        conn.execute("INSERT INTO feeds (title) VALUES ('f')", [])
            .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_entry(conn: &Connection, feed_id: i64, format: &str, title: &str) -> i64 {
        conn.execute(
            "INSERT INTO entries (feed_id, syndication_format, guid, published_at, title, url)
             VALUES (?1, ?2, ?3, 0, ?3, 'u')",
            params![feed_id, format, title],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn tags(conn: &Connection, entry_id: i64) -> Vec<String> {
        strings(
            conn,
            "SELECT t.name FROM tags t JOIN entry_tags et ON et.tag_id = t.id
             WHERE et.entry_id = ?1 ORDER BY t.name",
            entry_id,
        )
        .unwrap()
    }

    /// A plugin named `p`, running `text`, loaded into an in-process runner
    /// whose calls `services` answers, as the server wires them up.
    struct Harness {
        pool: Pool,
        runner: ScriptRunnerHandle,
        services: Arc<ServerServices>,
    }

    impl Harness {
        fn new(pool: Pool, text: &str) -> Self {
            let runner = ScriptRunnerHandle::empty();
            let services = Arc::new(ServerServices::new(
                pool.clone(),
                runner.clone(),
                CancellationToken::new(),
            ));
            services.set_loaded(HashSet::from(["p".to_string()]));
            let mut source = ScriptSource::new(text);
            source.name = "p".to_string();
            let lua = LuaScriptRunner::from_sources_with(
                &[source],
                Some(services.clone() as Arc<dyn ScriptServices>),
            )
            .unwrap();
            runner.set(Some(Arc::new(lua) as Arc<dyn ScriptRunner>));
            Self {
                pool,
                runner,
                services,
            }
        }

        fn load(&self) {
            self.runner
                .current()
                .unwrap()
                .dispatch_observe(Event::PluginLoad, EventPayload::PluginLoad);
        }

        /// Waits until no scan is running.
        fn wait_for_scans(&self) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !self.services.scans.running.lock().unwrap().is_empty() {
                assert!(Instant::now() < deadline, "scan did not finish");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    const HIDE_SPAM: &str = r#"
        local function hide_spam(entry)
            entry.title = "changed"
            if entry.guid:find("spam") then
                table.insert(entry.tags, "system:hidden")
                table.insert(entry.tags, "user-tag")
            end
            if entry.guid == "drop" then return nil end
            return entry
        end
        local options = ...
        kiki.on("plugin.load", function()
            kiki.entries.scan(options, hide_spam)
        end)
    "#;

    #[test]
    fn scans_apply_only_system_tags() {
        let pool = pool();
        let conn = pool.get().unwrap();
        let feed = insert_feed(&conn);
        let ham = insert_entry(&conn, feed, "rss", "ham");
        let spam = insert_entry(&conn, feed, "rss", "spam");
        let dropped = insert_entry(&conn, feed, "rss", "drop");

        let h = Harness::new(pool.clone(), HIDE_SPAM);
        h.load();
        h.wait_for_scans();
        assert!(tags(&conn, ham).is_empty());
        assert_eq!(tags(&conn, spam), ["system:hidden"]);
        assert!(tags(&conn, dropped).is_empty());
        let title: String = conn
            .query_row("SELECT title FROM entries WHERE id = ?1", [spam], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(title, "spam");
    }

    #[test]
    fn scans_cover_every_batch_and_skip_hidden_entries() {
        let pool = pool();
        let conn = pool.get().unwrap();
        let feed = insert_feed(&conn);
        let total = READ_BATCH * 2 + 5;
        for i in 0..total {
            insert_entry(&conn, feed, "rss", &format!("spam-{i}"));
        }

        let h = Harness::new(pool.clone(), HIDE_SPAM);
        let hidden = SystemTag::Hidden.id(&conn).unwrap();
        let mut counts = (0, 0);
        scan(
            &h.pool,
            &h.runner,
            0,
            &ScanOptions::default(),
            &CancellationToken::new(),
            |s, u| {
                counts.0 += s;
                counts.1 += u;
            },
        )
        .unwrap();
        // No scan 0 is registered, so the runner stops it at once.
        assert_eq!(counts, (0, 0));

        h.load();
        h.wait_for_scans();
        let count: usize = conn
            .query_row(
                "SELECT COUNT(*) FROM entry_tags WHERE tag_id = ?1",
                [hidden],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, total);

        // Hidden entries are skipped by default.
        let batch = load_batch(&conn, &ScanOptions::default(), hidden, 0).unwrap();
        assert!(batch.is_empty());
        let options = ScanOptions {
            include_hidden: true,
            ..Default::default()
        };
        assert_eq!(
            load_batch(&conn, &options, hidden, 0).unwrap().len(),
            READ_BATCH
        );
    }

    #[test]
    fn scans_can_be_limited() {
        let pool = pool();
        let conn = pool.get().unwrap();
        let a = insert_feed(&conn);
        let b = insert_feed(&conn);
        let in_a = insert_entry(&conn, a, "rss", "spam-a");
        let in_b = insert_entry(&conn, b, "rss", "spam-b");
        let hidden = SystemTag::Hidden.id(&conn).unwrap();

        let options = ScanOptions {
            feed_id: Some(b),
            ..Default::default()
        };
        let batch = load_batch(&conn, &options, hidden, 0).unwrap();
        assert_eq!(batch.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [in_b]);

        conn.execute(
            "UPDATE entries SET published_at = 100 WHERE id = ?1",
            [in_a],
        )
        .unwrap();
        let options = ScanOptions {
            since: Some(50),
            ..Default::default()
        };
        let batch = load_batch(&conn, &options, hidden, 0).unwrap();
        assert_eq!(batch.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [in_a]);
    }

    #[test]
    fn scanned_entries_carry_their_ids_authors_and_categories() {
        let pool = pool();
        let conn = pool.get().unwrap();
        let feed = insert_feed(&conn);
        let rss = insert_entry(&conn, feed, "rss", "r");
        conn.execute(
            "INSERT INTO rss_entry_data (entry_id, author) VALUES (?1, 'Ada')",
            [rss],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO rss_categories (entry_id, category) VALUES (?1, 'ads')",
            [rss],
        )
        .unwrap();
        let atom = insert_entry(&conn, feed, "atom", "a");
        conn.execute(
            "INSERT INTO atom_entry_authors (entry_id, author) VALUES (?1, 'Ada')",
            [atom],
        )
        .unwrap();
        conn.execute("INSERT INTO atom_categories (category) VALUES ('ads')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO atom_entry_categories (entry_id, category_id) VALUES (?1, ?2)",
            params![atom, conn.last_insert_rowid()],
        )
        .unwrap();

        // Tags entries through kiki.entries.tag rather than by returning
        // them, to cover that too.
        let h = Harness::new(
            pool.clone(),
            r#"
            kiki.on("plugin.load", function()
                kiki.entries.scan(function(entry)
                    if entry.authors[1] == "Ada" and entry.categories[1] == "ads" then
                        kiki.entries.tag(entry.id, "ada-ads")
                    end
                end)
            end)
            "#,
        );
        h.load();
        h.wait_for_scans();
        assert_eq!(tags(&conn, rss), ["ada-ads"]);
        assert_eq!(tags(&conn, atom), ["ada-ads"]);
    }

    #[test]
    fn plugins_can_use_their_store() {
        let pool = pool();
        let h = Harness::new(
            pool.clone(),
            r#"
            kiki.on("plugin.load", function()
                assert(kiki.store.get("missing") == nil)
                kiki.store.set("rules", { a = { 1, 2 }, b = "x" })
                local rules = kiki.store.get("rules")
                assert(rules.a[2] == 2 and rules.b == "x")
                kiki.store.set("gone", true)
                kiki.store.set("gone", nil)
                assert(kiki.store.get("gone") == nil)
                kiki.store.set("done", true)
            end)
            "#,
        );
        h.load();
        let conn = pool.get().unwrap();
        assert_eq!(
            store_get(&conn, "p", "done").unwrap(),
            Some(serde_json::json!(true))
        );
    }

    #[test]
    fn tagging_validates_tags_and_entries() {
        let pool = pool();
        let conn = pool.get().unwrap();
        let feed = insert_feed(&conn);
        let entry = insert_entry(&conn, feed, "rss", "e");

        assert!(set_entry_tag(&conn, entry, "system:hidden", true).unwrap());
        assert!(!set_entry_tag(&conn, entry, "system:hidden", true).unwrap());
        assert!(set_entry_tag(&conn, entry, "system:hidden", false).unwrap());
        assert!(set_entry_tag(&conn, entry, "news", true).unwrap());
        assert!(!set_entry_tag(&conn, entry, "never-created", false).unwrap());
        assert!(set_entry_tag(&conn, entry, "system:new", true).is_err());
        assert!(set_entry_tag(&conn, entry, " ", true).is_err());
        assert!(set_entry_tag(&conn, entry + 1, "news", true).is_err());
        assert_eq!(tags(&conn, entry), ["news"]);
    }

    #[test]
    fn unknown_plugins_are_refused() {
        let h = Harness::new(pool(), "");
        let err = h
            .services
            .call("other", ServiceCall::StoreGet { key: "k".into() })
            .unwrap_err();
        assert!(err.contains("no plugin"), "{err}");
    }

    #[test]
    fn scans_cannot_start_while_plugins_load() {
        let pool = pool();
        let services =
            ServerServices::new(pool, ScriptRunnerHandle::empty(), CancellationToken::new());
        services.set_loaded(HashSet::from([ScriptSource::new("").name]));
        let services: Arc<dyn ScriptServices> = Arc::new(services);
        let err = LuaScriptRunner::from_sources_with(
            &[ScriptSource::new("kiki.entries.scan(function() end)")],
            Some(services),
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("plugin.load"), "{err}");
    }

    /// A scan's `on_done` callback runs once it has gone through every
    /// entry, with a summary; a scan replaced by another never calls it.
    #[test]
    fn on_done_runs_only_when_a_scan_completes() {
        let pool = pool();
        let conn = pool.get().unwrap();
        let feed = insert_feed(&conn);
        for i in 0..(READ_BATCH + 3) {
            insert_entry(&conn, feed, "rss", &format!("spam-{i}"));
        }

        let h = Harness::new(
            pool.clone(),
            r#"
            local function hide(entry)
                table.insert(entry.tags, "system:hidden")
                return entry
            end
            kiki.on("plugin.load", function()
                -- Replaced at once by the second scan, so never done.
                kiki.entries.scan(hide, function() kiki.store.set("first", true) end)
                kiki.entries.scan(hide, function(summary)
                    kiki.store.set("summary", summary)
                end)
            end)
            "#,
        );
        h.load();
        let deadline = Instant::now() + Duration::from_secs(10);
        while store_get(&conn, "p", "summary").unwrap().is_none() {
            assert!(Instant::now() < deadline, "on_done never ran");
            std::thread::sleep(Duration::from_millis(10));
        }
        h.wait_for_scans();
        let total = (READ_BATCH + 3) as u64;
        assert_eq!(
            store_get(&conn, "p", "summary").unwrap(),
            Some(serde_json::json!({"scanned": total, "updated": total}))
        );
        assert_eq!(store_get(&conn, "p", "first").unwrap(), None);
    }

    #[test]
    fn plugins_cannot_create_tags_past_the_limit() {
        let pool = pool();
        let mut conn = pool.get().unwrap();
        let feed = insert_feed(&conn);
        let entry = insert_entry(&conn, feed, "rss", "e");
        {
            let tx = conn.transaction().unwrap();
            let existing: i64 = tx
                .query_row("SELECT COUNT(*) FROM tags WHERE kind = 'user'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            for i in existing..MAX_USER_TAGS {
                tx.execute("INSERT INTO tags (name) VALUES (?1)", [format!("t{i}")])
                    .unwrap();
            }
            tx.commit().unwrap();
        }
        let err = set_entry_tag(&conn, entry, "one-too-many", true).unwrap_err();
        assert!(err.contains("cannot create more"), "{err}");
        // Existing tags can still be used.
        assert!(set_entry_tag(&conn, entry, "t1", true).unwrap());
    }

    #[test]
    fn plugins_that_are_not_loaded_are_refused() {
        let h = Harness::new(pool(), "");
        let previous = h.services.set_loaded(HashSet::new());
        assert!(previous.contains("p"));
        let err = h
            .services
            .call("p", ServiceCall::StoreGet { key: "k".into() })
            .unwrap_err();
        assert!(err.contains("no plugin"), "{err}");
    }
}
