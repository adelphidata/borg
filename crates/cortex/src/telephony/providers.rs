//! Built-in phone providers and the registry that names them.
//!
//! Same shape as the speech and transcription registries: a
//! [`TelephonyProvider`] impl plus one [`TelephonyProviderDef`] row. The row
//! also says which of the shared settings the provider reads, so Settings
//! shows only the fields that matter for the provider picked.

use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde_json::{json, Value};

use super::{xml_escape, CallRequest, CallState, CallStatus, TelephonyProvider};
use crate::config::Config;
use crate::media::core::ensure_success;
use crate::media::provider_info::ProviderInfo;

// ── Twilio (plain call: speak the message, hang up) ──

struct TwilioProvider {
    client: Client,
    base_url: String,
    account_sid: String,
    auth_token: String,
    /// `TWILIO_PHONE_NUMBER`, used when `telephony.caller_id` is empty.
    default_caller_id: String,
}

impl TwilioProvider {
    fn calls_url(&self, suffix: &str) -> String {
        format!(
            "{}/2010-04-01/Accounts/{}/Calls{suffix}.json",
            self.base_url, self.account_sid
        )
    }
}

/// Twilio `status` words → shared state.
fn twilio_status(json: &Value) -> Result<CallStatus> {
    let id = json["sid"]
        .as_str()
        .context("Twilio response has no call sid")?;
    let word = json["status"].as_str().unwrap_or_default();
    let state = match word {
        "queued" | "initiated" => CallState::Queued,
        "ringing" => CallState::Ringing,
        "in-progress" => CallState::InProgress,
        "completed" => CallState::Ended,
        _ => CallState::Failed,
    };
    let mut status = CallStatus::new(id, state, word);
    // Twilio sends the duration as a string of seconds once the call ends.
    status.duration_secs = json["duration"].as_str().and_then(|d| d.parse().ok());
    Ok(status)
}

#[async_trait::async_trait]
impl TelephonyProvider for TwilioProvider {
    async fn place_call(&self, call: &CallRequest) -> Result<CallStatus> {
        let from = if call.caller_id.is_empty() {
            self.default_caller_id.as_str()
        } else {
            call.caller_id.as_str()
        };
        if from.is_empty() {
            bail!("Twilio needs the number calls come from. Set it under Settings → Advanced → Voice & phone.");
        }
        let voice = if call.voice.is_empty() {
            "Polly.Joanna"
        } else {
            &call.voice
        };
        let twiml = format!(
            "<Response><Say voice=\"{}\">{}</Say></Response>",
            xml_escape(voice),
            xml_escape(&call.message)
        );
        let resp = self
            .client
            .post(self.calls_url(""))
            .basic_auth(&self.account_sid, Some(&self.auth_token))
            .form(&[
                ("To", call.to.as_str()),
                ("From", from),
                ("Twiml", twiml.as_str()),
                ("TimeLimit", &call.max_call_secs.to_string()),
            ])
            .send()
            .await?;
        twilio_status(&ensure_success(resp, "Twilio").await?.json().await?)
    }

    async fn call_status(&self, call_id: &str) -> Result<CallStatus> {
        let resp = self
            .client
            .get(self.calls_url(&format!("/{call_id}")))
            .basic_auth(&self.account_sid, Some(&self.auth_token))
            .send()
            .await?;
        twilio_status(&ensure_success(resp, "Twilio").await?.json().await?)
    }

    async fn hang_up(&self, call_id: &str) -> Result<()> {
        let resp = self
            .client
            .post(self.calls_url(&format!("/{call_id}")))
            .basic_auth(&self.account_sid, Some(&self.auth_token))
            .form(&[("Status", "completed")])
            .send()
            .await?;
        ensure_success(resp, "Twilio").await?;
        Ok(())
    }
}

// ── Vapi (voice agent) ──

struct VapiProvider {
    client: Client,
    base_url: String,
    api_key: String,
    /// `VAPI_PHONE_NUMBER_ID`, used when `telephony.caller_id` is empty.
    default_caller_id: String,
}

