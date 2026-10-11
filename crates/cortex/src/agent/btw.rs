//! Ephemeral `/btw` side-question driver.
//!
//! Runs a tool-less LLM turn against a read-only snapshot of the current
//! session's transcript. Nothing is persisted (no DB writes, no session file,
//! no hooks). Callers (TUI popup, gateway threaded reply) display the answer
//! out-of-band so it does not derail the main conversation.

use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::llm::{LlmClient, StreamEvent};
use crate::types::Message;

/// Upper bound on how long a single `/btw` may stream before we give up. If
/// the provider genuinely needs longer the answer is unlikely to feel
/// "non-blocking" anyway, so we cut it off and surface whatever text we
/// accumulated.
pub const BTW_TIMEOUT: Duration = Duration::from_secs(60);

/// Prefix prepended to the user's question when we send it to the LLM. Makes
/// the model's framing explicit: side question, no tools, concise answer.
const BTW_PROMPT_PREFIX: &str =
    "[Ephemeral /btw side question. Answer using the conversation context above. \
     No tools are available. Be direct and concise.]\n\n";

/// Compact system prompt used for `/btw`. We deliberately avoid the full
/// `Agent::build_system_prompt` machinery — that loads skills, tool docs, and
/// workflow guidance that are all irrelevant here (no tools are exposed) and
/// would waste tokens on every side question.
async fn build_btw_system_prompt(agent_name: Option<&str>) -> String {
    let identity = crate::identity::load_identity_named(agent_name).unwrap_or_else(|e| {
        tracing::warn!("/btw: failed to load identity, using default: {e}");
        "You are Borg, a personal AI assistant.\n".to_string()
    });

    let memory_preamble = match crate::memory::load_memory_context_db(2000).await {
        Ok(m) if !m.is_empty() => {
            format!("\n<long_term_memory trust=\"stored\">\n{m}\n</long_term_memory>\n")
        }
        Ok(_) => String::new(),
        Err(e) => {
            tracing::warn!("/btw: failed to load memory, continuing without: {e}");
            String::new()
        }
    };

    format!(
        "{identity}\n\n\
         You are currently answering a `/btw` (by-the-way) side question. \
         This is an ephemeral side channel — nothing you say here is persisted \
         to the main conversation and no tools are available. Answer using only \
         the transcript snapshot and memory provided. Keep the answer tight: \
         a few sentences at most unless the user explicitly asks for depth.\n\
         {memory_preamble}"
    )
}

/// Run a `/btw` turn.
///
/// Spawns no side effects: no DB writes, no session persistence, no hooks.
/// Returns the assistant's text response on success.
///
/// `cancel` lets the caller abort a stale request when a new `/btw` fires;
/// cancellation resolves the future promptly with an error so the caller can
/// show a friendly message rather than leaving a spinner spinning.
pub async fn run_btw(
    config: Config,
    question: String,
    transcript_snapshot: Vec<Message>,
    cancel: CancellationToken,
) -> Result<String> {
    if question.trim().is_empty() {
        return Err(anyhow!("empty question"));
    }

    let mut client = LlmClient::new(&config).await?;

    let mut messages: Vec<Message> = Vec::with_capacity(transcript_snapshot.len() + 2);
    messages.push(Message::system(
        build_btw_system_prompt(config.user.agent_name.as_deref()).await,
    ));
    messages.extend(transcript_snapshot);
    messages.push(Message::user(format!("{BTW_PROMPT_PREFIX}{question}")));

    let (tx, mut rx) = mpsc::channel::<StreamEvent>(64);

    // Cancel token governs both the stream and our outer timeout — whichever
    // fires first terminates the LLM request.
    let stream_cancel = cancel.clone();
    let timeout_cancel = cancel.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::time::sleep(BTW_TIMEOUT) => {
                timeout_cancel.cancel();
            }
            _ = timeout_cancel.cancelled() => {}
        }
    });

    let stream_handle = tokio::spawn(async move {
        client
            .stream_chat_with_cancel(&messages, None, tx, stream_cancel)
            .await
    });

    let BtwStream {
        text: buf,
        saw_tool_call,
        error: mut stream_error,
    } = collect_btw_stream(&mut rx).await;

    // Surface any error the client raised after the stream task returned.
    // We await the handle so a panic in the stream task doesn't silently hang.
    match stream_handle.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            if stream_error.is_none() {
                stream_error = Some(format!("{e:#}"));
            }
        }
        Err(join_err) => {
            tracing::warn!("/btw: stream task panicked: {join_err}");
            if stream_error.is_none() {
                stream_error = Some("stream task panicked".to_string());
            }
        }
    }

    if let Some(err) = stream_error {
        return Err(anyhow!(err));
    }
    if cancel.is_cancelled() && buf.is_empty() {
        return Err(anyhow!("cancelled or timed out"));
    }
    if buf.trim().is_empty() {
        if saw_tool_call {
            return Err(anyhow!(
                "model tried to call a tool (tools are disabled in /btw)"
            ));
        }
        return Err(anyhow!("empty response from model"));
    }
    Ok(buf)
}

