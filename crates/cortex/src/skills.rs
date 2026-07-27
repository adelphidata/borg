mod environment;
#[cfg(test)]
use environment::format_skill_info;
pub use environment::{collect_required_env_vars, collect_skill_env, install_skill_deps};

mod selectors;

mod types;
#[cfg(test)]
use types::{check_os_requirements, check_requirements};
pub use types::{
    parse_skill_md, InstallSpec, PrerequisitesLegacy, Skill, SkillLoadLevel, SkillManifest,
    SkillRequires, SkillSource,
};

mod loader;
pub use loader::{
    install_default_skills, load_all_skills, reload_user_skills, skills_dir, SkillsReloadReport,
};
#[cfg(test)]
use loader::{is_skill_disabled, load_builtin_skill};

mod context;
#[cfg(test)]
use context::skill_relevance_score;
pub use context::{effective_skill_budget, load_skills_context, SkillBudgetReport};

// Test-only imports — the registry below needs none of these, but the colocated
// test module exercises functions across every skills submodule.
#[cfg(test)]
use crate::config::SkillsConfig;
#[cfg(test)]
use crate::memory::tokenizer::estimate_tokens;

/// Metadata for a single built-in skill entry in the registry.
/// Content and category are captured separately by `BUNDLED_SKILLS` and `SKILL_CATEGORY_MAP`.
struct SkillEntry {
    name: &'static str,
    default_enabled: bool,
    hidden: bool,
}

/// Declares all built-in skills in a single registry table, generating:
/// - `SKILL_REGISTRY`: `&[SkillEntry]` — the single source of truth
/// - `BUNDLED_SKILLS`: `&[(&str, &str)]` — name + included SKILL.md content
/// - `SKILL_CATEGORY_MAP`: `&[(&str, &str)]` — name → category
///
/// `DEFAULT_ENABLED_SKILLS` and `HIDDEN_SKILLS` are derived from the registry
/// so that all skill metadata lives in one place.
///
/// Each entry declares name, path, category, default_enabled, and hidden.
macro_rules! bundled_skills {
    ($(
        $name:literal => $path:literal {
            category: $cat:literal,
            default_enabled: $de:literal,
            hidden: $hid:literal
        }
    ),* $(,)?) => {
        const SKILL_REGISTRY: &[SkillEntry] = &[
            $(
                SkillEntry {
                    name: $name,
                    default_enabled: $de,
                    hidden: $hid,
                },
            )*
        ];

        const BUNDLED_SKILLS: &[(&str, &str)] = &[
            $( ($name, include_str!(concat!("../skills/", $path, "/SKILL.md"))) ),*
        ];

        /// Hardcoded category fallback for built-in skills without a `category` field.
        const SKILL_CATEGORY_MAP: &[(&str, &str)] = &[
            $( ($name, $cat) ),*
        ];
    };
}

bundled_skills! {
    "skill-creator"  => "skill-creator"  { category: "core",      default_enabled: true,  hidden: false },
    "borg-cli"       => "borg-cli"       { category: "core",      default_enabled: true,  hidden: false },
    "slack"          => "slack"          { category: "channels",  default_enabled: false, hidden: true  },
    "discord"        => "discord"        { category: "channels",  default_enabled: false, hidden: true  },
    "github"         => "github"         { category: "developer", default_enabled: true,  hidden: false },
    "weather"        => "weather"        { category: "utilities", default_enabled: false, hidden: false },
    "git"            => "git"            { category: "developer", default_enabled: true,  hidden: false },
    "search"         => "search"         { category: "core",      default_enabled: true,  hidden: false },
    "docker"         => "docker"         { category: "developer", default_enabled: false, hidden: false },
    "code-simplify"  => "code-simplify"  { category: "developer", default_enabled: false, hidden: false },
    "database"       => "database"       { category: "developer", default_enabled: false, hidden: false },
    "notes"          => "notes"          { category: "utilities", default_enabled: false, hidden: false },
    "obsidian"       => "obsidian"       { category: "utilities", default_enabled: false, hidden: false },
    "calendar"       => "calendar"       { category: "core",      default_enabled: true,  hidden: false },
    "1password"      => "1password"      { category: "utilities", default_enabled: false, hidden: false },
    "browser"        => "browser"        { category: "core",      default_enabled: true,  hidden: false },
    "email"          => "email"          { category: "core",      default_enabled: true,  hidden: false },
    "scheduler"      => "scheduler"      { category: "core",      default_enabled: true,  hidden: false },
    "paperclip"      => "paperclip"      { category: "developer", default_enabled: false, hidden: false },
    "pdf"            => "pdf"            { category: "utilities", default_enabled: true,  hidden: false },
    "reminders"      => "reminders"      { category: "productivity", default_enabled: false, hidden: false },
    "linear"         => "linear"         { category: "productivity", default_enabled: false, hidden: false },
    "stripe-link"    => "stripe-link"    { category: "payments",  default_enabled: false, hidden: false },
    "notion"         => "notion"         { category: "productivity", default_enabled: false, hidden: false },
    "healthcheck"    => "healthcheck"    { category: "core",      default_enabled: false, hidden: false },
    "video-frames"   => "video-frames"   { category: "utilities", default_enabled: false, hidden: false },
    "subagent-driven-development" => "subagent-driven-development" { category: "developer", default_enabled: true, hidden: false },
}

