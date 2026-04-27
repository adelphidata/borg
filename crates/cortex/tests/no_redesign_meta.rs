//! Source-grep guard: the working-spec language from the v1 redesign
//! (`Task NN`, `docs/redesign`, `Sentry mode`, `v3 deferred`,
//! `architecture-redesign`) must not creep back into source comments. The
//! redesign docs were deleted; references to them rot.
//!
//! Real failure mode this catches: someone pastes back a comment block
//! that links to a `docs/redesign/...` path that no longer exists, or
//! re-tags a deferral with a phase label nobody else can decode.

use std::path::{Path, PathBuf};

const FORBIDDEN: &[&str] = &[
    "docs/redesign",
    "architecture-redesign",
    "Sentry mode",
    "sentry mode",
    "v3 deferr",
    "deferred to v3",
    "v1 redesign",
];

/// Files known to contain the forbidden tokens by design — this guard
/// itself, plus the redesign-meta test in `interface`. Update this list
/// only if you have a structural reason.
fn is_exempt(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s.ends_with("tests/no_redesign_meta.rs")
        || s.ends_with("tests/no_external_tool_refs.rs")
        // Plan files outside the tree are not scanned (we walk crates/ + docs/).
        || s.contains("/target/")
        || s.contains("/.git/")
}

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points at this crate; walk up to the workspace.
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    here.parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .expect("workspace root above crates/cortex")
}

fn walk(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            // Skip vendored / build trees.
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
fn no_redesign_meta_in_source() {
    let root = workspace_root();
    let crates = root.join("crates");
    let docs = root.join("docs");

    let mut files = Vec::new();
    walk(&crates, &mut files);
    if docs.exists() {
        walk(&docs, &mut files);
    }
    let claude_md = root.join("CLAUDE.md");
    if claude_md.exists() {
        files.push(claude_md);
    }

    let mut hits: Vec<String> = Vec::new();
    for path in files {
        if is_exempt(&path) {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (lineno, line) in body.lines().enumerate() {
            for needle in FORBIDDEN {
                if line.contains(needle) {
                    hits.push(format!(
                        "{}:{}: matched `{}`",
                        path.display(),
                        lineno + 1,
                        needle
                    ));
                }
            }
        }
        // Task-NN as a standalone phrase: looser case-sensitive scan to
        // avoid false hits on benign words like `task-1` IDs in fixtures.
        for (lineno, line) in body.lines().enumerate() {
            if line.contains("Task 0") {
                // Allow only as part of a TODO-style "task 0" decimal, not
                // the redesign banner. Bias: if `Task 0` is followed by a
                // digit, treat as redesign-style and reject.
                let after = line.split("Task 0").nth(1).unwrap_or("");
                if after.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                    hits.push(format!(
                        "{}:{}: matched `Task 0<digit>`",
                        path.display(),
                        lineno + 1
                    ));
                }
            }
        }
    }

    assert!(
        hits.is_empty(),
        "redesign-meta tokens found in source:\n{}",
        hits.join("\n")
    );
}
