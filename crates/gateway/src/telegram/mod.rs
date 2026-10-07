/// Telegram Bot API HTTP client.
pub mod api;
/// Native channel implementation for Telegram.
pub mod channel;
/// Per-chat sequential message queue.
pub mod chat_queue;
/// Circuit breaker for Telegram API calls.
/// Update deduplication by update_id.
pub mod dedup;
/// Shared Telegram delivery helpers.
pub mod delivery;
/// Markdown-to-Telegram HTML formatting.
pub mod format;
/// Telegram update parsing into inbound messages.
pub mod parse;
/// Long-polling transport for Telegram updates.
pub mod polling;
/// Telegram API type definitions.
pub mod types;
/// Typing indicator keepalive for Telegram chats.
pub mod typing;
/// Webhook secret token verification.
pub mod verify;

use anyhow::Result;
use axum::http::HeaderMap;
use tokio::sync::Mutex;

use crate::message_processing::voice::VOICE_TRANSCRIPT_PREFIX;
use dedup::UpdateDeduplicator;
pub use parse::{TelegramAudioRef, TelegramInputKind, TelegramParsed, TelegramPhotoRef};

/// Download referenced Telegram media and inline it into the inbound message.
///
/// Photos become base64 image attachments. Audio/voice becomes transcribed text
/// when audio transcription is enabled.
pub async fn enrich_parsed_message(
    client: &api::TelegramClient,
    config: &cortex::config::Config,
    parsed: &mut TelegramParsed,
) {
    if let Some(ref photo) = parsed.photo {
        match async {
            let file_info = client.get_file(&photo.file_id).await?;
            let file_path = file_info
                .file_path
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("No file_path in getFile response"))?;
            let bytes = client.download_file(file_path).await?;
            Ok::<_, anyhow::Error>(bytes)
        }
        .await
        {
            Ok(bytes) => {
                use base64::Engine;
                parsed
                    .inbound
                    .attachments
                    .push(crate::handler::InboundAttachment {
                        mime_type: "image/jpeg".to_string(),
                        data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                        filename: Some(photo.filename.clone()),
                    });
            }
            Err(e) => {
                tracing::warn!("Telegram photo download failed: {e}");
                // Surface the failure to the agent instead of silently
                // presenting a caption-only (or bare "[Photo]") message.
                crate::message_processing::media_failure::append_download_failure(
                    &mut parsed.inbound.text,
                    "image",
                );
            }
        }
    }

    if let Some(ref audio) = parsed.audio {
        if let Some(transcriber) =
            cortex::media::understanding::AudioTranscriber::from_config(config)
        {
            // Download and transcription fail differently for the agent:
            // a missing file is "[audio could not be downloaded]", a file
            // that arrived but produced no text says why.
            let downloaded = async {
                let file_info = client.get_file(&audio.file_id).await?;
                let file_path = file_info
                    .file_path
                    .ok_or_else(|| anyhow::anyhow!("No file_path in getFile response"))?;
                let bytes = client.download_file(&file_path).await?;
                let filename = file_path
                    .rsplit('/')
                    .next()
                    .unwrap_or("audio.ogg")
                    .to_string();
                Ok::<_, anyhow::Error>((bytes, filename))
            }
            .await;
            let (bytes, filename) = match downloaded {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!("Telegram audio download failed: {e}");
                    crate::message_processing::media_failure::append_download_failure(
                        &mut parsed.inbound.text,
                        "audio",
                    );
                    return;
                }
            };
            let lang = config.audio.language.as_deref();
            match transcriber
                .transcribe(&bytes, &audio.mime_type, &filename, lang)
                .await
            {
                Ok((transcript, _attempts)) => {
                    parsed.inbound.text = format!("{VOICE_TRANSCRIPT_PREFIX}{transcript}");
                    if config.audio.echo_transcript {
                        let echo_chat_id: i64 = parsed
                            .inbound
                            .channel_id
                            .as_deref()
                            .and_then(|id| id.parse().ok())
                            .unwrap_or(0);
                        if echo_chat_id != 0 {
                            if let Err(e) = client
                                .send_message(echo_chat_id, &transcript, None, None, None)
                                .await
                            {
                                tracing::warn!("Failed to echo Telegram transcript: {e}");
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Audio transcription failed: {e}");
                    // "[Voice message]" alone reads as if the note were
                    // empty; tell the agent the note exists and why it has
                    // no text.
                    let placeholder =
                        crate::message_processing::voice::transcription_failure_placeholder(&e);
                    parsed.inbound.text = if parsed.inbound.text.trim().is_empty() {
                        placeholder
                    } else {
                        format!("{}\n{placeholder}", parsed.inbound.text)
                    };
                }
            }
        }
    }
}

/// Process an incoming Telegram webhook request.
///
/// Flow: validate body → verify secret → dedup → parse → return.
/// Returns `Ok(None)` for non-text updates or duplicates.
/// The returned [`TelegramParsed`] carries optional media references the
/// caller is responsible for downloading (audio for transcription, photo for
/// vision attachments).
pub async fn handle_telegram_webhook(
    headers: &HeaderMap,
    body: &str,
    secret: Option<&str>,
    dedup: &Mutex<UpdateDeduplicator>,
) -> Result<Option<TelegramParsed>> {
    let update = verify::validate_update(body)?;

    if let Some(secret) = secret {
        if !verify::verify_secret_token(headers, secret) {
            anyhow::bail!("Telegram webhook secret verification failed");
        }
    }

    // Deduplicate
    {
        let mut guard = dedup.lock().await;
        if guard.is_duplicate(update.update_id) {
            return Ok(None);
        }
    }

    Ok(parse::parse_update(&update))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn make_headers(secret: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(s) = secret {
            headers.insert(
                "x-telegram-bot-api-secret-token",
                HeaderValue::from_str(s).unwrap(),
            );
        }
        headers
    }

    fn text_body(update_id: i64, text: &str) -> String {
        format!(
            r#"{{
                "update_id": {update_id},
                "message": {{
                    "message_id": 1,
                    "from": {{ "id": 42, "first_name": "Alice", "is_bot": false }},
                    "chat": {{ "id": 42, "type": "private" }},
                    "date": 1700000000,
                    "text": {text_json}
                }}
            }}"#,
            text_json = serde_json::to_string(text).unwrap()
        )
    }

    #[tokio::test]
    async fn full_flow_no_secret() {
        let dedup = Mutex::new(UpdateDeduplicator::new());
        let headers = HeaderMap::new();
        let body = text_body(1, "hello");

        let result = handle_telegram_webhook(&headers, &body, None, &dedup)
            .await
            .unwrap();
        let msg = result.unwrap().inbound;
        assert_eq!(msg.text, "hello");
        assert_eq!(msg.sender_id, "42");
    }

    #[tokio::test]
    async fn full_flow_with_secret() {
        let dedup = Mutex::new(UpdateDeduplicator::new());
        let headers = make_headers(Some("secret123"));
        let body = text_body(2, "hi");

        let result = handle_telegram_webhook(&headers, &body, Some("secret123"), &dedup)
            .await
            .unwrap();
        // Matching secret must admit the update AND yield the parsed message
        // (paired with wrong_secret_rejected, this proves the secret gates
        // the flow without dropping the payload).
        let msg = result
            .expect("matching secret must produce a message")
            .inbound;
        assert_eq!(msg.text, "hi");
        assert_eq!(msg.sender_id, "42");
    }

    #[tokio::test]
    async fn wrong_secret_rejected() {
        let dedup = Mutex::new(UpdateDeduplicator::new());
        let headers = make_headers(Some("wrong"));
        let body = text_body(3, "hi");

        let result = handle_telegram_webhook(&headers, &body, Some("secret123"), &dedup).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn duplicate_update_returns_none() {
        let dedup = Mutex::new(UpdateDeduplicator::new());
        let headers = HeaderMap::new();
        let body = text_body(10, "hi");

        let first = handle_telegram_webhook(&headers, &body, None, &dedup)
            .await
            .unwrap();
        assert!(first.is_some());

        let second = handle_telegram_webhook(&headers, &body, None, &dedup)
            .await
            .unwrap();
        assert!(second.is_none());
    }

    #[tokio::test]
    async fn non_text_update_returns_none() {
        let dedup = Mutex::new(UpdateDeduplicator::new());
        let headers = HeaderMap::new();
        // Message with no text, no media — returns None
        let body = r#"{
            "update_id": 20,
            "message": {
                "message_id": 1,
                "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                "chat": { "id": 42, "type": "private" },
                "date": 1700000000
            }
        }"#;

        let result = handle_telegram_webhook(&headers, body, None, &dedup)
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn photo_message_returns_placeholder() {
        let dedup = Mutex::new(UpdateDeduplicator::new());
        let headers = HeaderMap::new();
        let body = r#"{
            "update_id": 21,
            "message": {
                "message_id": 1,
                "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                "chat": { "id": 42, "type": "private" },
                "date": 1700000000,
                "photo": [{ "file_id": "abc", "file_unique_id": "u1", "width": 100, "height": 100 }]
            }
        }"#;

        let result = handle_telegram_webhook(&headers, body, None, &dedup)
            .await
            .unwrap();
        let parsed = result.unwrap();
        assert_eq!(parsed.inbound.text, "[Photo]");
        // Photo ref is populated so the channel can download bytes before enqueue.
        let photo = parsed.photo.expect("photo ref populated");
        assert_eq!(photo.file_id, "abc");
    }

    async fn parse_body(body: &str) -> TelegramParsed {
        let dedup = Mutex::new(UpdateDeduplicator::new());
        handle_telegram_webhook(&HeaderMap::new(), body, None, &dedup)
            .await
            .unwrap()
            .expect("body must parse to a message")
    }

    /// A failed photo download must surface in the message text — a bare
    /// "[Photo]" hands the agent a message that pretends the image arrived.
    #[tokio::test]
    async fn enrich_appends_failure_note_when_photo_download_fails() {
        // Port 9 (discard) on loopback: nothing listens, connect is refused.
        let client = api::TelegramClient::with_base("testtok", "http://127.0.0.1:9").unwrap();
        let config = cortex::config::Config::default();
        let mut parsed = parse_body(
            r#"{
                "update_id": 31,
                "message": {
                    "message_id": 1,
                    "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                    "chat": { "id": 42, "type": "private" },
                    "date": 1700000000,
                    "photo": [{ "file_id": "abc", "file_unique_id": "u1", "width": 100, "height": 100 }]
                }
            }"#,
        )
        .await;

        enrich_parsed_message(&client, &config, &mut parsed).await;

        assert_eq!(
            parsed.inbound.text,
            "[Photo]\n[image could not be downloaded]"
        );
        assert!(parsed.inbound.attachments.is_empty());
    }

    /// Voice enrichment must transcribe into the message text, and the
    /// transcript echo must go back to the originating chat. The `!= 0`
    /// chat-id guard only suppresses the echo when the chat id failed to
    /// parse — never for a real chat.
    #[tokio::test]
    async fn enrich_transcribes_voice_and_echoes_to_originating_chat() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bottesttok/getFile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": {
                    "file_id": "vf1",
                    "file_unique_id": "u2",
                    "file_size": 1500,
                    "file_path": "voice/a.oga"
                }
            })))
            .mount(&server)
            .await;
        // 1500 printable bytes: above the transcriber's min_file_size and
        // valid UTF-8, so `cat` can serve as the transcript command.
        let audio_bytes = vec![b'x'; 1500];
        Mock::given(method("GET"))
            .and(path("/file/bottesttok/voice/a.oga"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(audio_bytes))
            .mount(&server)
            .await;
        // The echo: exactly one sendMessage back to the chat.
        Mock::given(method("POST"))
            .and(path("/bottesttok/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": {}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = api::TelegramClient::with_base("testtok", &server.uri()).unwrap();
        let mut config = cortex::config::Config::default();
        config.audio.enabled = true;
        config.audio.echo_transcript = true;
        config.audio.models = vec![cortex::config::AudioModelConfig {
            provider: "custom".to_string(),
            model: None,
            api_key_env: None,
            language: None,
            timeout_ms: None,
            base_url: None,
            xai_stt_format: None,
            xai_stt_diarize: None,
            command: Some("cat '{audio_file}'".to_string()),
        }];

        let mut parsed = parse_body(
            r#"{
                "update_id": 32,
                "message": {
                    "message_id": 1,
                    "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                    "chat": { "id": 4242, "type": "private" },
                    "date": 1700000000,
                    "voice": { "file_id": "vf1", "file_unique_id": "u2", "duration": 3 }
                }
            }"#,
        )
        .await;
        assert_eq!(parsed.inbound.channel_id.as_deref(), Some("4242"));

        enrich_parsed_message(&client, &config, &mut parsed).await;

        assert!(
            parsed.inbound.text.starts_with(VOICE_TRANSCRIPT_PREFIX),
            "voice text must be replaced by the transcript, got: {}",
            &parsed.inbound.text[..parsed.inbound.text.len().min(60)]
        );
        assert!(
            parsed.inbound.text.contains("xxxx"),
            "transcript must carry the command output"
        );
        // MockServer verifies the sendMessage expect(1) on drop; verify
        // explicitly so a missing echo fails here with context.
        server.verify().await;
    }

    /// A note that downloaded but failed STT must tell the agent so — a
    /// bare "[Voice message]" reads as an empty note. The placeholder
    /// names the reason without echoing the provider's error body.
    #[tokio::test]
    async fn enrich_appends_transcription_failure_note_when_stt_fails() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bottesttok/getFile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": {
                    "file_id": "vf2",
                    "file_unique_id": "u3",
                    "file_size": 1500,
                    "file_path": "voice/b.oga"
                }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/file/bottesttok/voice/b.oga"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 1500]))
            .mount(&server)
            .await;

        let client = api::TelegramClient::with_base("testtok", &server.uri()).unwrap();
        let mut config = cortex::config::Config::default();
        config.audio.enabled = true;
        config.audio.models = vec![cortex::config::AudioModelConfig {
            provider: "custom".to_string(),
            model: None,
            api_key_env: None,
            language: None,
            timeout_ms: None,
            base_url: None,
            xai_stt_format: None,
            xai_stt_diarize: None,
            // Exits non-zero: the engine reports a provider failure.
            command: Some("echo secret-host-detail >&2; : '{audio_file}'; exit 7".to_string()),
        }];

        let mut parsed = parse_body(
            r#"{
                "update_id": 33,
                "message": {
                    "message_id": 1,
                    "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                    "chat": { "id": 4242, "type": "private" },
                    "date": 1700000000,
                    "voice": { "file_id": "vf2", "file_unique_id": "u3", "duration": 3 }
                }
            }"#,
        )
        .await;

        enrich_parsed_message(&client, &config, &mut parsed).await;

        assert_eq!(
            parsed.inbound.text,
            "[Voice message]\n[audio could not be transcribed: provider error]"
        );
    }

    /// A voice note whose file never arrives is a download failure, not a
    /// transcription failure — the two placeholders must stay distinct.
    #[tokio::test]
    async fn enrich_appends_download_failure_note_when_voice_fetch_fails() {
        let client = api::TelegramClient::with_base("testtok", "http://127.0.0.1:9").unwrap();
        let mut config = cortex::config::Config::default();
        config.audio.enabled = true;
        config.audio.models = vec![cortex::config::AudioModelConfig {
            provider: "custom".to_string(),
            model: None,
            api_key_env: None,
            language: None,
            timeout_ms: None,
            base_url: None,
            xai_stt_format: None,
            xai_stt_diarize: None,
            command: Some("cat '{audio_file}'".to_string()),
        }];
        let mut parsed = parse_body(
            r#"{
                "update_id": 34,
                "message": {
                    "message_id": 1,
                    "from": { "id": 42, "first_name": "Alice", "is_bot": false },
                    "chat": { "id": 4242, "type": "private" },
                    "date": 1700000000,
                    "voice": { "file_id": "vf3", "file_unique_id": "u4", "duration": 3 }
                }
            }"#,
        )
        .await;

        enrich_parsed_message(&client, &config, &mut parsed).await;

        assert_eq!(
            parsed.inbound.text,
            "[Voice message]\n[audio could not be downloaded]"
        );
    }

    #[tokio::test]
    async fn invalid_body_returns_error() {
        let dedup = Mutex::new(UpdateDeduplicator::new());
        let headers = HeaderMap::new();

        let result = handle_telegram_webhook(&headers, "not json", None, &dedup).await;
        assert!(result.is_err());
    }
}
