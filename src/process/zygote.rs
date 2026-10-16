//! The zygote: the process that the server's children, and the feed
//! fetcher's, are forked from.
//!
//! Every Kiki process is the same executable, and each process started by
//! running it again (`execve`) relocates its own copy of the executable's
//! data, about a MiB of memory per process that is the same in all of them.
//! A process forked from another shares those pages with it instead. But a
//! process forked from the server would also get a copy of everything the
//! server holds — its database, the secrets in it, what it has decrypted —
//! and the threads it forked from could be holding locks the copy can
//! never release.
//!
//! So the server starts one process the usual way, `kiki __process-zygote`,
//! before it opens anything worth hiding, and has it fork its children for
//! it: the feed fetcher's supervisor and the script host, and the
//! supervisor's worker, parser and resolver. The zygote is a fresh process
//! that holds nothing but what it inherited when it started (the server's
//! environment, as its children did when they were started directly), is
//! sent nothing but requests to start or stop a child, and never starts a
//! thread, so each child it forks is a complete copy of a process that
//! knows nothing.
//!
//! A forked child runs the same function the subcommand it stands for
//! runs — [`crate::process::feed_fetcher::run_worker`] for
//! `__feed-worker`, and so on — with its channel on
//! [`crate::process::CHILD_FD`], as if it had been started by running
//! kiki again. It shows as that subcommand in process listings, too: it
//! writes the subcommand over the zygote's own (see [`SUBCOMMAND`]).
//!
//! # Protocol
//!
//! The server talks to the zygote over a socket pair, as to its other
//! children, and the feed fetcher's supervisor gets a connection of its
//! own, on [`ZYGOTE_FD`], for starting its children. Each request is a
//! length-prefixed postcard frame, as on every other channel (see
//! [`crate::process::ipc`]), and is answered before the next is read:
//!
//! * `Spawn` forks a child of the given [`Role`], and is
//!   answered with its pid, then the requester's end of the child's socket
//!   pair, passed as `SCM_RIGHTS`.
//! * `End` stops a child the same connection started, waiting
//!   a while for it to exit first, and reaps it: only the zygote, its
//!   parent, can. It is answered with the child's wait status.
//!
//! When a connection closes, the zygote stops every child it started; when
//! the server's does, it stops the rest too and exits. Every child also
//! dies with the zygote.
//!
//! The zygote is a Linux-only optimization: everywhere else, and with
//! `kiki serve --no-zygote`, each child is started by running the kiki
//! executable again, as before.

use crate::process::ipc::{decode, encode, read_frame, write_frame};
use crate::process::{feed_fetcher, script_host, CHILD_FD};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// The hidden subcommand the server starts the zygote with.
///
/// As long as the longest subcommand a child stands for, so that a child
/// can write its own over it in the arguments it inherited, and show as
/// that subcommand in process listings and to [`crate::process::stats`].
pub const SUBCOMMAND: &str = "__process-zygote";

/// Environment variable set on the zygote, naming [`CHILD_FD`]; see
/// [`crate::process::script_host::HOST_FD_ENV`].
pub const FD_ENV: &str = "KIKI_ZYGOTE_FD";

/// The descriptor a child that starts children of its own, the feed
/// fetcher's supervisor, finds its connection to the zygote on.
pub const ZYGOTE_FD: RawFd = CHILD_FD + 1;

/// How long a requester waits for the zygote to answer: starting a child
/// takes a `fork`, and stopping one at most the grace it was given.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// What a child forked by the zygote does: which subcommand it stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    /// The feed fetcher's supervisor, `__feed-fetcher`.
    FeedFetcher,
    /// The feed fetcher's worker, `__feed-worker`.
    FeedWorker,
    /// The feed fetcher's parser, `__feed-parser`, with this many threads.
    FeedParser { threads: u32 },
    /// The feed fetcher's resolver, `__feed-resolver`.
    FeedResolver,
    /// The script host, `__script-host`.
    ScriptHost,
}

