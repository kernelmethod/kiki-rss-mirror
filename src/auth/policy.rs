//! Which scope each route requires.
//!
//! Every route the server serves is listed in [`requirement`]. A route
//! missing from it requires [`Scope::Admin`], so a route added without a
//! thought for its scope is locked down rather than left open; the tests
//! check that every documented route is listed. Every route is documented
//! in [`ApiDoc`](crate::routes::v1::docs::ApiDoc), and
//! `routes::test::every_route_checks_tokens` checks that each one passes
//! through [`super::authorize`], which applies this table.

use super::Scope;
use axum::http::Method;

/// What a request must be allowed to reach a route: by its token's scopes,
/// or, for a request without a token, by the
/// [`AnonymousAccess`](crate::config::AnonymousAccess) setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requirement {
    /// Nothing in particular: any valid token will do, as will no token,
    /// whatever the anonymous access setting.
    Any,
    /// The scope.
    Scope(Scope),
}

/// What a request for `method` on the route `path` (the route's pattern,
/// such as `/v1/feeds/id/{id}`) must carry, or `None` if the route is not
/// listed, in which case [`Scope::Admin`] is required.
pub fn requirement(method: &Method, path: &str) -> Option<Requirement> {
    use Requirement::Any;
    use Scope::{Admin, Feeds, Metrics, Read, State, Tags};
    let scope = Requirement::Scope;

    let get = *method == Method::GET || *method == Method::HEAD;
    let m = method.as_str();
    Some(match (m, path) {
        // Meta
        (_, "/v1/" | "/v1/health" | "/v1/access" | "/docs") if get => Any,
        ("POST", "/v1/shutdown") => scope(Admin),
        (_, "/metrics") if get => scope(Metrics),

        // Tokens
        (_, "/v1/tokens/current") if get => Any,
        (_, "/v1/tokens") if get => scope(Admin),
        ("POST", "/v1/tokens") => scope(Admin),
        ("DELETE", "/v1/tokens/id/{id}") => scope(Admin),

        // Cached assets
        (_, "/v1/assets/by-url" | "/v1/assets/{hash}") if get => scope(Read),
        ("DELETE", "/v1/assets/{hash}") => scope(Admin),

        // Feeds
        (
            _,
            "/v1/feeds"
            | "/v1/feeds/id/{id}"
            | "/v1/feeds/id/{id}/tags"
            | "/v1/feeds/id/{id}/entries"
            | "/v1/feeds/id/{id}/favicon"
            | "/v1/feeds/export",
        ) if get => scope(Read),
        ("POST", "/v1/feeds/create" | "/v1/feeds/refresh" | "/v1/feeds/refresh/{id}") => {
            scope(Feeds)
        }
        ("POST", "/v1/feeds/import") => scope(Feeds),
        ("PUT" | "DELETE", "/v1/feeds/id/{id}") => scope(Feeds),
        ("PUT", "/v1/feeds/id/{id}/tags") => scope(Tags),

        // Entries
        (_, "/v1/entries" | "/v1/entries/id/{id}" | "/v1/entries/id/{id}/tags") if get => {
            scope(Read)
        }
        (_, "/v1/entries/id/{id}/assets") if get => scope(Read),
        // Searches and batch lookups only read, though they are POSTs.
        ("POST", "/v1/entries/search" | "/v1/entries/search/ids" | "/v1/entries/batch") => {
            scope(Read)
        }
        ("DELETE", "/v1/entries/id/{id}") => scope(Feeds),
        ("POST", "/v1/entries/cleanup") => scope(Feeds),
        ("PUT", "/v1/entries/id/{id}/tags") => scope(Tags),
        ("PUT" | "DELETE", "/v1/entries/id/{id}/system-tags/{name}") => scope(State),

        // Tags
        (
            _,
            "/v1/tags" | "/v1/tags/id/{id}" | "/v1/tags/id/{id}/feeds" | "/v1/tags/id/{id}/entries",
        ) if get => scope(Read),
        ("POST", "/v1/tags/create") => scope(Tags),
        ("PUT" | "DELETE", "/v1/tags/id/{id}") => scope(Tags),
        // Adding entries to a system tag only changes their state; the
        // handler requires `tags` as well when the tag is a user tag.
        ("POST" | "DELETE", "/v1/tags/id/{id}/entries") => scope(State),

        // Plugins and settings, which may hold credentials, are for
        // administrators only.
        (_, p) if p.starts_with("/v1/plugins") || p.starts_with("/v1/settings/") => scope(Admin),

        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::v1::docs::ApiDoc;
    use utoipa::OpenApi;

    /// Every operation in the OpenAPI document — every documented route —
    /// has its scope listed, rather than falling back to `admin`.
    #[test]
    fn every_documented_route_is_listed() {
        let doc = ApiDoc::openapi();
        let mut unlisted = Vec::new();
        for (path, item) in &doc.paths.paths {
            for (method, op) in [
                (Method::GET, &item.get),
                (Method::PUT, &item.put),
                (Method::POST, &item.post),
                (Method::DELETE, &item.delete),
                (Method::PATCH, &item.patch),
            ] {
                if op.is_some() && requirement(&method, path).is_none() {
                    unlisted.push(format!("{method} {path}"));
                }
            }
        }
        assert!(unlisted.is_empty(), "routes with no scope: {unlisted:?}");
    }

    #[test]
    fn unknown_routes_are_unlisted() {
        assert_eq!(requirement(&Method::GET, "/v1/nope"), None);
        assert_eq!(requirement(&Method::POST, "/v1/health"), None);
        assert_eq!(requirement(&Method::PATCH, "/v1/feeds/id/{id}"), None);
    }

    #[test]
    fn head_is_treated_as_get() {
        assert_eq!(
            requirement(&Method::HEAD, "/v1/feeds"),
            Some(Requirement::Scope(Scope::Read))
        );
    }
}
