use super::api::{fetch_cached_assets, fetch_entries, fetch_entry, fetch_system_tag_id};
use super::feeds::{display_title, render_meta, safe_link};
use super::layout::{render_page, render_search_page, server_unavailable};
use super::listing::{
    render_filter, render_pagination, render_search_help, render_sort, Listing, PageParams,
};
use super::login::Api;
use super::plugins::is_same_origin;
use super::sanitize;
use super::tags::render_tags;
use super::API_BASE;
use crate::db::tags::{SystemTag, TagKind};
use crate::routes::v1::entries::get_entry::GetEntryResponse;
use crate::routes::v1::entries::ListEntriesResponseEntry;
use crate::routes::v1::tags::list_tags::TagResponse;
use crate::routes::v1::tags::tag_entries::AddTagEntriesRequest;
use axum::{
    extract::{Path as UrlPath, Query},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use quick_xml::escape::escape;
use serde::Deserialize;
use std::collections::HashMap;

/// Render the index page: the total number of entries, and one page of
/// them, newest first, each with the feed it came from, and links to the
/// neighbouring pages. Read entries are left out unless the `show_read`
/// query parameter is true.
pub(super) async fn index(api: Api, Query(params): Query<PageParams>) -> Response {
    let listing = Listing {
        feed: None,
        tag: None,
        search: None,
        ..params.listing()
    };
    match fetch_entries(&api, &listing, None).await {
        Ok(entries) => render_page(
            StatusCode::OK,
            "Kiki",
            &render_entries(entries.count, &entries.entries, true, &listing),
        ),
        Err(e) => server_unavailable(&e),
    }
}

/// Render the page for entry `id`: a summary of the entry built from what
/// its feed says about it, with a link through to the entry itself.
///
/// The page links back to the list of entries it was opened from: the
/// index, a feed's page, or a tag's page.
pub(super) async fn entry_page(
    api: Api,
    UrlPath(id): UrlPath<i64>,
    Query(params): Query<PageParams>,
) -> Response {
    let entry = match fetch_entry(&api, id).await {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            return render_page(
                StatusCode::NOT_FOUND,
                "Entry not found - Kiki",
                &format!(
                    "<p>Entry not found.</p>\n{}",
                    render_back_link(&params.listing())
                ),
            )
        }
        Err(e) => return server_unavailable(&e),
    };

    let cached = fetch_cached_assets(&api, id).await;
    render_page(
        StatusCode::OK,
        &format!("{} - Kiki", display_title(&entry.title)),
        &render_entry_page(
            &entry,
            entry.feed_title.as_deref(),
            &cached,
            &params.listing(),
        ),
    )
}

/// Give entry `id` the system tag `name`. Called by the save buttons' and
/// swipe-to-read gestures' script; see [`set_entry_system_tag`].
pub(super) async fn add_entry_system_tag(
    api: Api,
    UrlPath((id, name)): UrlPath<(i64, String)>,
    headers: HeaderMap,
) -> Response {
    set_entry_system_tag(&api, id, &name, &headers, true).await
}

/// Remove the system tag `name` from entry `id`. Called by the save
/// buttons' script, and to undo a swipe; see [`set_entry_system_tag`].
pub(super) async fn remove_entry_system_tag(
    api: Api,
    UrlPath((id, name)): UrlPath<(i64, String)>,
    headers: HeaderMap,
) -> Response {
    set_entry_system_tag(&api, id, &name, &headers, false).await
}

/// Add (`add == true`) or remove the system tag `name` on entry `id`,
/// through `PUT`/`DELETE /v1/entries/id/{id}/system-tags/{name}` on the
/// Kiki API, responding with `204 No Content` once it is done. `name` is
/// the tag's name with or without its `system:` prefix, e.g. `read`.
///
/// Responds with `404 Not Found` if there is no such entry or system tag,
/// `502 Bad Gateway` if the Kiki server cannot make the change, `401` or
/// `403` if the Kiki server refuses the logged-in token, and `403
/// Forbidden` to requests from other sites, going by `headers`; see
/// [`is_same_origin`].
pub(super) async fn set_entry_system_tag(
    api: &Api,
    id: i64,
    name: &str,
    headers: &HeaderMap,
    add: bool,
) -> Response {
    if !is_same_origin(headers) {
        return (
            StatusCode::FORBIDDEN,
            "Entries may only be changed from the web UI's own pages.",
        )
            .into_response();
    }
    // Only a known tag's own name goes into the API's URL, so that `name`
    // cannot reach another of its routes, as `..%2F..` would.
    let Ok(tag) = name.parse::<SystemTag>() else {
        return (StatusCode::NOT_FOUND, "System tag not found.").into_response();
    };

    let url = format!(
        "{API_BASE}/v1/entries/id/{id}/system-tags/{}",
        tag.short_name()
    );
    let req = if add { api.put(url) } else { api.delete(url) };
    match req.send().await.map(|resp| resp.status()) {
        Ok(StatusCode::OK) => StatusCode::NO_CONTENT.into_response(),
        Ok(StatusCode::NOT_FOUND) => (StatusCode::NOT_FOUND, "Entry not found.").into_response(),
        Ok(status @ (StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)) => {
            (status, "Your token may not change entries.").into_response()
        }
        Ok(status) => {
            tracing::warn!(%status, entry_id = id, %tag, add, "failed to update entry's system tag");
            (StatusCode::BAD_GATEWAY, "The entry could not be updated.").into_response()
        }
        Err(e) => {
            tracing::warn!("failed to reach the Kiki server: {e:#}");
            (StatusCode::BAD_GATEWAY, "The Kiki server is unavailable.").into_response()
        }
    }
}

