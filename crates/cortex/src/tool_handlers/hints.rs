//! Failure hints for `run_shell` results.
//!
//! Mirrors the table-driven `ACTION_HINTS` pattern in
//! [`crate::error_format`]: a small ordered table of known failure shapes is
//! scanned against the head of a command's combined output (plus its exit
//! code), and at most one short `hint:` line is attached to the tool result.
//! Pure function, no I/O — the goal is to save the model a diagnostic
//! round-trip on the highest-frequency shell failures.

/// How much of the combined output is scanned for failure shapes. Failure
/// banners (`command not found`, `ModuleNotFoundError`, `error[E…]`) appear
/// early; scanning the whole buffer would just cost time on floods.
const SCAN_BYTES: usize = 4096;

/// How a failure shape is recognized.
enum Shape {
    /// Any of these substrings appears in the scanned output head.
    OutputAny(&'static [&'static str]),
    /// The command exited with exactly this code.
    Exit(i32),
    /// A `rg`/`grep` invocation exited 1 — the "zero matches" code, which
    /// models routinely misread as the tool failing.
    GrepNoMatches,
}

/// One failure shape → hint row. First matching row wins.
struct ShellHint {
    shape: Shape,
    hint: &'static str,
}

/// Ordered high-frequency failure shapes. Append rows to extend; order is
/// significance order because only the first match is attached.
const SHELL_HINTS: &[ShellHint] = &[
    ShellHint {
        shape: Shape::OutputAny(&["command not found", ": not found"]),
        hint: "the executable was not found — check that it is installed and on PATH \
               (`which <cmd>`), or install it first.",
    },
    ShellHint {
        shape: Shape::OutputAny(&["ModuleNotFoundError", "ImportError"]),
        hint: "a Python module is missing — install it into the environment actually \
               running the script (check `which python` / the active venv) rather than \
               the system interpreter.",
    },
    ShellHint {
        shape: Shape::OutputAny(&["CONFLICT (", "Automatic merge failed", "<<<<<<<"]),
        hint: "git merge conflict — resolve the conflicted files and commit, or back out \
               with `git merge --abort` / `git rebase --abort`.",
    },
    ShellHint {
        shape: Shape::Exit(124),
        hint: "exit 124 usually means the command hit a timeout — raise timeout_ms, or \
               run it as a background session (action=start) and poll.",
    },
    ShellHint {
        shape: Shape::Exit(126),
        hint: "exit 126: the file was found but is not executable — `chmod +x` it or \
               invoke it through its interpreter (`sh script.sh`, `python script.py`).",
    },
    ShellHint {
        shape: Shape::Exit(137),
        hint: "exit 137: killed by SIGKILL, often the OOM killer — retry with a smaller \
               scope (fewer files, less parallelism) or more memory headroom.",
    },
    ShellHint {
        shape: Shape::OutputAny(&["Permission denied", "permission denied"]),
        hint: "permission denied — check file ownership/permissions; sandboxed or \
               blocked paths cannot be accessed from here.",
    },
    ShellHint {
        shape: Shape::OutputAny(&["error[E"]),
        hint: "rustc compile error — fix the first error[E…] first; later errors are \
               often cascades from it.",
    },
    ShellHint {
        shape: Shape::GrepNoMatches,
        hint: "exit 1 from rg/grep means zero matches, not a failure — before concluding \
               the pattern is absent, try case-insensitive (-i) or include gitignored \
               files (--no-ignore).",
    },
];

/// Banners no build or test tool prints unless something actually failed.
///
/// Deliberately tool-specific rather than a bare "error" search: this list is
/// only consulted for commands that exited 0, where the whole question is
/// whether the visible output contradicts a clean status. A generic needle
/// would fire on any output that merely quotes an error.
const FAILURE_BANNERS: &[&str] = &[
    "error[E",                           // rustc
    "error: could not compile",          // cargo
    "error: aborting due to",            // rustc summary
    "test result: FAILED",               // cargo test / nextest
    "Traceback (most recent call last)", // python
    "short test summary info",           // pytest
    "npm ERR!",                          // npm
    "BUILD FAILED",                      // gradle / xcodebuild
    "compilation terminated.",           // gcc / clang
    "FAILED: ",                          // ninja
    "make: ***",                         // make
    "]: ***",                            // make[N]: ***
];

