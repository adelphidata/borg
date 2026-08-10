//! Skill context assembly and token budgeting: computes the effective skill
//! budget, loads all skills, ranks them by relevance to the user request, and
//! renders as much skill content as fits — emitting a [`SkillBudgetReport`] so
//! the agent loop can surface what was downgraded or dropped.

use anyhow::Result;
use tracing::{debug, instrument, trace};

use crate::config::SkillsConfig;
use crate::memory::tokenizer::estimate_tokens;

use super::loader::load_all_skills;
use super::selectors::{lexical_relevance, SkillSelectionDoc, SHADOW_SELECTORS};
use super::types::{Skill, SkillLoadLevel};

/// Diagnostic report from `load_skills_context` describing what fit, what was
/// downgraded, and what was dropped entirely. The agent loop logs and surfaces
/// non-empty drops to the user so silent loss of skill context is visible.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SkillBudgetReport {
    /// Available skills downgraded from full body to metadata-only because
    /// the full body would have exceeded the budget.
    pub dropped_full_body: Vec<String>,
    /// Skills whose even metadata didn't fit and were omitted entirely.
    /// Previously this was a silent loss path.
    pub omitted_entirely: Vec<String>,
    /// Effective budget actually applied (after `budget_pct` clamping).
    pub budget_tokens: usize,
    /// Estimated tokens consumed by the rendered skills section.
    pub used_tokens: usize,
    /// Per-skill token cost as actually rendered into the catalog this turn:
    /// full-body skills at their full cost, metadata-only (downgraded) skills
    /// at their small metadata cost. Skills omitted entirely do not appear —
    /// they contribute zero tokens. Order matches the rendered catalog
    /// (relevance-ranked), so it is deterministic for a given request.
    /// Feeds the per-skill rows of the `/context` breakdown.
    pub per_skill_tokens: Vec<(String, usize)>,
}

impl SkillBudgetReport {
    /// True if anything had to be dropped or downgraded — caller should warn.
    pub fn is_lossy(&self) -> bool {
        !self.dropped_full_body.is_empty() || !self.omitted_entirely.is_empty()
    }
}

/// Resolve the effective skill budget for a model with the given context window.
/// `context_window` is in tokens (e.g. 200_000 for Claude Sonnet 4); pass `None`
/// to skip percentage clamping and use only the absolute cap.
pub fn effective_skill_budget(skills_config: &SkillsConfig, context_window: Option<u32>) -> usize {
    let absolute = skills_config.max_context_tokens;
    match (skills_config.budget_pct, context_window) {
        (Some(pct), Some(window)) if pct > 0.0 && window > 0 => {
            // Use f64 with explicit rounding so f32→f64 conversion artifacts
            // (e.g. 0.02_f32 → 0.0199999...) don't silently shrink the budget.
            let pct_budget = ((window as f64) * (pct as f64)).round() as usize;
            absolute.min(pct_budget.max(1))
        }
        _ => absolute,
    }
}

