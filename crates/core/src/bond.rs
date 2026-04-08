//! Bond system — operational trust tracking between user and agent.
//!
//! Bond score is event-sourced: derived by replaying verified events from a
//! baseline of 40. HMAC chain prevents tampering; rate limiting caps impact
//! per event type per hour to prevent gaming.
//!
//! The HMAC secret is compiled into the binary. This prevents casual SQL
//! tampering but is not cryptographically secure against a determined user
//! who can inspect the binary. The autonomy tier is informational only and
//! does not bypass HITL safety checks.

use chrono::Utc;
use regex::Regex;
use std::fmt;
use std::sync::LazyLock;

use crate::db::Database;
use crate::hmac_chain;
use crate::hooks::{Hook, HookAction, HookContext, HookData, HookPoint};
use crate::vitals;

// ── HMAC ──

/// Domain string for HMAC key derivation. Combined with per-installation salt.
pub const BOND_HMAC_DOMAIN: &[u8] = b"borg-bond-chain-v1";

/// Legacy compiled-in secret for installations without per-install salt.
#[cfg(test)]
const BOND_HMAC_LEGACY: &[u8] = b"borg-bond-chain-v1";

/// Compute HMAC for a bond event using the shared HMAC chain module.
pub(crate) fn compute_event_hmac(
    key: &[u8],
    prev_hmac: &str,
    event_type: &str,
    score_delta: i32,
    reason: &str,
    created_at: i64,
) -> String {
    hmac_chain::compute_hmac(
        key,
        &[
            prev_hmac.as_bytes(),
            event_type.as_bytes(),
            &score_delta.to_le_bytes(),
            reason.as_bytes(),
            &created_at.to_le_bytes(),
        ],
    )
}

/// Verify a bond event's HMAC against the expected chain.
fn verify_event_hmac(key: &[u8], event: &BondEvent, expected_prev_hmac: &str) -> bool {
    let recomputed = compute_event_hmac(
        key,
        &event.prev_hmac,
        &event.event_type,
        event.score_delta,
        &event.reason,
        event.created_at,
    );
    hmac_chain::verify_chain_link(
        &event.hmac,
        &event.prev_hmac,
        expected_prev_hmac,
        &recomputed,
    )
}

// ── Rate Limiting ──

/// Maximum events per type per hour during replay.
pub(crate) fn rate_limit_for(event_type: &str) -> u32 {
    match event_type {
        "tool_success" => 8,
        "tool_failure" => 5,
        "creation" => 3,
        "correction" => 3,
        "suggestion_accepted" => 3,
        "suggestion_rejected" => 3,
        _ => 5,
    }
}

// ── Types ──

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

const BASELINE_SCORE: i32 = 25;
const DELTA_TOOL_SUCCESS: i32 = 1;
const DELTA_CREATION_EVENT: i32 = 1;
const DELTA_SUGGESTION_ACCEPTED: i32 = 1;
const DELTA_CORRECTION: i32 = -2;
const DELTA_TOOL_FAILURE: i32 = -1;
const DELTA_SUGGESTION_REJECTED: i32 = -1;

// ── Pure Functions ──

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

// ── Event Replay (Event Sourcing) ──

/// Replay verified events from baseline to compute current bond state.
/// Verifies HMAC chain and applies per-type-per-hour rate limits.
///
/// A broken HMAC link invalidates the rest of the chain — all subsequent
/// events are skipped. This is intentional: the chain is append-only and
/// any tampering makes the tail untrustworthy.
pub fn replay_events(events: &[BondEvent]) -> BondState {
    replay_events_with_key(BOND_HMAC_DOMAIN, events)
}

/// Replay events with a specific HMAC key (for per-installation derived keys).
pub fn replay_events_with_key(key: &[u8], events: &[BondEvent]) -> BondState {
    const VALID_EVENT_TYPES: &[&str] = &[
        "tool_success",
        "tool_failure",
        "creation",
        "correction",
        "suggestion_accepted",
        "suggestion_rejected",
    ];

    let mut score: i32 = BASELINE_SCORE;
    let mut expected_prev_hmac = "0".to_string();
    let mut rate_limiter = hmac_chain::HourlyRateLimiter::new(Some(15), Some(8));
    let mut chain_valid = true;

    for event in events {
        // Verify HMAC chain — skip tampered/injected events
        if !verify_event_hmac(key, event, &expected_prev_hmac) {
            tracing::warn!("bond: skipping event {} with broken HMAC chain", event.id);
            chain_valid = false;
            continue;
        }
        // Advance chain pointer. Rate-limited events still advance the pointer
        // since they are verified and trusted, just capped for scoring.
        expected_prev_hmac.clone_from(&event.hmac);

        // Defense-in-depth: skip unknown event types
        if !VALID_EVENT_TYPES.contains(&event.event_type.as_str()) {
            continue;
        }

        // Combined rate limiting: total/hour, positive/hour, per-type/hour
        let cap = rate_limit_for(&event.event_type);
        let is_positive = event.score_delta > 0;
        if !rate_limiter.check_and_consume(event.created_at, &event.event_type, cap, is_positive) {
            continue;
        }

        // Apply delta
        score = (score + event.score_delta).clamp(0, 100);
    }

    let score = score as u8;
    let level = level_from_score(score);

    BondState {
        score,
        level,
        autonomy_tier: autonomy_from_level(level),
        total_events: u32::try_from(events.len()).unwrap_or(u32::MAX),
        chain_valid,
    }
}

