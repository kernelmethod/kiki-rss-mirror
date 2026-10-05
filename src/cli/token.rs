//! The `kiki token` subcommands, for creating, listing and revoking API
//! tokens; see [`crate::auth`].
//!
//! Like `kiki opml` and `kiki plugin`, these work directly on the database,
//! so they run whether or not the server is up, and a running server
//! honours or refuses a token from the moment it is created or revoked.
use crate::auth::Scopes;
use crate::cli::paths::{self, Env};
use crate::db::tokens::{self, Token};
use crate::db::ConnectionBuilder;
use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Subcommand};
use rusqlite::Connection;
use std::path::Path;

/// Arguments for the `token` subcommand.
#[derive(Args)]
pub struct TokenArgs {
    #[command(subcommand)]
    command: TokenCommand,
}

#[derive(Subcommand)]
enum TokenCommand {
    /// Create an API token, and print it
    ///
    /// The token is printed on standard output, alone, and cannot be shown
    /// again: only a hash of it is kept.
    Create(CreateArgs),

    /// List API tokens
    #[command(visible_alias = "list")]
    Ls,

    /// Revoke an API token, by id or name
    #[command(visible_alias = "rm")]
    Revoke(RevokeArgs),
}

#[derive(Args)]
struct CreateArgs {
    /// A name to tell the token apart by, such as the device or program
    /// that will use it
    name: String,

    /// What the token may do: a comma-separated list of scopes (read,
    /// state, tags, feeds, metrics, admin) and presets (reader = read +
    /// state, curator = read + tags, manager = read + tags + feeds)
    #[arg(short, long, value_name = "SCOPES")]
    scopes: Scopes,

    /// How long until the token expires, as a number of hours, days, weeks
    /// or years: `12h`, `90d`, `6w` or `1y`. The token never expires if
    /// this is left out.
    #[arg(short, long, value_name = "DURATION", value_parser = parse_duration)]
    expires: Option<i64>,
}

#[derive(Args)]
struct RevokeArgs {
    /// The token's id, as `kiki token ls` shows it, or its name
    token: String,
}

/// Parse a duration such as `90d` into seconds.
fn parse_duration(s: &str) -> Result<i64> {
    let s = s.trim();
    let split = s
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| anyhow!("{s:?} has no unit; use h, d, w or y, as in 90d"))?;
    let (n, unit) = s.split_at(split);
    let n: i64 = n
        .parse()
        .map_err(|_| anyhow!("{s:?} does not start with a number"))?;
    let unit = match unit {
        "h" => 3600,
        "d" => 86400,
        "w" => 7 * 86400,
        "y" => 365 * 86400,
        _ => bail!("unknown unit {unit:?} in {s:?}; use h, d, w or y"),
    };
    match n.checked_mul(unit) {
        Some(secs) if secs > 0 => Ok(secs),
        _ => bail!("{s:?} is not a usable duration"),
    }
}

/// A Unix timestamp as a UTC date and time, to the minute.
fn format_time(t: Option<i64>, none: &str) -> String {
    t.and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| none.to_owned())
}

impl TokenArgs {
    /// Run the `token` subcommand.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be opened, or if the token
    /// cannot be created or found.
    pub fn run(&self) -> Result<()> {
        let data_dir = paths::resolve_data_dir(&Env::from_process())?.path;
        let database = data_dir.join(paths::DB_FILE_NAME);
        let conn = open(&database)?;
        let now = chrono::Utc::now().timestamp();
        match &self.command {
            TokenCommand::Create(args) => {
                let expires_at = args.expires.map(|secs| now.saturating_add(secs));
                let (token, secret) = tokens::create(&conn, &args.name, args.scopes, expires_at)?;
                eprintln!(
                    "Created token {:?} (id {}) with scopes {}, expiring {}. It will not be \
                     shown again.",
                    token.name,
                    token.id,
                    token.scopes,
                    format_time(token.expires_at, "never"),
                );
                println!("{secret}");
                Ok(())
            }
            TokenCommand::Ls => {
                print!("{}", render_list(&tokens::list(&conn)?, now));
                Ok(())
            }
            TokenCommand::Revoke(args) => {
                let token = find(&conn, &args.token)?;
                tokens::revoke(&conn, token.id)?;
                println!("Revoked token {:?} (id {}).", token.name, token.id);
                Ok(())
            }
        }
    }
}

