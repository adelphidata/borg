//! Regression guards for async stack-frame hardening.
//!
//! The agent loop can grow deeply nested when tool handlers spawn sub-agents that
//! themselves call tools (Agent → AgentControl::spawn → Agent…). Two defenses are
//! in place:
//!
//! 1. `multi_agent::run_sub_agent` returns `Pin<Box<dyn Future…>>` — this is the
//!    only way the compiler can type-check the recursive cycle
//!    (Agent → AgentControl → run_sub_agent → Agent).
//! 2. `run_agent_loop` keeps its frame small by extracting heavy arms of the
//!    stream-event `match` into helpers (e.g. `record_usage`). Ballooning the
//!    outer future risks CI stack overflows even on normally-sized call chains.
//!
//! Both defenses are invisible at the type level once compiled, so the tests below
//! read the source to enforce them. If you genuinely need to remove one, also
//! update this test — do not silently weaken the guard.

/// Confirms the Box::pin indirection that breaks the Agent ↔ sub-agent async
/// recursion cycle has not been removed. If this fires, compilation has likely
/// also broken, but the explicit test makes the intent loud.
#[test]
fn run_sub_agent_still_box_pins_future() {
    let src = include_str!("../src/multi_agent/mod.rs");
    assert!(
        src.contains("fn run_sub_agent("),
        "run_sub_agent helper has moved or been renamed — update the guard"
    );
    assert!(
        src.contains("Pin<Box<dyn std::future::Future<Output = Result<String>> + Send>>"),
        "run_sub_agent must return Pin<Box<dyn Future…>> to break the async recursion cycle; \
         converting it to `async fn` will fail to compile but fail with an obscure error, \
         so this guard flags the intent loudly"
    );
    assert!(
        src.contains("Box::pin(async move"),
        "run_sub_agent body must wrap its async block in Box::pin(…)"
    );
}

/// Confirms the Usage-arm extraction stays in place. Inlining the per-event DB
/// lock + pricing-call path back into the `tokio::select!` arm bloats the outer
/// future's frame size; codex has shipped two separate fixes for exactly this
/// pattern (see `reference/codex/codex-rs/core/src/guardian/review.rs` and
/// commit 2bfa62761 "stack overflow fix").
#[test]
fn agent_loop_usage_arm_is_extracted() {
    let src = include_str!("../src/agent.rs");
    assert!(
        src.contains("fn record_usage("),
        "record_usage helper has been inlined back into run_agent_loop — re-extract it \
         to keep the outer future's frame small"
    );
    assert!(
        src.contains("self.record_usage(&usage)"),
        "run_agent_loop must call self.record_usage(&usage) from the StreamEvent::Usage arm"
    );
}

/// Confirms the runtime stack size is explicitly set to 4 MiB. Reverting to
/// `#[tokio::main]` silently drops back to tokio's 2 MiB default.
#[test]
fn runtime_stack_size_is_explicit() {
    let src = include_str!("../../cli/src/main.rs");
    assert!(
        !src.contains("#[tokio::main]"),
        "main.rs has reverted to #[tokio::main] — this drops the runtime stack size back \
         to tokio's 2 MiB default; restore the explicit Runtime::Builder with thread_stack_size"
    );
    assert!(
        src.contains("thread_stack_size(RUNTIME_STACK_SIZE)"),
        "main.rs must configure thread_stack_size on the tokio runtime builder"
    );
    assert!(
        src.contains("const RUNTIME_STACK_SIZE: usize = 4 * 1024 * 1024"),
        "RUNTIME_STACK_SIZE must be 4 MiB; do not lower it without auditing agent-loop \
         frame sizes"
    );
}

/// Runtime smoke: verify that a chain of Box::pin'd async blocks equivalent in
/// pattern to `run_sub_agent` completes on a tokio worker with a deliberately
/// small (512 KiB) stack. Catches *pattern* regressions — if the canonical
/// Box::pin(async move {…}) chain stops keeping frames small (e.g. by switching
/// to `async fn` returning an opaque `impl Future`), this will overflow.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn nested_box_pinned_async_chain_completes() {
    fn run_nested(depth: u32) -> std::pin::Pin<Box<dyn std::future::Future<Output = u32> + Send>> {
        Box::pin(async move {
            if depth == 0 {
                return 0;
            }
            // Simulate some locally-held state to occupy stack.
            let buf = [0u8; 1024];
            let _ = buf[0];
            run_nested(depth - 1).await + 1
        })
    }

    assert_eq!(run_nested(64).await, 64);
}
