//! The feed fetcher process, and the server-side client that drives it.
//!
//! Fetching a feed means talking to an arbitrary server on the internet
//! and then parsing whatever it sent: TLS, HTTP framing, three
//! decompressors, and two XML-based parsers all run over bytes an
//! attacker may control. Before this process existed all of that ran in
//! the server, next to the SQLite handle, the asset cache and — in UDS
//! mode — the listening socket that is Kiki's only access control.
//!
//! Caching assets is the same kind of work — downloading entries' images
//! and enclosures, and feeds' favicons, and parsing HTML to find them — so
//! it runs here too ([`Job::FetchAsset`], [`Job::FindPageIcons`],
//! [`Job::ExtractImages`]), and the server holds no HTTP client at all.
//!
//! This module moves that work into a child that holds none of those: no
//! database, no writable filesystem (Landlock grants only read access to
//! the TLS trust stores), no way to create a Unix socket, bind, or listen
//! for connections (seccomp), and it gets from the server only a
//! [`FetchSpec`] per request. What it sends back is plain data — a
//! [`FetchReply`] — that the server validates and writes itself.
//!
//! Within the child, downloading and parsing are kept apart too. The
//! process that downloads needs the network, and is handed the feeds'
//! credentials; the code that parses — XML, HTML, SVG — is where a bug in
//! the face of hostile input is likeliest, and needs neither. So parsing
//! happens in a process of its own, the **parser**, which may not create
//! or connect a socket of any kind, cannot open a single file, and never
//! sees a [`FetchSpec`]: only the bytes to parse ([`ParseTask`]). A parser
//! compromised by a feed cannot reach the network, the LAN, or another
//! feed's credentials; all it can do is lie in its answers, which the
//! server checks as it would anything from the fetcher.
//!
//! # Name resolution
//!
//! The worker does no DNS of its own. Its HTTP client's resolver sends
//! each hostname to the server ([`FromFetcher::Resolve`]), which looks it
//! up with the system resolver and answers with addresses
//! ([`ToFetcher::Resolved`]). Hostname lookup therefore behaves exactly as
//! it does for every other program on the host — including setups that
//! resolve through a local daemon over a Unix socket (nscd, sssd,
//! systemd-resolved), which the fetcher's sandbox could not reach — and
//! the fetcher needs neither the resolver's configuration files nor the
//! syscalls `getaddrinfo` makes.
//!
//! This is for compatibility and a smaller sandbox, not an access control:
//! the worker can still connect to any address it is given, or that a feed
//! names by IP.
//!
//! # Processes
//!
//! `kiki __feed-fetcher` is a small **supervisor**. It installs the
//! sandbox, then `fork`s a **worker** that does the downloading and a
//! **parser** that does the parsing. Both inherit the sandbox, and the
//! parser tightens it further (see [`SandboxProfile::FeedParser`]). Neither
//! has a descriptor to the server or to each other, only a socket pair
//! each to the supervisor, which relays frames between the three: requests
//! and their answers between the server and the worker, and parse tasks
//! and their results between the worker and the parser.
//!
//! [`SandboxProfile::FeedParser`]: crate::sandbox::SandboxProfile::FeedParser
//!
//! The parser carries out tasks on a pool of threads, and the supervisor
//! never gives it more tasks at once than it has threads, holding the rest
//! back in a queue, so that every task it has in hand is being worked on.
//! That lets the supervisor put a time limit on each task: a parser that
//! takes longer than [`PARSE_TIMEOUT`] over one is killed.
//!
//! The split exists so the fetcher can be *replaced*. The server cannot
//! spawn anything once its own sandbox is up (every profile denies
//! `execve`), which is acceptable for the optional script host but not
//! for fetching, which is Kiki's core job. `fork` is not denied, so the
//! supervisor — single-threaded, and never touching untrusted input —
//! forks a fresh worker whenever the last one dies. The supervisor tracks
//! which requests were in flight and answers each of them with
//! [`JobResult::WorkerExited`]. Any of them may be what killed the worker,
//! so the server retries them one at a time: a request that kills the
//! worker again while it is alone is to blame, and its feed is recorded as
//! crashing the fetcher, while the others are served as if nothing had
//! happened.
//!
//! The parser is replaced the same way when it dies or is killed, and its
//! tasks are found out the same way, by the worker this time: each task
//! the parser had in hand is answered with [`ParseReply::Exited`], and the
//! worker retries them one at a time. A task that kills the parser while
//! it is alone fails its request with [`JobResult::Crashed`], which blames
//! the feed just as a worker crash would. When the worker dies, the parser
//! is replaced along with it, since everything it had in hand was the dead
//! worker's.
//!
//! Both children are forked from the supervisor, and so start with a copy
//! of its memory. The supervisor clears every request it relays from the
//! server as soon as it has passed it on, so that a parser forked after it
//! does not inherit the credentials in it.
//!
//! # Protocol
//!
//! Unlike the script host's lockstep request/response stream, fetches are
//! slow and concurrent, so the channel is multiplexed, and requests flow
//! both ways. The server sends [`ToFetcher`] frames — a [`Request`] for
//! work, or the answer to a lookup — and receives [`FromFetcher`] frames —
//! a [`Response`] to its request, or a lookup of the worker's own. Each
//! side picks the ids for the requests it starts, and answers carry them
//! back, in any order. Frames use [`crate::process::ipc`]'s
//! length-prefixed framing, capped at [`MAX_FRAME_BYTES`].
//!
//! The worker's parse tasks travel in the same two enums, which keeps the
//! supervisor's routing to a glance at each frame's kind and id: the
//! worker sends a [`FromFetcher::Parse`], which the supervisor passes to
//! the parser rather than to the server, and the parser's
//! [`ToFetcher::Parsed`] goes back to the worker. Neither ever reaches the
//! server, and the supervisor drops a `Parsed` that comes from it.

use crate::fetcher::assets::{
    asset_client_builder, fetch_asset, find_page_icons, AssetReply, AssetSpec, AssetTimeouts,
    PageIcons, PageSpec,
};
use crate::fetcher::parsing::{ParseOutput, ParseReply, ParseTask};
use crate::fetcher::{
    client_builder, fetch_with, FetchReply, FetchSpec, FetcherError, ParseFailure, ParseOutcome,
    Parsers, ProxiedClient, MAX_REDIRECTS,
};
use crate::process::ipc::{
    decode, decode_prefix, encode, read_frame_async, read_frame_limited, write_frame_async,
    write_frame_limited,
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::net::{Shutdown, SocketAddr, ToSocketAddrs};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::process::Child;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, watch};
use tracing::{debug, info, warn};

/// The hidden subcommand the server re-execs itself with.
pub const SUBCOMMAND: &str = "__feed-fetcher";

/// Environment variable set on the child, naming [`CHILD_FD`](crate::process::CHILD_FD).
///
/// As for the script host, purely informational: it lets a hand-run
/// `kiki __feed-fetcher` refuse with a clear message.
pub const HOST_FD_ENV: &str = "KIKI_FEED_FETCHER_FD";

/// How many times the server sends a job that was in hand when a worker
/// died, one suspect at a time, before giving up on it.
///
/// A job that kills the worker when it is the only one in hand is to
/// blame; one that is not alone when the worker dies again is retried.
const ISOLATED_ATTEMPTS: usize = 3;

/// Largest frame either side will write or accept.
///
/// A response carries a whole parsed feed, whose body is capped upstream
/// by the `max_feed_bytes` setting (32 MiB by default); a `Parse` request
/// carries a `file://` body, which is not capped at all. Postcard adds
/// only a few bytes of overhead per field, so the cap here is a multiple
/// of the default to leave room for raised settings; a reply that still
/// does not fit is turned into an error by the worker rather than sent.
pub const MAX_FRAME_BYTES: usize = 128 * 1024 * 1024;

/// Extra time the server allows past the fetch's own timeouts before it
/// stops waiting: time to parse, and to cross two process boundaries.
const DEADLINE_SLACK: Duration = Duration::from_secs(30);

/// How long the server waits for a `file://` body to be parsed, or for an
/// entry's images to be found.
const PARSE_DEADLINE: Duration = Duration::from_secs(60);

/// How long the server waits for an asset to be downloaded, or a page to
/// be searched for icons: the requests' own overall limit, plus slack.
const ASSET_DEADLINE: Duration = AssetTimeouts::DEFAULT.total.saturating_add(DEADLINE_SLACK);

/// How long the supervisor waits for a worker to finish a frame it has
/// started writing, or to accept one, before declaring it wedged.
const WORKER_IO_TIMEOUT: Duration = Duration::from_secs(30);

/// A worker that lived at least this long before dying is not counted as
/// part of a crash loop.
const HEALTHY_WORKER_LIFETIME: Duration = Duration::from_secs(60);

/// Ceiling on the delay between respawns of a crash-looping worker.
const MAX_RESPAWN_DELAY: Duration = Duration::from_secs(30);

/// How long a worker waits for the server to answer a lookup.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(30);

/// Threads the server dedicates to answering the worker's lookups. Each
/// lookup blocks in `getaddrinfo`, so this bounds how many run at once.
const RESOLVER_THREADS: usize = 4;

/// Lookups the server will queue behind [`RESOLVER_THREADS`] before it
/// starts refusing them, so a misbehaving worker cannot make the server
/// buffer without limit.
const RESOLVER_QUEUE: usize = 256;

/// Longest hostname the server will look up (RFC 1035's limit on a full
/// domain name, in its dotted text form).
const MAX_HOSTNAME_LEN: usize = 253;

/// Longest the parser may spend on one task before the supervisor kills
/// it. Parsing even a feed of the largest size the frame limit allows
/// takes a few seconds; one that takes this long is stuck.
pub const PARSE_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest a parse task may wait in the supervisor's queue for one of the
/// parser's threads, or for a parser that is being restarted, before it
/// is refused.
const PARSE_QUEUE_LIMIT: Duration = Duration::from_secs(15);

/// How long the worker waits for the answer to a parse task: as long as
/// it may be queued and then worked on, with a little slack.
const PARSE_WAIT: Duration = PARSE_QUEUE_LIMIT
    .saturating_add(PARSE_TIMEOUT)
    .saturating_add(Duration::from_secs(10));

/// The most threads the parser runs, and so the most tasks it is given at
/// once.
const MAX_PARSER_THREADS: usize = 4;

/// How many threads the parser runs: one per CPU, up to
/// [`MAX_PARSER_THREADS`].
fn parser_threads() -> usize {
    std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .clamp(1, MAX_PARSER_THREADS)
}

/// A frame from the server to the fetcher.
#[derive(Debug, Serialize, Deserialize)]
pub enum ToFetcher {
    /// Work for the fetcher.
    Request(Request),

    /// The answer to a [`FromFetcher::Resolve`] with the same id: the
    /// host's addresses (port 0), or why they could not be found.
    Resolved {
        id: u64,
        result: Result<Vec<SocketAddr>, String>,
    },

    /// The answer to a [`FromFetcher::Parse`] with the same id, from the
    /// parser (or, if it died, the supervisor) to the worker.
    Parsed { id: u64, reply: ParseReply },
}

/// A frame from the fetcher to the server.
#[derive(Debug, Serialize, Deserialize)]
pub enum FromFetcher {
    /// The answer to a [`Request`].
    Response(Response),

    /// Ask the server to look up `host`. `id` is chosen by the worker.
    Resolve { id: u64, host: String },

    /// A task for the parser, from the worker. `id` is chosen by the
    /// worker.
    Parse { id: u64, task: ParseTask },
}