/// What one `/btw` stream produced.
struct BtwStream {
    text: String,
    saw_tool_call: bool,
    error: Option<String>,
}

/// Collect the stream of a `/btw` request into the output of the attempt
/// that completes.
async fn collect_btw_stream(rx: &mut mpsc::Receiver<StreamEvent>) -> BtwStream {
    let mut text = String::new();
    let mut saw_tool_call = false;
    let mut error: Option<String> = None;

    while let Some(event) = rx.recv().await {
        match event {
            StreamEvent::TextDelta(delta) => text.push_str(&delta),
            StreamEvent::ToolCallDelta { .. } => {
                // We passed `tools=None`; a provider that still emits a tool
                // call is misbehaving. Drop it and note it so the caller can
                // surface a clearer error than "empty answer".
                saw_tool_call = true;
            }
            StreamEvent::Error(msg) => {
                error = Some(msg);
            }
            StreamEvent::Done => break,
            // Each attempt sends the full answer again. The output of a
            // failed attempt (a retry, or the attempt before a failover)
            // must not stay in front of the answer of the attempt that
            // completes.
            StreamEvent::AttemptStarted { .. } => {
                text.clear();
                saw_tool_call = false;
            }
            // Failover identity only matters for the conversational
            // <environment> section; this one-shot helper has none to sync.
            StreamEvent::ThinkingDelta(_)
            | StreamEvent::ReasoningBlock(_)
            | StreamEvent::Usage(_)
            | StreamEvent::Truncated
            | StreamEvent::ContentFiltered { .. }
            | StreamEvent::ToolIntent
            | StreamEvent::Reconnecting { .. }
            | StreamEvent::Failover { .. } => {}
        }
    }
    BtwStream {
        text,
        saw_tool_call,
        error,
    }
}

