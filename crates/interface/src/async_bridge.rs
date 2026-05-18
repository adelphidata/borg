//! Synchronous-to-async bridge for the `interface` crate.
//!
//! `cortex::db::Database::open()` and the rest of the DB surface are async
//! after the libsql port. Many of our call sites (CLI subcommand handlers,
//! TUI slash-command dispatchers, popup callbacks) are still sync — they
//! run from inside `#[tokio::main]` but the function signatures themselves
//! are not async.
//!
//! `block_on` is the standard bridge: when we're already on a runtime it
//! uses `block_in_place` to release the worker thread and drives the
//! future on the current runtime; otherwise it spins a fresh
//! current-thread runtime (only happens for tests / detached helpers).
//!
//! Prefer making call sites `async` whenever they're reachable from the
//! `#[tokio::main]` entry point. Use `block_on` only when you can't
//! change the signature.
#[allow(clippy::expect_used)]
pub(crate) fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(h) => tokio::task::block_in_place(|| h.block_on(fut)),
        Err(_) => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("async_bridge: failed to spin a current-thread runtime");
            rt.block_on(fut)
        }
    }
}
