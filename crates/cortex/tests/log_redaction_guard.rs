//! Source-grep guard: forbid log statements that emit the substrings
//! `api_key=`, `token=`, or `password=` followed by an interpolated value
//! without going through `redact_secrets`.
//!
//! This is a CLAUDE.md-sanctioned source-grep test (the redaction invariant
//! cannot be expressed in the type system because tracing macros take
//! arbitrary `&str`s). It scans every Rust source file in the workspace and
//! flags two leak shapes:
//!   1. format-literal interpolation: `info!("api_key={k}", ...)`
//!   2. structured-field syntax: `info!(api_key = %k, ...)` — `%k` calls
//!      `Display` on the value, so a `String` containing the raw key leaks.
//!
//! Allowlist: the leak shape is followed by a redaction marker token like
//! `<redacted>` / `[REDACTED` / `<masked>` / `<scrubbed>`.

use std::fs;
use std::path::{Path, PathBuf};

const FORBIDDEN: &[&str] = &["api_key=", "token=", "password=", "passphrase="];
const REDACTION_MARKERS: &[&str] = &["<redacted", "[REDACTED", "<masked", "<scrubbed"];
const LOG_MACROS: &[&str] = &[
    "tracing::trace!",
    "tracing::debug!",
    "tracing::info!",
    "tracing::warn!",
    "tracing::error!",
    "trace!(",
    "debug!(",
    "info!(",
    "warn!(",
    "error!(",
];

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points at crates/cortex; go up two for the workspace.
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    here.parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or(here)
}

fn collect_rs_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if path.is_dir() {
            // Skip target/ and tests/ (this very file would self-flag otherwise).
            if name == "target" || name == "tests" {
                continue;
            }
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Find each call to a logging macro and return its first-arg string literal,
/// if any. Coarse parser — good enough to catch the common shape
/// `info!("api_key={k}", ...)`. Skips field-style invocations that don't
/// start with a literal.
fn extract_log_literals(src: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (line_idx, line) in src.lines().enumerate() {
        for mac in LOG_MACROS {
            let Some(start) = line.find(mac) else {
                continue;
            };
            let rest = &line[start + mac.len()..];
            // The literal we care about is the first `"…"` on the same line.
            // Skip bytes until we find an opening quote that isn't preceded by `r`/`b`
            // (raw/byte string handling is approximated — we still scan their content).
            let Some(open) = rest.find('"') else { continue };
            let after = &rest[open + 1..];
            // Walk to the matching close quote, honoring `\"` escapes.
            let bytes = after.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    break;
                }
                i += 1;
            }
            if i < bytes.len() {
                out.push((line_idx + 1, after[..i].to_string()));
            }
        }
    }
    out
}

fn looks_redacted(literal: &str) -> bool {
    REDACTION_MARKERS.iter().any(|m| literal.contains(m))
}

/// Match structured-field leaks like `info!(api_key = %k, …)` or `… = ?k`.
/// `%`/`?` are tracing's Display/Debug sigils; both stringify the value.
/// Returns matched (line_idx, leak_shape) tuples.
fn extract_structured_field_leaks(src: &str) -> Vec<(usize, String)> {
    let names = ["api_key", "apikey", "token", "password", "passphrase"];
    let mut out = Vec::new();
    for (line_idx, line) in src.lines().enumerate() {
        // Only inspect lines that contain a logging macro at all.
        if !LOG_MACROS.iter().any(|m| line.contains(m)) {
            continue;
        }
        for name in names {
            // Must look like an identifier (avoid `key_path=` etc).
            let needle = format!("{name} = ");
            let Some(idx) = line.find(&needle) else {
                continue;
            };
            let after = &line[idx + needle.len()..];
            // Structured-field leak shape: `<name> = %x` or `<name> = ?x`.
            // A bare literal value (`api_key = "abc"`) is not flagged here —
            // it's caught by the format-literal scan if it lives in the
            // format string.
            let bytes = after.as_bytes();
            if bytes
                .first()
                .map(|b| *b == b'%' || *b == b'?')
                .unwrap_or(false)
            {
                let snippet = format!("{name} = {}", &after[..after.len().min(40)]);
                if !looks_redacted(&snippet) {
                    out.push((line_idx + 1, snippet));
                }
            }
        }
    }
    out
}

#[test]
fn no_log_macro_leaks_secrets_in_structured_fields() {
    let root = workspace_root().join("crates");
    let mut files = Vec::new();
    collect_rs_files(&root, &mut files);

    let mut violations: Vec<String> = Vec::new();
    for path in &files {
        let rel = path.strip_prefix(&root).unwrap_or(path);
        let rel_s = rel.to_string_lossy();
        if rel_s.contains("secrets.rs") || rel_s.contains("secrets_resolve.rs") {
            continue;
        }
        let Ok(src) = fs::read_to_string(path) else {
            continue;
        };
        for (line, snippet) in extract_structured_field_leaks(&src) {
            violations.push(format!(
                "{}:{line}: structured-field log emits sensitive name with %/? sigil: {snippet:?}",
                rel.display()
            ));
        }
    }
    if !violations.is_empty() {
        panic!(
            "log redaction guard found {} structured-field violation(s):\n{}",
            violations.len(),
            violations.join("\n")
        );
    }
}

#[test]
fn no_log_macro_leaks_secrets_in_format_literals() {
    let root = workspace_root().join("crates");
    let mut files = Vec::new();
    collect_rs_files(&root, &mut files);
    assert!(!files.is_empty(), "no source files found under {root:?}");

    let mut violations: Vec<String> = Vec::new();
    for path in &files {
        // Skip the redaction module itself — the secret patterns it defines
        // legitimately contain `password|token|key` substrings.
        let rel = path.strip_prefix(&root).unwrap_or(path);
        let rel_s = rel.to_string_lossy();
        if rel_s.contains("secrets.rs") || rel_s.contains("secrets_resolve.rs") {
            continue;
        }

        let Ok(src) = fs::read_to_string(path) else {
            continue;
        };
        for (line, literal) in extract_log_literals(&src) {
            for needle in FORBIDDEN {
                if !literal.contains(needle) {
                    continue;
                }
                if looks_redacted(&literal) {
                    continue;
                }
                violations.push(format!(
                    "{}:{line}: log literal contains {needle:?} without a redaction marker: {literal:?}",
                    rel.display()
                ));
            }
        }
    }

    if !violations.is_empty() {
        panic!(
            "log redaction guard found {} violation(s):\n{}",
            violations.len(),
            violations.join("\n")
        );
    }
}
