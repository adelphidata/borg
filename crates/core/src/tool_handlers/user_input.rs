use serde::{Deserialize, Serialize};
use tracing::instrument;

use crate::agent::AgentEvent;
use crate::types::{PlanStepStatus, ToolOutput};

use super::require_str_param;

/// A single selectable answer offered to the user for a `request_user_input` call.
///
/// `label` is shown to the user and is what gets returned as the answer when
/// selected. `description` is an optional hint rendered alongside the label.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserInputChoice {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Handle the `update_plan` tool: parse structured plan steps and emit a PlanUpdated event.
#[instrument(skip_all, fields(tool.name = "update_plan"))]
pub async fn handle_update_plan(
    args: &serde_json::Value,
    event_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
) -> anyhow::Result<ToolOutput> {
    let steps_val = args
        .get("steps")
        .ok_or_else(|| anyhow::anyhow!("Missing required parameter 'steps'"))?;
    let steps: Vec<crate::types::PlanStep> = serde_json::from_value(steps_val.clone())
        .map_err(|e| anyhow::anyhow!("Invalid steps format: {e}. Each step needs 'title' (string) and 'status' (pending|in_progress|completed)."))?;

    // Validate: at most one step may be in_progress
    let in_progress_count = steps
        .iter()
        .filter(|s| s.status == PlanStepStatus::InProgress)
        .count();
    if in_progress_count > 1 {
        return Ok(ToolOutput::Text(
            "Error: At most one step may be in_progress at a time.".to_string(),
        ));
    }

    let _ = event_tx.send(AgentEvent::PlanUpdated { steps }).await;

    Ok(ToolOutput::Text("Plan updated.".to_string()))
}

/// Parse the optional `choices` parameter into `Vec<UserInputChoice>`.
/// Labels must be non-empty and unique. Returns `Err` with a helpful message
/// the model can see and correct on retry.
fn parse_choices(args: &serde_json::Value) -> anyhow::Result<Vec<UserInputChoice>> {
    let Some(val) = args.get("choices") else {
        return Ok(Vec::new());
    };
    if val.is_null() {
        return Ok(Vec::new());
    }
    let parsed: Vec<UserInputChoice> = serde_json::from_value(val.clone()).map_err(|e| {
        anyhow::anyhow!(
            "Invalid 'choices' format: {e}. Each choice needs 'label' (string) and optional 'description' (string)."
        )
    })?;
    for c in &parsed {
        if c.label.trim().is_empty() {
            anyhow::bail!("'choices' contains an empty label");
        }
    }
    // Unique labels (case-sensitive).
    for i in 0..parsed.len() {
        for j in (i + 1)..parsed.len() {
            if parsed[i].label == parsed[j].label {
                anyhow::bail!("'choices' contains duplicate label: {:?}", parsed[i].label);
            }
        }
    }
    Ok(parsed)
}

/// Handle the `request_user_input` tool: prompt the user for input and block until they respond.
///
/// When `choices` is provided, the UI renders a selectable list and returns the chosen label.
/// `allow_custom` (default `true`) permits falling back to free-text entry.
#[instrument(skip_all, fields(tool.name = "request_user_input"))]
pub async fn handle_request_user_input(
    args: &serde_json::Value,
    event_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
) -> anyhow::Result<ToolOutput> {
    let prompt = require_str_param(args, "prompt")?;
    let choices = parse_choices(args)?;
    let allow_custom = args
        .get("allow_custom")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);

    let (respond_tx, respond_rx) = tokio::sync::oneshot::channel::<String>();
    let _ = event_tx
        .send(AgentEvent::UserInputRequest {
            prompt: prompt.to_string(),
            choices,
            allow_custom,
            respond: respond_tx,
        })
        .await;

    // Wait for user response with a 5-minute timeout
    match tokio::time::timeout(std::time::Duration::from_secs(300), respond_rx).await {
        Ok(Ok(response)) => Ok(ToolOutput::Text(response)),
        Ok(Err(_)) => Ok(ToolOutput::Text(
            "[No response received — channel closed]".to_string(),
        )),
        Err(_) => Ok(ToolOutput::Text(
            "[No response received — timed out after 5 minutes]".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn handle_request_user_input_requires_prompt() {
        let args = json!({});
        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(16);
        let result = handle_request_user_input(&args, &event_tx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn handle_request_user_input_emits_event_and_returns_response() {
        let args = json!({"prompt": "Which DB?"});
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(16);

        let handle = tokio::spawn(async move { handle_request_user_input(&args, &event_tx).await });

        let event = event_rx.recv().await.unwrap();
        match event {
            AgentEvent::UserInputRequest {
                prompt,
                choices,
                allow_custom,
                respond,
            } => {
                assert_eq!(prompt, "Which DB?");
                assert!(choices.is_empty());
                assert!(allow_custom);
                let _ = respond.send("PostgreSQL".to_string());
            }
            _ => panic!("expected UserInputRequest"),
        }

        let result = handle.await.unwrap().unwrap();
        match result {
            ToolOutput::Text(t) => assert_eq!(t, "PostgreSQL"),
            _ => panic!("expected Text output"),
        }
    }

    #[tokio::test]
    async fn handle_request_user_input_forwards_choices_and_allow_custom() {
        let args = json!({
            "prompt": "Pick one",
            "choices": [
                {"label": "Postgres", "description": "relational, strong"},
                {"label": "SQLite"}
            ],
            "allow_custom": false,
        });
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(16);

        let handle = tokio::spawn(async move { handle_request_user_input(&args, &event_tx).await });

        let event = event_rx.recv().await.unwrap();
        match event {
            AgentEvent::UserInputRequest {
                choices,
                allow_custom,
                respond,
                ..
            } => {
                assert_eq!(choices.len(), 2);
                assert_eq!(choices[0].label, "Postgres");
                assert_eq!(
                    choices[0].description.as_deref(),
                    Some("relational, strong")
                );
                assert_eq!(choices[1].label, "SQLite");
                assert_eq!(choices[1].description, None);
                assert!(!allow_custom);
                let _ = respond.send("Postgres".to_string());
            }
            _ => panic!("expected UserInputRequest"),
        }

        let result = handle.await.unwrap().unwrap();
        match result {
            ToolOutput::Text(t) => assert_eq!(t, "Postgres"),
            _ => panic!("expected Text output"),
        }
    }

    #[test]
    fn parse_choices_rejects_empty_and_duplicate_labels() {
        let empty = json!({"choices": [{"label": ""}]});
        assert!(parse_choices(&empty).is_err());

        let whitespace = json!({"choices": [{"label": "   "}]});
        assert!(parse_choices(&whitespace).is_err());

        let dup = json!({"choices": [{"label": "A"}, {"label": "A"}]});
        assert!(parse_choices(&dup).is_err());

        let missing_label = json!({"choices": [{"description": "no label here"}]});
        assert!(parse_choices(&missing_label).is_err());
    }

    #[test]
    fn parse_choices_absent_or_null_is_empty() {
        assert!(parse_choices(&json!({})).unwrap().is_empty());
        assert!(parse_choices(&json!({"choices": null})).unwrap().is_empty());
    }

    #[test]
    fn parse_choices_accepts_valid_list() {
        let v = json!({"choices": [{"label": "A"}, {"label": "B", "description": "the second"}]});
        let parsed = parse_choices(&v).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1].description.as_deref(), Some("the second"));
    }
}
