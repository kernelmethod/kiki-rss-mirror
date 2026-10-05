use super::api::{fetch_entries, fetch_tag, fetch_tags, EntryPage};
use super::entries::render_entries;
use super::layout::{render_page, server_unavailable};
use super::listing::{render_pagination, Listing, PageParams};
use super::login::Api;
use super::plugins::is_same_origin;
use super::API_BASE;
use crate::db::tags::{TagKind, SYSTEM_TAG_PREFIX};
use crate::routes::v1::tags::list_tags::{ListTagsResponse, TagResponse};
use axum::{
    extract::{Path as UrlPath, Query},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use quick_xml::escape::escape;

/// Render the list of tags: the total number of tags, and one page of them,
/// each linked to its page.
pub(super) async fn tags_page(api: Api, Query(params): Query<PageParams>) -> Response {
    let page = params.page();
    match fetch_tags(&api, page).await {
        Ok(tags) => render_page(StatusCode::OK, "Tags - Kiki", &render_tag_list(&tags, page)),
        Err(e) => server_unavailable(&e),
    }
}

/// Render the page for tag `id`: one page of the entries with the tag,
/// newest first. Read entries are left out unless the `show_read` query
/// parameter is true, or the tag is `system:read`.
pub(super) async fn tag_page(
    api: Api,
    UrlPath(id): UrlPath<i64>,
    Query(params): Query<PageParams>,
) -> Response {
    let listing = Listing {
        feed: None,
        tag: Some(id),
        search: None,
        ..params.listing()
    };
    let tag = match fetch_tag(&api, id).await {
        Ok(Some(tag)) => tag,
        Ok(None) => {
            return render_page(
                StatusCode::NOT_FOUND,
                "Tag not found - Kiki",
                "<p>Tag not found.</p>\n<p><a href=\"/tags\">&larr; Back to tags</a></p>\n",
            )
        }
        Err(e) => return server_unavailable(&e),
    };
    let entries = match fetch_entries(&api, &listing, Some(&tag.name)).await {
        Ok(entries) => entries,
        Err(e) => return server_unavailable(&e),
    };

    render_page(
        StatusCode::OK,
        &format!("{} - Kiki", tag.name),
        &render_tag_page(&tag, &entries, &listing),
    )
}

/// Delete user tag `id`, responding with `204 No Content` once it is done.
/// Called by the delete button on the tag's page, whose script then goes
/// back to the list of tags.
///
/// System tags cannot be deleted: the Kiki API refuses to, and this passes
/// its `403 Forbidden` on. Also responds with `404 Not Found` if there is
/// no such tag, `502 Bad Gateway` if the Kiki server cannot delete it, and
/// `403 Forbidden` to requests from other sites, going by `headers`; see
/// [`is_same_origin`].
pub(super) async fn delete_tag(
    api: Api,
    UrlPath(id): UrlPath<i64>,
    headers: HeaderMap,
) -> Response {
    if !is_same_origin(&headers) {
        return (
            StatusCode::FORBIDDEN,
            "Tags may only be deleted from the web UI's own pages.",
        )
            .into_response();
    }

    let resp = api
        .delete(format!("{API_BASE}/v1/tags/id/{id}"))
        .send()
        .await;
    match resp.map(|resp| resp.status()) {
        Ok(StatusCode::NO_CONTENT) => StatusCode::NO_CONTENT.into_response(),
        Ok(StatusCode::NOT_FOUND) => (StatusCode::NOT_FOUND, "Tag not found.").into_response(),
        Ok(StatusCode::FORBIDDEN) => {
            (StatusCode::FORBIDDEN, "System tags cannot be deleted.").into_response()
        }
        Ok(status) => {
            tracing::warn!(%status, tag_id = id, "failed to delete tag");
            (StatusCode::BAD_GATEWAY, "The tag could not be deleted.").into_response()
        }
        Err(e) => {
            tracing::warn!("failed to reach the Kiki server: {e:#}");
            (StatusCode::BAD_GATEWAY, "The Kiki server is unavailable.").into_response()
        }
    }
}

/// Render `tags`, the tags attached to an entry, as a list, or nothing if
/// there are none.
///
/// System tags come first, shown without their `system:` prefix and styled
/// apart from user tags; each group is sorted by name.
pub(super) fn render_tags(tags: &[TagResponse]) -> String {
    if tags.is_empty() {
        return String::new();
    }
    let mut tags: Vec<&TagResponse> = tags.iter().collect();
    tags.sort_by(|a, b| {
        (a.kind != TagKind::System, &a.name).cmp(&(b.kind != TagKind::System, &b.name))
    });

    let items: Vec<String> = tags
        .into_iter()
        .map(|tag| format!("<li {}>{}</li>", tag_attrs(tag), escape(tag_label(tag))))
        .collect();
    format!(
        "<ul class=\"tags\" aria-label=\"Tags\">{}</ul>",
        items.concat()
    )
}

/// The name `tag` is shown under: system tags lose their `system:` prefix.
pub(super) fn tag_label(tag: &TagResponse) -> &str {
    match tag.kind {
        TagKind::System => tag
            .name
            .strip_prefix(SYSTEM_TAG_PREFIX)
            .unwrap_or(&tag.name),
        TagKind::User => &tag.name,
    }
}

/// The attributes of the element `tag` is shown in: its class, and for a
/// system tag, its full name as a tooltip.
pub(super) fn tag_attrs(tag: &TagResponse) -> String {
    match tag.kind {
        TagKind::System => format!("class=\"tag system\" title=\"{}\"", escape(&tag.name)),
        TagKind::User => "class=\"tag\"".to_owned(),
    }
}

/// Render the tag count, the tags on page `page`, each linked to its page,
/// and the page links.
pub(super) fn render_tag_list(resp: &ListTagsResponse, page: u32) -> String {
    let mut html = format!(
        "<h2>Tags</h2>\n<p class=\"count\">{} {}</p>\n",
        resp.count,
        if resp.count == 1 { "tag" } else { "tags" }
    );

    if resp.tags.is_empty() {
        html.push_str("<p>No tags on this page.</p>\n");
    } else {
        let items: Vec<String> = resp
            .tags
            .iter()
            .map(|tag| {
                format!(
                    "<li><a href=\"/tags/{}\" {}>{}</a></li>",
                    tag.id,
                    tag_attrs(tag),
                    escape(tag_label(tag))
                )
            })
            .collect();
        html.push_str(&format!(
            "<ul class=\"tags tag-list\">{}</ul>\n",
            items.concat()
        ));
    }

    html.push_str(&render_pagination(
        resp.count,
        page,
        |page| format!("/tags?page={page}"),
        ("&larr; Previous", "Next &rarr;"),
    ));
    html
}

/// Render the page for `tag`: its name, a button that deletes it if it is a
/// user tag, and the entries on this page of `listing`, each with the title
/// of its feed.
pub(super) fn render_tag_page(tag: &TagResponse, entries: &EntryPage, listing: &Listing) -> String {
    let mut html = format!(
        "<div class=\"title-row\">\n<h2>Entries tagged <span {}>{}</span></h2>\n{}</div>\n",
        tag_attrs(tag),
        escape(tag_label(tag)),
        render_delete_tag_button(tag),
    );
    html.push_str(&render_entries(
        entries.count,
        &entries.entries,
        true,
        listing,
    ));
    html.push_str("<p><a href=\"/tags\">&larr; Back to tags</a></p>\n");
    html
}

/// Render the button that deletes `tag`, or nothing for a system tag, which
/// cannot be deleted. The button does nothing on its own: the script in
/// `page.js` asks to confirm, then sends the request to [`delete_tag`].
pub(super) fn render_delete_tag_button(tag: &TagResponse) -> String {
    match tag.kind {
        TagKind::System => String::new(),
        TagKind::User => format!(
            "<button type=\"button\" class=\"delete-tag\" data-tag=\"{}\" \
             data-name=\"{}\">Delete tag</button>\n",
            tag.id,
            escape(&tag.name)
        ),
    }
}
