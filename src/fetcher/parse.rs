//! Turning a feed body into a [`ParsedFeed`].

use super::{
    AtomCategory, AtomEntry, AtomEntryIngestData, AtomFeedIngestData, AtomGenerator, FeedHints,
    ParsedFeed, RssCategory, RssEntry, RssEntryIngestData,
};
use crate::scripting::FeedEntry;
use chrono::Utc;
use rss::extension::syndication::{self, UpdatePeriod};

/// Parse `body` as Atom, falling back to RSS.
///
/// Returns `None` if it is neither. `feed_id` is stamped onto each entry
/// for the benefit of scripts; it has no bearing on where the server
/// stores them.
///
/// # Examples
///
/// ```
/// use kiki_rss::fetcher::{parse_feed, ParsedFeed};
///
/// let rss = br#"<rss version="2.0"><channel><title>t</title><link>http://x/</link>
///     <description>d</description><item><title>hi</title><guid>g1</guid></item>
///     </channel></rss>"#;
/// let parsed = parse_feed(1, rss).expect("valid RSS");
/// assert!(matches!(parsed, ParsedFeed::Rss { ref entries, .. } if entries.len() == 1));
///
/// assert!(parse_feed(1, b"<html>not a feed</html>").is_none());
/// ```
pub fn parse_feed(feed_id: i64, body: &[u8]) -> Option<ParsedFeed> {
    if let Ok(feed) = atom_syndication::Feed::read_from(body) {
        let data = extract_atom_feed_data(&feed);
        let hints = atom_feed_hints(&feed);
        let site_url = atom_site_url(&feed);
        let entries = feed
            .entries
            .into_iter()
            .map(|e| atom_entry_to_parts(feed_id, e))
            .collect();
        return Some(ParsedFeed::Atom {
            feed: Box::new(data),
            entries,
            hints,
            site_url,
        });
    }
    if let Ok(channel) = rss::Channel::read_from(body) {
        let hints = rss_channel_hints(&channel);
        let site_url = Some(channel.link.trim().to_string()).filter(|l| !l.is_empty());
        let entries = channel
            .items
            .into_iter()
            .map(|i| rss_item_to_parts(feed_id, i))
            .collect();
        return Some(ParsedFeed::Rss {
            entries,
            hints,
            site_url,
        });
    }
    None
}

/// The website an Atom feed belongs to: its `rel="alternate"` link (the
/// default relation when `rel` is absent), preferring one declared as HTML.
fn atom_site_url(feed: &atom_syndication::Feed) -> Option<String> {
    let alternates: Vec<_> = feed
        .links
        .iter()
        .filter(|l| l.rel == "alternate" && !l.href.trim().is_empty())
        .collect();
    alternates
        .iter()
        .find(|l| {
            l.mime_type
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case("text/html"))
        })
        .or(alternates.first())
        .map(|l| l.href.trim().to_string())
}

/// Collect the refresh hints an RSS channel declares.
fn rss_channel_hints(channel: &rss::Channel) -> FeedHints {
    FeedHints {
        ttl_secs: channel.ttl().and_then(parse_ttl),
        update_interval_secs: channel
            .syndication_ext()
            .and_then(|sy| update_interval_secs(sy.period(), sy.frequency())),
        skip_hours: skip_hours_mask(channel.skip_hours()),
        skip_days: skip_days_mask(channel.skip_days()),
    }
}

/// Collect the refresh hints an Atom feed declares. Atom has no `<ttl>` or
/// skip elements of its own, so only the Syndication module applies.
fn atom_feed_hints(feed: &atom_syndication::Feed) -> FeedHints {
    FeedHints {
        update_interval_secs: atom_syndication_interval(feed),
        ..FeedHints::default()
    }
}

/// Read `sy:updatePeriod` / `sy:updateFrequency` out of an Atom feed's
/// extensions, which are keyed by whatever prefix the document bound to
/// the Syndication namespace.
///
/// Missing or unparseable values fall back to the module's defaults
/// (daily, once), matching how the `rss` crate reads the same elements.
fn atom_syndication_interval(feed: &atom_syndication::Feed) -> Option<u64> {
    let ext = feed
        .namespaces()
        .iter()
        .filter(|(_, uri)| uri.as_str() == syndication::NAMESPACE)
        .find_map(|(prefix, _)| feed.extensions().get(prefix))?;
    let first_value = |name: &str| {
        ext.get(name)
            .and_then(|values| values.first())
            .and_then(|e| e.value())
            .map(str::trim)
    };
    let period = first_value("updatePeriod")
        .and_then(|v| v.parse().ok())
        .unwrap_or(UpdatePeriod::Daily);
    let frequency = first_value("updateFrequency")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    update_interval_secs(&period, frequency)
}

