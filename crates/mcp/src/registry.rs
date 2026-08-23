//! Tool registry: owns the merged tool catalog across all MCP servers.
//!
//! The registry is what the cortex adapter queries each turn. It's
//! deliberately a *snapshot* type: the supervisor task updates it after
//! handshake / `tools/list` / `tools/list_changed`, and readers (the agent
//! loop) lock-and-clone briefly.
//!
//! Security responsibilities owned here:
//!
//! - Name sanitization (`mcp_<server>_<tool>`) with collision rejection.
//! - Schema gauntlet (size, root-type, `$ref`, property count).
//! - Per-server `tools.max` and global `mcp.max_tools_total` caps.
//! - Plan-mode classification (every tool defaults to mutating; operator
//!   opts in via `tools.read_only`).
//!
//! Supervisor lifecycle (spawn / reconnect / supervise) is intentionally
//! *not* in this module — it lives in `cortex::mcp_clients` so it can use
//! the cortex-side audit writer and `spawn_supervised`.

use crate::errors::SchemaRejectionReason;
use crate::manifest::ToolFilter;
use crate::protocol::McpToolSchema;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Acquire a write lock, logging once if the lock was poisoned and
/// recovering the inner state. A poison event means another thread
/// panicked while holding the lock — we surface that in the trace stream
/// so the operator can correlate it with whatever else broke (rather than
/// silently swallowing it).
fn write_recovering<'a, T>(lock: &'a RwLock<T>, name: &'static str) -> RwLockWriteGuard<'a, T> {
    match lock.write() {
        Ok(g) => g,
        Err(poisoned) => {
            tracing::warn!(lock = name, "mcp registry write lock poisoned; recovering");
            poisoned.into_inner()
        }
    }
}

/// Read-lock variant. Same recovery story as [`write_recovering`].
fn read_recovering<'a, T>(lock: &'a RwLock<T>, name: &'static str) -> RwLockReadGuard<'a, T> {
    match lock.read() {
        Ok(g) => g,
        Err(poisoned) => {
            tracing::warn!(lock = name, "mcp registry read lock poisoned; recovering");
            poisoned.into_inner()
        }
    }
}

/// Cap on a single tool's serialized schema. Above this, refuse to register.
///
/// Each tool's schema rides in the system prompt every turn — a 32 KB cap
/// keeps a single hostile server from monopolizing the context window.
pub const MAX_SCHEMA_BYTES: usize = 32 * 1024;

/// Cap on property count in a single tool's input schema. Defends against
/// adversarial schemas that try to balloon prompt size.
pub const MAX_PROPERTIES: usize = 64;

/// Global cap on the total number of MCP tools across all servers. Sized as
/// roughly four servers at the per-server [`ToolFilter::DEFAULT_MAX`] of 32;
/// past that, MCP schemas crowd out borg's own tools in the system prompt.
///
/// [`ToolFilter::DEFAULT_MAX`]: crate::manifest::ToolFilter::DEFAULT_MAX
pub const DEFAULT_GLOBAL_TOOL_CAP: usize = 128;

/// Lock names passed to the poison-recovering helpers; they appear verbatim
/// in the warning logged when a poisoned lock is recovered.
const INNER_LOCK: &str = "registry.inner";
const GLOBAL_CAP_LOCK: &str = "registry.global_cap";

/// One registered MCP tool, ready for the agent loop to dispatch.
#[derive(Debug, Clone)]
pub struct RegisteredTool {
    /// Namespaced name the LLM sees (`mcp_<server>_<tool>`).
    pub full_name: String,
    /// Server this tool belongs to.
    pub server: String,
    /// Original (pre-sanitization) tool name to forward to the server.
    pub original_name: String,
    /// Description forwarded to the LLM.
    pub description: Option<String>,
    /// Validated JSON schema for `inputSchema`, with provider-illegal
    /// property keys renamed (see [`sanitize_property_keys`]).
    pub input_schema: Value,
    /// Reverse map (sanitized → original) for property keys renamed in
    /// [`Self::input_schema`]. Applied to tool-call arguments at dispatch via
    /// [`unrename_tool_args`] so the server receives its original wire names.
    /// Empty for the common case of an already-conforming schema.
    pub arg_renames: BTreeMap<String, String>,
    /// True when the tool is parallel-safe to fan out with other read-only
    /// calls. Set by the operator via per-server `tools.read_only` config,
    /// OR — additively — by the server itself via
    /// `annotations.readOnlyHint: true`. Use this for batching decisions
    /// only. Plan-mode gating MUST use [`Self::operator_read_only`]
    /// because the hint is untrusted and Plan mode is a hard trust
    /// boundary.
    pub read_only: bool,
    /// True only when the operator explicitly opted this tool in via
    /// per-server `tools.read_only` config. Used as the Plan-mode trust
    /// gate so a malicious server cannot promote its mutating tool into
    /// Plan-mode visibility by self-reporting `readOnlyHint`.
    pub operator_read_only: bool,
}

/// Registry status for `borg mcp status`.
#[derive(Debug, Clone, Default)]
pub struct RegistryStatus {
    /// Per-server status entries.
    pub servers: BTreeMap<String, ServerStatus>,
    /// Total tools currently registered.
    pub total_tools: usize,
}

/// Per-server status.
#[derive(Debug, Clone)]
pub struct ServerStatus {
    /// Health snapshot.
    pub health: ServerHealth,
    /// Tools currently registered from this server.
    pub tool_count: usize,
    /// Reasons we rejected individual tools, for operator visibility.
    pub rejected_tools: Vec<(String, String)>,
    /// Last observed serverInfo line (`name version`).
    pub server_info: Option<String>,
}

/// Coarse health classification surfaced in `borg mcp status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerHealth {
    /// Connected and operating normally.
    Healthy,
    /// Tools are registered from a cached catalog while the live connection is
    /// still being established. Tool *calls* are refused until the session is
    /// live — the catalog is a hint about what will be available, not proof
    /// that it currently is.
    Starting,
    /// Connect failed or session dropped; supervisor is retrying.
    Reconnecting {
        /// Number of consecutive failures so far.
        attempts: u32,
    },
    /// Authentication failed — no further retries until the operator
    /// updates credentials.
    AuthFailed {
        /// Human-readable reason from the server.
        message: String,
    },
    /// Degraded: too many restarts in the window. Won't auto-retry.
    Degraded,
}

/// Shared registry state. Wrap in `Arc` for the cortex adapter; clone is
/// cheap.
#[derive(Clone, Default)]
pub struct McpRegistry {
    inner: Arc<RwLock<RegistryInner>>,
    /// Global cap. `Arc<RwLock>` keeps it adjustable at runtime without
    /// re-creating the registry.
    global_cap: Arc<RwLock<usize>>,
}

#[derive(Default)]
struct RegistryInner {
    /// All registered tools, keyed by namespaced name.
    tools: BTreeMap<String, RegisteredTool>,
    /// Per-server tool names (for collision detection and bulk-removal on
    /// disconnect / refresh).
    by_server: BTreeMap<String, Vec<String>>,
    /// Per-server status snapshots.
    status: BTreeMap<String, ServerStatus>,
}

impl McpRegistry {
    /// New empty registry with the default global tool cap.
    pub fn new() -> Self {
        Self::with_global_cap(DEFAULT_GLOBAL_TOOL_CAP)
    }