/// Heads whose output legitimately *contains* failure banners: a search or a
/// file read that turns up the word `error[E` has not failed. Excluded so the
/// masked-success check never fires on `rg 'error\[E' … | head`.
const CONTENT_HEADS: &[&str] = &[
    "grep",
    "rg",
    "ag",
    "find",
    "ls",
    "cat",
    "head",
    "tail",
    "jq",
    "awk",
    "sed",
    "strings",
    "journalctl",
    "dmesg",
    "echo",
    "printf",
];

/// Return the hint for the first failure shape matching this command's
/// outcome, or `None` for clean or unrecognized results. `output` is the
/// combined stdout+stderr; only its first [`SCAN_BYTES`] are scanned.
///
/// Exit 0 earns a hint in exactly one case — see [`masked_success_hint`].
pub(crate) fn shell_failure_hint(
    command: &str,
    exit_code: Option<i32>,
    output: &str,
) -> Option<std::borrow::Cow<'static, str>> {
    if exit_code == Some(0) {
        // A clean exit normally earns no hint, even if the output happens to
        // quote a failure-shaped string (`echo "Permission denied"`) — the
        // one exception is a status the shell never took from the real work.
        return masked_success_hint(command, output).map(std::borrow::Cow::Owned);
    }
    let head = output_head(output);
    SHELL_HINTS
        .iter()
        .find(|row| match row.shape {
            Shape::OutputAny(needles) => needles.iter().any(|n| head.contains(n)),
            Shape::Exit(code) => exit_code == Some(code),
            Shape::GrepNoMatches => exit_code == Some(1) && last_segment_head_is_grep(command),
        })
        .map(|row| std::borrow::Cow::Borrowed(row.hint))
}

/// Hint for an exit 0 that the shell took from something other than the work.
///
/// `cargo test 2>&1 | tee log` exits with `tee`'s 0 however the tests went, and
/// the model reads exit 0 as a strong success signal — so a failed build can be
/// reported as a passing one. Both halves must hold before this fires: the
/// command shape must be able to mask a status *and* the output must carry a
/// tool-specific failure banner. The exit code itself is never rewritten; this
/// only tells the model not to trust it.
fn masked_success_hint(command: &str, output: &str) -> Option<String> {
    let shape = crate::policy::masked_exit_shape(command)?;
    if first_head_is_content_producer(command) {
        return None;
    }
    let head = output_head(output);
    if !FAILURE_BANNERS.iter().any(|banner| head.contains(banner)) {
        return None;
    }
    Some(format!(
        "exit 0 is not evidence of success here — {}, and the output carries failure \
         banners. Treat this run as FAILED and re-run the command bare to get its real \
         exit code.",
        shape.describe()
    ))
}

/// Whether the command *starts* with a search or file-read tool, whose output
/// naturally quotes failure banners it did not produce.
fn first_head_is_content_producer(command: &str) -> bool {
    crate::policy::pipeline_segments(command)
        .first()
        .and_then(|segment| segment.split_whitespace().next())
        .is_some_and(|head| CONTENT_HEADS.contains(&head_token_base(head)))
}

/// First [`SCAN_BYTES`] of the output, cut back to a char boundary.
pub(crate) fn output_head(output: &str) -> &str {
    &output[..crate::util::truncate::floor_char_boundary(output, SCAN_BYTES)]
}

/// Whether the *last* pipeline segment's head token is an rg/grep binary —
/// the last segment is the one whose exit code a shell pipeline reports.
fn last_segment_head_is_grep(command: &str) -> bool {
    const GREPS: &[&str] = &["rg", "grep", "egrep", "fgrep"];
    let segments = crate::policy::pipeline_segments(command);
    let Some(segment) = segments.last() else {
        return false;
    };
    let Some(head) = segment.split_whitespace().next() else {
        return false;
    };
    GREPS.contains(&head_token_base(head))
}

