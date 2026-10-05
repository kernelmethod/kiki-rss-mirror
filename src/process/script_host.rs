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
//! WebAssembly plugins run there too, compiled to native code with
//! Cranelift. That makes the child the one Kiki process allowed memory that
//! is first writable and then executable (see [`crate::sandbox`]), and the
//! compiler one more body of code that handles what plugins supply.
//!
//! This module moves the VM into a child process that holds nothing: no
//! database, no filesystem (Landlock with an empty ruleset), and no way
//! to open a socket (seccomp). Its entire view of the world is one
//! inherited socket pair and whatever the server chooses to send down it.
//! What plugins ask of the server (`kiki.store`, `kiki.entries`,
//! `kiki.feeds`) goes back up that socket as a [`FromHost::Call`], and
//! the server decides how to answer: the child never touches the database
//! itself.
//!
//! # Roles
//!
//! * [`ScriptHost`] and [`SubprocessScriptRunner`] run in the server.
//!   The runner implements [`ScriptRunner`], so callers dispatch events
//!   exactly as they did against the in-process VM. Every response says
//!   which events the child's plugins have handlers for, and the runner
//!   sends nothing for the rest: an event no handler would see costs no
//!   round trip.
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

use crate::process::ipc::{
    decode, encode, read_frame, write_frame, FromHost, HostRequest, HostResponse, MAX_FRAME_BYTES,
};
use crate::scripting::composite::CompositeRunner;
use crate::scripting::{
    Event, EventPayload, EventSet, FeedEntry, FetchSchedule, ScanSummary, ScheduleDecision,
    ScriptRunner, ScriptServices, ScriptSource, ServiceCall, ServiceReply,
};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::io::{self, BufReader};
use std::os::unix::io::RawFd;
use std::os::unix::net::UnixStream;
use std::process::Child;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
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

/// How long the server waits for a response before declaring the host
/// dead.
///
/// Generous next to the child's own default per-handler budget
/// ([`SCRIPT_TIMEOUT_MS`]) so that a slow chain of handlers, or a reload
/// compiling many scripts, is never mistaken for a hung child — but
/// short enough that a genuinely wedged host cannot pin a feed worker
/// indefinitely. It is also the only limit on the handlers of a plugin
/// whose budget is [`TimeBudget::Unlimited`].
///
/// [`SCRIPT_TIMEOUT_MS`]: crate::scripting::lua::SCRIPT_TIMEOUT_MS
/// [`TimeBudget::Unlimited`]: crate::scripting::TimeBudget::Unlimited
pub const IPC_TIMEOUT: Duration = Duration::from_secs(10);

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

/// One end of the script host's socket.
///
/// Reads are buffered, so that a frame small enough for the buffer — most
/// of them — arrives in one `recv` rather than one for its length and
/// another for its body. The protocol is lockstep, so the peer never sends
/// ahead of what this side is about to read.
struct Channel {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Channel {
    fn new(stream: UnixStream) -> io::Result<Self> {
        Ok(Channel {
            writer: stream.try_clone()?,
            reader: BufReader::new(stream),
        })
    }
}

/// The live half of a [`ScriptHost`]: the child and the socket to it.
struct Live {
    channel: Channel,
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
    /// Answers the calls plugins make while a request is served. Calls
    /// fail until the server sets it with [`Self::set_services`].
    services: RwLock<Option<Arc<dyn ScriptServices>>>,
    /// The [`EventSet::to_bits`] of the events the child's plugins have
    /// handlers for, as of its last response: every event until it has
    /// answered once, and none once the host is retired.
    subscribed: AtomicU16,
    /// The hashes of the WebAssembly components the child has compiled and
    /// kept, which sources can name without sending them again.
    components: Mutex<HashSet<[u8; 32]>>,
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
        let (ours, child) =
            crate::process::spawn_child(SUBCOMMAND, HOST_FD_ENV, log_only, no_sandbox)
                .context("spawning the script host process")?;

        ours.set_read_timeout(Some(IPC_TIMEOUT))
            .context("setting the script host read timeout")?;
        ours.set_write_timeout(Some(IPC_TIMEOUT))
            .context("setting the script host write timeout")?;

        let channel = Channel::new(ours).context("cloning the script host socket")?;

        info!(pid = child.id(), "script host: spawned");
        Ok(ScriptHost {
            state: Mutex::new(Some(Live { channel, child })),
            services: RwLock::new(None),
            subscribed: AtomicU16::new(EventSet::ALL.to_bits()),
            components: Mutex::new(HashSet::new()),
        })
    }