// ── Rolling Metrics (computed on demand) ──

/// Compute routine success rate from task_runs in last 30 days.
pub fn compute_routine_success_rate(db: &Database) -> f32 {
    let since = Utc::now().timestamp() - 30 * 86400;
    match db.count_task_runs_since(since, Some("success")) {
        Ok((success, total)) if total > 0 => success as f32 / total as f32,
        _ => 0.0,
    }
}

/// Compute correction rate from vitals_events in last 30 days.
pub fn compute_correction_rate(db: &Database) -> f32 {
    let since = Utc::now().timestamp() - 30 * 86400;
    match db.count_vitals_events_by_category_since(since, "correction") {
        Ok((corrections, total)) if total > 0 => {
            let negatives = db
                .count_vitals_events_by_category_since(since, "negative_sentiment")
                .map(|(n, _)| n)
                .unwrap_or(0);
            (corrections + negatives) as f32 / total as f32
        }
        _ => 0.0,
    }
}

/// Count creation events (preference learning proxy) from vitals_events in last 30 days.
pub fn compute_preference_learning_count(db: &Database) -> u32 {
    let since = Utc::now().timestamp() - 30 * 86400;
    db.count_vitals_events_by_category_since(since, "creation")
        .map(|(count, _)| count)
        .unwrap_or(0)
}

// ── Suggestion Heuristics ──

/// Acceptance patterns require stronger signal than bare "yes" / "ok" to reduce
/// false positives from conversational filler. Multi-word phrases preferred.
#[allow(clippy::expect_used)]
static ACCEPTANCE_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(do it|go ahead|sounds good|approved|let'?s do it|yes,? do|yes please|absolutely)\b",
    )
    .expect("compile-time literal regex")
});

#[allow(clippy::expect_used)]
static REJECTION_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(no don'?t|don'?t do|cancel that|never mind|skip that|wrong suggestion|stop suggesting|not helpful)\b"
    ).expect("compile-time literal regex")
});

/// Returns `true` if the user message heuristically looks like acceptance.
pub fn looks_like_acceptance(msg: &str) -> bool {
    ACCEPTANCE_PATTERN.is_match(msg)
}

/// Returns `true` if the user message heuristically looks like rejection.
pub fn looks_like_rejection(msg: &str) -> bool {
    REJECTION_PATTERN.is_match(msg)
}

// ── Formatting ──

fn bar(val: u8, width: usize) -> String {
    let filled = (val as usize * width + 50) / 100;
    let empty = width - filled;
    format!("{}{}", "\u{2588}".repeat(filled), "\u{2591}".repeat(empty))
}

/// Compact XML context for system prompt injection (~60 tokens).
pub fn format_context(state: &BondState, correction_rate: f32, routine_rate: f32) -> String {
    format!(
        "<bond_context>\nBond: {} ({}/100) | Autonomy: {}\nCorrection rate: {:.0}% | Routine success: {:.0}%\n</bond_context>",
        state.level,
        state.score,
        state.autonomy_tier,
        correction_rate * 100.0,
        routine_rate * 100.0,
    )
}

/// Full status output for `borg bond`.
pub fn format_status(
    state: &BondState,
    correction_rate: f32,
    routine_rate: f32,
    pref_count: u32,
    recent_events: &[BondEvent],
) -> String {
    let mut out = String::from(
        "Bond Status\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n",
    );

    out.push_str(&format!(
        "  score        {}  {}\n",
        bar(state.score, 10),
        state.score
    ));
    out.push_str(&format!("  level        {}\n", state.level));
    out.push_str(&format!("  autonomy     {}\n", state.autonomy_tier));

    let integrity = if state.chain_valid {
        format!("Chain valid ({} events)", state.total_events)
    } else {
        format!("Chain broken ({} events)", state.total_events)
    };
    out.push_str(&format!("  integrity    {integrity}\n"));

    out.push_str("\n30d Signals\n");
    out.push_str(&format!(
        "  Routine Success Rate   {:.0}%\n",
        routine_rate * 100.0
    ));
    out.push_str(&format!(
        "  Correction Rate        {:.0}%\n",
        correction_rate * 100.0
    ));
    out.push_str(&format!("  Preferences Learned    {pref_count}\n"));

    if !recent_events.is_empty() {
        out.push_str("\nRecent Events\n");
        let now = Utc::now().timestamp();
        for event in recent_events.iter().take(5) {
            let ago = format_ago(now - event.created_at);
            let delta_str = if event.score_delta >= 0 {
                format!("+{}", event.score_delta)
            } else {
                format!("{}", event.score_delta)
            };
            out.push_str(&format!(
                "  {:<22} {:>3}  {:<20} {}\n",
                event.event_type, delta_str, event.reason, ago
            ));
        }
    }

    let tip = recommend(state);
    out.push_str(&format!("\nTip: {tip}\n"));

    out
}

