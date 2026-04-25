//! Shared input validators and sanitizers.
//!
//! Pure string-validation utilities used at trust boundaries (session
//! identifiers, inbound webhook fields). Logic-free helpers belong here so
//! they aren't duplicated across crates.

use anyhow::{bail, Result};

/// Reject session IDs that could traverse the filesystem when used as
/// path components. The on-disk session store keys files by ID directly.
pub fn validate_session_id(id: &str) -> Result<()> {
    if id.is_empty() {
        bail!("Session ID must not be empty");
    }
    if id.contains("..") || id.contains('/') || id.contains('\\') {
        bail!("Invalid session ID: must not contain path separators or '..'");
    }
    Ok(())
}

/// Sanitize a thread_id to prevent session-key confusion via delimiter
/// injection.
///
/// Allows alphanumeric characters, dots, hyphens, and underscores — enough
/// to cover all real platform formats (Slack `thread_ts` like
/// `1234567890.123456`, Discord numeric IDs, Google Chat
/// `spaces/*/threads/*` stripped to the leaf, Telegram integer IDs).
/// Colons are intentionally excluded because the session key is composed as
/// `{sender_id}:{thread_id}`, so an injected colon would corrupt the key
/// structure. Length is capped to keep keys bounded.
pub fn sanitize_thread_id(thread_id: &str, max_len: usize) -> String {
    thread_id
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '.' || *c == '-' || *c == '_')
        .take(max_len)
        .collect()
}

/// Sanitize an attachment filename from an external webhook.
/// Extracts the basename and rejects path traversal, hidden files, null
/// bytes, and empty strings; returns `"attachment"` as a safe fallback.
pub fn sanitize_filename(name: &Option<String>) -> Option<String> {
    name.as_ref().map(|n| {
        let basename = std::path::Path::new(n)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("attachment");
        if basename.contains("..")
            || basename.starts_with('.')
            || basename.contains('\0')
            || basename.is_empty()
        {
            "attachment".to_string()
        } else {
            basename.to_string()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_session_id_empty() {
        assert!(validate_session_id("").is_err());
    }

    #[test]
    fn validate_session_id_path_traversal() {
        assert!(validate_session_id("../etc/passwd").is_err());
        assert!(validate_session_id("foo/bar").is_err());
        assert!(validate_session_id("foo\\bar").is_err());
        assert!(validate_session_id("..").is_err());
    }

    #[test]
    fn validate_session_id_valid() {
        assert!(validate_session_id("abc-123").is_ok());
        assert!(validate_session_id("550e8400-e29b-41d4-a716-446655440000").is_ok());
    }

    #[test]
    fn sanitize_filename_strips_path_traversal() {
        assert_eq!(
            sanitize_filename(&Some("../../etc/passwd".to_string())),
            Some("passwd".to_string())
        );
        assert_eq!(
            sanitize_filename(&Some("/var/log/../secret".to_string())),
            Some("secret".to_string())
        );
    }

    #[test]
    fn sanitize_filename_blocks_hidden_files() {
        assert_eq!(
            sanitize_filename(&Some(".hidden".to_string())),
            Some("attachment".to_string())
        );
        assert_eq!(
            sanitize_filename(&Some(".env".to_string())),
            Some("attachment".to_string())
        );
    }

    #[test]
    fn sanitize_filename_passes_normal_names() {
        assert_eq!(
            sanitize_filename(&Some("photo.jpg".to_string())),
            Some("photo.jpg".to_string())
        );
        assert_eq!(
            sanitize_filename(&Some("document.pdf".to_string())),
            Some("document.pdf".to_string())
        );
    }

    #[test]
    fn sanitize_filename_handles_none() {
        assert_eq!(sanitize_filename(&None), None);
    }

    #[test]
    fn sanitize_filename_blocks_null_bytes() {
        assert_eq!(
            sanitize_filename(&Some("file\0name.txt".to_string())),
            Some("attachment".to_string())
        );
    }

    #[test]
    fn sanitize_filename_empty_string() {
        assert_eq!(
            sanitize_filename(&Some(String::new())),
            Some("attachment".to_string())
        );
    }

    #[test]
    fn sanitize_filename_double_dots() {
        assert_eq!(
            sanitize_filename(&Some("file..name.txt".to_string())),
            Some("attachment".to_string())
        );
    }

    #[test]
    fn sanitize_thread_id_strips_delimiters() {
        assert_eq!(sanitize_thread_id("foo:bar", 128), "foobar");
        assert_eq!(sanitize_thread_id("a/b\\c", 128), "abc");
    }

    #[test]
    fn sanitize_thread_id_keeps_safe_chars() {
        assert_eq!(
            sanitize_thread_id("1234567890.123456", 128),
            "1234567890.123456"
        );
        assert_eq!(sanitize_thread_id("abc-123_xyz", 128), "abc-123_xyz");
    }

    #[test]
    fn sanitize_thread_id_truncates() {
        let long = "a".repeat(200);
        assert_eq!(sanitize_thread_id(&long, 128).len(), 128);
    }
}
