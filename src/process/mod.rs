//! Kiki's multi-process model.
//!
//! A single process needs the union of everything Kiki does: read and
//! write the SQLite database, write into the asset cache, bind a
//! listening socket, and make outbound connections to arbitrary feed
//! servers. A sandbox applied to that process can be no tighter than
//! that union, which puts a floor under how much an in-process sandbox
//! can buy — see [`crate::sandbox`].
//!
//! Splitting the work across processes lifts that floor: each process
//! gets a policy scoped to its own job, so compromising one does not
//! hand an attacker the privileges of the others.
//!
//! # Processes
//!
//! | Process | Started by | Filesystem | Network | Sandbox profile |
//! |---|---|---|---|---|
//! | server | `kiki serve` | data dir + socket dir + SQLite temp dir (rw), system paths (ro) | listening on Unix sockets only | [`SandboxProfile::Server`] |
//! | feed fetcher's supervisor | the server, at startup | TLS trust stores, resolver configuration (ro) | none of its own | [`SandboxProfile::FeedFetcher`] |
//! | ↳ worker | the supervisor | TLS trust stores (ro) | outbound TCP only; DNS via the resolver | [`SandboxProfile::FeedWorker`] |
//! | ↳ parser | the supervisor | none | none | [`SandboxProfile::FeedParser`] |
//! | ↳ resolver | the supervisor | resolver configuration (ro) | DNS: UDP, TCP to port 53 only | [`SandboxProfile::FeedResolver`] |
//! | script host | the server, at startup | none | none | [`SandboxProfile::ScriptHost`] |
//!
//! [`SandboxProfile::Server`]: crate::sandbox::SandboxProfile::Server
//! [`SandboxProfile::FeedFetcher`]: crate::sandbox::SandboxProfile::FeedFetcher
//! [`SandboxProfile::FeedWorker`]: crate::sandbox::SandboxProfile::FeedWorker
//! [`SandboxProfile::FeedParser`]: crate::sandbox::SandboxProfile::FeedParser
//! [`SandboxProfile::FeedResolver`]: crate::sandbox::SandboxProfile::FeedResolver
//! [`SandboxProfile::ScriptHost`]: crate::sandbox::SandboxProfile::ScriptHost
//!
//! The feed fetcher ([`feed_fetcher`]) does every step of a feed refresh
//! that handles untrusted bytes — the HTTP exchange, TLS, decompression,
//! parsing, and name resolution — and hands the server back plain data.
//! It downloads assets and favicons the same way, so the server makes no
//! network connections at all. Within it, the worker downloads, the parser
//! parses, and the resolver looks hostnames up, each in a process of its
//! own. The server keeps the database, scheduling, and script dispatch.
//! The script host ([`script_host`]) runs plugins: user-supplied
//! WebAssembly, compiled to native code.
//!
//! # Spawning order
//!
//! The server's sandbox profile denies `execve`, so it cannot spawn
//! children once its sandbox is installed. It therefore
//! installs its sandbox in two halves, in [`crate::cli::serve`]: its
//! Landlock rules, then both children, then — once it has bound its API
//! socket, which the filter leaves it no way to do — its seccomp filter.
//! Starting the children under the server's Landlock rules nests their
//! Landlock domains inside its own, which is what lets it read their
//! memory use from `/proc` (see [`stats`]); see
//! [`crate::sandbox::restrict_filesystem`].
//!
//! The two children differ in what happens when they die:
//!
//! * A **script host** that dies cannot be replaced, and scripting stays
//!   disabled until the server is restarted. Since the host's own error
//!   handling keeps plugin failures — compile errors, traps, timeouts —
//!   inside the child, the ways it can actually die are an OOM
//!   kill, a panic, or a seccomp violation, and refusing to hand a fresh
//!   host to whatever caused the last one to die is the safer default.
//! * The **feed fetcher** is a supervisor that starts a replacement worker
//!   whenever the last one dies, because fetching is Kiki's core job: its
//!   profile is the one that may `execve`, and then only the kiki
//!   executable; see [`feed_fetcher`]. Should the
//!   supervisor itself go, the server stops with an error, so that
//!   whatever supervises it (systemd, say) can start both afresh.
//!
//! # Transport
//!
//! Parent and child talk over an anonymous `SOCK_STREAM` socket pair
//! created before the child is started, which it inherits on
//! [`CHILD_FD`]. Every child is the kiki executable run again with a hidden
//! subcommand, and inherits no other descriptor but the standard streams.
//! Messages are length-prefixed postcard frames (see [`ipc`]). Neither child
//! can open a Unix socket to connect elsewhere, so its parent is the only
//! local process it can ever talk to.

