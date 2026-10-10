//! The script host process, which runs plugins, and the server-side client
//! that drives it.
//!
//! Plugins are the one place where Kiki executes code it did not ship. They
//! are WebAssembly components, compiled to native code with Cranelift, so
//! the process that runs them is the one Kiki process allowed memory that
//! is first writable and then executable (see [`crate::sandbox`]), and the
//! compiler is one more body of code that handles what plugins supply.
//! Running them in the same address space as the SQLite handle and the
//! asset cache would mean a sandbox escape starts out holding everything
//! Kiki has.
//!
//! This module moves them into a child process that holds nothing: no
//! database, no filesystem (Landlock with an empty ruleset, and seccomp
//! refusing every syscall that takes a path), and no way to open a socket
//! (seccomp). Its entire view of the world is one
//! inherited socket pair and whatever the server chooses to send down it.
//! What plugins ask of the server (the store, stored entries, feeds) goes
//! back up that socket as a [`HostMessage::Call`], and
//! the server decides how to answer: the child never touches the database
//! itself.
//!
//! # Roles
//!
//! * [`ScriptHost`] and [`SubprocessScriptRunner`] run in the server.
//!   The runner implements [`ScriptRunner`], so callers dispatch events
//!   exactly as they would against an in-process runner. Every response says
//!   which events the child's plugins have handlers for, and the runner
//!   sends nothing for the rest: an event no handler would see costs no
//!   round trip.
//! * [`run_child`] is the child's entire main loop, reached through the
//!   hidden `kiki __script-host` subcommand.
//!
//! # Concurrency
//!
//! Many requests are in flight at once (see [`crate::process::ipc`]): each
//! server thread that dispatches an event sends its request and waits for
//! the answer with that request's id. In the child, a pool of [`WORKERS`]
//! threads serves them in the [`WasmScriptRunner`], where each plugin has a
//! lock of its own: one plugin's handlers run one call at a time, as they
//! always have, while different plugins run at the same time. A feed
//! refresh's entry can be in one plugin of the `entry.ingest` chain while
//! another refresh's is in the next, and a scan of one plugin no longer
//! holds up the others.
//!
//! Neither side has a thread of its own reading the socket, since handing
//! each frame from a reader thread to the thread it is for would cost a
//! thread wake-up per frame on each side, which is most of what a request
//! costs when plugins are quick. Instead the threads waiting for frames take
//! turns at reading: on the server, a request waiting for its answer; in the
//! child, an idle worker, which serves the request it reads itself. When the
//! thread reading has what it was waiting for, it hands the reading on. With
//! one request in flight, as while a feed's entries go through plugins one
//! after another, each frame is read by the thread it is for, as when
//! requests went one at a time.
//!
//! Reloading plugins is the exception: it waits for the requests in flight
//! to finish, and no others start until it is done, so that no event is
//! ever served by plugins that are being replaced.
//!
//! # Failure behaviour
//!
//! Plugin-level failures — a compile error, a trap, a timeout —
//! are handled inside the child and reported as
//! [`HostResponse::Failed`]; they do not disturb the connection. A
//! failure of the *channel* (the child died, or sent something that makes
//! no sense, or stopped answering; see [`IPC_TIMEOUT`]) marks the host
//! permanently dead, and scripting stays disabled until the server
//! restarts, because the server can no longer spawn anything once its own
//! sandbox is installed.
//!
//! Either way an `entry.ingest` dispatch that fails returns the entry
//! **unmodified** rather than dropping it, which is the contract the
//! in-process runner already documents.

use crate::process::ipc::{
    decode, encode_into, read_frame_into, write_frame, FromHost, HostMessage, HostRequest,
    HostResponse, PluginTime, RequestId, Subscriptions, ToHost, ToHostRef, KEPT_BUFFER_BYTES,
    MAX_FRAME_BYTES,
};
use crate::scripting::wasm::WasmScriptRunner;
use crate::scripting::{
    EntryPatch, Event, EventPayload, EventSet, FeedEntry, FetchSchedule, PluginRun, ScanSummary,
    ScheduleDecision, ScriptRunner, ScriptServices, ScriptSource, ServiceCall, ServiceReply,
};
use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::io::{self, BufReader};
use std::os::unix::io::RawFd;
use std::os::unix::net::UnixStream;
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// The hidden subcommand the server re-execs itself with.
pub const SUBCOMMAND: &str = "__script-host";

/// File descriptor the child inherits its end of the socket pair on.
pub const HOST_FD: RawFd = crate::process::CHILD_FD;

/// Environment variable set on the child, naming [`HOST_FD`].
///
/// Purely informational: it makes `kiki __script-host` legible in a
/// process listing and lets the child produce a clear diagnostic when
/// someone runs it by hand instead of failing obscurely on a read.
pub const HOST_FD_ENV: &str = "KIKI_SCRIPT_HOST_FD";

/// How long a request may wait for its answer, while the child sends
/// nothing at all, before the server declares the host dead.
///
/// The child is only taken for wedged if it is silent: while it answers
/// other requests it is working, and a request may be waiting its turn
/// behind them for a plugin that is busy. A request is still given up on,
/// and the host with it, once it has waited [`HARD_LIMIT_FACTOR`] times
/// this, so that a plugin stuck for good cannot hold up its callers for
/// ever while the others keep the child talking.
///
/// Generous next to the child's own default per-handler budget
/// ([`SCRIPT_TIMEOUT_MS`]) so that a slow chain of handlers, or a reload
/// loading many plugins, is never mistaken for a hung child — but
/// short enough that a genuinely wedged host cannot pin a feed worker
/// for long. It is also the only limit on the handlers of a plugin
/// whose budget is [`TimeBudget::Unlimited`].
///
/// Requests that make the child load WebAssembly plugins are allowed
/// longer; see [`ScriptHost::request`].
///
/// [`SCRIPT_TIMEOUT_MS`]: crate::scripting::SCRIPT_TIMEOUT_MS
/// [`TimeBudget::Unlimited`]: crate::scripting::TimeBudget::Unlimited
pub const IPC_TIMEOUT: Duration = Duration::from_secs(10);

/// How many times its timeout a request may wait for its answer, however
/// busy the child is, before the server declares the host dead. See
/// [`IPC_TIMEOUT`].
pub const HARD_LIMIT_FACTOR: u32 = 4;

/// How often a request waiting for its answer checks whether the child has
/// gone silent for too long; see [`IPC_TIMEOUT`].
const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// How much longer than [`IPC_TIMEOUT`] the server waits for the child to
/// compile a WebAssembly component, per MiB of component (or part of one).
///
/// Cranelift compiles about a MiB a second; this leaves room for a slow or
/// busy machine. Compiling can't be interrupted, but takes time in
/// proportion to the component's size, which is capped (see
/// [`crate::plugins::MAX_WASM_COMPONENT_BYTES`]).
pub const COMPILE_TIME_PER_MIB: Duration = Duration::from_secs(10);

/// How many threads the child serves requests on: as many as it has CPUs,
/// within these bounds. More than one, so that a request waiting on the
/// server, or for a busy plugin, does not hold up the rest; and not so many
/// that a burst of requests buys nothing but contention, since a request
/// waits for the plugins it needs however many threads there are.
pub const WORKERS: std::ops::RangeInclusive<usize> = 2..=8;

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
    fn is_fatal(&self) -> bool {
        match self {
            HostError::Io(_) | HostError::Protocol(_) => true,
            HostError::Dead | HostError::Failed(_) => false,
        }
    }
}