/// Tabular event history for `borg bond history`.
pub fn format_history(events: &[BondEvent]) -> String {
    if events.is_empty() {
        return "No bond events recorded yet.\n".to_string();
    }

    let mut out = format!(
        "{:<6} {:<22} {:>6} {:<20} {}\n",
        "ID", "Type", "Delta", "Reason", "Time"
    );
    out.push_str(&"\u{2500}".repeat(70));
    out.push('\n');

    let now = Utc::now().timestamp();
    for event in events {
        let ago = format_ago(now - event.created_at);
        let delta_str = if event.score_delta >= 0 {
            format!("+{}", event.score_delta)
        } else {
            format!("{}", event.score_delta)
        };
        out.push_str(&format!(
            "{:<6} {:<22} {:>6} {:<20} {}\n",
            event.id, event.event_type, delta_str, event.reason, ago
        ));
    }
    out
}

fn format_ago(seconds: i64) -> String {
    if seconds < 0 {
        "in the future".to_string()
    } else if seconds < 60 {
        "just now".to_string()
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86400)
    }
}

fn recommend(state: &BondState) -> &'static str {
    match state.level {
        BondLevel::Fragile => "Complete successful tasks to rebuild trust",
        BondLevel::Emerging => "Keep using borg consistently to strengthen trust",
        BondLevel::Stable => "Trust is building — try creating tools or skills to grow",
        BondLevel::Trusted => "Strong trust established — keep completing tasks successfully",
        BondLevel::Synced => "Maximum trust achieved — maintain regular usage",
    }
}

// ── BondHook ──

/// Lifecycle hook that tracks bond events with HMAC chain integrity.
/// Caches the last replayed `BondState` to avoid replaying the full event
/// chain on every `InjectContext` call. Cache is refreshed after recording.
pub struct BondHook {
    /// Database handle wrapped in a Mutex for thread-safety.
    db: std::sync::Mutex<Database>,
    /// Cached bond state to avoid redundant full-chain replays.
    cached_state: std::sync::Mutex<Option<BondState>>,
}

impl BondHook {
    /// Create a new bond hook, opening a database connection and pre-computing state.
    pub fn new() -> anyhow::Result<Self> {
        let db = Database::open()?;
        // Pre-compute initial state using per-installation derived key
        let events = db.get_all_bond_events().unwrap_or_default();
        let hmac_key = db.derive_hmac_key(BOND_HMAC_DOMAIN);
        let state = replay_events_with_key(&hmac_key, &events);
        Ok(Self {
            db: std::sync::Mutex::new(db),
            cached_state: std::sync::Mutex::new(Some(state)),
        })
    }

    /// Record a bond event atomically (read prev_hmac + compute + insert in one transaction).
    fn record(&self, event_type: &str, delta: i32, reason: &str) {
        let Ok(db) = self.db.lock() else {
            tracing::warn!("bond: mutex poisoned, skipping event");
            return;
        };

        if let Err(e) = db.record_bond_event_chained(event_type, delta, reason) {
            tracing::warn!("bond: failed to record event: {e}");
            return;
        }

        // Invalidate cache so next inject_context refreshes
        if let Ok(mut cache) = self.cached_state.lock() {
            *cache = None;
        }
    }

    fn get_or_refresh_state(&self, db: &Database) -> BondState {
        if let Ok(cache) = self.cached_state.lock() {
            if let Some(state) = cache.as_ref() {
                return state.clone();
            }
        }
        let events = db.get_all_bond_events().unwrap_or_default();
        let hmac_key = db.derive_hmac_key(BOND_HMAC_DOMAIN);
        let state = replay_events_with_key(&hmac_key, &events);
        if let Ok(mut cache) = self.cached_state.lock() {
            *cache = Some(state.clone());
        }
        state
    }

    fn inject_context(&self) -> HookAction {
        let Ok(db) = self.db.lock() else {
            return HookAction::Continue;
        };
        let state = self.get_or_refresh_state(&db);
        let correction_rate = compute_correction_rate(&db);
        let routine_rate = compute_routine_success_rate(&db);
        HookAction::InjectContext(format_context(&state, correction_rate, routine_rate))
    }
}

