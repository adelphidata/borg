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
    /// Loaded from the current workspace (`.borg/skills/`, `.agents/skills/`).
    ///
    /// Same trust posture as `User` — both are author-controlled files that
    /// go through the security scan — but the origin matters to the operator:
    /// a project skill arrived with a `git pull`, not by their own hand.
    Project,
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

/// A package manager that can put a missing binary on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManager {
    Brew,
    Apt,
    Npm,
}

/// One package-manager install, ready to show or to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallCommand {
    pub manager: PackageManager,
    pub package: String,
}

impl InstallCommand {
    /// The program and its arguments. Never a shell string: the package
    /// name comes from a skill's frontmatter.
    pub fn argv(&self) -> (&'static str, Vec<String>) {
        let package = self.package.clone();
        match self.manager {
            PackageManager::Brew => ("brew", vec!["install".into(), package]),
            PackageManager::Apt => (
                "sudo",
                vec!["apt".into(), "install".into(), "-y".into(), package],
            ),
            PackageManager::Npm => ("npm", vec!["install".into(), "-g".into(), package]),
        }
    }

    /// The command as someone would type it in a terminal.
    pub fn display(&self) -> String {
        let (program, args) = self.argv();
        format!("{program} {}", args.join(" "))
    }

    /// Whether it runs with no one at a terminal: apt asks for a sudo
    /// password, brew and npm do not.
    pub fn unattended(&self) -> bool {
        self.manager != PackageManager::Apt
    }
}

/// A name a package manager reads as a package, never as a flag.
fn is_package_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@/._+-".contains(c))
}

impl InstallSpec {
    /// The install command for this machine: brew on macOS; apt, then brew,
    /// on Linux; npm anywhere as the fallback. `None` when the spec names no
    /// manager this machine uses, which leaves `url` as the only way.
    pub fn command(&self) -> Option<InstallCommand> {
        let order: &[PackageManager] = if cfg!(target_os = "macos") {
            &[PackageManager::Brew, PackageManager::Npm]
        } else if cfg!(target_os = "linux") {
            &[
                PackageManager::Apt,
                PackageManager::Brew,
                PackageManager::Npm,
            ]
        } else {
            &[PackageManager::Npm]
        };
        order.iter().find_map(|&manager| {
            let package = match manager {
                PackageManager::Brew => self.brew.as_deref(),
                PackageManager::Apt => self.apt.as_deref(),
                PackageManager::Npm => self.npm.as_deref(),
            }?;
            is_package_name(package).then(|| InstallCommand {
                manager,
                package: package.to_string(),
            })
        })
    }
}

/// One requirement a skill is still waiting on before it can run here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillNeed {
    /// Runs only on these systems; nothing installed here changes that.
    Os(Vec<String>),
    /// Any one of these binaries on `$PATH`: one name for a `bins` entry,
    /// the whole list for `any_bins`.
    Bin(Vec<String>),
    /// An environment variable, or a `[credentials]` entry of that name.
    Env(String),
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
    /// Model this skill's work should run on, when it is worth running
    /// somewhere other than the session's model — a bulk extraction that only
    /// needs a cheap model, or a proof step that needs the strongest one.
    ///
    /// Advisory, not a switch: the session's own model never changes
    /// mid-turn. It is surfaced to the agent, which delegates by passing the
    /// value to `spawn_agent`'s `model` override. A blank value is dropped at
    /// parse time so an empty `model:` key cannot produce a request for the
    /// model named "".
    #[serde(default)]
    pub model: Option<String>,
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

/// Per-entry caps for the metadata (catalog) rendering level.
///
/// The catalog shares one global token budget, and entries are admitted in
/// relevance order until it runs out. Without per-entry caps, a single skill with
/// a 400-word description or fifty triggers eats a large slice of that budget and
/// pushes later skills out of the catalog entirely. Only the *catalog* view is
/// capped — `Summary` and `Full` still render the manifest verbatim.
const MAX_CATALOG_DESCRIPTION_CHARS: usize = 320;

/// Authoring limit for a skill's `description:`, enforced when a skill is
/// written. Deliberately the same number as the catalog cap: a description
/// longer than what the catalog renders is text no model ever reads.
pub const MAX_SKILL_DESCRIPTION_CHARS: usize = MAX_CATALOG_DESCRIPTION_CHARS;

/// Triggers listed per skill in the catalog. Past a handful they stop helping
/// selection and just spend prompt budget on every turn.
const MAX_CATALOG_TRIGGERS: usize = 8;
const TRUNCATION_SUFFIX: &str = "…";

/// Truncate `text` to `max_chars` on a char boundary, appending an ellipsis.
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    // Trim a dangling partial word so the ellipsis reads cleanly, but only when
    // the break point is late enough that we aren't throwing most of the entry
    // away. `rfind` yields a *byte* offset, so the "late enough" test has to be
    // made in chars — for CJK or accented text a byte offset runs 2–3× the char
    // index and the guard would pass no matter how early the space sits.
    if let Some(last_space) = out.rfind(' ') {
        let chars_kept = out[..last_space].chars().count();
        if chars_kept >= max_chars / 2 {
            out.truncate(last_space);
        }
    }
    out.push_str(TRUNCATION_SUFFIX);
    out
}