/// A message from the server to the fetcher.
#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    /// Chosen by the server, unique among its outstanding requests.
    pub id: u64,
    pub job: Job,
}

/// The work a [`Request`] asks for.
#[derive(Debug, Serialize, Deserialize)]
pub enum Job {
    /// Fetch a feed over HTTP(S) and parse it.
    Fetch(FetchSpec),

    /// Parse a body the server read itself (a `file://` feed).
    Parse {
        feed_id: i64,
        /// Encoded as one length and the raw bytes, not byte by byte.
        #[serde(with = "serde_bytes")]
        body: Vec<u8>,
    },

    /// Download an asset: an entry's image or enclosure, or a favicon.
    FetchAsset(AssetSpec),

    /// Fetch a web page and find the icons it links to.
    FindPageIcons(PageSpec),

    /// Find the images an entry's HTML `content` shows, resolving them
    /// against `base`.
    ExtractImages { content: String, base: String },
}

/// A message from the fetcher back to the server.
#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    /// The id of the [`Request`] this answers.
    pub id: u64,
    pub result: JobResult,
}

/// The outcome of a [`Job`].
#[derive(Debug, Serialize, Deserialize)]
pub enum JobResult {
    Fetched(FetchReply),
    Parsed(ParseOutcome),
    Asset(AssetReply),
    PageIcons(PageIcons),
    Images(Vec<String>),

    /// The job could not be completed: the task serving it panicked, the
    /// reply was too large to send, or the parser could not serve it.
    Failed {
        message: String,
    },

    /// What the job gave the parser to parse killed it, even when retried
    /// on its own; `why` says how the parser died. Sent by the worker.
    Crashed {
        why: String,
    },

    /// The worker died while it had this job, and `in_flight - 1` others,
    /// in hand; `why` says how it died. Sent by the supervisor, not the
    /// worker.
    ///
    /// Any one of those jobs may be what killed it, so this alone blames
    /// none of them: the server retries the job on its own to find out
    /// (see `FeedFetcherHost::request`).
    WorkerExited {
        in_flight: u32,
        why: String,
    },
}

/// Just the id of a message, for the supervisor, which needs to know what
/// is outstanding but has no reason to decode the rest.
///
/// Every message puts its id first, so [`decode_prefix`] reads it and
/// stops. The `Peek*` enums below mirror [`ToFetcher`] and [`FromFetcher`]
/// and must list their variants in the same order, since variants are
/// encoded by index.
#[derive(Deserialize)]
struct IdOnly {
    id: u64,
}

/// The kind and id of a [`ToFetcher`] frame.
#[derive(Deserialize)]
enum PeekTo {
    Request(IdOnly),
    Resolved(IdOnly),
    Parsed(IdOnly),
}

/// The kind and id of a [`FromFetcher`] frame.
#[derive(Deserialize)]
enum PeekFrom {
    Response(IdOnly),
    Resolve(IdOnly),
    Parse(IdOnly),
}

fn peek_to(frame: &[u8]) -> Option<PeekTo> {
    decode_prefix(frame).ok()
}

fn peek_from(frame: &[u8]) -> Option<PeekFrom> {
    decode_prefix(frame).ok()
}

/// Encode a frame the server sends, or fail the way a bad request does.
fn encode_to(msg: &ToFetcher) -> Result<Vec<u8>, FetcherError> {
    encode(msg).map_err(|e| FetcherError::Unavailable(format!("could not encode request: {e}")))
}

// ------------------------------------------------------------------
// Server side
// ------------------------------------------------------------------

/// Requests awaiting an answer, keyed by the id they were sent with.
///
/// Used on both sides of the channel: by the server for its requests, and
/// by the worker for its lookups. Closing it wakes every waiter with an
/// error and refuses later registrations, under one lock, so nothing is
/// ever left waiting on a channel that has already failed.
struct Pending<T> {
    waiters: Mutex<Option<HashMap<u64, oneshot::Sender<T>>>>,
    next_id: AtomicU64,
    /// Becomes `true` when the channel closes, for whoever needs to learn
    /// of it without having a request outstanding.
    closed: watch::Sender<bool>,
}

impl<T> Pending<T> {
    fn new() -> Self {
        Pending {
            waiters: Mutex::new(Some(HashMap::new())),
            next_id: AtomicU64::new(1),
            closed: watch::Sender::new(false),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<HashMap<u64, oneshot::Sender<T>>>> {
        self.waiters.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Allocate an id and a receiver for its answer, or `None` if closed.
    fn register(&self) -> Option<(u64, oneshot::Receiver<T>)> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.register_as(id).map(|rx| (id, rx))
    }

    /// A receiver for the answer to `id` once more, for a request resent
    /// under the id it was first given, or `None` if closed. `id` must
    /// have been allocated by [`Self::register`] and answered since.
    fn register_as(&self, id: u64) -> Option<oneshot::Receiver<T>> {
        let (tx, rx) = oneshot::channel();
        self.lock().as_mut()?.insert(id, tx);
        Some(rx)
    }

    /// Deliver the answer to `id`. Late answers, whose waiter has given
    /// up, are dropped.
    fn complete(&self, id: u64, value: T) {
        let waiter = self.lock().as_mut().and_then(|w| w.remove(&id));
        if let Some(w) = waiter {
            let _ = w.send(value);
        }
    }

    fn forget(&self, id: u64) {
        if let Some(w) = self.lock().as_mut() {
            w.remove(&id);
        }
    }

    /// Fail every waiter and refuse new ones. Returns whether this call
    /// was the one that closed it.
    fn close(&self) -> bool {
        let closed = self.lock().take().is_some();
        if closed {
            self.closed.send_replace(true);
        }
        closed
    }

    /// Wait until the channel has closed; see [`Self::close`].
    async fn closed(&self) {
        let mut rx = self.closed.subscribe();
        // Only fails if the sender is dropped, which `self` prevents.
        let _ = rx.wait_for(|closed| *closed).await;
    }

    fn is_open(&self) -> bool {
        self.lock().is_some()
    }
}

/// Close the server's side of the channel: every outstanding request
/// fails immediately, and every later one is refused.
fn retire(pending: &Pending<JobResult>, why: &str) {
    if pending.close() {
        warn!(
            reason = why,
            "feed fetcher: channel failed; nothing more can be fetched, so the server will stop"
        );
    }
}

/// A handle to the feed fetcher process.
///
/// Many requests can be outstanding at once. Each [`Self::fetch`] writes
/// its request through a dedicated writer thread and awaits its own
/// response, which a reader thread routes back by id, so a slow feed never
/// holds up any other — and no async task ever blocks on the socket.
pub struct FeedFetcherHost {
    pending: Arc<Pending<JobResult>>,
    writer: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    /// A clone of the socket kept only to shut it down on drop, which
    /// wakes the reader thread and tells the supervisor to exit.
    control: UnixStream,
    child: Mutex<Option<Child>>,
    /// Held while a job that was in hand when a worker died is retried,
    /// so that suspects are retried one at a time; see [`Self::request`].
    suspects: tokio::sync::Mutex<()>,
}

impl FeedFetcherHost {
    /// Spawn the feed fetcher.
    ///
    /// **Must be called before the caller installs its seccomp filter** —
    /// every sandbox profile denies `execve` — and, for the server to see
    /// the child's memory use, after its Landlock rules; see
    /// [`crate::sandbox::restrict_filesystem`]. `log_only` and `no_sandbox`
    /// are forwarded so the child's sandbox matches the operator's intent
    /// for the server's.
    ///
    /// # Errors
    ///
    /// Fails if the child cannot be spawned, or the threads that service
    /// its socket cannot be started.
    pub fn spawn(log_only: bool, no_sandbox: bool) -> Result<Self> {
        let (stream, child) =
            crate::process::spawn_child(SUBCOMMAND, HOST_FD_ENV, log_only, no_sandbox)
                .context("spawning the feed fetcher")?;
        info!(pid = child.id(), "feed fetcher: spawned");
        Self::from_stream(stream, Some(child))
    }

    /// Wrap an already-connected socket. The far end must speak the
    /// supervisor's side of the protocol.
    fn from_stream(stream: UnixStream, child: Option<Child>) -> Result<Self> {
        let pending = Arc::new(Pending::new());

        let control = stream.try_clone().context("cloning the fetcher socket")?;
        let mut write_half = stream.try_clone().context("cloning the fetcher socket")?;
        let mut read_half = stream;

        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let lookups = spawn_resolver_pool(tx.clone())?;

        let writer_pending = Arc::clone(&pending);
        std::thread::Builder::new()
            .name("fetcher-writer".into())
            .spawn(move || {
                for frame in rx {
                    if let Err(e) = write_frame_limited(&mut write_half, &frame, MAX_FRAME_BYTES) {
                        retire(&writer_pending, &format!("write failed: {e}"));
                        return;
                    }
                }
            })
            .context("starting the fetcher writer thread")?;

        let reader_pending = Arc::clone(&pending);
        std::thread::Builder::new()
            .name("fetcher-reader".into())
            .spawn(move || loop {
                let frame = match read_frame_limited(&mut read_half, MAX_FRAME_BYTES) {
                    Ok(f) => f,
                    Err(e) => {
                        retire(&reader_pending, &format!("read failed: {e}"));
                        return;
                    }
                };
                match decode(&frame) {
                    Ok(FromFetcher::Response(r)) => reader_pending.complete(r.id, r.result),
                    Ok(FromFetcher::Resolve { id, host }) => lookups.submit(id, host),
                    // The supervisor keeps these to itself.
                    Ok(FromFetcher::Parse { .. }) => {
                        retire(
                            &reader_pending,
                            "the fetcher sent a parse task to the server",
                        );
                        return;
                    }
                    Err(e) => {
                        retire(&reader_pending, &format!("malformed response: {e}"));
                        return;
                    }
                }
            })
            .context("starting the fetcher reader thread")?;

        Ok(FeedFetcherHost {
            pending,
            writer: Mutex::new(Some(tx)),
            control,
            child: Mutex::new(child),
            suspects: tokio::sync::Mutex::new(()),
        })
    }

    /// Fetch, and on success parse, the feed described by `spec`.
    ///
    /// # Errors
    ///
    /// Fails only if the fetcher could not serve the request; failures of
    /// the fetch itself are [`FetchReply`] variants.
    pub async fn fetch(&self, spec: FetchSpec) -> Result<FetchReply, FetcherError> {
        // A fetch can take up to one timeout per hop, then a parse.
        let hops = u32::try_from(MAX_REDIRECTS + 1).unwrap_or(u32::MAX);
        let deadline = Duration::from_secs(spec.timeout_secs)
            .saturating_mul(hops)
            .saturating_add(DEADLINE_SLACK);
        match self.request(Job::Fetch(spec), deadline).await? {
            JobResult::Fetched(reply) => Ok(reply),
            other => Err(unexpected(other)),
        }
    }

    /// Parse a body the server has already read.
    ///
    /// # Errors
    ///
    /// As for [`Self::fetch`].
    pub async fn parse(&self, feed_id: i64, body: Vec<u8>) -> Result<ParseOutcome, FetcherError> {
        match self
            .request(Job::Parse { feed_id, body }, PARSE_DEADLINE)
            .await?
        {
            JobResult::Parsed(outcome) => Ok(outcome),
            other => Err(unexpected(other)),
        }
    }

    /// Download the asset described by `spec`.
    ///
    /// # Errors
    ///
    /// As for [`Self::fetch`].
    pub async fn fetch_asset(&self, spec: AssetSpec) -> Result<AssetReply, FetcherError> {
        match self.request(Job::FetchAsset(spec), ASSET_DEADLINE).await? {
            JobResult::Asset(reply) => Ok(reply),
            other => Err(unexpected(other)),
        }
    }

    /// Fetch the web page in `spec` and find the icons it links to.
    ///
    /// # Errors
    ///
    /// As for [`Self::fetch`].
    pub async fn find_page_icons(&self, spec: PageSpec) -> Result<PageIcons, FetcherError> {
        match self
            .request(Job::FindPageIcons(spec), ASSET_DEADLINE)
            .await?
        {
            JobResult::PageIcons(icons) => Ok(icons),
            other => Err(unexpected(other)),
        }
    }

    /// Find the images an entry's HTML `content` shows.
    ///
    /// # Errors
    ///
    /// As for [`Self::fetch`].
    pub async fn extract_images(
        &self,
        content: String,
        base: String,
    ) -> Result<Vec<String>, FetcherError> {
        match self
            .request(Job::ExtractImages { content, base }, PARSE_DEADLINE)
            .await?
        {
            JobResult::Images(urls) => Ok(urls),
            other => Err(unexpected(other)),
        }
    }

    /// A host together with the far end of its channel, which nothing
    /// serves: tests drop it to see what the server does when the fetcher
    /// goes away.
    #[cfg(test)]
    pub(crate) fn with_far_end() -> (Self, UnixStream) {
        #[allow(clippy::unwrap_used)]
        let (ours, theirs) = UnixStream::pair().unwrap();
        #[allow(clippy::unwrap_used)]
        (Self::from_stream(ours, None).unwrap(), theirs)
    }

    /// A host whose far end answers every request with `answer`, on a
    /// thread of its own, standing in for a worker that may not follow the
    /// rules — so tests can check what the server does with its replies.
    #[cfg(test)]
    pub(crate) fn with_fake_worker(answer: impl Fn(Job) -> JobResult + Send + 'static) -> Self {
        #[allow(clippy::unwrap_used)]
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            while let Ok(frame) = read_frame_limited(&mut theirs, MAX_FRAME_BYTES) {
                let Ok(ToFetcher::Request(request)) = decode(&frame) else {
                    continue;
                };
                let reply = encode_response(request.id, answer(request.job));
                if write_frame_limited(&mut theirs, &reply, MAX_FRAME_BYTES).is_err() {
                    return;
                }
            }
        });
        #[allow(clippy::unwrap_used)]
        Self::from_stream(ours, None).unwrap()
    }

