//! MCP service exposing the v1 REST API as tools.
//!
//! The service holds a clone of the v1 axum router with state already bound,
//! and each `#[tool]` method dispatches into it via
//! [`tower::ServiceExt::oneshot`]. Responses are forwarded as
//! [`CallToolResult::structured`] for JSON bodies, [`Content::text`] for
//! XML/text bodies, and [`McpError`] for any non-2xx status.

use std::sync::Arc;

use axum::{
    body::Body,
    http::{header, Method, Request, StatusCode},
    Router,
};
use http_body_util::BodyExt;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ErrorData as McpError, ServerHandler,
};
use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use crate::routes::v1;
use crate::routes::v1::entries::search_entries::SearchEntriesRequest;
use crate::routes::v1::feeds::add_feed::AddFeedRequest;
use crate::routes::v1::scripts::add_script::AddScriptRequest;
use crate::routes::v1::settings::assets::AssetCacheSettingsRequest;
use crate::routes::v1::settings::retention::RetentionRequest;
use crate::routes::v1::tags::create_tag::CreateTagRequest;
use crate::server::AppState;

use super::params::{
    AssetByUrlArgs, DeleteFeedArgs, HashArgs, IdArgs, IdPaginationArgs, ImportOpmlArgs,
    PaginationArgs, SetEntryTagsArgs, SetFeedTagsArgs, UpdateFeedArgs, UpdateScriptArgs,
    UpdateTagArgs,
};

/// MCP service that proxies tool calls into the v1 REST router.
#[derive(Clone)]
pub struct KikiMcp {
    inner: Router,
    // Read by the `#[tool_handler]` macro expansion via `self.tool_router`,
    // which the dead-code analysis doesn't see as a read.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl KikiMcp {
    /// Build a new service. `state` is bound into a clone of the v1 router so
    /// every tool dispatch lands in the same handlers used by REST clients.
    pub fn new(state: AppState) -> Self {
        Self {
            inner: v1::create_router().with_state(state),
            tool_router: Self::tool_router(),
        }
    }

    /// Send a request through the inner v1 router and collect its response.
    ///
    /// `path` is appended to the `/v1` prefix.
    async fn dispatch_raw(
        &self,
        method: Method,
        path: &str,
        body: Body,
        content_type: Option<&'static str>,
    ) -> Result<(StatusCode, axum::body::Bytes), McpError> {
        let uri = format!("/v1{path}");
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(ct) = content_type {
            builder = builder.header(header::CONTENT_TYPE, ct);
        }
        let request = builder
            .body(body)
            .map_err(|e| McpError::internal_error(format!("invalid request: {e}"), None))?;

        let response = self
            .inner
            .clone()
            .oneshot(request)
            .await
            .map_err(|e| McpError::internal_error(format!("dispatch failed: {e}"), None))?;
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|e| McpError::internal_error(format!("read body failed: {e}"), None))?
            .to_bytes();
        Ok((status, bytes))
    }

    /// JSON dispatch: serializes `body` (when present), parses the response as
    /// `Value`, and wraps it in a structured tool result. Non-2xx responses
    /// surface as `McpError` carrying the upstream status and body.
    async fn dispatch_json<B: Serialize>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<CallToolResult, McpError> {
        let (status, bytes) = match body {
            Some(b) => {
                let payload = serde_json::to_vec(b)
                    .map_err(|e| McpError::internal_error(format!("encode request: {e}"), None))?;
                self.dispatch_raw(method, path, Body::from(payload), Some("application/json"))
                    .await?
            }
            None => self.dispatch_raw(method, path, Body::empty(), None).await?,
        };

        if !status.is_success() {
            let msg = String::from_utf8_lossy(&bytes).into_owned();
            return Err(McpError::invalid_request(
                format!("upstream returned {status}: {msg}"),
                None,
            ));
        }

        if bytes.is_empty() || status == StatusCode::NO_CONTENT {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "ok ({status})"
            ))]));
        }

        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|e| McpError::internal_error(format!("decode response: {e}"), None))?;
        Ok(CallToolResult::structured(value))
    }

    /// Plain text/XML dispatch (used by OPML export).
    async fn dispatch_text(&self, method: Method, path: &str) -> Result<CallToolResult, McpError> {
        let (status, bytes) = self.dispatch_raw(method, path, Body::empty(), None).await?;
        if !status.is_success() {
            let msg = String::from_utf8_lossy(&bytes).into_owned();
            return Err(McpError::invalid_request(
                format!("upstream returned {status}: {msg}"),
                None,
            ));
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// XML body dispatch with a JSON response (used by OPML import).
    async fn dispatch_xml_in_json_out(
        &self,
        method: Method,
        path: &str,
        body: String,
    ) -> Result<CallToolResult, McpError> {
        let (status, bytes) = self
            .dispatch_raw(method, path, Body::from(body), Some("application/xml"))
            .await?;
        if !status.is_success() {
            let msg = String::from_utf8_lossy(&bytes).into_owned();
            return Err(McpError::invalid_request(
                format!("upstream returned {status}: {msg}"),
                None,
            ));
        }
        if bytes.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "ok ({status})"
            ))]));
        }
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|e| McpError::internal_error(format!("decode response: {e}"), None))?;
        Ok(CallToolResult::structured(value))
    }
}