#[cfg(unix)]
use anyhow::Context;

pub mod ipc;

#[cfg(unix)]
pub mod feed_fetcher;

#[cfg(unix)]
pub mod script_host;

#[cfg(target_os = "linux")]
pub mod stats;

/// File descriptor each child inherits its end of the socket pair on.
///
/// 0/1/2 are taken by the standard streams, so 3 is the first free slot.
#[cfg(unix)]
pub const CHILD_FD: std::os::unix::io::RawFd = 3;

/// A shared handle to the script host, or `None` when plugins run in the
/// server process.
///
/// Aliased so that call sites which only pass the handle along do not
/// need to be `cfg`-gated: on platforms without an isolated
/// host the alias degrades to a unit that is always `None`.
#[cfg(unix)]
pub type ScriptHostHandle = Option<std::sync::Arc<script_host::ScriptHost>>;

/// See the `unix` variant of this alias.
#[cfg(not(unix))]
pub type ScriptHostHandle = Option<()>;

/// A shared handle to the feed fetcher, or `None` when feeds are fetched
/// in the server process.
///
/// Aliased for the same reason as [`ScriptHostHandle`].
#[cfg(unix)]
pub type FeedFetcherHandle = Option<std::sync::Arc<feed_fetcher::FeedFetcherHost>>;

/// See the `unix` variant of this alias.
#[cfg(not(unix))]
pub type FeedFetcherHandle = Option<()>;

/// How long a parent waits for a child to exit after closing its channel,
/// before killing it.
#[cfg(unix)]
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

/// Wait up to [`SHUTDOWN_GRACE`] for `child` to exit on its own — callers
/// close its channel first, which is its signal to go — then kill it.
#[cfg(unix)]
pub(crate) fn reap(mut child: std::process::Child) {
    let deadline = std::time::Instant::now() + SHUTDOWN_GRACE;
    while std::time::Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// In a child started by [`spawn_child`], take ownership of the socket to
/// the parent on [`CHILD_FD`].
///
/// The descriptor is marked close-on-exec, so that a child this process
/// starts in turn does not inherit it.
///
/// # Errors
///
/// Fails if nothing is open on [`CHILD_FD`] — the subcommand was run by
/// hand rather than spawned by `kiki serve`.
#[cfg(unix)]
pub(crate) fn take_parent_socket(
    subcommand: &str,
) -> anyhow::Result<std::os::unix::net::UnixStream> {
    use std::os::unix::io::FromRawFd;

    // Taking ownership of a descriptor that isn't open is an IO-safety
    // violation, and Rust aborts the process on the eventual drop rather
    // than returning an error. Check first so a bad invocation exits with
    // a diagnostic instead.
    // SAFETY: `fcntl(F_GETFD)` only inspects the descriptor table entry.
    let flags = unsafe { libc::fcntl(CHILD_FD, libc::F_GETFD) };
    if flags < 0 {
        anyhow::bail!(
            "no socket on fd {CHILD_FD}: {subcommand} is spawned by `kiki serve`, not run directly"
        );
    }
    // SAFETY: as above; setting a descriptor flag touches no memory.
    if unsafe { libc::fcntl(CHILD_FD, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error())
            .context("marking the parent socket close-on-exec");
    }
    // SAFETY: the parent dup2'd its end of the socket pair onto CHILD_FD
    // before exec, the check above confirms it is open, and nothing else
    // in this process has touched it.
    Ok(unsafe { std::os::unix::net::UnixStream::from_raw_fd(CHILD_FD) })
}

/// Which of its parent's environment variables a child gets.
#[cfg(unix)]
pub(crate) enum ChildEnv<'a> {
    /// All of them.
    Inherit,
    /// Only those named, if set: the rest may hold secrets, a proxy
    /// password among them, that the child has no use for.
    Only(&'a [&'a str]),
}

