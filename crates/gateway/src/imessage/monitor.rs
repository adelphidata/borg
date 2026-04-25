use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::constants::{PEER_KIND_DIRECT, PEER_KIND_GROUP};
use borg_core::config::Config;

use super::echo_cache::EchoCache;
use super::reflection_guard;
use super::sanitize::sanitize_outbound;
use super::self_chat_cache::SelfChatCache;
use super::send::send_imessage;
use super::types::{
    normalize_handle, AttachmentMeta, IMessagePayload, InboundDecision, ReplyContext,
};
use crate::handler::{self, InboundAttachment, InboundMessage};
use crate::registry::RegisteredChannel;

const DEFAULT_POLL_INTERVAL_MS: u64 = 2000;
const STATE_FILE: &str = "state.json";
/// Max attachment file size we will base64-encode and forward to the agent.
/// Larger files surface as a placeholder text note instead of binary payload.
const MAX_ATTACHMENT_BYTES: u64 = 10 * 1024 * 1024;

/// Start the iMessage monitor loop in a background task.
pub fn spawn_monitor(config: Config, shutdown: CancellationToken) -> Result<JoinHandle<()>> {
    let data_dir = Config::data_dir()?;
    let channel_dir = data_dir.join("channels/imessage");

    let handle = tokio::spawn(async move {
        if let Err(e) = monitor_loop(config, channel_dir, shutdown).await {
            tracing::warn!("iMessage monitor exited with error: {e}");
        }
    });

    Ok(handle)
}

async fn monitor_loop(
    config: Config,
    channel_dir: PathBuf,
    shutdown: CancellationToken,
) -> Result<()> {
    let db_path = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("no home directory"))?
        .join("Library/Messages/chat.db");

    if !db_path.exists() {
        anyhow::bail!("chat.db not found at {}", db_path.display());
    }

    let db_uri = format!("file:{}?mode=ro", db_path.display());

    // Load persisted state (last_rowid)
    let state_path = channel_dir.join(STATE_FILE);
    let mut last_rowid = load_last_rowid(&state_path).await;

    // If no state, initialize to current max ROWID (only process future messages)
    if last_rowid == 0 {
        let conn = open_chatdb(&db_uri)?;
        last_rowid = conn
            .query_row("SELECT MAX(ROWID) FROM message", [], |row| {
                row.get::<_, Option<i64>>(0)
            })?
            .unwrap_or(0);
        save_last_rowid(&state_path, last_rowid).await;
    }

    let poll_interval = Duration::from_millis(DEFAULT_POLL_INTERVAL_MS);
    let mut echo_cache = EchoCache::new();
    let mut self_chat_cache = SelfChatCache::new();

    // Load channel manifest for handler reuse
    let manifest_path = channel_dir.join("channel.toml");
    let manifest = crate::manifest::ChannelManifest::load_async(&manifest_path)
        .await
        .context("Failed to load imessage channel.toml")?;
    let registered_channel = RegisteredChannel {
        manifest,
        dir: channel_dir.clone(),
    };

    info!(
        "iMessage monitor started (last_rowid={last_rowid}, poll={}ms)",
        poll_interval.as_millis()
    );

    let mut interval = tokio::time::interval(poll_interval);

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                info!("iMessage monitor shutting down");
                break;
            }
            _ = interval.tick() => {
                match poll_new_messages(&db_uri, last_rowid) {
                    Ok(messages) => {
                        for payload in messages {
                            let rowid = payload.rowid;
                            let is_group = payload.is_group;
                            let attachments_meta = payload.attachments.clone();
                            let decision = evaluate_message(
                                &payload,
                                &mut echo_cache,
                                &mut self_chat_cache,
                                config.gateway.imessage_group_allowlist.as_deref(),
                            );

                            match decision {
                                InboundDecision::Dispatch { sender_id, text, channel_id } => {
                                    info!("iMessage from {sender_id}: {}", &text[..text.len().min(50)]);

                                    // Reading + base64-encoding attachments can take tens of ms
                                    // for a 10 MB image; offload to the blocking pool so the
                                    // gateway dispatch task doesn't stall the tokio reactor.
                                    let attachments = tokio::task::spawn_blocking(move || {
                                        load_attachments(&attachments_meta)
                                    })
                                    .await
                                    .unwrap_or_else(|e| {
                                        warn!("attachment load task panicked: {e}");
                                        Vec::new()
                                    });

                                    let inbound = InboundMessage {
                                        sender_id: sender_id.clone(),
                                        text,
                                        channel_id: Some(channel_id),
                                        thread_id: None,
                                        message_id: None,
                                        thread_ts: None,
                                        attachments,
                                        reaction: None,
                                        metadata: serde_json::Value::Null,
                                        peer_kind: Some(if is_group {
                                            PEER_KIND_GROUP.to_string()
                                        } else {
                                            PEER_KIND_DIRECT.to_string()
                                        }),
                                    };

                                    match handler::handle_polled_message(
                                        &registered_channel,
                                        inbound,
                                        &config,
                                        None,
                                    ).await {
                                        Ok(response) => {
                                            let sanitized = sanitize_outbound(&response);
                                            if !sanitized.is_empty() && sanitized != "(no response)" {
                                                // Detect service from chat identifier
                                                let service = payload.chat_identifier
                                                    .as_deref()
                                                    .and_then(|ci| {
                                                        if ci.starts_with("SMS") { Some("SMS") } else { None }
                                                    })
                                                    .unwrap_or("iMessage");

                                                if let Err(e) = send_imessage(&sender_id, &sanitized, service).await {
                                                    warn!("Failed to send iMessage reply: {e}");
                                                } else {
                                                    echo_cache.remember(&sanitized, None);
                                                }
                                            }
                                        }
                                        Err(e) => warn!("Failed to process iMessage: {e}"),
                                    }
                                }
                                InboundDecision::Drop { reason } => {
                                    debug!("Dropped iMessage rowid={rowid}: {reason}");
                                }
                            }

                            if rowid > last_rowid {
                                last_rowid = rowid;
                                save_last_rowid(&state_path, last_rowid).await;
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to poll chat.db: {e}");
                    }
                }
            }
        }
    }

    Ok(())
}

