//! Tool-call effect classification and concurrent-grouping planner.
//!
//! Given a batch of tool calls from a single assistant response, this module
//! decides which ones can safely fan out in parallel and which must stay
//! sequential. The rules are conservative: anything whose effect we can't
//! statically reason about falls back to a singleton group.
//!
//! The classifier recognizes three effect kinds:
//! - `ToolEffect::ReadOnly` — does not mutate filesystem or agent state.
//! - `ToolEffect::Writes` — mutates named filesystem paths (tracked so the
//!   planner can detect conflicts with concurrent writes to overlapping paths).
//! - `ToolEffect::Unknown` — escapes static analysis (shell, browser, user
//!   input, multi-agent, scheduling, generation). Always forces a sequential
//!   boundary.
//!
//! The planner emits groups of tool-call indices: each group's members can be
//! run concurrently with one another; consecutive groups run strictly in
//! sequence. Result ordering within a group is reconstructed by the caller.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::tool_handlers::filesystem::PATCH_DSL_MARKERS;
use crate::types::ToolCall;

/// The one marker whose payload is `src -> dst` rather than a bare path.
const MOVE_FILE_MARKER: &str = "*** Move File: ";

/// Static effect of a single tool invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolEffect {
    /// Does not mutate filesystem or agent state.
    ReadOnly,
    /// Mutates one or more filesystem paths.
    Writes(Vec<PathBuf>),
    /// Effect is not statically known. Forces a sequential group boundary.
    Unknown,
}

/// Classify a tool call by its name and parsed arguments.
///
/// The caller passes the arguments as a `serde_json::Value`. Unparseable args
/// yield `ToolEffect::Unknown` from the planner via an earlier gate; this
/// function assumes `args` has already deserialized cleanly.
///
/// `cwd` resolves relative path arguments and MUST match the cwd the executor
/// uses, or the planner will reason about different files than the ones the
/// tools actually write.
pub fn classify(name: &str, args: &Value, cwd: &Path) -> ToolEffect {
    // MCP tools live outside the typed `Tool` enum. The registry tracks per-
    // tool read-only state (operator opt-in OR server-advertised
    // `readOnlyHint`), so consult it before falling through to the
    // hardcoded match — an MCP tool name will never hit any of the literal
    // arms below.
    if name.starts_with("mcp_") {
        return if crate::mcp_clients::is_read_only_mcp_tool(name) {
            ToolEffect::ReadOnly
        } else {
            ToolEffect::Unknown
        };
    }
    match name {
        // Read-only tools — safe to fan out in parallel.
        //
        // `list` and `list_agents` are deliberately excluded: they need a
        // live `&AgentControl` reference that doesn't cheaply cross a task
        // boundary. Treating them as Unknown keeps them sequential.
        "read_file" | "read_memory" | "memory_search" | "list_skills" | "list_channels"
        | "list_dir" | "web_fetch" | "web_search" => ToolEffect::ReadOnly,

        // Patching tools mutate files at explicit paths.
        "apply_patch" => match extract_patch_paths(args, cwd) {
            Some(paths) if !paths.is_empty() => ToolEffect::Writes(paths),
            _ => ToolEffect::Unknown,
        },
        "apply_skill_patch" => match extract_skill_patch_paths(args) {
            Some(paths) if !paths.is_empty() => ToolEffect::Writes(paths),
            _ => ToolEffect::Unknown,
        },

        // Everything else — shell, browser, write_memory, multi-agent,
        // scheduling, media generation, user input — is Unknown by design.
        _ => ToolEffect::Unknown,
    }
}

