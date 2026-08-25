use std::fmt;

/// Result of probing iMessage / chat.db availability.
pub struct ProbeResult {
    pub status: ProbeStatus,
    pub max_rowid: Option<i64>,
}

pub enum ProbeStatus {
    /// chat.db is accessible and queryable.
    Ok,
    /// chat.db does not exist (not on macOS or Messages never opened).
    NoDb,
    /// chat.db exists but cannot be read (Full Disk Access not granted).
    NoDiskAccess,
    /// chat.db opened but query failed.
    QueryError(String),
}

impl fmt::Display for ProbeStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok => write!(f, "ok"),
            Self::NoDb => write!(f, "no chat.db found"),
            Self::NoDiskAccess => write!(
                f,
                "Full Disk Access required (System Settings > Privacy & Security)"
            ),
            Self::QueryError(e) => write!(f, "query error: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ProbeStatus::Display` is the user-facing surface for
    /// `borg status` / onboarding diagnostics. Two invariants must hold:
    /// `NoDiskAccess` MUST mention "Full Disk Access" verbatim (operators
    /// search for that string when troubleshooting macOS Privacy
    /// settings), and `QueryError` MUST propagate the inner error message
    /// so the user can act on it.
    #[test]
    fn probe_status_display_carries_actionable_signal() {
        assert_eq!(ProbeStatus::Ok.to_string(), "ok");
        assert_eq!(ProbeStatus::NoDb.to_string(), "no chat.db found");

        let no_access = ProbeStatus::NoDiskAccess.to_string();
        assert!(
            no_access.contains("Full Disk Access"),
            "NoDiskAccess must name the macOS setting verbatim, got {no_access:?}"
        );

        let err = ProbeStatus::QueryError("table missing".to_string()).to_string();
        assert!(err.contains("query error"), "got {err:?}");
        assert!(
            err.contains("table missing"),
            "QueryError must propagate the inner error message, got {err:?}"
        );
    }

    /// Restores `HOME` on drop so a panicking test can't leak the override.
    struct HomeOverride {
        prev: Option<std::ffi::OsString>,
    }

    impl HomeOverride {
        fn set(path: &std::path::Path) -> Self {
            let prev = std::env::var_os("HOME");
            std::env::set_var("HOME", path);
            Self { prev }
        }
    }

    impl Drop for HomeOverride {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[tokio::test]
    async fn probe_reports_no_db_when_chat_db_absent() {
        let _guard = crate::test_env::borg_home_guard();
        let tmp = tempfile::tempdir().unwrap();
        let _home = HomeOverride::set(tmp.path());

        let result = probe_imessage().await;
        // The missing-file check must short-circuit BEFORE opening: libsql
        // creates the file on open, so skipping the check would fabricate an
        // empty chat.db and misreport a query error instead of NoDb.
        assert!(
            matches!(result.status, ProbeStatus::NoDb),
            "expected NoDb, got: {}",
            result.status
        );
        assert!(result.max_rowid.is_none());
        assert!(
            !tmp.path().join("Library/Messages/chat.db").exists(),
            "probe must never create chat.db"
        );
    }

    #[tokio::test]
    async fn probe_reports_ok_and_max_rowid_for_readable_chat_db() {
        let _guard = crate::test_env::borg_home_guard();
        let tmp = tempfile::tempdir().unwrap();
        let messages_dir = tmp.path().join("Library/Messages");
        std::fs::create_dir_all(&messages_dir).unwrap();
        let db_path = messages_dir.join("chat.db");
        {
            let db = libsql::Builder::new_local(&db_path).build().await.unwrap();
            let conn = db.connect().unwrap();
            conn.execute(
                "CREATE TABLE message (ROWID INTEGER PRIMARY KEY, text TEXT)",
                (),
            )
            .await
            .unwrap();
            conn.execute("INSERT INTO message (ROWID, text) VALUES (7, 'hi')", ())
                .await
                .unwrap();
        }
        let _home = HomeOverride::set(tmp.path());

        let result = probe_imessage().await;
        assert!(
            matches!(result.status, ProbeStatus::Ok),
            "expected Ok, got: {}",
            result.status
        );
        assert_eq!(result.max_rowid, Some(7));
    }
}

/// Probe whether chat.db is accessible and return status + max ROWID.
///
/// Opens chat.db via libsql. libsql has no read-only open flag — we rely on
/// only issuing SELECT statements to enforce the read-only contract at the
/// application boundary (per the migration plan).
pub async fn probe_imessage() -> ProbeResult {
    let db_path = match dirs::home_dir() {
        Some(h) => h.join("Library/Messages/chat.db"),
        None => {
            return ProbeResult {
                status: ProbeStatus::NoDb,
                max_rowid: None,
            }
        }
    };

    if !db_path.exists() {
        return ProbeResult {
            status: ProbeStatus::NoDb,
            max_rowid: None,
        };
    }

    let db = match libsql::Builder::new_local(&db_path).build().await {
        Ok(db) => db,
        Err(_) => {
            return ProbeResult {
                status: ProbeStatus::NoDiskAccess,
                max_rowid: None,
            }
        }
    };
    let conn = match db.connect() {
        Ok(c) => c,
        Err(_) => {
            return ProbeResult {
                status: ProbeStatus::NoDiskAccess,
                max_rowid: None,
            }
        }
    };

    let mut rows = match conn.query("SELECT MAX(ROWID) FROM message", ()).await {
        Ok(r) => r,
        Err(e) => {
            return ProbeResult {
                status: ProbeStatus::QueryError(e.to_string()),
                max_rowid: None,
            }
        }
    };
    match rows.next().await {
        Ok(Some(row)) => match row.get::<Option<i64>>(0) {
            Ok(max_rowid) => ProbeResult {
                status: ProbeStatus::Ok,
                max_rowid: max_rowid.or(Some(0)),
            },
            Err(e) => ProbeResult {
                status: ProbeStatus::QueryError(e.to_string()),
                max_rowid: None,
            },
        },
        // No rows back from MAX() should never happen, but if it does treat
        // it as an empty DB rather than an error so the operator sees Ok.
        Ok(None) => ProbeResult {
            status: ProbeStatus::Ok,
            max_rowid: Some(0),
        },
        Err(e) => ProbeResult {
            status: ProbeStatus::QueryError(e.to_string()),
            max_rowid: None,
        },
    }
}