    /// New empty registry with a custom global cap.
    pub fn with_global_cap(cap: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(RegistryInner::default())),
            global_cap: Arc::new(RwLock::new(cap)),
        }
    }

    /// Replace the tools registered for `server` with the result of running
    /// `discovered` through the gauntlet. Returns the per-tool outcomes so
    /// the supervisor can audit rejections.
    pub fn replace_server_tools(
        &self,
        server: &str,
        discovered: &[McpToolSchema],
        filter: &ToolFilter,
        builtin_names: &[&str],
    ) -> Vec<ToolRegistrationOutcome> {
        let mut outcomes = Vec::new();
        let global_cap = *read_recovering(&self.global_cap, GLOBAL_CAP_LOCK);

        // Surface glob entries that matched nothing in the server's declared
        // catalog — an unmatched pattern used to silently match nothing, so a
        // filter meant to trim (or vet) a tool family actually did nothing.
        let declared: Vec<&str> = discovered.iter().map(|t| t.name.as_str()).collect();
        for (label, list) in [
            ("include", &filter.include),
            ("exclude", &filter.exclude),
            ("read_only", &filter.read_only),
        ] {
            for pattern in ToolFilter::unmatched_patterns(list, &declared) {
                tracing::warn!(
                    server = %server,
                    list = label,
                    pattern = %pattern,
                    "tools.{label} glob pattern matched no declared tool",
                );
            }
        }

        let mut inner = write_recovering(&self.inner, INNER_LOCK);

        // Drop existing entries for this server first — `tools/list_changed`
        // is a full re-declaration, not a diff.
        if let Some(existing) = inner.by_server.remove(server) {
            for name in existing {
                inner.tools.remove(&name);
            }
        }

        // Pre-pass: count how many *distinct* raw names in this batch sanitize
        // to each namespaced name. When two raw names (e.g. `read-file` and
        // `read_file`) collapse to one sanitized name, first-wins registration
        // would let the model call an ambiguous name that silently routes to
        // an arbitrarily-chosen handler. Fail closed instead: every member of
        // an ambiguous group is skipped, so the ambiguous name is never
        // registered at all. Filtered-out tools don't participate — they can
        // never register, so a lone admitted survivor is unambiguous.
        let mut sanitized_claims: BTreeMap<String, std::collections::BTreeSet<&str>> =
            BTreeMap::new();
        for tool in discovered {
            if !filter.admits(&tool.name) {
                continue;
            }
            if let Some(full) = sanitize_full_name(server, &tool.name) {
                sanitized_claims
                    .entry(full)
                    .or_default()
                    .insert(tool.name.as_str());
            }
        }

        // Track how many we've already accepted for this server in this call.
        let mut accepted_for_server: usize = 0;
        let mut registered_names: Vec<String> = Vec::new();

        for tool in discovered {
            if !filter.admits(&tool.name) {
                outcomes.push(ToolRegistrationOutcome::FilteredOut {
                    original: tool.name.clone(),
                });
                continue;
            }

            if accepted_for_server >= filter.max {
                outcomes.push(ToolRegistrationOutcome::Rejected {
                    original: tool.name.clone(),
                    reason: SchemaRejectionReason::PerServerCapReached,
                });
                continue;
            }

            let current_total: usize = inner.tools.len();
            if current_total + 1 > global_cap {
                outcomes.push(ToolRegistrationOutcome::Rejected {
                    original: tool.name.clone(),
                    reason: SchemaRejectionReason::GlobalCapReached,
                });
                continue;
            }

            let full_name = match sanitize_full_name(server, &tool.name) {
                Some(n) => n,
                None => {
                    outcomes.push(ToolRegistrationOutcome::Rejected {
                        original: tool.name.clone(),
                        reason: SchemaRejectionReason::EmptyName,
                    });
                    continue;
                }
            };

            let ambiguous_in_batch = sanitized_claims
                .get(&full_name)
                .is_some_and(|raw_names| raw_names.len() >= 2);
            if ambiguous_in_batch
                || builtin_names.iter().any(|b| *b == full_name)
                || inner.tools.contains_key(&full_name)
            {
                outcomes.push(ToolRegistrationOutcome::Collision {
                    original: tool.name.clone(),
                    full_name: full_name.clone(),
                });
                continue;
            }

            if let Err(reason) = validate_schema(&tool.input_schema) {
                outcomes.push(ToolRegistrationOutcome::Rejected {
                    original: tool.name.clone(),
                    reason,
                });
                continue;
            }

            // Rename provider-illegal property keys once, at registration —
            // one bad key anywhere in the tools array 400s the entire LLM
            // request. The reverse map restores wire names at dispatch.
            let mut input_schema = tool.input_schema.clone();
            let arg_renames = sanitize_property_keys(&mut input_schema, &tool.name);
            if !arg_renames.is_empty() {
                tracing::warn!(
                    server = %server,
                    tool = %tool.name,
                    renamed = arg_renames.len(),
                    "renamed provider-illegal property keys in tool schema \
                     (originals restored at dispatch)",
                );
            }

            // Read-only is true when either the operator opted in
            // (`tools.read_only`) OR the server advertised
            // `annotations.readOnlyHint: true`. The latter is untrusted,
            // but it only widens parallel-safe + Plan-mode-visible sets —
            // it never grants additional privileges, so trusting the
            // server's self-report here is acceptable. Operator config
            // remains the canonical opt-in for ambient tools that don't
            // ship the hint.
            let operator_read_only = filter.is_read_only(&tool.name);
            let advertised_read_only = tool
                .annotations
                .as_ref()
                .and_then(|a| a.read_only_hint)
                .unwrap_or(false);
            let read_only = operator_read_only || advertised_read_only;
            let registered = RegisteredTool {
                full_name: full_name.clone(),
                server: server.to_string(),
                original_name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema,
                arg_renames,
                read_only,
                operator_read_only,
            };
            inner.tools.insert(full_name.clone(), registered);
            registered_names.push(full_name.clone());
            accepted_for_server += 1;
            outcomes.push(ToolRegistrationOutcome::Registered { full_name });
        }

        inner.by_server.insert(server.to_string(), registered_names);

        let rejected_tools = outcomes
            .iter()
            .filter_map(|o| match o {
                ToolRegistrationOutcome::Rejected { original, reason } => {
                    Some((original.clone(), reason.to_string()))
                }
                ToolRegistrationOutcome::Collision {
                    original,
                    full_name,
                } => Some((original.clone(), format!("collision with {full_name}"))),
                ToolRegistrationOutcome::FilteredOut { .. }
                | ToolRegistrationOutcome::Registered { .. } => None,
            })
            .collect();

        let preserved_server_info = inner.status.get(server).and_then(|s| s.server_info.clone());
        inner.status.insert(
            server.to_string(),
            ServerStatus {
                health: ServerHealth::Healthy,
                tool_count: accepted_for_server,
                rejected_tools,
                server_info: preserved_server_info,
            },
        );

        outcomes
    }

    /// Remove all tools belonging to `server` (e.g. on disconnect /
    /// disable). Idempotent.
    pub fn drop_server(&self, server: &str) {
        let mut inner = write_recovering(&self.inner, INNER_LOCK);
        if let Some(names) = inner.by_server.remove(server) {
            for n in names {
                inner.tools.remove(&n);
            }
        }
        inner.status.remove(server);
    }

    /// Mark a server's health (without touching its tools — used when the
    /// session is reconnecting and we want to keep the catalog warm).
    pub fn set_health(&self, server: &str, health: ServerHealth) {
        let mut inner = write_recovering(&self.inner, INNER_LOCK);
        let entry = inner
            .status
            .entry(server.to_string())
            .or_insert_with(|| ServerStatus {
                health: health.clone(),
                tool_count: 0,
                rejected_tools: Vec::new(),
                server_info: None,
            });
        entry.health = health;
        // When a server is no longer Healthy, drop its tools — keeping stale
        // schemas in the LLM tool list would invite confusing failures.
        //
        // `Starting` is the exception: its tools come from a cached catalog and
        // are deliberately advertised before the session is live. Dropping them
        // here would defeat the cache entirely — the caller registers the
        // catalog and *then* marks the server starting.
        let drop_tools = !matches!(entry.health, ServerHealth::Healthy | ServerHealth::Starting);
        if drop_tools {
            if let Some(names) = inner.by_server.remove(server) {
                for n in names {
                    inner.tools.remove(&n);
                }
            }
            if let Some(s) = inner.status.get_mut(server) {
                s.tool_count = 0;
            }
        }
    }

    /// A single server's health, or `None` if no session has touched it.
    /// Cheaper than [`Self::status`] for the dispatch path, which asks about
    /// one server and would otherwise clone every server's status.
    pub fn health(&self, server: &str) -> Option<ServerHealth> {
        let inner = read_recovering(&self.inner, INNER_LOCK);
        inner.status.get(server).map(|s| s.health.clone())
    }

    /// Record server-info from a successful handshake.
    pub fn set_server_info(&self, server: &str, info: Option<String>) {
        let mut inner = write_recovering(&self.inner, INNER_LOCK);
        let entry = inner
            .status
            .entry(server.to_string())
            .or_insert_with(|| ServerStatus {
                health: ServerHealth::Healthy,
                tool_count: 0,
                rejected_tools: Vec::new(),
                server_info: None,
            });
        entry.server_info = info;
    }

    /// Snapshot of all currently-registered tools.
    pub fn tools(&self) -> Vec<RegisteredTool> {
        let inner = read_recovering(&self.inner, INNER_LOCK);
        inner.tools.values().cloned().collect()
    }

    /// Look up a tool by its namespaced name.
    pub fn lookup(&self, full_name: &str) -> Option<RegisteredTool> {
        let inner = read_recovering(&self.inner, INNER_LOCK);
        inner.tools.get(full_name).cloned()
    }

    /// Status snapshot for `borg mcp status`.
    pub fn status(&self) -> RegistryStatus {
        let inner = read_recovering(&self.inner, INNER_LOCK);
        RegistryStatus {
            servers: inner.status.clone(),
            total_tools: inner.tools.len(),
        }
    }

    /// Adjust the global cap at runtime.
    pub fn set_global_cap(&self, cap: usize) {
        *write_recovering(&self.global_cap, GLOBAL_CAP_LOCK) = cap;
    }
}

