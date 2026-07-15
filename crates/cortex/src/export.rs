//! Session export — shared logic behind `borg export`, `/export`, and the
//! `e` key in the TUI sessions popup. All three surfaces call `export_session`
//! so the serialization stays consistent.
//!
//! JSON is the default format (round-trippable via `Session` serde). CSV,
//! TXT, and Markdown are lossy, human-oriented flattenings for spreadsheets /
//! grepping / reading.
//!
//! Every format runs through secret redaction by default, fail-closed: if
//! the redaction step itself fails, the export ABORTS rather than writing
//! unredacted output. Callers opt out explicitly via
//! [`ExportOptions::redact`] (`--no-redact` on the CLI).

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
    /// Markdown transcript: role-headed sections, code fences preserved,
    /// tool calls/results summarized compactly.
    Markdown,
}

impl ExportFormat {
    /// File extension for this format (no dot).
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Json => "json",
            ExportFormat::Csv => "csv",
            ExportFormat::Txt => "txt",
            ExportFormat::Markdown => "md",
        }
    }

    /// Canonical wire/CLI name for this format (inverse of `FromStr`).
    pub fn as_str(self) -> &'static str {
        match self {
            ExportFormat::Json => "json",
            ExportFormat::Csv => "csv",
            ExportFormat::Txt => "txt",
            ExportFormat::Markdown => "md",
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
            "md" | "markdown" => Ok(ExportFormat::Markdown),
            other => Err(anyhow!(
                "unsupported export format '{other}' (expected: json, csv, txt, md)"
            )),
        }
    }
}

/// Options controlling export behavior.
#[derive(Debug, Clone, Copy)]
pub struct ExportOptions {
    /// Output format.
    pub format: ExportFormat,
    /// Run the rendered export through secret redaction (default `true`).
    /// Redaction is fail-closed: a redaction failure aborts the export.
    /// `false` is the explicit `--no-redact` opt-out for raw output.
    pub redact: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            format: ExportFormat::Json,
            redact: true,
        }
    }
}

/// Load `session_id` from the DB and serialize it.
///
/// Returns `(rendered, suggested_filename)`. The caller decides whether to
/// write to stdout, a file, or both.
pub async fn export_session(
    db: &Database,
    session_id: &str,
    opts: ExportOptions,
) -> Result<(String, String)> {
    let session = load_session(db, session_id).await?;
    let rendered = match opts.format {
        ExportFormat::Json => render_json(&session)?,
        ExportFormat::Csv => render_csv(&session),
        ExportFormat::Txt => render_txt(&session),
        ExportFormat::Markdown => render_markdown(&session),
    };
    let rendered = if opts.redact {
        apply_export_redaction(rendered, default_export_redactor)?
    } else {
        rendered
    };
    let filename = suggested_filename(&session.meta, opts.format);
    Ok((rendered, filename))
}

/// Fail-closed redaction gate for exports.
///
/// Runs the fully rendered export through `redactor` and returns the redacted
/// text. If the redactor fails for ANY reason the export is aborted with an
/// error — the unredacted `rendered` string is dropped, never returned. The
/// redactor is a parameter so tests can inject a failing one to exercise the
/// abort path.
fn apply_export_redaction(
    rendered: String,
    redactor: impl FnOnce(&str) -> Result<String>,
) -> Result<String> {
    redactor(&rendered).map_err(|e| {
        anyhow!(e).context(
            "secret redaction failed — aborting export (fail-closed: refusing to \
             write unredacted output; pass --no-redact to export raw)",
        )
    })
}

/// Production redactor: the same secret-pattern machinery guarding channel /
/// TUI / log sinks (see [`crate::security::secrets::redact_secrets`]).
///
/// The pattern engine is infallible by signature, so the only real-world
/// failure mode is a panic inside the regex machinery (e.g. a bad pattern
/// added later). `catch_unwind` converts that panic into an `Err` so the
/// fail-closed gate above aborts the export instead of the panic either
/// killing the daemon task or — worse — some caller swallowing it and
/// writing raw output.
fn default_export_redactor(text: &str) -> Result<String> {
    std::panic::catch_unwind(|| crate::security::secrets::redact_secrets(text))
        .map_err(|_| anyhow!("secret redaction panicked"))
}