thread_local! {
    /// A buffer to encode frames into, kept between them, so that most
    /// frames need no allocation. Each thread has its own, since many
    /// encode at once.
    static ENCODE_BUFFER: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Encode `msg` and write it to `writer` as one frame, holding the writer
/// only while writing.
///
/// Failures before the first byte goes out are [`HostError::Failed`], not
/// [`HostError::Io`] — see [`HostError::is_fatal`].
fn send<T: serde::Serialize>(writer: &Mutex<UnixStream>, msg: &T) -> Result<(), HostError> {
    ENCODE_BUFFER.with_borrow_mut(|buf| {
        encode_into(msg, buf)
            .map_err(|e| HostError::Failed(format!("could not encode the message: {e}")))?;
        if buf.len() > MAX_FRAME_BYTES {
            return Err(HostError::Failed(format!(
                "a message of {} bytes exceeds the {} byte frame limit",
                buf.len(),
                MAX_FRAME_BYTES
            )));
        }
        let written = write_frame(&mut *writer.lock().unwrap_or_else(|e| e.into_inner()), buf);
        // So that one large entry does not pin megabytes in every thread
        // that ever sent one.
        if buf.capacity() > KEPT_BUFFER_BYTES {
            buf.clear();
            buf.shrink_to(KEPT_BUFFER_BYTES);
        }
        Ok(written?)
    })
}

/// What reaches a request waiting for its answer.
enum Inbound {
    /// The answer.
    Done(HostResponse, Subscriptions),
    /// A call a plugin made while the request is served.
    Call(String, ServiceCall),
    /// The channel failed, and the host was retired.
    Retired(String),
    /// The request that was reading the channel has stopped: the request
    /// told this should read it, unless another already does.
    Lead,
}

/// The server's end of the channel, for reading.
struct Reader {
    stream: BufReader<UnixStream>,
    buf: Vec<u8>,
}

/// What the server's half of the channel shares between the threads that
/// send requests.
///
/// There is no thread of its own reading the answers: the requests waiting
/// for theirs take turns at it. One at a time holds [`Self::reader`] and
/// reads, handling what is for it and passing on what is for the others;
/// when it has what it was waiting for, it hands the reading on to one of
/// them ([`Inbound::Lead`]). With one request in flight, as while a feed's
/// entries go through plugins one after another, the request reads its own
/// answer, and no other thread has to be woken to pass it on.
struct Shared {
    writer: Mutex<UnixStream>,
    reader: Mutex<Reader>,
    state: Mutex<HostState>,
    /// Whether the channel is still usable.
    alive: AtomicBool,
    /// When the child last sent a frame, in milliseconds since `started`.
    last_frame: AtomicU64,
    started: Instant,
}

struct HostState {
    /// `None` when the host was not spawned as a process, as in tests, or
    /// once it has been reaped.
    child: Option<Child>,
    /// Where to send what arrives for each request in flight.
    pending: HashMap<RequestId, mpsc::Sender<Inbound>>,
    next_id: RequestId,
}

impl Shared {
    /// How long it has been since the child last sent a frame.
    fn silence(&self) -> Duration {
        let last = Duration::from_millis(self.last_frame.load(Ordering::Relaxed));
        self.started.elapsed().saturating_sub(last)
    }

    /// Note that the child just sent a frame.
    fn heard(&self) {
        let now = self.started.elapsed().as_millis() as u64;
        self.last_frame.store(now, Ordering::Relaxed);
    }

    /// Retire the host, because the channel failed with `reason`: kill the
    /// child, and fail every request in flight. Done once; later calls do
    /// nothing.
    fn retire(&self, reason: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !self.alive.swap(false, Ordering::SeqCst) {
            return;
        }
        warn!(
            error = reason,
            "script host: channel failed, disabling scripting until restart"
        );
        if let Some(mut child) = state.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Wakes the request reading the channel, if one is.
        let _ = self
            .writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shutdown(std::net::Shutdown::Both);
        for (_, waiting) in state.pending.drain() {
            let _ = waiting.send(Inbound::Retired(reason.to_string()));
        }
    }

    /// Hand the reading of the channel on to the requests in flight other
    /// than `id`; the first to wake takes it.
    fn hand_on(&self, id: RequestId) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        for (_, waiting) in state.pending.iter().filter(|(other, _)| **other != id) {
            let _ = waiting.send(Inbound::Lead);
        }
    }

    /// Wait up to `wait` for a frame from the child, and read it. Returns
    /// what it holds if it is for the request `id`; anything else is
    /// passed on to the request it is for.
    ///
    /// # Errors
    ///
    /// Fails, retiring the host, if the channel fails, or the frame is
    /// undecodable or for no request in flight.
    fn read_for(
        &self,
        reader: &mut Reader,
        id: RequestId,
        wait: Duration,
    ) -> Result<Option<Inbound>, HostError> {
        if reader.stream.buffer().is_empty() && !readable(reader.stream.get_ref(), wait) {
            return Ok(None);
        }
        let fail = |e: HostError| {
            if self.alive.load(Ordering::SeqCst) {
                self.retire(&e.to_string());
                e
            } else {
                HostError::Dead
            }
        };
        let frame = read_frame_into(&mut reader.stream, &mut reader.buf)
            .map_err(|e| fail(HostError::Io(e)))?;
        self.heard();
        let FromHost { id: about, message } = decode(frame)
            .map_err(|e| fail(HostError::Protocol(format!("decoding a frame: {e}"))))?;
        let inbound = match message {
            HostMessage::Done {
                response,
                subscribed,
            } => Inbound::Done(response, subscribed),
            HostMessage::Call { plugin, call } => Inbound::Call(plugin, call),
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let waiting = match &inbound {
            Inbound::Done(..) => state.pending.remove(&about),
            _ => state.pending.get(&about).cloned(),
        };
        drop(state);
        if about == id {
            return Ok(Some(inbound));
        }
        match waiting {
            Some(waiting) => {
                let _ = waiting.send(inbound);
                Ok(None)
            }
            None => Err(fail(HostError::Protocol(format!(
                "the script host sent a frame for request {about}, which is not in flight"
            )))),
        }
    }
}

/// Whether `stream` has something to read within `wait`. Interrupted waits
/// count as nothing to read; an error or a hang-up counts as something, so
/// that the read reports it.
fn readable(stream: &UnixStream, wait: Duration) -> bool {
    use std::os::unix::io::AsRawFd;
    let mut fd = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = wait.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: `fd` is one valid pollfd, and the count says so.
    unsafe { libc::poll(&mut fd, 1, millis) > 0 }
}

/// A handle to the script host child process.
///
/// Cheap to share behind an [`Arc`]. Requests from different threads are
/// in flight at once; see the [module documentation](self).
///
/// # Blocking
///
/// [`Self::request`] blocks the calling thread until its answer arrives.
/// Dispatch already happens from synchronous code interleaved with SQLite
/// writes, so this is not a new kind of stall. A wedged child costs the
/// callers waiting on it [`IPC_TIMEOUT`] (or the longer wait
/// [`Self::request`] allows a request that loads WebAssembly plugins),
/// *once*: the first to give up retires the host, failing the rest, and
/// every request after that finds a dead host and returns immediately.
pub struct ScriptHost {
    shared: Arc<Shared>,
    /// Held shared by every request, and exclusively by a reload, which so
    /// waits for the requests in flight and holds up any more until it is
    /// done.
    reloading: RwLock<()>,
    /// Answers the calls plugins make while a request is served. Calls
    /// fail until the server sets it with [`Self::set_services`].
    services: RwLock<Option<Arc<dyn ScriptServices>>>,
    /// The events the child's plugins have handlers for, as of the most
    /// recent report: its [`Subscriptions::seq`] above, and the
    /// [`EventSet::to_bits`] of its events in the low 16 bits, so that the
    /// most recent report is the largest. Every event until the child has
    /// answered once.
    subscribed: AtomicU64,
    /// The hashes of the WebAssembly components the child has compiled and
    /// kept, which sources can name without sending them again.
    components: Mutex<HashSet<[u8; 32]>>,
    /// Whether the child has plugins loaded, for [`request_timeout`].
    loaded: AtomicBool,
    /// The names of the plugins the child loaded, in order, to tell from a
    /// [`PluginTime`] which plugin it is about.
    names: ArcSwap<Vec<Arc<str>>>,
}

impl ScriptHost {
    /// Spawn the script host child.
    ///
    /// **Must be called before the caller installs its seccomp filter** —
    /// the server's sandbox profile denies `execve` — and, for the server to see
    /// the child's memory use, after its Landlock rules; see
    /// [`crate::sandbox::restrict_filesystem`]. `log_only` and `no_sandbox`
    /// are forwarded so the child's sandbox matches the operator's intent
    /// for the server's.
    ///
    /// # Errors
    ///
    /// Fails if the current executable cannot be located, the socket pair
    /// cannot be created, or the child cannot be spawned.
    pub fn spawn(log_only: bool, no_sandbox: bool) -> Result<Self> {
        let exe = std::env::current_exe().context("locating the kiki executable")?;
        Self::spawn_exe(&exe, log_only, no_sandbox)
    }

    /// [`Self::spawn`], running the `kiki` binary at `exe` rather than the
    /// current one, for benchmarks and tests built as binaries of their own.
    ///
    /// # Errors
    ///
    /// As for [`Self::spawn`].
    #[doc(hidden)]
    pub fn spawn_exe(exe: &std::path::Path, log_only: bool, no_sandbox: bool) -> Result<Self> {
        let (ours, child) =
            crate::process::spawn_child_exe(exe, SUBCOMMAND, HOST_FD_ENV, log_only, no_sandbox)
                .context("spawning the script host process")?;
        info!(pid = child.id(), "script host: spawned");
        Self::over(ours, Some(child))
    }

    /// A host that talks to whatever serves the other end of `stream`, the
    /// process `child` if it is one.
    fn over(stream: UnixStream, child: Option<Child>) -> Result<Self> {
        // Bounded both ways, so that a child that stops reading, or stops
        // in the middle of a frame, is noticed. Waiting for a frame to
        // start is bounded by the requests themselves.
        stream
            .set_write_timeout(Some(IPC_TIMEOUT))
            .and_then(|()| stream.set_read_timeout(Some(IPC_TIMEOUT)))
            .context("setting the script host timeouts")?;
        let reader = stream
            .try_clone()
            .context("cloning the script host socket")?;
        let shared = Arc::new(Shared {
            writer: Mutex::new(stream),
            reader: Mutex::new(Reader {
                stream: BufReader::new(reader),
                buf: Vec::new(),
            }),
            state: Mutex::new(HostState {
                child,
                pending: HashMap::new(),
                next_id: 0,
            }),
            alive: AtomicBool::new(true),
            last_frame: AtomicU64::new(0),
            started: Instant::now(),
        });
        Ok(ScriptHost {
            shared,
            reloading: RwLock::new(()),
            services: RwLock::new(None),
            subscribed: AtomicU64::new(u64::from(EventSet::ALL.to_bits())),
            components: Mutex::new(HashSet::new()),
            loaded: AtomicBool::new(false),
            names: ArcSwap::from_pointee(Vec::new()),
        })
    }

    /// Answer the calls plugins make to the server, such as to their
    /// stores, with `services`.
    pub fn set_services(&self, services: Arc<dyn ScriptServices>) {
        *self.services.write().unwrap_or_else(|e| e.into_inner()) = Some(services);
    }

    /// Send `request` and wait for the matching response, answering the
    /// calls plugins make along the way.
    ///
    /// The server waits up to [`IPC_TIMEOUT`] of silence from the child,
    /// and longer for the work of loading WebAssembly plugins, which is
    /// bounded but can take longer than that (see [`request_timeout`]).
    /// Any channel-level failure, a timeout among them, retires the host:
    /// the child is killed and every subsequent call returns
    /// [`HostError::Dead`].
    pub fn request(&self, request: &HostRequest<'_>) -> Result<HostResponse, HostError> {
        let _serving = self.reloading.read().unwrap_or_else(|e| e.into_inner());
        self.exchange(request)
    }

    /// [`Self::request`], for a caller that already holds [`Self::reloading`].
    fn exchange(&self, request: &HostRequest<'_>) -> Result<HostResponse, HostError> {
        let timeout = request_timeout(request, self.loaded.load(Ordering::Relaxed));
        let (id, answers, busy) = {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            if !self.shared.alive.load(Ordering::SeqCst) {
                return Err(HostError::Dead);
            }
            let id = state.next_id;
            state.next_id += 1;
            let (tx, rx) = mpsc::channel();
            state.pending.insert(id, tx);
            (id, rx, state.pending.len() > 1)
        };
        let forget = || {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.pending.remove(&id);
        };

        match send(&self.shared.writer, &ToHostRef { id, busy, request }) {
            Ok(()) => {}
            Err(e) => {
                forget();
                if e.is_fatal() {
                    self.shared.retire(&e.to_string());
                }
                return Err(e);
            }
        }

        let sent = Instant::now();
        let services = self
            .services
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        loop {
            let inbound = match self.next_inbound(id, &answers) {
                Ok(inbound) => inbound,
                Err(e) => {
                    forget();
                    return Err(e);
                }
            };
            match inbound {
                Some(Inbound::Done(response, subscribed)) => {
                    self.subscribed
                        .fetch_max(packed(subscribed), Ordering::SeqCst);
                    return match response {
                        HostResponse::Failed { message } => Err(HostError::Failed(message)),
                        response => Ok(response),
                    };
                }
                Some(Inbound::Call(plugin, call)) => {
                    let result =
                        match &services {
                            Some(services) => services.call(&plugin, call),
                            None => Err("not available: the server is not answering plugin calls"
                                .to_string()),
                        };
                    let answer = ToHostRef {
                        id,
                        busy: false,
                        request: &HostRequest::CallResult { result },
                    };
                    // An answer that cannot be sent would leave the plugin
                    // waiting for one, so failing to send it is fatal.
                    if let Err(e) = send(&self.shared.writer, &answer) {
                        let e = HostError::Protocol(format!("sending a call result: {e}"));
                        self.shared.retire(&e.to_string());
                        return Err(e);
                    }
                }
                Some(Inbound::Retired(reason)) => {
                    return Err(HostError::Io(io::Error::other(reason)));
                }
                Some(Inbound::Lead) => {}
                None => {
                    let waited = sent.elapsed();
                    let silent = waited >= timeout && self.shared.silence() >= timeout;
                    if silent || waited >= timeout * HARD_LIMIT_FACTOR {
                        let e = HostError::Io(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("no answer from the script host in {}s", waited.as_secs()),
                        ));
                        self.shared.retire(&e.to_string());
                        return Err(e);
                    }
                    // Not long yet, or the child is working, and this
                    // request waits its turn.
                }
            }
        }
    }

    /// What has been passed on to a request already, if anything, passing
    /// over the [`Inbound::Lead`]s that came with it: one may be queued ahead
    /// of an answer.
    fn passed_on(answers: &mpsc::Receiver<Inbound>) -> Result<Option<Inbound>, HostError> {
        loop {
            match answers.try_recv() {
                Ok(Inbound::Lead) => {}
                Ok(inbound) => return Ok(Some(inbound)),
                Err(mpsc::TryRecvError::Empty) => return Ok(None),
                Err(mpsc::TryRecvError::Disconnected) => return Err(HostError::Dead),
            }
        }
    }

    /// The next thing to arrive for the request `id`, waiting up to
    /// [`HEALTH_CHECK_INTERVAL`] for it: reading the channel if no other
    /// request is, and otherwise waiting for the one that is to pass it on.
    /// `None` if nothing arrived in time.
    fn next_inbound(
        &self,
        id: RequestId,
        answers: &mpsc::Receiver<Inbound>,
    ) -> Result<Option<Inbound>, HostError> {
        if let Some(inbound) = Self::passed_on(answers)? {
            return Ok(Some(inbound));
        }
        let reader = match self.shared.reader.try_lock() {
            Ok(reader) => Some(reader),
            Err(std::sync::TryLockError::Poisoned(e)) => Some(e.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        };
        let Some(mut reader) = reader else {
            // Another request is reading, and passes on what is for this one.
            return match answers.recv_timeout(HEALTH_CHECK_INTERVAL) {
                Ok(inbound) => Ok(Some(inbound)),
                Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
                Err(mpsc::RecvTimeoutError::Disconnected) => Err(HostError::Dead),
            };
        };
        // Passed on before this request took over the reading, perhaps.
        if let Some(inbound) = Self::passed_on(answers)? {
            drop(reader);
            self.shared.hand_on(id);
            return Ok(Some(inbound));
        }
        let read = self.shared.read_for(&mut reader, id, HEALTH_CHECK_INTERVAL);
        drop(reader);
        if matches!(read, Ok(Some(_))) {
            // Off to handle it: someone else reads in the meantime.
            self.shared.hand_on(id);
        }
        read
    }

    /// Rebuild the child's plugins from `sources`, replacing whatever it was
    /// running. If `sources` fail to load, the child keeps running the
    /// plugins it had.
    ///
    /// Waits for the requests in flight to finish, and holds up any more
    /// until the plugins have loaded.
    ///
    /// Each component the child has not compiled yet is sent first, in a
    /// message of its own; the sources then name it by hash.
    ///
    /// Returns the number of plugins the child loaded.
    pub fn reload(&self, mut sources: Vec<ScriptSource>) -> Result<usize, HostError> {
        let _reloading = self.reloading.write().unwrap_or_else(|e| e.into_inner());
        let mut used = HashSet::new();
        for source in &mut sources {
            let component = &mut source.component;
            used.insert(component.hash);
            let known = self
                .components
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&component.hash);
            if !known {
                let put = HostRequest::PutComponent {
                    plugin: source.name.clone(),
                    component: component.clone(),
                };
                match self.exchange(&put)? {
                    HostResponse::Ack => {}
                    other => {
                        return Err(HostError::Protocol(format!("expected Ack, got {other:?}")))
                    }
                }
                self.components
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(component.hash);
            }
            component.bytes = Vec::new();
        }
        let names: Vec<Arc<str>> = sources.iter().map(|s| Arc::from(s.name.as_str())).collect();
        match self.exchange(&HostRequest::Reload { sources })? {
            HostResponse::Reloaded { loaded } => {
                // The child forgets the components no plugin uses any more.
                self.components
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .retain(|hash| used.contains(hash));
                self.loaded.store(loaded > 0, Ordering::Relaxed);
                self.names.store(Arc::new(names));
                Ok(loaded)
            }
            other => Err(HostError::Protocol(format!(
                "expected Reloaded, got {other:?}"
            ))),
        }
    }

    /// The events the child's plugins had handlers for as of its most
    /// recent report. Every event until it has answered once; none once the
    /// host is retired, since nothing sent to it would be served.
    pub fn subscribed(&self) -> EventSet {
        if !self.shared.alive.load(Ordering::Relaxed) {
            return EventSet::default();
        }
        EventSet::from_bits(self.subscribed.load(Ordering::Relaxed) as u16)
    }

    /// Whether the channel to the child is still usable.
    pub fn is_alive(&self) -> bool {
        self.shared.alive.load(Ordering::SeqCst)
    }

    /// The name of the plugin with index `index` in load order.
    fn plugin_name(&self, index: u32) -> Option<Arc<str>> {
        self.names.load().get(index as usize).cloned()
    }
}

