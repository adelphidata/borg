//! Best-effort waterfall compaction for unusually large tool input schemas.
//!
//! Per-turn cost: every tool's JSON schema is shipped to the LLM. The
//! existing per-property metadata pass in [`crate::tool::definitions::
//! compact_tool_schema`] handles ordinary tools; this module handles the
//! pathological tail (large MCP / WASM plugin schemas with deep `$defs` or
//! verbose descriptions) by applying increasingly lossy passes *only while
//! the schema remains over budget*. Ordinary tools pay no cost.
//!
//! Top-level argument names (the `properties` keys) are preserved through
//! every pass so the model still knows *what* arguments exist; only the
//! description of *what they accept* is allowed to degrade.

use serde_json::Value;

/// Compact JSON byte length is a cheap local proxy for the ~1k-token schema
/// budget. 4_000 bytes ≈ 1k tokens on `o200k_base` for typical schema text.
const MAX_COMPACT_TOOL_SCHEMA_BYTES: usize = 4_000;

/// Collapse depth for stage 3: any object schema nested deeper than this
/// is replaced with `{}`. Depth 2 keeps `properties.foo` intact but
/// collapses `properties.foo.properties.bar`.
const MAX_COMPACT_TOOL_SCHEMA_DEPTH: usize = 2;

type CompactionPass = fn(&mut Value);

const PASSES: &[CompactionPass] = &[
    strip_schema_descriptions,
    drop_schema_definitions,
    collapse_deep_complex_schemas,
];

/// Apply increasingly lossy passes until the schema fits the budget proxy
/// or all passes are exhausted. No-op when the schema is already small.
pub fn compact_large_tool_schema(value: &mut Value) {
    for pass in PASSES {
        if fits_budget(value) {
            return;
        }
        pass(value);
    }
}

fn fits_budget(value: &Value) -> bool {
    // `to_vec` is allocating but the schema is already in memory and this
    // only runs at prompt-assembly time, not per token.
    serde_json::to_vec(value)
        .map(|v| v.len() <= MAX_COMPACT_TOOL_SCHEMA_BYTES)
        .unwrap_or(true)
}

/// Stage 1: strip every `description` key, anywhere in the tree. Descriptions
/// are the highest-bytes-per-information item in a typical large schema.
fn strip_schema_descriptions(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("description");
            for v in map.values_mut() {
                strip_schema_descriptions(v);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                strip_schema_descriptions(v);
            }
        }
        _ => {}
    }
}

/// Stage 2: drop root-level `$defs` and `definitions`. Local `$ref`s become
/// dangling pointers — that is intentional: a model handed a too-large
/// schema is better served by a permissive shape than by a precise one it
/// can't fit in context. Inline references stay; only the definition pool
/// is removed.
fn drop_schema_definitions(value: &mut Value) {
    if let Value::Object(map) = value {
        map.remove("$defs");
        map.remove("definitions");
    }
}

/// Stage 3: collapse deeply-nested complex object schemas to `{}`. The root
/// keys (`type`, `properties`, `required`) are preserved; each property's
/// inner schema is kept up to [`MAX_COMPACT_TOOL_SCHEMA_DEPTH`] levels and
/// then replaced. The model still sees the argument *names* but loses the
/// nested shape — a deliberate trade for fitting the budget.
fn collapse_deep_complex_schemas(value: &mut Value) {
    collapse_at_depth(value, 0);
}