impl Role {
    /// The subcommand the child stands for.
    pub fn subcommand(self) -> &'static str {
        match self {
            Role::FeedFetcher => feed_fetcher::SUBCOMMAND,
            Role::FeedWorker => feed_fetcher::WORKER_SUBCOMMAND,
            Role::FeedParser { .. } => feed_fetcher::PARSER_SUBCOMMAND,
            Role::FeedResolver => feed_fetcher::RESOLVER_SUBCOMMAND,
            Role::ScriptHost => script_host::SUBCOMMAND,
        }
    }

    /// The function the subcommand runs.
    fn entry(self) -> fn(bool, bool) -> Result<()> {
        match self {
            Role::FeedFetcher => feed_fetcher::run_child,
            Role::FeedWorker => feed_fetcher::run_worker,
            Role::FeedParser { .. } => feed_fetcher::run_parser,
            Role::FeedResolver => feed_fetcher::run_resolver,
            Role::ScriptHost => script_host::run_child,
        }
    }

    /// The environment variables the child keeps, or `None` for all of
    /// them, as when it is started by running kiki again.
    fn kept_environment(self) -> Option<&'static [&'static str]> {
        match self {
            Role::FeedParser { .. } => Some(feed_fetcher::PARSER_ENV),
            Role::FeedResolver => Some(feed_fetcher::RESOLVER_ENV),
            Role::FeedFetcher | Role::FeedWorker | Role::ScriptHost => None,
        }
    }

    /// Whether the child starts children of its own, and so is given a
    /// connection to the zygote.
    fn starts_children(self) -> bool {
        self == Role::FeedFetcher
    }

    /// Whether a connection to the zygote that `requester` was given may
    /// ask for a child of this role: `None` for the server's.
    ///
    /// The server starts the supervisor and the script host, and the
    /// supervisor its worker, parser and resolver, and none of them
    /// anything else: a compromised supervisor cannot have a script host
    /// started for it, for one, the one role allowed writable and
    /// executable memory.
    fn may_be_started_by(self, requester: Option<Role>) -> bool {
        match requester {
            None => matches!(self, Role::FeedFetcher | Role::ScriptHost),
            Some(Role::FeedFetcher) => matches!(
                self,
                Role::FeedWorker | Role::FeedParser { .. } | Role::FeedResolver
            ),
            Some(_) => false,
        }
    }
}

// Every subcommand fits over the zygote's own; see `SUBCOMMAND`.
const _: () = {
    assert!(feed_fetcher::SUBCOMMAND.len() <= SUBCOMMAND.len());
    assert!(feed_fetcher::WORKER_SUBCOMMAND.len() <= SUBCOMMAND.len());
    assert!(feed_fetcher::PARSER_SUBCOMMAND.len() <= SUBCOMMAND.len());
    assert!(feed_fetcher::RESOLVER_SUBCOMMAND.len() <= SUBCOMMAND.len());
    assert!(script_host::SUBCOMMAND.len() <= SUBCOMMAND.len());
};

/// A request to the zygote.
#[derive(Debug, Serialize, Deserialize)]
enum Request {
    /// Fork a child of `role`, whose sandbox is set as `log_only` and
    /// `no_sandbox` say, as the subcommand's flags would.
    Spawn {
        role: Role,
        log_only: bool,
        no_sandbox: bool,
    },
    /// Stop the child `pid`, giving it `grace_ms` to exit first, and reap
    /// it.
    End { pid: u32, grace_ms: u64 },
}

/// The zygote's answer to a [`Request`].
#[derive(Debug, Serialize, Deserialize)]
enum Reply {
    /// The child was forked; the requester's end of its socket pair
    /// follows.
    Spawned { pid: u32 },
    /// The child has exited, with this wait status.
    Ended { status: i32 },
    /// The request could not be carried out.
    Failed { message: String },
}

// ------------------------------------------------------------------
// The requesting side
// ------------------------------------------------------------------

/// A connection to the zygote, which forks children on request.
pub struct Zygote {
    stream: Mutex<UnixStream>,
    /// The zygote process, when this process started it.
    child: Mutex<Option<std::process::Child>>,
}

