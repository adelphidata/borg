//! Minimal Block Kit builder for outbound Slack messages and modals.
//!
//! This is a typed DSL that serializes to the JSON shape `chat.postMessage` /
//! `views.open` accept. It covers the subset borg needs to render structured
//! replies and prompt for user input: section, header, divider, actions, input,
//! button, static-select, plain-text-input. For full Block Kit reference see
//! <https://api.slack.com/block-kit>.
//!
//! Round-trip stability matters here — Slack rejects payloads that include
//! unexpected fields, so every `Option` is `skip_serializing_if = "Option::is_none"`.

use serde::{Deserialize, Serialize};

/// A top-level block. Slack `chat.postMessage` accepts an array of these in
/// the `blocks` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    /// Renders a paragraph of text (mrkdwn or plain_text).
    Section {
        /// The block's text content.
        text: TextObject,
        /// Optional `block_id` for action attribution.
        #[serde(skip_serializing_if = "Option::is_none")]
        block_id: Option<String>,
    },
    /// Renders bold header text. Must be plain_text.
    Header {
        /// The header text.
        text: TextObject,
    },
    /// Horizontal rule.
    Divider,
    /// A row of action elements (buttons, selects, etc.).
    Actions {
        /// Action elements rendered side-by-side.
        elements: Vec<Element>,
        /// Optional `block_id` for action attribution.
        #[serde(skip_serializing_if = "Option::is_none")]
        block_id: Option<String>,
    },
    /// An input field for use inside a modal view. The `element` is the input
    /// type (e.g. plain text, static select).
    Input {
        /// Visible label rendered above the input.
        label: TextObject,
        /// The input element itself.
        element: Element,
        /// `block_id` is required by Slack for input blocks so the response
        /// can be located in `view.state.values`.
        block_id: String,
        /// If true, Slack delivers a `block_actions` payload as the user types.
        #[serde(skip_serializing_if = "Option::is_none")]
        dispatch_action: Option<bool>,
        /// Whether this input may be left blank.
        #[serde(skip_serializing_if = "Option::is_none")]
        optional: Option<bool>,
    },
}

/// A Slack text object — either mrkdwn or plain_text.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TextObject {
    /// Slack-flavored markdown.
    Mrkdwn {
        /// The mrkdwn-formatted string.
        text: String,
    },
    /// Plain text — required for headers and most labels.
    PlainText {
        /// The plain text content.
        text: String,
        /// Whether to render emoji shortcuts.
        #[serde(skip_serializing_if = "Option::is_none")]
        emoji: Option<bool>,
    },
}

impl TextObject {
    /// Build a `mrkdwn` text object.
    pub fn mrkdwn(text: impl Into<String>) -> Self {
        Self::Mrkdwn { text: text.into() }
    }

    /// Build a `plain_text` text object with emoji shortcodes enabled.
    pub fn plain(text: impl Into<String>) -> Self {
        Self::PlainText {
            text: text.into(),
            emoji: Some(true),
        }
    }
}

/// An interactive Block Kit element. Used inside `actions` and `input` blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Element {
    /// A clickable button.
    Button {
        /// Button label (must be plain_text).
        text: TextObject,
        /// Stable identifier the action callback uses to discriminate.
        action_id: String,
        /// Optional value forwarded back in the action payload.
        #[serde(skip_serializing_if = "Option::is_none")]
        value: Option<String>,
        /// Optional style: `"primary"` (green) or `"danger"` (red).
        #[serde(skip_serializing_if = "Option::is_none")]
        style: Option<String>,
        /// Optional URL to open instead of dispatching an action.
        #[serde(skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    /// A single-select dropdown of static options.
    StaticSelect {
        /// Placeholder text shown when no option is selected.
        placeholder: TextObject,
        /// Stable identifier the action callback uses to discriminate.
        action_id: String,
        /// Selectable options.
        options: Vec<SelectOption>,
        /// Pre-selected option, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        initial_option: Option<SelectOption>,
    },
    /// A free-form text input. Used inside `input` blocks in modals.
    PlainTextInput {
        /// Stable identifier referenced from `view.state.values`.
        action_id: String,
        /// Placeholder text shown when empty.
        #[serde(skip_serializing_if = "Option::is_none")]
        placeholder: Option<TextObject>,
        /// Pre-filled value.
        #[serde(skip_serializing_if = "Option::is_none")]
        initial_value: Option<String>,
        /// Whether the input renders as a multi-line textarea.
        #[serde(skip_serializing_if = "Option::is_none")]
        multiline: Option<bool>,
        /// Maximum input length in characters.
        #[serde(skip_serializing_if = "Option::is_none")]
        max_length: Option<u32>,
    },
}

/// One selectable option in a static-select element.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectOption {
    /// Visible label (plain_text).
    pub text: TextObject,
    /// The value forwarded back in the action payload.
    pub value: String,
}

/// Convenience constructor: a section with mrkdwn body, no `block_id`.
pub fn section_mrkdwn(text: impl Into<String>) -> Block {
    Block::Section {
        text: TextObject::mrkdwn(text),
        block_id: None,
    }
}

/// Convenience constructor: a header with plain text.
pub fn header(text: impl Into<String>) -> Block {
    Block::Header {
        text: TextObject::plain(text),
    }
}

/// Convenience constructor: an action row of buttons.
pub fn buttons(buttons: Vec<Element>) -> Block {
    Block::Actions {
        elements: buttons,
        block_id: None,
    }
}