/// Vapi `status` + `endedReason` → shared state. An ended call whose reason
/// is a non-answer or an error never reached a conversation.
fn vapi_status(json: &Value) -> Result<CallStatus> {
    let id = json["id"]
        .as_str()
        .context("Vapi response has no call id")?;
    let word = json["status"].as_str().unwrap_or_default();
    let reason = json["endedReason"].as_str().unwrap_or_default();
    let state = match word {
        "scheduled" | "queued" => CallState::Queued,
        "ringing" => CallState::Ringing,
        "in-progress" | "forwarding" => CallState::InProgress,
        "ended"
            if ["did-not-answer", "busy", "error", "failed"]
                .iter()
                .any(|bad| reason.contains(bad)) =>
        {
            CallState::Failed
        }
        "ended" => CallState::Ended,
        _ => CallState::Queued,
    };
    let mut status = CallStatus::new(id, state, if reason.is_empty() { word } else { reason });
    status.transcript = json["transcript"]
        .as_str()
        .or_else(|| json["artifact"]["transcript"].as_str())
        .map(String::from);
    status.summary = json["summary"]
        .as_str()
        .or_else(|| json["analysis"]["summary"].as_str())
        .map(String::from);
    status.duration_secs = json["durationSeconds"].as_f64().map(|s| s.round() as u64);
    Ok(status)
}