/// Split a batch of tool calls into concurrency groups.
///
/// Each returned `Vec<usize>` is a list of indices into the input slice whose
/// calls can run concurrently. Groups are emitted in original order; callers
/// must run groups sequentially and preserve per-call result ordering when
/// stitching results back together.
///
/// Grouping rules, applied left-to-right:
/// 1. An `Unknown` call is always its own singleton group.
/// 2. A `ReadOnly` call can join any group that has no overlapping writes.
/// 3. A `Writes` call can join a group only if none of its paths overlap any
///    path already reserved by the group (by a prior `Writes`).
///
/// `cwd` resolves relative path arguments; pass the executor's workspace cwd.
pub fn plan_groups(tool_calls: &[ToolCall], cwd: &Path) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut reserved: Vec<PathBuf> = Vec::new();

    for (idx, tc) in tool_calls.iter().enumerate() {
        let args: Value = serde_json::from_str(&tc.function.arguments).unwrap_or(Value::Null);
        let effect = classify(&tc.function.name, &args, cwd);

        let fits = match &effect {
            ToolEffect::Unknown => false,
            ToolEffect::ReadOnly => true,
            ToolEffect::Writes(paths) => paths.iter().all(|p| {
                reserved
                    .iter()
                    .all(|r| !paths_overlap(p, r) && !paths_overlap(r, p))
            }),
        };

        if !fits && !current.is_empty() {
            groups.push(std::mem::take(&mut current));
            reserved.clear();
        }

        if matches!(effect, ToolEffect::Unknown) {
            groups.push(vec![idx]);
            continue;
        }

        if let ToolEffect::Writes(paths) = &effect {
            reserved.extend(paths.iter().cloned());
        }
        current.push(idx);
    }

    if !current.is_empty() {
        groups.push(current);
    }

    groups
}

/// Canonicalize a raw path argument into a form used **only** for overlap
/// comparison — never for I/O.
///
/// Two spellings that name the same physical file must produce the same value
/// here, or the planner will let concurrent writes race on it:
/// - `~` is expanded and relative paths are resolved against `cwd`, which must
///   be the same cwd the executor resolves against (see
///   [`crate::execution_context::ExecutionContext::workspace_cwd`]).
/// - Symlinks in the existing portion of the path are resolved, so an alias and
///   its target collapse together. The path may not exist yet (a pending
///   `Add File` patch), so only the deepest existing ancestor is canonicalized
///   and the missing tail is re-attached.
/// - On case-insensitive filesystems the result is case-folded, so `Foo.txt`
///   and `foo.txt` collide.
///
/// Every fallback here errs toward *more* overlap (a sequential group), which
/// costs parallelism but never correctness.
fn canonical_for_overlap(raw: &str, cwd: &Path) -> PathBuf {
    let expanded = crate::util::unix_ext::expand_tilde(raw.trim());
    let path = Path::new(&expanded);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };

    let resolved = match crate::execution_context::synthesize_canonical_for_write(&absolute) {
        Ok(p) => p,
        Err(e) => {
            // Unreachable for an absolute path (`/` always exists), but a
            // failure here would silently weaken conflict detection.
            tracing::warn!(
                "tool planner could not canonicalize {} ({e}); comparing the unresolved path",
                absolute.display()
            );
            absolute
        }
    };
    fold_case(resolved)
}

/// Case-fold a path for comparison on case-insensitive filesystems.
///
/// Folding on a case-sensitive volume (an APFS case-sensitive disk) can only
/// merge two distinct files into one sequential group — the conservative
/// direction — so the target-OS check is deliberately coarse.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn fold_case(path: PathBuf) -> PathBuf {
    PathBuf::from(path.as_os_str().to_string_lossy().to_lowercase())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn fold_case(path: PathBuf) -> PathBuf {
    path
}

/// True if `a` is a prefix of `b` or vice versa. Both paths are compared
/// component-wise to avoid prefix collisions on partial component matches
/// (e.g. `/foo/bar` must NOT overlap `/foo/bar2`).
fn paths_overlap(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    let mut ac = a.components();
    let mut bc = b.components();
    loop {
        match (ac.next(), bc.next()) {
            (None, _) | (_, None) => return true, // one is a proper prefix
            (Some(x), Some(y)) if x == y => continue,
            _ => return false,
        }
    }
}

