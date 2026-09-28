//! Length-prefixed framing for inter-process messages, the codec for
//! their bodies, and the script host's message types.
//!
//! Frames are a little-endian `u32` byte count followed by that many
//! bytes of [postcard]-encoded message, written with [`encode`] and read
//! with [`decode`]. Postcard is compact and not self-describing: fields
//! are written in declaration order with no names, and enum variants by
//! index. That is safe here because both ends of every channel are the
//! same binary, so the message types can never disagree.
//!
//! Both directions cap the frame size: the parent because a
//! compromised child must not be able to drive it into an allocation it
//! cannot afford, and the child for symmetry. The script host channel uses
//! [`MAX_FRAME_BYTES`]; the feed fetcher channel, which carries whole
//! parsed feeds, passes its own larger limit to the `_limited` variants.
//!
//! The script host protocol is strictly request/response — every request
//! written by the server is answered by exactly one response from the
//! script host, including for observe-only events whose result is
//! discarded. Keeping the two sides in lockstep means a dropped or
//! malformed frame shows up immediately as an error rather than as a
//! silently desynchronised stream. The feed fetcher's protocol is
//! multiplexed instead; see [`crate::process::feed_fetcher`].

use crate::scripting::{Event, EventPayload, FeedEntry, ScriptSource};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::io::{self, Read, Write};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest frame either side will write or accept.
///
/// Entries carry a feed's `content` field, which is capped upstream by
/// the `max_feed_bytes` setting; 8 MiB leaves generous room for a single
/// entry plus its encoding without letting one message balloon the
/// peer's memory.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// A message from the server to the script host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HostRequest {
    /// Discard the current VM, if any, and build a new one from
    /// `sources`, each script with its config.
    ///
    /// Sent once, when the server starts and has discovered its plugins.
    Reload { sources: Vec<ScriptSource> },

    /// Run `entry` through the `entry.ingest` handler chain.
    TransformEntry { entry: FeedEntry },

    /// Fire an observe-only event. The response carries no data, but is
    /// still awaited to keep the stream in lockstep.
    Observe { event: Event, payload: EventPayload },
}

/// A message from the script host back to the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HostResponse {
    /// The VM was rebuilt; `loaded` is the number of scripts compiled.
    Reloaded { loaded: usize },

    /// The handler chain ran. `None` means a handler filtered the entry
    /// out and it should not be inserted.
    Entry { entry: Option<FeedEntry> },

    /// An observe event was dispatched.
    Ack,

    /// The request could not be served. The server treats this as a
    /// script-level failure, not a dead host: for `entry.ingest` the
    /// entry passes through unmodified, matching the in-process
    /// runner's contract.
    Failed { message: String },
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
    let len = frame_len(payload, max)?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(payload)?;
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
    let len = frame_len(payload, max)?;
    w.write_all(&len.to_le_bytes()).await?;
    w.write_all(payload).await?;
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
            feed_id: 7,
            syndication_format: "rss".to_string(),
            guid: "urn:kiki:1".to_string(),
            published_at: Some(1_700_000_000),
            title: "title".to_string(),
            url: Some("https://example.com/1".to_string()),
            content: Some("<p>body</p>".to_string()),
            tags: vec!["a".to_string(), "b".to_string()],
        };

        let req = HostRequest::TransformEntry {
            entry: entry.clone(),
        };
        let encoded = encode(&req).unwrap();
        let decoded: HostRequest = decode(&encoded).unwrap();
        match decoded {
            HostRequest::TransformEntry { entry: e } => {
                assert_eq!(e.guid, entry.guid);
                assert_eq!(e.tags, entry.tags);
            }
            other => panic!("wrong variant: {other:?}"),
        }

        let resp = HostResponse::Entry { entry: Some(entry) };
        let encoded = encode(&resp).unwrap();
        let decoded: HostResponse = decode(&encoded).unwrap();
        assert!(matches!(decoded, HostResponse::Entry { entry: Some(_) }));
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
        let decoded: HostRequest = decode(&encoded).unwrap();
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
        assert!(decode::<HostRequest>(b"{\"Reload\":{\"sources\":[]}}").is_err());
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
