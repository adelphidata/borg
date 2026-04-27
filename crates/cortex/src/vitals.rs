//! Vitals system — passive agent health tracking via lifecycle hooks.
//!
//! Five stats (stability, focus, sync, growth, happiness) update automatically
//! from usage events. State is event-sourced: derived by replaying verified
//! events from baseline. HMAC chain prevents tampering; rate limiting caps
//! impact per category per hour to prevent gaming.

use chrono::{DateTime, Utc};
use regex::Regex;
use std::fmt;
use std::sync::LazyLock;

use crate::db::Database;
use crate::hmac_chain;
use crate::hooks::{Hook, HookAction, HookContext, HookData, HookPoint};

// ── HMAC ──

/// Domain string for HMAC key derivation. Combined with per-installation salt.
pub(crate) const VITALS_HMAC_DOMAIN: &[u8] = b"borg-vitals-chain-v1";

/// Legacy compiled-in secret for installations without per-install salt.
#[cfg(test)]
const VITALS_HMAC_LEGACY: &[u8] = b"borg-vitals-chain-v1";

/// Compute HMAC for a vitals event using the shared HMAC chain module.
pub(crate) fn compute_event_hmac(
    key: &[u8],
    prev_hmac: &str,
    category: &str,
    source: &str,
    deltas: StatDeltas,
    created_at: i64,
) -> String {
    hmac_chain::compute_hmac(
        key,
        &[
            prev_hmac.as_bytes(),
            category.as_bytes(),
            source.as_bytes(),
            &deltas.stability.to_le_bytes(),
            &deltas.focus.to_le_bytes(),
            &deltas.sync.to_le_bytes(),
            &deltas.growth.to_le_bytes(),
            &deltas.happiness.to_le_bytes(),
            &created_at.to_le_bytes(),
        ],
    )
}

/// Verify an event's HMAC against the expected chain.
fn verify_event_hmac(key: &[u8], event: &VitalsEvent, expected_prev_hmac: &str) -> bool {
    let deltas = StatDeltas {
        stability: event.stability_delta as i8,
        focus: event.focus_delta as i8,
        sync: event.sync_delta as i8,
        growth: event.growth_delta as i8,
        happiness: event.happiness_delta as i8,
    };
    let recomputed = compute_event_hmac(
        key,
        &event.prev_hmac,
        &event.category,
        &event.source,
        deltas,
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

/// Maximum events per category per hour during replay.
pub(crate) fn rate_limit_for(category: &str) -> u32 {
    match category {
        "interaction" => 5,
        "success" => 8,
        "failure" => 5,
        "correction" => 3,
        "creation" => 3,
        "negative_sentiment" => 3,
        _ => 5,
    }
}

/// Create a rate limiter configured for vitals events.
fn new_rate_limiter() -> hmac_chain::HourlyRateLimiter {
    hmac_chain::HourlyRateLimiter::new(None, None)
}

// ── Types ──

/// The 5 vitals stats, all 0..=100.
#[derive(Debug, Clone)]
pub struct VitalsState {
    /// Reliability of tool operations (0..=100).
    pub stability: u8,
    /// Clarity and directedness of agent activity (0..=100).
    pub focus: u8,
    /// Frequency of user–agent interaction (0..=100).
    pub sync: u8,
    /// Rate of capability expansion (0..=100).
    pub growth: u8,
    /// Overall agent wellbeing indicator (0..=100).
    pub happiness: u8,
    /// Timestamp of the most recent non-negative interaction.
    pub last_interaction_at: DateTime<Utc>,
    /// Timestamp of the most recent event of any kind.
    pub updated_at: DateTime<Utc>,
    /// Whether the HMAC chain is intact across all replayed events.
    pub chain_valid: bool,
}

/// Broad impact categories for events.
/// New tools automatically get classified by the hook based on outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventCategory {
    /// User or session interaction (e.g. message, session start).
    Interaction,
    /// Successful tool execution.
    Success,
    /// Failed tool execution.
    Failure,
    /// User expressed a correction or frustration.
    Correction,
    /// Agent created something durable (memory, skill, channel, file).
    Creation,
    /// User expressed strong negative sentiment (profanity, hostility).
    NegativeSentiment,
}

impl fmt::Display for EventCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Interaction => write!(f, "interaction"),
            Self::Success => write!(f, "success"),
            Self::Failure => write!(f, "failure"),
            Self::Correction => write!(f, "correction"),
            Self::Creation => write!(f, "creation"),
            Self::NegativeSentiment => write!(f, "negative_sentiment"),
        }
    }
}

impl EventCategory {
    /// Parse a category name string into the enum variant.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "interaction" => Some(Self::Interaction),
            "success" => Some(Self::Success),
            "failure" => Some(Self::Failure),
            "correction" => Some(Self::Correction),
            "creation" => Some(Self::Creation),
            "negative_sentiment" => Some(Self::NegativeSentiment),
            _ => None,
        }
    }
}

/// Stat deltas to apply from an event.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StatDeltas {
    /// Change to stability.
    pub stability: i8,
    /// Change to focus.
    pub focus: i8,
    /// Change to sync.
    pub sync: i8,
    /// Change to growth.
    pub growth: i8,
    /// Change to happiness.
    pub happiness: i8,
}

/// A recorded event from the vitals ledger.
#[derive(Debug, Clone)]
pub struct VitalsEvent {
    /// Auto-incremented row ID.
    pub id: i64,
    /// Event category (interaction, success, failure, correction, creation).
    pub category: String,
    /// What triggered this event (tool name, "session_start", etc.).
    pub source: String,
    /// Delta applied to stability.
    pub stability_delta: i32,
    /// Delta applied to focus.
    pub focus_delta: i32,
    /// Delta applied to sync.
    pub sync_delta: i32,
    /// Delta applied to growth.
    pub growth_delta: i32,
    /// Delta applied to happiness.
    pub happiness_delta: i32,
    /// Optional JSON metadata blob.
    pub metadata_json: Option<String>,
    /// Unix timestamp of event creation.
    pub created_at: i64,
    /// HMAC for this event in the chain.
    pub hmac: String,
    /// HMAC of the previous event in the chain.
    pub prev_hmac: String,
}

/// Drift issues to surface to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftFlag {
    /// No interaction for over 48 hours.
    InactiveTooLong,
    /// Stability stat critically low.
    LowStability,
    /// Sync stat critically low.
    LowSync,
    /// Happiness stat critically low.
    LowHappiness,
    /// Multiple recent tool failures.
    RepeatedFailures,
}

impl DriftFlag {
    /// Human-readable description of this drift flag.
    pub fn description(&self) -> &'static str {
        match self {
            Self::InactiveTooLong => "Agent inactive for over 48 hours",
            Self::LowStability => "Stability is critically low",
            Self::LowSync => "Sync is low — regular interaction helps",
            Self::LowHappiness => "Happiness is critically low",
            Self::RepeatedFailures => "Multiple recent tool failures detected",
        }
    }
}

/// Recommended action with human-readable text.
#[derive(Debug)]
pub struct Recommendation {
    /// Why this recommendation was chosen.
    pub reason: &'static str,
    /// Actionable advice for the user.
    pub tip: &'static str,
}

// ── Scoring ──

/// Baseline vitals for a fresh agent.
///
/// Timestamps are initialized to the Unix epoch so replay is deterministic:
/// two replays of the same event slice produce identical state regardless of
/// wall-clock time. Consumers that need "now" (e.g. `apply_decay`,
/// `detect_drift`) take it as an explicit parameter.
pub fn baseline() -> VitalsState {
    let epoch = DateTime::<Utc>::from_timestamp(0, 0).unwrap_or_else(Utc::now);
    VitalsState {
        stability: 40,
        focus: 40,
        sync: 40,
        growth: 40,
        happiness: 40,
        last_interaction_at: epoch,
        updated_at: epoch,
        chain_valid: true,
    }
}

