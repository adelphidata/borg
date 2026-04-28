//! Bond-system types, scoring constants, and pure derivations.

use std::fmt;

/// Bond levels derived from score ranges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BondLevel {
    /// Score 0-24: minimal trust established.
    Fragile,
    /// Score 25-44: trust is beginning to form.
    Emerging,
    /// Score 45-64: reliable working relationship.
    Stable,
    /// Score 65-84: strong trust established.
    Trusted,
    /// Score 85-100: maximum trust and autonomy.
    Synced,
}

impl fmt::Display for BondLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fragile => write!(f, "Fragile"),
            Self::Emerging => write!(f, "Emerging"),
            Self::Stable => write!(f, "Stable"),
            Self::Trusted => write!(f, "Trusted"),
            Self::Synced => write!(f, "Synced"),
        }
    }
}

/// Autonomy tiers that describe how proactively the agent should behave.
/// These are informational only — they do not bypass HITL safety checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutonomyTier {
    /// Watch and report only.
    ObserveOnly,
    /// Suggest actions for user approval.
    Recommend,
    /// Draft outputs for user review.
    DraftAssist,
    /// Execute with light guidance.
    GuidedAction,
    /// Full autonomous operation.
    HighTrust,
}

impl fmt::Display for AutonomyTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ObserveOnly => write!(f, "ObserveOnly"),
            Self::Recommend => write!(f, "Recommend"),
            Self::DraftAssist => write!(f, "DraftAssist"),
            Self::GuidedAction => write!(f, "GuidedAction"),
            Self::HighTrust => write!(f, "HighTrust"),
        }
    }
}

/// Bond state derived by replaying verified events.
#[derive(Debug, Clone)]
pub struct BondState {
    /// Current bond score (0..=100).
    pub score: u8,
    /// Bond level derived from the score.
    pub level: BondLevel,
    /// Autonomy tier derived from the bond level.
    pub autonomy_tier: AutonomyTier,
    /// Total number of events in the ledger.
    pub total_events: u32,
    /// Whether the HMAC chain is intact across all replayed events.
    pub chain_valid: bool,
}

/// A recorded bond event from the ledger.
#[derive(Debug, Clone)]
pub struct BondEvent {
    /// Auto-incremented row ID.
    pub id: i64,
    /// Event type (tool_success, tool_failure, creation, correction, etc.).
    pub event_type: String,
    /// Score change applied by this event.
    pub score_delta: i32,
    /// Human-readable reason for the event.
    pub reason: String,
    /// HMAC for this event in the chain.
    pub hmac: String,
    /// HMAC of the previous event in the chain.
    pub prev_hmac: String,
    /// Unix timestamp of event creation.
    pub created_at: i64,
}

// ── Scoring Constants ──

pub(super) const BASELINE_SCORE: i32 = 25;
pub(super) const DELTA_TOOL_SUCCESS: i32 = 1;
pub(super) const DELTA_CREATION_EVENT: i32 = 1;
pub(super) const DELTA_SUGGESTION_ACCEPTED: i32 = 1;
pub(super) const DELTA_CORRECTION: i32 = -2;
pub(super) const DELTA_TOOL_FAILURE: i32 = -1;
pub(super) const DELTA_SUGGESTION_REJECTED: i32 = -1;

/// Derive bond level from score.
pub fn level_from_score(score: u8) -> BondLevel {
    match score {
        0..=24 => BondLevel::Fragile,
        25..=44 => BondLevel::Emerging,
        45..=64 => BondLevel::Stable,
        65..=84 => BondLevel::Trusted,
        85..=100 => BondLevel::Synced,
        _ => BondLevel::Synced, // unreachable for u8 clamped to 100
    }
}

/// Derive autonomy tier from bond level.
pub fn autonomy_from_level(level: BondLevel) -> AutonomyTier {
    match level {
        BondLevel::Fragile => AutonomyTier::ObserveOnly,
        BondLevel::Emerging => AutonomyTier::Recommend,
        BondLevel::Stable => AutonomyTier::DraftAssist,
        BondLevel::Trusted => AutonomyTier::GuidedAction,
        BondLevel::Synced => AutonomyTier::HighTrust,
    }
}
