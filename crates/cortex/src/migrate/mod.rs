/// Apply a migration plan to the local Borg data directory.
pub mod apply;
/// Parser for Hermes Agent data directories.
pub mod hermes;
/// Parser for OpenClaw data directories.
pub mod openclaw;
/// Build a dry-run migration plan from parsed source data.
pub mod plan;

use std::path::PathBuf;

/// Which tool to migrate from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationSource {
    /// Hermes Agent data directory (~/.hermes).
    Hermes,
    /// OpenClaw data directory (~/.openclaw).
    OpenClaw,
}

impl MigrationSource {
    /// Human-readable display name for this source.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Hermes => "Hermes Agent",
            Self::OpenClaw => "OpenClaw",
        }
    }

    /// Path to the source tool's data directory.
    pub fn data_dir(&self) -> PathBuf {
        let home = dirs::home_dir().unwrap_or_default();
        match self {
            Self::Hermes => home.join(".hermes"),
            Self::OpenClaw => home.join(".openclaw"),
        }
    }

    /// Returns true if the source tool's data directory exists.
    pub fn is_installed(&self) -> bool {
        self.data_dir().is_dir()
    }
}

/// Detect which migration sources are available on this system.
pub fn detect_sources() -> Vec<MigrationSource> {
    [MigrationSource::Hermes, MigrationSource::OpenClaw]
        .into_iter()
        .filter(MigrationSource::is_installed)
        .collect()
}

/// What categories of data to migrate.
#[derive(Debug, Clone)]
pub struct MigrationCategories {
    /// Whether to migrate configuration settings.
    pub config: bool,
    /// Whether to migrate API keys and tokens.
    pub credentials: bool,
    /// Whether to migrate memory files.
    pub memory: bool,
    /// Whether to migrate persona/identity files.
    pub persona: bool,
    /// Whether to migrate custom skill files.
    pub skills: bool,
}

impl Default for MigrationCategories {
    fn default() -> Self {
        Self {
            config: true,
            credentials: true,
            memory: true,
            persona: true,
            skills: true,
        }
    }
}

impl MigrationCategories {
    /// Human-readable labels for each category, indexed by position.
    pub const LABELS: [&'static str; 5] = [
        "Configuration (LLM, tools, browser settings)",
        "Credentials (API keys, channel tokens)",
        "Memory (MEMORY.md, USER.md)",
        "Persona (SOUL.md -> IDENTITY.md)",
        "Skills (custom skill files)",
    ];

    /// Get the enabled state of a category by index.
    pub fn get(&self, index: usize) -> bool {
        match index {
            0 => self.config,
            1 => self.credentials,
            2 => self.memory,
            3 => self.persona,
            4 => self.skills,
            _ => false,
        }
    }

    /// Toggle the enabled state of a category by index.
    pub fn toggle(&mut self, index: usize) {
        match index {
            0 => self.config = !self.config,
            1 => self.credentials = !self.credentials,
            2 => self.memory = !self.memory,
            3 => self.persona = !self.persona,
            4 => self.skills = !self.skills,
            _ => {}
        }
    }
}

/// Intermediate representation of parsed source data.
#[derive(Debug, Clone, Default)]
pub struct SourceData {
    /// Config key-value pairs mapped to Borg field paths.
    pub config_changes: Vec<ConfigChange>,
    /// Env var credentials: (VAR_NAME, value).
    pub credentials: Vec<(String, String)>,
    /// Memory files to copy: (source_path, dest_relative_path).
    pub memory_files: Vec<(PathBuf, String)>,
    /// Persona file source path (if found).
    pub persona_file: Option<PathBuf>,
    /// Skill directories to copy: (source_path, dest_name).
    pub skill_dirs: Vec<(PathBuf, String)>,
}

/// A single config field change.
#[derive(Debug, Clone)]
pub struct ConfigChange {
    /// TOML section name (e.g. "llm", "sandbox").
    pub section: String,
    /// Field name within the section.
    pub field: String,
    /// Original key name in the source tool's config.
    pub source_key: String,
    /// Value to set in Borg's config.
    pub new_value: String,
}