/// Per-tool outcome of [`McpRegistry::replace_server_tools`].
#[derive(Debug, Clone)]
pub enum ToolRegistrationOutcome {
    /// Tool was registered.
    Registered {
        /// Namespaced name.
        full_name: String,
    },
    /// Tool was filtered out by `include`/`exclude` config.
    FilteredOut {
        /// Original (pre-sanitization) tool name.
        original: String,
    },
    /// Tool was rejected by the schema/cap gauntlet.
    Rejected {
        /// Original tool name.
        original: String,
        /// Why it was rejected.
        reason: SchemaRejectionReason,
    },
    /// Tool's namespaced name collided with a builtin or another MCP tool.
    Collision {
        /// Original tool name.
        original: String,
        /// Namespaced name that collided.
        full_name: String,
    },
}

/// Build the namespaced tool name from a server name + MCP tool name.
///
/// Sanitization rules:
/// - Lowercase.
/// - Any non-`[a-z0-9_]` byte → `_`.
/// - Trailing/leading underscores in the *components* are preserved (we
///   only collapse runs of underscores within the joined name to keep
///   names readable).
///
/// The name is an opaque key, not a decodable encoding: dispatch is a map
/// lookup on the full name, and an over-long one is shortened with a hash
/// suffix that cannot be reversed to server + tool.
///
/// Returns `None` if the resulting name has empty server *or* tool
/// component. The result is always within [`MAX_TOOL_NAME_LEN`].
pub fn sanitize_full_name(server: &str, tool: &str) -> Option<String> {
    let s = sanitize_component(server);
    let t = sanitize_component(tool);
    if s.is_empty() || t.is_empty() {
        return None;
    }
    Some(shorten_tool_name(format!("mcp_{s}_{t}")))
}

/// Max length providers accept for a *tool* name (Anthropic's
/// `^[a-zA-Z0-9_-]{1,128}$`; OpenAI-compatible endpoints are no stricter).
///
/// Distinct from [`MAX_PROPERTY_KEY_LEN`], which is 64: the two limits are
/// different in the API and were not interchangeable here.
const MAX_TOOL_NAME_LEN: usize = 128;

/// Bound `name` to [`MAX_TOOL_NAME_LEN`], keeping distinct inputs distinct.
///
/// Without this the namespaced name is unbounded: `mcp_<server>_<tool>` over
/// the provider limit fails validation for the *whole* request, so one verbose
/// server takes every other tool — builtins included — down with it. The
/// failure is a 400 with no indication of which tool caused it.
///
/// A plain truncation is not enough. Two long names from the same server would
/// collapse onto one string, and the registry fails colliding names closed as
/// ambiguous, so *both* tools would vanish. Appending a hash of the
/// untruncated name keeps them apart.
///
/// The hash is FNV-1a rather than `DefaultHasher` because the rendered tools
/// array must stay byte-identical across processes and releases for the prompt
/// prefix cache. `truncate` is byte-safe here: [`sanitize_component`] emits
/// only ASCII, so every index is a char boundary.
fn shorten_tool_name(name: String) -> String {
    if name.len() <= MAX_TOOL_NAME_LEN {
        return name;
    }
    let suffix = format!("_{:08x}", fnv1a(name.as_bytes()));
    let mut out = name;
    out.truncate(MAX_TOOL_NAME_LEN.saturating_sub(suffix.len()));
    out.push_str(&suffix);
    out
}