    /// Whether the channel to the fetcher is still usable.
    pub fn is_alive(&self) -> bool {
        self.pending.is_open()
    }

    /// Wait until the channel to the fetcher fails, after which no request
    /// can succeed. Returns at once if it already has.
    pub async fn closed(&self) {
        self.pending.closed().await
    }

    /// Send `job` to the fetcher and wait up to `deadline` for its result.
    ///
    /// When the worker dies with the job in hand, the job is sent again,
    /// holding [`Self::suspects`] so that no other suspect is retried
    /// alongside it. A worker may die of any of the jobs it was given, so
    /// the job is only blamed ([`FetcherError::Crashed`]) if it kills the
    /// worker while it is the only job in hand. The others it was in hand
    /// with are then served as usual, rather than all failing together.
    async fn request(&self, job: Job, deadline: Duration) -> Result<JobResult, FetcherError> {
        let (id, rx) = self.pending.register().ok_or(FetcherError::Gone)?;
        // Kept whole, to be sent again, under the same id, if need be.
        let msg = ToFetcher::Request(Request { id, job });
        let why = match self.send_and_wait(id, rx, &msg, deadline).await? {
            JobResult::WorkerExited { why, .. } => why,
            result => return finish(result),
        };
        debug!(id, reason = %why, "feed fetcher: retrying a job the worker died with");

        let _alone = self.suspects.lock().await;
        let mut why = why;
        for _ in 0..ISOLATED_ATTEMPTS {
            let rx = self.pending.register_as(id).ok_or(FetcherError::Gone)?;
            match self.send_and_wait(id, rx, &msg, deadline).await? {
                JobResult::WorkerExited { in_flight, why } if in_flight <= 1 => {
                    return Err(FetcherError::Crashed(why));
                }
                JobResult::WorkerExited { why: again, .. } => why = again,
                result => return finish(result),
            }
        }
        Err(FetcherError::Crashed(why))
    }

    /// Send `msg`, the request `id`, and wait up to `deadline` on `rx` for
    /// its answer.
    async fn send_and_wait(
        &self,
        id: u64,
        rx: oneshot::Receiver<JobResult>,
        msg: &ToFetcher,
        deadline: Duration,
    ) -> Result<JobResult, FetcherError> {
        // Refused before it reaches the wire, so the channel is fine.
        let encoded = encode_to(msg).and_then(|v| {
            if v.len() > MAX_FRAME_BYTES {
                Err(FetcherError::Unavailable(format!(
                    "request of {} bytes exceeds the {} byte frame limit",
                    v.len(),
                    MAX_FRAME_BYTES
                )))
            } else {
                Ok(v)
            }
        });
        let encoded = match encoded {
            Ok(v) => v,
            Err(e) => {
                self.pending.forget(id);
                return Err(e);
            }
        };

        let sent = self
            .writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|w| w.send(encoded).is_ok());
        if !sent {
            self.pending.forget(id);
            return Err(FetcherError::Gone);
        }

        match tokio::time::timeout(deadline, rx).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => Err(FetcherError::Gone),
            Err(_) => {
                // Not fatal: the worker may just be slow on this one feed,
                // and every other request is independent of it.
                self.pending.forget(id);
                Err(FetcherError::Timeout(deadline))
            }
        }
    }
}

/// The result of a request, with a job the fetcher could not complete
/// turned into an error.
fn finish(result: JobResult) -> Result<JobResult, FetcherError> {
    match result {
        JobResult::Failed { message } => Err(FetcherError::Unavailable(message)),
        JobResult::Crashed { why } => Err(FetcherError::Crashed(why)),
        result => Ok(result),
    }
}

fn unexpected(result: JobResult) -> FetcherError {
    FetcherError::Unavailable(format!("unexpected response from the fetcher: {result:?}"))
}

/// The server's side of name resolution: a small, fixed pool of threads
/// that answer the worker's [`FromFetcher::Resolve`] frames.
struct ResolverPool {
    jobs: mpsc::SyncSender<(u64, String)>,
    replies: mpsc::Sender<Vec<u8>>,
}

impl ResolverPool {
    /// Queue a lookup, or refuse it at once if the queue is full.
    fn submit(&self, id: u64, host: String) {
        if let Err(mpsc::TrySendError::Full((id, _))) = self.jobs.try_send((id, host)) {
            send_resolved(&self.replies, id, Err("too many lookups in flight".into()));
        }
    }
}

/// Start [`RESOLVER_THREADS`] threads that look hostnames up and write the
/// answers through `replies`. They exit when the returned pool is dropped,
/// which happens when the reader thread that owns it does.
fn spawn_resolver_pool(replies: mpsc::Sender<Vec<u8>>) -> Result<ResolverPool> {
    let (jobs, queue) = mpsc::sync_channel::<(u64, String)>(RESOLVER_QUEUE);
    let queue = Arc::new(Mutex::new(queue));
    for n in 0..RESOLVER_THREADS {
        let queue = Arc::clone(&queue);
        let replies = replies.clone();
        std::thread::Builder::new()
            .name(format!("fetcher-dns-{n}"))
            .spawn(move || loop {
                let next = queue.lock().unwrap_or_else(|e| e.into_inner()).recv();
                let Ok((id, host)) = next else { return };
                send_resolved(&replies, id, lookup_host(&host));
            })
            .context("starting a fetcher resolver thread")?;
    }
    Ok(ResolverPool { jobs, replies })
}

/// Resolve `host` with the system resolver, as any other program on this
/// host would.
fn lookup_host(host: &str) -> Result<Vec<SocketAddr>, String> {
    if host.is_empty() || host.len() > MAX_HOSTNAME_LEN || host.contains('\0') {
        return Err("not a valid hostname".into());
    }
    let addrs: Vec<SocketAddr> = (host, 0)
        .to_socket_addrs()
        .map_err(|e| format!("{e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("no addresses found for {host}"));
    }
    Ok(addrs)
}

fn send_resolved(
    replies: &mpsc::Sender<Vec<u8>>,
    id: u64,
    result: Result<Vec<SocketAddr>, String>,
) {
    if let Ok(frame) = encode_to(&ToFetcher::Resolved { id, result }) {
        let _ = replies.send(frame);
    }
}

impl Drop for FeedFetcherHost {
    fn drop(&mut self) {
        // Stop the writer, then shut the socket down: the reader thread
        // wakes with EOF, and the supervisor sees EOF and exits, taking
        // its worker with it.
        self.writer.lock().unwrap_or_else(|e| e.into_inner()).take();
        let _ = self.control.shutdown(Shutdown::Both);

        if let Some(child) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            crate::process::reap(child);
        }
    }
}

// ------------------------------------------------------------------
// Child side: supervisor
// ------------------------------------------------------------------

/// Run the feed fetcher: sandbox this process, then supervise a worker
/// and a parser until the server closes the channel.
///
/// This is the whole of the child's life. It never returns to any other
/// code path.
///
/// # Errors
///
/// Fails if the sandbox cannot be installed, or if a worker cannot be
/// forked at all. Once running, the server closing the channel is a
/// normal shutdown, not an error.
pub fn run_child(log_only: bool, no_sandbox: bool) -> Result<()> {
    if no_sandbox {
        warn!(
            "feed fetcher: sandbox disabled via --no-sandbox; feeds are fetched and parsed \
             with full filesystem and syscall access"
        );
    } else {
        crate::sandbox::apply(&crate::sandbox::SandboxConfig::feed_fetcher(log_only))
            .context("failed to install the feed fetcher sandbox")?;
    }

    let mut server = crate::process::take_parent_socket(SUBCOMMAND)?;

    let threads = parser_threads();
    let mut parser = Parser::new(
        threads,
        Box::new(move || spawn_parser_process(threads, log_only, no_sandbox)),
    );

    let mut quick_deaths: u32 = 0;
    loop {
        let (mut ours, theirs) =
            UnixStream::pair().context("creating the fetcher worker socket pair")?;
        ours.set_read_timeout(Some(WORKER_IO_TIMEOUT))?;
        ours.set_write_timeout(Some(WORKER_IO_TIMEOUT))?;

        // The worker must never be able to talk to the server, or to the
        // parser, directly: everything it says goes through the relay.
        let pid = fork_child(theirs, worker_main).context("forking a fetcher worker")?;

        info!(pid, "feed fetcher: worker started");
        let started = Instant::now();
        let end = relay(&mut server, &mut ours, &mut parser);

        // Whatever happened, this worker is finished; make sure of it.
        let status = kill_and_reap(pid);
        // And so is everything the parser was doing for it.
        parser.reset();

        let in_flight = match end {
            RelayEnd::ServerClosed => {
                debug!("feed fetcher: server closed the channel, exiting");
                return Ok(());
            }
            RelayEnd::WorkerGone { in_flight, why } => {
                warn!(
                    pid,
                    reason = %why,
                    status = %describe_wait_status(status),
                    in_flight = in_flight.len(),
                    "feed fetcher: worker exited; starting a new one"
                );
                in_flight
            }
        };

        // Answer everything the dead worker had accepted, so the server
        // can retry each of them now, and find which one killed it,
        // instead of waiting out their deadlines.
        let count = u32::try_from(in_flight.len()).unwrap_or(u32::MAX);
        let why = format!("the fetcher worker died ({})", describe_wait_status(status));
        for id in in_flight {
            let response = FromFetcher::Response(Response {
                id,
                result: JobResult::WorkerExited {
                    in_flight: count,
                    why: why.clone(),
                },
            });
            let encoded = encode(&response).context("encoding a failure response")?;
            if write_frame_limited(&mut server, &encoded, MAX_FRAME_BYTES).is_err() {
                return Ok(());
            }
        }

        if started.elapsed() >= HEALTHY_WORKER_LIFETIME {
            quick_deaths = 0;
        } else {
            quick_deaths = quick_deaths.saturating_add(1);
        }
        let delay = respawn_delay(quick_deaths);
        if !delay.is_zero() {
            warn!(
                ?delay,
                "feed fetcher: worker is crash-looping; delaying respawn"
            );
            std::thread::sleep(delay);
        }
    }
}

