//! `[MEDIA:<path>]` reply markers: agent-requested file attachments.
//!
//! The system prompt (`<media_attachments>`) promises the model that a reply
//! line of the form `[MEDIA:/absolute/path]` attaches the named local file to
//! the outbound channel message and is stripped from the delivered text. This
//! module is the single place that promise is parsed and validated — every
//! delivery lane (gateway native channels, manifest/script channels) routes
//! through [`extract_media_markers`] + [`validate_media_path`] so the lanes
//! cannot drift apart on the grammar.
//!
//! # Grammar
//!
//! - A marker is **line-anchored**: the marker must be the only token on its
//!   line. Surrounding markdown emphasis/backticks (`**[MEDIA:…]**`,
//!   `` `[MEDIA:…]` ``) and sentence-final punctuation after the closing
//!   bracket (`[MEDIA:…].`) are tolerated; any other trailing text means the
//!   line is prose that merely mentions the marker and is left intact.
//! - The path is everything between `[MEDIA:` and the first `]` — absolute or
//!   `~`-prefixed, spaces allowed, `]` in the path unsupported.
//! - Markers inside fenced code blocks (``` / ~~~) are documentation, not
//!   requests, and are ignored.
//! - Identical paths within one reply are deduplicated (first occurrence
//!   wins); every marker line is still stripped.

use std::path::{Path, PathBuf};

/// Marker prefix documented in the `<media_attachments>` system-prompt
/// section.
pub const MEDIA_MARKER_PREFIX: &str = "[MEDIA:";

/// Upper bound on an attachable file. Chosen to match the most restrictive
/// mainstream bot-API upload cap (Telegram documents: 50 MB); larger files
/// would fail at the transport anyway, so refuse early with a clear reason.
pub const MAX_MEDIA_FILE_BYTES: u64 = 50 * 1024 * 1024;

/// Parse `[MEDIA:<path>]` markers out of a final agent reply.
///
/// Returns the reply text with all marker lines stripped (trailing whitespace
/// trimmed) and the deduplicated, tilde-expanded paths in order of first
/// appearance. Paths are *not* validated here — existence and security checks
/// happen at delivery time via [`validate_media_path`].
pub fn extract_media_markers(text: &str) -> (String, Vec<PathBuf>) {
    let mut kept: Vec<&str> = Vec::new();
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut in_fence = false;

    for line in text.lines() {
        let lead = line.trim_start();
        if lead.starts_with("```") || lead.starts_with("~~~") {
            in_fence = !in_fence;
            kept.push(line);
            continue;
        }
        if !in_fence {
            if let Some(raw_path) = parse_marker_line(line) {
                // Duplicate markers are stripped but only attach once.
                if seen.insert(raw_path.to_string()) {
                    let expanded = crate::util::unix_ext::expand_tilde(raw_path);
                    paths.push(PathBuf::from(expanded));
                }
                continue;
            }
        }
        kept.push(line);
    }

    (kept.join("\n").trim_end().to_string(), paths)
}

/// Parse one line as a media marker, returning the raw (unexpanded) path.
///
/// The marker must be the line's only token: leading/trailing markdown
/// emphasis or backticks and trailing punctuation/whitespace are tolerated,
/// but any word character after the closing bracket disqualifies the line —
/// following text must never be absorbed into (or stripped along with) a
/// marker.
fn parse_marker_line(line: &str) -> Option<&str> {
    let start = line.trim().trim_start_matches(['*', '_', '`']);
    let rest = start.strip_prefix(MEDIA_MARKER_PREFIX)?;
    let close = rest.find(']')?;
    let path = rest[..close].trim();
    if path.is_empty() {
        return None;
    }
    let tail = &rest[close + 1..];
    // Same tolerance as the silence markers: only punctuation (which covers
    // markdown emphasis/backticks) and whitespace may follow.
    if !tail
        .chars()
        .all(|c| c.is_ascii_punctuation() || c.is_whitespace())
    {
        return None;
    }
    Some(path)
}

