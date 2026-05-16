//! Shared per-turn payload types that cross the gateway ↔ daemon boundary.
//!
//! These are the canonical in-process types used by `gateway::InboundTurn`
//! and `borgd::session::backend::SessionBackend::run_turn`. The corresponding
//! wire types live in `protocols::session` and are converted at the gRPC
//! boundary.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// One inline image attached to a multimodal turn.
#[derive(Debug, Clone)]
pub struct InlineImage {
    /// MIME type of the image (e.g. `image/png`).
    pub mime_type: String,
    /// Raw image bytes (not base64).
    pub data: Vec<u8>,
    /// Optional filename for display.
    pub filename: Option<String>,
}

/// Maximum length of a [`SessionId`], in bytes.
///
/// 128 covers UUIDs (36), gateway-prefixed forms like `slack:U123:thread`,
/// and human-readable test IDs while leaving healthy headroom. The cap
/// matters because session IDs are used as primary keys in SQLite, as path
/// components in the on-disk session store, and as cache keys.
pub const SESSION_ID_MAX_LEN: usize = 128;

/// Validation failures for [`SessionId::new`] / [`SessionId::from_str`].
///
/// Distinguished so callers (gRPC, HTTP) can map to specific error codes
/// instead of a single opaque "bad id" string.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum IdError {
    /// Empty string.
    #[error("session ID must not be empty")]
    Empty,
    /// Exceeds [`SESSION_ID_MAX_LEN`].
    #[error("session ID exceeds {SESSION_ID_MAX_LEN}-byte cap (got {got})")]
    TooLong {
        /// Observed length in bytes.
        got: usize,
    },
    /// Contains a non-ASCII byte.
    #[error("session ID must be ASCII")]
    NonAscii,
    /// Contains a path separator or `..` (would escape the on-disk store).
    #[error("session ID must not contain path separators or '..'")]
    PathTraversal,
    /// Contains an ASCII control character (e.g. NUL, newline).
    #[error("session ID must not contain control characters")]
    ControlChar,
}

/// A validated session identifier.
///
/// Constructed via [`SessionId::new`] / [`FromStr`]; the inner string is
/// guaranteed non-empty, ASCII-only, free of path separators and control
/// characters, and at most [`SESSION_ID_MAX_LEN`] bytes.
///
/// Crosses serde boundaries as a transparent string so existing wire
/// formats (gRPC `string session_id`, JSON event payloads, settings rows)
/// continue to round-trip without a schema change.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    /// Construct a [`SessionId`] from anything string-like, validating
    /// against the same invariants as the legacy `validate_session_id`
    /// plus an ASCII-only / 128-byte cap.
    pub fn new(s: impl Into<String>) -> Result<Self, IdError> {
        let s = s.into();
        if s.is_empty() {
            return Err(IdError::Empty);
        }
        if s.len() > SESSION_ID_MAX_LEN {
            return Err(IdError::TooLong { got: s.len() });
        }
        if !s.is_ascii() {
            return Err(IdError::NonAscii);
        }
        if s.contains("..") || s.contains('/') || s.contains('\\') {
            return Err(IdError::PathTraversal);
        }
        if s.chars().any(|c| c.is_ascii_control()) {
            return Err(IdError::ControlChar);
        }
        Ok(Self(s))
    }

    /// Borrow the underlying string slice. Use when handing the ID to a
    /// `&str`-typed legacy API.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume self and return the owned `String` for APIs that take ownership.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl FromStr for SessionId {
    type Err = IdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s.to_string())
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for SessionId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<SessionId> for String {
    fn from(value: SessionId) -> Self {
        value.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_accepts_uuid() {
        let id = SessionId::new("550e8400-e29b-41d4-a716-446655440000")
            .expect("uuid is valid session id");
        assert_eq!(id.as_str(), "550e8400-e29b-41d4-a716-446655440000");
    }

    #[test]
    fn new_accepts_gateway_composite() {
        let id =
            SessionId::new("slack:U12345:1234567890.123456").expect("gateway composite is valid");
        assert_eq!(id.as_str(), "slack:U12345:1234567890.123456");
    }

    #[test]
    fn new_rejects_empty() {
        assert_eq!(SessionId::new(""), Err(IdError::Empty));
    }

    #[test]
    fn new_rejects_path_traversal() {
        assert_eq!(SessionId::new("../etc/passwd"), Err(IdError::PathTraversal));
        assert_eq!(SessionId::new("foo/bar"), Err(IdError::PathTraversal));
        assert_eq!(SessionId::new("foo\\bar"), Err(IdError::PathTraversal));
        assert_eq!(SessionId::new(".."), Err(IdError::PathTraversal));
    }

    #[test]
    fn new_rejects_control_chars() {
        assert_eq!(SessionId::new("foo\nbar"), Err(IdError::ControlChar));
        assert_eq!(SessionId::new("foo\0bar"), Err(IdError::ControlChar));
    }

    #[test]
    fn new_rejects_non_ascii() {
        assert_eq!(SessionId::new("héllo"), Err(IdError::NonAscii));
        assert_eq!(
            SessionId::new("emoji-rocket"),
            Ok(SessionId("emoji-rocket".into()))
        );
        assert!(matches!(
            SessionId::new("\u{1F680}"),
            Err(IdError::NonAscii)
        ));
    }

    #[test]
    fn new_rejects_oversize() {
        let big = "a".repeat(SESSION_ID_MAX_LEN + 1);
        match SessionId::new(big) {
            Err(IdError::TooLong { got }) => assert_eq!(got, SESSION_ID_MAX_LEN + 1),
            other => panic!("expected TooLong, got {other:?}"),
        }
    }

    #[test]
    fn new_accepts_at_max_len() {
        let edge = "a".repeat(SESSION_ID_MAX_LEN);
        assert!(SessionId::new(edge).is_ok());
    }

    #[test]
    fn from_str_round_trip() {
        let id: SessionId = "abc-123".parse().expect("valid");
        assert_eq!(id.to_string(), "abc-123");
        assert_eq!(format!("{id}"), "abc-123");
    }

    #[test]
    fn serde_is_transparent_to_plain_string() {
        let id = SessionId::new("abc-123").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"abc-123\"");
        let back: SessionId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn serde_deserializes_legacy_string_payload() {
        let raw = "\"550e8400-e29b-41d4-a716-446655440000\"";
        let id: SessionId = serde_json::from_str(raw).unwrap();
        assert_eq!(id.as_str(), "550e8400-e29b-41d4-a716-446655440000");
    }

    #[test]
    fn as_ref_str_works_in_generic_context() {
        fn take<S: AsRef<str>>(s: S) -> usize {
            s.as_ref().len()
        }
        let id = SessionId::new("xyz").unwrap();
        assert_eq!(take(&id), 3);
    }

    #[test]
    fn into_string_yields_owned() {
        let id = SessionId::new("abc").unwrap();
        let s: String = id.into();
        assert_eq!(s, "abc");
    }
}