/// Build the skills section for system prompt injection within a token budget.
///
/// Returns the rendered string and a [`SkillBudgetReport`] capturing which
/// skills were downgraded or omitted so callers can surface a non-fatal warning
/// to the user. Earlier the omission path was silent.
#[instrument(skip_all)]
pub fn load_skills_context(
    max_tokens: usize,
    resolved_creds: &std::collections::HashMap<String, String>,
    skills_config: &SkillsConfig,
    user_request: &str,
) -> Result<(String, SkillBudgetReport)> {
    let mut report = SkillBudgetReport {
        budget_tokens: max_tokens,
        ..Default::default()
    };

    let skills = load_all_skills(resolved_creds, skills_config)?;

    if skills.is_empty() {
        return Ok((String::new(), report));
    }

    let skills: Vec<Skill> = skills.into_iter().filter(|s| !s.disabled).collect();

    // Shadow-mode selection: record what each relevance-gated selector WOULD
    // have injected this turn versus the full catalog we inject today.
    // Metrics only — behavior is unchanged until accumulated real-usage data
    // shows selection wouldn't starve turns of skills they actually needed.
    // Gated on the log level: it re-formats and re-scores every skill, work
    // the budget loop below already does, so it must not run when nobody
    // will read the result.
    if tracing::enabled!(tracing::Level::DEBUG) {
        let shadow = shadow_selection_stats(&skills, user_request);
        for method in &shadow.methods {
            debug!(
                method = method.method,
                total = shadow.total,
                selected = method.selected,
                all_tokens = shadow.all_tokens,
                selected_tokens = method.selected_tokens,
                query_truncated = method.query_truncated,
                candidate_set_truncated = method.candidate_set_truncated,
                top = ?method.top,
                "skill shadow-selection: {} would inject {}/{} skills (~{}/{} metadata tokens)",
                method.method,
                method.selected,
                shadow.total,
                method.selected_tokens,
                shadow.all_tokens,
            );
        }
    }

    // Sort metadata so relevant available skills consume the budget first.
    let mut sorted_skills = skills;
    sorted_skills.sort_by(|a, b| {
        let score_a = skill_relevance_score(a, user_request);
        let score_b = skill_relevance_score(b, user_request);
        (!a.available)
            .cmp(&!b.available)
            .then_with(|| score_b.cmp(&score_a))
            .then_with(|| a.manifest.name.cmp(&b.manifest.name))
    });

    let mut metadata_parts: Vec<Option<String>> = Vec::with_capacity(sorted_skills.len());
    // The rendered catalog costs more than the sum of its entries: a `# Skills`
    // header, a `\n---\n\n` rule between consecutive entries, and a trailing
    // newline. Charging those makes `max_tokens` a bound on what actually reaches
    // the prompt instead of on the entry bodies alone.
    let framing_tokens = estimate_tokens(CATALOG_HEADER) + estimate_tokens("\n");
    let separator_tokens = estimate_tokens(ENTRY_SEPARATOR);
    let mut estimated_tokens: usize = framing_tokens;
    let mut admitted = 0usize;

    for skill in &sorted_skills {
        let meta = skill.format_at_level(SkillLoadLevel::Metadata);
        let tokens = estimate_tokens(&meta) + if admitted == 0 { 0 } else { separator_tokens };
        if estimated_tokens.saturating_add(tokens) > max_tokens {
            trace!(
                "Skipping skill '{}' entirely — metadata exceeds budget",
                skill.manifest.name
            );
            report.omitted_entirely.push(skill.manifest.name.clone());
            metadata_parts.push(None);
            continue;
        }
        estimated_tokens += tokens;
        admitted += 1;
        metadata_parts.push(Some(meta));
    }

    // A marker that does not fit is worse than one fewer entry: the budget is
    // tightest exactly when the most is hidden, which is when the model most needs
    // to know that something is. Evict admitted entries from the tail — they are
    // the least relevant — until the terse marker has room.
    //
    // Never down to zero, though: one real skill entry is worth more to the model
    // than a bare "N omitted" note with no catalog attached. If the marker cannot
    // coexist with even a single entry, the marker is the thing that goes.
    while admitted > 1
        && !report.omitted_entirely.is_empty()
        && omission_marker_within_budget(
            report.omitted_entirely.len(),
            max_tokens.saturating_sub(estimated_tokens),
        )
        .is_none()
    {
        let Some(last) = metadata_parts.iter().rposition(Option::is_some) else {
            break;
        };
        let Some(evicted) = metadata_parts[last].take() else {
            break;
        };
        estimated_tokens = estimated_tokens
            .saturating_sub(estimate_tokens(&evicted))
            .saturating_sub(if admitted > 1 { separator_tokens } else { 0 });
        admitted -= 1;
        report
            .omitted_entirely
            .push(sorted_skills[last].manifest.name.clone());
    }

    let mut full_overrides: std::collections::HashMap<usize, String> =
        std::collections::HashMap::new();

    let mut upgrade_order: Vec<usize> = sorted_skills
        .iter()
        .enumerate()
        .filter_map(|(i, skill)| skill.available.then_some(i))
        .collect();
    upgrade_order.sort_by(|a, b| {
        let score_a = skill_relevance_score(&sorted_skills[*a], user_request);
        let score_b = skill_relevance_score(&sorted_skills[*b], user_request);
        score_b.cmp(&score_a).then_with(|| {
            sorted_skills[*a]
                .manifest
                .name
                .cmp(&sorted_skills[*b].manifest.name)
        })
    });

    for i in upgrade_order {
        let skill = &sorted_skills[i];
        let Some(meta) = metadata_parts[i].as_ref() else {
            continue;
        };
        let full = skill.format_at_level(SkillLoadLevel::Full);
        let full_tokens = estimate_tokens(&full);
        let meta_tokens = estimate_tokens(meta);
        let additional = full_tokens.saturating_sub(meta_tokens);

        if estimated_tokens + additional > max_tokens {
            trace!(
                "Skipping full body for skill '{}' (would exceed token budget)",
                skill.manifest.name
            );
            report.dropped_full_body.push(skill.manifest.name.clone());
            continue;
        }

        estimated_tokens += additional;
        trace!(
            "Included full skill '{}' ({full_tokens} estimated tokens)",
            skill.manifest.name
        );
        full_overrides.insert(i, full);
    }

    // Read before the drain below empties the map.
    let full_count = full_overrides.len();

    // Per-skill costs as rendered: the full body where one was admitted,
    // otherwise the metadata entry. Recorded before the drain below consumes
    // `full_overrides`.
    report.per_skill_tokens = metadata_parts
        .iter()
        .enumerate()
        .filter_map(|(i, maybe_meta)| {
            let rendered = full_overrides.get(&i).or(maybe_meta.as_ref())?;
            Some((
                sorted_skills[i].manifest.name.clone(),
                estimate_tokens(rendered),
            ))
        })
        .collect();

    // Build final output: full body for upgraded skills, metadata for the rest,
    // skipping omitted entries.
    let parts: Vec<String> = metadata_parts
        .into_iter()
        .enumerate()
        .filter_map(|(i, maybe_meta)| full_overrides.remove(&i).or(maybe_meta))
        .collect();

    // Tell the model what it can't see. Without this the omission is invisible:
    // the catalog simply ends, and the model has no way to know that skills exist
    // beyond the ones listed (it would have to think to call `list`). The marker
    // is charged against the same budget and shrunk until it fits, so it can
    // never be the reason the budget is blown.
    let omission_marker = if report.omitted_entirely.is_empty() {
        None
    } else {
        omission_marker_within_budget(
            report.omitted_entirely.len(),
            max_tokens.saturating_sub(estimated_tokens),
        )
    };
    if let Some(marker) = omission_marker.as_deref() {
        // The marker goes into the prompt, so it belongs in `used_tokens` —
        // otherwise the report under-states what this turn actually spent.
        estimated_tokens += omission_marker_cost(marker);
    }

    // Nothing admitted renders an empty string, framing included.
    report.used_tokens = if parts.is_empty() {
        0
    } else {
        estimated_tokens
    };

    // Single per-turn summary in place of the previous per-skill spam.
    let metadata_only = parts.len().saturating_sub(full_count);
    debug!(
        loaded = parts.len(),
        full = full_count,
        metadata_only,
        dropped_full = ?report.dropped_full_body,
        omitted = ?report.omitted_entirely,
        used_tokens = estimated_tokens,
        budget_tokens = max_tokens,
        "skills context built"
    );

    let rendered = match (parts.is_empty(), omission_marker.as_deref()) {
        (true, _) => String::new(),
        (false, Some(marker)) => format!(
            "{CATALOG_HEADER}{}\n\n{marker}\n",
            parts.join(ENTRY_SEPARATOR)
        ),
        (false, None) => format!("{CATALOG_HEADER}{}\n", parts.join(ENTRY_SEPARATOR)),
    };
    Ok((rendered, report))
}