/// Evaluate a message through all decision gates.
fn evaluate_message(
    payload: &IMessagePayload,
    echo_cache: &mut EchoCache,
    self_chat_cache: &mut SelfChatCache,
    group_allowlist: Option<&[String]>,
) -> InboundDecision {
    let text = match &payload.text {
        Some(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => {
            return InboundDecision::Drop {
                reason: "empty text".to_string(),
            }
        }
    };

    let chat_id = payload
        .chat_identifier
        .clone()
        .unwrap_or_else(|| "unknown".to_string());

    // Gate 0: group allowlist (groups only — DMs always pass).
    if payload.is_group {
        if let Some(allowed) = group_allowlist {
            let guid_match = payload
                .chat_guid
                .as_deref()
                .is_some_and(|g| allowed.iter().any(|a| a == g));
            let id_match = allowed.iter().any(|a| a == &chat_id);
            if !guid_match && !id_match {
                return InboundDecision::Drop {
                    reason: format!("group not in allowlist: {chat_id}"),
                };
            }
        }
    }

    // Gate 1: is_from_me — record in self-chat cache, then drop
    if payload.is_from_me {
        self_chat_cache.remember(&text, &chat_id);
        return InboundDecision::Drop {
            reason: "is_from_me".to_string(),
        };
    }

    // Gate 2: echo detection — did we recently send this exact text?
    if echo_cache.is_echo(&text, None) {
        return InboundDecision::Drop {
            reason: "echo of sent message".to_string(),
        };
    }

    // Gate 3: self-chat reflection — was this recently sent by us in the same chat?
    if self_chat_cache.is_self_echo(&text, &chat_id) {
        return InboundDecision::Drop {
            reason: "self-chat reflection".to_string(),
        };
    }

    // Gate 4: reflection guard — contains internal markers?
    if let Some(marker) = reflection_guard::detect_reflected_content(&text) {
        return InboundDecision::Drop {
            reason: format!("reflected content: {marker}"),
        };
    }

    // Determine sender (normalized) and prepend reply context if present.
    let sender_id = payload
        .sender
        .as_deref()
        .map(normalize_handle)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    let dispatched_text = match &payload.reply_to {
        Some(reply) => format_reply_quote(reply, &text),
        None => text,
    };

    InboundDecision::Dispatch {
        sender_id,
        text: dispatched_text,
        channel_id: chat_id,
    }
}

/// Render a reply context as a quoted prefix, mirroring how chat UIs display
/// inline replies. Keeps the parent visible to the agent so it can answer with
/// context even though iMessage's outbound API can't link to the parent.
fn format_reply_quote(reply: &ReplyContext, body: &str) -> String {
    let attribution = reply
        .sender
        .as_deref()
        .map(normalize_handle)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "you".to_string());
    // Prefix every line of the parent so multi-line quotes render correctly.
    let quoted: String = reply
        .text
        .lines()
        .map(|l| format!("> {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("In reply to {attribution}:\n{quoted}\n\n{body}")
}

/// Read each on-disk attachment, base64-encode, and convert to InboundAttachment.
/// Files larger than `MAX_ATTACHMENT_BYTES` or unreadable paths are skipped with
/// a warning rather than failing the whole message.
fn load_attachments(metas: &[AttachmentMeta]) -> Vec<InboundAttachment> {
    let mut out = Vec::with_capacity(metas.len());
    for meta in metas {
        let path = expand_tilde(&meta.path);
        let bytes = match std::fs::metadata(&path) {
            Ok(m) if m.len() > MAX_ATTACHMENT_BYTES => {
                warn!(
                    "Skipping iMessage attachment {} ({} bytes > {} limit)",
                    path.display(),
                    m.len(),
                    MAX_ATTACHMENT_BYTES
                );
                continue;
            }
            Ok(_) => match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    warn!("Failed to read iMessage attachment {}: {e}", path.display());
                    continue;
                }
            },
            Err(e) => {
                warn!("iMessage attachment unavailable {}: {e}", path.display());
                continue;
            }
        };
        let mime = meta
            .mime_type
            .clone()
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        out.push(InboundAttachment {
            mime_type: mime,
            data,
            filename: meta.filename.clone(),
        });
    }
    out
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    PathBuf::from(path)
}

