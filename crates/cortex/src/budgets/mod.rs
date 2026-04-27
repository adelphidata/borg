//! USD budget engine + versioned rate-card lookup.
//!
//! The [`engine::BudgetEngine`] owns pre-flight refusal (refuse before the
//! provider call when the cap would be exceeded) and post-flight
//! reconciliation (record actual spend against the cap, write a
//! `provider_call` event to the ledger). [`rate_card::RateCard`] resolves
//! `(provider_id, model, call_ts)` to the rate row that was active at call
//! time so a budget computation replayed from the audit chain at any future
//! point produces the same USD total.
//!
//! See `docs/redesign/approvals.md` for the full enforcement model.

pub mod engine;
pub mod hook;
pub mod rate_card;
pub mod usd;

#[cfg(test)]
mod tests;

pub use engine::{BudgetEngine, PreFlightDecision};
pub use hook::BudgetHook;
pub use rate_card::{RateCard, RateRow};
pub use usd::Usd;
