//! Length-prefixed framing for inter-process messages, the codec for
//! their bodies, and the script host's message types.
//!
//! Frames are a little-endian `u32` byte count followed by that many
//! bytes of [postcard]-encoded message, written with [`encode`] and read
//! with [`decode`]. The count and the body go out in a single vectored
//! write, so a frame costs the writer one system call and never reaches
//! the peer as a lone length prefix. Postcard is compact and not
//! self-describing: fields are written in declaration order with no
//! names, and enum variants by index. That is safe here because both ends of every channel are the
//! same binary, so the message types can never disagree.
//!
//! Both directions cap the frame size: the parent because a
//! compromised child must not be able to drive it into an allocation it
//! cannot afford, and the child for symmetry. The script host channel uses
//! [`MAX_FRAME_BYTES`]; the feed fetcher channel, which carries whole
//! parsed feeds, passes its own larger limit to the `_limited` variants.
//!
//! The script host protocol is request/response, with many requests in
//! flight at once: every request the server writes carries an id
//! ([`ToHost`]), and is answered by exactly one response with the same id
//! ([`FromHost`]), including for observe-only events whose result is
//! discarded. Responses come back in whatever order the script host
//! finishes them. Each travels as a [`HostMessage::Done`], which also names
//! the events the host has handlers for ([`Subscriptions`]), so the server
//! can skip sending events nothing would see. While it works on a request,
//! the script host may ask the server for something on a plugin's behalf
//! with [`HostMessage::Call`], under the request's id; the server answers
//! with [`HostRequest::CallResult`] under the same id, so calls nest inside
//! their request. A frame with an id the other side is not waiting on means
//! the two have fallen out of step, and is treated as corruption. The feed
//! fetcher's protocol is multiplexed too; see
//! [`crate::process::feed_fetcher`].

use crate::scripting::{
    EntryPatch, Event, EventPayload, EventSet, FeedEntry, FetchSchedule, ScanSummary,
    ScheduleDecision, ScriptSource, ServiceCall, ServiceReply, WasmComponent,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::borrow::Cow;
use std::io::{self, IoSlice, Read, Write};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest frame either side will write or accept.
///
/// Entries carry a feed's `content` field, which is capped upstream by
/// the `max_feed_bytes` setting; 8 MiB leaves generous room for a single
/// entry plus its encoding without letting one message balloon the
/// peer's memory.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Identifies a request to the script host, and everything sent about it.
pub type RequestId = u64;

/// A frame from the server to the script host: a request, or the answer to a
/// call made while the request `id` is served.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToHost<'a> {
    /// The request this is, or is about.
    pub id: RequestId,
    /// Whether other requests were in flight when this one was sent. The
    /// script host then has another thread take over reading at once, so
    /// that a request arriving while this one is served is read and served
    /// alongside it. Otherwise the thread that read this one serves it, and
    /// reads the next itself, so that requests sent one after another are
    /// served on one thread, whose caches are warm with the plugins.
    pub busy: bool,
    /// What the server sends.
    pub request: HostRequest<'a>,
}

/// A [`ToHost`] that borrows its request, to send one without copying it.
/// Encodes exactly as the [`ToHost`] it stands for.
#[derive(Debug, Serialize)]
pub struct ToHostRef<'r, 'a> {
    /// The request this is, or is about.
    pub id: RequestId,
    /// Whether other requests were in flight when this one was sent. The
    /// script host then has another thread take over reading at once, so
    /// that a request arriving while this one is served is read and served
    /// alongside it. Otherwise the thread that read this one serves it, and
    /// reads the next itself, so that requests sent one after another are
    /// served on one thread, whose caches are warm with the plugins.
    pub busy: bool,
    /// What the server sends.
    pub request: &'r HostRequest<'a>,
}

