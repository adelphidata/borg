//! Framed message protocol between the operator process and the Tier 2
//! elevated runner.
//!
//! Tier 1's anonymous-pipe transport is single-shot: the parent writes
//! one [`SpawnRequest`](crate::windows::ipc::SpawnRequest) and the child
//! reads it. Tier 2 is a *conversation* — the operator launches the
//! runner under a sandbox account, streams the spawn request, then
//! relays stdin/stdout/stderr and terminal-resize events bidirectionally
//! for the life of the child. That needs a multiplexed, length-delimited
//! message protocol rather than a one-shot blob.
//!
//! # Wire format
//!
//! Each message is a length-prefixed JSON [`FramedMessage`]:
//!
//! ```text
//! [u32 little-endian length][JSON-encoded FramedMessage]
//! ```
//!
//! The envelope carries a [`IPC_PROTOCOL_VERSION`] so an operator and a
//! runner built from skewed binaries fail loudly ([`FrameError::Version`])
//! instead of silently misinterpreting each other's bytes — the same
//! "fail closed on version skew" discipline the Tier 1
//! [`ipc`](crate::windows::ipc) module follows.
//!
//! # Why JSON, and why base64 for `Output`/`Stdin`
//!
//! `serde_json` is already a workspace dependency; a second serializer
//! is not worth the binary-size tax (same rationale as the Tier 1
//! transport). The one wrinkle is binary payloads: child stdout is raw
//! bytes, and serde_json's default `Vec<u8>` encoding is a JSON array of
//! integers (`[104,105,...]`) — ~6x bloat and a parse cost per byte.
//! Encoding those fields as base64 strings keeps frames compact while
//! staying valid JSON. The in-memory API stays `Vec<u8>`; the base64 is
//! purely a wire concern (see the [`b64`] serde adapter).
//!
//! # Cross-platform
//!
//! This module is pure data + framing — no Win32 FFI — so it compiles
//! and is unit-tested on every CI runner, not just the Windows lane.
//! The actual pipe plumbing that drives [`read_frame`]/[`write_frame`]
//! lives in the Windows-only `runner_pipe` / `runner_client` modules.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

use crate::windows::ipc::SpawnRequest;

/// Protocol version stamped on every [`FramedMessage`]. Bump when the
/// meaning of an existing message changes; the peer rejects mismatched
/// versions with [`FrameError::Version`] rather than guessing.
pub const IPC_PROTOCOL_VERSION: u8 = 1;

/// Hard upper bound on a single serialized frame. A malformed or
/// hostile length prefix cannot trick the reader into allocating an
/// arbitrary buffer. 8 MiB is generous: the largest legitimate frame is
/// a single output chunk, which the pump bounds well below this.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Which standard stream a chunk of child output came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputStream {
    /// The child's standard output.
    Stdout,
    /// The child's standard error.
    Stderr,
}

/// One message in the operator ⇄ runner conversation. Externally tagged
/// (the JSON object has a single key naming the variant) so adding a
/// variant is backward-compatible: an older peer that doesn't recognize
/// the key rejects the frame as [`FrameError::Decode`] rather than
/// silently misreading it as a known variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    /// operator → runner: launch this command. Boxed because
    /// `SpawnRequest` is the largest variant by far and clippy's
    /// `large_enum_variant` would otherwise fire.
    SpawnRequest(Box<SpawnRequest>),
    /// runner → operator: the child launched; here is its PID (for the
    /// operator's audit log and for `Terminate` correlation).
    SpawnReady {
        /// OS process id of the spawned child.
        pid: u32,
    },
    /// runner → operator: a chunk of child output on `stream`.
    Output {
        /// Which standard stream produced these bytes.
        stream: OutputStream,
        /// Raw output bytes (base64-encoded on the wire).
        #[serde(with = "b64")]
        data: Vec<u8>,
    },
    /// operator → runner: bytes to feed the child's standard input.
    Stdin {
        /// Raw input bytes (base64-encoded on the wire).
        #[serde(with = "b64")]
        data: Vec<u8>,
    },
    /// operator → runner: the child's stdin has reached EOF; close the
    /// write end so a child blocked on `read()` unblocks.
    CloseStdin,
    /// operator → runner: the controlling terminal resized. The runner
    /// relays this to the pseudo-console via `ResizePseudoConsole`.
    Resize {
        /// New column count.
        cols: u16,
        /// New row count.
        rows: u16,
    },
    /// runner → operator: the child exited with this code. Terminal
    /// message — the operator tears the session down after receiving it.
    Exit {
        /// Child process exit code.
        code: u32,
    },
    /// Either direction: a fatal protocol or spawn error. The peer
    /// should log it and tear the session down rather than continue.
    Error {
        /// Human-readable description (never contains secrets — the
        /// runner redacts argv the same way the Tier 1 audit log does).
        message: String,
    },
    /// operator → runner: kill the child now (cancellation / timeout).
    Terminate,
}