/// Parse an RSS `<ttl>` (minutes) into seconds. Zero is treated as no hint.
fn parse_ttl(ttl: &str) -> Option<u64> {
    match ttl.trim().parse::<u64>() {
        Ok(0) | Err(_) => None,
        Ok(minutes) => Some(minutes.saturating_mul(60)),
    }
}

/// Seconds between updates for `frequency` updates per `period`. A zero
/// frequency is meaningless and yields `None`.
fn update_interval_secs(period: &UpdatePeriod, frequency: u32) -> Option<u64> {
    let period_secs: u64 = match period {
        UpdatePeriod::Hourly => 60 * 60,
        UpdatePeriod::Daily => 24 * 60 * 60,
        UpdatePeriod::Weekly => 7 * 24 * 60 * 60,
        UpdatePeriod::Monthly => 30 * 24 * 60 * 60,
        UpdatePeriod::Yearly => 365 * 24 * 60 * 60,
    };
    (frequency > 0).then(|| period_secs / u64::from(frequency))
}

/// Turn `<skipHours>` values into a bitmask. Hours outside 0–23 are
/// ignored, except 24, which some older feeds use for midnight.
fn skip_hours_mask(hours: &[String]) -> u32 {
    hours
        .iter()
        .filter_map(|h| h.trim().parse::<u32>().ok())
        .filter_map(|h| match h {
            0..=23 => Some(h),
            24 => Some(0),
            _ => None,
        })
        .fold(0, |mask, h| mask | (1 << h))
}

/// Turn `<skipDays>` values into a bitmask, Monday at bit 0. Day names are
/// matched case-insensitively; unrecognized names are ignored.
fn skip_days_mask(days: &[String]) -> u8 {
    const DAYS: [&str; 7] = [
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
        "sunday",
    ];
    days.iter()
        .filter_map(|d| {
            DAYS.iter()
                .position(|name| d.trim().eq_ignore_ascii_case(name))
        })
        .fold(0, |mask, i| mask | (1 << i))
}

fn atom_category(c: &atom_syndication::Category) -> AtomCategory {
    AtomCategory {
        term: c.term.clone(),
        scheme: c.scheme.clone(),
        label: c.label.clone(),
    }
}

fn extract_atom_feed_data(feed: &atom_syndication::Feed) -> AtomFeedIngestData {
    AtomFeedIngestData {
        atom_uri: feed.base.clone(),
        atom_language_tag: feed.lang.clone(),
        rights: feed.rights.as_ref().map(|r| r.value.clone()),
        generator: feed.generator.as_ref().map(|g| AtomGenerator {
            value: g.value.clone(),
            uri: g.uri.clone(),
            version: g.version.clone(),
        }),
        logo: feed.logo.clone(),
        icon: feed.icon.clone(),
        authors: feed.authors.iter().map(|p| p.name.clone()).collect(),
        contributors: feed.contributors.iter().map(|p| p.name.clone()).collect(),
        categories: feed.categories.iter().map(atom_category).collect(),
    }
}

/// Split an Atom entry into a [`FeedEntry`] and its Atom-specific data.
fn atom_entry_to_parts(feed_id: i64, entry: atom_syndication::Entry) -> AtomEntry {
    let data = AtomEntryIngestData {
        rights: entry.rights.as_ref().map(|r| r.value.clone()),
        authors: entry.authors.iter().map(|p| p.name.clone()).collect(),
        contributors: entry.contributors.iter().map(|p| p.name.clone()).collect(),
        categories: entry.categories.iter().map(atom_category).collect(),
    };
    let entry = FeedEntry {
        id: None,
        feed_id,
        syndication_format: "atom".to_string(),
        guid: entry.id,
        published_at: entry.published.map(|d| d.to_utc().timestamp()),
        title: entry.title.value,
        url: entry.links.into_iter().next().map(|l| l.href),
        content: entry.content.and_then(|c| c.value),
        authors: data.authors.clone(),
        categories: data.categories.iter().map(|c| c.term.clone()).collect(),
        tags: vec![],
    };
    AtomEntry { entry, data }
}

