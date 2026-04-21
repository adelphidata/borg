use anyhow::{Context, Result};
use tracing::debug;

use crate::plain_text::strip_outer_code_fence;

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
}
