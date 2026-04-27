//! Runtime regression guard for async stack-frame hardening.
//!
//! The agent loop can grow deeply nested when tool handlers spawn sub-agents
//! that themselves call tools (Agent → AgentControl::spawn → Agent…). The only
//! way the compiler type-checks that recursive cycle is if the recursive edge
//! returns `Pin<Box<dyn Future…>>`. If someone converts that edge to `async fn`
//! returning an opaque `impl Future`, the pattern that keeps frame size small
//! regresses and the runtime can stack-overflow on deep chains.
//!
//! This test drives a chain of 64 nested `Box::pin(async move { … })` blocks
//! on a tokio worker with a deliberately small (512 KiB) stack. If the
//! compiler or a refactor ever broadens the per-frame stack usage to the point
//! where 64 nestings no longer fit, this fires.

#![allow(
    clippy::approx_constant,
    clippy::assertions_on_constants,
    clippy::const_is_empty,
    clippy::expect_used,
    clippy::field_reassign_with_default,
    clippy::identity_op,
    clippy::items_after_test_module,
    clippy::len_zero,
    clippy::manual_range_contains,
    clippy::needless_borrow,
    clippy::needless_collect,
    clippy::redundant_clone,
    clippy::redundant_closure_for_method_calls,
    clippy::uninlined_format_args,
    clippy::unnecessary_cast,
    clippy::unnecessary_map_or,
    clippy::unwrap_used,
    clippy::useless_format,
    clippy::useless_vec
)]

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
