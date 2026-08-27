//! The hidden `kiki __script-host` subcommand.
//!
//! Not part of Kiki's user-facing interface: `kiki serve` re-execs itself
//! with this subcommand to create the sandboxed Lua host described in
//! [`crate::process::script_host`]. It is useless when run by hand,
//! because it expects an already-connected socket on
//! [`HOST_FD`](crate::process::script_host::HOST_FD).

use crate::process::script_host;
use anyhow::{bail, Result};
use clap::Args;

#[derive(Args)]
pub struct ScriptHostArgs {
    /// Run the seccomp filter in log-only mode. Forwarded by `kiki serve`
    /// so the child matches the server's setting.
    #[arg(long)]
    seccomp_log_only: bool,

    /// Disable the child's sandbox. Forwarded by `kiki serve`.
    #[arg(long)]
    no_sandbox: bool,
}

impl ScriptHostArgs {
    pub fn run(&self) -> Result<()> {
        tracing_subscriber::fmt::init();

        // A bare `kiki __script-host` inherits whatever happens to be on
        // fd 3 — often nothing, sometimes the caller's terminal. Refuse
        // rather than blocking on a read that will never be answered.
        if std::env::var_os(script_host::HOST_FD_ENV).is_none() {
            bail!(
                "{} is an internal subcommand used by `kiki serve`; it cannot be run directly",
                script_host::SUBCOMMAND
            );
        }

        script_host::run_child(self.seccomp_log_only, self.no_sandbox)
    }
}