    /// Answer the calls plugins make through `kiki.store`, `kiki.entries`
    /// and `kiki.feeds` with `services`.
    pub fn set_services(&self, services: Arc<dyn ScriptServices>) {
        *self.services.write().unwrap_or_else(|e| e.into_inner()) = Some(services);
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

        let services = self
            .services
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let result = exchange(&mut live.channel, request, services.as_deref());
        if let Ok((_, subscribed)) = &result {
            self.subscribed
                .store(subscribed.to_bits(), Ordering::Relaxed);
        }
        match result {
            Ok((HostResponse::Failed { message }, _)) => Err(HostError::Failed(message)),
            Ok((response, _)) => Ok(response),
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
                self.subscribed
                    .store(EventSet::default().to_bits(), Ordering::Relaxed);
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    /// Rebuild the child's VM from `sources`, replacing whatever it was
    /// running. If `sources` fail to compile, the child keeps running the
    /// VM it had.
    ///
    /// Each WebAssembly component the child has not compiled yet is sent
    /// first, in a message of its own; the sources then name it by hash.
    ///
    /// Returns the number of scripts the child compiled.
    pub fn reload(&self, mut sources: Vec<ScriptSource>) -> Result<usize, HostError> {
        let mut used = HashSet::new();
        for source in &mut sources {
            let Some(component) = source.component.as_mut() else {
                continue;
            };
            used.insert(component.hash);
            let known = self
                .components
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&component.hash);
            if !known {
                let put = HostRequest::PutComponent {
                    component: component.clone(),
                };
                match self.request(&put)? {
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
        match self.request(&HostRequest::Reload { sources })? {
            HostResponse::Reloaded { loaded } => {
                // The child forgets the components no plugin uses any more.
                self.components
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .retain(|hash| used.contains(hash));
                Ok(loaded)
            }
            other => Err(HostError::Protocol(format!(
                "expected Reloaded, got {other:?}"
            ))),
        }
    }

    /// The events the child's plugins had handlers for as of its last
    /// response. Every event until it has answered once; none once the
    /// host is retired, since nothing sent to it would be served.
    pub fn subscribed(&self) -> EventSet {
        EventSet::from_bits(self.subscribed.load(Ordering::Relaxed))
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
        if let Some(live) = live {
            // Closing our end is the child's shutdown signal: its next
            // read returns EOF and it exits.
            drop(live.channel);
            crate::process::reap(live.child);
        }
    }
}

/// Write one request and read its response, answering the calls the host
/// makes with `services` along the way. Returns the response, and the
/// events the host has handlers for now that it has served the request.
///
/// Failures before the first byte goes out are [`HostError::Failed`], not
/// [`HostError::Io`] — see [`HostError::is_fatal`].
fn exchange(
    channel: &mut Channel,
    request: &HostRequest,
    services: Option<&dyn ScriptServices>,
) -> Result<(HostResponse, EventSet), HostError> {
    let encoded =
        encode(request).map_err(|e| HostError::Failed(format!("could not encode request: {e}")))?;
    if encoded.len() > MAX_FRAME_BYTES {
        return Err(HostError::Failed(format!(
            "request of {} bytes exceeds the {} byte frame limit",
            encoded.len(),
            MAX_FRAME_BYTES
        )));
    }
    write_frame(&mut channel.writer, &encoded)?;
    loop {
        let frame = read_frame(&mut channel.reader)?;
        let (plugin, call) = match decode(&frame)
            .map_err(|e| HostError::Protocol(format!("decoding response: {e}")))?
        {
            FromHost::Done {
                response,
                subscribed,
            } => return Ok((response, subscribed)),
            FromHost::Call { plugin, call } => (plugin, call),
        };
        let result = match services {
            Some(services) => services.call(&plugin, call),
            None => Err("not available: the server is not answering plugin calls".to_string()),
        };
        // An answer that cannot be sent would leave the host waiting for
        // one, so failing to send it is fatal to the channel.
        let encoded = encode(&HostRequest::CallResult { result })
            .map_err(|e| HostError::Protocol(format!("encoding a call result: {e}")))?;
        write_frame(&mut channel.writer, &encoded)?;
    }
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
    fn handles(&self, event: Event) -> bool {
        self.host.subscribed().contains(event)
    }

    fn dispatch_transform_entry(&self, entry: FeedEntry) -> Result<Option<FeedEntry>> {
        if !self.handles(Event::EntryIngest) {
            return Ok(Some(entry));
        }
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
/// variant, the scan id and the list's length, each a varint.
const SCAN_REQUEST_OVERHEAD: usize = 32;

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
        total += encode(entry)
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
            "script host: sandbox disabled via --no-sandbox; plugins run with full \
             filesystem and syscall access"
        );
    } else {
        crate::sandbox::apply(&crate::sandbox::SandboxConfig::script_host(log_only))
            .context("failed to install the script host sandbox")?;
    }

    let channel = Channel::new(crate::process::take_parent_socket(SUBCOMMAND)?)
        .context("cloning the parent socket")?;
    let channel = Arc::new(Mutex::new(channel));
    let services: Arc<dyn ScriptServices> = Arc::new(IpcServices {
        channel: channel.clone(),
    });

    let mut runner: Option<CompositeRunner> = None;

    loop {
        let frame = match read_frame(&mut channel.lock().unwrap_or_else(|e| e.into_inner()).reader)
        {
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

        let response = match decode::<HostRequest>(&frame) {
            Ok(request) => serve(&mut runner, &services, request),
            Err(e) => HostResponse::Failed {
                message: format!("undecodable request: {e}"),
            },
        };

        let subscribed = runner
            .as_ref()
            .map_or_else(EventSet::default, |r| r.subscriptions());
        let encoded = match encode(&FromHost::Done {
            response,
            subscribed,
        }) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "script host: could not encode a response, exiting");
                return Ok(());
            }
        };
        if let Err(e) = write_frame(
            &mut channel.lock().unwrap_or_else(|e| e.into_inner()).writer,
            &encoded,
        ) {
            warn!(error = %e, "script host: write failed, exiting");
            return Ok(());
        }
    }
}

/// Answers plugins' calls in the child by asking the server over the
/// channel, in the middle of the request being served.
struct IpcServices {
    channel: Arc<Mutex<Channel>>,
}

impl ScriptServices for IpcServices {
    fn call(&self, plugin: &str, call: ServiceCall) -> Result<ServiceReply, String> {
        let mut channel = self.channel.lock().unwrap_or_else(|e| e.into_inner());
        let encoded = encode(&FromHost::Call {
            plugin: plugin.to_string(),
            call,
        })
        .map_err(|e| format!("could not encode the call: {e}"))?;
        write_frame(&mut channel.writer, &encoded)
            .map_err(|e| format!("could not reach the server: {e}"))?;
        let frame = read_frame(&mut channel.reader)
            .map_err(|e| format!("could not reach the server: {e}"))?;
        match decode::<HostRequest>(&frame) {
            Ok(HostRequest::CallResult { result }) => result,
            Ok(other) => Err(format!(
                "the server sent {other:?} instead of a call result"
            )),
            Err(e) => Err(format!("undecodable call result: {e}")),
        }
    }
}

/// Handle a single request against the child's current VM.
fn serve(
    runner: &mut Option<CompositeRunner>,
    services: &Arc<dyn ScriptServices>,
    request: HostRequest,
) -> HostResponse {
    match request {
        HostRequest::Reload { sources } => {
            let count = sources.len();
            match CompositeRunner::from_sources_with(&sources, Some(services.clone())) {
                Ok(new_runner) => {
                    *runner = Some(new_runner);
                    #[cfg(feature = "wasm-plugins")]
                    crate::scripting::wasm::retain_components(
                        &sources
                            .iter()
                            .filter_map(|s| s.component.as_ref().map(|c| c.hash))
                            .collect(),
                    );
                    HostResponse::Reloaded { loaded: count }
                }
                Err(e) => {
                    // Keep the old VM: a broken edit to a plugin should not
                    // switch off the plugins that were working, just as an
                    // invalid edit to the config file leaves the last good
                    // settings in force.
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

        HostRequest::Schedule { schedule } => match runner.as_ref() {
            None => HostResponse::Schedule { decision: None },
            Some(r) => match r.dispatch_schedule(schedule) {
                Ok(decision) => HostResponse::Schedule { decision },
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

        HostRequest::Scan { scan_id, entries } => match runner.as_ref() {
            None => HostResponse::Scanned { entries: None },
            Some(r) => match r.dispatch_scan(scan_id, entries) {
                Ok(entries) => HostResponse::Scanned { entries },
                Err(e) => HostResponse::Failed {
                    message: format!("{e}"),
                },
            },
        },

        HostRequest::FinishScan { scan_id, summary } => {
            if let Some(r) = runner.as_ref() {
                r.finish_scan(scan_id, summary);
            }
            HostResponse::Ack
        }

        HostRequest::PutComponent { component } => put_component(&component),

        HostRequest::CallResult { .. } => HostResponse::Failed {
            message: "a call result arrived with no call outstanding".to_string(),
        },
    }
}

/// Compile `component` and keep it for the next reload.
#[cfg(feature = "wasm-plugins")]
fn put_component(component: &crate::scripting::WasmComponent) -> HostResponse {
    match crate::scripting::wasm::put_component(component) {
        Ok(()) => HostResponse::Ack,
        Err(e) => HostResponse::Failed {
            message: format!("{e}"),
        },
    }
}

/// See the variant of this function built with the `wasm-plugins` feature.
#[cfg(not(feature = "wasm-plugins"))]
fn put_component(_: &crate::scripting::WasmComponent) -> HostResponse {
    HostResponse::Failed {
        message: "this build of Kiki cannot run WebAssembly plugins".to_string(),
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
        encode(&HostRequest::Scan {
            scan_id: u64::MAX,
            entries: entries.to_vec(),
        })
        .unwrap()
        .len()
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
}