/// Parse a `/btw` slash-command from a raw message body.
///
/// Matches `/btw`, `/btw:`, `/btw <question>`, `/btw: <question>` (case-insensitive,
/// allowing leading whitespace). Returns the trimmed question text on match — an
/// empty string when the user typed `/btw` with no follow-up so callers can show
/// a usage hint rather than silently routing to the main agent.
pub fn parse_btw_command(text: &str) -> Option<&str> {
    let trimmed = text.trim_start();
    let rest = trimmed.strip_prefix('/')?;
    // case-insensitive prefix match for `btw`
    if rest.len() < 3 || !rest.as_bytes()[..3].eq_ignore_ascii_case(b"btw") {
        return None;
    }
    let after = &rest[3..];
    let question = match after.chars().next() {
        None => "",                                         // bare `/btw`
        Some(':') => after[1..].trim_start(),               // `/btw:` or `/btw: foo`
        Some(c) if c.is_whitespace() => after.trim_start(), // `/btw foo`
        Some(_) => return None,                             // `/btweet` — not a match
    };
    Some(question.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn collect(events: Vec<StreamEvent>) -> BtwStream {
        let (tx, mut rx) = mpsc::channel(64);
        for event in events {
            tx.send(event).await.expect("send");
        }
        drop(tx);
        collect_btw_stream(&mut rx).await
    }

    /// Spec: the answer is the output of the attempt that completes. A
    /// failed attempt leaves no text and no tool-call flag behind.
    #[tokio::test]
    async fn a_retry_after_partial_output_keeps_only_the_final_attempt() {
        let delta = |text: &str| StreamEvent::TextDelta(text.to_string());
        let stream = collect(vec![
            StreamEvent::AttemptStarted { attempt: 0 },
            delta("The answer is fo"),
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".to_string()),
                name: Some("read_file".to_string()),
                arguments_delta: "{".to_string(),
            },
            StreamEvent::Reconnecting {
                attempt: 0,
                delay_secs: 1,
            },
            StreamEvent::AttemptStarted { attempt: 1 },
            delta("The answer is "),
            delta("forty-two."),
            StreamEvent::Done,
        ])
        .await;
        assert_eq!(stream.text, "The answer is forty-two.");
        assert!(
            !stream.saw_tool_call,
            "the tool call belongs to the failed attempt"
        );
        assert!(stream.error.is_none());
    }

    /// Spec: a tool call in the attempt that completes is still reported.
    #[tokio::test]
    async fn a_tool_call_in_the_final_attempt_is_reported() {
        let stream = collect(vec![
            StreamEvent::AttemptStarted { attempt: 0 },
            StreamEvent::TextDelta("partial".to_string()),
            StreamEvent::AttemptStarted { attempt: 1 },
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".to_string()),
                name: Some("read_file".to_string()),
                arguments_delta: "{}".to_string(),
            },
            StreamEvent::Done,
        ])
        .await;
        assert_eq!(stream.text, "");
        assert!(stream.saw_tool_call);
    }

    #[test]
    fn parse_btw_positive_cases() {
        assert_eq!(
            parse_btw_command("/btw what is rust?"),
            Some("what is rust?")
        );
        assert_eq!(parse_btw_command("/BTW hi"), Some("hi"));
        assert_eq!(parse_btw_command("/btw: hi"), Some("hi"));
        assert_eq!(parse_btw_command("  /btw   foo  "), Some("foo"));
        assert_eq!(parse_btw_command("/btw\thello"), Some("hello"));
    }

    #[test]
    fn parse_btw_empty_question() {
        assert_eq!(parse_btw_command("/btw"), Some(""));
        assert_eq!(parse_btw_command("/btw "), Some(""));
        assert_eq!(parse_btw_command("/btw:"), Some(""));
    }

    #[test]
    fn parse_btw_negative_cases() {
        assert_eq!(parse_btw_command("/btweet me"), None);
        assert_eq!(parse_btw_command("by the way, foo"), None);
        assert_eq!(parse_btw_command("hello /btw inline"), None);
        assert_eq!(parse_btw_command(""), None);
        assert_eq!(parse_btw_command("/"), None);
        assert_eq!(parse_btw_command("/bt"), None);
    }

    #[tokio::test]
    async fn empty_question_rejected_without_llm_call() {
        // Config with no API keys — run_btw must fail early on empty question
        // before it tries to construct the LLM client, otherwise this test
        // would surface a credentials error instead.
        let config = Config::default();
        let result = run_btw(
            config,
            "   ".to_string(),
            Vec::new(),
            CancellationToken::new(),
        )
        .await;
        let err = result.expect_err("empty question must error");
        assert!(
            err.to_string().contains("empty question"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn system_prompt_mentions_btw_and_no_tools() {
        let p = build_btw_system_prompt(None).await;
        assert!(
            p.contains("/btw"),
            "system prompt should reference /btw framing"
        );
        assert!(
            p.to_lowercase().contains("no tools"),
            "system prompt must tell the model no tools are available"
        );
    }
}
