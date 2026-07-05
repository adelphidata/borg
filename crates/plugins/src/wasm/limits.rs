//! Runtime resource limiting: wall-clock epoch timer and memory-growth caps.

use anyhow::Result;

/// Wall-clock timeout enforcer backed by wasmtime's epoch interruption.
///
/// On construction it spawns a sentinel thread that waits for either the
/// timeout to elapse or a Drop-time cancellation signal. On timeout it calls
/// `Engine::increment_epoch` on a weak handle, which causes the wasm store's
/// `epoch_deadline` (set to `1` by the caller) to be reached and traps the
/// running guest at the next safe point.
///
/// The Drop impl sends a cancellation message so a normal call return doesn't
/// leak a sleeping thread for the full `timeout_ms` budget.
pub(super) struct EpochTimer {
    cancel: Option<std::sync::mpsc::SyncSender<()>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl EpochTimer {
    pub(super) fn new(engine: &wasmtime::Engine, timeout: std::time::Duration) -> Self {
        // Capacity 1 (NOT 0) so `try_send` from Drop always succeeds — even
        // if the spawned thread hasn't yet entered `recv_timeout`. With a
        // rendezvous (capacity 0) channel, try_send returns `Full` whenever
        // the receiver hasn't reached its recv call yet, and Drop then
        // blocks in `join()` for the full `timeout` waiting for the ticker
        // to wake up. That race showed up as 5–60s test hangs.
        let (tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
        let engine_weak = engine.weak();
        let handle = std::thread::spawn(move || {
            if rx.recv_timeout(timeout).is_err() {
                if let Some(e) = engine_weak.upgrade() {
                    e.increment_epoch();
                }
            }
        });
        Self {
            cancel: Some(tx),
            handle: Some(handle),
        }
    }
}

impl Drop for EpochTimer {
    fn drop(&mut self) {
        if let Some(tx) = self.cancel.take() {
            // With sync_channel(1), this writes to the buffer and returns
            // immediately. The ticker thread will pop it on its next
            // `recv_timeout` and exit cleanly. `Err` only happens if the
            // ticker already exited (timeout fired or panicked), which is fine.
            let _ = tx.try_send(());
        }
        if let Some(h) = self.handle.take() {
            // Bounded by the time it takes the ticker to read the buffered
            // cancel and return — milliseconds, not the full timeout.
            let _ = h.join();
        }
    }
}

pub(super) struct PluginLimits {
    pub(super) max_memory_bytes: usize,
}

pub(super) const WASM_PAGE_BYTES: usize = 65_536;

impl wasmtime::ResourceLimiter for PluginLimits {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> Result<bool> {
        if desired > self.max_memory_bytes {
            return Ok(false);
        }
        if let Some(max) = maximum {
            if desired > max {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        _desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool> {
        Ok(true)
    }
}
