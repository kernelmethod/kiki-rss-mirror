//! Timing the SQL statements Kiki runs.
//!
//! [`install`] asks SQLite to report every statement a connection finishes
//! (<https://sqlite.org/c3ref/trace_v2.html>) and records it into the
//! server's [`Metrics`]: how long it took and how many rows it read by
//! scanning a whole table rather than through an index. Statements are
//! labeled by their operation and the table they act on (see
//! [`statement_label`]), which keeps the number of series small while still
//! pointing at the query that needs work. Statements slower than
//! [`SLOW_STATEMENT`] are also logged in full, so the exact SQL is in the
//! journal.
//!
//! A statement is timed from its first step to its reset, so the duration
//! includes the time Kiki spends handling each row as it steps through the
//! results, not just the time SQLite spends producing them. (SQLite times
//! statements too, but only to the millisecond, which rounds most of Kiki's
//! down to nothing.)

use crate::metrics::{DbStatementSeries, Metrics};
use rusqlite::{ffi, Connection};
use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::{c_int, c_uint, c_void};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Statements that take at least this long are logged along with their SQL.
pub const SLOW_STATEMENT: Duration = Duration::from_millis(250);

/// What a connection's hook keeps between events.
struct Context {
    metrics: Arc<Metrics>,
    /// When each running statement, by address, started.
    ///
    /// A connection is only ever used by one thread at a time, so the lock
    /// is never contended; it is there because that thread can change.
    started: Mutex<HashMap<usize, Instant>>,
    /// The label and series of each statement recorded so far, by its SQL,
    /// so that a statement run again, as most are, is neither labeled nor
    /// looked up in the metrics registry again: FTS5 alone runs one of its
    /// own statements per matching entry when ranking search results.
    series: Mutex<HashMap<Box<str>, Series>>,
}

/// What runs of one statement are recorded under.
struct Series {
    op: &'static str,
    table: String,
    series: DbStatementSeries,
}

/// The most statements whose series a connection remembers. Kiki's own
/// statements are far fewer; past this, the connection starts over, so SQL
/// that differs every time cannot grow the map without bound.
const MAX_REMEMBERED_STATEMENTS: usize = 512;

/// Start recording every statement `conn` runs into `metrics`.
///
/// The connection holds its own reference to `metrics` until it closes.
/// Calling this again on the same connection replaces the previous hook,
/// and leaks the reference it held.
///
/// # Errors
///
/// Returns an error if SQLite refuses to install the hook.
pub fn install(conn: &Connection, metrics: Arc<Metrics>) -> rusqlite::Result<()> {
    let ctx = Box::into_raw(Box::new(Context {
        metrics,
        started: Mutex::new(HashMap::new()),
        series: Mutex::new(HashMap::new()),
    }));
    // SAFETY: `handle` is the live connection that `conn` owns, and
    // nothing else uses it during this call, since we hold a reference to
    // `conn`. SQLite passes `ctx` back to `on_event` unchanged, and
    // `on_event` frees it when the connection closes, which is the last
    // event SQLite delivers for it.
    let rc = unsafe {
        ffi::sqlite3_trace_v2(
            conn.handle(),
            ffi::SQLITE_TRACE_STMT | ffi::SQLITE_TRACE_PROFILE | ffi::SQLITE_TRACE_CLOSE,
            Some(on_event),
            ctx.cast(),
        )
    };
    if rc != ffi::SQLITE_OK {
        // SAFETY: SQLite did not take `ctx`, so this is its only owner.
        drop(unsafe { Box::from_raw(ctx) });
        return Err(rusqlite::Error::SqliteFailure(ffi::Error::new(rc), None));
    }
    Ok(())
}

