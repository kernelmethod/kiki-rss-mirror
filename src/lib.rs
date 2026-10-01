// Route handlers return `Result<Response, Response>` throughout, which is
// the idiomatic axum shape but trips `result_large_err`: `Response` is 128
// bytes, over the lint's default threshold. Boxing it, as the lint
// suggests, would obscure every handler signature to no benefit.
#![allow(clippy::result_large_err)]
#![deny(clippy::panic)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::indexing_slicing)]
// Tests build their own reqwest clients to talk to local test servers;
// everything else must go through `fetcher::ProxiedClient` (see
// `clippy.toml`).
#![cfg_attr(test, allow(clippy::disallowed_methods))]

pub mod config;
pub mod db;
pub mod docs;
pub mod fetcher;
pub mod http;
pub mod metrics;
pub mod notify;
pub mod opml;
pub mod plugins;
pub mod process;
pub mod routes;
pub mod sandbox;
pub mod server;
pub mod tasks;

pub mod scripting;

#[cfg(feature = "cli")]
pub mod cli;

#[cfg(test)]
pub mod test;