/// Framing of the rendered catalog. Named so the budget arithmetic above and the
/// `format!` calls below can never disagree about what the output costs.
const CATALOG_HEADER: &str = "# Skills\n\n";
const ENTRY_SEPARATOR: &str = "\n---\n\n";

/// Token cost of an omission marker as it is actually emitted — the rendered
/// catalog wraps it in blank-line separators, which the budget must account for.
fn omission_marker_cost(marker: &str) -> usize {
    estimate_tokens(&format!("\n\n{marker}\n"))
}

/// Render the "N skills omitted" notice, shortened until it fits `spare_tokens`.
///
/// Falls back to the terse form and then to `None` rather than overshooting the
/// budget — the marker exists to explain a truncation, so it must not cause one.
fn omission_marker_within_budget(omitted: usize, spare_tokens: usize) -> Option<String> {
    let noun = if omitted == 1 { "skill" } else { "skills" };
    let verbose = format!(
        "_{omitted} additional {noun} omitted from this bounded list — call `list` with `what: \"skills\"` to see them._"
    );
    if omission_marker_cost(&verbose) <= spare_tokens {
        return Some(verbose);
    }
    let terse = format!("_{omitted} additional {noun} omitted._");
    (omission_marker_cost(&terse) <= spare_tokens).then_some(terse)
}

/// What each shadow selection method WOULD have injected this turn, versus what
/// injecting the full catalog costs. Shadow-only: computed and logged from
/// `load_skills_context`, never used to gate injection — that decision needs
/// evidence from real conversations first.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ShadowSelectionStats {
    /// Enabled skills considered this turn.
    pub total: usize,
    /// Estimated metadata tokens for full injection (what we pay today).
    pub all_tokens: usize,
    /// One entry per method in [`SHADOW_SELECTORS`], in declaration order.
    pub methods: Vec<MethodShadowStats>,
}