/// `subscribed`, packed for [`ScriptHost::subscribed`].
fn packed(subscribed: Subscriptions) -> u64 {
    (subscribed.seq << 16) | u64::from(subscribed.events.to_bits())
}

impl Drop for ScriptHost {
    fn drop(&mut self) {
        self.shared.alive.store(false, Ordering::SeqCst);
        let child = {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.child.take()
        };
        // Closing our end is the child's shutdown signal: its next read
        // returns EOF and it exits.
        let _ = self
            .shared
            .writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shutdown(std::net::Shutdown::Write);
        if let Some(child) = child {
            crate::process::reap(child);
        }
    }
}

/// How long a request may wait for the answer to `request` while the child
/// is silent, given whether plugins are loaded:
///
/// * compiling a WebAssembly component gets [`COMPILE_TIME_PER_MIB`] more
///   per MiB of it;
/// * a reload gets [`WASM_LOAD_BUDGET`] more per plugin, each of which
///   may take that long to load;
/// * while plugins are loaded, anything else gets one
///   [`WASM_LOAD_BUDGET`] more, since the child restarts a plugin that
///   trapped when it is next needed, one per request at most.
///
/// [`WASM_LOAD_BUDGET`]: crate::scripting::WASM_LOAD_BUDGET
fn request_timeout(request: &HostRequest<'_>, loaded: bool) -> Duration {
    use crate::scripting::WASM_LOAD_BUDGET;
    const MIB: usize = 1024 * 1024;
    match request {
        HostRequest::PutComponent { component, .. } => {
            let mib = component.bytes.len().div_ceil(MIB).max(1) as u32;
            IPC_TIMEOUT + COMPILE_TIME_PER_MIB * mib
        }
        HostRequest::Reload { sources } => IPC_TIMEOUT + WASM_LOAD_BUDGET * sources.len() as u32,
        _ if loaded => IPC_TIMEOUT + WASM_LOAD_BUDGET,
        _ => IPC_TIMEOUT,
    }
}