/// Returns true if the state represents a fresh install with no real interactions.
/// Baseline initializes timestamps to epoch; a non-epoch `last_interaction_at`
/// means at least one real interaction event has been replayed.
pub fn is_fresh_install(state: &VitalsState) -> bool {
    state.last_interaction_at.timestamp() == 0
}

/// Deterministic stat deltas for each event category.
pub fn deltas_for(category: EventCategory) -> StatDeltas {
    match category {
        EventCategory::Interaction => StatDeltas {
            stability: 0,
            focus: 0,
            sync: 1,
            growth: 0,
            happiness: 0,
        },
        EventCategory::Success => StatDeltas {
            stability: 1,
            focus: 0,
            sync: 0,
            growth: 0,
            happiness: 0,
        },
        EventCategory::Failure => StatDeltas {
            stability: -1,
            focus: 0,
            sync: 0,
            growth: 0,
            happiness: 0,
        },
        EventCategory::Correction => StatDeltas {
            stability: -1,
            focus: -1,
            sync: 0,
            growth: 0,
            happiness: -1,
        },
        EventCategory::Creation => StatDeltas {
            stability: 1,
            focus: 0,
            sync: 0,
            growth: 1,
            happiness: 1,
        },
        EventCategory::NegativeSentiment => StatDeltas {
            stability: -2,
            focus: -1,
            sync: 0,
            growth: 0,
            happiness: -10,
        },
    }
}

/// Validate that deltas match the expected values for this category.
/// Returns Err if category is unknown or deltas don't match.
pub(crate) fn validate_deltas(category: &str, deltas: StatDeltas) -> Result<(), &'static str> {
    let expected = match category {
        "interaction" => deltas_for(EventCategory::Interaction),
        "success" => deltas_for(EventCategory::Success),
        "failure" => deltas_for(EventCategory::Failure),
        "correction" => deltas_for(EventCategory::Correction),
        "creation" => deltas_for(EventCategory::Creation),
        "negative_sentiment" => deltas_for(EventCategory::NegativeSentiment),
        _ => return Err("unknown vitals category"),
    };
    if deltas != expected {
        return Err("deltas do not match expected values for category");
    }
    Ok(())
}

/// Apply deltas to a mutable state, clamping each stat to 0..=100.
pub fn apply_deltas(state: &mut VitalsState, deltas: &StatDeltas) {
    state.stability = clamp_add(state.stability, deltas.stability);
    state.focus = clamp_add(state.focus, deltas.focus);
    state.sync = clamp_add(state.sync, deltas.sync);
    state.growth = clamp_add(state.growth, deltas.growth);
    state.happiness = clamp_add(state.happiness, deltas.happiness);
}

fn clamp_add(val: u8, delta: i8) -> u8 {
    let result = val as i16 + delta as i16;
    result.clamp(0, 100) as u8
}

/// Apply a decay multiplier to a stat delta. Positive deltas use floor (less reward),
/// negative deltas use ceil (less penalty) so repeated failures also attenuate.
fn apply_decay_to_delta(delta: i8, multiplier: f64) -> i8 {
    if multiplier >= 1.0 {
        return delta;
    }
    let scaled = delta as f64 * multiplier;
    if delta >= 0 {
        scaled.floor() as i8
    } else {
        scaled.ceil() as i8
    }
}

// ── Event Replay (Event Sourcing) ──

/// Replay verified events from baseline to compute current state.
/// Verifies HMAC chain and applies per-category-per-hour rate limits.
#[cfg(test)]
pub fn replay_events(events: &[VitalsEvent]) -> VitalsState {
    replay_events_with_key(VITALS_HMAC_LEGACY, events)
}

/// Replay events with a specific HMAC key (for per-installation derived keys).
pub fn replay_events_with_key(key: &[u8], events: &[VitalsEvent]) -> VitalsState {
    let mut state = baseline();
    let mut expected_prev_hmac = "0".to_string();
    let mut chain_valid = true;
    let mut rate_limiter = new_rate_limiter();
    let mut last_interaction_at: Option<i64> = None;

    for event in events {
        // Verify HMAC chain — skip tampered/injected events
        if !verify_event_hmac(key, event, &expected_prev_hmac) {
            tracing::warn!("vitals: skipping event {} with broken HMAC chain", event.id);
            chain_valid = false;
            continue;
        }
        expected_prev_hmac = event.hmac.clone();

        // Rate limit per category per hour
        let cap = rate_limit_for(&event.category);
        if !rate_limiter.check_and_consume(event.created_at, &event.category, cap, false) {
            continue;
        }

        // Diminishing returns: repeated same-source calls yield reduced deltas.
        // Negative sentiment bypasses decay — sustained cussing is a genuine signal.
        let is_negative_sentiment = event.category == "negative_sentiment";
        let source_multiplier =
            rate_limiter.source_decay_multiplier(event.created_at, &event.source);
        if source_multiplier == 0.0 && !is_negative_sentiment {
            continue;
        }

        // Apply deltas with source decay (negative sentiment always at full strength)
        let raw_deltas = StatDeltas {
            stability: event.stability_delta as i8,
            focus: event.focus_delta as i8,
            sync: event.sync_delta as i8,
            growth: event.growth_delta as i8,
            happiness: event.happiness_delta as i8,
        };
        let deltas = if source_multiplier < 1.0 && !is_negative_sentiment {
            StatDeltas {
                stability: apply_decay_to_delta(raw_deltas.stability, source_multiplier),
                focus: apply_decay_to_delta(raw_deltas.focus, source_multiplier),
                sync: apply_decay_to_delta(raw_deltas.sync, source_multiplier),
                growth: apply_decay_to_delta(raw_deltas.growth, source_multiplier),
                happiness: apply_decay_to_delta(raw_deltas.happiness, source_multiplier),
            }
        } else {
            raw_deltas
        };
        apply_deltas(&mut state, &deltas);

        // Track last interaction time (only for non-negative events)
        if event.category != "failure"
            && event.category != "correction"
            && event.category != "negative_sentiment"
        {
            last_interaction_at = Some(event.created_at);
        }
    }

    // Set timestamps from replay
    if let Some(ts) = last_interaction_at {
        if let Some(dt) = DateTime::<Utc>::from_timestamp(ts, 0) {
            state.last_interaction_at = dt;
        }
    }
    if let Some(last_event) = events.last() {
        if let Some(dt) = DateTime::<Utc>::from_timestamp(last_event.created_at, 0) {
            state.updated_at = dt;
        }
    }

    state.chain_valid = chain_valid;
    state
}

/// Apply time-based decay based on inactivity duration.
pub fn apply_decay(state: &VitalsState, now: DateTime<Utc>) -> VitalsState {
    let mut result = state.clone();

    // No decay on fresh install (epoch sentinel = no interactions yet)
    if is_fresh_install(state) {
        return result;
    }

    let hours = (now - state.last_interaction_at).num_hours();

    if hours >= 24 {
        result.sync = clamp_add(result.sync, -6);
        result.happiness = clamp_add(result.happiness, -4);
    }
    if hours >= 72 {
        result.stability = clamp_add(result.stability, -8);
        result.focus = clamp_add(result.focus, -8);
    }
    if hours >= 168 {
        result.growth = clamp_add(result.growth, -5);
        result.stability = clamp_add(result.stability, -5);
    }

    result
}

// ── Drift Detection ──