impl Zygote {
    /// Start the zygote, by running the kiki executable again, and connect
    /// to it.
    ///
    /// As for the server's other children, this must happen after the
    /// server's Landlock rules are in place and before its seccomp filter;
    /// see [`crate::sandbox::restrict_filesystem`].
    ///
    /// # Errors
    ///
    /// Fails if the zygote cannot be started.
    pub fn spawn(log_only: bool, no_sandbox: bool) -> Result<Arc<Self>> {
        let (stream, child) = crate::process::spawn_child(SUBCOMMAND, FD_ENV, log_only, no_sandbox)
            .context("spawning the zygote")?;
        info!(pid = child.id(), "zygote: spawned");
        Ok(Arc::new(Self::over(stream, Some(child))?))
    }

    /// The connection to the zygote this process was given, if it is a
    /// child the zygote gave one; see [`ZYGOTE_FD`].
    ///
    /// # Errors
    ///
    /// Fails if the connection was given but cannot be used.
    pub fn inherited() -> Result<Option<Arc<Self>>> {
        if !INHERITED.swap(false, Ordering::SeqCst) {
            return Ok(None);
        }
        // SAFETY: the zygote put the connection on `ZYGOTE_FD` before
        // running this child's main function, and nothing has taken it
        // since: `INHERITED` was set, and is cleared above.
        let stream = unsafe { UnixStream::from_raw_fd(ZYGOTE_FD) };
        set_cloexec(ZYGOTE_FD)?;
        Ok(Some(Arc::new(Self::over(stream, None)?)))
    }

    fn over(stream: UnixStream, child: Option<std::process::Child>) -> Result<Self> {
        stream.set_read_timeout(Some(REPLY_TIMEOUT))?;
        stream.set_write_timeout(Some(REPLY_TIMEOUT))?;
        Ok(Zygote {
            stream: Mutex::new(stream),
            child: Mutex::new(child),
        })
    }

    /// Have the zygote fork a child of `role`, and return this process's
    /// end of its socket pair, and the child.
    ///
    /// # Errors
    ///
    /// Fails if the zygote cannot be reached or cannot fork.
    pub fn start(
        self: &Arc<Self>,
        role: Role,
        log_only: bool,
        no_sandbox: bool,
    ) -> io::Result<(UnixStream, ZygoteChild)> {
        let mut stream = self.stream.lock().unwrap_or_else(|e| e.into_inner());
        let request = Request::Spawn {
            role,
            log_only,
            no_sandbox,
        };
        match exchange(&mut stream, &request)? {
            Reply::Spawned { pid } => {
                let fd = recv_fd(&stream)?;
                let channel = UnixStream::from(fd);
                Ok((
                    channel,
                    ZygoteChild {
                        pid,
                        zygote: self.clone(),
                        ended: false,
                    },
                ))
            }
            Reply::Failed { message } => Err(io::Error::other(message)),
            other => Err(io::Error::other(format!("unexpected reply {other:?}"))),
        }
    }

    /// Have the zygote stop `pid`, after up to `grace` for it to exit on
    /// its own, and reap it.
    fn end(&self, pid: u32, grace: Duration) -> io::Result<ExitStatus> {
        let mut stream = self.stream.lock().unwrap_or_else(|e| e.into_inner());
        let request = Request::End {
            pid,
            grace_ms: u64::try_from(grace.as_millis()).unwrap_or(u64::MAX),
        };
        match exchange(&mut stream, &request)? {
            Reply::Ended { status } => Ok(ExitStatus::from_raw(status)),
            Reply::Failed { message } => Err(io::Error::other(message)),
            other => Err(io::Error::other(format!("unexpected reply {other:?}"))),
        }
    }
}

impl Drop for Zygote {
    fn drop(&mut self) {
        // Closing the connection is the zygote's signal to stop the
        // children it started for it, and, for the server's, to exit.
        let _ = self
            .stream
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shutdown(std::net::Shutdown::Both);
        if let Some(child) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            crate::process::reap(crate::process::ChildProcess::Exec(child));
        }
    }
}

/// Send `request` and read the answer.
fn exchange(stream: &mut UnixStream, request: &Request) -> io::Result<Reply> {
    let frame = encode(request).map_err(io::Error::other)?;
    write_frame(stream, &frame)?;
    let frame = read_frame(stream)?;
    decode(&frame).map_err(io::Error::other)
}

/// A child the zygote forked for this process. Dropping it stops it.
pub struct ZygoteChild {
    pid: u32,
    zygote: Arc<Zygote>,
    ended: bool,
}