impl Hook for BondHook {
    fn name(&self) -> &str {
        "bond"
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
        match &ctx.data {
            HookData::SessionStart { .. } => {
                // No scoring — just a checkpoint
                HookAction::Continue
            }
            HookData::AgentStart { user_message } => {
                // Check correction first (reuses vitals heuristic), then suggestion heuristics
                if vitals::looks_like_correction(user_message) {
                    self.record("correction", DELTA_CORRECTION, "user_message");
                } else if looks_like_acceptance(user_message) {
                    self.record(
                        "suggestion_accepted",
                        DELTA_SUGGESTION_ACCEPTED,
                        "user_message",
                    );
                } else if looks_like_rejection(user_message) {
                    self.record(
                        "suggestion_rejected",
                        DELTA_SUGGESTION_REJECTED,
                        "user_message",
                    );
                }
                // Inject bond context into system prompt
                self.inject_context()
            }
            HookData::LlmCall { .. } => {
                // Inject bond context for multi-step turns
                self.inject_context()
            }
            HookData::ToolResult { name, is_error, .. } => {
                let category = vitals::classify_tool(name, *is_error);
                match category {
                    vitals::EventCategory::Failure => {
                        self.record("tool_failure", DELTA_TOOL_FAILURE, name);
                    }
                    vitals::EventCategory::Creation => {
                        self.record("creation", DELTA_CREATION_EVENT, name);
                    }
                    vitals::EventCategory::Success => {
                        self.record("tool_success", DELTA_TOOL_SUCCESS, name);
                    }
                    _ => {}
                }
                HookAction::Continue
            }
            _ => HookAction::Continue,
        }
    }
}

