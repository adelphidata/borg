//! Placeholder text for inbound media that could not be fetched.
//!
//! A failed attachment download must never be silent: the sender attached
//! media, so the agent must at least learn that it existed and is missing.
//! Otherwise the agent answers as if the message were text-only (or, for
//! uncaptioned media, the message can vanish entirely). Every channel's
//! inbound media path appends one of these placeholders on fetch failure,
//! alongside the usual `tracing::warn!`.

/// Coarse media kind for a user-visible placeholder, derived from a MIME type.
pub(crate) fn kind_for_mime(mime: &str) -> &'static str {
    if mime.starts_with("image/") {
        "image"
    } else if mime.starts_with("audio/") {
        "audio"
    } else if mime.starts_with("video/") {
        "video"
    } else {
        "file"
    }
}

/// Append `[<kind> could not be downloaded]` to the inbound message body.
///
/// Replaces the body when it is empty/whitespace so an uncaptioned failed
/// attachment still produces a non-empty message (empty-text guards in
/// several channels would otherwise drop the whole turn).
pub(crate) fn append_download_failure(text: &mut String, kind: &str) {
    let placeholder = format!("[{kind} could not be downloaded]");
    if text.trim().is_empty() {
        *text = placeholder;
    } else {
        text.push('\n');
        text.push_str(&placeholder);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_for_mime_maps_media_families() {
        let cases = [
            ("image/jpeg", "image"),
            ("image/png", "image"),
            ("audio/ogg", "audio"),
            ("video/mp4", "video"),
            ("application/pdf", "file"),
            ("application/octet-stream", "file"),
            ("text/plain", "file"),
        ];
        for (mime, expected) in cases {
            assert_eq!(kind_for_mime(mime), expected, "mime {mime}");
        }
    }

    #[test]
    fn appends_placeholder_on_new_line_after_existing_text() {
        let mut text = "check this out".to_string();
        append_download_failure(&mut text, "image");
        assert_eq!(text, "check this out\n[image could not be downloaded]");
    }

    #[test]
    fn empty_body_becomes_placeholder_not_empty_message() {
        // An uncaptioned failed attachment must still yield a non-empty body,
        // or downstream empty-text guards drop the whole message.
        for empty in ["", "   ", "\n"] {
            let mut text = empty.to_string();
            append_download_failure(&mut text, "audio");
            assert_eq!(text, "[audio could not be downloaded]");
        }
    }

    #[test]
    fn multiple_failures_stack_one_line_each() {
        let mut text = String::new();
        append_download_failure(&mut text, "image");
        append_download_failure(&mut text, "file");
        assert_eq!(
            text,
            "[image could not be downloaded]\n[file could not be downloaded]"
        );
    }
}
