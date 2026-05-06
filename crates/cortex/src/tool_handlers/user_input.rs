use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tracing::instrument;

use crate::agent::AgentEvent;
use crate::types::{PlanStepStatus, ToolOutput};

/// A single selectable option offered for a `request_user_input` question.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuestionOption {
    /// User-facing label (1-5 words). Suffix the recommended option with
    /// `(Recommended)`. Returned verbatim to the LLM as the answer.
    pub label: String,
    /// One-sentence explanation of the tradeoff for this option.
    pub description: String,
}

/// A single question in a `request_user_input` batch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Question {
    /// Stable snake_case id used to map the answer back when serialised to the LLM.
    pub id: String,
    /// Short header chip shown in the UI (≤ 12 chars, e.g. "Timezone").
    pub header: String,
    /// Single-sentence prompt shown to the user.
    pub question: String,
    /// 2-4 mutually-exclusive options. The UI auto-appends an "Other" free-form
    /// row, so callers should never include one explicitly.
    pub options: Vec<QuestionOption>,
    /// `true` when the UI should allow multiple selections. Defaults to single-select.
    #[serde(default, skip_serializing_if = "is_false")]
    pub multi_select: bool,
}

// `serde(skip_serializing_if = "...")` requires a `fn(&T) -> bool`; the
// trivially_copy_pass_by_ref lint is a false positive in that signature.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

/// Selected answer(s) for a single question, plus an optional free-form note.
///
/// `answers` holds the picked option labels in selection order. If the user
/// typed a free-form note in the Tab-toggled notes box, it is appended as a
/// trailing element with the `user_note: ` prefix so the LLM receives a flat
/// `Vec<String>` regardless of whether notes were used.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Answer {
    pub answers: Vec<String>,
}

/// Maximum number of questions the LLM is allowed to batch in one call.
pub const MAX_QUESTIONS: usize = 4;
/// Maximum byte length of a question's `header` chip label.
pub const MAX_HEADER_CHARS: usize = 12;
/// Min/max number of caller-provided options per question (the "Other"
/// fallback is appended client-side and is not counted here).
pub const MIN_OPTIONS: usize = 2;
pub const MAX_OPTIONS: usize = 4;

/// Sentinel prefix used to round-trip free-form notes back to the LLM as part
/// of the flat `Vec<String>` answer payload.
pub const USER_NOTE_PREFIX: &str = "user_note: ";

/// Sentinel "Other" label appended client-side. Kept here so handler/UI agree
/// on the exact spelling.
pub const OTHER_LABEL: &str = "Other";

/// Returned per-question when the user dismisses the widget without answering.
pub const INTERRUPTED_MARKER: &str = "[interrupted]";

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

    if let Err(e) = event_tx.send(AgentEvent::PlanUpdated { steps }).await {
        tracing::warn!("Failed to dispatch PlanUpdated event: {e}");
    }

    Ok(ToolOutput::Text("Plan updated.".to_string()))
}