/// Query parameters accepted by [`mark_entries_read`].
#[derive(Deserialize)]
pub(super) struct MarkReadParams {
    /// Only mark this feed's entries as read, rather than every entry.
    pub(super) feed: Option<i64>,
}

/// Mark every entry as read, or only those from the feed given in
/// `params`, by giving them the `system:read` tag in one request to the
/// Kiki API, once [`fetch_system_tag_id`] has looked up the tag. Called by the "Mark all as read" buttons' script, which
/// reloads the page afterwards.
///
/// Responds with `204 No Content` once it is done, `502 Bad Gateway` if
/// the Kiki server cannot make the change, and `403 Forbidden` to requests
/// from other sites, going by `headers`; see [`is_same_origin`].
pub(super) async fn mark_entries_read(
    api: Api,
    Query(params): Query<MarkReadParams>,
    headers: HeaderMap,
) -> Response {
    if !is_same_origin(&headers) {
        return (
            StatusCode::FORBIDDEN,
            "Entries may only be marked as read from the web UI's own pages.",
        )
            .into_response();
    }

    let result = async {
        let tag_id = fetch_system_tag_id(&api, SystemTag::Read).await?;
        let request = AddTagEntriesRequest {
            feed_id: params.feed,
            ..Default::default()
        };
        api.post(format!("{API_BASE}/v1/tags/id/{tag_id}/entries"))
            .json(&request)
            .send()
            .await?
            .error_for_status()?;
        anyhow::Ok(())
    }
    .await;
    let refused = result.as_ref().err().and_then(|e| {
        e.downcast_ref::<reqwest::Error>()
            .and_then(reqwest::Error::status)
            .filter(|s| matches!(*s, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN))
    });
    if let Some(status) = refused {
        return (status, "Your token may not mark entries as read.").into_response();
    }
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::warn!(
                feed_id = params.feed,
                "failed to mark entries as read: {e:#}"
            );
            (
                StatusCode::BAD_GATEWAY,
                "The entries could not be marked as read.",
            )
                .into_response()
        }
    }
}

/// Render the search page: one page of the entries matching the `q` query
/// parameter, best match first, or newest first if the `sort` parameter is
/// `newest`. Read entries are listed too, but hidden ones are not. With no
/// `q`, the page only asks what to search for.
pub(super) async fn search_page(api: Api, Query(params): Query<PageParams>) -> Response {
    let listing = Listing {
        feed: None,
        tag: None,
        ..params.listing()
    };
    let Some(search) = &listing.search else {
        return render_page(
            StatusCode::OK,
            "Search - Kiki",
            &format!("<h2>Search</h2>\n{}", render_search_help()),
        );
    };
    let entries = match fetch_entries(&api, &listing, None).await {
        Ok(entries) => entries,
        Err(e) => return server_unavailable(&e),
    };

    render_search_page(
        StatusCode::OK,
        &format!("{} - Search - Kiki", search.query),
        &search.query,
        &format!(
            "<h2>Search results for &ldquo;{}&rdquo;</h2>\n{}{}",
            escape(&search.query),
            render_entries(entries.count, &entries.entries, true, &listing),
            render_search_help(),
        ),
    )
}

