use anyhow::{bail, Context, Result};

use crate::config::LlmConfig;

const DEFAULT_LOCATION: &str = "global";

pub(crate) fn is_anthropic_model(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.starts_with("anthropic/") || m.starts_with("claude-")
}

pub(crate) fn compose_gemini_url(cfg: &LlmConfig) -> Result<String> {
    let (project, location) = project_and_location(cfg)?;
    Ok(format!(
        "https://aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/endpoints/openapi/chat/completions"
    ))
}

pub(crate) fn compose_anthropic_url(cfg: &LlmConfig, model: &str, stream: bool) -> Result<String> {
    let (project, location) = project_and_location(cfg)?;
    let model = model
        .strip_prefix("anthropic/")
        .unwrap_or(model)
        .trim()
        .to_string();
    validate_path_segment(&model, "Vertex AI model")?;
    let action = if stream {
        "streamRawPredict"
    } else {
        "rawPredict"
    };
    Ok(format!(
        "https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/publishers/anthropic/models/{model}:{action}"
    ))
}

fn project_and_location(cfg: &LlmConfig) -> Result<(String, String)> {
    let project = if let Some(project) = cfg
        .vertex_project
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        project.to_string()
    } else {
        std::env::var("GOOGLE_CLOUD_PROJECT")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .context(
                "Vertex AI: missing `llm.vertex_project` (or GOOGLE_CLOUD_PROJECT environment variable)",
            )?
    };
    let location = cfg
        .vertex_location
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(DEFAULT_LOCATION)
        .to_string();
    validate_project(&project)?;
    validate_path_segment(&location, "Vertex AI location")?;
    Ok((project, location))
}

fn validate_project(project: &str) -> Result<()> {
    if project
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == ':')
    {
        Ok(())
    } else {
        bail!("Vertex AI project contains invalid characters");
    }
}

fn validate_path_segment(value: &str, label: &str) -> Result<()> {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        Ok(())
    } else {
        bail!("{label} contains invalid characters");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> LlmConfig {
        let mut cfg = LlmConfig::default();
        cfg.vertex_project = Some("acme-prod".to_string());
        cfg.vertex_location = Some("us-central1".to_string());
        cfg
    }

    #[test]
    fn composes_vertex_gemini_openai_compat_url() {
        assert_eq!(
            compose_gemini_url(&cfg()).unwrap(),
            "https://aiplatform.googleapis.com/v1/projects/acme-prod/locations/us-central1/endpoints/openapi/chat/completions"
        );
    }

    #[test]
    fn composes_vertex_anthropic_stream_url() {
        assert_eq!(
            compose_anthropic_url(&cfg(), "anthropic/claude-sonnet-4", true).unwrap(),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/acme-prod/locations/us-central1/publishers/anthropic/models/claude-sonnet-4:streamRawPredict"
        );
    }

    #[test]
    fn rejects_path_injection() {
        let err = compose_anthropic_url(&cfg(), "claude/sonnet", true).unwrap_err();
        assert!(err.to_string().contains("model"));
    }
}
