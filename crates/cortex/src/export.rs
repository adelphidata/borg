//! Session export — shared logic behind `borg export`, `/export`, and the
//! `e` key in the TUI sessions popup. All three surfaces call `export_session`
//! so the serialization stays consistent.
//!
//! JSON is the default format (round-trippable via `Session` serde). CSV and
//! TXT are lossy, human-oriented flattenings for spreadsheets / grepping.

use anyhow::{anyhow, Context, Result};
use std::str::FromStr;

use crate::db::Database;
use crate::session::{Session, SessionMeta};
use crate::types::{ContentPart, MessageContent, Role};

/// Output format selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    /// Pretty-printed JSON (lossless; full `Session` including tool_calls).
    Json,
    /// CSV with `idx,role,timestamp,content,tool_calls_json` columns.
    Csv,
    /// Plain text, human-readable.
    Txt,
}

impl ExportFormat {
    /// File extension for this format (no dot).
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Json => "json",
            ExportFormat::Csv => "csv",
            ExportFormat::Txt => "txt",
        }
    }
}

impl FromStr for ExportFormat {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "json" => Ok(ExportFormat::Json),
            "csv" => Ok(ExportFormat::Csv),
            "txt" | "text" => Ok(ExportFormat::Txt),
            other => Err(anyhow!(
                "unsupported export format '{other}' (expected: json, csv, txt)"
            )),
        }
    }
}

/// Options controlling export behavior.
#[derive(Debug, Clone, Copy)]
pub struct ExportOptions {
    /// Output format.
    pub format: ExportFormat,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            format: ExportFormat::Json,
        }
    }
}

/// Load `session_id` from the DB and serialize it.
///
/// Returns `(rendered, suggested_filename)`. The caller decides whether to
/// write to stdout, a file, or both.
pub fn export_session(
    db: &Database,
    session_id: &str,
    opts: ExportOptions,
) -> Result<(String, String)> {
    let session = load_session(db, session_id)?;
    let rendered = match opts.format {
        ExportFormat::Json => render_json(&session)?,
        ExportFormat::Csv => render_csv(&session),
        ExportFormat::Txt => render_txt(&session),
    };
    let filename = suggested_filename(&session.meta, opts.format);
    Ok((rendered, filename))
}

/// Reconstruct a full `Session` from the DB. Fails loudly if the session
/// row or messages are missing — we don't silently emit an empty export
/// (see CLAUDE.md "no lying" invariant).
fn load_session(db: &Database, session_id: &str) -> Result<Session> {
    let row = db
        .session_by_id(session_id)
        .with_context(|| format!("looking up session '{session_id}'"))?
        .ok_or_else(|| anyhow!("session '{session_id}' not found"))?;

    let rows = db
        .load_session_messages(session_id)
        .with_context(|| format!("loading messages for session '{session_id}'"))?;

    let mut messages = Vec::with_capacity(rows.len());
    for mr in rows {
        let role = match mr.role.as_str() {
            "system" => Role::System,
            "user" => Role::User,
            "assistant" => Role::Assistant,
            "tool" => Role::Tool,
            other => {
                tracing::warn!("export: skipping message with unknown role '{other}'");
                continue;
            }
        };
        let tool_calls =
            mr.tool_calls_json
                .as_deref()
                .and_then(|j| match serde_json::from_str(j) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        tracing::warn!("export: failed to parse tool_calls_json: {e}");
                        None
                    }
                });
        let content = if let Some(parts_json) = &mr.content_parts_json {
            match serde_json::from_str(parts_json) {
                Ok(parts) => Some(MessageContent::Parts(parts)),
                Err(e) => {
                    tracing::warn!("export: failed to parse content_parts_json: {e}");
                    mr.content.map(MessageContent::Text)
                }
            }
        } else {
            mr.content.map(MessageContent::Text)
        };
        messages.push(crate::types::Message {
            role,
            content,
            tool_calls,
            tool_call_id: mr.tool_call_id,
            timestamp: mr.timestamp,
        });
    }

    let meta = SessionMeta {
        id: row.id,
        title: row.title,
        created_at: chrono::DateTime::from_timestamp(row.created_at, 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default(),
        updated_at: chrono::DateTime::from_timestamp(row.updated_at, 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default(),
        message_count: messages.len(),
    };
    Ok(Session { meta, messages })
}

fn render_json(session: &Session) -> Result<String> {
    serde_json::to_string_pretty(session).context("serializing session as JSON")
}

fn render_csv(session: &Session) -> String {
    let mut out = String::new();
    out.push_str("idx,role,timestamp,content,tool_calls_json\n");
    for (idx, msg) in session.messages.iter().enumerate() {
        let role = match msg.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        let ts = msg.timestamp.as_deref().unwrap_or("");
        let content = flatten_content(msg.content.as_ref());
        let tc = msg
            .tool_calls
            .as_ref()
            .map(|tc| match serde_json::to_string(tc) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("export: dropping tool_calls for CSV row {idx}: {e}");
                    String::new()
                }
            })
            .unwrap_or_default();
        out.push_str(&format!(
            "{idx},{},{},{},{}\n",
            csv_escape(role),
            csv_escape(ts),
            csv_escape(&csv_defuse_formula(&content)),
            csv_escape(&csv_defuse_formula(&tc)),
        ));
    }
    out
}

