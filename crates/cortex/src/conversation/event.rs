//! The session event log — the first-class, append-only history an agent is
//! reconstructed from.
//!
//! Every durable thing an agent does is a [`SessionEvent`]: a user message, an
//! assistant reply, a tool call, a tool result, a compaction, a fork. The log
//! is the source of truth; everything the rest of the system consumes is a
//! *projection* over it — the conversation the model reads
//! ([`project_history`]), the TUI transcript, an export. The runtime, model,
//! and tools are interpreters that read the log, act, and append the next
//! event.
//!
//! Physically the log lives in the `messages` table: one row per event,
//! ordered by the autoincrement `id` (= [`SessionEvent::seq`], also the offset
//! a fork branches from). Rows written before the log was first-class carry no
//! `event_kind`; they decode as plain [`SessionEventKind::Message`] values, so
//! existing sessions replay byte-for-byte unchanged.

use serde::{Deserialize, Serialize};

use crate::db::MessageRow;
use crate::types::{Message, MessageContent, Role, ToolCall};

/// Domain separator for the per-session HMAC chain. Mixed with the
/// per-install key by [`crate::db::Database::derive_hmac_key`] so the same
/// conversation content hashes differently on different installs.
pub const SESSION_HMAC_DOMAIN: &[u8] = b"borg-session-log-chain-v1";

/// The `event_kind` column value persisted for a
/// [`SessionEventKind::CompactionCheckpoint`] row (also its FTS-exclusion flag).
/// Must equal serde's snake_case tag for that variant — guarded by
/// `compaction_checkpoint_kind_matches_serde_tag`.
pub const COMPACTION_CHECKPOINT_KIND: &str = "compaction_checkpoint";

/// One durable entry in a session's append-only event log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEvent {
    /// Monotonic ordering key within the log — the `messages.id` rowid. Also
    /// the anchor a fork branches from.
    pub seq: i64,
    /// Owning session id.
    pub session_id: String,
    /// Unix timestamp when the event was appended.
    pub created_at: i64,
    /// What happened.
    pub kind: SessionEventKind,
    /// Chain link over this event's immutable payload. `None` on legacy rows
    /// written before the log was HMAC-chained.
    pub hmac: Option<String>,
    /// HMAC of the previous event in this session's chain. `None` on legacy
    /// rows; `"0"` marks the first chained event in a session.
    pub prev_hmac: Option<String>,
}

/// The immutable persisted fields of a session-log event that the HMAC chain
/// protects (never derived/FTS state, so the chain stays valid for the life of
/// the row).
///
/// Both the append path and the verify path build one of these, so the hashed
/// field set and its order can never drift between writing a link and checking
/// it — a divergence that would silently break the chain. Construct it from a
/// persisted row with [`SessionEventHashInput::from`].
pub struct SessionEventHashInput<'a> {
    /// Owning session id.
    pub session_id: &'a str,
    /// Message role (`system`/`user`/`assistant`/`tool`).
    pub role: &'a str,
    /// Fine-grained event tag, or `None` for a plain message row.
    pub event_kind: Option<&'a str>,
    /// Message text content.
    pub content: Option<&'a str>,
    /// Serialized tool calls.
    pub tool_calls_json: Option<&'a str>,
    /// Tool-call id this row answers.
    pub tool_call_id: Option<&'a str>,
    /// Optional client timestamp string.
    pub timestamp: Option<&'a str>,
    /// Serialized structured content parts.
    pub content_parts_json: Option<&'a str>,
    /// Typed payload for a fine-grained event.
    pub event_json: Option<&'a str>,
    /// Append timestamp (unix seconds).
    pub created_at: i64,
}

impl<'a> From<&'a MessageRow> for SessionEventHashInput<'a> {
    fn from(row: &'a MessageRow) -> Self {
        Self {
            session_id: &row.session_id,
            role: &row.role,
            event_kind: row.event_kind.as_deref(),
            content: row.content.as_deref(),
            tool_calls_json: row.tool_calls_json.as_deref(),
            tool_call_id: row.tool_call_id.as_deref(),
            timestamp: row.timestamp.as_deref(),
            content_parts_json: row.content_parts_json.as_deref(),
            event_json: row.event_json.as_deref(),
            created_at: row.created_at,
        }
    }
}

