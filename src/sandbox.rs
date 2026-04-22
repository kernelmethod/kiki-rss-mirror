//! OS-level sandboxing for the Kiki server.
//!
//! On Linux, applies two layered restrictions to the running process before
//! it starts accepting untrusted input:
//!
//! * **Landlock** restricts filesystem access to a handful of directories
//!   (the data directory holding the SQLite DB and cached assets, the Unix
//!   socket's parent directory, and a small read-only set of system paths
//!   needed for DNS and TLS trust stores).
//! * **seccomp-bpf** blocks a denylist of syscalls Kiki never uses at
//!   runtime (`ptrace`, `mount`, `unshare`, `bpf`, `kexec_load`, module
//!   loading, and friends). The default action for unmatched syscalls is
//!   `Allow` — this is a defence-in-depth layer that eliminates the most
//!   dangerous escape primitives without risking that a benign syscall we
//!   forgot about will kill the process.
//!
//! Both restrictions are installed before the server opens its listening
//! socket(s); they are inherited by every thread and task spawned later.
//!
//! On non-Linux platforms [`apply`] is a no-op that logs a warning.

use std::path::PathBuf;

/// Sandbox configuration derived from CLI flags and server paths.
pub struct SandboxConfig {
    /// Directory containing the SQLite database, its WAL/SHM companions,
    /// and the cached assets tree. Granted read-write access.
    pub data_dir: PathBuf,

    /// Parent directory of the Unix domain socket, if the server is
    /// listening on a UDS. Granted read-write access so the socket file
    /// can be created and unlinked.
    pub socket_dir: Option<PathBuf>,

    /// If `true`, seccomp violations are logged instead of killing the
    /// process. Useful when tightening the filter or diagnosing an
    /// unexpected denial in production. Has no effect on Landlock, which
    /// has no equivalent mode.
    pub log_only: bool,
}

#[cfg(target_os = "linux")]
mod linux;

/// Apply the configured sandbox to the current process.
///
/// Must be called before any untrusted input is accepted. On Linux this
/// installs Landlock filesystem rules and a seccomp-bpf syscall filter
/// that are inherited by every thread spawned after the call returns.
pub fn apply(config: &SandboxConfig) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::apply(config)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = config;
        tracing::warn!("sandbox: not supported on this platform, continuing unsandboxed");
        Ok(())
    }
}
