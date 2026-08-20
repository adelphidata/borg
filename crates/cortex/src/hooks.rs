use async_trait::async_trait;
use tracing::{debug, trace, warn};

mod script;
mod webhook;
pub use script::ScriptHook;
pub use webhook::WebhookHook;

/// Points in the agent lifecycle where hooks can intercept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookPoint {
    /// Fired once when a new session begins (before any turns).
    SessionStart,
    /// Fired when a session ends (graceful shutdown or explicit close).
    /// Extension point for `~/.borg/hooks.json`; no built-in consumer. See `docs/hooks.md`.
    SessionEnd,
    /// Before the agent starts processing a user message.
    BeforeAgentStart,
    /// Before sending messages to the LLM.
    BeforeLlmCall,
    /// After receiving a complete LLM response.
    /// Extension point for `~/.borg/hooks.json`; no built-in consumer. See `docs/hooks.md`.
    AfterLlmResponse,
    /// Before executing a tool call.
    BeforeToolCall,
    /// After a tool call completes.
    AfterToolCall,
    /// After a full agent turn (all tool calls resolved).
    TurnComplete,
}

/// Context passed to hooks when they are dispatched.
#[derive(Debug, Clone)]
pub struct HookContext {
    pub point: HookPoint,
    pub session_id: String,
    pub turn_count: u32,
    pub data: HookData,
}

/// Event-specific data carried by a hook context.
#[derive(Debug, Clone)]
pub enum HookData {
    /// Session started.
    SessionStart { session_id: String },
    /// Session ended.
    SessionEnd {
        /// Session identifier.
        session_id: String,
        /// Total turns in the session.
        total_turns: u32,
    },
    /// Agent starting to process a user message.
    AgentStart {
        /// The user's input message.
        user_message: String,
    },
    /// About to call the LLM.
    LlmCall {
        /// Number of messages in the conversation.
        message_count: usize,
        /// Pre-flight prompt-token estimate for the assembled message set.
        /// `None` when the caller hasn't run the tokenizer (e.g. AgentStart
        /// fallbacks). When `Some`, the budget hook prefers this over its
        /// own `message_count * 50` heuristic for cap-crossing checks.
        prompt_tokens: Option<u64>,
    },
    /// LLM response received.
    LlmResponse {
        /// Whether the response contains tool calls.
        has_tool_calls: bool,
        /// Length of the text response.
        text_length: usize,
    },
    /// Tool call about to execute.
    ToolCall {
        /// Tool name.
        name: String,
        /// Serialized arguments.
        args: String,
    },
    /// Tool call completed.
    ToolResult {
        /// Tool name.
        name: String,
        /// Tool output.
        result: String,
        /// Whether the tool returned an error.
        is_error: bool,
    },
    /// Turn completed.
    TurnEnd {
        /// Total tool calls in this turn.
        total_tool_calls: u32,
    },
}

/// Action a hook can return to influence the agent loop.
#[derive(Debug, Clone)]
pub enum HookAction {
    /// No-op, continue normally.
    Continue,
    /// Append text to the system prompt.
    InjectContext(String),
    /// Skip the current action (e.g., skip a tool call).
    Skip {
        /// Why the action was skipped, surfaced to the model in the tool
        /// result so a blocked call is never a silent mystery. Producers
        /// trim and length-bound the text; `None` means the hook gave no
        /// detail and the consumer falls back to a generic message.
        reason: Option<String>,
    },
    /// Run the tool call with rewritten arguments.
    ///
    /// Lets a `PreToolUse` hook correct a call before it executes — pin a
    /// flag, redirect a path, drop an argument — instead of blocking it and
    /// making the model guess, or repairing the damage afterwards. The
    /// payload is the full argument object to use, already merged over the
    /// original by the producer. `Skip` still wins: a hook that both blocks
    /// and modifies has blocked.
    Modify {
        /// Serialized JSON object of the arguments to execute with.
        args: String,
    },
}

/// Trait for implementing lifecycle hooks on the agent loop.
///
/// `execute` is async because hooks routinely persist state to the
/// libsql-backed `Database`, whose entire surface is async.
/// The trait stays object-safe via `#[async_trait]`.
#[async_trait]
pub trait Hook: Send + Sync {
    /// Human-readable hook name for logging.
    fn name(&self) -> &str;
    /// Which hook points this hook listens on.
    fn points(&self) -> &[HookPoint];
    /// Execute the hook and return an action.
    async fn execute(&self, ctx: &HookContext) -> HookAction;
}

