use anyhow::{anyhow, Result};
use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum SkillsAction {
    /// List all built-in and user skills with status
    List,
    /// Print a skill's full SKILL.md body
    Show {
        /// Skill name
        name: String,
    },
    /// Enable a skill (writes skills.entries.<name>.enabled = true)
    Enable {
        /// Skill name
        name: String,
    },
    /// Disable a skill (writes skills.entries.<name>.enabled = false)
    Disable {
        /// Skill name
        name: String,
    },
    /// Show which bundled skills you've modified on disk (no name), or diff one
    /// bundled skill against your `~/.borg/skills/<name>/SKILL.md` copy.
    Diff {
        /// Skill name to diff; omit to list all modified bundled skills.
        name: Option<String>,
    },
}

pub(crate) async fn dispatch_skills(action: Option<SkillsAction>) -> Result<()> {
    match action {
        None | Some(SkillsAction::List) => run_list().await,
        Some(SkillsAction::Show { name }) => run_show(&name).await,
        Some(SkillsAction::Enable { name }) => run_set_enabled(&name, true).await,
        Some(SkillsAction::Disable { name }) => run_set_enabled(&name, false).await,
        Some(SkillsAction::Diff { name }) => run_diff(name.as_deref()).await,
    }
}

/// `skills diff` — surface user modifications to bundled skills. With no name,
/// lists every bundled skill the user has a local copy of and whether it
/// diverges from the shipped baseline; with a name, prints a line-level diff.
async fn run_diff(name: Option<&str>) -> Result<()> {
    let dir = cortex::skills::skills_dir()?;
    match name {
        None => {
            let overrides = cortex::skills::modified_bundled_skills(&dir);
            if overrides.is_empty() {
                println!(
                    "No local copies of bundled skills found under {}.",
                    dir.display()
                );
                return Ok(());
            }
            println!("{:24} STATUS", "NAME");
            let mut any_modified = false;
            for o in &overrides {
                let status = if o.modified {
                    any_modified = true;
                    "modified (NOT loaded)"
                } else {
                    "unchanged"
                };
                println!("{:24} {status}", o.name);
            }
            if any_modified {
                // The loader deliberately ignores on-disk SKILL.md shadows of
                // bundled names — the embedded copy always wins. Saying only
                // "modified" read as "your edit is live", so users tuned a
                // bundled skill, saw it listed as modified, and never learned
                // the agent had not read a byte of it.
                println!(
                    "\nNote: edits to a bundled skill's SKILL.md are NOT loaded — the \
                     built-in copy wins.\n      To customize one, copy it to a new name \
                     under {}.\n      (Supporting files such as scripts/ DO run from disk.)",
                    dir.display()
                );
            }
        }
        Some(name) => {
            let (bundled, user) =
                cortex::skills::bundled_vs_user_skill(name, &dir).ok_or_else(|| {
                    anyhow!(
                        "no bundled skill '{name}' with a local copy at {}/{name}/SKILL.md",
                        dir.display()
                    )
                })?;
            if bundled.trim() == user.trim() {
                println!("'{name}' matches the bundled version (no changes).");
                return Ok(());
            }
            print_line_diff(bundled, &user);
            println!(
                "\nNote: these changes are NOT loaded — the built-in '{name}' wins. \
                 Copy it to a new name to customize it."
            );
        }
    }
    Ok(())
}

/// Minimal, dependency-free line diff: `-` bundled-only, `+` user-only, ` ` common.
fn print_line_diff(bundled: &str, user: &str) {
    let user_lines: std::collections::HashSet<&str> = user.lines().collect();
    let bundled_lines: std::collections::HashSet<&str> = bundled.lines().collect();
    println!("--- bundled\n+++ user (~/.borg/skills)");
    for line in bundled.lines() {
        if !user_lines.contains(line) {
            println!("-{line}");
        }
    }
    for line in user.lines() {
        if !bundled_lines.contains(line) {
            println!("+{line}");
        }
    }
}

async fn load_skills() -> Result<Vec<cortex::skills::Skill>> {
    // Propagate DB-load errors instead of silently using defaults — toggling a
    // skill against the wrong baseline would let the operator believe a change
    // landed when the daemon's effective config is something else entirely.
    let config = cortex::config::Config::load_from_db().await?;
    // The CLI's working directory is the operator's workspace, so the project
    // tier belongs in this listing. Without it, `borg skills list` would
    // report a skill the checkout injects into every prompt as absent — the
    // one place someone would look to audit exactly that.
    let workspace = std::env::current_dir().ok();
    cortex::skills::load_all_skills(
        &std::collections::HashMap::new(),
        &config.skills,
        workspace.as_deref(),
    )
}

async fn run_list() -> Result<()> {
    let skills = load_skills().await?;
    if skills.is_empty() {
        println!("No skills found.");
        return Ok(());
    }
    println!(
        "{:24} {:14} {:10} {:9}  DESCRIPTION",
        "NAME", "CATEGORY", "SOURCE", "STATUS"
    );
    for s in &skills {
        println!(
            "{:24} {:14} {:10} {:9}  {}",
            s.manifest.name,
            s.category(),
            s.source_label(),
            s.status_label(),
            s.manifest.description
        );
    }
    Ok(())
}

async fn run_show(name: &str) -> Result<()> {
    let skills = load_skills().await?;
    let skill = skills
        .into_iter()
        .find(|s| s.manifest.name == name)
        .ok_or_else(|| anyhow!("skill '{name}' not found"))?;
    println!("{}", skill.format_for_prompt());
    Ok(())
}

async fn run_set_enabled(name: &str, enabled: bool) -> Result<()> {
    // Validate the skill exists so we don't silently write a key for a typo'd name.
    let skills = load_skills().await?;
    if !skills.iter().any(|s| s.manifest.name == name) {
        return Err(anyhow!(
            "skill '{name}' not found — run `borg skills` to list known skills"
        ));
    }
    let key = format!("skills.entries.{name}.enabled");
    let value = enabled.to_string();
    let resolver = cortex::settings::SettingsResolver::load().await?;
    resolver.set(&key, &value).await?;
    let status = if enabled { "enabled" } else { "disabled" };
    println!("{name}: {status}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn enable_unknown_skill_errors_without_writing() {
        // Regression: a typo'd skill name must not silently land a
        // `skills.entries.<typo>.enabled = true` row in the DB. The CLI is
        // reachable from `run_shell`, so a confused agent could otherwise
        // pollute the settings table with junk keys that ride forward forever.
        let tmp = tempfile::tempdir().expect("tmpdir");
        // SAFETY: tests are single-threaded by default for env-var mutations
        // here; this binding scope ends with the function.
        unsafe {
            std::env::set_var("BORG_HOME", tmp.path());
        }
        let err = run_set_enabled("definitely-not-a-real-skill-xyz", true)
            .await
            .unwrap_err();
        let msg = err.to_string();
        unsafe {
            std::env::remove_var("BORG_HOME");
        }
        assert!(
            msg.contains("not found"),
            "expected 'not found' in error, got: {msg}"
        );
    }
}