/// A message from the server to the script host.
///
/// Entries are [`Cow`]s so that the server can send one it holds without
/// copying it. They arrive owned.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HostRequest<'a> {
    /// Discard the current plugins, if any, and load `sources` in their
    /// place, each plugin with its config.
    ///
    /// Sent when the server starts, and whenever plugins are reloaded.
    /// The components of `sources` carry only their hash: each was sent
    /// beforehand with [`HostRequest::PutComponent`].
    Reload { sources: Vec<ScriptSource> },

    /// Run `entry` through the `entry.ingest` handler chain.
    TransformEntry { entry: Cow<'a, FeedEntry> },

    /// Fire `entry.parsed` for `entry`. Answered with [`HostResponse::Ack`].
    EntryParsed { entry: Cow<'a, FeedEntry> },

    /// Fire an observe-only event. The response carries no data, but is
    /// still awaited to keep the stream in lockstep.
    Observe { event: Event, payload: EventPayload },

    /// Pass `entries` to the handler of the scan `scan_id`.
    Scan {
        scan_id: u64,
        entries: Vec<FeedEntry>,
    },

    /// Forget the scan `scan_id`, first calling its `on_done` callback
    /// with `summary` if the scan completed. Answered with
    /// [`HostResponse::Ack`].
    FinishScan {
        scan_id: u64,
        summary: Option<ScanSummary>,
    },

    /// The server's answer to a [`FromHost::Call`].
    CallResult {
        result: Result<ServiceReply, String>,
    },

    /// Run `schedule` through the `fetch.schedule` handler chain. Answered
    /// with [`HostResponse::Schedule`].
    Schedule { schedule: FetchSchedule },

    /// Compile `component`, the code of the WebAssembly plugin named
    /// `plugin`, and keep it, so that a later [`HostRequest::Reload`] can
    /// name it by hash alone. Answered
    /// with [`HostResponse::Ack`], or [`HostResponse::Failed`] if it does
    /// not compile.
    ///
    /// Each component gets a message of its own, so that a few large ones
    /// don't make a `Reload` too large for a frame.
    PutComponent {
        plugin: String,
        component: WasmComponent,
    },
}

/// A message from the script host back to the server.
// One is alive per request at a time, so boxing the entry would cost an
// allocation per entry to save a few hundred bytes of stack.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HostResponse {
    /// The plugins were reloaded; `loaded` is the number of plugins loaded.
    Reloaded { loaded: usize },

    /// The handler chain ran. `patch` holds the fields the handlers
    /// changed, or is `None` if a handler filtered the entry out and it
    /// should not be inserted. `runs` is how long each plugin's handler
    /// took.
    Entry {
        patch: Option<EntryPatch>,
        runs: Vec<PluginTime>,
    },

    /// An observe event was dispatched.
    Ack,

    /// What the scan handler returned for each entry it handled (a
    /// prefix of those sent), or `None` if the host has no such scan.
    Scanned {
        entries: Option<Vec<Option<FeedEntry>>>,
    },

    /// The request could not be served. The server treats this as a
    /// script-level failure, not a dead host: for `entry.ingest` the
    /// entry passes through unmodified, matching the in-process
    /// runner's contract.
    Failed { message: String },

    /// The `fetch.schedule` handler chain ran. `None` means the wait was
    /// left as it was.
    Schedule { decision: Option<ScheduleDecision> },
}

/// How long the handler of the plugin `plugin`, by its index in the order
/// plugins were loaded in, took, in seconds. See [`HostResponse::Entry`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PluginTime {
    /// The plugin's index among the sources of the last [`HostRequest::Reload`].
    pub plugin: u32,
    /// How long its handler took, in seconds.
    pub seconds: f64,
}

/// The events the script host's plugins have at least one handler for, as
/// of a response.
///
/// Responses to requests served at the same time can arrive in any order,
/// so each carries a sequence number, taken before the events were read: of
/// two reports, the one with the higher number is the more recent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscriptions {
    /// Grows with each report.
    pub seq: u64,
    /// The events.
    pub events: EventSet,
}

/// A frame from the script host to the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FromHost {
    /// The request the message is about.
    pub id: RequestId,
    /// The message.
    pub message: HostMessage,
}

/// A message from the script host to the server, about a request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HostMessage {
    /// The answer to the request, which ends it.
    Done {
        response: HostResponse,
        /// The events the host's plugins have at least one handler for once the
        /// request has been served. Plugins may register handlers at any
        /// time, so this is reported with every response.
        subscribed: Subscriptions,
    },

    /// Not a response: a plugin asks the server for something while the
    /// request is being served. The server answers with
    /// [`HostRequest::CallResult`], under the request's id, and keeps waiting
    /// for the response.
    Call { plugin: String, call: ServiceCall },
}

/// Why a frame body could not be encoded or decoded.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// The message could not be encoded, or the frame does not hold a
    /// valid encoding of the expected type.
    #[error(transparent)]
    Postcard(#[from] postcard::Error),

    /// The frame held a valid message followed by bytes that belong to
    /// no message.
    #[error("{0} unexpected bytes after the message")]
    TrailingBytes(usize),
}