impl ZygoteChild {
    /// The child's pid.
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Stop the child, after up to `grace` for it to exit on its own, and
    /// return how it ended.
    ///
    /// # Errors
    ///
    /// Fails if the zygote cannot be reached; the child then dies with it.
    pub fn end(mut self, grace: Duration) -> io::Result<ExitStatus> {
        self.ended = true;
        self.zygote.end(self.pid, grace)
    }
}

impl Drop for ZygoteChild {
    fn drop(&mut self) {
        if !self.ended {
            let _ = self.zygote.end(self.pid, Duration::ZERO);
        }
    }
}

// ------------------------------------------------------------------
// The zygote
// ------------------------------------------------------------------

/// Whether this process is a child the zygote gave a connection of its
/// own, on [`ZYGOTE_FD`], that [`Zygote::inherited`] has yet to take.
static INHERITED: AtomicBool = AtomicBool::new(false);

/// A connection the zygote serves, and the children it started.
struct Connection {
    stream: UnixStream,
    /// The role of the child the connection was given to, or `None` for
    /// the server's; see [`Role::may_be_started_by`].
    owner: Option<Role>,
    children: Vec<libc::pid_t>,
}

/// Run the zygote, `kiki __process-zygote`: sandbox this process, then
/// fork children on request until the server closes its connection.
///
/// # Errors
///
/// Fails if the sandbox cannot be installed or the server's connection is
/// missing.
pub fn run_zygote(log_only: bool, no_sandbox: bool) -> Result<()> {
    if !no_sandbox {
        crate::sandbox::apply(&crate::sandbox::SandboxConfig::zygote(log_only))
            .context("failed to install the zygote sandbox")?;
    }
    let server = crate::process::take_parent_socket(SUBCOMMAND)?;
    bound_reads(&server)?;
    let mut connections = vec![Connection {
        stream: server,
        owner: None,
        children: Vec::new(),
    }];

    loop {
        let ready = poll_readable(&connections)?;
        // Handled from the last, so that dropping a connection leaves the
        // indices of those not yet handled as they were.
        for index in ready.into_iter().rev() {
            let Some(connection) = connections.get_mut(index) else {
                continue;
            };
            let request = read_frame(&mut connection.stream)
                .ok()
                .and_then(|frame| decode::<Request>(&frame).ok());
            match request {
                Some(request) => {
                    if let Some(new) = serve(connection, request) {
                        connections.push(new);
                    }
                }
                None if index == 0 => {
                    debug!("zygote: server closed the channel, exiting");
                    for connection in &mut connections {
                        end_all(connection);
                    }
                    return Ok(());
                }
                None => {
                    let mut connection = connections.swap_remove(index);
                    end_all(&mut connection);
                }
            }
        }
    }
}

/// Bound how long reading a request from `stream` may take, once it has
/// begun: a connection is only read when it has something to read, so only
/// a frame that stops halfway — sent by a compromised supervisor, say —
/// runs into it, and its connection is then closed rather than holding up
/// the zygote, and the server's requests with it.
fn bound_reads(stream: &UnixStream) -> io::Result<()> {
    stream.set_read_timeout(Some(REPLY_TIMEOUT))
}

