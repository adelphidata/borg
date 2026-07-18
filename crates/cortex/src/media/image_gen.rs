use anyhow::{bail, Result};
use base64::Engine as _;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::config::ImageGenConfig;
use crate::constants;
use crate::media::core::ensure_success;

/// Supported image generation providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageGenProviderKind {
    OpenAi,
    Fal,
}

/// A configured image generation provider.
#[derive(Debug, Clone)]
pub struct ImageGenProvider {
    pub kind: ImageGenProviderKind,
    pub api_key: String,
    pub model: String,
}

/// Result of an image generation request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageResult {
    /// Base64-encoded image data (PNG).
    pub base64_data: String,
    /// Revised prompt (if the provider rewrote it).
    pub revised_prompt: Option<String>,
}

impl ImageGenProvider {
    /// Auto-detect the best available provider from config and environment.
    pub fn from_config(config: &ImageGenConfig) -> Option<Self> {
        let resolve_key =
            |var: &str| -> Option<String> { std::env::var(var).ok().filter(|k| !k.is_empty()) };

        // Explicit provider override
        if let Some(ref provider_name) = config.provider {
            return match provider_name.as_str() {
                "openai" => {
                    let env_var = config.api_key_env.as_deref().unwrap_or("OPENAI_API_KEY");
                    let api_key = resolve_key(env_var)?;
                    let model = config
                        .model
                        .clone()
                        .unwrap_or_else(|| "dall-e-3".to_string());
                    Some(Self {
                        kind: ImageGenProviderKind::OpenAi,
                        api_key,
                        model,
                    })
                }
                "fal" => {
                    let env_var = config.api_key_env.as_deref().unwrap_or("FAL_KEY");
                    let api_key = resolve_key(env_var)?;
                    let model = config
                        .model
                        .clone()
                        .unwrap_or_else(|| "fal-ai/flux/schnell".to_string());
                    Some(Self {
                        kind: ImageGenProviderKind::Fal,
                        api_key,
                        model,
                    })
                }
                _ => {
                    warn!("Unknown image generation provider: {provider_name}");
                    None
                }
            };
        }

        // Auto-detect from available API keys
        if let Some(key) = resolve_key("OPENAI_API_KEY") {
            return Some(Self {
                kind: ImageGenProviderKind::OpenAi,
                api_key: key,
                model: config
                    .model
                    .clone()
                    .unwrap_or_else(|| "dall-e-3".to_string()),
            });
        }

        if let Some(key) = resolve_key("FAL_KEY") {
            return Some(Self {
                kind: ImageGenProviderKind::Fal,
                api_key: key,
                model: config
                    .model
                    .clone()
                    .unwrap_or_else(|| "fal-ai/flux/schnell".to_string()),
            });
        }

        None
    }
}

/// Generate images from a text prompt.
pub async fn generate_image(
    provider: &ImageGenProvider,
    prompt: &str,
    size: Option<&str>,
    count: Option<u32>,
) -> Result<Vec<ImageResult>> {
    let count = count.unwrap_or(1).min(4);

    match provider.kind {
        ImageGenProviderKind::OpenAi => generate_openai(provider, prompt, size, count).await,
        ImageGenProviderKind::Fal => generate_fal(provider, prompt, size, count).await,
    }
}

// ── OpenAI DALL-E ──

#[derive(Deserialize)]
struct OpenAiImageResponse {
    data: Vec<OpenAiImageData>,
}

#[derive(Deserialize)]
struct OpenAiImageData {
    b64_json: Option<String>,
    revised_prompt: Option<String>,
}

async fn generate_openai(
    provider: &ImageGenProvider,
    prompt: &str,
    size: Option<&str>,
    count: u32,
) -> Result<Vec<ImageResult>> {
    let client = Client::new();
    let size = size.unwrap_or(constants::IMAGE_GEN_DEFAULT_SIZE);

    let body = serde_json::json!({
        "model": provider.model,
        "prompt": prompt,
        "n": count,
        "size": size,
        "response_format": "b64_json",
    });

    debug!(
        "OpenAI image gen: model={}, size={size}, n={count}",
        provider.model
    );

    let resp = client
        .post("https://api.openai.com/v1/images/generations")
        .header("Authorization", format!("Bearer {}", provider.api_key))
        .json(&body)
        .timeout(std::time::Duration::from_secs(
            crate::constants::IMAGE_GEN_TIMEOUT_SECS,
        ))
        .send()
        .await?;

    let resp = ensure_success(resp, "OpenAI image generation").await?;

    let data: OpenAiImageResponse = resp.json().await?;
    Ok(data
        .data
        .into_iter()
        .filter_map(|d| {
            d.b64_json.map(|b64| ImageResult {
                base64_data: b64,
                revised_prompt: d.revised_prompt,
            })
        })
        .collect())
}

// ── FAL (Flux) ──

#[derive(Deserialize)]
struct FalQueueResponse {
    request_id: String,
}

#[derive(Deserialize)]
struct FalStatusResponse {
    status: String,
    #[serde(default)]
    response_url: Option<String>,
}

#[derive(Deserialize)]
struct FalResultResponse {
    images: Vec<FalImage>,
}

#[derive(Deserialize)]
struct FalImage {
    url: String,
}

/// Validate that a model string is safe for URL interpolation.
fn validate_model_name(model: &str) -> Result<()> {
    if model.is_empty() {
        bail!("Model name is empty");
    }
    if model.contains("..") {
        bail!("Invalid model name (path traversal): {model}");
    }
    if model
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '/' || c == '.' || c == '_')
    {
        Ok(())
    } else {
        bail!("Invalid model name: {model}")
    }
}