/// Detect drift flags from current state.
pub fn detect_drift(state: &VitalsState, now: DateTime<Utc>) -> Vec<DriftFlag> {
    let mut flags = Vec::new();

    // Skip inactivity check on fresh install (epoch sentinel = no interactions yet)
    if !is_fresh_install(state) {
        let hours = (now - state.last_interaction_at).num_hours();
        if hours > 48 {
            flags.push(DriftFlag::InactiveTooLong);
        }
    }
    if state.stability < 30 {
        flags.push(DriftFlag::LowStability);
    }
    if state.sync < 35 {
        flags.push(DriftFlag::LowSync);
    }
    if state.happiness < 30 {
        flags.push(DriftFlag::LowHappiness);
    }

    flags
}

/// Detect repeated failures from recent events.
pub fn detect_failure_drift(events: &[VitalsEvent]) -> bool {
    let failure_count = events.iter().filter(|e| e.category == "failure").count();
    failure_count >= 3
}

// ── Recommendations ──

/// Deterministic recommendation based on current vitals.
pub fn recommend(state: &VitalsState) -> Recommendation {
    if state.sync < 35 {
        return Recommendation {
            reason: "Sync is low",
            tip: "Have a conversation or start a task to restore sync",
        };
    }
    if state.happiness < 30 {
        return Recommendation {
            reason: "Happiness is low",
            tip: "Use borg for a meaningful task to restore happiness",
        };
    }
    if state.stability < 30 {
        return Recommendation {
            reason: "Stability is low",
            tip: "Complete some successful tool operations to rebuild stability",
        };
    }
    if state.focus < 40 {
        return Recommendation {
            reason: "Focus is drifting",
            tip: "Clear, directed tasks help the agent stay focused",
        };
    }
    if state.growth < 40 {
        return Recommendation {
            reason: "Growth has stalled",
            tip: "Create tools, write memories, or install skills to grow capabilities",
        };
    }
    Recommendation {
        reason: "Vitals are healthy",
        tip: "Keep using borg regularly to maintain vitals",
    }
}

// ── Classification ──

/// Classify a tool call into an event category based on tool name and outcome.
pub fn classify_tool(tool_name: &str, is_error: bool) -> EventCategory {
    if is_error {
        return EventCategory::Failure;
    }
    match tool_name {
        "write_memory" | "apply_skill_patch" | "create_channel" | "apply_patch" => {
            EventCategory::Creation
        }
        _ => EventCategory::Success,
    }
}

#[allow(clippy::expect_used)]
static CORRECTION_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(that'?s wrong|that'?s not right|not what i (meant|asked)|you misunderstood|incorrect|try again|don'?t do that|redo (this|that|it)|wtf|wth|what the (fuck|hell)|fuck(ing)? (broken|useless|terrible|awful|horrible)|this (sucks|is broken|is wrong)|so frustrating|damn it|are you (stupid|dumb|deaf))\b"
    ).expect("compile-time literal regex")
});

/// Regex-based heuristic for detecting user corrections and frustration.
/// Deliberately conservative — prefers false negatives over false positives.
pub fn looks_like_correction(msg: &str) -> bool {
    CORRECTION_PATTERN.is_match(msg)
}

#[allow(clippy::expect_used)]
static NEGATIVE_SENTIMENT_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(wtf|wth|ffs|omfg|shit(ty|tiest)?|dumbass|horrible|awful|piss(ed|ing)? off|piece of (shit|crap|junk)|what the (fuck|hell)|fucking? (broken|useless|terrible|awful|horrible)|fuck you|screw (this|you)|so frustrating|this sucks|damn it)\b"
    ).expect("compile-time literal regex")
});

/// Regex-based heuristic for detecting strong negative sentiment (profanity, hostility).
/// Separate from corrections — captures pure frustration rather than agent error feedback.
pub fn looks_like_negative_sentiment(msg: &str) -> bool {
    NEGATIVE_SENTIMENT_PATTERN.is_match(msg)
}

// ── Formatting ──

fn bar(val: u8, width: usize) -> String {
    let filled = (val as usize * width + 50) / 100;
    let empty = width - filled;
    format!("{}{}", "\u{2588}".repeat(filled), "\u{2591}".repeat(empty))
}

/// Compact one-liner for TUI session header.
pub fn format_compact(state: &VitalsState) -> String {
    format!(
        "[stability:{} focus:{} sync:{} growth:{} happiness:{}]",
        state.stability, state.focus, state.sync, state.growth, state.happiness
    )
}

/// Full status output for `borg status` / `/status`.
pub fn format_status(state: &VitalsState, events: &[VitalsEvent], drift: &[DriftFlag]) -> String {
    let mut out =
        String::from("Borg Vitals\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n");

    out.push_str(&format!(
        "  stability    {}  {}\n",
        bar(state.stability, 10),
        state.stability
    ));
    out.push_str(&format!(
        "  focus        {}  {}\n",
        bar(state.focus, 10),
        state.focus
    ));
    out.push_str(&format!(
        "  sync         {}  {}\n",
        bar(state.sync, 10),
        state.sync
    ));
    out.push_str(&format!(
        "  growth       {}  {}\n",
        bar(state.growth, 10),
        state.growth
    ));
    out.push_str(&format!(
        "  happiness    {}  {}\n",
        bar(state.happiness, 10),
        state.happiness
    ));
    // Plain-English legend so these stats are legible to non-technical users
    // without needing to read the code or docs.
    out.push_str("\n  stability = long-term steadiness   focus = attention this session\n");
    out.push_str("  sync = alignment with you          growth = learning rate\n");
    out.push_str("  happiness = your sentiment\n");

    if !events.is_empty() {
        let mut interactions = 0u32;
        let mut successes = 0u32;
        let mut failures = 0u32;
        let mut creations = 0u32;
        let mut corrections = 0u32;
        let mut negative_sentiments = 0u32;

        for e in events {
            match e.category.as_str() {
                "interaction" => interactions += 1,
                "success" => successes += 1,
                "failure" => failures += 1,
                "creation" => creations += 1,
                "correction" => corrections += 1,
                "negative_sentiment" => negative_sentiments += 1,
                _ => {}
            }
        }

        out.push_str("\nRecent Activity (7d):\n");
        let mut parts = Vec::new();
        if interactions > 0 {
            parts.push(format!("{interactions} interactions"));
        }
        if successes > 0 {
            parts.push(format!("{successes} successes"));
        }
        if failures > 0 {
            parts.push(format!("{failures} failures"));
        }
        if creations > 0 {
            parts.push(format!("{creations} creations"));
        }
        if corrections > 0 {
            parts.push(format!("{corrections} corrections"));
        }
        if negative_sentiments > 0 {
            parts.push(format!("{negative_sentiments} negative"));
        }
        out.push_str(&format!("  {}\n", parts.join(", ")));
    }

    if !drift.is_empty() {
        out.push_str("\nDrift:\n");
        for flag in drift {
            out.push_str(&format!("  \u{26a0} {}\n", flag.description()));
        }
    }

    let rec = recommend(state);
    out.push_str(&format!("\nTip: {}\n", rec.tip));

    out
}

/// One-line drift notice for TUI session start. Returns None if no drift.
pub fn format_drift_notice(drift: &[DriftFlag]) -> Option<String> {
    let first = drift.first()?;
    Some(format!("Drift: {}", first.description()))
}

// ── VitalsHook ──

/// Lifecycle hook that passively records vitals events.
/// Wraps Database in a Mutex because Hook requires Send + Sync
/// but rusqlite::Connection is !Sync.
/// Lifecycle hook that records vitals events to SQLite on agent interactions.
pub struct VitalsHook {
    db: std::sync::Mutex<Database>,
}

impl VitalsHook {
    /// Create a new vitals hook, opening a database connection.
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            db: std::sync::Mutex::new(Database::open()?),
        })
    }

    fn record(&self, category: EventCategory, source: &str) {
        let deltas = deltas_for(category);
        let Ok(db) = self.db.lock() else {
            tracing::warn!("vitals: mutex poisoned, skipping event");
            return;
        };
        if let Err(e) = db.record_vitals_event(&category.to_string(), source, &deltas, None) {
            tracing::warn!("vitals: failed to record event: {e}");
        }
    }
}

