//! Evolution system — Pokemon-style agent specialization via sustained usage.
//!
//! Three permanent stages (Base → Evolved → Final) with Lvl.0–99 per stage.
//! Ten archetypes classify usage patterns; LLM generates unique evolution names.
//! State is event-sourced: derived by replaying verified events from baseline.
//! HMAC chain prevents tampering; rate limiting prevents gaming.
//!
//! XP curve is WoW-style: early levels fast, late levels exponentially harder.
//! Stage 1 completes in 2-5 days, Stage 2 in ~30 days, Stage 3 Lvl.99 in 6-12 months.

mod celebration;
mod classification;
pub mod commands;
mod feed;
mod format;
mod helpers;
mod hmac;
pub mod keychain;
mod milestones;
pub mod plan_emission;
mod replay;
pub mod security;
pub mod share_card;
pub mod signature;

#[cfg(all(test, feature = "phase9-tests-disabled"))]
pub(crate) use hmac::{compute_event_hmac, compute_event_hmac_legacy, EVOLUTION_HMAC_LEGACY};
pub(crate) use hmac::{verify_event_hmac, EVOLUTION_HMAC_DOMAIN};

pub use celebration::{
    celebration_art, format_celebration, format_celebration_message, CelebrationArt,
    CelebrationKind, CelebrationPayload, MilestonePayload,
};
pub use classification::*;
pub use commands::{dispatch, parse, CommandOutput, EvolutionCommand};
pub use feed::{recent_xp_feed, xp_summary, FeedEntry, FeedKind, XpSummary};
pub use format::{
    format_archetype_scores, format_archetype_scores_with_momentum, format_compact,
    format_evolution_context, format_evolution_overview, format_history, format_next_step_hints,
    format_readiness, format_status_section, format_status_section_with_width, format_xp_feed,
    format_xp_summary,
};
pub use helpers::{compute_momentum, compute_mood, compute_readiness, next_step_hints, render_bar};
pub use milestones::{check_milestones, Milestone, PostureContext};
pub use replay::*;
pub use share_card::render_ascii_card;

use std::collections::HashMap;
use std::fmt;

use async_trait::async_trait;
use tokio::sync::Mutex as AsyncMutex;

use crate::constants::SECS_PER_DAY;
use crate::db::Database;
use crate::hooks::{Hook, HookAction, HookContext, HookData, HookPoint};

/// Maximum events per bucket per hour during replay, indexed by event type.
/// Event types not listed fall back to `RATE_LIMIT_DEFAULT`.
const RATE_LIMITS: &[(&str, u32)] = &[
    ("xp_gain", 15),
    // Tight cap on milestone bonuses so a chain-tampering attempt can't
    // mint many `milestone_bonus` rows in one hour. Each milestone is
    // dedup'd at write-time, so legitimate flow needs at most 1/hr.
    ("milestone_bonus", 3),
    ("evolution", 3),
    ("classification", 3),
    ("archetype_shift", 5),
    ("level_up", 10),
    ("milestone_unlocked", 3),
    ("mood_changed", 5),
    ("share_card_created", 3),
];

const RATE_LIMIT_DEFAULT: u32 = 10;

/// Maximum events per bucket per hour during replay.
pub(crate) fn rate_limit_for(event_type: &str) -> u32 {
    crate::hmac_chain::lookup_rate_limit(RATE_LIMITS, event_type, RATE_LIMIT_DEFAULT)
}

/// The 11 archetypes that classify usage patterns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Archetype {
    /// Infrastructure and deployment operations.
    Ops,
    /// Software building and compilation.
    Builder,
    /// Data analysis and querying.
    Analyst,
    /// Messaging and email communications.
    Communicator,
    /// Security auditing and hardening.
    Guardian,
    /// Planning and decision-making.
    Strategist,
    /// Content and artifact creation.
    Creator,
    /// Maintenance and nurturing tasks.
    Caretaker,
    /// Commerce and transaction workflows.
    Merchant,
    /// Homelab and hardware tinkering.
    Tinkerer,
    /// Marketing, growth, and campaign workflows.
    Marketer,
}

impl Archetype {
    /// All archetype variants in definition order.
    pub const ALL: [Archetype; 11] = [
        Archetype::Ops,
        Archetype::Builder,
        Archetype::Analyst,
        Archetype::Communicator,
        Archetype::Guardian,
        Archetype::Strategist,
        Archetype::Creator,
        Archetype::Caretaker,
        Archetype::Merchant,
        Archetype::Tinkerer,
        Archetype::Marketer,
    ];

    /// Parse an archetype name (case-insensitive) into the enum variant.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "ops" => Some(Self::Ops),
            "builder" => Some(Self::Builder),
            "analyst" => Some(Self::Analyst),
            "communicator" => Some(Self::Communicator),
            "guardian" => Some(Self::Guardian),
            "strategist" => Some(Self::Strategist),
            "creator" => Some(Self::Creator),
            "caretaker" => Some(Self::Caretaker),
            "merchant" => Some(Self::Merchant),
            "tinkerer" => Some(Self::Tinkerer),
            "marketer" => Some(Self::Marketer),
            _ => None,
        }
    }
}

