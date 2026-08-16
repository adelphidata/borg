//! Shared vision gate for tools that can return image bytes.
//!
//! A screenshot handed to a text-only model is worse than no screenshot: the
//! LLM layer drops the pixels and the model answers as if it had looked. Every
//! image-producing tool routes its output through
//! [`downgrade_for_non_vision_model`], which replaces the whole multimodal
//! envelope with a text note that names the limitation and the way out.

use crate::config::Config;
use crate::types::{ContentPart, ToolOutput};

/// Whether the configured main model accepts image input. Unknown/unset
/// providers default to `true` — matching [`crate::llm::provider::Provider::supports_vision`]'s
/// "avoid false negatives that would silently downgrade multimodal" stance,
/// so we never wrongly strip a screenshot from a model that could see it.
pub(crate) fn main_model_supports_vision(config: &Config) -> bool {
    let raw = config.llm.provider.as_deref().unwrap_or("").trim();
    if raw.is_empty() {
        return true;
    }
    match raw.parse::<crate::llm::provider::Provider>() {
        Ok(provider) => provider.supports_vision(&config.llm.model),
        Err(_) => true,
    }
}

/// Replace a multimodal (image-bearing) tool result with a text-only one when
/// the main model can't accept images. Vision-capable models — and any
/// already-text result — pass through untouched.
///
/// `captured` names what was produced ("browse captured a page screenshot")
/// and `remedy` names the text-only alternative; both are folded into the note
/// ahead of the tool's own summary, which is always preserved.
pub(crate) fn downgrade_for_non_vision_model(
    out: ToolOutput,
    config: &Config,
    captured: &str,
    remedy: &str,
) -> ToolOutput {
    let ToolOutput::Multimodal { text, parts } = &out else {
        return out;
    };
    let has_image = parts.iter().any(|p| {
        matches!(
            p,
            ContentPart::ImageBase64 { .. } | ContentPart::ImageUrl { .. }
        )
    });
    if !has_image || main_model_supports_vision(config) {
        return out;
    }
    let summary = if text.trim().is_empty() {
        "(no text detail)".to_string()
    } else {
        text.clone()
    };
    ToolOutput::Text(format!(
        "Note: {captured}, but the active model ({}) does not support image \
         input, so the pixels were dropped. {remedy} Captured text summary:\n{summary}",
        config.llm.model
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MediaData;

    fn multimodal(text: &str) -> ToolOutput {
        ToolOutput::Multimodal {
            text: text.to_string(),
            parts: vec![
                ContentPart::Text(text.to_string()),
                ContentPart::ImageBase64 {
                    media: MediaData {
                        mime_type: "image/png".into(),
                        data: "aGk=".into(),
                        filename: Some("shot.png".into()),
                    },
                },
            ],
        }
    }

    fn config_for(provider: &str, model: &str) -> Config {
        let mut config = Config::default();
        config.llm.provider = Some(provider.into());
        config.llm.model = model.into();
        config
    }

    #[test]
    fn non_vision_model_gets_text_note_with_remedy_and_summary() {
        let config = config_for("groq", "llama-3.1-8b-instant");
        let out = downgrade_for_non_vision_model(
            multimodal("Full page screenshot\nSaved to: /tmp/a.png"),
            &config,
            "browse captured a page screenshot",
            "Use `get_text` instead.",
        );
        match out {
            ToolOutput::Text(t) => {
                assert!(t.contains("does not support image input"), "got: {t}");
                assert!(t.contains("llama-3.1-8b-instant"), "model unnamed: {t}");
                assert!(t.contains("Use `get_text` instead."), "no remedy: {t}");
                assert!(t.contains("Saved to: /tmp/a.png"), "summary dropped: {t}");
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn vision_model_keeps_the_pixels() {
        let config = config_for("anthropic", "claude-sonnet-5");
        let out = downgrade_for_non_vision_model(
            multimodal("shot"),
            &config,
            "browse captured a page screenshot",
            "Use `get_text`.",
        );
        assert!(matches!(out, ToolOutput::Multimodal { .. }), "{out:?}");
    }

    #[test]
    fn image_url_part_counts_as_an_image() {
        let config = config_for("groq", "llama-3.1-8b-instant");
        let out = downgrade_for_non_vision_model(
            ToolOutput::Multimodal {
                text: "linked".into(),
                parts: vec![ContentPart::ImageUrl {
                    url: "https://example.com/a.png".into(),
                }],
            },
            &config,
            "browse captured a page screenshot",
            "Use `get_text`.",
        );
        assert!(matches!(out, ToolOutput::Text(_)), "{out:?}");
    }

    #[test]
    fn text_only_multimodal_output_passes_through() {
        // No pixels to lose — downgrading would only add noise.
        let config = config_for("groq", "llama-3.1-8b-instant");
        let out = downgrade_for_non_vision_model(
            ToolOutput::Multimodal {
                text: "just text".into(),
                parts: vec![ContentPart::Text("just text".into())],
            },
            &config,
            "browse captured a page screenshot",
            "Use `get_text`.",
        );
        assert!(matches!(out, ToolOutput::Multimodal { .. }), "{out:?}");
    }

    #[test]
    fn empty_summary_is_labelled_rather_than_blank() {
        let config = config_for("groq", "llama-3.1-8b-instant");
        let out = downgrade_for_non_vision_model(
            ToolOutput::Multimodal {
                text: "   ".into(),
                parts: vec![ContentPart::ImageBase64 {
                    media: MediaData {
                        mime_type: "image/png".into(),
                        data: "aGk=".into(),
                        filename: None,
                    },
                }],
            },
            &config,
            "browse captured a page screenshot",
            "Use `get_text`.",
        );
        match out {
            ToolOutput::Text(t) => assert!(t.contains("(no text detail)"), "got: {t}"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn unknown_or_unset_provider_defaults_to_vision_capable() {
        // Mirrors Provider::supports_vision's "avoid false negatives" stance.
        let mut config = Config::default();
        config.llm.provider = None;
        config.llm.model = "some-model".into();
        assert!(main_model_supports_vision(&config));
        config.llm.provider = Some("not-a-real-provider".into());
        assert!(main_model_supports_vision(&config));
        config.llm.provider = Some("   ".into());
        assert!(main_model_supports_vision(&config));
    }
}
