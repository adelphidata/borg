//! The agent's name: the curated pool onboarding suggests from, and the
//! seeded memory files that carry it.
//!
//! Every surface draws from [`BORG_NAMES`] — the TUI wizard directly, the
//! web and desktop through `Admin.GetSetupStatus` — so there is one list.
//! The name itself lives in the `user.agent_name` setting; a rename rewrites
//! it in `IDENTITY.md`, `MEMORY.md` and `SETUP.md` so the agent's own
//! memory agrees with the sidebar.

use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use rand::Rng;

use crate::constants::{IDENTITY_FILE, MEMORY_INDEX_FILE, SETUP_FILE};

/// What the agent is called when nobody has named it.
pub const DEFAULT_AGENT_NAME: &str = "Borg";

/// Names onboarding suggests. Served to the GUI, so keep entries short
/// enough to sit in the sidebar header.
pub const BORG_NAMES: &[&str] = &[
    "Borgan Freeman",
    "Borgan Wallen",
    "Borgbacue",
    "Mark Zuckerborg",
    "Ruth Bader Ginsborg",
    "Borg Washington",
    "Goldborg",
    "Bloomborg",
    "Heidegborg",
    "Gutenborg",
    "Borgman",
    "Max Borg",
    "Bjorn Borg",
    "Piers Borgan",
    "Steven Spielborg",
    "Smorgasborg",
    "J.P. Borgan",
    "Tracy Borgan",
    "Borgan Fairchild",
    "Victor Borge",
    "Cyborg Jones",
    "Rube Goldborg",
    "Blomborg",
    "Harrisonborg",
    "Stromborg",
    "Borgström",
    "Weinborg",
    "Feinborg",
    "Borgenstein",
    "Borgward",
    "Borgson",
    "Borgy",
    "Borgie",
    "B0rg",
    "Pittsborg",
    "Luxemborg",
    "Salzborg",
    "Harrisborg",
    "Gettysborg",
    "Williamsborg",
    "Pittsborg Steelers",
    "Pittsborg Pirates",
    "Pittsborg Penguins",
    "Johannesborg",
    "Nuremborg",
    "Borgholm",
    "Saint Petersborg",
    "Borgtown",
    "Spongeborg Squarepants",
    "Borger King",
    "Borgcraft",
    "Borg Lightyear",
    "Borginator",
    "The Borg Identity",
    "Borg of the Rings",
    "Borgzilla",
    "Cyborg",
    "Iceborger",
    "Cheeseborger",
    "Hamborg",
    "Borgify",
    "Newborg Baby",
    "Borganic",
    "Borgana",
    "The Battle of The Borg",
    "Borga Borga",
    "LeBorg James",
    "The Good, The Borg, and The Ugly",
    "Borgs Bunny",
    "Borges",
    "Schoenberg",
    "Borgkovich",
    "Heisenborg",
    "Borgdorf",
    "Wittgenborg",
    "Schrödingborg",
    "Humphrey Borgart",
    "Borggart",
    "Schwarzenegborg",
    "Clint Eastborg",
    "Borgothy",
    "Borgatron",
    "Borgatti",
    "Borgleone",
    "Amelia Borghart",
    "Oppenborger",
    "Borgdwin",
    "Borgsworth",
    "Van Borghen",
    "Strasborg",
    "Borgdeaux",
    "Borgona",
    "Borggundy",
    "Borgcelona",
    "Borgamo",
    "Borgpool",
    "Borg Vader",
    "Frodo Borggins",
    "Borglock Holmes",
    "James Borgd",
    "Indiana Borgs",
    "Captain Borgmerica",
    "The Incredible Borg",
    "Borg to the Future",
    "Jurassic Borg",
    "The Borgfather",
    "Borgbusters",
    "Borg Wars",
    "The Borgrix",
    "Doctor Borgenstein",
    "Frankenborg",
    "Borgalicious",
    "Borgmeister",
    "Borgnado",
    "Borgtastic",
    "Borgberry",
    "Borgasaurus",
    "Borgnarok",
    "El Borgo",
    "Borgchamp",
    "Borgsmith",
    "Borganaut",
    "Borgopolis",
    "Ingrid Borgman",
];

