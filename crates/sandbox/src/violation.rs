//! Normalized sandbox denial events.
//!
//! A denial reaches the agent today as an exit code plus whatever the backend
//! chose to print, and the two backends print nothing alike: Seatbelt emits
//! `deny(1) file-read-data /path`, bwrap emits English prose, and a seccomp
//! kill emits nothing at all — the process just dies on `SIGSYS`. Every
//! consumer that wants to answer "what did the sandbox block" therefore has to
//! re-parse all three shapes.
//!
//! This module is that parse, done once. It is **classification only**: it
//! never changes whether a command is denied, and a denial that fails to
//! classify is simply not reported.

/// Ceiling on the raw evidence carried with a violation.
///
/// The snippet exists so a human can recognise the denial, not so the whole
/// stderr stream can be replayed — and it lands in a persisted, HMAC-chained
/// event, so it must not be able to grow without bound.
pub const MAX_SNIPPET_BYTES: usize = 512;

/// Which sandbox implementation reported the denial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxBackend {
    /// macOS `sandbox-exec` (Seatbelt).
    Seatbelt,
    /// Linux `bwrap` (Bubblewrap) namespaces plus seccomp.
    Bubblewrap,
    /// The Windows restricted-token backend.
    Windows,
}

impl SandboxBackend {
    /// Stable identifier for persisted events. Do not re-spell these: stored
    /// events are read back by string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Seatbelt => "seatbelt",
            Self::Bubblewrap => "bubblewrap",
            Self::Windows => "windows",
        }
    }

    /// The backend this build wraps commands with.
    pub fn current() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::Seatbelt
        }
        #[cfg(target_os = "windows")]
        {
            Self::Windows
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Self::Bubblewrap
        }
    }
}

/// What the denied operation was reaching for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViolationKind {
    /// A file or directory access. `path` is present when the backend named
    /// the target; bwrap and seccomp often do not.
    Filesystem {
        /// The denied path, as the backend reported it.
        path: Option<String>,
    },
    /// An outbound socket or DNS operation.
    Network,
}

impl ViolationKind {
    /// Stable identifier for persisted events.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Filesystem { .. } => "filesystem",
            Self::Network => "network",
        }
    }
}

/// One normalized denial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxViolation {
    /// Which backend produced it.
    pub backend: SandboxBackend,
    /// What was reached for.
    pub kind: ViolationKind,
    /// The backend's own operation name, kept verbatim (`file-read-data`,
    /// `seccomp`, `bind-mount`). Useful precisely because it is not
    /// normalized — it is the term the backend's own docs use.
    pub reason: String,
    /// Bounded raw evidence. Callers persisting this must redact it first;
    /// this crate has no secret patterns of its own.
    pub snippet: String,
}

/// The wait status a `SIGSYS` kill surfaces as through a shell (`128 + 31`).
/// bwrap's seccomp filter is the only thing in Borg's stack that raises it,
/// so it is an unambiguous denial signal even with empty stderr.
const SIGSYS_SHELL_STATUS: i32 = 159;

/// Classify a finished sandboxed command into a violation, or `None` if
/// nothing indicates the sandbox denied it.
///
/// Deliberately conservative. A bare "Permission denied" is *not* enough: an
/// ordinary `chmod 000` file produces exactly that text, and a false denial
/// event is worse than a missing one — it would teach an operator to distrust
/// the record. Only a backend-specific marker classifies.
pub fn classify(
    backend: SandboxBackend,
    exit_code: Option<i32>,
    stderr: &str,
) -> Option<SandboxViolation> {
    let build = |kind, reason: &str| {
        Some(SandboxViolation {
            backend,
            kind,
            reason: reason.to_string(),
            snippet: bound_snippet(stderr),
        })
    };

    if exit_code == Some(SIGSYS_SHELL_STATUS) {
        // Killed by the seccomp filter. There is no path and often no output.
        return build(ViolationKind::Filesystem { path: None }, "seccomp");
    }

    for line in stderr.lines() {
        if let Some((reason, path)) = seatbelt_denial(line) {
            let kind = if reason.starts_with("network") {
                ViolationKind::Network
            } else {
                ViolationKind::Filesystem { path }
            };
            return build(kind, &reason);
        }
        if let Some((reason, kind)) = bwrap_denial(line) {
            return build(kind, &reason);
        }
    }

    None
}

/// Parse a Seatbelt denial line: `deny(1) file-read-data /path/to/thing`.
///
/// The `(1)` group is a filter index and varies, so it is skipped rather than
/// matched — an exact-shape match would break on the next macOS release.
fn seatbelt_denial(line: &str) -> Option<(String, Option<String>)> {
    let rest = line.trim().strip_prefix("deny")?;
    // Either `deny(N) op ...` or `deny op ...`.
    let rest = match rest.strip_prefix('(') {
        Some(after) => after.split_once(')')?.1,
        None => rest,
    };
    let mut parts = rest.split_whitespace();
    let operation = parts.next()?;
    // Guard against matching a sentence that merely starts with "deny": the
    // operation token is always hyphenated or a known bare keyword.
    if !operation.contains('-') && !operation.starts_with("network") {
        return None;
    }
    let path = parts
        .next()
        .filter(|p| p.starts_with('/'))
        .map(str::to_string);
    Some((operation.to_string(), path))
}

