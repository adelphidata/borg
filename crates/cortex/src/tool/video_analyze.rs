//! `video_analyze` tool.
//!
//! Fetches a video from a URL (or decodes a `data:` URI) and returns it as a
//! multimodal tool result so the Gemini native transport can pass the bytes
//! through as `inline_data` on the next turn.
//!
//! Gemini-only: when the active LLM provider is not Gemini, the handler
//! returns a clear error so the model can route around. There is no
//! frame-extraction fallback — degraded "describe the first frame" output
//! would be silently lossy, and the route-around is cheap (the model can
//! delegate to a Gemini sub-agent or skip the call).

use anyhow::Result;
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::media;
use crate::types::{ContentPart, MediaData, ToolOutput};
use crate::url_policy::{self, ClassifyOutcome};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct VideoAnalyzeArgs {
    pub video_url: String,
    #[serde(default)]
    pub question: Option<String>,
}

pub(crate) async fn handle(args: &VideoAnalyzeArgs, config: &Config) -> Result<ToolOutput> {
    // AI-Studio-Gemini-only gate. Configured provider is the source of
    // truth — the LLM dispatcher may failover at runtime, but at tool-call
    // time the user ran us under whatever they configured, and that's what
    // tool routing should reflect. If someone is on OpenAI we'd rather
    // refuse than burn bandwidth fetching a video we can't render. Vertex
    // is called out by name because operators commonly assume Vertex-
    // Gemini works here — it does not until parity work lands.
    if !provider_is_gemini(config) {
        let configured = config
            .llm
            .provider
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .unwrap_or("(unset)");
        // Match every spelling `Provider::from_str` accepts for Vertex —
        // `vertex-ai` (canonical kebab), `vertex_ai`, `vertexai`, `vertex` —
        // case-insensitive. Normalising `-` → `_` collapses the canonical
        // form into the same key as the snake-case variant.
        let normalized = configured.replace('-', "_");
        let detail = if normalized.eq_ignore_ascii_case("vertex_ai")
            || normalized.eq_ignore_ascii_case("vertexai")
            || normalized.eq_ignore_ascii_case("vertex")
        {
            " Vertex-Gemini is not yet supported — see docs/specs/gemini-adapter-10-vertex-followup.md."
        } else {
            ""
        };
        return Ok(ToolOutput::Text(format!(
            "video_analyze requires AI Studio Gemini (configure `llm.provider = \"gemini\"`). \
             Configured provider: `{configured}`.{detail}"
        )));
    }

    let trimmed = args.video_url.trim();
    if trimmed.is_empty() {
        return Ok(ToolOutput::Text(
            "Error: video_url is required and must be a non-empty URL or data URI.".to_string(),
        ));
    }

    let max_bytes = config.media.max_video_bytes;

    let (raw_bytes, hint_mime, filename, source_label) = if trimmed.starts_with("data:") {
        match media::parse_data_uri(trimmed) {
            Ok(m) => {
                let engine = base64::engine::general_purpose::STANDARD;
                let bytes = match engine.decode(&m.data) {
                    Ok(b) => b,
                    Err(e) => {
                        return Ok(ToolOutput::Text(format!(
                            "Error: data URI base64 is invalid: {e}"
                        )));
                    }
                };
                if bytes.len() > max_bytes {
                    return Ok(ToolOutput::Text(format!(
                        "Error: video is {} MB; video_analyze caps at {} MB.",
                        bytes.len() / (1024 * 1024),
                        max_bytes / (1024 * 1024)
                    )));
                }
                (bytes, Some(m.mime_type), None, "data URI".to_string())
            }
            Err(e) => {
                return Ok(ToolOutput::Text(format!("Error: malformed data URI: {e}")));
            }
        }
    } else if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        match fetch_remote(trimmed, &config.security.url_policy, max_bytes).await {
            Ok(triple) => triple,
            Err(e) => return Ok(ToolOutput::Text(format!("Error: {e}"))),
        }
    } else {
        return Ok(ToolOutput::Text(
            "Error: video_url must be an http(s):// URL or a data: URI.".into(),
        ));
    };

    if raw_bytes.is_empty() {
        return Ok(ToolOutput::Text(format!(
            "Error: fetched {source_label} returned 0 bytes."
        )));
    }

    // Magic-bytes detection wins over the hint — a server can mislabel
    // content-type and we'd rather refuse than send garbage through Gemini.
    let mime = media::infer_mime_from_bytes(&raw_bytes)
        .map(std::string::ToString::to_string)
        .or(hint_mime)
        .unwrap_or_else(|| "application/octet-stream".to_string());

    if !media::is_supported_video(&mime) {
        return Ok(ToolOutput::Text(format!(
            "Error: content type '{mime}' is not a supported video format. Gemini accepts mp4, quicktime, webm, mkv."
        )));
    }

    let engine = base64::engine::general_purpose::STANDARD;
    let b64 = engine.encode(&raw_bytes);
    let raw_len = raw_bytes.len();

    let question_line = args
        .question
        .as_ref()
        .map(|q| q.trim())
        .filter(|q| !q.is_empty());

    let summary = match question_line {
        Some(q) => format!("Video from {source_label} ({raw_len} bytes). Question: {q}"),
        None => format!("Video from {source_label} ({raw_len} bytes)."),
    };

    let parts: Vec<ContentPart> = vec![
        ContentPart::Text(summary.clone()),
        ContentPart::VideoBase64 {
            media: MediaData {
                mime_type: mime,
                data: b64,
                filename,
            },
        },
    ];

    Ok(ToolOutput::Multimodal {
        text: summary,
        parts,
    })
}

