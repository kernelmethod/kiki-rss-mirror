//! OS-level sandboxing for Kiki's processes.
//!
//! Kiki runs as more than one process (see [`crate::process`]). Each has a
//! different job, so each gets its own policy rather than one policy wide
//! enough for the union of everything Kiki does. A profile is chosen with
//! [`SandboxProfile`] and applied by [`apply`].
//!
//! On Linux three layers are installed, per profile:
//!
//! * **Landlock** restricts filesystem access to the paths the profile
//!   actually needs — for the server that is the data directory holding
//!   the SQLite DB and cached assets, the Unix socket's parent directory,
//!   SQLite's temp directory (normally inside the data directory), and
//!   time zone data. The feed fetcher, which makes all of Kiki's HTTP(S)
//!   requests, gets only the TLS trust stores, its resolver only the
//!   resolver's configuration, and the feed fetcher's parser, the script
//!   host and the web UI get *nothing at all*. Where the kernel
//!   supports it (Linux 6.12+), every profile is also barred from
//!   reaching abstract Unix sockets outside its own sandbox, and every
//!   profile but the web UI from signalling processes outside it — its
//!   own children, whose sandboxes nest inside its own, excepted. The
//!   feed fetcher is barred from binding TCP ports, its resolver from
//!   binding them or connecting to any but DNS's, and its parser and the
//!   web UI from binding or connecting to them.
//! * **seccomp-bpf** allows only the syscalls the profile uses, and kills
//!   the process on any other — so `execve`, `ptrace`, `mount`,
//!   `unshare`, `bpf`, module loading, `io_uring`, `userfaultfd` and the
//!   rest of the kernel's surface are out of reach without being named.
//!   Every profile gets what the runtime needs for memory, threads,
//!   signals, time and the descriptors it holds; on top of that, the
//!   server may change files, make Unix sockets, and bind, listen and
//!   accept on them; the feed fetcher's worker may make and connect IPv4
//!   and IPv6 sockets, its resolver bind them too, and its supervisor
//!   whatever the three of those may, and fork and sandbox them; the web
//!   UI may accept on its listener and make and connect Unix sockets; and
//!   the script host and the feed fetcher's parser may make no socket at
//!   all. A few allowed calls are narrowed by their arguments: `clone`
//!   may not create namespaces (`clone3`, whose flags seccomp cannot see,
//!   fails with `ENOSYS`, and the C library falls back to `clone`),
//!   `ioctl` is limited to a handful of harmless requests, and `socket`
//!   to the address families above.
//! * **`PR_SET_MDWE`** (Linux 6.3+) makes the kernel refuse memory that is
//!   writable and executable, and refuse making any mapping executable
//!   that was not already, so injected code cannot be written and then
//!   run. Every profile gets it; it is the in-process counterpart of
//!   systemd's `MemoryDenyWriteExecute=`.
//!
//! All three are installed before the process touches untrusted
//! input — for the server, before it opens its listening socket(s); for
//! the children, before they read their first byte of IPC; for the web
//! UI, before it accepts its first connection. They are
//! inherited by every thread and task spawned later, and by the feed
//! fetcher's forked worker, parser and resolver, each of which adds a
//! stricter set of its own on top. The server installs them one at a time, with
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
    /// cache, and the listening socket. It makes no network connections of
    /// its own — no HTTP(S) requests and no DNS lookups — may create only
    /// Unix sockets, and may signal no process but itself and its
    /// children.
    ///
    /// Its children are started under its filesystem rules (see
    /// [`restrict_filesystem`]), so besides the paths below it is granted
    /// what they need: to read and execute the kiki executable and the
    /// libraries it loads, to open `/dev/null`, and to read the TLS trust
    /// stores and the resolver's configuration.
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
    /// This profile grants **no filesystem access whatsoever**, denies
    /// every syscall that could open a socket, and may signal no process
    /// but itself. The host's inherited IPC
    /// file descriptor already exists by the time the sandbox is applied,
    /// and is used through plain `read`/`write`.
    ScriptHost,

    /// The feed fetcher's supervisor: talks to the server over an
    /// inherited socket pair, and forks the processes that do the
    /// fetcher's work, each of which installs a profile of its own on top
    /// of this one — [`Self::FeedWorker`], [`Self::FeedParser`] and
    /// [`Self::FeedResolver`].
    ///
    /// A process forked from it can have no more than it has, so this
    /// profile holds what its children need between them, and no more:
    /// read-only access to the TLS trust stores and the resolver's
    /// configuration, and nothing else — no data directory, no `/proc`.
    /// It may make outbound connections, and bind UDP sockets, which the
    /// resolver needs, but may not bind TCP ports, listen for or accept
    /// connections, or create a Unix socket: the last keeps the fetcher's
    /// processes away from the server's API socket, whose only access
    /// control is reachability.
    FeedFetcher,

    /// The feed fetcher's worker: retrieves feeds over HTTP(S), downloads
    /// their assets and favicons, and hands what it downloads to the
    /// parser, all through the supervisor.
    ///
    /// Forked from the supervisor, it installs this profile on top of
    /// [`Self::FeedFetcher`], which takes away what only the resolver
    /// needs: it may read the TLS trust stores and nothing else, and may
    /// not bind any socket.
    FeedWorker,

    /// The feed fetcher's parser: parses what the fetcher downloads —
    /// feeds, web pages, SVG images, entries' HTML — and talks to the
    /// fetcher's supervisor over a socket pair it inherited, nothing else.
    ///
    /// Forked from the feed fetcher's supervisor, it starts out under the
    /// [`Self::FeedFetcher`] profile and installs this one on top, which
    /// takes away what parsing does not need: like the script host's, it
    /// grants **no filesystem access whatsoever**, denies every syscall
    /// that could open a socket, and bars TCP binds and connections — so
    /// a parser compromised by a hostile feed can reach neither the
    /// network nor any file.
    FeedParser,

    /// The feed fetcher's resolver: looks up hostnames for the worker with
    /// the C library's resolver, and talks to the fetcher's supervisor over
    /// a socket pair it inherited.
    ///
    /// Forked from the supervisor, it installs this profile on top of
    /// [`Self::FeedFetcher`]. It may read the resolver's configuration
    /// files and nothing else, create only IPv4 and IPv6 sockets, and make
    /// TCP connections only to port 53. With no Unix sockets it cannot
    /// reach the server's API socket — nor a local resolver daemon, so
    /// lookups go to the name servers in `resolv.conf`.
    FeedResolver,

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

    /// Configuration for the feed fetcher's worker process.
    pub fn feed_worker(log_only: bool) -> Self {
        SandboxConfig {
            profile: SandboxProfile::FeedWorker,
            log_only,
        }
    }

    /// Configuration for the feed fetcher's parser process.
    pub fn feed_parser(log_only: bool) -> Self {
        SandboxConfig {
            profile: SandboxProfile::FeedParser,
            log_only,
        }
    }

    /// Configuration for the feed fetcher's resolver process.
    pub fn feed_resolver(log_only: bool) -> Self {
        SandboxConfig {
            profile: SandboxProfile::FeedResolver,
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
            SandboxProfile::FeedWorker => "feed-worker",
            SandboxProfile::FeedParser => "feed-parser",
            SandboxProfile::FeedResolver => "feed-resolver",
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
/// the Landlock rules, which also scope signals and abstract Unix
/// sockets. [`restrict_syscalls`] installs the other half.
///
/// The server installs the two halves separately so that it can start its
/// children in between. Landlock lets a process inspect — through
/// `/proc/<pid>/smaps_rollup`, say — and, once scoped, signal only
/// processes in its own Landlock domain or one nested inside it, and a
/// child's domain nests inside its
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
/// seccomp-bpf filter and the refusal of writable and executable memory,
/// both of which apply to every thread of the process. See
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