/// Wait until one or more of `connections` has something to read, or has
/// closed, and return their indices, in order.
fn poll_readable(connections: &[Connection]) -> io::Result<Vec<usize>> {
    let mut fds: Vec<libc::pollfd> = connections
        .iter()
        .map(|c| libc::pollfd {
            fd: c.stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    loop {
        // SAFETY: `fds` is a valid array of `fds.len()` `pollfd`s.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if n >= 0 {
            break;
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
    Ok(fds
        .iter()
        .enumerate()
        .filter(|(_, fd)| fd.revents != 0)
        .map(|(i, _)| i)
        .collect())
}

/// Carry out `request` from `connection`, and answer it. Returns the
/// connection a child that starts children of its own was given, which
/// the zygote serves from then on.
fn serve(connection: &mut Connection, request: Request) -> Option<Connection> {
    match request {
        Request::Spawn { role, .. } if !role.may_be_started_by(connection.owner) => {
            warn!(?role, requester = ?connection.owner, "zygote: refused to start a child");
            let _ = answer(
                &mut connection.stream,
                &Reply::Failed {
                    message: format!("this connection may not start the {}", role.subcommand()),
                },
            );
            None
        }
        Request::Spawn {
            role,
            log_only,
            no_sandbox,
        } => match spawn(role, log_only, no_sandbox) {
            Ok((pid, channel, given)) => {
                connection.children.push(pid);
                let sent = answer(&mut connection.stream, &Reply::Spawned { pid: pid as u32 })
                    .and_then(|()| send_fd(&connection.stream, channel.as_raw_fd()));
                if let Err(e) = sent {
                    warn!(error = %e, "zygote: could not hand over a child's channel");
                }
                given.and_then(|stream| match bound_reads(&stream) {
                    Ok(()) => Some(Connection {
                        stream,
                        owner: Some(role),
                        children: Vec::new(),
                    }),
                    // The child cannot reach the zygote, so cannot start
                    // children: it fails to, and says so.
                    Err(e) => {
                        warn!(error = %e, "zygote: could not set up a child's connection");
                        None
                    }
                })
            }
            Err(e) => {
                warn!(error = %e, ?role, "zygote: could not start a child");
                let _ = answer(
                    &mut connection.stream,
                    &Reply::Failed {
                        message: format!("starting the {}: {e}", role.subcommand()),
                    },
                );
                None
            }
        },
        Request::End { pid, grace_ms } => {
            let reply = match connection.children.iter().position(|&c| c as u32 == pid) {
                Some(i) => {
                    let child = connection.children.swap_remove(i);
                    Reply::Ended {
                        status: end(child, Duration::from_millis(grace_ms)),
                    }
                }
                None => Reply::Failed {
                    message: format!("{pid} is not a child this connection started"),
                },
            };
            let _ = answer(&mut connection.stream, &reply);
            None
        }
    }
}

fn answer(stream: &mut UnixStream, reply: &Reply) -> io::Result<()> {
    let frame = encode(reply).map_err(io::Error::other)?;
    write_frame(stream, &frame)
}

/// Stop every child `connection` started.
fn end_all(connection: &mut Connection) {
    for pid in connection.children.drain(..) {
        end(pid, Duration::ZERO);
    }
}

/// Stop the child `pid`, after up to `grace` for it to exit on its own,
/// and reap it. Returns its wait status.
fn end(pid: libc::pid_t, grace: Duration) -> i32 {
    let deadline = Instant::now() + grace;
    let mut status: libc::c_int = 0;
    loop {
        // SAFETY: `pid` is this process's child, not yet reaped, and
        // `status` is valid for the call.
        let reaped = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if reaped == pid {
            return status;
        }
        if reaped < 0 || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // SAFETY: as above; killing a child that has just exited is harmless,
    // as it stays a zombie, and its pid unused, until it is reaped.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
        libc::waitpid(pid, &mut status, 0);
    }
    status
}

/// Fork a child of `role`. Returns its pid, the requester's end of its
/// socket pair, and, for a child that starts children of its own, the
/// zygote's end of the connection it was given.
fn spawn(
    role: Role,
    log_only: bool,
    no_sandbox: bool,
) -> io::Result<(libc::pid_t, UnixStream, Option<UnixStream>)> {
    let (ours, theirs) = UnixStream::pair()?;
    let (given_ours, given_theirs) = if role.starts_children() {
        let (a, b) = UnixStream::pair()?;
        (Some(a), Some(b))
    } else {
        (None, None)
    };
    let zygote = std::process::id();
    // SAFETY: the zygote never starts a thread, so the child is a complete
    // copy of a single-threaded process, and may run any code: no lock is
    // held by a thread that did not come with it.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        drop((ours, given_ours));
        run_forked(role, theirs, given_theirs, zygote, log_only, no_sandbox);
    }
    info!(pid, role = role.subcommand(), "zygote: started a child");
    Ok((pid, ours, given_ours))
}

/// Become the child of `role`: set the process up as running the
/// subcommand would, then run what the subcommand runs, and exit.
fn run_forked(
    role: Role,
    channel: UnixStream,
    given: Option<UnixStream>,
    zygote: u32,
    log_only: bool,
    no_sandbox: bool,
) -> ! {
    let code = (|| {
        // Die with the zygote. Checked again after the `prctl`, in case
        // the zygote exited before it took effect.
        // SAFETY: plain `prctl`/`getppid` calls with no pointer arguments.
        unsafe {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            if libc::getppid() as u32 != zygote {
                return 0;
            }
        }
        if let Err(e) = place_descriptors(channel, given) {
            warn!(error = %e, "zygote: could not set up a child's descriptors");
            return 1;
        }
        rename_process(role.subcommand());
        if let Some(keep) = role.kept_environment() {
            scrub_environment(keep);
        }
        if let Role::FeedParser { threads } = role {
            std::env::set_var(feed_fetcher::PARSER_THREADS_ENV, threads.to_string());
        }
        // Nothing the zygote owns may be dropped in the child, so a panic
        // must not unwind past this point.
        match std::panic::catch_unwind(|| (role.entry())(log_only, no_sandbox)) {
            Ok(Ok(())) => 0,
            Ok(Err(e)) => {
                eprintln!("Error: {e:?}");
                1
            }
            Err(_) => 101,
        }
    })();
    std::process::exit(code)
}

/// Put `channel` on [`CHILD_FD`] and `given` on [`ZYGOTE_FD`], as running
/// the subcommand would find them, and close every other descriptor but
/// the standard ones: the zygote's connections among them.
fn place_descriptors(channel: UnixStream, given: Option<UnixStream>) -> io::Result<()> {
    // Moved out of the way first, so that neither lands on the other's
    // slot before it has been moved.
    let channel = dup_above(channel.into_raw_fd())?;
    let given = given.map(|g| dup_above(g.into_raw_fd())).transpose()?;
    dup_to(channel, CHILD_FD)?;
    let first_closed = match given {
        Some(given) => {
            dup_to(given, ZYGOTE_FD)?;
            INHERITED.store(true, Ordering::SeqCst);
            ZYGOTE_FD + 1
        }
        None => CHILD_FD + 1,
    };
    close_from(first_closed)
}

/// Duplicate `fd` to the lowest free descriptor above every one
/// [`place_descriptors`] fills.
fn dup_above(fd: RawFd) -> io::Result<RawFd> {
    // SAFETY: `fcntl(F_DUPFD)` takes no pointers.
    let new = unsafe { libc::fcntl(fd, libc::F_DUPFD, ZYGOTE_FD + 1) };
    if new < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(new)
}

/// Duplicate `fd` onto `to`, which loses close-on-exec.
fn dup_to(fd: RawFd, to: RawFd) -> io::Result<()> {
    // SAFETY: `dup2` takes no pointers.
    if unsafe { libc::dup2(fd, to) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Close every descriptor from `first` up, with `close_range(2)`, or one
/// by one, up to the descriptor limit, on kernels that predate it (5.9).
fn close_from(first: RawFd) -> io::Result<()> {
    let first = libc::c_uint::try_from(first)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "negative descriptor"))?;
    // SAFETY: `close_range` takes no pointers; closing descriptors this
    // process owns is always memory-safe.
    let rc = unsafe { libc::syscall(libc::SYS_close_range, first, libc::c_uint::MAX, 0) };
    if rc == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() != Some(libc::ENOSYS) {
        return Err(err);
    }
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a valid, writable `rlimit`.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let end = libc::c_uint::try_from(limit.rlim_cur).unwrap_or(libc::c_uint::MAX);
    for fd in first..end {
        // SAFETY: as above; descriptors that are not open fail harmlessly.
        unsafe { libc::close(fd as libc::c_int) };
    }
    Ok(())
}

fn set_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fcntl` on a descriptor touches no memory.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    // SAFETY: as above.
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Write `subcommand` over the zygote's own, [`SUBCOMMAND`], in the
/// arguments the process was started with, so that it shows as running
/// it: in `/proc/<pid>/cmdline`, and so to `ps`, `systemd-cgls` and
/// [`crate::process::stats`]. The rest of [`SUBCOMMAND`]'s room is
/// filled with NULs, which show as empty arguments.
///
/// Leaves the arguments alone if the second is not [`SUBCOMMAND`].
fn rename_process(subcommand: &str) {
    extern "C" {
        // `argv[0]`, as the C library saw it: both glibc and musl keep it.
        static mut program_invocation_name: *mut libc::c_char;
    }
    // SAFETY: the process is single-threaded, so nothing reads the
    // arguments meanwhile. The kernel lays the arguments out one after
    // another, each NUL-terminated, so `argv[1]` follows `argv[0]`'s NUL;
    // it is only written if it reads as `SUBCOMMAND`, and only up to, not
    // including, its own NUL.
    unsafe {
        let argv0 = program_invocation_name;
        if argv0.is_null() {
            return;
        }
        let argv1 = argv0.add(libc::strlen(argv0) + 1);
        let len = libc::strlen(argv1);
        if std::slice::from_raw_parts(argv1.cast::<u8>(), len) != SUBCOMMAND.as_bytes()
            || subcommand.len() > len
        {
            return;
        }
        std::ptr::write_bytes(argv1, 0, len);
        std::ptr::copy_nonoverlapping(subcommand.as_ptr(), argv1.cast::<u8>(), subcommand.len());
    }
}

/// Overwrite every environment variable the process inherited with
/// zeroes, in place, but for those named in `keep`.
///
/// The environment is the server's, and may hold credentials — a proxy
/// URL with a password in `HTTPS_PROXY`, say — that the worker needs and
/// the parser and resolver must not have. Unsetting the variables would
/// leave their text where it was; this wipes it. The variables wiped read
/// as absent afterwards.
///
/// Must be called while the process is single-threaded.
fn scrub_environment(keep: &[&str]) {
    extern "C" {
        static mut environ: *mut *mut libc::c_char;
    }
    // SAFETY: the process is single-threaded, so nothing reads or changes
    // the environment meanwhile; `environ` is the C library's
    // NULL-terminated array of NUL-terminated strings, each of which is
    // overwritten only up to, and not including, its NUL.
    unsafe {
        let mut var = environ;
        while !var.is_null() && !(*var).is_null() {
            let text = *var;
            let len = libc::strlen(text);
            let bytes = std::slice::from_raw_parts(text.cast::<u8>(), len);
            let name = bytes.split(|&b| b == b'=').next().unwrap_or_default();
            if !keep.iter().any(|k| k.as_bytes() == name) {
                std::ptr::write_bytes(text, 0, len);
            }
            var = var.add(1);
        }
    }
}

/// Send `fd` over `stream`, as `SCM_RIGHTS` on a single byte.
fn send_fd(stream: &UnixStream, fd: RawFd) -> io::Result<()> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    // SAFETY: `CMSG_SPACE` only computes a size.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    // SAFETY: `msghdr` is plain data, for which all zeroes is valid.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: `msg` points at `control`, which has room for one header and
    // one descriptor, so the first header is there and its data is in
    // bounds; `iov` and `control` outlive the `sendmsg` call.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&msg);
        if header.is_null() {
            return Err(io::Error::other("no room for the control message"));
        }
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<RawFd>(), fd);
        if libc::sendmsg(stream.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Receive a descriptor sent with [`send_fd`], close-on-exec.
fn recv_fd(stream: &UnixStream) -> io::Result<OwnedFd> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    // SAFETY: `CMSG_SPACE` only computes a size.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    // SAFETY: `msghdr` is plain data, for which all zeroes is valid.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: `iov` and `control` outlive the call, and are as long as
    // `msg` says.
    let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    // SAFETY: the kernel filled in `msg`'s control buffer and length;
    // `CMSG_FIRSTHDR` checks the length before handing out a header, and
    // its data holds one descriptor when it is `SCM_RIGHTS` of that
    // length. The descriptor is this process's now, and owned once.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&msg);
        if header.is_null()
            || (*header).cmsg_level != libc::SOL_SOCKET
            || (*header).cmsg_type != libc::SCM_RIGHTS
            || (*header).cmsg_len as usize
                != libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as usize
        {
            return Err(io::Error::other("no descriptor came with the reply"));
        }
        let fd = std::ptr::read_unaligned(libc::CMSG_DATA(header).cast::<RawFd>());
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

#[cfg(test)]
#[path = "zygote_tests.rs"]
mod tests;
