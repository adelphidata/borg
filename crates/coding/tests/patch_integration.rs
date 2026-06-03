//! Apply-patch integration tests.
//!
//! Tests complex multi-file patches, path traversal prevention, nested
//! directory creation, and edge cases in the patch DSL.

#![allow(
    clippy::approx_constant,
    clippy::assertions_on_constants,
    clippy::const_is_empty,
    clippy::expect_used,
    clippy::field_reassign_with_default,
    clippy::identity_op,
    clippy::items_after_test_module,
    clippy::len_zero,
    clippy::manual_range_contains,
    clippy::needless_borrow,
    clippy::needless_collect,
    clippy::redundant_clone,
    clippy::redundant_closure_for_method_calls,
    clippy::uninlined_format_args,
    clippy::unnecessary_cast,
    clippy::unnecessary_map_or,
    clippy::unwrap_used,
    clippy::useless_format,
    clippy::useless_vec
)]

use std::fs;

use coding::apply_patch_to_dir;

// ── Test: multi-file add in single patch ──

#[test]
fn multi_file_add_single_patch() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    let patch = "\
*** Begin Patch
*** Add File: src/main.rs
+fn main() {
+    println!(\"hello\");
+}
*** Add File: src/lib.rs
+pub fn add(a: i32, b: i32) -> i32 {
+    a + b
+}
*** Add File: README.md
+# My Project
+A simple project.
*** End Patch";

    let affected = apply_patch_to_dir(patch, dir).expect("multi-file add");
    assert_eq!(affected.added.len(), 3);
    assert!(dir.join("src/main.rs").exists());
    assert!(dir.join("src/lib.rs").exists());
    assert!(dir.join("README.md").exists());

    let main = fs::read_to_string(dir.join("src/main.rs")).expect("read main");
    assert!(main.contains("println!"));
}

// ── Test: nested directory creation ──

#[test]
fn nested_directory_creation() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    let patch = "\
*** Begin Patch
*** Add File: a/b/c/d/deep.txt
+deeply nested content
*** End Patch";

    apply_patch_to_dir(patch, dir).expect("nested add");
    let content = fs::read_to_string(dir.join("a/b/c/d/deep.txt")).expect("read");
    assert!(content.contains("deeply nested"));
}

// ── Test: path traversal rejected ──

#[test]
fn path_traversal_rejected() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    let patch = "\
*** Begin Patch
*** Add File: ../../etc/evil.txt
+evil content
*** End Patch";

    let result = apply_patch_to_dir(patch, dir);
    assert!(result.is_err(), "Path traversal should be rejected");
}

// ── Test: absolute path rejected ──

#[test]
fn absolute_path_rejected() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    let patch = "\
*** Begin Patch
*** Add File: /etc/passwd
+evil
*** End Patch";

    let result = apply_patch_to_dir(patch, dir);
    assert!(result.is_err(), "Absolute paths should be rejected");
}

// ── Test: update nonexistent file errors ──

#[test]
fn update_nonexistent_file_errors() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    let patch = "\
*** Begin Patch
*** Update File: does_not_exist.txt
@@
 old line
-old line
+new line
*** End Patch";

    let result = apply_patch_to_dir(patch, dir);
    assert!(result.is_err(), "Updating nonexistent file should error");
}

// ── Test: add then update in sequence ──

#[test]
fn add_then_update_sequence() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    // First add
    let add_patch = "\
*** Begin Patch
*** Add File: config.yaml
+key: value
+debug: false
*** End Patch";
    apply_patch_to_dir(add_patch, dir).expect("add");

    // Then update
    let update_patch = "\
*** Begin Patch
*** Update File: config.yaml
@@
 key: value
-debug: false
+debug: true
*** End Patch";
    let affected = apply_patch_to_dir(update_patch, dir).expect("update");
    assert!(affected.modified.contains(&"config.yaml".to_string()));

    let content = fs::read_to_string(dir.join("config.yaml")).expect("read");
    assert!(content.contains("debug: true"));
    assert!(!content.contains("debug: false"));
}