/// Parse a bwrap denial line. bwrap prefixes its own errors with `bwrap:`,
/// which is what makes them distinguishable from the child's output.
fn bwrap_denial(line: &str) -> Option<(String, ViolationKind)> {
    let message = line.trim().strip_prefix("bwrap:")?.trim();
    let lowered = message.to_ascii_lowercase();
    if lowered.contains("network") || lowered.contains("unshare net") {
        return Some(("bwrap-network".to_string(), ViolationKind::Network));
    }
    // `bwrap: Can't create file at /etc/passwd: Permission denied`
    let path = message
        .split_once(" at ")
        .and_then(|(_, tail)| tail.split(':').next())
        .map(|p| p.trim().to_string())
        .filter(|p| p.starts_with('/'));
    Some((
        "bwrap-mount".to_string(),
        ViolationKind::Filesystem { path },
    ))
}

/// Trim `stderr` to [`MAX_SNIPPET_BYTES`] on a character boundary. Splitting
/// mid-character would make the persisted JSON unreadable for every consumer.
fn bound_snippet(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.len() <= MAX_SNIPPET_BYTES {
        return trimmed.to_string();
    }
    let mut end = MAX_SNIPPET_BYTES;
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &trimmed[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seatbelt_file_denial_yields_the_operation_and_the_path() {
        let violation = classify(
            SandboxBackend::Seatbelt,
            Some(1),
            "deny(1) file-read-data /Users/someone/.ssh/id_ed25519\n",
        )
        .expect("a seatbelt deny line must classify");

        assert_eq!(violation.reason, "file-read-data");
        assert_eq!(
            violation.kind,
            ViolationKind::Filesystem {
                path: Some("/Users/someone/.ssh/id_ed25519".to_string()),
            }
        );
    }

    #[test]
    fn a_seatbelt_denial_without_the_filter_index_still_classifies() {
        // The `(N)` group is not guaranteed; the parse must not depend on it.
        let violation = classify(
            SandboxBackend::Seatbelt,
            Some(1),
            "deny file-write-create /tmp/out",
        )
        .expect("classifies");
        assert_eq!(violation.reason, "file-write-create");
    }

    #[test]
    fn a_seatbelt_network_denial_classifies_as_network() {
        let violation = classify(
            SandboxBackend::Seatbelt,
            Some(1),
            "deny(1) network-outbound /private/var/run/mDNSResponder",
        )
        .expect("classifies");
        assert_eq!(violation.kind, ViolationKind::Network);
    }

    #[test]
    fn a_bwrap_mount_denial_extracts_the_path() {
        let violation = classify(
            SandboxBackend::Bubblewrap,
            Some(1),
            "bwrap: Can't create file at /etc/passwd: Permission denied",
        )
        .expect("classifies");
        assert_eq!(
            violation.kind,
            ViolationKind::Filesystem {
                path: Some("/etc/passwd".to_string()),
            }
        );
    }

    #[test]
    fn a_seccomp_kill_classifies_from_the_status_alone() {
        // The defining case: the child is killed by SIGSYS and prints nothing,
        // so status is the only evidence there is.
        let violation = classify(SandboxBackend::Bubblewrap, Some(SIGSYS_SHELL_STATUS), "")
            .expect("a seccomp kill must be recorded");
        assert_eq!(violation.reason, "seccomp");
        assert_eq!(violation.kind, ViolationKind::Filesystem { path: None });
    }

    #[test]
    fn an_ordinary_permission_error_is_not_reported_as_a_denial() {
        // `chmod 000 f && cat f` produces this. Recording it would fill the
        // ledger with denials the sandbox never made.
        assert!(classify(
            SandboxBackend::Seatbelt,
            Some(1),
            "cat: /tmp/locked: Permission denied",
        )
        .is_none());
    }

    #[test]
    fn a_successful_command_reports_nothing() {
        assert!(classify(SandboxBackend::Seatbelt, Some(0), "").is_none());
    }

    #[test]
    fn prose_beginning_with_deny_does_not_masquerade_as_a_denial() {
        assert!(classify(
            SandboxBackend::Seatbelt,
            Some(1),
            "denying the request was the right call",
        )
        .is_none());
    }

    #[test]
    fn a_long_stderr_is_bounded_on_a_character_boundary() {
        let noise = "é".repeat(MAX_SNIPPET_BYTES);
        let stderr = format!("deny(1) file-read-data /secret\n{noise}");

        let violation = classify(SandboxBackend::Seatbelt, Some(1), &stderr).expect("classifies");

        assert!(
            violation.snippet.len() <= MAX_SNIPPET_BYTES + "…".len(),
            "the snippet must not carry an unbounded stderr into a persisted event"
        );
        assert!(violation.snippet.ends_with('…'));
        assert!(!violation.snippet.contains('\u{fffd}'));
    }

    #[test]
    fn backend_identifiers_are_stable() {
        // Persisted events are read back by string; renaming one silently
        // orphans every row already written.
        assert_eq!(SandboxBackend::Seatbelt.as_str(), "seatbelt");
        assert_eq!(SandboxBackend::Bubblewrap.as_str(), "bubblewrap");
        assert_eq!(SandboxBackend::Windows.as_str(), "windows");
        assert_eq!(ViolationKind::Network.as_str(), "network");
        assert_eq!(
            ViolationKind::Filesystem { path: None }.as_str(),
            "filesystem"
        );
    }
}