impl std::fmt::Display for ConfigChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}: {}", self.section, self.field, self.new_value)
    }
}

/// The full migration plan (dry-run output).
#[derive(Debug, Clone)]
pub struct MigrationPlan {
    /// Which tool this plan migrates from.
    pub source: MigrationSource,
    /// Config changes with current vs new values.
    pub config_changes: Vec<PlanChange>,
    /// Credential env var names to add.
    pub credentials_to_add: Vec<String>,
    /// Memory files to copy: (source_path, dest_relative_path).
    pub memory_files: Vec<(PathBuf, String)>,
    /// Persona/identity file source path, if found.
    pub persona_file: Option<PathBuf>,
    /// Skill directories to copy: (source_path, dest_name).
    pub skill_dirs: Vec<(PathBuf, String)>,
}

/// A planned config change with current vs new value.
#[derive(Debug, Clone)]
pub struct PlanChange {
    /// Borg config key (e.g. "llm.model").
    pub key: String,
    /// Original key name in the source tool's config.
    pub source_key: String,
    /// Current value in Borg's config, if any.
    pub current_value: Option<String>,
    /// Proposed new value.
    pub new_value: String,
    /// Whether this change was skipped (e.g. already set).
    pub skipped: bool,
}

impl MigrationPlan {
    /// Returns true if the plan contains no changes to apply.
    pub fn is_empty(&self) -> bool {
        self.config_changes.is_empty()
            && self.credentials_to_add.is_empty()
            && self.memory_files.is_empty()
            && self.persona_file.is_none()
            && self.skill_dirs.is_empty()
    }

    /// Format the plan as human-readable summary lines.
    pub fn summary_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();

        if !self.config_changes.is_empty() {
            lines.push("Config changes:".to_string());
            for c in &self.config_changes {
                let status = if c.skipped {
                    " (skip: already set)"
                } else {
                    ""
                };
                let current = c
                    .current_value
                    .as_deref()
                    .map(|v| format!("{v} -> "))
                    .unwrap_or_default();
                lines.push(format!("  {}: {}{}{}", c.key, current, c.new_value, status));
            }
            lines.push(String::new());
        }

        if !self.credentials_to_add.is_empty() {
            lines.push(format!("Credentials ({}):", self.credentials_to_add.len()));
            for name in &self.credentials_to_add {
                lines.push(format!("  {name}"));
            }
            lines.push(String::new());
        }

        if !self.memory_files.is_empty() {
            lines.push("Memory files:".to_string());
            for (_, dest) in &self.memory_files {
                lines.push(format!("  {dest}"));
            }
            lines.push(String::new());
        }

        if self.persona_file.is_some() {
            lines.push("Persona:".to_string());
            lines.push("  SOUL.md -> IDENTITY.md".to_string());
            lines.push(String::new());
        }

        if !self.skill_dirs.is_empty() {
            lines.push("Skills:".to_string());
            for (_, name) in &self.skill_dirs {
                lines.push(format!("  {name}/"));
            }
            lines.push(String::new());
        }

        if lines.is_empty() {
            lines.push("Nothing to migrate.".to_string());
        }

        lines
    }

    /// Count of config changes that will actually be applied (not skipped).
    pub fn active_change_count(&self) -> usize {
        self.config_changes.iter().filter(|c| !c.skipped).count()
    }
}

/// Result of applying a migration.
#[derive(Debug)]
pub struct MigrationResult {
    /// Number of config settings that were changed.
    pub config_changes_applied: usize,
    /// Number of credentials that were imported.
    pub credentials_added: usize,
    /// Number of memory files that were copied.
    pub memory_files_copied: usize,
    /// Whether the persona/identity file was copied.
    pub persona_copied: bool,
    /// Number of skill directories that were copied.
    pub skills_copied: usize,
    /// Non-fatal warnings encountered during migration.
    pub warnings: Vec<String>,
}