/// Fork a child of the supervisor that runs `body` on `stream`, its end
/// of a socket pair, and exits with what `body` returns. Returns the
/// child's pid.
///
/// The child closes every descriptor it inherited but `stream` and the
/// standard ones before `body` runs — the supervisor's channel to the
/// server, and to its other children, among them — and dies with the
/// supervisor.
///
/// Must only be called while the supervisor is single-threaded, which it
/// always is: it never starts a thread or a runtime.
fn fork_child(stream: UnixStream, body: impl FnOnce(UnixStream) -> i32) -> io::Result<libc::pid_t> {
    let supervisor_pid = std::process::id();
    // SAFETY: this process is single-threaded — the supervisor never
    // starts a thread or a runtime — so the child is a complete copy and
    // may run arbitrary code, not just async-signal-safe calls.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        let code = (|| {
            // Die with the supervisor. Checked again after the `prctl` in
            // case the supervisor exited before it took effect.
            // SAFETY: plain `prctl`/`getppid` calls with no pointer
            // arguments.
            unsafe {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                if libc::getppid() as u32 != supervisor_pid {
                    return 0;
                }
            }
            if let Err(e) = close_fds_except(stream.as_raw_fd()) {
                warn!(error = %e, "feed fetcher: could not close inherited descriptors");
                return 1;
            }
            // Nothing the supervisor owns may be dropped here — dropping a
            // `ParserProc` would kill the parser — so a panic must not
            // unwind past this point.
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(stream))).unwrap_or(101)
        })();
        std::process::exit(code);
    }
    Ok(pid)
}

/// Close every descriptor from 3 up but `keep`.
fn close_fds_except(keep: RawFd) -> io::Result<()> {
    let keep = libc::c_uint::try_from(keep)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "negative descriptor"))?;
    if keep > 3 {
        close_range(3, keep - 1)?;
    }
    close_range(keep.max(2) + 1, libc::c_uint::MAX)
}

/// Close the descriptors `first..=last` with `close_range(2)`, or one by
/// one, up to the descriptor limit, on kernels that predate it (5.9).
fn close_range(first: libc::c_uint, last: libc::c_uint) -> io::Result<()> {
    // SAFETY: `close_range` takes no pointers; closing descriptors this
    // process owns is always memory-safe.
    let rc = unsafe { libc::syscall(libc::SYS_close_range, first, last, 0) };
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
    for fd in first..end.min(last.saturating_add(1)) {
        // SAFETY: as above; descriptors that are not open fail harmlessly.
        unsafe { libc::close(fd as libc::c_int) };
    }
    Ok(())
}

/// SIGKILL the child `pid`, if it is still running, and reap it. Returns
/// its wait status.
fn kill_and_reap(pid: libc::pid_t) -> libc::c_int {
    // SAFETY: `pid` is our own child, which has not been reaped yet, and
    // `status` is a valid pointer for the call.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
        let mut status: libc::c_int = 0;
        libc::waitpid(pid, &mut status, 0);
        status
    }
}

/// How long to wait before starting the next worker, given how many have
/// died young in a row. The first death is free; after that the delay
/// doubles from one second up to [`MAX_RESPAWN_DELAY`].
fn respawn_delay(quick_deaths: u32) -> Duration {
    if quick_deaths <= 1 {
        return Duration::ZERO;
    }
    let exp = quick_deaths.saturating_sub(2).min(16);
    Duration::from_secs(1u64 << exp).min(MAX_RESPAWN_DELAY)
}

fn describe_wait_status(status: libc::c_int) -> String {
    if libc::WIFSIGNALED(status) {
        let sig = libc::WTERMSIG(status);
        if sig == libc::SIGSYS {
            "killed by SIGSYS (a syscall denied by the seccomp filter)".into()
        } else {
            format!("killed by signal {sig}")
        }
    } else if libc::WIFEXITED(status) {
        format!("exited with status {}", libc::WEXITSTATUS(status))
    } else {
        format!("wait status {status}")
    }
}

/// Why [`relay`] stopped.
enum RelayEnd {
    /// The server closed its end: time to exit.
    ServerClosed,

    /// The worker died, wedged, or broke the protocol. `in_flight` holds
    /// the ids it had been given but not answered.
    WorkerGone {
        in_flight: HashSet<u64>,
        why: String,
    },
}

/// A frame whose bytes are overwritten with zeroes when it is dropped, for
/// the server's requests, which may carry a feed's credentials: the parser
/// is forked from the supervisor, and must not find them in what it
/// inherits.
struct Scrubbed<B: AsMut<[u8]>>(B);

impl<B: AsMut<[u8]>> Drop for Scrubbed<B> {
    fn drop(&mut self) {
        let bytes = self.0.as_mut();
        bytes.fill(0);
        // Keep the writes from being optimised away as dead stores.
        std::hint::black_box(bytes);
    }
}

/// Copy frames between the server, the current worker and the parser
/// until the server or the worker goes away, keeping track of which
/// requests are outstanding in each direction.
///
/// Lookups are tracked so that an answer meant for a worker that has since
/// died is dropped rather than delivered to its replacement, whose ids
/// start again from the beginning.
///
/// Every peer reads and writes independently, so blocking on a write to
/// one can never deadlock against a write of theirs.
fn relay(server: &mut UnixStream, worker: &mut UnixStream, parser: &mut Parser) -> RelayEnd {
    let mut in_flight: HashSet<u64> = HashSet::new();
    let mut lookups: HashSet<u64> = HashSet::new();
    let gone = |in_flight: HashSet<u64>, why: String| RelayEnd::WorkerGone { in_flight, why };

    loop {
        if let Err(why) = parser.tend(worker) {
            return gone(in_flight, why);
        }

        let parser_fd = parser.fd();
        let mut fds = [
            server.as_raw_fd(),
            worker.as_raw_fd(),
            parser_fd.unwrap_or(-1),
        ]
        .map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        });
        let watched = if parser_fd.is_some() { 3 } else { 2 };
        let timeout = parser.next_wakeup().map_or(-1, |at| {
            let ms = at.saturating_duration_since(Instant::now()).as_millis();
            // Rounded up, so as not to wake just before the moment.
            libc::c_int::try_from(ms.saturating_add(1)).unwrap_or(libc::c_int::MAX)
        });
        // SAFETY: `fds` is a valid array of initialised pollfds, at least
        // `watched` long.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), watched as libc::nfds_t, timeout) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return gone(in_flight, format!("poll failed: {err}"));
        }

        let [server_fd, worker_fd, parser_fd] = fds;

        // Drain responses first so a busy server cannot starve them.
        if worker_fd.revents != 0 {
            let frame = match read_frame_limited(worker, MAX_FRAME_BYTES) {
                Ok(f) => f,
                Err(e) => return gone(in_flight, format!("read failed: {e}")),
            };
            match peek_from(&frame) {
                Some(PeekFrom::Response(IdOnly { id })) => {
                    in_flight.remove(&id);
                }
                Some(PeekFrom::Resolve(IdOnly { id })) => {
                    lookups.insert(id);
                }
                Some(PeekFrom::Parse(IdOnly { id })) => {
                    if let Err(why) = parser.submit(id, frame, worker) {
                        return gone(in_flight, why);
                    }
                    continue;
                }
                None => return gone(in_flight, "sent a malformed frame".into()),
            }
            if write_frame_limited(server, &frame, MAX_FRAME_BYTES).is_err() {
                return RelayEnd::ServerClosed;
            }
        }

        if watched == 3 && parser_fd.revents != 0 {
            if let Err(why) = parser.receive(worker) {
                return gone(in_flight, why);
            }
        }

        if server_fd.revents != 0 {
            let frame = match read_frame_limited(server, MAX_FRAME_BYTES) {
                Ok(f) => Scrubbed(f),
                Err(_) => return RelayEnd::ServerClosed,
            };
            match peek_to(&frame.0) {
                Some(PeekTo::Request(IdOnly { id })) => {
                    in_flight.insert(id);
                }
                Some(PeekTo::Resolved(IdOnly { id })) => {
                    if !lookups.remove(&id) {
                        // For a previous worker, or never asked for.
                        continue;
                    }
                }
                // Only ever from the parser.
                Some(PeekTo::Parsed(_)) | None => continue,
            }
            if let Err(e) = write_frame_limited(worker, &frame.0, MAX_FRAME_BYTES) {
                return gone(in_flight, format!("write failed: {e}"));
            }
        }
    }
}

// ------------------------------------------------------------------
// Supervisor: the parser
// ------------------------------------------------------------------

/// A running parser, as the supervisor sees it.
struct ParserProc {
    /// The supervisor's end of the parser's socket pair, with
    /// [`WORKER_IO_TIMEOUT`] on reads and writes.
    stream: UnixStream,
    /// `None` for a parser that is not a process of its own, in tests.
    pid: Option<libc::pid_t>,
    started: Instant,
}

impl ParserProc {
    /// Stop the parser, and say how it ended.
    fn end(self) -> String {
        match self.pid {
            Some(pid) => describe_wait_status(kill_and_reap(pid)),
            // A parser on a thread stops when its socket closes.
            None => "stopped".into(),
        }
    }
}

impl Drop for ParserProc {
    fn drop(&mut self) {
        if let Some(pid) = self.pid.take() {
            kill_and_reap(pid);
        }
    }
}

/// Starts a parser.
type SpawnParser = Box<dyn FnMut() -> io::Result<ParserProc>>;

/// Fork a parser process with `threads` threads.
fn spawn_parser_process(
    threads: usize,
    log_only: bool,
    no_sandbox: bool,
) -> io::Result<ParserProc> {
    let (ours, theirs) = UnixStream::pair()?;
    ours.set_read_timeout(Some(WORKER_IO_TIMEOUT))?;
    ours.set_write_timeout(Some(WORKER_IO_TIMEOUT))?;
    let pid = fork_child(theirs, |stream| {
        parser_main(stream, threads, log_only, no_sandbox)
    })?;
    info!(pid, threads, "feed fetcher: parser started");
    Ok(ParserProc {
        stream: ours,
        pid: Some(pid),
        started: Instant::now(),
    })
}

/// The supervisor's view of the parser: the process itself, the tasks it
/// has in hand, and those waiting for one of its threads.
struct Parser {
    proc: Option<ParserProc>,
    spawn: SpawnParser,
    /// How many tasks the parser may have in hand: its thread count.
    capacity: usize,
    /// How long the parser may spend on one task: [`PARSE_TIMEOUT`],
    /// except in tests.
    task_timeout: Duration,
    /// Tasks the parser has in hand, by id, with when each was handed
    /// over.
    in_hand: HashMap<u64, Instant>,
    /// Tasks waiting for a thread, as the worker's frames, with when each
    /// arrived.
    queue: VecDeque<(u64, Vec<u8>, Instant)>,
    /// How many parsers in a row have died young.
    quick_deaths: u32,
    /// When the next parser may be started, after one died young.
    down_until: Option<Instant>,
}