impl fmt::Display for Archetype {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ops => write!(f, "ops"),
            Self::Builder => write!(f, "builder"),
            Self::Analyst => write!(f, "analyst"),
            Self::Communicator => write!(f, "communicator"),
            Self::Guardian => write!(f, "guardian"),
            Self::Strategist => write!(f, "strategist"),
            Self::Creator => write!(f, "creator"),
            Self::Caretaker => write!(f, "caretaker"),
            Self::Merchant => write!(f, "merchant"),
            Self::Tinkerer => write!(f, "tinkerer"),
            Self::Marketer => write!(f, "marketer"),
        }
    }
}

/// Three permanent evolution stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Initial stage (Stage 1).
    Base,
    /// Intermediate stage after first evolution (Stage 2).
    Evolved,
    /// Maximum stage after second evolution (Stage 3).
    Final,
}

impl Stage {
    /// Returns the 1-based stage number.
    pub fn number(&self) -> u8 {
        match self {
            Self::Base => 1,
            Self::Evolved => 2,
            Self::Final => 3,
        }
    }

    /// Parse a stage name (case-insensitive) into the enum variant.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "base" => Some(Self::Base),
            "evolved" => Some(Self::Evolved),
            "final" => Some(Self::Final),
            _ => None,
        }
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Base => write!(f, "base"),
            Self::Evolved => write!(f, "evolved"),
            Self::Final => write!(f, "final"),
        }
    }
}

/// Direction of movement for an archetype's recent effective score.
///
/// Computed by comparing the last 7 days of aligned XP to the prior 7 days.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trend {
    /// Recent score strictly greater than the prior window.
    Rising,
    /// Recent and prior windows within a small tolerance of each other.
    Stable,
    /// Recent score strictly lower than the prior window.
    Falling,
}

impl fmt::Display for Trend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rising => write!(f, "rising"),
            Self::Stable => write!(f, "stable"),
            Self::Falling => write!(f, "falling"),
        }
    }
}

impl Trend {
    /// Compact arrow suitable for inline rendering (↑ / → / ↓).
    pub fn arrow(&self) -> &'static str {
        match self {
            Self::Rising => "\u{2191}",
            Self::Stable => "\u{2192}",
            Self::Falling => "\u{2193}",
        }
    }
}

/// Companion mood derived from vitals, bond, and evolution state.
///
/// Mood is a lossy UX signal, not authoritative state — the underlying
/// vitals/bond/evolution numbers remain the source of truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mood {
    /// Baseline — nothing notable.
    Stable,
    /// Low correction rate, high focus — doing work smoothly.
    Focused,
    /// High correction rate or low min vital — struggling.
    Strained,
    /// Growth vital trending up and archetype still forming.
    Learning,
    /// Dominant archetype still shifting; no stable specialization yet.
    Drifting,
    /// Near an evolution boundary (Lvl 99 with gates close to passing).
    Ascending,
}

impl fmt::Display for Mood {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stable => write!(f, "stable"),
            Self::Focused => write!(f, "focused"),
            Self::Strained => write!(f, "strained"),
            Self::Learning => write!(f, "learning"),
            Self::Drifting => write!(f, "drifting"),
            Self::Ascending => write!(f, "ascending"),
        }
    }
}

/// One prerequisite for evolution that is not yet satisfied.
#[derive(Debug, Clone)]
pub struct BlockingGate {
    /// Short name of the gate (e.g. "level", "bond", "dominance_ratio").
    pub name: String,
    /// Current value toward the target.
    pub current: f64,
    /// Target value needed to clear the gate.
    pub target: f64,
    /// Human-facing hint about how to move this gate toward target.
    pub hint: String,
}

/// Overall readiness snapshot for the next stage transition.
#[derive(Debug, Clone)]
pub struct ReadinessReport {
    /// True when all gates are satisfied (i.e. evolution is imminent).
    pub ready: bool,
    /// List of gates still blocking the transition (empty when `ready`).
    pub blocking: Vec<BlockingGate>,
    /// Coarse 0.0..=1.0 progress estimate across all gates.
    pub progress: f32,
}

/// A recorded event from the evolution ledger.
#[derive(Debug, Clone)]
pub struct EvolutionEvent {
    /// Auto-incremented row ID.
    pub id: i64,
    /// Event type (xp_gain, evolution, classification, archetype_shift).
    pub event_type: String,
    /// XP amount gained or lost.
    pub xp_delta: i32,
    /// Archetype associated with this event, if any.
    pub archetype: Option<String>,
    /// What triggered this event (tool name, hook, etc.).
    pub source: String,
    /// Optional JSON metadata blob.
    pub metadata_json: Option<String>,
    /// Unix timestamp of event creation.
    pub created_at: i64,
    /// Chain signature for this event. Either an HMAC-SHA256 (legacy rows
    /// where `pubkey_id IS NULL`) or an Ed25519 signature (rows linked to a
    /// `device_keys` row via `pubkey_id`). Hex-encoded in either case.
    pub hmac: String,
    /// Chain signature of the previous event.
    pub prev_hmac: String,
    /// Originating session for this event, if known. Populated for events
    /// recorded by signing-aware code paths; legacy rows are NULL.
    pub session_id: Option<String>,
    /// FK into `device_keys` identifying which Ed25519 public key signed this
    /// row. `None` selects the legacy HMAC verification path on replay.
    pub pubkey_id: Option<i64>,
}