/// A [`ScriptRunner`] that forwards every dispatch to the script host
/// child.
///
/// Drop-in for [`WasmScriptRunner`]: same trait,
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
    fn handles(&self, event: Event) -> bool {
        self.host.subscribed().contains(event)
    }

    fn dispatch_transform_entry(&self, mut entry: FeedEntry) -> Result<Option<FeedEntry>> {
        let keep = self.dispatch_transform_entry_timed(&mut entry, &mut Vec::new())?;
        Ok(keep.then_some(entry))
    }

    /// Sends the entry without copying it, and applies the changes the
    /// host sends back, which are only the fields plugins changed.
    fn dispatch_transform_entry_timed(
        &self,
        entry: &mut FeedEntry,
        runs: &mut Vec<PluginRun>,
    ) -> Result<bool> {
        if !self.handles(Event::EntryIngest) {
            return Ok(true);
        }
        let request = HostRequest::TransformEntry {
            entry: Cow::Borrowed(entry),
        };
        let response = self.host.request(&request);
        match response {
            Ok(HostResponse::Entry { patch, runs: done }) => {
                runs.extend(
                    done.into_iter()
                        .filter_map(|PluginTime { plugin, seconds }| {
                            Some(PluginRun {
                                plugin: self.host.plugin_name(plugin)?,
                                seconds,
                            })
                        }),
                );
                match patch {
                    Some(patch) => {
                        patch.apply(entry);
                        Ok(true)
                    }
                    None => Ok(false),
                }
            }
            Ok(other) => {
                warn!("script host: expected an Entry response, got {other:?}");
                Ok(true)
            }
            Err(e) => {
                // Never drop an entry because the scripting layer broke.
                warn!(error = %e, "script host: entry.ingest dispatch failed; passing the entry through unmodified");
                Ok(true)
            }
        }
    }

    /// Sends the entry without copying it.
    fn dispatch_parsed(&self, entry: &FeedEntry) {
        if !self.handles(Event::EntryParsed) {
            return;
        }
        let request = HostRequest::EntryParsed {
            entry: Cow::Borrowed(entry),
        };
        if let Err(e) = self.host.request(&request) {
            warn!(event = Event::EntryParsed.name(), error = %e, "script host: observe dispatch failed");
        }
    }

    fn dispatch_schedule(&self, schedule: FetchSchedule) -> Result<Option<ScheduleDecision>> {
        if !self.handles(Event::FetchSchedule) {
            return Ok(None);
        }
        match self.host.request(&HostRequest::Schedule { schedule })? {
            HostResponse::Schedule { decision } => Ok(decision),
            HostResponse::Failed { message } => Err(anyhow::anyhow!(message)),
            other => Err(anyhow::anyhow!(
                "expected a Schedule response, got {other:?}"
            )),
        }
    }

    fn dispatch_observe(&self, event: Event, payload: EventPayload) {
        if !self.handles(event) {
            return;
        }
        let request = HostRequest::Observe { event, payload };
        if let Err(e) = self.host.request(&request) {
            warn!(event = event.name(), error = %e, "script host: observe dispatch failed");
        }
    }

    /// Sends the host as many of `entries` as fit in one request; see
    /// [`scan_prefix_len`]. The scan hands the rest back on its next
    /// dispatch, as it does when the handler runs out of time.
    ///
    /// An entry too large to send even on its own is skipped, and reported
    /// as if the handler had returned `nil` for it, so it is left as it is
    /// and the rest of the scan carries on. A new entry too large to send
    /// to `entry.ingest` is likewise stored without the scripts' changes.
    fn dispatch_scan(
        &self,
        scan_id: u64,
        mut entries: Vec<FeedEntry>,
    ) -> Result<Option<Vec<Option<FeedEntry>>>> {
        let fit = scan_prefix_len(&entries, SCAN_REQUEST_BUDGET)?;
        if fit == 0 {
            if let Some(entry) = entries.first() {
                warn!(
                    entry_id = entry.id,
                    guid = %entry.guid,
                    "script host: entry too large to send to a scan handler; skipping it"
                );
                return Ok(Some(vec![None]));
            }
        }
        entries.truncate(fit);
        match self.host.request(&HostRequest::Scan { scan_id, entries })? {
            HostResponse::Scanned { entries } => Ok(entries),
            other => anyhow::bail!("script host: expected a Scanned response, got {other:?}"),
        }
    }

    fn finish_scan(&self, scan_id: u64, summary: Option<ScanSummary>) {
        if let Err(e) = self
            .host
            .request(&HostRequest::FinishScan { scan_id, summary })
        {
            warn!(error = %e, "script host: finishing scan {scan_id} failed");
        }
    }
}