/// The `sqlite3_trace_v2` callback. `ctx` is the `Box<Context>` that
/// [`install`] leaked.
unsafe extern "C" fn on_event(
    event: c_uint,
    ctx: *mut c_void,
    p: *mut c_void,
    x: *mut c_void,
) -> c_int {
    match event {
        ffi::SQLITE_TRACE_STMT => {
            // SAFETY: `ctx` stays valid until the close event below. For a
            // statement event, `p` is the statement and `x` its SQL.
            let ctx = unsafe { &*ctx.cast::<Context>() };
            // SQLite also reports the start of each trigger a statement
            // fires, with SQL that starts with `--`; those must not restart
            // the statement's clock.
            let trigger = !x.is_null()
                && unsafe { CStr::from_ptr(x.cast()) }
                    .to_bytes()
                    .starts_with(b"--");
            if let Ok(mut started) = ctx.started.lock() {
                let entry = started.entry(p as usize);
                if trigger {
                    entry.or_insert_with(Instant::now);
                } else {
                    entry.insert_entry(Instant::now());
                }
            }
        }
        ffi::SQLITE_TRACE_PROFILE => {
            // SAFETY: `ctx` stays valid until the close event below. For a
            // profile event, SQLite passes the statement as `p` and a
            // pointer to its running time in nanoseconds as `x`.
            let ctx = unsafe { &*ctx.cast::<Context>() };
            let stmt = p.cast::<ffi::sqlite3_stmt>();
            let sqlite_nanos = unsafe { *x.cast::<i64>() };
            // Reading the counter with a reset leaves it at zero for the
            // statement's next run, so each event sees only its own rows.
            let fullscan_steps =
                unsafe { ffi::sqlite3_stmt_status(stmt, ffi::SQLITE_STMTSTATUS_FULLSCAN_STEP, 1) };
            let sql = unsafe { ffi::sqlite3_sql(stmt) };

            let started = ctx
                .started
                .lock()
                .ok()
                .and_then(|mut started| started.remove(&(p as usize)));
            if sql.is_null() {
                return 0;
            }
            let sql = unsafe { CStr::from_ptr(sql) }.to_string_lossy();
            let duration = match started {
                Some(at) => at.elapsed(),
                // Started before the hook was installed.
                None => Duration::from_nanos(u64::try_from(sqlite_nanos).unwrap_or_default()),
            };
            let fullscan_steps = u64::try_from(fullscan_steps).unwrap_or_default();
            // Unwinding out of an `extern "C"` function aborts the process;
            // a bug in recording a sample must not take the server down.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                record(ctx, &sql, duration, fullscan_steps)
            }));
        }
        ffi::SQLITE_TRACE_CLOSE => {
            // Unhook first: if the close fails because the connection is
            // still busy, SQLite keeps it open, and it must not call back
            // with the context we are about to free.
            // SAFETY: for a close event, `p` is the connection itself.
            unsafe {
                ffi::sqlite3_trace_v2(p.cast(), 0, None, std::ptr::null_mut());
            }
            // SAFETY: this is the context `install` leaked, and no further
            // event can reach it now that the hook is gone.
            drop(unsafe { Box::from_raw(ctx.cast::<Context>()) });
        }
        _ => {}
    }
    // SQLite ignores the return value, and asks for zero.
    0
}

fn record(ctx: &Context, sql: &str, duration: Duration, fullscan_steps: u64) {
    // A panic while the lock was held cannot have left the map half
    // updated, so carry on with it rather than stop recording.
    let mut remembered = ctx.series.lock().unwrap_or_else(|e| e.into_inner());
    if !remembered.contains_key(sql) {
        if remembered.len() >= MAX_REMEMBERED_STATEMENTS {
            remembered.clear();
        }
        let (op, table) = statement_label(sql);
        let series = ctx.metrics.db_statement_series(op, &table);
        remembered.insert(sql.into(), Series { op, table, series });
    }
    let Some(Series { op, table, series }) = remembered.get(sql) else {
        return;
    };
    series.record(duration.as_secs_f64(), fullscan_steps);
    if duration >= SLOW_STATEMENT {
        tracing::info!(
            target: "sqlite::slow",
            op = *op,
            table = %table,
            duration_ms = duration.as_millis() as u64,
            fullscan_steps,
            sql = %sql.split_whitespace().collect::<Vec<_>>().join(" "),
            "slow SQL statement"
        );
    }
}