/// Computed evolution state (derived from replaying events).
#[derive(Debug, Clone)]
pub struct EvolutionState {
    /// Current evolution stage.
    pub stage: Stage,
    /// Current level within the stage (0..=99).
    pub level: u8,
    /// Total XP accumulated in the current stage.
    pub total_xp: u32,
    /// XP remaining to reach the next level.
    pub xp_to_next_level: u32,
    /// Archetype with the highest *effective* score (see `archetype_scores`).
    pub dominant_archetype: Option<Archetype>,
    /// LLM-generated evolution name (set on stage transition).
    pub evolution_name: Option<String>,
    /// LLM-generated evolution description (set on stage transition).
    pub evolution_description: Option<String>,
    /// Effective XP score per archetype, computed as
    /// `lifetime * 0.20 + last_30d * 0.80` and rounded to u32.
    /// This is what drives dominant-archetype selection and surfaces that
    /// gate on `DOMINANCE_THRESHOLD`.
    pub archetype_scores: HashMap<Archetype, u32>,
    /// Raw lifetime score per archetype (all verified xp_gain events).
    pub lifetime_scores: HashMap<Archetype, u32>,
    /// Raw score from events within the last 30 days (per `now_ts` passed at replay).
    pub last_30d_scores: HashMap<Archetype, u32>,
    /// Ordered log of (unix_ts, dominant_archetype) shifts observed during replay.
    /// Last entry's timestamp is when the current dominant became dominant —
    /// used to compute archetype-stability for Stage 2→3 gate.
    pub dominant_history: Vec<(i64, Archetype)>,
    /// Number of verified events that were accepted during replay.
    pub total_events: u32,
    /// Whether the HMAC chain is intact across all replayed events.
    pub chain_valid: bool,
    /// Per-archetype 7d-vs-7d trend. Populated at replay time from the
    /// accepted event stream; does not persist.
    pub momentum: HashMap<Archetype, Trend>,
    /// Last N `level_up` + `milestone_unlocked` events, newest first.
    /// Used to drive the `/xp` feed without re-scanning the full ledger.
    pub level_up_events_recent: Vec<EvolutionEvent>,
    /// Companion mood — `None` until a caller wires in vitals+bond via
    /// [`compute_mood`]. Left empty by pure event replay.
    pub mood: Option<Mood>,
    /// Readiness snapshot for the next stage transition. `None` until a
    /// caller wires in vitals+bond via [`compute_readiness`], or at Stage 3
    /// where no transition is possible.
    pub readiness: Option<ReadinessReport>,
}

//
// Level/XP math lives in `evolution::xp`; re-exported here for call-site
// compatibility.

mod xp;
pub use xp::{level_from_xp, total_xp_for_level, xp_for_level};

/// Base XP awarded for a successful tool call.
const BASE_XP_TOOL_SUCCESS: i32 = 1;
/// Bonus XP for archetype-aligned tool success.
const BONUS_XP_ALIGNED: i32 = 1;
/// Base XP for creation events.
const BASE_XP_CREATION: i32 = 2;
/// Bonus XP for archetype-aligned creation.
const BONUS_XP_CREATION_ALIGNED: i32 = 1;
/// Base XP for session interaction.
const BASE_XP_INTERACTION: i32 = 1;
/// Maximum XP delta allowed per event.
pub(crate) const MAX_XP_DELTA: i32 = BASE_XP_CREATION + BONUS_XP_CREATION_ALIGNED;
/// Maximum XP delta for `milestone_bonus` events. Lifted above
/// [`MAX_XP_DELTA`] because milestone bonuses are one-shot, dedup-gated,
/// and produced by deterministic transitions (e.g. posture upgrade) —
/// the per-event cap exists to prevent tool-driven gaming, which doesn't
/// apply to chain-derived bonuses.
///
/// Set well above any planned milestone bonus so a future "+250 XP for
/// X" doesn't silently truncate against the cap. The cap is a safety
/// net, not a budget.
pub(crate) const MAX_MILESTONE_BONUS_XP: i32 = 500;
/// XP awarded by the `secure_headless_setup` milestone when a headless
/// install upgrades from the file-encrypted KEK to the OS keychain.
pub(crate) const SECURE_HEADLESS_SETUP_XP: i32 = 100;
// Compile-time guard: every named milestone bonus must fit under the
// per-event cap. Add new `_BONUS_XP` consts to this assertion when
// landing future milestone-bonus rewards.
const _: () = assert!(
    SECURE_HEADLESS_SETUP_XP <= MAX_MILESTONE_BONUS_XP,
    "SECURE_HEADLESS_SETUP_XP exceeds MAX_MILESTONE_BONUS_XP"
);
/// Valid evolution event types.
pub(crate) const VALID_EVOLUTION_EVENT_TYPES: &[&str] = &[
    "xp_gain",
    "milestone_bonus",
    "evolution",
    "classification",
    "archetype_shift",
    "level_up",
    "milestone_unlocked",
    "mood_changed",
    "share_card_created",
];
/// Total evolution events per hour (write-time cap).
///
/// Per-type caps sum to well above this — the global cap is a coarse
/// safety net against a flood across all types. Bumped from 20 → 40 when
/// V2 added the informational types (`level_up`, `milestone_unlocked`,
/// `mood_changed`, `share_card_created`) so bursty level-ups near a stage
/// boundary don't starve the legitimate `evolution` row that follows.
pub(crate) const TOTAL_EVENTS_PER_HOUR: i64 = 40;
/// Per-source events per hour (write-time coarse gate; replay applies graduated decay).
pub(crate) const WRITE_SOURCE_RATE_LIMIT: i64 = 5;