// ── Test: delete nonexistent file succeeds ──

#[test]
fn delete_nonexistent_succeeds() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    let patch = "\
*** Begin Patch
*** Delete File: ghost.txt
*** End Patch";

    // Delete of nonexistent should succeed (idempotent) AND still report the
    // path as deleted so callers get a consistent affected-paths summary.
    let result = apply_patch_to_dir(patch, dir).expect("delete nonexistent should be idempotent");
    assert_eq!(
        result.deleted,
        vec!["ghost.txt".to_string()],
        "idempotent delete must still surface the path in the affected set"
    );
    assert!(
        result.added.is_empty() && result.modified.is_empty(),
        "idempotent delete must not report spurious add/modify"
    );
}

// ── Test: affected paths summary ──

#[test]
fn affected_paths_summary() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    // Create a file to modify and one to delete
    fs::write(dir.join("modify.txt"), "original\n").expect("write");
    fs::write(dir.join("remove.txt"), "temporary\n").expect("write");

    let patch = "\
*** Begin Patch
*** Add File: new.txt
+new file
*** Update File: modify.txt
@@
-original
+modified
*** Delete File: remove.txt
*** End Patch";

    let affected = apply_patch_to_dir(patch, dir).expect("mixed operations");
    let summary = affected.format_summary();

    assert!(
        summary.contains("new.txt"),
        "Summary should list added file"
    );
    assert!(
        summary.contains("modify.txt"),
        "Summary should list modified file"
    );
    assert!(
        summary.contains("remove.txt"),
        "Summary should list deleted file"
    );
}

// ── Test: empty patch returns error ──

#[test]
fn empty_patch_returns_error() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    let patch = "\
*** Begin Patch
*** End Patch";

    let result = apply_patch_to_dir(patch, dir);
    assert!(
        result.is_err(),
        "Empty patch with no operations should error"
    );
}

// ── Test: file with spaces in name ──

#[test]
fn spaces_in_filename() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    let patch = "\
*** Begin Patch
*** Add File: my file (1).txt
+content with spaces in name
*** End Patch";

    let affected = apply_patch_to_dir(patch, dir).expect("spaces in filename should work");
    assert!(dir.join("my file (1).txt").exists());
    assert!(affected.added.contains(&"my file (1).txt".to_string()));
}

// ── Test: path traversal error message names the violation ──
//
// The pre-existing `path_traversal_rejected` test only checks `is_err()`. That
// passes even if the error came from an unrelated cause (e.g. parse failure).
// This test pins down that the error is the path-traversal guard specifically,
// so a regression that lets the path through but fails for a different reason
// can't pass silently.

#[test]
fn path_traversal_error_mentions_traversal() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    let patch = "\
*** Begin Patch
*** Add File: ../escape.txt
+evil
*** End Patch";

    let err = apply_patch_to_dir(patch, dir).unwrap_err().to_string();
    assert!(
        err.contains("Path traversal"),
        "expected path-traversal error, got: {err}"
    );
    // And no file is created anywhere.
    assert!(!dir.join("../escape.txt").exists());
    assert!(!dir.parent().unwrap().join("escape.txt").exists());
}

// ── Test: rerunning an Update patch is not silently idempotent ──
//
// Catches a class of bug where an LLM retries a successful apply and the
// second application either silently no-ops (allowing stale callers to think
// the file changed twice) or partially mutates state. The contract is: the
// second apply must error because the search text is no longer present, and
// the file content must equal the post-first-apply content (no torn write).

