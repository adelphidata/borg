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
}

pub(crate) fn dispatch_skills(action: Option<SkillsAction>) -> Result<()> {
    match action {
        None | Some(SkillsAction::List) => run_list(),
        Some(SkillsAction::Show { name }) => run_show(&name),
        Some(SkillsAction::Enable { name }) => run_set_enabled(&name, true),
        Some(SkillsAction::Disable { name }) => run_set_enabled(&name, false),
    }
}

fn load_skills() -> Result<Vec<cortex::skills::Skill>> {
    // Propagate DB-load errors instead of silently using defaults — toggling a
    // skill against the wrong baseline would let the operator believe a change
    // landed when the daemon's effective config is something else entirely.
    let config = cortex::config::Config::load_from_db()?;
    cortex::skills::load_all_skills(&std::collections::HashMap::new(), &config.skills)
}

fn run_list() -> Result<()> {
    let skills = load_skills()?;
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

fn run_show(name: &str) -> Result<()> {
    let skills = load_skills()?;
    let skill = skills
        .into_iter()
        .find(|s| s.manifest.name == name)
        .ok_or_else(|| anyhow!("skill '{name}' not found"))?;
    println!("{}", skill.format_for_prompt());
    Ok(())
}

fn run_set_enabled(name: &str, enabled: bool) -> Result<()> {
    // Validate the skill exists so we don't silently write a key for a typo'd name.
    let skills = load_skills()?;
    if !skills.iter().any(|s| s.manifest.name == name) {
        return Err(anyhow!(
            "skill '{name}' not found — run `borg skills` to list known skills"
        ));
    }
    let key = format!("skills.entries.{name}.enabled");
    let value = enabled.to_string();
    let resolver = cortex::settings::SettingsResolver::load()?;
    resolver.set(&key, &value)?;
    let status = if enabled { "enabled" } else { "disabled" };
    println!("{name}: {status}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enable_unknown_skill_errors_without_writing() {
        // Regression: a typo'd skill name must not silently land a
        // `skills.entries.<typo>.enabled = true` row in the DB. The CLI is
        // reachable from `run_shell`, so a confused agent could otherwise
        // pollute the settings table with junk keys that ride forward forever.
        let err = run_set_enabled("definitely-not-a-real-skill-xyz", true).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not found"),
            "expected 'not found' in error, got: {msg}"
        );
    }
}
