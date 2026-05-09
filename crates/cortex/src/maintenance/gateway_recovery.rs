//! Gateway abandonment recovery probe.
//!
//! The gateway respawn supervisor in `borgd` writes a meta flag when it
//! gives up after `GATEWAY_MAX_CRASH_RESPAWNS`. This module's probe runs
//! during the daily sweep — if the listen address now answers, the flag
//! is cleared so heartbeats stop nagging.

use anyhow::Result;

use crate::config::Config;
use crate::db::Database;

/// Meta key set by the gateway respawn supervisor when it abandons after
/// `GATEWAY_MAX_CRASH_RESPAWNS`. Mirrors the constants used by the
/// heartbeat augmenter — kept here as a string literal so cortex doesn't
/// need to depend on borgd.
const GATEWAY_UNRECOVERABLE_KEY: &str = "gateway_unrecoverable";
const GATEWAY_UNRECOVERABLE_AT_KEY: &str = "gateway_unrecoverable_at";

/// Probe the configured gateway listen address. If it's reachable, the
/// gateway must have come back (daemon restart, port freed, upstream
/// crash cleared) — clear the abandonment flag so the heartbeat
/// augmenter stops nudging. Leaves the flag intact when the probe
/// fails.
///
/// Note: this does NOT re-spawn the supervisor task. That requires
/// orchestrator/runtime handles we don't have in sync maintenance code,
/// and a clean daemon restart is the deterministic recovery path.
/// What this step *does* solve is the case where the user fixed the
/// underlying problem (port conflict, OOM, etc.) and restarted —
/// without it, the augmenter would keep nagging until someone
/// hand-cleared the meta key.
pub fn recover_gateway_if_abandoned(db: &Database, config: &Config) -> Result<()> {
    if db.get_meta(GATEWAY_UNRECOVERABLE_KEY)?.as_deref() != Some("1") {
        return Ok(());
    }
    let addr = format!("{}:{}", config.gateway.host, config.gateway.port);
    let socket_addr: std::net::SocketAddr = match addr.parse() {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("recover_gateway_if_abandoned: bad listen addr {addr}: {e}");
            return Ok(());
        }
    };
    let probe =
        std::net::TcpStream::connect_timeout(&socket_addr, std::time::Duration::from_millis(500));
    match probe {
        Ok(_) => {
            tracing::info!("gateway probe at {addr} succeeded — clearing unrecoverable flag");
            // Delete rather than set to "0" — the augmenter only checks
            // for "1", and a leftover sentinel pollutes DB exports.
            db.delete_meta(GATEWAY_UNRECOVERABLE_KEY)?;
            db.delete_meta(GATEWAY_UNRECOVERABLE_AT_KEY)?;
        }
        Err(e) => {
            tracing::debug!("gateway probe at {addr} still failing: {e}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recover_gateway_silent_when_flag_not_set() {
        let db = Database::test_db();
        let cfg = Config::default();
        // Flag absent → no-op, no error.
        recover_gateway_if_abandoned(&db, &cfg).expect("noop");
        assert!(db.get_meta(GATEWAY_UNRECOVERABLE_KEY).unwrap().is_none());
    }

    #[test]
    fn recover_gateway_leaves_flag_when_probe_fails() {
        let db = Database::test_db();
        let mut cfg = Config::default();
        // Point at a port that nobody could possibly be listening on.
        cfg.gateway.host = "127.0.0.1".into();
        cfg.gateway.port = 1; // privileged + universally closed in tests
        db.set_meta(GATEWAY_UNRECOVERABLE_KEY, "1").unwrap();
        db.set_meta(GATEWAY_UNRECOVERABLE_AT_KEY, "1234567890")
            .unwrap();
        recover_gateway_if_abandoned(&db, &cfg).expect("probe");
        // Flag must remain — no false recovery.
        assert_eq!(
            db.get_meta(GATEWAY_UNRECOVERABLE_KEY).unwrap().as_deref(),
            Some("1"),
            "failed probe must leave the flag set"
        );
    }

    #[test]
    fn recover_gateway_clears_flag_when_probe_succeeds() {
        // Bind a temporary listener on an OS-chosen port, then point the
        // config at it. The probe succeeds → flag clears. This exercises
        // the actual TCP-connect path rather than mocking it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let db = Database::test_db();
        let mut cfg = Config::default();
        cfg.gateway.host = addr.ip().to_string();
        cfg.gateway.port = addr.port();
        db.set_meta(GATEWAY_UNRECOVERABLE_KEY, "1").unwrap();
        db.set_meta(GATEWAY_UNRECOVERABLE_AT_KEY, "1234567890")
            .unwrap();

        recover_gateway_if_abandoned(&db, &cfg).expect("probe");

        assert!(
            db.get_meta(GATEWAY_UNRECOVERABLE_KEY).unwrap().is_none(),
            "successful probe must delete the flag row"
        );
        assert!(
            db.get_meta(GATEWAY_UNRECOVERABLE_AT_KEY).unwrap().is_none(),
            "successful probe must delete the timestamp row"
        );
    }
}