/// Validate that a URL is under the fal.run domain.
fn is_fal_domain(url: &str) -> bool {
    url.starts_with("https://fal.run/")
        || url.starts_with("https://queue.fal.run/")
        || url.starts_with("https://storage.fal.run/")
        || url.starts_with("https://v3.fal.media/")
        || url.starts_with("https://fal-cdn.batuhan.me/") // legacy FAL CDN
}

async fn generate_fal(
    provider: &ImageGenProvider,
    prompt: &str,
    size: Option<&str>,
    count: u32,
) -> Result<Vec<ImageResult>> {
    validate_model_name(&provider.model)?;
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()?;

    let (width, height) = parse_size(size.unwrap_or(constants::IMAGE_GEN_DEFAULT_SIZE));

    let body = serde_json::json!({
        "prompt": prompt,
        "image_size": {
            "width": width,
            "height": height,
        },
        "num_images": count,
    });

    let queue_url = format!("https://queue.fal.run/{}", provider.model);
    debug!(
        "FAL image gen: model={}, size={width}x{height}, n={count}",
        provider.model
    );

    let resp = client
        .post(&queue_url)
        .header("Authorization", format!("Key {}", provider.api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .timeout(constants::DEFAULT_HTTP_TIMEOUT)
        .send()
        .await?;

    let resp = ensure_success(resp, "FAL queue submission").await?;

    let queue: FalQueueResponse = resp.json().await?;
    debug!("FAL queued request: {}", queue.request_id);

    // Poll for completion
    let status_url = format!(
        "https://queue.fal.run/{}/requests/{}",
        provider.model, queue.request_id
    );

    let mut attempts = 0;
    let result_url = loop {
        attempts += 1;
        if attempts > 60 {
            bail!("FAL image generation timed out after 60 poll attempts");
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let status_resp = client
            .get(format!("{status_url}/status"))
            .header("Authorization", format!("Key {}", provider.api_key))
            .send()
            .await?;

        let status: FalStatusResponse = status_resp.json().await?;
        match status.status.as_str() {
            "COMPLETED" => {
                // Validate response_url is under fal.run to prevent SSRF
                let url = status.response_url.unwrap_or(status_url.clone());
                if !is_fal_domain(&url) {
                    bail!("FAL returned untrusted response URL: {url}");
                }
                break url;
            }
            "FAILED" => bail!("FAL image generation failed"),
            _ => continue,
        }
    };

    // Fetch result (with auth, validated as fal.run domain)
    let result_resp = client
        .get(&result_url)
        .header("Authorization", format!("Key {}", provider.api_key))
        .send()
        .await?;

    let result: FalResultResponse = result_resp.json().await?;

    // Download images — no auth header on CDN URLs, with size cap and domain validation
    let mut images = Vec::new();
    for fal_img in result.images {
        if !is_fal_domain(&fal_img.url) {
            warn!("Skipping FAL image with untrusted CDN URL: {}", fal_img.url);
            continue;
        }
        match client.get(&fal_img.url).send().await {
            Ok(img_resp) if img_resp.status().is_success() => {
                let bytes = img_resp.bytes().await?;
                if bytes.len() > constants::MAX_IMAGE_FILE_SIZE {
                    warn!(
                        "Skipping oversized FAL image ({} bytes, max {})",
                        bytes.len(),
                        constants::MAX_IMAGE_FILE_SIZE
                    );
                    continue;
                }
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                images.push(ImageResult {
                    base64_data: b64,
                    revised_prompt: None,
                });
            }
            _ => {
                warn!("Failed to download FAL image from {}", fal_img.url);
            }
        }
    }

    Ok(images)
}

fn parse_size(size: &str) -> (u32, u32) {
    let parts: Vec<&str> = size.split('x').collect();
    if parts.len() == 2 {
        let w = parts[0].parse().unwrap_or(1024);
        let h = parts[1].parse().unwrap_or(1024);
        (w, h)
    } else {
        (1024, 1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_provider_unknown_returns_none() {
        let config = ImageGenConfig {
            enabled: true,
            provider: Some("unknown_provider".into()),
            ..Default::default()
        };
        assert!(ImageGenProvider::from_config(&config).is_none());
    }

    #[test]
    fn parse_size_valid() {
        assert_eq!(parse_size("1024x1024"), (1024, 1024));
        assert_eq!(parse_size("1792x1024"), (1792, 1024));
        assert_eq!(parse_size("512x512"), (512, 512));
    }

    #[test]
    fn parse_size_invalid_falls_back() {
        assert_eq!(parse_size("invalid"), (1024, 1024));
        assert_eq!(parse_size(""), (1024, 1024));
    }

    #[test]
    fn validate_model_name_valid() {
        assert!(validate_model_name("fal-ai/flux/schnell").is_ok());
        assert!(validate_model_name("dall-e-3").is_ok());
        assert!(validate_model_name("stable-diffusion_v1.5").is_ok());
    }

    #[test]
    fn validate_model_name_rejects_traversal() {
        assert!(validate_model_name("../../malicious").is_err());
        assert!(validate_model_name("model?param=val").is_err());
        assert!(validate_model_name("model#fragment").is_err());
        assert!(validate_model_name("").is_err());
    }

    #[test]
    fn is_fal_domain_valid() {
        assert!(is_fal_domain("https://queue.fal.run/some/path"));
        assert!(is_fal_domain("https://storage.fal.run/output/img.png"));
        assert!(is_fal_domain("https://v3.fal.media/files/img.png"));
    }

    #[test]
    fn is_fal_domain_rejects_other() {
        assert!(!is_fal_domain("https://evil.com/fake"));
        assert!(!is_fal_domain("http://localhost:8080"));
        assert!(!is_fal_domain("https://fal.run.evil.com/path"));
    }
}
