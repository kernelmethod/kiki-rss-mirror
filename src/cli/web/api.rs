use super::listing::{fts_query, Listing, PAGE_SIZE};
use super::API_BASE;
use crate::db::tags::SystemTag;
use crate::routes::v1::entries::entry_assets::ListEntryAssetsResponse;
use crate::routes::v1::entries::get_entry::GetEntryResponse;
use crate::routes::v1::entries::search_entries::SearchEntriesResponse;
use crate::routes::v1::entries::ListEntriesResponseEntry;
use crate::routes::v1::feeds::list_feeds::ListFeedsResponse;
use crate::routes::v1::plugins::list_plugins::ListPluginsResponse;
use crate::routes::v1::tags::list_tags::{ListTagsResponse, TagResponse};
use anyhow::{anyhow, Result};
use axum::http::StatusCode;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

/// Look up the ID of the system tag `tag` through the Kiki API.
pub(super) async fn fetch_system_tag_id(api: &reqwest::Client, tag: SystemTag) -> Result<i64> {
    let tags: ListTagsResponse = api
        .get(format!("{API_BASE}/v1/tags?kind=system"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    tags.tags
        .into_iter()
        .find(|t| t.name == tag.name())
        .map(|t| t.id)
        .ok_or_else(|| anyhow!("the Kiki server has no {tag} tag"))
}

/// Fetch the assets — images and enclosures — cached for entry `id` from
/// the Kiki API, mapping each asset's original URL to the web UI URL that
/// serves the cached copy.
///
/// If the list cannot be fetched it is logged and treated as empty, so the
/// entry is still shown, linking to its images and attachments where they
/// were found.
pub(super) async fn fetch_cached_assets(api: &reqwest::Client, id: i64) -> HashMap<String, String> {
    let assets: Result<ListEntryAssetsResponse> = async {
        Ok(api
            .get(format!("{API_BASE}/v1/entries/id/{id}/assets"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }
    .await;

    match assets {
        Ok(assets) => assets
            .assets
            .into_iter()
            .map(|a| (a.original_url, format!("/assets/{}", a.blake3)))
            .collect(),
        Err(e) => {
            tracing::warn!(entry_id = id, "failed to fetch cached assets: {e:#}");
            HashMap::new()
        }
    }
}

/// A page of a list of entries, as fetched by [`fetch_entries`].
pub(super) struct EntryPage {
    /// Number of entries in the whole list.
    pub(super) count: usize,
    /// The entries on the page, newest first.
    pub(super) entries: Vec<ListEntriesResponseEntry>,
}

/// Fetch the entries on this page of `listing` with `/v1/entries/search`:
/// the entries of its feed, or with the tag named `tag`, or of every feed,
/// newest first; or the entries matching its search, sorted as it asks.
/// Hidden entries are left out, and so are read ones unless the listing
/// shows them or is of search results — except on the pages of the `system:hidden` and
/// `system:read` tags themselves, which would otherwise always be empty.
///
/// A search with no words to search for (see [`fts_query`]) matches no
/// entries, and the Kiki API is not asked.
pub(super) async fn fetch_entries(
    api: &reqwest::Client,
    listing: &Listing,
    tag: Option<&str>,
) -> Result<EntryPage> {
    let (query, sort) = match &listing.search {
        Some(search) => match fts_query(&search.query) {
            Some(query) => (
                Some(query),
                if search.newest {
                    "published_at"
                } else {
                    "relevance"
                },
            ),
            None => {
                return Ok(EntryPage {
                    count: 0,
                    entries: Vec::new(),
                })
            }
        },
        None => (None, "published_at"),
    };
    let offset = u64::from(listing.page - 1) * u64::from(PAGE_SIZE);
    let mut excluded = vec![SystemTag::Hidden.name()];
    if !listing.show_read && listing.search.is_none() {
        excluded.push(SystemTag::Read.name());
    }
    excluded.retain(|&name| Some(name) != tag);
    let exclude = match excluded.as_slice() {
        [] => None,
        [name] => Some(serde_json::json!({ "not": name })),
        names => Some(serde_json::json!({ "not": { "or": names } })),
    };
    let tags = match (tag, exclude) {
        (Some(tag), Some(exclude)) => serde_json::json!({ "and": [tag, exclude] }),
        (Some(tag), None) => serde_json::json!(tag),
        (None, Some(exclude)) => exclude,
        (None, None) => Value::Null,
    };
    let resp: SearchEntriesResponse = api
        .post(format!("{API_BASE}/v1/entries/search"))
        .json(&serde_json::json!({
            "tags": tags,
            "feed_id": listing.feed,
            "query": query,
            "sort": sort,
            "offset": offset,
            "limit": PAGE_SIZE,
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(EntryPage {
        count: resp.count,
        entries: resp.entries.into_iter().map(|e| e.entry).collect(),
    })
}

/// Fetch entry `id` from the Kiki API, or `None` if there is no such entry.
pub(super) async fn fetch_entry(
    api: &reqwest::Client,
    id: i64,
) -> Result<Option<GetEntryResponse>> {
    let resp = api
        .get(format!("{API_BASE}/v1/entries/id/{id}"))
        .send()
        .await?;
    if resp.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    Ok(Some(resp.error_for_status()?.json().await?))
}

/// Fetch page `page` (counting from 1) of `/v1/feeds` from the Kiki API.
pub(super) async fn fetch_feeds(api: &reqwest::Client, page: u32) -> Result<ListFeedsResponse> {
    let offset = u64::from(page - 1) * u64::from(PAGE_SIZE);
    Ok(api
        .get(format!(
            "{API_BASE}/v1/feeds?offset={offset}&limit={PAGE_SIZE}"
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// Fetch feed `id` from the Kiki API, or `None` if there is no such feed.
pub(super) async fn fetch_feed(api: &reqwest::Client, id: i64) -> Result<Option<Feed>> {
    let resp = api
        .get(format!("{API_BASE}/v1/feeds/id/{id}"))
        .send()
        .await?;
    if resp.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    Ok(Some(resp.error_for_status()?.json().await?))
}

/// Fetch page `page` (counting from 1) of `/v1/tags` from the Kiki API.
pub(super) async fn fetch_tags(api: &reqwest::Client, page: u32) -> Result<ListTagsResponse> {
    let offset = u64::from(page - 1) * u64::from(PAGE_SIZE);
    Ok(api
        .get(format!(
            "{API_BASE}/v1/tags?offset={offset}&limit={PAGE_SIZE}"
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// Fetch tag `id` from the Kiki API, or `None` if there is no such tag.
pub(super) async fn fetch_tag(api: &reqwest::Client, id: i64) -> Result<Option<TagResponse>> {
    fetch_optional(api, format!("{API_BASE}/v1/tags/id/{id}")).await
}

/// Fetch `/v1/plugins` from the Kiki API.
pub(super) async fn fetch_plugins(api: &reqwest::Client) -> Result<ListPluginsResponse> {
    Ok(api
        .get(format!("{API_BASE}/v1/plugins"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// Fetch `url` from the Kiki API, or `None` if it is not found.
pub(super) async fn fetch_optional<T: DeserializeOwned>(
    api: &reqwest::Client,
    url: String,
) -> Result<Option<T>> {
    let resp = api.get(url).send().await?;
    if resp.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    Ok(Some(resp.error_for_status()?.json().await?))
}

/// The Kiki API URL of plugin `name`, followed by the path segments in
/// `rest`, each percent-encoded.
pub(super) fn plugin_api_url(name: &str, rest: &[&str]) -> String {
    let mut url = format!("{API_BASE}/v1/plugins/name/{}", encode_path_segment(name));
    for segment in rest {
        url.push('/');
        url.push_str(&encode_path_segment(segment));
    }
    url
}

/// Percent-encode `s` for use as one segment of a URL's path, leaving only
/// ASCII letters, digits, `-`, `.`, `_` and `~` as they are. The result
/// needs no further escaping to go in HTML.
pub(super) fn encode_path_segment(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// The part of a `/v1/feeds/id/{id}` response the web UI uses.
#[derive(Deserialize)]
pub(super) struct Feed {
    pub(super) title: String,
    pub(super) url: String,
    pub(super) description: Option<String>,
    /// When the feed was last checked, in RFC 3339.
    pub(super) last_checked: Option<String>,
    /// The API URL of the feed's cached favicon.
    #[serde(default)]
    pub(super) favicon_url: Option<String>,
}
