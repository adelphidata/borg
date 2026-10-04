//! Voice gRPC service: the provider catalog Settings renders, and the
//! transcription behind the composer's microphone.
//!
//! Both read the live settings table on every call, so a key saved or a
//! provider switched in Settings shows up without a daemon restart.

use cortex::config::Config;
use cortex::media::provider_info::ProviderInfo;
use cortex::media::tts::TtsSynthesizer;
use cortex::media::understanding::AudioTranscriber;
use cortex::telephony::{PhoneDialer, TelephonyProviderDef};
use protocols::voice::voice_server::Voice;
use protocols::voice::{
    Empty, PhoneProvider, TranscribeRequest, TranscribeResponse, VoiceCatalog, VoiceProvider,
};
use tonic::{Request, Response, Status as TStatus};

/// Largest recording `Transcribe` accepts; also the service's decode limit.
/// Matches the default `audio.max_file_size`, which the transcriber enforces
/// again with the operator's own value.
pub const MAX_RECORDING_BYTES: usize = 20 * 1024 * 1024;

/// Voice service implementation. Stateless: config is read per call.
#[derive(Debug, Default)]
pub struct VoiceSvc;

fn provider(info: &ProviderInfo, config: &Config) -> VoiceProvider {
    VoiceProvider {
        id: info.id.to_string(),
        label: info.label.to_string(),
        needs_key: info.needs_key(),
        key_url: info.key_url.to_string(),
        default_model: info.default_model.to_string(),
        default_voice: info.default_voice.to_string(),
        requires_command: info.requires_command,
        has_key: info.api_key(config, None).is_some(),
        key_label: info.key_name(),
    }
}

fn phone_provider(def: &TelephonyProviderDef, config: &Config) -> PhoneProvider {
    PhoneProvider {
        provider: Some(provider(&def.info, config)),
        caller_id_label: def.caller_id_label.to_string(),
        uses_agent_id: def.uses_agent_id,
        agent_required: def.agent_required,
        uses_account_id: def.uses_account_id,
    }
}

/// The catalog for `config`. Split out of the RPC so tests can drive it with
/// an in-memory config.
pub fn catalog(config: &Config) -> VoiceCatalog {
    VoiceCatalog {
        speech: cortex::media::tts::providers()
            .map(|info| provider(info, config))
            .collect(),
        transcription: cortex::media::understanding::providers()
            .map(|info| provider(info, config))
            .collect(),
        phone: cortex::telephony::providers()
            .map(|def| phone_provider(def, config))
            .collect(),
        speech_ready: TtsSynthesizer::from_config(config).is_some(),
        transcription_ready: AudioTranscriber::from_config(config).is_some(),
        phone_problem: PhoneDialer::from_config(config)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default(),
    }
}

/// File name the transcription providers see; some infer the codec from it.
fn file_name_for(mime_type: &str) -> &'static str {
    match mime_type.split(';').next().unwrap_or_default().trim() {
        "audio/wav" | "audio/x-wav" | "audio/wave" => "recording.wav",
        "audio/ogg" => "recording.ogg",
        "audio/mpeg" | "audio/mp3" => "recording.mp3",
        "audio/mp4" | "audio/m4a" | "audio/x-m4a" => "recording.m4a",
        _ => "recording.webm",
    }
}

async fn load_config() -> Result<Config, TStatus> {
    Config::load_from_db()
        .await
        .map_err(|e| TStatus::internal(format!("load settings: {e}")))
}