/// Compute the chain link for a session-log event.
///
/// Fields are concatenated without separators, matching the other ledgers'
/// [`crate::hmac_chain`] convention.
pub fn compute_session_event_hmac(
    key: &[u8],
    prev_hmac: &str,
    fields: &SessionEventHashInput,
) -> String {
    crate::hmac_chain::compute_hmac(
        key,
        &[
            prev_hmac.as_bytes(),
            fields.session_id.as_bytes(),
            fields.role.as_bytes(),
            fields.event_kind.unwrap_or("").as_bytes(),
            fields.content.unwrap_or("").as_bytes(),
            fields.tool_calls_json.unwrap_or("").as_bytes(),
            fields.tool_call_id.unwrap_or("").as_bytes(),
            fields.timestamp.unwrap_or("").as_bytes(),
            fields.content_parts_json.unwrap_or("").as_bytes(),
            fields.event_json.unwrap_or("").as_bytes(),
            &fields.created_at.to_le_bytes(),
        ],
    )
}

/// The closed set of things that can happen in a session.
///
/// Deliberately minimal. [`SessionEventKind::Message`] is the conversational
/// backbone (and the only variant legacy rows decode to). The other properties
/// the log gives us fall out of the existing structure rather than needing
/// their own event kinds:
///
/// - **Reliability / resume** — the assistant message (carrying its tool calls)
///   is persisted *before* any tool dispatches, partial output is persisted on
///   stream interruption, and [`crate::conversation::normalize_history`]
///   repairs dangling calls on replay. No per-call marker event is needed.
/// - **Migration** — a [`Message`] is provider-neutral; switching models is the
///   provider adapter re-rendering the same log, not a logged event.
/// - **Forking** — branch lineage lives on the `sessions` row
///   (`parent_session_id` + `forked_from_message_id`), so a fork references its
///   parent's log by offset without copying rows or logging a marker.
///
/// Only compaction needs to be expressed as an event, because it changes what
/// the model sees while the raw history must stay intact.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEventKind {
    /// A conversational message (system / user / assistant / tool).
    Message(Message),
    /// Compaction expressed as a non-destructive event: events in
    /// `[from_seq, to_seq]` are summarized by `summary`. The raw events are NOT
    /// deleted — the compacted view is just one projection
    /// ([`project_history`] folds it in), so the full history stays
    /// re-derivable, exportable, and forkable from before the compaction.
    CompactionCheckpoint {
        /// First event seq covered by the summary (inclusive).
        from_seq: i64,
        /// Last event seq covered by the summary (inclusive).
        to_seq: i64,
        /// The summary text that stands in for the covered range.
        summary: String,
        /// Model that produced the summary.
        model: String,
    },
}

/// Decode a persisted `messages` row into a [`SessionEvent`].
///
/// Rows with a non-NULL `event_kind` carry a fine-grained lifecycle event in
/// `event_json`; everything else (including every legacy row) decodes as a
/// conversational [`SessionEventKind::Message`] from the role/content columns.
/// Returns `None` for an unrecognized role or an undecodable event payload —
/// matching the legacy loader, which silently skipped malformed rows.
pub fn event_from_row(row: MessageRow) -> Option<SessionEvent> {
    let kind = match row.event_kind.as_deref() {
        Some(tag) => {
            // A fine-grained event must carry its typed payload in `event_json`;
            // a missing or undecodable payload is corruption — skip the row.
            let Some(json) = row.event_json.as_deref() else {
                tracing::warn!(
                    "session log: '{tag}' event at seq {} has no payload; skipping",
                    row.id
                );
                return None;
            };
            match serde_json::from_str(json) {
                Ok(kind) => kind,
                Err(e) => {
                    tracing::warn!(
                        "session log: undecodable '{tag}' event at seq {} ({e}); skipping",
                        row.id
                    );
                    return None;
                }
            }
        }
        None => SessionEventKind::Message(message_from_row(&row)?),
    };
    Some(SessionEvent {
        seq: row.id,
        session_id: row.session_id,
        created_at: row.created_at,
        kind,
        hmac: row.hmac,
        prev_hmac: row.prev_hmac,
    })
}