#[async_trait::async_trait]
impl TelephonyProvider for VapiProvider {
    async fn place_call(&self, call: &CallRequest) -> Result<CallStatus> {
        let phone_number_id = if call.caller_id.is_empty() {
            self.default_caller_id.as_str()
        } else {
            call.caller_id.as_str()
        };
        if phone_number_id.is_empty() {
            bail!("Vapi needs the phone number id calls come from. Set it under Settings → Advanced → Voice & phone.");
        }
        let mut body = json!({
            "phoneNumberId": phone_number_id,
            "customer": {"number": call.to},
        });
        if call.agent_id.is_empty() {
            // A one-off assistant whose instructions are the task.
            let voice = if call.voice.is_empty() {
                "cjVigY5qzO86Huf0OWal"
            } else {
                &call.voice
            };
            body["assistant"] = json!({
                "model": {
                    "provider": "openai",
                    "model": "gpt-4o",
                    "messages": [{"role": "system", "content": call.message}],
                },
                "voice": {"provider": "11labs", "voiceId": voice},
                "maxDurationSeconds": call.max_call_secs,
            });
        } else {
            // The operator's assistant reads the task as `{{task}}`.
            body["assistantId"] = json!(call.agent_id);
            body["assistantOverrides"] = json!({
                "variableValues": {"task": call.message},
                "maxDurationSeconds": call.max_call_secs,
            });
        }
        let resp = self
            .client
            .post(format!("{}/call", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;
        vapi_status(&ensure_success(resp, "Vapi").await?.json().await?)
    }

    async fn call_status(&self, call_id: &str) -> Result<CallStatus> {
        let resp = self
            .client
            .get(format!("{}/call/{call_id}", self.base_url))
            .bearer_auth(&self.api_key)
            .send()
            .await?;
        vapi_status(&ensure_success(resp, "Vapi").await?.json().await?)
    }
}

// ── Bland (voice agent) ──

struct BlandProvider {
    client: Client,
    base_url: String,
    api_key: String,
}

/// Bland reports progress through `queue_status`, the outcome through
/// `status`, and talk time in minutes (`call_length`).
fn bland_status(id: &str, json: &Value) -> CallStatus {
    let queue = json["queue_status"].as_str().unwrap_or_default();
    let word = json["status"].as_str().unwrap_or(queue);
    let state = if json["completed"].as_bool() == Some(true) {
        match word {
            "no-answer" | "busy" | "failed" | "canceled" => CallState::Failed,
            _ => CallState::Ended,
        }
    } else {
        match queue {
            "started" | "in-progress" => CallState::InProgress,
            "complete" | "completed" => CallState::Ended,
            "error" | "failed" => CallState::Failed,
            _ => CallState::Queued,
        }
    };
    let mut status = CallStatus::new(id, state, word);
    status.duration_secs = json["call_length"]
        .as_f64()
        .map(|m| (m * 60.0).round() as u64);
    status.transcript = json["concatenated_transcript"].as_str().map(String::from);
    status.summary = json["summary"].as_str().map(String::from);
    status
}

#[async_trait::async_trait]
impl TelephonyProvider for BlandProvider {
    async fn place_call(&self, call: &CallRequest) -> Result<CallStatus> {
        let mut body = json!({
            "phone_number": call.to,
            "task": call.message,
            "voice": if call.voice.is_empty() { "mason" } else { &call.voice },
            // Bland bills and caps in whole minutes.
            "max_duration": call.max_call_secs.div_ceil(60),
            "wait_for_greeting": true,
        });
        if !call.caller_id.is_empty() {
            body["from"] = json!(call.caller_id);
        }
        if !call.agent_id.is_empty() {
            body["pathway_id"] = json!(call.agent_id);
        }
        let resp = self
            .client
            .post(format!("{}/calls", self.base_url))
            .header("authorization", &self.api_key)
            .json(&body)
            .send()
            .await?;
        let json: Value = ensure_success(resp, "Bland").await?.json().await?;
        let id = json["call_id"]
            .as_str()
            .with_context(|| format!("Bland did not queue the call: {}", json["message"]))?;
        Ok(CallStatus::new(id, CallState::Queued, "queued"))
    }

    async fn call_status(&self, call_id: &str) -> Result<CallStatus> {
        let resp = self
            .client
            .get(format!("{}/calls/{call_id}", self.base_url))
            .header("authorization", &self.api_key)
            .send()
            .await?;
        Ok(bland_status(
            call_id,
            &ensure_success(resp, "Bland").await?.json().await?,
        ))
    }

    async fn hang_up(&self, call_id: &str) -> Result<()> {
        let resp = self
            .client
            .post(format!("{}/calls/{call_id}/stop", self.base_url))
            .header("authorization", &self.api_key)
            .send()
            .await?;
        ensure_success(resp, "Bland").await?;
        Ok(())
    }
}

// ── ElevenLabs Agents (voice agent over a Twilio number linked in ElevenLabs) ──

struct ElevenLabsAgentProvider {
    client: Client,
    base_url: String,
    api_key: String,
}

/// Conversation `status` → shared state; the transcript arrives as turns.
fn elevenlabs_status(id: &str, json: &Value) -> CallStatus {
    let word = json["status"].as_str().unwrap_or_default();
    let state = match word {
        "initiated" => CallState::Queued,
        "in-progress" => CallState::InProgress,
        "processing" | "done" => CallState::Ended,
        "failed" => CallState::Failed,
        _ => CallState::Queued,
    };
    let mut status = CallStatus::new(id, state, word);
    status.duration_secs = json["metadata"]["call_duration_secs"].as_u64();
    status.summary = json["analysis"]["transcript_summary"]
        .as_str()
        .map(String::from);
    status.transcript = json["transcript"].as_array().map(|turns| {
        turns
            .iter()
            .filter_map(|t| {
                Some(format!(
                    "{}: {}",
                    t["role"].as_str()?,
                    t["message"].as_str()?
                ))
            })
            .collect::<Vec<_>>()
            .join("\n")
    });
    status
}

#[async_trait::async_trait]
impl TelephonyProvider for ElevenLabsAgentProvider {
    async fn place_call(&self, call: &CallRequest) -> Result<CallStatus> {
        if call.agent_id.is_empty() || call.caller_id.is_empty() {
            bail!(
                "ElevenLabs calls need an agent id and the phone number id linked to it. Set both \
                 under Settings → Advanced → Voice & phone."
            );
        }
        let resp = self
            .client
            .post(format!("{}/v1/convai/twilio/outbound-call", self.base_url))
            .header("xi-api-key", &self.api_key)
            .json(&json!({
                "agent_id": call.agent_id,
                "agent_phone_number_id": call.caller_id,
                "to_number": call.to,
                // The agent's prompt reads the task as `{{task}}`.
                "conversation_initiation_client_data": {
                    "dynamic_variables": {"task": call.message},
                },
            }))
            .send()
            .await?;
        let json: Value = ensure_success(resp, "ElevenLabs").await?.json().await?;
        if json["success"].as_bool() == Some(false) {
            bail!("ElevenLabs refused the call: {}", json["message"]);
        }
        let id = json["conversation_id"]
            .as_str()
            .context("ElevenLabs response has no conversation id")?;
        Ok(CallStatus::new(id, CallState::Queued, "initiated"))
    }

    async fn call_status(&self, call_id: &str) -> Result<CallStatus> {
        let resp = self
            .client
            .get(format!(
                "{}/v1/convai/conversations/{call_id}",
                self.base_url
            ))
            .header("xi-api-key", &self.api_key)
            .send()
            .await?;
        Ok(elevenlabs_status(
            call_id,
            &ensure_success(resp, "ElevenLabs").await?.json().await?,
        ))
    }
}

// ── Registry ──

/// What a registry row's constructor receives.
struct BuildContext<'a> {
    config: &'a Config,
    /// Already resolved; every phone provider needs one.
    api_key: String,
    base_url: String,
}

impl BuildContext<'_> {
    /// `setting` when set, else the named credential, else empty.
    fn setting_or_credential(&self, setting: &str, credential: &str) -> String {
        let setting = setting.trim();
        if setting.is_empty() {
            self.config
                .resolve_credential_or_env(credential)
                .unwrap_or_default()
        } else {
            setting.to_string()
        }
    }
}

