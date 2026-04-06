use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde_json::{json, Value};

use super::http::{send_and_check, send_json};
use super::{format_list, require_str, validate_resource_id};
use crate::config::Config;
use crate::types::ToolDefinition;

const API_BASE: &str = "https://gmail.googleapis.com/gmail/v1/users/me";

pub fn tool_definition() -> ToolDefinition {
    ToolDefinition::new(
        "gmail",
        "Send, search, read, reply, forward, draft, label, archive, and manage emails via Gmail API.",
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["send", "search", "read", "reply", "forward",
                             "create_draft", "list_drafts", "send_draft", "read_draft",
                             "list_labels", "modify_labels", "trash", "untrash",
                             "get_thread", "mark_read", "mark_unread", "archive",
                             "batch_modify", "get_attachment"],
                    "description": "Action to perform"
                },
                "to": { "type": "string", "description": "Recipient email (for send, forward)" },
                "cc": { "type": "string", "description": "CC recipients, comma-separated (for send, create_draft)" },
                "bcc": { "type": "string", "description": "BCC recipients, comma-separated (for send, create_draft)" },
                "subject": { "type": "string", "description": "Email subject (for send, create_draft)" },
                "body": { "type": "string", "description": "Email body as plain text (for send, reply, forward, create_draft)" },
                "body_html": { "type": "string", "description": "Email body as HTML (overrides body for send, reply, create_draft)" },
                "query": { "type": "string", "description": "Search query using Gmail search operators (for search)" },
                "message_id": { "type": "string", "description": "Message ID (for read, reply, forward, modify_labels, trash, untrash, mark_read, mark_unread, archive, get_attachment)" },
                "reply_to_message_id": { "type": "string", "description": "Message ID to reply to (for send, threads the reply)" },
                "draft_id": { "type": "string", "description": "Draft ID (for send_draft, read_draft)" },
                "thread_id": { "type": "string", "description": "Thread ID (for get_thread)" },
                "limit": { "type": "integer", "description": "Max results (for search/list_drafts, default 10)" },
                "page_token": { "type": "string", "description": "Pagination token (for search)" },
                "add_labels": { "type": "array", "items": { "type": "string" }, "description": "Label IDs to add (for modify_labels, batch_modify)" },
                "remove_labels": { "type": "array", "items": { "type": "string" }, "description": "Label IDs to remove (for modify_labels, batch_modify)" },
                "message_ids": { "type": "array", "items": { "type": "string" }, "description": "Message IDs (for batch_modify)" },
                "attachment_id": { "type": "string", "description": "Attachment ID (for get_attachment)" }
            },
            "required": ["action"]
        }),
    )
}

pub async fn handle(arguments: &Value, config: &Config) -> Result<String> {
    let (client, token, action) =
        super::resolve_credential_and_action(arguments, config, "GMAIL_API_KEY")?;

    match action {
        "send" => send_email(&client, &token, arguments).await,
        "search" => search_emails(&client, &token, arguments).await,
        "read" => read_email(&client, &token, arguments).await,
        "reply" => reply_email(&client, &token, arguments).await,
        "forward" => forward_email(&client, &token, arguments).await,
        "create_draft" => create_draft(&client, &token, arguments).await,
        "list_drafts" => list_drafts(&client, &token, arguments).await,
        "send_draft" => send_draft(&client, &token, arguments).await,
        "list_labels" => list_labels(&client, &token).await,
        "modify_labels" => modify_labels(&client, &token, arguments).await,
        "trash" => trash_email(&client, &token, arguments).await,
        "untrash" => untrash_email(&client, &token, arguments).await,
        "get_thread" => get_thread(&client, &token, arguments).await,
        "mark_read" => mark_read(&client, &token, arguments).await,
        "mark_unread" => mark_unread(&client, &token, arguments).await,
        "archive" => archive_email(&client, &token, arguments).await,
        "batch_modify" => batch_modify(&client, &token, arguments).await,
        "read_draft" => read_draft(&client, &token, arguments).await,
        "get_attachment" => get_attachment(&client, &token, arguments).await,
        _ => bail!("Unknown action: {action}"),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn base64_url_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

fn base64_url_decode(data: &str) -> Result<String> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(data)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(data))
        .context("base64 decode error")?;
    String::from_utf8(bytes).context("UTF-8 decode error")
}

fn extract_header(message: &Value, name: &str) -> Option<String> {
    message["payload"]["headers"]
        .as_array()?
        .iter()
        .find(|h| h["name"].as_str() == Some(name))
        .and_then(|h| h["value"].as_str())
        .map(String::from)
}