/// Basename of a command token (`/usr/bin/grep` → `grep`), shared by the
/// hint and env-file detectors so both classify heads identically.
pub(crate) fn head_token_base(head: &str) -> &str {
    std::path::Path::new(head)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Table-driven spec of the hint contract: each known failure shape maps
    /// to its hint (identified by a distinctive fragment), clean or
    /// unrecognized results map to none, and the first matching row wins.
    #[test]
    fn hint_table_maps_failure_shapes_and_stays_silent_on_clean_output() {
        // (command, exit_code, output, expected hint fragment)
        let cases: &[(&str, Option<i32>, &str, Option<&str>)] = &[
            // Clean output: never a hint, even with failure-shaped text.
            ("echo ok", Some(0), "ok\n", None),
            ("echo denied", Some(0), "Permission denied\n", None),
            // Unrecognized failure: no hint rather than a wrong one.
            ("false", Some(1), "", None),
            // Output shapes.
            (
                "frobnicate",
                Some(127),
                "sh: frobnicate: command not found\n",
                Some("PATH"),
            ),
            ("sh x", Some(127), "sh: 1: foo: not found\n", Some("PATH")),
            (
                "python x.py",
                Some(1),
                "Traceback...\nModuleNotFoundError: No module named 'requests'\n",
                Some("venv"),
            ),
            (
                "python x.py",
                Some(1),
                "ImportError: cannot import name 'x'\n",
                Some("venv"),
            ),
            (
                "git merge feature",
                Some(1),
                "CONFLICT (content): Merge conflict in a.rs\nAutomatic merge failed\n",
                Some("git merge --abort"),
            ),
            (
                "cat /etc/sudoers",
                Some(1),
                "cat: /etc/sudoers: Permission denied\n",
                Some("ownership"),
            ),
            (
                "cargo build",
                Some(101),
                "error[E0308]: mismatched types\n",
                Some("error[E"),
            ),
            // Exit-code shapes (empty output).
            ("timeout 5 slowcmd", Some(124), "", Some("timeout_ms")),
            ("./script.sh", Some(126), "", Some("chmod +x")),
            ("cargo test -j32", Some(137), "", Some("SIGKILL")),
            // rg/grep zero matches: only for grep-shaped commands at exit 1.
            ("rg NEEDLE src/", Some(1), "", Some("--no-ignore")),
            ("grep -r NEEDLE .", Some(1), "", Some("--no-ignore")),
            ("cat f | grep NEEDLE", Some(1), "", Some("--no-ignore")),
            // grep succeeded upstream but tail command failed → not the grep hint.
            ("rg NEEDLE | head -1", Some(1), "", None),
            // exit 1 from a non-grep command: no grep hint.
            ("ls missing", Some(1), "", None),
            // exit 2 from grep is a real error (bad args), not zero matches.
            (
                "grep --bogus x",
                Some(2),
                "grep: unrecognized option\n",
                None,
            ),
        ];
        for (command, exit, output, expected) in cases {
            let hint = shell_failure_hint(command, *exit, output);
            match expected {
                None => assert_eq!(
                    hint, None,
                    "expected no hint for {command:?} (exit {exit:?}), got {hint:?}"
                ),
                Some(fragment) => {
                    let hint = hint.unwrap_or_else(|| {
                        panic!("expected a hint for {command:?} (exit {exit:?})")
                    });
                    assert!(
                        hint.contains(fragment),
                        "hint for {command:?} should mention {fragment:?}: {hint}"
                    );
                }
            }
        }
    }

    /// Only one hint is ever attached: when several shapes match, the earlier
    /// table row wins so the result doesn't accumulate hint noise.
    #[test]
    fn first_matching_row_wins() {
        // Both "command not found" (row 0) and error[E (later row) present.
        let out = "sh: cc: command not found\nerror[E9999]: fake\n";
        let hint = shell_failure_hint("cc x.c", Some(127), out).expect("hint");
        assert!(hint.contains("PATH"), "earlier row must win: {hint}");
    }

    /// A masked exit 0 only earns a hint when BOTH halves hold: the shell took
    /// its status from a passthrough consumer or a `||` fallback, AND the
    /// output carries a real build/test failure banner. Either half alone
    /// leaves the clean status alone.
    #[test]
    fn masked_exit_zero_needs_both_a_masking_shape_and_a_failure_banner() {
        const CARGO_FAIL: &str = "error[E0308]: mismatched types\nerror: could not compile `x`\n";
        // (command, output, should hint)
        let cases: &[(&str, &str, bool)] = &[
            // Both halves: the canonical `| tee` and `| tail` build runs.
            ("cargo test 2>&1 | tee /tmp/borg-test.log", CARGO_FAIL, true),
            ("cargo build | tail -20", CARGO_FAIL, true),
            ("cargo build || echo failed", CARGO_FAIL, true),
            (
                "pytest | tee out",
                "=== short test summary info ===\n",
                true,
            ),
            ("npm run build | tee out", "npm ERR! build failed\n", true),
            ("make | tee out", "make: *** [all] Error 2\n", true),
            // Masking shape but a genuinely clean build: no hint.
            ("cargo build | tee log", "Finished dev profile\n", false),
            ("cargo test || true", "test result: ok. 12 passed\n", false),
            // Failure banner but no masking shape: exit 0 is trustworthy.
            ("cargo build", CARGO_FAIL, false),
            ("cargo build && cargo test", CARGO_FAIL, false),
            // Pipe into a real program, not a passthrough consumer.
            ("cargo build | grep -q Finished", CARGO_FAIL, false),
            // A search whose OUTPUT quotes failure banners has not failed.
            ("rg 'error\\[E' src | head -20", CARGO_FAIL, false),
            ("cat build.log | tail -50", CARGO_FAIL, false),
        ];
        for (command, output, want_hint) in cases {
            let hint = shell_failure_hint(command, Some(0), output);
            assert_eq!(
                hint.is_some(),
                *want_hint,
                "masked-success verdict wrong for {command:?} with output {output:?}: {hint:?}"
            );
        }
    }

    /// The hint has to be actionable: it names what the exit code actually
    /// measured and tells the model to re-run bare, so the model does not just
    /// learn "something is off" and retry the same masked command.
    #[test]
    fn masked_success_hint_names_the_consumer_and_the_next_step() {
        let hint = shell_failure_hint(
            "cargo test 2>&1 | tee /tmp/borg-test.log",
            Some(0),
            "error: could not compile `cortex`\n",
        )
        .expect("masked exit 0 must earn a hint");
        assert!(hint.contains("tee"), "must name the consumer: {hint}");
        assert!(hint.contains("FAILED"), "must reverse the verdict: {hint}");
        assert!(hint.contains("bare"), "must name the next step: {hint}");
    }

    /// A real failure keeps its own diagnostic hint: the masked-success path
    /// must not shadow the table for non-zero exits.
    #[test]
    fn nonzero_exit_still_gets_its_table_hint_when_piped() {
        let hint = shell_failure_hint(
            "cargo build | tee log",
            Some(101),
            "error[E0308]: mismatched types\n",
        )
        .expect("hint");
        assert!(hint.contains("error[E"), "table hint expected: {hint}");
    }

    /// The scan is bounded: a failure banner buried past the scan window does
    /// not produce a hint (and a multibyte flood must not panic the slicer).
    #[test]
    fn scan_is_bounded_to_the_output_head() {
        let mut flood = "é".repeat(SCAN_BYTES); // 2 bytes each, > SCAN_BYTES
        flood.push_str("command not found");
        assert_eq!(shell_failure_hint("x", Some(127), &flood), None);
    }
}