/// Returns true if an `evolution`-type event carries `gates_verified: true`
/// in its metadata JSON blob. Evolution events that fail this check are
/// rejected during replay *before* consuming rate-limit budget.
fn evolution_gates_verified(event: &EvolutionEvent) -> bool {
    event
        .metadata_json
        .as_deref()
        .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
        .and_then(|v| v.get("gates_verified").and_then(serde_json::Value::as_bool))
        .unwrap_or(false)
}

/// Find the dominant archetype (highest score). Only used by test fixtures —
/// production replay uses `replay::dominant_from_effective` against the
/// time-weighted effective scores.
#[cfg(all(test, feature = "phase9-tests-disabled"))]
fn dominant_archetype(scores: &HashMap<Archetype, u32>) -> Option<Archetype> {
    scores
        .iter()
        .max_by_key(|(_, &score)| score)
        .filter(|(_, &score)| score > 0)
        .map(|(&arch, _)| arch)
}

/// Ratio at which we treat the top archetype as dominant for surface decisions
/// (heartbeat seeds, memory consolidation weighting, skill priority tiebreaks,
/// hybrid vs specialized naming). One knob, used everywhere; if runaway
/// specialization is observed, raise this single constant.
pub const DOMINANCE_THRESHOLD: f64 = 1.3;

/// Check if Stage 1→2 evolution gates are met.
///
/// The previous implementation also required the dominant archetype to score
/// ≥ `DOMINANCE_THRESHOLD` × runner-up — this locked generalists out of the
/// first transition entirely. The dominance ratio is no longer a hard gate at
/// any stage; it survives only as the query used by surfaces to decide
/// "is there a clear archetype right now".
pub fn check_stage1_gates(state: &EvolutionState, bond_score: u8, min_vital: u8) -> bool {
    if state.stage != Stage::Base {
        return false;
    }
    if state.level < 99 {
        return false;
    }
    if bond_score < 30 {
        return false;
    }
    if min_vital < 20 {
        return false;
    }
    true
}

/// Tri-state result for Stage 2→3 gate evaluation.
///
/// `Deferred` covers the "not enough signal yet" case: with fewer than
/// [`STAGE2_MIN_VITALS_EVENTS`] vitals events in the 14-day window, a single
/// correction can swing correction-rate above 20% and falsely fail the gate.
/// Surfaces should render `Deferred` as "need N more events", not "failed".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateState {
    /// All gates satisfied — transition is allowed.
    Pass,
    /// One or more gates failed.
    Fail,
    /// Insufficient signal to evaluate (not enough vitals events yet).
    Deferred,
}

/// Minimum vitals events in the 14-day window before correction-rate is trusted.
pub const STAGE2_MIN_VITALS_EVENTS: u32 = 50;

/// Check if Stage 2→3 evolution gates are met.
///
/// Returns [`GateState::Deferred`] when fewer than [`STAGE2_MIN_VITALS_EVENTS`]
/// vitals events have been recorded in the 14-day window — there isn't enough
/// signal to evaluate the correction-rate gate without false-positives. This
/// replaced the old bool return that conflated "not enough data" with "failed".
pub fn check_stage2_gates(
    state: &EvolutionState,
    bond_score: u8,
    correction_rate: f64,
    archetype_stable_days: u32,
    vitals_events_total: u32,
) -> GateState {
    if state.stage != Stage::Evolved {
        return GateState::Fail;
    }
    // Lowered from 99 → 80 in the Final-stage rework: the n^1.8 curve made
    // 99 a 6–12 month grind, well past the engagement cliff for active users
    // who'd already demonstrated the other gates' mastery (bond ≥ 55,
    // correction rate < 20%, archetype stable ≥ 14 days). Lvl.99 in Stage 2
    // remains as the `level_99_evolved` milestone for completionists.
    if state.level < 80 {
        return GateState::Fail;
    }
    if bond_score < 55 {
        return GateState::Fail;
    }
    if archetype_stable_days < 14 {
        return GateState::Fail;
    }
    if vitals_events_total < STAGE2_MIN_VITALS_EVENTS {
        return GateState::Deferred;
    }
    if correction_rate >= 0.20 {
        return GateState::Fail;
    }
    GateState::Pass
}

/// Compute how many consecutive days the dominant archetype has been stable.
pub async fn compute_archetype_stable_days(db: &Database) -> u32 {
    // Delegates to the trusted replay so the Stage 2→3 gate evaluates against
    // the same archetype timeline that produced the dominant archetype. The
    // previous parallel walker bypassed HMAC verification, rate-limiting, and
    // source decay — it could disagree with `get_evolution_state()`.
    let state = match db.get_evolution_state().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("evolution: failed to get evolution state: {e}");
            return 0;
        }
    };
    let Some((shift_ts, _)) = state.dominant_history.last() else {
        return 0;
    };
    let now = chrono::Utc::now().timestamp();
    let seconds = (now - shift_ts).max(0) as u64;
    (seconds / SECS_PER_DAY as u64) as u32
}

// Status/history/context renderers live in `evolution::format`. The
// `pub use format::{…}` at the top of this file re-exports them so the
// public API is unchanged.

pub(super) fn capitalize_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