#[tool_router]
impl KikiMcp {
    // ── meta ─────────────────────────────────────────────────────────────

    /// Return the server's API and schema versions.
    #[tool(description = "Return the server's API and schema versions.")]
    async fn root(&self) -> Result<CallToolResult, McpError> {
        self.dispatch_json::<()>(Method::GET, "/", None).await
    }

    /// Report server health, including total feed and entry counts.
    #[tool(description = "Report server health, including total feed and entry counts.")]
    async fn health(&self) -> Result<CallToolResult, McpError> {
        self.dispatch_json::<()>(Method::GET, "/health", None).await
    }

    // ── feeds ────────────────────────────────────────────────────────────

    /// List feeds with pagination. Defaults: offset=0, limit=50.
    #[tool(description = "List feeds with pagination. Defaults: offset=0, limit=50.")]
    async fn list_feeds(
        &self,
        Parameters(args): Parameters<PaginationArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("/feeds{}", build_query(&args));
        self.dispatch_json::<()>(Method::GET, &path, None).await
    }

    /// Register a new feed to fetch content from.
    #[tool(description = "Register a new feed to fetch content from.")]
    async fn add_feed(
        &self,
        Parameters(body): Parameters<AddFeedRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.dispatch_json(Method::POST, "/feeds/create", Some(&body))
            .await
    }

    /// Get a single feed (with format-specific data) by id.
    #[tool(description = "Get a single feed (with format-specific data) by id.")]
    async fn get_feed(
        &self,
        Parameters(args): Parameters<IdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("/feeds/id/{}", args.id);
        self.dispatch_json::<()>(Method::GET, &path, None).await
    }