/// Decode the conversational columns of a row into a [`Message`]. Returns
/// `None` for an unrecognized role.
fn message_from_row(row: &MessageRow) -> Option<Message> {
    let role = match row.role.as_str() {
        "system" => Role::System,
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        _ => return None,
    };
    let tool_calls: Option<Vec<ToolCall>> = row
        .tool_calls_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok());
    let content: Option<MessageContent> = if let Some(parts_json) = &row.content_parts_json {
        match serde_json::from_str(parts_json) {
            Ok(parts) => Some(parts),
            Err(e) => {
                tracing::warn!("Failed to deserialize content_parts_json: {e}");
                row.content.clone().map(MessageContent::Text)
            }
        }
    } else {
        row.content.clone().map(MessageContent::Text)
    };
    Some(Message {
        role,
        content,
        tool_calls,
        tool_call_id: row.tool_call_id.clone(),
        timestamp: row.timestamp.clone(),
        // The working set carries its log position so compaction can record an
        // exact checkpoint range and any reader can map back to the log.
        seq: Some(row.id),
    })
}

/// Verify the per-session HMAC chain over persisted rows (ordered by `id`).
///
/// The chain protects the immutable persisted bytes, so this works on the raw
/// rows rather than projected events. Each session is its own chain rooted at
/// `"0"`, so when the rows span a fork lineage (see
/// [`crate::db::Database::load_session_lineage_rows`]) a change of `session_id`
/// restarts the expected link at `"0"` — the boundary between an ancestor's
/// chain and the child's own chain is legitimate, not a break. Legacy rows
/// written before chaining (NULL `hmac`) are tolerated — they don't advance the
/// expected link, so a chained suffix appended after an upgrade still verifies.
/// Returns `false` if any chained row's link is broken (tampering or reordering).
pub fn verify_session_chain(key: &[u8], rows: &[MessageRow]) -> bool {
    let mut expected_prev = String::from("0");
    let mut current_session: Option<&str> = None;
    let mut all_valid = true;
    for row in rows {
        // A fork boundary: rows from a different session start their own chain.
        if current_session != Some(row.session_id.as_str()) {
            current_session = Some(row.session_id.as_str());
            expected_prev = String::from("0");
        }
        let (Some(hmac), Some(prev)) = (row.hmac.as_deref(), row.prev_hmac.as_deref()) else {
            continue; // unchained legacy row — cannot verify, don't fail the chain
        };
        let recomputed = compute_session_event_hmac(key, prev, &row.into());
        if crate::hmac_chain::verify_chain_link(hmac, prev, &expected_prev, &recomputed) {
            expected_prev = hmac.to_string();
        } else {
            // Mirror the vitals/bond/evolution replay convention: a broken link
            // does NOT advance the expected-prev, so every row after the break
            // is also treated as unverifiable rather than re-anchoring to a
            // possibly-tampered hmac.
            all_valid = false;
        }
    }
    all_valid
}

/// Decode a session's persisted rows (ordered by `id`) into the event log,
/// skipping any row with an unrecognized role.
pub fn events_from_rows(rows: Vec<MessageRow>) -> Vec<SessionEvent> {
    rows.into_iter().filter_map(event_from_row).collect()
}

/// Project the event log into the conversation the model reads.
///
/// This is the canonical reconstruction step: hand a fresh executor the log
/// and this rebuilds exactly the working history.
///
/// Compaction is folded in here. The newest [`SessionEventKind::CompactionCheckpoint`]
/// wins (compaction is iterative — each summary subsumes the previous one). The
/// summary stands in for the *range* `[from_seq, to_seq]` it covers: events
/// before `from_seq` (borg's protected head — the original request and early
/// framing) replay verbatim, then the summary, then events after `to_seq` (the
/// recent tail). This range fold mirrors the live head-protected compactor,
/// which keeps `[head] ++ [summary] ++ [tail]`. The raw covered events remain in
/// the log, untouched — this projection simply skips them, so export, audit, and
/// forking from before the compaction all stay possible.
pub fn project_history(events: &[SessionEvent]) -> Vec<Message> {
    // Find the most recent compaction checkpoint, if any.
    let compaction = events.iter().rev().find_map(|e| match &e.kind {
        SessionEventKind::CompactionCheckpoint {
            from_seq,
            to_seq,
            summary,
            ..
        } => Some((*from_seq, *to_seq, summary.as_str())),
        SessionEventKind::Message(_) => None,
    });

    let mut out = Vec::with_capacity(events.len());
    match compaction {
        Some((from_seq, to_seq, summary)) => {
            // Protected head: everything before the summarized range stays.
            out.extend(
                events
                    .iter()
                    .filter(|e| e.seq < from_seq)
                    .filter_map(message_of),
            );
            out.push(compaction_summary_message(summary));
            // Recent tail: everything after the summarized range replays.
            out.extend(
                events
                    .iter()
                    .filter(|e| e.seq > to_seq)
                    .filter_map(message_of),
            );
        }
        None => out.extend(events.iter().filter_map(message_of)),
    }
    out
}