/// Versioned envelope wrapping a [`Message`] on the wire (owned form,
/// used when decoding).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FramedMessage {
    /// Sender's [`IPC_PROTOCOL_VERSION`]. The reader rejects any other
    /// value with [`FrameError::Version`].
    pub version: u8,
    /// The payload.
    pub message: Message,
}

/// Borrowing twin of [`FramedMessage`] used on the encode path so
/// [`write_frame`] doesn't clone the (potentially large `Output`)
/// message just to stamp the version onto it.
#[derive(Serialize)]
struct FramedMessageRef<'a> {
    version: u8,
    message: &'a Message,
}

/// Errors reading or writing a frame.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// Underlying transport I/O failed. A truncated frame surfaces here
    /// as `UnexpectedEof` from `read_exact` — distinct from a decode
    /// error, so "peer crashed mid-write" reads differently in logs
    /// than "peer sent garbage".
    #[error("frame I/O error: {0}")]
    Io(#[from] io::Error),
    /// Length prefix exceeded [`MAX_FRAME_BYTES`]. Refused before
    /// allocating the body, so a hostile prefix cannot cause an OOM.
    #[error("frame length {0} exceeds cap of {MAX_FRAME_BYTES} bytes")]
    Oversize(usize),
    /// Frame body did not decode as a [`FramedMessage`]. Most often a
    /// version skew that changed the message shape, or an unknown
    /// variant from a newer peer.
    #[error("frame body failed to decode: {0}")]
    Decode(serde_json::Error),
    /// Peer's protocol version differs from [`IPC_PROTOCOL_VERSION`].
    #[error("unsupported protocol version {got} (this binary speaks {IPC_PROTOCOL_VERSION})")]
    Version {
        /// The version the peer stamped on the frame.
        got: u8,
    },
}

/// Encode `message` as a length-prefixed frame and write it to `w`.
///
/// Flushes before returning so the peer sees the bytes promptly (named
/// pipes buffer otherwise, which would deadlock a request/response
/// handshake). Returns [`FrameError::Oversize`] if the encoded frame
/// exceeds [`MAX_FRAME_BYTES`] — refusing to emit a frame the reader is
/// guaranteed to reject.
pub fn write_frame<W: Write>(w: &mut W, message: &Message) -> Result<(), FrameError> {
    let framed = FramedMessageRef {
        version: IPC_PROTOCOL_VERSION,
        message,
    };
    let body = serde_json::to_vec(&framed).map_err(FrameError::Decode)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(FrameError::Oversize(body.len()));
    }
    let len = body.len() as u32;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()?;
    Ok(())
}

/// Read one length-prefixed frame from `r` and return its [`Message`].
///
/// Validates the length against [`MAX_FRAME_BYTES`] before allocating
/// the body and the envelope version against [`IPC_PROTOCOL_VERSION`]
/// after decoding. A short read (peer closed mid-frame) surfaces as
/// [`FrameError::Io`] with `UnexpectedEof`.
pub fn read_frame<R: Read>(r: &mut R) -> Result<Message, FrameError> {
    let mut len_bytes = [0u8; 4];
    r.read_exact(&mut len_bytes)?;
    let declared = u32::from_le_bytes(len_bytes) as usize;
    if declared > MAX_FRAME_BYTES {
        return Err(FrameError::Oversize(declared));
    }
    let mut body = vec![0u8; declared];
    r.read_exact(&mut body)?;
    let framed: FramedMessage = serde_json::from_slice(&body).map_err(FrameError::Decode)?;
    if framed.version != IPC_PROTOCOL_VERSION {
        return Err(FrameError::Version {
            got: framed.version,
        });
    }
    Ok(framed.message)
}