/// Most bytes of entries [`SubprocessScriptRunner::dispatch_scan`] puts in
/// one request. The response carries the entries back, so this leaves the
/// host room to answer within [`MAX_FRAME_BYTES`] even if the handler grows
/// them.
const SCAN_REQUEST_BUDGET: usize = MAX_FRAME_BYTES / 2;

/// Bytes a [`HostRequest::Scan`] adds to its entries' own encoding: the
/// request id, the variant, the scan id and the list's length, each a
/// varint.
const SCAN_REQUEST_OVERHEAD: usize = 48;

/// How many of `entries`, from the start, to send in one scan request:
/// as many as fit in `budget` bytes, and always the first if it fits in a
/// frame on its own, so the scan makes progress. Zero means the first
/// entry is too large to send at all.
///
/// # Errors
///
/// Returns an error if an entry cannot be encoded.
fn scan_prefix_len(entries: &[FeedEntry], budget: usize) -> Result<usize> {
    let mut total = SCAN_REQUEST_OVERHEAD;
    for (i, entry) in entries.iter().enumerate() {
        total += crate::process::ipc::encode(entry)
            .context("could not encode an entry for a scan")?
            .len();
        let limit = if i == 0 { MAX_FRAME_BYTES } else { budget };
        if total > limit {
            return Ok(i);
        }
    }
    Ok(entries.len())
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
    if no_sandbox {
        warn!(
            "script host: sandbox disabled via --no-sandbox; plugins run with full \
             filesystem and syscall access"
        );
    } else {
        crate::sandbox::apply(&crate::sandbox::SandboxConfig::script_host(log_only))
            .context("failed to install the script host sandbox")?;
    }
    serve_channel(crate::process::take_parent_socket(SUBCOMMAND)?)
}

/// The child's end of the channel, for reading.
struct ChildReader {
    stream: BufReader<UnixStream>,
    buf: Vec<u8>,
}

/// Who reads the channel in the child, and the requests read but not yet
/// served.
///
/// There is no thread of its own reading the channel: an idle worker reads
/// it, and once it has read a request, hands the reading on to another and
/// serves the request itself, so that a request is served on the thread
/// that read it. A worker waiting for the answer to a plugin's call to the
/// server reads the channel too while no idle worker does, since otherwise,
/// with every worker waiting on the server, nobody would read the answers;
/// the requests it reads meanwhile wait in [`Self::queue`] for a worker.
///
/// A request sent while no other was in flight (see [`ToHost::busy`]) is
/// the exception: the reading is kept for the thread that read it
/// ([`Self::kept_for`]), which reads the next request itself once it has
/// answered, so that requests sent one after another, as a feed's entries
/// are, are all served on one thread, whose caches are warm with the
/// plugins. Meanwhile [`watch`] looks at the channel every
/// [`WATCH_INTERVAL`], and hands the reading to an idle worker if a request
/// has come before the thread it is kept for is done. (Watching from an
/// idle worker instead, blocked on the channel, would cost every request a
/// thread wake-up, which is what keeping the reading saves.)
#[derive(Default)]
struct Sched {
    /// The thread reading the channel, if one is.
    reader: Option<std::thread::ThreadId>,
    /// The thread the reading is kept for while it serves a request.
    kept_for: Option<std::thread::ThreadId>,
    /// Whether [`watch`] is asleep, for want of a reading kept to watch.
    watch_asleep: bool,
    /// Requests read by a thread that could not serve them.
    queue: std::collections::VecDeque<(RequestId, HostRequest<'static>)>,
    /// Whether the channel has closed.
    closed: bool,
}

impl Sched {
    /// Whether the thread `me` may take the reading: nobody reads, and it is
    /// not kept for another thread.
    fn free_for(&self, me: std::thread::ThreadId) -> bool {
        self.reader.is_none() && !self.closed && self.kept_for.is_none_or(|t| t == me)
    }
}

/// How often [`watch`] looks at the channel while the reading is kept, and
/// so the longest a request coming meanwhile waits for a thread to read it.
const WATCH_INTERVAL: Duration = Duration::from_millis(1);

/// How long [`watch`] goes on looking with no reading kept before it sleeps
/// until there is one.
const WATCH_IDLE: Duration = Duration::from_millis(100);

/// What a plugin's call to the server waits for.
enum CallInbound {
    /// The server's answer.
    Answer(Result<ServiceReply, String>),
    /// Nobody is reading the channel: the call told this should, unless
    /// another thread already does.
    Lead,
}

/// What reading the channel came to.
// One is alive per read, so boxing the request would cost an allocation per
// request to save a few hundred bytes of stack.
#[allow(clippy::large_enum_variant)]
enum Read {
    /// A request to serve, and whether to hand the reading on at once:
    /// because others were in flight when it was sent (see
    /// [`ToHost::busy`]), or more has been read already.
    Request(RequestId, bool, HostRequest<'static>),
    /// The answer to the call the reader was waiting for.
    Answer(Result<ServiceReply, String>),
    /// The channel closed.
    Closed,
}

/// What the child's threads share: the channel to the server.
struct ChildIo {
    writer: Mutex<UnixStream>,
    /// Read only by the thread [`Sched::reader`] says reads.
    reader: Mutex<ChildReader>,
    /// The channel, for watching whether there is something to read.
    watch: UnixStream,
    sched: Mutex<Sched>,
    /// Signalled when a request is queued, the reading is free, or the
    /// channel closes, for idle workers.
    idle: std::sync::Condvar,
    /// Signalled when the reading is kept while [`watch`] sleeps, or the
    /// channel closes.
    watch_wake: std::sync::Condvar,
    /// Where to send the answer to the call each request in flight made.
    calls: Mutex<HashMap<RequestId, mpsc::Sender<CallInbound>>>,
}

impl ChildIo {
    fn sched(&self) -> std::sync::MutexGuard<'_, Sched> {
        self.sched.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Take the reading of the channel, unless another thread has it, or it
    /// is kept for another.
    fn take_reading(&self) -> bool {
        let me = std::thread::current().id();
        let mut sched = self.sched();
        if !sched.free_for(me) {
            return false;
        }
        sched.reader = Some(me);
        sched.kept_for = None;
        true
    }

    /// Give up the reading of the channel, to an idle worker, or else to a
    /// call waiting for its answer.
    fn hand_on(&self) {
        let mut sched = self.sched();
        sched.reader = None;
        sched.kept_for = None;
        drop(sched);
        self.idle.notify_one();
        for waiting in self
            .calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            let _ = waiting.send(CallInbound::Lead);
        }
    }

    /// Stop reading the channel, but keep the reading for this thread, which
    /// is off to serve a request; see [`Sched`].
    fn keep(&self) {
        let mut sched = self.sched();
        sched.reader = None;
        sched.kept_for = Some(std::thread::current().id());
        let asleep = std::mem::take(&mut sched.watch_asleep);
        drop(sched);
        if asleep {
            self.watch_wake.notify_one();
        }
    }

    /// Take back the reading kept for this thread, if it still is, before
    /// answering the request it served, so that it reads the next request
    /// itself rather than leaving it to the worker watching.
    fn reclaim(&self) {
        let me = std::thread::current().id();
        let mut sched = self.sched();
        if sched.kept_for == Some(me) && sched.reader.is_none() {
            sched.reader = Some(me);
            sched.kept_for = None;
        }
    }

    /// Note that the channel closed: idle workers stop, and calls waiting
    /// for answers fail.
    fn close(&self) {
        let mut sched = self.sched();
        sched.closed = true;
        sched.reader = None;
        sched.kept_for = None;
        drop(sched);
        self.idle.notify_all();
        self.watch_wake.notify_all();
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Read the channel until a request arrives, or the answer to the call
    /// made by the request `waiting`, passing on the answers to other calls.
    /// Only for the thread that has the reading.
    fn read(&self, waiting: Option<RequestId>) -> Read {
        let mut reader = self.reader.lock().unwrap_or_else(|e| e.into_inner());
        let ChildReader { stream, buf } = &mut *reader;
        loop {
            let frame = match read_frame_into(stream, buf) {
                Ok(frame) => frame,
                Err(e) => {
                    if e.kind() == io::ErrorKind::UnexpectedEof {
                        debug!("script host: server closed the channel, exiting");
                    } else {
                        warn!(error = %e, "script host: read failed, exiting");
                    }
                    return Read::Closed;
                }
            };
            match decode::<ToHost<'static>>(frame) {
                Ok(ToHost {
                    id,
                    request: HostRequest::CallResult { result },
                    ..
                }) => {
                    if Some(id) == waiting {
                        return Read::Answer(result);
                    }
                    let call = self
                        .calls
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id);
                    match call {
                        Some(call) => {
                            let _ = call.send(CallInbound::Answer(result));
                        }
                        None => warn!(
                            "script host: a call result arrived for request {id}, which made no call"
                        ),
                    }
                }
                Ok(ToHost { id, busy, request }) => {
                    // What is buffered already is out of sight of the worker
                    // watching the channel.
                    let more = !stream.buffer().is_empty();
                    return Read::Request(id, busy || more, request);
                }
                Err(e) => {
                    warn!(error = %e, "script host: undecodable request, exiting");
                    return Read::Closed;
                }
            }
        }
    }