impl Parser {
    fn new(capacity: usize, spawn: SpawnParser) -> Self {
        Parser {
            proc: None,
            spawn,
            capacity: capacity.max(1),
            task_timeout: PARSE_TIMEOUT,
            in_hand: HashMap::new(),
            queue: VecDeque::new(),
            quick_deaths: 0,
            down_until: None,
        }
    }

    /// The parser's socket, if it is running.
    fn fd(&self) -> Option<RawFd> {
        self.proc.as_ref().map(|p| p.stream.as_raw_fd())
    }

    /// Accept the worker's task `id`, carried by `frame`.
    ///
    /// `Err` means the worker could not be written to; it carries why.
    fn submit(&mut self, id: u64, frame: Vec<u8>, worker: &mut UnixStream) -> Result<(), String> {
        let known = self.in_hand.contains_key(&id) || self.queue.iter().any(|(q, ..)| *q == id);
        if known {
            let message = format!("parse task {id} is already in hand");
            return answer(worker, id, ParseReply::Failed { message });
        }
        self.queue.push_back((id, frame, Instant::now()));
        self.tend(worker)
    }

    /// Do what is due: kill a parser that has spent too long over a task,
    /// refuse tasks that have waited too long, start a parser if one is
    /// needed and may be started, and hand it tasks while it has threads
    /// free.
    ///
    /// `Err` means the worker could not be written to; it carries why.
    fn tend(&mut self, worker: &mut UnixStream) -> Result<(), String> {
        let now = Instant::now();
        if self
            .in_hand
            .values()
            .any(|&since| now.duration_since(since) >= self.task_timeout)
        {
            let why = format!(
                "the parser was killed after a task ran for over {:?}",
                self.task_timeout
            );
            self.died(&why, worker)?;
        }

        while let Some((id, _, since)) = self.queue.front() {
            if now.duration_since(*since) < PARSE_QUEUE_LIMIT {
                break;
            }
            let id = *id;
            self.queue.pop_front();
            let message = format!("the parser was not available within {PARSE_QUEUE_LIMIT:?}");
            answer(worker, id, ParseReply::Failed { message })?;
        }

        if self.queue.is_empty() {
            return Ok(());
        }
        if self.proc.is_none() {
            if self.down_until.is_some_and(|t| now < t) {
                return Ok(());
            }
            match (self.spawn)() {
                Ok(proc) => {
                    self.proc = Some(proc);
                    self.down_until = None;
                }
                Err(e) => {
                    warn!(error = %e, "feed fetcher: could not start the parser");
                    self.died_young();
                    return Ok(());
                }
            }
        }

        while self.in_hand.len() < self.capacity {
            let Some((id, frame, _)) = self.queue.pop_front() else {
                break;
            };
            let Some(proc) = self.proc.as_mut() else {
                break;
            };
            // In hand from here on, so that a parser that dies taking it
            // answers for it.
            self.in_hand.insert(id, Instant::now());
            if let Err(e) = write_frame_limited(&mut proc.stream, &frame, MAX_FRAME_BYTES) {
                self.died(&format!("the parser could not be written to: {e}"), worker)?;
                break;
            }
        }
        Ok(())
    }

    /// Read a result from the parser, which has one ready (or has gone
    /// away), and pass it on to the worker.
    ///
    /// `Err` means the worker could not be written to; it carries why.
    fn receive(&mut self, worker: &mut UnixStream) -> Result<(), String> {
        let Some(proc) = self.proc.as_mut() else {
            return Ok(());
        };
        let frame = match read_frame_limited(&mut proc.stream, MAX_FRAME_BYTES) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                return self.died("the parser died", worker);
            }
            Err(e) => return self.died(&format!("the parser could not be read: {e}"), worker),
        };
        match peek_to(&frame) {
            Some(PeekTo::Parsed(IdOnly { id })) if self.in_hand.remove(&id).is_some() => {
                write_frame_limited(worker, &frame, MAX_FRAME_BYTES)
                    .map_err(|e| format!("write failed: {e}"))?;
                self.tend(worker)
            }
            _ => self.died("the parser answered a task it did not have", worker),
        }
    }

    /// Stop the parser, which has died or must, and answer every task it
    /// had in hand with [`ParseReply::Exited`], so that the worker retries
    /// them; `what` says what happened. Tasks still queued wait for the
    /// next parser.
    ///
    /// `Err` means the worker could not be written to; it carries why.
    fn died(&mut self, what: &str, worker: &mut UnixStream) -> Result<(), String> {
        let (status, lived) = match self.proc.take() {
            Some(proc) => {
                let lived = proc.started.elapsed();
                (proc.end(), lived)
            }
            None => return Ok(()),
        };
        let in_hand: Vec<u64> = self.in_hand.drain().map(|(id, _)| id).collect();
        warn!(
            reason = what,
            status = %status,
            in_hand = in_hand.len(),
            "feed fetcher: parser stopped; starting a new one"
        );
        if lived >= HEALTHY_WORKER_LIFETIME {
            self.quick_deaths = 0;
        } else {
            self.died_young();
        }

        let count = u32::try_from(in_hand.len()).unwrap_or(u32::MAX);
        let why = format!("{what} ({status})");
        for id in in_hand {
            answer(
                worker,
                id,
                ParseReply::Exited {
                    in_flight: count,
                    why: why.clone(),
                },
            )?;
        }
        Ok(())
    }

    /// Note that a parser died young, or could not be started, and put off
    /// starting the next one if that keeps happening.
    fn died_young(&mut self) {
        self.quick_deaths = self.quick_deaths.saturating_add(1);
        let delay = respawn_delay(self.quick_deaths);
        if !delay.is_zero() {
            warn!(
                ?delay,
                "feed fetcher: parser is crash-looping; delaying restart"
            );
            self.down_until = Some(Instant::now() + delay);
        }
    }

    /// When [`Self::tend`] next has something to do, if ever.
    fn next_wakeup(&self) -> Option<Instant> {
        let timeouts = self
            .in_hand
            .values()
            .map(|&since| since + self.task_timeout);
        let expiries = self
            .queue
            .front()
            .map(|(_, _, since)| *since + PARSE_QUEUE_LIMIT);
        let restart = self.down_until.filter(|_| !self.queue.is_empty());
        timeouts.chain(expiries).chain(restart).min()
    }

    /// Forget everything, and stop the parser: the worker it was working
    /// for has gone. The next task starts a new one.
    fn reset(&mut self) {
        if let Some(proc) = self.proc.take() {
            proc.end();
        }
        self.in_hand.clear();
        self.queue.clear();
    }
}

/// Send the worker `reply` to its parse task `id`.
///
/// `Err` means the worker could not be written to; it carries why.
fn answer(worker: &mut UnixStream, id: u64, reply: ParseReply) -> Result<(), String> {
    let frame = encode(&ToFetcher::Parsed { id, reply })
        .map_err(|e| format!("could not encode a parse reply: {e}"))?;
    write_frame_limited(worker, &frame, MAX_FRAME_BYTES).map_err(|e| format!("write failed: {e}"))
}

// ------------------------------------------------------------------
// Child side: worker
// ------------------------------------------------------------------

/// Entry point of a forked worker. Returns the process exit code.
fn worker_main(stream: UnixStream) -> i32 {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("fetcher-worker")
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            warn!(error = %e, "feed fetcher: could not start the worker runtime");
            return 1;
        }
    };

    match runtime.block_on(serve(stream)) {
        Ok(()) => 0,
        Err(e) => {
            warn!(error = %format!("{e:#}"), "feed fetcher: worker failed");
            1
        }
    }
}

/// Serve requests from the supervisor, each on its own task, until the
/// supervisor closes the channel.
async fn serve(stream: UnixStream) -> Result<()> {
    stream.set_nonblocking(true)?;
    let stream = tokio::net::UnixStream::from_std(stream)?;
    let (mut rd, mut wr) = stream.into_split();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if let Err(e) = write_frame_async(&mut wr, &frame, MAX_FRAME_BYTES).await {
                warn!(error = %e, "feed fetcher: write failed");
                return;
            }
        }
    });

    let resolver = ServerResolver {
        frames: tx.clone(),
        pending: Arc::new(Pending::new()),
    };
    let parser = ParserClient {
        frames: tx.clone(),
        pending: Arc::new(Pending::new()),
        suspects: Arc::new(tokio::sync::Mutex::new(())),
    };
    let feeds_resolver = resolver.clone();
    let assets_resolver = resolver.clone();
    let clients = Clients {
        parsers: Parsers::Pool(parser.clone()),
        feeds: ProxiedClient::new(move || {
            client_builder().dns_resolver(Arc::new(feeds_resolver.clone()))
        })
        .context("building the HTTP client")?,
        assets: ProxiedClient::new(move || {
            asset_client_builder(AssetTimeouts::DEFAULT)
                .dns_resolver(Arc::new(assets_resolver.clone()))
        })
        .context("building the HTTP client for assets")?,
    };

    loop {
        let frame = match read_frame_async(&mut rd, MAX_FRAME_BYTES).await {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e).context("reading a request"),
        };
        let request = match decode(&frame) {
            Ok(ToFetcher::Request(r)) => r,
            Ok(ToFetcher::Resolved { id, result }) => {
                resolver.pending.complete(id, result);
                continue;
            }
            Ok(ToFetcher::Parsed { id, reply }) => {
                parser.pending.complete(id, reply);
                continue;
            }
            // Both ends are the same binary, so this is corruption: exit,
            // and let the supervisor fail what was in flight and respawn.
            Err(e) => return Err(e).context("decoding a request"),
        };

        let clients = clients.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let id = request.id;
            // Run the job on a task of its own so a panic in it becomes an
            // answer instead of a request the server waits out.
            let result = match tokio::spawn(run_job(clients, request.job)).await {
                Ok(r) => r,
                Err(e) => JobResult::Failed {
                    message: format!("fetch task failed: {e}"),
                },
            };
            let _ = tx.send(encode_response(id, result));
        });
    }

    drop(tx);
    writer.abort();
    Ok(())
}

/// A host's addresses, or why they could not be found.
type LookupResult = Result<Vec<SocketAddr>, String>;

/// The worker's DNS resolver: asks the server, over the same channel as
/// everything else, instead of resolving anything itself.
#[derive(Clone)]
struct ServerResolver {
    frames: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    pending: Arc<Pending<LookupResult>>,
}

impl reqwest::dns::Resolve for ServerResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let this = self.clone();
        let host = name.as_str().to_string();
        Box::pin(async move {
            let (id, rx) = this
                .pending
                .register()
                .ok_or("the worker is shutting down")?;
            let frame = encode(&FromFetcher::Resolve {
                id,
                host: host.clone(),
            })?;
            if this.frames.send(frame).is_err() {
                this.pending.forget(id);
                return Err("the channel to the server is closed".into());
            }
            let answer = tokio::time::timeout(RESOLVE_TIMEOUT, rx).await;
            this.pending.forget(id);
            match answer {
                Ok(Ok(Ok(addrs))) => Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs),
                Ok(Ok(Err(message))) => Err(format!("resolving {host}: {message}").into()),
                Ok(Err(_)) => Err("the channel to the server is closed".into()),
                Err(_) => Err(format!("resolving {host}: no answer from the server").into()),
            }
        })
    }
}