/// One selection method's shadow result for a turn.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct MethodShadowStats {
    /// Stable metrics identifier, e.g. `fielded_bm25_v1`.
    pub method: &'static str,
    /// Skills this method would keep.
    pub selected: usize,
    /// Estimated metadata tokens for the kept subset.
    pub selected_tokens: usize,
    /// The query hit a bound.
    pub query_truncated: bool,
    /// The catalog hit a bound.
    pub candidate_set_truncated: bool,
    /// Highest-ranked names, for eyeballing agreement between methods.
    pub top: Vec<String>,
}

/// Names logged per method. Enough to compare methods at a glance without
/// turning one turn's log line into a wall of text.
const SHADOW_TOP_N: usize = 5;

/// Compute [`ShadowSelectionStats`] for this turn's request over the enabled
/// skill set. Every method sees the same docs and the same per-skill metadata
/// cost, so their numbers are directly comparable.
pub(crate) fn shadow_selection_stats(skills: &[Skill], user_request: &str) -> ShadowSelectionStats {
    let docs: Vec<SkillSelectionDoc<'_>> = skills
        .iter()
        .enumerate()
        .map(|(id, s)| SkillSelectionDoc {
            id,
            name: &s.manifest.name,
            triggers: &s.manifest.triggers,
            description: &s.manifest.description,
        })
        .collect();
    // Metadata formatting is the expensive part; do it once and reuse it for
    // every method's token total.
    let token_costs: Vec<usize> = skills
        .iter()
        .map(|s| estimate_tokens(&s.format_at_level(SkillLoadLevel::Metadata)))
        .collect();

    let methods = SHADOW_SELECTORS
        .iter()
        .map(|selector| {
            let outcome = selector.select(user_request, &docs, skills.len());
            MethodShadowStats {
                method: selector.method(),
                selected: outcome.candidate_ids.len(),
                selected_tokens: outcome
                    .candidate_ids
                    .iter()
                    .map(|id| token_costs[*id])
                    .sum(),
                query_truncated: outcome.query_truncated,
                candidate_set_truncated: outcome.candidate_set_truncated,
                top: outcome
                    .candidate_ids
                    .iter()
                    .take(SHADOW_TOP_N)
                    .map(|id| skills[*id].manifest.name.clone())
                    .collect(),
            }
        })
        .collect();

    ShadowSelectionStats {
        total: skills.len(),
        all_tokens: token_costs.iter().sum(),
        methods,
    }
}

pub(super) fn skill_relevance_score(skill: &Skill, user_request: &str) -> u32 {
    lexical_relevance(
        &skill.manifest.name,
        &skill.manifest.description,
        &skill.manifest.triggers,
        user_request,
    )
}

#[cfg(test)]
mod marker_tests {
    use super::*;

    /// The marker explains a truncation, so it must never cause one. With no
    /// room left it has to be dropped entirely rather than silently appended —
    /// the terse fallback used to be returned without any budget check.
    #[test]
    fn marker_is_dropped_when_even_the_terse_form_does_not_fit() {
        assert_eq!(omission_marker_within_budget(3, 0), None);
        assert_eq!(omission_marker_within_budget(3, 1), None);
    }

    /// Whatever comes back must fit the spare budget it was handed, at every
    /// width — that is the entire contract.
    #[test]
    fn returned_marker_always_fits_the_spare_budget() {
        for spare in 0..40 {
            if let Some(marker) = omission_marker_within_budget(7, spare) {
                assert!(
                    omission_marker_cost(&marker) <= spare,
                    "spare {spare}: {marker:?} costs {}",
                    omission_marker_cost(&marker)
                );
            }
        }
    }

    /// A generous budget keeps the actionable form, which names the `list` call.
    /// Degrading to the terse form unconditionally would lose that.
    #[test]
    fn ample_budget_keeps_the_actionable_wording() {
        let marker = omission_marker_within_budget(7, 1_000).expect("marker must fit");
        assert!(marker.contains("`list`"), "{marker:?}");
        assert!(marker.contains("7 additional skills"), "{marker:?}");
    }

    /// Singular/plural: "1 additional skills omitted" reads as a bug to the model.
    #[test]
    fn single_omission_uses_the_singular_noun() {
        let marker = omission_marker_within_budget(1, 1_000).expect("marker must fit");
        assert!(marker.contains("1 additional skill "), "{marker:?}");
    }
}
