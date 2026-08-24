use anyhow::{Context, Result};
use tracing::debug;

use crate::message_processing::plain_text::strip_outer_code_fence;

/// Send a message via iMessage using osascript (AppleScript).
///
/// `to` is the recipient identifier (phone number or email).
/// `service` is typically "iMessage" or "SMS".
pub async fn send_imessage(to: &str, text: &str, service: &str) -> Result<()> {
    // iMessage renders plain text only — strip any outer markdown code fence so
    // callers that wrap preformatted output (e.g. /xp, /card, /evolution) don't
    // leak literal backticks into the message body.
    let text = strip_outer_code_fence(text);

    // Escape text for AppleScript string literal
    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");

    let script = format!(
        r#"tell application "Messages"
    set targetService to 1st account whose service type = {service_type}
    set targetBuddy to participant "{to}" of targetService
    send "{escaped}" to targetBuddy
end tell"#,
        service_type = applescript_service_type(service),
        to = to,
        escaped = escaped,
    );

    debug!("Sending iMessage to {to} via {service}");

    let output = tokio::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .await
        .context("Failed to execute osascript")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("osascript failed: {stderr}");
    }

    debug!("Message sent to {to}");
    Ok(())
}

fn applescript_service_type(service: &str) -> &str {
    match service.to_lowercase().as_str() {
        "sms" => "SMS",
        _ => "iMessage",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_type_mapping() {
        assert_eq!(applescript_service_type("iMessage"), "iMessage");
        assert_eq!(applescript_service_type("SMS"), "SMS");
        assert_eq!(applescript_service_type("sms"), "SMS");
        assert_eq!(applescript_service_type("unknown"), "iMessage");
    }

    use std::os::unix::fs::PermissionsExt;

    /// Shadows `osascript` with a fake shell script prepended to `PATH` so no
    /// real AppleScript (and no real iMessage send) ever runs. Restores the
    /// prior `PATH` on drop. Callers must hold `test_env::borg_home_guard()`
    /// for the test body — `PATH` is process-global.
    struct OsascriptShim {
        tmp: tempfile::TempDir,
        prev_path: Option<std::ffi::OsString>,
    }

    impl OsascriptShim {
        fn install(body: &str) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let log = tmp.path().join("osascript.log");
            let script = format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n{body}\n",
                log.display()
            );
            let bin = tmp.path().join("osascript");
            std::fs::write(&bin, script).unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            let prev_path = std::env::var_os("PATH");
            let mut path = tmp.path().as_os_str().to_os_string();
            path.push(":");
            path.push(prev_path.clone().unwrap_or_default());
            std::env::set_var("PATH", &path);
            Self { tmp, prev_path }
        }

        fn logged_args(&self) -> String {
            std::fs::read_to_string(self.tmp.path().join("osascript.log")).unwrap_or_default()
        }
    }

    impl Drop for OsascriptShim {
        fn drop(&mut self) {
            match &self.prev_path {
                Some(v) => std::env::set_var("PATH", v),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    #[tokio::test]
    async fn send_invokes_osascript_with_recipient_and_text() {
        let _guard = crate::test_env::borg_home_guard();
        let shim = OsascriptShim::install("exit 0");

        send_imessage("+15551234567", "hello world", "iMessage")
            .await
            .expect("send must succeed when osascript exits 0");

        // The AppleScript must actually be handed to osascript with the
        // recipient, body, and service baked in — a send that reports Ok
        // without invoking osascript never delivered anything.
        let args = shim.logged_args();
        assert!(
            args.contains(r#"participant "+15551234567""#),
            "script must target the recipient, got: {args}"
        );
        assert!(
            args.contains(r#"send "hello world""#),
            "script must send the text, got: {args}"
        );
        assert!(
            args.contains("service type = iMessage"),
            "script must select the iMessage service, got: {args}"
        );
    }

    #[tokio::test]
    async fn send_surfaces_osascript_failure_with_stderr() {
        let _guard = crate::test_env::borg_home_guard();
        let _shim = OsascriptShim::install("echo 'not authorised' >&2\nexit 1");

        let err = send_imessage("+15551234567", "hello", "iMessage")
            .await
            .expect_err("non-zero osascript exit must be an error");
        let msg = err.to_string();
        assert!(msg.contains("osascript failed"), "got: {msg}");
        assert!(
            msg.contains("not authorised"),
            "stderr must be propagated so the operator can act, got: {msg}"
        );
    }
}
