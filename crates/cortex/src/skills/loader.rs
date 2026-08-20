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

/// Resolve `path` and return it only if it stays inside `root`.
///
/// `root` must already be canonical. A skill directory is allowed to *be* a
/// symlink — that is how an author keeps a skill in a checkout elsewhere — but
/// an individual file beneath it must not point back out. Without this check a
/// `scripts/deploy.sh` symlinked at `/etc/cron.d/…`, or at another skill's
/// scripts directory, loads under this skill's name and runs with whatever
/// trust the operator granted it.
///
/// Returns `None` for anything that cannot be resolved (a broken symlink, a
/// racing delete) — unreadable is treated as not-contained, never as allowed.
pub(super) fn contained_path(root: &std::path::Path, path: &std::path::Path) -> Option<PathBuf> {
    let resolved = std::fs::canonicalize(path).ok()?;
    resolved.starts_with(root).then_some(resolved)
}

/// Load every skill directory under `dir`, tagging each with `source`.
///
/// Shared by the user tier (`~/.borg/skills/`) and the project tier
/// (`<workspace>/.borg/skills/`, `<workspace>/.agents/skills/`): both are
/// on-disk, author-controlled skills that must clear the same security scan.
fn load_skills_from_dir(
    dir: &std::path::Path,
    source: SkillSource,
    resolved_creds: &std::collections::HashMap<String, String>,
    skills_config: &SkillsConfig,
    skills: &mut Vec<Skill>,
) {
    if !dir.exists() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
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
                                    trace!("Skipping bundled skill copy on disk for '{}'", name);
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

                                // The directory itself may be a symlink into a
                                // checkout elsewhere; resolve it so supporting
                                // files can be tested against where they really
                                // load from, not where they were discovered.
                                let Ok(source_dir) = std::fs::canonicalize(&skill_dir) else {
                                    warn!(
                                        "Skipping skill '{name}': cannot resolve {}",
                                        skill_dir.display()
                                    );
                                    continue;
                                };

                                // References are loaded before validation so the
                                // security scan sees them too — an injection payload
                                // hides just as well in a reference file as in the body.
                                let mut references = Vec::new();
                                let refs_dir = skill_dir.join("references");
                                if refs_dir.is_dir() {
                                    if let Ok(ref_entries) = std::fs::read_dir(&refs_dir) {
                                        for ref_entry in ref_entries.flatten() {
                                            let ref_path = ref_entry.path();
                                            if !ref_path.extension().is_some_and(|e| e == "md") {
                                                continue;
                                            }
                                            let Some(ref_path) =
                                                contained_path(&source_dir, &ref_path)
                                            else {
                                                warn!(
                                                    "Skill '{name}': ignoring reference \
                                                         '{}' — it resolves outside the skill \
                                                         directory",
                                                    ref_path.display()
                                                );
                                                continue;
                                            };
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
                                    debug!("Skipping {source:?} skill '{name}' due to security findings");
                                    continue;
                                }
                                if matches!(outcome.threat_level, ThreatLevel::Flagged { .. }) {
                                    body = sanitize::wrap_with_injection_warning(&name, &body);
                                }

                                let mut scripts = Vec::new();
                                let scripts_dir = skill_dir.join("scripts");
                                if scripts_dir.is_dir() {
                                    if let Ok(script_entries) = std::fs::read_dir(&scripts_dir) {
                                        for script_entry in script_entries.flatten() {
                                            // Defence in depth, not a boundary:
                                            // the agent runs skill scripts by path
                                            // through `run_shell`, which this does
                                            // not gate. It keeps a symlinked-out
                                            // script from being *advertised* as
                                            // part of the skill.
                                            match contained_path(&source_dir, &script_entry.path())
                                            {
                                                Some(script) => scripts.push(script),
                                                None => warn!(
                                                    "Skill '{name}': ignoring script '{}' \
                                                         — it resolves outside the skill \
                                                         directory",
                                                    script_entry.path().display()
                                                ),
                                            }
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
                                    source: source.clone(),
                                    available,
                                    disabled,
                                    references,
                                    scripts,
                                });
                                debug!("Loaded {source:?} skill: {name}");
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

/// Skill directories belonging to `workspace`, highest precedence first.
///
/// `.borg/skills/` is Borg's own; `.agents/skills/` is the cross-tool
/// convention, so a repo that already ships skills for another harness is
/// picked up without duplicating them.
///
/// The root is passed in rather than read from the process working
/// directory: `borgd` is long-lived and its cwd is not any session's
/// workspace, so reading the process cwd would load a different repo's
/// skills — or none — depending on where the daemon happened to start. A
/// session's workspace is fixed for the life of the conversation, which also
/// keeps the skill set (and the cached system-prompt prefix) byte-stable
/// across turns.
pub fn project_skills_dirs(workspace: &std::path::Path) -> Vec<PathBuf> {
    [".borg/skills", ".agents/skills"]
        .iter()
        .map(|rel| workspace.join(rel))
        .filter(|p| p.is_dir())
        .collect()
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
    workspace: Option<&std::path::Path>,
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

    // Project tier before the user tier: a repo that ships a skill is
    // describing how to work on *this* checkout, which is more specific than
    // a personal skill of the same name. Built-ins still win over both — a
    // repo must never be able to redefine `run_shell`-adjacent behavior by
    // name (`security::skill` hard-blocks that case as well).
    if let Some(workspace) = workspace.filter(|_| skills_config.project_discovery) {
        for dir in project_skills_dirs(workspace) {
            load_skills_from_dir(
                &dir,
                SkillSource::Project,
                resolved_creds,
                skills_config,
                &mut skills,
            );
        }
    }

    let user_dir = skills_dir()?;
    load_skills_from_dir(
        &user_dir,
        SkillSource::User,
        resolved_creds,
        skills_config,
        &mut skills,
    );

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
    let skills = load_all_skills(resolved_creds, skills_config, None)?;
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