/// The conversational message carried by an event, if it is one.
fn message_of(event: &SessionEvent) -> Option<Message> {
    match &event.kind {
        SessionEventKind::Message(m) => Some(m.clone()),
        SessionEventKind::CompactionCheckpoint { .. } => None,
    }
}

/// Render a compaction summary as the marker message the model sees, via the
/// shared [`crate::conversation::render_compaction_marker`] so the format is
/// byte-identical whether a session was compacted live or replayed from the log.
fn compaction_summary_message(summary: &str) -> Message {
    crate::conversation::render_compaction_marker(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, role: &str, content: Option<&str>) -> MessageRow {
        MessageRow {
            id,
            session_id: "s1".to_string(),
            role: role.to_string(),
            content: content.map(str::to_string),
            content_parts_json: None,
            tool_calls_json: None,
            tool_call_id: None,
            timestamp: None,
            created_at: 1000 + id,
            event_kind: None,
            event_json: None,
            hmac: None,
            prev_hmac: None,
        }
    }

    #[test]
    fn project_history_matches_legacy_row_decode() {
        // The Phase 0 invariant: projecting the log must yield exactly what the
        // old row→Message loop produced, including dropping unknown-role rows.
        let rows = vec![
            row(1, "user", Some("hello")),
            row(2, "assistant", Some("hi")),
            row(3, "bogus", Some("ignored")),
            row(4, "tool", Some("result")),
        ];
        let legacy = legacy_decode(&rows);
        let projected = project_history(&events_from_rows(rows));
        // Compare via the serialized form — `Message` has no `PartialEq`, and
        // JSON equality is exactly the "byte-identical projection" guarantee.
        assert_eq!(
            serde_json::to_value(&projected).unwrap(),
            serde_json::to_value(&legacy).unwrap(),
        );
    }

    #[test]
    fn event_from_row_skips_unknown_role() {
        assert!(event_from_row(row(1, "weird", Some("x"))).is_none());
    }

    fn msg_event(seq: i64, role: Role, text: &str) -> SessionEvent {
        SessionEvent {
            seq,
            session_id: "s1".into(),
            created_at: 1000 + seq,
            kind: SessionEventKind::Message(Message {
                role,
                content: Some(MessageContent::Text(text.into())),
                tool_calls: None,
                tool_call_id: None,
                timestamp: None,
                seq: None,
            }),
            hmac: None,
            prev_hmac: None,
        }
    }

    fn compaction_event(seq: i64, from_seq: i64, to_seq: i64, summary: &str) -> SessionEvent {
        SessionEvent {
            seq,
            session_id: "s1".into(),
            created_at: 1000 + seq,
            kind: SessionEventKind::CompactionCheckpoint {
                from_seq,
                to_seq,
                summary: summary.into(),
                model: "test-model".into(),
            },
            hmac: None,
            prev_hmac: None,
        }
    }

    #[test]
    fn project_history_folds_compaction_non_destructively() {
        // A compaction checkpoint replaces the covered range with its summary
        // in the projection, but the raw covered events stay in the log —
        // that's what keeps export/audit/fork-from-before possible.
        let events = vec![
            msg_event(1, Role::User, "old question"),
            msg_event(2, Role::Assistant, "old answer"),
            compaction_event(3, 1, 2, "summary of the early turns"),
            msg_event(4, Role::User, "new question"),
        ];
        let out = project_history(&events);
        assert_eq!(out.len(), 2, "summary stands in for the two covered turns");
        assert!(out[0]
            .text_content()
            .unwrap()
            .contains("summary of the early turns"));
        assert_eq!(out[1].text_content(), Some("new question"));
        // The raw log is untouched — projection is a read, not a mutation.
        assert_eq!(events.len(), 4);
    }

    #[test]
    fn project_history_preserves_protected_head_around_compaction() {
        // borg's compactor is head-protected: it keeps the first messages, drops
        // the middle, keeps the recent tail. A checkpoint covering only the
        // middle [from_seq, to_seq] must fold to [head] ++ [summary] ++ [tail],
        // NOT drop the head. (A prefix-only fold would lose the head on reload.)
        let events = vec![
            msg_event(1, Role::User, "original request"),
            msg_event(2, Role::Assistant, "early framing"),
            msg_event(3, Role::User, "middle q"),
            msg_event(4, Role::Assistant, "middle a"),
            compaction_event(5, 3, 4, "summary of the middle"),
            msg_event(6, Role::User, "recent q"),
        ];
        let out = project_history(&events);
        let texts: Vec<_> = out
            .iter()
            .map(|m| m.text_content().unwrap_or("").to_string())
            .collect();
        assert_eq!(texts.len(), 4, "head(2) + summary(1) + tail(1)");
        assert_eq!(texts[0], "original request");
        assert_eq!(texts[1], "early framing");
        assert!(texts[2].contains("summary of the middle"));
        assert_eq!(texts[3], "recent q");
    }

    #[test]
    fn project_history_uses_newest_compaction_checkpoint() {
        // Iterative compaction: the latest checkpoint subsumes earlier ones.
        let events = vec![
            msg_event(1, Role::User, "q1"),
            compaction_event(2, 1, 1, "first summary"),
            msg_event(3, Role::User, "q2"),
            compaction_event(4, 1, 3, "second summary"),
            msg_event(5, Role::User, "q3"),
        ];
        let out = project_history(&events);
        assert_eq!(out.len(), 2);
        assert!(out[0].text_content().unwrap().contains("second summary"));
        assert!(!out[0].text_content().unwrap().contains("first summary"));
        assert_eq!(out[1].text_content(), Some("q3"));
    }

    #[test]
    fn seq_is_row_id() {
        let ev = event_from_row(row(42, "user", Some("hi"))).unwrap();
        assert_eq!(ev.seq, 42);
    }

    #[test]
    fn compaction_checkpoint_kind_matches_serde_tag() {
        // The persisted `event_kind` string must equal serde's internally-tagged
        // variant name, or `event_from_row` won't decode rows we wrote.
        let json = serde_json::to_value(SessionEventKind::CompactionCheckpoint {
            from_seq: 1,
            to_seq: 2,
            summary: "s".into(),
            model: "m".into(),
        })
        .unwrap();
        assert_eq!(json["kind"], COMPACTION_CHECKPOINT_KIND);
    }

    // Mirror of the original `recover_session_from_db` row→Message decode, kept
    // here so the parity test fails if `project_history` ever diverges from the
    // behavior it replaced.
    fn legacy_decode(rows: &[MessageRow]) -> Vec<Message> {
        let mut messages = Vec::new();
        for row in rows {
            let role = match row.role.as_str() {
                "system" => Role::System,
                "user" => Role::User,
                "assistant" => Role::Assistant,
                "tool" => Role::Tool,
                _ => continue,
            };
            let tool_calls: Option<Vec<ToolCall>> = row
                .tool_calls_json
                .as_deref()
                .and_then(|j| serde_json::from_str(j).ok());
            let content = if let Some(parts_json) = &row.content_parts_json {
                serde_json::from_str(parts_json)
                    .ok()
                    .or_else(|| row.content.clone().map(MessageContent::Text))
            } else {
                row.content.clone().map(MessageContent::Text)
            };
            messages.push(Message {
                role,
                content,
                tool_calls,
                tool_call_id: row.tool_call_id.clone(),
                timestamp: row.timestamp.clone(),
                seq: Some(row.id),
            });
        }
        messages
    }
}