/// One name from [`BORG_NAMES`], uniformly.
pub fn random_borg_name() -> String {
    let idx = rand::rng().random_range(0..BORG_NAMES.len());
    BORG_NAMES[idx].to_string()
}

/// Longest name accepted; the GUI field enforces the same.
pub const MAX_AGENT_NAME_CHARS: usize = 40;

/// Held across every read-modify-write of the seeded memory files, so two
/// quick renames (or a rename and an identity save) cannot interleave and
/// leave the setting and the files naming different agents.
pub static MEMORY_FILE_LOCK: Mutex<()> = Mutex::new(());

/// Why `name` cannot be the agent's name, if it cannot. Empty is allowed
/// (it means the default). Line breaks would split the `# Name` heading and
/// `**` would end the bold span `SETUP.md` keeps it in.
pub fn validate(name: &str) -> std::result::Result<(), String> {
    let name = name.trim();
    if name.chars().count() > MAX_AGENT_NAME_CHARS {
        return Err(format!(
            "keep the name to {MAX_AGENT_NAME_CHARS} characters"
        ));
    }
    if name.chars().any(char::is_control) || name.contains("**") {
        return Err("the name cannot contain line breaks, control characters or `**`".into());
    }
    Ok(())
}

/// The name `IDENTITY.md` gives itself in its `# Name` heading, if any.
pub fn name_in_identity(content: &str) -> Option<&str> {
    content
        .lines()
        .next()?
        .strip_prefix("# ")
        .map(str::trim)
        .filter(|n| !n.is_empty())
}

/// Rewrites one memory file's text from the old name to the new; `None` when unchanged.
type RenameEdit = fn(&str, &str, &str) -> Option<String>;

/// Rewrite the agent's name in the seeded memory files under `data_dir`,
/// from `old_name` (the setting's previous value) to `new_name`.
///
/// A file that does not exist is left alone (the web onboarding never seeds
/// them, and the no-file identity fallback already reads the setting). An
/// empty name means the default.
pub fn rename_in_memory_files(data_dir: &Path, old_name: &str, new_name: &str) -> Result<()> {
    let or_default = |n: &str| match n.trim() {
        "" => DEFAULT_AGENT_NAME.to_string(),
        n => n.to_string(),
    };
    let (old, name) = (or_default(old_name), or_default(new_name));
    let _guard = MEMORY_FILE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let edits: [(&str, RenameEdit); 3] = [
        (IDENTITY_FILE, rename_identity),
        (MEMORY_INDEX_FILE, rename_memory_index),
        (SETUP_FILE, rename_setup),
    ];
    for (file, edit) in edits {
        let path = data_dir.join(file);
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        if let Some(updated) = edit(&content, &old, &name) {
            coding::fs_atomic::atomic_write(&path, updated.as_bytes())?;
        }
    }
    Ok(())
}

/// `IDENTITY.md`: the `# Name` heading and every `You are Name,`. The
/// heading wins over `old` when there is one, so a hand-edited file still
/// renames. A distinctive old name is replaced as a whole word everywhere;
/// "Borg" only in those two spots, since the product is called that too.
fn rename_identity(content: &str, old: &str, new: &str) -> Option<String> {
    let old = name_in_identity(content).unwrap_or(old.trim());
    if old.is_empty() || old == new {
        return None;
    }
    let updated = if old == DEFAULT_AGENT_NAME {
        let body = content.replace(&format!("You are {old},"), &format!("You are {new},"));
        match body.strip_prefix(&format!("# {old}")) {
            Some(rest) => format!("# {new}{rest}"),
            None => body,
        }
    } else {
        replace_whole_word(content, old, new)
    };
    (updated != content).then_some(updated)
}

/// `content` with each whole-word `old` replaced by `new`. A match glued to
/// a letter or digit is part of another word ("Al" in "Always", "Sam" in
/// "Samantha") and stays.
fn replace_whole_word(content: &str, old: &str, new: &str) -> String {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = String::with_capacity(content.len());
    let mut last = 0;
    for (i, _) in content.match_indices(old) {
        let before = content[..i].chars().next_back();
        let after = content[i + old.len()..].chars().next();
        if before.is_some_and(is_word) || after.is_some_and(is_word) {
            continue;
        }
        out.push_str(&content[last..i]);
        out.push_str(new);
        last = i + old.len();
    }
    out.push_str(&content[last..]);
    out
}

