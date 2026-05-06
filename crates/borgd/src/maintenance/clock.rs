//! Pluggable wall-clock source for the maintenance module.
//!
//! Exists so unit tests can drive [`super::Maintenance`] deterministically
//! with a [`FakeClock`] without spinning up a daemon. Two real impls
//! (`SystemClock` in production, `FakeClock` under `cfg(test)`) is what
//! earns the trait — per `RUST-IDIOMS.md`, a single-impl trait would be
//! indirection without leverage.

/// Wall-clock abstraction. Returns unix seconds.
pub(crate) trait Clock: Send + Sync {
    /// Current time in unix seconds.
    fn now_secs(&self) -> i64;
}

/// Production clock — reads `SystemTime::now()`.
#[derive(Debug, Default)]
pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn now_secs(&self) -> i64 {
        chrono::Utc::now().timestamp()
    }
}

/// Test clock with externally-controlled time.
///
/// Cheap to clone — internally an `Arc<AtomicI64>` so tests can mutate
/// after handing it to `Maintenance`.
#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct FakeClock(std::sync::Arc<std::sync::atomic::AtomicI64>);

#[cfg(test)]
impl FakeClock {
    pub(crate) fn new(now_secs: i64) -> Self {
        Self(std::sync::Arc::new(std::sync::atomic::AtomicI64::new(
            now_secs,
        )))
    }

    #[allow(dead_code)]
    pub(crate) fn advance_secs(&self, secs: i64) {
        self.0.fetch_add(secs, std::sync::atomic::Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn set(&self, now_secs: i64) {
        self.0.store(now_secs, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
impl Clock for FakeClock {
    fn now_secs(&self) -> i64 {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}
