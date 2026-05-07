//! HTTP MCP server exposing the v1 REST API as MCP tools.
//!
//! When the `mcp` feature is enabled, [`create_mcp_router`] returns an axum
//! router serving an MCP-over-streamable-HTTP endpoint. Each tool is a thin
//! wrapper that builds an internal `http::Request` and dispatches it through
//! the same v1 router used by the REST API via [`tower::ServiceExt::oneshot`],
//! so REST and MCP cannot drift apart.

mod params;
mod service;

pub use service::{create_mcp_router, KikiMcp};