/// Render the entry count (`count`, of all the entries in the list), the
/// "Mark all as read" button and the filter menu — or, for search results,
/// the links that sort them — the entries on this page of `listing`, and the
/// page links. Each entry is shown with its own tags, and with the title of
/// its feed if `show_feed`.
pub(super) fn render_entries(
    count: usize,
    entries: &[ListEntriesResponseEntry],
    show_feed: bool,
    listing: &Listing,
) -> String {
    let searching = listing.search.is_some();
    let unread = if listing.show_read { "" } else { "unread " };
    let (noun, actions) = if searching {
        (
            if count == 1 { "result" } else { "results" },
            render_sort(listing),
        )
    } else {
        (
            if count == 1 { "entry" } else { "entries" },
            // Entries can be marked as read in bulk by feed, but not by tag.
            if count == 0 || listing.tag.is_some() {
                render_filter(listing)
            } else {
                render_mark_read_button(listing.feed) + &render_filter(listing)
            },
        )
    };
    let mut html = format!(
        "<div class=\"list-header\">\n<p class=\"count\">{count} {}{noun}</p>\n\
         <div class=\"list-actions\">{actions}</div>\n</div>\n",
        if searching { "" } else { unread },
    );

    if entries.is_empty() {
        html.push_str(match (count, searching, listing.show_read) {
            (0, true, _) => "<p>No entries match your search.</p>\n",
            (0, false, true) => "<p>No entries yet.</p>\n",
            (0, false, false) => "<p>No unread entries.</p>\n",
            _ => "<p>No entries on this page.</p>\n",
        });
    } else {
        // Where read entries are left out, an unread entry can be swiped
        // away to mark it as read; the script in `page.js` does the rest.
        let swipeable = !listing.show_read && !searching;
        html.push_str("<ol class=\"entries\">\n");
        for entry in entries {
            let feed = entry.feed_title.as_deref().filter(|_| show_feed);
            let tags = entry.tags.as_slice();
            if swipeable && !has_system_tag(tags, SystemTag::Read) {
                html.push_str(&format!(
                    "<li class=\"swipe-read\" data-entry=\"{}\">",
                    entry.id
                ));
            } else {
                html.push_str("<li>");
            }
            html.push_str(&render_entry(entry, feed, tags, listing));
            html.push_str("</li>\n");
        }
        html.push_str("</ol>\n");
    }

    html.push_str(&render_pagination(
        count,
        listing.page,
        |page| listing.page_href(page),
        match &listing.search {
            Some(search) if !search.newest => ("&larr; Previous", "Next &rarr;"),
            _ => ("&larr; Newer", "Older &rarr;"),
        },
    ));
    html
}

/// Render a single entry in a list: its title, linked to the entry's page,
/// and its save button, and below them its publication date, the title of
/// `feed`, the feed it came from, and its `tags`. The entry's page links
/// back to `listing`.
pub(super) fn render_entry(
    entry: &ListEntriesResponseEntry,
    feed: Option<&str>,
    tags: &[TagResponse],
    listing: &Listing,
) -> String {
    let href = listing.entry_href(entry.id);
    let meta = render_meta(
        entry.published_at.as_deref(),
        feed,
        entry.feed_favicon_url.as_deref(),
        None,
    );
    format!(
        "<a href=\"{href}\">{}</a> {}{meta}{}",
        escape(display_title(&entry.title)),
        render_save_button(entry.id, tags),
        render_tags(tags)
    )
}

/// Render the button that marks every entry as read, or only those from
/// `feed`. The button does nothing on its own: the script in `page.js`
/// sends the request to [`mark_entries_read`].
pub(super) fn render_mark_read_button(feed: Option<i64>) -> String {
    let (data, label) = match feed {
        Some(id) => (format!(" data-feed=\"{id}\""), "this feed&rsquo;s entries"),
        None => (String::new(), "every entry"),
    };
    format!(
        "<button type=\"button\" class=\"mark-read\"{data} \
         title=\"Mark {label} as read\">Mark all as read</button>"
    )
}

/// The bookmark drawn on save buttons; filled in when the entry is saved.
pub(super) const SAVE_ICON: &str =
    "<svg viewBox=\"0 0 24 24\" aria-hidden=\"true\" focusable=\"false\">\
    <path d=\"M6 3h12v18l-6-4.5L6 21z\"/></svg>";

/// Render the button that saves entry `id`, or unsaves it if its `tags`
/// include `system:saved`. The button does nothing on its own: the script
/// in `page.js` sends the change to [`add_entry_system_tag`] or
/// [`remove_entry_system_tag`].
pub(super) fn render_save_button(id: i64, tags: &[TagResponse]) -> String {
    let saved = has_system_tag(tags, SystemTag::Saved);
    format!(
        "<button type=\"button\" class=\"save\" data-entry=\"{id}\" aria-pressed=\"{saved}\" \
         aria-label=\"Save\" title=\"{}\">{SAVE_ICON}</button>",
        if saved { "Unsave" } else { "Save" }
    )
}

/// Whether an entry's `tags` include the system tag `tag`.
pub(super) fn has_system_tag(tags: &[TagResponse], tag: SystemTag) -> bool {
    tags.iter()
        .any(|t| t.kind == TagKind::System && t.name == tag.name())
}

