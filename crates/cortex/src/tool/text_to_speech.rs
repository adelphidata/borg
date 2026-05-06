//! `text_to_speech` tool.

use serde::{Deserialize, Serialize};

use crate::tool_handlers;
use crate::tts::TtsSynthesizer;
use crate::types::ToolOutput;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TextToSpeechArgs {
    pub text: String,
    #[serde(default)]
    pub voice: Option<String>,
    #[serde(default)]
    pub format: Option<String>,
}

impl TextToSpeechArgs {
    fn to_value(&self) -> serde_json::Value {
        // Plain struct of primitives; serialization is infallible.
        // The expect would only fire on a serde upstream bug.
        #[allow(clippy::expect_used)]
        {
            serde_json::to_value(self).expect("TextToSpeechArgs is JSON-safe")
        }
    }
}

pub(crate) async fn handle(args: &TextToSpeechArgs, synth: &TtsSynthesizer) -> ToolOutput {
    tool_handlers::handle_text_to_speech(&args.to_value(), synth).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_minimal_args() {
        let args: TextToSpeechArgs =
            serde_json::from_value(serde_json::json!({"text":"hello"})).unwrap();
        assert_eq!(args.text, "hello");
        assert!(args.voice.is_none());
    }

    #[test]
    fn deserialize_with_voice_and_format() {
        let args: TextToSpeechArgs =
            serde_json::from_value(serde_json::json!({"text":"x","voice":"alloy","format":"mp3"}))
                .unwrap();
        assert_eq!(args.voice.as_deref(), Some("alloy"));
        assert_eq!(args.format.as_deref(), Some("mp3"));
    }
}
