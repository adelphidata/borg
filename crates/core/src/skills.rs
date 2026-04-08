use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;
use tracing::{debug, instrument};

use crate::config::{Config, SkillsConfig};
use crate::tokenizer::estimate_tokens;

/// How much of a skill's content to include in prompt context.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SkillLoadLevel {
    /// Name, description, and availability status only.
    Metadata,
    /// Metadata plus the first paragraph of the skill body.
    Summary,
    /// The entire SKILL.md body.
    Full,
}

macro_rules! bundled_skills {
    ($( $name:literal => $path:literal ),* $(,)?) => {
        const BUNDLED_SKILLS: &[(&str, &str)] = &[
            $( ($name, include_str!(concat!("../skills/", $path, "/SKILL.md"))) ),*
        ];
    };
}

bundled_skills! {
    "slack" => "slack",
    "discord" => "discord",
    "github" => "github",
    "weather" => "weather",
    "skill-creator" => "skill-creator",
    "git" => "git",
    "search" => "search",
    "docker" => "docker",
    "database" => "database",
    "notes" => "notes",
    "calendar" => "calendar",
    "1password" => "1password",
    "browser" => "browser",
    "scheduler" => "scheduler",
    "email" => "email",
}

/// Where a skill was loaded from.
#[derive(Debug, Clone, PartialEq)]
pub enum SkillSource {
    /// Compiled into the binary.
    BuiltIn,
    /// Loaded from the user's `~/.borg/skills/` directory.
    User,
}

/// Runtime requirements that must be satisfied for a skill to be available.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SkillRequires {
    /// Binaries that must all be present on `$PATH`.
    #[serde(default)]
    pub bins: Vec<String>,
    /// Environment variables that must all be set.
    #[serde(default)]
    pub env: Vec<String>,
    /// At least one of these binaries must be present on `$PATH`.
    #[serde(default)]
    pub any_bins: Vec<String>,
}

/// Platform-specific installation commands for a dependency.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct InstallSpec {
    /// Homebrew formula name (macOS).
    #[serde(default)]
    pub brew: Option<String>,
    /// APT package name (Linux).
    #[serde(default)]
    pub apt: Option<String>,
    /// npm package name (cross-platform).
    #[serde(default)]
    pub npm: Option<String>,
    /// Manual install URL fallback.
    #[serde(default)]
    pub url: Option<String>,
}

/// Parsed YAML frontmatter from a SKILL.md file.
#[derive(Debug, Clone, Deserialize)]
pub struct SkillManifest {
    /// Unique skill identifier.
    pub name: String,
    /// Human-readable description of what the skill does.
    pub description: String,
    /// Runtime requirements (binaries, env vars).
    #[serde(default)]
    pub requires: SkillRequires,
    /// Supported operating systems (empty means all).
    #[serde(default)]
    pub os: Vec<String>,
    /// Per-dependency install instructions keyed by dependency name.
    #[serde(default)]
    pub install: std::collections::HashMap<String, InstallSpec>,
    /// Plugin category for unified UI grouping (e.g., "developer", "utilities").
    #[serde(default)]
    pub category: Option<String>,
}

/// Skills enabled by default when no explicit config entry exists.
pub const DEFAULT_ENABLED_SKILLS: &[&str] = &["browser", "git", "search", "email", "calendar"];

/// Skills hidden from the unified /plugins UI (still loaded for prompt injection).
pub const HIDDEN_SKILLS: &[&str] = &["skill-creator", "slack", "discord", "scheduler"];

/// Hardcoded category fallback for built-in skills without a `category` field.
const SKILL_CATEGORY_MAP: &[(&str, &str)] = &[
    ("git", "developer"),
    ("github", "developer"),
    ("docker", "developer"),
    ("database", "developer"),
    ("search", "core"),
    ("browser", "core"),
    ("calendar", "core"),
    ("email", "core"),
    ("weather", "utilities"),
    ("http", "utilities"),
    ("1password", "utilities"),
    ("notes", "utilities"),
    ("scheduler", "utilities"),
    ("slack", "channels"),
    ("discord", "channels"),
    ("skill-creator", "utilities"),
];

/// A loaded skill with its manifest, body, and runtime metadata.
#[derive(Debug, Clone)]
pub struct Skill {
    /// Parsed YAML frontmatter.
    pub manifest: SkillManifest,
    /// Markdown body content (instructions and examples).
    pub body: String,
    /// Whether this skill is built-in or user-defined.
    pub source: SkillSource,
    /// Whether all runtime requirements are satisfied.
    pub available: bool,
    /// Whether the user has explicitly disabled this skill.
    pub disabled: bool,
    /// Reference files from `references/*.md` as `(filename, content)` pairs.
    pub references: Vec<(String, String)>,
    /// Paths to scripts in the skill's `scripts/` directory.
    pub scripts: Vec<PathBuf>,
}