    /// Read a request, with the reading taken, and hand the reading on or
    /// keep it, as the request says.
    fn read_request(&self) -> Option<(RequestId, HostRequest<'static>)> {
        match self.read(None) {
            Read::Request(id, true, request) => {
                self.hand_on();
                Some((id, request))
            }
            Read::Request(id, false, request) => {
                self.keep();
                Some((id, request))
            }
            Read::Answer(_) | Read::Closed => {
                self.close();
                None
            }
        }
    }

    /// The next request for an idle worker to serve, reading the channel for
    /// it if no other thread is. `None` once the channel has closed.
    fn next_request(&self) -> Option<(RequestId, HostRequest<'static>)> {
        let me = std::thread::current().id();
        let mut sched = self.sched();
        loop {
            if let Some(job) = sched.queue.pop_front() {
                let reading = sched.reader == Some(me);
                drop(sched);
                if reading {
                    self.hand_on();
                }
                return Some(job);
            }
            if sched.closed {
                return None;
            }
            if sched.reader == Some(me) || sched.free_for(me) {
                sched.reader = Some(me);
                sched.kept_for = None;
                drop(sched);
                return self.read_request();
            }
            sched = self.idle.wait(sched).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// While the reading is kept for a thread serving a request (see [`Sched`]),
/// look at the channel every [`WATCH_INTERVAL`], and if a request has come,
/// free the reading for an idle worker. Sleeps once nothing has been kept
/// for [`WATCH_IDLE`], until something is; returns once the channel closes.
fn watch(io: &ChildIo) {
    let mut last_kept = Instant::now();
    loop {
        let mut sched = io.sched();
        if sched.closed {
            return;
        }
        let kept = sched.reader.is_none() && sched.kept_for.is_some();
        if kept {
            last_kept = Instant::now();
        } else if last_kept.elapsed() >= WATCH_IDLE {
            sched.watch_asleep = true;
            while sched.watch_asleep && !sched.closed {
                sched = io.watch_wake.wait(sched).unwrap_or_else(|e| e.into_inner());
            }
            last_kept = Instant::now();
            continue;
        }
        drop(sched);
        if kept && readable(&io.watch, Duration::ZERO) {
            let mut sched = io.sched();
            if sched.reader.is_none() && sched.kept_for.is_some() {
                // The thread it was kept for is busy: someone else reads.
                sched.kept_for = None;
                drop(sched);
                io.idle.notify_one();
            }
        }
        std::thread::sleep(WATCH_INTERVAL);
    }
}

/// What the child's threads share to serve requests.
struct ChildState {
    io: Arc<ChildIo>,
    services: Arc<dyn ScriptServices>,
    runner: RwLock<Option<Arc<WasmScriptRunner>>>,
    /// Numbers the reports of what plugins handle; see [`Subscriptions`].
    seq: AtomicU64,
}

thread_local! {
    /// The request the current thread of the child is serving, which the
    /// calls its plugins make are about.
    static SERVING: Cell<Option<RequestId>> = const { Cell::new(None) };
}

/// Serve requests from the server on `stream` until it closes the channel,
/// on [`WORKERS`] threads, this one among them.
///
/// # Errors
///
/// Fails if the socket cannot be cloned or the worker threads cannot be
/// started. A broken channel is a normal shutdown, not an error.
fn serve_channel(stream: UnixStream) -> Result<()> {
    let io = Arc::new(ChildIo {
        writer: Mutex::new(stream.try_clone().context("cloning the parent socket")?),
        watch: stream.try_clone().context("cloning the parent socket")?,
        reader: Mutex::new(ChildReader {
            stream: BufReader::new(stream),
            buf: Vec::new(),
        }),
        sched: Mutex::new(Sched::default()),
        idle: std::sync::Condvar::new(),
        watch_wake: std::sync::Condvar::new(),
        calls: Mutex::new(HashMap::new()),
    });
    let state = Arc::new(ChildState {
        io: io.clone(),
        services: Arc::new(IpcServices { io }),
        runner: RwLock::new(None),
        seq: AtomicU64::new(0),
    });

    let workers = std::thread::available_parallelism()
        .map_or(*WORKERS.start(), |n| n.get())
        .clamp(*WORKERS.start(), *WORKERS.end());
    let mut threads = Vec::with_capacity(workers);
    threads.push(
        std::thread::Builder::new()
            .name("kiki-script-host-watch".to_string())
            .spawn({
                let io = state.io.clone();
                move || watch(&io)
            })
            .context("starting the script host watcher")?,
    );
    for i in 1..workers {
        let state = state.clone();
        threads.push(
            std::thread::Builder::new()
                .name(format!("kiki-script-host-{i}"))
                .spawn(move || serve_requests(&state))
                .context("starting a script host worker")?,
        );
    }
    serve_requests(&state);
    for thread in threads {
        let _ = thread.join();
    }
    Ok(())
}

/// Serve requests until the channel closes.
fn serve_requests(state: &ChildState) {
    while let Some((id, request)) = state.io.next_request() {
        SERVING.set(Some(id));
        let response = serve(state, request);
        SERVING.set(None);
        // Taken before the events are read; see `Subscriptions`.
        let seq = state.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let events = state
            .runner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map_or_else(EventSet::default, |r| r.subscriptions());
        let done = FromHost {
            id,
            message: HostMessage::Done {
                response,
                subscribed: Subscriptions { seq, events },
            },
        };
        // Before the answer goes, which may bring the next request.
        state.io.reclaim();
        if let Err(e) = send(&state.io.writer, &done) {
            warn!(error = %e, "script host: could not send a response");
        }
    }
}

/// Answers plugins' calls in the child by asking the server over the
/// channel, in the middle of the request being served.
struct IpcServices {
    io: Arc<ChildIo>,
}

/// The answer passed on to a call already, if any, passing over the
/// [`CallInbound::Lead`]s that came with it: one may be queued ahead of the
/// answer. `Err` if the channel has closed.
fn passed_on(
    answers: &mpsc::Receiver<CallInbound>,
) -> Result<Option<Result<ServiceReply, String>>, ()> {
    loop {
        match answers.try_recv() {
            Ok(CallInbound::Lead) => {}
            Ok(CallInbound::Answer(answer)) => return Ok(Some(answer)),
            Err(mpsc::TryRecvError::Empty) => return Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => return Err(()),
        }
    }
}

impl IpcServices {
    /// Wait for the answer to the call the request `id` made, reading the
    /// channel while no other thread does.
    fn answer(
        &self,
        id: RequestId,
        answers: &mpsc::Receiver<CallInbound>,
    ) -> Result<ServiceReply, String> {
        const CLOSED: &str = "could not reach the server: the channel closed";
        let io = &self.io;
        loop {
            match passed_on(answers) {
                Ok(Some(answer)) => return answer,
                Ok(None) => {}
                Err(()) => return Err(CLOSED.to_string()),
            }
            if !io.take_reading() {
                // Another thread reads, and passes on the answer, or the
                // reading, when it stops.
                match answers.recv() {
                    Ok(CallInbound::Answer(answer)) => return answer,
                    Ok(CallInbound::Lead) => continue,
                    Err(_) => return Err(CLOSED.to_string()),
                }
            }
            // Passed on before this thread took over the reading, perhaps.
            if let Ok(Some(answer)) = passed_on(answers) {
                io.calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                io.hand_on();
                return answer;
            }
            match io.read(Some(id)) {
                Read::Answer(answer) => {
                    io.calls
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id);
                    io.hand_on();
                    return answer;
                }
                Read::Request(other, _, request) => {
                    // This thread is busy: the request waits for a worker.
                    io.sched().queue.push_back((other, request));
                    io.hand_on();
                }
                Read::Closed => {
                    io.close();
                    return Err(CLOSED.to_string());
                }
            }
        }
    }
}

impl ScriptServices for IpcServices {
    fn call(&self, plugin: &str, call: ServiceCall) -> Result<ServiceReply, String> {
        let id = SERVING
            .get()
            .ok_or_else(|| "a plugin called the server with no request being served".to_string())?;
        let (tx, rx) = mpsc::channel();
        self.io
            .calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        let message = FromHost {
            id,
            message: HostMessage::Call {
                plugin: plugin.to_string(),
                call,
            },
        };
        let result = match send(&self.io.writer, &message) {
            Ok(()) => self.answer(id, &rx),
            Err(e) => Err(format!("could not reach the server: {e}")),
        };
        self.io
            .calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        result
    }
}

/// Handle a single request against the child's current plugins.
fn serve(state: &ChildState, request: HostRequest<'static>) -> HostResponse {
    // Taken once, so that the request is served by one set of plugins
    // however long it takes. (A reload waits for the requests in flight in
    // the server, so none are in flight here when one comes.)
    let runner = || {
        state
            .runner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    };
    match request {
        HostRequest::Reload { sources } => {
            let count = sources.len();
            match WasmScriptRunner::from_sources_with(&sources, Some(state.services.clone())) {
                Ok(new_runner) => {
                    *state.runner.write().unwrap_or_else(|e| e.into_inner()) =
                        Some(Arc::new(new_runner));
                    crate::scripting::wasm::retain_components(
                        &sources.iter().map(|s| s.component.hash).collect(),
                    );
                    HostResponse::Reloaded { loaded: count }
                }
                Err(e) => {
                    // Keep the old plugins: a broken edit to a plugin should not
                    // switch off the plugins that were working, just as an
                    // invalid edit to the config file leaves the last good
                    // settings in force.
                    HostResponse::Failed {
                        message: format!("{e}"),
                    }
                }
            }
        }

        HostRequest::TransformEntry { entry } => {
            let mut entry = entry.into_owned();
            let Some(r) = runner() else {
                return HostResponse::Entry {
                    patch: Some(EntryPatch::default()),
                    runs: Vec::new(),
                };
            };
            let mut runs = Vec::new();
            let changes = r.transform_entry(&mut entry, |index, _, seconds| {
                runs.push(PluginTime {
                    plugin: index as u32,
                    seconds,
                })
            });
            HostResponse::Entry {
                // Only what changed goes back: the server has the rest.
                patch: changes.map(|changes| EntryPatch::take(&mut entry, changes)),
                runs,
            }
        }

        HostRequest::EntryParsed { entry } => {
            if let Some(r) = runner() {
                r.dispatch_observe(Event::EntryParsed, EventPayload::Entry(entry.into_owned()));
            }
            HostResponse::Ack
        }

        HostRequest::Schedule { schedule } => match runner() {
            None => HostResponse::Schedule { decision: None },
            Some(r) => match r.dispatch_schedule(schedule) {
                Ok(decision) => HostResponse::Schedule { decision },
                Err(e) => HostResponse::Failed {
                    message: format!("{e}"),
                },
            },
        },

        HostRequest::Observe { event, payload } => {
            if let Some(r) = runner() {
                r.dispatch_observe(event, payload);
            }
            HostResponse::Ack
        }

        HostRequest::Scan { scan_id, entries } => match runner() {
            None => HostResponse::Scanned { entries: None },
            Some(r) => match r.dispatch_scan(scan_id, entries) {
                Ok(entries) => HostResponse::Scanned { entries },
                Err(e) => HostResponse::Failed {
                    message: format!("{e}"),
                },
            },
        },

        HostRequest::FinishScan { scan_id, summary } => {
            if let Some(r) = runner() {
                r.finish_scan(scan_id, summary);
            }
            HostResponse::Ack
        }

        HostRequest::PutComponent { plugin, component } => put_component(&plugin, &component),

        HostRequest::CallResult { .. } => HostResponse::Failed {
            message: "a call result arrived with no call outstanding".to_string(),
        },
    }
}

/// Compile `component` and keep it for the next reload.
fn put_component(plugin: &str, component: &crate::scripting::WasmComponent) -> HostResponse {
    match crate::scripting::wasm::put_component(plugin, component) {
        Ok(()) => HostResponse::Ack,
        Err(e) => HostResponse::Failed {
            message: format!("{e}"),
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// A stored entry whose content is `content_bytes` long.
    fn entry(id: i64, content_bytes: usize) -> FeedEntry {
        FeedEntry {
            id: Some(id),
            feed_id: 1,
            syndication_format: "rss".to_string(),
            guid: format!("guid-{id}"),
            published_at: Some(0),
            title: format!("entry {id}"),
            url: None,
            content: Some("x".repeat(content_bytes)),
            authors: Vec::new(),
            categories: Vec::new(),
            tags: Vec::new(),
            cache_assets: true,
        }
    }

    /// The request the server would send for `entries`.
    fn request_len(entries: &[FeedEntry]) -> usize {
        crate::process::ipc::encode(&ToHost {
            id: u64::MAX,
            busy: true,
            request: HostRequest::Scan {
                scan_id: u64::MAX,
                entries: entries.to_vec(),
            },
        })
        .unwrap()
        .len()
    }

    /// Loading WebAssembly plugins is given time in proportion to the work.
    #[test]
    fn timeouts_grow_with_the_work_of_loading_plugins() {
        use crate::scripting::{ScriptSource, WasmComponent, WASM_LOAD_BUDGET};
        let put = |len: usize| HostRequest::PutComponent {
            plugin: "p".to_string(),
            component: WasmComponent::new(vec![0; len]),
        };
        assert_eq!(
            request_timeout(&put(10), false),
            IPC_TIMEOUT + COMPILE_TIME_PER_MIB
        );
        assert_eq!(
            request_timeout(&put(7 * 1024 * 1024), false),
            IPC_TIMEOUT + COMPILE_TIME_PER_MIB * 7
        );

        let reload = HostRequest::Reload {
            sources: vec![ScriptSource::new(vec![]), ScriptSource::new(vec![])],
        };
        assert_eq!(
            request_timeout(&reload, false),
            IPC_TIMEOUT + WASM_LOAD_BUDGET * 2
        );

        let event = HostRequest::TransformEntry {
            entry: Cow::Owned(entry(1, 1)),
        };
        assert_eq!(request_timeout(&event, false), IPC_TIMEOUT);
        assert_eq!(
            request_timeout(&event, true),
            IPC_TIMEOUT + WASM_LOAD_BUDGET
        );
    }

    #[test]
    fn small_entries_are_all_sent() {
        let entries: Vec<_> = (1..=25).map(|id| entry(id, 1_000)).collect();
        assert_eq!(scan_prefix_len(&entries, SCAN_REQUEST_BUDGET).unwrap(), 25);
        assert_eq!(scan_prefix_len(&[], SCAN_REQUEST_BUDGET).unwrap(), 0);
    }

    /// Entries that are large together are split over several requests,
    /// each within the budget.
    #[test]
    fn large_entries_are_split_to_fit_the_budget() {
        // 25 entries of 480 KiB, about 12 MB in all, as on nixdev.
        let mut entries: Vec<_> = (1..=25).map(|id| entry(id, 480 * 1024)).collect();
        let mut requests = 0;
        while !entries.is_empty() {
            let fit = scan_prefix_len(&entries, SCAN_REQUEST_BUDGET).unwrap();
            assert!(fit > 0);
            assert!(request_len(&entries[..fit]) <= SCAN_REQUEST_BUDGET);
            if fit < entries.len() {
                assert!(request_len(&entries[..=fit]) > SCAN_REQUEST_BUDGET);
            }
            entries.drain(..fit);
            requests += 1;
        }
        assert!(requests > 1);
    }

    /// An entry over the budget but within a frame still goes, alone.
    #[test]
    fn a_first_entry_over_the_budget_is_sent_alone() {
        let entries = [entry(1, SCAN_REQUEST_BUDGET + 1), entry(2, 10)];
        assert_eq!(scan_prefix_len(&entries, SCAN_REQUEST_BUDGET).unwrap(), 1);
        assert!(request_len(&entries[..1]) <= MAX_FRAME_BYTES);
    }

    /// An entry too large for a frame can't be sent at all.
    #[test]
    fn a_first_entry_over_the_frame_limit_is_not_sent() {
        let entries = [entry(1, MAX_FRAME_BYTES), entry(2, 10)];
        assert_eq!(scan_prefix_len(&entries, SCAN_REQUEST_BUDGET).unwrap(), 0);
        assert_eq!(
            scan_prefix_len(&entries[1..], SCAN_REQUEST_BUDGET).unwrap(),
            1
        );
    }

    /// The fixture plugin, named `name`, with `config`.
    fn fixture(name: &str, config: serde_json::Value) -> ScriptSource {
        ScriptSource {
            name: name.to_string(),
            config: config.to_string(),
            ..ScriptSource::new(crate::test::WASM_FIXTURE.clone())
        }
    }

    /// Services that count the calls plugins make, and answer every one as
    /// done.
    #[derive(Default)]
    struct Counter(AtomicU64);

    impl ScriptServices for Counter {
        fn call(&self, _: &str, call: ServiceCall) -> Result<ServiceReply, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(match call {
                ServiceCall::StoreGet { .. } => ServiceReply::Value(None),
                _ => ServiceReply::Done,
            })
        }
    }

    /// A host whose child is served by a thread of this process, loaded
    /// with `sources`.
    fn host_in_thread(sources: Vec<ScriptSource>) -> (Arc<ScriptHost>, Arc<Counter>) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        std::thread::spawn(move || serve_channel(theirs).unwrap());
        let host = Arc::new(ScriptHost::over(ours, None).unwrap());
        let counter = Arc::new(Counter::default());
        host.set_services(counter.clone());
        host.reload(sources).unwrap();
        (host, counter)
    }

    /// An entry being ingested, titled `title`.
    fn new_entry(title: &str) -> FeedEntry {
        FeedEntry {
            id: None,
            title: title.to_string(),
            ..entry(1, 100)
        }
    }

    /// A request is answered as soon as it is done, however long another
    /// request sent before it takes.
    #[test]
    fn requests_are_answered_as_they_finish() {
        let dropper = fixture("dropper", serde_json::json!({ "drop_title": "x" }));
        let mut slow = fixture(
            "slow",
            serde_json::json!({ "spin_title": "spin", "suffix": "!" }),
        );
        slow.time_budget = crate::scripting::TimeBudget::Millis(600);
        let (host, _) = host_in_thread(vec![dropper, slow]);
        let runner = SubprocessScriptRunner::new(host.clone());
        assert!(runner.handles(Event::EntryIngest));
        std::thread::scope(|s| {
            let spinning = s.spawn(|| runner.dispatch_transform_entry(new_entry("spin")));
            std::thread::sleep(Duration::from_millis(150));
            let start = Instant::now();
            assert!(runner
                .dispatch_transform_entry(new_entry("x"))
                .unwrap()
                .is_none());
            assert!(
                start.elapsed() < Duration::from_millis(300),
                "waited {:?} behind the other request",
                start.elapsed()
            );
            // The slow plugin ran out of time: the entry passes through it
            // unmodified, but whole.
            let spun = spinning.join().unwrap().unwrap().unwrap();
            assert_eq!(spun.title, "spin");
            assert_eq!(spun.content, Some("x".repeat(100)));
        });
        // Entries come back with what plugins changed applied, and the
        // runs are reported under the plugins' names.
        let mut entry = new_entry("t");
        let mut runs = Vec::new();
        assert!(runner
            .dispatch_transform_entry_timed(&mut entry, &mut runs)
            .unwrap());
        assert_eq!(entry.title, "t!");
        assert_eq!(entry.content, Some("x".repeat(100)));
        let names: Vec<&str> = runs.iter().map(|r| &*r.plugin).collect();
        assert_eq!(names, ["dropper", "slow"]);
        assert!(host.is_alive());
    }

    /// The calls plugins make while many requests are in flight are each
    /// answered to the request that made them.
    #[test]
    fn calls_reach_the_requests_that_made_them() {
        // Every entry ingested makes a call, to store what the regex found.
        let config = serde_json::json!({ "regex": ["t", ""], "suffix": "!" });
        let (host, counter) =
            host_in_thread(vec![fixture("a", config.clone()), fixture("b", config)]);
        let runner = SubprocessScriptRunner::new(host.clone());
        std::thread::scope(|s| {
            for t in 0..4 {
                let runner = &runner;
                s.spawn(move || {
                    for i in 0..25 {
                        let title = format!("t{t}-{i}");
                        let out = runner
                            .dispatch_transform_entry(new_entry(&title))
                            .unwrap()
                            .unwrap();
                        assert_eq!(out.title, format!("{title}!!"));
                    }
                });
            }
        });
        assert_eq!(counter.0.load(Ordering::SeqCst), 4 * 25 * 2);
        assert!(host.is_alive());
    }

    /// A child that goes away retires the host: the request in flight
    /// fails, entries pass through unmodified, and later requests fail at
    /// once.
    #[test]
    fn a_child_that_goes_away_retires_the_host() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let child = std::thread::spawn(move || {
            // Read one request, then hang up without answering.
            let mut reader = BufReader::new(theirs);
            crate::process::ipc::read_frame(&mut reader).unwrap();
        });
        let host = Arc::new(ScriptHost::over(ours, None).unwrap());
        let runner = SubprocessScriptRunner::new(host.clone());
        let out = runner
            .dispatch_transform_entry(new_entry("t"))
            .unwrap()
            .unwrap();
        assert_eq!(out.title, "t");
        child.join().unwrap();
        assert!(!host.is_alive());
        assert!(!runner.handles(Event::EntryIngest));
        assert!(matches!(
            host.request(&HostRequest::FinishScan {
                scan_id: 1,
                summary: None
            }),
            Err(HostError::Dead)
        ));
    }

    /// A frame about a request that is not in flight means the two sides
    /// are out of step: the request that reads it fails, and the host is
    /// retired.
    #[test]
    fn a_frame_for_no_request_retires_the_host() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let host = ScriptHost::over(ours, None).unwrap();
        let writer = Mutex::new(theirs);
        let stray = FromHost {
            id: 42,
            message: HostMessage::Done {
                response: HostResponse::Ack,
                subscribed: Subscriptions {
                    seq: 1,
                    events: EventSet::default(),
                },
            },
        };
        send(&writer, &stray).unwrap();
        let result = host.request(&HostRequest::FinishScan {
            scan_id: 1,
            summary: None,
        });
        assert!(matches!(result, Err(HostError::Protocol(_))), "{result:?}");
        assert!(!host.is_alive());
    }

