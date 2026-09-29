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
//! | server | `kiki serve` | data dir + socket dir + SQLite temp dir (rw), system paths (ro) | outbound (asset caching) + listening | [`SandboxProfile::Server`] |
//! | feed fetcher | the server, at startup | TLS trust stores (ro) | outbound TCP only; DNS via the server | [`SandboxProfile::FeedFetcher`] |
//! | script host | the server, at startup | none | none | [`SandboxProfile::ScriptHost`] |
//!
//! [`SandboxProfile::Server`]: crate::sandbox::SandboxProfile::Server
//! [`SandboxProfile::FeedFetcher`]: crate::sandbox::SandboxProfile::FeedFetcher
//! [`SandboxProfile::ScriptHost`]: crate::sandbox::SandboxProfile::ScriptHost
//!
//! The feed fetcher ([`feed_fetcher`]) does every step of a feed refresh
//! that handles untrusted bytes — the HTTP exchange, TLS, decompression,
//! and parsing — and hands the server back plain data. The server keeps
//! the database, scheduling, and script dispatch. The script host
//! ([`script_host`]) runs user-supplied Lua.
//!
//! # Spawning order
//!
//! Every sandbox profile denies `execve`, so a process cannot spawn
//! children once its own sandbox is installed. The server therefore
//! spawns both children *first*, in [`crate::cli::serve`], and only then
//! restricts itself.
//!
//! The two children differ in what happens when they die:
//!
//! * A **script host** that dies cannot be replaced, and scripting stays
//!   disabled until the server is restarted. Since the host's own error
//!   handling keeps script failures — compile errors, runtime errors,
//!   timeouts — inside the child, the ways it can actually die are an OOM
//!   kill, a panic, or a seccomp violation, and refusing to hand a fresh
//!   VM to whatever caused the last one to die is the safer default.
//! * The **feed fetcher** is a supervisor that `fork`s (allowed, unlike
//!   `exec`) a replacement worker whenever the last one dies, because
//!   fetching is Kiki's core job; see [`feed_fetcher`].
//!
//! # Transport
//!
//! Parent and child talk over an anonymous `SOCK_STREAM` socket pair
//! created before the fork, which the child inherits on [`CHILD_FD`].
//! Messages are length-prefixed postcard frames (see [`ipc`]). Neither child
//! can open a Unix socket to connect elsewhere, so its parent is the only
//! local process it can ever talk to.

pub mod ipc;

#[cfg(unix)]
pub mod feed_fetcher;

#[cfg(all(unix, feature = "lua"))]
pub mod script_host;

#[cfg(target_os = "linux")]
pub mod stats;

/// File descriptor each child inherits its end of the socket pair on.
///
/// 0/1/2 are taken by the standard streams, so 3 is the first free slot.
#[cfg(unix)]
pub const CHILD_FD: std::os::unix::io::RawFd = 3;

/// A shared handle to the script host, or `None` when Lua runs in the
/// server process.
///
/// Aliased so that call sites which only pass the handle along do not
/// need to be `cfg`-gated: on platforms or builds without an isolated
/// host the alias degrades to a unit that is always `None`.
#[cfg(all(unix, feature = "lua"))]
pub type ScriptHostHandle = Option<std::sync::Arc<script_host::ScriptHost>>;

/// See the `unix` + `lua` variant of this alias.
#[cfg(not(all(unix, feature = "lua")))]
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
    if unsafe { libc::fcntl(CHILD_FD, libc::F_GETFD) } < 0 {
        anyhow::bail!(
            "no socket on fd {CHILD_FD}: {subcommand} is spawned by `kiki serve`, not run directly"
        );
    }
    // SAFETY: the parent dup2'd its end of the socket pair onto CHILD_FD
    // before exec, the check above confirms it is open, and nothing else
    // in this process has touched it.
    Ok(unsafe { std::os::unix::net::UnixStream::from_raw_fd(CHILD_FD) })
}

/// Re-exec the current binary as `kiki <subcommand>`, with one end of a
/// fresh socket pair on [`CHILD_FD`], and return the other end.
///
/// `fd_env` is set on the child to name the descriptor, and `log_only`
/// and `no_sandbox` are forwarded as `--seccomp-log-only` and
/// `--no-sandbox` so the child's sandbox matches the server's. The child's
/// stdin and stdout are closed; stderr is inherited so its logs land
/// wherever the server's do.
///
/// **Must be called before the caller installs its own sandbox.**
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
    use anyhow::Context;
    use std::io;
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().context("locating the kiki executable")?;
    let (ours, theirs) = UnixStream::pair().context("creating the socket pair")?;

    let mut cmd = Command::new(&exe);
    cmd.arg(subcommand)
        .env(fd_env, CHILD_FD.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    if log_only {
        cmd.arg("--seccomp-log-only");
    }
    if no_sandbox {
        cmd.arg("--no-sandbox");
    }

    let their_fd = theirs.as_raw_fd();
    // SAFETY: the closure runs between fork and exec, where only
    // async-signal-safe calls are permitted. `dup2` and `fcntl` are both
    // on that list, and neither allocates nor takes a lock.
    unsafe {
        cmd.pre_exec(move || {
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

    let child = cmd
        .spawn()
        .with_context(|| format!("spawning `{} {}`", exe.display(), subcommand))?;
    // The child has its own copy now; holding ours open would keep the
    // socket from ever reporting EOF.
    drop(theirs);
    Ok((ours, child))
}
