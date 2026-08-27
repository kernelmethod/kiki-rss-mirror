//! Length-prefixed framing for inter-process messages.
//!
//! Frames are a little-endian `u32` byte count followed by that many
//! bytes of JSON. Both directions cap the frame size at
//! [`MAX_FRAME_BYTES`]: the parent because a compromised child must not
//! be able to drive it into an allocation it cannot afford, and the child
//! for symmetry.
//!
//! The protocol is strictly request/response — every request written by
//! the server is answered by exactly one response from the script host,
//! including for observe-only events whose result is discarded. Keeping
//! the two sides in lockstep means a dropped or malformed frame shows up
//! immediately as an error rather than as a silently desynchronised
//! stream.

use crate::scripting::{Event, EventPayload, FeedEntry};
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

/// Largest frame either side will write or accept.
///
/// Entries carry a feed's `content` field, which is capped upstream by
/// the `max_feed_bytes` setting; 8 MiB leaves generous room for a single
/// entry plus its JSON encoding without letting one message balloon the
/// peer's memory.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// A message from the server to the script host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HostRequest {
    /// Discard the current VM and rebuild it from `sources`.
    ///
    /// Sent at startup and on every script reload, so the host never
    /// needs to be respawned to pick up new scripts.
    Reload { sources: Vec<String> },

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

/// Write `payload` as a single length-prefixed frame.
///
/// # Errors
///
/// Returns [`io::ErrorKind::InvalidInput`] if the payload exceeds
/// [`MAX_FRAME_BYTES`], and propagates any error from the underlying
/// writer.
pub fn write_frame<W: Write>(w: &mut W, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len())
        .ok()
        .filter(|_| payload.len() <= MAX_FRAME_BYTES)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "frame of {} bytes exceeds the {} byte limit",
                    payload.len(),
                    MAX_FRAME_BYTES
                ),
            )
        })?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()
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
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "peer announced a {} byte frame, over the {} byte limit",
                len, MAX_FRAME_BYTES
            ),
        ));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(payload)
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
    fn requests_and_responses_survive_a_json_round_trip() {
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
        let encoded = serde_json::to_vec(&req).unwrap();
        let decoded: HostRequest = serde_json::from_slice(&encoded).unwrap();
        match decoded {
            HostRequest::TransformEntry { entry: e } => {
                assert_eq!(e.guid, entry.guid);
                assert_eq!(e.tags, entry.tags);
            }
            other => panic!("wrong variant: {other:?}"),
        }

        let resp = HostResponse::Entry { entry: Some(entry) };
        let encoded = serde_json::to_vec(&resp).unwrap();
        let decoded: HostResponse = serde_json::from_slice(&encoded).unwrap();
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
        let encoded = serde_json::to_vec(&req).unwrap();
        let decoded: HostRequest = serde_json::from_slice(&encoded).unwrap();
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
}