/// A command that runs `exe` as `kiki <subcommand>`, with `fd_env` set to
/// name [`CHILD_FD`], and `log_only` and `no_sandbox` forwarded as
/// `--seccomp-log-only` and `--no-sandbox` so the child's sandbox matches
/// its parent's. Start it with [`spawn_with_socket`].
#[cfg(unix)]
pub(crate) fn child_command(
    exe: &std::path::Path,
    subcommand: &str,
    fd_env: &str,
    env: ChildEnv<'_>,
    log_only: bool,
    no_sandbox: bool,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(exe);
    if let ChildEnv::Only(keep) = env {
        cmd.env_clear();
        for name in keep {
            if let Some(value) = std::env::var_os(name) {
                cmd.env(name, value);
            }
        }
    }
    cmd.arg(subcommand).env(fd_env, CHILD_FD.to_string());
    if log_only {
        cmd.arg("--seccomp-log-only");
    }
    if no_sandbox {
        cmd.arg("--no-sandbox");
    }
    cmd
}

/// Start `cmd` with one end of a fresh socket pair on [`CHILD_FD`], and
/// return the other end.
///
/// With `die_with_parent`, the child is killed (on Linux) when the
/// thread that started it exits, and does not start at all if that has
/// already happened by the time it would.
///
/// # Errors
///
/// Fails if the socket pair cannot be created or the child cannot be
/// started.
#[cfg(unix)]
pub(crate) fn spawn_with_socket(
    cmd: &mut std::process::Command,
    die_with_parent: bool,
) -> std::io::Result<(std::os::unix::net::UnixStream, std::process::Child)> {
    use std::io;
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt;

    let (ours, theirs) = UnixStream::pair()?;
    let their_fd = theirs.as_raw_fd();
    let parent = std::process::id();
    // SAFETY: the closure runs between fork and exec, where only
    // async-signal-safe calls are permitted. `dup2`, `fcntl`, `prctl` and
    // `getppid` are plain syscalls, and none of them allocates or takes a
    // lock.
    unsafe {
        cmd.pre_exec(move || {
            #[cfg(target_os = "linux")]
            if die_with_parent {
                // Checked after the `prctl`, in case the parent exited
                // before it took effect.
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() as u32 != parent {
                    return Err(io::Error::other("the parent has exited"));
                }
            }
            #[cfg(not(target_os = "linux"))]
            let _ = (die_with_parent, parent);
            if their_fd == CHILD_FD {
                // Already in the right slot; just clear CLOEXEC so it
                // survives the exec.
                let flags = libc::fcntl(CHILD_FD, libc::F_GETFD);
                if flags < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::fcntl(CHILD_FD, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(io::Error::last_os_error());
                }
            } else if libc::dup2(their_fd, CHILD_FD) < 0 {
                // `dup2` clears CLOEXEC on the new descriptor for us.
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd.spawn()?;
    // The child has its own copy now; holding ours open would keep the
    // socket from ever reporting EOF.
    drop(theirs);
    Ok((ours, child))
}

/// Re-exec the current binary as `kiki <subcommand>`, with one end of a
/// fresh socket pair on [`CHILD_FD`], and return the other end.
///
/// See [`child_command`] for `fd_env`, `log_only` and `no_sandbox`. The
/// child's stdin and stdout are closed; stderr is inherited so its logs
/// land wherever the server's do.
///
/// **Must be called before the caller installs its seccomp filter**,
/// which denies `execve`. Calling it after the caller's Landlock rules
/// are in place nests the child's Landlock domain inside the caller's;
/// see [`crate::sandbox::restrict_filesystem`].
///
/// # Errors
///
/// Fails if the current executable cannot be located, the socket pair
/// cannot be created, or the child cannot be spawned.
#[cfg(unix)]
pub(crate) fn spawn_child(
    subcommand: &str,
    fd_env: &str,
    log_only: bool,
    no_sandbox: bool,
) -> anyhow::Result<(std::os::unix::net::UnixStream, std::process::Child)> {
    use std::process::Stdio;

    let exe = std::env::current_exe().context("locating the kiki executable")?;
    let mut cmd = child_command(
        &exe,
        subcommand,
        fd_env,
        ChildEnv::Inherit,
        log_only,
        no_sandbox,
    );
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    spawn_with_socket(&mut cmd, false)
        .with_context(|| format!("spawning `{} {}`", exe.display(), subcommand))
}
