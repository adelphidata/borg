use serde::Deserialize;

/// Twilio addresses WhatsApp endpoints as `whatsapp:+1555…`; SMS numbers carry
/// no prefix. The prefix is both the wire format and the channel discriminator.
pub const WHATSAPP_PREFIX: &str = "whatsapp:";

/// Distinguishes SMS vs WhatsApp for routing and response logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TwilioChannelType {
    Sms,
    WhatsApp,
}

impl TwilioChannelType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Sms => "sms",
            Self::WhatsApp => "whatsapp",
        }
    }
}

impl std::fmt::Display for TwilioChannelType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Twilio webhook payload (form-urlencoded).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct TwilioWebhook {
    /// The unique message SID.
    pub message_sid: String,
    /// The account SID that owns this message.
    #[serde(default)]
    pub account_sid: String,
    /// The sender phone number (E.164 format, e.g. "+14155551234").
    pub from: String,
    /// The recipient phone number.
    pub to: String,
    /// The message body text.
    #[serde(default)]
    pub body: String,
    /// Number of media items attached (SMS/MMS).
    #[serde(default)]
    pub num_media: Option<String>,
    /// First media URL (for voice/image/video attachments).
    #[serde(rename = "MediaUrl0", default)]
    pub media_url_0: Option<String>,
    /// Content type of the first media attachment.
    #[serde(rename = "MediaContentType0", default)]
    pub media_content_type_0: Option<String>,
}

impl TwilioWebhook {
    /// Returns the channel type based on the From number prefix.
    pub fn channel_type(&self) -> TwilioChannelType {
        if self.from.starts_with(WHATSAPP_PREFIX) {
            TwilioChannelType::WhatsApp
        } else {
            TwilioChannelType::Sms
        }
    }

    /// Returns true if this webhook is from a WhatsApp message.
    pub fn is_whatsapp(&self) -> bool {
        self.channel_type() == TwilioChannelType::WhatsApp
    }

    /// Returns the sender number without the "whatsapp:" prefix.
    pub fn sender_number(&self) -> &str {
        self.from
            .strip_prefix(WHATSAPP_PREFIX)
            .unwrap_or(&self.from)
    }

    /// Returns the recipient number without the "whatsapp:" prefix.
    pub fn recipient_number(&self) -> &str {
        self.to.strip_prefix(WHATSAPP_PREFIX).unwrap_or(&self.to)
    }

    /// Returns the first media (URL, content-type) if present.
    pub fn first_media(&self) -> Option<(&str, &str)> {
        match (&self.media_url_0, &self.media_content_type_0) {
            (Some(url), Some(ct)) => Some((url, ct)),
            _ => None,
        }
    }

    /// Returns true if the first media attachment is an audio type.
    pub fn has_audio_media(&self) -> bool {
        self.media_content_type_0
            .as_deref()
            .map(|ct| ct.starts_with("audio/"))
            .unwrap_or(false)
    }
}