/// Format label IDs from a Gmail message metadata response.
fn format_label_ids(message: &Value) -> String {
    message["labelIds"]
        .as_array()
        .map(|labels| {
            labels
                .iter()
                .filter_map(|l| l.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

/// Truncate a snippet to `max_chars`, appending "..." if truncated.
fn truncate_snippet(snippet: &str, max_chars: usize) -> String {
    if snippet.chars().count() <= max_chars {
        snippet.to_string()
    } else {
        let boundary = snippet
            .char_indices()
            .nth(max_chars)
            .map(|(i, _)| i)
            .unwrap_or(snippet.len());
        format!("{}...", &snippet[..boundary])
    }
}

/// Recursively extract the text body from a Gmail message payload.
/// Prefers text/plain over text/html. Handles single-part and multipart messages.
/// Depth-limited to 10 levels to guard against malicious nesting.
fn extract_body_text(payload: &Value) -> String {
    extract_body_text_inner(payload, 10)
}

fn extract_body_text_inner(payload: &Value, depth: u8) -> String {
    if depth == 0 {
        return String::new();
    }

    // Single-part message: body directly on payload
    if let Some(mime) = payload["mimeType"].as_str() {
        if mime == "text/plain" {
            if let Some(data) = payload["body"]["data"].as_str() {
                if let Ok(text) = base64_url_decode(data) {
                    return text;
                }
            }
        }
    }

    // Multipart: recurse into parts
    if let Some(parts) = payload["parts"].as_array() {
        // First pass: look for text/plain at this level
        for part in parts {
            if part["mimeType"].as_str() == Some("text/plain") {
                if let Some(data) = part["body"]["data"].as_str() {
                    if let Ok(text) = base64_url_decode(data) {
                        return text;
                    }
                }
            }
        }
        // Second pass: recurse into nested multipart parts (finds text/plain
        // inside multipart/alternative, then falls back to text/html)
        for part in parts {
            let nested = extract_body_text_inner(part, depth - 1);
            if !nested.is_empty() {
                return nested;
            }
        }
    }

    // Fallback: text/html on the payload itself (single-part HTML message)
    if let Some(mime) = payload["mimeType"].as_str() {
        if mime == "text/html" {
            if let Some(data) = payload["body"]["data"].as_str() {
                if let Ok(text) = base64_url_decode(data) {
                    return text;
                }
            }
        }
    }

    String::new()
}

/// Build an RFC2822 raw message and return it base64url-encoded.
/// Validates that no header value or content_type contains \r or \n (header injection prevention).
fn build_raw_message(headers: &[(&str, &str)], body: &str, content_type: &str) -> Result<String> {
    if content_type.contains('\r') || content_type.contains('\n') {
        bail!("Invalid content type: contains newline characters");
    }
    let mut raw = String::new();
    raw.push_str("MIME-Version: 1.0\r\n");
    for (name, value) in headers {
        if name.contains('\r')
            || name.contains('\n')
            || value.contains('\r')
            || value.contains('\n')
        {
            bail!("Invalid '{name}': contains newline characters");
        }
        raw.push_str(&format!("{name}: {value}\r\n"));
    }
    raw.push_str(&format!("Content-Type: {content_type}; charset=utf-8\r\n"));
    raw.push_str("\r\n");
    raw.push_str(body);
    Ok(base64_url_encode(raw.as_bytes()))
}

/// Fetch a message with full format (headers + body) for reply/forward.
async fn fetch_message_full(client: &Client, token: &str, message_id: &str) -> Result<Value> {
    let message_id = validate_resource_id(message_id, "message_id")?;
    send_json(
        client
            .get(format!("{API_BASE}/messages/{message_id}"))
            .bearer_auth(token)
            .query(&[("format", "full")]),
        "Gmail",
    )
    .await
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

async fn send_email(client: &Client, token: &str, args: &Value) -> Result<String> {
    let to = require_str(args, "to")?;
    let subject = args["subject"].as_str().unwrap_or("(no subject)");
    let body_text = args["body"].as_str().unwrap_or("");
    let cc = args["cc"].as_str();
    let bcc = args["bcc"].as_str();
    let body_html = args["body_html"].as_str();
    let reply_to_id = args["reply_to_message_id"].as_str();

    let (content_type, body) = if let Some(html) = body_html {
        ("text/html", html)
    } else {
        ("text/plain", body_text)
    };

    let mut hdrs: Vec<(&str, &str)> = vec![("To", to), ("Subject", subject)];
    if let Some(cc_val) = cc {
        hdrs.push(("Cc", cc_val));
    }
    if let Some(bcc_val) = bcc {
        hdrs.push(("Bcc", bcc_val));
    }

    // Threading support: reply to an existing message
    let mut thread_id_owned = String::new();
    let mut in_reply_to_owned = String::new();
    let mut references_owned = String::new();

    if let Some(orig_id) = reply_to_id {
        let orig = fetch_message_full(client, token, orig_id).await?;
        if let Some(tid) = orig["threadId"].as_str() {
            thread_id_owned = tid.to_string();
        }
        if let Some(msg_id_header) = extract_header(&orig, "Message-ID") {
            in_reply_to_owned.clone_from(&msg_id_header);
            references_owned = if let Some(existing_refs) = extract_header(&orig, "References") {
                format!("{existing_refs} {msg_id_header}")
            } else {
                msg_id_header
            };
        }
    }

    if !in_reply_to_owned.is_empty() {
        hdrs.push(("In-Reply-To", &in_reply_to_owned));
        hdrs.push(("References", &references_owned));
    }

    let encoded = build_raw_message(&hdrs, body, content_type)?;

    let mut payload = json!({ "raw": encoded });
    if !thread_id_owned.is_empty() {
        payload["threadId"] = json!(thread_id_owned);
    }

    let result: Value = send_json(
        client
            .post(format!("{API_BASE}/messages/send"))
            .bearer_auth(token)
            .json(&payload),
        "Gmail",
    )
    .await?;

    Ok(format!(
        "Email sent (id: {})",
        result["id"].as_str().unwrap_or("unknown")
    ))
}

async fn search_emails(client: &Client, token: &str, args: &Value) -> Result<String> {
    let query = require_str(args, "query")?;
    let limit = args["limit"].as_u64().unwrap_or(10).min(50);
    let page_token = args["page_token"].as_str();

    let mut params: Vec<(&str, String)> =
        vec![("q", query.to_string()), ("maxResults", limit.to_string())];
    if let Some(pt) = page_token {
        params.push(("pageToken", pt.to_string()));
    }

    let result: Value = send_json(
        client
            .get(format!("{API_BASE}/messages"))
            .bearer_auth(token)
            .query(&params),
        "Gmail",
    )
    .await?;

    let messages = result["messages"].as_array();
    let next_page = result["nextPageToken"].as_str();

    match messages {
        Some(msgs) if !msgs.is_empty() => {
            let mut summaries = Vec::new();
            for msg in msgs {
                if let Some(id) = msg["id"].as_str() {
                    let id = match validate_resource_id(id, "message_id") {
                        Ok(id) => id,
                        Err(_) => continue,
                    };
                    if let Ok(meta) = send_json(
                        client
                            .get(format!("{API_BASE}/messages/{id}"))
                            .bearer_auth(token)
                            .query(&[
                                ("format", "metadata"),
                                ("metadataHeaders", "Subject"),
                                ("metadataHeaders", "From"),
                                ("metadataHeaders", "Date"),
                            ]),
                        "Gmail",
                    )
                    .await
                    {
                        let subject = extract_header(&meta, "Subject")
                            .unwrap_or_else(|| "(no subject)".to_string());
                        let from =
                            extract_header(&meta, "From").unwrap_or_else(|| "unknown".to_string());
                        let date = extract_header(&meta, "Date").unwrap_or_default();
                        let labels = format_label_ids(&meta);
                        let snippet = truncate_snippet(meta["snippet"].as_str().unwrap_or(""), 100);
                        let mut entry =
                            format!("- [{id}] {subject}\n  From: {from} | Date: {date}");
                        if !labels.is_empty() && !snippet.is_empty() {
                            entry.push_str(&format!("\n  Labels: {labels} | {snippet}"));
                        } else if !labels.is_empty() {
                            entry.push_str(&format!("\n  Labels: {labels}"));
                        } else if !snippet.is_empty() {
                            entry.push_str(&format!("\n  {snippet}"));
                        }
                        summaries.push(entry);
                    } else {
                        summaries.push(format!("- [{id}] (metadata unavailable)"));
                    }
                }
            }
            let mut output = format!(
                "Found {} message(s):\n{}",
                summaries.len(),
                summaries.join("\n")
            );
            if let Some(npt) = next_page {
                output.push_str(&format!("\n\nNext page token: {npt}"));
            }
            Ok(output)
        }
        _ => Ok("No messages found.".to_string()),
    }
}

async fn read_email(client: &Client, token: &str, args: &Value) -> Result<String> {
    let message_id = require_str(args, "message_id")?;
    let result = fetch_message_full(client, token, message_id).await?;

    let from = extract_header(&result, "From").unwrap_or_else(|| "unknown".to_string());
    let to = extract_header(&result, "To").unwrap_or_default();
    let cc = extract_header(&result, "Cc");
    let date = extract_header(&result, "Date").unwrap_or_default();
    let subject = extract_header(&result, "Subject").unwrap_or_else(|| "(no subject)".to_string());
    let thread_id = result["threadId"].as_str().unwrap_or("");

    let body = extract_body_text(&result["payload"]);
    let body_display = if body.is_empty() {
        result["snippet"].as_str().unwrap_or("").to_string()
    } else {
        body
    };

    let mut output = format!("From: {from}\nTo: {to}\n");
    if let Some(cc_val) = cc {
        output.push_str(&format!("Cc: {cc_val}\n"));
    }
    output.push_str(&format!(
        "Date: {date}\nSubject: {subject}\nThread: {thread_id}\n\n{body_display}"
    ));
    Ok(output)
}

async fn reply_email(client: &Client, token: &str, args: &Value) -> Result<String> {
    let message_id = require_str(args, "message_id")?;
    let body_text = args["body"].as_str().unwrap_or("");
    let body_html = args["body_html"].as_str();

    let orig = fetch_message_full(client, token, message_id).await?;

    let orig_from = extract_header(&orig, "From").unwrap_or_else(|| "unknown".to_string());
    let orig_subject = extract_header(&orig, "Subject").unwrap_or_default();
    let thread_id = orig["threadId"].as_str().unwrap_or("");

    let reply_subject = if orig_subject
        .get(..4)
        .is_some_and(|s| s.eq_ignore_ascii_case("re: "))
    {
        orig_subject
    } else {
        format!("Re: {orig_subject}")
    };

    let (content_type, body) = if let Some(html) = body_html {
        ("text/html", html.to_string())
    } else {
        ("text/plain", body_text.to_string())
    };

    let mut hdrs: Vec<(&str, &str)> = vec![("To", &orig_from), ("Subject", &reply_subject)];

    let in_reply_to = extract_header(&orig, "Message-ID").unwrap_or_default();
    let references = if let Some(existing_refs) = extract_header(&orig, "References") {
        format!("{existing_refs} {in_reply_to}")
    } else {
        in_reply_to.clone()
    };

    if !in_reply_to.is_empty() {
        hdrs.push(("In-Reply-To", &in_reply_to));
        hdrs.push(("References", &references));
    }

    let encoded = build_raw_message(&hdrs, &body, content_type)?;

    let mut payload = json!({ "raw": encoded });
    if !thread_id.is_empty() {
        payload["threadId"] = json!(thread_id);
    }

    let result: Value = send_json(
        client
            .post(format!("{API_BASE}/messages/send"))
            .bearer_auth(token)
            .json(&payload),
        "Gmail",
    )
    .await?;

    Ok(format!(
        "Reply sent (id: {})",
        result["id"].as_str().unwrap_or("unknown")
    ))
}

async fn forward_email(client: &Client, token: &str, args: &Value) -> Result<String> {
    let message_id = require_str(args, "message_id")?;
    let to = require_str(args, "to")?;
    let additional_body = args["body"].as_str().unwrap_or("");

    let orig = fetch_message_full(client, token, message_id).await?;

    let orig_from = extract_header(&orig, "From").unwrap_or_else(|| "unknown".to_string());
    let orig_to = extract_header(&orig, "To").unwrap_or_default();
    let orig_date = extract_header(&orig, "Date").unwrap_or_default();
    let orig_subject =
        extract_header(&orig, "Subject").unwrap_or_else(|| "(no subject)".to_string());
    let orig_body = extract_body_text(&orig["payload"]);

    let fwd_subject = if orig_subject
        .get(..5)
        .is_some_and(|s| s.eq_ignore_ascii_case("fwd: "))
    {
        orig_subject.clone()
    } else {
        format!("Fwd: {orig_subject}")
    };

    let mut body = String::new();
    if !additional_body.is_empty() {
        body.push_str(additional_body);
        body.push_str("\n\n");
    }
    body.push_str("---------- Forwarded message ----------\n");
    body.push_str(&format!("From: {orig_from}\n"));
    body.push_str(&format!("Date: {orig_date}\n"));
    body.push_str(&format!("Subject: {orig_subject}\n"));
    body.push_str(&format!("To: {orig_to}\n\n"));
    body.push_str(&orig_body);

    let hdrs: Vec<(&str, &str)> = vec![("To", to), ("Subject", &fwd_subject)];
    let encoded = build_raw_message(&hdrs, &body, "text/plain")?;

    let result: Value = send_json(
        client
            .post(format!("{API_BASE}/messages/send"))
            .bearer_auth(token)
            .json(&json!({ "raw": encoded })),
        "Gmail",
    )
    .await?;

    Ok(format!(
        "Forwarded (id: {})",
        result["id"].as_str().unwrap_or("unknown")
    ))
}

async fn create_draft(client: &Client, token: &str, args: &Value) -> Result<String> {
    let to = require_str(args, "to")?;
    let subject = args["subject"].as_str().unwrap_or("(no subject)");
    let body_text = args["body"].as_str().unwrap_or("");
    let cc = args["cc"].as_str();
    let bcc = args["bcc"].as_str();
    let body_html = args["body_html"].as_str();

    let (content_type, body) = if let Some(html) = body_html {
        ("text/html", html)
    } else {
        ("text/plain", body_text)
    };

    let mut hdrs: Vec<(&str, &str)> = vec![("To", to), ("Subject", subject)];
    if let Some(cc_val) = cc {
        hdrs.push(("Cc", cc_val));
    }
    if let Some(bcc_val) = bcc {
        hdrs.push(("Bcc", bcc_val));
    }

    let encoded = build_raw_message(&hdrs, body, content_type)?;

    let result: Value = send_json(
        client
            .post(format!("{API_BASE}/drafts"))
            .bearer_auth(token)
            .json(&json!({ "message": { "raw": encoded } })),
        "Gmail",
    )
    .await?;

    Ok(format!(
        "Draft created (id: {})",
        result["id"].as_str().unwrap_or("unknown")
    ))
}

async fn list_drafts(client: &Client, token: &str, args: &Value) -> Result<String> {
    let limit = args["limit"].as_u64().unwrap_or(10).min(50);

    let result: Value = send_json(
        client
            .get(format!("{API_BASE}/drafts"))
            .bearer_auth(token)
            .query(&[("maxResults", &limit.to_string())]),
        "Gmail",
    )
    .await?;

    Ok(format_list(
        result["drafts"].as_array().into_iter().flatten(),
        |n| format!("Found {n} draft(s):"),
        "No drafts found.",
        |d| {
            let id = d["id"].as_str().unwrap_or("?");
            let msg_id = d["message"]["id"].as_str().unwrap_or("");
            format!("- {id} (message: {msg_id})")
        },
    ))
}

async fn send_draft(client: &Client, token: &str, args: &Value) -> Result<String> {
    let draft_id = require_str(args, "draft_id")?;
    let draft_id = validate_resource_id(draft_id, "draft_id")?;

    let result: Value = send_json(
        client
            .post(format!("{API_BASE}/drafts/send"))
            .bearer_auth(token)
            .json(&json!({ "id": draft_id })),
        "Gmail",
    )
    .await?;

    Ok(format!(
        "Draft sent (message id: {})",
        result["id"].as_str().unwrap_or("unknown")
    ))
}

async fn list_labels(client: &Client, token: &str) -> Result<String> {
    let result: Value = send_json(
        client.get(format!("{API_BASE}/labels")).bearer_auth(token),
        "Gmail",
    )
    .await?;

    Ok(format_list(
        result["labels"].as_array().into_iter().flatten(),
        |n| format!("Found {n} label(s):"),
        "No labels found.",
        |l| {
            let name = l["name"].as_str().unwrap_or("?");
            let id = l["id"].as_str().unwrap_or("?");
            let ltype = l["type"].as_str().unwrap_or("user");
            format!("- {name} (id: {id}, type: {ltype})")
        },
    ))
}

async fn modify_labels(client: &Client, token: &str, args: &Value) -> Result<String> {
    let message_id = require_str(args, "message_id")?;
    let message_id = validate_resource_id(message_id, "message_id")?;

    let add: Vec<&str> = args["add_labels"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let remove: Vec<&str> = args["remove_labels"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    if add.is_empty() && remove.is_empty() {
        bail!("At least one of 'add_labels' or 'remove_labels' must be provided");
    }

    send_json(
        client
            .post(format!("{API_BASE}/messages/{message_id}/modify"))
            .bearer_auth(token)
            .json(&json!({
                "addLabelIds": add,
                "removeLabelIds": remove
            })),
        "Gmail",
    )
    .await?;

    let mut parts = Vec::new();
    if !add.is_empty() {
        parts.push(format!("added: {}", add.join(", ")));
    }
    if !remove.is_empty() {
        parts.push(format!("removed: {}", remove.join(", ")));
    }
    Ok(format!(
        "Labels updated for {message_id} ({})",
        parts.join("; ")
    ))
}

async fn trash_email(client: &Client, token: &str, args: &Value) -> Result<String> {
    let message_id = require_str(args, "message_id")?;
    let message_id = validate_resource_id(message_id, "message_id")?;

    send_and_check(
        client
            .post(format!("{API_BASE}/messages/{message_id}/trash"))
            .bearer_auth(token),
        "Gmail",
    )
    .await?;

    Ok(format!("Message {message_id} moved to trash."))
}

async fn get_thread(client: &Client, token: &str, args: &Value) -> Result<String> {
    let thread_id = require_str(args, "thread_id")?;
    let thread_id = validate_resource_id(thread_id, "thread_id")?;

    let result: Value = send_json(
        client
            .get(format!("{API_BASE}/threads/{thread_id}"))
            .bearer_auth(token)
            .query(&[
                ("format", "metadata"),
                ("metadataHeaders", "Subject"),
                ("metadataHeaders", "From"),
                ("metadataHeaders", "Date"),
            ]),
        "Gmail",
    )
    .await?;

    Ok(format_list(
        result["messages"].as_array().into_iter().flatten(),
        |n| format!("Thread {thread_id} ({n} message(s)):"),
        "No messages in thread.",
        |m| {
            let id = m["id"].as_str().unwrap_or("?");
            let subject =
                extract_header(m, "Subject").unwrap_or_else(|| "(no subject)".to_string());
            let from = extract_header(m, "From").unwrap_or_else(|| "unknown".to_string());
            let date = extract_header(m, "Date").unwrap_or_default();
            let snippet = m["snippet"].as_str().unwrap_or("");
            format!("- [{id}] {subject} (from: {from}, {date})\n  {snippet}")
        },
    ))
}

// ---------------------------------------------------------------------------
// Shortcut actions
// ---------------------------------------------------------------------------

/// Helper for single-message label modification shortcuts.
async fn modify_single_label(
    client: &Client,
    token: &str,
    args: &Value,
    add: &[&str],
    remove: &[&str],
    success_msg: &str,
) -> Result<String> {
    let message_id = require_str(args, "message_id")?;
    let message_id = validate_resource_id(message_id, "message_id")?;

    send_json(
        client
            .post(format!("{API_BASE}/messages/{message_id}/modify"))
            .bearer_auth(token)
            .json(&json!({
                "addLabelIds": add,
                "removeLabelIds": remove
            })),
        "Gmail",
    )
    .await
    .map(|_: Value| format!("{success_msg} {message_id}"))
}

async fn mark_read(client: &Client, token: &str, args: &Value) -> Result<String> {
    modify_single_label(
        client,
        token,
        args,
        &[],
        &["UNREAD"],
        "Message marked as read:",
    )
    .await
}

async fn mark_unread(client: &Client, token: &str, args: &Value) -> Result<String> {
    modify_single_label(
        client,
        token,
        args,
        &["UNREAD"],
        &[],
        "Message marked as unread:",
    )
    .await
}

async fn archive_email(client: &Client, token: &str, args: &Value) -> Result<String> {
    modify_single_label(client, token, args, &[], &["INBOX"], "Message archived:").await
}

async fn untrash_email(client: &Client, token: &str, args: &Value) -> Result<String> {
    let message_id = require_str(args, "message_id")?;
    let message_id = validate_resource_id(message_id, "message_id")?;

    send_and_check(
        client
            .post(format!("{API_BASE}/messages/{message_id}/untrash"))
            .bearer_auth(token),
        "Gmail",
    )
    .await?;

    Ok(format!("Message {message_id} restored from trash."))
}

// ---------------------------------------------------------------------------
// Batch operations
// ---------------------------------------------------------------------------

async fn batch_modify(client: &Client, token: &str, args: &Value) -> Result<String> {
    let ids: Vec<&str> = args["message_ids"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Missing 'message_ids' array"))?
        .iter()
        .filter_map(|v| v.as_str())
        .collect();

    if ids.is_empty() {
        bail!("At least one message ID is required in 'message_ids'");
    }
    if ids.len() > 1000 {
        bail!("Too many message IDs (max 1000)");
    }

    for id in &ids {
        validate_resource_id(id, "message_id")?;
    }

    let add: Vec<&str> = args["add_labels"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let remove: Vec<&str> = args["remove_labels"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    if add.is_empty() && remove.is_empty() {
        bail!("At least one of 'add_labels' or 'remove_labels' must be provided");
    }

    send_and_check(
        client
            .post(format!("{API_BASE}/messages/batchModify"))
            .bearer_auth(token)
            .json(&json!({
                "ids": ids,
                "addLabelIds": add,
                "removeLabelIds": remove
            })),
        "Gmail",
    )
    .await?;

    let mut parts = Vec::new();
    if !add.is_empty() {
        parts.push(format!("added: {}", add.join(", ")));
    }
    if !remove.is_empty() {
        parts.push(format!("removed: {}", remove.join(", ")));
    }
    Ok(format!(
        "Batch label update applied to {} message(s) ({})",
        ids.len(),
        parts.join("; ")
    ))
}

// ---------------------------------------------------------------------------
// Draft reading & attachments
// ---------------------------------------------------------------------------

async fn read_draft(client: &Client, token: &str, args: &Value) -> Result<String> {
    let draft_id = require_str(args, "draft_id")?;
    let draft_id = validate_resource_id(draft_id, "draft_id")?;

    let result: Value = send_json(
        client
            .get(format!("{API_BASE}/drafts/{draft_id}"))
            .bearer_auth(token)
            .query(&[("format", "full")]),
        "Gmail",
    )
    .await?;

    let message = &result["message"];
    let from = extract_header(message, "From").unwrap_or_else(|| "unknown".to_string());
    let to = extract_header(message, "To").unwrap_or_default();
    let cc = extract_header(message, "Cc");
    let date = extract_header(message, "Date").unwrap_or_default();
    let subject = extract_header(message, "Subject").unwrap_or_else(|| "(no subject)".to_string());

    let body = extract_body_text(&message["payload"]);
    let body_display = if body.is_empty() {
        message["snippet"].as_str().unwrap_or("").to_string()
    } else {
        body
    };

    let mut output = format!("Draft ID: {draft_id}\nFrom: {from}\nTo: {to}\n");
    if let Some(cc_val) = cc {
        output.push_str(&format!("Cc: {cc_val}\n"));
    }
    output.push_str(&format!(
        "Date: {date}\nSubject: {subject}\n\n{body_display}"
    ));
    Ok(output)
}

async fn get_attachment(client: &Client, token: &str, args: &Value) -> Result<String> {
    let message_id = require_str(args, "message_id")?;
    let message_id = validate_resource_id(message_id, "message_id")?;
    let attachment_id = require_str(args, "attachment_id")?;
    let attachment_id = validate_resource_id(attachment_id, "attachment_id")?;

    let result: Value = send_json(
        client
            .get(format!(
                "{API_BASE}/messages/{message_id}/attachments/{attachment_id}"
            ))
            .bearer_auth(token),
        "Gmail",
    )
    .await?;

    let size = result["size"].as_u64().unwrap_or(0);
    let data = result["data"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing attachment data"))?;

    // Guard against large attachments flooding the LLM context window
    const MAX_DISPLAY_BYTES: u64 = 256 * 1024; // 256 KB
    if size > MAX_DISPLAY_BYTES {
        return Ok(format!(
            "Attachment too large to display ({size} bytes). Use the attachment ID for direct download.",
        ));
    }

    Ok(format!("Attachment ({size} bytes):\n{data}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_definition() {
        let def = tool_definition();
        assert_eq!(def.function.name, "gmail");
        assert!(!def.function.description.is_empty());
        let props = &def.function.parameters["properties"];
        assert!(props["action"].is_object());
        let actions = props["action"]["enum"].as_array().unwrap();
        assert_eq!(actions.len(), 19);
        let action_strs: Vec<&str> = actions.iter().filter_map(|v| v.as_str()).collect();
        for expected in &[
            "send",
            "search",
            "read",
            "reply",
            "forward",
            "create_draft",
            "list_drafts",
            "send_draft",
            "read_draft",
            "list_labels",
            "modify_labels",
            "trash",
            "untrash",
            "get_thread",
            "mark_read",
            "mark_unread",
            "archive",
            "batch_modify",
            "get_attachment",
        ] {
            assert!(action_strs.contains(expected), "missing action: {expected}");
        }
        assert!(props["cc"].is_object());
        assert!(props["bcc"].is_object());
        assert!(props["body_html"].is_object());
        assert!(props["reply_to_message_id"].is_object());
        assert!(props["draft_id"].is_object());
        assert!(props["thread_id"].is_object());
        assert!(props["add_labels"].is_object());
        assert!(props["remove_labels"].is_object());
        assert!(props["page_token"].is_object());
        assert!(props["message_ids"].is_object());
        assert!(props["attachment_id"].is_object());
    }

    #[test]
    fn test_base64_url_encode() {
        let encoded = base64_url_encode(b"To: test@example.com\r\nSubject: Hi\r\n\r\nBody");
        assert!(!encoded.contains('+'));
        assert!(!encoded.contains('/'));
        assert!(!encoded.contains('='));
    }

    #[test]
    fn test_base64_url_decode() {
        let original = "Hello, Gmail!";
        let encoded = base64_url_encode(original.as_bytes());
        let decoded = base64_url_decode(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn test_base64_url_decode_with_padding() {
        use base64::Engine;
        let data = "Test message with padding";
        let with_padding = base64::engine::general_purpose::URL_SAFE.encode(data);
        let decoded = base64_url_decode(&with_padding).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_extract_header() {
        let msg = json!({
            "payload": {
                "headers": [
                    { "name": "Subject", "value": "Test Subject" },
                    { "name": "From", "value": "alice@example.com" }
                ]
            }
        });
        assert_eq!(
            extract_header(&msg, "Subject"),
            Some("Test Subject".to_string())
        );
        assert_eq!(
            extract_header(&msg, "From"),
            Some("alice@example.com".to_string())
        );
        assert_eq!(extract_header(&msg, "Missing"), None);
    }

    #[test]
    fn test_extract_body_text_single_part() {
        let encoded = base64_url_encode(b"Hello plain text");
        let payload = json!({
            "mimeType": "text/plain",
            "body": { "data": encoded }
        });
        assert_eq!(extract_body_text(&payload), "Hello plain text");
    }

    #[test]
    fn test_extract_body_text_multipart() {
        let plain = base64_url_encode(b"Plain version");
        let html = base64_url_encode(b"<p>HTML version</p>");
        let payload = json!({
            "mimeType": "multipart/alternative",
            "parts": [
                { "mimeType": "text/plain", "body": { "data": plain } },
                { "mimeType": "text/html", "body": { "data": html } }
            ]
        });
        assert_eq!(extract_body_text(&payload), "Plain version");
    }

    #[test]
    fn test_extract_body_text_nested_multipart() {
        let plain = base64_url_encode(b"Nested plain");
        let payload = json!({
            "mimeType": "multipart/mixed",
            "parts": [
                {
                    "mimeType": "multipart/alternative",
                    "parts": [
                        { "mimeType": "text/plain", "body": { "data": plain } },
                        { "mimeType": "text/html", "body": { "data": base64_url_encode(b"<p>nested html</p>") } }
                    ]
                },
                {
                    "mimeType": "application/pdf",
                    "filename": "doc.pdf",
                    "body": { "attachmentId": "abc" }
                }
            ]
        });
        assert_eq!(extract_body_text(&payload), "Nested plain");
    }

    #[test]
    fn test_extract_body_text_html_fallback() {
        let html = base64_url_encode(b"<p>Only HTML</p>");
        let payload = json!({
            "mimeType": "text/html",
            "body": { "data": html }
        });
        assert_eq!(extract_body_text(&payload), "<p>Only HTML</p>");
    }

    #[test]
    fn test_extract_body_text_empty() {
        let payload = json!({
            "mimeType": "multipart/mixed",
            "parts": [
                { "mimeType": "application/pdf", "body": { "attachmentId": "xyz" } }
            ]
        });
        assert_eq!(extract_body_text(&payload), "");
    }

    #[test]
    fn test_build_raw_message() {
        let encoded = build_raw_message(
            &[("To", "bob@example.com"), ("Subject", "Hi")],
            "Hello",
            "text/plain",
        )
        .unwrap();
        let decoded = base64_url_decode(&encoded).unwrap();
        assert!(decoded.contains("To: bob@example.com\r\n"));
        assert!(decoded.contains("Subject: Hi\r\n"));
        assert!(decoded.contains("Content-Type: text/plain; charset=utf-8\r\n"));
        assert!(decoded.contains("\r\n\r\nHello"));
    }

    #[test]
    fn test_build_raw_message_with_cc_bcc() {
        let encoded = build_raw_message(
            &[
                ("To", "bob@example.com"),
                ("Cc", "carol@example.com"),
                ("Bcc", "dave@example.com"),
                ("Subject", "Test"),
            ],
            "Body",
            "text/plain",
        )
        .unwrap();
        let decoded = base64_url_decode(&encoded).unwrap();
        assert!(decoded.contains("Cc: carol@example.com\r\n"));
        assert!(decoded.contains("Bcc: dave@example.com\r\n"));
    }

    #[test]
    fn test_build_raw_message_html() {
        let encoded = build_raw_message(
            &[("To", "bob@example.com"), ("Subject", "Hi")],
            "<p>Hello</p>",
            "text/html",
        )
        .unwrap();
        let decoded = base64_url_decode(&encoded).unwrap();
        assert!(decoded.contains("Content-Type: text/html; charset=utf-8\r\n"));
        assert!(decoded.contains("<p>Hello</p>"));
    }

    #[test]
    fn test_build_raw_message_header_injection() {
        let result = build_raw_message(
            &[
                ("To", "bob@example.com\r\nBcc: evil@example.com"),
                ("Subject", "Hi"),
            ],
            "body",
            "text/plain",
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("newline"));

        let result = build_raw_message(
            &[
                ("To", "bob@example.com"),
                ("Subject", "Hi\nBcc: evil@example.com"),
            ],
            "body",
            "text/plain",
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_build_raw_message_includes_mime_version() {
        let encoded = build_raw_message(
            &[("To", "bob@example.com"), ("Subject", "Hi")],
            "Hello",
            "text/plain",
        )
        .unwrap();
        let decoded = base64_url_decode(&encoded).unwrap();
        assert!(decoded.starts_with("MIME-Version: 1.0\r\n"));
    }

    #[test]
    fn test_build_raw_message_content_type_injection() {
        let result = build_raw_message(
            &[("To", "bob@example.com")],
            "body",
            "text/plain\r\nX-Injected: true",
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("newline"));
    }

    #[test]
    fn test_extract_body_text_multipart_html_only() {
        let html = base64_url_encode(b"<p>HTML in multipart</p>");
        let payload = json!({
            "mimeType": "multipart/alternative",
            "parts": [
                { "mimeType": "text/html", "body": { "data": html } }
            ]
        });
        // Falls back to text/html via recursion when no text/plain found
        assert_eq!(extract_body_text(&payload), "<p>HTML in multipart</p>");
    }

    #[test]
    fn test_reply_subject_prefix_case_insensitive() {
        let check = |subject: &str| -> String {
            let s = subject.to_string();
            if s.get(..4).map_or(false, |p| p.eq_ignore_ascii_case("re: ")) {
                s
            } else {
                format!("Re: {s}")
            }
        };
        assert_eq!(check("Re: Hello"), "Re: Hello");
        assert_eq!(check("RE: Hello"), "RE: Hello");
        assert_eq!(check("re: Hello"), "re: Hello");
        assert_eq!(check("Hello"), "Re: Hello");
        assert_eq!(check(""), "Re: ");
        // Non-ASCII subject must not panic
        assert_eq!(check("日本語"), "Re: 日本語");
    }

    #[test]
    fn test_forward_subject_prefix_case_insensitive() {
        let check = |subject: &str| -> String {
            let s = subject.to_string();
            if s.get(..5)
                .map_or(false, |p| p.eq_ignore_ascii_case("fwd: "))
            {
                s
            } else {
                format!("Fwd: {s}")
            }
        };
        assert_eq!(check("Fwd: Hello"), "Fwd: Hello");
        assert_eq!(check("FWD: Hello"), "FWD: Hello");
        assert_eq!(check("fwd: Hello"), "fwd: Hello");
        assert_eq!(check("Hello"), "Fwd: Hello");
        // Non-ASCII subject must not panic
        assert_eq!(check("日本語"), "Fwd: 日本語");
    }

    #[test]
    fn test_extract_body_text_depth_limit() {
        // Build a deeply nested structure (11 levels) to verify depth limit
        fn nest(depth: u8) -> Value {
            if depth == 0 {
                let data = base64_url_encode(b"deep text");
                json!({ "mimeType": "text/plain", "body": { "data": data } })
            } else {
                json!({
                    "mimeType": "multipart/mixed",
                    "parts": [nest(depth - 1)]
                })
            }
        }
        // 9 levels of nesting (within limit of 10)
        assert_eq!(extract_body_text(&nest(9)), "deep text");
        // 15 levels of nesting (exceeds limit) — returns empty
        assert_eq!(extract_body_text(&nest(15)), "");
    }

    // -----------------------------------------------------------------------
    // New helper tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_label_ids() {
        let msg = json!({ "labelIds": ["INBOX", "UNREAD", "CATEGORY_PERSONAL"] });
        assert_eq!(format_label_ids(&msg), "INBOX, UNREAD, CATEGORY_PERSONAL");

        let msg_empty = json!({ "labelIds": [] });
        assert_eq!(format_label_ids(&msg_empty), "");

        let msg_missing = json!({});
        assert_eq!(format_label_ids(&msg_missing), "");
    }

    #[test]
    fn test_truncate_snippet() {
        assert_eq!(truncate_snippet("short", 100), "short");
        assert_eq!(truncate_snippet("", 100), "");

        let long = "a".repeat(200);
        let truncated = truncate_snippet(&long, 100);
        assert!(truncated.ends_with("..."));
        // 100 'a' chars + "..."
        assert_eq!(truncated.len(), 103);

        // Exact boundary
        let exact = "a".repeat(100);
        assert_eq!(truncate_snippet(&exact, 100), exact);

        // Unicode safety
        let unicode = "日本語のテスト文字列です";
        let result = truncate_snippet(unicode, 5);
        assert!(result.ends_with("..."));
    }

    // -----------------------------------------------------------------------
    // Shortcut action validation tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_mark_read_missing_message_id() {
        let client = Client::new();
        let args = json!({});
        let err = mark_read(&client, "fake-token", &args)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Missing 'message_id'"), "got: {err}");
    }

    #[tokio::test]
    async fn test_mark_unread_missing_message_id() {
        let client = Client::new();
        let args = json!({});
        let err = mark_unread(&client, "fake-token", &args)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Missing 'message_id'"), "got: {err}");
    }

    #[tokio::test]
    async fn test_archive_missing_message_id() {
        let client = Client::new();
        let args = json!({});
        let err = archive_email(&client, "fake-token", &args)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Missing 'message_id'"), "got: {err}");
    }

    #[tokio::test]
    async fn test_untrash_missing_message_id() {
        let client = Client::new();
        let args = json!({});
        let err = untrash_email(&client, "fake-token", &args)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Missing 'message_id'"), "got: {err}");
    }

    #[tokio::test]
    async fn test_mark_read_invalid_message_id() {
        let client = Client::new();
        let args = json!({"message_id": "../../evil"});
        let err = mark_read(&client, "fake-token", &args)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Invalid message_id"), "got: {err}");
    }

    // -----------------------------------------------------------------------
    // Batch modify validation tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_batch_modify_missing_message_ids() {
        let client = Client::new();
        let args = json!({"add_labels": ["STARRED"]});
        let err = batch_modify(&client, "fake-token", &args)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Missing 'message_ids'"), "got: {err}");
    }

    #[tokio::test]
    async fn test_batch_modify_empty_ids() {
        let client = Client::new();
        let args = json!({"message_ids": [], "add_labels": ["STARRED"]});
        let err = batch_modify(&client, "fake-token", &args)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("At least one message ID"), "got: {err}");
    }

    #[tokio::test]
    async fn test_batch_modify_missing_labels() {
        let client = Client::new();
        let args = json!({"message_ids": ["abc123"]});
        let err = batch_modify(&client, "fake-token", &args)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("At least one of 'add_labels' or 'remove_labels'"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn test_batch_modify_invalid_id() {
        let client = Client::new();
        let args = json!({
            "message_ids": ["valid123", "../evil"],
            "add_labels": ["STARRED"]
        });
        let err = batch_modify(&client, "fake-token", &args)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Invalid message_id"), "got: {err}");
    }

    // -----------------------------------------------------------------------
    // Draft reading & attachment validation tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_read_draft_missing_draft_id() {
        let client = Client::new();
        let args = json!({});
        let err = read_draft(&client, "fake-token", &args)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Missing 'draft_id'"), "got: {err}");
    }

    #[tokio::test]
    async fn test_get_attachment_missing_message_id() {
        let client = Client::new();
        let args = json!({"attachment_id": "att123"});
        let err = get_attachment(&client, "fake-token", &args)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Missing 'message_id'"), "got: {err}");
    }

    #[tokio::test]
    async fn test_get_attachment_missing_attachment_id() {
        let client = Client::new();
        let args = json!({"message_id": "msg123"});
        let err = get_attachment(&client, "fake-token", &args)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Missing 'attachment_id'"), "got: {err}");
    }

    #[tokio::test]
    async fn test_get_attachment_invalid_ids() {
        let client = Client::new();
        let args = json!({"message_id": "msg/../bad", "attachment_id": "att123"});
        let err = get_attachment(&client, "fake-token", &args)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Invalid message_id"), "got: {err}");

        let args = json!({"message_id": "msg123", "attachment_id": "att/../bad"});
        let err = get_attachment(&client, "fake-token", &args)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Invalid attachment_id"), "got: {err}");
    }

    // -----------------------------------------------------------------------
    // Meta-test: all schema actions are dispatched
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_all_schema_actions_dispatched() {
        let def = tool_definition();
        let actions = def.function.parameters["properties"]["action"]["enum"]
            .as_array()
            .unwrap();

        let mut config = crate::config::Config::default();
        let env_var = "__BORG_TEST_GMAIL_DISPATCH__";
        config.credentials.insert(
            "GMAIL_API_KEY".to_string(),
            crate::config::CredentialValue::EnvVar(env_var.to_string()),
        );
        unsafe {
            std::env::set_var(env_var, "fake-token");
        }

        for action_val in actions {
            let action = action_val.as_str().unwrap();
            let args = json!({"action": action});
            let result = handle(&args, &config).await;
            // The call will fail (no real API), but it must NOT fail with "Unknown action"
            if let Err(e) = &result {
                let msg = e.to_string();
                assert!(
                    !msg.contains("Unknown action"),
                    "action '{action}' is in schema but not dispatched"
                );
            }
        }
    }

    integration_handle_tests!(gmail, "GMAIL_API_KEY");
}
