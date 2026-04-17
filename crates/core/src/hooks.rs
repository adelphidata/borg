use tracing::debug;

mod script;
pub use script::ScriptHook;

/// Points in the agent lifecycle where hooks can intercept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookPoint {
    /// Fired once when a new session begins (before any turns).
    SessionStart,
    /// Fired when a session ends (graceful shutdown or explicit close).
    SessionEnd,
    /// Before the agent starts processing a user message.
    BeforeAgentStart,
    /// Before sending messages to the LLM.
    BeforeLlmCall,
    /// After receiving a complete LLM response.
    AfterLlmResponse,
    /// Before executing a tool call.
    BeforeToolCall,
    /// After a tool call completes.
    AfterToolCall,
    /// After a full agent turn (all tool calls resolved).
    TurnComplete,
    /// When an error occurs during the agent loop.
    OnError,
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
    /// Error occurred.
    Error {
        /// Error description.
        message: String,
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
    Skip,
}

/// Trait for implementing lifecycle hooks on the agent loop.
pub trait Hook: Send + Sync {
    /// Human-readable hook name for logging.
    fn name(&self) -> &str;
    /// Which hook points this hook listens on.
    fn points(&self) -> &[HookPoint];
    /// Execute the hook and return an action.
    fn execute(&self, ctx: &HookContext) -> HookAction;
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
    /// Create an empty hook registry.
    pub fn new() -> Self {
        Self { hooks: Vec::new() }
    }

    /// Register a hook to be dispatched on matching hook points.
    pub fn register(&mut self, hook: Box<dyn Hook>) {
        debug!("Registered hook: {}", hook.name());
        self.hooks.push(hook);
    }

    /// Dispatch a hook context to all registered hooks. Returns merged action.
    pub fn dispatch(&self, ctx: &HookContext) -> HookAction {
        let mut result = HookAction::Continue;

        for hook in &self.hooks {
            if !hook.points().contains(&ctx.point) {
                continue;
            }

            debug!("Dispatching {:?} to hook '{}'", ctx.point, hook.name());
            match hook.execute(ctx) {
                HookAction::Continue => {}
                HookAction::InjectContext(text) => {
                    result = match result {
                        HookAction::InjectContext(existing) => {
                            HookAction::InjectContext(format!("{existing}\n{text}"))
                        }
                        _ => HookAction::InjectContext(text),
                    };
                }
                HookAction::Skip => {
                    debug!("Hook '{}' returned Skip for {:?}", hook.name(), ctx.point);
                    return HookAction::Skip;
                }
            }
        }

        result
    }

    /// Number of registered hooks.
    pub fn hook_count(&self) -> usize {
        self.hooks.len()
    }
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

    impl Hook for TestHook {
        fn name(&self) -> &str {
            &self.name
        }
        fn points(&self) -> &[HookPoint] {
            &self.points
        }
        fn execute(&self, _ctx: &HookContext) -> HookAction {
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

    #[test]
    fn empty_registry_returns_continue() {
        let registry = HookRegistry::new();
        let ctx = make_ctx(HookPoint::BeforeAgentStart);
        assert!(matches!(registry.dispatch(&ctx), HookAction::Continue));
    }

    #[test]
    fn hook_receives_matching_point() {
        let mut registry = HookRegistry::new();
        registry.register(Box::new(TestHook {
            name: "test".to_string(),
            points: vec![HookPoint::BeforeAgentStart],
            action: HookAction::InjectContext("injected".to_string()),
        }));

        let ctx = make_ctx(HookPoint::BeforeAgentStart);
        match registry.dispatch(&ctx) {
            HookAction::InjectContext(text) => assert_eq!(text, "injected"),
            other => panic!("Expected InjectContext, got {other:?}"),
        }
    }

    #[test]
    fn hook_ignores_non_matching_point() {
        let mut registry = HookRegistry::new();
        registry.register(Box::new(TestHook {
            name: "test".to_string(),
            points: vec![HookPoint::TurnComplete],
            action: HookAction::InjectContext("injected".to_string()),
        }));

        let ctx = make_ctx(HookPoint::BeforeAgentStart);
        assert!(matches!(registry.dispatch(&ctx), HookAction::Continue));
    }

    #[test]
    fn skip_short_circuits() {
        let mut registry = HookRegistry::new();
        registry.register(Box::new(TestHook {
            name: "skipper".to_string(),
            points: vec![HookPoint::BeforeToolCall],
            action: HookAction::Skip,
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
        assert!(matches!(registry.dispatch(&ctx), HookAction::Skip));
    }

    #[test]
    fn multiple_inject_contexts_merge() {
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
        match registry.dispatch(&ctx) {
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

    #[test]
    fn continue_hook_does_not_affect_result() {
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
            data: HookData::LlmCall { message_count: 5 },
        };
        assert!(matches!(registry.dispatch(&ctx), HookAction::Continue));
    }

    #[test]
    fn hook_multi_point_registration() {
        let mut registry = HookRegistry::new();
        registry.register(Box::new(TestHook {
            name: "multi".to_string(),
            points: vec![HookPoint::BeforeAgentStart, HookPoint::TurnComplete],
            action: HookAction::InjectContext("ctx".to_string()),
        }));

        let ctx1 = make_ctx(HookPoint::BeforeAgentStart);
        assert!(matches!(
            registry.dispatch(&ctx1),
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
            registry.dispatch(&ctx2),
            HookAction::InjectContext(_)
        ));
    }

    #[test]
    fn skip_before_inject_short_circuits() {
        let mut registry = HookRegistry::new();
        // Skip registered first
        registry.register(Box::new(TestHook {
            name: "skipper".to_string(),
            points: vec![HookPoint::AfterLlmResponse],
            action: HookAction::Skip,
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
        assert!(matches!(registry.dispatch(&ctx), HookAction::Skip));
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
        let _ = HookData::LlmCall { message_count: 1 };
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
        let _ = HookData::Error {
            message: "oops".to_string(),
        };
    }

    #[test]
    fn hook_point_equality() {
        assert_eq!(HookPoint::BeforeAgentStart, HookPoint::BeforeAgentStart);
        assert_ne!(HookPoint::BeforeAgentStart, HookPoint::TurnComplete);
    }

    #[test]
    fn session_start_hook_fires() {
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
        match registry.dispatch(&ctx) {
            HookAction::InjectContext(text) => assert_eq!(text, "session started"),
            other => panic!("Expected InjectContext, got {other:?}"),
        }
    }

    #[test]
    fn session_end_hook_fires() {
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
        match registry.dispatch(&ctx) {
            HookAction::InjectContext(text) => assert_eq!(text, "session ended"),
            other => panic!("Expected InjectContext, got {other:?}"),
        }
    }

    #[test]
    fn session_hooks_do_not_fire_on_other_points() {
        let mut registry = HookRegistry::new();
        registry.register(Box::new(TestHook {
            name: "session_only".to_string(),
            points: vec![HookPoint::SessionStart, HookPoint::SessionEnd],
            action: HookAction::InjectContext("session".to_string()),
        }));

        let ctx = make_ctx(HookPoint::BeforeAgentStart);
        assert!(matches!(registry.dispatch(&ctx), HookAction::Continue));
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
}