/// Registry of lifecycle hooks dispatched during the agent loop.
pub struct HookRegistry {
    hooks: Vec<Box<dyn Hook>>,
}

impl Default for HookRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HookRegistry {
    pub fn new() -> Self {
        Self { hooks: Vec::new() }
    }

    /// Register a hook to be dispatched on matching hook points.
    pub fn register(&mut self, hook: Box<dyn Hook>) {
        // Per-hook registration is logged at TRACE — startup spam is a known
        // smell. Use `hook_count()` + a single DEBUG summary at the call site
        // when registry assembly is complete.
        trace!("Registered hook: {}", hook.name());
        self.hooks.push(hook);
    }

    /// Dispatch a hook context to all registered hooks. Returns merged action.
    pub async fn dispatch(&self, ctx: &HookContext) -> HookAction {
        let mut result = HookAction::Continue;
        // Accumulated argument rewrites, seeded lazily from the original call
        // so several hooks can each adjust a different field.
        let mut modified: Option<serde_json::Map<String, serde_json::Value>> = None;

        for hook in &self.hooks {
            if !hook.points().contains(&ctx.point) {
                continue;
            }

            // Fires per (hook × dispatch_point) — up to ~30 lines/turn at
            // DEBUG, which drowns the actual signal. TRACE is the right level.
            trace!("Dispatching {:?} to hook '{}'", ctx.point, hook.name());
            match hook.execute(ctx).await {
                HookAction::Continue => {}
                HookAction::InjectContext(text) => {
                    result = match result {
                        HookAction::InjectContext(existing) => {
                            HookAction::InjectContext(format!("{existing}\n{text}"))
                        }
                        _ => HookAction::InjectContext(text),
                    };
                }
                HookAction::Skip { reason } => {
                    debug!("Hook '{}' returned Skip for {:?}", hook.name(), ctx.point);
                    // A block outranks any rewrite accumulated so far — the
                    // call is not running, so its arguments are moot.
                    return HookAction::Skip { reason };
                }
                HookAction::Modify { args } => {
                    let Some(patch) = parse_args_object(&args) else {
                        warn!(
                            "Hook '{}' returned a modify action whose args are not a JSON object; ignoring the rewrite",
                            hook.name()
                        );
                        continue;
                    };
                    let base = modified.take().unwrap_or_else(|| original_args(ctx));
                    modified = Some(shallow_merge(base, patch));
                }
            }
        }

        match modified {
            Some(args) => {
                // Only one action can be returned. A rewrite and an injected
                // context at the same dispatch point means two hooks disagreed
                // about what kind of hook this is; say so rather than dropping
                // one silently.
                if matches!(result, HookAction::InjectContext(_)) {
                    warn!(
                        "A hook injected context at the same point another hook rewrote tool arguments; the rewrite wins and the injected context is discarded"
                    );
                }
                HookAction::Modify {
                    args: serde_json::Value::Object(args).to_string(),
                }
            }
            None => result,
        }
    }

    /// Number of registered hooks.
    pub fn hook_count(&self) -> usize {
        self.hooks.len()
    }
}

