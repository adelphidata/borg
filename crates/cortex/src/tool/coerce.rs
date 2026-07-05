//! Lenient coercion of tool-call arguments for open-weight models.
//!
//! Open-weight models (DeepSeek/Qwen/GLM) frequently emit array or object tool
//! arguments as JSON *strings* — e.g. `{"operations": "[{...}]"}` for a
//! `write_memory` batch, or an array whose items are each a stringified object.
//! Strict `serde_json::from_value` rejects those, producing a synthetic
//! `InvalidArgs` error and wasting a retry round-trip. This module retries the
//! parse once with such strings coerced to their parsed form.

use serde_json::Value;

use super::{Tool, ToolParseError};

/// Parse `raw_args` into a typed [`Tool`], retrying once with JSON-string
/// arguments coerced to their parsed array/object form if the strict parse
/// fails with [`ToolParseError::InvalidArgs`].
///
/// The coercion is only *kept* when it makes the parse succeed. If the retry
/// still fails, the original strict error is returned unchanged — so a genuinely
/// malformed call still surfaces its real message, and a legitimate string
/// argument is never silently mangled (a string field won't deserialize from an
/// array/object, so a wrongful coercion just fails the retry and is discarded).
pub(crate) fn parse_with_coercion(name: &str, raw_args: Value) -> Result<Tool, ToolParseError> {
    match Tool::parse(name, raw_args.clone()) {
        Ok(tool) => Ok(tool),
        Err(err @ ToolParseError::InvalidArgs { .. }) => {
            if let Some(coerced) = coerce_json_string_args(&raw_args) {
                if let Ok(tool) = Tool::parse(name, coerced) {
                    tracing::debug!(tool = %name, "coerced JSON-string tool args on retry");
                    return Ok(tool);
                }
            }
            Err(err)
        }
        Err(other) => Err(other),
    }
}

/// Walk `args`, replacing any string value that itself parses as a JSON array
/// or object with the parsed value (recursing into arrays/objects). Scalars
/// encoded as strings (`"5"`, `"true"`, plain prose) are left untouched, so a
/// genuine string field is not mangled. Returns `Some(coerced)` only when
/// something actually changed, so the caller can cheaply skip a pointless retry.
fn coerce_json_string_args(args: &Value) -> Option<Value> {
    let mut out = args.clone();
    coerce_in_place(&mut out).then_some(out)
}

/// Recursively coerce JSON-string array/object values in place. Returns whether
/// any value was changed. A replaced value is not recursed into again — the
/// parsed JSON is already structured, and the observed failure modes are one
/// level of stringification, not nested re-encoding.
fn coerce_in_place(v: &mut Value) -> bool {
    match v {
        Value::Object(map) => map
            .values_mut()
            .fold(false, |acc, val| coerce_in_place(val) || acc),
        Value::Array(arr) => arr
            .iter_mut()
            .fold(false, |acc, item| coerce_in_place(item) || acc),
        Value::String(s) => {
            let trimmed = s.trim_start();
            if trimmed.starts_with('[') || trimmed.starts_with('{') {
                if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                    if matches!(parsed, Value::Array(_) | Value::Object(_)) {
                        *v = parsed;
                        return true;
                    }
                }
            }
            false
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::names as tn;
    use serde_json::json;

    #[test]
    fn coerces_whole_array_field_sent_as_json_string() {
        // write_memory batch: `operations` is an array of objects, but the model
        // sent the entire array as a JSON string. Strict parse fails; coercion
        // should make it parse.
        let op = json!({"action": "add", "filename": "new.md", "content": "hi"});
        let raw = json!({ "operations": serde_json::to_string(&json!([op])).unwrap() });
        // Strict parse fails on the stringified array...
        assert!(matches!(
            Tool::parse(tn::WRITE_MEMORY, raw.clone()),
            Err(ToolParseError::InvalidArgs { .. })
        ));
        // ...but the coercion-aware parse succeeds.
        assert!(matches!(
            parse_with_coercion(tn::WRITE_MEMORY, raw),
            Ok(Tool::WriteMemory(_))
        ));
    }

    #[test]
    fn coerces_array_items_sent_as_json_strings() {
        // Each array item stringified individually.
        let op = json!({"action": "add", "filename": "new.md", "content": "hi"});
        let raw = json!({ "operations": [serde_json::to_string(&op).unwrap()] });
        assert!(matches!(
            parse_with_coercion(tn::WRITE_MEMORY, raw),
            Ok(Tool::WriteMemory(_))
        ));
    }

    #[test]
    fn well_formed_args_are_untouched() {
        // A correct call must not be altered or double-parsed.
        let raw = json!({
            "operations": [{"action": "add", "filename": "new.md", "content": "hi"}]
        });
        assert!(matches!(
            parse_with_coercion(tn::WRITE_MEMORY, raw),
            Ok(Tool::WriteMemory(_))
        ));
    }

    #[test]
    fn genuinely_malformed_args_keep_their_original_error() {
        // A call that is broken in a way coercion can't fix must still surface
        // an InvalidArgs error (not a false success, not a different error).
        let raw = json!({ "operations": 42 }); // wrong type entirely
        assert!(matches!(
            parse_with_coercion(tn::WRITE_MEMORY, raw),
            Err(ToolParseError::InvalidArgs { .. })
        ));
    }

    #[test]
    fn coerce_leaves_non_json_strings_alone() {
        // Strings that merely start with a bracket-ish char but aren't valid
        // JSON, and ordinary prose, are left as-is (returns None → no change).
        let raw = json!({ "a": "hello", "b": "[not json", "c": "42" });
        assert!(coerce_json_string_args(&raw).is_none());
    }

    #[test]
    fn coerce_reports_change_only_when_it_happens() {
        let changed = json!({ "x": "[1,2,3]" });
        let out = coerce_json_string_args(&changed).expect("should coerce");
        assert_eq!(out["x"], json!([1, 2, 3]));
    }
}