/// Twilio message status callback (delivery receipt).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct StatusCallback {
    /// The message SID.
    pub message_sid: String,
    /// The message status (queued, sent, delivered, failed, undelivered).
    pub message_status: String,
    /// The account SID.
    #[serde(default)]
    pub account_sid: String,
    /// The sender number.
    #[serde(default)]
    pub from: String,
    /// The recipient number.
    #[serde(default)]
    pub to: String,
    /// Error code (if failed/undelivered).
    #[serde(default)]
    pub error_code: Option<String>,
    /// Error message (if failed/undelivered).
    #[serde(default)]
    pub error_message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn webhook(from: &str, to: &str) -> TwilioWebhook {
        TwilioWebhook {
            message_sid: "SM123".into(),
            account_sid: "AC123".into(),
            from: from.into(),
            to: to.into(),
            body: "hello".into(),
            num_media: None,
            media_url_0: None,
            media_content_type_0: None,
        }
    }

    /// Channel-type detection: the `whatsapp:` prefix on `From` is the
    /// single discriminator and must round-trip through `as_str` / `Display`
    /// with no casing change (Twilio's REST API expects lowercase).
    #[test]
    fn channel_type_detection_and_wire_format() {
        let cases = [
            (
                "whatsapp:+14155551234",
                "whatsapp:+14155555678",
                TwilioChannelType::WhatsApp,
                "whatsapp",
                "+14155551234",
                "+14155555678",
            ),
            (
                "+14155551234",
                "+14155555678",
                TwilioChannelType::Sms,
                "sms",
                "+14155551234",
                "+14155555678",
            ),
        ];
        for (from, to, kind, wire, sender, recipient) in cases {
            let wh = webhook(from, to);
            assert_eq!(wh.channel_type(), kind, "channel_type for {from}");
            assert_eq!(wh.is_whatsapp(), kind == TwilioChannelType::WhatsApp);
            assert_eq!(wh.sender_number(), sender, "sender for {from}");
            assert_eq!(wh.recipient_number(), recipient, "recipient for {to}");
            assert_eq!(kind.as_str(), wire, "as_str for {kind:?}");
            assert_eq!(format!("{kind}"), wire, "Display for {kind:?}");
        }
    }

    /// Media classification: `has_audio_media` must key off the
    /// `audio/` MIME prefix, and `first_media` must require BOTH URL and
    /// content-type. Partial media (one component present) returns `None`
    /// to avoid surfacing un-fetchable references to the agent.
    #[test]
    fn media_classification_matrix() {
        struct Case {
            name: &'static str,
            url: Option<&'static str>,
            content_type: Option<&'static str>,
            expect_audio: bool,
            expect_first_media: bool,
        }
        let cases = [
            Case {
                name: "audio/ogg url + ct → audio + first_media",
                url: Some("https://api.twilio.com/media/123"),
                content_type: Some("audio/ogg"),
                expect_audio: true,
                expect_first_media: true,
            },
            Case {
                name: "image/jpeg url + ct → not audio, first_media present",
                url: Some("https://api.twilio.com/media/456"),
                content_type: Some("image/jpeg"),
                expect_audio: false,
                expect_first_media: true,
            },
            Case {
                name: "no media at all → no audio, no first_media",
                url: None,
                content_type: None,
                expect_audio: false,
                expect_first_media: false,
            },
            Case {
                name: "url only (no ct) → no audio, no first_media",
                url: Some("https://api.twilio.com/media/789"),
                content_type: None,
                expect_audio: false,
                expect_first_media: false,
            },
            Case {
                name: "ct only (no url) → audio iff audio/*, but no first_media",
                url: None,
                content_type: Some("audio/mp4"),
                expect_audio: true,
                expect_first_media: false,
            },
        ];

        for c in cases {
            let wh = TwilioWebhook {
                message_sid: "SM123".into(),
                account_sid: "AC123".into(),
                from: "+14155551234".into(),
                to: "+14155555678".into(),
                body: String::new(),
                num_media: None,
                media_url_0: c.url.map(String::from),
                media_content_type_0: c.content_type.map(String::from),
            };
            assert_eq!(
                wh.has_audio_media(),
                c.expect_audio,
                "has_audio_media for {}",
                c.name
            );
            assert_eq!(
                wh.first_media().is_some(),
                c.expect_first_media,
                "first_media for {}",
                c.name
            );
        }
    }

    #[test]
    fn deserialize_status_callback_delivered() {
        let body = "MessageSid=SM123&MessageStatus=delivered&AccountSid=AC123&From=%2B14155551234&To=%2B14155555678";
        let cb: StatusCallback = serde_urlencoded::from_str(body).unwrap();
        assert_eq!(cb.message_sid, "SM123");
        assert_eq!(cb.message_status, "delivered");
        assert!(cb.error_code.is_none());
    }

    #[test]
    fn deserialize_status_callback_failed() {
        let body = "MessageSid=SM456&MessageStatus=failed&AccountSid=AC123&From=%2B14155551234&To=%2B14155555678&ErrorCode=30008&ErrorMessage=Unknown+error";
        let cb: StatusCallback = serde_urlencoded::from_str(body).unwrap();
        assert_eq!(cb.message_sid, "SM456");
        assert_eq!(cb.message_status, "failed");
        assert_eq!(cb.error_code.as_deref(), Some("30008"));
        assert_eq!(cb.error_message.as_deref(), Some("Unknown error"));
    }
}
