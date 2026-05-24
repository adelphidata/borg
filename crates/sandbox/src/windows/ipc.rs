//! Trampoline IPC: `SpawnRequest` + length-prefixed framing.
//!
//! Moves the sandbox policy off the trampoline's argv. The wire format is:
//!
//! ```text
//! [u32 little-endian length][JSON-encoded SpawnRequest]
//! ```
//!
//! On Windows the bytes travel through an anonymous pipe whose read-end
//! handle is the only argv reference. On non-Windows hosts the same
//! framing is used for tests so the parsing logic is exercised on every
//! CI runner, not just the (currently absent) Windows lane.
//!
//! # Why JSON, not bincode/postcard
//!
//! `serde_json` is already a workspace dependency; adding a second
//! serialization crate for a 4 KB payload is not worth the binary-size
//! tax. The framing is binary (length-prefix is u32 LE) so the JSON
//! payload's text-vs-binary nature is irrelevant to the transport.
//!
//! # Why a length prefix
//!
//! Anonymous pipes are byte streams with no record boundary. Without a
//! length prefix the reader has to read-until-EOF, which is fine for a
//! single-shot request but leaves no room to multiplex or to detect
//! truncation. The prefix lets the reader allocate exactly once and
//! reject partial payloads as `Truncated` rather than as a JSON parse
//! error (the latter is a confusing failure mode for "the parent crashed
//! mid-write").

use serde::{Deserialize, Serialize};

use crate::policy::SandboxPolicy;

/// Hard upper bound on a serialized `SpawnRequest`. Refusing to
/// allocate beyond this is a defense against a malformed length prefix
/// (a same-session attacker who can write to the pipe could otherwise
/// trick the reader into allocating `u32::MAX` bytes). The chosen value
/// is generous: a policy with hundreds of paths fits in a few KB; 1 MiB
/// is ~250x headroom.
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;

/// Everything the trampoline needs to launch the wrapped command,
/// transferred over the IPC pipe instead of via argv.
///
/// Moving these fields off argv resolves the WMI/`tasklist`-readability
/// concern noted in `windows/mod.rs` — a co-tenant scraping process
/// command lines now sees only `borg.exe __windows-sandbox-spawn
/// --request-pipe <decimal>`. The handle value is meaningless without
/// `PROCESS_DUP_HANDLE` rights on the parent (closed by the
/// elevated-runner named-pipe transport with explicit SDDL).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnRequest {
    /// Policy whose ACEs the trampoline will apply before spawning.
    pub policy: SandboxPolicy,
    /// Tool-directory scope (same meaning as on the Unix backends).
    pub tool_dir: std::path::PathBuf,
    /// Wrapped program path.
    pub program: String,
    /// Wrapped program arguments, in declaration order.
    pub args: Vec<String>,
}

/// Errors deserializing a framed `SpawnRequest`.
#[derive(Debug, thiserror::Error)]
pub enum RequestParseError {
    /// Header was shorter than the 4-byte length prefix.
    #[error("framed request shorter than 4-byte length prefix (got {0} bytes)")]
    ShortHeader(usize),
    /// Length prefix exceeded [`MAX_REQUEST_BYTES`]. Refused without
    /// allocating to avoid an OOM amplification.
    #[error("framed request length {0} exceeds cap of {MAX_REQUEST_BYTES} bytes")]
    Oversize(usize),
    /// Length prefix promised more bytes than were actually delivered.
    /// Distinct from `PolicyDecode` so a parent that crashed mid-write
    /// surfaces a clean error instead of a confusing JSON syntax message.
    #[error("framed request truncated: header promised {expected} bytes, got {got}")]
    Truncated {
        /// Bytes the header said to expect.
        expected: usize,
        /// Bytes actually present after the header.
        got: usize,
    },
    /// Body bytes did not parse as `SpawnRequest` JSON. Most often a
    /// version mismatch between parent and trampoline binaries.
    #[error("failed to decode SpawnRequest body: {0}")]
    BodyDecode(serde_json::Error),
}

