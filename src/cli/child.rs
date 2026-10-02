//! Arguments for Kiki's hidden child-process subcommands,
//! `kiki __feed-fetcher` and `kiki __script-host`.
//!
//! Not part of Kiki's user-facing interface: `kiki serve` re-execs itself
//! with these subcommands to create the sandboxed children described in
//! [`crate::process`]. They are useless when run by hand, because they
//! expect an already-connected socket on
//! [`CHILD_FD`](crate::process::CHILD_FD).

use anyhow::{bail, Result};
use clap::Args;

#[derive(Args)]
pub struct ChildArgs {
    /// Run the seccomp filter in log-only mode. Forwarded by `kiki serve`
    /// so the child matches the server's setting.
    #[arg(long)]
    seccomp_log_only: bool,

    /// Disable the child's sandbox. Forwarded by `kiki serve`.
    #[arg(long)]
    no_sandbox: bool,
}

impl ChildArgs {
    /// Run a child's main loop, `run_child`, after checking that this
    /// process really was spawned by `kiki serve`.
    ///
    /// # Errors
    ///
    /// Fails if `fd_env` is unset — a bare invocation would otherwise
    /// treat whatever happens to be on fd 3 (often nothing, sometimes the
    /// caller's terminal) as the server — or if `run_child` fails.
    pub fn run(
        &self,
        subcommand: &str,
        fd_env: &str,
        run_child: fn(bool, bool) -> Result<()>,
    ) -> Result<()> {
        // To stderr, the one standard stream `kiki serve` leaves its
        // children, so their logs land wherever its own do.
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .init();

        if std::env::var_os(fd_env).is_none() {
            bail!(
                "{subcommand} is an internal subcommand used by `kiki serve`; it cannot be run \
                 directly"
            );
        }

        run_child(self.seccomp_log_only, self.no_sandbox)
    }
}