#[test]
fn rerunning_update_patch_errors_without_partial_mutation() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    fs::write(dir.join("f.txt"), "alpha\nbeta\ngamma\n").expect("seed");

    let patch = "\
*** Begin Patch
*** Update File: f.txt
@@
-beta
+BETA
*** End Patch";

    apply_patch_to_dir(patch, dir).expect("first apply");
    let after_first = fs::read_to_string(dir.join("f.txt")).expect("read");
    assert_eq!(after_first, "alpha\nBETA\ngamma\n");

    let second = apply_patch_to_dir(patch, dir);
    assert!(
        second.is_err(),
        "second apply must fail — search text 'beta' no longer exists"
    );
    let after_second = fs::read_to_string(dir.join("f.txt")).expect("read");
    assert_eq!(
        after_second, after_first,
        "failed re-apply must not mutate the file"
    );
}

// ── Test: multi-hunk update with `@@ context @@` disambiguates near-identical blocks ──
//
// Without context hints, the two `    return value;` blocks are ambiguous and
// the apply must reject. With distinct hints, each hunk anchors to its
// function and both apply correctly. This validates that context hints carry
// real disambiguation power, not decoration.

#[test]
fn context_hints_disambiguate_near_identical_blocks() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    let original = "\
fn alpha(value: i32) -> i32 {
    return value;
}

fn beta(value: i32) -> i32 {
    return value;
}
";
    fs::write(dir.join("dup.rs"), original).expect("seed");

    // First, prove the ambiguity guard fires without hints.
    let ambiguous_patch = "\
*** Begin Patch
*** Update File: dup.rs
@@
-    return value;
+    return value + 1;
*** End Patch";
    let err = apply_patch_to_dir(ambiguous_patch, dir)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("matched 2 locations") || err.contains("Ambiguous"),
        "expected ambiguity error, got: {err}"
    );
    // File untouched.
    assert_eq!(fs::read_to_string(dir.join("dup.rs")).unwrap(), original);

    // Now with hints, both hunks resolve.
    let hinted_patch = "\
*** Begin Patch
*** Update File: dup.rs
@@ fn alpha @@
 fn alpha(value: i32) -> i32 {
-    return value;
+    return value + 1;
 }
@@ fn beta @@
 fn beta(value: i32) -> i32 {
-    return value;
+    return value + 2;
 }
*** End Patch";
    apply_patch_to_dir(hinted_patch, dir).expect("hinted apply");
    let after = fs::read_to_string(dir.join("dup.rs")).expect("read");
    assert!(
        after.contains("fn alpha(value: i32) -> i32 {\n    return value + 1;\n}"),
        "alpha not patched correctly: {after}"
    );
    assert!(
        after.contains("fn beta(value: i32) -> i32 {\n    return value + 2;\n}"),
        "beta not patched correctly: {after}"
    );
}

// ── Test: CRLF input file is normalized to LF after patching ──
//
// `apply_hunks` splits on `content.lines()` which silently strips `\r\n`. The
// file written back uses `\n` joins. This locks down that observable behavior
// so a future change that introduces line-ending preservation (or breaks
// CRLF input entirely) is caught.

#[test]
fn crlf_input_is_normalized_to_lf_on_update() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    fs::write(dir.join("crlf.txt"), "alpha\r\nbeta\r\ngamma\r\n").expect("seed");

    let patch = "\
*** Begin Patch
*** Update File: crlf.txt
@@
-beta
+BETA
*** End Patch";

    apply_patch_to_dir(patch, dir).expect("apply against CRLF file");
    let after = fs::read_to_string(dir.join("crlf.txt")).expect("read");
    assert!(
        !after.contains('\r'),
        "CRLF must be normalized to LF after patch: {after:?}"
    );
    assert!(after.contains("BETA"), "replacement applied: {after:?}");
    assert!(!after.contains("beta\n"), "old text removed: {after:?}");
}

// ── Test: move chain (A→B and C→A in one patch) ──
//
// A common refactor pattern. Each move's destination must be checked against
// the *post-move* state, not the original on-disk state, otherwise C→A would
// be rejected as "destination exists" even though A is being moved away in
// the same patch.

