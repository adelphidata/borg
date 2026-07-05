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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::CheckStatus;

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
