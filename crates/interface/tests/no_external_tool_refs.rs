//! Source-grep guard: external-tool names (Codex, OpenCode, OpenClaw,
//! Hermes Agent) must not appear in the codebase except inside the
//! `borg migrate` carve-out, which exists specifically to import data
//! from those tools.
//!
//! Real failure mode this catches: someone reintroduces a "Codex-style"
//! comment, a "matches openclaw …" doc reference, or a head-to-head
//! comparison file outside the migrate subtree. Those references rot
//! (linked code drifts) and clutter the codebase with names of unrelated
//! third-party tools.

use std::path::{Path, PathBuf};

const FORBIDDEN: &[&str] = &[
    // Match case-insensitively below; lowercased here.
    "openclaw", "opencode", "hermes", "codex",
];

/// Subtrees + files where the names are allowed. Three categories:
///
/// 1. The `borg migrate` feature, whose entire purpose is to import
///    settings/memory from Hermes Agent and OpenClaw.
/// 2. This test itself + the matching meta-guard.
/// 3. Build artifacts and VCS metadata.
fn is_exempt(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s.contains("/crates/cortex/src/migrate/")
        || s.ends_with("crates/interface/src/migrate_tui.rs")
        || s.ends_with("crates/interface/src/tui/migrate_popup.rs")
        || s.ends_with("crates/interface/src/commands/misc.rs")
        // The `Migrate` clap arm in main.rs has a doc-comment that names
        // the source tools; that's user-visible help text for the
        // carve-out feature.
        || s.ends_with("crates/interface/src/main.rs")
        || s.ends_with("tests/no_external_tool_refs.rs")
        || s.ends_with("tests/no_redesign_meta.rs")
        || s.contains("/target/")
        || s.contains("/.git/")
        || s.contains("/node_modules/")
}

fn workspace_root() -> PathBuf {
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    here.parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .expect("workspace root above crates/interface")
}

fn walk(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or_default();
            if matches!(name, "target" | ".git" | "node_modules") {
                continue;
            }
            walk(&p, out);
        } else if let Some(ext) = p.extension().and_then(|s| s.to_str()) {
            if matches!(ext, "rs" | "md" | "toml" | "proto") {
                out.push(p);
            }
        }
    }
}

#[test]
fn no_external_tool_refs_outside_migrate_carveout() {
    let root = workspace_root();
    let crates = root.join("crates");
    let docs = root.join("docs");

    let mut files = Vec::new();
    walk(&crates, &mut files);
    if docs.exists() {
        walk(&docs, &mut files);
    }
    for top in ["CLAUDE.md", "README.md"] {
        let p = root.join(top);
        if p.exists() {
            files.push(p);
        }
    }

    let mut hits: Vec<String> = Vec::new();
    for path in files {
        if is_exempt(&path) {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        let lower = body.to_ascii_lowercase();
        for needle in FORBIDDEN {
            if lower.contains(needle) {
                // Find first matching line for a useful failure message.
                for (lineno, line) in body.lines().enumerate() {
                    if line.to_ascii_lowercase().contains(needle) {
                        hits.push(format!(
                            "{}:{}: matched `{}`: {}",
                            path.display(),
                            lineno + 1,
                            needle,
                            line.trim(),
                        ));
                        break;
                    }
                }
            }
        }
    }

    assert!(
        hits.is_empty(),
        "external-tool references found outside the migrate carve-out:\n{}",
        hits.join("\n")
    );
}