/// Parse a serialized JSON object into its map, or `None` for anything else
/// (a bare array, a scalar, malformed text).
fn parse_args_object(args: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    match serde_json::from_str::<serde_json::Value>(args) {
        Ok(serde_json::Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// The tool call's own arguments, as the base a rewrite merges over. A call
/// whose args are not a JSON object starts from an empty base — the hook is
/// then supplying the whole object.
fn original_args(ctx: &HookContext) -> serde_json::Map<String, serde_json::Value> {
    match &ctx.data {
        HookData::ToolCall { args, .. } => parse_args_object(args).unwrap_or_default(),
        _ => serde_json::Map::new(),
    }
}

/// Merge `patch` over `base`, one level deep. Shallow by design: a hook that
/// wants to change one field of a nested object supplies that whole object,
/// which is unambiguous, where a deep merge would leave the hook unable to
/// *remove* a nested key.
fn shallow_merge(
    mut base: serde_json::Map<String, serde_json::Value>,
    patch: serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    for (k, v) in patch {
        base.insert(k, v);
    }
    base
}

impl std::fmt::Debug for HookRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookRegistry")
            .field("hook_count", &self.hooks.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestHook {
        name: String,
        points: Vec<HookPoint>,
        action: HookAction,
    }

    #[async_trait]
    impl Hook for TestHook {
        fn name(&self) -> &str {
            &self.name
        }
        fn points(&self) -> &[HookPoint] {
            &self.points
        }
        async fn execute(&self, _ctx: &HookContext) -> HookAction {
            self.action.clone()
        }
    }

    fn make_ctx(point: HookPoint) -> HookContext {
        HookContext {
            point,
            session_id: "test-session".to_string(),
            turn_count: 1,
            data: HookData::AgentStart {
                user_message: "hello".to_string(),
            },
        }
    }

    #[tokio::test]
    async fn empty_registry_returns_continue() {
        let registry = HookRegistry::new();
        let ctx = make_ctx(HookPoint::BeforeAgentStart);
        assert!(matches!(
            registry.dispatch(&ctx).await,
            HookAction::Continue
        ));
    }

    #[tokio::test]
    async fn hook_receives_matching_point() {
        let mut registry = HookRegistry::new();
        registry.register(Box::new(TestHook {
            name: "test".to_string(),
            points: vec![HookPoint::BeforeAgentStart],
            action: HookAction::InjectContext("injected".to_string()),
        }));

        let ctx = make_ctx(HookPoint::BeforeAgentStart);
        match registry.dispatch(&ctx).await {
            HookAction::InjectContext(text) => assert_eq!(text, "injected"),
            other => panic!("Expected InjectContext, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn hook_ignores_non_matching_point() {
        let mut registry = HookRegistry::new();
        registry.register(Box::new(TestHook {
            name: "test".to_string(),
            points: vec![HookPoint::TurnComplete],
            action: HookAction::InjectContext("injected".to_string()),
        }));

        let ctx = make_ctx(HookPoint::BeforeAgentStart);
        assert!(matches!(
            registry.dispatch(&ctx).await,
            HookAction::Continue
        ));
    }

    #[tokio::test]
    async fn skip_short_circuits() {
        let mut registry = HookRegistry::new();
        registry.register(Box::new(TestHook {
            name: "skipper".to_string(),
            points: vec![HookPoint::BeforeToolCall],
            action: HookAction::Skip { reason: None },
        }));
        registry.register(Box::new(TestHook {
            name: "injector".to_string(),
            points: vec![HookPoint::BeforeToolCall],
            action: HookAction::InjectContext("should not reach".to_string()),
        }));

        let ctx = HookContext {
            point: HookPoint::BeforeToolCall,
            session_id: "test".to_string(),
            turn_count: 1,
            data: HookData::ToolCall {
                name: "test_tool".to_string(),
                args: "{}".to_string(),
            },
        };
        assert!(matches!(
            registry.dispatch(&ctx).await,
            HookAction::Skip { .. }
        ));
    }

    #[tokio::test]
    async fn multiple_inject_contexts_merge() {
        let mut registry = HookRegistry::new();
        registry.register(Box::new(TestHook {
            name: "hook1".to_string(),
            points: vec![HookPoint::BeforeAgentStart],
            action: HookAction::InjectContext("first".to_string()),
        }));
        registry.register(Box::new(TestHook {
            name: "hook2".to_string(),
            points: vec![HookPoint::BeforeAgentStart],
            action: HookAction::InjectContext("second".to_string()),
        }));

        let ctx = make_ctx(HookPoint::BeforeAgentStart);
        match registry.dispatch(&ctx).await {
            HookAction::InjectContext(text) => {
                assert!(text.contains("first"));
                assert!(text.contains("second"));
            }
            other => panic!("Expected InjectContext, got {other:?}"),
        }
    }

    #[test]
    fn hook_count() {
        let mut registry = HookRegistry::new();
        assert_eq!(registry.hook_count(), 0);
        registry.register(Box::new(TestHook {
            name: "a".to_string(),
            points: vec![HookPoint::BeforeAgentStart],
            action: HookAction::Continue,
        }));
        assert_eq!(registry.hook_count(), 1);
    }

    #[tokio::test]
    async fn continue_hook_does_not_affect_result() {
        let mut registry = HookRegistry::new();
        registry.register(Box::new(TestHook {
            name: "noop".to_string(),
            points: vec![HookPoint::BeforeLlmCall],
            action: HookAction::Continue,
        }));
        let ctx = HookContext {
            point: HookPoint::BeforeLlmCall,
            session_id: "test".to_string(),
            turn_count: 1,
            data: HookData::LlmCall {
                message_count: 5,
                prompt_tokens: None,
            },
        };
        assert!(matches!(
            registry.dispatch(&ctx).await,
            HookAction::Continue
        ));
    }

    #[tokio::test]
    async fn hook_multi_point_registration() {
        let mut registry = HookRegistry::new();
        registry.register(Box::new(TestHook {
            name: "multi".to_string(),
            points: vec![HookPoint::BeforeAgentStart, HookPoint::TurnComplete],
            action: HookAction::InjectContext("ctx".to_string()),
        }));

        let ctx1 = make_ctx(HookPoint::BeforeAgentStart);
        assert!(matches!(
            registry.dispatch(&ctx1).await,
            HookAction::InjectContext(_)
        ));

        let ctx2 = HookContext {
            point: HookPoint::TurnComplete,
            session_id: "test".to_string(),
            turn_count: 1,
            data: HookData::TurnEnd {
                total_tool_calls: 3,
            },
        };
        assert!(matches!(
            registry.dispatch(&ctx2).await,
            HookAction::InjectContext(_)
        ));
    }

    #[tokio::test]
    async fn skip_before_inject_short_circuits() {
        let mut registry = HookRegistry::new();
        // Skip registered first
        registry.register(Box::new(TestHook {
            name: "skipper".to_string(),
            points: vec![HookPoint::AfterLlmResponse],
            action: HookAction::Skip { reason: None },
        }));
        // Inject registered second — should not execute
        registry.register(Box::new(TestHook {
            name: "injector".to_string(),
            points: vec![HookPoint::AfterLlmResponse],
            action: HookAction::InjectContext("should not appear".to_string()),
        }));

        let ctx = HookContext {
            point: HookPoint::AfterLlmResponse,
            session_id: "test".to_string(),
            turn_count: 1,
            data: HookData::LlmResponse {
                has_tool_calls: false,
                text_length: 100,
            },
        };
        assert!(matches!(
            registry.dispatch(&ctx).await,
            HookAction::Skip { .. }
        ));
    }

    #[test]
    fn registry_debug_format() {
        let mut registry = HookRegistry::new();
        registry.register(Box::new(TestHook {
            name: "test".to_string(),
            points: vec![HookPoint::BeforeAgentStart],
            action: HookAction::Continue,
        }));
        let debug = format!("{registry:?}");
        assert!(debug.contains("hook_count: 1"));
    }

    #[test]
    fn hook_data_variants_constructible() {
        // Ensure all HookData variants can be constructed (compile-time coverage)
        let _ = HookData::AgentStart {
            user_message: "hi".to_string(),
        };
        let _ = HookData::LlmCall {
            message_count: 1,
            prompt_tokens: None,
        };
        let _ = HookData::LlmResponse {
            has_tool_calls: true,
            text_length: 42,
        };
        let _ = HookData::ToolCall {
            name: "foo".to_string(),
            args: "{}".to_string(),
        };
        let _ = HookData::ToolResult {
            name: "foo".to_string(),
            result: "ok".to_string(),
            is_error: false,
        };
        let _ = HookData::TurnEnd {
            total_tool_calls: 5,
        };
    }

    #[test]
    fn hook_point_equality() {
        assert_eq!(HookPoint::BeforeAgentStart, HookPoint::BeforeAgentStart);
        assert_ne!(HookPoint::BeforeAgentStart, HookPoint::TurnComplete);
    }

    #[tokio::test]
    async fn session_start_hook_fires() {
        let mut registry = HookRegistry::new();
        registry.register(Box::new(TestHook {
            name: "session_start".to_string(),
            points: vec![HookPoint::SessionStart],
            action: HookAction::InjectContext("session started".to_string()),
        }));

        let ctx = HookContext {
            point: HookPoint::SessionStart,
            session_id: "s1".to_string(),
            turn_count: 0,
            data: HookData::SessionStart {
                session_id: "s1".to_string(),
            },
        };
        match registry.dispatch(&ctx).await {
            HookAction::InjectContext(text) => assert_eq!(text, "session started"),
            other => panic!("Expected InjectContext, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn session_end_hook_fires() {
        let mut registry = HookRegistry::new();
        registry.register(Box::new(TestHook {
            name: "session_end".to_string(),
            points: vec![HookPoint::SessionEnd],
            action: HookAction::InjectContext("session ended".to_string()),
        }));

        let ctx = HookContext {
            point: HookPoint::SessionEnd,
            session_id: "s1".to_string(),
            turn_count: 5,
            data: HookData::SessionEnd {
                session_id: "s1".to_string(),
                total_turns: 5,
            },
        };
        match registry.dispatch(&ctx).await {
            HookAction::InjectContext(text) => assert_eq!(text, "session ended"),
            other => panic!("Expected InjectContext, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn session_hooks_do_not_fire_on_other_points() {
        let mut registry = HookRegistry::new();
        registry.register(Box::new(TestHook {
            name: "session_only".to_string(),
            points: vec![HookPoint::SessionStart, HookPoint::SessionEnd],
            action: HookAction::InjectContext("session".to_string()),
        }));

        let ctx = make_ctx(HookPoint::BeforeAgentStart);
        assert!(matches!(
            registry.dispatch(&ctx).await,
            HookAction::Continue
        ));
    }

    #[test]
    fn hook_data_session_variants_constructible() {
        let _ = HookData::SessionStart {
            session_id: "s1".to_string(),
        };
        let _ = HookData::SessionEnd {
            session_id: "s1".to_string(),
            total_turns: 10,
        };
    }

    /// A hook that always returns one fixed action.
    struct FixedHook(&'static str, HookAction);

    #[async_trait]
    impl Hook for FixedHook {
        fn name(&self) -> &str {
            self.0
        }
        fn points(&self) -> &[HookPoint] {
            &[HookPoint::BeforeToolCall]
        }
        async fn execute(&self, _ctx: &HookContext) -> HookAction {
            self.1.clone()
        }
    }

    fn tool_call_ctx(args: &str) -> HookContext {
        HookContext {
            point: HookPoint::BeforeToolCall,
            session_id: "s".to_string(),
            turn_count: 1,
            data: HookData::ToolCall {
                name: "run_shell".to_string(),
                args: args.to_string(),
            },
        }
    }

    fn modify(args: &str) -> HookAction {
        HookAction::Modify {
            args: args.to_string(),
        }
    }

    #[tokio::test]
    async fn a_rewrite_merges_over_the_original_arguments() {
        // A hook that pins one field must not have to restate the rest of the
        // call — it cannot know what else the model passed.
        let mut reg = HookRegistry::new();
        reg.register(Box::new(FixedHook("pin-cwd", modify(r#"{"cwd":"/tmp"}"#))));

        let action = reg
            .dispatch(&tool_call_ctx(r#"{"command":"ls","timeout_ms":5000}"#))
            .await;
        let HookAction::Modify { args } = action else {
            panic!("expected a rewrite, got {action:?}");
        };
        let got: serde_json::Value = serde_json::from_str(&args).unwrap();
        assert_eq!(got["command"], "ls", "untouched fields must survive");
        assert_eq!(got["timeout_ms"], 5000);
        assert_eq!(got["cwd"], "/tmp", "the hook's field must be applied");
    }

    #[tokio::test]
    async fn later_rewrites_stack_and_the_last_writer_of_a_field_wins() {
        let mut reg = HookRegistry::new();
        reg.register(Box::new(FixedHook(
            "first",
            modify(r#"{"cwd":"/a","x":1}"#),
        )));
        reg.register(Box::new(FixedHook("second", modify(r#"{"cwd":"/b"}"#))));

        let action = reg.dispatch(&tool_call_ctx(r#"{"command":"ls"}"#)).await;
        let HookAction::Modify { args } = action else {
            panic!("expected a rewrite, got {action:?}");
        };
        let got: serde_json::Value = serde_json::from_str(&args).unwrap();
        assert_eq!(got["command"], "ls");
        assert_eq!(got["x"], 1, "the first hook's field must survive");
        assert_eq!(got["cwd"], "/b", "the later hook must win the conflict");
    }

    #[tokio::test]
    async fn a_block_discards_rewrites_from_earlier_hooks() {
        // The call is not running, so its arguments are moot — and a rewrite
        // must never mask a veto.
        let mut reg = HookRegistry::new();
        reg.register(Box::new(FixedHook("rewriter", modify(r#"{"cwd":"/tmp"}"#))));
        reg.register(Box::new(FixedHook(
            "blocker",
            HookAction::Skip {
                reason: Some("nope".to_string()),
            },
        )));

        let action = reg.dispatch(&tool_call_ctx(r#"{"command":"ls"}"#)).await;
        assert!(
            matches!(action, HookAction::Skip { .. }),
            "block must win, got {action:?}"
        );
    }

    #[tokio::test]
    async fn a_rewrite_that_is_not_an_object_is_ignored() {
        // Fail-safe: a hook returning junk leaves the call exactly as the
        // model wrote it.
        let mut reg = HookRegistry::new();
        reg.register(Box::new(FixedHook("junk", modify("[1,2,3]"))));

        let action = reg.dispatch(&tool_call_ctx(r#"{"command":"ls"}"#)).await;
        assert!(
            matches!(action, HookAction::Continue),
            "junk must not become a rewrite, got {action:?}"
        );
    }
}
