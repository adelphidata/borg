/// Check that `args[key]` is a non-null string, returning an error message if missing.
fn require_str(action: &str, args: &serde_json::Value, key: &str) -> Option<String> {
    if args.get(key).and_then(serde_json::Value::as_str).is_some() {
        None
    } else {
        Some(format!("{action} requires '{key}' parameter"))
    }
}

/// Check that `args[key]` is a non-null integer, returning an error message if missing.
fn require_u64(action: &str, args: &serde_json::Value, key: &str) -> Option<String> {
    if args.get(key).and_then(serde_json::Value::as_u64).is_some() {
        None
    } else {
        Some(format!("{action} requires '{key}' parameter (integer)"))
    }
}

/// Validate arguments for a browser action. Returns an error message if invalid.
pub fn validate_browser_args(action: &str, args: &serde_json::Value) -> Option<String> {
    match action {
        "navigate" => require_str(action, args, "url"),
        "click" | "hover" => require_str(action, args, "selector"),
        "type" => {
            require_str(action, args, "selector").or_else(|| require_str(action, args, "text"))
        }
        "evaluate_js" => require_str(action, args, "expression"),
        "select" => {
            require_str(action, args, "selector").or_else(|| require_str(action, args, "value"))
        }
        "press" => require_str(action, args, "key"),
        "drag" => {
            require_str(action, args, "source").or_else(|| require_str(action, args, "target"))
        }
        "fill" => {
            if args.get("fields").and_then(|v| v.as_object()).is_none() {
                return Some("fill requires 'fields' parameter (object)".to_string());
            }
            None
        }
        "wait" => {
            let condition = args.get("condition").and_then(serde_json::Value::as_str);
            match condition {
                Some(c @ ("text" | "element" | "url" | "js")) => {
                    if args
                        .get("value")
                        .and_then(serde_json::Value::as_str)
                        .is_none()
                    {
                        return Some(format!(
                            "wait with condition '{c}' requires 'value' parameter"
                        ));
                    }
                    None
                }
                Some("load") => None,
                Some(c) => Some(format!(
                    "Unknown wait condition: {c}. Use: text, element, url, load, js"
                )),
                None => Some(
                    "wait requires 'condition' parameter (text|element|url|load|js)".to_string(),
                ),
            }
        }
        "resize" => {
            require_u64(action, args, "width").or_else(|| require_u64(action, args, "height"))
        }
        "switch_tab" => require_u64(action, args, "tab_index"),
        "new_tab" | "list_tabs" | "close_tab" => None,
        "screenshot" | "get_text" | "close" | "get_console_logs" => None,
        _ => Some(format!("Unknown browser action: {action}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validate_navigate_requires_url() {
        assert!(validate_browser_args("navigate", &json!({})).is_some());
        assert!(
            validate_browser_args("navigate", &json!({"url": "https://example.com"})).is_none()
        );
    }

    #[test]
    fn validate_click_requires_selector() {
        assert!(validate_browser_args("click", &json!({})).is_some());
        assert!(validate_browser_args("click", &json!({"selector": "#btn"})).is_none());
    }

    #[test]
    fn validate_type_requires_selector_and_text() {
        assert!(validate_browser_args("type", &json!({"selector": "#input"})).is_some());
        assert!(validate_browser_args("type", &json!({"text": "hello"})).is_some());
        assert!(
            validate_browser_args("type", &json!({"selector": "#input", "text": "hello"}))
                .is_none()
        );
    }

    #[test]
    fn validate_evaluate_js_requires_expression() {
        assert!(validate_browser_args("evaluate_js", &json!({})).is_some());
        assert!(validate_browser_args("evaluate_js", &json!({"expression": "1+1"})).is_none());
    }

    #[test]
    fn validate_screenshot_no_required_params() {
        assert!(validate_browser_args("screenshot", &json!({})).is_none());
    }

    #[test]
    fn validate_unknown_action() {
        assert!(validate_browser_args("unknown_action", &json!({})).is_some());
    }

    #[test]
    fn validate_hover_requires_selector() {
        assert!(validate_browser_args("hover", &json!({})).is_some());
        assert!(validate_browser_args("hover", &json!({"selector": ".btn"})).is_none());
    }

    #[test]
    fn validate_select_requires_selector_and_value() {
        assert!(validate_browser_args("select", &json!({})).is_some());
        assert!(validate_browser_args("select", &json!({"selector": "select"})).is_some());
        assert!(
            validate_browser_args("select", &json!({"selector": "select", "value": "opt1"}))
                .is_none()
        );
    }

    #[test]
    fn validate_press_requires_key() {
        assert!(validate_browser_args("press", &json!({})).is_some());
        assert!(validate_browser_args("press", &json!({"key": "Enter"})).is_none());
    }

    #[test]
    fn validate_drag_requires_source_and_target() {
        assert!(validate_browser_args("drag", &json!({})).is_some());
        assert!(validate_browser_args("drag", &json!({"source": "#a"})).is_some());
        assert!(validate_browser_args("drag", &json!({"source": "#a", "target": "#b"})).is_none());
    }

    #[test]
    fn validate_fill_requires_fields_object() {
        assert!(validate_browser_args("fill", &json!({})).is_some());
        assert!(validate_browser_args("fill", &json!({"fields": "not_object"})).is_some());
        assert!(validate_browser_args("fill", &json!({"fields": {"#email": "a@b.com"}})).is_none());
    }

    #[test]
    fn validate_wait_requires_condition() {
        assert!(validate_browser_args("wait", &json!({})).is_some());
    }

    #[test]
    fn validate_wait_text_requires_value() {
        assert!(validate_browser_args("wait", &json!({"condition": "text"})).is_some());
        assert!(
            validate_browser_args("wait", &json!({"condition": "text", "value": "hello"}))
                .is_none()
        );
    }

    #[test]
    fn validate_wait_element_requires_value() {
        assert!(validate_browser_args("wait", &json!({"condition": "element"})).is_some());
        assert!(
            validate_browser_args("wait", &json!({"condition": "element", "value": "#el"}))
                .is_none()
        );
    }

    #[test]
    fn validate_wait_url_requires_value() {
        assert!(
            validate_browser_args("wait", &json!({"condition": "url", "value": "/page"})).is_none()
        );
    }

    #[test]
    fn validate_wait_load_no_value_needed() {
        assert!(validate_browser_args("wait", &json!({"condition": "load"})).is_none());
    }

    #[test]
    fn validate_wait_js_requires_value() {
        assert!(validate_browser_args("wait", &json!({"condition": "js"})).is_some());
        assert!(validate_browser_args(
            "wait",
            &json!({"condition": "js", "value": "document.ready"})
        )
        .is_none());
    }

    #[test]
    fn validate_wait_unknown_condition() {
        assert!(validate_browser_args("wait", &json!({"condition": "magic"})).is_some());
    }

    #[test]
    fn validate_resize_requires_width_and_height() {
        assert!(validate_browser_args("resize", &json!({})).is_some());
        assert!(validate_browser_args("resize", &json!({"width": 800})).is_some());
        assert!(validate_browser_args("resize", &json!({"width": 800, "height": 600})).is_none());
    }

    #[test]
    fn validate_switch_tab_requires_index() {
        assert!(validate_browser_args("switch_tab", &json!({})).is_some());
        assert!(validate_browser_args("switch_tab", &json!({"tab_index": 0})).is_none());
    }

    #[test]
    fn validate_tab_actions_no_required_params() {
        assert!(validate_browser_args("new_tab", &json!({})).is_none());
        assert!(validate_browser_args("list_tabs", &json!({})).is_none());
        assert!(validate_browser_args("close_tab", &json!({})).is_none());
    }

    #[test]
    fn validate_get_console_logs_no_required_params() {
        assert!(validate_browser_args("get_console_logs", &json!({})).is_none());
    }
}
