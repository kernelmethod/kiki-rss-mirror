//! Adaptive fetching: backing off from feeds that keep not changing.
//!
//! Some servers send a freshness hint far shorter than the feed's fetch
//! interval (`Cache-Control: max-age=0` is common), which on its own has
//! the feed polled at the minimum cadence even if it changes once a week.
//! Adaptive fetching keeps one small counter per feed, its *level*:
//!
//! - a fetch that finds the feed unchanged (a `304`, or a `200` whose body
//!   hashes the same as the last one) raises the level by one;
//! - a fetch that finds it changed lowers the level by one;
//! - a fetch that cannot tell (no stored body hash yet) leaves it alone.
//!
//! The short hint is then stretched to `max(hint, min_cadence) * 2^level`,
//! never past the feed's own fetch interval. Raising on no change and
//! lowering on change settles the wait near the feed's real update period:
//! about where half the fetches find something new.
//!
//! A feed without a short hint already waits its full interval, so its
//! level is kept at zero.

/// What a successful fetch revealed about the feed's content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ContentChange {
    /// The content differs from the last fetch.
    Changed,
    /// The content is the same as at the last fetch.
    Unchanged,
    /// There is nothing to compare against.
    Unknown,
}

impl ContentChange {
    /// Compare a fresh body hash with the one stored from the last `200`.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// assert_eq!(ContentChange::from_hashes(Some("a"), "a"), ContentChange::Unchanged);
    /// assert_eq!(ContentChange::from_hashes(Some("a"), "b"), ContentChange::Changed);
    /// assert_eq!(ContentChange::from_hashes(None, "b"), ContentChange::Unknown);
    /// ```
    pub(super) fn from_hashes(stored: Option<&str>, fresh: &str) -> Self {
        match stored {
            Some(stored) if stored == fresh => ContentChange::Unchanged,
            Some(_) => ContentChange::Changed,
            None => ContentChange::Unknown,
        }
    }
}

/// The wait a short freshness hint starts from, or `None` when adaptive
/// fetching does not apply because there is no hint shorter than the
/// feed's `interval`.
fn base_wait(hint_secs: Option<u64>, min_cadence: u64, interval: u64) -> Option<u64> {
    hint_secs
        .filter(|&hint| hint < interval)
        .map(|hint| hint.max(min_cadence).max(1))
}

/// The lowest level at which `base` doubled that many times reaches
/// `interval`. Levels above it change nothing, so the level is held there
/// to let a feed that starts changing come back down quickly.
fn max_level(base: u64, interval: u64) -> u32 {
    let mut level = 0;
    let mut wait = base.max(1);
    while wait < interval {
        wait = wait.saturating_mul(2);
        level += 1;
    }
    level
}

/// The feed's new adaptive level after a successful fetch.
///
/// `previous` is the stored level, `hint_secs` the freshness hint the
/// fetch is scheduled with, `min_cadence` the global polling floor, and
/// `interval` the feed's own fetch interval, which is the most the wait is
/// ever stretched to.
///
/// # Examples
///
/// ```ignore
/// // A 0s hint on a feed with a 1h interval and a 1m floor.
/// assert_eq!(next_level(0, ContentChange::Unchanged, Some(0), 60, 3600), 1);
/// assert_eq!(next_level(3, ContentChange::Changed, Some(0), 60, 3600), 2);
/// // Held where the wait reaches the interval: 1m * 2^6 >= 1h.
/// assert_eq!(next_level(6, ContentChange::Unchanged, Some(0), 60, 3600), 6);
/// // No short hint: nothing to adapt.
/// assert_eq!(next_level(4, ContentChange::Unchanged, None, 60, 3600), 0);
/// ```
pub(super) fn next_level(
    previous: u32,
    change: ContentChange,
    hint_secs: Option<u64>,
    min_cadence: u64,
    interval: u64,
) -> u32 {
    let Some(base) = base_wait(hint_secs, min_cadence, interval) else {
        return 0;
    };
    let previous = previous.min(max_level(base, interval));
    match change {
        ContentChange::Unchanged => previous.saturating_add(1).min(max_level(base, interval)),
        ContentChange::Changed => previous.saturating_sub(1),
        ContentChange::Unknown => previous,
    }
}

