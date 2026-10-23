//! Skill environment helpers.
//!
//! Two concerns extracted from `skills.rs`:
//!   - **Env aggregation** ([`collect_skill_env`]): per-skill `env` blocks
//!     merged into the map injected into `run_shell`.
//!   - **Env allowlist** ([`collect_required_env_vars`]): set of env var
//!     names declared in `requires.env` across enabled skills — drives which
//!     credentials are forwarded into subprocess environments.
//!
//! These don't touch the skill loader / parser internals, so they live in
//! their own module to keep the main `skills.rs` focused on registry +
//! loading + relevance scoring.

use crate::config::SkillsConfig;

use super::load_all_skills;

/// Aggregate per-skill `env` blocks into a single map. Disabled entries are
/// skipped; the first definition of any given key wins (no override-by-order
/// surprises).
pub fn collect_skill_env(
    skills_config: &SkillsConfig,
) -> std::collections::HashMap<String, String> {
    let mut env = std::collections::HashMap::new();
    for entry in skills_config.entries.values() {
        if entry.enabled {
            for (k, v) in &entry.env {
                env.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
    }
    env
}

/// Collect the set of env var names declared in `requires.env` across all skills.
/// Used to filter which credentials are injected into `run_shell` subprocesses.
///
/// `workspace` MUST match what the prompt-side loader was given: a project
/// skill (`.borg/skills/`) that is injected into the system prompt but
/// missing from this allowlist runs its scripts with the credential it
/// declared filtered out — an auth failure the model cannot explain.
pub fn collect_required_env_vars(
    resolved_creds: &std::collections::HashMap<String, String>,
    skills_config: &SkillsConfig,
    workspace: Option<&std::path::Path>,
) -> std::collections::HashSet<String> {
    match load_all_skills(resolved_creds, skills_config, workspace) {
        Ok(skills) => skills
            .iter()
            .flat_map(|s| s.manifest.requires.env.iter().cloned())
            .collect(),
        Err(e) => {
            tracing::warn!("Failed to load skills for env allowlist: {e}; no credentials will be injected into run_shell");
            std::collections::HashSet::new()
        }
    }
}

/// Format detailed info about a skill. Test-only — the production `borg skill
/// info` path lives in the CLI and renders into a TUI popup, not into
/// stdout.
#[cfg(test)]
pub(super) fn format_skill_info(
    skill: &super::Skill,
    resolved_creds: &std::collections::HashMap<String, String>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("Name:        {}\n", skill.manifest.name));
    out.push_str(&format!("Description: {}\n", skill.manifest.description));
    out.push_str(&format!("Source:      {}\n", skill.source_label()));
    out.push_str(&format!("Status:      {}\n", skill.status_label()));

    if !skill.manifest.os.is_empty() {
        out.push_str(&format!("OS:          {}\n", skill.manifest.os.join(", ")));
    }

    if !skill.manifest.requires.bins.is_empty() {
        out.push_str("Binaries:\n");
        for bin in &skill.manifest.requires.bins {
            let found = which::which(bin).is_ok();
            let mark = if found { "✓" } else { "✗" };
            out.push_str(&format!("  [{mark}] {bin}\n"));
        }
    }
    if !skill.manifest.requires.any_bins.is_empty() {
        out.push_str("Any of:\n");
        for bin in &skill.manifest.requires.any_bins {
            let found = which::which(bin).is_ok();
            let mark = if found { "✓" } else { "✗" };
            out.push_str(&format!("  [{mark}] {bin}\n"));
        }
    }
    if !skill.manifest.requires.env.is_empty() {
        out.push_str("Env vars:\n");
        for var in &skill.manifest.requires.env {
            let found = std::env::var(var).is_ok() || resolved_creds.contains_key(var);
            let mark = if found { "✓" } else { "✗" };
            out.push_str(&format!("  [{mark}] {var}\n"));
        }
    }
    if !skill.manifest.install.is_empty() {
        out.push_str("Install specs:\n");
        for (dep, spec) in &skill.manifest.install {
            let mut methods = Vec::new();
            if let Some(b) = &spec.brew {
                methods.push(format!("brew:{b}"));
            }
            if let Some(a) = &spec.apt {
                methods.push(format!("apt:{a}"));
            }
            if let Some(n) = &spec.npm {
                methods.push(format!("npm:{n}"));
            }
            if let Some(u) = &spec.url {
                methods.push(format!("url:{u}"));
            }
            out.push_str(&format!("  {dep}: {}\n", methods.join(", ")));
        }
    }

    if let Some(first_para) = skill.body.split("\n\n").find(|p| !p.trim().is_empty()) {
        out.push_str(&format!("\n{first_para}\n"));
    }

    out
}