    /// What was passed on to a request is found however many notices to
    /// take over the reading were queued ahead of it, on either side.
    #[test]
    fn answers_behind_hand_offs_are_found() {
        let (tx, rx) = mpsc::channel();
        tx.send(Inbound::Lead).unwrap();
        tx.send(Inbound::Lead).unwrap();
        tx.send(Inbound::Retired("gone".to_string())).unwrap();
        assert!(matches!(
            ScriptHost::passed_on(&rx),
            Ok(Some(Inbound::Retired(_)))
        ));
        assert!(matches!(ScriptHost::passed_on(&rx), Ok(None)));

        let (tx, rx) = mpsc::channel();
        tx.send(CallInbound::Lead).unwrap();
        tx.send(CallInbound::Answer(Ok(ServiceReply::Done)))
            .unwrap();
        assert_eq!(passed_on(&rx), Ok(Some(Ok(ServiceReply::Done))));
        drop(tx);
        assert_eq!(passed_on(&rx), Err(()));
    }

    /// Of reports of what plugins handle, the most recent counts, whatever
    /// order they arrive in.
    #[test]
    fn the_most_recent_subscriptions_win() {
        let report = |seq, event| {
            let mut events = EventSet::default();
            events.insert(event);
            packed(Subscriptions { seq, events })
        };
        let latest = AtomicU64::new(0);
        latest.fetch_max(report(2, Event::Timer), Ordering::SeqCst);
        latest.fetch_max(report(1, Event::EntryIngest), Ordering::SeqCst);
        let events = EventSet::from_bits(latest.load(Ordering::SeqCst) as u16);
        assert!(events.contains(Event::Timer));
        assert!(!events.contains(Event::EntryIngest));
    }
}