/// The wait for a feed at adaptive `level` with freshness hint
/// `hint_secs`, or `None` when adaptive fetching does not stretch it: the
/// level is zero, or there is no hint shorter than `interval`.
///
/// The result lies in `[min_cadence, interval]`.
pub(super) fn stretched_wait(
    hint_secs: Option<u64>,
    level: u32,
    min_cadence: u64,
    interval: u64,
) -> Option<u64> {
    let base = base_wait(hint_secs, min_cadence, interval)?;
    if level == 0 {
        return None;
    }
    let multiplier = 1u64.checked_shl(level.min(63)).unwrap_or(u64::MAX);
    Some(base.saturating_mul(multiplier).min(interval).max(base))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN_CADENCE: u64 = 60;
    const DAY: u64 = 86_400;

    #[test]
    fn content_change_from_hashes() {
        assert_eq!(
            ContentChange::from_hashes(Some("a"), "a"),
            ContentChange::Unchanged
        );
        assert_eq!(
            ContentChange::from_hashes(Some("a"), "b"),
            ContentChange::Changed
        );
        assert_eq!(
            ContentChange::from_hashes(None, "b"),
            ContentChange::Unknown
        );
    }

    #[test]
    fn unchanged_fetches_double_the_wait_up_to_the_interval() {
        let mut level = 0;
        let mut waits = Vec::new();
        for _ in 0..15 {
            level = next_level(level, ContentChange::Unchanged, Some(0), MIN_CADENCE, DAY);
            waits.push(stretched_wait(Some(0), level, MIN_CADENCE, DAY));
        }
        let expected: Vec<Option<u64>> = [
            120, 240, 480, 960, 1920, 3840, 7680, 15_360, 30_720, 61_440, DAY, DAY, DAY, DAY, DAY,
        ]
        .into_iter()
        .map(Some)
        .collect();
        assert_eq!(waits, expected);
        // Held at the first level that reaches the interval.
        assert_eq!(level, 11);
    }

    #[test]
    fn changed_fetches_halve_the_wait_down_to_the_hint() {
        let mut level = 11;
        for expected in [61_440, 30_720, 15_360] {
            level = next_level(level, ContentChange::Changed, Some(0), MIN_CADENCE, DAY);
            assert_eq!(
                stretched_wait(Some(0), level, MIN_CADENCE, DAY),
                Some(expected)
            );
        }
        for _ in 0..20 {
            level = next_level(level, ContentChange::Changed, Some(0), MIN_CADENCE, DAY);
        }
        assert_eq!(level, 0);
        assert_eq!(stretched_wait(Some(0), level, MIN_CADENCE, DAY), None);
    }

    #[test]
    fn unknown_change_keeps_the_level() {
        assert_eq!(
            next_level(3, ContentChange::Unknown, Some(0), MIN_CADENCE, DAY),
            3
        );
    }

    #[test]
    fn a_stored_level_beyond_the_interval_is_brought_back() {
        // The feed's interval was lowered to 1h since the level was stored.
        assert_eq!(
            next_level(11, ContentChange::Changed, Some(0), MIN_CADENCE, 3600),
            5
        );
        assert_eq!(stretched_wait(Some(0), 11, MIN_CADENCE, 3600), Some(3600));
    }

    #[test]
    fn the_hint_is_the_starting_point_when_above_the_floor() {
        // A 10m hint on a 1h feed: 20m, 40m, then the interval.
        let waits: Vec<_> = (1..=4)
            .map(|level| stretched_wait(Some(600), level, MIN_CADENCE, 3600))
            .collect();
        assert_eq!(waits, [Some(1200), Some(2400), Some(3600), Some(3600)]);
        assert_eq!(max_level(600, 3600), 3);
    }

    #[test]
    fn no_short_hint_means_nothing_to_adapt() {
        for hint in [None, Some(DAY), Some(2 * DAY)] {
            assert_eq!(
                next_level(5, ContentChange::Unchanged, hint, MIN_CADENCE, DAY),
                0
            );
            assert_eq!(stretched_wait(hint, 5, MIN_CADENCE, DAY), None);
        }
    }

    #[test]
    fn huge_levels_saturate() {
        assert_eq!(
            stretched_wait(Some(0), u32::MAX, MIN_CADENCE, u64::MAX),
            Some(MIN_CADENCE.saturating_mul(1 << 63))
        );
        assert_eq!(max_level(1, u64::MAX), 64);
    }
}
