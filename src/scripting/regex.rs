//! Regular expressions as plugins write them, through the `regex` interface WebAssembly
//! plugins import.
//!
//! Patterns are compiled with the `regex` crate, which matches in time linear in the
//! input, so a plugin matching untrusted feed content cannot be made to backtrack for
//! ever. Flags are letters: `i` (case-insensitive), `m` (multi-line), `s` (`.` matches a
//! newline), `x` (ignore whitespace) and `U` (swap greed).
//!
//! # Resource limits
//!
//! Compiled regexes live outside a plugin's own memory, so its memory cap does not see
//! them. They are bounded here instead: each compiled program is limited to
//! [`REGEX_SIZE_LIMIT_BYTES`] and its lazy DFA cache to [`REGEX_DFA_SIZE_LIMIT_BYTES`]. The
//! engine also keeps at most [`MAX_LIVE_REGEXES`] distinct regexes alive for a plugin at a
//! time, compiling a pattern and flags already alive only once.
//!
//! # Sets
//!
//! The WebAssembly interface's regex sets are lists of regexes each matched alone, not a
//! [`regex::bytes::RegexSet`]. A set searches for every pattern in one pass, but one
//! pattern without a literal to look for, such as `^Author \d+$`, takes away the fast
//! literal search from all of them: a set of word patterns such as `(?i)\bcasino\b` then
//! matched a few kilobytes of text with non-ASCII letters in about 2 ms rather than 15 µs,
//! since the lazy DFA gives up on Unicode word boundaries outside ASCII.

use regex::bytes::{Regex, RegexBuilder};

/// Largest compiled program a single regex may have.
pub const REGEX_SIZE_LIMIT_BYTES: usize = 256 * 1024;

/// Largest lazy DFA cache a single regex may use while matching.
pub const REGEX_DFA_SIZE_LIMIT_BYTES: usize = 256 * 1024;

/// Most distinct regexes that may be alive at once in one plugin.
pub const MAX_LIVE_REGEXES: usize = 128;

/// Compiles `pattern` with `flags`.
///
/// # Errors
///
/// Returns a message, fit for the plugin, if a flag is unknown or the pattern is invalid
/// or too large.
///
/// # Examples
///
/// ```
/// use kiki_rss::scripting::regex::compile;
///
/// let re = compile(r"\bkiki\b", "i").unwrap();
/// assert!(re.is_match(b"Hello, Kiki!"));
/// assert!(compile("x", "z").unwrap_err().contains("unknown flag 'z'"));
/// ```
pub fn compile(pattern: &str, flags: &str) -> Result<Regex, String> {
    let mut builder = RegexBuilder::new(pattern);
    builder
        .size_limit(REGEX_SIZE_LIMIT_BYTES)
        .dfa_size_limit(REGEX_DFA_SIZE_LIMIT_BYTES);
    for flag in flags.chars() {
        match flag {
            'i' => builder.case_insensitive(true),
            'm' => builder.multi_line(true),
            's' => builder.dot_matches_new_line(true),
            'x' => builder.ignore_whitespace(true),
            'U' => builder.swap_greed(true),
            other => {
                return Err(format!(
                    "unknown flag '{other}'; expected any of i, m, s, x, U"
                ))
            }
        };
    }
    builder.build().map_err(|e| format!("invalid pattern: {e}"))
}