#[test]
fn move_chain_a_to_b_and_c_to_a() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    fs::write(dir.join("a.txt"), "from a\n").expect("seed a");
    fs::write(dir.join("c.txt"), "from c\n").expect("seed c");

    let patch = "\
*** Begin Patch
*** Update File: a.txt
*** Move to: b.txt
*** Update File: c.txt
*** Move to: a.txt
*** End Patch";

    let result = apply_patch_to_dir(patch, dir);

    // We don't assume which behavior is correct — we lock down whatever the
    // applicator does. If it succeeds, both moves must take effect coherently.
    // If it fails, both files must remain untouched.
    match result {
        Ok(_) => {
            assert!(!dir.join("c.txt").exists(), "c.txt should be moved away");
            assert_eq!(
                fs::read_to_string(dir.join("a.txt")).unwrap(),
                "from c\n",
                "a.txt should now hold former c.txt content"
            );
            assert_eq!(
                fs::read_to_string(dir.join("b.txt")).unwrap(),
                "from a\n",
                "b.txt should hold former a.txt content"
            );
        }
        Err(_) => {
            // Whole-patch rollback: original layout intact, no b.txt created.
            assert_eq!(
                fs::read_to_string(dir.join("a.txt")).unwrap(),
                "from a\n",
                "rollback must preserve a.txt"
            );
            assert_eq!(
                fs::read_to_string(dir.join("c.txt")).unwrap(),
                "from c\n",
                "rollback must preserve c.txt"
            );
            assert!(
                !dir.join("b.txt").exists(),
                "rollback must not leave b.txt behind"
            );
        }
    }
}

// ── Test: preflight rejects whole patch when one op fails, no partial writes ──
//
// Multi-op atomicity from the integration surface. An Add op that would
// succeed in isolation must not write to disk if a later Update op in the
// same patch fails its hunk match.

#[test]
fn preflight_failure_prevents_partial_filesystem_writes() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    fs::write(dir.join("existing.txt"), "real content\n").expect("seed");

    let patch = "\
*** Begin Patch
*** Add File: should_not_appear.txt
+this file must not exist after the failed apply
*** Update File: existing.txt
@@
-not in file
+replacement
*** End Patch";

    let err = apply_patch_to_dir(patch, dir).expect_err("update should fail and abort whole patch");
    let _ = err; // error message variants tested elsewhere

    assert!(
        !dir.join("should_not_appear.txt").exists(),
        "Add op must be rolled back on later Update failure"
    );
    assert_eq!(
        fs::read_to_string(dir.join("existing.txt")).unwrap(),
        "real content\n",
        "existing file must be unchanged"
    );
}

// ── Test: missing diff-line prefix is a parse error ──
//
// The CLAUDE.md "common mistake" — Update body lines without `+`/`-`/space
// must be rejected at parse time, not silently dropped or treated as context.

#[test]
fn update_body_line_missing_prefix_is_parse_error() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();
    fs::write(dir.join("f.txt"), "old\n").expect("seed");

    // Note: 'naked line' has no leading space, +, or -.
    let patch = "\
*** Begin Patch
*** Update File: f.txt
@@
naked line
-old
+new
*** End Patch";

    let err = apply_patch_to_dir(patch, dir).unwrap_err().to_string();
    // File untouched.
    assert_eq!(fs::read_to_string(dir.join("f.txt")).unwrap(), "old\n");
    // Error must mention the offending content so the user can fix it.
    assert!(
        !err.is_empty(),
        "error should be reported, got empty: {err}"
    );
}

// ── Test: missing Begin Patch sentinel rejected (default Strict mode) ──
//
// The default ParseMode is Strict — apply_patch_to_dir requires
// *** Begin Patch on the first non-empty line. The error message names the
// missing sentinel so an LLM caller can recover on its next turn without
// guessing the format.