/// The operation a statement performs and the table it acts on, used to
/// label its metrics.
///
/// The operation is the statement's leading keyword, lowercased (`select`,
/// `insert`, `update`, `delete`, `pragma`, ...), or `other` for anything
/// unrecognised; for a `WITH` statement it is the keyword of the statement
/// that follows the common table expressions. The table is the one named
/// after the outermost `FROM` (for `select` and `delete`), `INTO` (for
/// `insert` and `replace`) or `UPDATE`, or the pragma's name; it is
/// `subquery` when that is a parenthesised query, and empty for operations
/// that name no table. Names that aren't plain identifiers are replaced by
/// `other`, so a label can't carry arbitrary text from the SQL.
///
/// # Examples
///
/// ```
/// use kiki_rss::db::profile::statement_label;
///
/// assert_eq!(
///     statement_label("SELECT id FROM entries e JOIN feeds f ON e.feed_id = f.id"),
///     ("select", "entries".to_string()),
/// );
/// assert_eq!(
///     statement_label("INSERT OR IGNORE INTO entry_tags (entry_id, tag_id) VALUES (?1, ?2)"),
///     ("insert", "entry_tags".to_string()),
/// );
/// assert_eq!(statement_label("COMMIT"), ("commit", String::new()));
/// ```
pub fn statement_label(sql: &str) -> (&'static str, String) {
    let tokens = top_level_tokens(sql);
    let mut keywords = tokens.iter().enumerate().filter_map(|(i, t)| match t {
        Token::Word {
            text,
            quoted: false,
        } => Some((i, text.to_ascii_lowercase())),
        _ => None,
    });

    let Some((mut at, mut keyword)) = keywords.next() else {
        return ("other", String::new());
    };
    if keyword == "with" {
        // Skip past the CTEs, whose bodies are parenthesised and so
        // invisible here, to the statement that uses them.
        match keywords.find(|(_, w)| {
            matches!(
                w.as_str(),
                "select" | "insert" | "replace" | "update" | "delete"
            )
        }) {
            Some(found) => (at, keyword) = found,
            None => return ("other", String::new()),
        }
    }

    let op = match keyword.as_str() {
        "select" => "select",
        "insert" => "insert",
        "replace" => "replace",
        "update" => "update",
        "delete" => "delete",
        "pragma" => "pragma",
        "begin" => "begin",
        "commit" | "end" => "commit",
        "rollback" => "rollback",
        "savepoint" => "savepoint",
        "release" => "release",
        "create" => "create",
        "drop" => "drop",
        "alter" => "alter",
        "vacuum" => "vacuum",
        "analyze" => "analyze",
        _ => "other",
    };

    let rest = tokens.get(at + 1..).unwrap_or_default();
    let target = match op {
        "select" | "delete" => after_keyword(rest, "from"),
        "insert" | "replace" => after_keyword(rest, "into"),
        // `UPDATE [OR <conflict resolution>] <table>`
        "update" => match rest {
            [or, _, next, ..] if or.is_keyword("or") => Some(next),
            [next, ..] => Some(next),
            [] => None,
        },
        "pragma" => rest.first(),
        _ => return (op, String::new()),
    };
    let table = match target {
        Some(Token::Word { text, .. }) => identifier(text),
        Some(Token::Paren) => "subquery".to_string(),
        None => String::new(),
    };
    (op, table)
}

/// The token following the first `keyword` in `tokens`.
fn after_keyword<'t>(tokens: &'t [Token], keyword: &str) -> Option<&'t Token> {
    let at = tokens.iter().position(|t| t.is_keyword(keyword))?;
    tokens.get(at + 1)
}

/// `name` as a label value: lowercased, without a `schema.` prefix, or
/// `other` if it isn't a plain identifier.
fn identifier(name: &str) -> String {
    let name = name.rsplit('.').next().unwrap_or(name);
    let plain = !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if plain {
        name.to_ascii_lowercase()
    } else {
        "other".to_string()
    }
}

#[derive(Debug, PartialEq)]
enum Token {
    /// A keyword, name or literal outside any parentheses. The parts of a
    /// dotted name (`main.entries`) make up one word.
    Word {
        /// The word, with any quotes removed.
        text: String,
        /// Whether any part of it was quoted, which rules out its being a
        /// keyword. Quoted words include string literals, since SQLite
        /// accepts those as names too, as in the SQL that FTS5 runs on its
        /// tables (`INSERT INTO 'main'.'entries_fts_data' ...`).
        quoted: bool,
    },
    /// A parenthesised group, whose contents are skipped.
    Paren,
}

impl Token {
    fn is_keyword(&self, keyword: &str) -> bool {
        matches!(self, Token::Word { text, quoted: false } if text.eq_ignore_ascii_case(keyword))
    }
}

type Chars<'s> = std::iter::Peekable<std::str::Chars<'s>>;

