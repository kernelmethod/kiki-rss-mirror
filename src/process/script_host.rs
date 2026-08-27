//! The Lua script host process, and the server-side client that drives it.
//!
//! User-supplied Lua is the one place where Kiki executes code it did not
//! ship, and — since [#58] moved TLS onto rustls — the Lua VM is also the
//! largest remaining body of C in the server. Running it in the same
//! address space as the SQLite handle and the asset cache means a VM
//! escape starts out holding everything Kiki has.
//!
//! [#58]: https://github.com/kernelmethod/kiki-rss/pull/58
//!
//! This module moves the VM into a child process that holds nothing: no
//! database, no filesystem (Landlock with an empty ruleset), and no way
//! to open a socket (seccomp). Its entire view of the world is one
//! inherited socket pair and whatever the server chooses to send down it.
//!
//! # Roles
//!
//! * [`ScriptHost`] and [`SubprocessScriptRunner`] run in the server.
//!   The runner implements [`ScriptRunner`], so callers dispatch events
//!   exactly as they did against the in-process VM.
//! * [`run_child`] is the child's entire main loop, reached through the
//!   hidden `kiki __script-host` subcommand.
//!
//! # Failure behaviour
//!
//! Script-level failures — a compile error, a runtime error, a timeout —
//! are handled inside the child and reported as
//! [`HostResponse::Failed`]; they do not disturb the connection. A
//! failure of the *channel* (the child died, or stopped answering within
//! [`IPC_TIMEOUT`]) marks the host permanently dead, and scripting stays
//! disabled until the server restarts, because the server can no longer
//! spawn anything once its own sandbox is installed.
//!
//! Either way an `entry.ingest` dispatch that fails returns the entry
//! **unmodified** rather than dropping it, which is the contract the
//! in-process runner already documents.

use crate::process::ipc::{read_frame, write_frame, HostRequest, HostResponse, MAX_FRAME_BYTES};
use crate::scripting::{Event, EventPayload, FeedEntry, ScriptRunner};
use anyhow::{bail, Context, Result};
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, info, warn};

/// The hidden subcommand the server re-execs itself with.
pub const SUBCOMMAND: &str = "__script-host";

/// File descriptor the child inherits its end of the socket pair on.
///
/// 0/1/2 are taken by the standard streams, so 3 is the first free slot.
pub const HOST_FD: RawFd = 3;

/// Environment variable set on the child, naming [`HOST_FD`].
///
/// Purely informational: it makes `kiki __script-host` legible in a
/// process listing and lets the child produce a clear diagnostic when
/// someone runs it by hand instead of failing obscurely on a read.
pub const HOST_FD_ENV: &str = "KIKI_SCRIPT_HOST_FD";

/// How long the server waits for a response before declaring the host
/// dead.
///
/// Generous next to the child's own per-handler budget
/// ([`SCRIPT_TIMEOUT_MS`]) so that a slow chain of handlers, or a reload
/// compiling many scripts, is never mistaken for a hung child — but
/// short enough that a genuinely wedged host cannot pin a feed worker
/// indefinitely.
///
/// [`SCRIPT_TIMEOUT_MS`]: crate::scripting::lua::SCRIPT_TIMEOUT_MS
pub const IPC_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the server waits for the child to exit on shutdown before
/// killing it.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

/// Why a request to the script host could not be served.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    /// The channel failed earlier and the host has been retired.
    #[error("script host is no longer running")]
    Dead,

    /// The channel failed on this request. The host is retired as a
    /// result.
    #[error("script host IPC failed: {0}")]
    Io(#[from] io::Error),

    /// The host answered, but with something that could not be decoded
    /// or did not match the request. Also fatal to the channel.
    #[error("script host sent a malformed response: {0}")]
    Protocol(String),

    /// The host answered normally to say the request could not be
    /// served, or the server declined to send the request at all. The
    /// channel is still healthy either way.
    #[error("{0}")]
    Failed(String),
}

