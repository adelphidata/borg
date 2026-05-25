//! Skill environment + install helpers.
//!
//! Three concerns extracted from `skills.rs`:
//!   - **Env aggregation** ([`collect_skill_env`]): per-skill `env` blocks
//!     merged into the map injected into `run_shell`.
//!   - **Env allowlist** ([`collect_required_env_vars`]): set of env var
//!     names declared in `requires.env` across enabled skills — drives which
//!     credentials are forwarded into subprocess environments.
//!   - **Install dispatch** ([`install_skill_deps`]): walk a skill's
//!     `install:` block and run the platform-appropriate package manager
//!     for each missing dep.
//!
//! These don't touch the skill loader / parser internals, so they live in
//! their own module to keep the main `skills.rs` focused on registry +
//! loading + relevance scoring.

use anyhow::Result;

use crate::config::SkillsConfig;

use super::{load_all_skills, Skill};

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
pub fn collect_required_env_vars(
    resolved_creds: &std::collections::HashMap<String, String>,
    skills_config: &SkillsConfig,
) -> std::collections::HashSet<String> {
    match load_all_skills(resolved_creds, skills_config) {
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

/// Install missing dependencies for a skill, returning names of those installed.
pub fn install_skill_deps(skill: &Skill) -> Result<Vec<String>> {
    let mut installed = Vec::new();
    for (dep_name, spec) in &skill.manifest.install {
        if which::which(dep_name).is_ok() {
            println!("  {dep_name}: already installed");
            continue;
        }
        let cmd = match std::env::consts::OS {
            "macos" => spec.brew.as_ref().map(|p| format!("brew install {p}")),
            "linux" => spec
                .apt
                .as_ref()
                .map(|p| format!("sudo apt install -y {p}")),
            _ => None,
        }
        .or_else(|| spec.npm.as_ref().map(|p| format!("npm install -g {p}")));

        match cmd {
            Some(c) => {
                println!("  Installing {dep_name}: {c}");
                #[cfg(unix)]
                let (shell, shell_flag) = ("sh", "-c");
                #[cfg(windows)]
                let (shell, shell_flag) = ("cmd.exe", "/C");
                let status = std::process::Command::new(shell)
                    .arg(shell_flag)
                    .arg(&c)
                    .status()?;
                if status.success() {
                    installed.push(dep_name.clone());
                } else {
                    eprintln!("  Failed to install {dep_name}");
                }
            }
            None => {
                if let Some(url) = &spec.url {
                    eprintln!("  Cannot auto-install {dep_name}. Install manually: {url}");
                } else {
                    eprintln!("  No install method available for {dep_name}");
                }
            }
        }
    }
    Ok(installed)
}

/// Format detailed info about a skill. Test-only — the production `borg skill
/// info` path lives in the CLI and renders into a TUI popup, not into
/// stdout.
#[cfg(test)]
pub(super) fn format_skill_info(
    skill: &Skill,
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

    // Body preview (first paragraph)
    if let Some(first_para) = skill.body.split("\n\n").find(|p| !p.trim().is_empty()) {
        out.push_str(&format!("\n{first_para}\n"));
    }

    out
}
