//! Factory: build the active [`MemoryProvider`] from `Config`.
//!
//! Selecting an external provider with missing credentials falls back to
//! [`NativeProvider`] with a one-shot `tracing::warn!` — borg always
//! boots, the operator sees the misconfiguration in `daemon.log`, and the
//! `memory_search` tool keeps working against the local SQLite store.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use super::{
    honcho::HonchoProvider, letta::LettaProvider, mem0::Mem0Provider,
    openviking::OpenVikingProvider, zep::ZepProvider,
};
use super::{MemoryProvider, NativeProvider};
use crate::config::media::MemoryProviderKind;
use crate::config::Config;

/// One-shot tracker so a misconfigured external provider doesn't spam
/// the log every turn. Bits: 1=mem0, 2=honcho, 4=openviking, 8=letta,
/// 16=zep.
static WARNED_MASK: AtomicU8 = AtomicU8::new(0);

fn warn_once(bit: u8, name: &str, detail: &str) {
    let prev = WARNED_MASK.fetch_or(bit, Ordering::Relaxed);
    if prev & bit == 0 {
        tracing::warn!(
            "memory provider '{name}' selected but unusable: {detail} \
             — falling back to native"
        );
    }
}

/// Resolve the configured backend, returning `NativeProvider` when an
/// external provider is selected but unconfigured.
pub fn resolve(config: &Config) -> Arc<dyn MemoryProvider> {
    match config.memory.provider {
        MemoryProviderKind::Native => Arc::new(NativeProvider::new()),
        MemoryProviderKind::Mem0 => match Mem0Provider::from_settings(&config.memory.mem0) {
            Ok(p) => Arc::new(p),
            Err(e) => {
                warn_once(1, "mem0", &e.to_string());
                Arc::new(NativeProvider::new())
            }
        },
        MemoryProviderKind::Honcho => match HonchoProvider::from_settings(&config.memory.honcho) {
            Ok(p) => Arc::new(p),
            Err(e) => {
                warn_once(2, "honcho", &e.to_string());
                Arc::new(NativeProvider::new())
            }
        },
        MemoryProviderKind::Openviking => {
            match OpenVikingProvider::from_settings(&config.memory.openviking) {
                Ok(p) => Arc::new(p),
                Err(e) => {
                    warn_once(4, "openviking", &e.to_string());
                    Arc::new(NativeProvider::new())
                }
            }
        }
        MemoryProviderKind::Letta => match LettaProvider::from_settings(&config.memory.letta) {
            Ok(p) => Arc::new(p),
            Err(e) => {
                warn_once(8, "letta", &e.to_string());
                Arc::new(NativeProvider::new())
            }
        },
        MemoryProviderKind::Zep => match ZepProvider::from_settings(&config.memory.zep) {
            Ok(p) => Arc::new(p),
            Err(e) => {
                warn_once(16, "zep", &e.to_string());
                Arc::new(NativeProvider::new())
            }
        },
    }
}

/// Test-only helper: clear the warn-once mask between tests so each
/// test starts from a fresh state.
#[cfg(test)]
pub(crate) fn reset_warn_once_for_tests() {
    WARNED_MASK.store(0, Ordering::Relaxed);
}
