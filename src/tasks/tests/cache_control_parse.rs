#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::super::cache::CacheControl;

#[test]
fn test_parse_max_age_only() {
    let cc = CacheControl::parse_many(&["max-age=600"]);
    assert_eq!(cc.max_age, Some(600));
    assert!(!cc.no_cache);
    assert!(!cc.no_store);
    assert!(!cc.immutable);
    assert_eq!(cc.stale_if_error, None);
}

#[test]
fn test_parse_multiple_directives_single_header() {
    let cc = CacheControl::parse_many(&["public, max-age=3600, immutable"]);
    assert_eq!(cc.max_age, Some(3600));
    assert!(cc.immutable);
}

#[test]
fn test_parse_many_combines_multiple_headers() {
    // RFC 9110 §5.3: multiple field values are combined. The parser must
    // notice both `max-age` on the first header and `no-store` on the second.
    let cc = CacheControl::parse_many(&["max-age=600", "no-store"]);
    assert_eq!(cc.max_age, Some(600));
    assert!(cc.no_store);
}

#[test]
fn test_parse_case_insensitive() {
    let cc = CacheControl::parse_many(&["MAX-AGE=42, NO-STORE, Immutable"]);
    assert_eq!(cc.max_age, Some(42));
    assert!(cc.no_store);
    assert!(cc.immutable);
}

#[test]
fn test_parse_whitespace_tolerant() {
    let cc = CacheControl::parse_many(&["  max-age = 60 ,   no-cache "]);
    assert_eq!(cc.max_age, Some(60));
    assert!(cc.no_cache);
}

#[test]
fn test_parse_quoted_value() {
    // RFC 9111 §5.2: directives MAY be quoted-strings.
    let cc = CacheControl::parse_many(&["max-age=\"600\""]);
    assert_eq!(cc.max_age, Some(600));
}

#[test]
fn test_parse_unknown_directive_ignored() {
    let cc = CacheControl::parse_many(&["public, s-maxage=100, max-age=5"]);
    assert_eq!(cc.max_age, Some(5));
    // s-maxage is intentionally not tracked; parser must not crash on it.
}

#[test]
fn test_parse_malformed_max_age_leaves_field_none() {
    let cc = CacheControl::parse_many(&["max-age=not-a-number"]);
    assert_eq!(cc.max_age, None);
}

#[test]
fn test_parse_empty_max_age_value_is_ignored() {
    let cc = CacheControl::parse_many(&["max-age="]);
    assert_eq!(cc.max_age, None);
}

#[test]
fn test_parse_overflow_max_age_is_ignored() {
    let cc = CacheControl::parse_many(&["max-age=99999999999999999999"]);
    assert_eq!(cc.max_age, None);
}

#[test]
fn test_parse_no_cache_and_no_store() {
    let cc = CacheControl::parse_many(&["no-cache, no-store"]);
    assert!(cc.no_cache);
    assert!(cc.no_store);
    assert_eq!(cc.max_age, None);
}

#[test]
fn test_parse_stale_if_error_numeric() {
    let cc = CacheControl::parse_many(&["max-age=60, stale-if-error=3600"]);
    assert_eq!(cc.stale_if_error, Some(3600));
}

#[test]
fn test_parse_stale_if_error_without_value() {
    let cc = CacheControl::parse_many(&["stale-if-error"]);
    // Token-only form has no value; leniency means the field stays None.
    assert_eq!(cc.stale_if_error, None);
}

#[test]
fn test_parse_empty_input() {
    let cc = CacheControl::parse_many(&[]);
    assert_eq!(cc.max_age, None);
    assert!(!cc.no_cache);
    assert!(!cc.no_store);
    assert!(!cc.immutable);
    assert_eq!(cc.stale_if_error, None);
}

#[test]
fn test_parse_empty_directive_tokens_are_skipped() {
    let cc = CacheControl::parse_many(&[",,, max-age=30,,"]);
    assert_eq!(cc.max_age, Some(30));
}