impl std::fmt::Debug for BondHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BondHook").finish()
    }
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;

    // ── Level / Autonomy ──

    #[test]
    fn test_level_from_score_boundaries() {
        assert_eq!(level_from_score(0), BondLevel::Fragile);
        assert_eq!(level_from_score(24), BondLevel::Fragile);
        assert_eq!(level_from_score(25), BondLevel::Emerging);
        assert_eq!(level_from_score(44), BondLevel::Emerging);
        assert_eq!(level_from_score(45), BondLevel::Stable);
        assert_eq!(level_from_score(64), BondLevel::Stable);
        assert_eq!(level_from_score(65), BondLevel::Trusted);
        assert_eq!(level_from_score(84), BondLevel::Trusted);
        assert_eq!(level_from_score(85), BondLevel::Synced);
        assert_eq!(level_from_score(100), BondLevel::Synced);
    }

    #[test]
    fn test_autonomy_from_level() {
        assert_eq!(
            autonomy_from_level(BondLevel::Fragile),
            AutonomyTier::ObserveOnly
        );
        assert_eq!(
            autonomy_from_level(BondLevel::Emerging),
            AutonomyTier::Recommend
        );
        assert_eq!(
            autonomy_from_level(BondLevel::Stable),
            AutonomyTier::DraftAssist
        );
        assert_eq!(
            autonomy_from_level(BondLevel::Trusted),
            AutonomyTier::GuidedAction
        );
        assert_eq!(
            autonomy_from_level(BondLevel::Synced),
            AutonomyTier::HighTrust
        );
    }

    // ── HMAC ──

    #[test]
    fn test_compute_event_hmac_deterministic() {
        let h1 = compute_event_hmac(BOND_HMAC_LEGACY, "0", "tool_success", 1, "run_shell", 1000);
        let h2 = compute_event_hmac(BOND_HMAC_LEGACY, "0", "tool_success", 1, "run_shell", 1000);
        assert_eq!(h1, h2);
        assert!(!h1.is_empty());
    }

    #[test]
    fn test_compute_event_hmac_different_inputs() {
        let h1 = compute_event_hmac(BOND_HMAC_LEGACY, "0", "tool_success", 1, "run_shell", 1000);
        let h2 = compute_event_hmac(BOND_HMAC_LEGACY, "0", "tool_failure", -1, "run_shell", 1000);
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_compute_event_hmac_different_prev() {
        let h1 = compute_event_hmac(BOND_HMAC_LEGACY, "0", "tool_success", 1, "test", 1000);
        let h2 = compute_event_hmac(BOND_HMAC_LEGACY, "abc", "tool_success", 1, "test", 1000);
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_verify_chain_valid() {
        let hmac1 = compute_event_hmac(BOND_HMAC_LEGACY, "0", "tool_success", 1, "test", 1000);
        let hmac2 = compute_event_hmac(
            BOND_HMAC_LEGACY,
            &hmac1,
            "creation",
            1,
            "write_memory",
            2000,
        );
        let hmac3 = compute_event_hmac(
            BOND_HMAC_LEGACY,
            &hmac2,
            "tool_failure",
            -1,
            "run_shell",
            3000,
        );

        let events = vec![
            BondEvent {
                id: 1,
                event_type: "tool_success".to_string(),
                score_delta: 1,
                reason: "test".to_string(),
                hmac: hmac1.clone(),
                prev_hmac: "0".to_string(),
                created_at: 1000,
            },
            BondEvent {
                id: 2,
                event_type: "creation".to_string(),
                score_delta: 1,
                reason: "write_memory".to_string(),
                hmac: hmac2.clone(),
                prev_hmac: hmac1,
                created_at: 2000,
            },
            BondEvent {
                id: 3,
                event_type: "tool_failure".to_string(),
                score_delta: -1,
                reason: "run_shell".to_string(),
                hmac: hmac3,
                prev_hmac: hmac2,
                created_at: 3000,
            },
        ];

        let state = replay_events(&events);
        assert!(state.chain_valid);
        // 25 + 1 + 1 - 1 = 26
        assert_eq!(state.score, 26);
        assert_eq!(state.level, BondLevel::Emerging);
    }

    #[test]
    fn test_verify_chain_tampered_delta() {
        let hmac1 = compute_event_hmac(BOND_HMAC_LEGACY, "0", "tool_success", 1, "test", 1000);

        let events = vec![BondEvent {
            id: 1,
            event_type: "tool_success".to_string(),
            score_delta: 99, // Tampered delta (was 1)
            reason: "test".to_string(),
            hmac: hmac1,
            prev_hmac: "0".to_string(),
            created_at: 1000,
        }];

        let state = replay_events(&events);
        assert!(!state.chain_valid);
        // Tampered event is skipped, score stays at baseline
        assert_eq!(state.score, BASELINE_SCORE as u8);
    }

    #[test]
    fn test_verify_chain_mid_corruption_invalidates_tail() {
        // Event 1: valid, event 2: tampered, event 3: valid but after broken chain
        let hmac1 = compute_event_hmac(BOND_HMAC_LEGACY, "0", "tool_success", 1, "test", 1000);
        let hmac2_real = compute_event_hmac(
            BOND_HMAC_LEGACY,
            &hmac1,
            "creation",
            1,
            "write_memory",
            2000,
        );
        let hmac3 = compute_event_hmac(
            BOND_HMAC_LEGACY,
            &hmac2_real,
            "tool_success",
            1,
            "read_file",
            3000,
        );

        let events = vec![
            BondEvent {
                id: 1,
                event_type: "tool_success".to_string(),
                score_delta: 1,
                reason: "test".to_string(),
                hmac: hmac1.clone(),
                prev_hmac: "0".to_string(),
                created_at: 1000,
            },
            BondEvent {
                id: 2,
                event_type: "creation".to_string(),
                score_delta: 99, // Tampered (was 1)
                reason: "write_memory".to_string(),
                hmac: hmac2_real.clone(),
                prev_hmac: hmac1,
                created_at: 2000,
            },
            BondEvent {
                id: 3,
                event_type: "tool_success".to_string(),
                score_delta: 1,
                reason: "read_file".to_string(),
                hmac: hmac3,
                prev_hmac: hmac2_real,
                created_at: 3000,
            },
        ];

        let state = replay_events(&events);
        assert!(!state.chain_valid);
        // Event 1 applied (+1), event 2 skipped (tampered), event 3 skipped (broken chain)
        assert_eq!(state.score, 26);
    }

    #[test]
    fn test_verify_chain_empty() {
        let state = replay_events(&[]);
        assert!(state.chain_valid);
        assert_eq!(state.score, BASELINE_SCORE as u8);
        assert_eq!(state.level, BondLevel::Emerging);
    }

    // ── Rate Limiting ──

    #[test]
    fn test_rate_limiting_caps_events() {
        // Generate 20 tool_success events in the same hour bucket
        let mut events = Vec::new();
        let mut prev_hmac = "0".to_string();
        let base_time = 3600 * 1000; // some hour bucket

        for i in 0..20 {
            let created_at = base_time + i;
            let hmac = compute_event_hmac(
                BOND_HMAC_LEGACY,
                &prev_hmac,
                "tool_success",
                1,
                "test",
                created_at,
            );
            events.push(BondEvent {
                id: i as i64 + 1,
                event_type: "tool_success".to_string(),
                score_delta: 1,
                reason: "test".to_string(),
                hmac: hmac.clone(),
                prev_hmac: prev_hmac.clone(),
                created_at,
            });
            prev_hmac = hmac;
        }

        let state = replay_events(&events);
        // Rate limit for tool_success is 8 per hour
        // So only 8 events should be applied: 25 + 8 = 33
        assert_eq!(state.score, 33);
        assert!(state.chain_valid);
    }

    #[test]
    fn test_rate_limiting_different_hours_not_capped() {
        // Events across different hour buckets should not be capped
        let mut events = Vec::new();
        let mut prev_hmac = "0".to_string();

        for i in 0..20 {
            let created_at = 3600 * (i as i64 + 1); // each in a different hour
            let hmac = compute_event_hmac(
                BOND_HMAC_LEGACY,
                &prev_hmac,
                "tool_success",
                1,
                "test",
                created_at,
            );
            events.push(BondEvent {
                id: i as i64 + 1,
                event_type: "tool_success".to_string(),
                score_delta: 1,
                reason: "test".to_string(),
                hmac: hmac.clone(),
                prev_hmac: prev_hmac.clone(),
                created_at,
            });
            prev_hmac = hmac;
        }

        let state = replay_events(&events);
        // No rate limiting since each event is in a different hour
        // 25 + 20 = 45
        assert_eq!(state.score, 45);
    }

    // ── Replay Clamping ──

    #[test]
    fn test_replay_clamp_upper() {
        let mut events = Vec::new();
        let mut prev_hmac = "0".to_string();

        for i in 0..80 {
            let created_at = 3600 * (i as i64 + 1);
            let hmac = compute_event_hmac(
                BOND_HMAC_LEGACY,
                &prev_hmac,
                "creation",
                1,
                "test",
                created_at,
            );
            events.push(BondEvent {
                id: i as i64 + 1,
                event_type: "creation".to_string(),
                score_delta: 1,
                reason: "test".to_string(),
                hmac: hmac.clone(),
                prev_hmac: prev_hmac.clone(),
                created_at,
            });
            prev_hmac = hmac;
        }

        let state = replay_events(&events);
        assert_eq!(state.score, 100);
    }

    #[test]
    fn test_replay_clamp_lower() {
        let mut events = Vec::new();
        let mut prev_hmac = "0".to_string();

        for i in 0..50 {
            let created_at = 3600 * (i as i64 + 1);
            let hmac = compute_event_hmac(
                BOND_HMAC_LEGACY,
                &prev_hmac,
                "correction",
                -2,
                "test",
                created_at,
            );
            events.push(BondEvent {
                id: i as i64 + 1,
                event_type: "correction".to_string(),
                score_delta: -2,
                reason: "test".to_string(),
                hmac: hmac.clone(),
                prev_hmac: prev_hmac.clone(),
                created_at,
            });
            prev_hmac = hmac;
        }

        let state = replay_events(&events);
        assert_eq!(state.score, 0);
    }

    // ── Heuristics ──

    #[test]
    fn test_acceptance_positive() {
        assert!(looks_like_acceptance("yes, do it"));
        assert!(looks_like_acceptance("Go ahead"));
        assert!(looks_like_acceptance("sounds good to me"));
        assert!(looks_like_acceptance("approved"));
        assert!(looks_like_acceptance("let's do it"));
        assert!(looks_like_acceptance("yes please"));
        assert!(looks_like_acceptance("absolutely"));
    }

    #[test]
    fn test_acceptance_negative() {
        // Common conversational phrases should NOT trigger acceptance
        assert!(!looks_like_acceptance("can you help me?"));
        assert!(!looks_like_acceptance("what does this do?"));
        assert!(!looks_like_acceptance("show me the code"));
        assert!(!looks_like_acceptance("that's wrong"));
        assert!(!looks_like_acceptance("yes, what is the status?"));
        assert!(!looks_like_acceptance("great work on that"));
        assert!(!looks_like_acceptance("ok so what happened?"));
        assert!(!looks_like_acceptance("sure, can you explain?"));
    }

    #[test]
    fn test_rejection_positive() {
        assert!(looks_like_rejection("no don't do that"));
        assert!(looks_like_rejection("don't do it"));
        assert!(looks_like_rejection("cancel that"));
        assert!(looks_like_rejection("never mind"));
        assert!(looks_like_rejection("skip that"));
        assert!(looks_like_rejection("stop suggesting things"));
    }

    #[test]
    fn test_rejection_negative() {
        assert!(!looks_like_rejection("yes please"));
        assert!(!looks_like_rejection("sounds good"));
        assert!(!looks_like_rejection("help me with this"));
        assert!(!looks_like_rejection("I don't understand"));
    }

    // ── Formatting ──

    #[test]
    fn test_format_context_compact() {
        let state = BondState {
            score: 68,
            level: BondLevel::Trusted,
            autonomy_tier: AutonomyTier::GuidedAction,
            total_events: 100,
            chain_valid: true,
        };
        let ctx = format_context(&state, 0.09, 0.83);
        assert!(ctx.contains("Trusted"));
        assert!(ctx.contains("68"));
        assert!(ctx.contains("GuidedAction"));
        assert!(ctx.contains("bond_context"));
        assert!(ctx.len() < 200);
    }

    #[test]
    fn test_format_status_structure() {
        let state = BondState {
            score: 40,
            level: BondLevel::Emerging,
            autonomy_tier: AutonomyTier::Recommend,
            total_events: 0,
            chain_valid: true,
        };
        let output = format_status(&state, 0.05, 0.80, 3, &[]);
        assert!(output.contains("Bond Status"));
        assert!(output.contains("Emerging"));
        assert!(output.contains("Recommend"));
        assert!(output.contains("Chain valid"));
        assert!(output.contains("80%"));
        assert!(output.contains("5%"));
        assert!(output.contains("Tip:"));
    }

    #[test]
    fn test_format_status_broken_chain() {
        let state = BondState {
            score: 40,
            level: BondLevel::Emerging,
            autonomy_tier: AutonomyTier::Recommend,
            total_events: 5,
            chain_valid: false,
        };
        let output = format_status(&state, 0.0, 0.0, 0, &[]);
        assert!(output.contains("Chain broken"));
    }

    #[test]
    fn test_format_history_empty() {
        let output = format_history(&[]);
        assert!(output.contains("No bond events"));
    }

    #[test]
    fn test_format_history_with_events() {
        let now = Utc::now().timestamp();
        let events = vec![BondEvent {
            id: 1,
            event_type: "tool_success".to_string(),
            score_delta: 1,
            reason: "run_shell".to_string(),
            hmac: "abc".to_string(),
            prev_hmac: "0".to_string(),
            created_at: now - 120, // 2 min ago
        }];
        let output = format_history(&events);
        assert!(output.contains("tool_success"));
        assert!(output.contains("+1"));
        assert!(output.contains("run_shell"));
        assert!(output.contains("2m ago"));
    }

    #[test]
    fn test_format_ago_negative() {
        assert_eq!(format_ago(-100), "in the future");
    }

    #[test]
    fn test_format_ago_ranges() {
        assert_eq!(format_ago(0), "just now");
        assert_eq!(format_ago(30), "just now");
        assert_eq!(format_ago(120), "2m ago");
        assert_eq!(format_ago(7200), "2h ago");
        assert_eq!(format_ago(172800), "2d ago");
    }

    // ── Display ──

    #[test]
    fn test_display_bond_level() {
        assert_eq!(format!("{}", BondLevel::Fragile), "Fragile");
        assert_eq!(format!("{}", BondLevel::Emerging), "Emerging");
        assert_eq!(format!("{}", BondLevel::Stable), "Stable");
        assert_eq!(format!("{}", BondLevel::Trusted), "Trusted");
        assert_eq!(format!("{}", BondLevel::Synced), "Synced");
    }

    #[test]
    fn test_display_autonomy_tier() {
        assert_eq!(format!("{}", AutonomyTier::ObserveOnly), "ObserveOnly");
        assert_eq!(format!("{}", AutonomyTier::Recommend), "Recommend");
        assert_eq!(format!("{}", AutonomyTier::DraftAssist), "DraftAssist");
        assert_eq!(format!("{}", AutonomyTier::GuidedAction), "GuidedAction");
        assert_eq!(format!("{}", AutonomyTier::HighTrust), "HighTrust");
    }

    // ── Bar Rendering ──

    #[test]
    fn test_bar_full() {
        assert_eq!(
            bar(100, 10),
            "\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}"
        );
    }

    #[test]
    fn test_bar_empty() {
        assert_eq!(
            bar(0, 10),
            "\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}"
        );
    }

    #[test]
    fn test_bar_half() {
        let b = bar(50, 10);
        assert_eq!(b.chars().count(), 10);
    }

    // ── Anti-tamper hardening tests ──

    #[test]
    fn test_replay_with_derived_key() {
        let derived_key = b"derived-test-key-for-testing";
        let hmac1 = compute_event_hmac(derived_key, "0", "tool_success", 1, "test", 1000);
        let events = vec![BondEvent {
            id: 1,
            event_type: "tool_success".to_string(),
            score_delta: 1,
            reason: "test".to_string(),
            hmac: hmac1.clone(),
            prev_hmac: "0".to_string(),
            created_at: 1000,
        }];
        let state = replay_events_with_key(derived_key, &events);
        assert!(state.chain_valid);
        assert_eq!(state.score, BASELINE_SCORE as u8 + 1);
        // Legacy key should NOT validate derived-key events
        let state_legacy = replay_events(&events);
        assert!(!state_legacy.chain_valid);
        assert_eq!(state_legacy.score, BASELINE_SCORE as u8);
    }

    #[test]
    fn test_total_hourly_rate_limit() {
        let mut events = Vec::new();
        let mut prev_hmac = "0".to_string();
        let hour_base = 3600 * 100;
        for i in 0..35i64 {
            let (event_type, delta) = match i % 3 {
                0 => ("tool_success", 1),
                1 => ("creation", 2),
                _ => ("suggestion_accepted", 1),
            };
            let created_at = hour_base + i;
            let hmac = compute_event_hmac(
                BOND_HMAC_LEGACY,
                &prev_hmac,
                event_type,
                delta,
                "test",
                created_at,
            );
            events.push(BondEvent {
                id: i + 1,
                event_type: event_type.to_string(),
                score_delta: delta,
                reason: "test".to_string(),
                hmac: hmac.clone(),
                prev_hmac: prev_hmac.clone(),
                created_at,
            });
            prev_hmac = hmac;
        }
        let state = replay_events(&events);
        assert!(state.chain_valid);
        let all_sum: i32 = events.iter().map(|e| e.score_delta).sum();
        assert!((state.score as i32) < BASELINE_SCORE + all_sum);
    }

    #[test]
    fn test_positive_delta_hourly_rate_limit() {
        let mut events = Vec::new();
        let mut prev_hmac = "0".to_string();
        let hour_base = 3600 * 200;
        for i in 0..20i64 {
            let created_at = hour_base + i;
            let hmac = compute_event_hmac(
                BOND_HMAC_LEGACY,
                &prev_hmac,
                "tool_success",
                1,
                "test",
                created_at,
            );
            events.push(BondEvent {
                id: i + 1,
                event_type: "tool_success".to_string(),
                score_delta: 1,
                reason: "test".to_string(),
                hmac: hmac.clone(),
                prev_hmac: prev_hmac.clone(),
                created_at,
            });
            prev_hmac = hmac;
        }
        let state = replay_events(&events);
        assert!(state.chain_valid);
        assert_eq!(state.score, BASELINE_SCORE as u8 + 8);
    }

    #[test]
    fn test_invalid_event_type_skipped_in_replay() {
        let hmac1 = compute_event_hmac(BOND_HMAC_LEGACY, "0", "custom_exploit", 99, "test", 1000);
        let events = vec![BondEvent {
            id: 1,
            event_type: "custom_exploit".to_string(),
            score_delta: 99,
            reason: "test".to_string(),
            hmac: hmac1,
            prev_hmac: "0".to_string(),
            created_at: 1000,
        }];
        let state = replay_events(&events);
        assert_eq!(state.score, BASELINE_SCORE as u8);
    }

    #[test]
    fn test_negative_events_bypass_positive_cap() {
        let mut events = Vec::new();
        let mut prev_hmac = "0".to_string();
        let hour_base = 3600 * 300;
        for i in 0..15i64 {
            let created_at = hour_base + i;
            let hmac = compute_event_hmac(
                BOND_HMAC_LEGACY,
                &prev_hmac,
                "tool_success",
                1,
                "test",
                created_at,
            );
            events.push(BondEvent {
                id: i + 1,
                event_type: "tool_success".to_string(),
                score_delta: 1,
                reason: "test".to_string(),
                hmac: hmac.clone(),
                prev_hmac: prev_hmac.clone(),
                created_at,
            });
            prev_hmac = hmac;
        }
        for i in 0..10i64 {
            let created_at = hour_base + 15 + i;
            let hmac = compute_event_hmac(
                BOND_HMAC_LEGACY,
                &prev_hmac,
                "tool_failure",
                -1,
                "test",
                created_at,
            );
            events.push(BondEvent {
                id: 15 + i + 1,
                event_type: "tool_failure".to_string(),
                score_delta: -1,
                reason: "test".to_string(),
                hmac: hmac.clone(),
                prev_hmac: prev_hmac.clone(),
                created_at,
            });
            prev_hmac = hmac;
        }
        let state = replay_events(&events);
        assert!(state.chain_valid);
        assert_eq!(state.score, 28); // 25 + 8 - 5
    }

    #[test]
    fn test_inject_context_format() {
        let state = BondState {
            score: 68,
            level: BondLevel::Trusted,
            autonomy_tier: AutonomyTier::GuidedAction,
            total_events: 100,
            chain_valid: true,
        };
        let ctx = format_context(&state, 0.09, 0.83);
        assert!(ctx.contains("<bond_context>"));
        assert!(ctx.contains("</bond_context>"));
        assert!(ctx.contains("Trusted"));
        assert!(ctx.contains("68/100"));
        assert!(ctx.contains("GuidedAction"));
        assert!(ctx.contains("9%"));
        assert!(ctx.contains("83%"));
    }

    #[test]
    fn test_mid_chain_corruption_invalidates_tail() {
        // Build 3 events: valid → corrupted → valid-but-orphaned
        let hmac1 = compute_event_hmac(BOND_HMAC_LEGACY, "0", "tool_success", 1, "test", 1000);
        let event1 = BondEvent {
            id: 1,
            event_type: "tool_success".to_string(),
            score_delta: 1,
            reason: "test".to_string(),
            hmac: hmac1.clone(),
            prev_hmac: "0".to_string(),
            created_at: 1000,
        };
        // Event 2: corrupted
        let event2 = BondEvent {
            id: 2,
            event_type: "creation".to_string(),
            score_delta: 2,
            reason: "test".to_string(),
            hmac: "corrupted".to_string(),
            prev_hmac: hmac1,
            created_at: 2000,
        };
        // Event 3: chains from corrupted — should be skipped too
        let hmac3 = compute_event_hmac(
            BOND_HMAC_LEGACY,
            "corrupted",
            "tool_success",
            1,
            "test",
            3000,
        );
        let event3 = BondEvent {
            id: 3,
            event_type: "tool_success".to_string(),
            score_delta: 1,
            reason: "test".to_string(),
            hmac: hmac3,
            prev_hmac: "corrupted".to_string(),
            created_at: 3000,
        };

        let state = replay_events(&[event1, event2, event3]);
        // Only event1 applies: baseline 25 + 1 = 26
        assert_eq!(state.score, 26);
        assert!(!state.chain_valid);
    }
}
