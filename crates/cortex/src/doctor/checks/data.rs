//! Data-directory and database health checks.

use anyhow::Result;

use crate::config::Config;
use crate::db::Database;
use crate::doctor::{DiagnosticCheck, RemediationKind};

pub(in crate::doctor) async fn check_data_dir(checks: &mut Vec<DiagnosticCheck>) {
    match Config::data_dir() {
        Ok(data_dir) => {
            if data_dir.exists() {
                let test_file = data_dir.join(".doctor_write_test");
                match std::fs::write(&test_file, "test") {
                    Ok(()) => {
                        let _ = std::fs::remove_file(&test_file);
                        checks.push(DiagnosticCheck::pass("Data", "data directory writable"));
                        // Writable today says nothing about tomorrow: the DB,
                        // its WAL, embeddings and logs all grow on their own.
                        check_disk_space(&data_dir, checks);
                    }
                    Err(e) => {
                        checks.push(DiagnosticCheck::fail(
                            "Data",
                            "data directory writable",
                            format!("{e}"),
                        ));
                    }
                }
            } else {
                checks.push(
                    DiagnosticCheck::warn(
                        "Data",
                        "data directory exists",
                        "~/.borg not found — run `/heal` or `borg init`",
                    )
                    .with_remediation(
                        RemediationKind::AutoSafe,
                        "data.ensure_subdirs",
                        "run /heal to recreate the runtime subdirectories",
                    ),
                );
            }
        }
        Err(e) => {
            checks.push(DiagnosticCheck::fail(
                "Data",
                "data directory",
                format!("{e}"),
            ));
        }
    }

    checks.push(DiagnosticCheck::from_result(
        "Data",
        "database accessible",
        Database::open().await,
    ));

    // `PRAGMA integrity_check` — surfaces SQLite-level corruption so a
    // bad disk or aborted write stops silently rotting the DB under us.
    // A clean DB returns the single row `"ok"`. Anything else is a
    // concrete problem; we downgrade to Warn (not Fail) so the
    // persistent-warning pipeline in maintenance can escalate it only
    // after it appears on two consecutive runs.
    // `Err` branch already surfaced by the "database accessible" check
    // above — don't double-report. Only run integrity if we could open.
    if let Ok(db) = Database::open().await {
        let integrity_result: Result<String> = async {
            let mut rows = db.conn().query("PRAGMA integrity_check", ()).await?;
            match rows.next().await? {
                Some(row) => Ok(row.get::<String>(0)?),
                None => Ok(String::new()),
            }
        }
        .await;
        match integrity_result {
            Ok(result) if result == "ok" => {
                checks.push(DiagnosticCheck::pass("Data", "database integrity"));
            }
            Ok(result) => {
                checks.push(DiagnosticCheck::warn("Data", "database integrity", result));
            }
            Err(e) => {
                checks.push(DiagnosticCheck::fail(
                    "Data",
                    "database integrity",
                    format!("{e}"),
                ));
            }
        }
    }
}

/// Surface an automatic database recovery that happened earlier in this
/// process: the live DB was unreadable on open, so it was preserved aside and
/// rebuilt empty. Non-draining (`peek`) so the boot path's user-facing notice
/// still fires. Passes silently when no recovery occurred.
pub(in crate::doctor) fn check_db_recovery(checks: &mut Vec<DiagnosticCheck>) {
    checks.push(db_recovery_check(
        crate::db::Database::peek_recovery_notice(),
    ));
}

/// Pure mapping from a recovery notice to a diagnostic check, split out so the
/// warn/pass rendering is testable without poking the process-global.
fn db_recovery_check(notice: Option<crate::db::RecoveryNotice>) -> DiagnosticCheck {
    match notice {
        Some(notice) => DiagnosticCheck::warn(
            "Database",
            "Auto-recovered",
            format!(
                "database was unreadable and rebuilt empty; the original is preserved at {}",
                notice.backup_path.display()
            ),
        ),
        None => DiagnosticCheck::pass("Database", "Integrity"),
    }
}

/// Free space below this is a warning: the DB, its WAL, the embedding store and
/// the log directory all grow without asking, and a `VACUUM` transiently needs
/// room for a second copy of the database.
#[cfg(unix)]
const DISK_WARN_BYTES: u64 = 5 * 1024 * 1024 * 1024;
/// Below this the next write is expected to fail outright.
#[cfg(unix)]
const DISK_FAIL_BYTES: u64 = 1024 * 1024 * 1024;