/// Extract the `path` argument (or the patch-level file headers) from an
/// `apply_patch` call.
///
/// The header set is [`PATCH_DSL_MARKERS`] — shared with the handler that
/// actually applies the patch, so the planner can't reserve a narrower set of
/// files than the executor writes.
///
/// Header matching is **column-0 only** — patch body content lines start
/// with `+`, `-`, or space in the patch DSL, so a header-like
/// string embedded in a content line will have a leading prefix character
/// and will correctly be ignored.
///
/// Caps the extracted path list at 256 entries to bound the planner's
/// O(n²) overlap check against a pathological patch.
fn extract_patch_paths(args: &Value, cwd: &Path) -> Option<Vec<PathBuf>> {
    const MAX_PATHS: usize = 256;
    let mut paths: Vec<PathBuf> = Vec::new();

    // Single-path form: `{"path": "...", ...}`.
    if let Some(p) = args.get("path").and_then(Value::as_str) {
        paths.push(canonical_for_overlap(p, cwd));
    }

    // Unified patch form: `{"patch": "*** Begin Patch\n*** Update File: x\n..."}`.
    if let Some(text) = args.get("patch").and_then(Value::as_str) {
        for line in text.lines() {
            if paths.len() >= MAX_PATHS {
                // Bail out: unreasonably many files in one patch. Treat as
                // Unknown by returning None so the planner falls back to a
                // sequential group.
                return None;
            }
            // `Move File: src -> dst` writes BOTH endpoints, and must be
            // matched before the generic loop or the whole `src -> dst` string
            // would be reserved as one nonexistent path — reserving nothing
            // real and letting a concurrent write to `src` run in parallel.
            if let Some(rest) = line.strip_prefix(MOVE_FILE_MARKER) {
                match rest.split_once("->") {
                    Some((src, dst)) => {
                        paths.push(canonical_for_overlap(src.trim(), cwd));
                        paths.push(canonical_for_overlap(dst.trim(), cwd));
                    }
                    // Malformed move header — the executor will reject it, but
                    // we can't tell which files it meant. Force a sequential
                    // group rather than guess.
                    None => return None,
                }
                continue;
            }
            for prefix in PATCH_DSL_MARKERS {
                if let Some(rest) = line.strip_prefix(prefix) {
                    paths.push(canonical_for_overlap(rest.trim(), cwd));
                    break;
                }
            }
        }
    }

    if paths.is_empty() {
        None
    } else {
        Some(paths)
    }
}