/// Encode `msg` as a frame body.
///
/// # Examples
///
/// ```
/// use kiki_rss::process::ipc::{decode, encode, HostResponse};
///
/// let body = encode(&HostResponse::Reloaded { loaded: 2 })?;
/// let back: HostResponse = decode(&body)?;
/// assert!(matches!(back, HostResponse::Reloaded { loaded: 2 }));
/// # Ok::<(), kiki_rss::process::ipc::CodecError>(())
/// ```
///
/// # Errors
///
/// Returns [`CodecError::Postcard`] if `msg`'s `Serialize` impl fails.
/// The size limit is not checked here; the `write_frame` functions
/// enforce it.
pub fn encode<T: Serialize + ?Sized>(msg: &T) -> Result<Vec<u8>, CodecError> {
    Ok(postcard::to_stdvec(msg)?)
}

/// Encode `msg` as a frame body into `buf`, replacing what it held, so that
/// a buffer kept for the purpose saves allocating one per message.
///
/// # Errors
///
/// As for [`encode`].
pub fn encode_into<T: Serialize + ?Sized>(msg: &T, buf: &mut Vec<u8>) -> Result<(), CodecError> {
    buf.clear();
    let taken = std::mem::take(buf);
    *buf = postcard::to_extend(msg, taken)?;
    Ok(())
}

/// Decode a frame body written by [`encode`].
///
/// The whole frame must be consumed: leftover bytes mean the peer and
/// this side disagree about the message, which is treated as corruption.
///
/// # Errors
///
/// Returns [`CodecError::Postcard`] if `frame` is not a valid encoding of
/// `T`, and [`CodecError::TrailingBytes`] if it is followed by anything.
pub fn decode<T: DeserializeOwned>(frame: &[u8]) -> Result<T, CodecError> {
    let (msg, rest) = postcard::take_from_bytes(frame)?;
    if rest.is_empty() {
        Ok(msg)
    } else {
        Err(CodecError::TrailingBytes(rest.len()))
    }
}

/// Decode only the start of a frame body, ignoring whatever follows.
///
/// Because postcard writes fields in declaration order, a type that
/// mirrors a message's variants and its leading fields can pick those out
/// without touching the rest of the frame — which is how the feed
/// fetcher's supervisor reads request ids out of frames that may carry
/// megabytes of feed. The mirror's variants must be declared in the same
/// order as the message's.
///
/// # Errors
///
/// Returns [`CodecError::Postcard`] if the start of `frame` is not a valid
/// encoding of `T`.
pub fn decode_prefix<T: DeserializeOwned>(frame: &[u8]) -> Result<T, CodecError> {
    Ok(postcard::take_from_bytes(frame)?.0)
}

/// Write `payload` as a single length-prefixed frame.
///
/// # Errors
///
/// Returns [`io::ErrorKind::InvalidInput`] if the payload exceeds
/// [`MAX_FRAME_BYTES`], and propagates any error from the underlying
/// writer.
pub fn write_frame<W: Write>(w: &mut W, payload: &[u8]) -> io::Result<()> {
    write_frame_limited(w, payload, MAX_FRAME_BYTES)
}

/// [`write_frame`] with a caller-chosen size limit in place of
/// [`MAX_FRAME_BYTES`].
///
/// # Errors
///
/// As for [`write_frame`], with `max` as the limit.
pub fn write_frame_limited<W: Write>(w: &mut W, payload: &[u8], max: usize) -> io::Result<()> {
    let len = frame_len(payload, max)?.to_le_bytes();
    let mut bufs = [IoSlice::new(&len), IoSlice::new(payload)];
    let mut bufs = &mut bufs[..];
    while !bufs.is_empty() {
        match w.write_vectored(bufs) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => IoSlice::advance_slices(&mut bufs, n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    w.flush()
}

/// Async counterpart of [`write_frame_limited`].
///
/// # Errors
///
/// As for [`write_frame_limited`].
pub async fn write_frame_async<W: AsyncWrite + Unpin>(
    w: &mut W,
    payload: &[u8],
    max: usize,
) -> io::Result<()> {
    let len = frame_len(payload, max)?.to_le_bytes();
    let mut bufs = [IoSlice::new(&len), IoSlice::new(payload)];
    let mut bufs = &mut bufs[..];
    while !bufs.is_empty() {
        match w.write_vectored(bufs).await {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => IoSlice::advance_slices(&mut bufs, n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    w.flush().await
}

/// The length prefix for `payload`, or an error if it is over `max`.
fn frame_len(payload: &[u8], max: usize) -> io::Result<u32> {
    u32::try_from(payload.len())
        .ok()
        .filter(|_| payload.len() <= max)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "frame of {} bytes exceeds the {} byte limit",
                    payload.len(),
                    max
                ),
            )
        })
}

/// Read a single length-prefixed frame.
///
/// # Errors
///
/// Returns [`io::ErrorKind::UnexpectedEof`] when the peer closes the
/// connection between frames, and [`io::ErrorKind::InvalidData`] if the
/// peer announces a frame larger than [`MAX_FRAME_BYTES`] — that is a
/// protocol violation, and the length is never used to size an
/// allocation before it has been checked.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    read_frame_limited(r, MAX_FRAME_BYTES)
}