/// The worker's HTTP clients, which resolve hostnames through the server,
/// and its way to the parser.
#[derive(Clone)]
struct Clients {
    feeds: ProxiedClient,
    assets: ProxiedClient,
    parsers: Parsers,
}

async fn run_job(clients: Clients, job: Job) -> JobResult {
    let parsers = &clients.parsers;
    let result = match job {
        Job::Fetch(spec) => fetch_with(&clients.feeds, parsers, &spec)
            .await
            .map(JobResult::Fetched),
        Job::Parse { feed_id, body } => parsers.feed(feed_id, body).await.map(JobResult::Parsed),
        Job::FetchAsset(spec) => fetch_asset(&clients.assets, parsers, &spec)
            .await
            .map(JobResult::Asset),
        Job::FindPageIcons(spec) => find_page_icons(&clients.assets, parsers, &spec)
            .await
            .map(JobResult::PageIcons),
        Job::ExtractImages { content, base } => {
            parsers.images(content, base).await.map(JobResult::Images)
        }
    };
    result.unwrap_or_else(|failure| match failure {
        ParseFailure::Crashed(why) => JobResult::Crashed { why },
        ParseFailure::Unavailable(message) => JobResult::Failed { message },
    })
}

/// The worker's way to the parser: tasks go to the supervisor, over the
/// same channel as everything else, and their results come back by id.
#[derive(Clone)]
pub struct ParserClient {
    frames: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    pending: Arc<Pending<ParseReply>>,
    /// Held while a task that was in hand when the parser died is retried,
    /// so that suspects are retried one at a time; see [`Self::run`].
    suspects: Arc<tokio::sync::Mutex<()>>,
}

impl ParserClient {
    /// Have the parser carry out `task`.
    ///
    /// When the parser dies with the task in hand, the task is sent again,
    /// holding [`Self::suspects`] so that no other suspect is retried
    /// alongside it, exactly as the server does with the worker's jobs
    /// (see `FeedFetcherHost::request`): the task is only blamed
    /// ([`ParseFailure::Crashed`]) if it kills the parser while it is the
    /// only task in hand.
    ///
    /// # Errors
    ///
    /// As for [`Parsers::run`].
    pub(crate) async fn run(&self, task: ParseTask) -> Result<ParseOutput, ParseFailure> {
        let (id, rx) = self
            .pending
            .register()
            .ok_or_else(|| ParseFailure::Unavailable("the worker is shutting down".into()))?;
        let frame = encode(&FromFetcher::Parse { id, task })
            .map_err(|e| format!("could not encode a parse task: {e}"))
            .and_then(|v| {
                if v.len() > MAX_FRAME_BYTES {
                    Err(format!(
                        "parse task of {} bytes exceeds the {} byte frame limit",
                        v.len(),
                        MAX_FRAME_BYTES
                    ))
                } else {
                    Ok(v)
                }
            });
        let frame = match frame {
            Ok(v) => v,
            Err(message) => {
                self.pending.forget(id);
                return Err(ParseFailure::Unavailable(message));
            }
        };

        let why = match self.send_and_wait(id, rx, &frame).await? {
            ParseReply::Exited { why, .. } => why,
            reply => return finish_parse(reply),
        };
        debug!(id, reason = %why, "feed fetcher: retrying a task the parser died with");

        let _alone = self.suspects.lock().await;
        let mut why = why;
        for _ in 0..ISOLATED_ATTEMPTS {
            let rx = self
                .pending
                .register_as(id)
                .ok_or_else(|| ParseFailure::Unavailable("the worker is shutting down".into()))?;
            match self.send_and_wait(id, rx, &frame).await? {
                ParseReply::Exited { in_flight, why } if in_flight <= 1 => {
                    return Err(ParseFailure::Crashed(why));
                }
                ParseReply::Exited { why: again, .. } => why = again,
                reply => return finish_parse(reply),
            }
        }
        Err(ParseFailure::Crashed(why))
    }

    /// Send `frame`, the task `id`, and wait up to [`PARSE_WAIT`] on `rx`
    /// for its answer.
    async fn send_and_wait(
        &self,
        id: u64,
        rx: oneshot::Receiver<ParseReply>,
        frame: &[u8],
    ) -> Result<ParseReply, ParseFailure> {
        let closed = || ParseFailure::Unavailable("the channel to the supervisor is closed".into());
        if self.frames.send(frame.to_vec()).is_err() {
            self.pending.forget(id);
            return Err(closed());
        }
        match tokio::time::timeout(PARSE_WAIT, rx).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) => Err(closed()),
            Err(_) => {
                self.pending.forget(id);
                Err(ParseFailure::Unavailable(format!(
                    "no answer from the parser within {PARSE_WAIT:?}"
                )))
            }
        }
    }
}

/// The result of a parse task, with one the parser could not complete
/// turned into an error.
fn finish_parse(reply: ParseReply) -> Result<ParseOutput, ParseFailure> {
    match reply {
        ParseReply::Done(output) => Ok(output),
        ParseReply::Failed { message } => Err(ParseFailure::Unavailable(message)),
        ParseReply::Exited { why, .. } => Err(ParseFailure::Crashed(why)),
    }
}

// ------------------------------------------------------------------
// Child side: parser
// ------------------------------------------------------------------

/// The parser's process name, as `/proc/<pid>/comm` shows it.
pub const PARSER_NAME: &std::ffi::CStr = c"kiki-parser";

/// Entry point of a forked parser. Returns the process exit code.
///
/// Tightens the sandbox it inherited from the supervisor to the
/// [`crate::sandbox::SandboxProfile::FeedParser`] profile before it reads
/// a single task, unless the operator turned sandboxing off.
fn parser_main(stream: UnixStream, threads: usize, log_only: bool, no_sandbox: bool) -> i32 {
    // Named, so that it can be told apart from the worker, which shares
    // its command line; its threads inherit the name until they are
    // given their own.
    // SAFETY: `PARSER_NAME` is NUL-terminated and outlives the call.
    unsafe {
        libc::prctl(libc::PR_SET_NAME, PARSER_NAME.as_ptr());
    }
    scrub_environment();
    if !no_sandbox {
        let config = crate::sandbox::SandboxConfig::feed_parser(log_only);
        if let Err(e) = crate::sandbox::apply(&config) {
            warn!(error = %format!("{e:#}"), "feed fetcher: could not sandbox the parser");
            return 1;
        }
    }
    match serve_parses(stream, threads) {
        Ok(()) => 0,
        Err(e) => {
            warn!(error = %e, "feed fetcher: parser failed");
            1
        }
    }
}

/// Overwrite every environment variable the process inherited with
/// zeroes, in place.
///
/// The environment is the server's, and may hold credentials — a proxy
/// URL with a password in `HTTPS_PROXY`, say — that the worker needs and
/// the parser must not have. Unsetting the variables would leave their
/// text where it was; this wipes it. Nothing in the parser reads the
/// environment, which reads as empty afterwards.
///
/// Must be called while the process is single-threaded.
fn scrub_environment() {
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
            std::ptr::write_bytes(text, 0, libc::strlen(text));
            var = var.add(1);
        }
    }
}

/// Carry out the tasks that arrive on `stream` on `threads` threads,
/// writing each result back as it is ready, until the supervisor closes
/// the channel.
fn serve_parses(stream: UnixStream, threads: usize) -> io::Result<()> {
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    let (tx, rx) = mpsc::channel::<(u64, ParseTask)>();
    let rx = Arc::new(Mutex::new(rx));
    for n in 0..threads.max(1) {
        let rx = Arc::clone(&rx);
        let writer = Arc::clone(&writer);
        std::thread::Builder::new()
            .name(format!("parser-{n}"))
            .spawn(move || loop {
                let next = rx.lock().unwrap_or_else(|e| e.into_inner()).recv();
                let Ok((id, task)) = next else { return };
                let reply = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    crate::fetcher::parsing::run(task)
                })) {
                    Ok(output) => ParseReply::Done(output),
                    Err(_) => ParseReply::Failed {
                        message: "the parser panicked".into(),
                    },
                };
                let frame = encode_parsed(id, reply);
                let mut writer = writer.lock().unwrap_or_else(|e| e.into_inner());
                if write_frame_limited(&mut *writer, &frame, MAX_FRAME_BYTES).is_err() {
                    // The supervisor is gone; the reader will see it too.
                    return;
                }
            })?;
    }

    let mut reader = stream;
    loop {
        let frame = match read_frame_limited(&mut reader, MAX_FRAME_BYTES) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        match decode(&frame) {
            Ok(FromFetcher::Parse { id, task }) => {
                if tx.send((id, task)).is_err() {
                    return Err(io::Error::other("every parser thread has exited"));
                }
            }
            // Both ends are the same binary, so this is corruption.
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "malformed parse task",
                ))
            }
        }
    }
}

/// Encode a parse result, replacing it with a failure if it cannot be
/// sent.
fn encode_parsed(id: u64, reply: ParseReply) -> Vec<u8> {
    let encoded = encode(&ToFetcher::Parsed { id, reply })
        .map_err(|e| format!("could not encode the parse result: {e}"))
        .and_then(|v| {
            if v.len() > MAX_FRAME_BYTES {
                Err(format!(
                    "the parse result is {} bytes encoded, over the {} byte frame limit",
                    v.len(),
                    MAX_FRAME_BYTES
                ))
            } else {
                Ok(v)
            }
        });
    match encoded {
        Ok(v) => v,
        Err(message) => {
            let fallback = ToFetcher::Parsed {
                id,
                reply: ParseReply::Failed { message },
            };
            // A bare id and a short string always encode.
            encode(&fallback).unwrap_or_default()
        }
    }
}