/// Convenience constructor: a primary-styled button.
pub fn button(
    label: impl Into<String>,
    action_id: impl Into<String>,
    value: impl Into<String>,
) -> Element {
    Element::Button {
        text: TextObject::plain(label),
        action_id: action_id.into(),
        value: Some(value.into()),
        style: None,
        url: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn section_mrkdwn_matches_slack_reference_shape() {
        // Reference shape from <https://api.slack.com/reference/block-kit/blocks#section>.
        let block = section_mrkdwn("*Hello*");
        let value = serde_json::to_value(&block).unwrap();
        assert_eq!(
            value,
            json!({
                "type": "section",
                "text": { "type": "mrkdwn", "text": "*Hello*" }
            })
        );
    }

    #[test]
    fn header_serializes_as_plain_text_with_emoji_flag() {
        let block = header("Welcome");
        let value = serde_json::to_value(&block).unwrap();
        assert_eq!(
            value,
            json!({
                "type": "header",
                "text": { "type": "plain_text", "text": "Welcome", "emoji": true }
            })
        );
    }

    #[test]
    fn divider_serializes_to_just_type() {
        let value = serde_json::to_value(&Block::Divider).unwrap();
        assert_eq!(value, json!({ "type": "divider" }));
    }

    #[test]
    fn actions_block_with_button_matches_slack_shape() {
        let block = buttons(vec![button("Approve", "approve_btn", "yes")]);
        let value = serde_json::to_value(&block).unwrap();
        assert_eq!(
            value,
            json!({
                "type": "actions",
                "elements": [
                    {
                        "type": "button",
                        "text": { "type": "plain_text", "text": "Approve", "emoji": true },
                        "action_id": "approve_btn",
                        "value": "yes"
                    }
                ]
            })
        );
    }

    #[test]
    fn button_omits_optional_fields_when_none() {
        let value = serde_json::to_value(button("Go", "a", "v")).unwrap();
        let obj = value.as_object().unwrap();
        assert!(!obj.contains_key("style"));
        assert!(!obj.contains_key("url"));
    }

    #[test]
    fn input_block_renders_plain_text_input_with_block_id() {
        let block = Block::Input {
            label: TextObject::plain("Your name"),
            element: Element::PlainTextInput {
                action_id: "name_input".into(),
                placeholder: Some(TextObject::plain("Type here")),
                initial_value: None,
                multiline: None,
                max_length: None,
            },
            block_id: "name_block".into(),
            dispatch_action: None,
            optional: None,
        };
        let value = serde_json::to_value(&block).unwrap();
        assert_eq!(value["type"], "input");
        assert_eq!(value["block_id"], "name_block");
        assert_eq!(value["element"]["type"], "plain_text_input");
        assert_eq!(value["element"]["action_id"], "name_input");
        assert_eq!(value["label"]["text"], "Your name");
    }

    #[test]
    fn static_select_serializes_options_array() {
        let element = Element::StaticSelect {
            placeholder: TextObject::plain("Pick one"),
            action_id: "priority".into(),
            options: vec![
                SelectOption {
                    text: TextObject::plain("High"),
                    value: "high".into(),
                },
                SelectOption {
                    text: TextObject::plain("Low"),
                    value: "low".into(),
                },
            ],
            initial_option: None,
        };
        let value = serde_json::to_value(&element).unwrap();
        assert_eq!(value["type"], "static_select");
        let opts = value["options"].as_array().unwrap();
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0]["value"], "high");
        assert_eq!(opts[1]["text"]["text"], "Low");
        // initial_option must be omitted, not null.
        assert!(value.as_object().unwrap().get("initial_option").is_none());
    }

    #[test]
    fn round_trip_stability_full_block_array() {
        // Round-tripping the canonical mixed payload must not drift fields. This
        // catches regressions where `Option`s get serialized as `null` (which
        // Slack rejects with `invalid_blocks`).
        let blocks = vec![
            header("Status"),
            section_mrkdwn("All systems *nominal*."),
            Block::Divider,
            buttons(vec![
                button("Restart", "restart_btn", "restart"),
                button("Logs", "logs_btn", "logs"),
            ]),
        ];
        let serialized = serde_json::to_value(&blocks).unwrap();
        let round_tripped: Vec<Block> = serde_json::from_value(serialized.clone()).unwrap();
        let re_serialized = serde_json::to_value(&round_tripped).unwrap();
        assert_eq!(serialized, re_serialized, "round-trip must be stable");
        // No null fields anywhere in the serialized blocks — Slack rejects
        // them with `invalid_blocks`. Walk the JSON tree structurally rather
        // than substring-matching `":null"` (which would false-positive on a
        // legitimate string value containing that sequence).
        fn assert_no_nulls(v: &serde_json::Value) {
            match v {
                serde_json::Value::Null => panic!("Block Kit emitted a null"),
                serde_json::Value::Array(arr) => arr.iter().for_each(assert_no_nulls),
                serde_json::Value::Object(map) => map.values().for_each(assert_no_nulls),
                _ => {}
            }
        }
        assert_no_nulls(&serialized);
    }

    #[test]
    fn button_with_style_and_url_includes_them() {
        let btn = Element::Button {
            text: TextObject::plain("Open Docs"),
            action_id: "docs".into(),
            value: None,
            style: Some("primary".into()),
            url: Some("https://example.com".into()),
        };
        let value = serde_json::to_value(&btn).unwrap();
        assert_eq!(value["style"], "primary");
        assert_eq!(value["url"], "https://example.com");
        // value omitted when None
        assert!(value.as_object().unwrap().get("value").is_none());
    }
}