#[tonic::async_trait]
impl Voice for VoiceSvc {
    async fn get_voice_catalog(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<VoiceCatalog>, TStatus> {
        Ok(Response::new(catalog(&load_config().await?)))
    }

    async fn transcribe(
        &self,
        req: Request<TranscribeRequest>,
    ) -> Result<Response<TranscribeResponse>, TStatus> {
        let config = load_config().await?;
        transcribe_with(&config, req.into_inner())
            .await
            .map(Response::new)
    }
}

/// `Transcribe` against an already-loaded config.
async fn transcribe_with(
    config: &Config,
    req: TranscribeRequest,
) -> Result<TranscribeResponse, TStatus> {
    let Some(transcriber) = AudioTranscriber::from_config(config) else {
        return Err(TStatus::failed_precondition(
            "Voice input is off. Turn it on under Settings → Advanced → Voice & phone.",
        ));
    };
    let mime = if req.mime_type.is_empty() {
        "audio/webm"
    } else {
        req.mime_type.as_str()
    };
    let language = Some(req.language.as_str()).filter(|l| !l.is_empty());
    match transcriber
        .transcribe(&req.audio, mime, file_name_for(mime), language)
        .await
    {
        Ok((text, _)) => Ok(TranscribeResponse {
            text: text.trim().to_string(),
        }),
        Err(e) => {
            tracing::warn!("voice transcription failed: {e:#}");
            Err(TStatus::unavailable(format!(
                "Could not transcribe the recording: {e}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortex::config::TtsModelConfig;

    #[test]
    fn catalog_lists_every_registry_row_and_off_by_default() {
        let config = Config::default();
        let cat = catalog(&config);
        assert_eq!(
            cat.speech.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            cortex::media::tts::providers()
                .map(|i| i.id)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            cat.transcription.len(),
            cortex::media::understanding::providers().count()
        );
        assert_eq!(cat.phone.len(), cortex::telephony::providers().count());
        assert!(!cat.speech_ready && !cat.transcription_ready);
        assert!(cat.phone_problem.contains("off"), "{}", cat.phone_problem);
    }

    #[test]
    fn has_key_and_ready_follow_the_saved_key() {
        let rows = [
            ("tts.enabled".to_string(), "true".to_string()),
            (
                "tts.models".to_string(),
                serde_json::to_string(&[TtsModelConfig {
                    provider: "elevenlabs".into(),
                    // Points at a variable nobody sets, so only the saved key counts.
                    api_key_env: Some("BORGD_VOICE_TEST_UNSET".into()),
                    ..Default::default()
                }])
                .unwrap(),
            ),
            (
                "credentials.elevenlabs".to_string(),
                r#"{"type":"literal","value":"xi"}"#.to_string(),
            ),
        ];
        let cat = catalog(&Config::from_setting_rows(rows));
        let eleven = cat.speech.iter().find(|p| p.id == "elevenlabs").unwrap();
        assert!(eleven.has_key && eleven.needs_key);
        assert!(cat.speech_ready);
        // Keyless providers always report a key.
        assert!(cat.speech.iter().find(|p| p.id == "edge").unwrap().has_key);
    }

    #[test]
    fn phone_rows_carry_the_fields_settings_shows() {
        let cat = catalog(&Config::default());
        let twilio = cat
            .phone
            .iter()
            .find(|p| p.provider.as_ref().unwrap().id == "twilio")
            .unwrap();
        assert!(twilio.uses_account_id && !twilio.uses_agent_id);
        let eleven = cat
            .phone
            .iter()
            .find(|p| p.provider.as_ref().unwrap().id == "elevenlabs")
            .unwrap();
        assert!(eleven.agent_required);
    }

    #[test]
    fn file_name_follows_mime_and_defaults_to_webm() {
        assert_eq!(file_name_for("audio/webm;codecs=opus"), "recording.webm");
        assert_eq!(file_name_for("audio/wav"), "recording.wav");
        assert_eq!(file_name_for("audio/mp4"), "recording.m4a");
        assert_eq!(file_name_for(""), "recording.webm");
    }

    fn recording() -> TranscribeRequest {
        TranscribeRequest {
            audio: vec![0; 2048],
            mime_type: "audio/webm".into(),
            language: String::new(),
        }
    }

    #[tokio::test]
    async fn transcribe_with_voice_input_off_names_the_setting() {
        let err = transcribe_with(&Config::default(), recording())
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("Voice input is off"));
    }

    #[tokio::test]
    async fn transcribe_returns_trimmed_text_from_the_chain() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/audio/transcriptions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"text": "  hi there \n"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let rows = [
            ("audio.enabled".to_string(), "true".to_string()),
            (
                "audio.models".to_string(),
                serde_json::json!([{"provider": "openai", "base_url": server.uri(),
                                     "api_key_env": "BORGD_VOICE_TEST_UNSET"}])
                .to_string(),
            ),
            (
                "credentials.openai".to_string(),
                r#"{"type":"literal","value":"sk"}"#.to_string(),
            ),
        ];
        let out = transcribe_with(&Config::from_setting_rows(rows), recording())
            .await
            .unwrap();
        assert_eq!(out.text, "hi there");
    }
}
