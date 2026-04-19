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
mod milestones;
mod replay;
pub mod share_card;

pub use celebration::{
    celebration_art, format_celebration, format_celebration_message, CelebrationArt,
    CelebrationKind, CelebrationPayload, MilestonePayload,
};
pub use classification::*;
pub use commands::{dispatch, parse, CommandOutput, EvolutionCommand};
pub use feed::{recent_xp_feed, xp_summary, FeedEntry, FeedKind, XpSummary};
pub use format::{
    format_archetype_scores_with_momentum, format_evolution_overview, format_next_step_hints,
    format_readiness, format_xp_feed, format_xp_summary,
};
pub use helpers::{compute_momentum, compute_mood, compute_readiness, next_step_hints, render_bar};
pub use milestones::{check_milestones, Milestone};
pub use replay::*;
pub use share_card::render_ascii_card;

use std::collections::HashMap;
use std::fmt;

use crate::db::Database;
use crate::hmac_chain;
use crate::hooks::{Hook, HookAction, HookContext, HookData, HookPoint};

// ── HMAC ──

/// Domain string for HMAC key derivation. Combined with per-installation salt.
pub(crate) const EVOLUTION_HMAC_DOMAIN: &[u8] = b"borg-evolution-chain-v1";

/// Legacy compiled-in secret for installations without per-install salt.
#[cfg(test)]
const EVOLUTION_HMAC_LEGACY: &[u8] = b"borg-evolution-chain-v1";

/// Compute HMAC for an evolution event (v2: includes metadata).
#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_event_hmac(
    key: &[u8],
    prev_hmac: &str,
    event_type: &str,
    xp_delta: i32,
    archetype: &str,
    source: &str,
    metadata: &str,
    created_at: i64,
) -> String {
    hmac_chain::compute_hmac(
        key,
        &[
            prev_hmac.as_bytes(),
            event_type.as_bytes(),
            &xp_delta.to_le_bytes(),
            archetype.as_bytes(),
            source.as_bytes(),
            metadata.as_bytes(),
            &created_at.to_le_bytes(),
        ],
    )
}

/// Legacy HMAC computation (v1: without metadata). Used for backward-compat verification.
fn compute_event_hmac_legacy(
    key: &[u8],
    prev_hmac: &str,
    event_type: &str,
    xp_delta: i32,
    archetype: &str,
    source: &str,
    created_at: i64,
) -> String {
    hmac_chain::compute_hmac(
        key,
        &[
            prev_hmac.as_bytes(),
            event_type.as_bytes(),
            &xp_delta.to_le_bytes(),
            archetype.as_bytes(),
            source.as_bytes(),
            &created_at.to_le_bytes(),
        ],
    )
}

/// Verify an event's HMAC against the expected chain.
/// Tries v2 (with metadata) first, falls back to v1 (legacy) for existing events.
fn verify_event_hmac(key: &[u8], event: &EvolutionEvent, expected_prev_hmac: &str) -> bool {
    let meta = event.metadata_json.as_deref().unwrap_or("");
    let archetype = event.archetype.as_deref().unwrap_or("");

    // Try v2 HMAC (includes metadata)
    let recomputed_v2 = compute_event_hmac(
        key,
        &event.prev_hmac,
        &event.event_type,
        event.xp_delta,
        archetype,
        &event.source,
        meta,
        event.created_at,
    );
    if hmac_chain::verify_chain_link(
        &event.hmac,
        &event.prev_hmac,
        expected_prev_hmac,
        &recomputed_v2,
    ) {
        return true;
    }

    // Fall back to v1 HMAC (legacy, without metadata)
    let recomputed_v1 = compute_event_hmac_legacy(
        key,
        &event.prev_hmac,
        &event.event_type,
        event.xp_delta,
        archetype,
        &event.source,
        event.created_at,
    );
    hmac_chain::verify_chain_link(
        &event.hmac,
        &event.prev_hmac,
        expected_prev_hmac,
        &recomputed_v1,
    )
}

// ── Rate Limiting ──

/// Maximum events per bucket per hour during replay.
pub(crate) fn rate_limit_for(event_type: &str) -> u32 {
    match event_type {
        "xp_gain" => 15,
        "evolution" => 3,
        "classification" => 3,
        "archetype_shift" => 5,
        "level_up" => 10,
        "milestone_unlocked" => 3,
        "mood_changed" => 5,
        "share_card_created" => 3,
        _ => 10,
    }
}

// ── Types ──

/// The 10 archetypes that classify usage patterns.
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
}