fn poll_new_messages(db_uri: &str, last_rowid: i64) -> Result<Vec<IMessagePayload>> {
    let conn = open_chatdb(db_uri)?;
    poll_new_messages_with_conn(&conn, last_rowid)
}

/// Same as `poll_new_messages` but uses a caller-supplied connection. Split
/// out so tests can drive the SQL against a fixture chat.db without going
/// through `open_chatdb`'s read-only flag (the fixture builder needs RW).
fn poll_new_messages_with_conn(
    conn: &rusqlite::Connection,
    last_rowid: i64,
) -> Result<Vec<IMessagePayload>> {
    let mut stmt = conn.prepare(
        "SELECT m.ROWID, m.text, m.date, h.id, m.is_from_me, m.cache_roomnames, \
         c.chat_identifier, c.guid, m.thread_originator_guid, \
         parent.text AS reply_text, ph.id AS reply_sender \
         FROM message m \
         LEFT JOIN handle h ON m.handle_id = h.ROWID \
         LEFT JOIN chat_message_join cmj ON cmj.message_id = m.ROWID \
         LEFT JOIN chat c ON c.ROWID = cmj.chat_id \
         LEFT JOIN message parent ON parent.guid = m.thread_originator_guid \
         LEFT JOIN handle ph ON parent.handle_id = ph.ROWID \
         WHERE m.ROWID > ?1 AND m.text IS NOT NULL \
         ORDER BY m.ROWID ASC \
         LIMIT 50",
    )?;

    let rows = stmt.query_map([last_rowid], |row| {
        let cache_roomnames: Option<String> = row.get(5)?;
        let is_group = cache_roomnames.as_deref().is_some_and(|r| !r.is_empty());
        let reply_text: Option<String> = row.get(9)?;
        let reply_sender: Option<String> = row.get(10)?;
        let reply_to = reply_text.map(|t| ReplyContext {
            text: t,
            sender: reply_sender,
        });

        Ok(IMessagePayload {
            rowid: row.get(0)?,
            text: row.get(1)?,
            sender: row.get(3)?,
            is_from_me: row.get::<_, i32>(4)? != 0,
            chat_identifier: row.get(6)?,
            chat_guid: row.get(7)?,
            is_group,
            reply_to,
            attachments: Vec::new(),
        })
    })?;

    let mut messages = Vec::new();
    for row in rows {
        match row {
            Ok(mut msg) => {
                msg.attachments = load_attachment_meta(conn, msg.rowid);
                messages.push(msg);
            }
            Err(e) => warn!("Failed to parse message row: {e}"),
        }
    }

    Ok(messages)
}

/// Fetch attachment metadata for a single message. Returns empty on error so
/// one malformed attachment row never drops the message itself.
fn load_attachment_meta(conn: &rusqlite::Connection, message_rowid: i64) -> Vec<AttachmentMeta> {
    let sql = "SELECT a.filename, a.transfer_name, a.mime_type \
               FROM attachment a \
               JOIN message_attachment_join maj ON maj.attachment_id = a.ROWID \
               WHERE maj.message_id = ?1";
    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to prepare attachment query: {e}");
            return Vec::new();
        }
    };
    let rows = stmt.query_map([message_rowid], |row| {
        let filename: Option<String> = row.get(0)?;
        let transfer_name: Option<String> = row.get(1)?;
        let mime_type: Option<String> = row.get(2)?;
        Ok((filename, transfer_name, mime_type))
    });
    let mut out = Vec::new();
    match rows {
        Ok(iter) => {
            for r in iter {
                match r {
                    Ok((Some(path), transfer_name, mime_type)) => {
                        out.push(AttachmentMeta {
                            path,
                            filename: transfer_name,
                            mime_type,
                        });
                    }
                    Ok((None, _, _)) => {
                        // Attachment row with no filename — nothing to read.
                    }
                    Err(e) => warn!("Failed to read attachment row: {e}"),
                }
            }
        }
        Err(e) => warn!("Failed to query attachments for rowid {message_rowid}: {e}"),
    }
    out
}