/// [`read_frame`] with a caller-chosen size limit in place of
/// [`MAX_FRAME_BYTES`].
///
/// # Errors
///
/// As for [`read_frame`], with `max` as the limit.
pub fn read_frame_limited<R: Read>(r: &mut R, max: usize) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = checked_len(len_buf, max)?;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(payload)
}

/// Most bytes a buffer passed to [`read_frame_into`] keeps between frames,
/// as do the buffers the script host's messages are encoded into.
/// One that grew past this for a large frame is shrunk back, so that one
/// large entry does not pin megabytes for the life of the channel.
pub(crate) const KEPT_BUFFER_BYTES: usize = 1024 * 1024;

/// [`read_frame`] into `buf`, which is kept between frames so that most
/// frames need no allocation, rather than into a new vector. Returns the
/// frame, which is the start of `buf`.
///
/// # Errors
///
/// As for [`read_frame`].
pub fn read_frame_into<'b, R: Read>(r: &mut R, buf: &'b mut Vec<u8>) -> io::Result<&'b [u8]> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = checked_len(len_buf, MAX_FRAME_BYTES)?;
    if buf.capacity() > KEPT_BUFFER_BYTES && len <= KEPT_BUFFER_BYTES {
        buf.truncate(KEPT_BUFFER_BYTES);
        buf.shrink_to(KEPT_BUFFER_BYTES);
    }
    // Zeroed only where it grows: what a previous frame left is overwritten.
    if buf.len() < len {
        buf.resize(len, 0);
    }
    let frame = buf
        .get_mut(..len)
        .ok_or_else(|| io::Error::other("frame buffer too short"))?;
    r.read_exact(frame)?;
    Ok(frame)
}

/// Async counterpart of [`read_frame_limited`].
///
/// # Errors
///
/// As for [`read_frame_limited`].
pub async fn read_frame_async<R: AsyncRead + Unpin>(r: &mut R, max: usize) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = checked_len(len_buf, max)?;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).await?;
    Ok(payload)
}

