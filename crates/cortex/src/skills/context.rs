//! Skill context assembly and token budgeting: computes the effective skill
//! budget, loads all skills, ranks them by relevance to the user request, and
//! renders as much skill content as fits — emitting a [`SkillBudgetReport`] so
//! the agent loop can surface what was downgraded or dropped.

use anyhow::Result;
use tracing::{debug, instrument, trace};

use crate::config::SkillsConfig;
use crate::memory::tokenizer::estimate_tokens;

use super::loader::load_all_skills;
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

    // Exclude disabled skills from context injection entirely
    let skills: Vec<Skill> = skills.into_iter().filter(|s| !s.disabled).collect();

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
    let mut estimated_tokens: usize = 0;

    for skill in &sorted_skills {
        let meta = skill.format_at_level(SkillLoadLevel::Metadata);
        let tokens = estimate_tokens(&meta);
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
        metadata_parts.push(Some(meta));
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

    // Build final output: full body for upgraded skills, metadata for the rest,
    // skipping omitted entries.
    let parts: Vec<String> = metadata_parts
        .into_iter()
        .enumerate()
        .filter_map(|(i, maybe_meta)| full_overrides.remove(&i).or(maybe_meta))
        .collect();

    report.used_tokens = estimated_tokens;

    // Single per-turn summary in place of the previous per-skill spam.
    let metadata_only = parts.len().saturating_sub(full_overrides.len());
    debug!(
        loaded = parts.len(),
        full = full_overrides.len(),
        metadata_only,
        dropped_full = ?report.dropped_full_body,
        omitted = ?report.omitted_entirely,
        used_tokens = estimated_tokens,
        budget_tokens = max_tokens,
        "skills context built"
    );

    let rendered = if parts.is_empty() {
        String::new()
    } else {
        format!("# Skills\n\n{}\n", parts.join("\n---\n\n"))
    };
    Ok((rendered, report))
}

fn normalized_skill_terms(skill: &Skill) -> Vec<String> {
    let mut terms = vec![
        skill.manifest.name.to_lowercase(),
        skill.manifest.description.to_lowercase(),
    ];
    terms.extend(skill.manifest.triggers.iter().map(|t| t.to_lowercase()));
    terms
}

pub(super) fn skill_relevance_score(skill: &Skill, user_request: &str) -> u32 {
    if user_request.trim().is_empty() {
        return 0;
    }

    let request = user_request.to_lowercase();
    let mut score = 0;

    for trigger in &skill.manifest.triggers {
        let trigger = trigger.trim().to_lowercase();
        if trigger.is_empty() {
            continue;
        }
        if request.contains(&trigger) {
            score += 10;
        } else if trigger
            .split_whitespace()
            .all(|word| !word.is_empty() && request.contains(word))
        {
            score += 5;
        }
    }

    for term in normalized_skill_terms(skill) {
        if term.is_empty() {
            continue;
        }
        if request.contains(&term) {
            score += 3;
        }
    }

    score
}

// `collect_skill_env`, `collect_required_env_vars`, `install_skill_deps`,
// and the test-only `format_skill_info` live in `skills::environment`.
