use std::sync::Arc;

use anyhow::Result;
use tokio::sync::RwLock;
use tracing::warn;

use crate::health::ChannelHealthRegistry;
use crate::voice::{should_send_voice_reply, VoiceReplyMode};

use super::api::TelegramClient;

#[allow(clippy::too_many_arguments)]
pub async fn send_response(
    client: &TelegramClient,
    chat_id: i64,
    response_text: &str,
    thread_id: Option<i64>,
    reply_to: Option<i64>,
    input_was_voice: bool,
    voice_reply_mode: Option<VoiceReplyMode>,
    health: &Arc<RwLock<ChannelHealthRegistry>>,
    tts_synthesizer: Option<&cortex::tts::TtsSynthesizer>,
) -> Result<()> {
    if response_text.trim().is_empty() {
        tracing::debug!("Telegram response empty after trim, skipping send for chat {chat_id}");
        return Ok(());
    }

    let html = crate::telegram::format::markdown_to_telegram_html(response_text);

    if let Err(e) = client
        .send_message(chat_id, &html, Some("HTML"), thread_id, reply_to)
        .await
    {
        warn!("HTML send failed, retrying as plain text: {e}");
        if let Err(e2) = client
            .send_message(chat_id, response_text, None, thread_id, reply_to)
            .await
        {
            health
                .write()
                .await
                .record_error("telegram", &e2.to_string());
            return Err(e2);
        }
    }
    health.write().await.record_outbound("telegram");

    if let Some(synth) = tts_synthesizer {
        if let Some(mode) = voice_reply_mode {
            if should_send_voice_reply(mode, response_text, input_was_voice) {
                let tts_text = cortex::tts::truncate_for_tts(response_text, 4096);
                match synth
                    .synthesize(&tts_text, None, Some(cortex::tts::AudioFormat::Opus))
                    .await
                {
                    Ok((audio_bytes, _, _)) => {
                        if let Err(e) = client
                            .send_voice(chat_id, &audio_bytes, None, thread_id, None)
                            .await
                        {
                            warn!("Failed to send TTS voice message: {e}");
                        }
                    }
                    Err(e) => warn!("TTS synthesis failed: {e}"),
                }
            }
        }
    }

    Ok(())
}
