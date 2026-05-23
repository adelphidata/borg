/// Check that `args[key]` is a non-null string, returning an error message if missing.
fn require_str(action: &str, args: &serde_json::Value, key: &str) -> Option<String> {
    if args.get(key).and_then(serde_json::Value::as_str).is_some() {
        None
    } else {
        Some(format!("{action} requires '{key}' parameter"))
    }
}

/// Click/type targeting accepts either `selector` (CSS) or `ref` (`@e<N>`
/// from a prior `snapshot`). Exactly one must be set — both means the model
/// is ambiguous, neither means we have nothing to act on.
fn require_selector_or_ref(action: &str, args: &serde_json::Value) -> Option<String> {
    let sel = args.get("selector").and_then(serde_json::Value::as_str);
    let r = args.get("ref").and_then(serde_json::Value::as_str);
    match (sel, r) {
        (Some(_), Some(_)) => Some(format!(
            "{action}: provide either 'selector' or 'ref' (@e<N>), not both"
        )),
        (None, None) => Some(format!(
            "{action} requires 'selector' (CSS) or 'ref' (e.g. '@e5' from snapshot)"
        )),
        (Some(s), None) if s.is_empty() => {
            Some(format!("{action}: 'selector' must be non-empty"))
        }
        (None, Some(r)) if !r.starts_with("@e") => Some(format!(
            "{action}: 'ref' must look like '@e<N>' (from snapshot); got '{r}'"
        )),
        _ => None,
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
        // click/hover accept either a CSS `selector` or an `@e<N>` snapshot
        // ref. Reject when both are set so we don't silently pick one.
        "click" | "hover" => require_selector_or_ref(action, args),
        "type" => {
            require_selector_or_ref(action, args).or_else(|| require_str(action, args, "text"))
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
        "scroll" => {
            let dx = args.get("delta_x").and_then(serde_json::Value::as_i64);
            let dy = args.get("delta_y").and_then(serde_json::Value::as_i64);
            match (dx, dy) {
                (None, None) => Some(
                    "scroll requires 'delta_x' or 'delta_y' (signed integer)".to_string(),
                ),
                _ => None,
            }
        }
        "snapshot" => {
            // Optional max_elements; when present it must be a positive int.
            // (Zero is a no-op snapshot — refuse it rather than silently
            // returning an empty tree.)
            if let Some(v) = args.get("max_elements") {
                match v.as_u64() {
                    Some(0) | None => {
                        return Some(
                            "snapshot 'max_elements' must be a positive integer".to_string(),
                        );
                    }
                    Some(_) => {}
                }
            }
            None
        }
        "upload" => {
            if require_str(action, args, "selector").is_some() {
                return Some("upload requires 'selector' parameter".to_string());
            }
            let files = args.get("files").and_then(|v| v.as_array());
            match files {
                Some(arr) if !arr.is_empty() && arr.iter().all(|v| v.is_string()) => None,
                _ => Some(
                    "upload requires 'files' parameter (non-empty array of paths)".to_string(),
                ),
            }
        }
        "back" | "forward" | "get_images" => None,
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

    /// Every browser action's required-parameter contract in one table. A
    /// row is `(action, args, expect_err)` — `expect_err = true` means
    /// `validate_browser_args` must return `Some(msg)` (rejection), `false`
    /// means `None` (accepted). Adding a new browser action means
    /// appending one row per acceptance case plus at least one rejection.
    #[test]
    fn validate_browser_args_required_parameter_contract() {
        let cases: &[(&str, serde_json::Value, bool)] = &[
            // Unknown action — fails regardless of args.
            ("unknown_action", json!({}), true),
            // navigate
            ("navigate", json!({}), true),
            ("navigate", json!({"url": "https://example.com"}), false),
            // click / hover share require_str("selector")
            ("click", json!({}), true),
            ("click", json!({"selector": "#btn"}), false),
            ("hover", json!({}), true),
            ("hover", json!({"selector": ".btn"}), false),
            // type — needs both selector and text
            ("type", json!({"selector": "#input"}), true),
            ("type", json!({"text": "hello"}), true),
            (
                "type",
                json!({"selector": "#input", "text": "hello"}),
                false,
            ),
            // evaluate_js
            ("evaluate_js", json!({}), true),
            ("evaluate_js", json!({"expression": "1+1"}), false),
            // select
            ("select", json!({}), true),
            ("select", json!({"selector": "select"}), true),
            (
                "select",
                json!({"selector": "select", "value": "opt1"}),
                false,
            ),
            // press
            ("press", json!({}), true),
            ("press", json!({"key": "Enter"}), false),
            // drag — needs source + target
            ("drag", json!({}), true),
            ("drag", json!({"source": "#a"}), true),
            ("drag", json!({"source": "#a", "target": "#b"}), false),
            // fill — fields must be a JSON object, not a string
            ("fill", json!({}), true),
            ("fill", json!({"fields": "not_object"}), true),
            ("fill", json!({"fields": {"#email": "a@b.com"}}), false),
            // wait — condition is required; each condition has its own
            // value rule.
            ("wait", json!({}), true),
            ("wait", json!({"condition": "magic"}), true),
            ("wait", json!({"condition": "text"}), true),
            (
                "wait",
                json!({"condition": "text", "value": "hello"}),
                false,
            ),
            ("wait", json!({"condition": "element"}), true),
            (
                "wait",
                json!({"condition": "element", "value": "#el"}),
                false,
            ),
            ("wait", json!({"condition": "url", "value": "/page"}), false),
            // `load` is the one wait condition that takes no value.
            ("wait", json!({"condition": "load"}), false),
            ("wait", json!({"condition": "js"}), true),
            (
                "wait",
                json!({"condition": "js", "value": "document.ready"}),
                false,
            ),
            // resize — both width and height required as integers
            ("resize", json!({}), true),
            ("resize", json!({"width": 800}), true),
            ("resize", json!({"width": 800, "height": 600}), false),
            // switch_tab
            ("switch_tab", json!({}), true),
            ("switch_tab", json!({"tab_index": 0}), false),
            // Actions with no required params accept empty args.
            ("screenshot", json!({}), false),
            ("new_tab", json!({}), false),
            ("list_tabs", json!({}), false),
            ("close_tab", json!({}), false),
            ("get_console_logs", json!({}), false),
            // back / forward / get_images take no params.
            ("back", json!({}), false),
            ("forward", json!({}), false),
            ("get_images", json!({}), false),
            // scroll — at least one of delta_x / delta_y required.
            ("scroll", json!({}), true),
            ("scroll", json!({"delta_y": 200}), false),
            ("scroll", json!({"delta_x": -100}), false),
            ("scroll", json!({"delta_x": 0, "delta_y": 0}), false), // both zero is allowed at validate; runtime rejects
            // snapshot — optional max_elements, but 0 is rejected.
            ("snapshot", json!({}), false),
            ("snapshot", json!({"max_elements": 50}), false),
            ("snapshot", json!({"max_elements": 0}), true),
            ("snapshot", json!({"max_elements": "lots"}), true),
            // upload — selector required, files must be non-empty string array.
            ("upload", json!({}), true),
            ("upload", json!({"selector": "input"}), true),
            ("upload", json!({"selector": "input", "files": []}), true),
            (
                "upload",
                json!({"selector": "input", "files": [42]}),
                true,
            ),
            (
                "upload",
                json!({"selector": "input", "files": ["/tmp/a.png"]}),
                false,
            ),
            // click — selector OR ref, not both, not neither.
            ("click", json!({"ref": "@e5"}), false),
            (
                "click",
                json!({"selector": "#btn", "ref": "@e5"}),
                true,
            ),
            ("click", json!({"ref": "e5"}), true),
            ("click", json!({"selector": ""}), true),
            // type — same selector-or-ref rule + text required.
            (
                "type",
                json!({"ref": "@e1", "text": "hi"}),
                false,
            ),
            (
                "type",
                json!({"ref": "@e1"}),
                true,
            ),
        ];

        for (action, args, expect_err) in cases {
            let got = validate_browser_args(action, args);
            if *expect_err {
                assert!(
                    got.is_some(),
                    "validate({action}, {args}) expected Err, got Ok"
                );
            } else {
                assert!(
                    got.is_none(),
                    "validate({action}, {args}) expected Ok, got Err: {got:?}"
                );
            }
        }
    }
}