/// `MEMORY.md`: the `- Agent:` line onboarding seeds under `## Owner`. Only
/// the first such line: later ones are the agent's own entries.
fn rename_memory_index(content: &str, _old: &str, new: &str) -> Option<String> {
    let mut offset = 0;
    for line in content.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix("- Agent: ") {
            if rest.trim_end() == new {
                return None;
            }
            let eol = &rest[rest.trim_end_matches(['\r', '\n']).len()..];
            return Some(format!(
                "{}- Agent: {new}{eol}{}",
                &content[..offset],
                &content[offset + line.len()..]
            ));
        }
        offset += line.len();
    }
    None
}

/// `SETUP.md`: "chose the name **Name**" in the first-boot brief.
fn rename_setup(content: &str, _old: &str, new: &str) -> Option<String> {
    const LEAD: &str = "chose the name **";
    let start = content.find(LEAD)? + LEAD.len();
    let len = content[start..].find("**")?;
    if &content[start..start + len] == new {
        return None;
    }
    Some(format!(
        "{}{new}{}",
        &content[..start],
        &content[start + len..]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The "≥100" floor keeps the suggestion from being predictable; the
    /// dedup guard catches a copy-paste duplicate doubling a pick rate.
    #[test]
    fn pool_is_large_and_deduplicated() {
        assert!(BORG_NAMES.len() >= 100, "got {}", BORG_NAMES.len());
        let mut seen = std::collections::HashSet::new();
        for name in BORG_NAMES {
            assert!(seen.insert(name), "Duplicate Borg name: {name}");
            assert!(!name.trim().is_empty());
        }
    }

    #[test]
    fn random_name_comes_from_the_pool() {
        for _ in 0..20 {
            let name = random_borg_name();
            assert!(BORG_NAMES.contains(&name.as_str()), "unknown: {name}");
        }
    }

    #[test]
    fn identity_template_renames_heading_and_body() {
        let before = "# Borgzilla\n\nYou are Borgzilla, an AI personal assistant belonging to Sam.\n\n## Personality\nBorgzilla likes puns.\n";
        let after = rename_identity(before, "Borg", "Frodo Borggins").unwrap();
        assert_eq!(
            after,
            "# Frodo Borggins\n\nYou are Frodo Borggins, an AI personal assistant belonging to Sam.\n\n## Personality\nFrodo Borggins likes puns.\n"
        );
    }

    #[test]
    fn identity_named_borg_keeps_product_mentions() {
        let before = "# Borg\n\nYou are Borg, an instance of Borg.\n";
        let after = rename_identity(before, "Borg", "Borgzilla").unwrap();
        assert_eq!(
            after,
            "# Borgzilla\n\nYou are Borgzilla, an instance of Borg.\n"
        );
    }

    #[test]
    fn default_identity_without_heading_renames() {
        let after = rename_identity(
            "You are Borg, a personal AI assistant.\n",
            "Borg",
            "Borgzilla",
        );
        assert_eq!(
            after.as_deref(),
            Some("You are Borgzilla, a personal AI assistant.\n")
        );
    }

    /// A pool name with commas must not be cut at the first one: the old
    /// name comes from the setting, not from scanning the sentence.
    #[test]
    fn headingless_identity_uses_the_previous_name() {
        let old = "The Good, The Borg, and The Ugly";
        let before = format!("You are {old}, a personal AI assistant built on Borg.\n");
        let after = rename_identity(&before, old, "Borgzilla").unwrap();
        assert_eq!(
            after,
            "You are Borgzilla, a personal AI assistant built on Borg.\n"
        );
    }

    #[test]
    fn empty_heading_or_old_name_changes_nothing() {
        assert_eq!(rename_identity("# \nYou are a helper.\n", "", "X"), None);
    }

    #[test]
    fn validate_rejects_what_would_break_the_files() {
        assert!(validate("Frodo Borggins").is_ok());
        assert!(validate("").is_ok());
        assert!(validate("a\nb").is_err());
        assert!(validate("**bold**").is_err());
        assert!(validate(&"x".repeat(MAX_AGENT_NAME_CHARS + 1)).is_err());
        assert!(validate(&"é".repeat(MAX_AGENT_NAME_CHARS)).is_ok());
    }

    #[test]
    fn name_in_identity_reads_the_heading() {
        assert_eq!(
            name_in_identity("# Jarvis\n\nYou are Jarvis."),
            Some("Jarvis")
        );
        assert_eq!(name_in_identity("You are Borg."), None);
        assert_eq!(name_in_identity("# \n"), None);
    }

    #[test]
    fn identity_unchanged_returns_none() {
        assert_eq!(rename_identity("# Borgzilla\n", "Borg", "Borgzilla"), None);
        assert_eq!(
            rename_identity("free-form persona\n", "Borg", "Borgzilla"),
            None
        );
    }

    #[test]
    fn memory_index_rewrites_agent_line_only() {
        let before = "# Memory Index\n\n## Owner\n- Name: Sam\n- Agent: Borgzilla\n- Agent notes\n";
        let after = rename_memory_index(before, "", "Borg Vader").unwrap();
        assert_eq!(
            after,
            "# Memory Index\n\n## Owner\n- Name: Sam\n- Agent: Borg Vader\n- Agent notes\n"
        );
        assert_eq!(rename_memory_index(&after, "", "Borg Vader"), None);
        assert_eq!(rename_memory_index("# Memory Index\n", "", "X"), None);
    }

    #[test]
    fn identity_rename_skips_matches_inside_other_words() {
        let before = "# Al\n\nYou are Al, an AI personal assistant belonging to Alma.\nAlways kind. Al's rule.\n";
        let after = rename_identity(before, "Borg", "Ada").unwrap();
        assert_eq!(
            after,
            "# Ada\n\nYou are Ada, an AI personal assistant belonging to Alma.\nAlways kind. Ada's rule.\n"
        );
    }

    #[test]
    fn memory_index_rewrites_only_the_first_agent_line() {
        let before = "## Owner\n- Agent: A\n\n## Notes\n- Agent: A runs nightly\n";
        let after = rename_memory_index(before, "", "B").unwrap();
        assert_eq!(
            after,
            "## Owner\n- Agent: B\n\n## Notes\n- Agent: A runs nightly\n"
        );
    }

    #[test]
    fn memory_index_keeps_crlf() {
        let after = rename_memory_index("- Agent: A\r\n", "", "B").unwrap();
        assert_eq!(after, "- Agent: B\r\n");
    }

    #[test]
    fn setup_rewrites_chosen_name() {
        let before = "Sam created you and chose the name **Borgzilla** for this instance.";
        let after = rename_setup(before, "", "Borg Vader").unwrap();
        assert_eq!(
            after,
            "Sam created you and chose the name **Borg Vader** for this instance."
        );
        assert_eq!(rename_setup(&after, "", "Borg Vader"), None);
        assert_eq!(rename_setup("no marker", "", "X"), None);
    }

    #[test]
    fn rename_in_memory_files_rewrites_present_files_and_skips_missing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(IDENTITY_FILE),
            "# Borgzilla\n\nYou are Borgzilla, hi.\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join(MEMORY_INDEX_FILE),
            "## Owner\n- Agent: Borgzilla\n",
        )
        .unwrap();

        rename_in_memory_files(dir.path(), "Borgzilla", "  Borg Vader ").unwrap();

        let identity = std::fs::read_to_string(dir.path().join(IDENTITY_FILE)).unwrap();
        assert_eq!(identity, "# Borg Vader\n\nYou are Borg Vader, hi.\n");
        let memory = std::fs::read_to_string(dir.path().join(MEMORY_INDEX_FILE)).unwrap();
        assert_eq!(memory, "## Owner\n- Agent: Borg Vader\n");
        assert!(
            !dir.path().join(SETUP_FILE).exists(),
            "missing files stay missing"
        );
    }

    #[test]
    fn empty_name_renames_back_to_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(MEMORY_INDEX_FILE), "- Agent: Borgzilla\n").unwrap();
        rename_in_memory_files(dir.path(), "Borgzilla", "").unwrap();
        let memory = std::fs::read_to_string(dir.path().join(MEMORY_INDEX_FILE)).unwrap();
        assert_eq!(memory, "- Agent: Borg\n");
    }
}