/// Filter `SKILL_REGISTRY` to the names satisfying a predicate, all at compile time.
/// Usage: `registry_filtered_names!(e => e.default_enabled)`.
macro_rules! registry_filtered_names {
    ($e:ident => $pred:expr) => {{
        const fn count() -> usize {
            let mut n = 0;
            let mut i = 0;
            while i < SKILL_REGISTRY.len() {
                let $e = &SKILL_REGISTRY[i];
                if $pred {
                    n += 1;
                }
                i += 1;
            }
            n
        }
        const N: usize = count();
        const fn build() -> [&'static str; N] {
            let mut out = [""; N];
            let mut i = 0;
            let mut j = 0;
            while i < SKILL_REGISTRY.len() {
                let $e = &SKILL_REGISTRY[i];
                if $pred {
                    out[j] = SKILL_REGISTRY[i].name;
                    j += 1;
                }
                i += 1;
            }
            out
        }
        build()
    }};
}

/// Skills enabled by default when no explicit config entry exists.
pub const DEFAULT_ENABLED_SKILLS: &[&str] = &registry_filtered_names!(e => e.default_enabled);

/// Skills hidden from the unified /plugins UI (still loaded for prompt injection).
pub const HIDDEN_SKILLS: &[&str] = &registry_filtered_names!(e => e.hidden);

/// Returns `true` if the given name matches a built-in skill.
///
/// Case-insensitive: the default macOS filesystem is, so `Scheduler/` and
/// `scheduler/` are the same on-disk directory — an exact match here would
/// let a case-twiddled path bypass guards that key on this check.
pub fn is_builtin_skill_name(name: &str) -> bool {
    BUNDLED_SKILLS
        .iter()
        .any(|(n, _)| n.eq_ignore_ascii_case(name))
}

fn bundled_skill_content(name: &str) -> Option<&'static str> {
    BUNDLED_SKILLS
        .iter()
        .find_map(|(skill_name, content)| (*skill_name == name).then_some(*content))
}

/// A bundled skill the user has shadowed with an on-disk copy under
/// `~/.borg/skills/<name>/SKILL.md`, and whether that copy diverges from the
/// compiled-in baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundledSkillOverride {
    /// Skill name (matches a `BUNDLED_SKILLS` entry).
    pub name: String,
    /// True when the user's `SKILL.md` differs from the bundled version.
    pub modified: bool,
}

/// Compare two `SKILL.md` bodies ignoring leading/trailing whitespace so a
/// stray trailing newline from an editor doesn't read as a modification.
fn skill_bodies_differ(a: &str, b: &str) -> bool {
    a.trim() != b.trim()
}

/// Return the bundled and user-on-disk copies of a bundled skill, if the user
/// has a copy at `skills_dir/<name>/SKILL.md`. `None` when `name` is not a
/// bundled skill or the user has no copy. Used by `borg skills diff <name>`.
pub fn bundled_vs_user_skill(
    name: &str,
    skills_dir: &std::path::Path,
) -> Option<(&'static str, String)> {
    let bundled = bundled_skill_content(name)?;
    let user = std::fs::read_to_string(skills_dir.join(name).join("SKILL.md")).ok()?;
    Some((bundled, user))
}

/// Scan `skills_dir` for user copies of bundled skills and report which diverge
/// from the compiled-in baseline. Only names present in `BUNDLED_SKILLS` are
/// considered — a purely custom user skill has no baseline to diff against and
/// is skipped. A missing/unreadable skills dir yields an empty list, not an
/// error, so fresh installs don't error out.
pub fn modified_bundled_skills(skills_dir: &std::path::Path) -> Vec<BundledSkillOverride> {
    let mut out = Vec::new();
    for (name, bundled) in BUNDLED_SKILLS {
        let path = skills_dir.join(name).join("SKILL.md");
        let Ok(user_content) = std::fs::read_to_string(&path) else {
            continue;
        };
        out.push(BundledSkillOverride {
            name: (*name).to_string(),
            modified: skill_bodies_differ(&user_content, bundled),
        });
    }
    out
}

/// Skills that cannot be disabled — always loaded regardless of user config.
/// Use sparingly: only for skills that teach core agent capabilities whose
/// tools are always present (e.g. `scheduler` teaches the always-available
/// `schedule` tool — disabling it would strand a top use case).
pub const MANDATORY_SKILLS: &[&str] = &["scheduler", "skill-creator"];

#[cfg(test)]
mod tests;