/// Encode a response, replacing it with a failure if it cannot be sent.
fn encode_response(id: u64, result: JobResult) -> Vec<u8> {
    let encoded = encode(&FromFetcher::Response(Response { id, result }))
        .map_err(|e| format!("could not encode the response: {e}"))
        .and_then(|v| {
            if v.len() > MAX_FRAME_BYTES {
                Err(format!(
                    "the parsed feed is {} bytes encoded, over the {} byte frame limit",
                    v.len(),
                    MAX_FRAME_BYTES
                ))
            } else {
                Ok(v)
            }
        });
    match encoded {
        Ok(v) => v,
        Err(message) => {
            let fallback = FromFetcher::Response(Response {
                id,
                result: JobResult::Failed { message },
            });
            // A bare id and a short string always encode.
            encode(&fallback).unwrap_or_default()
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::http::FeedAuth;

    fn spec(url: &str) -> FetchSpec {
        FetchSpec {
            feed_id: 1,
            url: url.to_string(),
            etag: None,
            last_modified: None,
            send_conditionals: true,
            auth: FeedAuth::default(),
            timeout_secs: 5,
            max_feed_bytes: 1024 * 1024,
            proxy: Default::default(),
        }
    }

    const RSS: &[u8] = br#"<rss version="2.0"><channel><title>t</title><link>http://x/</link>
        <description>d</description><item><title>hi</title><guid>g1</guid></item>
        </channel></rss>"#;

    /// A host wired to the supervisor's relay, over a worker and parsers
    /// that run on threads rather than in processes of their own, but are
    /// otherwise the real thing.
    fn host_with_in_thread_worker() -> FeedFetcherHost {
        host_with_in_thread_children(|stream| {
            let _ = serve_parses(stream, 2);
        })
    }

    /// As [`host_with_in_thread_worker`], with `parser` run on a thread of
    /// its own each time the relay starts a parser, and with tasks allowed
    /// `task_timeout` each.
    fn host_with_parser(
        parser: impl Fn(UnixStream) + Send + Clone + 'static,
        task_timeout: Duration,
    ) -> FeedFetcherHost {
        let (host_end, mut server) = UnixStream::pair().unwrap();
        let (mut worker, worker_end) = UnixStream::pair().unwrap();
        worker.set_read_timeout(Some(WORKER_IO_TIMEOUT)).unwrap();
        worker.set_write_timeout(Some(WORKER_IO_TIMEOUT)).unwrap();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            let _ = rt.block_on(serve(worker_end));
        });
        std::thread::spawn(move || {
            let spawn = move || {
                let (ours, theirs) = UnixStream::pair()?;
                ours.set_read_timeout(Some(WORKER_IO_TIMEOUT))?;
                ours.set_write_timeout(Some(WORKER_IO_TIMEOUT))?;
                let parser = parser.clone();
                std::thread::spawn(move || parser(theirs));
                Ok(ParserProc {
                    stream: ours,
                    pid: None,
                    started: Instant::now(),
                })
            };
            let mut parser = Parser::new(2, Box::new(spawn));
            parser.task_timeout = task_timeout;
            relay(&mut server, &mut worker, &mut parser);
        });
        FeedFetcherHost::from_stream(host_end, None).unwrap()
    }

    fn host_with_in_thread_children(
        parser: impl Fn(UnixStream) + Send + Clone + 'static,
    ) -> FeedFetcherHost {
        host_with_parser(parser, PARSE_TIMEOUT)
    }

    /// A stand-in for the parser that dies when it is given a feed whose
    /// body is `CRASH`, goes quiet for good on one whose body is `HANG`,
    /// and holds every other task until no task has arrived for a while,
    /// so that it can be in hand together with one of those, then answers
    /// it as parsing to nothing.
    fn fragile_parser(mut stream: UnixStream) {
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut held: Vec<u64> = Vec::new();
        loop {
            match read_frame_limited(&mut stream, MAX_FRAME_BYTES) {
                Ok(frame) => {
                    let Ok(FromFetcher::Parse { id, task }) = decode(&frame) else {
                        return;
                    };
                    match task {
                        ParseTask::Feed { body, .. } if body == b"CRASH" => return,
                        ParseTask::Feed { body, .. } if body == b"HANG" => {}
                        _ => held.push(id),
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    for id in std::mem::take(&mut held) {
                        let reply = ParseReply::Done(ParseOutput::Feed(ParseOutcome {
                            feed: None,
                            seconds: 0.0,
                        }));
                        let frame = encode_parsed(id, reply);
                        if write_frame_limited(&mut stream, &frame, MAX_FRAME_BYTES).is_err() {
                            return;
                        }
                    }
                }
                Err(_) => return,
            }
        }
    }

    /// When the parser dies with several tasks in hand, only the request
    /// whose task killed it is blamed: the others are retried and served,
    /// by a new parser.
    #[tokio::test]
    async fn only_the_task_that_kills_the_parser_is_blamed() {
        let host = Arc::new(host_with_in_thread_children(fragile_parser));
        let innocent = {
            let host = Arc::clone(&host);
            tokio::spawn(async move { host.parse(1, RSS.to_vec()).await })
        };
        // Let the innocent task reach the parser first.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let culprit = host.parse(2, b"CRASH".to_vec()).await;

        match culprit {
            Err(FetcherError::Crashed(why)) => assert!(why.contains("parser died"), "{why}"),
            other => panic!("got {other:?}"),
        }
        let innocent = innocent.await.unwrap();
        assert!(innocent.is_ok(), "got {innocent:?}");
        // The fetcher, and a parser, are still there for everything else.
        assert!(host.parse(3, RSS.to_vec()).await.is_ok());
        assert!(host.is_alive());
    }

    /// A parser that takes too long over a task is killed, and the request
    /// it was for is blamed once it has been tried on its own, while the
    /// others it was in hand with are served.
    #[tokio::test]
    async fn a_task_that_hangs_the_parser_is_blamed() {
        let host = Arc::new(host_with_parser(fragile_parser, Duration::from_millis(300)));
        let innocent = {
            let host = Arc::clone(&host);
            tokio::spawn(async move { host.parse(1, RSS.to_vec()).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        let culprit = host.parse(2, b"HANG".to_vec()).await;

        match culprit {
            Err(FetcherError::Crashed(why)) => assert!(why.contains("killed"), "{why}"),
            other => panic!("got {other:?}"),
        }
        assert!(innocent.await.unwrap().is_ok());
        assert!(host.is_alive());
    }

    /// The parser is given no more tasks than it has threads; the rest
    /// wait their turn rather than counting against the time limit.
    #[tokio::test]
    async fn tasks_beyond_the_parsers_threads_wait_their_turn() {
        let host = Arc::new(host_with_in_thread_children(|stream| {
            let _ = serve_parses(stream, 2);
        }));
        let parses: Vec<_> = (0..16)
            .map(|i| {
                let host = Arc::clone(&host);
                tokio::spawn(async move { host.parse(i, RSS.to_vec()).await })
            })
            .collect();
        for parse in parses {
            let outcome = parse.await.unwrap().unwrap();
            assert_eq!(outcome.feed.unwrap().entry_count(), 1);
        }
    }

    /// A parser that answers a task it was never given is stopped, and
    /// what it did have in hand is retried with the next one.
    #[tokio::test]
    async fn a_parser_that_breaks_the_protocol_is_replaced() {
        use std::sync::atomic::AtomicBool;
        let lied = Arc::new(AtomicBool::new(false));
        let host = host_with_in_thread_children({
            let lied = Arc::clone(&lied);
            move |mut stream: UnixStream| {
                if lied.swap(true, Ordering::SeqCst) {
                    let _ = serve_parses(stream, 1);
                    return;
                }
                let Ok(frame) = read_frame_limited(&mut stream, MAX_FRAME_BYTES) else {
                    return;
                };
                let Ok(FromFetcher::Parse { id, .. }) = decode(&frame) else {
                    return;
                };
                let reply = ParseReply::Failed {
                    message: "not yours".into(),
                };
                let frame = encode_parsed(id.wrapping_add(1000), reply);
                let _ = write_frame_limited(&mut stream, &frame, MAX_FRAME_BYTES);
                // Stay up: it is the supervisor that must end this.
                let _ = read_frame_limited(&mut stream, MAX_FRAME_BYTES);
            }
        });
        let outcome = host.parse(1, RSS.to_vec()).await.unwrap();
        assert_eq!(outcome.feed.unwrap().entry_count(), 1);
    }

    #[test]
    fn requests_from_the_server_are_scrubbed_once_relayed() {
        let mut buf = *b"password";
        drop(Scrubbed(&mut buf[..]));
        assert_eq!(buf, [0; 8]);
    }

    #[tokio::test]
    async fn parse_requests_round_trip_through_a_worker() {
        let host = host_with_in_thread_worker();
        let outcome = host.parse(7, RSS.to_vec()).await.unwrap();
        let feed = outcome.feed.expect("parsed");
        assert_eq!(feed.format(), "rss");
        assert_eq!(feed.entry_count(), 1);

        let outcome = host.parse(7, b"not xml".to_vec()).await.unwrap();
        assert!(outcome.feed.is_none());
        assert!(host.is_alive());
    }

    /// A `file://` body is not capped by `max_feed_bytes`, and used to
    /// cost about four bytes per byte on the wire; one well over a quarter
    /// of the frame limit must still reach the worker.
    #[tokio::test]
    async fn large_parse_bodies_fit_in_a_frame() {
        let body = vec![b'x'; MAX_FRAME_BYTES / 3];
        let host = host_with_in_thread_worker();
        let outcome = host.parse(7, body).await.unwrap();
        assert!(outcome.feed.is_none());
        assert!(host.is_alive());
    }

    #[test]
    fn parse_bodies_are_encoded_as_raw_bytes() {
        let body = vec![0xffu8; 4096];
        let encoded = encode(&ToFetcher::Request(Request {
            id: u64::MAX,
            job: Job::Parse {
                feed_id: i64::MIN,
                body: body.clone(),
            },
        }))
        .unwrap();
        assert!(encoded.len() <= body.len() + 32, "{} bytes", encoded.len());
        match decode(&encoded).unwrap() {
            ToFetcher::Request(Request {
                job: Job::Parse { body: decoded, .. },
                ..
            }) => assert_eq!(decoded, body),
            other => panic!("wrong message: {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_requests_are_served_concurrently() {
        use axum::{routing::get, Router};

        // The slow feed answers only once the test has seen the fast one
        // come back, so the fast request can't have waited behind it.
        let release = Arc::new(tokio::sync::Notify::new());
        let app = Router::new()
            .route(
                "/slow",
                get({
                    let release = Arc::clone(&release);
                    || async move {
                        release.notified().await;
                        RSS
                    }
                }),
            )
            .route("/fast", get(|| async { RSS }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.ok() });

        let host = Arc::new(host_with_in_thread_worker());
        let slow = {
            let host = Arc::clone(&host);
            let url = format!("http://{addr}/slow");
            tokio::spawn(async move { host.fetch(spec(&url)).await })
        };

        let fast = tokio::time::timeout(
            Duration::from_secs(5),
            host.fetch(spec(&format!("http://{addr}/fast"))),
        )
        .await
        .expect("a fast feed waited behind a slow one");
        assert!(matches!(fast, Ok(FetchReply::Body(_))), "got {fast:?}");
        release.notify_one();
        assert!(matches!(slow.await.unwrap(), Ok(FetchReply::Body(_))));
    }

    /// The worker fetches through the proxy in the spec, with the proxy's
    /// own hostname resolved by the server like any other.
    #[tokio::test]
    async fn fetches_go_through_the_proxy_in_the_spec() {
        use crate::config::ProxySettings;
        use crate::fetcher::tests::{proxied_spec, start_proxy};

        let addr = start_proxy().await;
        let host = host_with_in_thread_worker();
        let proxy = ProxySettings {
            url: Some(format!("http://localhost:{}", addr.port())),
            no_proxy: None,
        };
        let reply = host.fetch(proxied_spec(proxy)).await.unwrap();
        assert!(matches!(reply, FetchReply::Body(_)), "got {reply:?}");
    }

    /// A SOCKS5 proxy works from the worker too, with the proxy's own
    /// hostname resolved by the server and the feed's by the proxy.
    #[tokio::test]
    async fn fetches_go_through_a_socks5_proxy_in_the_spec() {
        use crate::config::ProxySettings;
        use crate::fetcher::tests::{proxied_spec, start_socks_proxy};

        let addr = start_socks_proxy().await;
        let host = host_with_in_thread_worker();
        let proxy = ProxySettings {
            url: Some(format!("socks5h://localhost:{}", addr.port())),
            no_proxy: None,
        };
        let reply = host.fetch(proxied_spec(proxy)).await.unwrap();
        assert!(matches!(reply, FetchReply::Body(_)), "got {reply:?}");
    }

    /// When the far end goes away, outstanding and later requests fail
    /// promptly instead of waiting out their deadlines.
    #[tokio::test]
    async fn a_dead_channel_fails_requests_immediately() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let host = FeedFetcherHost::from_stream(ours, None).unwrap();
        drop(theirs);

        let start = Instant::now();
        let err = host.parse(1, RSS.to_vec()).await.unwrap_err();
        assert!(matches!(err, FetcherError::Gone), "got {err:?}");
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(!host.is_alive());
    }

    /// A host whose far end stands in for a supervisor over a worker that
    /// dies whenever it is given a `Parse` job whose body is `CRASH`,
    /// answering every job it then had in hand with
    /// [`JobResult::WorkerExited`]. Other jobs are held until no frame has
    /// arrived for a while, so that they can be in hand together with a
    /// crashing one, and are then answered as parsing to nothing.
    fn host_with_fragile_worker() -> FeedFetcherHost {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        theirs
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        std::thread::spawn(move || {
            let mut held: Vec<u64> = Vec::new();
            let answer = |theirs: &mut UnixStream, id, result| {
                let reply = encode_response(id, result);
                write_frame_limited(theirs, &reply, MAX_FRAME_BYTES).is_ok()
            };
            loop {
                match read_frame_limited(&mut theirs, MAX_FRAME_BYTES) {
                    Ok(frame) => {
                        let Ok(ToFetcher::Request(Request { id, job })) = decode(&frame) else {
                            continue;
                        };
                        held.push(id);
                        if matches!(&job, Job::Parse { body, .. } if body == b"CRASH") {
                            let in_flight = held.len() as u32;
                            for id in std::mem::take(&mut held) {
                                let why = "the fetcher worker died (test)".to_string();
                                if !answer(
                                    &mut theirs,
                                    id,
                                    JobResult::WorkerExited { in_flight, why },
                                ) {
                                    return;
                                }
                            }
                        }
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) =>
                    {
                        for id in std::mem::take(&mut held) {
                            let parsed = JobResult::Parsed(ParseOutcome {
                                feed: None,
                                seconds: 0.0,
                            });
                            if !answer(&mut theirs, id, parsed) {
                                return;
                            }
                        }
                    }
                    Err(_) => return,
                }
            }
        });
        FeedFetcherHost::from_stream(ours, None).unwrap()
    }

    /// When a worker dies with several jobs in hand, only the one that
    /// killed it is blamed: the others are retried and served.
    #[tokio::test]
    async fn only_the_job_that_kills_the_worker_is_blamed() {
        let host = Arc::new(host_with_fragile_worker());
        let innocent = {
            let host = Arc::clone(&host);
            tokio::spawn(async move { host.parse(1, RSS.to_vec()).await })
        };
        // Let the innocent job reach the worker first.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let culprit = host.parse(2, b"CRASH".to_vec()).await;

        assert!(
            matches!(culprit, Err(FetcherError::Crashed(_))),
            "got {culprit:?}"
        );
        let innocent = innocent.await.unwrap();
        assert!(innocent.is_ok(), "got {innocent:?}");
        assert!(host.is_alive());
    }

    /// A job that kills the worker on its own is blamed, with how the
    /// worker died.
    #[tokio::test]
    async fn a_job_that_kills_the_worker_alone_is_blamed() {
        let host = host_with_fragile_worker();
        let err = host.parse(1, b"CRASH".to_vec()).await.unwrap_err();
        match err {
            FetcherError::Crashed(why) => assert!(why.contains("died"), "{why}"),
            other => panic!("got {other:?}"),
        }
        // The fetcher is still there for everything else.
        assert!(host.parse(1, RSS.to_vec()).await.is_ok());
    }

    /// Whoever waits on [`FeedFetcherHost::closed`] learns the channel has
    /// failed without having a request of its own outstanding, and later
    /// waiters return at once.
    #[tokio::test]
    async fn closed_resolves_when_the_channel_fails() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let host = FeedFetcherHost::from_stream(ours, None).unwrap();
        let open = tokio::time::timeout(Duration::from_millis(100), host.closed()).await;
        assert!(open.is_err(), "closed() returned while the channel was up");

        drop(theirs);
        tokio::time::timeout(Duration::from_secs(5), host.closed())
            .await
            .expect("closed() should return once the channel fails");
        tokio::time::timeout(Duration::from_secs(1), host.closed())
            .await
            .expect("closed() should return at once when already closed");
    }

    /// Asset downloads, icon searches and image extraction all run in the
    /// worker, with hostnames resolved by the server as for feeds.
    #[tokio::test]
    async fn asset_jobs_round_trip_through_a_worker() {
        use crate::fetcher::assets::{AssetKind, AssetReply, AssetSpec, PageSpec};
        use axum::{routing::get, Router};

        const PNG: &[u8] = b"\x89PNG not really";
        let app = Router::new()
            .route(
                "/img.png",
                get(|| async { ([("content-type", "image/png")], PNG) }),
            )
            .route(
                "/evil.svg",
                get(|| async {
                    (
                        [("content-type", "image/svg+xml")],
                        r#"<svg onload="alert(1)"><script>alert(2)</script><rect/></svg>"#,
                    )
                }),
            )
            .route(
                "/not.svg",
                get(|| async { ([("content-type", "image/svg+xml")], "<html/>") }),
            )
            .route(
                "/",
                get(|| async {
                    (
                        [("content-type", "text/html")],
                        r#"<head><link rel="icon" href="/img.png"></head>"#,
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.ok() });
        let base = format!("http://localhost:{port}");

        let host = host_with_in_thread_worker();
        let asset = |path: &str, kind| AssetSpec {
            url: format!("{base}{path}"),
            kind,
            proxy: Default::default(),
        };

        match host
            .fetch_asset(asset("/img.png", AssetKind::InlineImg))
            .await
            .unwrap()
        {
            AssetReply::Fetched(a) => {
                assert_eq!(a.content_type, "image/png");
                assert_eq!(a.bytes, PNG);
            }
            other => panic!("expected the image, got {other:?}"),
        }
        match host
            .fetch_asset(asset("/evil.svg", AssetKind::InlineImg))
            .await
            .unwrap()
        {
            AssetReply::Fetched(a) => {
                assert_eq!(a.content_type, "image/svg+xml");
                let svg = String::from_utf8(a.bytes).unwrap();
                assert!(svg.contains("<rect/>"), "{svg}");
                assert!(!svg.contains("script") && !svg.contains("onload"), "{svg}");
            }
            other => panic!("expected the sanitized SVG, got {other:?}"),
        }
        let reply = host
            .fetch_asset(asset("/not.svg", AssetKind::InlineImg))
            .await
            .unwrap();
        assert!(matches!(reply, AssetReply::UnsafeSvg), "got {reply:?}");
        let reply = host
            .fetch_asset(asset("/evil.svg", AssetKind::Enclosure))
            .await
            .unwrap();
        assert!(
            matches!(reply, AssetReply::DisallowedType { .. }),
            "got {reply:?}"
        );
        let reply = host
            .fetch_asset(asset("/missing.png", AssetKind::InlineImg))
            .await
            .unwrap();
        assert!(
            matches!(reply, AssetReply::HttpStatus { status: 404 }),
            "got {reply:?}"
        );

        let icons = host
            .find_page_icons(PageSpec {
                url: format!("{base}/"),
                proxy: Default::default(),
            })
            .await
            .unwrap();
        assert_eq!(icons.icons, [format!("{base}/img.png")]);
        assert!(icons.problem.is_none(), "{:?}", icons.problem);

        let images = host
            .extract_images(
                r#"<img src="/a.png"><img src="javascript:x"><img src="/a.png">"#.into(),
                format!("{base}/post/1"),
            )
            .await
            .unwrap();
        assert_eq!(images, [format!("{base}/a.png")]);
        assert!(host.is_alive());
    }

    #[test]
    fn respawns_back_off_only_when_crash_looping() {
        assert_eq!(respawn_delay(0), Duration::ZERO);
        assert_eq!(respawn_delay(1), Duration::ZERO);
        assert_eq!(respawn_delay(2), Duration::from_secs(1));
        assert_eq!(respawn_delay(3), Duration::from_secs(2));
        assert_eq!(respawn_delay(40), MAX_RESPAWN_DELAY);
    }

    #[test]
    fn oversized_replies_become_failures() {
        let huge = "x".repeat(MAX_FRAME_BYTES + 1);
        let encoded = encode_response(
            9,
            JobResult::Failed {
                message: huge.clone(),
            },
        );
        assert!(encoded.len() < MAX_FRAME_BYTES);
        let decoded: FromFetcher = decode(&encoded).unwrap();
        match decoded {
            FromFetcher::Response(r) => {
                assert_eq!(r.id, 9);
                assert!(matches!(r.result, JobResult::Failed { .. }));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn kinds_and_ids_can_be_read_without_decoding_the_body() {
        let frame = encode(&ToFetcher::Request(Request {
            id: 42,
            job: Job::Parse {
                feed_id: 1,
                body: vec![1, 2, 3],
            },
        }))
        .unwrap();
        assert!(matches!(
            peek_to(&frame),
            Some(PeekTo::Request(IdOnly { id: 42 }))
        ));

        let frame = encode(&ToFetcher::Resolved {
            id: 7,
            result: Ok(vec!["127.0.0.1:0".parse().unwrap()]),
        })
        .unwrap();
        assert!(matches!(
            peek_to(&frame),
            Some(PeekTo::Resolved(IdOnly { id: 7 }))
        ));

        let frame = encode(&FromFetcher::Resolve {
            id: 3,
            host: "example.com".into(),
        })
        .unwrap();
        assert!(matches!(
            peek_from(&frame),
            Some(PeekFrom::Resolve(IdOnly { id: 3 }))
        ));

        let frame = encode(&FromFetcher::Response(Response {
            id: 11,
            result: JobResult::Failed {
                message: "gone".into(),
            },
        }))
        .unwrap();
        assert!(matches!(
            peek_from(&frame),
            Some(PeekFrom::Response(IdOnly { id: 11 }))
        ));

        let frame = encode(&ToFetcher::Parsed {
            id: 5,
            reply: ParseReply::Failed {
                message: "x".into(),
            },
        })
        .unwrap();
        assert!(matches!(
            peek_to(&frame),
            Some(PeekTo::Parsed(IdOnly { id: 5 }))
        ));

        let frame = encode(&FromFetcher::Parse {
            id: 6,
            task: ParseTask::Images {
                content: String::new(),
                base: String::new(),
            },
        })
        .unwrap();
        assert!(matches!(
            peek_from(&frame),
            Some(PeekFrom::Parse(IdOnly { id: 6 }))
        ));

        // An empty frame, and a variant index neither side defines.
        assert!(peek_to(b"").is_none());
        assert!(peek_to(&[3, 1]).is_none());
        assert!(peek_from(&[3, 1]).is_none());
    }

    #[test]
    fn the_server_refuses_to_look_up_nonsense() {
        assert!(lookup_host("").is_err());
        assert!(lookup_host(&"a".repeat(MAX_HOSTNAME_LEN + 1)).is_err());
        assert!(lookup_host("bad\0host").is_err());
        assert!(lookup_host("localhost").is_ok());
    }

    /// A feed named by hostname is resolved by the server side of the
    /// channel, and a name that does not resolve surfaces as an ordinary
    /// network failure of that one fetch.
    #[tokio::test]
    async fn hostnames_are_resolved_through_the_server() {
        use axum::{routing::get, Router};

        let app = Router::new().route("/feed", get(|| async { RSS }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.ok() });

        let host = host_with_in_thread_worker();
        let reply = host
            .fetch(spec(&format!("http://localhost:{port}/feed")))
            .await;
        assert!(matches!(reply, Ok(FetchReply::Body(_))), "got {reply:?}");

        let reply = host
            .fetch(spec("http://no-such-host.invalid/feed"))
            .await
            .unwrap();
        match reply {
            FetchReply::Network { message, .. } => {
                assert!(!message.is_empty());
            }
            other => panic!("expected a network failure, got {other:?}"),
        }
        assert!(host.is_alive());
    }
}