/// Decode a length prefix, refusing anything over `max` before it is used
/// to size an allocation.
fn checked_len(len_buf: [u8; 4], max: usize) -> io::Result<usize> {
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "peer announced a {} byte frame, over the {} byte limit",
                len, max
            ),
        ));
    }
    Ok(len)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"hello").unwrap();
        write_frame(&mut buf, b"world").unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        assert_eq!(read_frame(&mut cursor).unwrap(), b"hello");
        assert_eq!(read_frame(&mut cursor).unwrap(), b"world");
    }

    /// A writer that takes at most three bytes per call, as a socket with a
    /// full buffer might.
    struct Trickle(Vec<u8>);

    impl Write for Trickle {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let n = buf.len().min(3);
            self.0.extend_from_slice(&buf[..n]);
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A short vectored write resumes where it stopped, within and across
    /// the length prefix and the body.
    #[test]
    fn short_writes_are_resumed() {
        let mut w = Trickle(Vec::new());
        write_frame(&mut w, b"hello, world").unwrap();
        let mut cursor = std::io::Cursor::new(w.0);
        assert_eq!(read_frame(&mut cursor).unwrap(), b"hello, world");
    }

    /// Frames go through a real socket intact, read through a buffer that
    /// may hold more than one of them.
    #[test]
    fn frames_cross_a_socket() {
        let (mut a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        let big = vec![7u8; 300_000];
        let writer = std::thread::spawn(move || {
            write_frame(&mut a, b"one").unwrap();
            write_frame(&mut a, &big).unwrap();
            write_frame(&mut a, b"").unwrap();
            write_frame(&mut a, b"three").unwrap();
        });
        let mut r = std::io::BufReader::new(b);
        assert_eq!(read_frame(&mut r).unwrap(), b"one");
        assert_eq!(read_frame(&mut r).unwrap(), vec![7u8; 300_000]);
        assert!(read_frame(&mut r).unwrap().is_empty());
        assert_eq!(read_frame(&mut r).unwrap(), b"three");
        writer.join().unwrap();
    }

    #[test]
    fn empty_frames_round_trip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"").unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        assert!(read_frame(&mut cursor).unwrap().is_empty());
    }

    #[test]
    fn oversized_writes_are_refused() {
        let payload = vec![0u8; MAX_FRAME_BYTES + 1];
        let mut buf = Vec::new();
        let err = write_frame(&mut buf, &payload).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(buf.is_empty(), "nothing should be written for a bad frame");
    }

    /// A hostile peer announcing a huge frame must be rejected on the
    /// length alone, before anything is allocated for the body.
    #[test]
    fn oversized_announcements_are_refused_without_allocating() {
        let mut framed = Vec::new();
        framed.extend_from_slice(&u32::MAX.to_le_bytes());
        let mut cursor = std::io::Cursor::new(framed);
        let err = read_frame(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_truncated_body_is_an_error() {
        let mut framed = Vec::new();
        framed.extend_from_slice(&8u32.to_le_bytes());
        framed.extend_from_slice(b"only4");
        let mut cursor = std::io::Cursor::new(framed);
        let err = read_frame(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn closing_between_frames_is_an_eof() {
        let mut cursor = std::io::Cursor::new(Vec::new());
        let err = read_frame(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_custom_limit_is_enforced_both_ways() {
        let mut buf = Vec::new();
        let err = write_frame_limited(&mut buf, b"12345", 4).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

        write_frame_limited(&mut buf, b"12345", 5).unwrap();
        let err = read_frame_limited(&mut std::io::Cursor::new(&buf), 4).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            read_frame_limited(&mut std::io::Cursor::new(&buf), 5).unwrap(),
            b"12345"
        );
    }

    #[tokio::test]
    async fn async_frames_interoperate_with_blocking_ones() {
        let mut buf = Vec::new();
        write_frame_async(&mut buf, b"from async", 64)
            .await
            .unwrap();
        write_frame(&mut buf, b"from sync").unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        assert_eq!(read_frame(&mut cursor).unwrap(), b"from async");
        assert_eq!(
            read_frame_async(&mut cursor, 64).await.unwrap(),
            b"from sync"
        );
    }

    #[test]
    fn requests_and_responses_survive_a_round_trip() {
        let entry = FeedEntry {
            id: None,
            feed_id: 7,
            syndication_format: "rss".to_string(),
            guid: "urn:kiki:1".to_string(),
            published_at: Some(1_700_000_000),
            title: "title".to_string(),
            url: Some("https://example.com/1".to_string()),
            content: Some("<p>body</p>".to_string()),
            authors: vec!["Ada".to_string()],
            categories: vec!["news".to_string()],
            tags: vec!["a".to_string(), "b".to_string()],
            cache_assets: true,
        };

        let req = ToHost {
            id: 7,
            busy: false,
            request: HostRequest::TransformEntry {
                entry: Cow::Borrowed(&entry),
            },
        };
        let encoded = encode(&req).unwrap();
        let decoded: ToHost<'static> = decode(&encoded).unwrap();
        assert_eq!(decoded.id, 7);
        match decoded.request {
            HostRequest::TransformEntry { entry: e } => {
                assert_eq!(e.guid, entry.guid);
                assert_eq!(e.tags, entry.tags);
            }
            other => panic!("wrong variant: {other:?}"),
        }

        let patch = EntryPatch {
            title: Some("new".to_string()),
            ..EntryPatch::default()
        };
        let resp = FromHost {
            id: 7,
            message: HostMessage::Done {
                response: HostResponse::Entry {
                    patch: Some(patch.clone()),
                    runs: vec![PluginTime {
                        plugin: 1,
                        seconds: 0.5,
                    }],
                },
                subscribed: Subscriptions {
                    seq: 3,
                    events: EventSet::ALL,
                },
            },
        };
        let encoded = encode(&resp).unwrap();
        let decoded: FromHost = decode(&encoded).unwrap();
        assert_eq!(decoded.id, 7);
        match decoded.message {
            HostMessage::Done {
                response: HostResponse::Entry { patch: p, runs },
                subscribed,
            } => {
                assert_eq!(p, Some(patch));
                assert_eq!(runs[0].plugin, 1);
                assert_eq!(subscribed.seq, 3);
            }
            other => panic!("wrong message: {other:?}"),
        }
    }

    #[test]
    fn a_borrowed_request_encodes_as_an_owned_one() {
        let request = HostRequest::FinishScan {
            scan_id: 3,
            summary: None,
        };
        assert_eq!(
            encode(&ToHostRef {
                id: 9,
                busy: true,
                request: &request
            })
            .unwrap(),
            encode(&ToHost {
                id: 9,
                busy: true,
                request
            })
            .unwrap()
        );
    }

    /// A buffer kept between frames holds each in turn, larger or smaller.
    #[test]
    fn frames_can_be_read_into_a_kept_buffer() {
        let mut framed = Vec::new();
        write_frame(&mut framed, b"a longer frame").unwrap();
        write_frame(&mut framed, b"short").unwrap();
        write_frame(&mut framed, &vec![1u8; KEPT_BUFFER_BYTES + 1]).unwrap();
        write_frame(&mut framed, b"after").unwrap();
        let mut cursor = std::io::Cursor::new(framed);
        let mut buf = Vec::new();
        assert_eq!(
            read_frame_into(&mut cursor, &mut buf).unwrap(),
            b"a longer frame"
        );
        assert_eq!(read_frame_into(&mut cursor, &mut buf).unwrap(), b"short");
        assert_eq!(
            read_frame_into(&mut cursor, &mut buf).unwrap().len(),
            KEPT_BUFFER_BYTES + 1
        );
        assert_eq!(read_frame_into(&mut cursor, &mut buf).unwrap(), b"after");
        assert!(buf.capacity() <= KEPT_BUFFER_BYTES);
    }

    #[test]
    fn messages_can_be_encoded_into_a_kept_buffer() {
        let mut buf = vec![9; 100];
        encode_into(&HostResponse::Reloaded { loaded: 2 }, &mut buf).unwrap();
        assert_eq!(buf, encode(&HostResponse::Reloaded { loaded: 2 }).unwrap());
    }

    /// `EventPayload::FetchError` carries a `Cow<'static, str>`; it must
    /// come back as owned data on the far side of the wire.
    #[test]
    fn borrowed_event_payload_fields_survive_the_wire() {
        let payload = EventPayload::FetchError {
            feed_id: 1,
            kind: "timeout".into(),
            status: None,
            message: "timed out".to_string(),
            retry_after: Some(30),
        };
        let req = HostRequest::Observe {
            event: Event::FetchError,
            payload,
        };
        let encoded = encode(&req).unwrap();
        let decoded: HostRequest<'static> = decode(&encoded).unwrap();
        match decoded {
            HostRequest::Observe {
                event,
                payload: EventPayload::FetchError { kind, .. },
            } => {
                assert_eq!(event, Event::FetchError);
                assert_eq!(kind.as_ref(), "timeout");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn trailing_bytes_are_refused() {
        let mut encoded = encode(&HostResponse::Ack).unwrap();
        encoded.push(0);
        let err = decode::<HostResponse>(&encoded).unwrap_err();
        assert!(matches!(err, CodecError::TrailingBytes(1)), "got {err:?}");
    }

    #[test]
    fn garbage_is_refused() {
        assert!(decode::<HostResponse>(b"").is_err());
        assert!(decode::<HostResponse>(&[0xff]).is_err());
        assert!(decode::<HostRequest<'static>>(b"{\"Reload\":{\"sources\":[]}}").is_err());
    }

    #[test]
    fn a_prefix_can_be_decoded_on_its_own() {
        #[derive(Deserialize)]
        enum Mirror {
            Reloaded { loaded: usize },
        }
        let encoded = encode(&HostResponse::Reloaded { loaded: 3 }).unwrap();
        let mut padded = encoded.clone();
        padded.extend_from_slice(b"ignored");
        let Mirror::Reloaded { loaded } = decode_prefix(&padded).unwrap();
        assert_eq!(loaded, 3);
    }
}