/// Bucket recent xp_gain events by source and return the top-N descending.
/// Used to personalize the LLM naming prompt with the tools the agent actually uses.
async fn top_sources_from_db(db: &Database) -> anyhow::Result<Vec<(String, u32)>> {
    let events = db.load_all_evolution_events().await?;
    let mut counts: HashMap<String, u32> = HashMap::new();
    for e in &events {
        if e.event_type == "xp_gain" {
            *counts.entry(e.source.clone()).or_insert(0) += 1;
        }
    }
    let mut sorted: Vec<(String, u32)> = counts.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));
    sorted.truncate(5);
    Ok(sorted)
}

/// Load the current evolution + bond state as a snapshot pair. Returns
/// `(None, 0)` for the evolution slot if the DB read fails; callers should
/// treat that as a skip signal for the downstream milestone diff.
async fn snapshot_evolution_and_bond(db: &Database) -> (Option<EvolutionState>, u8) {
    let evo = match db.get_evolution_state().await {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("evolution: failed to snapshot state for milestone diff: {e}");
            None
        }
    };
    let bond_events = match db.get_all_bond_events().await {
        Ok(events) => events,
        Err(e) => {
            tracing::warn!("evolution: failed to snapshot bond events for milestone diff: {e}");
            return (evo, 0);
        }
    };
    let bond_key = db.derive_hmac_key(crate::bond::BOND_HMAC_DOMAIN);
    let bond = crate::bond::replay_events_with_key(&bond_key, &bond_events);
    (evo, bond.score)
}

/// Apply the live posture multiplier to a base+align XP grant and produce
/// the metadata payload for `evolution_events.metadata_json`.
///
/// Read-then-write is racy under concurrent posture transitions, but the
/// emitted `XpBreakdown` records both the resolved state and its
/// `posture_event_seq`, so replay can re-derive the same `final_xp`
/// deterministically from the chain.
pub(crate) async fn build_posture_aware_xp(
    db: &Database,
    base: i32,
    align_bonus: i32,
) -> (i32, String) {
    let base_u = base.max(0) as u32;
    let align_u = align_bonus.max(0) as u32;
    let (state, posture_seq, fallback) = security::current_posture_for_xp(db).await;
    let breakdown = security::compute_xp_breakdown(base_u, align_u, state, posture_seq, fallback);
    let metadata = serde_json::json!({
        "xp_breakdown": breakdown,
        "posture_event_seq": posture_seq,
    })
    .to_string();
    (breakdown.final_xp as i32, metadata)
}

