//! Lightweight accessibility-tree snapshot for the `browser_snapshot` tool.
//!
//! The model navigates dense pages by asking for a `snapshot`, then targeting
//! follow-up clicks/types by the `@e<N>` ref the snapshot assigned to each
//! interactive element. This avoids selector-engineering on every page —
//! the snapshot becomes the page's "table of contents" for the next turn.
//!
//! The walk is deliberately minimal: we collect interactive elements only
//! (links, buttons, inputs, [role] elements). Capping to `max_elements`
//! prevents huge SPAs from saturating LLM context with thousands of nodes.
//! Refs are page-scoped, allocated server-side, and held in a sidecar map
//! on [`crate::browser::BrowserSession`] so a follow-up `click {ref}` knows
//! what to act on. Switching tabs invalidates the previous snapshot's refs.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use chromiumoxide::Page;
use serde::{Deserialize, Serialize};

/// One interactive element from the snapshot. The model only needs enough
/// to decide what to do next — role, accessible name, a short text excerpt,
/// and the ref to address it by. We deliberately omit raw selectors so the
/// LLM doesn't try to invent stale CSS from the bounds it sees here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotElement {
    /// `@e<N>` — opaque, page-scoped. Pass back as `ref` on click/type.
    pub r#ref: String,
    /// `tag` is the lowercased element tag (`a`, `button`, `input`).
    pub tag: String,
    /// Best-effort accessible name (aria-label, alt, value, or trimmed text).
    pub name: String,
    /// Where the element sits on the page in CSS pixels, post-layout.
    pub bounds: ElementBounds,
}

/// `getBoundingClientRect()` in serialised form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElementBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Resolved snapshot: the elements the model will see plus the
/// ref → CSS-selector map the session retains so a follow-up `click @e5`
/// knows which element to interact with.
pub struct SnapshotResult {
    pub elements: Vec<SnapshotElement>,
    pub selectors: HashMap<String, String>,
    /// Total interactive elements on the page — when this exceeds
    /// `elements.len()` the snapshot was truncated. Surfaced so the
    /// model knows it isn't seeing the whole page.
    pub total: usize,
}

/// Snapshot the current page. Returns up to `max_elements` interactive
/// elements with stable per-snapshot refs.
///
/// The JS walks the DOM once and emits a unique CSS selector per element
/// (via nth-of-type chains) so the Rust side can dispatch follow-up clicks
/// by ref without needing a second round-trip to the page.
pub async fn snapshot(
    page: &Page,
    max_elements: usize,
    timeout: Duration,
) -> Result<SnapshotResult> {
    // The selector builder walks up to `document` accumulating `tag:nth-of-type(N)`
    // segments. It's not the prettiest selector in the world, but it's
    // unambiguous and survives the page rendering between snapshot and
    // follow-up action. Using IDs when present makes the selector readable
    // for the operator who's watching tracing logs.
    let js = format!(
        r#"(() => {{
            const MAX = {max_elements};
            const interactive = 'a, button, input, select, textarea, [role="button"], [role="link"], [role="checkbox"], [role="textbox"], [role="tab"], [role="menuitem"]';
            const nodes = Array.from(document.querySelectorAll(interactive));
            const total = nodes.length;
            const slice = nodes.slice(0, MAX);
            const cssPath = (el) => {{
                if (el.id) return '#' + CSS.escape(el.id);
                const parts = [];
                let cur = el;
                while (cur && cur.nodeType === 1 && cur !== document.documentElement) {{
                    const tag = cur.tagName.toLowerCase();
                    const parent = cur.parentNode;
                    if (!parent) {{ parts.unshift(tag); break; }}
                    const same = Array.from(parent.children).filter(c => c.tagName === cur.tagName);
                    const idx = same.indexOf(cur) + 1;
                    parts.unshift(same.length > 1 ? `${{tag}}:nth-of-type(${{idx}})` : tag);
                    cur = parent;
                }}
                return parts.join(' > ');
            }};
            const name = (el) => {{
                const cand = el.getAttribute('aria-label')
                    || el.getAttribute('alt')
                    || el.getAttribute('title')
                    || el.getAttribute('placeholder')
                    || el.value
                    || (el.innerText || '').trim();
                return (cand || '').slice(0, 200);
            }};
            const out = slice.map((el) => {{
                const r = el.getBoundingClientRect();
                return {{
                    tag: el.tagName.toLowerCase(),
                    name: name(el),
                    selector: cssPath(el),
                    bounds: {{ x: r.x, y: r.y, width: r.width, height: r.height }},
                }};
            }});
            return {{ total, elements: out }};
        }})()"#
    );

    let raw: serde_json::Value = tokio::time::timeout(timeout, async {
        page.evaluate(js.as_str())
            .await
            .context("Snapshot JS evaluation failed")?
            .into_value()
            .context("Snapshot result was not serialisable")
    })
    .await
    .context("Snapshot timed out")??;

    let total = raw
        .get("total")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as usize;
    let arr = raw
        .get("elements")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut elements = Vec::with_capacity(arr.len());
    let mut selectors = HashMap::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let r#ref = format!("@e{i}");
        let selector = item
            .get("selector")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let tag = item
            .get("tag")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let name = item
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let bounds = item
            .get("bounds")
            .map(|b| ElementBounds {
                x: b.get("x")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(0.0),
                y: b.get("y")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(0.0),
                width: b
                    .get("width")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(0.0),
                height: b
                    .get("height")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(0.0),
            })
            .unwrap_or(ElementBounds {
                x: 0.0,
                y: 0.0,
                width: 0.0,
                height: 0.0,
            });
        selectors.insert(r#ref.clone(), selector);
        elements.push(SnapshotElement {
            r#ref,
            tag,
            name,
            bounds,
        });
    }

    Ok(SnapshotResult {
        elements,
        selectors,
        total,
    })
}