    /// Update mutable fields on a feed (title, url, description, fetch interval, auth).
    #[tool(
        description = "Update mutable fields on a feed (title, url, description, fetch interval, auth)."
    )]
    async fn update_feed(
        &self,
        Parameters(args): Parameters<UpdateFeedArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("/feeds/id/{}", args.id);
        self.dispatch_json(Method::PUT, &path, Some(&args.body))
            .await
    }

    /// Delete a feed. By default also deletes its entries; set delete_entries=false to keep them.
    #[tool(
        description = "Delete a feed. By default also deletes its entries; set delete_entries=false to keep them."
    )]
    async fn delete_feed(
        &self,
        Parameters(args): Parameters<DeleteFeedArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut path = format!("/feeds/id/{}", args.id);
        if let Some(de) = args.delete_entries {
            path.push_str(&format!("?delete_entries={de}"));
        }
        self.dispatch_json::<()>(Method::DELETE, &path, None).await
    }

    /// Get the tags currently associated with a feed.
    #[tool(description = "Get the tags currently associated with a feed.")]
    async fn get_feed_tags(
        &self,
        Parameters(args): Parameters<IdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("/feeds/id/{}/tags", args.id);
        self.dispatch_json::<()>(Method::GET, &path, None).await
    }

    /// Replace the set of tags associated with a feed.
    #[tool(description = "Replace the set of tags associated with a feed.")]
    async fn set_feed_tags(
        &self,
        Parameters(args): Parameters<SetFeedTagsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("/feeds/id/{}/tags", args.id);
        self.dispatch_json(Method::PUT, &path, Some(&args.body))
            .await
    }

    /// List entries belonging to a feed, with pagination.
    #[tool(description = "List entries belonging to a feed, with pagination.")]
    async fn list_feed_entries(
        &self,
        Parameters(args): Parameters<IdPaginationArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!(
            "/feeds/id/{}/entries{}",
            args.id,
            build_query_offset_limit(args.offset, args.limit)
        );
        self.dispatch_json::<()>(Method::GET, &path, None).await
    }

    /// Queue a refresh for every feed.
    #[tool(description = "Queue a refresh for every feed.")]
    async fn refresh_all_feeds(&self) -> Result<CallToolResult, McpError> {
        self.dispatch_json::<()>(Method::POST, "/feeds/refresh", None)
            .await
    }

    /// Queue a refresh for a single feed by id.
    #[tool(description = "Queue a refresh for a single feed by id.")]
    async fn refresh_feed(
        &self,
        Parameters(args): Parameters<IdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("/feeds/refresh/{}", args.id);
        self.dispatch_json::<()>(Method::POST, &path, None).await
    }

    /// Export all feeds and their tag groupings as an OPML 2.0 document.
    #[tool(description = "Export all feeds and their tag groupings as an OPML 2.0 document.")]
    async fn export_opml(&self) -> Result<CallToolResult, McpError> {
        self.dispatch_text(Method::GET, "/feeds/export").await
    }

    /// Import feeds from an OPML 2.0 document. Folders become tags.
    #[tool(description = "Import feeds from an OPML 2.0 document. Folders become tags.")]
    async fn import_opml(
        &self,
        Parameters(args): Parameters<ImportOpmlArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.dispatch_xml_in_json_out(Method::POST, "/feeds/import", args.opml)
            .await
    }

    // ── entries ──────────────────────────────────────────────────────────

    /// List entries with pagination. Defaults: offset=0, limit=50.
    #[tool(description = "List entries with pagination. Defaults: offset=0, limit=50.")]
    async fn list_entries(
        &self,
        Parameters(args): Parameters<PaginationArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("/entries{}", build_query(&args));
        self.dispatch_json::<()>(Method::GET, &path, None).await
    }

    /// Get a single entry (with format-specific data) by id.
    #[tool(description = "Get a single entry (with format-specific data) by id.")]
    async fn get_entry(
        &self,
        Parameters(args): Parameters<IdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("/entries/id/{}", args.id);
        self.dispatch_json::<()>(Method::GET, &path, None).await
    }

    /// Delete an entry by id.
    #[tool(description = "Delete an entry by id.")]
    async fn delete_entry(
        &self,
        Parameters(args): Parameters<IdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("/entries/id/{}", args.id);
        self.dispatch_json::<()>(Method::DELETE, &path, None).await
    }

    /// Get the tags associated with an entry.
    #[tool(description = "Get the tags associated with an entry.")]
    async fn get_entry_tags(
        &self,
        Parameters(args): Parameters<IdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("/entries/id/{}/tags", args.id);
        self.dispatch_json::<()>(Method::GET, &path, None).await
    }

    /// Replace the set of tags associated with an entry.
    #[tool(description = "Replace the set of tags associated with an entry.")]
    async fn set_entry_tags(
        &self,
        Parameters(args): Parameters<SetEntryTagsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("/entries/id/{}/tags", args.id);
        self.dispatch_json(Method::PUT, &path, Some(&args.body))
            .await
    }

    /// List the cached assets referenced by an entry.
    #[tool(description = "List the cached assets referenced by an entry.")]
    async fn list_entry_assets(
        &self,
        Parameters(args): Parameters<IdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("/entries/id/{}/assets", args.id);
        self.dispatch_json::<()>(Method::GET, &path, None).await
    }

    /// Search entries by tag, date range, GLOB / regex / FTS5 query.
    #[tool(description = "Search entries by tag, date range, GLOB / regex / FTS5 query.")]
    async fn search_entries(
        &self,
        Parameters(body): Parameters<SearchEntriesRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.dispatch_json(Method::POST, "/entries/search", Some(&body))
            .await
    }

    // ── tags ─────────────────────────────────────────────────────────────

    /// List tags with pagination.
    #[tool(description = "List tags with pagination.")]
    async fn list_tags(
        &self,
        Parameters(args): Parameters<PaginationArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("/tags{}", build_query(&args));
        self.dispatch_json::<()>(Method::GET, &path, None).await
    }

    /// Create a new tag with a unique name.
    #[tool(description = "Create a new tag with a unique name.")]
    async fn create_tag(
        &self,
        Parameters(body): Parameters<CreateTagRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.dispatch_json(Method::POST, "/tags/create", Some(&body))
            .await
    }

    /// Get a tag by id.
    #[tool(description = "Get a tag by id.")]
    async fn get_tag(
        &self,
        Parameters(args): Parameters<IdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("/tags/id/{}", args.id);
        self.dispatch_json::<()>(Method::GET, &path, None).await
    }

    /// Rename a tag.
    #[tool(description = "Rename a tag.")]
    async fn update_tag(
        &self,
        Parameters(args): Parameters<UpdateTagArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("/tags/id/{}", args.id);
        self.dispatch_json(Method::PUT, &path, Some(&args.body))
            .await
    }

    /// Delete a tag by id.
    #[tool(description = "Delete a tag by id.")]
    async fn delete_tag(
        &self,
        Parameters(args): Parameters<IdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("/tags/id/{}", args.id);
        self.dispatch_json::<()>(Method::DELETE, &path, None).await
    }

    /// List feeds tagged with the given tag, with pagination.
    #[tool(description = "List feeds tagged with the given tag, with pagination.")]
    async fn list_tag_feeds(
        &self,
        Parameters(args): Parameters<IdPaginationArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!(
            "/tags/id/{}/feeds{}",
            args.id,
            build_query_offset_limit(args.offset, args.limit)
        );
        self.dispatch_json::<()>(Method::GET, &path, None).await
    }

    /// List entries tagged with the given tag, with pagination.
    #[tool(description = "List entries tagged with the given tag, with pagination.")]
    async fn list_tag_entries(
        &self,
        Parameters(args): Parameters<IdPaginationArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!(
            "/tags/id/{}/entries{}",
            args.id,
            build_query_offset_limit(args.offset, args.limit)
        );
        self.dispatch_json::<()>(Method::GET, &path, None).await
    }

    // ── scripts ──────────────────────────────────────────────────────────

    /// List installed scripts (engine, text, kind).
    #[tool(description = "List installed scripts (engine, text, kind).")]
    async fn list_scripts(&self) -> Result<CallToolResult, McpError> {
        self.dispatch_json::<()>(Method::GET, "/scripts", None)
            .await
    }

    /// Add a new script. Calls to this tool queue a script reload in workers.
    #[tool(description = "Add a new script. Calls to this tool queue a script reload in workers.")]
    async fn create_script(
        &self,
        Parameters(body): Parameters<AddScriptRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.dispatch_json(Method::POST, "/scripts/create", Some(&body))
            .await
    }

    /// Get a script by id.
    #[tool(description = "Get a script by id.")]
    async fn get_script(
        &self,
        Parameters(args): Parameters<IdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("/scripts/id/{}", args.id);
        self.dispatch_json::<()>(Method::GET, &path, None).await
    }

    /// Update a script. Queues a script reload in workers.
    #[tool(description = "Update a script. Queues a script reload in workers.")]
    async fn update_script(
        &self,
        Parameters(args): Parameters<UpdateScriptArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("/scripts/id/{}", args.id);
        self.dispatch_json(Method::PUT, &path, Some(&args.body))
            .await
    }

    /// Delete a script by id. Queues a script reload in workers.
    #[tool(description = "Delete a script by id. Queues a script reload in workers.")]
    async fn delete_script(
        &self,
        Parameters(args): Parameters<IdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("/scripts/id/{}", args.id);
        self.dispatch_json::<()>(Method::DELETE, &path, None).await
    }

    /// Reload all scripts in the worker pool.
    #[tool(description = "Reload all scripts in the worker pool.")]
    async fn reload_scripts(&self) -> Result<CallToolResult, McpError> {
        self.dispatch_json::<()>(Method::POST, "/scripts/reload", None)
            .await
    }

    // ── assets ───────────────────────────────────────────────────────────

    /// Look up cached-asset metadata by blake3 hex hash.
    ///
    /// Returns `{ hash, content_type, size, url }`. Use the URL through
    /// the REST endpoint (`GET /v1/assets/{hash}`) to retrieve the raw
    /// bytes — they are not embedded in the tool response.
    #[tool(
        description = "Look up cached-asset metadata by blake3 hex hash. Returns hash, content_type, size, and url; bytes must be fetched via the REST endpoint."
    )]
    async fn get_asset_metadata(
        &self,
        Parameters(args): Parameters<HashArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("/assets/{}", args.hash);
        let (status, bytes) = self
            .dispatch_raw(Method::GET, &path, Body::empty(), None)
            .await?;
        if !status.is_success() {
            let msg = String::from_utf8_lossy(&bytes).into_owned();
            return Err(McpError::invalid_request(
                format!("upstream returned {status}: {msg}"),
                None,
            ));
        }
        let value = serde_json::json!({
            "hash": args.hash,
            "size": bytes.len(),
            "url": format!("/v1/assets/{}", args.hash),
        });
        Ok(CallToolResult::structured(value))
    }

    /// Look up a cached asset by its original (pre-cache) URL. Returns the
    /// hash and the REST URL clients can fetch.
    #[tool(
        description = "Look up a cached asset by its original (pre-cache) URL. Returns the hash and REST URL."
    )]
    async fn get_asset_by_url(
        &self,
        Parameters(args): Parameters<AssetByUrlArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("/assets/by-url?url={}", urlencoding_encode(&args.url));
        let (status, bytes) = self
            .dispatch_raw(Method::GET, &path, Body::empty(), None)
            .await?;
        // The REST endpoint replies 302 redirecting to /v1/assets/{hash}; we
        // surface the hash and target URL instead of following the redirect.
        if status == StatusCode::FOUND {
            return Ok(CallToolResult::structured(serde_json::json!({
                "url": args.url,
            })));
        }
        if !status.is_success() {
            let msg = String::from_utf8_lossy(&bytes).into_owned();
            return Err(McpError::invalid_request(
                format!("upstream returned {status}: {msg}"),
                None,
            ));
        }
        Ok(CallToolResult::success(vec![Content::text(format!(
            "ok ({status})"
        ))]))
    }

    /// Evict a cached asset (database row + on-disk file).
    #[tool(description = "Evict a cached asset (database row + on-disk file).")]
    async fn delete_asset(
        &self,
        Parameters(args): Parameters<HashArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("/assets/{}", args.hash);
        self.dispatch_json::<()>(Method::DELETE, &path, None).await
    }

    // ── settings ─────────────────────────────────────────────────────────

    /// Get the current entry retention policy.
    #[tool(description = "Get the current entry retention policy.")]
    async fn get_retention(&self) -> Result<CallToolResult, McpError> {
        self.dispatch_json::<()>(Method::GET, "/settings/retention", None)
            .await
    }

    /// Update the entry retention policy (max_age_days; null disables).
    #[tool(description = "Update the entry retention policy (max_age_days; null disables).")]
    async fn update_retention(
        &self,
        Parameters(body): Parameters<RetentionRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.dispatch_json(Method::PUT, "/settings/retention", Some(&body))
            .await
    }

    /// Get the asset cache settings (enabled, max_bytes, current_bytes).
    #[tool(description = "Get the asset cache settings (enabled, max_bytes, current_bytes).")]
    async fn get_asset_cache_settings(&self) -> Result<CallToolResult, McpError> {
        self.dispatch_json::<()>(Method::GET, "/settings/asset-cache", None)
            .await
    }

    /// Update the asset cache settings (enabled and/or max_bytes).
    #[tool(description = "Update the asset cache settings (enabled and/or max_bytes).")]
    async fn update_asset_cache_settings(
        &self,
        Parameters(body): Parameters<AssetCacheSettingsRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.dispatch_json(Method::PUT, "/settings/asset-cache", Some(&body))
            .await
    }
}

#[tool_handler]
impl ServerHandler for KikiMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "Kiki RSS aggregator. Tools mirror the v1 REST API: manage feeds, \
                 entries, tags, scripts, assets, and settings."
                    .to_string(),
            )
    }
}

/// Build an axum router serving MCP-over-streamable-HTTP. Mount this under
/// `/mcp` from the main router.
pub fn create_mcp_router(state: AppState, cancel_token: CancellationToken) -> Router {
    let service = StreamableHttpService::new(
        move || Ok(KikiMcp::new(state.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default().with_cancellation_token(cancel_token),
    );
    Router::new().fallback_service(service)
}

fn build_query(args: &PaginationArgs) -> String {
    build_query_offset_limit(args.offset, args.limit)
}

fn build_query_offset_limit(offset: Option<usize>, limit: Option<usize>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(o) = offset {
        parts.push(format!("offset={o}"));
    }
    if let Some(l) = limit {
        parts.push(format!("limit={l}"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}

/// Minimal percent-encoder for query string values. Encodes anything outside
/// the unreserved set (RFC 3986).
fn urlencoding_encode(input: &str) -> String {
    const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.~";
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        if UNRESERVED.contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
