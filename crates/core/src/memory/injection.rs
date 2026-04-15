//! Prompt-injection scanning for memory content.
//!
//! Memory entries are trusted context injected verbatim into the system
//! prompt. Any attacker-controlled content reaching the memory store must
//! first pass `scan_for_injection` to catch the most obvious override,
//! exfiltration, and invisible-character attacks.

use anyhow::{bail, Result};
use std::sync::OnceLock;

/// Pattern category for injection detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectionCategory {
    /// Attempts to override system instructions.
    PromptOverride,
    /// Attempts to exfiltrate secrets via shell commands.
    Exfiltration,
    /// Invisible Unicode characters that can hide malicious content.
    InvisibleUnicode,
}

impl std::fmt::Display for InjectionCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PromptOverride => write!(f, "prompt_override"),
            Self::Exfiltration => write!(f, "exfiltration"),
            Self::InvisibleUnicode => write!(f, "invisible_unicode"),
        }
    }
}

static INJECTION_PATTERNS: OnceLock<Vec<(regex::Regex, InjectionCategory)>> = OnceLock::new();

fn injection_patterns() -> &'static [(regex::Regex, InjectionCategory)] {
    // SAFETY: These regex patterns are compile-time-valid literals.
    #[allow(clippy::expect_used)]
    INJECTION_PATTERNS.get_or_init(|| {
        vec![
            (
                // Imperative injection framings. The `disregard …` and
                // `you are now` clauses are anchored/qualified to avoid false
                // positives on benign prose that mentions deprecated content
                // ("disregard the old README instructions") or incidentally
                // contains "you are now" mid-sentence ("I think you are now
                // ready"). The anchor-word list for `disregard` keeps true
                // injection patterns (above/previous/prior/following/these/those)
                // while letting arbitrary nouns through.
                regex::Regex::new(
                    r"(?i)(ignore\s+(all\s+)?previous\s+instructions|(^|[\n.!?:]\s*)you\s+are\s+now\b|system\s+prompt\s+override|disregard\s+(\w+\s+){0,2}(above|previous|prior|following|these|those)\s+instructions|new\s+instructions?\s*:)"
                ).expect("compile-time valid regex"),
                InjectionCategory::PromptOverride,
            ),
            (
                regex::Regex::new(
                    r"(?i)(curl|wget|nc|ncat)\s+.*?(api.?key|secret|token|password|credential)"
                ).expect("compile-time valid regex"),
                InjectionCategory::Exfiltration,
            ),
            (
                // Only reject zero-width joiners and bidi overrides that are
                // commonly used for prompt-injection hiding. BOM (FEFF) and
                // LTR/RTL marks (200E/200F) are legitimate in multilingual
                // content and Windows-authored files — excluded here.
                regex::Regex::new(
                    r"[\x{200B}\x{200C}\x{200D}\x{202A}-\x{202E}\x{2060}]"
                ).expect("compile-time valid regex"),
                InjectionCategory::InvisibleUnicode,
            ),
        ]
    })
}

/// Scan content for prompt injection patterns. Returns Ok(()) if clean,
/// or an error identifying the detected category.
///
/// The returned error message intentionally does not expose the match position
/// — that would let a caller iteratively probe/craft bypasses. Position is
/// logged via tracing::debug for operator debugging instead.
pub fn scan_for_injection(content: &str) -> Result<()> {
    for (re, category) in injection_patterns() {
        if let Some(m) = re.find(content) {
            tracing::debug!(
                "scan_for_injection: {category} match at byte offset {}",
                m.start()
            );
            bail!("Memory write rejected: {category} pattern detected");
        }
    }
    Ok(())
}