/// Persist a single milestone: a `level_up` row (for level thresholds), a
/// `milestone_unlocked` row, and the paired `pending_celebration`.
async fn emit_milestone(db: &Database, milestone: &Milestone, next: &EvolutionState) {
    let stage_str = next.stage.to_string();

    // `secure_headless_setup` carries a +100 XP bonus on top of the
    // ongoing posture multiplier difference. Emit the `milestone_bonus`
    // event BEFORE the `milestone_unlocked` row so the feed renders
    // bonus-then-marker in the natural order, and so an audit reader
    // sees XP land before its dedup marker.
    if milestone.id == "secure_headless_setup" {
        let arch = milestone.archetype.map(|a| a.to_string());
        if let Err(e) = db
            .record_evolution_event(
                "milestone_bonus",
                SECURE_HEADLESS_SETUP_XP,
                arch.as_deref(),
                "secure_headless_setup",
                None,
            )
            .await
        {
            tracing::warn!(
                "evolution: failed to record milestone_bonus for {}: {e}",
                milestone.id
            );
            // Continue — we still want the milestone_unlocked dedup row
            // so we don't re-emit the bonus next tick on a write hiccup.
        }
    }

    // `level_*` ids get a paired `level_up` event so the XP feed has a row
    // to render at the exact level boundary.
    if milestone.id.starts_with("level_") {
        if let Some(threshold) = parse_level_from_id(&milestone.id) {
            let meta = serde_json::json!({
                "from_level": threshold.saturating_sub(1),
                "to_level": threshold,
                "stage": stage_str,
            })
            .to_string();
            if let Err(e) = db
                .record_evolution_event("level_up", 0, None, "hook", Some(&meta))
                .await
            {
                tracing::warn!(
                    "evolution: failed to record level_up for {}: {e}",
                    milestone.id
                );
            }
        }
    }

    let arch_str = milestone.archetype.map(|a| a.to_string());
    let meta = serde_json::json!({
        "milestone_id": milestone.id,
        "title": milestone.title,
        "archetype": arch_str,
    })
    .to_string();
    if let Err(e) = db
        .record_evolution_event(
            "milestone_unlocked",
            0,
            arch_str.as_deref(),
            "hook",
            Some(&meta),
        )
        .await
    {
        tracing::warn!(
            "evolution: failed to record milestone_unlocked for {}: {e}",
            milestone.id
        );
        return;
    }

    let payload = celebration::MilestonePayload {
        milestone_id: milestone.id.clone(),
        title: milestone.title.clone(),
        level: next.level,
        stage: stage_str,
        archetype: arch_str,
    };
    match serde_json::to_string(&payload) {
        Ok(json) => {
            if let Err(e) = db.insert_pending_celebration("milestone", &json).await {
                tracing::warn!(
                    "evolution: failed to insert milestone celebration for {}: {e}",
                    milestone.id
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                "evolution: failed to serialize milestone payload for {}: {e}",
                milestone.id
            );
        }
    }
}

/// Extract the numeric level from a `level_{N}_{stage}` milestone id.
fn parse_level_from_id(id: &str) -> Option<u8> {
    let rest = id.strip_prefix("level_")?;
    let (num, _) = rest.split_once('_')?;
    num.parse::<u8>().ok()
}

/// Lifecycle hook that passively records evolution XP events and injects context.
pub struct EvolutionHook {
    /// Database handle wrapped in a Mutex for thread-safety.
    db: AsyncMutex<Database>,
    /// Cached `evolution.enabled` flag. Read once at construction; toggling the
    /// setting takes effect on the next session (no hot reload).
    enabled: bool,
    /// Handle to the tokio runtime for spawning the async naming task, if one
    /// is active at construction time. `None` in pure-sync contexts (some tests).
    rt: Option<tokio::runtime::Handle>,
}

impl EvolutionHook {
    pub async fn new() -> anyhow::Result<Self> {
        let enabled = crate::config::Config::load_from_db()
            .await
            .map(|c| c.evolution.enabled)
            .unwrap_or(true);
        Ok(Self {
            db: AsyncMutex::new(Database::open().await?),
            enabled,
            rt: tokio::runtime::Handle::try_current().ok(),
        })
    }

    /// Construct a hook with explicit overrides — test seam.
    #[cfg(test)]
    pub fn new_with(db: Database, enabled: bool) -> Self {
        Self {
            db: AsyncMutex::new(db),
            enabled,
            rt: tokio::runtime::Handle::try_current().ok(),
        }
    }

    async fn record_xp(
        &self,
        source: &str,
        archetype: Option<Archetype>,
        base: i32,
        align_bonus: i32,
    ) {
        let db = self.db.lock().await;

        // Snapshot pre-state so we can diff against post-state for milestones.
        let (prev_evo, prev_bond) = snapshot_evolution_and_bond(&db).await;

        let arch_str = archetype.map(|a| a.to_string());
        let (final_xp, metadata) = build_posture_aware_xp(&db, base, align_bonus).await;
        if let Err(e) = db
            .record_evolution_event(
                "xp_gain",
                final_xp,
                arch_str.as_deref(),
                source,
                Some(&metadata),
            )
            .await
        {
            tracing::warn!("evolution: failed to record XP event: {e}");
            return;
        }
        // Check if we should attempt an evolution after this XP gain
        self.attempt_evolution(&db).await;

        // Detect sub-evolution milestones against the post-state. Runs after
        // evolution attempt so a stage transition feeds into the diff.
        let (next_evo, next_bond) = snapshot_evolution_and_bond(&db).await;
        if let (Some(prev), Some(next)) = (prev_evo, next_evo) {
            self.detect_milestones(&db, &prev, &next, prev_bond, next_bond)
                .await;
        }
    }

    /// Compare pre/post evolution + bond snapshots and emit `level_up` /
    /// `milestone_unlocked` events plus mini-celebration rows for any newly
    /// unlocked milestones. Wrapped as a helper so sibling detection passes
    /// (e.g. mood) can sequence cleanly alongside.
    async fn detect_milestones(
        &self,
        db: &Database,
        prev: &EvolutionState,
        next: &EvolutionState,
        bond_prev: u8,
        bond_next: u8,
    ) {
        let events = match db.load_all_evolution_events().await {
            Ok(events) => events,
            Err(e) => {
                tracing::warn!("evolution: failed to load events for milestone check: {e}");
                return;
            }
        };
        let now = chrono::Utc::now().timestamp();
        let (current_posture_state, _, fallback) = security::current_posture_for_xp(db).await;
        // Suppress posture-derived milestones when no posture event exists yet
        // (fresh DB) — `current_posture_for_xp` falls back to `Default` for XP
        // math, but milestones must wait until live posture is real.
        let posture_ctx = if fallback.is_some() {
            None
        } else {
            // `previous_was_file_encrypted` is a "did this install ever
            // run on FileEncrypted" question — answered authoritatively
            // by the audit chain. Use the chain only for that.
            let history = security::load_posture_history(db).await;
            let previous_was_file_encrypted = history.iter().any(|s| {
                s.global.secret_storage_backend == security::SecretStorageBackend::FileEncrypted
            });
            // For the *current* backend, prefer the live config: posture
            // emission is rate-limited at 2/hr (see emit.rs), so the
            // chain tail can lag the actual setting by minutes after a
            // backend swap. Reading from `Config::load_from_db` matches
            // what the next snapshot will record, so the milestone gate
            // doesn't misfire on the boundary.
            let current_backend = match crate::config::Config::load_from_db().await {
                Ok(c) => match c.security.kek_backend {
                    crate::config::security::KekBackend::OsKeychain
                    | crate::config::security::KekBackend::Auto => {
                        security::SecretStorageBackend::OsKeychain
                    }
                    crate::config::security::KekBackend::File => {
                        security::SecretStorageBackend::FileEncrypted
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        "evolution: kek_backend lookup failed ({e}); falling back to chain tail"
                    );
                    history
                        .last()
                        .map(|s| s.global.secret_storage_backend)
                        .unwrap_or(security::SecretStorageBackend::OsKeychain)
                }
            };
            Some(milestones::PostureContext {
                current: current_posture_state,
                current_backend,
                previous_was_file_encrypted,
            })
        };
        let unlocked =
            check_milestones(prev, next, bond_prev, bond_next, &events, now, posture_ctx);
        for milestone in unlocked {
            emit_milestone(db, &milestone, next).await;
        }
    }

    /// Check evolution gates and record an evolution event if all prerequisites are met.
    async fn attempt_evolution(&self, db: &Database) {
        let evo_state = match db.get_evolution_state().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("evolution: failed to get evolution state: {e}");
                return;
            }
        };

        // Only trigger at level 99 (stage cap)
        if evo_state.level < 99 {
            return;
        }

        // Already at final stage
        if evo_state.stage == Stage::Final {
            return;
        }

        // Get bond state for gate checks (use derived key for HMAC verification)
        let bond_events = match db.get_all_bond_events().await {
            Ok(events) => events,
            Err(e) => {
                tracing::warn!("evolution: failed to load bond events: {e}");
                return;
            }
        };
        let bond_key = db.derive_hmac_key(crate::bond::BOND_HMAC_DOMAIN);
        let bond_state = crate::bond::replay_events_with_key(&bond_key, &bond_events);

        // Get vitals state for gate checks
        let vitals_state = match db.get_vitals_state().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("evolution: failed to get vitals state: {e}");
                return;
            }
        };
        let min_vital = vitals_state
            .stability
            .min(vitals_state.focus)
            .min(vitals_state.sync)
            .min(vitals_state.growth)
            .min(vitals_state.happiness);

        let gates_passed = match evo_state.stage {
            Stage::Base => check_stage1_gates(&evo_state, bond_state.score, min_vital),
            Stage::Evolved => {
                // Compute correction rate from vitals events (last 14 days)
                // Includes both corrections and negative sentiment
                let fourteen_days_ago = chrono::Utc::now().timestamp() - 14 * SECS_PER_DAY;
                let (corrections, total) = match db
                    .count_vitals_events_by_category_since(fourteen_days_ago, "correction")
                    .await
                {
                    Ok(pair) => pair,
                    Err(e) => {
                        tracing::warn!("evolution: count corrections failed: {e}");
                        (0, 0)
                    }
                };
                let negatives = match db
                    .count_vitals_events_by_category_since(fourteen_days_ago, "negative_sentiment")
                    .await
                {
                    Ok((n, _)) => n,
                    Err(e) => {
                        tracing::warn!("evolution: count negative_sentiment failed: {e}");
                        0
                    }
                };
                let correction_rate = if total > 0 {
                    (corrections + negatives) as f64 / total as f64
                } else {
                    0.0
                };
                let archetype_stable_days = compute_archetype_stable_days(db).await;
                matches!(
                    check_stage2_gates(
                        &evo_state,
                        bond_state.score,
                        correction_rate,
                        archetype_stable_days,
                        total,
                    ),
                    GateState::Pass
                )
            }
            Stage::Final => false,
        };

        if !gates_passed {
            return;
        }

        let metadata =
            serde_json::json!({ "gates_verified": true, "pending_name": true }).to_string();
        if let Err(e) = db
            .record_evolution_event("evolution", 0, None, "gate_check", Some(&metadata))
            .await
        {
            tracing::warn!("evolution: failed to record evolution event: {e}");
            return;
        }
        tracing::info!("evolution: stage transition triggered — gates verified");

        // Kick off async LLM naming. The stage transition has already been
        // recorded, so the replay reports the new stage immediately; the
        // generated name lands later as a `classification` event that replay
        // folds into `evolution_name` / `evolution_description`.
        let new_stage = match evo_state.stage {
            Stage::Base => Stage::Evolved,
            Stage::Evolved => Stage::Final,
            Stage::Final => Stage::Final,
        };
        self.spawn_evolution_naming(new_stage, evo_state.dominant_archetype)
            .await;

        let to_stage = match evo_state.stage {
            Stage::Base => "evolved",
            Stage::Evolved => "final",
            Stage::Final => "final",
        };
        let celebration = CelebrationPayload {
            from_stage: evo_state.stage.to_string(),
            to_stage: to_stage.to_string(),
            evolution_name: evo_state.evolution_name.clone(),
            evolution_description: evo_state.evolution_description.clone(),
            dominant_archetype: evo_state.dominant_archetype.map(|a| a.to_string()),
            bond_score: bond_state.score,
            stability: vitals_state.stability,
            focus: vitals_state.focus,
            sync_stat: vitals_state.sync,
            growth: vitals_state.growth,
            happiness: vitals_state.happiness,
        };
        match serde_json::to_string(&celebration) {
            Ok(payload_json) => {
                if let Err(e) = db
                    .insert_pending_celebration("evolution", &payload_json)
                    .await
                {
                    tracing::warn!("evolution: failed to insert pending celebration: {e}");
                }
            }
            Err(e) => {
                tracing::warn!("evolution: failed to serialize celebration payload: {e}");
            }
        }
    }

    /// Spawn a detached task to generate an evolution name + description.
    /// Falls back to a deterministic name on any error. No-op if no tokio
    /// runtime is available (e.g. pure-sync test context).
    async fn spawn_evolution_naming(&self, new_stage: Stage, archetype: Option<Archetype>) {
        let Some(rt) = self.rt.clone() else {
            // No runtime: record the fallback synchronously via a blocking DB call.
            let (name, description) = classification::fallback_evolution_name(archetype, new_stage);
            self.record_naming_event(archetype, &name, &description)
                .await;
            return;
        };
        // Compute top tools under the already-held DB lock would deadlock; do
        // it lazily inside the task with a fresh Database::open().
        let archetype_copy = archetype;
        rt.spawn(async move {
            let top_tools = match Database::open().await {
                Ok(db) => top_sources_from_db(&db).await.unwrap_or_default(),
                Err(_) => Vec::new(),
            };
            let (name, description) =
                classification::generate_evolution_name(archetype_copy, new_stage, &top_tools)
                    .await;
            // Open a fresh DB handle inside the async task (original hook DB is behind a Mutex).
            match Database::open().await {
                Ok(db) => {
                    let arch_str = archetype_copy.map(|a| a.to_string());
                    let metadata =
                        serde_json::json!({ "name": name, "description": description }).to_string();
                    if let Err(e) = db
                        .record_evolution_event(
                            "classification",
                            0,
                            arch_str.as_deref(),
                            "llm_naming",
                            Some(&metadata),
                        )
                        .await
                    {
                        tracing::warn!("evolution: failed to record naming event: {e}");
                    }
                }
                Err(e) => {
                    tracing::warn!("evolution: failed to open DB for naming event: {e}");
                }
            }
        });
    }

    /// Synchronous naming record — used when no tokio runtime is available.
    async fn record_naming_event(
        &self,
        archetype: Option<Archetype>,
        name: &str,
        description: &str,
    ) {
        let db = self.db.lock().await;
        let arch_str = archetype.map(|a| a.to_string());
        let metadata = serde_json::json!({ "name": name, "description": description }).to_string();
        if let Err(e) = db
            .record_evolution_event(
                "classification",
                0,
                arch_str.as_deref(),
                "llm_naming",
                Some(&metadata),
            )
            .await
        {
            tracing::warn!("evolution: failed to record naming event: {e}");
        }
    }

    /// Snapshot the current `Mood` by opening the DB and replaying vitals +
    /// bond + evolution state. Returns `None` if any load fails — callers
    /// treat that as "no transition to emit".
    async fn snapshot_mood(&self) -> Option<Mood> {
        let db = self.db.lock().await;
        let evo = db.get_evolution_state().await.ok()?;
        let vitals = db.get_vitals_state().await.ok()?;
        let bond_events = db.get_all_bond_events().await.ok()?;
        let bond_key = db.derive_hmac_key(crate::bond::BOND_HMAC_DOMAIN);
        let bond = crate::bond::replay_events_with_key(&bond_key, &bond_events);
        Some(compute_mood(&evo, &vitals, &bond))
    }

    /// Emit a `mood_changed` event when the computed mood flipped between
    /// tool calls. `reason` names the source that triggered the transition
    /// (typically the tool name). Rate-limited at 5/hr by `rate_limit_for`.
    async fn emit_mood_changed(&self, from: Mood, to: Mood, reason: &str) {
        let db = self.db.lock().await;
        let metadata = serde_json::json!({
            "from_mood": from.to_string(),
            "to_mood": to.to_string(),
            "reason": reason,
        })
        .to_string();
        if let Err(e) = db
            .record_evolution_event("mood_changed", 0, None, reason, Some(&metadata))
            .await
        {
            tracing::warn!("evolution: failed to record mood_changed event: {e}");
        }
    }
}