/// One built-in phone provider.
pub struct TelephonyProviderDef {
    /// Shared provider metadata.
    pub info: ProviderInfo,
    /// What `telephony.caller_id` means for this provider, for Settings.
    pub caller_id_label: &'static str,
    /// Whether it reads `telephony.agent_id` (required when `true` and
    /// `agent_required`).
    pub uses_agent_id: bool,
    /// Whether a call cannot run without `telephony.agent_id`.
    pub agent_required: bool,
    /// Whether it reads `telephony.account_id`.
    pub uses_account_id: bool,
    build: fn(BuildContext<'_>) -> Result<Box<dyn TelephonyProvider>>,
}

impl TelephonyProviderDef {
    /// Build this provider, or say in plain words what is missing.
    pub(super) fn build(&'static self, config: &Config) -> Result<Box<dyn TelephonyProvider>> {
        let Some(api_key) = self.info.api_key(config, None) else {
            bail!(
                "Phone calls need the {}. Add it under Settings → Advanced → Voice & phone.",
                self.info.key_name()
            );
        };
        (self.build)(BuildContext {
            config,
            api_key,
            base_url: self.info.base_url_or(None),
        })
    }
}

/// Every built-in phone provider, in picker order.
static PROVIDERS: &[TelephonyProviderDef] = &[
    TelephonyProviderDef {
        info: ProviderInfo {
            key_env: &["TWILIO_AUTH_TOKEN"],
            key_label: "Twilio Auth Token",
            key_url: "https://console.twilio.com",
            base_url: "https://api.twilio.com",
            default_voice: "Polly.Joanna",
            ..ProviderInfo::new("twilio", "Twilio (speak a message)")
        },
        caller_id_label: "Twilio phone number",
        uses_agent_id: false,
        agent_required: false,
        uses_account_id: true,
        build: |cx| {
            let account_sid =
                cx.setting_or_credential(&cx.config.telephony.account_id, "TWILIO_ACCOUNT_SID");
            if account_sid.is_empty() {
                bail!("Twilio needs your Account SID. Add it under Settings → Advanced → Voice & phone.");
            }
            Ok(Box::new(TwilioProvider {
                client: crate::http::shared().clone(),
                default_caller_id: cx.setting_or_credential("", "TWILIO_PHONE_NUMBER"),
                account_sid,
                auth_token: cx.api_key,
                base_url: cx.base_url,
            }))
        },
    },
    TelephonyProviderDef {
        info: ProviderInfo {
            key_env: &["VAPI_API_KEY"],
            key_url: "https://dashboard.vapi.ai",
            base_url: "https://api.vapi.ai",
            ..ProviderInfo::new("vapi", "Vapi voice agent")
        },
        caller_id_label: "Vapi phone number id",
        uses_agent_id: true,
        agent_required: false,
        uses_account_id: false,
        build: |cx| {
            Ok(Box::new(VapiProvider {
                client: crate::http::shared().clone(),
                default_caller_id: cx.setting_or_credential("", "VAPI_PHONE_NUMBER_ID"),
                api_key: cx.api_key,
                base_url: cx.base_url,
            }))
        },
    },
    TelephonyProviderDef {
        info: ProviderInfo {
            key_env: &["BLAND_API_KEY"],
            key_url: "https://app.bland.ai",
            base_url: "https://api.bland.ai/v1",
            ..ProviderInfo::new("bland", "Bland voice agent")
        },
        caller_id_label: "Caller number (optional)",
        uses_agent_id: true,
        agent_required: false,
        uses_account_id: false,
        build: |cx| {
            Ok(Box::new(BlandProvider {
                client: crate::http::shared().clone(),
                api_key: cx.api_key,
                base_url: cx.base_url,
            }))
        },
    },
    TelephonyProviderDef {
        info: ProviderInfo {
            key_env: &["ELEVENLABS_API_KEY"],
            key_url: "https://elevenlabs.io/app/settings/api-keys",
            base_url: "https://api.elevenlabs.io",
            ..ProviderInfo::new("elevenlabs", "ElevenLabs voice agent")
        },
        caller_id_label: "ElevenLabs phone number id",
        uses_agent_id: true,
        agent_required: true,
        uses_account_id: false,
        build: |cx| {
            Ok(Box::new(ElevenLabsAgentProvider {
                client: crate::http::shared().clone(),
                api_key: cx.api_key,
                base_url: cx.base_url,
            }))
        },
    },
];

/// Look up a built-in phone provider by id.
pub(super) fn find(id: &str) -> Option<&'static TelephonyProviderDef> {
    crate::media::provider_info::find(PROVIDERS, id, |d| &d.info)
}