/// Bytes available to an unprivileged user on the filesystem holding `path`.
///
/// Unix only. Reading this on Windows needs `GetDiskFreeSpaceExW`, and pulling
/// a Windows FFI crate into `cortex` for one diagnostic is not worth it — the
/// caller emits no row there rather than guessing, so the report never claims
/// a check it did not run.
#[cfg(unix)]
fn available_bytes(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c_path` is a valid NUL-terminated string that outlives the call,
    // and `stat` is only read after `statvfs` reports success.
    let stat = unsafe {
        let mut stat = std::mem::zeroed::<libc::statvfs>();
        if libc::statvfs(c_path.as_ptr(), &mut stat) != 0 {
            return None;
        }
        stat
    };
    // `f_bavail` (not `f_bfree`) is the unprivileged figure — the difference is
    // the reserved-blocks pool, which borg cannot write into.
    //
    // Widened with `as`, and the lints silenced, because these fields are not
    // the same width on every target: `f_bavail` is 32-bit on macOS and 64-bit
    // on Linux. Every spelling that satisfies one platform's lint trips the
    // other's (`unnecessary_fallible_conversions` here, `useless_conversion`
    // there), and the cast is lossless in both directions.
    #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
    let blocks = stat.f_bavail as u64;
    #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
    let block_size = stat.f_frsize as u64;
    blocks.checked_mul(block_size)
}

#[cfg(unix)]
fn describe_gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

/// Classify free space against the thresholds above. Split from the syscall so
/// the boundaries are testable without a full disk.
#[cfg(unix)]
fn classify_free_space(available: u64) -> DiagnosticCheck {
    let free = describe_gib(available);
    if available < DISK_FAIL_BYTES {
        DiagnosticCheck::fail(
            "Data",
            "disk space",
            format!(
                "only {free} free in ~/.borg — free up space; borg cannot write reliably below {}",
                describe_gib(DISK_FAIL_BYTES)
            ),
        )
    } else if available < DISK_WARN_BYTES {
        DiagnosticCheck::warn(
            "Data",
            "disk space",
            format!(
                "{free} free in ~/.borg — free up space soon; below {} maintenance and database growth start to fail",
                describe_gib(DISK_FAIL_BYTES)
            ),
        )
    } else {
        DiagnosticCheck::pass("Data", "disk space")
    }
}

/// Push a free-space row for the data directory, if it can be measured.
#[cfg_attr(not(unix), allow(unused_variables))]
fn check_disk_space(data_dir: &std::path::Path, checks: &mut Vec<DiagnosticCheck>) {
    #[cfg(unix)]
    if let Some(available) = available_bytes(data_dir) {
        checks.push(classify_free_space(available));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::CheckStatus;

    #[cfg(unix)]
    #[test]
    fn free_space_thresholds_escalate_from_pass_to_warn_to_fail() {
        // The point of the row is to reach the operator *before* the disk
        // fills: a DB that cannot extend its WAL loses the turn, and a
        // `VACUUM` needs room for a second copy of the file. The boundaries
        // are inclusive-below, so a value exactly on a threshold is the
        // gentler status.
        let gib = 1024 * 1024 * 1024;
        let cases: &[(u64, &str)] = &[
            (0, "fail"),
            (gib / 2, "fail"),
            (gib - 1, "fail"),
            (gib, "warn"),
            (3 * gib, "warn"),
            (5 * gib - 1, "warn"),
            (5 * gib, "pass"),
            (500 * gib, "pass"),
        ];
        for (available, want) in cases.iter().copied() {
            let check = classify_free_space(available);
            let got = match check.status {
                CheckStatus::Pass => "pass",
                CheckStatus::Warn(_) => "warn",
                CheckStatus::Fail(_) => "fail",
            };
            assert_eq!(got, want, "for {available} bytes free");
        }
    }

    #[cfg(unix)]
    #[test]
    fn low_space_messages_name_the_action_not_a_bare_number() {
        // Borg's UX invariant: an error says what to do next in plain words.
        for available in [0, 2 * 1024 * 1024 * 1024] {
            let check = classify_free_space(available);
            let message = match &check.status {
                CheckStatus::Warn(m) | CheckStatus::Fail(m) => m.clone(),
                other => panic!("expected a problem status, got {other:?}"),
            };
            assert!(
                message.contains("free up space"),
                "message must name the next action; got: {message}"
            );
            assert!(
                message.contains("GiB"),
                "message must quantify what is left; got: {message}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn available_bytes_reads_a_real_filesystem_and_rejects_a_missing_path() {
        // Guards the syscall wiring itself: a wrong `f_frsize`/`f_bavail`
        // pairing or a botched CString would either panic, return 0, or report
        // a nonsense figure, and the classifier above would then be grading
        // garbage. Any real filesystem the test can write to has some space.
        let temp = tempfile::tempdir().expect("tempdir");
        let available = available_bytes(temp.path()).expect("statvfs on a live temp dir");
        assert!(
            available > 0,
            "a writable temp dir must report some free space"
        );
        assert!(
            available_bytes(&temp.path().join("no-such-child")).is_none(),
            "a path that does not exist must report nothing, not zero"
        );
    }

    #[test]
    fn db_recovery_check_warns_with_backup_path_and_passes_otherwise() {
        // A recorded recovery surfaces as a Warn naming the preserved backup,
        // so the operator knows their DB was rebuilt and where the original is.
        let notice = crate::db::RecoveryNotice {
            backup_path: std::path::PathBuf::from("/home/u/.borg/borg.db.corrupt-42"),
            new_path: std::path::PathBuf::from("/home/u/.borg/borg.db"),
        };
        let warn = db_recovery_check(Some(notice));
        assert_eq!(warn.category, "Database");
        assert!(matches!(warn.status, CheckStatus::Warn(_)));
        match &warn.status {
            CheckStatus::Warn(msg) => assert!(msg.contains("borg.db.corrupt-42"), "got: {msg}"),
            other => panic!("expected warn, got {other:?}"),
        }
        // No recovery → a quiet pass, no spurious warning.
        let ok = db_recovery_check(None);
        assert!(matches!(ok.status, CheckStatus::Pass));
    }
}
