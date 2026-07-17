/// Parser for `AGENTS.md`-based CLI data directories.
pub mod agents_md;
/// Apply a migration plan to the local Borg data directory.
pub mod apply;
/// Parser for Claude Code data directories.
pub mod claude_code;
/// Parser for Cursor rules directories.
pub mod cursor;
/// Parser for Hermes Agent data directories.
pub mod hermes;
/// Parser for OpenClaw data directories.
pub mod openclaw;
/// Build a dry-run migration plan from parsed source data.
pub mod plan;

use std::path::{Path, PathBuf};

/// Which tool to migrate from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationSource {
    /// Hermes Agent data directory (~/.hermes).
    Hermes,
    /// OpenClaw data directory (~/.openclaw).
    OpenClaw,
    /// Claude Code data directory (~/.claude).
    ClaudeCode,
    /// Cursor data directory (~/.cursor).
    Cursor,
    /// A CLI that keeps its user-level instructions in an `AGENTS.md`.
    AgentsMd,
}

/// Every source, in the order pickers present them.
const ALL_SOURCES: [MigrationSource; 5] = [
    MigrationSource::Hermes,
    MigrationSource::OpenClaw,
    MigrationSource::ClaudeCode,
    MigrationSource::Cursor,
    MigrationSource::AgentsMd,
];

impl MigrationSource {
    /// Human-readable display name for this source.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Hermes => "Hermes Agent",
            Self::OpenClaw => "OpenClaw",
            Self::ClaudeCode => "Claude Code",
            Self::Cursor => "Cursor",
            Self::AgentsMd => "AGENTS.md CLI",
        }
    }

    /// Path to the source tool's data directory.
    pub fn data_dir(&self) -> PathBuf {
        let home = dirs::home_dir().unwrap_or_default();
        match self {
            Self::Hermes => home.join(".hermes"),
            Self::OpenClaw => home.join(".openclaw"),
            Self::ClaudeCode => home.join(".claude"),
            Self::Cursor => home.join(".cursor"),
            Self::AgentsMd => home.join(".codex"),
        }
    }

    /// Returns true if this source has anything to migrate on this system.
    ///
    /// The memory-only sources probe for the specific files they import rather
    /// than the directory: `~/.cursor` and `~/.claude` exist on plenty of
    /// machines that have no rules or memories in them, and offering an import
    /// that would copy nothing is just noise in the picker.
    pub fn is_installed(&self) -> bool {
        match self {
            Self::Hermes | Self::OpenClaw => self.data_dir().is_dir(),
            Self::ClaudeCode => claude_code::has_importable(&self.data_dir()),
            Self::Cursor => cursor::has_importable(&self.data_dir()),
            Self::AgentsMd => agents_md::has_importable(&self.data_dir()),
        }
    }
}

/// Detect which migration sources are available on this system.
pub fn detect_sources() -> Vec<MigrationSource> {
    ALL_SOURCES
        .into_iter()
        .filter(MigrationSource::is_installed)
        .collect()
}

/// One memory/rules file to import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryImport {
    /// File to read.
    pub source: PathBuf,
    /// Destination relative to `~/.borg/memory/`. May contain
    /// `scopes/<scope>/` path segments.
    pub dest_rel: String,
    /// Project scope this belongs to; `None` for globally-loaded memory.
    pub scope: Option<String>,
    /// Where it came from, stamped into the imported file so its origin
    /// survives the copy.
    pub origin: &'static str,
}

impl MemoryImport {
    /// A globally-scoped import.
    pub fn global(source: PathBuf, dest_rel: impl Into<String>, origin: &'static str) -> Self {
        Self {
            source,
            dest_rel: dest_rel.into(),
            scope: None,
            origin,
        }
    }