/// Every built-in phone provider, for pickers.
pub fn providers() -> impl Iterator<Item = &'static TelephonyProviderDef> {
    PROVIDERS.iter()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn request(caller_id: &str, agent_id: &str) -> CallRequest {
        CallRequest {
            to: "+14155550123".into(),
            message: "Book a table for two at 7 & ask about <parking>".into(),
            caller_id: caller_id.into(),
            agent_id: agent_id.into(),
            voice: String::new(),
            max_call_secs: 300,
        }
    }

    #[test]
    fn registry_is_well_formed() {
        crate::media::provider_info::assert_registry_well_formed(providers().map(|d| &d.info));
        for def in providers() {
            assert!(def.info.needs_key(), "{} must take a key", def.info.id);
            assert!(
                def.uses_agent_id || !def.agent_required,
                "{} requires an agent id it never reads",
                def.info.id
            );
        }
    }

    #[test]
    fn provider_without_key_names_the_fix() {
        let mut config = Config::default();
        config.telephony.provider = "bland".into();
        // Conventional variable unset in tests, nothing saved.
        let err = find("bland")
            .unwrap()
            .build(&config)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("need the Bland voice agent API key"), "{err}");
    }

    #[test]
    fn status_words_map_to_shared_states() {
        let twilio = |w: &str| {
            twilio_status(&json!({"sid": "CA1", "status": w}))
                .unwrap()
                .state
        };
        assert_eq!(twilio("queued"), CallState::Queued);
        assert_eq!(twilio("ringing"), CallState::Ringing);
        assert_eq!(twilio("in-progress"), CallState::InProgress);
        assert_eq!(twilio("completed"), CallState::Ended);
        for failed in ["busy", "no-answer", "failed", "canceled"] {
            assert_eq!(twilio(failed), CallState::Failed, "{failed}");
        }

        let vapi = |w: &str, r: &str| {
            vapi_status(&json!({"id": "v1", "status": w, "endedReason": r}))
                .unwrap()
                .state
        };
        assert_eq!(vapi("ended", "customer-ended-call"), CallState::Ended);
        assert_eq!(vapi("ended", "customer-did-not-answer"), CallState::Failed);
        assert_eq!(vapi("ended", "customer-busy"), CallState::Failed);
        assert_eq!(vapi("in-progress", ""), CallState::InProgress);

        let bland = |j: Value| bland_status("b1", &j).state;
        assert_eq!(bland(json!({"queue_status": "queued"})), CallState::Queued);
        assert_eq!(
            bland(json!({"queue_status": "started"})),
            CallState::InProgress
        );
        assert_eq!(
            bland(json!({"completed": true, "status": "completed"})),
            CallState::Ended
        );
        assert_eq!(
            bland(json!({"completed": true, "status": "no-answer"})),
            CallState::Failed
        );

        let eleven = |w: &str| elevenlabs_status("e1", &json!({"status": w})).state;
        assert_eq!(eleven("initiated"), CallState::Queued);
        assert_eq!(eleven("in-progress"), CallState::InProgress);
        assert_eq!(eleven("done"), CallState::Ended);
        assert_eq!(eleven("failed"), CallState::Failed);
    }

    #[test]
    fn status_details_carry_duration_transcript_and_summary() {
        let twilio =
            twilio_status(&json!({"sid": "CA1", "status": "completed", "duration": "61"})).unwrap();
        assert_eq!(twilio.duration_secs, Some(61));

        let bland = bland_status(
            "b1",
            &json!({"completed": true, "status": "completed", "call_length": 1.5,
                    "concatenated_transcript": "user: hi", "summary": "said hi"}),
        );
        assert_eq!(bland.duration_secs, Some(90));
        assert_eq!(bland.transcript.as_deref(), Some("user: hi"));
        assert_eq!(bland.summary.as_deref(), Some("said hi"));

        let eleven = elevenlabs_status(
            "e1",
            &json!({"status": "done",
                    "metadata": {"call_duration_secs": 33},
                    "analysis": {"transcript_summary": "booked"},
                    "transcript": [{"role": "agent", "message": "Hello"},
                                   {"role": "user", "message": "Hi"}]}),
        );
        assert_eq!(eleven.duration_secs, Some(33));
        assert_eq!(eleven.summary.as_deref(), Some("booked"));
        assert_eq!(eleven.transcript.as_deref(), Some("agent: Hello\nuser: Hi"));
    }

    #[tokio::test]
    async fn twilio_places_call_with_escaped_twiml_and_time_limit() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/2010-04-01/Accounts/AC1/Calls.json"))
            // Basic auth of AC1:tok.
            .and(header("authorization", "Basic QUMxOnRvaw=="))
            .and(body_string_contains("TimeLimit=300"))
            .and(body_string_contains("From=%2B15550001111"))
            // `&` and `<` from the message arrive escaped inside <Say>.
            .and(body_string_contains(
                "7+%26amp%3B+ask+about+%26lt%3Bparking%26gt%3B",
            ))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(json!({"sid": "CA9", "status": "queued"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let provider = TwilioProvider {
            client: Client::new(),
            base_url: server.uri(),
            account_sid: "AC1".into(),
            auth_token: "tok".into(),
            default_caller_id: "+15550001111".into(),
        };
        let status = provider.place_call(&request("", "")).await.unwrap();
        assert_eq!(
            (status.id.as_str(), status.state),
            ("CA9", CallState::Queued)
        );
    }

    #[tokio::test]
    async fn twilio_without_any_caller_number_refuses_before_calling() {
        let provider = TwilioProvider {
            client: Client::new(),
            base_url: "http://127.0.0.1:9".into(),
            account_sid: "AC1".into(),
            auth_token: "tok".into(),
            default_caller_id: String::new(),
        };
        let err = provider
            .place_call(&request("", ""))
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("number calls come from"), "{err}");
    }

    #[tokio::test]
    async fn vapi_uses_saved_assistant_with_task_variable_when_agent_set() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/call"))
            .and(header("authorization", "Bearer vk"))
            .and(body_partial_json(json!({
                "phoneNumberId": "pn1",
                "customer": {"number": "+14155550123"},
                "assistantId": "asst1",
                "assistantOverrides": {"variableValues": {"task": "Book a table for two at 7 & ask about <parking>"}},
            })))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(json!({"id": "v1", "status": "queued"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let provider = VapiProvider {
            client: Client::new(),
            base_url: server.uri(),
            api_key: "vk".into(),
            default_caller_id: String::new(),
        };
        let status = provider.place_call(&request("pn1", "asst1")).await.unwrap();
        assert_eq!(status.id, "v1");
    }

    #[tokio::test]
    async fn vapi_without_agent_sends_task_as_one_off_assistant_prompt() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/call"))
            .and(body_partial_json(json!({
                "assistant": {"maxDurationSeconds": 300,
                              "model": {"messages": [{"role": "system",
                                  "content": "Book a table for two at 7 & ask about <parking>"}]}},
            })))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(json!({"id": "v2", "status": "queued"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let provider = VapiProvider {
            client: Client::new(),
            base_url: server.uri(),
            api_key: "vk".into(),
            default_caller_id: "pn-default".into(),
        };
        provider.place_call(&request("", "")).await.unwrap();
    }

    #[tokio::test]
    async fn bland_rounds_call_cap_up_to_whole_minutes_and_hangs_up() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/calls"))
            .and(header("authorization", "bk"))
            .and(body_partial_json(
                json!({"phone_number": "+14155550123", "max_duration": 2}),
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"status": "success", "call_id": "b1"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/calls/b1/stop"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "success"})))
            .expect(1)
            .mount(&server)
            .await;
        let provider = BlandProvider {
            client: Client::new(),
            base_url: server.uri(),
            api_key: "bk".into(),
        };
        let mut call = request("", "");
        call.max_call_secs = 61;
        assert_eq!(provider.place_call(&call).await.unwrap().id, "b1");
        provider.hang_up("b1").await.unwrap();
    }

    #[tokio::test]
    async fn elevenlabs_passes_task_as_dynamic_variable_and_needs_agent() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/convai/twilio/outbound-call"))
            .and(header("xi-api-key", "xi"))
            .and(body_partial_json(json!({
                "agent_id": "agent1",
                "agent_phone_number_id": "phn1",
                "to_number": "+14155550123",
                "conversation_initiation_client_data": {"dynamic_variables": {"task": "Book a table for two at 7 & ask about <parking>"}},
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"success": true, "conversation_id": "conv1", "callSid": "CA1"}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let provider = ElevenLabsAgentProvider {
            client: Client::new(),
            base_url: server.uri(),
            api_key: "xi".into(),
        };
        assert_eq!(
            provider
                .place_call(&request("phn1", "agent1"))
                .await
                .unwrap()
                .id,
            "conv1"
        );
        let err = provider
            .place_call(&request("phn1", ""))
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("need an agent id"), "{err}");
        assert!(
            provider.hang_up("conv1").await.is_err(),
            "no early hang-up for agents"
        );
    }
}