/// Parse and validate the `questions` array from raw tool args.
///
/// Validates per-question shape (snake_case id, header length, option count +
/// uniqueness), then is consumed by the handler before being emitted to the UI.
/// Returns `Err` with a message the model can see and correct on retry.
pub fn parse_questions(args: &serde_json::Value) -> anyhow::Result<Vec<Question>> {
    let val = args
        .get("questions")
        .ok_or_else(|| anyhow::anyhow!("Missing required parameter 'questions'"))?;
    let questions: Vec<Question> = serde_json::from_value(val.clone()).map_err(|e| {
        anyhow::anyhow!(
            "Invalid 'questions' format: {e}. Each question needs 'id', 'header', 'question', and 'options' (array of {{label, description}})."
        )
    })?;

    if questions.is_empty() {
        anyhow::bail!("'questions' must contain at least one question");
    }
    if questions.len() > MAX_QUESTIONS {
        anyhow::bail!(
            "'questions' may contain at most {MAX_QUESTIONS} questions (got {})",
            questions.len()
        );
    }

    let mut seen_ids = std::collections::HashSet::new();
    for (i, q) in questions.iter().enumerate() {
        let qref = format!("question[{i}]");
        if q.id.trim().is_empty() {
            anyhow::bail!("{qref}.id must be non-empty");
        }
        if !q
            .id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            anyhow::bail!("{qref}.id must be snake_case (got {:?})", q.id);
        }
        if !seen_ids.insert(q.id.clone()) {
            anyhow::bail!("{qref}.id duplicates another question id: {:?}", q.id);
        }
        if q.header.trim().is_empty() {
            anyhow::bail!("{qref}.header must be non-empty");
        }
        if q.header.chars().count() > MAX_HEADER_CHARS {
            anyhow::bail!(
                "{qref}.header must be {MAX_HEADER_CHARS} characters or fewer (got {})",
                q.header.chars().count()
            );
        }
        if q.question.trim().is_empty() {
            anyhow::bail!("{qref}.question must be non-empty");
        }
        if q.options.len() < MIN_OPTIONS {
            anyhow::bail!(
                "{qref} requires at least {MIN_OPTIONS} options (got {})",
                q.options.len()
            );
        }
        if q.options.len() > MAX_OPTIONS {
            anyhow::bail!(
                "{qref} accepts at most {MAX_OPTIONS} options (got {})",
                q.options.len()
            );
        }
        let mut seen_labels = std::collections::HashSet::new();
        for (j, opt) in q.options.iter().enumerate() {
            if opt.label.trim().is_empty() {
                anyhow::bail!("{qref}.options[{j}].label must be non-empty");
            }
            if opt.label == OTHER_LABEL {
                anyhow::bail!(
                    "{qref}.options[{j}].label must not be {OTHER_LABEL:?} — the UI appends one automatically"
                );
            }
            if !seen_labels.insert(opt.label.clone()) {
                anyhow::bail!(
                    "{qref}.options[{j}].label duplicates another option: {:?}",
                    opt.label
                );
            }
        }
    }

    Ok(questions)
}

/// Handle the `request_user_input` tool: ask the user 1-4 short multiple-choice
/// questions and block until answers come back.
///
/// On success, returns the LLM-facing JSON `{ "answers": { "<id>": { "answers": [...] } } }`.
/// On timeout/cancel, returns a marker string the model can reason about.
#[instrument(skip_all, fields(tool.name = "request_user_input"))]
pub async fn handle_request_user_input(
    args: &serde_json::Value,
    event_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
) -> anyhow::Result<ToolOutput> {
    let questions = parse_questions(args)?;

    let (respond_tx, respond_rx) = tokio::sync::oneshot::channel::<HashMap<String, Answer>>();
    if let Err(e) = event_tx
        .send(AgentEvent::UserInputRequest {
            questions: questions.clone(),
            respond: respond_tx,
        })
        .await
    {
        anyhow::bail!("Failed to dispatch UserInputRequest event: {e}");
    }

    // Wait for user response with a 5-minute timeout.
    let answers = match tokio::time::timeout(std::time::Duration::from_secs(300), respond_rx).await
    {
        Ok(Ok(map)) => map,
        Ok(Err(_)) => {
            return Ok(ToolOutput::Text(
                "[No response received — channel closed]".to_string(),
            ));
        }
        Err(_) => {
            return Ok(ToolOutput::Text(
                "[No response received — timed out after 5 minutes]".to_string(),
            ));
        }
    };

    Ok(ToolOutput::Text(serialize_answers(&questions, &answers)))
}