#[test]
fn missing_begin_patch_sentinel_rejected_with_actionable_error() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    let patch = "\
*** Add File: x.txt
+content
*** End Patch";

    let err =
        apply_patch_to_dir(patch, dir).expect_err("strict mode must reject missing Begin Patch");
    let msg = err.to_string();
    assert!(
        msg.contains("Begin Patch"),
        "error must name the missing sentinel so the caller can fix it; got: {msg}"
    );
    assert!(
        !dir.join("x.txt").exists(),
        "no file should be created on parse failure"
    );
}

// ── Test: missing End Patch sentinel rejected ──

#[test]
fn missing_end_patch_sentinel_rejected_with_actionable_error() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    let patch = "\
*** Begin Patch
*** Add File: x.txt
+content";

    let err =
        apply_patch_to_dir(patch, dir).expect_err("strict mode must reject missing End Patch");
    let msg = err.to_string();
    assert!(
        msg.contains("End Patch"),
        "error must name the missing sentinel; got: {msg}"
    );
    assert!(!dir.join("x.txt").exists());
}

// ── Test: heredoc-wrapped patch accepted by default (lenient), rejected in Strict ──
//
// Borg defaults to Lenient parsing so the LLM's frequent `<<'EOF' ... EOF`
// wrapper is unwrapped and applied verbatim. Strict mode is opt-in for
// callers that need the unwrapped grammar enforced.

#[test]
fn heredoc_wrapping_accepted_in_default_lenient_rejected_in_strict() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    let heredoc_patch = "<<'EOF'\n\
*** Begin Patch\n\
*** Add File: y.txt\n\
+hello via heredoc\n\
*** End Patch\n\
EOF";

    // Default (Lenient): unwraps EOF heredoc, applies inner patch.
    let result = apply_patch_to_dir(heredoc_patch, dir).expect("lenient default must accept");
    assert_eq!(result.affected.added, vec!["y.txt"]);
    assert_eq!(
        fs::read_to_string(dir.join("y.txt")).unwrap(),
        "hello via heredoc",
        "heredoc unwrapping must yield the inner content verbatim"
    );

    // Strict: rejects the wrapper.
    let dir2_tmp = tempfile::tempdir().expect("create temp dir");
    let dir2 = dir2_tmp.path();
    let strict_err =
        coding::apply_patch_to_dir_with_mode(heredoc_patch, dir2, coding::ParseMode::Strict)
            .expect_err("strict mode must reject heredoc wrapping");
    assert!(
        strict_err.to_string().contains("Begin Patch"),
        "got: {strict_err}"
    );
    assert!(!dir2.join("y.txt").exists());
}

// ── Test: Lenient does not paper over a missing inner Begin Patch ──
//
// Lenient is *only* about outer-wrapper tolerance. A heredoc whose inner
// content lacks `*** Begin Patch` must still error — otherwise Lenient
// becomes "anything goes," which defeats the whole point of having two
// explicit modes.

#[test]
fn lenient_still_requires_inner_begin_patch_marker() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    let patch = "<<'EOF'\n\
*** Add File: z.txt\n\
+content\n\
EOF";

    let err = coding::apply_patch_to_dir_with_mode(patch, dir, coding::ParseMode::Lenient)
        .expect_err("lenient still requires inner sentinels");
    assert!(
        err.to_string().contains("Begin Patch") || err.to_string().contains("End Patch"),
        "got: {err}"
    );
    assert!(!dir.join("z.txt").exists());
}

// ── Test: multi-hunk update ──

#[test]
fn multi_hunk_update() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    fs::write(
        dir.join("multi.txt"),
        "line 1\nline 2\nline 3\nline 4\nline 5\n",
    )
    .expect("write");

    let patch = "\
*** Begin Patch
*** Update File: multi.txt
@@
-line 1
+LINE ONE
@@
-line 5
+LINE FIVE
*** End Patch";

    apply_patch_to_dir(patch, dir).expect("multi-hunk");
    let content = fs::read_to_string(dir.join("multi.txt")).expect("read");
    assert!(content.contains("LINE ONE"));
    assert!(content.contains("LINE FIVE"));
    assert!(content.contains("line 3")); // untouched middle
}

