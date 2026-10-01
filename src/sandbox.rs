//! OS-level sandboxing for Kiki's processes.
//!
//! Kiki runs as more than one process (see [`crate::process`]). Each has a
//! different job, so each gets its own policy rather than one policy wide
//! enough for the union of everything Kiki does. A profile is chosen with
//! [`SandboxProfile`] and applied by [`apply`].
//!
//! On Linux two layers are installed, per profile:
//!
//! * **Landlock** restricts filesystem access to the paths the profile
//!   actually needs — for the server that is the data directory holding
//!   the SQLite DB and cached assets, the Unix socket's parent directory,
//!   SQLite's temp directory (normally inside the data directory), and a
//!   small read-only set of system paths needed for DNS. The feed fetcher,
//!   which makes all of Kiki's HTTP(S) requests, gets only the TLS trust
//!   stores (it has the server resolve hostnames for it), and the script
//!   host and the web UI get *nothing at all*. Where the kernel
//!   supports it, the feed fetcher is also barred from binding TCP ports
//!   and from reaching abstract Unix sockets or signalling processes
//!   outside its own sandbox, and the web UI from binding or connecting
//!   to TCP ports and from reaching abstract Unix sockets.
//! * **seccomp-bpf** blocks a denylist of syscalls the profile never uses
//!   (`ptrace`, `mount`, `unshare`, `bpf`, `kexec_load`, module loading,
//!   `io_uring`, `userfaultfd`, and friends; plus, for the server,
//!   creating any socket but a Unix, IPv4 or IPv6 one; for the script
//!   host, every socket call; for the feed fetcher, binding, listening,
//!   accepting, and creating Unix sockets; and for the web UI, binding,
//!   listening, and creating any socket but a Unix one).
//!   The default action for unmatched syscalls is `Allow` — this is a
//!   defence-in-depth layer that eliminates the most dangerous escape
//!   primitives without risking that a benign syscall we forgot about
//!   will kill the process.
//!
//! Both restrictions are installed before the process touches untrusted
//! input — for the server, before it opens its listening socket(s); for
//! the children, before they read their first byte of IPC; for the web
//! UI, before it accepts its first connection. They are
//! inherited by every thread and task spawned later, and by the feed
//! fetcher's forked workers. The server installs them one at a time, with
//! its children started in between; see [`restrict_filesystem`].
//!
//! On non-Linux platforms [`apply`] is a no-op that logs a warning.

use std::path::PathBuf;

/// Which of Kiki's processes a [`SandboxConfig`] describes.
///
/// Each variant carries only the paths its process legitimately needs, so
/// adding a profile is the way to add a process — there is no "default"
/// set of privileges to inherit by accident.
pub enum SandboxProfile {
    /// The main `kiki serve` process: owns the SQLite database, the asset
    /// cache, and the listening socket, and resolves hostnames for the
    /// feed fetcher. It makes no HTTP(S) requests of its own, and may
    /// create only Unix, IPv4 and IPv6 sockets.
    ///
    /// Its children are started under its filesystem rules (see
    /// [`restrict_filesystem`]), so besides the paths below it is granted
    /// what they need: to read and execute the kiki executable and the
    /// libraries it loads, to open `/dev/null`, and to read the TLS trust
    /// stores.
    Server {
        /// Directory containing the SQLite database, its WAL/SHM
        /// companions, and the cached assets tree. Granted read-write
        /// access.
        data_dir: PathBuf,

        /// Parent directory of the Unix domain socket. Granted read-write
        /// access so the socket file can be created and unlinked.
        socket_dir: PathBuf,

        /// Directory SQLite puts its temporary files in (statement
        /// journals, and sorts, indexes and tables too big for the page
        /// cache). Granted read-write access. Normally inside `data_dir`,
        /// which already covers it.
        temp_dir: PathBuf,
    },

    /// The Lua script host: evaluates user-supplied scripts and talks to
    /// the server over an inherited socket pair, nothing else.
    ///
    /// This profile grants **no filesystem access whatsoever** and denies
    /// every syscall that could open a socket. The host's inherited IPC
    /// file descriptor already exists by the time the sandbox is applied,
    /// and is used through plain `read`/`write`.
    ScriptHost,

    /// The feed fetcher: retrieves feeds over HTTP(S) and parses them,
    /// downloads their assets and favicons, and talks to the server over an
    /// inherited socket pair.
    ///
    /// This profile grants read-only access to the TLS trust stores and
    /// nothing else — no data directory, no resolver configuration, no
    /// `/proc`; the server resolves hostnames on its behalf. It may make
    /// outbound TCP connections, but may not bind, listen for or accept
    /// them, or create a Unix socket: the last keeps it away from the
    /// server's API socket, whose only access control is reachability.
    FeedFetcher,