/// The words and parenthesised groups of `sql` at the outermost level,
/// skipping comments and punctuation.
fn top_level_tokens(sql: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut depth = 0usize;
    let mut chars = sql.chars().peekable();
    while let Some(&c) = chars.peek() {
        if starts_word(c) {
            let word = word(&mut chars);
            if depth == 0 {
                tokens.push(word);
            }
            continue;
        }
        chars.next();
        match c {
            '(' => {
                if depth == 0 {
                    tokens.push(Token::Paren);
                }
                depth += 1;
            }
            ')' => depth = depth.saturating_sub(1),
            '-' if chars.next_if_eq(&'-').is_some() => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.next_if_eq(&'*').is_some() => {
                while let Some(c) = chars.next() {
                    if c == '*' && chars.next_if_eq(&'/').is_some() {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    tokens
}

fn starts_word(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '\'' | '"' | '`' | '[')
}

/// Read the word `chars` starts with, following dots to the parts of a
/// dotted name.
fn word(chars: &mut Chars<'_>) -> Token {
    let mut text = String::new();
    let mut quoted = false;
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' | '`' | '[' => {
                quoted = true;
                let close = if c == '[' { ']' } else { c };
                while let Some(c) = chars.next() {
                    if c == close {
                        // Inside quotes, a doubled quote stands for itself.
                        if close != ']' && chars.next_if_eq(&close).is_some() {
                            text.push(c);
                            continue;
                        }
                        break;
                    }
                    text.push(c);
                }
            }
            c => {
                text.push(c);
                while let Some(c) = chars.next_if(|&c| c.is_alphanumeric() || c == '_') {
                    text.push(c);
                }
            }
        }
        // Carry on through `.` to the next part of the name, if there is
        // one (`t.*` ends at the dot).
        let mut ahead = chars.clone();
        if ahead.next() == Some('.') && ahead.peek().is_some_and(|&c| starts_word(c)) {
            chars.next();
            text.push('.');
        } else {
            break;
        }
    }
    Token::Word { text, quoted }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn label(sql: &str) -> (&'static str, String) {
        statement_label(sql)
    }

    #[test]
    fn selects_are_labeled_by_their_outermost_from() {
        assert_eq!(
            label("SELECT (SELECT COUNT(*) FROM tags) AS n, e.id FROM entries AS e WHERE e.id = ?"),
            ("select", "entries".into())
        );
        assert_eq!(
            label("select count(*) from (select 1 from feeds)"),
            ("select", "subquery".into())
        );
        assert_eq!(label("SELECT 1"), ("select", String::new()));
    }

    #[test]
    fn writes_are_labeled_by_their_target() {
        assert_eq!(
            label("INSERT INTO entries (title) VALUES (?1) ON CONFLICT DO UPDATE SET title = ?1"),
            ("insert", "entries".into())
        );
        assert_eq!(
            label("REPLACE INTO plugin_state VALUES (?, ?)"),
            ("replace", "plugin_state".into())
        );
        assert_eq!(
            label("UPDATE OR IGNORE feeds SET title = ?"),
            ("update", "feeds".into())
        );
        assert_eq!(
            label("UPDATE feeds SET last_checked = ?"),
            ("update", "feeds".into())
        );
        assert_eq!(
            label("DELETE FROM entries WHERE id IN (SELECT id FROM entries LIMIT 10)"),
            ("delete", "entries".into())
        );
    }

    #[test]
    fn ctes_are_labeled_by_the_statement_that_uses_them() {
        assert_eq!(
            label(
                "WITH old AS (SELECT id FROM entries) DELETE FROM entry_tags WHERE entry_id IN old"
            ),
            ("delete", "entry_tags".into())
        );
        assert_eq!(
            label(
                "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n) SELECT x FROM n"
            ),
            ("select", "n".into())
        );
    }

    #[test]
    fn comments_literals_and_quotes_are_skipped() {
        assert_eq!(
            label("-- FROM nowhere\n/* FROM elsewhere */ SELECT 'FROM x' FROM \"Feeds\""),
            ("select", "feeds".into())
        );
        assert_eq!(
            label("SELECT * FROM main.entries"),
            ("select", "entries".into())
        );
        assert_eq!(
            label("SELECT * FROM [odd name]"),
            ("select", "other".into())
        );
        assert_eq!(
            label("SELECT 'from', \"it''s\" FROM t"),
            ("select", "t".into())
        );
    }

    #[test]
    fn fts5_shadow_tables_are_labeled_by_name() {
        assert_eq!(
            label("REPLACE INTO 'main'.'entries_fts_data'(id, block) VALUES(?,?)"),
            ("replace", "entries_fts_data".into())
        );
        assert_eq!(
            label("INSERT INTO 'main'.'entries_fts_docsize' VALUES(?,?)"),
            ("insert", "entries_fts_docsize".into())
        );
        assert_eq!(
            label("DELETE FROM 'main'.'entries_fts_idx' WHERE segid=?"),
            ("delete", "entries_fts_idx".into())
        );
    }

    #[test]
    fn other_statements_have_no_table() {
        assert_eq!(
            label("PRAGMA busy_timeout=5000"),
            ("pragma", "busy_timeout".into())
        );
        assert_eq!(label("BEGIN IMMEDIATE"), ("begin", String::new()));
        assert_eq!(label("END TRANSACTION"), ("commit", String::new()));
        assert_eq!(label("frobnicate everything"), ("other", String::new()));
        assert_eq!(label("   "), ("other", String::new()));
    }

    #[cfg(feature = "metrics")]
    #[test]
    fn statements_are_recorded_until_the_connection_closes() -> anyhow::Result<()> {
        let metrics = Arc::new(Metrics::new()?);
        let conn = Connection::open_in_memory()?;
        install(&conn, metrics.clone())?;
        conn.execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1), (2), (3);")?;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM t WHERE x > 1", [], |r| r.get(0))?;
        assert_eq!(n, 2);

        let rendered = metrics.render();
        let count = |op: &str, table: &str| {
            let series = format!(
                "kiki_db_statement_duration_seconds_count{{op=\"{op}\",table=\"{table}\"}} "
            );
            rendered
                .lines()
                .find_map(|l| l.strip_prefix(&series))
                .map(|v| v.parse::<f64>().unwrap())
        };
        assert_eq!(count("create", ""), Some(1.0));
        assert_eq!(count("insert", "t"), Some(1.0));
        assert_eq!(count("select", "t"), Some(1.0));
        // Timed by the clock, not SQLite's, which rounds to milliseconds.
        assert!(!rendered
            .contains("kiki_db_statement_duration_seconds_sum{op=\"select\",table=\"t\"} 0\n"));
        // The WHERE clause has no index to use, so the query scanned the
        // whole table, stepping from its first row to each of the other two.
        assert!(rendered
            .contains("kiki_db_statement_fullscan_steps_total{op=\"select\",table=\"t\"} 2"));

        // The connection's reference to the metrics goes away with it.
        assert_eq!(Arc::strong_count(&metrics), 2);
        drop(conn);
        assert_eq!(Arc::strong_count(&metrics), 1);
        Ok(())
    }

    /// The value of the `kiki_db_statement_duration_seconds` count for
    /// `labels` in `metrics`' scrape output.
    #[cfg(feature = "metrics")]
    fn statement_count(metrics: &Metrics, labels: &str) -> Option<u64> {
        let prefix = format!("kiki_db_statement_duration_seconds_count{{{labels}}} ");
        metrics
            .render()
            .lines()
            .find_map(|line| line.strip_prefix(&prefix).map(|n| n.parse().unwrap()))
    }

    /// Every run of a statement is recorded under its label, the same
    /// however many times it runs, alongside other statements'.
    #[cfg(feature = "metrics")]
    #[test]
    fn repeated_statements_are_each_recorded() {
        let metrics = Arc::new(Metrics::new().unwrap());
        let conn = Connection::open_in_memory().unwrap();
        install(&conn, metrics.clone()).unwrap();
        conn.execute_batch("CREATE TABLE t (x); CREATE TABLE u (y);")
            .unwrap();
        for _ in 0..3 {
            conn.query_row("SELECT COUNT(*) FROM t", [], |row| row.get::<_, i64>(0))
                .unwrap();
        }
        conn.execute("INSERT INTO u VALUES (1)", []).unwrap();
        assert_eq!(
            statement_count(&metrics, r#"op="select",table="t""#),
            Some(3)
        );
        assert_eq!(
            statement_count(&metrics, r#"op="insert",table="u""#),
            Some(1)
        );
    }

    /// A connection remembers at most [`MAX_REMEMBERED_STATEMENTS`]
    /// statements, and still records every run once it has started over.
    #[cfg(feature = "metrics")]
    #[test]
    fn remembered_statements_are_bounded() {
        let metrics = Arc::new(Metrics::new().unwrap());
        let ctx = Context {
            metrics: metrics.clone(),
            started: Mutex::new(HashMap::new()),
            series: Mutex::new(HashMap::new()),
        };
        let runs = MAX_REMEMBERED_STATEMENTS + 10;
        for i in 0..runs {
            record(&ctx, &format!("SELECT {i} FROM t"), Duration::ZERO, 0);
            assert!(ctx.series.lock().unwrap().len() <= MAX_REMEMBERED_STATEMENTS);
        }
        assert_eq!(
            statement_count(&metrics, r#"op="select",table="t""#),
            Some(runs as u64)
        );
    }
}
