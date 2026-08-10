//! Skill loading: reads bundled (compiled-in) and user (`~/.borg/skills/`)
//! SKILL.md files into [`Skill`] values, applies per-skill config + requirement
//! gating, runs the security audit on user skills, and deduplicates by name.
//! `reload_user_skills` re-scans disk eagerly so operators get a parse/audit
//! signal immediately rather than on the next turn.

use std::path::PathBuf;

use anyhow::Result;
use tracing::{debug, trace, warn};

use crate::config::{Config, SkillsConfig};
use crate::security::sanitize::{self, ThreatLevel};
use crate::security::skill as skill_security;

use super::types::{check_requirements, parse_skill_md, Skill, SkillSource};
use super::{bundled_skill_content, BUNDLED_SKILLS, DEFAULT_ENABLED_SKILLS, MANDATORY_SKILLS};

/// Determine if a skill is disabled given config entries and the default-enabled list.
pub(super) fn is_skill_disabled(name: &str, skills_config: &SkillsConfig) -> bool {
    if MANDATORY_SKILLS.contains(&name) {
        return false;
    }
    match skills_config.entries.get(name) {
        Some(entry) => !entry.enabled, // explicit config wins
        None => !DEFAULT_ENABLED_SKILLS.contains(&name), // default off unless listed
    }
}

pub(super) fn load_builtin_skill(
    content: &str,
    resolved_creds: &std::collections::HashMap<String, String>,
    skills_config: &SkillsConfig,
) -> Result<Skill> {
    let (manifest, body) = parse_skill_md(content)?;
    let disabled = is_skill_disabled(&manifest.name, skills_config);
    let available =
        !disabled && check_requirements(&manifest.requires, &manifest.os, resolved_creds);
    Ok(Skill {
        manifest,
        body,
        source: SkillSource::BuiltIn,
        available,
        disabled,
        references: Vec::new(),
        scripts: Vec::new(),
    })
}

/// Returns the path to the user skills directory (`~/.borg/skills/`).
pub fn skills_dir() -> Result<PathBuf> {
    Config::skills_dir()
}

/// Install bundled skills to the filesystem at `data_dir/skills/<name>/SKILL.md`.
/// Skips any skill that already exists (preserving user customizations).
pub fn install_default_skills(data_dir: &std::path::Path) -> Result<usize> {
    let skills_dir = crate::paths::subdir(data_dir, crate::paths::BorgSubdir::Skills);
    let mut installed = 0;

    for &(name, content) in BUNDLED_SKILLS {
        let skill_dir = skills_dir.join(name);
        let skill_file = skill_dir.join("SKILL.md");

        if skill_file.exists() {
            continue;
        }

        std::fs::create_dir_all(&skill_dir)?;
        std::fs::write(&skill_file, content)?;
        installed += 1;
        debug!("Installed default skill: {name}");
    }

    Ok(installed)
}