/// Split an RSS item into a [`FeedEntry`] and its RSS-specific data.
fn rss_item_to_parts(feed_id: i64, item: rss::Item) -> RssEntry {
    let rss::Item {
        pub_date,
        guid,
        title,
        link,
        description,
        author,
        comments,
        enclosure,
        categories,
        ..
    } = item;

    let timestamp = pub_date
        .as_deref()
        .and_then(|d| chrono::DateTime::parse_from_rfc2822(d).ok())
        .map(|d| d.timestamp())
        .unwrap_or_else(|| Utc::now().timestamp());

    let guid = guid.map(|g| g.value).unwrap_or_else(|| {
        format!(
            "rss-{}-{}",
            timestamp,
            title.as_deref().unwrap_or("no-title")
        )
    });

    let (enclosure_url, enclosure_length, enclosure_mime_type) = match enclosure {
        Some(e) => (Some(e.url), e.length.parse::<i64>().ok(), Some(e.mime_type)),
        None => (None, None, None),
    };

    let data = RssEntryIngestData {
        description: description.clone(),
        comments,
        author,
        enclosure_url,
        enclosure_length,
        enclosure_mime_type,
        categories: categories
            .into_iter()
            .map(|c| RssCategory {
                name: c.name,
                domain: c.domain,
            })
            .collect(),
    };

    let entry = FeedEntry {
        id: None,
        feed_id,
        syndication_format: "rss".to_string(),
        guid,
        published_at: Some(timestamp),
        title: title.unwrap_or_default(),
        url: link,
        content: description,
        authors: data.author.iter().cloned().collect(),
        categories: data.categories.iter().map(|c| c.name.clone()).collect(),
        tags: vec![],
    };
    RssEntry { entry, data }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn rss_hints(channel_extra: &str) -> FeedHints {
        let body = format!(
            r#"<rss version="2.0" xmlns:sy="http://purl.org/rss/1.0/modules/syndication/">
            <channel><title>t</title><link>http://x/</link><description>d</description>
            {channel_extra}</channel></rss>"#
        );
        *parse_feed(1, body.as_bytes()).expect("valid RSS").hints()
    }

    fn atom_hints(prefix: &str, feed_extra: &str) -> FeedHints {
        let body = format!(
            r#"<feed xmlns="http://www.w3.org/2005/Atom"
                xmlns:{prefix}="http://purl.org/rss/1.0/modules/syndication/">
            <title>t</title><id>urn:x</id><updated>2024-01-01T00:00:00Z</updated>
            {feed_extra}</feed>"#
        );
        *parse_feed(1, body.as_bytes()).expect("valid Atom").hints()
    }

    fn atom_site_url_of(links: &str) -> Option<String> {
        let body = format!(
            r#"<feed xmlns="http://www.w3.org/2005/Atom">
            <title>t</title><id>urn:x</id><updated>2024-01-01T00:00:00Z</updated>
            {links}</feed>"#
        );
        parse_feed(1, body.as_bytes())
            .expect("valid Atom")
            .site_url()
            .map(str::to_string)
    }

    #[test]
    fn rss_site_url_is_channel_link() {
        let body = br#"<rss version="2.0"><channel><title>t</title>
            <link> https://example.com/blog </link><description>d</description>
            </channel></rss>"#;
        let parsed = parse_feed(1, body).expect("valid RSS");
        assert_eq!(parsed.site_url(), Some("https://example.com/blog"));
    }

    #[test]
    fn rss_empty_channel_link_is_no_site_url() {
        let body = br#"<rss version="2.0"><channel><title>t</title>
            <link></link><description>d</description></channel></rss>"#;
        assert_eq!(parse_feed(1, body).expect("valid RSS").site_url(), None);
    }

    #[test]
    fn atom_site_url_prefers_html_alternate() {
        assert_eq!(
            atom_site_url_of(
                r#"<link rel="self" href="https://example.com/feed.atom"/>
                <link rel="alternate" type="application/json" href="https://example.com/feed.json"/>
                <link rel="alternate" type="text/html" href="https://example.com/"/>"#
            )
            .as_deref(),
            Some("https://example.com/")
        );
    }

    #[test]
    fn atom_link_without_rel_is_alternate() {
        assert_eq!(
            atom_site_url_of(r#"<link href="https://example.com/"/>"#).as_deref(),
            Some("https://example.com/")
        );
    }

    #[test]
    fn atom_without_alternate_has_no_site_url() {
        assert_eq!(
            atom_site_url_of(r#"<link rel="self" href="https://example.com/feed"/>"#),
            None
        );
    }

    #[test]
    fn rss_without_hints_is_default() {
        assert_eq!(rss_hints(""), FeedHints::default());
    }

    #[test]
    fn rss_ttl_is_minutes() {
        let hints = rss_hints("<ttl> 45 </ttl>");
        assert_eq!(hints.ttl_secs, Some(45 * 60));
        assert_eq!(hints.refresh_hint_secs(), Some(45 * 60));
    }

    #[test]
    fn rss_zero_or_garbage_ttl_is_ignored() {
        assert_eq!(rss_hints("<ttl>0</ttl>").ttl_secs, None);
        assert_eq!(rss_hints("<ttl>soon</ttl>").ttl_secs, None);
        assert_eq!(rss_hints("<ttl>-5</ttl>").ttl_secs, None);
    }

    #[test]
    fn rss_skip_hours_mask() {
        let hints = rss_hints(
            "<skipHours><hour>0</hour><hour>7</hour><hour>23</hour>\
             <hour>99</hour><hour>x</hour></skipHours>",
        );
        assert_eq!(hints.skip_hours, (1 << 0) | (1 << 7) | (1 << 23));
    }

    #[test]
    fn rss_skip_hour_24_means_midnight() {
        assert_eq!(
            rss_hints("<skipHours><hour>24</hour></skipHours>").skip_hours,
            1
        );
    }

    #[test]
    fn rss_skip_days_mask() {
        let hints = rss_hints(
            "<skipDays><day>Monday</day><day>SATURDAY</day><day> sunday </day>\
             <day>Caturday</day></skipDays>",
        );
        assert_eq!(hints.skip_days, (1 << 0) | (1 << 5) | (1 << 6));
    }

    #[test]
    fn rss_syndication_module() {
        let hints = rss_hints(
            "<sy:updatePeriod>hourly</sy:updatePeriod><sy:updateFrequency>2</sy:updateFrequency>",
        );
        assert_eq!(hints.update_interval_secs, Some(30 * 60));
    }

    #[test]
    fn rss_syndication_defaults_to_daily_once() {
        let hints = rss_hints("<sy:updateBase>2000-01-01T12:00+00:00</sy:updateBase>");
        assert_eq!(hints.update_interval_secs, Some(24 * 60 * 60));
    }

    #[test]
    fn rss_syndication_zero_frequency_is_ignored() {
        let hints = rss_hints("<sy:updateFrequency>0</sy:updateFrequency>");
        assert_eq!(hints.update_interval_secs, None);
    }

    #[test]
    fn refresh_hint_prefers_the_longer_of_ttl_and_syndication() {
        let hints = rss_hints("<ttl>30</ttl><sy:updatePeriod>hourly</sy:updatePeriod>");
        assert_eq!(hints.refresh_hint_secs(), Some(60 * 60));
        let hints = rss_hints("<ttl>120</ttl><sy:updatePeriod>hourly</sy:updatePeriod>");
        assert_eq!(hints.refresh_hint_secs(), Some(120 * 60));
    }

    #[test]
    fn atom_without_hints_is_default() {
        assert_eq!(atom_hints("sy", ""), FeedHints::default());
    }

    #[test]
    fn atom_syndication_module() {
        let hints = atom_hints(
            "sy",
            "<sy:updatePeriod>weekly</sy:updatePeriod><sy:updateFrequency>7</sy:updateFrequency>",
        );
        assert_eq!(hints.update_interval_secs, Some(24 * 60 * 60));
    }

    #[test]
    fn atom_syndication_module_under_another_prefix() {
        let hints = atom_hints("syn", "<syn:updatePeriod>hourly</syn:updatePeriod>");
        assert_eq!(hints.update_interval_secs, Some(60 * 60));
    }

    #[test]
    fn atom_bad_syndication_values_fall_back_to_defaults() {
        let hints = atom_hints(
            "sy",
            "<sy:updatePeriod>fortnightly</sy:updatePeriod><sy:updateFrequency>lots</sy:updateFrequency>",
        );
        assert_eq!(hints.update_interval_secs, Some(24 * 60 * 60));
    }
}