/// FNV-1a, 32-bit. Chosen for cross-process stability, not collision
/// resistance — the names it disambiguates are not adversarial.
fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for b in bytes {
        hash ^= u32::from(*b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

fn sanitize_component(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

/// Max length providers accept for a schema property key (Anthropic's
/// `^[a-zA-Z0-9_.-]{1,64}$` pattern; OpenAI-compatible endpoints are no
/// stricter).
const MAX_PROPERTY_KEY_LEN: usize = 64;

/// True iff `key` already conforms to the provider property-key pattern
/// `^[a-zA-Z0-9_.-]{1,64}$`.
fn property_key_conforms(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_PROPERTY_KEY_LEN
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Rename provider-illegal property keys throughout `schema`, in place.
///
/// Some servers ship property keys that violate the provider pattern
/// (query-filter params like `issue_class~neq` or `meta.<field>[<op>]`).
/// One bad key anywhere in the tools array 400s the *entire* request at
/// provider validation, killing every tool with it. Renaming here keeps the
/// tool usable; [`unrename_tool_args`] restores the wire names at dispatch so
/// the server never sees the renamed form.
///
/// Rules, deterministic on both sides so the rendered tools array stays
/// byte-stable across assemblies (prompt-prefix-cache safe):
/// - every non-conforming char → `_`
/// - truncate to 64 chars
/// - collisions deduped with a numeric `_2`/`_3`… suffix in schema order
/// - `required` arrays remapped alongside their sibling `properties`, as are
///   `dependentRequired` (keys + value name lists) and `dependentSchemas`
///   (keys) property references
/// - nested schemas (`properties`, `items`, `additionalProperties`, …) are
///   walked recursively
///
/// Returns the reverse map (sanitized → original). Empty when the schema was
/// already clean — the schema is then byte-identical to its input.
///
/// `tool` is used only for log attribution when a remapped
/// `dependentRequired`/`dependentSchemas` key collides with an existing key.
pub fn sanitize_property_keys(schema: &mut Value, tool: &str) -> BTreeMap<String, String> {
    let mut renames = BTreeMap::new();
    // One `taken` set for the WHOLE schema, pre-seeded with every conforming
    // property key at every nesting level. `unrename_tool_args` applies the
    // reverse map to object keys at any depth, so a sanitized name must be
    // unique across the entire schema: a per-level set would let two levels
    // produce the same sanitized name for different originals (the second
    // silently overwriting the first in the map), or let a rename collide
    // with a legitimate conforming key elsewhere and rewrite that key's
    // arguments at dispatch.
    let mut taken = std::collections::BTreeSet::new();
    collect_conforming_property_keys(schema, &mut taken);
    sanitize_property_keys_inner(schema, &mut renames, &mut taken, tool);
    renames
}

/// Seed pass for [`sanitize_property_keys`]: every already-conforming
/// `properties` key, at every nesting level, is off-limits as a rename target.
fn collect_conforming_property_keys(value: &Value, taken: &mut std::collections::BTreeSet<String>) {
    match value {
        Value::Object(obj) => {
            if let Some(Value::Object(props)) = obj.get("properties") {
                for key in props.keys() {
                    if property_key_conforms(key) {
                        taken.insert(key.clone());
                    }
                }
            }
            for v in obj.values() {
                collect_conforming_property_keys(v, taken);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_conforming_property_keys(item, taken);
            }
        }
        _ => {}
    }
}

fn sanitize_property_keys_inner(
    value: &mut Value,
    renames: &mut BTreeMap<String, String>,
    taken: &mut std::collections::BTreeSet<String>,
    tool: &str,
) {
    let Value::Object(obj) = value else {
        // Arrays of schemas (`oneOf`, `anyOf`, tuple `items`) are handled by
        // the recursion below via their owning object.
        if let Value::Array(items) = value {
            for item in items {
                sanitize_property_keys_inner(item, renames, taken, tool);
            }
        }
        return;
    };

    // Rename this level's `properties` keys, tracking old → new for the
    // sibling `required` remap.
    let mut local: Vec<(String, String)> = Vec::new();
    if let Some(Value::Object(props)) = obj.get_mut("properties") {
        let needs_rename = props.keys().any(|k| !property_key_conforms(k));
        if needs_rename {
            let entries: Vec<(String, Value)> = std::mem::take(props).into_iter().collect();
            for (key, val) in entries {
                if property_key_conforms(&key) {
                    props.insert(key, val);
                    continue;
                }
                let renamed = dedupe_key(&sanitize_property_key(&key), taken);
                renames.insert(renamed.clone(), key.clone());
                local.push((key, renamed.clone()));
                props.insert(renamed, val);
            }
        }
    }
    if !local.is_empty() {
        if let Some(Value::Array(required)) = obj.get_mut("required") {
            for entry in required {
                if let Value::String(name) = entry {
                    if let Some((_, renamed)) = local.iter().find(|(old, _)| old == name) {
                        *name = renamed.clone();
                    }
                }
            }
        }

        // `dependentRequired` / `dependentSchemas` reference sibling property
        // names too — a rename that skips them leaves keywords pointing at
        // names that no longer exist (the provider then 400s or silently
        // drops the constraint). `dependentRequired` keys AND the names in
        // its value lists are property references; its values are string
        // lists, NOT schemas, so they must not be recursed into as schemas.
        // `dependentSchemas` keys are property references; its values are
        // schemas and are covered by the generic recursion below.
        let remap = |name: &str| -> Option<String> {
            local
                .iter()
                .find(|(old, _)| old == name)
                .map(|(_, renamed)| renamed.clone())
        };
        if let Some(Value::Object(dep_req)) = obj.get_mut("dependentRequired") {
            let entries: Vec<(String, Value)> = std::mem::take(dep_req).into_iter().collect();
            for (key, mut val) in entries {
                if let Value::Array(names) = &mut val {
                    for entry in names {
                        if let Value::String(name) = entry {
                            if let Some(renamed) = remap(name) {
                                *name = renamed;
                            }
                        }
                    }
                }
                let key = remap(&key).unwrap_or(key);
                if dep_req.contains_key(&key) {
                    // A remapped key landing on an existing one loses a
                    // constraint — surface it rather than dropping silently.
                    tracing::warn!(
                        tool = %tool,
                        key = %key,
                        "dependentRequired key remap collided with an existing key; \
                         the earlier entry is overwritten",
                    );
                }
                dep_req.insert(key, val);
            }
        }
        if let Some(Value::Object(dep_schemas)) = obj.get_mut("dependentSchemas") {
            let entries: Vec<(String, Value)> = std::mem::take(dep_schemas).into_iter().collect();
            for (key, val) in entries {
                let key = remap(&key).unwrap_or(key);
                if dep_schemas.contains_key(&key) {
                    tracing::warn!(
                        tool = %tool,
                        key = %key,
                        "dependentSchemas key remap collided with an existing key; \
                         the earlier entry is overwritten",
                    );
                }
                dep_schemas.insert(key, val);
            }
        }
    }

    // Recurse into every nested value. This covers `properties` values,
    // `items`, `additionalProperties`, `oneOf`/`anyOf`/`allOf`, `$defs`, and
    // anything else carrying sub-schemas — renaming only ever happens at a
    // `properties` map, so visiting non-schema values is harmless.
    for (_, v) in obj.iter_mut() {
        sanitize_property_keys_inner(v, renames, taken, tool);
    }
}

/// Map a non-conforming key onto the provider alphabet.
fn sanitize_property_key(key: &str) -> String {
    let mut out: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    out.truncate(MAX_PROPERTY_KEY_LEN);
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// Resolve collisions against already-taken keys with a numeric suffix,
/// truncating the stem so the result stays within the length cap.
fn dedupe_key(candidate: &str, taken: &mut std::collections::BTreeSet<String>) -> String {
    if !taken.contains(candidate) {
        taken.insert(candidate.to_string());
        return candidate.to_string();
    }
    for n in 2usize.. {
        let suffix = format!("_{n}");
        let stem_len = MAX_PROPERTY_KEY_LEN.saturating_sub(suffix.len());
        let mut attempt = candidate.to_string();
        attempt.truncate(stem_len);
        attempt.push_str(&suffix);
        if !taken.contains(&attempt) {
            taken.insert(attempt.clone());
            return attempt;
        }
    }
    unreachable!("dedupe_key: unbounded suffix search always terminates")
}

/// Restore original wire names in tool-call `args` before they reach the MCP
/// server, reversing [`sanitize_property_keys`]. Applies the map to every
/// object key at any depth and recurses into array items — the model only
/// ever sees the renamed schema, so any occurrence of a renamed key in its
/// arguments refers to the original.
pub fn unrename_tool_args(args: &mut Value, renames: &BTreeMap<String, String>) {
    if renames.is_empty() {
        return;
    }
    match args {
        Value::Object(obj) => {
            let entries: Vec<(String, Value)> = std::mem::take(obj).into_iter().collect();
            for (key, mut val) in entries {
                unrename_tool_args(&mut val, renames);
                let restored = renames.get(&key).cloned().unwrap_or(key);
                obj.insert(restored, val);
            }
        }
        Value::Array(items) => {
            for item in items {
                unrename_tool_args(item, renames);
            }
        }
        _ => {}
    }
}

/// Run an MCP tool's `inputSchema` through the validation gauntlet.
///
/// Returns `Err` with a [`SchemaRejectionReason`] if the schema is rejected;
/// `Ok(())` if it passes.
pub fn validate_schema(schema: &Value) -> std::result::Result<(), SchemaRejectionReason> {
    // `serde_json::to_string(&Value)` is infallible on the current `Value`
    // type — re-serialization can't fail. If that ever changes (e.g. a swap
    // to `RawValue`), treat the failure as "too large to size-check safely"
    // and refuse rather than waving the schema through with bytes=0.
    let serialized = match serde_json::to_string(schema) {
        Ok(s) => s,
        Err(_) => {
            return Err(SchemaRejectionReason::TooLarge { bytes: usize::MAX });
        }
    };
    if serialized.len() > MAX_SCHEMA_BYTES {
        return Err(SchemaRejectionReason::TooLarge {
            bytes: serialized.len(),
        });
    }
    if contains_ref(schema) {
        return Err(SchemaRejectionReason::ContainsRef);
    }
    let obj = match schema {
        Value::Object(o) => o,
        _ => return Err(SchemaRejectionReason::NotObjectRoot),
    };
    match obj.get("type") {
        Some(Value::String(t)) if t == "object" => {}
        _ => return Err(SchemaRejectionReason::NotObjectRoot),
    }
    if let Some(Value::Object(props)) = obj.get("properties") {
        if props.len() > MAX_PROPERTIES {
            return Err(SchemaRejectionReason::TooManyProperties { count: props.len() });
        }
    }
    Ok(())
}

fn contains_ref(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            if map.contains_key("$ref") {
                return true;
            }
            map.values().any(contains_ref)
        }
        Value::Array(items) => items.iter().any(contains_ref),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema_with_props(n: usize) -> Value {
        let mut props = serde_json::Map::new();
        for i in 0..n {
            props.insert(format!("p{i}"), json!({"type": "string"}));
        }
        json!({"type": "object", "properties": props})
    }

    #[test]
    fn sanitize_namespaces_with_mcp_prefix() {
        assert_eq!(
            sanitize_full_name("github", "create_issue").as_deref(),
            Some("mcp_github_create_issue")
        );
        // Mixed case + punctuation gets normalized but information preserved.
        assert_eq!(
            sanitize_full_name("My-API", "list.items.v2").as_deref(),
            Some("mcp_my_api_list_items_v2")
        );
    }

    /// Spec: a namespaced name must never exceed the provider's 128-byte tool
    /// name limit. An over-long name is not merely cosmetic — the provider
    /// rejects the entire request, so every other tool in the array dies with
    /// it. Real-world trigger: an MCP server that names its tools after full
    /// API operation IDs.
    #[test]
    fn long_names_are_bounded_to_the_provider_limit() {
        let name = sanitize_full_name("server", &"a".repeat(400)).expect("non-empty components");
        assert!(
            name.len() <= MAX_TOOL_NAME_LEN,
            "name must fit the provider limit, got {} bytes: {name}",
            name.len()
        );
    }

    /// Spec: shortening must not merge two distinct tools. The registry fails
    /// colliding sanitized names closed as ambiguous, so a plain truncation
    /// would delete *both* tools rather than one — the hash suffix is what
    /// keeps them registrable.
    #[test]
    fn distinct_long_names_stay_distinct_after_shortening() {
        let stem = "z".repeat(300);
        let a = sanitize_full_name("server", &format!("{stem}_alpha")).unwrap();
        let b = sanitize_full_name("server", &format!("{stem}_beta")).unwrap();
        assert_ne!(a, b, "truncation collapsed two tools onto one name");
        assert!(a.len() <= MAX_TOOL_NAME_LEN && b.len() <= MAX_TOOL_NAME_LEN);
    }

    /// Spec: the rendered tools array must be byte-stable across processes so
    /// the prompt prefix cache survives a daemon restart. That rules out
    /// `DefaultHasher`, whose output is not guaranteed stable; this pins the
    /// actual emitted bytes so a swap to a non-portable hasher fails here.
    #[test]
    fn shortened_names_are_stable_across_runs() {
        // Literal bytes, not a value recomputed with `fnv1a` — comparing the
        // function against itself would still pass if the hash were swapped
        // for a non-portable one, which is the regression this guards.
        let long = sanitize_full_name("srv", &"q".repeat(200)).unwrap();
        assert_eq!(long, format!("mcp_srv_{}_e21d1e0a", "q".repeat(119 - 8)));
    }

    /// Spec: `fnv1a` is the published FNV-1a/32 function, pinned by its
    /// reference vectors. It is an on-the-wire format, so a "cleanup" that
    /// changed the offset basis or prime would silently renumber every
    /// shortened tool name and cold-start the prompt cache.
    #[test]
    fn fnv1a_matches_the_reference_vectors() {
        assert_eq!(fnv1a(b""), 0x811c_9dc5);
        assert_eq!(fnv1a(b"a"), 0xe40c_292c);
        assert_eq!(fnv1a(b"foobar"), 0xbf9c_f968);
    }

    /// Spec: names that already fit are passed through untouched. A hash
    /// suffix on an ordinary name would be pure churn and would break every
    /// existing per-tool allow/deny filter the operator has configured.
    #[test]
    fn names_within_the_limit_are_untouched() {
        assert_eq!(
            sanitize_full_name("github", "create_issue").as_deref(),
            Some("mcp_github_create_issue")
        );
        let exact = "b".repeat(MAX_TOOL_NAME_LEN - "mcp_s_".len());
        let name = sanitize_full_name("s", &exact).unwrap();
        assert_eq!(
            name.len(),
            MAX_TOOL_NAME_LEN,
            "boundary must not be shortened"
        );
        assert_eq!(name, format!("mcp_s_{exact}"));
    }

    #[test]
    fn sanitize_rejects_empty_components() {
        assert!(sanitize_full_name("", "tool").is_none());
        assert!(sanitize_full_name("server", "").is_none());
        assert!(sanitize_full_name("---", "tool").is_none());
    }

    #[test]
    fn schema_gauntlet_rejects_non_object_root() {
        // Spec: every MCP tool input must be a JSON object at the root.
        // Anything else is rejected.
        assert!(matches!(
            validate_schema(&json!("not-an-object")).unwrap_err(),
            SchemaRejectionReason::NotObjectRoot
        ));
        assert!(matches!(
            validate_schema(&json!({"type": "string"})).unwrap_err(),
            SchemaRejectionReason::NotObjectRoot
        ));
    }

    #[test]
    fn schema_gauntlet_rejects_ref_at_any_depth() {
        // Spec: we never resolve `$ref` (external-ref attack surface).
        let nested = json!({
            "type": "object",
            "properties": {
                "inner": { "$ref": "#/definitions/foo" }
            }
        });
        assert!(matches!(
            validate_schema(&nested).unwrap_err(),
            SchemaRejectionReason::ContainsRef
        ));
    }

    #[test]
    fn schema_gauntlet_rejects_too_many_properties() {
        let s = schema_with_props(MAX_PROPERTIES + 1);
        let err = validate_schema(&s).unwrap_err();
        assert!(matches!(
            err,
            SchemaRejectionReason::TooManyProperties { count } if count == MAX_PROPERTIES + 1
        ));
    }

    #[test]
    fn schema_gauntlet_rejects_oversize() {
        // A schema with one giant description string crosses the byte cap.
        let s = json!({
            "type": "object",
            "description": "x".repeat(MAX_SCHEMA_BYTES + 1)
        });
        let err = validate_schema(&s).unwrap_err();
        assert!(matches!(err, SchemaRejectionReason::TooLarge { .. }));
    }

    /// Regression for the silent-swallow bug: `validate_schema` used to
    /// call `serde_json::to_string(...).unwrap_or_default()`, which
    /// would collapse any serialization failure to an empty string and
    /// then *pass* the size check on `bytes == 0`. `serde_json::Value`
    /// can't actually fail to re-serialize, so the bug was latent — but
    /// the gauntlet must still report the correct size for a *real*
    /// oversize schema rather than masking it. This test pins that the
    /// reported `bytes` reflects the actual serialized length, not the
    /// fallback default. A regression that re-introduces the swallow
    /// would either return `bytes == 0` (gauntlet bypassed) or skip the
    /// reject entirely.
    #[test]
    fn schema_gauntlet_reports_real_byte_count_on_oversize() {
        let payload = "x".repeat(MAX_SCHEMA_BYTES + 100);
        let s = json!({ "type": "object", "description": payload });
        let err = validate_schema(&s).unwrap_err();
        match err {
            SchemaRejectionReason::TooLarge { bytes } => {
                assert!(
                    bytes > MAX_SCHEMA_BYTES,
                    "reported {bytes} bytes must exceed cap {MAX_SCHEMA_BYTES}; \
                     a bytes==0 result would mean the size check was bypassed"
                );
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn schema_gauntlet_admits_normal_tool() {
        let s = json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"]
        });
        validate_schema(&s).unwrap();
    }

    /// `Starting` means "tools known from cache, session not live yet". Health
    /// changes drop a server's tools precisely so stale schemas can't linger —
    /// but applying that to `Starting` would erase the catalog the cache just
    /// registered, silently defeating it. Every other non-healthy state must
    /// still drop.
    #[test]
    fn set_health_keeps_tools_only_for_healthy_and_starting() {
        let tools = vec![McpToolSchema {
            name: "read".into(),
            description: None,
            input_schema: json!({"type": "object"}),
            annotations: None,
        }];

        for (health, expect_kept) in [
            (ServerHealth::Healthy, true),
            (ServerHealth::Starting, true),
            (ServerHealth::Reconnecting { attempts: 1 }, false),
            (
                ServerHealth::AuthFailed {
                    message: "401".into(),
                },
                false,
            ),
            (ServerHealth::Degraded, false),
        ] {
            let reg = McpRegistry::new();
            reg.replace_server_tools("files", &tools, &ToolFilter::default(), &[]);
            assert!(!reg.tools().is_empty(), "precondition: tool registered");

            reg.set_health("files", health.clone());

            assert_eq!(
                !reg.tools().is_empty(),
                expect_kept,
                "{health:?}: tools kept should be {expect_kept}"
            );
            assert_eq!(reg.health("files"), Some(health));
        }
    }

    #[test]
    fn replace_server_tools_namespaces_and_filters() {
        let reg = McpRegistry::new();
        let tools = vec![
            McpToolSchema {
                name: "search".into(),
                description: Some("search".into()),
                input_schema: json!({"type": "object", "properties": {"q": {"type": "string"}}}),
                annotations: None,
            },
            McpToolSchema {
                name: "delete".into(),
                description: None,
                input_schema: json!({"type": "object"}),
                annotations: None,
            },
        ];
        let filter = ToolFilter {
            exclude: vec!["delete".into()],
            read_only: vec!["search".into()],
            ..Default::default()
        };
        let outcomes = reg.replace_server_tools("github", &tools, &filter, &[]);

        // search: registered as mcp_github_search, read_only=true.
        let names: Vec<_> = reg.tools().into_iter().map(|t| t.full_name).collect();
        assert_eq!(names, vec!["mcp_github_search"]);
        let entry = reg.lookup("mcp_github_search").unwrap();
        assert!(entry.read_only, "operator-marked read-only must propagate");
        // delete: filtered out.
        assert!(outcomes
            .iter()
            .any(|o| matches!(o, ToolRegistrationOutcome::FilteredOut { original } if original == "delete")));
    }

    /// Spec: two over-long tool names from one server must both survive
    /// registration. This is the behaviour the hash suffix exists for — under
    /// a plain truncation the two names collapse onto one string, the registry
    /// fails the second closed as ambiguous, and the operator loses *both*
    /// tools with only a log line to say so.
    #[test]
    fn two_over_long_tool_names_both_register() {
        let reg = McpRegistry::new();
        // Identical for the first 300 bytes; they differ only past the cut.
        let shared = "x".repeat(300);
        let tools = vec![
            McpToolSchema {
                name: format!("{shared}_alpha"),
                description: None,
                input_schema: json!({"type": "object"}),
                annotations: None,
            },
            McpToolSchema {
                name: format!("{shared}_beta"),
                description: None,
                input_schema: json!({"type": "object"}),
                annotations: None,
            },
        ];
        let outcomes = reg.replace_server_tools("srv", &tools, &ToolFilter::default(), &[]);
        assert!(
            !outcomes
                .iter()
                .any(|o| matches!(o, ToolRegistrationOutcome::Collision { .. })),
            "shortening must not manufacture a collision: {outcomes:?}"
        );

        let names: Vec<String> = reg.tools().into_iter().map(|t| t.full_name).collect();
        assert_eq!(names.len(), 2, "both tools must remain callable: {names:?}");
        assert_ne!(names[0], names[1]);
        for name in &names {
            assert!(
                name.len() <= MAX_TOOL_NAME_LEN,
                "{name} exceeds the provider limit"
            );
            assert!(reg.lookup(name).is_some(), "{name} must be dispatchable");
        }
    }

    #[test]
    fn collision_with_builtin_is_rejected() {
        let reg = McpRegistry::new();
        let tools = vec![McpToolSchema {
            name: "fetch".into(),
            description: None,
            input_schema: json!({"type": "object"}),
            annotations: None,
        }];
        let outcomes =
            reg.replace_server_tools("web", &tools, &ToolFilter::default(), &["mcp_web_fetch"]);
        assert!(outcomes
            .iter()
            .any(|o| matches!(o, ToolRegistrationOutcome::Collision { .. })));
        assert!(reg.tools().is_empty());
    }

    /// Confused-deputy guard: when two distinct raw names from the same
    /// server sanitize to one registry name, the model would be calling an
    /// ambiguous name that routes to an arbitrarily-chosen handler. The
    /// whole ambiguous group must fail closed — no member registers, every
    /// member reports a collision — while unique names in the same batch
    /// register normally.
    #[test]
    fn ambiguous_sanitized_names_fail_closed_both_ways() {
        struct Case {
            name: &'static str,
            raw_names: &'static [&'static str],
            expect_registered: &'static [&'static str],
            expect_collisions: &'static [&'static str],
        }
        let cases = [
            Case {
                name: "two raw names collapse to one sanitized name",
                raw_names: &["read-file", "read_file"],
                expect_registered: &[],
                expect_collisions: &["read-file", "read_file"],
            },
            Case {
                name: "three-way collapse skips all members",
                raw_names: &["read-file", "read_file", "read.file"],
                expect_registered: &[],
                expect_collisions: &["read-file", "read_file", "read.file"],
            },
            Case {
                name: "control: unique names all register",
                raw_names: &["read_file", "write_file"],
                expect_registered: &["mcp_fs_read_file", "mcp_fs_write_file"],
                expect_collisions: &[],
            },
            Case {
                name: "ambiguous group does not poison unrelated unique name",
                raw_names: &["read-file", "read_file", "write_file"],
                expect_registered: &["mcp_fs_write_file"],
                expect_collisions: &["read-file", "read_file"],
            },
        ];
        for case in cases {
            let reg = McpRegistry::new();
            let tools: Vec<McpToolSchema> = case
                .raw_names
                .iter()
                .map(|n| McpToolSchema {
                    name: (*n).into(),
                    description: None,
                    input_schema: json!({"type": "object"}),
                    annotations: None,
                })
                .collect();
            let outcomes = reg.replace_server_tools("fs", &tools, &ToolFilter::default(), &[]);

            let registered: Vec<_> = reg.tools().into_iter().map(|t| t.full_name).collect();
            assert_eq!(
                registered, case.expect_registered,
                "case `{}`: registered set mismatch",
                case.name
            );
            for raw in case.expect_collisions {
                assert!(
                    outcomes.iter().any(|o| matches!(
                        o,
                        ToolRegistrationOutcome::Collision { original, .. } if original == raw
                    )),
                    "case `{}`: expected collision outcome for `{raw}`, got {outcomes:?}",
                    case.name
                );
            }
        }
    }

    #[test]
    fn replace_full_drops_previous() {
        // tools/list_changed semantics: a fresh list fully replaces the
        // previous declaration. Stale tools must not linger in the catalog.
        let reg = McpRegistry::new();
        reg.replace_server_tools(
            "x",
            &[McpToolSchema {
                name: "a".into(),
                description: None,
                input_schema: json!({"type": "object"}),
                annotations: None,
            }],
            &ToolFilter::default(),
            &[],
        );
        reg.replace_server_tools(
            "x",
            &[McpToolSchema {
                name: "b".into(),
                description: None,
                input_schema: json!({"type": "object"}),
                annotations: None,
            }],
            &ToolFilter::default(),
            &[],
        );
        let names: Vec<_> = reg.tools().into_iter().map(|t| t.full_name).collect();
        assert_eq!(names, vec!["mcp_x_b"]);
    }

    #[test]
    fn per_server_cap_enforced() {
        let reg = McpRegistry::new();
        let tools: Vec<_> = (0..5)
            .map(|i| McpToolSchema {
                name: format!("t{i}"),
                description: None,
                input_schema: json!({"type": "object"}),
                annotations: None,
            })
            .collect();
        let filter = ToolFilter {
            max: 3,
            ..Default::default()
        };
        let outcomes = reg.replace_server_tools("s", &tools, &filter, &[]);
        let accepted = outcomes
            .iter()
            .filter(|o| matches!(o, ToolRegistrationOutcome::Registered { .. }))
            .count();
        let cap_rejections = outcomes
            .iter()
            .filter(|o| {
                matches!(
                    o,
                    ToolRegistrationOutcome::Rejected {
                        reason: SchemaRejectionReason::PerServerCapReached,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(accepted, 3);
        assert_eq!(cap_rejections, 2);
    }

    #[test]
    fn unhealthy_server_drops_its_tools() {
        // Stale schemas in the LLM tool list lead to confusing failures.
        // Spec: any non-Healthy state purges the tools.
        let reg = McpRegistry::new();
        reg.replace_server_tools(
            "x",
            &[McpToolSchema {
                name: "t".into(),
                description: None,
                input_schema: json!({"type": "object"}),
                annotations: None,
            }],
            &ToolFilter::default(),
            &[],
        );
        assert_eq!(reg.tools().len(), 1);
        reg.set_health("x", ServerHealth::Reconnecting { attempts: 1 });
        assert!(reg.tools().is_empty());
    }

    #[test]
    fn server_advertised_read_only_hint_widens_parallel_only_not_plan_mode() {
        // Spec: a server's `readOnlyHint: true` flips `read_only` (the
        // batching-safe flag) but MUST NOT flip `operator_read_only` (the
        // Plan-mode gate). The hint is untrusted; Plan mode requires
        // explicit operator opt-in via per-server `tools.read_only`.
        use crate::protocol::ToolAnnotations;
        let reg = McpRegistry::new();
        let tools = vec![McpToolSchema {
            name: "search".into(),
            description: None,
            input_schema: json!({"type": "object"}),
            annotations: Some(ToolAnnotations {
                read_only_hint: Some(true),
            }),
        }];
        reg.replace_server_tools("srv", &tools, &ToolFilter::default(), &[]);
        let entry = reg.lookup("mcp_srv_search").unwrap();
        assert!(
            entry.read_only,
            "server-advertised readOnlyHint must widen parallel-safe set"
        );
        assert!(
            !entry.operator_read_only,
            "untrusted hint must NOT widen the Plan-mode trust boundary"
        );
    }

    #[test]
    fn operator_opt_in_sets_both_read_only_flags() {
        // Inverse spec: explicit operator config flips both flags so the
        // tool is parallel-safe AND Plan-mode-visible.
        let reg = McpRegistry::new();
        let tools = vec![McpToolSchema {
            name: "search".into(),
            description: None,
            input_schema: json!({"type": "object"}),
            annotations: None,
        }];
        let filter = ToolFilter {
            read_only: vec!["search".into()],
            ..Default::default()
        };
        reg.replace_server_tools("srv", &tools, &filter, &[]);
        let entry = reg.lookup("mcp_srv_search").unwrap();
        assert!(entry.read_only, "operator opt-in must set read_only");
        assert!(
            entry.operator_read_only,
            "operator opt-in must set operator_read_only"
        );
    }

    #[test]
    fn missing_read_only_hint_does_not_flip_read_only() {
        // Negative guard: an unannotated tool stays mutating-by-default.
        // Without this, the planner would batch unknown MCP calls and a
        // misbehaving server's mutating tool would run concurrently with
        // peers.
        let reg = McpRegistry::new();
        let tools = vec![McpToolSchema {
            name: "delete".into(),
            description: None,
            input_schema: json!({"type": "object"}),
            annotations: None,
        }];
        reg.replace_server_tools("srv", &tools, &ToolFilter::default(), &[]);
        let entry = reg.lookup("mcp_srv_delete").unwrap();
        assert!(
            !entry.read_only,
            "tool without readOnlyHint must remain mutating-by-default"
        );
    }

    // ── Property-key sanitization ──

    #[test]
    fn sanitize_property_keys_renames_illegal_chars_and_remaps_required() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "issue_class~neq": {"type": "string"},
                "meta.field[gt]": {"type": "number"},
                "has space": {"type": "boolean"},
                "clean_key": {"type": "string"},
            },
            "required": ["issue_class~neq", "clean_key"],
        });
        let renames = sanitize_property_keys(&mut schema, "test_tool");

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("issue_class_neq"));
        assert!(props.contains_key("meta.field_gt_"));
        assert!(props.contains_key("has_space"));
        assert!(props.contains_key("clean_key"), "clean keys untouched");

        // Reverse map restores originals.
        assert_eq!(renames["issue_class_neq"], "issue_class~neq");
        assert_eq!(renames["meta.field_gt_"], "meta.field[gt]");
        assert_eq!(renames["has_space"], "has space");
        assert_eq!(renames.len(), 3, "clean key must not be mapped");

        // `required` follows the rename.
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, ["issue_class_neq", "clean_key"]);
    }

    #[test]
    fn sanitize_property_keys_truncates_and_dedupes_deterministically() {
        let long_a = format!("{}~a", "x".repeat(70));
        let long_b = format!("{}~b", "x".repeat(70));
        let mut schema = json!({
            "type": "object",
            "properties": {
                long_a.clone(): {"type": "string"},
                long_b.clone(): {"type": "string"},
                "key!": {"type": "string"},
                "key?": {"type": "string"},
            },
        });
        let renames = sanitize_property_keys(&mut schema, "test_tool");
        let props = schema["properties"].as_object().unwrap();

        // Both long keys truncate to the same 64-char stem; the collision is
        // deduped with a numeric suffix and every key stays within the cap.
        for key in props.keys() {
            assert!(key.len() <= 64, "key '{key}' exceeds 64 chars");
        }
        assert_eq!(props.len(), 4, "no keys lost to collisions");
        assert_eq!(renames.len(), 4);

        // Determinism: sanitizing the same input again yields identical output.
        let mut schema2 = json!({
            "type": "object",
            "properties": {
                long_a: {"type": "string"},
                long_b: {"type": "string"},
                "key!": {"type": "string"},
                "key?": {"type": "string"},
            },
        });
        let renames2 = sanitize_property_keys(&mut schema2, "test_tool");
        assert_eq!(schema, schema2);
        assert_eq!(renames, renames2);
    }

    #[test]
    fn sanitize_property_keys_recurses_into_nested_schemas() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "filters": {
                    "type": "object",
                    "properties": {"nested~key": {"type": "string"}},
                    "required": ["nested~key"],
                },
                "list": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {"item[0]": {"type": "string"}},
                    },
                },
            },
        });
        let renames = sanitize_property_keys(&mut schema, "test_tool");
        assert!(schema["properties"]["filters"]["properties"]
            .as_object()
            .unwrap()
            .contains_key("nested_key"));
        assert_eq!(
            schema["properties"]["filters"]["required"][0],
            json!("nested_key")
        );
        assert!(schema["properties"]["list"]["items"]["properties"]
            .as_object()
            .unwrap()
            .contains_key("item_0_"));
        assert_eq!(renames.len(), 2);
    }

    #[test]
    fn sanitize_property_keys_cross_level_collisions_stay_globally_unique() {
        // Two failure modes of a per-level dedupe scope, both must be pinned:
        // (1) `a~x` (top) and `a!x` (nested) sanitize to the same stem — the
        //     second must take a suffix, or the reverse map silently maps one
        //     sanitized name to two originals and restores the wrong one;
        // (2) `b~y` (top) sanitizes to `b_y`, which is a LEGITIMATE conforming
        //     key in the nested schema — the rename must dodge it, or
        //     `unrename_tool_args` (applied at every depth) rewrites the
        //     nested legit arg to an original it never was.
        let mut schema = json!({
            "type": "object",
            "properties": {
                "a~x": {"type": "string"},
                "b~y": {"type": "string"},
                "nested": {
                    "type": "object",
                    "properties": {
                        "a!x": {"type": "integer"},
                        "b_y": {"type": "integer"},
                    },
                },
            },
        });
        let renames = sanitize_property_keys(&mut schema, "test_tool");

        let top = schema["properties"].as_object().unwrap();
        let nested = schema["properties"]["nested"]["properties"]
            .as_object()
            .unwrap();
        assert!(top.contains_key("a_x"));
        assert!(top.contains_key("b_y_2"), "must dodge the nested legit b_y");
        assert!(nested.contains_key("a_x_2"), "cross-level stem collision");
        assert!(nested.contains_key("b_y"), "legit key untouched");

        assert_eq!(renames["a_x"], "a~x");
        assert_eq!(renames["a_x_2"], "a!x");
        assert_eq!(renames["b_y_2"], "b~y");

        // Round-trip: every level restores its own original, and the nested
        // legitimate `b_y` survives unrenamed.
        let mut args = json!({
            "a_x": "top",
            "b_y_2": "renamed-top",
            "nested": {"a_x_2": 1, "b_y": 2},
        });
        unrename_tool_args(&mut args, &renames);
        assert_eq!(
            args,
            json!({
                "a~x": "top",
                "b~y": "renamed-top",
                "nested": {"a!x": 1, "b_y": 2},
            })
        );
    }

    /// JSON Schema 2020-12 `dependentRequired` and `dependentSchemas`
    /// reference property names; after a rename those references must be
    /// remapped or the provider 400s / silently drops the constraint.
    /// `dependentRequired` values are string lists (NOT schemas) and must
    /// be remapped as name references, not recursed into.
    #[test]
    fn sanitize_property_keys_remaps_dependent_keywords() {
        struct Case {
            name: &'static str,
            input: Value,
            expected: Value,
        }
        let cases = [
            Case {
                name: "dependentRequired keys and value lists follow the rename",
                input: json!({
                    "type": "object",
                    "properties": {
                        "credit~card": {"type": "string"},
                        "billing_address": {"type": "string"}
                    },
                    "dependentRequired": {
                        "credit~card": ["billing_address"],
                        "billing_address": ["credit~card"]
                    }
                }),
                expected: json!({
                    "type": "object",
                    "properties": {
                        "credit_card": {"type": "string"},
                        "billing_address": {"type": "string"}
                    },
                    "dependentRequired": {
                        "credit_card": ["billing_address"],
                        "billing_address": ["credit_card"]
                    }
                }),
            },
            Case {
                name: "dependentSchemas keys follow the rename; values recurse as schemas",
                input: json!({
                    "type": "object",
                    "properties": {
                        "issue~class": {"type": "string"}
                    },
                    "dependentSchemas": {
                        "issue~class": {
                            "properties": {"nested~bad": {"type": "string"}},
                            "required": ["nested~bad"]
                        }
                    }
                }),
                expected: json!({
                    "type": "object",
                    "properties": {
                        "issue_class": {"type": "string"}
                    },
                    "dependentSchemas": {
                        "issue_class": {
                            "properties": {"nested_bad": {"type": "string"}},
                            "required": ["nested_bad"]
                        }
                    }
                }),
            },
            Case {
                name: "dependent entries for untouched keys stay untouched",
                input: json!({
                    "type": "object",
                    "properties": {
                        "bad~one": {"type": "string"},
                        "clean": {"type": "string"},
                        "other": {"type": "string"}
                    },
                    "dependentRequired": {
                        "clean": ["other"]
                    },
                    "dependentSchemas": {
                        "clean": {"required": ["other"]}
                    }
                }),
                expected: json!({
                    "type": "object",
                    "properties": {
                        "bad_one": {"type": "string"},
                        "clean": {"type": "string"},
                        "other": {"type": "string"}
                    },
                    "dependentRequired": {
                        "clean": ["other"]
                    },
                    "dependentSchemas": {
                        "clean": {"required": ["other"]}
                    }
                }),
            },
        ];
        for case in cases {
            let mut schema = case.input.clone();
            let renames = sanitize_property_keys(&mut schema, "test_tool");
            assert_eq!(
                schema, case.expected,
                "case `{}`: dependent keywords not consistently remapped",
                case.name
            );
            for (renamed, original) in &renames {
                assert_ne!(renamed, original, "case `{}`: no-op rename", case.name);
            }
        }
    }

    #[test]
    fn sanitize_property_keys_clean_schema_is_untouched() {
        let original = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "dotted.key": {"type": "string"},
                "hyphen-key": {"type": "number"},
            },
            "required": ["name"],
        });
        let mut schema = original.clone();
        let renames = sanitize_property_keys(&mut schema, "test_tool");
        assert!(renames.is_empty());
        assert_eq!(schema, original, "clean schema must be byte-identical");
    }

    #[test]
    fn unrename_tool_args_round_trips_originals_recursively() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "issue_class~neq": {"type": "string"},
                "filters": {
                    "type": "object",
                    "properties": {"meta.f[gt]": {"type": "number"}},
                },
            },
        });
        let renames = sanitize_property_keys(&mut schema, "test_tool");

        // Model calls with the sanitized names it was shown, at any depth.
        let mut args = json!({
            "issue_class_neq": "spoof",
            "filters": {"meta.f_gt_": 5},
            "batch": [{"issue_class_neq": "x"}],
        });
        unrename_tool_args(&mut args, &renames);
        assert_eq!(
            args,
            json!({
                "issue_class~neq": "spoof",
                "filters": {"meta.f[gt]": 5},
                "batch": [{"issue_class~neq": "x"}],
            })
        );
    }

    #[test]
    fn registration_stores_sanitized_schema_and_reverse_map() {
        let reg = McpRegistry::new();
        let tools = vec![McpToolSchema {
            name: "query".into(),
            description: None,
            input_schema: json!({
                "type": "object",
                "properties": {"class~neq": {"type": "string"}},
            }),
            annotations: None,
        }];
        reg.replace_server_tools("srv", &tools, &ToolFilter::default(), &[]);
        let entry = reg.lookup("mcp_srv_query").unwrap();
        assert!(
            entry.input_schema["properties"]
                .as_object()
                .unwrap()
                .contains_key("class_neq"),
            "registered schema must carry the sanitized key"
        );
        assert_eq!(entry.arg_renames["class_neq"], "class~neq");
    }
}
