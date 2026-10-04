//! `phone_call` tool: place, follow and end outbound calls through the
//! configured telephony provider.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::telephony::PhoneDialer;

/// What the model asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PhoneCallAction {
    Call,
    Status,
    HangUp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PhoneCallArgs {
    pub action: PhoneCallAction,
    /// Callee in E.164 (`call`).
    #[serde(default)]
    pub to: Option<String>,
    /// What to say, or the voice agent's task (`call`).
    #[serde(default)]
    pub message: Option<String>,
    /// Id returned by `call` (`status`, `hang_up`).
    #[serde(default)]
    pub call_id: Option<String>,
}

/// Run one action. Provider and validation failures come back as text the
/// model can act on, not as tool errors, matching the other media tools.
pub(crate) async fn handle(args: &PhoneCallArgs, config: &Config) -> Result<String> {
    let dialer = match PhoneDialer::from_config(config) {
        Ok(dialer) => dialer,
        Err(e) => return Ok(e.to_string()),
    };
    let outcome = match args.action {
        PhoneCallAction::Call => {
            let (Some(to), Some(message)) = (args.to.as_deref(), args.message.as_deref()) else {
                return Ok("`call` needs `to` (E.164 number) and `message`.".into());
            };
            dialer.call(to, message).await.map(|status| {
                format!(
                    "{} via {}. Check progress with action=status, call_id={}.",
                    status.render(),
                    dialer.provider_label(),
                    status.id
                )
            })
        }
        PhoneCallAction::Status => match args.call_id.as_deref() {
            Some(id) => dialer.status(id).await.map(|s| s.render()),
            None => return Ok("`status` needs `call_id`.".into()),
        },
        PhoneCallAction::HangUp => match args.call_id.as_deref() {
            Some(id) => dialer
                .hang_up(id)
                .await
                .map(|()| format!("Call {id} ended.")),
            None => return Ok("`hang_up` needs `call_id`.".into()),
        },
    };
    Ok(outcome.unwrap_or_else(|e| format!("Phone call failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(json: serde_json::Value) -> PhoneCallArgs {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn hang_up_action_uses_snake_case() {
        assert_eq!(
            args(serde_json::json!({"action": "hang_up", "call_id": "c"})).action,
            PhoneCallAction::HangUp
        );
    }

    #[tokio::test]
    async fn off_config_answers_with_the_next_step() {
        let out = handle(
            &args(serde_json::json!({"action": "call", "to": "+14155550123", "message": "hi"})),
            &Config::default(),
        )
        .await
        .unwrap();
        assert!(out.contains("Phone calls are off"), "{out}");
    }

    #[tokio::test]
    async fn missing_fields_are_named_before_any_network_call() {
        let mut config = Config::default();
        config.telephony.provider = "bland".into();
        config.credentials.insert(
            "BLAND_API_KEY".into(),
            crate::config::CredentialValue::Ref(
                crate::security::secrets_resolve::SecretRef::Literal { value: "k".into() },
            ),
        );
        let cases = [
            (
                serde_json::json!({"action": "call", "to": "+14155550123"}),
                "`call` needs",
            ),
            (
                serde_json::json!({"action": "status"}),
                "`status` needs `call_id`",
            ),
            (
                serde_json::json!({"action": "hang_up"}),
                "`hang_up` needs `call_id`",
            ),
            (
                serde_json::json!({"action": "call", "to": "555-0123", "message": "hi"}),
                "E.164",
            ),
        ];
        for (json, want) in cases {
            let out = handle(&args(json.clone()), &config).await.unwrap();
            assert!(out.contains(want), "{json}: {out}");
        }
    }

    #[tokio::test]
    async fn number_outside_allowlist_is_refused() {
        let mut config = Config::default();
        config.telephony.provider = "bland".into();
        config.telephony.allowed_numbers = "+44".into();
        config.credentials.insert(
            "BLAND_API_KEY".into(),
            crate::config::CredentialValue::Ref(
                crate::security::secrets_resolve::SecretRef::Literal { value: "k".into() },
            ),
        );
        let out = handle(
            &args(serde_json::json!({"action": "call", "to": "+14155550123", "message": "hi"})),
            &config,
        )
        .await
        .unwrap();
        assert!(out.contains("not in the allowed numbers"), "{out}");
    }
}