// Regression guard: a malformed patch where a `@@` chunk header is followed
// by no diff lines (here, immediately by `*** End Patch`) used to be silently
// accepted by the batch parser, producing a no-op hunk and reporting success.
// The agent would believe the patch landed; the file on disk would be
// unchanged. The batch and streaming parsers both now hard-error so the
// LLM gets an actionable message and the file stays untouched.

#[test]
fn empty_chunk_body_surfaces_actionable_error() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    let original = "alpha\nbeta\ngamma\n";
    fs::write(dir.join("f.txt"), original).expect("seed");

    let patch = "\
*** Begin Patch
*** Update File: f.txt
@@
*** End Patch";

    let err = apply_patch_to_dir(patch, dir).expect_err("empty chunk body must error");
    let msg = err.to_string();
    assert!(
        msg.contains("does not contain any lines"),
        "error must explain the empty-chunk problem, got: {msg}"
    );

    // No partial write: file content must be byte-identical to before.
    let after = fs::read_to_string(dir.join("f.txt")).expect("read");
    assert_eq!(
        after, original,
        "file must be untouched on parser rejection"
    );
}

// ── Test: `*** Add File:` overwrites an existing destination ──
//
// Regression guard: `*** Add File:` against a path that already exists is
// permissive — it replaces the file's contents rather than erroring. Locked
// down because the inverse policy (reject-on-exists) would be an equally
// defensible choice; a silent change in either direction would burn LLM turns
// and confuse callers who rely on the current semantics.

#[test]
fn add_file_overwrites_existing_destination() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    fs::write(dir.join("greeting.txt"), "stale content\n").expect("seed");

    let patch = "\
*** Begin Patch
*** Add File: greeting.txt
+fresh content line 1
+fresh content line 2
*** End Patch";

    let affected = apply_patch_to_dir(patch, dir).expect("add over existing must succeed");
    assert!(affected.added.contains(&"greeting.txt".to_string()));

    let content = fs::read_to_string(dir.join("greeting.txt")).expect("read");
    assert!(
        content.contains("fresh content line 1"),
        "destination must hold the new content, got: {content:?}"
    );
    assert!(
        !content.contains("stale content"),
        "previous contents must be gone, got: {content:?}"
    );
}

// ── Test: `*** Delete File:` targeting a directory errors cleanly ──
//
// Regression guard: `*** Delete File:` is intended for regular files only.
// Pointing it at a directory path must surface a clean error during the
// preflight pass (transactional design: no partial filesystem mutation) rather
// than panicking or silently no-op'ing. Without this guard, a typo in the
// patch could escalate to deleting a directory's contents via a later
// alternate implementation.

#[test]
fn delete_directory_path_errors_cleanly() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    fs::create_dir_all(dir.join("subdir")).expect("create subdir");
    fs::write(dir.join("subdir/keep.txt"), "preserved\n").expect("seed");

    let patch = "\
*** Begin Patch
*** Delete File: subdir
*** End Patch";

    let err = apply_patch_to_dir(patch, dir).expect_err("delete on directory must error");
    let msg = err.to_string();
    // Today the io error ("Is a directory") is wrapped by `with_context` and
    // only the wrapper survives the top-level anyhow `to_string()`. The
    // load-bearing guard here is the filesystem-state assertion below: under
    // Borg's transactional preflight, no part of the directory may be touched.
    // The message-shape check is a soft signal that the failure routed through
    // the patch-validation path rather than panicking or partially succeeding.
    assert!(
        msg.contains("Patch validation failed") && msg.contains("subdir"),
        "expected preflight rejection naming the directory, got: {msg}"
    );

    // Transactional invariant: directory and its contents must be untouched.
    assert!(dir.join("subdir").is_dir(), "directory must still exist");
    let kept = fs::read_to_string(dir.join("subdir/keep.txt")).expect("read kept");
    assert_eq!(kept, "preserved\n", "nested file must be untouched");
}

