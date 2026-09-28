//! Forwarding SQLite's own error log into Kiki's logs.
//!
//! An error SQLite returns to Kiki carries only its message, which is often
//! too little to act on: "unable to open database file" doesn't say which
//! file. SQLite's error log (<https://sqlite.org/errlog.html>) says more —
//! the path it tried, the OS error, and the statement that failed — so
//! [`install`] routes it through `tracing`.

use rusqlite::ffi;
use std::os::raw::c_int;

/// Route SQLite's error log into `tracing`, under the `sqlite` target.
///
/// Must be called before the process opens its first SQLite connection;
/// SQLite only accepts the setting before it initializes.
///
/// # Errors
///
/// Returns an error if SQLite has already been initialized in this
/// process.
pub fn install() -> rusqlite::Result<()> {
    // SAFETY: `config_log` must not race other SQLite calls. Callers run
    // this before the process has opened any connection or started the
    // threads that would, and `log_message` makes no SQLite calls.
    unsafe { rusqlite::trace::config_log(Some(log_message)) }
}

/// How loudly to log a message SQLite logged with the (extended) result
/// code `code`.
///
/// Errors are warnings, except for those Kiki expects and handles
/// itself: constraint violations (a duplicate tag name, say) and schema
/// changes, which SQLite recovers from by re-preparing the statement.
/// Notices, such as recovering a WAL file after a crash, are informational,
/// and hints that a query had to build an automatic index are debug
/// output.
fn level_for(code: c_int) -> tracing::Level {
    match code {
        ffi::SQLITE_WARNING_AUTOINDEX => tracing::Level::DEBUG,
        _ => match code & 0xff {
            ffi::SQLITE_NOTICE => tracing::Level::INFO,
            ffi::SQLITE_CONSTRAINT | ffi::SQLITE_SCHEMA => tracing::Level::DEBUG,
            _ => tracing::Level::WARN,
        },
    }
}

fn log_message(code: c_int, msg: &str) {
    match level_for(code) {
        tracing::Level::DEBUG => tracing::debug!(target: "sqlite", code, "{msg}"),
        tracing::Level::INFO => tracing::info!(target: "sqlite", code, "{msg}"),
        _ => tracing::warn!(target: "sqlite", code, "{msg}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::Level;

    #[test]
    fn failures_to_open_files_are_warnings() {
        assert_eq!(level_for(ffi::SQLITE_CANTOPEN), Level::WARN);
        assert_eq!(level_for(ffi::SQLITE_IOERR_WRITE), Level::WARN);
        assert_eq!(level_for(ffi::SQLITE_BUSY), Level::WARN);
    }

    #[test]
    fn errors_kiki_handles_itself_are_debug_output() {
        assert_eq!(level_for(ffi::SQLITE_CONSTRAINT_UNIQUE), Level::DEBUG);
        assert_eq!(level_for(ffi::SQLITE_SCHEMA), Level::DEBUG);
        assert_eq!(level_for(ffi::SQLITE_WARNING_AUTOINDEX), Level::DEBUG);
    }

    #[test]
    fn notices_are_informational() {
        assert_eq!(level_for(ffi::SQLITE_NOTICE_RECOVER_WAL), Level::INFO);
    }
}