    /// An import that only loads when `memory.memory_scope` matches `scope`.
    pub fn scoped(
        source: PathBuf,
        scope: impl Into<String>,
        file_name: &str,
        origin: &'static str,
    ) -> Self {
        let scope = scope.into();
        Self {
            source,
            dest_rel: format!("scopes/{scope}/{file_name}"),
            scope: Some(scope),
            origin,
        }
    }
}

/// Turn an external project key into a scope slug Borg can load.
///
/// External tools key projects by encoded absolute path
/// (`-Users-me-src-proj`); Borg scopes are plain identifiers. Runs of
/// non-alphanumerics collapse to a single `-` so the result stays readable
/// and, more importantly, stable — two projects must not collide.
pub fn scope_slug(project_key: &str) -> String {
    let mut out = String::with_capacity(project_key.len());
    let mut last_dash = true; // trims leading separators
    for c in project_key.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Collect `*.md` files under `dir`, recursively, sorted for deterministic
/// plans. Symlinks are skipped: an import should copy the tool's own files,
/// not follow a link out of the directory it was pointed at.
pub(crate) fn collect_markdown(dir: &Path, out: &mut Vec<(PathBuf, String)>, prefix: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.file_type().is_symlink() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if meta.is_dir() {
            let nested = if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix}/{name}")
            };
            collect_markdown(&path, out, &nested);
        } else if path.extension().is_some_and(|e| e == "md") {
            let rel = if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix}/{name}")
            };
            out.push((path, rel));
        }
    }
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
    /// Memory/rules files to import.
    pub memory_files: Vec<MemoryImport>,
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
    /// Memory/rules files to import.
    pub memory_files: Vec<MemoryImport>,
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
            for import in &self.memory_files {
                lines.push(format!("  {}", import.dest_rel));
            }
            if self.memory_files.iter().any(|i| i.scope.is_some()) {
                lines.push(
                    "  (scoped entries load when memory.memory_scope is set to that scope)"
                        .to_string(),
                );
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
        "xai" | "x-ai" | "grok" => "xai",
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
        MigrationSource::ClaudeCode | MigrationSource::Cursor | MigrationSource::AgentsMd => {
            parse_source_from(source, &source.data_dir(), categories)
        }
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
        MigrationSource::ClaudeCode => claude_code::parse_from(root, categories),
        MigrationSource::Cursor => cursor::parse_from(root, categories),
        MigrationSource::AgentsMd => agents_md::parse_from(root, categories),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_source_labels() {
        assert_eq!(MigrationSource::Hermes.label(), "Hermes Agent");
        assert_eq!(MigrationSource::OpenClaw.label(), "OpenClaw");
        assert_eq!(MigrationSource::ClaudeCode.label(), "Claude Code");
        assert_eq!(MigrationSource::Cursor.label(), "Cursor");
        assert_eq!(MigrationSource::AgentsMd.label(), "AGENTS.md CLI");
    }

    /// Slugs key Borg's scope directories, so two distinct projects must never
    /// collapse onto one — that would merge unrelated projects' memories.
    #[test]
    fn scope_slug_normalizes_without_collapsing_distinct_projects() {
        assert_eq!(scope_slug("-Users-me-src-Proj"), "users-me-src-proj");
        assert_eq!(scope_slug("/Users/me/src/proj"), "users-me-src-proj");
        // Leading/trailing/repeated separators are trimmed, not carried.
        assert_eq!(scope_slug("--foo__bar--"), "foo-bar");
        assert_ne!(scope_slug("-Users-me-a"), scope_slug("-Users-me-b"));
        // Degenerate keys produce an empty slug, which parsers must skip.
        assert_eq!(scope_slug("---"), "");
    }

    #[test]
    fn memory_import_scoped_lands_under_its_scope_dir() {
        let import = MemoryImport::scoped(
            PathBuf::from("/src/fact.md"),
            "my-proj",
            "claude-fact.md",
            "claude-code",
        );
        assert_eq!(import.dest_rel, "scopes/my-proj/claude-fact.md");
        assert_eq!(import.scope.as_deref(), Some("my-proj"));
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