/// Extract the target skill + file path from an `apply_skill_patch` call.
/// Returns a synthetic path scoped to the skill so concurrent patches against
/// the same skill serialize, while patches against different skills can run
/// in parallel.
///
/// The synthetic path is case-folded on the same platforms as a real one:
/// skills live in a single on-disk directory per name, so two case-variant
/// spellings are the same skill wherever the filesystem says they are.
fn extract_skill_patch_paths(args: &Value) -> Option<Vec<PathBuf>> {
    let skill = args
        .get("skill")
        .and_then(Value::as_str)
        .or_else(|| args.get("name").and_then(Value::as_str))?;
    Some(vec![fold_case(PathBuf::from(format!("/skills/{skill}")))])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FunctionCall, ToolCall};

    fn tc(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: id.into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: args.to_string(),
            },
        }
    }

    /// Planner cwd for tests that only use absolute paths.
    fn cwd() -> &'static Path {
        Path::new("/tmp")
    }

    #[test]
    fn readonly_tools_classify_readonly() {
        assert_eq!(
            classify("read_file", &serde_json::json!({"path": "a.txt"}), cwd()),
            ToolEffect::ReadOnly
        );
        assert_eq!(
            classify("web_search", &serde_json::Value::Null, cwd()),
            ToolEffect::ReadOnly
        );
        assert_eq!(
            classify("memory_search", &serde_json::json!({"query": "foo"}), cwd()),
            ToolEffect::ReadOnly
        );
    }

    #[test]
    fn unknown_tools_classify_unknown() {
        for name in [
            "run_shell",
            "browser",
            "write_memory",
            "spawn_agent",
            "schedule",
            "request_user_input",
            "generate_image",
        ] {
            assert_eq!(
                classify(name, &serde_json::Value::Null, cwd()),
                ToolEffect::Unknown,
                "{name} should be Unknown"
            );
        }
    }

    #[test]
    fn apply_patch_extracts_path_field() {
        let effect = classify(
            "apply_patch",
            &serde_json::json!({"path": "/tmp/foo.rs", "patch": ""}),
            cwd(),
        );
        match effect {
            ToolEffect::Writes(paths) => {
                assert_eq!(paths.len(), 1);
                assert!(paths[0].ends_with("foo.rs"));
            }
            other => panic!("expected Writes, got {other:?}"),
        }
    }

    #[test]
    fn apply_patch_extracts_headers_from_patch_body() {
        let body = "*** Begin Patch\n*** Update File: a.rs\n @@\n-old\n+new\n*** Add File: b.rs\n+line\n*** End Patch";
        let effect = classify("apply_patch", &serde_json::json!({"patch": body}), cwd());
        match effect {
            ToolEffect::Writes(paths) => {
                assert_eq!(paths.len(), 2);
                assert!(paths.iter().any(|p| p.ends_with("a.rs")));
                assert!(paths.iter().any(|p| p.ends_with("b.rs")));
            }
            other => panic!("expected Writes, got {other:?}"),
        }
    }

    #[test]
    fn apply_patch_with_no_paths_is_unknown() {
        assert_eq!(
            classify("apply_patch", &serde_json::json!({}), cwd()),
            ToolEffect::Unknown
        );
    }

    #[test]
    fn apply_patch_ignores_header_in_patch_body_content() {
        // A patch body content line like `+*** Add File: evil.rs` has a leading
        // `+` (per patch DSL), and must NOT be treated as a header.
        let body = "*** Begin Patch\n*** Update File: real.rs\n@@\n+*** Add File: fake.rs\n+normal line\n*** End Patch";
        let effect = classify("apply_patch", &serde_json::json!({"patch": body}), cwd());
        match effect {
            ToolEffect::Writes(paths) => {
                assert_eq!(paths.len(), 1, "only real.rs should match, not fake.rs");
                assert!(paths[0].ends_with("real.rs"));
            }
            other => panic!("expected Writes, got {other:?}"),
        }
    }

    #[test]
    fn apply_patch_caps_path_list() {
        // Pathological patch with >256 Add File headers should bail to
        // Unknown (the planner then makes this a singleton sequential group).
        let mut body = String::from("*** Begin Patch\n");
        for i in 0..300 {
            body.push_str(&format!("*** Add File: f{i}.rs\n+line\n"));
        }
        body.push_str("*** End Patch");
        let effect = classify("apply_patch", &serde_json::json!({"patch": body}), cwd());
        assert_eq!(effect, ToolEffect::Unknown);
    }

    #[test]
    fn paths_overlap_same_file() {
        assert!(paths_overlap(Path::new("/a/b.rs"), Path::new("/a/b.rs")));
    }

    #[test]
    fn paths_overlap_parent_child() {
        assert!(paths_overlap(Path::new("/a"), Path::new("/a/b.rs")));
        assert!(paths_overlap(Path::new("/a/b.rs"), Path::new("/a")));
    }

    #[test]
    fn paths_overlap_siblings_do_not() {
        assert!(!paths_overlap(Path::new("/a/b.rs"), Path::new("/a/c.rs")));
    }

    #[test]
    fn paths_overlap_prefix_collision_is_not_overlap() {
        // `/foo/bar` must NOT overlap `/foo/bar2` (component-wise comparison).
        assert!(!paths_overlap(
            Path::new("/foo/bar"),
            Path::new("/foo/bar2")
        ));
    }

    #[test]
    fn planner_groups_three_reads_together() {
        let calls = vec![
            tc("1", "read_file", serde_json::json!({"path": "a"})),
            tc("2", "read_file", serde_json::json!({"path": "b"})),
            tc("3", "read_file", serde_json::json!({"path": "c"})),
        ];
        let groups = plan_groups(&calls, cwd());
        assert_eq!(groups, vec![vec![0, 1, 2]]);
    }

    #[test]
    fn planner_splits_on_unknown() {
        let calls = vec![
            tc("1", "read_file", serde_json::json!({"path": "a"})),
            tc("2", "run_shell", serde_json::json!({"command": "ls"})),
            tc("3", "read_file", serde_json::json!({"path": "b"})),
        ];
        let groups = plan_groups(&calls, cwd());
        assert_eq!(groups, vec![vec![0], vec![1], vec![2]]);
    }

    #[test]
    fn planner_merges_reads_with_disjoint_writes() {
        let calls = vec![
            tc("1", "read_file", serde_json::json!({"path": "/tmp/a.rs"})),
            tc("2", "apply_patch", serde_json::json!({"path": "/var/b.rs"})),
            tc("3", "read_file", serde_json::json!({"path": "/etc/c"})),
        ];
        let groups = plan_groups(&calls, cwd());
        assert_eq!(groups.len(), 1, "all three should fit in one group");
    }

    #[test]
    fn planner_splits_overlapping_writes() {
        let calls = vec![
            tc("1", "apply_patch", serde_json::json!({"path": "/tmp/a.rs"})),
            tc("2", "apply_patch", serde_json::json!({"path": "/tmp/a.rs"})),
        ];
        let groups = plan_groups(&calls, cwd());
        assert_eq!(groups, vec![vec![0], vec![1]]);
    }

    #[test]
    fn planner_empty_batch_returns_empty() {
        assert!(plan_groups(&[], cwd()).is_empty());
    }

    #[test]
    fn planner_all_unknown_each_singleton() {
        let calls = vec![
            tc("1", "run_shell", serde_json::json!({})),
            tc("2", "run_shell", serde_json::json!({})),
            tc("3", "run_shell", serde_json::json!({})),
        ];
        let groups = plan_groups(&calls, cwd());
        assert_eq!(groups, vec![vec![0], vec![1], vec![2]]);
    }

    #[test]
    fn planner_read_write_read_write_disjoint() {
        // Alternating reads/writes on disjoint paths should all merge.
        let calls = vec![
            tc("1", "read_file", serde_json::json!({"path": "/a"})),
            tc("2", "apply_patch", serde_json::json!({"path": "/b"})),
            tc("3", "read_file", serde_json::json!({"path": "/c"})),
            tc("4", "apply_patch", serde_json::json!({"path": "/d"})),
        ];
        let groups = plan_groups(&calls, cwd());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0], vec![0, 1, 2, 3]);
    }

    /// Two writes that resolve to the same physical file must never share a
    /// group — whatever spelling the model used to name it.
    #[test]
    fn planner_splits_writes_aliased_by_relative_and_absolute_spelling() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("a.rs"), "x").unwrap();

        let calls = vec![
            tc(
                "1",
                "apply_patch",
                serde_json::json!({"path": root.join("a.rs").to_str().unwrap()}),
            ),
            tc("2", "apply_patch", serde_json::json!({"path": "a.rs"})),
        ];
        let groups = plan_groups(&calls, &root);
        assert_eq!(
            groups,
            vec![vec![0], vec![1]],
            "the same file named absolutely and relatively must serialize"
        );
    }

    #[test]
    fn planner_splits_writes_through_a_symlink_alias() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let real = root.join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("a.rs"), "x").unwrap();

        let link = root.join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&real, &link).unwrap();

        let calls = vec![
            tc(
                "1",
                "apply_patch",
                serde_json::json!({"path": real.join("a.rs").to_str().unwrap()}),
            ),
            tc(
                "2",
                "apply_patch",
                serde_json::json!({"path": link.join("a.rs").to_str().unwrap()}),
            ),
        ];
        let groups = plan_groups(&calls, &root);
        assert_eq!(
            groups,
            vec![vec![0], vec![1]],
            "a symlinked alias of a file is that file"
        );
    }

    /// A not-yet-created file still canonicalizes through its existing parent,
    /// so an `Add File` behind a symlinked directory conflicts correctly.
    #[test]
    fn planner_splits_writes_to_missing_file_under_symlinked_parent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let real = root.join("real");
        std::fs::create_dir(&real).unwrap();

        let link = root.join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&real, &link).unwrap();

        let calls = vec![
            tc(
                "1",
                "apply_patch",
                serde_json::json!({"path": real.join("new.rs").to_str().unwrap()}),
            ),
            tc(
                "2",
                "apply_patch",
                serde_json::json!({"path": link.join("new.rs").to_str().unwrap()}),
            ),
        ];
        let groups = plan_groups(&calls, &root);
        assert_eq!(groups, vec![vec![0], vec![1]]);
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn planner_splits_case_variant_writes_on_case_insensitive_fs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();

        let calls = vec![
            tc("1", "apply_patch", serde_json::json!({"path": "Foo.txt"})),
            tc("2", "apply_patch", serde_json::json!({"path": "foo.txt"})),
        ];
        let groups = plan_groups(&calls, &root);
        assert_eq!(
            groups,
            vec![vec![0], vec![1]],
            "Foo.txt and foo.txt are one file here"
        );
    }

    /// The planner must resolve relative paths against the cwd it is given,
    /// not the process cwd — the executor uses the former.
    #[test]
    fn planner_resolves_relative_paths_against_the_passed_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();

        let effect = classify("apply_patch", &serde_json::json!({"path": "a.rs"}), &root);
        match effect {
            ToolEffect::Writes(paths) => {
                assert_eq!(paths.len(), 1);
                assert!(
                    paths[0].starts_with(fold_case(root.clone())),
                    "expected {} under {}",
                    paths[0].display(),
                    root.display()
                );
            }
            other => panic!("expected Writes, got {other:?}"),
        }
    }

    /// `Move File: src -> dst` writes both endpoints. If the planner only sees
    /// the `Add File` in the same patch, a concurrent patch updating `src` is
    /// planned as non-overlapping and races the move.
    #[test]
    fn planner_reserves_both_endpoints_of_a_move() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("a.rs"), "x").unwrap();

        let mover = "*** Begin Patch\n*** Add File: new.rs\n+line\n*** Move File: a.rs -> b.rs\n*** End Patch";
        let calls = vec![
            tc("1", "apply_patch", serde_json::json!({"patch": mover})),
            tc("2", "apply_patch", serde_json::json!({"path": "a.rs"})),
        ];
        let groups = plan_groups(&calls, &root);
        assert_eq!(
            groups,
            vec![vec![0], vec![1]],
            "a move's source must be reserved against a concurrent write"
        );

        // ...and the destination too.
        let calls = vec![
            tc("1", "apply_patch", serde_json::json!({"patch": mover})),
            tc("2", "apply_patch", serde_json::json!({"path": "b.rs"})),
        ];
        assert_eq!(plan_groups(&calls, &root), vec![vec![0], vec![1]]);

        // An unrelated file in the same batch still fans out.
        let calls = vec![
            tc("1", "apply_patch", serde_json::json!({"patch": mover})),
            tc(
                "2",
                "apply_patch",
                serde_json::json!({"path": "unrelated.rs"}),
            ),
        ];
        assert_eq!(
            plan_groups(&calls, &root),
            vec![vec![0, 1]],
            "reserving move endpoints must not serialize the whole batch"
        );
    }

    /// A malformed move header names files we can't identify — the planner must
    /// fall back to a sequential group rather than guess and let a write race.
    #[test]
    fn malformed_move_header_forces_sequential() {
        let body = "*** Begin Patch\n*** Move File: a.rs\n*** End Patch";
        assert_eq!(
            classify("apply_patch", &serde_json::json!({"patch": body}), cwd()),
            ToolEffect::Unknown
        );
    }

    #[test]
    fn skill_patch_scopes_by_skill_name() {
        let eff_a = classify(
            "apply_skill_patch",
            &serde_json::json!({"skill": "git"}),
            cwd(),
        );
        let eff_b = classify(
            "apply_skill_patch",
            &serde_json::json!({"skill": "github"}),
            cwd(),
        );
        // Different skills must not overlap.
        match (eff_a, eff_b) {
            (ToolEffect::Writes(pa), ToolEffect::Writes(pb)) => {
                assert!(!paths_overlap(&pa[0], &pb[0]));
            }
            other => panic!("expected two Writes, got {other:?}"),
        }
    }
}