impl HostError {
    /// Whether this failure means the channel can no longer be trusted.
    ///
    /// A request we never put on the wire — one that would not encode,
    /// or that exceeds [`MAX_FRAME_BYTES`] — leaves the stream perfectly
    /// in sync, so it must not cost the operator their script host.
    ///
    /// [`MAX_FRAME_BYTES`]: crate::process::ipc::MAX_FRAME_BYTES
    fn is_fatal(&self) -> bool {
        match self {
            HostError::Io(_) | HostError::Protocol(_) => true,
            HostError::Dead | HostError::Failed(_) => false,
        }
    }
}

/// The live half of a [`ScriptHost`]: the child and the socket to it.
struct Live {
    stream: UnixStream,
    child: Child,
}

/// A handle to the script host child process.
///
/// Cheap to share behind an [`Arc`]; all requests serialise on an
/// internal mutex, since the protocol is a single request/response
/// stream.
///
/// # Blocking
///
/// [`Self::request`] blocks the calling thread. Dispatch already happens
/// from synchronous code interleaved with SQLite writes, so this is not
/// a new kind of stall — but the worst case is bounded differently:
/// a wedged child costs one caller up to [`IPC_TIMEOUT`], *once*. That
/// caller retires the host while still holding the mutex, so everyone
/// queued behind it finds a dead host and returns immediately rather
/// than each waiting out its own timeout.
pub struct ScriptHost {
    state: Mutex<Option<Live>>,
}