/// Open the database at `database` for writing, waiting a while for a
/// running server to finish a write rather than failing at once.
fn open(database: &Path) -> Result<Connection> {
    let conn = ConnectionBuilder::default()
        .at_path(database)
        .read_write()
        .build()
        .with_context(|| format!("failed to open database at {database:?}"))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(conn)
}

/// The token whose id or name is `token`.
fn find(conn: &Connection, token: &str) -> Result<Token> {
    let by_id = match token.parse::<i64>() {
        Ok(id) => tokens::get(conn, id)?,
        Err(_) => None,
    };
    match by_id {
        Some(t) => Ok(t),
        None => tokens::get_by_name(conn, token)?
            .ok_or_else(|| anyhow!("no token has the id or name {token:?}")),
    }
}

/// `tokens` as a table, one per line, as of the Unix timestamp `now`.
fn render_list(tokens: &[Token], now: i64) -> String {
    if tokens.is_empty() {
        return "No tokens. Create one with `kiki token create`.\n".to_owned();
    }
    let header = ["ID", "NAME", "SCOPES", "CREATED", "EXPIRES", "LAST USED"].map(String::from);
    let rows: Vec<[String; 6]> = tokens
        .iter()
        .map(|t| {
            let expires = format_time(t.expires_at, "never");
            [
                t.id.to_string(),
                t.name.clone(),
                t.scopes.to_string(),
                format_time(Some(t.created_at), ""),
                if t.expired_at(now) {
                    format!("{expires} (expired)")
                } else {
                    expires
                },
                format_time(t.last_used_at, "never"),
            ]
        })
        .collect();
    let mut widths = header.clone().map(|h| h.chars().count());
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    std::iter::once(&header)
        .chain(&rows)
        .map(|row| {
            let line: Vec<String> = row
                .iter()
                .zip(widths)
                .map(|(cell, w)| format!("{cell:<w$}"))
                .collect();
            format!("{}\n", line.join("  ").trim_end())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("12h").ok(), Some(12 * 3600));
        assert_eq!(parse_duration("90d").ok(), Some(90 * 86400));
        assert_eq!(parse_duration("1y").ok(), Some(365 * 86400));
        for bad in ["", "90", "d", "0d", "5m", "-1d", "99999999999999999y"] {
            assert!(parse_duration(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn finds_by_id_or_name() -> Result<()> {
        let conn = ConnectionBuilder::default().in_memory().create().build()?;
        let (a, _) = tokens::create(&conn, "phone", Scopes::all(), None)?;
        // A name that looks like another token's id.
        let (b, _) = tokens::create(&conn, "99", Scopes::all(), None)?;
        assert_eq!(find(&conn, &a.id.to_string())?, a);
        assert_eq!(find(&conn, "phone")?, a);
        assert_eq!(find(&conn, "99")?, b);
        assert!(find(&conn, "nope").is_err());
        Ok(())
    }

    #[test]
    fn renders_a_table() {
        let token = |id, name: &str, expires_at, last_used_at| Token {
            id,
            name: name.to_owned(),
            scopes: "reader".parse().unwrap_or_default(),
            created_at: 0,
            expires_at,
            last_used_at,
        };
        let table = render_list(
            &[
                token(1, "phone", None, Some(60)),
                token(2, "old laptop", Some(86400), None),
            ],
            90000,
        );
        assert_eq!(
            table,
            "ID  NAME        SCOPES      CREATED           EXPIRES                     LAST USED\n\
             1   phone       read,state  1970-01-01 00:00  never                       1970-01-01 00:01\n\
             2   old laptop  read,state  1970-01-01 00:00  1970-01-02 00:00 (expired)  never\n"
        );
        assert!(render_list(&[], 0).starts_with("No tokens"));
    }
}