/// Load all skills, built-in and user-authored.
///
/// Built-ins win on a name collision: an on-disk `SKILL.md` under a bundled
/// skill's name is skipped and the embedded copy is used. (The doc here used
/// to claim the opposite, which is how the `skills diff` output came to imply
/// that edits to a bundled skill were live.) A bundled skill's supporting
/// files — notably `scripts/` — still run from disk.
pub fn load_all_skills(
    resolved_creds: &std::collections::HashMap<String, String>,
    skills_config: &SkillsConfig,
) -> Result<Vec<Skill>> {
    let mut skills: Vec<Skill> = Vec::new();

    {
        for &(_name, content) in BUNDLED_SKILLS {
            match load_builtin_skill(content, resolved_creds, skills_config) {
                Ok(skill) => {
                    skills.push(skill);
                }
                Err(e) => {
                    debug!("Failed to load built-in skill: {e}");
                }
            }
        }
    }

    let user_dir = skills_dir()?;
    if user_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&user_dir) {
            for entry in entries.flatten() {
                let skill_file = entry.path().join("SKILL.md");
                if skill_file.exists() {
                    match std::fs::read_to_string(&skill_file) {
                        Ok(content) => {
                            let dir_name = entry.file_name().to_string_lossy().to_string();
                            // Auto-installed copies of bundled skills (written by
                            // `install_default_skills`) are byte-identical to the
                            // embedded version. The bundled in-memory skill is
                            // already loaded above; loading the disk copy as a
                            // "user skill" would trigger noisy collision/env-var
                            // warnings for nothing. Skip silently.
                            if BUNDLED_SKILLS.iter().any(|(_, c)| content.as_str() == *c) {
                                // Auto-installed copy of a bundled skill;
                                // bundled in-memory version is already loaded.
                                continue;
                            }
                            match parse_skill_md(&content) {
                                Ok((manifest, mut body)) => {
                                    let name = manifest.name.clone();
                                    let skill_dir = entry.path();

                                    // Older auto-installed bundled copies remain on disk after
                                    // built-in SKILL.md updates. Treat matching built-in
                                    // directory/name pairs as bundled identity rather than
                                    // noisy conflicting user skills.
                                    if dir_name == name && bundled_skill_content(&name).is_some() {
                                        trace!(
                                            "Skipping bundled skill copy on disk for '{}'",
                                            name
                                        );
                                        continue;
                                    }

                                    // Everything downstream keys off the
                                    // frontmatter name — including the
                                    // built-in-shadow check just above, which
                                    // compares the two. A directory that
                                    // disagrees loads fine but behaves as the
                                    // name it declares, so an author editing
                                    // `foo/` and seeing `bar` in the catalog
                                    // has no way to connect the two. Warn, do
                                    // not reject: existing skills must keep
                                    // loading.
                                    if dir_name != name {
                                        warn!(
                                            "Skill directory '{dir_name}' declares name '{name}' \
                                             in its frontmatter; '{name}' is what the catalog, \
                                             invocation, and built-in shadowing all use. Rename \
                                             the directory to match."
                                        );
                                    }

                                    // References are loaded before validation so the
                                    // security scan sees them too — an injection payload
                                    // hides just as well in a reference file as in the body.
                                    let mut references = Vec::new();
                                    let refs_dir = skill_dir.join("references");
                                    if refs_dir.is_dir() {
                                        if let Ok(ref_entries) = std::fs::read_dir(&refs_dir) {
                                            for ref_entry in ref_entries.flatten() {
                                                let ref_path = ref_entry.path();
                                                if ref_path.extension().is_some_and(|e| e == "md") {
                                                    if let Ok(ref_content) =
                                                        std::fs::read_to_string(&ref_path)
                                                    {
                                                        let ref_name = ref_path
                                                            .file_name()
                                                            .unwrap_or_default()
                                                            .to_string_lossy()
                                                            .to_string();
                                                        references.push((ref_name, ref_content));
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    let skill_config = skills_config.entries.get(&name);
                                    let outcome = skill_security::validate_user_skill(
                                        &name,
                                        &manifest,
                                        &body,
                                        &references,
                                        &skill_file,
                                        skill_config,
                                    );
                                    for f in &outcome.findings {
                                        warn!("{}", f.message);
                                    }
                                    if !outcome.allowed {
                                        debug!(
                                            "Skipping user skill '{name}' due to security findings"
                                        );
                                        continue;
                                    }
                                    if matches!(outcome.threat_level, ThreatLevel::Flagged { .. }) {
                                        body = sanitize::wrap_with_injection_warning(&name, &body);
                                    }

                                    let mut scripts = Vec::new();
                                    let scripts_dir = skill_dir.join("scripts");
                                    if scripts_dir.is_dir() {
                                        if let Ok(script_entries) = std::fs::read_dir(&scripts_dir)
                                        {
                                            for script_entry in script_entries.flatten() {
                                                scripts.push(script_entry.path());
                                            }
                                        }
                                    }

                                    let disabled = is_skill_disabled(&manifest.name, skills_config);
                                    let available = !disabled
                                        && check_requirements(
                                            &manifest.requires,
                                            &manifest.os,
                                            resolved_creds,
                                        );

                                    skills.push(Skill {
                                        manifest,
                                        body,
                                        source: SkillSource::User,
                                        available,
                                        disabled,
                                        references,
                                        scripts,
                                    });
                                    debug!("Loaded user skill: {name}");
                                }
                                Err(e) => {
                                    debug!("Failed to parse {}: {e}", skill_file.display());
                                }
                            }
                        }
                        Err(e) => {
                            debug!("Failed to read {}: {e}", skill_file.display());
                        }
                    }
                }
            }
        }
    }

    // Deduplicate by skill name (keep first occurrence per name)
    let mut seen = std::collections::HashSet::new();
    skills.retain(|s| seen.insert(s.manifest.name.clone()));

    Ok(skills)
}

/// Result of a [`reload_user_skills`] pass.
///
/// Per-session skills context is rebuilt from disk on every turn (the cache
/// in `SystemPromptCache` re-derives whenever the user request changes, and
/// is force-cleared by `on_config_reloaded`), so this function does **not**
/// mutate any process-global state. Its job is to (a) re-scan the on-disk
/// `~/.borg/skills/` directory eagerly so the operator gets a parse/audit
/// signal *now* instead of on the next turn, and (b) return counts the TUI
/// can render in a one-line summary.
///
/// Caller is responsible for kicking the config-watch broadcast (see
/// `notify_runtime_config_reload`) so live sessions invalidate their cached
/// skills prompt and re-render on the next turn.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SkillsReloadReport {
    /// Total skills loaded (built-in + user, post-dedup).
    pub total: usize,
    /// User skills loaded from `~/.borg/skills/`.
    pub user: usize,
    /// User skills that failed `check_requirements` (missing bin / env).
    pub user_unavailable: usize,
    /// Skills explicitly disabled via per-skill config.
    pub disabled: usize,
}

/// Re-scan user skills on disk and return a summary report.
///
/// See [`SkillsReloadReport`] for the contract — this function does not
/// touch any in-memory cache. The agent loop's per-turn skills load already
/// reads from disk; this exists so the operator can validate that a newly
/// dropped `~/.borg/skills/<name>/SKILL.md` parses, meets requirements,
/// and is reachable before the next turn fires.
pub fn reload_user_skills(
    resolved_creds: &std::collections::HashMap<String, String>,
    skills_config: &SkillsConfig,
) -> Result<SkillsReloadReport> {
    let skills = load_all_skills(resolved_creds, skills_config)?;
    let mut report = SkillsReloadReport {
        total: skills.len(),
        ..SkillsReloadReport::default()
    };
    for s in &skills {
        if matches!(s.source, SkillSource::User) {
            report.user += 1;
            if !s.available && !s.disabled {
                report.user_unavailable += 1;
            }
        }
        if s.disabled {
            report.disabled += 1;
        }
    }
    Ok(report)
}
