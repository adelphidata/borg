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

#[cfg(test)]
mod tests {
    use wasmtime::ResourceLimiter;

    use super::super::build_engine;
    use super::super::test_util::minimal_wasm;
    use super::*;

    /// The runtime memory cap is inclusive: a guest may grow to exactly
    /// `max_memory_bytes`; the first byte past it is denied. The module's own
    /// declared maximum is enforced the same way when present.
    #[test]
    fn memory_growing_cap_is_inclusive() {
        let cases: &[(usize, Option<usize>, bool, &str)] = &[
            (2 * WASM_PAGE_BYTES, None, true, "exactly at manifest cap"),
            (
                2 * WASM_PAGE_BYTES + 1,
                None,
                false,
                "one byte past manifest cap",
            ),
            (
                WASM_PAGE_BYTES,
                Some(WASM_PAGE_BYTES),
                true,
                "exactly at declared module maximum",
            ),
            (
                2 * WASM_PAGE_BYTES,
                Some(WASM_PAGE_BYTES),
                false,
                "past declared module maximum",
            ),
        ];
        for (desired, maximum, expect, why) in cases {
            let mut limits = PluginLimits {
                max_memory_bytes: 2 * WASM_PAGE_BYTES,
            };
            let got = limits.memory_growing(0, *desired, *maximum).unwrap();
            assert_eq!(got, *expect, "{why} (desired={desired}, maximum={maximum:?})");
        }
    }

    /// Funcref tables are deliberately unbounded here — memory is the
    /// resource the manifest cap governs. Denying table growth would break
    /// guests compiled with grow-on-demand call tables.
    #[test]
    fn table_growing_is_not_limited() {
        let mut limits = PluginLimits {
            max_memory_bytes: WASM_PAGE_BYTES,
        };
        assert!(
            limits.table_growing(0, 1024, None).unwrap(),
            "table growth must be permitted regardless of the memory cap"
        );
    }

    /// Dropping the timer must CANCEL the ticker, not abandon it. If Drop did
    /// nothing, the struct's sender would still be dropped, the ticker's
    /// `recv_timeout` would wake with a disconnect error (`is_err`), and the
    /// engine's epoch would advance even though the guest call already
    /// finished — poisoning the shared engine for the next call.
    #[test]
    #[ntest::timeout(15_000)]
    fn epoch_timer_drop_cancels_instead_of_firing() {
        let engine = build_engine().expect("engine");
        let module = wasmtime::Module::new(&engine, minimal_wasm()).expect("module");
        let mut store = wasmtime::Store::new(&engine, ());
        store.set_fuel(1_000_000).unwrap();
        // Relative deadline of one tick, armed BEFORE the timer is dropped,
        // so any stray post-drop increment is observable as a trap below.
        store.set_epoch_deadline(1);
        let linker = wasmtime::Linker::new(&engine);
        let instance = linker.instantiate(&mut store, &module).expect("instantiate");
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .expect("alloc export");

        let timer = EpochTimer::new(&engine, std::time::Duration::from_secs(30));
        drop(timer);
        // Give a mis-cancelled ticker time to fire its increment.
        std::thread::sleep(std::time::Duration::from_millis(200));

        alloc
            .call(&mut store, 0)
            .expect("epoch must not advance after a clean cancel");
    }
}