    /// The `kiki web` UI: accepts browser connections on a TCP listener it
    /// bound before the sandbox went up, and turns each request into calls
    /// to the Kiki API over the server's Unix socket.
    ///
    /// This profile grants **no filesystem access whatsoever**: every page
    /// and cached asset it serves comes from the API. It may accept
    /// connections on its existing listener and connect to Unix sockets,
    /// but may not bind or listen on a new socket, create any socket that
    /// is not a Unix one, or make TCP connections — so a compromised web
    /// UI can reach the Kiki API and nothing else on the network. It may
    /// still signal the `kiki serve` child it started, to stop it.
    WebUi,
}

/// Sandbox configuration derived from CLI flags and the process's role.
pub struct SandboxConfig {
    /// The process this configuration applies to.
    pub profile: SandboxProfile,

    /// If `true`, seccomp violations are logged instead of killing the
    /// process. Useful when tightening the filter or diagnosing an
    /// unexpected denial in production. Has no effect on Landlock, which
    /// has no equivalent mode.
    pub log_only: bool,
}

impl SandboxConfig {
    /// Configuration for the main server process.
    pub fn server(
        data_dir: PathBuf,
        socket_dir: PathBuf,
        temp_dir: PathBuf,
        log_only: bool,
    ) -> Self {
        SandboxConfig {
            profile: SandboxProfile::Server {
                data_dir,
                socket_dir,
                temp_dir,
            },
            log_only,
        }
    }

    /// Configuration for the Lua script host process.
    pub fn script_host(log_only: bool) -> Self {
        SandboxConfig {
            profile: SandboxProfile::ScriptHost,
            log_only,
        }
    }

    /// Configuration for the feed fetcher process.
    pub fn feed_fetcher(log_only: bool) -> Self {
        SandboxConfig {
            profile: SandboxProfile::FeedFetcher,
            log_only,
        }
    }

    /// Configuration for the `kiki web` UI process.
    pub fn web_ui(log_only: bool) -> Self {
        SandboxConfig {
            profile: SandboxProfile::WebUi,
            log_only,
        }
    }

    /// A short name for the profile, used in log messages.
    pub fn profile_name(&self) -> &'static str {
        match self.profile {
            SandboxProfile::Server { .. } => "server",
            SandboxProfile::ScriptHost => "script-host",
            SandboxProfile::FeedFetcher => "feed-fetcher",
            SandboxProfile::WebUi => "web-ui",
        }
    }
}

#[cfg(target_os = "linux")]
mod linux;

/// Apply the configured sandbox to the current process.
///
/// Must be called before any untrusted input is accepted. On Linux this
/// installs Landlock filesystem rules and a seccomp-bpf syscall filter
/// that are inherited by every thread spawned after the call returns.
///
/// Landlock restricts only the calling thread and those it spawns later,
/// so call this while the process is still single-threaded — before
/// building a multi-threaded tokio runtime, for instance.
///
/// Note that every profile denies `execve`, so a process must spawn any
/// children it needs *before* calling this — or call
/// [`restrict_filesystem`], spawn them, then [`restrict_syscalls`].
pub fn apply(config: &SandboxConfig) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::apply(config)
    }
    #[cfg(not(target_os = "linux"))]
    {
        tracing::warn!(
            profile = config.profile_name(),
            "sandbox: not supported on this platform, continuing unsandboxed"
        );
        Ok(())
    }
}

/// Install only the filesystem half of the configured sandbox: on Linux,
/// the Landlock rules. [`restrict_syscalls`] installs the other half.
///
/// The server installs the two halves separately so that it can start its
/// children in between. Landlock lets a process inspect — through
/// `/proc/<pid>/smaps_rollup`, say — only processes in its own Landlock
/// domain or one nested inside it, and a child's domain nests inside its
/// parent's only if the parent's rules were in place when the child was
/// started. Children started before the server restricted itself would end
/// up in domains of their own, beyond its reach. So the server restricts
/// its filesystem access first, which leaves `execve` allowed, starts its
/// children, and only then installs the syscall filter that denies it. Its
/// [`SandboxProfile::Server`] rules include what the children need to
/// start up under them.
///
/// The same threading rule as for [`apply`] holds: call this while the
/// process is still single-threaded.
pub fn restrict_filesystem(config: &SandboxConfig) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::restrict_filesystem(config)
    }
    #[cfg(not(target_os = "linux"))]
    {
        tracing::warn!(
            profile = config.profile_name(),
            "sandbox: not supported on this platform, continuing unsandboxed"
        );
        Ok(())
    }
}

/// Install only the syscall half of the configured sandbox: on Linux, the
/// seccomp-bpf filter, which applies to every thread of the process. See
/// [`restrict_filesystem`].
pub fn restrict_syscalls(config: &SandboxConfig) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::restrict_syscalls(config)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = config;
        Ok(())
    }
}
