pub mod assets;
mod backoff;
mod cache;
mod command;
mod entry_assets;
mod error;
mod error_recording;
mod fetch;
mod maintenance;
mod parsing;
mod processing;
mod scripting;
mod worker;

pub use command::TaskManagerCommand;
pub use error::FetchError;
#[cfg(feature = "lua")]
pub use scripting::{reload_script_runner, run_script_reloader};
pub use worker::{spawn_workers, worker_count};

// Re-exported for use from tests (which reach them via `crate::tasks::*`).
// The #[allow] keeps the `cargo build` / clippy on the lib target green —
// non-test code inside the crate does not consume these symbols through
// this path (it imports them directly from the submodules instead).
#[allow(unused_imports)]
pub(crate) use entry_assets::cache_entry_assets;
#[allow(unused_imports)]
pub(crate) use fetch::refresh_feed;
#[allow(unused_imports)]
pub(crate) use maintenance::run_maintenance;

#[cfg(test)]
mod tests;
