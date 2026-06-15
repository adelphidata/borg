//! Skill manifest types and parsing: the `SkillManifest` DTO (parsed SKILL.md
//! frontmatter), the loaded `Skill` and its display/formatting methods, the
//! `parse_skill_md` frontmatter parser, and runtime-requirement checks. This is
//! the foundation layer — depended on by the loader and context modules.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::trace;

use super::{HIDDEN_SKILLS, SKILL_CATEGORY_MAP};

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

/// Legacy `prerequisites` block from agentskills.io frontmatter. Normalized
/// into `SkillRequires` at parse time so the rest of the loader doesn't have
/// to know about two shapes.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct PrerequisitesLegacy {
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub env_vars: Vec<String>,
}

/// Parsed YAML frontmatter from a SKILL.md file.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SkillManifest {
    /// Unique skill identifier.
    pub name: String,
    /// Human-readable description of what the skill does.
    pub description: String,
    /// Short task phrases that should trigger this skill when the user asks.
    #[serde(default)]
    pub triggers: Vec<String>,
    /// Runtime requirements (binaries, env vars).
    #[serde(default)]
    pub requires: SkillRequires,
    /// Supported operating systems (empty means all). Accepts `platforms` as an
    /// alias for cross-compatibility with the agentskills.io standard.
    #[serde(default, alias = "platforms")]
    pub os: Vec<String>,
    /// Per-dependency install instructions keyed by dependency name.
    #[serde(default)]
    pub install: std::collections::HashMap<String, InstallSpec>,
    /// Plugin category for unified UI grouping (e.g., "developer", "utilities").
    #[serde(default)]
    pub category: Option<String>,
    /// Semantic version (agentskills.io standard, optional).
    #[serde(default)]
    pub version: Option<String>,
    /// Author / creator attribution (agentskills.io standard, optional).
    #[serde(default)]
    pub author: Option<String>,
    /// SPDX license identifier (agentskills.io standard, optional).
    #[serde(default)]
    pub license: Option<String>,
    /// Compatibility notes (agentskills.io standard, optional).
    #[serde(default)]
    pub compatibility: Option<String>,
    /// Arbitrary vendor metadata. agentskills.io convention nests under
    /// `metadata.<vendor>.*`; we just round-trip it for now.
    #[serde(default)]
    pub metadata: Option<serde_yaml::Value>,
    /// Legacy agentskills.io requirement block. Folded into `requires` during
    /// parsing when `requires` is absent; do not consume directly.
    #[serde(default)]
    pub prerequisites: Option<PrerequisitesLegacy>,
}

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
    fn trigger_summary(&self) -> Option<String> {
        if self.manifest.triggers.is_empty() {
            None
        } else {
            Some(self.manifest.triggers.join(", "))
        }
    }

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
                let mut meta = format!(
                    "### {} ({}, {}) — {}",
                    self.manifest.name, source, status, self.manifest.description
                );
                if let Some(triggers) = self.trigger_summary() {
                    meta.push_str(&format!("\nTriggers: {triggers}"));
                }
                meta
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
        if let Some(triggers) = self.trigger_summary() {
            line.push_str(&format!("\n    Triggers: {triggers}"));
        }
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

    let mut manifest: SkillManifest =
        serde_yaml::from_str(yaml_str).context("Failed to parse SKILL.md YAML frontmatter")?;

    // Fold legacy `prerequisites` (agentskills.io frontmatter) into `requires`
    // when no explicit `requires` was provided. Explicit requires always wins:
    // a SKILL.md that sets both is treating `requires` as the source of truth
    // and likely shipped `prerequisites` for cross-tool compatibility.
    if let Some(prereqs) = manifest.prerequisites.take() {
        let requires_empty = manifest.requires.bins.is_empty()
            && manifest.requires.env.is_empty()
            && manifest.requires.any_bins.is_empty();
        if requires_empty {
            manifest.requires.bins = prereqs.commands;
            manifest.requires.env = prereqs.env_vars;
        }
    }

    Ok((manifest, body))
}

pub(super) fn check_os_requirements(os: &[String]) -> bool {
    if os.is_empty() {
        return true;
    }
    let current = std::env::consts::OS;
    os.iter()
        .any(|o| o == current || (o == "darwin" && current == "macos"))
}

pub(super) fn check_requirements(
    requires: &SkillRequires,
    os: &[String],
    resolved_creds: &std::collections::HashMap<String, String>,
) -> bool {
    if !check_os_requirements(os) {
        trace!("Skill requirement missing: unsupported OS (need {:?})", os);
        return false;
    }
    for bin in &requires.bins {
        if which::which(bin).is_err() {
            trace!("Skill requirement missing: binary '{bin}'");
            return false;
        }
    }
    if !requires.any_bins.is_empty() && !requires.any_bins.iter().any(|b| which::which(b).is_ok()) {
        trace!(
            "Skill requirement missing: none of {:?} found",
            requires.any_bins
        );
        return false;
    }
    for var in &requires.env {
        if std::env::var(var).is_err() && !resolved_creds.contains_key(var) {
            trace!("Skill requirement missing: env var '{var}'");
            return false;
        }
    }
    true
}
