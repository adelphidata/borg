//! `vision_analyze` tool.
//!
//! Fetches an image from a URL (or decodes a `data:` URI) and returns it as a
//! multimodal tool result so providers that accept image content blocks in
//! tool results (Anthropic, Bedrock, Vertex-Anthropic) see the image inline
//! on the next turn. Text-only providers receive the human-readable summary.
//!
//! Local file paths are intentionally not accepted — `read_file` already
//! returns image bytes as a multimodal output and enforces blocked-paths.

use anyhow::Result;
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::constants;
use crate::media;
use crate::types::{ContentPart, MediaData, ToolOutput};
use crate::url_policy::{self, ClassifyOutcome};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct VisionAnalyzeArgs {
    pub image_url: String,
    #[serde(default)]
    pub question: Option<String>,
}

/// Hard ceiling on raw fetched bytes (matches read_file's 50 MB image cap).
const MAX_DOWNLOAD_BYTES: usize = constants::MAX_IMAGE_FILE_SIZE;

/// Target post-compression size for the base64 envelope. Gemini's tool-result
/// content-block ceiling is ~20 MB; 5 MB leaves headroom for prompt overhead.
const COMPRESSION_TARGET_BYTES: usize = 5 * 1024 * 1024;

pub(crate) async fn handle(args: &VisionAnalyzeArgs, config: &Config) -> Result<ToolOutput> {
    let trimmed = args.image_url.trim();
    if trimmed.is_empty() {
        return Ok(ToolOutput::Text(
            "Error: image_url is required and must be a non-empty URL or data URI.".to_string(),
        ));
    }

    // Branch by source: data URI vs http(s) URL. Local file paths are
    // routed back to `read_file` for blocked-paths enforcement.
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
                if bytes.len() > MAX_DOWNLOAD_BYTES {
                    return Ok(ToolOutput::Text(format!(
                        "Error: image is {} MB; vision_analyze caps at {} MB.",
                        bytes.len() / (1024 * 1024),
                        MAX_DOWNLOAD_BYTES / (1024 * 1024)
                    )));
                }
                (bytes, Some(m.mime_type), None, "data URI".to_string())
            }
            Err(e) => {
                return Ok(ToolOutput::Text(format!("Error: malformed data URI: {e}")));
            }
        }
    } else if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        match fetch_remote(trimmed, &config.security.url_policy).await {
            Ok(triple) => triple,
            Err(e) => return Ok(ToolOutput::Text(format!("Error: {e}"))),
        }
    } else {
        return Ok(ToolOutput::Text(
            "Error: image_url must be an http(s):// URL or a data: URI. For local files use read_file."
                .into(),
        ));
    };

    if raw_bytes.is_empty() {
        return Ok(ToolOutput::Text(format!(
            "Error: fetched {source_label} returned 0 bytes."
        )));
    }

    // MIME detection: magic bytes first (most authoritative), fall back to hint.
    let mime = media::infer_mime_from_bytes(&raw_bytes)
        .map(std::string::ToString::to_string)
        .or(hint_mime)
        .unwrap_or_else(|| "application/octet-stream".to_string());

    if !media::is_compressible_image(&mime) {
        return Ok(ToolOutput::Text(format!(
            "Error: content type '{mime}' is not a supported image format."
        )));
    }

    let engine = base64::engine::general_purpose::STANDARD;
    let b64 = engine.encode(&raw_bytes);
    let raw_len = raw_bytes.len();

    let (final_b64, final_mime) = match media::compress_image(&b64, &mime, COMPRESSION_TARGET_BYTES)
    {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!("vision_analyze: compression failed, sending raw image: {e}");
            (b64, mime)
        }
    };

    let question_line = args
        .question
        .as_ref()
        .map(|q| q.trim())
        .filter(|q| !q.is_empty());

    let summary = match question_line {
        Some(q) => format!("Image from {source_label} ({raw_len} bytes). Question: {q}"),
        None => format!("Image from {source_label} ({raw_len} bytes)."),
    };

    let parts: Vec<ContentPart> = vec![
        ContentPart::Text(summary.clone()),
        ContentPart::ImageBase64 {
            media: MediaData {
                mime_type: final_mime,
                data: final_b64,
                filename,
            },
        },
    ];

    Ok(ToolOutput::Multimodal {
        text: summary,
        parts,
    })
}

