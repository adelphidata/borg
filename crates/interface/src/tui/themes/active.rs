//! Hot-swappable active theme. `theme.rs` calls `active()` on every render
//! pass; `set_active()` is called once at TUI bootstrap and again from the
//! `/theme` popup on selection (and on cursor move for live preview).
//!
//! Storage is `OnceLock<RwLock<Arc<Theme>>>` — cheap reads (clone an Arc),
//! rare writes. The Arc indirection keeps the readback-and-drop pattern
//! deadlock-free even under concurrent preview swaps.

use std::sync::{Arc, OnceLock, RwLock};

use super::builtins;
use super::palette::Theme;

fn cell() -> &'static RwLock<Arc<Theme>> {
    static CELL: OnceLock<RwLock<Arc<Theme>>> = OnceLock::new();
    CELL.get_or_init(|| RwLock::new(Arc::new(builtins::default_theme_static().clone())))
}

/// Read the currently-active theme. Cheap: clones an `Arc`.
///
/// Poison recovery: `RwLock::read` only fails when a writer panicked
/// while holding the lock. The `Arc<Theme>` data is still valid (Theme
/// has no interior mutability), so we recover the inner guard and read
/// it. The alternative — `.expect()` — would propagate a single
/// transient write panic into every subsequent render and effectively
/// kill the TUI. Themes are too cosmetic for that.
pub fn active() -> Arc<Theme> {
    cell()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Hot-swap the active theme. Subsequent `active()` calls observe the new
/// value. Render loops see the swap on their next frame.
pub fn set_active(theme: Arc<Theme>) {
    let mut guard = cell()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = theme;
}

/// Lock for tests that mutate the process-global active cell. `cargo test`
/// runs tests in parallel by default, so a snapshot-restore pattern alone
/// will race when two tests both swap the theme. Tests that touch
/// `set_active`/`active` should hold this lock for their whole body.
///
/// Exposed `pub(crate)` so the sibling `tui::theme_popup` and `tui::theme`
/// test modules can share the same lock — a per-module lock would let
/// those modules race with each other.
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    // NOTE: these tests share the process-global active-theme cell with the
    // rest of the test binary. Each holds `TEST_LOCK` for its entire body
    // AND snapshot-restores, so a sibling test that takes the same lock
    // sees a clean cell.

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn builtin_arc(name: &str) -> Arc<Theme> {
        let Some(t) = builtins::builtin(name) else {
            panic!("builtin '{name}' must exist");
        };
        Arc::new(t.clone())
    }

    #[test]
    fn set_active_then_active_reads_back_same_theme() {
        let _guard = lock();
        let before = active();
        set_active(builtin_arc("mono"));
        assert_eq!(active().name, "mono");
        set_active(before);
    }

    #[test]
    fn concurrent_reads_during_swap_do_not_deadlock() {
        let _guard = lock();
        let before = active();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_writer = stop.clone();

        let writer = thread::spawn(move || {
            let mono = builtin_arc("mono");
            let slate = builtin_arc("slate");
            for i in 0..50 {
                if i % 2 == 0 {
                    set_active(mono.clone());
                } else {
                    set_active(slate.clone());
                }
                thread::sleep(Duration::from_micros(50));
            }
            stop_writer.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        let mut readers = Vec::new();
        for _ in 0..4 {
            let stop = stop.clone();
            readers.push(thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                    let t = active();
                    // The name must always be one of the values we set or
                    // the initial default — never garbage.
                    assert!(
                        matches!(t.name.as_ref(), "default" | "mono" | "slate"),
                        "unexpected theme name during concurrent read: {}",
                        t.name
                    );
                }
            }));
        }

        writer.join().unwrap();
        for r in readers {
            r.join().unwrap();
        }

        set_active(before);
    }
}
