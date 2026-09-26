//! Turning a feed body into a [`ParsedFeed`].

use super::{
    AtomCategory, AtomEntry, AtomEntryIngestData, AtomFeedIngestData, AtomGenerator, ParsedFeed,
    RssCategory, RssEntry, RssEntryIngestData,
};
use crate::scripting::FeedEntry;
use chrono::Utc;

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
/// assert!(matches!(parsed, ParsedFeed::Rss { ref entries } if entries.len() == 1));
///
/// assert!(parse_feed(1, b"<html>not a feed</html>").is_none());
/// ```
pub fn parse_feed(feed_id: i64, body: &[u8]) -> Option<ParsedFeed> {
    if let Ok(feed) = atom_syndication::Feed::read_from(body) {
        let data = extract_atom_feed_data(&feed);
        let entries = feed
            .entries
            .into_iter()
            .map(|e| atom_entry_to_parts(feed_id, e))
            .collect();
        return Some(ParsedFeed::Atom {
            feed: Box::new(data),
            entries,
        });
    }
    if let Ok(channel) = rss::Channel::read_from(body) {
        let entries = channel
            .items
            .into_iter()
            .map(|i| rss_item_to_parts(feed_id, i))
            .collect();
        return Some(ParsedFeed::Rss { entries });
    }
    None
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
        feed_id,
        syndication_format: "atom".to_string(),
        guid: entry.id,
        published_at: entry.published.map(|d| d.to_utc().timestamp()),
        title: entry.title.value,
        url: entry.links.into_iter().next().map(|l| l.href),
        content: entry.content.and_then(|c| c.value),
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
        feed_id,
        syndication_format: "rss".to_string(),
        guid,
        published_at: Some(timestamp),
        title: title.unwrap_or_default(),
        url: link,
        content: description,
        tags: vec![],
    };
    RssEntry { entry, data }
}