/// Render the snapshot to a model-friendly text block. We keep the wire
/// format stable enough that prompt examples stay valid release-to-release —
/// `@e<N> <tag> "<name>"` is the line the model is taught to recognise.
pub fn render(result: &SnapshotResult) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Page snapshot ({} of {} interactive elements):\n",
        result.elements.len(),
        result.total
    ));
    for el in &result.elements {
        out.push_str(&format!("  {} <{}> \"{}\"\n", el.r#ref, el.tag, el.name));
    }
    if result.elements.len() < result.total {
        out.push_str(&format!(
            "\n[truncated — {} more elements. Increase 'max_elements' or scroll.]\n",
            result.total - result.elements.len()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Rendering must include each element's ref + tag + name so the model
    /// can parse it deterministically across releases. The format is the
    /// stable surface other prompts assume.
    #[test]
    fn render_emits_one_line_per_element_with_ref_tag_name() {
        let result = SnapshotResult {
            elements: vec![
                SnapshotElement {
                    r#ref: "@e0".into(),
                    tag: "button".into(),
                    name: "Submit".into(),
                    bounds: ElementBounds {
                        x: 0.0,
                        y: 0.0,
                        width: 0.0,
                        height: 0.0,
                    },
                },
                SnapshotElement {
                    r#ref: "@e1".into(),
                    tag: "a".into(),
                    name: "Help".into(),
                    bounds: ElementBounds {
                        x: 0.0,
                        y: 0.0,
                        width: 0.0,
                        height: 0.0,
                    },
                },
            ],
            selectors: HashMap::new(),
            total: 2,
        };
        let text = render(&result);
        assert!(text.contains("@e0 <button> \"Submit\""));
        assert!(text.contains("@e1 <a> \"Help\""));
        assert!(text.contains("2 of 2"));
        // No truncation footer when nothing was dropped.
        assert!(!text.contains("truncated"));
    }

    /// When the page has more interactive elements than the snapshot cap,
    /// the rendered output must signal truncation so the model knows it
    /// isn't seeing everything. A silent truncation would have the model
    /// believing "there's nothing else" and skipping interactions.
    #[test]
    fn render_signals_truncation_when_total_exceeds_returned() {
        let result = SnapshotResult {
            elements: vec![SnapshotElement {
                r#ref: "@e0".into(),
                tag: "a".into(),
                name: "First".into(),
                bounds: ElementBounds {
                    x: 0.0,
                    y: 0.0,
                    width: 0.0,
                    height: 0.0,
                },
            }],
            selectors: HashMap::new(),
            total: 250,
        };
        let text = render(&result);
        assert!(text.contains("1 of 250"));
        assert!(
            text.contains("truncated"),
            "missing truncation marker: {text}"
        );
        assert!(text.contains("249 more"));
    }
}