#[async_trait]
impl Hook for EvolutionHook {
    fn name(&self) -> &str {
        "evolution"
    }

    fn points(&self) -> &[HookPoint] {
        // We deliberately do NOT inject `<evolution_context>` per-turn on
        // BeforeAgentStart / BeforeLlmCall: archetype claims at request time
        // miscast the agent on cross-domain tasks. The surfaces that actually
        // benefit from archetype context (heartbeat seeds, memory
        // consolidation, skill priority) read evolution state directly.
        // `format_evolution_context` is still used by status surfaces.
        &[HookPoint::SessionStart, HookPoint::AfterToolCall]
    }

    async fn execute(&self, ctx: &HookContext) -> HookAction {
        if !self.enabled {
            return HookAction::Continue;
        }
        match &ctx.data {
            HookData::SessionStart { .. } => {
                // Record interaction XP
                self.record_xp("session_start", None, BASE_XP_INTERACTION, 0)
                    .await;
            }
            HookData::AgentStart { .. } | HookData::LlmCall { .. } => {
                // No-op: see `points()` for why per-turn injection is omitted.
            }
            HookData::ToolResult {
                name,
                is_error,
                result,
                ..
            } => {
                if !*is_error {
                    let archetype = classify_tool_archetype(name, Some(result.as_str()));
                    let is_creation = matches!(
                        name.as_str(),
                        "apply_patch" | "apply_skill_patch" | "create_channel" | "write_memory"
                    );
                    let (base, align_bonus) = if is_creation {
                        let bonus = if archetype.is_some() {
                            BONUS_XP_CREATION_ALIGNED
                        } else {
                            0
                        };
                        (BASE_XP_CREATION, bonus)
                    } else {
                        let bonus = if archetype.is_some() {
                            BONUS_XP_ALIGNED
                        } else {
                            0
                        };
                        (BASE_XP_TOOL_SUCCESS, bonus)
                    };

                    let mood_before = self.snapshot_mood().await;
                    self.record_xp(name, archetype, base, align_bonus).await;

                    if let Some(before) = mood_before {
                        if let Some(after) = self.snapshot_mood().await {
                            if before != after {
                                self.emit_mood_changed(before, after, name).await;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        HookAction::Continue
    }
}

impl std::fmt::Debug for EvolutionHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EvolutionHook").finish()
    }
}

#[cfg(all(test, feature = "phase9-tests-disabled"))]
mod tests;
