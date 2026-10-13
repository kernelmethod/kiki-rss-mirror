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

mod cache_rules {
    use super::super::super::cache::{entry_after_response, request_validators, Response};
    use crate::db::http_cache::CacheEntry;
    use crate::fetcher::Validators;
    use reqwest::header::HeaderMap;

    const NOW: i64 = 1_000_000;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(*name, value.parse().unwrap());
        }
        map
    }

    fn stored() -> CacheEntry {
        CacheEntry {
            etag: Some("\"old\"".into()),
            last_modified: Some("Mon, 01 Jan 2024 00:00:00 GMT".into()),
            expires_at: Some(NOW + 60),
            immutable_until: None,
            body_hash: Some("old-hash".into()),
            last_full_refresh_at: Some(NOW - 100),
        }
    }

    #[test]
    fn validators_come_from_the_stored_entry() {
        let v = request_validators(&stored(), NOW, false);
        assert_eq!(v.etag.as_deref(), Some("\"old\""));
        assert!(v.last_modified.is_some());
    }

    #[test]
    fn no_validators_on_a_full_request_or_while_immutable() {
        assert_eq!(
            request_validators(&stored(), NOW, true),
            Validators::default()
        );
        let immutable = CacheEntry {
            immutable_until: Some(NOW + 1),
            ..stored()
        };
        assert_eq!(
            request_validators(&immutable, NOW, false),
            Validators::default()
        );
        // Once the window closes, revalidation resumes.
        assert!(request_validators(&immutable, NOW + 1, false)
            .etag
            .is_some());
    }

    #[test]
    fn a_full_response_replaces_the_entry() {
        let h = headers(&[("etag", "\"new\""), ("cache-control", "max-age=600")]);
        let entry = entry_after_response(&stored(), Response::Full { body_hash: "h" }, &h, NOW);
        assert_eq!(
            entry,
            CacheEntry {
                etag: Some("\"new\"".into()),
                last_modified: None,
                expires_at: Some(NOW + 600),
                immutable_until: None,
                body_hash: Some("h".into()),
                last_full_refresh_at: Some(NOW),
            }
        );
    }

    #[test]
    fn no_store_keeps_nothing_but_the_refresh_time() {
        let h = headers(&[("etag", "\"new\""), ("cache-control", "no-store")]);
        let full = entry_after_response(&stored(), Response::Full { body_hash: "h" }, &h, NOW);
        assert_eq!(
            full,
            CacheEntry {
                last_full_refresh_at: Some(NOW),
                ..CacheEntry::default()
            }
        );
        // A 304 has no body, so the last one's fingerprint stands.
        let revalidated = entry_after_response(&stored(), Response::NotModified, &h, NOW);
        assert_eq!(
            revalidated,
            CacheEntry {
                body_hash: Some("old-hash".into()),
                last_full_refresh_at: Some(NOW - 100),
                ..CacheEntry::default()
            }
        );
    }

    #[test]
    fn a_304_freshens_the_stored_entry() {
        // Nothing carried: everything stays as stored.
        let entry = entry_after_response(&stored(), Response::NotModified, &headers(&[]), NOW);
        assert_eq!(entry, stored());

        // What is carried replaces what was stored.
        let h = headers(&[
            ("etag", "\"rotated\""),
            ("cache-control", "max-age=10, immutable"),
        ]);
        let entry = entry_after_response(&stored(), Response::NotModified, &h, NOW);
        assert_eq!(entry.etag.as_deref(), Some("\"rotated\""));
        assert_eq!(entry.last_modified, stored().last_modified);
        assert_eq!(entry.expires_at, Some(NOW + 10));
        assert_eq!(entry.immutable_until, Some(NOW + 10));
        assert_eq!(entry.body_hash, stored().body_hash);
    }
}