/// Fetch an image over http(s) under url-policy + size caps.
async fn fetch_remote(
    url: &str,
    policy: &crate::config::UrlPolicyConfig,
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

    // Per-hop redirect re-validation — without this a vetted public host
    // could 30x to 169.254.169.254 / RFC1918 / loopback and we'd silently
    // return the body. Mirrors `web::web_fetch`.
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
        .timeout(std::time::Duration::from_secs(60))
        .redirect(redirect)
        .build()?;
    let resp = client.get(&effective_url).send().await?;

    // Post-fetch DNS-rebinding recheck on the final URL — even a 200 with no
    // redirects can resolve to a different IP at connect time than at
    // pre-flight. Same defence `web_fetch` uses.
    let final_url = resp.url().to_string();
    let post = url_policy::classify_no_throttle(&final_url, policy).await;
    if !matches!(post, ClassifyOutcome::Allow { .. }) {
        let msg = url_policy::describe_block_or_throttle(&post)
            .unwrap_or_else(|| "URL policy refused final URL".to_string());
        anyhow::bail!("post-fetch: {msg} (final url: {final_url})");
    }

    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("HTTP {status} fetching image");
    }

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_string());

    // Inspect content-length when the server reports it so we reject oversized
    // payloads before streaming the body.
    if let Some(reported) = resp.content_length() {
        if reported as usize > MAX_DOWNLOAD_BYTES {
            anyhow::bail!(
                "image is {} MB; vision_analyze caps at {} MB.",
                reported as usize / (1024 * 1024),
                MAX_DOWNLOAD_BYTES / (1024 * 1024)
            );
        }
    }

    let bytes = resp.bytes().await?.to_vec();
    if bytes.len() > MAX_DOWNLOAD_BYTES {
        anyhow::bail!(
            "image is {} MB; vision_analyze caps at {} MB.",
            bytes.len() / (1024 * 1024),
            MAX_DOWNLOAD_BYTES / (1024 * 1024)
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

    fn test_config() -> Config {
        Config::default()
    }

    #[tokio::test]
    async fn rejects_empty_image_url() {
        let args = VisionAnalyzeArgs {
            image_url: "   ".into(),
            question: None,
        };
        let out = handle(&args, &test_config()).await.unwrap();
        match out {
            ToolOutput::Text(t) => assert!(t.contains("image_url is required")),
            other => panic!("expected text error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_local_file_path() {
        let args = VisionAnalyzeArgs {
            image_url: "/tmp/x.png".into(),
            question: None,
        };
        let out = handle(&args, &test_config()).await.unwrap();
        let text = match out {
            ToolOutput::Text(t) => t,
            other => panic!("expected text error, got {other:?}"),
        };
        assert!(text.contains("read_file"), "got: {text}");
    }

    #[tokio::test]
    async fn rejects_malformed_data_uri() {
        let args = VisionAnalyzeArgs {
            image_url: "data:image/png;base64".into(),
            question: None,
        };
        let out = handle(&args, &test_config()).await.unwrap();
        let text = match out {
            ToolOutput::Text(t) => t,
            other => panic!("expected text error, got {other:?}"),
        };
        assert!(text.contains("malformed data URI"), "got: {text}");
    }

    #[tokio::test]
    async fn rejects_non_image_data_uri() {
        // Valid data URI shape but the decoded bytes don't match any image
        // magic bytes and the mime hint is text/plain — must reject.
        let engine = base64::engine::general_purpose::STANDARD;
        let payload = engine.encode("hello world".as_bytes());
        let uri = format!("data:text/plain;base64,{payload}");
        let args = VisionAnalyzeArgs {
            image_url: uri,
            question: None,
        };
        let out = handle(&args, &test_config()).await.unwrap();
        let text = match out {
            ToolOutput::Text(t) => t,
            other => panic!("expected text error, got {other:?}"),
        };
        assert!(text.contains("not a supported image format"), "got: {text}");
    }

    #[tokio::test]
    async fn accepts_tiny_png_data_uri_and_returns_multimodal() {
        // 1x1 transparent PNG.
        let png_bytes: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x62, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        let engine = base64::engine::general_purpose::STANDARD;
        let b64 = engine.encode(png_bytes);
        let uri = format!("data:image/png;base64,{b64}");
        let args = VisionAnalyzeArgs {
            image_url: uri,
            question: Some("what's in this image?".into()),
        };
        let out = handle(&args, &test_config()).await.unwrap();
        let (text, parts) = match out {
            ToolOutput::Multimodal { text, parts } => (text, parts),
            other => panic!("expected multimodal, got {other:?}"),
        };
        assert!(text.contains("Question: what's in this image?"));
        assert_eq!(parts.len(), 2);
        let has_image = parts.iter().any(|p| matches!(p, ContentPart::ImageBase64 { media } if media.mime_type.starts_with("image/")));
        assert!(has_image, "expected an ImageBase64 part: {parts:?}");
    }

    #[tokio::test]
    async fn rejects_oversized_data_uri() {
        // Build a ~51 MB base64 payload of zero bytes. Magic-bytes detection
        // will fail before size check fires for non-image bytes — but the size
        // check runs at decode time, so we must use image magic bytes prefix.
        // To keep the test cheap, encode raw bytes large enough to exceed cap.
        let big = vec![0u8; MAX_DOWNLOAD_BYTES + 1];
        let engine = base64::engine::general_purpose::STANDARD;
        let b64 = engine.encode(&big);
        let uri = format!("data:image/png;base64,{b64}");
        let args = VisionAnalyzeArgs {
            image_url: uri,
            question: None,
        };
        let out = handle(&args, &test_config()).await.unwrap();
        let text = match out {
            ToolOutput::Text(t) => t,
            other => panic!("expected text error, got {other:?}"),
        };
        assert!(text.contains("caps at"), "got: {text}");
    }

    #[test]
    fn deserialize_minimal_args() {
        let args: VisionAnalyzeArgs =
            serde_json::from_value(serde_json::json!({"image_url":"https://example.com/cat.png"}))
                .unwrap();
        assert_eq!(args.image_url, "https://example.com/cat.png");
        assert!(args.question.is_none());
    }

    #[test]
    fn deserialize_with_question() {
        let args: VisionAnalyzeArgs = serde_json::from_value(
            serde_json::json!({"image_url":"https://example.com/x.jpg","question":"describe"}),
        )
        .unwrap();
        assert_eq!(args.question.as_deref(), Some("describe"));
    }
}