impl Hook for VitalsHook {
    fn name(&self) -> &str {
        "vitals"
    }

    fn points(&self) -> &[HookPoint] {
        &[
            HookPoint::SessionStart,
            HookPoint::BeforeAgentStart,
            HookPoint::AfterToolCall,
        ]
    }

    fn execute(&self, ctx: &HookContext) -> HookAction {
        match &ctx.data {
            HookData::SessionStart { .. } => {
                self.record(EventCategory::Interaction, "session_start");
            }
            HookData::AgentStart { user_message } => {
                if looks_like_negative_sentiment(user_message) {
                    self.record(EventCategory::NegativeSentiment, "user_message");
                } else if looks_like_correction(user_message) {
                    self.record(EventCategory::Correction, "user_message");
                } else {
                    self.record(EventCategory::Interaction, "user_message");
                }
            }
            HookData::ToolResult { name, is_error, .. } => {
                let category = classify_tool(name, *is_error);
                self.record(category, name);
            }
            _ => {}
        }
        HookAction::Continue
    }
}

impl std::fmt::Debug for VitalsHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VitalsHook").finish()
    }
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    // ── Scoring ──

    #[test]
    fn test_baseline_values() {
        let state = baseline();
        assert_eq!(state.stability, 40);
        assert_eq!(state.focus, 40);
        assert_eq!(state.sync, 40);
        assert_eq!(state.growth, 40);
        assert_eq!(state.happiness, 40);
    }

    #[test]
    fn test_deltas_for_all_categories() {
        let d = deltas_for(EventCategory::Interaction);
        assert_eq!(
            (d.stability, d.focus, d.sync, d.growth, d.happiness),
            (0, 0, 1, 0, 0)
        );

        let d = deltas_for(EventCategory::Success);
        assert_eq!(
            (d.stability, d.focus, d.sync, d.growth, d.happiness),
            (1, 0, 0, 0, 0)
        );

        let d = deltas_for(EventCategory::Failure);
        assert_eq!(
            (d.stability, d.focus, d.sync, d.growth, d.happiness),
            (-1, 0, 0, 0, 0)
        );

        let d = deltas_for(EventCategory::Correction);
        assert_eq!(
            (d.stability, d.focus, d.sync, d.growth, d.happiness),
            (-1, -1, 0, 0, -1)
        );

        let d = deltas_for(EventCategory::Creation);
        assert_eq!(
            (d.stability, d.focus, d.sync, d.growth, d.happiness),
            (1, 0, 0, 1, 1)
        );

        let d = deltas_for(EventCategory::NegativeSentiment);
        assert_eq!(
            (d.stability, d.focus, d.sync, d.growth, d.happiness),
            (-2, -1, 0, 0, -10)
        );
    }

    #[test]
    fn test_apply_deltas_normal() {
        let mut state = baseline();
        let deltas = deltas_for(EventCategory::Interaction);
        apply_deltas(&mut state, &deltas);
        assert_eq!(state.stability, 40);
        assert_eq!(state.focus, 40);
        assert_eq!(state.sync, 41);
        assert_eq!(state.growth, 40);
        assert_eq!(state.happiness, 40);
    }

    #[test]
    fn test_apply_deltas_clamp_upper() {
        let mut state = baseline();
        state.stability = 100;
        state.growth = 99;
        let deltas = deltas_for(EventCategory::Creation);
        apply_deltas(&mut state, &deltas);
        assert_eq!(state.stability, 100); // 100 + 1 clamped to 100
        assert_eq!(state.growth, 100); // 99 + 1 = 100
    }

    #[test]
    fn test_apply_deltas_clamp_lower() {
        let mut state = baseline();
        state.stability = 1;
        state.focus = 0;
        let deltas = deltas_for(EventCategory::Correction);
        apply_deltas(&mut state, &deltas);
        assert_eq!(state.stability, 0);
        assert_eq!(state.focus, 0);
    }

    #[test]
    fn test_classify_tool() {
        assert_eq!(
            classify_tool("write_memory", false),
            EventCategory::Creation
        );
        assert_eq!(
            classify_tool("apply_skill_patch", false),
            EventCategory::Creation
        );
        assert_eq!(
            classify_tool("create_channel", false),
            EventCategory::Creation
        );
        assert_eq!(classify_tool("apply_patch", false), EventCategory::Creation);
        assert_eq!(classify_tool("run_shell", false), EventCategory::Success);
        assert_eq!(classify_tool("read_file", false), EventCategory::Success);
        assert_eq!(classify_tool("write_memory", true), EventCategory::Failure);
        assert_eq!(classify_tool("run_shell", true), EventCategory::Failure);
    }

    // ── Decay ──

    #[test]
    fn test_no_decay_within_24h() {
        let mut state = baseline();
        state.last_interaction_at = Utc::now();
        let now = state.last_interaction_at + Duration::hours(23);
        let decayed = apply_decay(&state, now);
        assert_eq!(decayed.sync, state.sync);
        assert_eq!(decayed.happiness, state.happiness);
        assert_eq!(decayed.stability, state.stability);
        assert_eq!(decayed.focus, state.focus);
        assert_eq!(decayed.growth, state.growth);
    }

    #[test]
    fn test_decay_24h() {
        let mut state = baseline();
        state.last_interaction_at = Utc::now();
        let now = state.last_interaction_at + Duration::hours(25);
        let decayed = apply_decay(&state, now);
        assert_eq!(decayed.sync, 40 - 6);
        assert_eq!(decayed.happiness, 40 - 4);
        assert_eq!(decayed.stability, 40);
        assert_eq!(decayed.focus, 40);
    }

    #[test]
    fn test_decay_72h() {
        let mut state = baseline();
        state.last_interaction_at = Utc::now();
        let now = state.last_interaction_at + Duration::hours(73);
        let decayed = apply_decay(&state, now);
        assert_eq!(decayed.sync, 40 - 6);
        assert_eq!(decayed.happiness, 40 - 4);
        assert_eq!(decayed.stability, 40 - 8);
        assert_eq!(decayed.focus, 40 - 8);
    }

    #[test]
    fn test_decay_7_days() {
        let mut state = baseline();
        state.last_interaction_at = Utc::now();
        let now = state.last_interaction_at + Duration::hours(169);
        let decayed = apply_decay(&state, now);
        assert_eq!(decayed.sync, 40 - 6);
        assert_eq!(decayed.happiness, 40 - 4);
        assert_eq!(decayed.stability, 40 - 8 - 5);
        assert_eq!(decayed.focus, 40 - 8);
        assert_eq!(decayed.growth, 40 - 5);
    }

    // ── Drift Detection ──

    #[test]
    fn test_drift_inactive() {
        let mut state = baseline();
        state.last_interaction_at = Utc::now() - Duration::hours(49);
        let drift = detect_drift(&state, Utc::now());
        assert!(drift.contains(&DriftFlag::InactiveTooLong));
    }

    #[test]
    fn test_drift_low_stats() {
        let mut state = baseline();
        state.stability = 25;
        state.sync = 34;
        state.happiness = 20;
        let drift = detect_drift(&state, Utc::now());
        assert!(drift.contains(&DriftFlag::LowStability));
        assert!(drift.contains(&DriftFlag::LowSync));
        assert!(drift.contains(&DriftFlag::LowHappiness));
    }

    #[test]
    fn test_no_drift_healthy() {
        let mut state = baseline();
        // baseline() uses epoch for timestamps; for a "healthy" check we need
        // to simulate that an interaction just happened.
        state.last_interaction_at = Utc::now();
        let drift = detect_drift(&state, Utc::now());
        assert!(drift.is_empty());
    }

    // ── Recommendations ──

    #[test]
    fn test_recommend_low_sync() {
        let mut state = baseline();
        state.sync = 34;
        let rec = recommend(&state);
        assert!(rec.reason.contains("Sync"));
    }

    #[test]
    fn test_recommend_low_happiness() {
        let mut state = baseline();
        state.happiness = 25;
        let rec = recommend(&state);
        assert!(rec.reason.contains("Happiness"));
    }

    #[test]
    fn test_recommend_healthy() {
        let state = baseline();
        let rec = recommend(&state);
        assert!(rec.reason.contains("healthy"));
    }

    // ── Correction Heuristic ──

    #[test]
    fn test_correction_positive() {
        assert!(looks_like_correction("that's wrong"));
        assert!(looks_like_correction("That's not right at all"));
        assert!(looks_like_correction("Not what I meant, please fix"));
        assert!(looks_like_correction("incorrect, use the other file"));
        assert!(looks_like_correction("wtf is this output"));
        assert!(looks_like_correction("this sucks"));
        assert!(looks_like_correction("so frustrating"));
        assert!(looks_like_correction("try again please"));
    }

    #[test]
    fn test_correction_negative() {
        assert!(!looks_like_correction("sounds good, let's do that"));
        assert!(!looks_like_correction("yes, perfect"));
        assert!(!looks_like_correction("can you help me with this?"));
        assert!(!looks_like_correction("great work"));
        assert!(!looks_like_correction("I have no idea"));
        assert!(!looks_like_correction("there is no way to know"));
    }

    // ── Negative Sentiment ──

    #[test]
    fn test_negative_sentiment_positive() {
        assert!(looks_like_negative_sentiment("wtf is this"));
        assert!(looks_like_negative_sentiment("wth happened"));
        assert!(looks_like_negative_sentiment("ffs not again"));
        assert!(looks_like_negative_sentiment("omfg this is bad"));
        assert!(looks_like_negative_sentiment("this is shitty"));
        assert!(looks_like_negative_sentiment("what a dumbass move"));
        assert!(looks_like_negative_sentiment("that's horrible"));
        assert!(looks_like_negative_sentiment("that's awful"));
        assert!(looks_like_negative_sentiment("I'm pissed off"));
        assert!(looks_like_negative_sentiment("piece of shit"));
        assert!(looks_like_negative_sentiment("piece of crap"));
        assert!(looks_like_negative_sentiment("what the fuck"));
        assert!(looks_like_negative_sentiment("what the hell"));
        assert!(looks_like_negative_sentiment("fucking broken again"));
        assert!(looks_like_negative_sentiment("fuck you"));
        assert!(looks_like_negative_sentiment("screw this"));
        assert!(looks_like_negative_sentiment("screw you"));
        assert!(looks_like_negative_sentiment("so frustrating"));
        assert!(looks_like_negative_sentiment("this sucks"));
        assert!(looks_like_negative_sentiment("damn it"));
    }

    #[test]
    fn test_negative_sentiment_negative() {
        assert!(!looks_like_negative_sentiment("sounds good"));
        assert!(!looks_like_negative_sentiment("ship it"));
        assert!(!looks_like_negative_sentiment("awesome work"));
        assert!(!looks_like_negative_sentiment("pass the test"));
        assert!(!looks_like_negative_sentiment("can you help me?"));
        assert!(!looks_like_negative_sentiment("try again please"));
    }

    #[test]
    fn test_negative_sentiment_takes_priority() {
        // Messages matching both patterns should be classified as negative sentiment
        assert!(looks_like_negative_sentiment("wtf is this output"));
        assert!(looks_like_correction("wtf is this output"));
        // Messages matching only correction should not trigger negative sentiment
        assert!(!looks_like_negative_sentiment("try again please"));
        assert!(looks_like_correction("try again please"));
    }

    // ── Formatting ──

    #[test]
    fn test_format_compact() {
        let state = baseline();
        let compact = format_compact(&state);
        assert!(compact.contains("stability:40"));
        assert!(compact.contains("focus:40"));
        assert!(compact.contains("sync:40"));
        assert!(compact.contains("growth:40"));
        assert!(compact.contains("happiness:40"));
    }

    #[test]
    fn test_format_status_structure() {
        let state = baseline();
        let drift = vec![DriftFlag::LowSync];
        let output = format_status(&state, &[], &drift);
        assert!(output.contains("Borg Vitals"));
        assert!(output.contains("stability"));
        assert!(output.contains("focus"));
        assert!(output.contains("sync"));
        assert!(output.contains("growth"));
        assert!(output.contains("happiness"));
        assert!(output.contains("Drift:"));
        assert!(output.contains("Tip:"));
    }

    // ── Failure Drift ──

    #[test]
    fn test_detect_failure_drift() {
        let events = vec![
            make_event("failure"),
            make_event("failure"),
            make_event("failure"),
        ];
        assert!(detect_failure_drift(&events));
    }

    #[test]
    fn test_no_failure_drift() {
        let events = vec![
            make_event("success"),
            make_event("failure"),
            make_event("success"),
        ];
        assert!(!detect_failure_drift(&events));
    }

    fn make_event(category: &str) -> VitalsEvent {
        VitalsEvent {
            id: 0,
            category: category.to_string(),
            source: "test".to_string(),
            stability_delta: 0,
            focus_delta: 0,
            sync_delta: 0,
            growth_delta: 0,
            happiness_delta: 0,
            metadata_json: None,
            created_at: Utc::now().timestamp(),
            hmac: String::new(),
            prev_hmac: String::new(),
        }
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

    // ── HMAC Chain ──

    #[test]
    fn test_hmac_deterministic() {
        let deltas = deltas_for(EventCategory::Interaction);
        let h1 = compute_event_hmac(VITALS_HMAC_LEGACY, "0", "interaction", "test", deltas, 1000);
        let h2 = compute_event_hmac(VITALS_HMAC_LEGACY, "0", "interaction", "test", deltas, 1000);
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_hmac_changes_with_input() {
        let deltas = deltas_for(EventCategory::Interaction);
        let h1 = compute_event_hmac(VITALS_HMAC_LEGACY, "0", "interaction", "test", deltas, 1000);
        let h2 = compute_event_hmac(VITALS_HMAC_LEGACY, "0", "interaction", "test", deltas, 1001);
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_hmac_chain_verification() {
        let deltas = deltas_for(EventCategory::Success);
        let hmac1 =
            compute_event_hmac(VITALS_HMAC_LEGACY, "0", "success", "run_shell", deltas, 100);
        let event = VitalsEvent {
            id: 1,
            category: "success".to_string(),
            source: "run_shell".to_string(),
            stability_delta: deltas.stability as i32,
            focus_delta: deltas.focus as i32,
            sync_delta: deltas.sync as i32,
            growth_delta: deltas.growth as i32,
            happiness_delta: deltas.happiness as i32,
            metadata_json: None,
            created_at: 100,
            hmac: hmac1.clone(),
            prev_hmac: "0".to_string(),
        };
        assert!(verify_event_hmac(VITALS_HMAC_LEGACY, &event, "0"));
        assert!(!verify_event_hmac(VITALS_HMAC_LEGACY, &event, "wrong_prev"));
    }

    #[test]
    fn test_tampered_event_skipped_in_replay() {
        let deltas = deltas_for(EventCategory::Creation);
        let ts = 1000;
        let hmac = compute_event_hmac(
            VITALS_HMAC_LEGACY,
            "0",
            "creation",
            "apply_patch",
            deltas,
            ts,
        );

        let legit = VitalsEvent {
            id: 1,
            category: "creation".to_string(),
            source: "apply_patch".to_string(),
            stability_delta: deltas.stability as i32,
            focus_delta: deltas.focus as i32,
            sync_delta: deltas.sync as i32,
            growth_delta: deltas.growth as i32,
            happiness_delta: deltas.happiness as i32,
            metadata_json: None,
            created_at: ts,
            hmac: hmac.clone(),
            prev_hmac: "0".to_string(),
        };

        // Tampered event with wrong HMAC
        let tampered = VitalsEvent {
            id: 2,
            category: "creation".to_string(),
            source: "fake".to_string(),
            stability_delta: 100,
            focus_delta: 100,
            sync_delta: 100,
            growth_delta: 100,
            happiness_delta: 100,
            metadata_json: None,
            created_at: ts + 1,
            hmac: "fake_hmac".to_string(),
            prev_hmac: hmac,
        };

        let state = replay_events(&[legit, tampered]);
        // Only the legit event should be applied (creation: +1 stab)
        assert_eq!(state.stability, 41); // 40 + 1
        assert_eq!(state.growth, 41); // 40 + 1
    }

    // ── Rate Limiting ──

    #[test]
    fn test_rate_limiting_caps_events() {
        let deltas = deltas_for(EventCategory::Creation);
        let hour_base = 1000 * 3600; // some hour

        let mut events = Vec::new();
        let mut prev = "0".to_string();
        // Create 20 events in the same hour (cap is 3 for creation)
        for i in 0..20 {
            let ts = hour_base + i;
            let hmac =
                compute_event_hmac(VITALS_HMAC_LEGACY, &prev, "creation", "test", deltas, ts);
            events.push(VitalsEvent {
                id: i + 1,
                category: "creation".to_string(),
                source: "test".to_string(),
                stability_delta: deltas.stability as i32,
                focus_delta: deltas.focus as i32,
                sync_delta: deltas.sync as i32,
                growth_delta: deltas.growth as i32,
                happiness_delta: deltas.happiness as i32,
                metadata_json: None,
                created_at: ts,
                hmac: hmac.clone(),
                prev_hmac: prev,
            });
            prev = hmac;
        }

        let state = replay_events(&events);
        // Category cap is 3, but source decay also applies (all same source "test"):
        // count 1: 1.0 (full), count 2: 1.0 (full), count 3: 0.5 → floor(1*0.5)=0
        // So only 2 creation events contribute their +1 stability/growth
        assert_eq!(state.stability, 40 + 2 * 1); // 42
        assert_eq!(state.growth, 40 + 2 * 1); // 42
    }

    #[test]
    fn test_source_decay_skips_repeated_success() {
        // 4 success events from same source "run_shell" in same hour
        // Success delta: stability +1
        // Decay: count 1-2 full, count 3 floor(1*0.5)=0, count 4 floor(1*0.25)=0
        let deltas = deltas_for(EventCategory::Success);
        let hour_base: i64 = 3600;
        let mut events = Vec::new();
        let mut prev = "0".to_string();
        for i in 0..4 {
            let ts = hour_base + i;
            let hmac = compute_event_hmac(
                VITALS_HMAC_LEGACY,
                &prev,
                "success",
                "run_shell",
                deltas,
                ts,
            );
            events.push(VitalsEvent {
                id: i + 1,
                category: "success".to_string(),
                source: "run_shell".to_string(),
                stability_delta: deltas.stability as i32,
                focus_delta: deltas.focus as i32,
                sync_delta: deltas.sync as i32,
                growth_delta: deltas.growth as i32,
                happiness_delta: deltas.happiness as i32,
                metadata_json: None,
                created_at: ts,
                hmac: hmac.clone(),
                prev_hmac: prev,
            });
            prev = hmac;
        }
        let state = replay_events(&events);
        assert_eq!(state.stability, 42); // 40 + 2, not 40 + 4
    }

    #[test]
    fn test_diverse_sources_full_credit() {
        // 4 success events from different sources = full credit for each
        let deltas = deltas_for(EventCategory::Success);
        let hour_base: i64 = 3600;
        let sources = ["run_shell", "browser", "apply_patch", "read_file"];
        let mut events = Vec::new();
        let mut prev = "0".to_string();
        for (i, source) in sources.iter().enumerate() {
            let ts = hour_base + i as i64;
            let hmac = compute_event_hmac(VITALS_HMAC_LEGACY, &prev, "success", source, deltas, ts);
            events.push(VitalsEvent {
                id: i as i64 + 1,
                category: "success".to_string(),
                source: source.to_string(),
                stability_delta: deltas.stability as i32,
                focus_delta: deltas.focus as i32,
                sync_delta: deltas.sync as i32,
                growth_delta: deltas.growth as i32,
                happiness_delta: deltas.happiness as i32,
                metadata_json: None,
                created_at: ts,
                hmac: hmac.clone(),
                prev_hmac: prev,
            });
            prev = hmac;
        }
        let state = replay_events(&events);
        assert_eq!(state.stability, 44); // 40 + 4, all full credit
    }

    #[test]
    fn test_decay_attenuates_negative_deltas() {
        // Failure deltas: stability -1. Repeated same-source failures should also decay.
        let deltas = deltas_for(EventCategory::Failure);
        let hour_base: i64 = 3600;
        let mut events = Vec::new();
        let mut prev = "0".to_string();
        for i in 0..4 {
            let ts = hour_base + i;
            let hmac = compute_event_hmac(
                VITALS_HMAC_LEGACY,
                &prev,
                "failure",
                "run_shell",
                deltas,
                ts,
            );
            events.push(VitalsEvent {
                id: i + 1,
                category: "failure".to_string(),
                source: "run_shell".to_string(),
                stability_delta: deltas.stability as i32,
                focus_delta: deltas.focus as i32,
                sync_delta: deltas.sync as i32,
                growth_delta: deltas.growth as i32,
                happiness_delta: deltas.happiness as i32,
                metadata_json: None,
                created_at: ts,
                hmac: hmac.clone(),
                prev_hmac: prev,
            });
            prev = hmac;
        }
        let state = replay_events(&events);
        // Failure stability: -1. Decay: count 1-2 full (-1 each), count 3 ceil(-1*0.5)=0, count 4: 0
        assert_eq!(state.stability, 38); // 40 - 2, not 40 - 4
    }

    #[test]
    fn test_apply_decay_to_delta_helper() {
        // Positive deltas: floor
        assert_eq!(apply_decay_to_delta(1, 1.0), 1);
        assert_eq!(apply_decay_to_delta(1, 0.5), 0);
        assert_eq!(apply_decay_to_delta(2, 0.5), 1);
        assert_eq!(apply_decay_to_delta(3, 0.5), 1);
        assert_eq!(apply_decay_to_delta(1, 0.25), 0);
        assert_eq!(apply_decay_to_delta(1, 0.0), 0);
        // Negative deltas: ceil (attenuate penalty)
        assert_eq!(apply_decay_to_delta(-1, 1.0), -1);
        assert_eq!(apply_decay_to_delta(-1, 0.5), 0);
        assert_eq!(apply_decay_to_delta(-2, 0.5), -1);
        assert_eq!(apply_decay_to_delta(-3, 0.5), -1);
        assert_eq!(apply_decay_to_delta(-1, 0.25), 0);
        assert_eq!(apply_decay_to_delta(-1, 0.0), 0);
        // Zero
        assert_eq!(apply_decay_to_delta(0, 0.5), 0);
    }

    #[test]
    fn test_i8_to_le_bytes_matches_as_u8_cast() {
        for val in [-3i8, -2, -1, 0, 1, 2, 3] {
            let as_u8 = val as u8;
            let le_bytes = val.to_le_bytes();
            assert_eq!(as_u8, le_bytes[0], "mismatch for {val}");
        }
    }

    #[test]
    fn test_mid_chain_corruption_skips_tail() {
        // Build a 3-event chain: event1 (valid) → event2 (corrupted) → event3 (valid chain from event2)
        let deltas = deltas_for(EventCategory::Success);

        let hmac1 = compute_event_hmac(VITALS_HMAC_LEGACY, "0", "success", "test", deltas, 1000);
        let event1 = VitalsEvent {
            id: 1,
            category: "success".to_string(),
            source: "test".to_string(),
            stability_delta: deltas.stability as i32,
            focus_delta: deltas.focus as i32,
            sync_delta: deltas.sync as i32,
            growth_delta: deltas.growth as i32,
            happiness_delta: deltas.happiness as i32,
            metadata_json: None,
            created_at: 1000,
            hmac: hmac1.clone(),
            prev_hmac: "0".to_string(),
        };

        // Event 2: corrupted HMAC
        let event2 = VitalsEvent {
            id: 2,
            category: "success".to_string(),
            source: "test".to_string(),
            stability_delta: deltas.stability as i32,
            focus_delta: deltas.focus as i32,
            sync_delta: deltas.sync as i32,
            growth_delta: deltas.growth as i32,
            happiness_delta: deltas.happiness as i32,
            metadata_json: None,
            created_at: 2000,
            hmac: "corrupted".to_string(),
            prev_hmac: hmac1.clone(),
        };

        // Event 3: valid chain from event2's corrupted hmac — should still be skipped
        let hmac3 = compute_event_hmac(
            VITALS_HMAC_LEGACY,
            "corrupted",
            "success",
            "test",
            deltas,
            3000,
        );
        let event3 = VitalsEvent {
            id: 3,
            category: "success".to_string(),
            source: "test".to_string(),
            stability_delta: deltas.stability as i32,
            focus_delta: deltas.focus as i32,
            sync_delta: deltas.sync as i32,
            growth_delta: deltas.growth as i32,
            happiness_delta: deltas.happiness as i32,
            metadata_json: None,
            created_at: 3000,
            hmac: hmac3,
            prev_hmac: "corrupted".to_string(),
        };

        let state = replay_events(&[event1, event2, event3]);
        // Only event1 should be applied (success: stability +1)
        assert_eq!(state.stability, 41); // 40 + 1
        assert!(!state.chain_valid);
    }

    // ── Baseline Invariants ──

    #[test]
    fn test_baseline_all_equal() {
        let state = baseline();
        assert_eq!(state.stability, state.focus);
        assert_eq!(state.focus, state.sync);
        assert_eq!(state.sync, state.growth);
        assert_eq!(state.growth, state.happiness);
    }

    #[test]
    fn test_baseline_is_consistent() {
        let state = baseline();
        for val in [
            state.stability,
            state.focus,
            state.sync,
            state.growth,
            state.happiness,
        ] {
            assert_eq!(val, 40);
        }
    }

    #[test]
    fn test_decay_triggers_drift_from_baseline() {
        let mut state = baseline();
        // Simulate 8 days of inactivity (max decay)
        state.last_interaction_at = Utc::now() - Duration::hours(200);
        let decayed = apply_decay(&state, Utc::now());
        // InactiveTooLong fires (>48h), plus stat-based drift flags from baseline=40
        let drift = detect_drift(&decayed, Utc::now());
        assert!(drift.contains(&DriftFlag::LowStability)); // 40-8-5=27 < 30
        assert!(drift.contains(&DriftFlag::LowSync)); // 40-6=34 < 35
        assert!(!drift.contains(&DriftFlag::LowHappiness)); // 40-4=36 >= 30
    }

    #[test]
    fn test_all_negative_events_from_baseline() {
        let mut state = baseline();
        let deltas = deltas_for(EventCategory::Correction); // -1,-1,0,0,-1
                                                            // Apply 50 corrections — should drive all affected stats to 0 without underflow
        for _ in 0..50 {
            apply_deltas(&mut state, &deltas);
        }
        assert_eq!(state.stability, 0);
        assert_eq!(state.focus, 0);
        assert_eq!(state.sync, 40); // corrections don't affect sync
        assert_eq!(state.growth, 40); // corrections don't affect growth
        assert_eq!(state.happiness, 0);
    }

    #[test]
    fn test_growth_from_baseline() {
        let mut state = baseline();
        let deltas = deltas_for(EventCategory::Creation); // +1,0,0,+1,+1
                                                          // Apply 60 creations — growth should hit 100 (40 + 60 = 100)
        for _ in 0..60 {
            apply_deltas(&mut state, &deltas);
        }
        assert_eq!(state.growth, 100);
        assert_eq!(state.happiness, 100); // 40 + 60 = 100
        assert_eq!(state.stability, 100); // 40 + 60 = 100
        assert_eq!(state.focus, 40); // creation doesn't affect focus
    }

    #[test]
    fn test_hmac_with_negative_deltas() {
        let deltas = StatDeltas {
            stability: -3,
            focus: -2,
            sync: -1,
            growth: 0,
            happiness: -1,
        };
        let hmac1 = compute_event_hmac(VITALS_HMAC_LEGACY, "0", "correction", "test", deltas, 1000);
        let hmac2 = compute_event_hmac(VITALS_HMAC_LEGACY, "0", "correction", "test", deltas, 1000);
        assert_eq!(hmac1, hmac2);
        assert!(!hmac1.is_empty());
    }

    // ── Delta Validation ──

    #[test]
    fn validate_deltas_accepts_correct() {
        let categories = [
            ("interaction", EventCategory::Interaction),
            ("success", EventCategory::Success),
            ("failure", EventCategory::Failure),
            ("correction", EventCategory::Correction),
            ("creation", EventCategory::Creation),
        ];
        for (name, cat) in &categories {
            let deltas = deltas_for(*cat);
            assert!(
                validate_deltas(name, deltas).is_ok(),
                "should accept correct deltas for {name}"
            );
        }
    }

    #[test]
    fn validate_deltas_rejects_inflated() {
        let bad = StatDeltas {
            stability: 100,
            focus: 0,
            sync: 0,
            growth: 0,
            happiness: 0,
        };
        assert_eq!(
            validate_deltas("interaction", bad),
            Err("deltas do not match expected values for category")
        );
    }

    #[test]
    fn validate_deltas_rejects_unknown_category() {
        let deltas = StatDeltas::default();
        assert_eq!(
            validate_deltas("hacked", deltas),
            Err("unknown vitals category")
        );
    }

    // ── Replay Stability (Event Sourcing Invariants) ──
    //
    // These tests pin down properties that make event-sourcing a trustworthy
    // state model: determinism, HMAC tampering detection, rate-limit boundary
    // correctness, and honest handling of degenerate ledgers.

    /// Build a properly chained vitals event for the given category/source/ts.
    /// Uses canonical deltas from `deltas_for`, matching what production writes.
    fn chained_vitals_event(
        id: i64,
        category: EventCategory,
        source: &str,
        ts: i64,
        prev_hmac: &str,
    ) -> VitalsEvent {
        let deltas = deltas_for(category);
        let cat_str = category.to_string();
        let hmac = compute_event_hmac(VITALS_HMAC_LEGACY, prev_hmac, &cat_str, source, deltas, ts);
        VitalsEvent {
            id,
            category: cat_str,
            source: source.to_string(),
            stability_delta: deltas.stability as i32,
            focus_delta: deltas.focus as i32,
            sync_delta: deltas.sync as i32,
            growth_delta: deltas.growth as i32,
            happiness_delta: deltas.happiness as i32,
            metadata_json: None,
            created_at: ts,
            hmac,
            prev_hmac: prev_hmac.to_string(),
        }
    }

    /// Chain a slice of (category, source, ts) into valid VitalsEvents.
    fn build_chain(specs: &[(EventCategory, &str, i64)]) -> Vec<VitalsEvent> {
        let mut events = Vec::new();
        let mut prev = "0".to_string();
        for (i, (cat, source, ts)) in specs.iter().enumerate() {
            let ev = chained_vitals_event(i as i64 + 1, *cat, source, *ts, &prev);
            prev = ev.hmac.clone();
            events.push(ev);
        }
        events
    }

    #[test]
    fn replay_determinism_identical_events() {
        // Guards Issue 1: baseline() must not depend on wall clock, so replays
        // of the same slice produce identical state.
        let events = build_chain(&[
            (EventCategory::Interaction, "session_start", 1_000),
            (EventCategory::Success, "run_shell", 1_500),
            (EventCategory::Creation, "apply_patch", 2_000),
            (EventCategory::Failure, "run_shell", 3_700),
            (EventCategory::Correction, "user", 3_900),
            (EventCategory::Success, "read_file", 7_300),
        ]);

        let s1 = replay_events(&events);
        let s2 = replay_events(&events);

        assert_eq!(s1.stability, s2.stability);
        assert_eq!(s1.focus, s2.focus);
        assert_eq!(s1.sync, s2.sync);
        assert_eq!(s1.growth, s2.growth);
        assert_eq!(s1.happiness, s2.happiness);
        assert_eq!(s1.chain_valid, s2.chain_valid);
        assert_eq!(s1.last_interaction_at, s2.last_interaction_at);
        assert_eq!(s1.updated_at, s2.updated_at);
    }

    #[test]
    fn replay_empty_returns_deterministic_baseline() {
        // An empty ledger must return a state whose timestamps are epoch, not
        // "now". Otherwise downstream decay/drift logic sees wall clock noise.
        let s = replay_events(&[]);
        let epoch = DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        assert_eq!(s.last_interaction_at, epoch);
        assert_eq!(s.updated_at, epoch);
        assert_eq!(s.stability, 40);
        assert!(s.chain_valid);
    }

    #[test]
    fn replay_only_failures_keeps_epoch_interaction() {
        // With baseline timestamps anchored to epoch, a ledger consisting
        // entirely of failures/corrections never advances last_interaction_at
        // past epoch. Since epoch = fresh install, drift does NOT fire.
        let ts = 86_400 * 10; // arbitrary 10-day point, well past epoch
        let events = build_chain(&[
            (EventCategory::Failure, "a", ts),
            (EventCategory::Failure, "b", ts + 1),
            (EventCategory::Correction, "user", ts + 2),
        ]);

        let s = replay_events(&events);
        let epoch = DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        assert_eq!(
            s.last_interaction_at, epoch,
            "failures/corrections must not bump last_interaction_at past epoch"
        );

        // Epoch sentinel = fresh install → no InactiveTooLong drift.
        let now = DateTime::<Utc>::from_timestamp(ts + 200, 0).unwrap();
        let drift = detect_drift(&s, now);
        assert!(!drift.contains(&DriftFlag::InactiveTooLong));
    }

    #[test]
    fn test_no_drift_on_fresh_install() {
        let state = baseline();
        let drift = detect_drift(&state, Utc::now());
        assert!(
            !drift.contains(&DriftFlag::InactiveTooLong),
            "fresh install (epoch sentinel) must not flag InactiveTooLong"
        );
    }

    #[test]
    fn test_no_decay_on_fresh_install() {
        let state = baseline();
        let decayed = apply_decay(&state, Utc::now());
        assert_eq!(decayed.stability, state.stability);
        assert_eq!(decayed.sync, state.sync);
        assert_eq!(decayed.happiness, state.happiness);
        assert_eq!(decayed.focus, state.focus);
        assert_eq!(decayed.growth, state.growth);
    }

    #[test]
    fn test_is_fresh_install() {
        let fresh = baseline();
        assert!(is_fresh_install(&fresh));

        let mut used = baseline();
        used.last_interaction_at = Utc::now();
        assert!(!is_fresh_install(&used));
    }

    #[test]
    fn test_drift_fires_after_real_interaction_then_inactivity() {
        let mut state = baseline();
        state.last_interaction_at = Utc::now() - chrono::Duration::hours(72);
        let drift = detect_drift(&state, Utc::now());
        assert!(
            drift.contains(&DriftFlag::InactiveTooLong),
            "72h-old real interaction must trigger InactiveTooLong"
        );
    }

    #[test]
    fn replay_tampered_middle_event_sets_chain_invalid_and_skips_tail() {
        // Strengthens `test_mid_chain_corruption_skips_tail`: explicitly assert
        // that chain_valid flips to false and that no tail event applies even
        // if the tampered event's byte-flip looks "plausible".
        let mut events = build_chain(&[
            (EventCategory::Success, "a", 1_000),
            (EventCategory::Success, "b", 1_500),
            (EventCategory::Creation, "apply_patch", 2_000),
            (EventCategory::Success, "c", 2_500),
        ]);
        // Tamper the middle event's hmac (byte-level): flip one hex char.
        let orig = events[1].hmac.clone();
        let mut tampered = orig.chars().collect::<Vec<_>>();
        tampered[0] = if tampered[0] == '0' { 'f' } else { '0' };
        events[1].hmac = tampered.into_iter().collect::<String>();

        let state = replay_events(&events);

        // Only event 0 (success: stability +1) applied.
        assert_eq!(state.stability, 41);
        // growth would be +1 from the creation event, but that event is in the
        // tampered tail and must be skipped.
        assert_eq!(state.growth, 40);
        assert!(!state.chain_valid);
    }

    #[test]
    fn replay_rate_limit_exact_boundary_interaction() {
        // Interaction cap is 5/hr. With distinct sources (to defeat source
        // decay) the first 5 must apply and the 6th in the same hour must not.
        // A 7th in a fresh hour must apply again.
        let hour = 3600_i64;
        let mut specs: Vec<(EventCategory, &str, i64)> = Vec::new();
        let names = ["s1", "s2", "s3", "s4", "s5"]; // 5 distinct sources
        for (i, n) in names.iter().enumerate() {
            specs.push((EventCategory::Interaction, n, hour + i as i64));
        }
        // 6th in same hour bucket:
        specs.push((EventCategory::Interaction, "s6", hour + 10));
        // 7th in next hour bucket:
        specs.push((EventCategory::Interaction, "s7", 2 * hour));
        let events = build_chain(&specs);

        let state = replay_events(&events);
        // sync delta per interaction is +1. First 5 apply (+5), 6th dropped,
        // 7th in next hour applies (+1) → sync = 40 + 6 = 46.
        assert_eq!(state.sync, 46);
    }

    #[test]
    fn replay_hour_bucket_boundary_timestamps() {
        // ts = 3599 is the last second of hour bucket 0.
        // ts = 3600 is the first second of hour bucket 1.
        // Two same-source events straddling that boundary should each get a
        // full multiplier (source decay counter resets per hour).
        let events = build_chain(&[
            (EventCategory::Success, "run_shell", 3_599),
            (EventCategory::Success, "run_shell", 3_600),
        ]);
        let state = replay_events(&events);
        // Both events full credit → stability 40 + 2 = 42.
        assert_eq!(state.stability, 42);
    }

    #[test]
    fn replay_hmac_key_mismatch_invalidates_all_events() {
        // Events signed with key A must fail verification when replayed under
        // a different key, and must contribute no deltas.
        let events = build_chain(&[
            (EventCategory::Success, "a", 1_000),
            (EventCategory::Creation, "b", 2_000),
        ]);
        let other_key = b"entirely-different-key-for-vitals";
        let state = replay_events_with_key(other_key, &events);
        assert!(!state.chain_valid);
        assert_eq!(state.stability, 40);
        assert_eq!(state.growth, 40);
    }
}
