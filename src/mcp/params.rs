//! Parameter structs for MCP tool calls.
//!
//! Tools that take only a body re-use the existing request types from
//! `crate::routes::v1::*` directly. Tools that need a path id, query
//! parameter, or path-id + body combination use the wrapper structs
//! defined here so the macro-generated JSON schema sees a single flat
//! object.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::routes::v1::entries::entry_tags::SetEntryTagsRequest;
use crate::routes::v1::feeds::feed_tags::SetFeedTagsRequest;
use crate::routes::v1::feeds::update_feed::UpdateFeedRequest;
use crate::routes::v1::scripts::update_script::UpdateScriptRequest;
use crate::routes::v1::tags::update_tag::UpdateTagRequest;

/// Pagination parameters used by list-style tools.
#[derive(Default, Deserialize, Serialize, JsonSchema)]
pub struct PaginationArgs {
    /// Number of records to skip (default: 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
    /// Maximum number of records to return (default: 50).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// Single integer id parameter.
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct IdArgs {
    pub id: i64,
}

/// Path id plus pagination.
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct IdPaginationArgs {
    pub id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct DeleteFeedArgs {
    pub id: i64,
    /// When true (default), entries from this feed are also deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_entries: Option<bool>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct UpdateFeedArgs {
    pub id: i64,
    #[serde(flatten)]
    pub body: UpdateFeedRequest,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct SetFeedTagsArgs {
    pub id: i64,
    #[serde(flatten)]
    pub body: SetFeedTagsRequest,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct SetEntryTagsArgs {
    pub id: i64,
    #[serde(flatten)]
    pub body: SetEntryTagsRequest,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct UpdateTagArgs {
    pub id: i64,
    #[serde(flatten)]
    pub body: UpdateTagRequest,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct UpdateScriptArgs {
    pub id: i64,
    #[serde(flatten)]
    pub body: UpdateScriptRequest,
}

/// Raw OPML XML payload for the import tool.
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct ImportOpmlArgs {
    /// OPML 2.0 XML document.
    pub opml: String,
}

/// 64-character blake3 hex hash of an asset.
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct HashArgs {
    pub hash: String,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct AssetByUrlArgs {
    /// Original asset URL (pre-cache).
    pub url: String,
}