impl ScriptHost {
    /// Spawn the script host child.
    ///
    /// **Must be called before the caller installs its own sandbox** —
    /// every sandbox profile denies `execve`. `log_only` and `no_sandbox`
    /// are forwarded so the child's sandbox matches the operator's intent
    /// for the server's.
    ///
    /// # Errors
    ///
    /// Fails if the current executable cannot be located, the socket pair
    /// cannot be created, or the child cannot be spawned.
    pub fn spawn(log_only: bool, no_sandbox: bool) -> Result<Self> {
        let exe = std::env::current_exe().context("locating the kiki executable")?;
        let (ours, theirs) = UnixStream::pair().context("creating the script host socket pair")?;

        ours.set_read_timeout(Some(IPC_TIMEOUT))
            .context("setting the script host read timeout")?;
        ours.set_write_timeout(Some(IPC_TIMEOUT))
            .context("setting the script host write timeout")?;

        let mut cmd = Command::new(&exe);
        cmd.arg(SUBCOMMAND)
            .env(HOST_FD_ENV, HOST_FD.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            // Keep stderr so the child's tracing output lands wherever
            // the server's does.
            .stderr(Stdio::inherit());
        if log_only {
            cmd.arg("--seccomp-log-only");
        }
        if no_sandbox {
            cmd.arg("--no-sandbox");
        }

        let their_fd = theirs.as_raw_fd();
        // SAFETY: the closure runs between fork and exec, where only
        // async-signal-safe calls are permitted. `dup2` and `fcntl` are
        // both on that list, and neither allocates nor takes a lock.
        unsafe {
            cmd.pre_exec(move || {
                if their_fd == HOST_FD {
                    // Already in the right slot; just clear CLOEXEC so it
                    // survives the exec.
                    let flags = libc::fcntl(HOST_FD, libc::F_GETFD);
                    if flags < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::fcntl(HOST_FD, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                } else if libc::dup2(their_fd, HOST_FD) < 0 {
                    // `dup2` clears CLOEXEC on the new descriptor for us.
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let child = cmd.spawn().context("spawning the script host process")?;
        // The child has its own copy now; holding ours open would keep
        // the socket from ever reporting EOF.
        drop(theirs);

        info!(pid = child.id(), "script host: spawned");
        Ok(ScriptHost {
            state: Mutex::new(Some(Live {
                stream: ours,
                child,
            })),
        })
    }

    /// Send `request` and wait for the matching response.
    ///
    /// Any channel-level failure retires the host: the child is killed
    /// and every subsequent call returns [`HostError::Dead`].
    pub fn request(&self, request: &HostRequest) -> Result<HostResponse, HostError> {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let live = match guard.as_mut() {
            Some(live) => live,
            None => return Err(HostError::Dead),
        };

        let result = exchange(&mut live.stream, request);
        match result {
            Ok(HostResponse::Failed { message }) => Err(HostError::Failed(message)),
            Ok(response) => Ok(response),
            Err(e) if e.is_fatal() => {
                // The channel is unusable; retire the host so we stop
                // paying an IPC timeout on every subsequent entry. Done
                // while still holding the lock, so callers queued behind
                // this one find a dead host rather than each waiting out
                // a timeout of their own.
                warn!(
                    error = %e,
                    "script host: channel failed, disabling scripting until restart"
                );
                if let Some(mut live) = guard.take() {
                    let _ = live.child.kill();
                    let _ = live.child.wait();
                }
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    /// Rebuild the child's VM from `sources`, replacing whatever it was
    /// running.
    ///
    /// Returns the number of scripts the child compiled.
    pub fn reload(&self, sources: Vec<String>) -> Result<usize, HostError> {
        match self.request(&HostRequest::Reload { sources })? {
            HostResponse::Reloaded { loaded } => Ok(loaded),
            other => Err(HostError::Protocol(format!(
                "expected Reloaded, got {other:?}"
            ))),
        }
    }

    /// Whether the channel to the child is still usable.
    pub fn is_alive(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }
}

impl Drop for ScriptHost {
    fn drop(&mut self) {
        let live = self.state.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(mut live) = live {
            // Closing our end is the child's shutdown signal: its next
            // read returns EOF and it exits.
            drop(live.stream);

            let deadline = std::time::Instant::now() + SHUTDOWN_GRACE;
            loop {
                match live.child.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            let _ = live.child.kill();
            let _ = live.child.wait();
        }
    }
}

/// Write one request and read one response.
///
/// Failures before the first byte goes out are [`HostError::Failed`], not
/// [`HostError::Io`] — see [`HostError::is_fatal`].
fn exchange(stream: &mut UnixStream, request: &HostRequest) -> Result<HostResponse, HostError> {
    let encoded = serde_json::to_vec(request)
        .map_err(|e| HostError::Failed(format!("could not encode request: {e}")))?;
    if encoded.len() > MAX_FRAME_BYTES {
        return Err(HostError::Failed(format!(
            "request of {} bytes exceeds the {} byte frame limit",
            encoded.len(),
            MAX_FRAME_BYTES
        )));
    }
    write_frame(stream, &encoded)?;
    let frame = read_frame(stream)?;
    serde_json::from_slice(&frame)
        .map_err(|e| HostError::Protocol(format!("decoding response: {e}")))
}

/// A [`ScriptRunner`] that forwards every dispatch to the script host
/// child.
///
/// Drop-in for [`crate::scripting::lua::LuaScriptRunner`]: same trait,
/// same failure semantics, different address space.
pub struct SubprocessScriptRunner {
    host: Arc<ScriptHost>,
}

impl SubprocessScriptRunner {
    /// Wrap an already-spawned, already-loaded host.
    pub fn new(host: Arc<ScriptHost>) -> Self {
        SubprocessScriptRunner { host }
    }
}

impl ScriptRunner for SubprocessScriptRunner {
    fn dispatch_transform_entry(&self, entry: FeedEntry) -> Result<Option<FeedEntry>> {
        let request = HostRequest::TransformEntry {
            entry: entry.clone(),
        };
        match self.host.request(&request) {
            Ok(HostResponse::Entry { entry }) => Ok(entry),
            Ok(other) => {
                warn!("script host: expected an Entry response, got {other:?}");
                Ok(Some(entry))
            }
            Err(e) => {
                // Never drop an entry because the scripting layer broke.
                warn!(error = %e, "script host: entry.ingest dispatch failed; passing the entry through unmodified");
                Ok(Some(entry))
            }
        }
    }

    fn dispatch_observe(&self, event: Event, payload: EventPayload) {
        let request = HostRequest::Observe { event, payload };
        if let Err(e) = self.host.request(&request) {
            warn!(event = event.name(), error = %e, "script host: observe dispatch failed");
        }
    }
}

// ------------------------------------------------------------------
// Child side
// ------------------------------------------------------------------

/// Run the script host child: sandbox itself, then serve requests from
/// [`HOST_FD`] until the server closes the channel.
///
/// This is the whole of the child's life. It never returns to any other
/// code path.
///
/// # Errors
///
/// Fails if the sandbox cannot be installed. Once the loop is running,
/// a broken channel is a normal shutdown, not an error.
pub fn run_child(log_only: bool, no_sandbox: bool) -> Result<()> {
    // Warm the C library's time zone cache before the door shuts.
    // `os.date` in a script goes through `localtime(3)`, which reads
    // /etc/localtime on its first call; the sandbox is about to make
    // that path unreachable, so make that first call now. libc caches
    // the parsed zone, and scripts keep seeing local time.
    //
    // SAFETY: `localtime` has no preconditions beyond a valid pointer to
    // a `time_t`, and the process is still single-threaded here, so its
    // use of a static return buffer is not a race. The result is
    // deliberately discarded — only the caching side effect matters.
    unsafe {
        let epoch: libc::time_t = 0;
        let _ = libc::localtime(&epoch);
    }

    if no_sandbox {
        warn!(
            "script host: sandbox disabled via --no-sandbox; the Lua VM runs with full \
             filesystem and syscall access"
        );
    } else {
        crate::sandbox::apply(&crate::sandbox::SandboxConfig::script_host(log_only))
            .context("failed to install the script host sandbox")?;
    }

    // Taking ownership of a descriptor that isn't open is an IO-safety
    // violation, and Rust aborts the process on the eventual drop rather
    // than returning an error. Check first so a bad invocation exits with
    // a diagnostic instead.
    // SAFETY: `fcntl(F_GETFD)` only inspects the descriptor table entry.
    if unsafe { libc::fcntl(HOST_FD, libc::F_GETFD) } < 0 {
        bail!(
            "no socket on fd {}: {} is spawned by `kiki serve`, not run directly",
            HOST_FD,
            SUBCOMMAND
        );
    }

    // SAFETY: the parent dup2'd its end of the socket pair onto HOST_FD
    // before exec, the check above confirms it is open, and nothing else
    // in this process has touched it.
    let mut stream = unsafe { UnixStream::from_raw_fd(HOST_FD) };

    let mut runner: Option<crate::scripting::lua::LuaScriptRunner> = None;

    loop {
        let frame = match read_frame(&mut stream) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                debug!("script host: server closed the channel, exiting");
                return Ok(());
            }
            Err(e) => {
                warn!(error = %e, "script host: read failed, exiting");
                return Ok(());
            }
        };

        let response = match serde_json::from_slice::<HostRequest>(&frame) {
            Ok(request) => serve(&mut runner, request),
            Err(e) => HostResponse::Failed {
                message: format!("undecodable request: {e}"),
            },
        };

        let encoded = match serde_json::to_vec(&response) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "script host: could not encode a response, exiting");
                return Ok(());
            }
        };
        if let Err(e) = write_frame(&mut stream, &encoded) {
            warn!(error = %e, "script host: write failed, exiting");
            return Ok(());
        }
    }
}

/// Handle a single request against the child's current VM.
fn serve(
    runner: &mut Option<crate::scripting::lua::LuaScriptRunner>,
    request: HostRequest,
) -> HostResponse {
    match request {
        HostRequest::Reload { sources } => {
            let count = sources.len();
            match crate::scripting::lua::LuaScriptRunner::new(&sources) {
                Ok(new_runner) => {
                    *runner = Some(new_runner);
                    HostResponse::Reloaded { loaded: count }
                }
                Err(e) => {
                    // Drop the old VM too: continuing to run superseded
                    // scripts would be more surprising than running none.
                    *runner = None;
                    HostResponse::Failed {
                        message: format!("{e}"),
                    }
                }
            }
        }

        HostRequest::TransformEntry { entry } => match runner.as_ref() {
            None => HostResponse::Entry { entry: Some(entry) },
            Some(r) => match r.dispatch_transform_entry(entry) {
                Ok(entry) => HostResponse::Entry { entry },
                Err(e) => HostResponse::Failed {
                    message: format!("{e}"),
                },
            },
        },

        HostRequest::Observe { event, payload } => {
            if let Some(r) = runner.as_ref() {
                r.dispatch_observe(event, payload);
            }
            HostResponse::Ack
        }
    }
}
