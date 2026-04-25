//! Data structures for the native iMessage integration.

/// Reply context: the parent message this one is a reply to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyContext {
    /// Text of the parent message.
    pub text: String,
    /// Sender identifier of the parent message (phone or email; `None` if from self).
    pub sender: Option<String>,
}

/// Metadata for an attachment surfaced from chat.db.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentMeta {
    /// Absolute file path on disk (typically under `~/Library/Messages/Attachments/`).
    pub path: String,
    /// Original filename as transferred (`transfer_name` column).
    pub filename: Option<String>,
    /// MIME type recorded by Messages.app, when available.
    pub mime_type: Option<String>,
}

/// A raw message row from chat.db.
#[derive(Debug)]
pub struct IMessagePayload {
    /// SQLite row ID from chat.db.
    pub rowid: i64,
    /// Sender phone number or email.
    pub sender: Option<String>,
    /// Whether this message was sent by the current user.
    pub is_from_me: bool,
    /// Text content of the message.
    pub text: Option<String>,
    /// Chat identifier string (e.g. "iMessage;-;+1234567890").
    pub chat_identifier: Option<String>,
    /// Whether the message is from a group chat.
    pub is_group: bool,
    /// Group GUID (chat.guid), used for allowlist matching on group threads.
    pub chat_guid: Option<String>,
    /// Inline-reply parent context (`thread_originator_guid` resolution).
    pub reply_to: Option<ReplyContext>,
    /// Attachment metadata (filenames, paths, MIME types) referenced by this message.
    pub attachments: Vec<AttachmentMeta>,
}

/// Decision result after running inbound message through all filters.
pub enum InboundDecision {
    /// Message should be dispatched to the agent.
    Dispatch {
        /// Sender identifier (phone number or email).
        sender_id: String,
        /// Message text content (may include a quoted reply prefix).
        text: String,
        /// Chat identifier for routing the response.
        channel_id: String,
    },
    /// Message should be dropped silently.
    Drop {
        /// Reason the message was dropped.
        reason: String,
    },
}

/// Normalize an iMessage handle (phone number or email) for stable identity.
///
/// - Emails: lowercased and trimmed.
/// - Phones: whitespace, dashes, parens, and dots stripped; leading `+` preserved.
///   A bare 10-digit string is left as-is (no country code is assumed) so we
///   don't fabricate identities; callers see exactly what chat.db stored.
/// - Anything else (e.g. iCloud display handles): trimmed only.
pub fn normalize_handle(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if trimmed.contains('@') {
        return trimmed.to_lowercase();
    }

    // Heuristic: if it has any digit, treat as phone.
    if trimmed.chars().any(|c| c.is_ascii_digit()) {
        let mut out = String::with_capacity(trimmed.len());
        let mut chars = trimmed.chars();
        if let Some(first) = chars.next() {
            if first == '+' {
                out.push('+');
            } else if first.is_ascii_digit() {
                out.push(first);
            }
            // Drop other leading punctuation.
        }
        for c in chars {
            if c.is_ascii_digit() {
                out.push(c);
            }
        }
        return out;
    }

    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imessage_payload_debug() {
        let payload = IMessagePayload {
            rowid: 42,
            sender: Some("+1234567890".to_string()),
            is_from_me: false,
            text: Some("Hello".to_string()),
            chat_identifier: Some("iMessage;-;+1234567890".to_string()),
            is_group: false,
            chat_guid: None,
            reply_to: None,
            attachments: Vec::new(),
        };
        let debug = format!("{payload:?}");
        assert!(debug.contains("42"));
        assert!(debug.contains("+1234567890"));
    }

    #[test]
    fn normalize_handle_phone_e164_strips_punctuation() {
        assert_eq!(normalize_handle("+1 (555) 123-4567"), "+15551234567");
        assert_eq!(normalize_handle("+44 20 7946 0958"), "+442079460958");
        assert_eq!(normalize_handle("+1.555.123.4567"), "+15551234567");
    }

    #[test]
    fn normalize_handle_phone_no_country_code_left_as_digits() {
        // Don't fabricate a country code — preserve what chat.db gave us.
        assert_eq!(normalize_handle("(555) 123-4567"), "5551234567");
        assert_eq!(normalize_handle("555-123-4567"), "5551234567");
    }

    #[test]
    fn normalize_handle_email_lowercases() {
        assert_eq!(normalize_handle("Foo@Bar.COM"), "foo@bar.com");
        assert_eq!(
            normalize_handle("  alice@example.org  "),
            "alice@example.org"
        );
    }

    #[test]
    fn normalize_handle_idempotent() {
        for input in [
            "+1 (555) 123-4567",
            "Foo@Bar.COM",
            "alice@example.org",
            "5551234567",
            "+15551234567",
        ] {
            let once = normalize_handle(input);
            let twice = normalize_handle(&once);
            assert_eq!(once, twice, "not idempotent for {input:?}");
        }
    }

    #[test]
    fn normalize_handle_empty_and_whitespace() {
        assert_eq!(normalize_handle(""), "");
        assert_eq!(normalize_handle("   "), "");
    }

    #[test]
    fn normalize_handle_non_phone_non_email_passthrough_trimmed() {
        // Some Messages.app handles are display-name strings (rare but possible).
        assert_eq!(normalize_handle("  alice  "), "alice");
    }

    #[test]
    fn inbound_decision_dispatch_variant() {
        let decision = InboundDecision::Dispatch {
            sender_id: "user1".to_string(),
            text: "hello".to_string(),
            channel_id: "ch1".to_string(),
        };
        match decision {
            InboundDecision::Dispatch {
                sender_id,
                text,
                channel_id,
            } => {
                assert_eq!(sender_id, "user1");
                assert_eq!(text, "hello");
                assert_eq!(channel_id, "ch1");
            }
            InboundDecision::Drop { .. } => panic!("expected Dispatch"),
        }
    }
}
