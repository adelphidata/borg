/// Cache for detecting and filtering echo (self-sent) messages.
pub mod echo_cache;
/// Background monitor that polls chat.db for new messages.
pub mod monitor;
/// Probe utility to check iMessage availability.
pub mod probe;
/// Guard against reflection loops from self-chat messages.
pub mod reflection_guard;
/// Input sanitization for iMessage content.
pub mod sanitize;
/// Cache for tracking self-chat conversation state.
pub mod self_chat_cache;
/// AppleScript-based message sending.
pub mod send;
/// iMessage type definitions.
pub mod types;

use anyhow::Result;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use cortex::config::Config;

/// Start the native iMessage monitor. Reads `~/Library/Messages/chat.db`
/// directly via libsql, processes inbound messages through echo detection
/// and reflection guards, dispatches to the agent, and sends replies via
/// osascript.
///
/// Returns a `JoinHandle` for the monitor task. The handle is detached, so a
/// panic inside the loop is not observable through it — supervised callers
/// want [`run_imessage_monitor`].
pub async fn start_imessage_monitor(
    config: Config,
    shutdown: CancellationToken,
) -> Result<JoinHandle<()>> {
    monitor::spawn_monitor(config, shutdown)
}

/// Run the iMessage monitor in the caller's task, resolving only when the
/// monitor stops. This is the form a supervisor must await: it surfaces
/// errors and panics for restart, where [`start_imessage_monitor`] would
/// return `Ok` immediately and end supervision at boot.
pub async fn run_imessage_monitor(config: Config, shutdown: CancellationToken) -> Result<()> {
    monitor::run_monitor(config, shutdown).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The supervised entry point must surface a startup failure as `Err` —
    /// swallowing it into `Ok(())` reads to the supervisor as a clean exit and
    /// ends supervision at boot while iMessage is silently dead.
    #[tokio::test]
    async fn run_imessage_monitor_surfaces_startup_failure() {
        let _guard = crate::test_env::borg_home_guard();
        let tmp = tempfile::tempdir().unwrap();
        let fake_home = tmp.path().join("home");
        std::fs::create_dir_all(&fake_home).unwrap();

        let old_home = std::env::var_os("HOME");
        let old_borg_home = std::env::var_os("BORG_HOME");
        // A home with no Library/Messages/chat.db makes startup fail
        // deterministically without touching the real Messages database.
        std::env::set_var("HOME", &fake_home);
        std::env::set_var("BORG_HOME", tmp.path());

        let result = run_imessage_monitor(Config::default(), CancellationToken::new()).await;

        match old_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match old_borg_home {
            Some(v) => std::env::set_var("BORG_HOME", v),
            None => std::env::remove_var("BORG_HOME"),
        }

        let err = result.expect_err("monitor startup without chat.db must be an error");
        assert!(
            err.to_string().contains("chat.db"),
            "error must name the missing chat.db, got: {err}"
        );
    }
}