/// Serialize a `SpawnRequest` into framed bytes ready for the pipe or
/// for an in-memory test transport.
///
/// `SpawnRequest` is plain data (strings + bools + serde-default vecs)
/// that `serde_json::to_vec` cannot fail to serialize. A non-serializable
/// field would be a compile-time-detectable bug, so we surface it as a
/// panic rather than an error variant the type system would force every
/// caller to handle.
#[allow(clippy::expect_used)]
pub fn serialize_request(req: &SpawnRequest) -> Vec<u8> {
    let body = serde_json::to_vec(req)
        .expect("SpawnRequest must always serialize — non-serializable field added?");
    assert!(
        body.len() <= MAX_REQUEST_BYTES,
        "serialized SpawnRequest ({} bytes) exceeds cap {}; refusing to encode \
         a request the reader will refuse to allocate",
        body.len(),
        MAX_REQUEST_BYTES,
    );
    let len = body.len() as u32;
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// Parse framed bytes into a [`SpawnRequest`]. Strict: the header
/// length must exactly match the body length. Trailing bytes beyond
/// the declared body are an error (they would indicate the transport
/// concatenated two requests, which the trampoline must refuse rather
/// than silently process the first one).
pub fn deserialize_request(framed: &[u8]) -> Result<SpawnRequest, RequestParseError> {
    if framed.len() < 4 {
        return Err(RequestParseError::ShortHeader(framed.len()));
    }
    let mut len_bytes = [0u8; 4];
    len_bytes.copy_from_slice(&framed[..4]);
    let declared = u32::from_le_bytes(len_bytes) as usize;
    if declared > MAX_REQUEST_BYTES {
        return Err(RequestParseError::Oversize(declared));
    }
    let body = &framed[4..];
    if body.len() != declared {
        return Err(RequestParseError::Truncated {
            expected: declared,
            got: body.len(),
        });
    }
    serde_json::from_slice::<SpawnRequest>(body).map_err(RequestParseError::BodyDecode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample() -> SpawnRequest {
        SpawnRequest {
            policy: SandboxPolicy {
                network: true,
                fs_read: vec!["/etc/ssl".into()],
                fs_write: vec!["/tmp/work".into()],
                deny_read: vec!["/home/u/.ssh".into()],
                deny_write: vec!["/home/u/.borg".into()],
                allowed_domains: vec!["api.openai.com".into(), ".github.com".into()],
            },
            tool_dir: PathBuf::from(r"C:\tools\echo"),
            program: "cmd.exe".into(),
            args: vec!["/c".into(), "echo hi".into()],
        }
    }

    #[test]
    fn round_trip_preserves_every_field() {
        // Spec: encode → decode is the identity for every SpawnRequest
        // field. A drift between encoder and decoder would silently drop
        // policy fields (the same sandbox-bypass failure mode the argv
        // round-trip test guards against, now restated for the pipe path).
        let req = sample();
        let bytes = serialize_request(&req);
        let parsed = deserialize_request(&bytes).expect("must decode");
        assert_eq!(parsed.policy.fs_read, req.policy.fs_read);
        assert_eq!(parsed.policy.fs_write, req.policy.fs_write);
        assert_eq!(parsed.policy.deny_read, req.policy.deny_read);
        assert_eq!(parsed.policy.deny_write, req.policy.deny_write);
        assert_eq!(parsed.policy.allowed_domains, req.policy.allowed_domains);
        assert_eq!(parsed.policy.network, req.policy.network);
        assert_eq!(parsed.tool_dir, req.tool_dir);
        assert_eq!(parsed.program, req.program);
        assert_eq!(parsed.args, req.args);
    }

    #[test]
    fn header_length_is_little_endian_u32() {
        // Spec: the wire format is documented as LE u32; a future
        // refactor that switches to BE or to varint would break
        // cross-binary compatibility silently. Pin the byte layout.
        let req = SpawnRequest {
            policy: SandboxPolicy::default(),
            tool_dir: PathBuf::from("/t"),
            program: "x".into(),
            args: vec![],
        };
        let bytes = serialize_request(&req);
        let declared = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert_eq!(declared, bytes.len() - 4);
    }

    #[test]
    fn short_header_is_rejected() {
        // Spec: a header with fewer than 4 bytes is a transport-level
        // failure (parent crashed before writing length). Must not
        // panic on slice access and must not silently treat as length 0.
        for n in 0..4 {
            let bytes = vec![0u8; n];
            match deserialize_request(&bytes) {
                Err(RequestParseError::ShortHeader(got)) => assert_eq!(got, n),
                other => panic!("expected ShortHeader, got {other:?}"),
            }
        }
    }

    #[test]
    fn oversize_header_is_rejected_without_allocating_body() {
        // Spec: a length prefix exceeding the cap must error before any
        // body bytes are read, otherwise an attacker controlling the
        // pipe could force u32::MAX byte allocation. The bytes after the
        // header are intentionally absent — the test verifies the cap
        // check fires before the truncation check.
        let mut bytes = Vec::with_capacity(4);
        bytes.extend_from_slice(&((MAX_REQUEST_BYTES + 1) as u32).to_le_bytes());
        match deserialize_request(&bytes) {
            Err(RequestParseError::Oversize(n)) => assert_eq!(n, MAX_REQUEST_BYTES + 1),
            other => panic!("expected Oversize, got {other:?}"),
        }
    }

    #[test]
    fn truncated_body_is_rejected_distinct_from_decode_error() {
        // Spec: a header promising N bytes followed by fewer than N
        // bytes is a Truncated error, NOT a JSON parse error. This
        // matters because operators reading logs need to distinguish
        // "parent crashed mid-write" from "binaries are version-skewed".
        let req = sample();
        let mut bytes = serialize_request(&req);
        bytes.truncate(bytes.len() - 5);
        match deserialize_request(&bytes) {
            Err(RequestParseError::Truncated { expected, got }) => {
                assert!(expected > got, "expected > got, got {expected} vs {got}");
            }
            other => panic!("expected Truncated, got {other:?}"),
        }
    }

    #[test]
    fn trailing_bytes_after_declared_body_are_rejected() {
        // Spec: extra bytes after the declared body are a framing
        // violation. Silently ignoring them would let a same-session
        // attacker who can write to the pipe concatenate a second
        // request whose policy fields would never be enforced —
        // surprising and exploitable.
        let req = sample();
        let mut bytes = serialize_request(&req);
        bytes.extend_from_slice(b"trailing-garbage");
        match deserialize_request(&bytes) {
            Err(RequestParseError::Truncated { expected, got }) => {
                assert!(got > expected, "got {got} > expected {expected}");
            }
            other => panic!("expected Truncated for trailing-bytes case, got {other:?}"),
        }
    }

    #[test]
    fn malformed_json_body_surfaces_as_body_decode_error() {
        // Spec: a header that matches its body length but whose body is
        // not valid SpawnRequest JSON must error as BodyDecode (not
        // Truncated). This is the version-skew failure mode.
        let body = b"{not valid";
        let len = (body.len() as u32).to_le_bytes();
        let mut framed = Vec::new();
        framed.extend_from_slice(&len);
        framed.extend_from_slice(body);
        match deserialize_request(&framed) {
            Err(RequestParseError::BodyDecode(_)) => {}
            other => panic!("expected BodyDecode, got {other:?}"),
        }
    }

    #[test]
    fn special_chars_in_paths_round_trip() {
        // Spec: Windows filenames are user-controlled and routinely
        // contain spaces, quotes, and non-ASCII. The framed transport
        // must not depend on any quoting beyond what JSON already
        // guarantees.
        let req = SpawnRequest {
            policy: SandboxPolicy {
                fs_read: vec![
                    r"C:\Users\Jürgen O'Brien\Docs".into(),
                    r#"C:\path with "quote" inside"#.into(),
                ],
                ..Default::default()
            },
            tool_dir: PathBuf::from(r"C:\tool dir\with space"),
            program: r"C:\Program Files\node.exe".into(),
            args: vec![r"--script=C:\a b\c.js".into()],
        };
        let bytes = serialize_request(&req);
        let parsed = deserialize_request(&bytes).expect("round-trip");
        assert_eq!(parsed.policy.fs_read, req.policy.fs_read);
        assert_eq!(parsed.program, req.program);
        assert_eq!(parsed.args, req.args);
        assert_eq!(parsed.tool_dir, req.tool_dir);
    }
}