fn render_txt(session: &Session) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n", session.meta.title));
    out.push_str(&format!("# id: {}\n", session.meta.id));
    out.push_str(&format!("# messages: {}\n\n", session.meta.message_count));
    for msg in &session.messages {
        let role = match msg.role {
            Role::System => "SYSTEM",
            Role::User => "USER",
            Role::Assistant => "ASSISTANT",
            Role::Tool => "TOOL",
        };
        let ts = msg.timestamp.as_deref().unwrap_or("-");
        out.push_str(&format!("[{role}] {ts}\n"));
        let body = flatten_content(msg.content.as_ref());
        if !body.is_empty() {
            out.push_str(&body);
            if !body.ends_with('\n') {
                out.push('\n');
            }
        }
        if let Some(tcs) = &msg.tool_calls {
            for tc in tcs {
                out.push_str(&format!(
                    "  -> tool_call {} {}({})\n",
                    tc.id, tc.function.name, tc.function.arguments
                ));
            }
        }
        out.push_str("---\n");
    }
    out
}

/// Flatten `MessageContent` into a single text blob, with bracketed placeholders
/// for non-text parts so CSV/TXT readers still see *something* for image/audio.
fn flatten_content(content: Option<&MessageContent>) -> String {
    let Some(c) = content else {
        return String::new();
    };
    match c {
        MessageContent::Text(s) => s.clone(),
        MessageContent::Parts(parts) => {
            let mut buf = String::new();
            for p in parts {
                match p {
                    ContentPart::Text(t) => {
                        if !buf.is_empty() && !buf.ends_with('\n') {
                            buf.push(' ');
                        }
                        buf.push_str(t);
                    }
                    ContentPart::ImageBase64 { media } => {
                        buf.push_str(&format!(
                            " [image: {}]",
                            media.filename.as_deref().unwrap_or("attached")
                        ));
                    }
                    ContentPart::ImageUrl { url } => {
                        buf.push_str(&format!(" [image: {url}]"));
                    }
                    ContentPart::AudioBase64 { media } => {
                        buf.push_str(&format!(
                            " [audio: {}]",
                            media.filename.as_deref().unwrap_or("attached")
                        ));
                    }
                }
            }
            buf
        }
    }
}

/// Prefix fields that spreadsheet apps (Excel, Sheets, Numbers) would execute
/// as formulas with a leading single quote. See OWASP "CSV Injection". Only
/// applied to free-form content fields — not to known-safe columns like idx/role.
fn csv_defuse_formula(s: &str) -> String {
    match s.chars().next() {
        Some('=' | '+' | '-' | '@' | '\t' | '\r') => format!("'{s}"),
        _ => s.to_string(),
    }
}

/// RFC 4180 CSV field escaping: wrap in quotes if the field contains `,` `"`
/// `\n` or `\r`, doubling any interior quotes.
fn csv_escape(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        let doubled = s.replace('"', "\"\"");
        format!("\"{doubled}\"")
    } else {
        s.to_string()
    }
}