/// Render the page for `entry`: its title, date, feed (`feed`), author,
/// categories and `tags`, its content from the feed, and links to the entry
/// itself and to anything else the feed links it to. The page links back to
/// `listing`.
///
/// Images in the content, and the entry's attachment, are taken from the
/// asset cache: `cached` maps an asset's original URL to the URL of its
/// cached copy. Images that are not cached are shown as links instead, and
/// an attachment that is not cached is linked where the feed says it is.
pub(super) fn render_entry_page(
    entry: &GetEntryResponse,
    feed: Option<&str>,
    cached: &HashMap<String, String>,
    listing: &Listing,
) -> String {
    let authors: Vec<&str> = match (&entry.rss, &entry.atom) {
        (Some(rss), _) => rss.author.as_deref().into_iter().collect(),
        (None, Some(atom)) => atom.authors.iter().map(String::as_str).collect(),
        (None, None) => Vec::new(),
    };
    let categories: Vec<&str> = match (&entry.rss, &entry.atom) {
        (Some(rss), _) => rss.categories.iter().map(|c| c.category.as_str()).collect(),
        (None, Some(atom)) => atom
            .categories
            .iter()
            .map(|c| c.label.as_deref().unwrap_or(&c.term))
            .collect(),
        (None, None) => Vec::new(),
    };
    let authors = authors.join(", ");

    let mut html = format!(
        "<article class=\"entry\">\n<div class=\"title-row\"><h2>{}</h2>{}</div>\n{}\n",
        escape(display_title(&entry.title)),
        render_save_button(entry.id, &entry.tags),
        render_meta(
            entry.published_at.as_deref(),
            feed,
            entry.feed_favicon_url.as_deref(),
            (!authors.trim().is_empty()).then_some(authors.as_str()),
        ),
    );
    if !categories.is_empty() {
        let categories: Vec<_> = categories.into_iter().map(escape).collect();
        html.push_str(&format!(
            "<span class=\"meta categories\">Filed under {}</span>\n",
            categories.join(", ")
        ));
    }
    html.push_str(&render_tags(&entry.tags));

    // Links and images in the content are resolved against the entry's own
    // URL, where the content was written to appear.
    let base = url::Url::parse(&entry.url).ok();
    let content = entry
        .content
        .as_deref()
        .or_else(|| {
            entry
                .rss
                .as_ref()
                .and_then(|rss| rss.description.as_deref())
        })
        .filter(|c| !c.trim().is_empty());
    match content
        .map(|c| sanitize::sanitize_html(c, base.as_ref(), |url| cached.get(url.as_str()).cloned()))
    {
        Some(Ok(content)) => {
            html.push_str(&format!("<div class=\"content\">\n{content}\n</div>\n"));
        }
        Some(Err(e)) => {
            tracing::warn!(entry_id = entry.id, "failed to sanitize entry content: {e}");
            html.push_str(
                "<p class=\"content\"><em>This entry's summary could not be shown.</em></p>\n",
            );
        }
        None => html.push_str(
            "<p class=\"content\"><em>The feed gives no summary of this entry.</em></p>\n",
        ),
    }

    let mut links = Vec::new();
    if let Some(url) = safe_link(&entry.url) {
        links.push(format!(
            "<a href=\"{}\" rel=\"noopener noreferrer\">Read the full entry &rarr;</a>",
            escape(url)
        ));
    }
    if let Some(rss) = &entry.rss {
        if let Some(url) = rss.comments.as_deref().and_then(safe_link) {
            links.push(format!(
                "<a href=\"{}\" rel=\"noopener noreferrer\">Comments</a>",
                escape(url)
            ));
        }
        if let Some(url) = rss.enclosure_url.as_deref().and_then(safe_link) {
            // The cache keys an enclosure by its URL as `url` writes it.
            let url = url::Url::parse(url)
                .ok()
                .and_then(|u| cached.get(u.as_str()))
                .map_or(url, String::as_str);
            let kind = rss
                .enclosure_mime_type
                .as_deref()
                .map(|t| format!(" ({})", escape(t)))
                .unwrap_or_default();
            links.push(format!(
                "<a href=\"{}\" rel=\"noopener noreferrer\">Attachment{kind}</a>",
                escape(url)
            ));
        }
    }
    if !links.is_empty() {
        html.push_str(&format!(
            "<p class=\"links\">{}</p>\n",
            links.join(" &middot; ")
        ));
    }

    html.push_str("</article>\n");
    html.push_str(&render_back_link(listing));
    html
}

/// Render the link back to `listing`.
pub(super) fn render_back_link(listing: &Listing) -> String {
    let label = match (listing.feed, listing.tag, &listing.search) {
        (Some(_), _, _) => "Back to feed",
        (None, Some(_), _) => "Back to tag",
        (None, None, Some(_)) => "Back to search results",
        (None, None, None) => "Back to entries",
    };
    format!("<p><a href=\"{}\">&larr; {label}</a></p>\n", listing.href())
}