/// Serialise the answer map into the JSON shape the LLM expects, preserving
/// question order so the model can read answers back deterministically.
///
/// The default `serde_json::Map` does not preserve insertion order without the
/// `preserve_order` feature, and we don't want to flip that workspace-wide
/// flag for a single tool. So we assemble the JSON string by hand: each
/// question's id and answer payload are individually serialised (which gives
/// proper escaping) and concatenated in question order.
pub fn serialize_answers(questions: &[Question], answers: &HashMap<String, Answer>) -> String {
    let mut s = String::from("{\"answers\":{");
    for (i, q) in questions.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        // String/Answer serialization is infallible — neither contains a Map
        // with non-string keys, a non-finite float, or any other shape
        // serde_json refuses. A failure here would mean a serde upstream bug,
        // not a runtime condition we can recover from, so panic loudly rather
        // than ship malformed JSON to the model.
        let key = match serde_json::to_string(&q.id) {
            Ok(k) => k,
            Err(e) => unreachable!("String serialization cannot fail: {e}"),
        };
        s.push_str(&key);
        s.push(':');
        let entry = answers.get(&q.id).cloned().unwrap_or_default();
        let value = match serde_json::to_string(&entry) {
            Ok(v) => v,
            Err(e) => unreachable!("Answer serialization cannot fail: {e}"),
        };
        s.push_str(&value);
    }
    s.push_str("}}");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_args(extra: serde_json::Value) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        obj.insert("questions".into(), extra);
        serde_json::Value::Object(obj)
    }

    fn one_valid_question() -> serde_json::Value {
        json!({
            "id": "color",
            "header": "Color",
            "question": "Pick a color",
            "options": [
                {"label": "Red", "description": "warm"},
                {"label": "Blue", "description": "cool"}
            ]
        })
    }

    #[tokio::test]
    async fn handle_request_user_input_emits_questions_and_returns_serialized_answers() {
        let args = sample_args(json!([
            {
                "id": "timezone",
                "header": "Timezone",
                "question": "Which timezone?",
                "options": [
                    {"label": "America/Los_Angeles", "description": "Pacific"},
                    {"label": "Europe/Berlin", "description": "Central Europe"}
                ]
            },
            {
                "id": "channels",
                "header": "Channels",
                "question": "Where should I reach you?",
                "multi_select": true,
                "options": [
                    {"label": "Telegram", "description": "fast push"},
                    {"label": "Slack", "description": "work hours"}
                ]
            }
        ]));
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(16);

        let handle = tokio::spawn(async move { handle_request_user_input(&args, &event_tx).await });

        let event = event_rx.recv().await.expect("event sent");
        let respond = match event {
            AgentEvent::UserInputRequest { questions, respond } => {
                assert_eq!(questions.len(), 2);
                assert_eq!(questions[0].id, "timezone");
                assert!(questions[1].multi_select);
                respond
            }
            _ => panic!("expected UserInputRequest"),
        };

        let mut map: HashMap<String, Answer> = HashMap::new();
        map.insert(
            "timezone".into(),
            Answer {
                answers: vec!["America/Los_Angeles".into()],
            },
        );
        map.insert(
            "channels".into(),
            Answer {
                answers: vec!["Telegram".into(), "Slack".into()],
            },
        );
        respond.send(map).expect("send answers");

        let result = handle.await.unwrap().unwrap();
        let ToolOutput::Text(payload) = result else {
            panic!("expected Text output");
        };
        // Order is preserved deterministically in the raw payload string —
        // timezone (the first question in the batch) appears before channels,
        // independent of the unordered HashMap that fed `serialize_answers`.
        let tz_pos = payload.find("\"timezone\"").expect("timezone in payload");
        let ch_pos = payload.find("\"channels\"").expect("channels in payload");
        assert!(
            tz_pos < ch_pos,
            "expected timezone before channels, got: {payload}"
        );
        // Re-parse for value-level assertions; serde_json::Value's Map
        // ordering doesn't matter for these.
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let answers = parsed.get("answers").unwrap().as_object().unwrap();
        assert_eq!(
            answers["timezone"]["answers"][0].as_str().unwrap(),
            "America/Los_Angeles"
        );
        assert_eq!(answers["channels"]["answers"][1].as_str().unwrap(), "Slack");
    }

    #[test]
    fn parse_questions_rejects_empty_questions_array() {
        let err = parse_questions(&sample_args(json!([]))).unwrap_err();
        assert!(err.to_string().contains("at least one"));
    }

    #[test]
    fn parse_questions_rejects_more_than_max_questions() {
        let q = one_valid_question();
        // MAX_QUESTIONS + 1 entries → over the cap.
        let arr: Vec<serde_json::Value> = (0..=MAX_QUESTIONS)
            .map(|i| {
                let mut item = q.clone();
                item["id"] = json!(format!("q{i}"));
                item
            })
            .collect();
        let err = parse_questions(&sample_args(serde_json::Value::Array(arr))).unwrap_err();
        assert!(err
            .to_string()
            .contains(&format!("at most {MAX_QUESTIONS}")));
    }

    #[test]
    fn parse_questions_rejects_question_with_fewer_than_two_options() {
        let bad = json!([{
            "id": "x", "header": "X", "question": "?",
            "options": [{"label": "Only", "description": "lonely"}]
        }]);
        let err = parse_questions(&sample_args(bad)).unwrap_err();
        assert!(err.to_string().contains("at least 2 options"));
    }

    #[test]
    fn parse_questions_rejects_duplicate_option_labels_within_a_question() {
        let bad = json!([{
            "id": "x", "header": "X", "question": "?",
            "options": [
                {"label": "Same", "description": "a"},
                {"label": "Same", "description": "b"}
            ]
        }]);
        let err = parse_questions(&sample_args(bad)).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("duplicate"));
    }

    #[test]
    fn parse_questions_rejects_header_over_12_chars() {
        let bad = json!([{
            "id": "x", "header": "ThisIsTooLongHeader", "question": "?",
            "options": [
                {"label": "A", "description": "a"},
                {"label": "B", "description": "b"}
            ]
        }]);
        let err = parse_questions(&sample_args(bad)).unwrap_err();
        assert!(err.to_string().contains("12 characters"));
    }

    #[test]
    fn parse_questions_rejects_other_label_collision() {
        // "Other" is appended client-side; the model must not provide it.
        let bad = json!([{
            "id": "x", "header": "X", "question": "?",
            "options": [
                {"label": "Other", "description": "free form"},
                {"label": "B", "description": "b"}
            ]
        }]);
        let err = parse_questions(&sample_args(bad)).unwrap_err();
        assert!(err.to_string().contains("Other"));
    }

    #[test]
    fn parse_questions_rejects_non_snake_case_id() {
        let bad = json!([{
            "id": "TimeZone", "header": "X", "question": "?",
            "options": [
                {"label": "A", "description": "a"},
                {"label": "B", "description": "b"}
            ]
        }]);
        let err = parse_questions(&sample_args(bad)).unwrap_err();
        assert!(err.to_string().contains("snake_case"));
    }

    #[test]
    fn parse_questions_accepts_minimal_valid_batch() {
        let parsed = parse_questions(&sample_args(json!([one_valid_question()]))).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, "color");
        assert!(!parsed[0].multi_select);
    }

    #[test]
    fn notes_are_serialized_with_user_note_prefix() {
        // Single question; UI returns the picked label plus a free-form note
        // already prefixed with USER_NOTE_PREFIX. Round-trip must surface that
        // verbatim in the JSON the LLM sees.
        let questions = vec![Question {
            id: "color".into(),
            header: "Color".into(),
            question: "Pick".into(),
            options: vec![
                QuestionOption {
                    label: "Red".into(),
                    description: "warm".into(),
                },
                QuestionOption {
                    label: "Blue".into(),
                    description: "cool".into(),
                },
            ],
            multi_select: false,
        }];
        let mut map = HashMap::new();
        map.insert(
            "color".to_string(),
            Answer {
                answers: vec![
                    "Red".into(),
                    format!("{USER_NOTE_PREFIX}I actually prefer crimson"),
                ],
            },
        );
        let payload = serialize_answers(&questions, &map);
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let answers = &parsed["answers"]["color"]["answers"];
        assert_eq!(answers[0].as_str().unwrap(), "Red");
        assert_eq!(
            answers[1].as_str().unwrap(),
            "user_note: I actually prefer crimson"
        );
    }

    #[test]
    fn serialize_answers_preserves_question_order_and_fills_missing_ids() {
        let questions = vec![
            Question {
                id: "a".into(),
                header: "A".into(),
                question: "?".into(),
                options: vec![
                    QuestionOption {
                        label: "X".into(),
                        description: "x".into(),
                    },
                    QuestionOption {
                        label: "Y".into(),
                        description: "y".into(),
                    },
                ],
                multi_select: false,
            },
            Question {
                id: "b".into(),
                header: "B".into(),
                question: "?".into(),
                options: vec![
                    QuestionOption {
                        label: "X".into(),
                        description: "x".into(),
                    },
                    QuestionOption {
                        label: "Y".into(),
                        description: "y".into(),
                    },
                ],
                multi_select: false,
            },
        ];
        // Only answer "b" — "a" must still appear with an empty answers list,
        // so the model never has to guess whether a key was dropped.
        let mut map = HashMap::new();
        map.insert(
            "b".into(),
            Answer {
                answers: vec!["X".into()],
            },
        );
        let payload = serialize_answers(&questions, &map);
        // Verify question order in the raw string; the in-memory parsed map
        // doesn't preserve insertion order without the preserve_order feature.
        let a_pos = payload.find("\"a\"").expect("a in payload");
        let b_pos = payload.find("\"b\"").expect("b in payload");
        assert!(a_pos < b_pos, "expected a before b, got: {payload}");
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        // Missing answers fill with an empty list rather than dropping the key.
        assert!(parsed["answers"]["a"]["answers"]
            .as_array()
            .unwrap()
            .is_empty());
    }
}