/// `borg-session-{short_id}-{yyyymmdd-HHMMSS}.{ext}`.
fn suggested_filename(meta: &SessionMeta, format: ExportFormat) -> String {
    let short: String = meta.id.chars().take(8).collect();
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    format!("borg-session-{short}-{stamp}.{}", format.extension())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_session(db: &Database, id: &str) {
        db.upsert_session(
            id,
            1_700_000_000,
            1_700_000_100,
            0,
            "test-model",
            "Demo chat",
        )
        .unwrap();
        db.insert_message(
            id,
            "user",
            Some("hello, agent"),
            None,
            None,
            Some("2024-01-01T00:00:00Z"),
            None,
        )
        .unwrap();
        db.insert_message(
            id,
            "assistant",
            Some("calling a tool"),
            Some(r#"[{"id":"tc1","type":"function","function":{"name":"run_shell","arguments":"{\"cmd\":\"ls\"}"}}]"#),
            None,
            Some("2024-01-01T00:00:01Z"),
            None,
        )
        .unwrap();
        db.insert_message(
            id,
            "tool",
            Some("file1.txt\nfile2.txt"),
            None,
            Some("tc1"),
            Some("2024-01-01T00:00:02Z"),
            None,
        )
        .unwrap();
    }

    #[test]
    fn format_from_str_accepts_variants() {
        for (input, expected) in &[
            ("json", ExportFormat::Json),
            ("JSON", ExportFormat::Json),
            ("  csv  ", ExportFormat::Csv),
            ("txt", ExportFormat::Txt),
            ("text", ExportFormat::Txt),
        ] {
            assert_eq!(ExportFormat::from_str(input).unwrap(), *expected);
        }
        for bad in &["xml", "yaml", ""] {
            assert!(ExportFormat::from_str(bad).is_err(), "'{bad}' should fail");
        }
    }

    #[test]
    fn export_json_round_trips() {
        let db = Database::test_db();
        seed_session(&db, "sess-json");
        let (out, filename) = export_session(&db, "sess-json", ExportOptions::default()).unwrap();

        assert!(filename.starts_with("borg-session-sess-jso-"));
        assert!(filename.ends_with(".json"));

        let parsed: Session = serde_json::from_str(&out).expect("json round-trip");
        assert_eq!(parsed.meta.id, "sess-json");
        assert_eq!(parsed.messages.len(), 3);
        assert_eq!(parsed.messages[0].role, Role::User);
        assert_eq!(parsed.messages[1].role, Role::Assistant);
        assert_eq!(parsed.messages[2].role, Role::Tool);

        let tcs = parsed.messages[1].tool_calls.as_ref().expect("tool_calls");
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].function.name, "run_shell");
    }

    #[test]
    fn export_csv_has_header_and_rows() {
        let db = Database::test_db();
        seed_session(&db, "sess-csv");
        let opts = ExportOptions {
            format: ExportFormat::Csv,
        };
        let (out, filename) = export_session(&db, "sess-csv", opts).unwrap();

        assert!(filename.ends_with(".csv"));
        assert!(out.starts_with("idx,role,timestamp,content,tool_calls_json\n"));

        // Each CSV record starts at the beginning of a line with `<idx>,`. The
        // seeded tool message embeds a literal newline in its content — that's
        // exactly the case quoting exists for, so count by logical row id here
        // rather than raw `.lines()`.
        assert!(out.contains("\n0,user,2024-01-01T00:00:00Z,"));
        assert!(
            out.contains("\"hello, agent\""),
            "comma field must be quoted"
        );
        assert!(out.contains("\n1,assistant,"));
        assert!(out.contains("run_shell"), "tool_calls_json should appear");
        assert!(out.contains("\n2,tool,"));
        // The tool message's content has an embedded \n — must be quoted.
        assert!(out.contains("\"file1.txt\nfile2.txt\""));
    }

    #[test]
    fn export_csv_escapes_quotes_and_newlines() {
        let db = Database::test_db();
        db.upsert_session("sess-esc", 1, 2, 0, "m", "Escape Test")
            .unwrap();
        db.insert_message(
            "sess-esc",
            "user",
            Some("she said \"hi\"\nand left"),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let opts = ExportOptions {
            format: ExportFormat::Csv,
        };
        let (out, _) = export_session(&db, "sess-esc", opts).unwrap();
        // Doubled quote + wrapped in quotes because of `,` / `"` / `\n`.
        assert!(out.contains("\"she said \"\"hi\"\"\nand left\""));
    }

    #[test]
    fn export_txt_is_human_readable() {
        let db = Database::test_db();
        seed_session(&db, "sess-txt");
        let opts = ExportOptions {
            format: ExportFormat::Txt,
        };
        let (out, filename) = export_session(&db, "sess-txt", opts).unwrap();

        assert!(filename.ends_with(".txt"));
        assert!(out.contains("# Demo chat"));
        assert!(out.contains("[USER]"));
        assert!(out.contains("[ASSISTANT]"));
        assert!(out.contains("[TOOL]"));
        assert!(out.contains("hello, agent"));
        assert!(out.contains("-> tool_call tc1 run_shell"));
    }

    #[test]
    fn export_missing_session_errors() {
        let db = Database::test_db();
        let err = export_session(&db, "does-not-exist", ExportOptions::default())
            .expect_err("should fail");
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn flatten_content_handles_multimodal_parts() {
        use crate::types::MediaData;
        let content = MessageContent::Parts(vec![
            ContentPart::Text("look:".to_string()),
            ContentPart::ImageBase64 {
                media: MediaData {
                    mime_type: "image/png".to_string(),
                    data: "xxx".to_string(),
                    filename: Some("photo.png".to_string()),
                },
            },
            ContentPart::Text("nice".to_string()),
        ]);
        let out = flatten_content(Some(&content));
        assert!(out.contains("look:"));
        assert!(out.contains("[image: photo.png]"));
        assert!(out.contains("nice"));
    }

    #[test]
    fn suggested_filename_shape() {
        let meta = SessionMeta {
            id: "abcdef12345678".to_string(),
            title: "t".to_string(),
            created_at: "x".to_string(),
            updated_at: "x".to_string(),
            message_count: 0,
        };
        let name = suggested_filename(&meta, ExportFormat::Json);
        let re = regex::Regex::new(r"^borg-session-abcdef12-\d{8}-\d{6}\.json$").unwrap();
        assert!(re.is_match(&name), "filename was {name}");
    }

    #[test]
    fn csv_defuses_formula_prefixes() {
        let db = Database::test_db();
        db.upsert_session("sess-inj", 1, 2, 0, "m", "Inj").unwrap();
        // Cover all four dangerous prefixes spreadsheets evaluate as formulas.
        for (idx, payload) in ["=SUM(A1)", "+1+1", "-cmd", "@IF(1,1,0)"]
            .iter()
            .enumerate()
        {
            db.insert_message(
                "sess-inj",
                "user",
                Some(payload),
                None,
                None,
                Some(&format!("2024-01-01T00:00:{idx:02}Z")),
                None,
            )
            .unwrap();
        }
        let (out, _) = export_session(
            &db,
            "sess-inj",
            ExportOptions {
                format: ExportFormat::Csv,
            },
        )
        .unwrap();
        // Each payload must appear with a literal single-quote prefix so no
        // spreadsheet app evaluates it. A `,` in the raw payload would also
        // trigger RFC 4180 quoting, but these don't contain commas.
        for payload in ["=SUM(A1)", "+1+1", "-cmd", "@IF(1,1,0)"] {
            let defused = format!(",'{payload}");
            // @IF(1,1,0) contains a comma — csv_escape will wrap in quotes.
            let defused_quoted = format!(",\"'{payload}\"");
            assert!(
                out.contains(&defused) || out.contains(&defused_quoted),
                "formula {payload:?} not defused in output: {out}"
            );
            // Safety check: the raw payload must NOT appear without the quote
            // prefix (no false-positive from a lucky substring).
            let raw = format!(",{payload}\n");
            assert!(!out.contains(&raw), "raw {payload:?} leaked: {out}");
        }
    }

    #[test]
    fn suggested_filename_handles_non_ascii_id() {
        let meta = SessionMeta {
            id: "café-session-ümlaut".to_string(),
            title: "t".to_string(),
            created_at: "x".to_string(),
            updated_at: "x".to_string(),
            message_count: 0,
        };
        // Byte-slicing would have panicked on the multibyte boundary.
        let name = suggested_filename(&meta, ExportFormat::Json);
        assert!(name.starts_with("borg-session-café-ses"), "got {name}");
        assert!(name.ends_with(".json"));
    }
}