fn collapse_at_depth(value: &mut Value, depth: usize) {
    if depth >= MAX_COMPACT_TOOL_SCHEMA_DEPTH {
        if let Value::Object(map) = value {
            // Replace any nested object/array schema bodies with `{}` so the
            // shape disappears; primitives are already cheap.
            let keys: Vec<String> = map.keys().cloned().collect();
            for k in keys {
                if matches!(map.get(&k), Some(Value::Object(_)) | Some(Value::Array(_))) {
                    map.insert(k, Value::Object(serde_json::Map::new()));
                }
            }
        }
        return;
    }
    match value {
        Value::Object(map) => {
            for v in map.values_mut() {
                collapse_at_depth(v, depth + 1);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                collapse_at_depth(v, depth + 1);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bytes(v: &Value) -> usize {
        serde_json::to_vec(v).unwrap().len()
    }

    #[test]
    fn small_schema_passes_through_unchanged() {
        // Spec: schemas that already fit the budget must be byte-identical
        // after a compaction round-trip. Anything else would needlessly
        // perturb the Anthropic prompt-cache breakpoint on the tools array.
        let mut s = json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "search string"}
            },
            "required": ["query"]
        });
        let before = s.clone();
        compact_large_tool_schema(&mut s);
        assert_eq!(s, before);
    }

    #[test]
    fn stage_one_strips_oversize_descriptions_but_keeps_property_names() {
        // 5 KB of description in a single property — over budget. The
        // description must drop but `properties.query` must survive so the
        // model still knows the tool takes a `query` argument.
        let mut s = json!({
            "type": "object",
            "description": "x".repeat(5_000),
            "properties": {
                "query": {"type": "string", "description": "y".repeat(2_000)}
            },
            "required": ["query"]
        });
        compact_large_tool_schema(&mut s);
        assert!(bytes(&s) <= MAX_COMPACT_TOOL_SCHEMA_BYTES);
        assert!(s.get("description").is_none());
        let q = s.pointer("/properties/query").expect("query survives");
        assert!(q.get("description").is_none());
        // Argument surface is intact.
        assert_eq!(
            s.pointer("/properties/query/type"),
            Some(&Value::String("string".into()))
        );
        assert_eq!(s.get("required"), Some(&json!(["query"])));
    }

    #[test]
    fn stage_two_drops_definitions_when_still_over_budget() {
        // `$defs` block large enough to remain over budget AFTER stage 1
        // strips descriptions. Use bulky structural content (long property
        // names and enum strings) so stage 1 alone cannot get it under.
        let mut defs = serde_json::Map::new();
        let bulky_enum: Vec<Value> = (0..20)
            .map(|i| Value::String(format!("variant_{i}_{}", "x".repeat(80))))
            .collect();
        for i in 0..40 {
            defs.insert(
                format!("Def{i}_{}", "y".repeat(20)),
                json!({"type": "string", "enum": bulky_enum.clone()}),
            );
        }
        let mut s = json!({
            "type": "object",
            "properties": {"q": {"type": "string"}},
            "$defs": Value::Object(defs),
        });
        assert!(
            bytes(&s) > MAX_COMPACT_TOOL_SCHEMA_BYTES,
            "fixture must start over budget"
        );
        compact_large_tool_schema(&mut s);
        assert!(bytes(&s) <= MAX_COMPACT_TOOL_SCHEMA_BYTES);
        assert!(s.get("$defs").is_none(), "$defs must be dropped");
        assert!(
            s.pointer("/properties/q").is_some(),
            "argument name survives"
        );
    }

    #[test]
    fn stage_three_collapses_deep_objects_to_empty() {
        // Construct a schema whose oversize is entirely in deep nesting of
        // property bodies (no description, no $defs). Stage 3 must collapse
        // the deep objects while leaving the root `properties` keys intact.
        let mut deep_props = serde_json::Map::new();
        let big_enum: Vec<Value> = (0..5).map(|_| Value::String("a".repeat(100))).collect();
        for i in 0..40 {
            // Each property's `properties` block has a chunky nested object.
            deep_props.insert(
                format!("p{i}"),
                json!({
                    "type": "object",
                    "properties": {
                        "nested": {
                            "type": "object",
                            "properties": {
                                "deep": {"type": "string", "enum": big_enum.clone()}
                            }
                        }
                    }
                }),
            );
        }
        let mut s = json!({
            "type": "object",
            "properties": Value::Object(deep_props),
        });
        compact_large_tool_schema(&mut s);
        assert!(bytes(&s) <= MAX_COMPACT_TOOL_SCHEMA_BYTES);
        // Top-level argument names must survive.
        for i in 0..40 {
            assert!(
                s.pointer(&format!("/properties/p{i}")).is_some(),
                "property p{i} must remain present after collapse"
            );
        }
    }

    #[test]
    fn stops_as_soon_as_budget_is_met() {
        // A schema that needs only stage 1 must not have its $defs removed
        // by stage 2 — the waterfall is "run while over budget", not "run
        // every pass".
        let mut s = json!({
            "type": "object",
            "description": "x".repeat(5_000),
            "$defs": {"Tiny": {"type": "string"}},
            "properties": {"q": {"type": "string"}}
        });
        compact_large_tool_schema(&mut s);
        assert!(s.get("description").is_none(), "stage 1 ran");
        assert!(
            s.get("$defs").is_some(),
            "stage 2 must not run once budget is met"
        );
    }
}