/// serde adapter: (de)serialize `Vec<u8>` as a base64 string on the
/// wire while keeping the in-memory type raw bytes.
mod b64 {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        STANDARD
            .decode(s.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn round_trip(msg: &Message) -> Message {
        let mut buf = Vec::new();
        write_frame(&mut buf, msg).expect("write");
        let mut cur = Cursor::new(buf);
        read_frame(&mut cur).expect("read")
    }

    #[test]
    fn every_variant_round_trips() {
        // Spec: each control + data message survives encode→decode. A
        // drift between the serializer and a hand-rolled reader would
        // drop, say, the Resize dimensions or the Exit code — silently
        // breaking interactive resize or exit-status propagation.
        let req = SpawnRequest {
            policy: Default::default(),
            tool_dir: std::path::PathBuf::from(r"C:\tool"),
            program: "cmd.exe".into(),
            args: vec!["/c".into(), "echo hi".into()],
        };
        let cases = [
            Message::SpawnRequest(Box::new(req)),
            Message::SpawnReady { pid: 4321 },
            Message::Output {
                stream: OutputStream::Stdout,
                data: b"hello".to_vec(),
            },
            Message::Output {
                stream: OutputStream::Stderr,
                data: b"oops".to_vec(),
            },
            Message::Stdin {
                data: b"input line\n".to_vec(),
            },
            Message::CloseStdin,
            Message::Resize {
                cols: 120,
                rows: 40,
            },
            Message::Exit { code: 137 },
            Message::Error {
                message: "spawn failed".into(),
            },
            Message::Terminate,
        ];
        for msg in cases {
            let got = round_trip(&msg);
            // Compare via JSON so we don't need PartialEq on the whole
            // enum (SpawnRequest has no PartialEq); equal JSON ⇔ equal
            // logical message for these plain-data types.
            let a = serde_json::to_value(&msg).unwrap();
            let b = serde_json::to_value(&got).unwrap();
            assert_eq!(a, b, "variant did not round-trip: {msg:?}");
        }
    }

    #[test]
    fn frame_header_is_little_endian_u32_body_length() {
        // Spec: the documented wire format is a LE u32 length prefix.
        // A future switch to BE/varint would silently desync a runner
        // built from an older binary. Pin the layout.
        let mut buf = Vec::new();
        write_frame(&mut buf, &Message::Exit { code: 0 }).unwrap();
        let declared = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
        assert_eq!(declared, buf.len() - 4);
    }

    #[test]
    fn oversize_length_prefix_is_rejected_before_allocation() {
        // Spec: a length prefix above the cap must error before the
        // body is read, so a hostile peer cannot force a huge alloc.
        // We supply ONLY the 4-byte header — if the reader tried to
        // allocate+read the body it would instead hit EOF; getting
        // Oversize proves the cap check fires first.
        let header = ((MAX_FRAME_BYTES + 1) as u32).to_le_bytes();
        let mut cur = Cursor::new(header.to_vec());
        match read_frame(&mut cur) {
            Err(FrameError::Oversize(n)) => assert_eq!(n, MAX_FRAME_BYTES + 1),
            other => panic!("expected Oversize, got {other:?}"),
        }
    }

    #[test]
    fn truncated_frame_is_io_error_not_decode_error() {
        // Spec: a header promising N bytes followed by fewer than N is
        // a transport failure (peer crashed mid-write), surfaced as Io
        // (UnexpectedEof) — distinct from a decode/version error so the
        // two failure modes are separable in logs.
        let mut buf = Vec::new();
        write_frame(&mut buf, &Message::Terminate).unwrap();
        buf.truncate(buf.len() - 2);
        let mut cur = Cursor::new(buf);
        match read_frame(&mut cur) {
            Err(FrameError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof),
            other => panic!("expected Io(UnexpectedEof), got {other:?}"),
        }
    }

    #[test]
    fn version_mismatch_is_rejected() {
        // Spec: a frame stamped with a different protocol version must
        // be refused, not interpreted. This is the cross-binary skew
        // guard — a runner one release ahead of the operator must not
        // silently misread its messages.
        let framed = FramedMessage {
            version: IPC_PROTOCOL_VERSION.wrapping_add(7),
            message: Message::Terminate,
        };
        let body = serde_json::to_vec(&framed).unwrap();
        let mut buf = (body.len() as u32).to_le_bytes().to_vec();
        buf.extend_from_slice(&body);
        let mut cur = Cursor::new(buf);
        match read_frame(&mut cur) {
            Err(FrameError::Version { got }) => {
                assert_eq!(got, IPC_PROTOCOL_VERSION.wrapping_add(7));
            }
            other => panic!("expected Version, got {other:?}"),
        }
    }

    #[test]
    fn output_payload_survives_all_byte_values_via_base64() {
        // Spec: child output is arbitrary binary (a tool emitting a PNG
        // to stdout, or raw VT escapes). The base64 wire encoding must
        // round-trip every byte 0x00..=0xFF — a naive UTF-8 String field
        // would corrupt non-text output.
        let data: Vec<u8> = (0..=255u8).collect();
        let got = round_trip(&Message::Output {
            stream: OutputStream::Stdout,
            data: data.clone(),
        });
        match got {
            Message::Output { data: out, stream } => {
                assert_eq!(stream, OutputStream::Stdout);
                assert_eq!(out, data, "binary payload corrupted through base64");
            }
            other => panic!("expected Output, got {other:?}"),
        }
    }

    #[test]
    fn output_is_base64_string_on_the_wire_not_a_json_array() {
        // Spec: the whole point of the base64 adapter is compactness —
        // the data field must serialize as a JSON string, not the
        // default integer array. Regressing the #[serde(with)] adapter
        // would 6x frame sizes and break a hand-written peer expecting
        // a string.
        let v = serde_json::to_value(Message::Output {
            stream: OutputStream::Stdout,
            data: b"hi".to_vec(),
        })
        .unwrap();
        let data_field = &v["Output"]["data"];
        assert!(data_field.is_string(), "data must be a base64 string: {v}");
        assert_eq!(data_field.as_str().unwrap(), "aGk=");
    }
}