impl Archetype {
    /// All archetype variants in definition order.
    pub const ALL: [Archetype; 10] = [
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
    /// HMAC for this event in the chain.
    pub hmac: String,
    /// HMAC of the previous event in the chain.
    pub prev_hmac: String,
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
    /// `lifetime * 0.35 + last_30d * 0.65` and rounded to u32.
    /// This is what drives dominant-archetype selection and the 1.3× gate.
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

// ── XP Curve ──
//
// Level/XP math lives in `evolution::xp`; re-exported here for call-site
// compatibility.

mod xp;
pub use xp::{level_from_xp, total_xp_for_level, xp_for_level};

// ── Scoring ──

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
/// Valid evolution event types.
pub(crate) const VALID_EVOLUTION_EVENT_TYPES: &[&str] = &[
    "xp_gain",
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
#[cfg(test)]
fn dominant_archetype(scores: &HashMap<Archetype, u32>) -> Option<Archetype> {
    scores
        .iter()
        .max_by_key(|(_, &score)| score)
        .filter(|(_, &score)| score > 0)
        .map(|(&arch, _)| arch)
}

// ── Evolution Gates ──

/// Check if Stage 1→2 evolution gates are met.
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

    // Dominant archetype must be ≥ 1.3x runner-up
    let mut scores: Vec<u32> = state.archetype_scores.values().copied().collect();
    scores.sort_unstable_by(|a, b| b.cmp(a));
    if scores.is_empty() || scores[0] == 0 {
        return false;
    }
    let runner_up = if scores.len() > 1 { scores[1] } else { 0 };
    // Allow evolution if runner_up is 0 (only one archetype used)
    if runner_up > 0 && (scores[0] as f64) < (runner_up as f64 * 1.3) {
        return false;
    }

    true
}

/// Check if Stage 2→3 evolution gates are met.
pub fn check_stage2_gates(
    state: &EvolutionState,
    bond_score: u8,
    correction_rate: f64,
    archetype_stable_days: u32,
) -> bool {
    if state.stage != Stage::Evolved {
        return false;
    }
    if state.level < 99 {
        return false;
    }
    if bond_score < 55 {
        return false;
    }
    if correction_rate >= 0.20 {
        return false;
    }
    if archetype_stable_days < 14 {
        return false;
    }

    true
}

/// Compute how many consecutive days the dominant archetype has been stable.
pub fn compute_archetype_stable_days(db: &Database) -> u32 {
    // Delegates to the trusted replay so the Stage 2→3 gate evaluates against
    // the same archetype timeline that produced the dominant archetype. The
    // previous parallel walker bypassed HMAC verification, rate-limiting, and
    // source decay — it could disagree with `get_evolution_state()`.
    let state = match db.get_evolution_state() {
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
    (seconds / 86400) as u32
}

// ── Formatting ──

/// Compact one-liner for TUI session header.
pub fn format_compact(state: &EvolutionState) -> String {
    match (&state.evolution_name, &state.dominant_archetype) {
        (Some(name), Some(arch)) => {
            let arch_display = format!("{arch}");
            let capitalized = capitalize_first(&arch_display);
            format!("{name} Lvl.{} | {capitalized}", state.level)
        }
        (Some(name), None) => format!("{name} Lvl.{}", state.level),
        (None, Some(arch)) => {
            let arch_display = format!("{arch}");
            let capitalized = capitalize_first(&arch_display);
            format!("Base Borg Lvl.{} | {capitalized}", state.level)
        }
        (None, None) => format!("Base Borg Lvl.{}", state.level),
    }
}

/// Full status section for `borg status` output (default width).
pub fn format_status_section(state: &EvolutionState) -> String {
    format_status_section_with_width(state, 48)
}

/// Full status section with configurable card width.
///
/// `card_width` is the total width of the tip card including borders (minimum 34).
pub fn format_status_section_with_width(state: &EvolutionState, card_width: usize) -> String {
    let card_width = card_width.max(34);
    let mut out = String::new();

    // Header: name + level
    match &state.evolution_name {
        Some(name) => out.push_str(&format!("  {name} Lvl.{}\n", state.level)),
        None => out.push_str(&format!("  Base Borg Lvl.{}\n", state.level)),
    }

    // Description
    match &state.evolution_description {
        Some(desc) => out.push_str(&format!("  \"{desc}\"\n")),
        None => {
            let inner = card_width - 2; // space between │ and │
            let title = " How Evolution Works ";
            let title_len = title.len(); // 21
            let left_dashes = 3;
            let right_dashes = inner.saturating_sub(left_dashes + title_len);

            out.push('\n');
            // Top border
            let left = "\u{2500}".repeat(left_dashes);
            let right = "\u{2500}".repeat(right_dashes);
            out.push_str(&format!("  \u{256D}{left}{title}{right}\u{256E}\n"));

            let lines = [
                "",
                "Your borg is learning how you use it.",
                "Every tool call, shell command, and task",
                "shapes what it becomes.",
                "",
                "Evolution is permanent -- earned through",
                "sustained usage, not toggled. Your usage",
                "patterns determine your borg's archetype",
                "and unlock a unique evolution name.",
                "",
                "Keep using borg the way you imagine.",
                "",
            ];
            for line in &lines {
                if line.is_empty() {
                    out.push_str(&format!("  \u{2502}{}\u{2502}\n", " ".repeat(inner)));
                } else {
                    // inner >= 32 because card_width >= 34
                    let padded = format!("  {:<width$}", line, width = inner - 2);
                    // Truncate if content is wider than available space
                    let padded: String = padded.chars().take(inner).collect();
                    out.push_str(&format!("  \u{2502}{padded}\u{2502}\n"));
                }
            }

            // Bottom border
            out.push_str(&format!("  \u{2570}{}\u{256F}\n", "\u{2500}".repeat(inner)));
        }
    }

    out.push('\n');

    // Stage progress bar — scale bar to fit card width
    let bar_width = (card_width - 2).min(30); // bar portion, max 30
    let stage_label = match state.stage {
        Stage::Base => "Base (1/3)",
        Stage::Evolved => "Evolved (2/3)",
        Stage::Final => "Final (3/3)",
    };
    let stage_fill = match state.stage {
        Stage::Base => bar_width / 3,
        Stage::Evolved => bar_width * 2 / 3,
        Stage::Final => bar_width,
    };
    let stage_bar = format!(
        "{}{}",
        "\u{2588}".repeat(stage_fill),
        "\u{2591}".repeat(bar_width - stage_fill)
    );
    out.push_str(&format!("  Stage        {stage_bar}  {stage_label}\n"));

    // XP progress
    if state.level < 99 {
        let xp_needed = xp_for_level(&state.stage, state.level);
        let xp_into_level = xp_needed.saturating_sub(state.xp_to_next_level);
        out.push_str(&format!(
            "  XP           {xp_into_level} / {xp_needed} to Lvl.{}\n",
            state.level + 1
        ));
    } else {
        out.push_str("  XP           MAX LEVEL\n");
    }

    out
}

/// Format archetype scores for `borg status archetypes`.
pub fn format_archetype_scores(state: &EvolutionState) -> String {
    let mut out = String::from("Archetype Scores\n");

    let mut sorted: Vec<(Archetype, u32)> = Archetype::ALL
        .iter()
        .map(|a| (*a, *state.archetype_scores.get(a).unwrap_or(&0)))
        .collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));

    let max_score = sorted.first().map(|(_, s)| *s).unwrap_or(1).max(1);

    for (arch, score) in &sorted {
        let arch_display = format!("{arch}");
        let capitalized = capitalize_first(&arch_display);
        let bar_len = (*score as usize * 10) / max_score as usize;
        let bar = format!(
            "{}{}",
            "\u{2588}".repeat(bar_len),
            "\u{2591}".repeat(10 - bar_len)
        );
        let marker = if Some(*arch) == state.dominant_archetype {
            " *"
        } else {
            ""
        };
        out.push_str(&format!("  {capitalized:<15} {score:>5}  {bar}{marker}\n"));
    }

    out
}

/// Format evolution history timeline.
pub fn format_history(events: &[EvolutionEvent]) -> String {
    let evolution_events: Vec<&EvolutionEvent> = events
        .iter()
        .filter(|e| e.event_type == "evolution")
        .collect();

    if evolution_events.is_empty() {
        return "Evolution History\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n  No evolutions yet. Keep using Borg!\n".to_string();
    }

    let mut out = String::from("Evolution History\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n");

    for event in &evolution_events {
        let ts = chrono::DateTime::from_timestamp(event.created_at, 0)
            .map(|dt| dt.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let mut name = String::new();
        let mut desc = String::new();
        if let Some(ref meta) = event.metadata_json {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(meta) {
                if let Some(n) = parsed.get("name").and_then(|v| v.as_str()) {
                    name = n.to_string();
                }
                if let Some(d) = parsed.get("description").and_then(|v| v.as_str()) {
                    desc = d.to_string();
                }
            }
        }

        let stage_label = if name.is_empty() {
            "Evolved".to_string()
        } else {
            name.clone()
        };

        out.push_str(&format!("  {ts}  → {stage_label}\n"));
        if !desc.is_empty() {
            out.push_str(&format!("           \"{desc}\"\n"));
        }
    }

    out
}

/// XML evolution context for system prompt injection.
pub fn format_evolution_context(state: &EvolutionState) -> String {
    let name = state.evolution_name.as_deref().unwrap_or("Base Borg");
    let stage = match state.stage {
        Stage::Base => "Base",
        Stage::Evolved => "Evolved",
        Stage::Final => "Final",
    };
    let arch = state
        .dominant_archetype
        .map(|a| {
            let s = format!("{a}");
            let score = state.archetype_scores.get(&a).unwrap_or(&0);
            format!("\nArchetype: {} (score: {score})", capitalize_first(&s))
        })
        .unwrap_or_default();

    format!(
        "<evolution_context>\nStage: {stage} | {name} Lvl.{}{arch}\n</evolution_context>",
        state.level
    )
}

pub(super) fn capitalize_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

/// Bucket recent xp_gain events by source and return the top-N descending.
/// Used to personalize the LLM naming prompt with the tools the agent actually uses.
fn top_sources_from_db(db: &Database) -> anyhow::Result<Vec<(String, u32)>> {
    let events = db.load_all_evolution_events()?;
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

// ── Milestone helpers ──

/// Load the current evolution + bond state as a snapshot pair. Returns
/// `(None, 0)` for the evolution slot if the DB read fails; callers should
/// treat that as a skip signal for the downstream milestone diff.
fn snapshot_evolution_and_bond(db: &Database) -> (Option<EvolutionState>, u8) {
    let evo = match db.get_evolution_state() {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("evolution: failed to snapshot state for milestone diff: {e}");
            None
        }
    };
    let bond_events = match db.get_all_bond_events() {
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

/// Persist a single milestone: a `level_up` row (for level thresholds), a
/// `milestone_unlocked` row, and the paired `pending_celebration`.
fn emit_milestone(db: &Database, milestone: &Milestone, next: &EvolutionState) {
    let stage_str = next.stage.to_string();

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
            if let Err(e) = db.record_evolution_event("level_up", 0, None, "hook", Some(&meta)) {
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
    if let Err(e) = db.record_evolution_event(
        "milestone_unlocked",
        0,
        arch_str.as_deref(),
        "hook",
        Some(&meta),
    ) {
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
            if let Err(e) = db.insert_pending_celebration("milestone", &json) {
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

// ── EvolutionHook ──

/// Lifecycle hook that passively records evolution XP events and injects context.
pub struct EvolutionHook {
    /// Database handle wrapped in a Mutex for thread-safety.
    db: std::sync::Mutex<Database>,
    /// Cached `evolution.enabled` flag. Read once at construction; toggling the
    /// setting takes effect on the next session (no hot reload).
    enabled: bool,
    /// Handle to the tokio runtime for spawning the async naming task, if one
    /// is active at construction time. `None` in pure-sync contexts (some tests).
    rt: Option<tokio::runtime::Handle>,
}

impl EvolutionHook {
    /// Create a new evolution hook, opening a database connection.
    pub fn new() -> anyhow::Result<Self> {
        let enabled = crate::config::Config::load_from_db()
            .map(|c| c.evolution.enabled)
            .unwrap_or(true);
        Ok(Self {
            db: std::sync::Mutex::new(Database::open()?),
            enabled,
            rt: tokio::runtime::Handle::try_current().ok(),
        })
    }

    /// Construct a hook with explicit overrides — test seam.
    #[cfg(test)]
    pub fn new_with(db: Database, enabled: bool) -> Self {
        Self {
            db: std::sync::Mutex::new(db),
            enabled,
            rt: tokio::runtime::Handle::try_current().ok(),
        }
    }

    fn record_xp(&self, source: &str, archetype: Option<Archetype>, xp: i32) {
        let Ok(db) = self.db.lock() else {
            tracing::warn!("evolution: mutex poisoned, skipping event");
            return;
        };

        // Snapshot pre-state so we can diff against post-state for milestones.
        let (prev_evo, prev_bond) = snapshot_evolution_and_bond(&db);

        let arch_str = archetype.map(|a| a.to_string());
        if let Err(e) = db.record_evolution_event("xp_gain", xp, arch_str.as_deref(), source, None)
        {
            tracing::warn!("evolution: failed to record XP event: {e}");
            return;
        }
        // Check if we should attempt an evolution after this XP gain
        self.attempt_evolution(&db);

        // Detect sub-evolution milestones against the post-state. Runs after
        // evolution attempt so a stage transition feeds into the diff.
        let (next_evo, next_bond) = snapshot_evolution_and_bond(&db);
        if let (Some(prev), Some(next)) = (prev_evo, next_evo) {
            self.detect_milestones(&db, &prev, &next, prev_bond, next_bond);
        }
    }

    /// Compare pre/post evolution + bond snapshots and emit `level_up` /
    /// `milestone_unlocked` events plus mini-celebration rows for any newly
    /// unlocked milestones. Wrapped as a helper so sibling detection passes
    /// (e.g. mood) can sequence cleanly alongside.
    fn detect_milestones(
        &self,
        db: &Database,
        prev: &EvolutionState,
        next: &EvolutionState,
        bond_prev: u8,
        bond_next: u8,
    ) {
        let events = match db.load_all_evolution_events() {
            Ok(events) => events,
            Err(e) => {
                tracing::warn!("evolution: failed to load events for milestone check: {e}");
                return;
            }
        };
        let now = chrono::Utc::now().timestamp();
        let unlocked = check_milestones(prev, next, bond_prev, bond_next, &events, now);
        for milestone in unlocked {
            emit_milestone(db, &milestone, next);
        }
    }

    /// Check evolution gates and record an evolution event if all prerequisites are met.
    fn attempt_evolution(&self, db: &Database) {
        let evo_state = match db.get_evolution_state() {
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
        let bond_events = match db.get_all_bond_events() {
            Ok(events) => events,
            Err(e) => {
                tracing::warn!("evolution: failed to load bond events: {e}");
                return;
            }
        };
        let bond_key = db.derive_hmac_key(crate::bond::BOND_HMAC_DOMAIN);
        let bond_state = crate::bond::replay_events_with_key(&bond_key, &bond_events);

        // Get vitals state for gate checks
        let vitals_state = match db.get_vitals_state() {
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
                let fourteen_days_ago = chrono::Utc::now().timestamp() - 14 * 86400;
                let (corrections, total) = db
                    .count_vitals_events_by_category_since(fourteen_days_ago, "correction")
                    .unwrap_or((0, 1));
                let negatives = db
                    .count_vitals_events_by_category_since(fourteen_days_ago, "negative_sentiment")
                    .map(|(n, _)| n)
                    .unwrap_or(0);
                let correction_rate = if total > 0 {
                    (corrections + negatives) as f64 / total as f64
                } else {
                    0.0
                };
                // Approximate archetype stable days from first event with current dominant archetype
                let archetype_stable_days = compute_archetype_stable_days(db);
                check_stage2_gates(
                    &evo_state,
                    bond_state.score,
                    correction_rate,
                    archetype_stable_days,
                )
            }
            Stage::Final => false,
        };

        if !gates_passed {
            return;
        }

        let metadata =
            serde_json::json!({ "gates_verified": true, "pending_name": true }).to_string();
        if let Err(e) =
            db.record_evolution_event("evolution", 0, None, "gate_check", Some(&metadata))
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
        self.spawn_evolution_naming(new_stage, evo_state.dominant_archetype);

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
                if let Err(e) = db.insert_pending_celebration("evolution", &payload_json) {
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
    fn spawn_evolution_naming(&self, new_stage: Stage, archetype: Option<Archetype>) {
        let Some(rt) = self.rt.clone() else {
            // No runtime: record the fallback synchronously via a blocking DB call.
            let (name, description) = classification::fallback_evolution_name(archetype, new_stage);
            self.record_naming_event(archetype, &name, &description);
            return;
        };
        // Compute top tools under the already-held DB lock would deadlock; do
        // it lazily inside the task with a fresh Database::open().
        let archetype_copy = archetype;
        rt.spawn(async move {
            let top_tools = match Database::open() {
                Ok(db) => top_sources_from_db(&db).unwrap_or_default(),
                Err(_) => Vec::new(),
            };
            let (name, description) =
                classification::generate_evolution_name(archetype_copy, new_stage, &top_tools)
                    .await;
            // Open a fresh DB handle inside the async task (original hook DB is behind a Mutex).
            match Database::open() {
                Ok(db) => {
                    let arch_str = archetype_copy.map(|a| a.to_string());
                    let metadata =
                        serde_json::json!({ "name": name, "description": description }).to_string();
                    if let Err(e) = db.record_evolution_event(
                        "classification",
                        0,
                        arch_str.as_deref(),
                        "llm_naming",
                        Some(&metadata),
                    ) {
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
    fn record_naming_event(&self, archetype: Option<Archetype>, name: &str, description: &str) {
        let Ok(db) = self.db.lock() else {
            tracing::warn!("evolution: mutex poisoned, skipping naming event");
            return;
        };
        let arch_str = archetype.map(|a| a.to_string());
        let metadata = serde_json::json!({ "name": name, "description": description }).to_string();
        if let Err(e) = db.record_evolution_event(
            "classification",
            0,
            arch_str.as_deref(),
            "llm_naming",
            Some(&metadata),
        ) {
            tracing::warn!("evolution: failed to record naming event: {e}");
        }
    }

    fn evolution_context(&self) -> String {
        let Ok(db) = self.db.lock() else {
            return String::new();
        };
        match db.get_evolution_state() {
            Ok(state) => format_evolution_context(&state),
            Err(_) => String::new(),
        }
    }

    /// Snapshot the current `Mood` by opening the DB and replaying vitals +
    /// bond + evolution state. Returns `None` if any load fails — callers
    /// treat that as "no transition to emit".
    fn snapshot_mood(&self) -> Option<Mood> {
        let db = match self.db.lock() {
            Ok(db) => db,
            Err(_) => {
                tracing::warn!("evolution: mutex poisoned, cannot snapshot mood");
                return None;
            }
        };
        let evo = db.get_evolution_state().ok()?;
        let vitals = db.get_vitals_state().ok()?;
        let bond_events = db.get_all_bond_events().ok()?;
        let bond_key = db.derive_hmac_key(crate::bond::BOND_HMAC_DOMAIN);
        let bond = crate::bond::replay_events_with_key(&bond_key, &bond_events);
        Some(compute_mood(&evo, &vitals, &bond))
    }

    /// Emit a `mood_changed` event when the computed mood flipped between
    /// tool calls. `reason` names the source that triggered the transition
    /// (typically the tool name). Rate-limited at 5/hr by `rate_limit_for`.
    fn emit_mood_changed(&self, from: Mood, to: Mood, reason: &str) {
        let Ok(db) = self.db.lock() else {
            tracing::warn!("evolution: mutex poisoned, skipping mood_changed event");
            return;
        };
        let metadata = serde_json::json!({
            "from_mood": from.to_string(),
            "to_mood": to.to_string(),
            "reason": reason,
        })
        .to_string();
        if let Err(e) = db.record_evolution_event("mood_changed", 0, None, reason, Some(&metadata))
        {
            tracing::warn!("evolution: failed to record mood_changed event: {e}");
        }
    }
}

impl Hook for EvolutionHook {
    fn name(&self) -> &str {
        "evolution"
    }

    fn points(&self) -> &[HookPoint] {
        &[
            HookPoint::SessionStart,
            HookPoint::BeforeAgentStart,
            HookPoint::BeforeLlmCall,
            HookPoint::AfterToolCall,
        ]
    }

    fn execute(&self, ctx: &HookContext) -> HookAction {
        if !self.enabled {
            return HookAction::Continue;
        }
        match &ctx.data {
            HookData::SessionStart { .. } => {
                // Record interaction XP
                self.record_xp("session_start", None, BASE_XP_INTERACTION);
            }
            HookData::AgentStart { .. } => {
                let context = self.evolution_context();
                if !context.is_empty() {
                    return HookAction::InjectContext(context);
                }
            }
            HookData::LlmCall { .. } => {
                let context = self.evolution_context();
                if !context.is_empty() {
                    return HookAction::InjectContext(context);
                }
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
                    let xp = if is_creation {
                        let bonus = if archetype.is_some() {
                            BONUS_XP_CREATION_ALIGNED
                        } else {
                            0
                        };
                        BASE_XP_CREATION + bonus
                    } else {
                        let bonus = if archetype.is_some() {
                            BONUS_XP_ALIGNED
                        } else {
                            0
                        };
                        BASE_XP_TOOL_SUCCESS + bonus
                    };

                    let mood_before = self.snapshot_mood();
                    self.record_xp(name, archetype, xp);

                    if let Some(before) = mood_before {
                        if let Some(after) = self.snapshot_mood() {
                            if before != after {
                                self.emit_mood_changed(before, after, name);
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

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;

    // ── XP Curve ──

    #[test]
    fn xp_for_level_stage1_boundaries() {
        assert_eq!(xp_for_level(&Stage::Base, 0), 20);
        assert_eq!(
            xp_for_level(&Stage::Base, 50),
            20 + (50.0_f64.powf(1.4)) as u32
        );
        assert_eq!(
            xp_for_level(&Stage::Base, 99),
            20 + (99.0_f64.powf(1.4)) as u32
        );
    }

    #[test]
    fn xp_for_level_stage2_grows_faster() {
        let s1_total = total_xp_for_level(&Stage::Base, 99);
        let s2_total = total_xp_for_level(&Stage::Evolved, 99);
        assert!(
            s2_total > s1_total,
            "Stage 2 total {s2_total} should be larger than Stage 1 {s1_total}"
        );
    }

    #[test]
    fn xp_for_level_stage3_exponential() {
        let s3_early = xp_for_level(&Stage::Final, 10);
        let s3_late = xp_for_level(&Stage::Final, 90);
        assert!(
            s3_late > s3_early * 5,
            "Stage 3 late levels should be much harder: early={s3_early} late={s3_late}"
        );
    }

    #[test]
    fn level_from_xp_round_trip() {
        for stage in &[Stage::Base, Stage::Evolved, Stage::Final] {
            let target = 50u8;
            let needed = total_xp_for_level(stage, target);
            let (level, _remaining) = level_from_xp(stage, needed);
            assert_eq!(
                level, target,
                "stage={stage}: xp={needed} should give level {target}"
            );
        }
    }

    #[test]
    fn level_from_xp_max_at_99() {
        let (level, remaining) = level_from_xp(&Stage::Base, 999_999);
        assert_eq!(level, 99);
        assert_eq!(remaining, 0);
    }

    // ── Archetype Classification ──

    #[test]
    fn classify_builder_tools() {
        assert_eq!(
            classify_tool_archetype("apply_patch", None),
            Some(Archetype::Builder)
        );
        assert_eq!(
            classify_tool_archetype("apply_skill_patch", None),
            Some(Archetype::Builder)
        );
    }

    #[test]
    fn classify_guardian_tools() {
        // Guardian is reached via shell commands with security keywords
        assert_eq!(
            classify_tool_archetype("run_shell", Some("ufw status")),
            Some(Archetype::Guardian)
        );
    }

    #[test]
    fn classify_analyst_tools() {
        assert_eq!(
            classify_tool_archetype("browser", None),
            Some(Archetype::Analyst)
        );
        assert_eq!(
            classify_tool_archetype("search", None),
            Some(Archetype::Analyst)
        );
    }

    #[test]
    fn classify_shell_ops_keywords() {
        assert_eq!(
            classify_tool_archetype("run_shell", Some("docker compose up -d")),
            Some(Archetype::Ops)
        );
        assert_eq!(
            classify_tool_archetype("run_shell", Some("kubectl apply -f deploy.yaml")),
            Some(Archetype::Ops)
        );
    }

    #[test]
    fn classify_shell_builder_keywords() {
        assert_eq!(
            classify_tool_archetype("run_shell", Some("cargo build --release")),
            Some(Archetype::Builder)
        );
        assert_eq!(
            classify_tool_archetype("run_shell", Some("npm install express")),
            Some(Archetype::Builder)
        );
    }

    #[test]
    fn classify_shell_tinkerer_keywords() {
        assert_eq!(
            classify_tool_archetype("run_shell", Some("ssh proxmox 'qm list'")),
            Some(Archetype::Tinkerer)
        );
        assert_eq!(
            classify_tool_archetype("run_shell", Some("tailscale status")),
            Some(Archetype::Tinkerer)
        );
    }

    #[test]
    fn classify_unknown_tool_returns_none() {
        assert_eq!(classify_tool_archetype("list_dir", None), None);
        assert_eq!(classify_tool_archetype("read_file", None), None);
    }

    #[test]
    fn classify_channel_names() {
        assert_eq!(
            classify_tool_archetype("telegram_send", None),
            Some(Archetype::Communicator)
        );
        assert_eq!(
            classify_tool_archetype("slack_post", None),
            Some(Archetype::Communicator)
        );
    }

    #[test]
    fn all_archetypes_have_at_least_one_signal() {
        // Verify each archetype can be reached
        let cases: &[(&str, Option<&str>, Archetype)] = &[
            ("run_shell", Some("docker ps"), Archetype::Ops),
            ("apply_patch", None, Archetype::Builder),
            ("browser", None, Archetype::Analyst),
            ("telegram_send", None, Archetype::Communicator),
            ("run_shell", Some("ufw status"), Archetype::Guardian),
            ("calendar", None, Archetype::Strategist),
            ("write_memory", None, Archetype::Creator),
            // Caretaker, Merchant are primarily classified by user-created tools
            ("run_shell", Some("homelab proxmox"), Archetype::Tinkerer),
        ];
        for (tool, meta, expected) in cases {
            assert_eq!(
                classify_tool_archetype(tool, *meta),
                Some(*expected),
                "Expected {expected} for tool={tool} meta={meta:?}"
            );
        }
    }

    // ── HMAC Chain ──

    #[test]
    fn hmac_deterministic() {
        let h1 = compute_event_hmac(
            EVOLUTION_HMAC_LEGACY,
            "0",
            "xp_gain",
            3,
            "builder",
            "apply_patch",
            "",
            1000,
        );
        let h2 = compute_event_hmac(
            EVOLUTION_HMAC_LEGACY,
            "0",
            "xp_gain",
            3,
            "builder",
            "apply_patch",
            "",
            1000,
        );
        assert_eq!(h1, h2);
    }

    #[test]
    fn hmac_verify_valid() {
        let hmac = compute_event_hmac(
            EVOLUTION_HMAC_LEGACY,
            "0",
            "xp_gain",
            1,
            "ops",
            "run_shell",
            "",
            1000,
        );
        let event = EvolutionEvent {
            id: 1,
            event_type: "xp_gain".to_string(),
            xp_delta: 1,
            archetype: Some("ops".to_string()),
            source: "run_shell".to_string(),
            metadata_json: None,
            created_at: 1000,
            hmac: hmac.clone(),
            prev_hmac: "0".to_string(),
        };
        assert!(verify_event_hmac(EVOLUTION_HMAC_LEGACY, &event, "0"));
    }

    #[test]
    fn hmac_tamper_detection() {
        let hmac = compute_event_hmac(
            EVOLUTION_HMAC_LEGACY,
            "0",
            "xp_gain",
            1,
            "ops",
            "run_shell",
            "",
            1000,
        );
        let mut event = EvolutionEvent {
            id: 1,
            event_type: "xp_gain".to_string(),
            xp_delta: 1,
            archetype: Some("ops".to_string()),
            source: "run_shell".to_string(),
            metadata_json: None,
            created_at: 1000,
            hmac,
            prev_hmac: "0".to_string(),
        };
        // Tamper with XP
        event.xp_delta = 999;
        assert!(!verify_event_hmac(EVOLUTION_HMAC_LEGACY, &event, "0"));
    }

    #[test]
    fn hmac_chain_linking() {
        let h1 = compute_event_hmac(EVOLUTION_HMAC_LEGACY, "0", "xp_gain", 1, "", "a", "", 1000);
        let h2 = compute_event_hmac(EVOLUTION_HMAC_LEGACY, &h1, "xp_gain", 1, "", "b", "", 2000);
        assert_ne!(h1, h2);

        let e2 = EvolutionEvent {
            id: 2,
            event_type: "xp_gain".to_string(),
            xp_delta: 1,
            archetype: None,
            source: "b".to_string(),
            metadata_json: None,
            created_at: 2000,
            hmac: h2,
            prev_hmac: h1.clone(),
        };
        assert!(verify_event_hmac(EVOLUTION_HMAC_LEGACY, &e2, &h1));
        // Wrong prev_hmac
        assert!(!verify_event_hmac(EVOLUTION_HMAC_LEGACY, &e2, "0"));
    }

    // ── Replay ──

    fn make_event(
        id: i64,
        event_type: &str,
        xp_delta: i32,
        archetype: Option<&str>,
        source: &str,
        created_at: i64,
        prev_hmac: &str,
    ) -> EvolutionEvent {
        let hmac = compute_event_hmac(
            EVOLUTION_HMAC_LEGACY,
            prev_hmac,
            event_type,
            xp_delta,
            archetype.unwrap_or(""),
            source,
            "",
            created_at,
        );
        EvolutionEvent {
            id,
            event_type: event_type.to_string(),
            xp_delta,
            archetype: archetype.map(|s| s.to_string()),
            source: source.to_string(),
            metadata_json: None,
            created_at,
            hmac,
            prev_hmac: prev_hmac.to_string(),
        }
    }

    #[test]
    fn replay_empty_events_gives_baseline() {
        let state = replay_events(&[]);
        assert_eq!(state.stage, Stage::Base);
        assert_eq!(state.level, 0);
        assert_eq!(state.total_xp, 0);
        assert!(state.chain_valid);
        assert!(state.dominant_archetype.is_none());
    }

    #[test]
    fn replay_xp_accumulates() {
        let e1 = make_event(1, "xp_gain", 3, Some("ops"), "run_shell", 1000, "0");
        let e2 = make_event(2, "xp_gain", 3, Some("ops"), "run_shell", 2000, &e1.hmac);
        let state = replay_events(&[e1, e2]);
        assert_eq!(state.total_xp, 6);
        assert_eq!(state.dominant_archetype, Some(Archetype::Ops));
        // Lifetime carries the raw accumulation independent of the 30-day window.
        assert_eq!(*state.lifetime_scores.get(&Archetype::Ops).unwrap(), 6);
    }

    #[test]
    fn replay_level_up() {
        // Stage 1: level 0 costs 20 XP
        let e1 = make_event(1, "xp_gain", 5, None, "test", 1000, "0");
        let state = replay_events(&[e1]);
        assert_eq!(state.total_xp, 5);
        // 5 XP: level 0 costs 20 → still at level 0
        assert_eq!(state.level, 0);
    }

    #[test]
    fn replay_stage_transition() {
        let e1 = make_event(1, "xp_gain", 999_999, None, "test", 1000, "0");
        let meta = r#"{"gates_verified":true,"name":"Pipeline Warden","description":"A vigilant guardian"}"#;
        let mut e2 = make_event(2, "evolution", 0, None, "system", 2000, &e1.hmac);
        e2.metadata_json = Some(meta.to_string());

        let state = replay_events(&[e1, e2]);
        assert_eq!(state.stage, Stage::Evolved);
        assert_eq!(state.total_xp, 0); // Reset after evolution
        assert_eq!(state.level, 0);
        assert_eq!(state.evolution_name.as_deref(), Some("Pipeline Warden"));
        assert_eq!(
            state.evolution_description.as_deref(),
            Some("A vigilant guardian")
        );
    }

    #[test]
    fn replay_broken_hmac_skips_event() {
        let e1 = make_event(1, "xp_gain", 3, None, "test", 1000, "0");
        let mut e2 = make_event(2, "xp_gain", 100, None, "test", 2000, &e1.hmac);
        e2.hmac = "tampered".to_string(); // Break the chain
        let e3 = make_event(3, "xp_gain", 1, None, "test", 3000, &e2.hmac);

        let state = replay_events(&[e1, e2, e3]);
        // e2 and e3 should be skipped (broken chain)
        assert_eq!(state.total_xp, 3);
        assert!(!state.chain_valid);
    }

    #[test]
    fn replay_rate_limiting() {
        // Create 35 events in the same hour (limit is 15 for xp_gain)
        let mut events = Vec::new();
        let mut prev = "0".to_string();
        for i in 0..35 {
            let e = make_event(
                i + 1,
                "xp_gain",
                1,
                None,
                "test",
                1000 + i as i64, // all in same hour
                &prev,
            );
            prev = e.hmac.clone();
            events.push(e);
        }
        let state = replay_events(&events);
        // All same source "test" with xp_delta=1: diminishing returns
        // counts 1-2: 1.0*1=1 each (2), count 3: floor(0.5*1)=0, count 4: floor(0.25*1)=0, 5+: 0
        // Plus type rate limit caps at 15 total xp_gain events per hour
        assert_eq!(state.total_xp, 2);
    }

    #[test]
    fn replay_source_decay_with_creation_xp() {
        // Creation events have xp_delta=2. Decay: 2*1.0, 2*1.0, floor(2*0.5)=1, floor(2*0.25)=0
        let mut events = Vec::new();
        let mut prev = "0".to_string();
        for i in 0..4 {
            let e = make_event(
                i + 1,
                "xp_gain",
                2,
                Some("creator"),
                "write_memory",
                1000 + i as i64,
                &prev,
            );
            prev = e.hmac.clone();
            events.push(e);
        }
        let state = replay_events(&events);
        assert_eq!(state.total_xp, 5); // 2 + 2 + 1 + 0
    }

    #[test]
    fn replay_diverse_tools_earn_more() {
        // 4 different sources, each used once = full XP
        let e1 = make_event(1, "xp_gain", 1, Some("ops"), "run_shell", 1000, "0");
        let e2 = make_event(
            2,
            "xp_gain",
            1,
            Some("builder"),
            "apply_patch",
            1001,
            &e1.hmac,
        );
        let e3 = make_event(3, "xp_gain", 1, Some("analyst"), "browser", 1002, &e2.hmac);
        let e4 = make_event(
            4,
            "xp_gain",
            1,
            Some("creator"),
            "write_memory",
            1003,
            &e3.hmac,
        );
        let diverse = replay_events(&[e1, e2, e3, e4]);
        assert_eq!(diverse.total_xp, 4); // All full

        // 4 same source = 2 XP (1+1+0+0)
        let mut spam = Vec::new();
        let mut prev = "0".to_string();
        for i in 0..4 {
            let e = make_event(
                i + 1,
                "xp_gain",
                1,
                None,
                "run_shell",
                1000 + i as i64,
                &prev,
            );
            prev = e.hmac.clone();
            spam.push(e);
        }
        let spammed = replay_events(&spam);
        assert_eq!(spammed.total_xp, 2); // Diminished

        assert!(diverse.total_xp > spammed.total_xp);
    }

    #[test]
    fn replay_source_decay_resets_each_hour() {
        let mut events = Vec::new();
        let mut prev = "0".to_string();
        // 3 events in hour 0 (timestamps 100-102)
        for i in 0..3 {
            let e = make_event(
                i + 1,
                "xp_gain",
                1,
                None,
                "run_shell",
                100 + i as i64,
                &prev,
            );
            prev = e.hmac.clone();
            events.push(e);
        }
        // 3 events in hour 1 (timestamps 3700-3702)
        for i in 0..3 {
            let e = make_event(
                i + 4,
                "xp_gain",
                1,
                None,
                "run_shell",
                3700 + i as i64,
                &prev,
            );
            prev = e.hmac.clone();
            events.push(e);
        }
        let state = replay_events(&events);
        // Hour 0: 1+1+0 = 2, Hour 1: 1+1+0 = 2, Total = 4
        assert_eq!(state.total_xp, 4);
    }

    #[test]
    fn replay_multiple_stages() {
        let e1 = make_event(1, "xp_gain", 999_999, Some("ops"), "run_shell", 1000, "0");
        let mut e2 = make_event(2, "evolution", 0, None, "system", 2000, &e1.hmac);
        e2.metadata_json = Some(r#"{"gates_verified":true,"name":"Sentinel"}"#.to_string());
        let e3 = make_event(
            3,
            "xp_gain",
            999_999,
            Some("ops"),
            "run_shell",
            3000,
            &e2.hmac,
        );
        let mut e4 = make_event(4, "evolution", 0, None, "system", 4000, &e3.hmac);
        e4.metadata_json = Some(
            r#"{"gates_verified":true,"name":"Overseer","description":"Supreme commander"}"#
                .to_string(),
        );
        let e5 = make_event(5, "xp_gain", 50, Some("ops"), "run_shell", 5000, &e4.hmac);

        let state = replay_events(&[e1, e2, e3, e4, e5]);
        assert_eq!(state.stage, Stage::Final);
        assert_eq!(state.total_xp, 50);
        assert_eq!(state.evolution_name.as_deref(), Some("Overseer"));
        assert_eq!(
            state.evolution_description.as_deref(),
            Some("Supreme commander")
        );
    }

    // ── Evolution Gates ──

    fn make_state_at_level(
        stage: Stage,
        level: u8,
        archetype_scores: HashMap<Archetype, u32>,
    ) -> EvolutionState {
        let xp = total_xp_for_level(&stage, level);
        let (_, xp_to_next) = level_from_xp(&stage, xp);
        EvolutionState {
            stage,
            level,
            total_xp: xp,
            xp_to_next_level: xp_to_next,
            dominant_archetype: dominant_archetype(&archetype_scores),
            evolution_name: None,
            evolution_description: None,
            // Treat the seeded scores as lifetime for gate tests; effective
            // computation will preserve the ordering because last_30d is empty.
            lifetime_scores: archetype_scores.clone(),
            last_30d_scores: HashMap::new(),
            archetype_scores,
            dominant_history: Vec::new(),
            total_events: 100,
            chain_valid: true,
            momentum: HashMap::new(),
            level_up_events_recent: Vec::new(),
            mood: None,
            readiness: None,
        }
    }

    #[test]
    fn stage1_gates_pass() {
        let mut scores = HashMap::new();
        scores.insert(Archetype::Ops, 100);
        scores.insert(Archetype::Builder, 50);
        let state = make_state_at_level(Stage::Base, 99, scores);
        assert!(check_stage1_gates(&state, 30, 20));
    }

    #[test]
    fn stage1_gates_fail_level() {
        let mut scores = HashMap::new();
        scores.insert(Archetype::Ops, 100);
        let state = make_state_at_level(Stage::Base, 50, scores);
        assert!(!check_stage1_gates(&state, 30, 20));
    }

    #[test]
    fn stage1_gates_fail_bond() {
        let mut scores = HashMap::new();
        scores.insert(Archetype::Ops, 100);
        let state = make_state_at_level(Stage::Base, 99, scores);
        assert!(!check_stage1_gates(&state, 20, 20)); // bond < 30
    }

    #[test]
    fn stage1_gates_fail_close_archetypes() {
        let mut scores = HashMap::new();
        scores.insert(Archetype::Ops, 100);
        scores.insert(Archetype::Builder, 90); // too close (100/90 < 1.3)
        let state = make_state_at_level(Stage::Base, 99, scores);
        assert!(!check_stage1_gates(&state, 30, 20));
    }

    #[test]
    fn stage2_gates_pass() {
        let mut scores = HashMap::new();
        scores.insert(Archetype::Ops, 200);
        let state = make_state_at_level(Stage::Evolved, 99, scores);
        assert!(check_stage2_gates(&state, 55, 0.10, 30));
    }

    #[test]
    fn stage2_gates_fail_correction_rate() {
        let mut scores = HashMap::new();
        scores.insert(Archetype::Ops, 200);
        let state = make_state_at_level(Stage::Evolved, 99, scores);
        assert!(!check_stage2_gates(&state, 55, 0.25, 30)); // correction too high
    }

    #[test]
    fn stage2_gates_fail_archetype_stability() {
        let mut scores = HashMap::new();
        scores.insert(Archetype::Ops, 200);
        let state = make_state_at_level(Stage::Evolved, 99, scores);
        assert!(!check_stage2_gates(&state, 55, 0.10, 10)); // only 10 days stable
    }

    // ── Formatting ──

    #[test]
    fn compact_format_base() {
        let state = replay_events(&[]);
        let compact = format_compact(&state);
        assert!(compact.contains("Base Borg"));
        assert!(compact.contains("Lvl.0"));
    }

    #[test]
    fn compact_format_evolved() {
        let state = EvolutionState {
            stage: Stage::Evolved,
            level: 42,
            total_xp: 1000,
            xp_to_next_level: 50,
            dominant_archetype: Some(Archetype::Ops),
            evolution_name: Some("Pipeline Warden".to_string()),
            evolution_description: Some("A vigilant guardian".to_string()),
            archetype_scores: HashMap::new(),
            lifetime_scores: HashMap::new(),
            last_30d_scores: HashMap::new(),
            dominant_history: Vec::new(),
            total_events: 100,
            chain_valid: true,
            momentum: HashMap::new(),
            level_up_events_recent: Vec::new(),
            mood: None,
            readiness: None,
        };
        let compact = format_compact(&state);
        assert!(compact.contains("Pipeline Warden"));
        assert!(compact.contains("Lvl.42"));
        assert!(compact.contains("Ops"));
    }

    #[test]
    fn status_section_shows_description() {
        let state = EvolutionState {
            stage: Stage::Evolved,
            level: 42,
            total_xp: 1000,
            xp_to_next_level: 50,
            dominant_archetype: Some(Archetype::Ops),
            evolution_name: Some("Pipeline Warden".to_string()),
            evolution_description: Some("A vigilant guardian".to_string()),
            archetype_scores: HashMap::new(),
            lifetime_scores: HashMap::new(),
            last_30d_scores: HashMap::new(),
            dominant_history: Vec::new(),
            total_events: 100,
            chain_valid: true,
            momentum: HashMap::new(),
            level_up_events_recent: Vec::new(),
            mood: None,
            readiness: None,
        };
        let section = format_status_section(&state);
        assert!(section.contains("Pipeline Warden"));
        assert!(section.contains("A vigilant guardian"));
        assert!(section.contains("Evolved (2/3)"));
    }

    #[test]
    fn evolution_context_xml() {
        let state = EvolutionState {
            stage: Stage::Evolved,
            level: 42,
            total_xp: 1000,
            xp_to_next_level: 50,
            dominant_archetype: Some(Archetype::Ops),
            evolution_name: Some("Pipeline Warden".to_string()),
            evolution_description: None,
            archetype_scores: {
                let mut m = HashMap::new();
                m.insert(Archetype::Ops, 74);
                m
            },
            lifetime_scores: HashMap::new(),
            last_30d_scores: HashMap::new(),
            dominant_history: Vec::new(),
            total_events: 100,
            chain_valid: true,
            momentum: HashMap::new(),
            level_up_events_recent: Vec::new(),
            mood: None,
            readiness: None,
        };
        let ctx = format_evolution_context(&state);
        assert!(ctx.contains("<evolution_context>"));
        assert!(ctx.contains("Pipeline Warden Lvl.42"));
        assert!(ctx.contains("Ops"));
        assert!(ctx.contains("Evolved"));
    }

    // ── Enum roundtrips ──

    #[test]
    fn archetype_display_parse_roundtrip() {
        for arch in &Archetype::ALL {
            let s = arch.to_string();
            assert_eq!(
                Archetype::parse(&s),
                Some(*arch),
                "roundtrip failed for {arch:?}"
            );
        }
    }

    #[test]
    fn stage_display_parse_roundtrip() {
        for stage in &[Stage::Base, Stage::Evolved, Stage::Final] {
            let s = stage.to_string();
            assert_eq!(
                Stage::parse(&s),
                Some(*stage),
                "roundtrip failed for {stage:?}"
            );
        }
    }

    // ── Gate Enforcement Tests ──

    #[test]
    fn replay_rejects_unverified_evolution_event() {
        // An evolution event without gates_verified metadata should be rejected
        let e1 = make_event(1, "xp_gain", 5, Some("ops"), "test", 1000, "0");
        let e2 = make_event(2, "evolution", 0, None, "system", 2000, &e1.hmac);
        let state = replay_events(&[e1, e2]);
        assert_eq!(
            state.stage,
            Stage::Base,
            "evolution event without gates_verified should be rejected"
        );
        assert!(state.chain_valid);
    }

    #[test]
    fn replay_accepts_gates_verified_evolution_event() {
        let e1 = make_event(1, "xp_gain", 5, Some("ops"), "test", 1000, "0");
        let mut e2 = make_event(2, "evolution", 0, None, "gate_check", 2000, &e1.hmac);
        e2.metadata_json = Some(r#"{"gates_verified": true}"#.to_string());
        // Recompute HMAC since metadata isn't included in HMAC (only type, xp_delta, archetype, source, created_at)
        let state = replay_events(&[e1, e2]);
        assert_eq!(state.stage, Stage::Evolved);
    }

    #[test]
    fn stage1_gates_require_level_99() {
        let mut scores = HashMap::new();
        scores.insert(Archetype::Ops, 100);
        let state_50 = make_state_at_level(Stage::Base, 50, scores.clone());
        assert!(
            !check_stage1_gates(&state_50, 50, 30),
            "level 50 should not pass gate"
        );

        let state_99 = make_state_at_level(Stage::Base, 99, scores);
        assert!(
            check_stage1_gates(&state_99, 50, 30),
            "level 99 should pass gate"
        );
    }

    #[test]
    fn stage2_gates_require_low_correction_rate() {
        let mut scores = HashMap::new();
        scores.insert(Archetype::Ops, 200);
        let state = make_state_at_level(Stage::Evolved, 99, scores);
        assert!(
            check_stage2_gates(&state, 60, 0.10, 30),
            "low correction rate should pass"
        );
        assert!(
            !check_stage2_gates(&state, 60, 0.25, 30),
            "high correction rate should fail"
        );
    }

    #[test]
    fn derived_key_produces_different_hmacs_than_legacy() {
        let legacy = compute_event_hmac(
            EVOLUTION_HMAC_LEGACY,
            "0",
            "xp_gain",
            5,
            "ops",
            "test",
            "",
            1000,
        );
        let derived_key = b"some-derived-key-that-differs-!!";
        let derived = compute_event_hmac(derived_key, "0", "xp_gain", 5, "ops", "test", "", 1000);
        assert_ne!(
            legacy, derived,
            "derived key should produce different HMAC than legacy"
        );
    }

    #[test]
    fn xp_overflow_saturates() {
        // Build events with massive XP to test u32 saturation
        let key = EVOLUTION_HMAC_LEGACY;
        let mut events = Vec::new();
        let mut prev_hmac = "0".to_string();

        for i in 0..10 {
            let ts = 1000 + i * 3600; // spread across hours
            let hmac =
                compute_event_hmac(key, &prev_hmac, "xp_gain", i32::MAX, "ops", "test", "", ts);
            events.push(EvolutionEvent {
                id: i + 1,
                event_type: "xp_gain".to_string(),
                xp_delta: i32::MAX,
                archetype: Some("ops".to_string()),
                source: "test".to_string(),
                metadata_json: None,
                created_at: ts,
                hmac: hmac.clone(),
                prev_hmac,
            });
            prev_hmac = hmac;
        }

        let state = replay_events(&events);
        // Should not panic — saturating_add handles overflow
        assert_eq!(state.level, 99);
        assert!(state.chain_valid);
    }

    #[test]
    fn status_section_shows_tip_card_when_no_description() {
        let state = EvolutionState {
            stage: Stage::Base,
            level: 5,
            total_xp: 20,
            xp_to_next_level: 3,
            dominant_archetype: None,
            evolution_name: None,
            evolution_description: None,
            archetype_scores: HashMap::new(),
            lifetime_scores: HashMap::new(),
            last_30d_scores: HashMap::new(),
            dominant_history: Vec::new(),
            total_events: 10,
            chain_valid: true,
            momentum: HashMap::new(),
            level_up_events_recent: Vec::new(),
            mood: None,
            readiness: None,
        };
        let section = format_status_section(&state);
        assert!(
            section.contains("How Evolution Works"),
            "tip card should appear when no evolution description"
        );
        assert!(section.contains("learning how you use it"));
        assert!(section.contains("Evolution is permanent"));
    }

    #[test]
    fn status_section_hides_tip_card_when_description_present() {
        let state = EvolutionState {
            stage: Stage::Evolved,
            level: 42,
            total_xp: 1000,
            xp_to_next_level: 50,
            dominant_archetype: Some(Archetype::Ops),
            evolution_name: Some("Pipeline Warden".to_string()),
            evolution_description: Some("A vigilant guardian".to_string()),
            archetype_scores: HashMap::new(),
            lifetime_scores: HashMap::new(),
            last_30d_scores: HashMap::new(),
            dominant_history: Vec::new(),
            total_events: 100,
            chain_valid: true,
            momentum: HashMap::new(),
            level_up_events_recent: Vec::new(),
            mood: None,
            readiness: None,
        };
        let section = format_status_section(&state);
        assert!(
            !section.contains("How Evolution Works"),
            "tip card should not appear when description exists"
        );
        assert!(section.contains("A vigilant guardian"));
    }

    #[test]
    fn no_autonomy_tier_in_evolution() {
        // Verify AutonomyTier no longer exists in evolution module.
        // Bond owns the autonomy concept. Evolution uses Stage directly.
        let state = EvolutionState {
            stage: Stage::Evolved,
            level: 50,
            total_xp: 5000,
            xp_to_next_level: 100,
            dominant_archetype: Some(Archetype::Ops),
            evolution_name: Some("Test".to_string()),
            evolution_description: None,
            archetype_scores: HashMap::new(),
            lifetime_scores: HashMap::new(),
            last_30d_scores: HashMap::new(),
            dominant_history: Vec::new(),
            total_events: 10,
            chain_valid: true,
            momentum: HashMap::new(),
            level_up_events_recent: Vec::new(),
            mood: None,
            readiness: None,
        };
        let ctx = format_evolution_context(&state);
        // Should contain Stage name, not autonomy tier
        assert!(ctx.contains("Stage: Evolved"));
        assert!(!ctx.contains("Autonomy:"));
    }

    #[test]
    fn tip_card_lines_aligned() {
        let state = EvolutionState {
            stage: Stage::Base,
            level: 1,
            total_xp: 10,
            xp_to_next_level: 90,
            dominant_archetype: None,
            evolution_name: None,
            evolution_description: None,
            archetype_scores: HashMap::new(),
            lifetime_scores: HashMap::new(),
            last_30d_scores: HashMap::new(),
            dominant_history: Vec::new(),
            total_events: 1,
            chain_valid: true,
            momentum: HashMap::new(),
            level_up_events_recent: Vec::new(),
            mood: None,
            readiness: None,
        };
        for width in [34, 44, 48, 60, 80] {
            let section = format_status_section_with_width(&state, width);
            let card_lines: Vec<&str> = section
                .lines()
                .skip_while(|l| !l.contains('\u{256D}'))
                .take_while(|l| !l.contains('\u{256F}') && !l.is_empty())
                .chain(section.lines().filter(|l| l.contains('\u{256F}')))
                .collect();
            assert!(
                card_lines.len() >= 2,
                "card should have borders at width {width}"
            );
            let first_len = card_lines[0].chars().count();
            for (i, line) in card_lines.iter().enumerate() {
                assert_eq!(
                    line.chars().count(),
                    first_len,
                    "line {i} width mismatch at card_width={width}: {:?}",
                    line
                );
            }
        }
    }

    // ── Error path coverage ──

    fn test_db() -> Database {
        Database::test_db()
    }

    #[test]
    fn compute_archetype_stable_days_empty_db_returns_zero() {
        let db = test_db();
        assert_eq!(compute_archetype_stable_days(&db), 0);
    }

    // ── Metadata HMAC tests ──

    #[test]
    fn metadata_included_in_hmac_v2() {
        let key = EVOLUTION_HMAC_LEGACY;
        // Same event data, different metadata should produce different HMACs
        let h1 = compute_event_hmac(key, "0", "evolution", 0, "", "gate_check", "", 1000);
        let h2 = compute_event_hmac(
            key,
            "0",
            "evolution",
            0,
            "",
            "gate_check",
            r#"{"gates_verified":true}"#,
            1000,
        );
        assert_ne!(h1, h2, "different metadata should produce different HMACs");
    }

    #[test]
    fn legacy_events_still_verify() {
        // Event with HMAC computed WITHOUT metadata (legacy v1)
        let key = EVOLUTION_HMAC_LEGACY;
        let legacy_hmac =
            compute_event_hmac_legacy(key, "0", "xp_gain", 1, "ops", "run_shell", 1000);
        let event = EvolutionEvent {
            id: 1,
            event_type: "xp_gain".to_string(),
            xp_delta: 1,
            archetype: Some("ops".to_string()),
            source: "run_shell".to_string(),
            metadata_json: None,
            created_at: 1000,
            hmac: legacy_hmac,
            prev_hmac: "0".to_string(),
        };
        // verify_event_hmac should accept via legacy fallback
        assert!(
            verify_event_hmac(key, &event, "0"),
            "legacy events should still verify via fallback"
        );
    }

    #[test]
    fn metadata_tampering_detected_on_v2_events() {
        let key = EVOLUTION_HMAC_LEGACY;
        let meta = r#"{"gates_verified":false}"#;
        let hmac = compute_event_hmac(key, "0", "evolution", 0, "", "gate_check", meta, 1000);
        let mut event = EvolutionEvent {
            id: 1,
            event_type: "evolution".to_string(),
            xp_delta: 0,
            archetype: None,
            source: "gate_check".to_string(),
            metadata_json: Some(meta.to_string()),
            created_at: 1000,
            hmac,
            prev_hmac: "0".to_string(),
        };
        // Valid before tampering
        assert!(verify_event_hmac(key, &event, "0"));
        // Tamper: inject gates_verified
        event.metadata_json = Some(r#"{"gates_verified":true}"#.to_string());
        assert!(
            !verify_event_hmac(key, &event, "0"),
            "metadata tampering should be detected on v2 events"
        );
    }

    #[test]
    fn legacy_events_accept_metadata_via_fallback_path() {
        // Create an xp_gain event (no metadata), then an evolution event
        // where someone injected gates_verified into the metadata AFTER the HMAC was computed
        let e1 = make_event(1, "xp_gain", 3, Some("ops"), "run_shell", 1000, "0");

        // Evolution event with v2 HMAC including metadata
        let meta = r#"{"gates_verified":true}"#;
        let hmac = compute_event_hmac(
            EVOLUTION_HMAC_LEGACY,
            &e1.hmac,
            "evolution",
            0,
            "",
            "gate_check",
            meta,
            2000,
        );
        let e2 = EvolutionEvent {
            id: 2,
            event_type: "evolution".to_string(),
            xp_delta: 0,
            archetype: None,
            source: "gate_check".to_string(),
            metadata_json: Some(meta.to_string()),
            created_at: 2000,
            hmac,
            prev_hmac: e1.hmac.clone(),
        };

        let state = replay_events(&[e1, e2]);
        // Should evolve since metadata HMAC matches
        assert_eq!(state.stage, Stage::Evolved);

        // Now create the same evolution event but with tampered metadata
        let e1b = make_event(1, "xp_gain", 3, Some("ops"), "run_shell", 1000, "0");
        let no_meta_hmac = compute_event_hmac(
            EVOLUTION_HMAC_LEGACY,
            &e1b.hmac,
            "evolution",
            0,
            "",
            "gate_check",
            "", // computed without metadata
            2000,
        );
        let e2_tampered = EvolutionEvent {
            id: 2,
            event_type: "evolution".to_string(),
            xp_delta: 0,
            archetype: None,
            source: "gate_check".to_string(),
            metadata_json: Some(r#"{"gates_verified":true}"#.to_string()), // injected after
            created_at: 2000,
            hmac: no_meta_hmac,
            prev_hmac: e1b.hmac.clone(),
        };

        let state2 = replay_events(&[e1b, e2_tampered]);
        // The legacy fallback will verify the HMAC (computed without metadata),
        // but the metadata injection is still accepted via legacy path.
        // This is expected — legacy events are trusted. The protection is that
        // NEW events (written after this code change) include metadata in HMAC.
        // The key point is that verify_event_hmac accepts it via fallback, which is correct
        // for backward compatibility.
        assert!(
            state2.chain_valid,
            "legacy-format events should still pass chain verification"
        );
    }

    // ── Replay Stability (Event Sourcing Invariants) ──
    //
    // These tests pin down properties that make evolution's event-sourced
    // model trustworthy: determinism, tampering detection, rate-limit
    // boundaries, v1/v2 HMAC compatibility, and honest handling of
    // semantically-invalid events (unverified evolution transitions).

    /// Build a verified evolution event with `gates_verified: true` metadata.
    /// Computes the v2 HMAC over the metadata so the event verifies cleanly.
    fn make_verified_evolution(
        id: i64,
        source: &str,
        created_at: i64,
        prev_hmac: &str,
        name: Option<&str>,
    ) -> EvolutionEvent {
        let meta = match name {
            Some(n) => format!(r#"{{"gates_verified":true,"name":"{n}"}}"#),
            None => r#"{"gates_verified":true}"#.to_string(),
        };
        let hmac = compute_event_hmac(
            EVOLUTION_HMAC_LEGACY,
            prev_hmac,
            "evolution",
            0,
            "",
            source,
            &meta,
            created_at,
        );
        EvolutionEvent {
            id,
            event_type: "evolution".to_string(),
            xp_delta: 0,
            archetype: None,
            source: source.to_string(),
            metadata_json: Some(meta),
            created_at,
            hmac,
            prev_hmac: prev_hmac.to_string(),
        }
    }

    /// Build an `evolution`-type event that lacks `gates_verified`. Computes
    /// the v2 HMAC over the empty metadata so it verifies chain-wise — replay
    /// should still reject the *semantic* transition.
    fn make_unverified_evolution(
        id: i64,
        source: &str,
        created_at: i64,
        prev_hmac: &str,
    ) -> EvolutionEvent {
        // Empty metadata: HMAC v2 signs "" so it verifies via v1 legacy path.
        let hmac = compute_event_hmac(
            EVOLUTION_HMAC_LEGACY,
            prev_hmac,
            "evolution",
            0,
            "",
            source,
            "",
            created_at,
        );
        EvolutionEvent {
            id,
            event_type: "evolution".to_string(),
            xp_delta: 0,
            archetype: None,
            source: source.to_string(),
            metadata_json: None,
            created_at,
            hmac,
            prev_hmac: prev_hmac.to_string(),
        }
    }

    #[test]
    fn replay_determinism_identical_events_evolution() {
        // Determinism: replaying the same slice twice yields identical state.
        let e1 = make_event(1, "xp_gain", 3, Some("ops"), "run_shell", 1_000, "0");
        let e2 = make_event(
            2,
            "xp_gain",
            2,
            Some("builder"),
            "apply_patch",
            1_100,
            &e1.hmac,
        );
        let e3 = make_event(3, "xp_gain", 1, Some("analyst"), "browser", 3_700, &e2.hmac);
        let e4 = make_verified_evolution(4, "gate_check", 3_800, &e3.hmac, Some("Warden"));
        let e5 = make_event(5, "xp_gain", 1, Some("ops"), "run_shell", 7_300, &e4.hmac);

        let events = vec![e1, e2, e3, e4, e5];
        let s1 = replay_events(&events);
        let s2 = replay_events(&events);

        assert_eq!(s1.stage, s2.stage);
        assert_eq!(s1.level, s2.level);
        assert_eq!(s1.total_xp, s2.total_xp);
        assert_eq!(s1.xp_to_next_level, s2.xp_to_next_level);
        assert_eq!(s1.archetype_scores, s2.archetype_scores);
        assert_eq!(s1.total_events, s2.total_events);
        assert_eq!(s1.chain_valid, s2.chain_valid);
        assert_eq!(s1.evolution_name, s2.evolution_name);
    }

    #[test]
    fn replay_unverified_evolution_does_not_consume_rate_limit() {
        // Regression for Issue 2: 3 unverified evolution events in one hour
        // used to consume the entire evolution rate-limit budget (3/hr),
        // starving a legitimate gates_verified transition that followed.
        // After the fix, unverified events are rejected *before* the rate
        // limiter, so the real transition still applies.
        let hour = 3_600_i64;

        // First earn enough XP to reach level 99 at Stage::Base. A single
        // huge-xp event does the trick.
        let e1 = make_event(1, "xp_gain", 999_999, Some("ops"), "run_shell", hour, "0");

        // 3 unverified evolution events in the same next-hour bucket.
        let mut prev = e1.hmac.clone();
        let mut unverified = Vec::new();
        for i in 0..3 {
            let ev = make_unverified_evolution(i + 2, "noise", 2 * hour + i, &prev);
            prev = ev.hmac.clone();
            unverified.push(ev);
        }

        // One verified evolution event, same hour bucket as the 3 unverified.
        let real = make_verified_evolution(5, "gate_check", 2 * hour + 10, &prev, Some("Warden"));

        let mut events = vec![e1];
        events.extend(unverified);
        events.push(real);

        let state = replay_events(&events);
        assert!(state.chain_valid);
        assert_eq!(
            state.stage,
            Stage::Evolved,
            "the gates_verified evolution must still transition after 3 bogus rows"
        );
        assert_eq!(state.evolution_name.as_deref(), Some("Warden"));
    }

    #[test]
    fn replay_unverified_evolution_not_counted_in_total_events() {
        // Regression for Issue 2: unverified evolution events must not inflate
        // `total_events` (the accepted-event counter surfaced on EvolutionState).
        let e1 = make_event(1, "xp_gain", 1, Some("ops"), "run_shell", 1_000, "0");
        let u1 = make_unverified_evolution(2, "noise", 1_100, &e1.hmac);
        let u2 = make_unverified_evolution(3, "noise", 1_200, &u1.hmac);
        let e2 = make_event(4, "xp_gain", 1, Some("ops"), "run_shell", 3_700, &u2.hmac);

        let state = replay_events(&[e1, u1, u2, e2]);
        assert_eq!(
            state.total_events, 2,
            "only the 2 xp_gain events should count toward total_events"
        );
        assert_eq!(state.stage, Stage::Base);
    }

    #[test]
    fn replay_unverified_evolution_advances_hmac_chain() {
        // A rejected-but-chain-valid unverified evolution event should still
        // advance `expected_prev_hmac` so that downstream events chained off
        // its hmac verify successfully. (Only *HMAC-invalid* events break the
        // chain for their descendants.)
        let e1 = make_event(1, "xp_gain", 2, Some("ops"), "run_shell", 1_000, "0");
        let u1 = make_unverified_evolution(2, "noise", 1_100, &e1.hmac);
        // e2 chains off u1.hmac:
        let e2 = make_event(3, "xp_gain", 2, Some("ops"), "run_shell", 3_700, &u1.hmac);

        let state = replay_events(&[e1, u1, e2]);
        assert!(state.chain_valid, "chain must still be valid");
        assert_eq!(state.stage, Stage::Base);
        // Both xp_gains apply fully (distinct hours, no decay): 2 + 2 = 4.
        assert_eq!(state.total_xp, 4);
    }

    #[test]
    fn replay_tampered_middle_xp_gain_skips_tail() {
        // Flip a byte in a middle event's hmac; that event and all descendants
        // chained off it must be dropped, and chain_valid must go false.
        let e1 = make_event(1, "xp_gain", 1, Some("ops"), "run_shell", 1_000, "0");
        let mut e2 = make_event(2, "xp_gain", 10, Some("ops"), "run_shell", 3_700, &e1.hmac);
        let mut chars: Vec<char> = e2.hmac.chars().collect();
        chars[0] = if chars[0] == '0' { 'f' } else { '0' };
        e2.hmac = chars.into_iter().collect();
        // e3 chains off the ORIGINAL (now-unreachable) e2 hmac, so it also fails.
        let e3 = make_event(3, "xp_gain", 100, Some("ops"), "run_shell", 7_300, &e2.hmac);

        let state = replay_events(&[e1, e2, e3]);
        assert!(!state.chain_valid);
        assert_eq!(state.total_xp, 1, "only e1 should apply");
    }

    #[test]
    fn replay_rate_limit_exact_boundary_xp_gain() {
        // xp_gain cap is 15 events/hr. With distinct sources (so source decay
        // doesn't mask the type cap) the first 15 apply and the 16th is
        // dropped. A 17th event in a fresh hour must apply again.
        let hour = 3_600_i64;
        let mut events = Vec::new();
        let mut prev = "0".to_string();
        for i in 0..15 {
            let source = format!("tool_{i}");
            let e = make_event(
                i as i64 + 1,
                "xp_gain",
                1,
                Some("ops"),
                &source,
                hour + i as i64,
                &prev,
            );
            prev = e.hmac.clone();
            events.push(e);
        }
        // 16th in same hour — over cap, must be dropped.
        let over = make_event(16, "xp_gain", 1, Some("ops"), "tool_over", hour + 50, &prev);
        prev = over.hmac.clone();
        events.push(over);
        // 17th in next hour — must apply.
        let next_hour = make_event(17, "xp_gain", 1, Some("ops"), "tool_next", 2 * hour, &prev);
        events.push(next_hour);

        let state = replay_events(&events);
        // 15 (cap) + 1 (next hour) = 16, 16th within hour dropped.
        assert_eq!(state.total_xp, 16);
        assert_eq!(state.total_events, 16);
    }

    #[test]
    fn replay_rate_limits_milestone_unlocked_to_three_per_hour() {
        // milestone_unlocked caps at 3/hr. A burst of 4 in one hour (e.g. an
        // XP import crossing multiple level thresholds at once) must drop
        // the 4th. `level_up_events_recent` is populated from accepted rows
        // and is the easiest way to count survivors.
        let hour = 3_600_i64;
        let mut events = Vec::new();
        let mut prev = "0".to_string();
        for i in 0..4 {
            let e = make_event(
                i as i64 + 1,
                "milestone_unlocked",
                0,
                None,
                "hook",
                hour + i as i64,
                &prev,
            );
            prev = e.hmac.clone();
            events.push(e);
        }
        let state = replay_events(&events);
        let milestones: Vec<_> = state
            .level_up_events_recent
            .iter()
            .filter(|e| e.event_type == "milestone_unlocked")
            .collect();
        assert_eq!(
            milestones.len(),
            3,
            "milestone_unlocked must cap at 3/hr, got {}",
            milestones.len()
        );
    }

    #[test]
    fn replay_rate_limits_level_up_to_ten_per_hour() {
        // level_up caps at 10/hr. Guards against a single XP gain that
        // would otherwise emit a burst across multiple thresholds.
        let hour = 3_600_i64;
        let mut events = Vec::new();
        let mut prev = "0".to_string();
        for i in 0..11 {
            let e = make_event(
                i as i64 + 1,
                "level_up",
                0,
                None,
                "hook",
                hour + i as i64,
                &prev,
            );
            prev = e.hmac.clone();
            events.push(e);
        }
        let state = replay_events(&events);
        let ups: Vec<_> = state
            .level_up_events_recent
            .iter()
            .filter(|e| e.event_type == "level_up")
            .collect();
        assert_eq!(
            ups.len(),
            10,
            "level_up must cap at 10/hr, got {}",
            ups.len()
        );
    }

    #[test]
    fn replay_rate_limits_mood_changed_to_five_per_hour() {
        // mood_changed caps at 5/hr. Stops a per-tool-call ambient-header
        // recompute from flooding the ledger.
        let hour = 3_600_i64;
        let mut events = Vec::new();
        let mut prev = "0".to_string();
        for i in 0..6 {
            let e = make_event(
                i as i64 + 1,
                "mood_changed",
                0,
                None,
                "apply_patch",
                hour + i as i64,
                &prev,
            );
            prev = e.hmac.clone();
            events.push(e);
        }
        let state = replay_events(&events);
        assert_eq!(
            state.total_events, 5,
            "mood_changed must cap at 5/hr, got {}",
            state.total_events
        );
    }

    #[test]
    fn replay_rate_limits_share_card_created_to_three_per_hour() {
        // share_card_created caps at 3/hr.
        let hour = 3_600_i64;
        let mut events = Vec::new();
        let mut prev = "0".to_string();
        for i in 0..4 {
            let e = make_event(
                i as i64 + 1,
                "share_card_created",
                0,
                None,
                "hook",
                hour + i as i64,
                &prev,
            );
            prev = e.hmac.clone();
            events.push(e);
        }
        let state = replay_events(&events);
        assert_eq!(
            state.total_events, 3,
            "share_card_created must cap at 3/hr, got {}",
            state.total_events
        );
    }

    #[test]
    fn replay_hmac_v1_v2_mixed_chain() {
        // A chain where early events use legacy v1 HMAC (no metadata) and
        // later events use v2 HMAC (with metadata). Both must verify.
        //
        // e1: legacy v1 (constructed via compute_event_hmac_legacy)
        let h1 = compute_event_hmac_legacy(
            EVOLUTION_HMAC_LEGACY,
            "0",
            "xp_gain",
            2,
            "ops",
            "run_shell",
            1_000,
        );
        let e1 = EvolutionEvent {
            id: 1,
            event_type: "xp_gain".to_string(),
            xp_delta: 2,
            archetype: Some("ops".to_string()),
            source: "run_shell".to_string(),
            metadata_json: None,
            created_at: 1_000,
            hmac: h1.clone(),
            prev_hmac: "0".to_string(),
        };

        // e2: v2 evolution event with metadata, chained off e1
        let e2 = make_verified_evolution(2, "gate_check", 2_000, &h1, Some("Warden"));

        // e3: v1 again (legacy path), chained off e2
        let h3 = compute_event_hmac_legacy(
            EVOLUTION_HMAC_LEGACY,
            &e2.hmac,
            "xp_gain",
            1,
            "ops",
            "run_shell",
            3_700,
        );
        let e3 = EvolutionEvent {
            id: 3,
            event_type: "xp_gain".to_string(),
            xp_delta: 1,
            archetype: Some("ops".to_string()),
            source: "run_shell".to_string(),
            metadata_json: None,
            created_at: 3_700,
            hmac: h3,
            prev_hmac: e2.hmac.clone(),
        };

        let state = replay_events(&[e1, e2, e3]);
        assert!(
            state.chain_valid,
            "mixed v1/v2 chain should verify end-to-end"
        );
        assert_eq!(state.stage, Stage::Evolved);
        assert_eq!(state.evolution_name.as_deref(), Some("Warden"));
        // Post-evolution xp_gain of 1 should be the only XP in current stage.
        assert_eq!(state.total_xp, 1);
    }

    #[test]
    fn replay_hmac_key_mismatch_invalidates_all_events_evolution() {
        let e1 = make_event(1, "xp_gain", 3, Some("ops"), "run_shell", 1_000, "0");
        let e2 = make_event(2, "xp_gain", 3, Some("ops"), "run_shell", 3_700, &e1.hmac);
        let other = b"not-the-evolution-hmac-key------";
        let state = replay_events_with_key(other, &[e1, e2]);
        assert!(!state.chain_valid);
        assert_eq!(state.total_xp, 0);
        assert_eq!(state.total_events, 0);
    }

    #[test]
    fn replay_level_up_across_multi_hour_ledger() {
        // Reaching Stage::Base Lvl.1 costs 20 XP. Earn it by spreading events
        // across many hours with distinct sources to defeat source decay,
        // confirming XP accumulates cleanly across hour bucket transitions.
        let mut events = Vec::new();
        let mut prev = "0".to_string();
        // 12 hours, 2 xp_gain events per hour with distinct sources → 24 XP.
        for hour in 0..12 {
            for slot in 0..2 {
                let ts = (hour as i64 + 1) * 3_600 + slot;
                let source = format!("src_{hour}_{slot}");
                let e = make_event(
                    events.len() as i64 + 1,
                    "xp_gain",
                    1,
                    Some("ops"),
                    &source,
                    ts,
                    &prev,
                );
                prev = e.hmac.clone();
                events.push(e);
            }
        }
        let state = replay_events(&events);
        assert!(state.chain_valid);
        assert_eq!(state.total_xp, 24);
        // Level 0 costs 20 XP → after 24 XP we should be at level 1 with 4 XP into it.
        assert_eq!(state.level, 1);
        // xp_to_next = xp_for_level(stage, level=1) - 4 = (20 + floor(1^1.4)) - 4 = 21 - 4 = 17.
        assert_eq!(state.xp_to_next_level, 17);
    }

    // ── Celebration Messages ──

    fn test_payload(to_stage: &str) -> CelebrationPayload {
        CelebrationPayload {
            from_stage: if to_stage == "final" {
                "evolved".to_string()
            } else {
                "base".to_string()
            },
            to_stage: to_stage.to_string(),
            evolution_name: Some("Pipeline Warden".to_string()),
            evolution_description: Some(
                "A vigilant DevOps guardian that keeps your builds green".to_string(),
            ),
            dominant_archetype: Some("ops".to_string()),
            bond_score: 45,
            stability: 85,
            focus: 72,
            sync_stat: 90,
            growth: 65,
            happiness: 88,
        }
    }

    #[test]
    fn celebration_message_base_to_evolved() {
        let payload = test_payload("evolved");
        let msg = format_celebration_message(&payload);
        assert!(
            msg.contains("E V O L U T I O N"),
            "should have evolution header"
        );
        assert!(
            msg.contains("Pipeline Warden"),
            "should contain evolution name"
        );
        assert!(msg.contains("Stage 1/3"), "should show from stage");
        assert!(msg.contains("Stage 2/3"), "should show to stage");
        assert!(msg.contains("-->"), "should have transition arrow");
        // Verify box borders present
        assert!(msg.contains('\u{2554}'), "should have top-left corner");
        assert!(msg.contains('\u{255A}'), "should have bottom-left corner");
    }

    #[test]
    fn celebration_message_evolved_to_final() {
        let payload = test_payload("final");
        let msg = format_celebration_message(&payload);
        assert!(
            msg.contains("F I N A L   F O R M"),
            "should have final form header"
        );
        assert!(msg.contains("Stage 2/3"), "should show from stage");
        assert!(msg.contains("Stage 3/3"), "should show to stage");
    }

    #[test]
    fn celebration_message_no_name() {
        let mut payload = test_payload("evolved");
        payload.evolution_name = None;
        let msg = format_celebration_message(&payload);
        assert!(msg.contains("Base Borg"), "should fall back to Base Borg");
        assert!(
            msg.contains("Evolved Borg"),
            "should fall back to Evolved Borg"
        );
    }

    #[test]
    fn celebration_message_no_archetype() {
        let mut payload = test_payload("evolved");
        payload.dominant_archetype = None;
        let msg = format_celebration_message(&payload);
        assert!(!msg.contains("Archetype:"), "should omit archetype line");
    }

    #[test]
    fn celebration_message_includes_vitals_and_bond() {
        let payload = test_payload("evolved");
        let msg = format_celebration_message(&payload);
        assert!(msg.contains("STB:  85"), "should show stability");
        assert!(msg.contains("FOC:  72"), "should show focus");
        assert!(msg.contains("SYN:  90"), "should show sync");
        assert!(msg.contains("GRW:  65"), "should show growth");
        assert!(msg.contains("HAP:  88"), "should show happiness");
        assert!(msg.contains("Bond: 45"), "should show bond score");
    }

    #[test]
    fn celebration_message_includes_description() {
        let payload = test_payload("evolved");
        let msg = format_celebration_message(&payload);
        assert!(
            msg.contains("vigilant DevOps"),
            "should contain description text"
        );
    }

    #[test]
    fn celebration_art_default_for_unknown_stage() {
        let art = celebration_art("unknown", None);
        assert!(!art.lines.is_empty(), "should return default art");
    }

    #[test]
    fn celebration_art_final_differs_from_evolved() {
        let evolved_art = celebration_art("evolved", None);
        let final_art = celebration_art("final", None);
        assert_ne!(
            evolved_art.lines, final_art.lines,
            "evolved and final art should differ"
        );
    }

    #[test]
    fn celebration_payload_serialization_round_trip() {
        let payload = test_payload("evolved");
        let json = serde_json::to_string(&payload).unwrap();
        let deserialized: CelebrationPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.from_stage, "base");
        assert_eq!(deserialized.to_stage, "evolved");
        assert_eq!(
            deserialized.evolution_name.as_deref(),
            Some("Pipeline Warden")
        );
        assert_eq!(deserialized.bond_score, 45);
        assert_eq!(deserialized.stability, 85);
    }

    // ── Time-weighted scoring and dominant_history ──

    /// Helper: build an xp_gain event with explicit metadata.
    #[allow(clippy::too_many_arguments)]
    fn make_event_with_meta(
        id: i64,
        event_type: &str,
        xp_delta: i32,
        archetype: Option<&str>,
        source: &str,
        created_at: i64,
        prev_hmac: &str,
        metadata: Option<&str>,
    ) -> EvolutionEvent {
        let hmac = compute_event_hmac(
            EVOLUTION_HMAC_LEGACY,
            prev_hmac,
            event_type,
            xp_delta,
            archetype.unwrap_or(""),
            source,
            metadata.unwrap_or(""),
            created_at,
        );
        EvolutionEvent {
            id,
            event_type: event_type.to_string(),
            xp_delta,
            archetype: archetype.map(|s| s.to_string()),
            source: source.to_string(),
            metadata_json: metadata.map(|s| s.to_string()),
            created_at,
            hmac,
            prev_hmac: prev_hmac.to_string(),
        }
    }

    #[test]
    fn time_weighted_scoring_prefers_recent_when_in_window() {
        // Ops dominates lifetime (12 old events), Builder dominates last 30d (3 recent events).
        // With weights 0.35/0.65, effective should favor Builder.
        let now = 1_700_000_000i64;
        let old_ts = now - 90 * 86_400; // way outside 30d window
        let recent_ts = now - 5 * 86_400;

        let mut events = Vec::new();
        let mut prev = "0".to_string();
        // 12 Ops events with distinct sources far in the past
        for i in 0..12 {
            let e = make_event(
                i + 1,
                "xp_gain",
                1,
                Some("ops"),
                &format!("op_src_{i}"),
                old_ts + i * 60,
                &prev,
            );
            prev = e.hmac.clone();
            events.push(e);
        }
        // 3 Builder events recently
        for i in 0..3 {
            let e = make_event(
                100 + i,
                "xp_gain",
                3,
                Some("builder"),
                &format!("bld_src_{i}"),
                recent_ts + i * 60,
                &prev,
            );
            prev = e.hmac.clone();
            events.push(e);
        }
        let state = replay_events_with_key_at(EVOLUTION_HMAC_LEGACY, &events, now);
        // Builder: lifetime=9, last_30d=9 → effective = 9*0.35 + 9*0.65 = 9
        // Ops: lifetime=12, last_30d=0 → effective = 12*0.35 = 4.2 → 4
        assert_eq!(state.dominant_archetype, Some(Archetype::Builder));
        assert_eq!(*state.lifetime_scores.get(&Archetype::Ops).unwrap(), 12);
        assert_eq!(state.last_30d_scores.get(&Archetype::Ops), None);
        assert_eq!(*state.lifetime_scores.get(&Archetype::Builder).unwrap(), 9);
    }

    #[test]
    fn time_weighted_scoring_reverts_when_window_shifts_out() {
        // Same as above, but now_ts is moved forward so even the "recent" Builder events fall
        // outside the 30-day window. Lifetime should dominate: Ops wins.
        let original_now = 1_700_000_000i64;
        let old_ts = original_now - 90 * 86_400;
        let recent_ts = original_now - 5 * 86_400;
        let mut events = Vec::new();
        let mut prev = "0".to_string();
        for i in 0..12 {
            let e = make_event(
                i + 1,
                "xp_gain",
                1,
                Some("ops"),
                &format!("op_src_{i}"),
                old_ts + i * 60,
                &prev,
            );
            prev = e.hmac.clone();
            events.push(e);
        }
        for i in 0..3 {
            let e = make_event(
                100 + i,
                "xp_gain",
                3,
                Some("builder"),
                &format!("bld_src_{i}"),
                recent_ts + i * 60,
                &prev,
            );
            prev = e.hmac.clone();
            events.push(e);
        }
        // Shift forward 60 days → nothing is within last 30d
        let future_now = original_now + 60 * 86_400;
        let state = replay_events_with_key_at(EVOLUTION_HMAC_LEGACY, &events, future_now);
        assert_eq!(state.dominant_archetype, Some(Archetype::Ops));
        // last_30d is empty for both archetypes
        assert!(state.last_30d_scores.is_empty());
    }

    #[test]
    fn dominant_history_records_shifts() {
        let now = 1_700_000_000i64;
        let mut events = Vec::new();
        let mut prev = "0".to_string();
        // 3 Ops events (becomes dominant at first event)
        for i in 0..3 {
            let ts = now - (60 - i as i64) * 86_400;
            let e = make_event(
                i + 1,
                "xp_gain",
                3,
                Some("ops"),
                &format!("op_{i}"),
                ts,
                &prev,
            );
            prev = e.hmac.clone();
            events.push(e);
        }
        // Now a burst of Builder events that should eventually flip dominance
        for i in 0..10 {
            let ts = now - (30 - i as i64) * 86_400;
            let e = make_event(
                100 + i,
                "xp_gain",
                3,
                Some("builder"),
                &format!("bld_{i}"),
                ts,
                &prev,
            );
            prev = e.hmac.clone();
            events.push(e);
        }
        let state = replay_events_with_key_at(EVOLUTION_HMAC_LEGACY, &events, now);
        assert!(
            state.dominant_history.len() >= 2,
            "expected at least one shift, got {:?}",
            state.dominant_history
        );
        assert_eq!(state.dominant_history.first().unwrap().1, Archetype::Ops);
        assert_eq!(state.dominant_history.last().unwrap().1, Archetype::Builder);
    }

    #[test]
    fn classification_event_sets_evolution_name() {
        // Seed enough xp to allow the evolution event to advance stage, then
        // follow with a classification event carrying the LLM-generated name.
        let now = 1_700_000_000i64;
        let e1 = make_event(1, "xp_gain", 3, Some("ops"), "run_shell", now - 86_400, "0");
        let evo_meta = r#"{"gates_verified":true,"pending_name":true}"#;
        let e2 = make_event_with_meta(
            2,
            "evolution",
            0,
            None,
            "gate_check",
            now - 86_000,
            &e1.hmac,
            Some(evo_meta),
        );
        let class_meta = r#"{"name":"Tool Forgemaster","description":"A restless builder."}"#;
        let e3 = make_event_with_meta(
            3,
            "classification",
            0,
            Some("builder"),
            "llm_naming",
            now - 85_000,
            &e2.hmac,
            Some(class_meta),
        );
        let state = replay_events_with_key_at(EVOLUTION_HMAC_LEGACY, &[e1, e2, e3], now);
        assert_eq!(state.stage, Stage::Evolved);
        assert_eq!(state.evolution_name.as_deref(), Some("Tool Forgemaster"));
        assert_eq!(
            state.evolution_description.as_deref(),
            Some("A restless builder.")
        );
    }

    #[test]
    fn classification_does_not_advance_stage() {
        // Classification events carry names but must not trigger a stage change.
        let now = 1_700_000_000i64;
        let class_meta = r#"{"name":"Name","description":"Desc"}"#;
        let e1 = make_event_with_meta(
            1,
            "classification",
            0,
            Some("ops"),
            "llm_naming",
            now - 1000,
            "0",
            Some(class_meta),
        );
        let state = replay_events_with_key_at(EVOLUTION_HMAC_LEGACY, &[e1], now);
        assert_eq!(state.stage, Stage::Base);
        assert_eq!(state.evolution_name.as_deref(), Some("Name"));
    }

    #[test]
    fn source_decay_contract_holds() {
        // 4 same-source xp_gain events with xp_delta=2 in a single hour:
        // decay multipliers 1.0, 1.0, 0.5, 0.25 → effective = 2+2+1+0 = 5.
        let now = 1_700_000_000i64;
        let mut events = Vec::new();
        let mut prev = "0".to_string();
        for i in 0..4 {
            let e = make_event(
                i + 1,
                "xp_gain",
                2,
                Some("ops"),
                "same_src",
                now - 3_000 + i * 10,
                &prev,
            );
            prev = e.hmac.clone();
            events.push(e);
        }
        let state = replay_events_with_key_at(EVOLUTION_HMAC_LEGACY, &events, now);
        assert_eq!(state.total_xp, 5, "decay contract: 2+2+1+0 = 5");
    }

    // ── Archetype stability via trusted replay ──

    #[test]
    fn stable_days_reflects_last_dominant_shift() {
        let db = test_db();
        for i in 0..3 {
            db.record_evolution_event("xp_gain", 3, Some("ops"), &format!("op_{i}"), None)
                .unwrap();
        }
        for i in 0..10 {
            db.record_evolution_event("xp_gain", 3, Some("builder"), &format!("bld_{i}"), None)
                .unwrap();
        }
        let days = compute_archetype_stable_days(&db);
        // Events just recorded → shift just happened → 0 days.
        assert!(days <= 1, "expected stable_days <= 1, got {days}");
        let state = db.get_evolution_state().unwrap();
        assert_eq!(state.dominant_archetype, Some(Archetype::Builder));
        assert!(state.dominant_history.len() >= 2);
        assert_eq!(state.dominant_history.first().unwrap().1, Archetype::Ops);
        assert_eq!(state.dominant_history.last().unwrap().1, Archetype::Builder);
    }

    #[test]
    fn stable_days_ignores_hmac_tampered_rows() {
        // Valid Ops events through the normal path.
        let db = test_db();
        for i in 0..3 {
            db.record_evolution_event("xp_gain", 3, Some("ops"), &format!("op_{i}"), None)
                .unwrap();
        }
        // Sneak in a Builder row with a bogus HMAC — bypasses record_evolution_event.
        let now = chrono::Utc::now().timestamp();
        db.conn()
            .execute(
                "INSERT INTO evolution_events (event_type, xp_delta, archetype, source,
                    metadata_json, created_at, hmac, prev_hmac)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    "xp_gain",
                    3,
                    "builder",
                    "tampered_src",
                    Option::<String>::None,
                    now,
                    "bogus_hmac",
                    "bogus_prev",
                ],
            )
            .unwrap();
        let state = db.get_evolution_state().unwrap();
        assert_eq!(state.dominant_archetype, Some(Archetype::Ops));
        assert!(!state.chain_valid);
        assert_eq!(state.lifetime_scores.get(&Archetype::Builder), None);
    }

    // ── Fallback naming ──

    #[test]
    fn fallback_name_per_archetype_and_stage() {
        let (n_s2, d) =
            classification::fallback_evolution_name(Some(Archetype::Ops), Stage::Evolved);
        assert_eq!(n_s2, "Pipeline Warden");
        assert!(!d.is_empty());
        let (n_s3, _) = classification::fallback_evolution_name(Some(Archetype::Ops), Stage::Final);
        assert_eq!(n_s3, "Infrastructure Sovereign");
        for a in [
            Archetype::Builder,
            Archetype::Analyst,
            Archetype::Communicator,
            Archetype::Guardian,
            Archetype::Strategist,
            Archetype::Creator,
            Archetype::Caretaker,
            Archetype::Merchant,
            Archetype::Tinkerer,
        ] {
            let (n2, d2) = classification::fallback_evolution_name(Some(a), Stage::Evolved);
            let (n3, d3) = classification::fallback_evolution_name(Some(a), Stage::Final);
            assert!(!n2.is_empty() && !d2.is_empty(), "empty for {a:?} s2");
            assert!(!n3.is_empty() && !d3.is_empty(), "empty for {a:?} s3");
            assert_ne!(n2, n3, "s2 and s3 names must differ for {a:?}");
        }
    }

    #[test]
    fn fallback_name_no_archetype_returns_unbound() {
        let (n, _) = classification::fallback_evolution_name(None, Stage::Evolved);
        assert_eq!(n, "Unbound Borg");
    }

    // ── EvolutionHook disabled gating ──

    #[test]
    fn hook_disabled_writes_no_events() {
        let db = Database::test_db();
        let hook = EvolutionHook::new_with(db, /* enabled */ false);
        let ctx = HookContext {
            point: HookPoint::SessionStart,
            session_id: "test".to_string(),
            turn_count: 0,
            data: HookData::SessionStart {
                session_id: "test".to_string(),
            },
        };
        let action = hook.execute(&ctx);
        assert!(matches!(action, HookAction::Continue));
        let events = hook.db.lock().unwrap().load_all_evolution_events().unwrap();
        assert!(
            events.is_empty(),
            "disabled hook should write nothing; got {} events",
            events.len()
        );
    }

    #[test]
    fn hook_enabled_session_start_records_xp() {
        let db = Database::test_db();
        let hook = EvolutionHook::new_with(db, /* enabled */ true);
        let ctx = HookContext {
            point: HookPoint::SessionStart,
            session_id: "test".to_string(),
            turn_count: 0,
            data: HookData::SessionStart {
                session_id: "test".to_string(),
            },
        };
        hook.execute(&ctx);
        let events = hook.db.lock().unwrap().load_all_evolution_events().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "xp_gain");
        assert_eq!(events[0].source, "session_start");
        assert_eq!(events[0].xp_delta, BASE_XP_INTERACTION);
    }

    #[test]
    fn hook_tool_result_success_records_creation_bonus() {
        let db = Database::test_db();
        let hook = EvolutionHook::new_with(db, /* enabled */ true);
        let ctx = HookContext {
            point: HookPoint::AfterToolCall,
            session_id: "test".to_string(),
            turn_count: 1,
            data: HookData::ToolResult {
                name: "apply_patch".to_string(),
                result: String::new(),
                is_error: false,
            },
        };
        hook.execute(&ctx);
        let events = hook.db.lock().unwrap().load_all_evolution_events().unwrap();
        // Expect an xp_gain row plus a mood_changed row: fresh DB starts
        // Drifting (no dominant archetype); after the Builder XP the
        // archetype settles and mood flips to Stable.
        let xp_events: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == "xp_gain")
            .collect();
        assert_eq!(xp_events.len(), 1);
        // apply_patch → Builder, creation tool: BASE_XP_CREATION (2) + BONUS_XP_CREATION_ALIGNED (1).
        assert_eq!(
            xp_events[0].xp_delta,
            BASE_XP_CREATION + BONUS_XP_CREATION_ALIGNED
        );
        assert_eq!(xp_events[0].archetype.as_deref(), Some("builder"));
    }

    #[test]
    fn hook_tool_result_error_records_nothing() {
        let db = Database::test_db();
        let hook = EvolutionHook::new_with(db, /* enabled */ true);
        let ctx = HookContext {
            point: HookPoint::AfterToolCall,
            session_id: "test".to_string(),
            turn_count: 1,
            data: HookData::ToolResult {
                name: "apply_patch".to_string(),
                result: "boom".to_string(),
                is_error: true,
            },
        };
        hook.execute(&ctx);
        let events = hook.db.lock().unwrap().load_all_evolution_events().unwrap();
        assert!(events.is_empty(), "error tool calls must not record XP");
    }

    // ── Milestone emission via emit_milestone helper ──

    #[test]
    fn emit_milestone_inserts_level_up_and_celebration() {
        let db = Database::test_db();
        let next = EvolutionState {
            stage: Stage::Base,
            level: 10,
            total_xp: 0,
            xp_to_next_level: 0,
            dominant_archetype: Some(Archetype::Ops),
            evolution_name: None,
            evolution_description: None,
            archetype_scores: HashMap::new(),
            lifetime_scores: HashMap::new(),
            last_30d_scores: HashMap::new(),
            dominant_history: Vec::new(),
            total_events: 0,
            chain_valid: true,
            momentum: HashMap::new(),
            level_up_events_recent: Vec::new(),
            mood: None,
            readiness: None,
        };
        let m = Milestone {
            id: "level_10_base".to_string(),
            title: "Lvl.10".to_string(),
            archetype: Some(Archetype::Ops),
        };
        emit_milestone(&db, &m, &next);

        let events = db.load_all_evolution_events().unwrap();
        let types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
        assert!(
            types.contains(&"level_up"),
            "expected level_up event; got: {types:?}"
        );
        assert!(
            types.contains(&"milestone_unlocked"),
            "expected milestone_unlocked event; got: {types:?}"
        );

        let pending = db.get_pending_celebrations().unwrap();
        assert_eq!(pending.len(), 1, "expected one pending celebration");
        assert_eq!(pending[0].celebration_type, "milestone");
        // Payload should deserialize cleanly back into a MilestonePayload.
        let payload: celebration::MilestonePayload =
            serde_json::from_str(&pending[0].payload_json).unwrap();
        assert_eq!(payload.milestone_id, "level_10_base");
        assert_eq!(payload.level, 10);
        assert_eq!(payload.stage, "base");
        assert_eq!(payload.archetype.as_deref(), Some("ops"));
    }

    #[test]
    fn emit_milestone_non_level_skips_level_up_event() {
        let db = Database::test_db();
        let next = EvolutionState {
            stage: Stage::Base,
            level: 5,
            total_xp: 0,
            xp_to_next_level: 0,
            dominant_archetype: None,
            evolution_name: None,
            evolution_description: None,
            archetype_scores: HashMap::new(),
            lifetime_scores: HashMap::new(),
            last_30d_scores: HashMap::new(),
            dominant_history: Vec::new(),
            total_events: 0,
            chain_valid: true,
            momentum: HashMap::new(),
            level_up_events_recent: Vec::new(),
            mood: None,
            readiness: None,
        };
        let m = Milestone {
            id: "first_strong_bond".to_string(),
            title: "Strong Bond".to_string(),
            archetype: None,
        };
        emit_milestone(&db, &m, &next);

        let events = db.load_all_evolution_events().unwrap();
        let level_ups: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == "level_up")
            .collect();
        assert!(
            level_ups.is_empty(),
            "non-level milestone should not emit level_up"
        );
        let milestones: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == "milestone_unlocked")
            .collect();
        assert_eq!(milestones.len(), 1);
    }

    #[test]
    fn parse_level_from_id_handles_stage_suffix() {
        assert_eq!(parse_level_from_id("level_10_base"), Some(10));
        assert_eq!(parse_level_from_id("level_99_final"), Some(99));
        assert_eq!(parse_level_from_id("first_evolution"), None);
        assert_eq!(parse_level_from_id("level_abc_base"), None);
    }

    // ── V2: new event types (level_up, milestone_unlocked, mood_changed, share_card_created) ──

    #[test]
    fn replay_accepts_new_v2_event_types() {
        // xp_gain + level_up + milestone_unlocked chain cleanly and replay
        // ignores the informational rows without corrupting state.
        let e1 = make_event(1, "xp_gain", 3, Some("ops"), "run_shell", 1000, "0");
        let e2 = {
            let mut e = make_event(2, "level_up", 0, None, "hook", 1500, &e1.hmac);
            e.metadata_json = Some(r#"{"from_level":0,"to_level":1,"stage":"base"}"#.to_string());
            // Re-sign with metadata so v2 HMAC matches.
            e.hmac = compute_event_hmac(
                EVOLUTION_HMAC_LEGACY,
                &e.prev_hmac,
                &e.event_type,
                e.xp_delta,
                e.archetype.as_deref().unwrap_or(""),
                &e.source,
                e.metadata_json.as_deref().unwrap_or(""),
                e.created_at,
            );
            e
        };
        let e3 = {
            let mut e = make_event(3, "milestone_unlocked", 0, None, "hook", 1600, &e2.hmac);
            e.metadata_json = Some(r#"{"milestone_id":"level_10","title":"Lvl.10"}"#.to_string());
            e.hmac = compute_event_hmac(
                EVOLUTION_HMAC_LEGACY,
                &e.prev_hmac,
                &e.event_type,
                e.xp_delta,
                e.archetype.as_deref().unwrap_or(""),
                &e.source,
                e.metadata_json.as_deref().unwrap_or(""),
                e.created_at,
            );
            e
        };

        let state = replay_events(&[e1, e2, e3]);
        assert!(
            state.chain_valid,
            "chain should validate across v2 event types"
        );
        assert_eq!(state.total_xp, 3);
        assert_eq!(
            state.level_up_events_recent.len(),
            2,
            "level_up + milestone_unlocked should populate the feed"
        );
        // Newest first.
        assert_eq!(
            state.level_up_events_recent[0].event_type,
            "milestone_unlocked"
        );
    }

    #[test]
    fn hmac_tamper_detection_on_new_event_types() {
        let meta = r#"{"from_level":9,"to_level":10,"stage":"base"}"#;
        let hmac = compute_event_hmac(
            EVOLUTION_HMAC_LEGACY,
            "0",
            "level_up",
            0,
            "",
            "hook",
            meta,
            1000,
        );
        let mut event = EvolutionEvent {
            id: 1,
            event_type: "level_up".to_string(),
            xp_delta: 0,
            archetype: None,
            source: "hook".to_string(),
            metadata_json: Some(meta.to_string()),
            created_at: 1000,
            hmac,
            prev_hmac: "0".to_string(),
        };
        // Pristine event verifies.
        assert!(verify_event_hmac(EVOLUTION_HMAC_LEGACY, &event, "0"));
        // Tamper with metadata — v2 HMAC must reject.
        event.metadata_json = Some(r#"{"from_level":9,"to_level":99,"stage":"base"}"#.to_string());
        assert!(!verify_event_hmac(EVOLUTION_HMAC_LEGACY, &event, "0"));
    }

    #[test]
    fn replay_populates_momentum_from_recent_xp() {
        let now = chrono::Utc::now().timestamp();
        let recent = now - 86_400;
        let e1 = make_event(1, "xp_gain", 3, Some("ops"), "run_shell", recent, "0");
        let e2 = make_event(
            2,
            "xp_gain",
            3,
            Some("ops"),
            "run_shell",
            recent + 1,
            &e1.hmac,
        );
        let state = replay_events(&[e1, e2]);
        // At least one archetype should have a trend classification.
        assert!(!state.momentum.is_empty());
        assert!(state.momentum.contains_key(&Archetype::Ops));
    }

    #[test]
    fn hook_agent_start_injects_context() {
        let db = Database::test_db();
        let hook = EvolutionHook::new_with(db, /* enabled */ true);
        let ctx = HookContext {
            point: HookPoint::BeforeAgentStart,
            session_id: "test".to_string(),
            turn_count: 0,
            data: HookData::AgentStart {
                user_message: "hi".to_string(),
            },
        };
        match hook.execute(&ctx) {
            HookAction::InjectContext(s) => {
                assert!(s.contains("<evolution_context>"), "got: {s}");
            }
            other => panic!("expected InjectContext, got {other:?}"),
        }
    }

    fn mk_bond_for_mood(score: u8) -> crate::bond::BondState {
        use crate::bond::{AutonomyTier, BondLevel, BondState};
        BondState {
            score,
            level: BondLevel::Fragile,
            autonomy_tier: AutonomyTier::ObserveOnly,
            total_events: 0,
            chain_valid: true,
        }
    }

    fn mk_vitals_for_mood(v: u8) -> crate::vitals::VitalsState {
        use chrono::Utc;
        crate::vitals::VitalsState {
            stability: v,
            focus: v,
            sync: v,
            growth: v,
            happiness: v,
            last_interaction_at: Utc::now(),
            updated_at: Utc::now(),
            chain_valid: true,
        }
    }

    fn mk_evo_for_mood(stage: Stage, level: u8, arch: Option<Archetype>) -> EvolutionState {
        let mut scores: HashMap<Archetype, u32> = HashMap::new();
        if let Some(a) = arch {
            scores.insert(a, 100);
        }
        EvolutionState {
            stage,
            level,
            total_xp: 1,
            xp_to_next_level: 10,
            dominant_archetype: arch,
            evolution_name: None,
            evolution_description: None,
            archetype_scores: scores.clone(),
            lifetime_scores: scores.clone(),
            last_30d_scores: scores,
            dominant_history: arch.map(|a| vec![(0, a)]).unwrap_or_default(),
            total_events: 0,
            chain_valid: true,
            momentum: HashMap::new(),
            level_up_events_recent: Vec::new(),
            mood: None,
            readiness: None,
        }
    }

    #[test]
    fn mood_strained_to_stable_when_min_vital_recovers() {
        let evo = mk_evo_for_mood(Stage::Base, 20, Some(Archetype::Builder));
        let bond = mk_bond_for_mood(50);

        // Below 30: Strained.
        let low = mk_vitals_for_mood(25);
        assert_eq!(compute_mood(&evo, &low, &bond), Mood::Strained);

        // At 30: Strained gone; with focus=30 not ≥70, falls back to Stable.
        let recovered = mk_vitals_for_mood(30);
        assert_eq!(compute_mood(&evo, &recovered, &bond), Mood::Stable);
    }

    #[test]
    fn mood_stable_to_focused_when_focus_and_stability_cross_threshold() {
        use chrono::Utc;
        let evo = mk_evo_for_mood(Stage::Evolved, 10, Some(Archetype::Ops));
        let bond = mk_bond_for_mood(50);

        // Stability=60, focus=50 → Stable (focus below 70 threshold).
        let stable = crate::vitals::VitalsState {
            stability: 60,
            focus: 50,
            sync: 50,
            growth: 30,
            happiness: 50,
            last_interaction_at: Utc::now(),
            updated_at: Utc::now(),
            chain_valid: true,
        };
        assert_eq!(compute_mood(&evo, &stable, &bond), Mood::Stable);

        // Focus=70 crosses → Focused.
        let focused = crate::vitals::VitalsState {
            focus: 70,
            ..stable
        };
        assert_eq!(compute_mood(&evo, &focused, &bond), Mood::Focused);
    }

    #[test]
    fn hook_emits_mood_changed_on_transition() {
        // `snapshot_mood` + `emit_mood_changed` round-trip: drive a mood
        // transition through the tool-result hook path and assert the
        // mood_changed row is persisted with the expected metadata. A fresh
        // DB starts at mood=Drifting (no dominant archetype); a single
        // apply_patch call earns Builder XP which flips mood to Stable.
        let db = Database::test_db();
        let hook = EvolutionHook::new_with(db, /* enabled */ true);

        let ctx = HookContext {
            point: HookPoint::AfterToolCall,
            session_id: "test".to_string(),
            turn_count: 1,
            data: HookData::ToolResult {
                name: "apply_patch".to_string(),
                result: String::new(),
                is_error: false,
            },
        };
        hook.execute(&ctx);

        let events = hook.db.lock().unwrap().load_all_evolution_events().unwrap();
        let mood_events: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == "mood_changed")
            .collect();
        assert_eq!(mood_events.len(), 1, "expected 1 mood_changed event");
        let meta = mood_events[0].metadata_json.as_deref().unwrap();
        assert!(meta.contains("\"from_mood\":\"drifting\""), "got: {meta}");
        assert!(meta.contains("\"to_mood\":\"stable\""), "got: {meta}");
        assert!(meta.contains("\"reason\":\"apply_patch\""), "got: {meta}");
    }

    #[test]
    fn hook_xp_gain_crossing_level_threshold_inserts_milestone_celebration() {
        // End-to-end wiring test: pre-seed enough HMAC-valid xp_gain events
        // to land just under level 10 (Stage::Base), then drive one more
        // apply_patch call through the hook. The full `record_xp →
        // attempt_evolution → detect_milestones → emit_milestone` chain must
        // fire `level_up` + `milestone_unlocked` rows and insert a pending
        // milestone celebration with a `MilestonePayload` that round-trips.
        //
        // Seeded events use distinct sources per (hour, source) so source
        // decay stays at 1.0, and span multiple hours so replay's 15/hr
        // xp_gain cap doesn't drop any.
        use rusqlite::params;

        let db = Database::test_db();
        let evo_key = db.derive_hmac_key(EVOLUTION_HMAC_DOMAIN);

        // Compute seed target: total XP to reach level 9 is a known sum from
        // the level curve. Pick a seed XP such that `level_from_xp(Base, seed)`
        // yields `(9, xp_to_next)` with `xp_to_next <= 3` so one more hook
        // call (Builder alignment → 3 XP) crosses into level 10.
        let lvl9_total: u32 = (0..9).map(|n| xp_for_level(&Stage::Base, n)).sum();
        let lvl10_total: u32 = lvl9_total + xp_for_level(&Stage::Base, 9);
        let seed_target = lvl10_total.saturating_sub(3);
        assert!(seed_target >= lvl9_total, "sanity: seed within level 9");
        let xp_per_event: u32 = (BASE_XP_CREATION + BONUS_XP_CREATION_ALIGNED) as u32;
        let needed_events = seed_target.div_ceil(xp_per_event);

        // Place seeded events far enough in the past that they don't collide
        // with the hook's `now` hour bucket. 14 events/hr stays below the
        // 15/hr replay cap for `xp_gain`.
        let now_ts = chrono::Utc::now().timestamp();
        let base_ts = now_ts - 48 * 3600;
        let mut prev_hmac = "0".to_string();
        let mut seeded_xp: u32 = 0;

        for i in 0..needed_events {
            let hour = (i / 14) as i64;
            let sec = (i % 14) as i64;
            let ts = base_ts + hour * 3600 + sec;
            let source = format!("seed_tool_{i}");
            let hmac = compute_event_hmac(
                &evo_key,
                &prev_hmac,
                "xp_gain",
                xp_per_event as i32,
                "builder",
                &source,
                "",
                ts,
            );
            db.conn()
                .execute(
                    "INSERT INTO evolution_events (event_type, xp_delta, archetype, source,
                        metadata_json, created_at, hmac, prev_hmac)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        "xp_gain",
                        xp_per_event as i32,
                        "builder",
                        source,
                        Option::<String>::None,
                        ts,
                        hmac.clone(),
                        prev_hmac,
                    ],
                )
                .expect("seed insert");
            prev_hmac = hmac;
            seeded_xp += xp_per_event;
        }

        // Sanity: replay sees level 9 with Builder dominant, close enough to
        // threshold that +3 XP will cross into level 10.
        let pre = db.get_evolution_state().expect("pre state");
        assert!(pre.chain_valid, "HMAC chain must verify across seeded rows");
        assert_eq!(
            pre.level, 9,
            "seed must land at level 9 (seeded_xp={seeded_xp}, total_xp={})",
            pre.total_xp
        );
        assert_eq!(pre.dominant_archetype, Some(Archetype::Builder));
        assert!(
            pre.xp_to_next_level <= xp_per_event,
            "seed must be within one hook-call of level 10; xp_to_next={}",
            pre.xp_to_next_level
        );

        // Trigger: a single apply_patch tool result runs the full hook path
        // and should cross level 10.
        let hook = EvolutionHook::new_with(db, /* enabled */ true);
        let ctx = HookContext {
            point: HookPoint::AfterToolCall,
            session_id: "test".to_string(),
            turn_count: 1,
            data: HookData::ToolResult {
                name: "apply_patch".to_string(),
                result: String::new(),
                is_error: false,
            },
        };
        hook.execute(&ctx);

        let db_guard = hook.db.lock().unwrap();
        let events = db_guard.load_all_evolution_events().unwrap();
        let level_ups: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == "level_up")
            .collect();
        let milestones: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == "milestone_unlocked")
            .collect();
        assert!(
            !level_ups.is_empty(),
            "expected level_up event after crossing threshold"
        );
        assert!(
            milestones.iter().any(|e| e
                .metadata_json
                .as_deref()
                .map(|m| m.contains("\"milestone_id\":\"level_10_base\""))
                .unwrap_or(false)),
            "expected level_10_base milestone event; got milestones: {:?}",
            milestones
                .iter()
                .map(|e| e.metadata_json.as_deref().unwrap_or(""))
                .collect::<Vec<_>>()
        );

        let pending = db_guard.get_pending_celebrations().unwrap();
        let milestone_rows: Vec<_> = pending
            .iter()
            .filter(|c| c.celebration_type == "milestone")
            .collect();
        assert!(
            !milestone_rows.is_empty(),
            "expected pending milestone celebration row"
        );
        let payload: celebration::MilestonePayload =
            serde_json::from_str(&milestone_rows[0].payload_json)
                .expect("milestone payload round-trips");
        assert_eq!(payload.milestone_id, "level_10_base");
        assert_eq!(payload.level, 10);
        assert_eq!(payload.stage, "base");
    }

    #[test]
    fn hook_does_not_emit_mood_changed_when_stable() {
        // Second apply_patch after the first: both snapshots return Stable,
        // so no mood_changed event should be appended.
        let db = Database::test_db();
        let hook = EvolutionHook::new_with(db, /* enabled */ true);
        let ctx = HookContext {
            point: HookPoint::AfterToolCall,
            session_id: "test".to_string(),
            turn_count: 1,
            data: HookData::ToolResult {
                name: "apply_patch".to_string(),
                result: String::new(),
                is_error: false,
            },
        };
        hook.execute(&ctx);
        hook.execute(&ctx);

        let events = hook.db.lock().unwrap().load_all_evolution_events().unwrap();
        let mood_events: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == "mood_changed")
            .collect();
        assert_eq!(
            mood_events.len(),
            1,
            "only the first transition should emit; second call is stable→stable"
        );
    }
}