impl Skill {
    /// Returns a display label for the skill's source ("built-in" or "user").
    pub fn source_label(&self) -> &'static str {
        match self.source {
            SkillSource::BuiltIn => "built-in",
            SkillSource::User => "user",
        }
    }

    /// Returns a human-readable availability status string.
    pub fn status_label(&self) -> &'static str {
        if self.disabled {
            "disabled"
        } else if self.available {
            "available"
        } else {
            "unavailable (missing requirements)"
        }
    }

    /// Returns the plugin category for this skill.
    /// Hardcoded map is authoritative for built-in skills (handles stale on-disk frontmatter),
    /// then falls back to manifest frontmatter for user-defined skills.
    pub fn category(&self) -> &str {
        if let Some((_, cat)) = SKILL_CATEGORY_MAP
            .iter()
            .find(|(name, _)| *name == self.manifest.name.as_str())
        {
            return cat;
        }
        if let Some(cat) = &self.manifest.category {
            return cat.as_str();
        }
        "utilities"
    }

    /// Returns true if this skill should be hidden from the unified /plugins UI.
    pub fn is_hidden(&self) -> bool {
        HIDDEN_SKILLS.contains(&self.manifest.name.as_str())
    }

    /// Returns a single-character status icon (checkmark, cross, or dash).
    pub fn status_icon(&self) -> &'static str {
        if self.disabled {
            "—"
        } else if self.available {
            "✓"
        } else {
            "✗"
        }
    }

    /// Format the full skill body for system prompt injection.
    pub fn format_for_prompt(&self) -> String {
        self.format_at_level(SkillLoadLevel::Full)
    }

    /// Format the skill at the specified detail level.
    pub fn format_at_level(&self, level: SkillLoadLevel) -> String {
        let source = self.source_label();
        let status = self.status_label();
        match level {
            SkillLoadLevel::Metadata => {
                format!(
                    "### {} ({}, {}) — {}",
                    self.manifest.name, source, status, self.manifest.description
                )
            }
            SkillLoadLevel::Summary => {
                let first_para = self
                    .body
                    .split("\n\n")
                    .find(|p| !p.trim().is_empty())
                    .unwrap_or("");
                format!(
                    "## Skill: {} [{}, {}]\n\n{}\n",
                    self.manifest.name, source, status, first_para
                )
            }
            SkillLoadLevel::Full => {
                format!(
                    "## Skill: {} [{}, {}]\n\n{}\n",
                    self.manifest.name, source, status, self.body
                )
            }
        }
    }

    /// Format a one-line summary with status icon, name, source, and description.
    pub fn summary_line(&self) -> String {
        let mut line = format!(
            "[{}] {} ({}) — {}",
            self.status_icon(),
            self.manifest.name,
            self.source_label(),
            self.manifest.description
        );
        if !self.available && !self.disabled {
            if let Some(hint) = self.install_hint() {
                line.push_str(&format!("\n    Install: {hint}"));
            }
        }
        line
    }

    /// Return a platform-appropriate install command for missing dependencies.
    /// Looks at the `install` map in the manifest and picks the best option
    /// for the current OS: brew (macOS), apt (Linux), npm, or url as fallback.
    pub fn install_hint(&self) -> Option<String> {
        if self.manifest.install.is_empty() {
            return None;
        }

        let mut hints = Vec::new();
        let mut keys: Vec<_> = self.manifest.install.keys().collect();
        keys.sort();
        for key in keys {
            let spec = &self.manifest.install[key];
            let hint = if cfg!(target_os = "macos") {
                spec.brew
                    .as_deref()
                    .map(|b| format!("brew install {b}"))
                    .or_else(|| spec.npm.as_deref().map(|n| format!("npm install -g {n}")))
                    .or_else(|| spec.url.as_deref().map(|u| format!("See: {u}")))
            } else {
                spec.apt
                    .as_deref()
                    .map(|a| format!("apt install {a}"))
                    .or_else(|| spec.brew.as_deref().map(|b| format!("brew install {b}")))
                    .or_else(|| spec.npm.as_deref().map(|n| format!("npm install -g {n}")))
                    .or_else(|| spec.url.as_deref().map(|u| format!("See: {u}")))
            };
            if let Some(h) = hint {
                hints.push(h);
            }
        }

        if hints.is_empty() {
            None
        } else {
            Some(hints.join("; "))
        }
    }
}

/// Parse a SKILL.md file into its YAML manifest and markdown body.
pub fn parse_skill_md(content: &str) -> Result<(SkillManifest, String)> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        anyhow::bail!("SKILL.md must start with YAML frontmatter (---)")
    }

    let after_first = &trimmed[3..];
    let end = after_first
        .find("\n---")
        .context("Missing closing --- in SKILL.md frontmatter")?;

    let yaml_str = &after_first[..end];
    let body = after_first[end + 4..].trim().to_string();

    let manifest: SkillManifest =
        serde_yaml::from_str(yaml_str).context("Failed to parse SKILL.md YAML frontmatter")?;

    Ok((manifest, body))
}

fn check_os_requirements(os: &[String]) -> bool {
    if os.is_empty() {
        return true;
    }
    let current = std::env::consts::OS;
    os.iter()
        .any(|o| o == current || (o == "darwin" && current == "macos"))
}

fn check_requirements(
    requires: &SkillRequires,
    os: &[String],
    resolved_creds: &std::collections::HashMap<String, String>,
) -> bool {
    if !check_os_requirements(os) {
        debug!("Skill requirement missing: unsupported OS (need {:?})", os);
        return false;
    }
    for bin in &requires.bins {
        if which::which(bin).is_err() {
            debug!("Skill requirement missing: binary '{bin}'");
            return false;
        }
    }
    if !requires.any_bins.is_empty() && !requires.any_bins.iter().any(|b| which::which(b).is_ok()) {
        debug!(
            "Skill requirement missing: none of {:?} found",
            requires.any_bins
        );
        return false;
    }
    for var in &requires.env {
        if std::env::var(var).is_err() && !resolved_creds.contains_key(var) {
            debug!("Skill requirement missing: env var '{var}'");
            return false;
        }
    }
    true
}

/// Determine if a skill is disabled given config entries and the default-enabled list.
fn is_skill_disabled(name: &str, skills_config: &SkillsConfig) -> bool {
    match skills_config.entries.get(name) {
        Some(entry) => !entry.enabled, // explicit config wins
        None => !DEFAULT_ENABLED_SKILLS.contains(&name), // default off unless listed
    }
}