/// Reconstruct a full `Session` from the DB. Fails loudly if the session
/// row or messages are missing — we don't silently emit an empty export
/// (see CLAUDE.md "no lying" invariant).
async fn load_session(db: &Database, session_id: &str) -> Result<Session> {
    let row = db
        .session_by_id(session_id)
        .await
        .with_context(|| format!("looking up session '{session_id}'"))?
        .ok_or_else(|| anyhow!("session '{session_id}' not found"))?;

    let rows = db
        .load_session_lineage_rows(session_id)
        .await
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
            seq: Some(mr.id),
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

/// Compact-summary budget for tool-call arguments in Markdown exports (chars).
const MD_TOOL_ARGS_MAX_CHARS: usize = 200;
/// Compact-summary budget for tool-result output in Markdown exports (chars).
const MD_TOOL_OUTPUT_MAX_CHARS: usize = 1000;

/// Truncate `s` to at most `max` characters (not bytes — never splits a
/// multibyte char), appending a marker with the elided count.
fn truncate_chars(s: &str, max: usize) -> String {
    let total = s.chars().count();
    if total <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max).collect();
    format!("{kept}… [+{} chars truncated]", total - max)
}

/// Longest run of consecutive backticks in `s`.
fn longest_backtick_run(s: &str) -> usize {
    let mut longest = 0usize;
    let mut run = 0usize;
    for c in s.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    longest
}

/// A code fence long enough that `content` can never close it early: one
/// backtick more than the longest backtick run inside, minimum three.
fn safe_fence(content: &str) -> String {
    "`".repeat((longest_backtick_run(content) + 1).max(3))
}

/// A CommonMark inline code span that renders `s` literally: delimiter one
/// backtick longer than any run inside, space-padded when `s` begins or ends
/// with a backtick.
fn inline_code(s: &str) -> String {
    let delim = "`".repeat(longest_backtick_run(s) + 1);
    if s.starts_with('`') || s.ends_with('`') {
        format!("{delim} {s} {delim}")
    } else {
        format!("{delim}{s}{delim}")
    }
}

