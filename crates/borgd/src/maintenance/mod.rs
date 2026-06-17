//! Daemon-side self-healing helpers.
//!
//! 1. **Boot/drift/stall recovery** — [`sweep`]. Each is a small free fn
//!    taking only the `&Database` it needs (no fat context).
//! 2. **Augmenter cooldowns and monthly budget bookkeeping** — meta-table
//!    strings are encapsulated in [`CooldownTable`] and
//!    [`MonthlyBudgetTracker`], so augmenters consume typed handles
//!    instead of reading meta strings ad-hoc.
//!
//! The actual daily sweep (prune/evict/heal/WAL/VACUUM) lives in
//! `cortex::maintenance::run_daily_maintenance` — `task_runner` and
//! `grpc::admin` invoke it directly. This module is the daemon-side
//! typed-handle layer that augmenters and the dispatcher loop reach into.

pub(crate) mod budget;
pub(crate) mod cooldowns;
pub(crate) mod sweep;

pub(crate) use budget::MonthlyBudgetTracker;