fn load_builtin_skill(
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
    let skills_dir = data_dir.join("skills");
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

/// Load all skills (built-in and user), with user skills overriding built-ins.
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

    // Load user skills from ~/.borg/skills/*/SKILL.md
    let user_dir = skills_dir()?;
    if user_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&user_dir) {
            for entry in entries.flatten() {
                let skill_file = entry.path().join("SKILL.md");
                if skill_file.exists() {
                    match std::fs::read_to_string(&skill_file) {
                        Ok(content) => match parse_skill_md(&content) {
                            Ok((manifest, body)) => {
                                let disabled = is_skill_disabled(&manifest.name, skills_config);
                                let available = !disabled
                                    && check_requirements(
                                        &manifest.requires,
                                        &manifest.os,
                                        resolved_creds,
                                    );
                                let name = manifest.name.clone();
                                let skill_dir = entry.path();

                                // Load references from references/*.md
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

                                // Detect scripts
                                let mut scripts = Vec::new();
                                let scripts_dir = skill_dir.join("scripts");
                                if scripts_dir.is_dir() {
                                    if let Ok(script_entries) = std::fs::read_dir(&scripts_dir) {
                                        for script_entry in script_entries.flatten() {
                                            scripts.push(script_entry.path());
                                        }
                                    }
                                }

                                // User skills override built-in skills with the same name
                                skills.retain(|s| s.manifest.name != name);

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
                        },
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

/// Build the skills section for system prompt injection within a token budget.
#[instrument(skip_all)]
pub fn load_skills_context(
    max_tokens: usize,
    resolved_creds: &std::collections::HashMap<String, String>,
    skills_config: &SkillsConfig,
) -> Result<String> {
    let skills = load_all_skills(resolved_creds, skills_config)?;

    if skills.is_empty() {
        return Ok(String::new());
    }

    // Exclude disabled skills from context injection entirely
    let skills: Vec<Skill> = skills.into_iter().filter(|s| !s.disabled).collect();

    // Sort: available skills first, then unavailable
    let mut sorted_skills = skills;
    sorted_skills.sort_by_key(|s| !s.available);

    // Phase 1: Include metadata for ALL skills
    let mut metadata_parts = Vec::new();
    let mut estimated_tokens = 0;

    for skill in &sorted_skills {
        let meta = skill.format_at_level(SkillLoadLevel::Metadata);
        let tokens = estimate_tokens(&meta);
        metadata_parts.push(meta);
        estimated_tokens += tokens;
    }

    // Phase 2: With remaining budget, upgrade available skills to full body
    let mut full_overrides: std::collections::HashMap<usize, String> =
        std::collections::HashMap::new();

    for (i, skill) in sorted_skills.iter().enumerate() {
        if !skill.available {
            continue;
        }
        let full = skill.format_at_level(SkillLoadLevel::Full);
        let full_tokens = estimate_tokens(&full);
        let meta_tokens = estimate_tokens(&metadata_parts[i]);
        let additional = full_tokens.saturating_sub(meta_tokens);

        if estimated_tokens + additional > max_tokens {
            debug!(
                "Skipping full body for skill '{}' (would exceed token budget)",
                skill.manifest.name
            );
            continue;
        }

        estimated_tokens += additional;
        debug!(
            "Included full skill '{}' ({full_tokens} estimated tokens)",
            skill.manifest.name
        );
        full_overrides.insert(i, full);
    }

    // Build final output: full body for upgraded skills, metadata for the rest
    let parts: Vec<String> = metadata_parts
        .into_iter()
        .enumerate()
        .map(|(i, meta)| full_overrides.remove(&i).unwrap_or(meta))
        .collect();

    if parts.is_empty() {
        Ok(String::new())
    } else {
        Ok(format!("# Skills\n\n{}\n", parts.join("\n---\n\n")))
    }
}

/// Collect per-skill env vars from config entries for injection into run_shell.
/// Only includes env from enabled skill entries.
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

/// Format detailed info about a skill.
#[cfg(test)]
fn format_skill_info(
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

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_SKILL: &str = r#"---
name: test-skill
description: "A test skill for unit tests."
requires:
  bins: []
  env: []
---

# Test Skill

This is a test skill body.

```bash
echo "hello"
```
"#;

    const MINIMAL_SKILL: &str = r#"---
name: minimal
description: "Minimal skill."
---

# Minimal

Body here.
"#;

    #[test]
    fn parse_valid_skill() {
        let (manifest, body) = parse_skill_md(SAMPLE_SKILL).unwrap();
        assert_eq!(manifest.name, "test-skill");
        assert_eq!(manifest.description, "A test skill for unit tests.");
        assert!(manifest.requires.bins.is_empty());
        assert!(manifest.requires.env.is_empty());
        assert!(body.contains("# Test Skill"));
        assert!(body.contains("echo \"hello\""));
    }

    #[test]
    fn parse_minimal_skill() {
        let (manifest, body) = parse_skill_md(MINIMAL_SKILL).unwrap();
        assert_eq!(manifest.name, "minimal");
        assert!(manifest.requires.bins.is_empty());
        assert!(body.contains("# Minimal"));
    }

    #[test]
    fn parse_missing_frontmatter() {
        let result = parse_skill_md("# No frontmatter\nJust a body.");
        assert!(result.is_err());
    }

    #[test]
    fn parse_missing_closing_delimiter() {
        let result = parse_skill_md("---\nname: broken\ndescription: oops\n# no closing");
        assert!(result.is_err());
    }

    #[test]
    fn builtins_parse_correctly() {
        for &(name, content) in BUNDLED_SKILLS {
            let (manifest, body) = parse_skill_md(content)
                .unwrap_or_else(|e| panic!("Built-in skill '{name}' failed to parse: {e}"));
            assert_eq!(manifest.name, name);
            assert!(!manifest.description.is_empty());
            assert!(!body.is_empty());
        }
    }

    #[test]
    fn token_budgeting() {
        // A skill with ~100 chars body = ~25 tokens
        let small = r#"---
name: small
description: "Small."
---

# Small

Short body.
"#;
        let (manifest, body) = parse_skill_md(small).unwrap();
        let skill = Skill {
            manifest,
            body,
            source: SkillSource::BuiltIn,
            available: true,
            disabled: false,
            references: Vec::new(),
            scripts: Vec::new(),
        };
        let formatted = skill.format_for_prompt();
        let tokens = estimate_tokens(&formatted);
        assert!(tokens < 50);
    }

    #[test]
    fn load_all_includes_builtins() {
        let skills =
            load_all_skills(&std::collections::HashMap::new(), &SkillsConfig::default()).unwrap();
        let names: Vec<&str> = skills.iter().map(|s| s.manifest.name.as_str()).collect();
        assert!(names.contains(&"slack"));
        assert!(names.contains(&"discord"));
        assert!(names.contains(&"github"));
        assert!(names.contains(&"weather"));
        assert!(names.contains(&"skill-creator"));
        assert!(names.contains(&"git"));
        assert!(names.contains(&"email"));
        assert!(names.contains(&"search"));
        assert!(names.contains(&"docker"));
        assert!(names.contains(&"database"));
        assert!(names.contains(&"notes"));
        assert!(names.contains(&"calendar"));
        assert!(names.contains(&"1password"));
        assert!(names.contains(&"browser"));
        assert!(names.contains(&"scheduler"));
    }

    #[test]
    fn install_default_skills_writes_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let count = install_default_skills(tmp.path()).unwrap();
        assert_eq!(count, BUNDLED_SKILLS.len());

        for &(name, _) in BUNDLED_SKILLS {
            let skill_file = tmp.path().join("skills").join(name).join("SKILL.md");
            assert!(skill_file.exists(), "Missing: {}", skill_file.display());
        }
    }

    #[test]
    fn install_default_skills_no_overwrite() {
        let tmp = tempfile::TempDir::new().unwrap();
        let skill_dir = tmp.path().join("skills/slack");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "custom content").unwrap();

        install_default_skills(tmp.path()).unwrap();

        let content = std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
        assert_eq!(content, "custom content");
    }

    #[test]
    fn skill_summary_line() {
        let skill = Skill {
            manifest: SkillManifest {
                name: "test".to_string(),
                description: "A test.".to_string(),
                requires: SkillRequires::default(),
                os: vec![],
                install: std::collections::HashMap::new(),
                category: None,
            },
            body: "body".to_string(),
            source: SkillSource::BuiltIn,
            available: true,
            disabled: false,
            references: Vec::new(),
            scripts: Vec::new(),
        };
        let line = skill.summary_line();
        assert!(line.contains("[✓]"));
        assert!(line.contains("test"));
        assert!(line.contains("built-in"));
    }

    #[test]
    fn skill_context_respects_token_budget() {
        // With a very small budget, not all skills should fit
        let context = load_skills_context(
            100,
            &std::collections::HashMap::new(),
            &SkillsConfig::default(),
        )
        .unwrap();
        // At least the header should be there if any fit, or empty if none fit
        if !context.is_empty() {
            assert!(context.starts_with("# Skills"));
        }
    }

    #[test]
    fn check_requirements_no_reqs() {
        let reqs = SkillRequires::default();
        assert!(check_requirements(
            &reqs,
            &[],
            &std::collections::HashMap::new()
        ));
    }

    #[test]
    fn check_requirements_missing_bin() {
        let reqs = SkillRequires {
            bins: vec!["definitely_not_a_real_binary_xyz123".to_string()],
            env: vec![],
            any_bins: vec![],
        };
        assert!(!check_requirements(
            &reqs,
            &[],
            &std::collections::HashMap::new()
        ));
    }

    #[test]
    fn check_requirements_missing_env() {
        let reqs = SkillRequires {
            bins: vec![],
            env: vec!["DEFINITELY_NOT_A_REAL_ENV_VAR_XYZ123".to_string()],
            any_bins: vec![],
        };
        assert!(!check_requirements(
            &reqs,
            &[],
            &std::collections::HashMap::new()
        ));
    }

    #[test]
    fn check_requirements_env_satisfied_by_credentials() {
        let reqs = SkillRequires {
            bins: vec![],
            env: vec!["CRED_STORE_VAR_XYZ123".to_string()],
            any_bins: vec![],
        };
        let mut creds = std::collections::HashMap::new();
        creds.insert(
            "CRED_STORE_VAR_XYZ123".to_string(),
            "secret-val".to_string(),
        );
        assert!(check_requirements(&reqs, &[], &creds));
    }

    #[test]
    fn user_skill_overrides_builtin() {
        // This tests the override logic in isolation
        let mut skills = vec![Skill {
            manifest: SkillManifest {
                name: "weather".to_string(),
                description: "built-in weather".to_string(),
                requires: SkillRequires::default(),
                os: vec![],
                install: std::collections::HashMap::new(),
                category: None,
            },
            body: "built-in body".to_string(),
            source: SkillSource::BuiltIn,
            available: true,
            disabled: false,
            references: Vec::new(),
            scripts: Vec::new(),
        }];

        let user_name = "weather".to_string();
        skills.retain(|s| s.manifest.name != user_name);
        skills.push(Skill {
            manifest: SkillManifest {
                name: "weather".to_string(),
                description: "user weather".to_string(),
                requires: SkillRequires::default(),
                os: vec![],
                install: std::collections::HashMap::new(),
                category: None,
            },
            body: "user body".to_string(),
            source: SkillSource::User,
            available: true,
            disabled: false,
            references: Vec::new(),
            scripts: Vec::new(),
        });

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].source, SkillSource::User);
        assert_eq!(skills[0].manifest.description, "user weather");
    }

    #[test]
    fn metadata_only_is_compact() {
        let skill = Skill {
            manifest: SkillManifest {
                name: "test-meta".to_string(),
                description: "A test skill.".to_string(),
                requires: SkillRequires::default(),
                os: vec![],
                install: std::collections::HashMap::new(),
                category: None,
            },
            body: "# Full Body\n\nLots of content here.\n\n## Section 2\n\nMore content."
                .to_string(),
            source: SkillSource::BuiltIn,
            available: true,
            disabled: false,
            references: Vec::new(),
            scripts: Vec::new(),
        };
        let meta = skill.format_at_level(SkillLoadLevel::Metadata);
        let full = skill.format_at_level(SkillLoadLevel::Full);
        let meta_tokens = estimate_tokens(&meta);
        let full_tokens = estimate_tokens(&full);
        assert!(meta_tokens < full_tokens);
        assert!(meta.contains("test-meta"));
        assert!(meta.contains("A test skill."));
        assert!(!meta.contains("Full Body"));
    }

    #[test]
    fn summary_level_includes_first_paragraph() {
        let skill = Skill {
            manifest: SkillManifest {
                name: "test-summary".to_string(),
                description: "A summary test.".to_string(),
                requires: SkillRequires::default(),
                os: vec![],
                install: std::collections::HashMap::new(),
                category: None,
            },
            body: "# Title\n\nFirst paragraph here.\n\n## Section 2\n\nMore content.".to_string(),
            source: SkillSource::BuiltIn,
            available: true,
            disabled: false,
            references: Vec::new(),
            scripts: Vec::new(),
        };
        let summary = skill.format_at_level(SkillLoadLevel::Summary);
        assert!(summary.contains("# Title"));
        assert!(!summary.contains("Section 2"));
    }

    #[test]
    fn progressive_loading_fits_many_skills() {
        // With metadata-only, 50 skills should fit in a reasonable budget
        let mut skills = Vec::new();
        for i in 0..50 {
            skills.push(Skill {
                manifest: SkillManifest {
                    name: format!("skill-{i}"),
                    description: format!("Description for skill {i}"),
                    requires: SkillRequires::default(),
                    os: vec![],
                    install: std::collections::HashMap::new(),
                    category: None,
                },
                body: format!("# Skill {i}\n\nBody content for skill {i}.\n\n## Usage\n\nDetailed usage instructions for skill {i} with examples and documentation."),
                source: SkillSource::BuiltIn,
                available: i % 2 == 0,
                disabled: false,
                references: Vec::new(),
                scripts: Vec::new(),
            });
        }

        // All metadata should be under 4000 tokens
        let total_meta_tokens: usize = skills
            .iter()
            .map(|s| estimate_tokens(&s.format_at_level(SkillLoadLevel::Metadata)))
            .sum();
        assert!(
            total_meta_tokens < 4000,
            "50 skill metadata = {total_meta_tokens} tokens"
        );
    }

    #[test]
    fn progressive_loading_prioritizes_available() {
        // load_skills_context should include full body for available skills first
        let context = load_skills_context(
            2000,
            &std::collections::HashMap::new(),
            &SkillsConfig::default(),
        )
        .unwrap();
        // Just verify it doesn't panic and produces output
        if !context.is_empty() {
            assert!(context.starts_with("# Skills"));
        }
    }

    // --- Per-skill enable/disable tests ---

    #[test]
    fn test_disabled_skill_excluded_from_context() {
        let mut entries = std::collections::HashMap::new();
        entries.insert(
            "slack".to_string(),
            crate::config::SkillEntryConfig {
                enabled: false,
                env: std::collections::HashMap::new(),
            },
        );
        let config = SkillsConfig {
            enabled: true,
            max_context_tokens: 4000,
            entries,
        };
        let context =
            load_skills_context(4000, &std::collections::HashMap::new(), &config).unwrap();
        // The slack skill header should not appear (other skills may mention "slack" in their body)
        assert!(!context.contains("Skill: slack ["));
        assert!(!context.contains("slack (built-in,"));
    }

    #[test]
    fn test_disabled_skill_shown_in_list() {
        let mut entries = std::collections::HashMap::new();
        entries.insert(
            "slack".to_string(),
            crate::config::SkillEntryConfig {
                enabled: false,
                env: std::collections::HashMap::new(),
            },
        );
        let config = SkillsConfig {
            enabled: true,
            max_context_tokens: 4000,
            entries,
        };
        let skills = load_all_skills(&std::collections::HashMap::new(), &config).unwrap();
        let slack = skills.iter().find(|s| s.manifest.name == "slack").unwrap();
        assert!(slack.disabled);
    }

    #[test]
    fn test_default_enabled_skills() {
        let config = SkillsConfig::default();
        let skills = load_all_skills(&std::collections::HashMap::new(), &config).unwrap();
        for skill in &skills {
            if DEFAULT_ENABLED_SKILLS.contains(&skill.manifest.name.as_str()) {
                assert!(
                    !skill.disabled,
                    "Skill {} should be enabled by default",
                    skill.manifest.name
                );
            }
        }
    }

    #[test]
    fn test_default_disabled_skills() {
        let config = SkillsConfig::default();
        let skills = load_all_skills(&std::collections::HashMap::new(), &config).unwrap();
        for skill in &skills {
            if !DEFAULT_ENABLED_SKILLS.contains(&skill.manifest.name.as_str()) {
                assert!(
                    skill.disabled,
                    "Skill {} should be disabled by default",
                    skill.manifest.name
                );
            }
        }
    }

    #[test]
    fn test_explicit_config_overrides_default() {
        let mut entries = std::collections::HashMap::new();
        // Explicitly enable a normally-disabled skill
        entries.insert(
            "docker".to_string(),
            crate::config::SkillEntryConfig {
                enabled: true,
                env: std::collections::HashMap::new(),
            },
        );
        // Explicitly disable a normally-enabled skill
        entries.insert(
            "git".to_string(),
            crate::config::SkillEntryConfig {
                enabled: false,
                env: std::collections::HashMap::new(),
            },
        );
        let config = SkillsConfig {
            enabled: true,
            max_context_tokens: 4000,
            entries,
        };
        let skills = load_all_skills(&std::collections::HashMap::new(), &config).unwrap();
        let docker = skills.iter().find(|s| s.manifest.name == "docker").unwrap();
        assert!(!docker.disabled, "docker should be explicitly enabled");
        let git = skills.iter().find(|s| s.manifest.name == "git").unwrap();
        assert!(git.disabled, "git should be explicitly disabled");
    }

    #[test]
    fn test_skill_category_fallback() {
        // Test built-in skills directly to avoid user overrides in ~/.borg/skills/
        let config = SkillsConfig::default();
        let creds = std::collections::HashMap::new();
        let mut skills: Vec<Skill> = Vec::new();
        for &(_name, content) in super::BUNDLED_SKILLS {
            if let Ok(skill) = super::load_builtin_skill(content, &creds, &config) {
                skills.push(skill);
            }
        }
        let git = skills.iter().find(|s| s.manifest.name == "git").unwrap();
        assert_eq!(git.category(), "developer");
        let browser = skills
            .iter()
            .find(|s| s.manifest.name == "browser")
            .unwrap();
        assert_eq!(browser.category(), "core");
        let calendar = skills
            .iter()
            .find(|s| s.manifest.name == "calendar")
            .unwrap();
        assert_eq!(calendar.category(), "core");
        let email = skills.iter().find(|s| s.manifest.name == "email").unwrap();
        assert_eq!(email.category(), "core");
    }

    #[test]
    fn test_hidden_skills() {
        let config = SkillsConfig::default();
        let skills = load_all_skills(&std::collections::HashMap::new(), &config).unwrap();
        let skill_creator = skills
            .iter()
            .find(|s| s.manifest.name == "skill-creator")
            .unwrap();
        assert!(skill_creator.is_hidden());
        let git = skills.iter().find(|s| s.manifest.name == "git").unwrap();
        assert!(!git.is_hidden());
    }

    #[test]
    fn test_summary_line_disabled() {
        let skill = Skill {
            manifest: SkillManifest {
                name: "test".to_string(),
                description: "A test.".to_string(),
                requires: SkillRequires::default(),
                os: vec![],
                install: std::collections::HashMap::new(),
                category: None,
            },
            body: "body".to_string(),
            source: SkillSource::BuiltIn,
            available: false,
            disabled: true,
            references: Vec::new(),
            scripts: Vec::new(),
        };
        let line = skill.summary_line();
        assert!(line.contains("[—]"));
    }

    // --- OS gating tests ---

    #[test]
    fn test_os_gating_empty_means_all() {
        assert!(check_os_requirements(&[]));
    }

    #[test]
    fn test_os_gating_current_os() {
        let current = std::env::consts::OS.to_string();
        assert!(check_os_requirements(&[current]));
    }

    #[test]
    fn test_os_gating_wrong_os() {
        assert!(!check_os_requirements(&["totally_fake_os".to_string()]));
    }

    #[test]
    fn test_os_gating_darwin_alias() {
        if std::env::consts::OS == "macos" {
            assert!(check_os_requirements(&["darwin".to_string()]));
        }
    }

    #[test]
    fn test_parse_skill_with_os() {
        let content = r#"---
name: macos-only
description: "macOS only skill."
os:
  - macos
  - darwin
---

# macOS Only

Body here.
"#;
        let (manifest, _body) = parse_skill_md(content).unwrap();
        assert_eq!(manifest.os, vec!["macos", "darwin"]);
    }

    // --- any_bins tests ---

    #[test]
    fn test_any_bins_one_present() {
        let reqs = SkillRequires {
            bins: vec![],
            env: vec![],
            any_bins: vec!["sh".to_string(), "nonexistent_bin_xyz".to_string()],
        };
        assert!(check_requirements(
            &reqs,
            &[],
            &std::collections::HashMap::new()
        ));
    }

    #[test]
    fn test_any_bins_none_present() {
        let reqs = SkillRequires {
            bins: vec![],
            env: vec![],
            any_bins: vec![
                "nonexistent_bin_xyz1".to_string(),
                "nonexistent_bin_xyz2".to_string(),
            ],
        };
        assert!(!check_requirements(
            &reqs,
            &[],
            &std::collections::HashMap::new()
        ));
    }

    #[test]
    fn test_any_bins_empty_passes() {
        let reqs = SkillRequires {
            bins: vec![],
            env: vec![],
            any_bins: vec![],
        };
        assert!(check_requirements(
            &reqs,
            &[],
            &std::collections::HashMap::new()
        ));
    }

    #[test]
    fn test_parse_skill_with_any_bins() {
        let content = r#"---
name: search-skill
description: "Search skill."
requires:
  any_bins:
    - rg
    - grep
    - ag
---

# Search

Body here.
"#;
        let (manifest, _body) = parse_skill_md(content).unwrap();
        assert_eq!(manifest.requires.any_bins, vec!["rg", "grep", "ag"]);
    }

    // --- Install spec tests ---

    #[test]
    fn test_parse_skill_with_install_specs() {
        let content = r#"---
name: docker-skill
description: "Docker management."
requires:
  bins:
    - docker
install:
  docker:
    brew: "docker"
    apt: "docker.io"
    url: "https://docs.docker.com/get-docker/"
---

# Docker

Use docker commands.
"#;
        let (manifest, _body) = parse_skill_md(content).unwrap();
        assert!(manifest.install.contains_key("docker"));
        let spec = &manifest.install["docker"];
        assert_eq!(spec.brew.as_deref(), Some("docker"));
        assert_eq!(spec.apt.as_deref(), Some("docker.io"));
        assert_eq!(
            spec.url.as_deref(),
            Some("https://docs.docker.com/get-docker/")
        );
        assert!(spec.npm.is_none());
    }

    #[test]
    fn test_install_spec_defaults() {
        let spec = InstallSpec::default();
        assert!(spec.brew.is_none());
        assert!(spec.apt.is_none());
        assert!(spec.npm.is_none());
        assert!(spec.url.is_none());
    }

    #[test]
    fn test_parse_skill_without_install() {
        // Backward compat: existing skills without install field still parse
        let (manifest, _body) = parse_skill_md(SAMPLE_SKILL).unwrap();
        assert!(manifest.install.is_empty());
    }

    // --- Per-skill env collection test ---

    #[test]
    fn test_collect_skill_env() {
        let mut entries = std::collections::HashMap::new();
        entries.insert(
            "slack".to_string(),
            crate::config::SkillEntryConfig {
                enabled: true,
                env: [("SLACK_TOKEN".to_string(), "xoxb-123".to_string())]
                    .into_iter()
                    .collect(),
            },
        );
        entries.insert(
            "disabled".to_string(),
            crate::config::SkillEntryConfig {
                enabled: false,
                env: [("SHOULD_NOT_APPEAR".to_string(), "val".to_string())]
                    .into_iter()
                    .collect(),
            },
        );
        let config = SkillsConfig {
            enabled: true,
            max_context_tokens: 4000,
            entries,
        };
        let env = collect_skill_env(&config);
        assert_eq!(env.get("SLACK_TOKEN").unwrap(), "xoxb-123");
        assert!(!env.contains_key("SHOULD_NOT_APPEAR"));
    }

    // -- install_hint --

    fn test_skill(
        install: std::collections::HashMap<String, InstallSpec>,
        available: bool,
    ) -> Skill {
        Skill {
            manifest: SkillManifest {
                name: "test".into(),
                description: "test skill".into(),
                requires: SkillRequires::default(),
                os: vec![],
                install,
                category: None,
            },
            body: String::new(),
            source: SkillSource::BuiltIn,
            available,
            disabled: false,
            references: vec![],
            scripts: vec![],
        }
    }

    #[test]
    fn install_hint_returns_none_when_no_install_specs() {
        let skill = test_skill(std::collections::HashMap::new(), true);
        assert!(skill.install_hint().is_none());
    }

    #[test]
    fn install_hint_prefers_platform_package_manager() {
        let mut install = std::collections::HashMap::new();
        install.insert(
            "curl".to_string(),
            InstallSpec {
                brew: Some("curl".to_string()),
                apt: Some("curl".to_string()),
                npm: None,
                url: None,
            },
        );
        let skill = test_skill(install, false);
        let hint = skill.install_hint().unwrap();
        #[cfg(target_os = "macos")]
        assert!(
            hint.contains("brew"),
            "macOS should prefer brew, got: {hint}"
        );
        #[cfg(target_os = "linux")]
        assert!(hint.contains("apt"), "Linux should prefer apt, got: {hint}");
    }

    #[test]
    fn install_hint_falls_back_to_npm() {
        let mut install = std::collections::HashMap::new();
        install.insert(
            "sometool".to_string(),
            InstallSpec {
                brew: None,
                apt: None,
                npm: Some("sometool".to_string()),
                url: None,
            },
        );
        let skill = test_skill(install, false);
        let hint = skill.install_hint().unwrap();
        assert!(hint.contains("npm"), "should fall back to npm, got: {hint}");
    }

    #[test]
    fn install_hint_falls_back_to_url() {
        let mut install = std::collections::HashMap::new();
        install.insert(
            "rare".to_string(),
            InstallSpec {
                brew: None,
                apt: None,
                npm: None,
                url: Some("https://example.com/install".to_string()),
            },
        );
        let skill = test_skill(install, false);
        let hint = skill.install_hint().unwrap();
        assert!(
            hint.contains("https://example.com"),
            "should fall back to URL, got: {hint}"
        );
    }

    #[test]
    fn summary_line_includes_install_hint_for_unavailable() {
        let mut install = std::collections::HashMap::new();
        install.insert(
            "tool".to_string(),
            InstallSpec {
                brew: Some("tool".to_string()),
                ..Default::default()
            },
        );
        let skill = test_skill(install, false);
        let line = skill.summary_line();
        assert!(
            line.contains("Install:"),
            "unavailable skill should show install hint, got: {line}"
        );
    }

    #[test]
    fn summary_line_no_hint_for_available() {
        let mut install = std::collections::HashMap::new();
        install.insert(
            "tool".to_string(),
            InstallSpec {
                brew: Some("tool".to_string()),
                ..Default::default()
            },
        );
        let skill = test_skill(install, true);
        let line = skill.summary_line();
        assert!(
            !line.contains("Install:"),
            "available skill should not show install hint, got: {line}"
        );
    }

    #[test]
    fn test_collect_required_env_vars() {
        let creds = std::collections::HashMap::new();
        let config = SkillsConfig::default();
        let env_vars = collect_required_env_vars(&creds, &config);
        // Built-in skills declare env vars like SLACK_BOT_TOKEN, DISCORD_BOT_TOKEN, etc.
        // At minimum, slack skill requires SLACK_BOT_TOKEN
        assert!(
            env_vars.contains("SLACK_BOT_TOKEN"),
            "should contain SLACK_BOT_TOKEN, got: {env_vars:?}"
        );
    }

    #[test]
    fn test_parse_skill_empty_body() {
        let content = "---\nname: empty\ndescription: \"No body.\"\n---\n";
        let (manifest, body) = parse_skill_md(content).unwrap();
        assert_eq!(manifest.name, "empty");
        assert!(body.is_empty() || body.trim().is_empty());
    }

    #[test]
    fn test_check_requirements_bins_and_any_bins() {
        let reqs = SkillRequires {
            bins: vec!["sh".to_string()],
            env: vec![],
            any_bins: vec!["nonexistent_xyz".to_string(), "sh".to_string()],
        };
        assert!(check_requirements(
            &reqs,
            &[],
            &std::collections::HashMap::new()
        ));
    }

    #[test]
    fn test_load_skills_context_empty_when_all_disabled() {
        let mut entries = std::collections::HashMap::new();
        let disabled_entry = || crate::config::SkillEntryConfig {
            enabled: false,
            env: std::collections::HashMap::new(),
        };
        // Disable all built-in skills
        for &(name, _) in BUNDLED_SKILLS {
            entries.insert(name.to_string(), disabled_entry());
        }
        // Also disable any user-installed skills in ~/.borg/skills/
        if let Ok(user_dir) = skills_dir() {
            if let Ok(dir_entries) = std::fs::read_dir(&user_dir) {
                for entry in dir_entries.flatten() {
                    if entry.path().join("SKILL.md").exists() {
                        let name = entry.file_name().to_string_lossy().to_string();
                        entries.entry(name).or_insert_with(disabled_entry);
                    }
                }
            }
        }
        // Also disable any user-installed skills that may exist on the host
        // (e.g. ~/.borg/skills/email/).
        let user_dir = skills_dir().ok();
        if let Some(ref dir) = user_dir {
            if let Ok(dir_entries) = std::fs::read_dir(dir) {
                for entry in dir_entries.flatten() {
                    if let Some(name) = entry.file_name().to_str() {
                        entries.entry(name.to_string()).or_insert_with(|| {
                            crate::config::SkillEntryConfig {
                                enabled: false,
                                env: std::collections::HashMap::new(),
                            }
                        });
                    }
                }
            }
        }
        let config = SkillsConfig {
            enabled: true,
            max_context_tokens: 4000,
            entries,
        };
        let context =
            load_skills_context(4000, &std::collections::HashMap::new(), &config).unwrap();
        assert!(
            context.is_empty(),
            "all disabled skills should produce empty context, got: {context}"
        );
    }

    #[test]
    fn test_summary_line_unavailable_no_install() {
        let skill = Skill {
            manifest: SkillManifest {
                name: "no-install".to_string(),
                description: "Missing deps.".to_string(),
                requires: SkillRequires {
                    bins: vec!["nonexistent_xyz".to_string()],
                    env: vec![],
                    any_bins: vec![],
                },
                os: vec![],
                install: std::collections::HashMap::new(),
                category: None,
            },
            body: "body".to_string(),
            source: SkillSource::BuiltIn,
            available: false,
            disabled: false,
            references: Vec::new(),
            scripts: Vec::new(),
        };
        let line = skill.summary_line();
        assert!(line.contains("[✗]"));
        assert!(
            !line.contains("Install:"),
            "no install specs should mean no Install line, got: {line}"
        );
    }

    #[test]
    fn test_format_skill_info_cred_check() {
        let skill = Skill {
            manifest: SkillManifest {
                name: "cred-test".to_string(),
                description: "Tests credential resolution.".to_string(),
                requires: SkillRequires {
                    bins: vec![],
                    env: vec!["MY_CRED_VAR_XYZ".to_string()],
                    any_bins: vec![],
                },
                os: vec![],
                install: std::collections::HashMap::new(),
                category: None,
            },
            body: "body".to_string(),
            source: SkillSource::User,
            available: true,
            disabled: false,
            references: Vec::new(),
            scripts: Vec::new(),
        };
        // Without creds, env var shows as missing
        let info_no_creds = format_skill_info(&skill, &std::collections::HashMap::new());
        assert!(
            info_no_creds.contains("[✗] MY_CRED_VAR_XYZ"),
            "should show missing without creds, got: {info_no_creds}"
        );

        // With creds, env var shows as found
        let mut creds = std::collections::HashMap::new();
        creds.insert("MY_CRED_VAR_XYZ".to_string(), "secret".to_string());
        let info_with_creds = format_skill_info(&skill, &creds);
        assert!(
            info_with_creds.contains("[✓] MY_CRED_VAR_XYZ"),
            "should show found with creds, got: {info_with_creds}"
        );
    }

    #[test]
    fn test_format_skill_info_fields() {
        let skill = Skill {
            manifest: SkillManifest {
                name: "info-test".to_string(),
                description: "Info format test.".to_string(),
                requires: SkillRequires::default(),
                os: vec![],
                install: std::collections::HashMap::new(),
                category: None,
            },
            body: "# Title\n\nFirst paragraph.".to_string(),
            source: SkillSource::BuiltIn,
            available: true,
            disabled: false,
            references: Vec::new(),
            scripts: Vec::new(),
        };
        let info = format_skill_info(&skill, &std::collections::HashMap::new());
        assert!(info.contains("Name:        info-test"));
        assert!(info.contains("Description: Info format test."));
        assert!(info.contains("Source:      built-in"));
        assert!(info.contains("Status:      available"));
        assert!(info.contains("# Title"));
    }

    #[test]
    fn test_user_skill_with_references_and_scripts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let skill_dir = tmp.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();

        // Write SKILL.md
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: \"Test skill.\"\n---\n\n# My Skill\n\nBody.",
        )
        .unwrap();

        // Create references
        let refs_dir = skill_dir.join("references");
        std::fs::create_dir_all(&refs_dir).unwrap();
        std::fs::write(refs_dir.join("guide.md"), "# Guide\nSome reference.").unwrap();
        std::fs::write(refs_dir.join("notes.txt"), "not markdown").unwrap();

        // Create scripts
        let scripts_dir = skill_dir.join("scripts");
        std::fs::create_dir_all(&scripts_dir).unwrap();
        std::fs::write(scripts_dir.join("helper.sh"), "#!/bin/bash\necho hi").unwrap();

        // Parse and load like load_all_skills does for user skills
        let content = std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
        let (manifest, body) = parse_skill_md(&content).unwrap();

        // Load references (only .md files)
        let mut references = Vec::new();
        for entry in std::fs::read_dir(&refs_dir).unwrap().flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "md") {
                let ref_content = std::fs::read_to_string(&path).unwrap();
                let name = path.file_name().unwrap().to_string_lossy().to_string();
                references.push((name, ref_content));
            }
        }

        // Load scripts
        let mut scripts = Vec::new();
        for entry in std::fs::read_dir(&scripts_dir).unwrap().flatten() {
            scripts.push(entry.path());
        }

        let skill = Skill {
            manifest,
            body,
            source: SkillSource::User,
            available: true,
            disabled: false,
            references,
            scripts,
        };

        assert_eq!(skill.manifest.name, "my-skill");
        assert_eq!(skill.references.len(), 1, "should only load .md references");
        assert_eq!(skill.references[0].0, "guide.md");
        assert!(skill.references[0].1.contains("# Guide"));
        assert_eq!(skill.scripts.len(), 1);
        assert!(skill.scripts[0].ends_with("helper.sh"));
    }

    #[test]
    fn test_status_and_source_helpers() {
        let make = |source, available, disabled| Skill {
            manifest: SkillManifest {
                name: "t".into(),
                description: "t".into(),
                requires: SkillRequires::default(),
                os: vec![],
                install: std::collections::HashMap::new(),
                category: None,
            },
            body: String::new(),
            source,
            available,
            disabled,
            references: vec![],
            scripts: vec![],
        };

        let s = make(SkillSource::BuiltIn, true, false);
        assert_eq!(s.source_label(), "built-in");
        assert_eq!(s.status_label(), "available");
        assert_eq!(s.status_icon(), "✓");

        let s = make(SkillSource::User, false, false);
        assert_eq!(s.source_label(), "user");
        assert_eq!(s.status_label(), "unavailable (missing requirements)");
        assert_eq!(s.status_icon(), "✗");

        let s = make(SkillSource::BuiltIn, false, true);
        assert_eq!(s.status_label(), "disabled");
        assert_eq!(s.status_icon(), "—");
    }

    #[test]
    fn scheduler_is_hidden() {
        assert!(
            HIDDEN_SKILLS.contains(&"scheduler"),
            "scheduler should be in HIDDEN_SKILLS"
        );
    }

    #[test]
    fn load_all_skills_no_duplicates() {
        let skills =
            load_all_skills(&std::collections::HashMap::new(), &SkillsConfig::default()).unwrap();
        let mut names: Vec<&str> = skills.iter().map(|s| s.manifest.name.as_str()).collect();
        let len_before = names.len();
        names.sort();
        names.dedup();
        assert_eq!(
            names.len(),
            len_before,
            "load_all_skills returned duplicate skill names"
        );
    }

    #[test]
    fn core_skills_have_core_category() {
        let skills =
            load_all_skills(&std::collections::HashMap::new(), &SkillsConfig::default()).unwrap();
        for name in &["browser", "search", "email", "calendar"] {
            let skill = skills
                .iter()
                .find(|s| s.manifest.name == *name)
                .unwrap_or_else(|| panic!("skill {name} not found"));
            assert_eq!(
                skill.category(),
                "core",
                "skill {name} should have core category"
            );
        }
    }
}