impl Skill {
    fn trigger_summary(&self) -> Option<String> {
        if self.manifest.triggers.is_empty() {
            None
        } else {
            Some(self.manifest.triggers.join(", "))
        }
    }

    /// [`Self::trigger_summary`] capped for the catalog view: at most
    /// [`MAX_CATALOG_TRIGGERS`] entries, with a `+N more` tail.
    fn catalog_trigger_summary(&self) -> Option<String> {
        let triggers = &self.manifest.triggers;
        if triggers.is_empty() {
            return None;
        }
        if triggers.len() <= MAX_CATALOG_TRIGGERS {
            return Some(triggers.join(", "));
        }
        let shown = triggers[..MAX_CATALOG_TRIGGERS].join(", ");
        let extra = triggers.len() - MAX_CATALOG_TRIGGERS;
        Some(format!("{shown} (+{extra} more)"))
    }

    /// Returns a display label for the skill's source.
    pub fn source_label(&self) -> &'static str {
        match self.source {
            SkillSource::BuiltIn => "built-in",
            SkillSource::User => "user",
            SkillSource::Project => "project",
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

    /// One line telling the agent how to honour a `model:` declaration, or
    /// empty when the skill did not declare one.
    ///
    /// Phrased as an instruction with the mechanism named, because the model
    /// cannot switch its own model mid-turn — delegating is the only way to
    /// act on this, and a bare "Model: X" line reads as trivia it can ignore.
    fn model_delegation_note(&self) -> String {
        match &self.manifest.model {
            Some(model) => format!(
                "\nThis skill declares model `{model}`. Run its work through \
                 `spawn_agent` with `model` set to `{model}` rather than doing \
                 it inline, unless the user asked for something else."
            ),
            None => String::new(),
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
                let description =
                    truncate_chars(&self.manifest.description, MAX_CATALOG_DESCRIPTION_CHARS);
                let mut meta = format!(
                    "### {} ({}, {}) — {}",
                    self.manifest.name, source, status, description
                );
                if let Some(triggers) = self.catalog_trigger_summary() {
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
                    "## Skill: {} [{}, {}]{}\n\n{}\n",
                    self.manifest.name,
                    source,
                    status,
                    self.model_delegation_note(),
                    self.body
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

    /// The install commands for this machine, one per dependency in the
    /// `install` map, with a link where no package manager fits.
    pub fn install_hint(&self) -> Option<String> {
        let mut keys: Vec<_> = self.manifest.install.keys().collect();
        keys.sort();
        let hints: Vec<String> = keys
            .into_iter()
            .filter_map(|key| {
                let spec = &self.manifest.install[key];
                spec.command()
                    .map(|c| c.display())
                    .or_else(|| spec.url.as_deref().map(|u| format!("See: {u}")))
            })
            .collect();
        (!hints.is_empty()).then(|| hints.join("; "))
    }

    /// What this skill still needs before it can run here, probed now.
    pub fn needs(
        &self,
        resolved_creds: &std::collections::HashMap<String, String>,
    ) -> Vec<SkillNeed> {
        unmet_requirements(&self.manifest.requires, &self.manifest.os, resolved_creds)
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

    let mut manifest: SkillManifest = match serde_yaml::from_str(yaml_str) {
        Ok(manifest) => manifest,
        Err(original_err) => {
            // Some third-party SKILL.md files write frontmatter values as prose
            // that is not valid YAML — e.g. `description: Build for AWS: ECS`
            // (the second `: ` reads as nested structure) or `tags: [a,@b]`.
            // Try a line-oriented repair that quotes the offending scalar, then
            // retry. If the repaired text still fails, surface the ORIGINAL error
            // so genuinely-broken frontmatter is not masked.
            match repair_frontmatter_scalar_fields(yaml_str)
                .and_then(|repaired| serde_yaml::from_str(&repaired).ok())
            {
                Some(manifest) => manifest,
                None => {
                    return Err(original_err).context("Failed to parse SKILL.md YAML frontmatter");
                }
            }
        }
    };

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

    // A blank `model:` is the author leaving the key in place, not a request
    // for the model named "". Drop it here so nothing downstream has to
    // re-check emptiness before passing it to `spawn_agent`.
    if let Some(model) = &manifest.model {
        if model.trim().is_empty() {
            manifest.model = None;
        } else {
            manifest.model = Some(model.trim().to_string());
        }
    }

    Ok((manifest, body))
}

/// Repair common non-YAML prose in SKILL.md frontmatter scalar values so the
/// manifest still loads. `serde_yaml` rejects values like
/// `description: Build for AWS: ECS` or `tags: [a,@b]` before it can ignore
/// unknown fields, so the repair happens before a retry. This is a line-oriented
/// pass that single-quotes offending scalar values while leaving `|`/`>` block
/// scalars and already-quoted values untouched. Returns `Some` only when at
/// least one line changed (an unchanged result means the failure was unrelated).
fn repair_frontmatter_scalar_fields(frontmatter: &str) -> Option<String> {
    let mut changed = false;
    // When `Some`, we're inside a `|`/`>` block scalar body opened at this indent.
    let mut block_scalar_indent: Option<usize> = None;
    let mut out: Vec<String> = Vec::new();

    for line in frontmatter.lines() {
        let indent = line.chars().take_while(|c| *c == ' ').count();

        // Copy block-scalar body lines (indented deeper, or blank) verbatim.
        if let Some(block_indent) = block_scalar_indent {
            if line.trim().is_empty() || indent > block_indent {
                out.push(line.to_string());
                continue;
            }
            block_scalar_indent = None;
        }

        let Some((key, value)) = line.split_once(':') else {
            out.push(line.to_string());
            continue;
        };
        // Require the `key: value` shape (a space after the colon). A colon with
        // no following whitespace belongs to a URL or flow construct we leave be.
        if key.trim().is_empty() || value.chars().next().is_some_and(|c| !c.is_whitespace()) {
            out.push(line.to_string());
            continue;
        }

        let trimmed = value.trim_start();
        let leading_ws = &value[..value.len() - trimmed.len()];
        let (scalar, comment) = split_trailing_comment(trimmed);
        let scalar = scalar.trim_end();

        let Some(first) = scalar.chars().next() else {
            out.push(line.to_string());
            continue;
        };
        // A `|`/`>` block scalar: leave the header and its body alone.
        if matches!(first, '|' | '>') {
            block_scalar_indent = Some(indent);
            out.push(line.to_string());
            continue;
        }
        // Already quoted — valid YAML, nothing to repair.
        if matches!(first, '\'' | '"') {
            out.push(line.to_string());
            continue;
        }

        let has_colon_separator = scalar_has_colon_separator(scalar);
        let invalid_flow_like = matches!(first, '[' | '{' | '@' | '`')
            && serde_yaml::from_str::<serde_yaml::Value>(scalar).is_err();
        if !has_colon_separator && !invalid_flow_like {
            out.push(line.to_string());
            continue;
        }

        let quoted = format!("'{}'", scalar.replace('\'', "''"));
        out.push(format!("{key}:{leading_ws}{quoted}{comment}"));
        changed = true;
    }

    changed.then(|| out.join("\n"))
}

/// Split an unquoted ` # comment` suffix off a frontmatter value, returning
/// `(scalar, comment_with_leading_whitespace)`. A `#` only starts a comment when
/// it is at the start or preceded by whitespace (so `C#` is left intact).
fn split_trailing_comment(value: &str) -> (&str, &str) {
    let mut prev_is_ws = true;
    for (i, c) in value.char_indices() {
        if c == '#' && prev_is_ws {
            let scalar_end = value[..i].trim_end().len();
            return (&value[..scalar_end], &value[scalar_end..]);
        }
        prev_is_ws = c.is_whitespace();
    }
    (value, "")
}

/// True when a scalar contains a YAML `key: value` separator (a `:` followed by
/// whitespace), which `serde_yaml` would otherwise read as nested structure.
fn scalar_has_colon_separator(scalar: &str) -> bool {
    let mut chars = scalar.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ':' && chars.peek().is_some_and(|n| n.is_whitespace()) {
            return true;
        }
    }
    false
}

pub(super) fn check_os_requirements(os: &[String]) -> bool {
    if os.is_empty() {
        return true;
    }
    let current = std::env::consts::OS;
    os.iter()
        .any(|o| o == current || (o == "darwin" && current == "macos"))
}

/// Every requirement not met on this machine. An unsupported system is the
/// only need reported then: installing anything else would not help.
pub(super) fn unmet_requirements(
    requires: &SkillRequires,
    os: &[String],
    resolved_creds: &std::collections::HashMap<String, String>,
) -> Vec<SkillNeed> {
    if !check_os_requirements(os) {
        trace!("Skill requirement missing: unsupported OS (need {:?})", os);
        return vec![SkillNeed::Os(os.to_vec())];
    }
    let mut needs = Vec::new();
    for bin in &requires.bins {
        if which::which(bin).is_err() {
            trace!("Skill requirement missing: binary '{bin}'");
            needs.push(SkillNeed::Bin(vec![bin.clone()]));
        }
    }
    if !requires.any_bins.is_empty() && !requires.any_bins.iter().any(|b| which::which(b).is_ok()) {
        trace!(
            "Skill requirement missing: none of {:?} found",
            requires.any_bins
        );
        needs.push(SkillNeed::Bin(requires.any_bins.clone()));
    }
    for var in &requires.env {
        if std::env::var(var).is_err() && !resolved_creds.contains_key(var) {
            trace!("Skill requirement missing: env var '{var}'");
            needs.push(SkillNeed::Env(var.clone()));
        }
    }
    needs
}

pub(super) fn check_requirements(
    requires: &SkillRequires,
    os: &[String],
    resolved_creds: &std::collections::HashMap<String, String>,
) -> bool {
    unmet_requirements(requires, os, resolved_creds).is_empty()
}
