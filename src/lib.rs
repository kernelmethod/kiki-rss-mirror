#![deny(clippy::panic)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::indexing_slicing)]

pub mod db;
pub mod docs;
pub mod http;
pub mod metrics;
pub mod routes;
pub mod sandbox;
pub mod server;
pub mod tasks;

pub mod scripting;

#[cfg(feature = "cli")]
pub mod cli;

#[cfg(test)]
pub mod test;