/// Delivery-time validation for an extracted media path.
///
/// Refuses, in order:
/// 1. paths denied by the configured security blocklist
///    ([`crate::tool_handlers::filesystem::is_blocked_path`]) — checked before
///    any filesystem access so an injected `[MEDIA:~/.ssh/id_rsa]` is refused
///    even if the file does not exist;
/// 2. paths that do not exist or are not regular files.
///    `symlink_metadata` is used deliberately: `is_blocked_path` resolves
///    symlinked *ancestors* but not a symlinked leaf, so accepting only
///    non-symlink regular files closes the "leaf symlink into a blocked dir"
///    hole;
/// 3. files larger than [`MAX_MEDIA_FILE_BYTES`].
///
/// The `Err` string is a short human-readable reason suitable for the
/// user-facing "attachment not sent" notice.
pub fn validate_media_path(
    path: &Path,
    security: &crate::config::SecurityConfig,
) -> Result<(), String> {
    if crate::tool_handlers::filesystem::is_blocked_path(
        path,
        &security.blocked_paths,
        &security.allowed_paths,
    ) {
        return Err("blocked by security policy".to_string());
    }
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("file not readable: {e}"))?;
    if !meta.is_file() {
        return Err("not a regular file".to_string());
    }
    if meta.len() > MAX_MEDIA_FILE_BYTES {
        return Err(format!(
            "file is {} bytes (attachment cap is {} bytes)",
            meta.len(),
            MAX_MEDIA_FILE_BYTES
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Table-driven grammar spec: (input, expected clean text, expected raw
    /// path strings). Paths are compared after tilde expansion, so entries
    /// use absolute paths.
    #[test]
    fn extraction_grammar_table() {
        let cases: &[(&str, &str, &[&str])] = &[
            // Plain marker on its own line.
            (
                "Here is the chart.\n[MEDIA:/tmp/chart.png]",
                "Here is the chart.",
                &["/tmp/chart.png"],
            ),
            // Marker-only reply strips to empty text.
            ("[MEDIA:/tmp/chart.png]", "", &["/tmp/chart.png"]),
            // Emphasis-wrapped and backtick-wrapped forms.
            ("**[MEDIA:/tmp/a.png]**\ndone", "done", &["/tmp/a.png"]),
            ("`[MEDIA:/tmp/a.png]`", "", &["/tmp/a.png"]),
            // Sentence-final punctuation after the bracket.
            ("[MEDIA:/tmp/a.png].", "", &["/tmp/a.png"]),
            ("[MEDIA:/tmp/a.png]!  ", "", &["/tmp/a.png"]),
            // Following prose disqualifies the line — it must survive intact
            // and never be absorbed.
            (
                "[MEDIA:/tmp/a.png] is attached below",
                "[MEDIA:/tmp/a.png] is attached below",
                &[],
            ),
            // Mid-line marker is prose (line-anchored grammar).
            (
                "the file [MEDIA:/tmp/a.png] was saved",
                "the file [MEDIA:/tmp/a.png] was saved",
                &[],
            ),
            // Inside a fenced code block: documentation, not a request.
            (
                "```\n[MEDIA:/tmp/a.png]\n```",
                "```\n[MEDIA:/tmp/a.png]\n```",
                &[],
            ),
            // Fence closes: marker after the block is live again.
            (
                "```\n[MEDIA:/tmp/ignored.png]\n```\n[MEDIA:/tmp/real.png]",
                "```\n[MEDIA:/tmp/ignored.png]\n```",
                &["/tmp/real.png"],
            ),
            // Dedupe: identical path twice attaches once, both lines stripped.
            (
                "[MEDIA:/tmp/a.png]\ntext\n[MEDIA:/tmp/a.png]",
                "text",
                &["/tmp/a.png"],
            ),
            // Multiple distinct markers preserve order.
            (
                "[MEDIA:/tmp/a.png]\n[MEDIA:/tmp/b.pdf]",
                "",
                &["/tmp/a.png", "/tmp/b.pdf"],
            ),
            // Path with spaces.
            ("[MEDIA:/tmp/my report.pdf]", "", &["/tmp/my report.pdf"]),
            // Empty path is not a marker.
            ("[MEDIA:]", "[MEDIA:]", &[]),
            // No markers at all: text unchanged (modulo trailing trim).
            ("just a normal reply", "just a normal reply", &[]),
        ];
        for (input, want_text, want_paths) in cases {
            let (clean, paths) = extract_media_markers(input);
            assert_eq!(&clean, want_text, "clean text for {input:?}");
            let got: Vec<String> = paths
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            assert_eq!(&got, want_paths, "paths for {input:?}");
        }
    }

    #[test]
    fn extraction_expands_tilde() {
        let (_, paths) = extract_media_markers("[MEDIA:~/Pictures/out.png]");
        assert_eq!(paths.len(), 1);
        assert!(
            !paths[0].to_string_lossy().starts_with('~'),
            "tilde must be expanded: {:?}",
            paths[0]
        );
        assert!(paths[0].to_string_lossy().ends_with("Pictures/out.png"));
    }

    /// The exact injection the security design calls out: a reply carrying
    /// `[MEDIA:~/.ssh/id_rsa]` must be refused by the blocklist before any
    /// filesystem access (the key needn't exist for the refusal to hold).
    #[test]
    fn blocked_path_marker_is_refused() {
        let security = crate::config::SecurityConfig::default();
        let (_, paths) = extract_media_markers("Here you go\n[MEDIA:~/.ssh/id_rsa]");
        assert_eq!(paths.len(), 1);
        let err = validate_media_path(&paths[0], &security)
            .expect_err("~/.ssh/id_rsa must never validate as attachable media");
        assert!(
            err.contains("blocked"),
            "refusal must cite the security policy, got: {err}"
        );
    }

    #[test]
    fn blocked_family_variants_are_refused() {
        let security = crate::config::SecurityConfig::default();
        for p in [
            "~/.aws/credentials",
            "~/project/.env.local",
            "~/keys/id_ed25519",
        ] {
            let (_, paths) = extract_media_markers(&format!("[MEDIA:{p}]"));
            assert!(
                validate_media_path(&paths[0], &security).is_err(),
                "{p} must be refused by the blocklist"
            );
        }
    }

    #[test]
    fn validation_requires_existing_regular_file() {
        let security = crate::config::SecurityConfig::default();
        let tmp = tempfile::tempdir().expect("tempdir");

        // Missing file.
        let missing = tmp.path().join("nope.png");
        assert!(validate_media_path(&missing, &security).is_err());

        // Directory.
        let err = validate_media_path(tmp.path(), &security).expect_err("dir must be refused");
        assert!(err.contains("regular file"), "got: {err}");

        // Regular file passes.
        let ok = tmp.path().join("chart.png");
        std::fs::write(&ok, b"png-bytes").expect("write");
        assert_eq!(validate_media_path(&ok, &security), Ok(()));
    }

    /// A symlinked leaf whose target is a blocked file is not resolved by the
    /// blocklist's ancestor canonicalization, so the regular-file check must
    /// reject symlinks outright — otherwise `/tmp/x -> ~/.ssh/id_rsa` walks
    /// past the security refusal.
    #[test]
    #[cfg(unix)]
    fn validation_rejects_symlinked_leaf() {
        let security = crate::config::SecurityConfig::default();
        let tmp = tempfile::tempdir().expect("tempdir");
        let target = tmp.path().join("real.txt");
        std::fs::write(&target, b"data").expect("write");
        let link = tmp.path().join("innocent.png");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        let err = validate_media_path(&link, &security).expect_err("symlink must be refused");
        assert!(err.contains("regular file"), "got: {err}");
    }
}