/// Whether the configured LLM provider is Gemini (AI Studio's native path).
/// Vertex-Gemini is intentionally NOT treated as Gemini for this gate — its
/// URL/auth shape is different and `video_analyze` ships against AI Studio
/// first. Once Vertex-Gemini wire-format parity lands the gate can widen.
fn provider_is_gemini(config: &Config) -> bool {
    let raw = config.llm.provider.as_deref().unwrap_or("").trim();
    if raw.is_empty() {
        return false;
    }
    raw.eq_ignore_ascii_case("gemini")
}

/// Fetch a video over http(s) under url-policy + size caps. Mirrors
/// `vision_analyze::fetch_remote` so the SSRF/redirect/post-fetch checks
/// are identical — videos are bigger but the network attack surface is
/// the same.
async fn fetch_remote(
    url: &str,
    policy: &crate::config::UrlPolicyConfig,
    max_bytes: usize,
) -> Result<(Vec<u8>, Option<String>, Option<String>, String)> {
    let outcome = url_policy::classify(url, policy).await;
    let effective_url = match &outcome {
        ClassifyOutcome::Allow { effective_url } => effective_url.clone(),
        _ => {
            let msg = url_policy::describe_block_or_throttle(&outcome)
                .unwrap_or_else(|| "URL policy refused request".to_string());
            anyhow::bail!("{msg}");
        }
    };

    let redirect_policy_cfg = policy.clone();
    let redirect = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error("too many redirects");
        }
        let next_url = attempt.url().to_string();
        match url_policy::classify_offline(&next_url, &redirect_policy_cfg) {
            ClassifyOutcome::Allow { .. } => attempt.follow(),
            ClassifyOutcome::Block { reason, rule } => attempt.error(format!(
                "URL policy blocked redirect to {next_url}: {reason} (rule: {rule})"
            )),
            ClassifyOutcome::Throttle { .. } => attempt.follow(),
        }
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .redirect(redirect)
        .build()?;
    let resp = client.get(&effective_url).send().await?;

    let final_url = resp.url().to_string();
    let post = url_policy::classify_no_throttle(&final_url, policy).await;
    if !matches!(post, ClassifyOutcome::Allow { .. }) {
        let msg = url_policy::describe_block_or_throttle(&post)
            .unwrap_or_else(|| "URL policy refused final URL".to_string());
        anyhow::bail!("post-fetch: {msg} (final url: {final_url})");
    }

    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("HTTP {status} fetching video");
    }

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_string());

    if let Some(reported) = resp.content_length() {
        if reported as usize > max_bytes {
            anyhow::bail!(
                "video is {} MB; video_analyze caps at {} MB.",
                reported as usize / (1024 * 1024),
                max_bytes / (1024 * 1024)
            );
        }
    }

    let bytes = resp.bytes().await?.to_vec();
    if bytes.len() > max_bytes {
        anyhow::bail!(
            "video is {} MB; video_analyze caps at {} MB.",
            bytes.len() / (1024 * 1024),
            max_bytes / (1024 * 1024)
        );
    }

    let filename = url::Url::parse(&effective_url)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|mut s| s.next_back().map(std::string::ToString::to_string))
        })
        .filter(|s| !s.is_empty());

    Ok((
        bytes,
        content_type,
        filename,
        format!("URL {effective_url}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn gemini_config() -> Config {
        let mut c = Config::default();
        c.llm.provider = Some("gemini".into());
        c
    }

    #[tokio::test]
    async fn refuses_when_provider_is_not_gemini() {
        // Spec: video_analyze is Gemini-only. On any other provider the tool
        // returns a clean error so the model can re-route — no silent
        // degradation, no fork to a frame-extraction fallback.
        let mut c = Config::default();
        c.llm.provider = Some("openai".into());
        let args = VideoAnalyzeArgs {
            video_url: "data:video/mp4;base64,AAAA".into(),
            question: None,
        };
        let out = handle(&args, &c).await.unwrap();
        match out {
            ToolOutput::Text(t) => {
                assert!(t.contains("Gemini"), "got: {t}");
            }
            other => panic!("expected text error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn refuses_vertex_ai_with_pointer_to_followup_spec() {
        // Vertex-Gemini is the most common operator confusion: they have
        // GCP set up, they expect Gemini-on-Vertex to just work for video.
        // The error message must call out Vertex by name and point them at
        // the design follow-up rather than letting them think the tool is
        // broken.
        // Every alias `Provider::from_str` accepts — including the canonical
        // `vertex-ai` kebab form that the prior loop silently missed.
        for raw in [
            "vertex_ai",
            "vertex-ai",
            "vertex",
            "vertexai",
            "Vertex",
            "Vertex-AI",
            "VERTEX_AI",
        ] {
            let mut c = Config::default();
            c.llm.provider = Some(raw.into());
            let args = VideoAnalyzeArgs {
                video_url: "data:video/mp4;base64,AAAA".into(),
                question: None,
            };
            let out = handle(&args, &c).await.unwrap();
            let text = match out {
                ToolOutput::Text(t) => t,
                other => panic!("expected text error, got {other:?}"),
            };
            assert!(text.contains("AI Studio Gemini"), "raw={raw} got: {text}");
            assert!(
                text.contains("Vertex-Gemini is not yet supported"),
                "raw={raw} got: {text}"
            );
            assert!(
                text.contains("gemini-adapter-10-vertex-followup.md"),
                "raw={raw} got: {text}"
            );
        }
    }

    #[tokio::test]
    async fn refuses_when_provider_is_unset() {
        // Empty/missing provider must NOT default to "Gemini ok" — the gate
        // is positive (provider == gemini), not negative (provider != openai).
        let mut c = Config::default();
        c.llm.provider = None;
        let args = VideoAnalyzeArgs {
            video_url: "data:video/mp4;base64,AAAA".into(),
            question: None,
        };
        let out = handle(&args, &c).await.unwrap();
        match out {
            ToolOutput::Text(t) => assert!(t.contains("Gemini"), "got: {t}"),
            other => panic!("expected text error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_empty_video_url() {
        let args = VideoAnalyzeArgs {
            video_url: "   ".into(),
            question: None,
        };
        let out = handle(&args, &gemini_config()).await.unwrap();
        match out {
            ToolOutput::Text(t) => assert!(t.contains("video_url is required")),
            other => panic!("expected text error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_local_file_path() {
        let args = VideoAnalyzeArgs {
            video_url: "/tmp/movie.mp4".into(),
            question: None,
        };
        let out = handle(&args, &gemini_config()).await.unwrap();
        let text = match out {
            ToolOutput::Text(t) => t,
            other => panic!("expected text error, got {other:?}"),
        };
        assert!(
            text.contains("http(s)") || text.contains("data:"),
            "got: {text}"
        );
    }

    #[tokio::test]
    async fn rejects_non_video_mime() {
        // PNG magic bytes; ftyp absent. Hint says image/png. Must refuse.
        let png_bytes: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D,
        ];
        let engine = base64::engine::general_purpose::STANDARD;
        let b64 = engine.encode(png_bytes);
        let uri = format!("data:image/png;base64,{b64}");
        let args = VideoAnalyzeArgs {
            video_url: uri,
            question: None,
        };
        let out = handle(&args, &gemini_config()).await.unwrap();
        let text = match out {
            ToolOutput::Text(t) => t,
            other => panic!("expected text error, got {other:?}"),
        };
        assert!(text.contains("not a supported video format"), "got: {text}");
    }

    #[tokio::test]
    async fn accepts_tiny_mp4_data_uri_and_returns_video_part() {
        // Minimal mp4 magic-bytes prefix: 4 bytes of box size + `ftyp` + brand.
        // Real mp4s have an mdat box etc., but the MIME sniffer only needs
        // the ftyp header to classify, and the handler doesn't decode the
        // stream — it base64-encodes and forwards.
        let mp4_prefix: &[u8] = &[
            0x00, 0x00, 0x00, 0x18, // size
            b'f', b't', b'y', b'p', // ftyp
            b'm', b'p', b'4', b'2', // brand
            0x00, 0x00, 0x00, 0x00, // minor version
            b'i', b's', b'o', b'm', b'm', b'p', b'4', b'2',
        ];
        let engine = base64::engine::general_purpose::STANDARD;
        let b64 = engine.encode(mp4_prefix);
        let uri = format!("data:video/mp4;base64,{b64}");
        let args = VideoAnalyzeArgs {
            video_url: uri,
            question: Some("summarise".into()),
        };
        let out = handle(&args, &gemini_config()).await.unwrap();
        let (text, parts) = match out {
            ToolOutput::Multimodal { text, parts } => (text, parts),
            other => panic!("expected multimodal, got {other:?}"),
        };
        assert!(text.contains("Question: summarise"));
        assert_eq!(parts.len(), 2);
        let has_video = parts.iter().any(|p| {
            matches!(
                p,
                ContentPart::VideoBase64 { media } if media.mime_type == "video/mp4"
            )
        });
        assert!(has_video, "expected a VideoBase64 part: {parts:?}");
    }

    #[tokio::test]
    async fn rejects_oversized_data_uri() {
        let mut c = gemini_config();
        c.media.max_video_bytes = 1024; // 1 KB cap forces failure on any real video.
                                        // ftyp prefix to pass MIME check, padded to exceed cap.
        let mut bytes = vec![
            0x00, 0x00, 0x00, 0x18, b'f', b't', b'y', b'p', b'm', b'p', b'4', b'2',
        ];
        bytes.extend(std::iter::repeat_n(0u8, 2048));
        let engine = base64::engine::general_purpose::STANDARD;
        let b64 = engine.encode(&bytes);
        let uri = format!("data:video/mp4;base64,{b64}");
        let args = VideoAnalyzeArgs {
            video_url: uri,
            question: None,
        };
        let out = handle(&args, &c).await.unwrap();
        let text = match out {
            ToolOutput::Text(t) => t,
            other => panic!("expected text error, got {other:?}"),
        };
        assert!(text.contains("caps at"), "got: {text}");
    }

    #[test]
    fn deserialize_minimal_args() {
        let args: VideoAnalyzeArgs =
            serde_json::from_value(serde_json::json!({"video_url":"https://example.com/clip.mp4"}))
                .unwrap();
        assert_eq!(args.video_url, "https://example.com/clip.mp4");
        assert!(args.question.is_none());
    }
}