/// Readable Markdown transcript. Role-headed sections; user/assistant text is
/// emitted verbatim (so fenced code blocks inside messages survive intact);
/// tool calls and tool results are summarized compactly — tool name plus
/// truncated args/output — rather than dumped in full.
fn render_markdown(session: &Session) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", session.meta.title));
    out.push_str(&format!(
        "- **Session:** `{}`\n- **Messages:** {}\n\n",
        session.meta.id, session.meta.message_count
    ));
    for msg in &session.messages {
        let role = match msg.role {
            Role::System => "System",
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::Tool => "Tool",
        };
        let ts = msg.timestamp.as_deref().unwrap_or("-");
        out.push_str(&format!("## {role} — {ts}\n\n"));
        let body = flatten_content(msg.content.as_ref());
        match msg.role {
            // Tool results: compact fenced summary, truncated. The fence is
            // sized past any backtick run in the output so embedded ``` can't
            // break the document structure.
            Role::Tool => {
                if !body.is_empty() {
                    let summary = truncate_chars(&body, MD_TOOL_OUTPUT_MAX_CHARS);
                    let fence = safe_fence(&summary);
                    out.push_str(&format!("{fence}\n{summary}\n{fence}\n\n"));
                }
            }
            // Human/model prose is emitted verbatim so authored code fences
            // render as the author wrote them.
            _ => {
                if !body.is_empty() {
                    out.push_str(&body);
                    if !body.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push('\n');
                }
            }
        }
        if let Some(tcs) = &msg.tool_calls {
            for tc in tcs {
                let args = truncate_chars(&tc.function.arguments, MD_TOOL_ARGS_MAX_CHARS);
                out.push_str(&format!(
                    "**Tool call:** `{}` — {}\n\n",
                    tc.function.name,
                    inline_code(&args)
                ));
            }
        }
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
                    ContentPart::VideoBase64 { media } => {
                        buf.push_str(&format!(
                            " [video: {}]",
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

    async fn seed_session(db: &Database, id: &str) {
        db.upsert_session(
            id,
            1_700_000_000,
            1_700_000_100,
            0,
            "test-model",
            "Demo chat",
        )
        .await
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
        .await
        .unwrap();
        db.insert_message(
            id,
            "assistant",
            Some("calling a tool"),
            Some(r#"[{"id":"tc1","type":"function","function":{"name":"run_shell","arguments":"{\"cmd\":\"ls\"}"}}]"#),
            None,
            Some("2024-01-01T00:00:01Z"),
            None,
        ).await
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
        .await
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
            ("md", ExportFormat::Markdown),
            ("markdown", ExportFormat::Markdown),
            ("Markdown", ExportFormat::Markdown),
        ] {
            assert_eq!(ExportFormat::from_str(input).unwrap(), *expected);
        }
        for bad in &["xml", "yaml", ""] {
            assert!(ExportFormat::from_str(bad).is_err(), "'{bad}' should fail");
        }
    }

    #[tokio::test]
    async fn export_json_round_trips() {
        let db = Database::test_db().await;
        seed_session(&db, "sess-json").await;
        let (out, filename) = export_session(&db, "sess-json", ExportOptions::default())
            .await
            .unwrap();

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

    #[tokio::test]
    async fn export_csv_has_header_and_rows() {
        let db = Database::test_db().await;
        seed_session(&db, "sess-csv").await;
        let opts = ExportOptions {
            format: ExportFormat::Csv,
            ..Default::default()
        };
        let (out, filename) = export_session(&db, "sess-csv", opts).await.unwrap();

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

    #[tokio::test]
    async fn export_csv_escapes_quotes_and_newlines() {
        let db = Database::test_db().await;
        db.upsert_session("sess-esc", 1, 2, 0, "m", "Escape Test")
            .await
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
        .await
        .unwrap();
        let opts = ExportOptions {
            format: ExportFormat::Csv,
            ..Default::default()
        };
        let (out, _) = export_session(&db, "sess-esc", opts).await.unwrap();
        // Doubled quote + wrapped in quotes because of `,` / `"` / `\n`.
        assert!(out.contains("\"she said \"\"hi\"\"\nand left\""));
    }

    #[tokio::test]
    async fn export_txt_is_human_readable() {
        let db = Database::test_db().await;
        seed_session(&db, "sess-txt").await;
        let opts = ExportOptions {
            format: ExportFormat::Txt,
            ..Default::default()
        };
        let (out, filename) = export_session(&db, "sess-txt", opts).await.unwrap();

        assert!(filename.ends_with(".txt"));
        assert!(out.contains("# Demo chat"));
        assert!(out.contains("[USER]"));
        assert!(out.contains("[ASSISTANT]"));
        assert!(out.contains("[TOOL]"));
        assert!(out.contains("hello, agent"));
        assert!(out.contains("-> tool_call tc1 run_shell"));
    }

    #[tokio::test]
    async fn export_missing_session_errors() {
        let db = Database::test_db().await;
        let err = export_session(&db, "does-not-exist", ExportOptions::default())
            .await
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

    #[tokio::test]
    async fn csv_defuses_formula_prefixes() {
        let db = Database::test_db().await;
        db.upsert_session("sess-inj", 1, 2, 0, "m", "Inj")
            .await
            .unwrap();
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
            .await
            .unwrap();
        }
        let (out, _) = export_session(
            &db,
            "sess-inj",
            ExportOptions {
                format: ExportFormat::Csv,
                ..Default::default()
            },
        )
        .await
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

    // ── Markdown format ──

    #[tokio::test]
    async fn export_markdown_renders_transcript_structure() {
        let db = Database::test_db().await;
        seed_session(&db, "sess-md").await;
        // Add an assistant message containing a fenced code block — it must
        // survive verbatim in the Markdown output.
        db.insert_message(
            "sess-md",
            "assistant",
            Some("here you go:\n```rust\nfn main() {}\n```\ndone"),
            None,
            None,
            Some("2024-01-01T00:00:03Z"),
            None,
        )
        .await
        .unwrap();
        let opts = ExportOptions {
            format: ExportFormat::Markdown,
            ..Default::default()
        };
        let (out, filename) = export_session(&db, "sess-md", opts).await.unwrap();

        assert!(filename.ends_with(".md"));
        // Title + role-headed sections.
        assert!(out.starts_with("# Demo chat\n"));
        assert!(out.contains("## User — 2024-01-01T00:00:00Z"));
        assert!(out.contains("## Assistant — 2024-01-01T00:00:01Z"));
        assert!(out.contains("## Tool — 2024-01-01T00:00:02Z"));
        assert!(out.contains("hello, agent"));
        // Authored code fence preserved verbatim.
        assert!(out.contains("```rust\nfn main() {}\n```"));
        // Tool call summarized compactly: name + args.
        assert!(out.contains("**Tool call:** `run_shell`"));
        assert!(out.contains(r#"{\"cmd\":\"ls\"}"#) || out.contains(r#"{"cmd":"ls"}"#));
        // Tool result fenced.
        assert!(out.contains("file1.txt\nfile2.txt"));
    }

    #[test]
    fn markdown_truncates_long_tool_output_and_args() {
        use crate::types::{FunctionCall, Message, ToolCall};
        let long_output = "x".repeat(MD_TOOL_OUTPUT_MAX_CHARS + 500);
        let long_args = format!(
            "{{\"data\":\"{}\"}}",
            "y".repeat(MD_TOOL_ARGS_MAX_CHARS + 300)
        );
        let session = Session {
            meta: SessionMeta {
                id: "s".into(),
                title: "T".into(),
                created_at: "x".into(),
                updated_at: "x".into(),
                message_count: 2,
            },
            messages: vec![
                Message {
                    role: Role::Assistant,
                    content: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "tc1".into(),
                        call_type: "function".into(),
                        function: FunctionCall {
                            name: "web_fetch".into(),
                            arguments: long_args,
                        },
                    }]),
                    tool_call_id: None,
                    timestamp: None,
                    seq: None,
                },
                Message {
                    role: Role::Tool,
                    content: Some(MessageContent::Text(long_output)),
                    tool_calls: None,
                    tool_call_id: Some("tc1".into()),
                    timestamp: None,
                    seq: None,
                },
            ],
        };
        let out = render_markdown(&session);
        assert!(
            out.contains("chars truncated]"),
            "truncation marker missing"
        );
        // Full payloads must not be dumped.
        assert!(!out.contains(&"x".repeat(MD_TOOL_OUTPUT_MAX_CHARS + 500)));
        assert!(!out.contains(&"y".repeat(MD_TOOL_ARGS_MAX_CHARS + 300)));
    }

    #[test]
    fn markdown_tool_output_fence_survives_embedded_backticks() {
        // Tool output containing ``` must not close the summary fence early:
        // the fence is sized one backtick past the longest run inside.
        let session = Session {
            meta: SessionMeta {
                id: "s".into(),
                title: "T".into(),
                created_at: "x".into(),
                updated_at: "x".into(),
                message_count: 1,
            },
            messages: vec![crate::types::Message {
                role: Role::Tool,
                content: Some(MessageContent::Text(
                    "before\n```\ninner fence\n```\nafter".into(),
                )),
                tool_calls: None,
                tool_call_id: Some("tc1".into()),
                timestamp: None,
                seq: None,
            }],
        };
        let out = render_markdown(&session);
        assert!(
            out.contains("````\nbefore\n```\ninner fence\n```\nafter\n````"),
            "expected 4-backtick fence around output containing ```: {out}"
        );
    }

    // ── Redaction (fail-closed, on by default) ──

    /// 48 alnum chars after `sk-` — matches the generic "API Key" secret
    /// pattern. Fixture value, not a real credential.
    const FAKE_SK_KEY: &str = "sk-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    async fn seed_secret_session(db: &Database, id: &str) {
        db.upsert_session(id, 1, 2, 0, "m", "Secret chat")
            .await
            .unwrap();
        db.insert_message(
            id,
            "assistant",
            Some(&format!("your key is {FAKE_SK_KEY} — keep it safe")),
            None,
            None,
            Some("2024-01-01T00:00:00Z"),
            None,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn redaction_is_on_by_default_for_every_format() {
        let db = Database::test_db().await;
        seed_secret_session(&db, "sess-red").await;
        for format in [
            ExportFormat::Json,
            ExportFormat::Csv,
            ExportFormat::Txt,
            ExportFormat::Markdown,
        ] {
            let (out, _) = export_session(
                &db,
                "sess-red",
                ExportOptions {
                    format,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            assert!(
                !out.contains(FAKE_SK_KEY),
                "raw key leaked in {format:?} export"
            );
            assert!(
                out.contains("[REDACTED"),
                "redaction marker missing in {format:?} export"
            );
        }
    }

    #[tokio::test]
    async fn no_redact_opt_out_emits_raw_output() {
        let db = Database::test_db().await;
        seed_secret_session(&db, "sess-raw").await;
        let (out, _) = export_session(
            &db,
            "sess-raw",
            ExportOptions {
                format: ExportFormat::Txt,
                redact: false,
            },
        )
        .await
        .unwrap();
        assert!(out.contains(FAKE_SK_KEY), "opt-out must emit raw output");
        assert!(!out.contains("[REDACTED"));
    }

    /// Fail-closed contract: if the redaction step errors, the export aborts
    /// — no unredacted text escapes. The redactor is injectable precisely so
    /// this path is testable without breaking the real pattern engine.
    #[test]
    fn redaction_failure_aborts_export_fail_closed() {
        let rendered = format!("leaky {FAKE_SK_KEY}");
        let err = apply_export_redaction(rendered, |_| Err(anyhow!("regex engine exploded")))
            .expect_err("failing redactor must abort the export");
        let msg = format!("{err:#}");
        assert!(msg.contains("aborting export"), "got: {msg}");
        assert!(msg.contains("regex engine exploded"), "got: {msg}");
    }

    #[test]
    fn default_redactor_strips_secrets_and_reports_ok() {
        let out = default_export_redactor(&format!("k={FAKE_SK_KEY}")).unwrap();
        assert!(!out.contains(FAKE_SK_KEY));
        assert!(out.contains("[REDACTED"));
    }
}
