//! Source-grep invariant: every output sink that produces agent text
//! visible to a human (or persisted to a DB surface an operator inspects)
//! MUST go through `RedactionPolicy::apply` (or
//! `output_redaction::current_policy()`), and MUST NOT call
//! `redact_secrets` directly.
//!
//! This is a CLAUDE.md-sanctioned source-grep guard (the chokepoint
//! invariant can't be expressed in the type system because `&str` carries
//! no provenance about which sink it's headed for). It pins four call
//! sites:
//!
//!   1. `crates/cortex/src/logging.rs`         — log-file writer.
//!   2. `crates/gateway/src/channel/channel_trait.rs`  — channel delivery dispatch.
//!   3. `crates/gateway/src/handler.rs`        — `channel_messages` DB log.
//!   4. `crates/interface/src/tui/history.rs`  — TUI assistant + tool result.
//!
//! Adding a new output sink WITHOUT updating this list is the failure mode
//! this test is designed to catch. If you add a fifth sink, extend the
//! `CHOKEPOINTS` array and route the sink through `apply()`.

use std::fs;
use std::path::PathBuf;

struct Chokepoint {
    /// Path relative to the workspace root.
    file: &'static str,
    /// Substring that proves the file routes through the policy.
    must_contain: &'static str,
    /// Human-readable description (printed on failure).
    description: &'static str,
}

const CHOKEPOINTS: &[Chokepoint] = &[
    Chokepoint {
        file: "crates/cortex/src/logging.rs",
        must_contain: "current_policy()",
        description: "log-file writer must route through current_policy().apply(Log, …)",
    },
    Chokepoint {
        file: "crates/gateway/src/channel/channel_trait.rs",
        must_contain: "output_redaction::current_policy()",
        description: "gateway dispatch must redact response_text before enqueue/send",
    },
    Chokepoint {
        file: "crates/gateway/src/handler.rs",
        must_contain: "output_redaction::current_policy()",
        description: "channel_messages DB log must redact outbound text through the policy",
    },
    Chokepoint {
        file: "crates/interface/src/tui/history.rs",
        must_contain: "current_policy().apply(OutputSink::Tui",
        description: "TUI Assistant/ToolResult cells must redact before line building",
    },
];

fn workspace_root() -> PathBuf {
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    here.parent()
        .and_then(std::path::Path::parent)
        .map(std::path::Path::to_path_buf)
        .unwrap_or(here)
}

#[test]
fn every_output_chokepoint_routes_through_policy() {
    let root = workspace_root();
    let mut violations = Vec::new();
    for cp in CHOKEPOINTS {
        let path = root.join(cp.file);
        match fs::read_to_string(&path) {
            Ok(src) => {
                if !src.contains(cp.must_contain) {
                    violations.push(format!(
                        "{}: missing `{}` — {}",
                        cp.file, cp.must_contain, cp.description,
                    ));
                }
            }
            Err(e) => violations.push(format!("{}: could not read ({e})", cp.file)),
        }
    }
    assert!(
        violations.is_empty(),
        "output redaction chokepoint(s) regressed:\n  {}",
        violations.join("\n  ")
    );
}

#[test]
fn chokepoints_do_not_bypass_with_direct_redact_secrets_call() {
    // `redact_secrets()` exists and is fine to call (it's the underlying
    // primitive), but the three chokepoint files MUST go through the
    // policy so the toggle and audit log are honoured. A bare
    // `redact_secrets(` call at a sink is the leak shape this catches.
    let root = workspace_root();
    let mut violations = Vec::new();
    for cp in CHOKEPOINTS {
        let path = root.join(cp.file);
        let Ok(src) = fs::read_to_string(&path) else {
            violations.push(format!("{}: could not read", cp.file));
            continue;
        };
        // Allow the substring `redact_secrets` to appear in module-level
        // doc comments / imports for readability — only flag actual
        // function-call shape `redact_secrets(`.
        for (idx, line) in src.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            if line.contains("redact_secrets(") {
                violations.push(format!(
                    "{}:{}: direct `redact_secrets(` call at output chokepoint — must use RedactionPolicy::apply (line: {:?})",
                    cp.file,
                    idx + 1,
                    line.trim(),
                ));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "chokepoint bypass via raw redact_secrets():\n  {}",
        violations.join("\n  ")
    );
}