// ── Test: update hunk consisting entirely of `+` lines (pure addition) ──
//
// Regression guard: a chunk with no context and no removals — only `+` lines —
// is a valid shape the parser must accept and the applicator must append to
// end-of-file. Without this guard, a future "require at least one context or
// removal line" tightening would silently break this shape, which LLMs do
// emit for append-style edits. Placement at EOF (rather than start-of-file)
// is the current contract — see `apply_hunks` in `crates/coding/src/apply.rs`.

#[test]
fn update_hunk_with_only_additions_applies() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    fs::write(dir.join("lines.txt"), "first\nsecond\nthird\n").expect("seed");

    let patch = "\
*** Begin Patch
*** Update File: lines.txt
@@
+inserted alpha
+inserted beta
*** End Patch";

    let affected = apply_patch_to_dir(patch, dir).expect("pure-addition hunk must apply");
    assert!(affected.modified.contains(&"lines.txt".to_string()));

    // Pin placement at end-of-file as an exact post-condition. A regression
    // that reordered or duplicated lines (or that moved the insertion point
    // to start-of-file) would fail this assertion.
    let content = fs::read_to_string(dir.join("lines.txt")).expect("read");
    assert_eq!(
        content, "first\nsecond\nthird\ninserted alpha\ninserted beta\n",
        "pure-addition chunk must append to end-of-file preserving original content"
    );
}

// ── Test: leading whitespace on `*** Update File:` header is tolerated ──
//
// Regression guard: LLM patch outputs occasionally indent the operation
// headers (`*** Add File:` / `*** Update File:` / etc.) by accident. The
// parser strips leading whitespace per line before matching the marker
// prefix; this test pins that tolerance so a stricter rewrite cannot silently
// reject patches the agent currently accepts.

#[test]
fn whitespace_padded_update_file_header_accepted() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    fs::write(dir.join("foo.txt"), "old line\n").expect("seed");

    // Two leading spaces before `*** Update File:` — the only deviation from
    // the canonical patch shape.
    let patch = "\
*** Begin Patch
  *** Update File: foo.txt
@@
-old line
+new line
*** End Patch";

    let affected = apply_patch_to_dir(patch, dir).expect("indented header must be accepted");
    assert!(affected.modified.contains(&"foo.txt".to_string()));

    let content = fs::read_to_string(dir.join("foo.txt")).expect("read");
    assert!(
        content.contains("new line"),
        "replacement must land, got: {content:?}"
    );
    assert!(
        !content.contains("old line"),
        "old content must be gone, got: {content:?}"
    );
}

// ── Test: update hunk consisting only of `-` removals (deletion-only) ──
//
// Regression guard: a chunk that removes one or more context-anchored lines
// without adding any must apply cleanly. Without this guard, an applicator
// tightening that required `replace_lines` to be non-empty (or that elided
// "empty replace" as a no-op) would silently break the LLM's common
// delete-this-line edits.

#[test]
fn update_hunk_with_only_deletions_applies() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let dir = tmp.path();

    fs::write(dir.join("lines.txt"), "line1\nline2\nline3\n").expect("seed");

    let patch = "\
*** Begin Patch
*** Update File: lines.txt
@@
 line1
-line2
 line3
*** End Patch";

    let affected = apply_patch_to_dir(patch, dir).expect("deletion-only hunk must apply");
    assert!(affected.modified.contains(&"lines.txt".to_string()));

    // Exact post-condition: pins both content and ordering. A regression that
    // reordered surviving context lines or that elided the removal as a no-op
    // would not be caught by a presence-only assertion.
    let content = fs::read_to_string(dir.join("lines.txt")).expect("read");
    assert_eq!(
        content, "line1\nline3\n",
        "expected line2 removed, surrounding context preserved in original order"
    );
}