/// Map a provider name from a legacy tool to Borg's canonical provider string.
pub fn map_provider_name(name: &str) -> &'static str {
    match name.to_lowercase().as_str() {
        "openrouter" => "openrouter",
        "anthropic" => "anthropic",
        "openai" => "openai",
        "google" | "gemini" => "gemini",
        "deepseek" => "deepseek",
        "groq" => "groq",
        "ollama" => "ollama",
        _ => "openrouter",
    }
}

/// Parse source data for a given migration source.
pub fn parse_source(
    source: MigrationSource,
    categories: &MigrationCategories,
) -> anyhow::Result<SourceData> {
    match source {
        MigrationSource::Hermes => hermes::parse(categories),
        MigrationSource::OpenClaw => openclaw::parse(categories),
    }
}

/// Parse source data from a specific root directory (for testing).
pub fn parse_source_from(
    source: MigrationSource,
    root: &std::path::Path,
    categories: &MigrationCategories,
) -> anyhow::Result<SourceData> {
    match source {
        MigrationSource::Hermes => hermes::parse_from(root, categories),
        MigrationSource::OpenClaw => openclaw::parse_from(root, categories),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_source_labels() {
        assert_eq!(MigrationSource::Hermes.label(), "Hermes Agent");
        assert_eq!(MigrationSource::OpenClaw.label(), "OpenClaw");
    }

    #[test]
    fn categories_default_all_true() {
        let c = MigrationCategories::default();
        assert!(c.config);
        assert!(c.credentials);
        assert!(c.memory);
        assert!(c.persona);
        assert!(c.skills);
    }

    #[test]
    fn categories_toggle() {
        let mut c = MigrationCategories::default();
        c.toggle(0);
        assert!(!c.config);
        c.toggle(0);
        assert!(c.config);
        c.toggle(4);
        assert!(!c.skills);
    }

    #[test]
    fn categories_get() {
        let c = MigrationCategories::default();
        for i in 0..5 {
            assert!(c.get(i));
        }
        assert!(!c.get(99));
    }

    #[test]
    fn map_provider_name_known() {
        assert_eq!(super::map_provider_name("anthropic"), "anthropic");
        assert_eq!(super::map_provider_name("openai"), "openai");
        assert_eq!(super::map_provider_name("google"), "gemini");
        assert_eq!(super::map_provider_name("Gemini"), "gemini");
        assert_eq!(super::map_provider_name("unknown"), "openrouter");
    }

    #[test]
    fn plan_is_empty_when_default() {
        let plan = MigrationPlan {
            source: MigrationSource::Hermes,
            config_changes: vec![],
            credentials_to_add: vec![],
            memory_files: vec![],
            persona_file: None,
            skill_dirs: vec![],
        };
        assert!(plan.is_empty());
    }

    #[test]
    fn plan_summary_lines_empty() {
        let plan = MigrationPlan {
            source: MigrationSource::Hermes,
            config_changes: vec![],
            credentials_to_add: vec![],
            memory_files: vec![],
            persona_file: None,
            skill_dirs: vec![],
        };
        let lines = plan.summary_lines();
        assert_eq!(lines, vec!["Nothing to migrate."]);
    }

    #[test]
    fn plan_summary_lines_with_changes() {
        let plan = MigrationPlan {
            source: MigrationSource::Hermes,
            config_changes: vec![PlanChange {
                key: "llm.model".to_string(),
                source_key: "model.default".to_string(),
                current_value: None,
                new_value: "gpt-4".to_string(),
                skipped: false,
            }],
            credentials_to_add: vec!["OPENAI_API_KEY".to_string()],
            memory_files: vec![],
            persona_file: Some(PathBuf::from("/tmp/SOUL.md")),
            skill_dirs: vec![],
        };
        let lines = plan.summary_lines();
        assert!(lines.iter().any(|l| l.contains("llm.model")));
        assert!(lines.iter().any(|l| l.contains("OPENAI_API_KEY")));
        assert!(lines.iter().any(|l| l.contains("SOUL.md")));
    }
}