fn open_chatdb(db_uri: &str) -> Result<rusqlite::Connection> {
    rusqlite::Connection::open_with_flags(
        db_uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .context("Failed to open chat.db (check Full Disk Access)")
}

async fn load_last_rowid(state_path: &std::path::Path) -> i64 {
    let content = match tokio::fs::read_to_string(state_path).await {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(e) => {
            warn!("Failed to parse iMessage state file, starting from rowid 0: {e}");
            return 0;
        }
    };
    match serde_json::from_str::<serde_json::Value>(&content) {
        Ok(v) => v
            .get("last_rowid")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0),
        Err(e) => {
            warn!("Failed to parse iMessage state file, starting from rowid 0: {e}");
            0
        }
    }
}

async fn save_last_rowid(state_path: &std::path::Path, rowid: i64) {
    let state = format!("{{\"last_rowid\": {rowid}}}");
    if let Err(e) = tokio::fs::write(state_path, &state).await {
        warn!("Failed to save iMessage state: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- load_last_rowid / save_last_rowid --

    #[tokio::test]
    async fn load_last_rowid_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        assert_eq!(load_last_rowid(&path).await, 0);
    }

    #[tokio::test]
    async fn save_and_load_last_rowid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        save_last_rowid(&path, 42).await;
        assert_eq!(load_last_rowid(&path).await, 42);
    }

    #[tokio::test]
    async fn load_last_rowid_corrupt_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "not json at all").unwrap();
        assert_eq!(load_last_rowid(&path).await, 0);
    }

    #[tokio::test]
    async fn load_last_rowid_missing_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, r#"{"other_key": 99}"#).unwrap();
        assert_eq!(load_last_rowid(&path).await, 0);
    }

    #[tokio::test]
    async fn save_last_rowid_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        save_last_rowid(&path, 10).await;
        save_last_rowid(&path, 20).await;
        assert_eq!(load_last_rowid(&path).await, 20);
    }

    // -- evaluate_message --

    fn make_payload(
        rowid: i64,
        text: Option<&str>,
        sender: Option<&str>,
        is_from_me: bool,
        chat_identifier: Option<&str>,
    ) -> IMessagePayload {
        IMessagePayload {
            rowid,
            text: text.map(|t| t.to_string()),
            sender: sender.map(|s| s.to_string()),
            is_from_me,
            chat_identifier: chat_identifier.map(|c| c.to_string()),
            chat_guid: None,
            is_group: false,
            reply_to: None,
            attachments: Vec::new(),
        }
    }

    fn no_allowlist() -> Option<&'static [String]> {
        None
    }

    #[test]
    fn evaluate_drops_empty_text() {
        let payload = make_payload(1, None, Some("+1234"), false, Some("chat1"));
        let mut echo = EchoCache::new();
        let mut self_chat = SelfChatCache::new();
        match evaluate_message(&payload, &mut echo, &mut self_chat, no_allowlist()) {
            InboundDecision::Drop { reason } => assert!(reason.contains("empty")),
            InboundDecision::Dispatch { .. } => panic!("expected Drop"),
        }
    }

    #[test]
    fn evaluate_drops_whitespace_only_text() {
        let payload = make_payload(1, Some("   "), Some("+1234"), false, Some("chat1"));
        let mut echo = EchoCache::new();
        let mut self_chat = SelfChatCache::new();
        match evaluate_message(&payload, &mut echo, &mut self_chat, no_allowlist()) {
            InboundDecision::Drop { reason } => assert!(reason.contains("empty")),
            InboundDecision::Dispatch { .. } => panic!("expected Drop"),
        }
    }

    #[test]
    fn evaluate_drops_from_me() {
        let payload = make_payload(1, Some("hello"), Some("+1234"), true, Some("chat1"));
        let mut echo = EchoCache::new();
        let mut self_chat = SelfChatCache::new();
        match evaluate_message(&payload, &mut echo, &mut self_chat, no_allowlist()) {
            InboundDecision::Drop { reason } => assert!(reason.contains("is_from_me")),
            InboundDecision::Dispatch { .. } => panic!("expected Drop"),
        }
    }

    #[test]
    fn evaluate_drops_echo() {
        let payload = make_payload(1, Some("hello"), Some("+1234"), false, Some("chat1"));
        let mut echo = EchoCache::new();
        let mut self_chat = SelfChatCache::new();
        echo.remember("hello", None);
        match evaluate_message(&payload, &mut echo, &mut self_chat, no_allowlist()) {
            InboundDecision::Drop { reason } => assert!(reason.contains("echo")),
            InboundDecision::Dispatch { .. } => panic!("expected Drop"),
        }
    }

    #[test]
    fn evaluate_drops_self_chat_echo() {
        let payload = make_payload(1, Some("hello"), Some("+1234"), false, Some("chat1"));
        let mut echo = EchoCache::new();
        let mut self_chat = SelfChatCache::new();
        self_chat.remember("hello", "chat1");
        match evaluate_message(&payload, &mut echo, &mut self_chat, no_allowlist()) {
            InboundDecision::Drop { reason } => assert!(reason.contains("self-chat")),
            InboundDecision::Dispatch { .. } => panic!("expected Drop"),
        }
    }

    #[test]
    fn evaluate_drops_reflected_content() {
        let payload = make_payload(
            1,
            Some("<thinking>leaked thought</thinking>"),
            Some("+1234"),
            false,
            Some("chat1"),
        );
        let mut echo = EchoCache::new();
        let mut self_chat = SelfChatCache::new();
        match evaluate_message(&payload, &mut echo, &mut self_chat, no_allowlist()) {
            InboundDecision::Drop { reason } => assert!(reason.contains("reflected")),
            InboundDecision::Dispatch { .. } => panic!("expected Drop"),
        }
    }

    #[test]
    fn evaluate_dispatches_clean_message() {
        let payload = make_payload(1, Some("hello"), Some("+1 234"), false, Some("chat1"));
        let mut echo = EchoCache::new();
        let mut self_chat = SelfChatCache::new();
        match evaluate_message(&payload, &mut echo, &mut self_chat, no_allowlist()) {
            InboundDecision::Dispatch {
                sender_id,
                text,
                channel_id,
            } => {
                // Sender is normalized: "+1 234" → "+1234"
                assert_eq!(sender_id, "+1234");
                assert_eq!(text, "hello");
                assert_eq!(channel_id, "chat1");
            }
            InboundDecision::Drop { reason } => panic!("expected Dispatch, got Drop: {reason}"),
        }
    }

    #[test]
    fn evaluate_uses_unknown_for_missing_sender() {
        let payload = make_payload(1, Some("hello"), None, false, Some("chat1"));
        let mut echo = EchoCache::new();
        let mut self_chat = SelfChatCache::new();
        match evaluate_message(&payload, &mut echo, &mut self_chat, no_allowlist()) {
            InboundDecision::Dispatch { sender_id, .. } => {
                assert_eq!(sender_id, "unknown");
            }
            InboundDecision::Drop { .. } => panic!("expected Dispatch"),
        }
    }

    #[test]
    fn evaluate_uses_unknown_for_missing_chat_id() {
        let payload = make_payload(1, Some("hello"), Some("+1234"), false, None);
        let mut echo = EchoCache::new();
        let mut self_chat = SelfChatCache::new();
        match evaluate_message(&payload, &mut echo, &mut self_chat, no_allowlist()) {
            InboundDecision::Dispatch { channel_id, .. } => {
                assert_eq!(channel_id, "unknown");
            }
            InboundDecision::Drop { .. } => panic!("expected Dispatch"),
        }
    }

    #[test]
    fn evaluate_from_me_populates_self_chat_cache() {
        let payload = make_payload(1, Some("my message"), Some("+1234"), true, Some("chat1"));
        let mut echo = EchoCache::new();
        let mut self_chat = SelfChatCache::new();
        evaluate_message(&payload, &mut echo, &mut self_chat, no_allowlist());
        assert!(self_chat.is_self_echo("my message", "chat1"));
    }

    // -- group allowlist gate (Gate 0) --

    #[test]
    fn allowlist_blocks_unlisted_group() {
        let mut payload = make_payload(1, Some("hi"), Some("+1234"), false, Some("chat1"));
        payload.is_group = true;
        payload.chat_guid = Some("guid-other".into());
        let allow: Vec<String> = vec!["guid-allowed".into()];
        let mut echo = EchoCache::new();
        let mut self_chat = SelfChatCache::new();
        match evaluate_message(&payload, &mut echo, &mut self_chat, Some(&allow)) {
            InboundDecision::Drop { reason } => assert!(reason.contains("not in allowlist")),
            InboundDecision::Dispatch { .. } => panic!("expected Drop"),
        }
    }

    #[test]
    fn allowlist_admits_listed_group_by_guid() {
        let mut payload = make_payload(1, Some("hi"), Some("+1234"), false, Some("chat1"));
        payload.is_group = true;
        payload.chat_guid = Some("guid-allowed".into());
        let allow: Vec<String> = vec!["guid-allowed".into()];
        let mut echo = EchoCache::new();
        let mut self_chat = SelfChatCache::new();
        match evaluate_message(&payload, &mut echo, &mut self_chat, Some(&allow)) {
            InboundDecision::Dispatch { .. } => {}
            InboundDecision::Drop { reason } => panic!("expected Dispatch, got Drop: {reason}"),
        }
    }

    #[test]
    fn allowlist_admits_listed_group_by_chat_identifier() {
        let mut payload = make_payload(1, Some("hi"), Some("+1234"), false, Some("chat-id-1"));
        payload.is_group = true;
        payload.chat_guid = Some("guid-other".into());
        let allow: Vec<String> = vec!["chat-id-1".into()];
        let mut echo = EchoCache::new();
        let mut self_chat = SelfChatCache::new();
        match evaluate_message(&payload, &mut echo, &mut self_chat, Some(&allow)) {
            InboundDecision::Dispatch { .. } => {}
            InboundDecision::Drop { reason } => panic!("expected Dispatch, got Drop: {reason}"),
        }
    }

    #[test]
    fn allowlist_does_not_filter_dms() {
        // DM: is_group=false, allowlist set but no entries match
        let payload = make_payload(1, Some("hi"), Some("+1234"), false, Some("dm-chat"));
        let allow: Vec<String> = vec!["only-this-group".into()];
        let mut echo = EchoCache::new();
        let mut self_chat = SelfChatCache::new();
        match evaluate_message(&payload, &mut echo, &mut self_chat, Some(&allow)) {
            InboundDecision::Dispatch { .. } => {}
            InboundDecision::Drop { reason } => panic!("expected Dispatch, got Drop: {reason}"),
        }
    }

    #[test]
    fn allowlist_none_means_all_groups_pass() {
        let mut payload = make_payload(1, Some("hi"), Some("+1234"), false, Some("any-chat"));
        payload.is_group = true;
        payload.chat_guid = Some("any-guid".into());
        let mut echo = EchoCache::new();
        let mut self_chat = SelfChatCache::new();
        match evaluate_message(&payload, &mut echo, &mut self_chat, None) {
            InboundDecision::Dispatch { .. } => {}
            InboundDecision::Drop { reason } => panic!("expected Dispatch, got Drop: {reason}"),
        }
    }

    #[test]
    fn allowlist_empty_blocks_all_groups() {
        let mut payload = make_payload(1, Some("hi"), Some("+1234"), false, Some("any-chat"));
        payload.is_group = true;
        payload.chat_guid = Some("any-guid".into());
        let allow: Vec<String> = Vec::new();
        let mut echo = EchoCache::new();
        let mut self_chat = SelfChatCache::new();
        match evaluate_message(&payload, &mut echo, &mut self_chat, Some(&allow)) {
            InboundDecision::Drop { reason } => assert!(reason.contains("not in allowlist")),
            InboundDecision::Dispatch { .. } => panic!("expected Drop"),
        }
    }

    // -- reply quoting --

    #[test]
    fn reply_context_prefixed_to_dispatched_text() {
        let mut payload = make_payload(1, Some("ok"), Some("+1234"), false, Some("chat1"));
        payload.reply_to = Some(ReplyContext {
            text: "Are you free?".into(),
            sender: Some("alice@example.com".into()),
        });
        let mut echo = EchoCache::new();
        let mut self_chat = SelfChatCache::new();
        match evaluate_message(&payload, &mut echo, &mut self_chat, no_allowlist()) {
            InboundDecision::Dispatch { text, .. } => {
                assert!(text.contains("In reply to alice@example.com"));
                assert!(text.contains("> Are you free?"));
                assert!(text.ends_with("\n\nok"));
            }
            InboundDecision::Drop { reason } => panic!("expected Dispatch, got Drop: {reason}"),
        }
    }

    #[test]
    fn reply_context_multiline_each_line_quoted() {
        let mut payload = make_payload(1, Some("yes"), Some("+1234"), false, Some("chat1"));
        payload.reply_to = Some(ReplyContext {
            text: "line one\nline two".into(),
            sender: None,
        });
        let mut echo = EchoCache::new();
        let mut self_chat = SelfChatCache::new();
        match evaluate_message(&payload, &mut echo, &mut self_chat, no_allowlist()) {
            InboundDecision::Dispatch { text, .. } => {
                assert!(text.contains("In reply to you"));
                assert!(text.contains("> line one\n> line two"));
            }
            InboundDecision::Drop { .. } => panic!("expected Dispatch"),
        }
    }

    // -- SQL fixture tests --

    /// Build a minimal chat.db schema sufficient to exercise poll_new_messages.
    /// Mirrors the columns Borg actually reads — not a full chat.db replica.
    fn build_fixture_db() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE handle (
                ROWID INTEGER PRIMARY KEY,
                id TEXT
            );
            CREATE TABLE chat (
                ROWID INTEGER PRIMARY KEY,
                chat_identifier TEXT,
                guid TEXT
            );
            CREATE TABLE message (
                ROWID INTEGER PRIMARY KEY,
                guid TEXT,
                text TEXT,
                date INTEGER,
                handle_id INTEGER,
                is_from_me INTEGER DEFAULT 0,
                cache_roomnames TEXT,
                thread_originator_guid TEXT
            );
            CREATE TABLE chat_message_join (
                chat_id INTEGER,
                message_id INTEGER
            );
            CREATE TABLE attachment (
                ROWID INTEGER PRIMARY KEY,
                filename TEXT,
                transfer_name TEXT,
                mime_type TEXT
            );
            CREATE TABLE message_attachment_join (
                message_id INTEGER,
                attachment_id INTEGER
            );
            "#,
        )
        .unwrap();
        conn
    }

    #[test]
    fn sql_extracts_basic_message() {
        let conn = build_fixture_db();
        conn.execute(
            "INSERT INTO handle (ROWID, id) VALUES (1, '+15551234567')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chat (ROWID, chat_identifier, guid) VALUES (1, '+15551234567', 'iMessage;-;+15551234567')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (ROWID, guid, text, handle_id, is_from_me) \
             VALUES (10, 'msg-10', 'hello world', 1, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chat_message_join (chat_id, message_id) VALUES (1, 10)",
            [],
        )
        .unwrap();

        let msgs = poll_new_messages_with_conn(&conn, 0).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].rowid, 10);
        assert_eq!(msgs[0].text.as_deref(), Some("hello world"));
        assert_eq!(msgs[0].sender.as_deref(), Some("+15551234567"));
        assert_eq!(msgs[0].chat_identifier.as_deref(), Some("+15551234567"));
        assert!(!msgs[0].is_group);
        assert!(msgs[0].reply_to.is_none());
        assert!(msgs[0].attachments.is_empty());
    }

    #[test]
    fn sql_extracts_reply_context() {
        let conn = build_fixture_db();
        // Parent message
        conn.execute(
            "INSERT INTO handle (ROWID, id) VALUES (1, 'alice@x.com')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO handle (ROWID, id) VALUES (2, 'bob@x.com')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO chat (ROWID, chat_identifier, guid) VALUES (1, 'g1', 'g1')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (ROWID, guid, text, handle_id) VALUES (5, 'parent-guid', 'are you free?', 1)",
            [],
        )
        .unwrap();
        // Child reply with thread_originator_guid pointing to parent's guid
        conn.execute(
            "INSERT INTO message (ROWID, guid, text, handle_id, thread_originator_guid) \
             VALUES (10, 'child-guid', 'yes', 2, 'parent-guid')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chat_message_join (chat_id, message_id) VALUES (1, 5), (1, 10)",
            [],
        )
        .unwrap();

        let msgs = poll_new_messages_with_conn(&conn, 9).unwrap();
        assert_eq!(msgs.len(), 1, "only the reply (rowid > 9) should appear");
        let reply = msgs[0].reply_to.as_ref().expect("reply_to populated");
        assert_eq!(reply.text, "are you free?");
        assert_eq!(reply.sender.as_deref(), Some("alice@x.com"));
    }

    #[test]
    fn sql_no_reply_returns_none() {
        let conn = build_fixture_db();
        conn.execute("INSERT INTO handle (ROWID, id) VALUES (1, '+1')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO chat (ROWID, chat_identifier, guid) VALUES (1, 'c', 'c')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (ROWID, guid, text, handle_id) VALUES (1, 'g1', 'hi', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chat_message_join (chat_id, message_id) VALUES (1, 1)",
            [],
        )
        .unwrap();
        let msgs = poll_new_messages_with_conn(&conn, 0).unwrap();
        assert!(msgs[0].reply_to.is_none());
    }

    #[test]
    fn sql_extracts_attachment_metadata() {
        let conn = build_fixture_db();
        conn.execute("INSERT INTO handle (ROWID, id) VALUES (1, '+1')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO chat (ROWID, chat_identifier, guid) VALUES (1, 'c', 'c')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (ROWID, guid, text, handle_id) VALUES (1, 'g', 'see attached', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chat_message_join (chat_id, message_id) VALUES (1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO attachment (ROWID, filename, transfer_name, mime_type) \
             VALUES (100, '/tmp/borg-test-image.png', 'image.png', 'image/png')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message_attachment_join (message_id, attachment_id) VALUES (1, 100)",
            [],
        )
        .unwrap();

        let msgs = poll_new_messages_with_conn(&conn, 0).unwrap();
        assert_eq!(msgs[0].attachments.len(), 1);
        let att = &msgs[0].attachments[0];
        assert_eq!(att.path, "/tmp/borg-test-image.png");
        assert_eq!(att.filename.as_deref(), Some("image.png"));
        assert_eq!(att.mime_type.as_deref(), Some("image/png"));
    }

    #[test]
    fn sql_attachment_with_null_filename_is_skipped() {
        let conn = build_fixture_db();
        conn.execute("INSERT INTO handle (ROWID, id) VALUES (1, '+1')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO chat (ROWID, chat_identifier, guid) VALUES (1, 'c', 'c')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (ROWID, guid, text, handle_id) VALUES (1, 'g', 't', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chat_message_join (chat_id, message_id) VALUES (1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO attachment (ROWID, filename, transfer_name, mime_type) \
             VALUES (100, NULL, 'image.png', 'image/png')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message_attachment_join (message_id, attachment_id) VALUES (1, 100)",
            [],
        )
        .unwrap();
        let msgs = poll_new_messages_with_conn(&conn, 0).unwrap();
        // Message still present; just no attachments.
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].attachments.is_empty());
    }

    #[test]
    fn sql_group_chat_flag_set_from_cache_roomnames() {
        let conn = build_fixture_db();
        conn.execute("INSERT INTO handle (ROWID, id) VALUES (1, '+1')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO chat (ROWID, chat_identifier, guid) VALUES (1, 'chat-grp', 'group-guid-1')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (ROWID, guid, text, handle_id, cache_roomnames) \
             VALUES (1, 'g', 'hi all', 1, 'chat-grp')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chat_message_join (chat_id, message_id) VALUES (1, 1)",
            [],
        )
        .unwrap();
        let msgs = poll_new_messages_with_conn(&conn, 0).unwrap();
        assert!(msgs[0].is_group);
        assert_eq!(msgs[0].chat_guid.as_deref(), Some("group-guid-1"));
    }

    // -- load_attachments --

    #[test]
    fn load_attachments_skips_missing_files() {
        let metas = vec![AttachmentMeta {
            path: "/nonexistent/path/that/does/not/exist.png".into(),
            filename: Some("x.png".into()),
            mime_type: Some("image/png".into()),
        }];
        let out = load_attachments(&metas);
        assert!(out.is_empty(), "missing files should be skipped, not panic");
    }

    #[test]
    fn load_attachments_reads_and_base64s_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello.bin");
        std::fs::write(&path, b"hello bytes").unwrap();
        let metas = vec![AttachmentMeta {
            path: path.to_string_lossy().into_owned(),
            filename: Some("hello.bin".into()),
            mime_type: Some("application/octet-stream".into()),
        }];
        let out = load_attachments(&metas);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].mime_type, "application/octet-stream");
        assert_eq!(out[0].filename.as_deref(), Some("hello.bin"));
        // base64("hello bytes") == "aGVsbG8gYnl0ZXM="
        assert_eq!(out[0].data, "aGVsbG8gYnl0ZXM=");
    }

    #[test]
    fn load_attachments_skips_oversized_files() {
        // Ensure the size guard rejects without OOM-loading the file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        // We can't easily fabricate a >10MB file in a unit test without doing it,
        // so write 11MB of zeros — fast on tmpfs / APFS scratch.
        std::fs::write(&path, vec![0u8; (MAX_ATTACHMENT_BYTES + 1) as usize]).unwrap();
        let metas = vec![AttachmentMeta {
            path: path.to_string_lossy().into_owned(),
            filename: None,
            mime_type: None,
        }];
        let out = load_attachments(&metas);
        assert!(out.is_empty());
    }

    #[test]
    fn expand_tilde_expands_home_prefix() {
        let expanded = expand_tilde("~/some/file.txt");
        let home = dirs::home_dir().unwrap();
        assert_eq!(expanded, home.join("some/file.txt"));
    }

    #[test]
    fn expand_tilde_passthrough_for_absolute() {
        let expanded = expand_tilde("/abs/path/file");
        assert_eq!(expanded, PathBuf::from("/abs/path/file"));
    }
}
