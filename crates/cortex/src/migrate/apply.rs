use std::path::Path;

use anyhow::{Context, Result};

use crate::constants::IDENTITY_FILE;
use crate::db::Database;

use super::{MigrationPlan, MigrationResult, SourceData};

/// Apply a migration plan to the Borg data directory.
pub async fn apply_plan(
    plan: &MigrationPlan,
    data: &SourceData,
    borg_data_dir: &Path,
) -> Result<MigrationResult> {
    let mut result = MigrationResult {
        config_changes_applied: 0,
        credentials_added: 0,
        memory_files_copied: 0,
        persona_copied: false,
        skills_copied: 0,
        warnings: Vec::new(),
    };

    // Apply config changes to DB
    apply_config_changes(plan, &mut result).await?;

    // Apply credentials to DB
    apply_credentials(plan, data, &mut result).await?;

    // Copy memory files
    apply_memory_files(plan, borg_data_dir, &mut result)?;

    // Copy persona
    apply_persona(plan, borg_data_dir, &mut result)?;

    // Copy skills
    apply_skills(plan, borg_data_dir, &mut result)?;

    Ok(result)
}

async fn apply_config_changes(plan: &MigrationPlan, result: &mut MigrationResult) -> Result<()> {
    let active_changes: Vec<_> = plan.config_changes.iter().filter(|c| !c.skipped).collect();

    if active_changes.is_empty() {
        return Ok(());
    }

    let db = Database::open()
        .await
        .context("Failed to open database for migration")?;

    for change in &active_changes {
        // Use apply_setting for validation, then write to DB
        let mut scratch = crate::config::Config::default();
        match scratch.apply_setting(&change.key, &change.new_value) {
            Ok(_) => match db.set_setting(&change.key, &change.new_value).await {
                Ok(()) => result.config_changes_applied += 1,
                Err(e) => result.warnings.push(format!("Config {}: {e}", change.key)),
            },
            Err(e) => result.warnings.push(format!("Config {}: {e}", change.key)),
        }
    }

    Ok(())
}

async fn apply_credentials(
    plan: &MigrationPlan,
    data: &SourceData,
    result: &mut MigrationResult,
) -> Result<()> {
    if plan.credentials_to_add.is_empty() {
        return Ok(());
    }

    let db = Database::open()
        .await
        .context("Failed to open database for credential migration")?;

    // Load existing credentials from DB
    let existing_json = db.get_setting("credentials").await.ok().flatten();
    let mut creds: std::collections::HashMap<String, crate::config::CredentialValue> =
        existing_json
            .as_deref()
            .and_then(|s| {
                serde_json::from_str::<
                    std::collections::HashMap<String, crate::config::CredentialValue>,
                >(s)
                .ok()
            })
            .unwrap_or_default();

    for key_name in &plan.credentials_to_add {
        if data.credentials.iter().any(|(k, _)| k == key_name) {
            // Store as env var reference in DB credentials
            creds.insert(
                key_name.clone(),
                crate::config::CredentialValue::EnvVar(key_name.clone()),
            );
            result.credentials_added += 1;
        }
    }

    if !creds.is_empty() {
        if let Ok(json) = serde_json::to_string(&creds) {
            db.set_setting("credentials", &json).await?;
        }
    }

    Ok(())
}

/// Marker key identifying an already-stamped file.
const PROVENANCE_KEY: &str = "imported_from";

/// Record where an imported memory file came from, so its origin survives the
/// copy and a later reader can tell it apart from something Borg wrote.
///
/// Files with YAML frontmatter get the keys injected into it (keeping the
/// frontmatter valid and parseable); everything else gets one HTML comment,
/// which renders as nothing in markdown. Idempotent, so a re-import of a file
/// that was already stamped doesn't stack headers.
fn stamp_provenance(content: &str, origin: &str, source: &Path, imported_at: &str) -> String {
    let source = source.display();
    let fields = format!(
        "{PROVENANCE_KEY}: {origin}\nimported_source: {source}\nimported_at: {imported_at}"
    );

    if let Some(rest) = content.strip_prefix("---\n") {
        // Empty frontmatter (`---\n---\n…`) closes immediately, so there is no
        // preceding newline for the search below to find. Falling through would
        // prepend the comment *above* the fence and stop it being frontmatter
        // at all.
        if let Some(after_fence) = rest.strip_prefix("---") {
            return format!("---\n{fields}\n---{after_fence}");
        }
        if let Some(end) = rest.find("\n---") {
            let (frontmatter, tail) = rest.split_at(end);
            if frontmatter.contains(&format!("{PROVENANCE_KEY}:")) {
                return content.to_string();
            }
            return format!("---\n{frontmatter}\n{fields}{tail}");
        }
    }

    if content.contains(&format!("{PROVENANCE_KEY}:")) {
        return content.to_string();
    }
    let header = fields.replace('\n', " | ");
    format!("<!-- {header} -->\n\n{content}")
}

fn apply_memory_files(
    plan: &MigrationPlan,
    borg_data_dir: &Path,
    result: &mut MigrationResult,
) -> Result<()> {
    if plan.memory_files.is_empty() {
        return Ok(());
    }

    let memory_dir = borg_data_dir.join("memory");
    std::fs::create_dir_all(&memory_dir)?;
    let imported_at = crate::util::time::now_unix().to_string();

    for import in &plan.memory_files {
        let dest = memory_dir.join(&import.dest_rel);
        // Scoped imports land under `scopes/<scope>/`, which may not exist yet.
        if let Some(parent) = dest.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                result
                    .warnings
                    .push(format!("Failed to create {}: {e}", parent.display()));
                continue;
            }
        }

        let content = match std::fs::read_to_string(&import.source) {
            Ok(c) => c,
            Err(e) => {
                result
                    .warnings
                    .push(format!("Failed to read {}: {e}", import.source.display()));
                continue;
            }
        };

        // Warn rather than reject. The 20k cap and injection scan gate what the
        // *agent* writes to memory; this is the operator explicitly importing
        // their own instruction files, and refusing them here would be a
        // false-positive machine. The warning keeps it non-silent.
        let tokens = crate::memory::tokenizer::estimate_tokens(&content);
        if tokens >= crate::constants::MEMORY_ENTRY_REJECT_TOKENS {
            let msg = format!(
                "{} is ~{tokens} tokens, over the {}-token memory entry cap — imported anyway; \
                 consider splitting it",
                import.dest_rel,
                crate::constants::MEMORY_ENTRY_REJECT_TOKENS
            );
            tracing::warn!(target: "borg.migrate", file = %import.dest_rel, tokens, "{msg}");
            result.warnings.push(msg);
        }
        if !matches!(
            crate::security::sanitize::scan_for_injection(&content),
            crate::security::sanitize::ThreatLevel::Clean
        ) {
            let msg = format!(
                "{} matched prompt-injection patterns — imported anyway; review it before relying \
                 on it",
                import.dest_rel
            );
            tracing::warn!(target: "borg.migrate", file = %import.dest_rel, "{msg}");
            result.warnings.push(msg);
        }

        let stamped = stamp_provenance(&content, import.origin, &import.source, &imported_at);
        match std::fs::write(&dest, stamped) {
            Ok(()) => result.memory_files_copied += 1,
            Err(e) => result
                .warnings
                .push(format!("Failed to write {}: {e}", dest.display())),
        }
    }

    Ok(())
}

fn apply_persona(
    plan: &MigrationPlan,
    borg_data_dir: &Path,
    result: &mut MigrationResult,
) -> Result<()> {
    if let Some(src) = &plan.persona_file {
        let dest = borg_data_dir.join(IDENTITY_FILE);
        match std::fs::copy(src, &dest) {
            Ok(_) => result.persona_copied = true,
            Err(e) => result.warnings.push(format!("Failed to copy persona: {e}")),
        }
    }
    Ok(())
}

fn apply_skills(
    plan: &MigrationPlan,
    borg_data_dir: &Path,
    result: &mut MigrationResult,
) -> Result<()> {
    if plan.skill_dirs.is_empty() {
        return Ok(());
    }

    let skills_dir = borg_data_dir.join("skills");
    std::fs::create_dir_all(&skills_dir)?;

    for (src_dir, name) in &plan.skill_dirs {
        let dest_dir = skills_dir.join(name);
        match copy_dir_recursive(src_dir, &dest_dir) {
            Ok(()) => result.skills_copied += 1,
            Err(e) => result
                .warnings
                .push(format!("Failed to copy skill {name}: {e}")),
        }
    }

    Ok(())
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dest_path)?;
        } else {
            std::fs::copy(&src_path, &dest_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::{MigrationSource, PlanChange};
    use tempfile::TempDir;

    fn make_borg_dir() -> TempDir {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        std::fs::create_dir_all(dir.path().join("skills")).unwrap();
        dir
    }

    #[test]
    fn test_apply_config_changes_validates_keys() {
        // Verify apply_setting validates config keys correctly
        let mut config = crate::config::Config::default();
        assert!(config.apply_setting("model", "gpt-4o").is_ok());
        assert_eq!(config.llm.model, "gpt-4o");
        assert!(config.apply_setting("provider", "openai").is_ok());
        assert_eq!(config.llm.provider.as_deref(), Some("openai"));
    }

    #[test]
    fn test_skipped_changes_not_applied() {
        let plan = MigrationPlan {
            source: MigrationSource::Hermes,
            config_changes: vec![PlanChange {
                key: "llm.model".into(),
                source_key: "other".into(),
                current_value: Some("existing".into()),
                new_value: "should-skip".into(),
                skipped: true,
            }],
            credentials_to_add: vec![],
            memory_files: vec![],
            persona_file: None,
            skill_dirs: vec![],
        };
        let active: Vec<_> = plan.config_changes.iter().filter(|c| !c.skipped).collect();
        assert!(active.is_empty());
    }

    #[tokio::test]
    async fn test_apply_copies_memory_files() {
        let borg = make_borg_dir();
        let src = TempDir::new().unwrap();
        std::fs::write(src.path().join("MEMORY.md"), "# My Memories").unwrap();

        let plan = MigrationPlan {
            source: MigrationSource::Hermes,
            config_changes: vec![],
            credentials_to_add: vec![],
            memory_files: vec![crate::migrate::MemoryImport::global(
                src.path().join("MEMORY.md"),
                "hermes-MEMORY.md",
                "hermes",
            )],
            persona_file: None,
            skill_dirs: vec![],
        };

        let data = SourceData::default();
        let result = apply_plan(&plan, &data, borg.path()).await.unwrap();

        assert_eq!(result.memory_files_copied, 1);
        let content = std::fs::read_to_string(borg.path().join("memory/hermes-MEMORY.md")).unwrap();
        assert!(
            content.contains("# My Memories"),
            "imported file must keep its content, got {content:?}"
        );
        assert!(
            content.contains("imported_from: hermes"),
            "imported file must record where it came from, got {content:?}"
        );
    }

    #[tokio::test]
    async fn test_apply_copies_persona() {
        let borg = make_borg_dir();
        let src = TempDir::new().unwrap();
        std::fs::write(src.path().join("SOUL.md"), "# My Persona").unwrap();

        let plan = MigrationPlan {
            source: MigrationSource::Hermes,
            config_changes: vec![],
            credentials_to_add: vec![],
            memory_files: vec![],
            persona_file: Some(src.path().join("SOUL.md")),
            skill_dirs: vec![],
        };

        let data = SourceData::default();
        let result = apply_plan(&plan, &data, borg.path()).await.unwrap();

        assert!(result.persona_copied);
        let content = std::fs::read_to_string(borg.path().join("IDENTITY.md")).unwrap();
        assert_eq!(content, "# My Persona");
    }

    #[tokio::test]
    async fn test_apply_copies_skills() {
        let borg = make_borg_dir();
        let src = TempDir::new().unwrap();
        std::fs::create_dir_all(src.path().join("my-skill")).unwrap();
        std::fs::write(
            src.path().join("my-skill/SKILL.md"),
            "---\nname: my-skill\n---",
        )
        .unwrap();

        let plan = MigrationPlan {
            source: MigrationSource::Hermes,
            config_changes: vec![],
            credentials_to_add: vec![],
            memory_files: vec![],
            persona_file: None,
            skill_dirs: vec![(src.path().join("my-skill"), "my-skill".into())],
        };

        let data = SourceData::default();
        let result = apply_plan(&plan, &data, borg.path()).await.unwrap();

        assert_eq!(result.skills_copied, 1);
        assert!(borg.path().join("skills/my-skill/SKILL.md").exists());
    }

    #[test]
    fn test_credential_value_variants() {
        // Verify CredentialValue round-trips through JSON (used by DB storage)
        let cred = crate::config::CredentialValue::EnvVar("MY_KEY".to_string());
        let json = serde_json::to_string(&cred).unwrap();
        assert_eq!(json, "\"MY_KEY\"");
        let parsed: crate::config::CredentialValue = serde_json::from_str(&json).unwrap();
        match parsed {
            crate::config::CredentialValue::EnvVar(v) => assert_eq!(v, "MY_KEY"),
            _ => panic!("expected EnvVar"),
        }
    }

    /// Frontmatter files must stay parseable: the keys go *inside* the block,
    /// not above it, or the `---` fence would no longer open the file.
    #[test]
    fn provenance_goes_inside_existing_frontmatter() {
        let content = "---\nname: my-fact\ntype: user\n---\n\nThe fact body.\n";
        let stamped = stamp_provenance(content, "claude-code", Path::new("/src/fact.md"), "1000");

        let body = stamped
            .strip_prefix("---\n")
            .expect("stamped file still opens with a frontmatter fence");
        let end = body.find("\n---").expect("frontmatter still closes");
        let frontmatter = &body[..end];
        assert!(frontmatter.contains("name: my-fact"), "{stamped:?}");
        assert!(
            frontmatter.contains("imported_from: claude-code"),
            "{stamped:?}"
        );
        assert!(
            frontmatter.contains("imported_source: /src/fact.md"),
            "{stamped:?}"
        );
        assert!(stamped.contains("The fact body."), "{stamped:?}");
    }

    /// A plain markdown file gets a comment, which renders as nothing.
    #[test]
    fn provenance_prepends_comment_for_plain_markdown() {
        let stamped = stamp_provenance(
            "# Rules\n\nBe nice.\n",
            "cursor",
            Path::new("/r.md"),
            "1000",
        );
        assert!(
            stamped.starts_with("<!-- imported_from: cursor |"),
            "{stamped:?}"
        );
        assert!(stamped.contains("# Rules"), "{stamped:?}");
    }

    /// Empty frontmatter closes immediately, so there is no newline before the
    /// closing fence. The stamp must land between the fences — prepending a
    /// comment above them would stop the block being frontmatter at all.
    #[test]
    fn provenance_handles_empty_frontmatter() {
        let stamped = stamp_provenance("---\n---\n\nBody\n", "cursor", Path::new("/r.md"), "1000");
        assert!(
            stamped.starts_with("---\nimported_from: cursor"),
            "stamp must go inside the fences, got {stamped:?}"
        );
        let body = stamped.strip_prefix("---\n").unwrap();
        assert!(
            body.contains("\n---"),
            "frontmatter must still close: {stamped:?}"
        );
        assert!(stamped.contains("Body"), "{stamped:?}");
    }

    /// Re-importing must not stack headers on top of each other.
    #[test]
    fn provenance_stamping_is_idempotent() {
        for original in [
            "# Rules\n\nBe nice.\n",
            "---\nname: f\n---\n\nBody\n",
            "---\n---\n\nBody\n",
        ] {
            let once = stamp_provenance(original, "cursor", Path::new("/r.md"), "1000");
            let twice = stamp_provenance(&once, "cursor", Path::new("/r.md"), "2000");
            assert_eq!(once, twice, "re-stamping changed {original:?}");
        }
    }

    /// Scoped imports live under `scopes/<scope>/`, which won't exist yet on a
    /// fresh install — the copy has to create it rather than fail.
    #[tokio::test]
    async fn apply_creates_scope_dirs_for_scoped_imports() {
        let borg = make_borg_dir();
        let src = TempDir::new().unwrap();
        std::fs::write(src.path().join("fact.md"), "project fact").unwrap();

        let plan = MigrationPlan {
            source: MigrationSource::ClaudeCode,
            config_changes: vec![],
            credentials_to_add: vec![],
            memory_files: vec![crate::migrate::MemoryImport::scoped(
                src.path().join("fact.md"),
                "my-proj",
                "claude-fact.md",
                "claude-code",
            )],
            persona_file: None,
            skill_dirs: vec![],
        };

        let result = apply_plan(&plan, &SourceData::default(), borg.path())
            .await
            .unwrap();

        assert_eq!(result.memory_files_copied, 1);
        let content =
            std::fs::read_to_string(borg.path().join("memory/scopes/my-proj/claude-fact.md"))
                .expect("scoped import lands in its scope dir");
        assert!(content.contains("project fact"));
    }

    /// An oversized file is imported with a warning, not rejected: the caps
    /// exist to gate what the agent writes, and this is the operator importing
    /// their own file. But it must not be silent.
    #[tokio::test]
    async fn oversized_import_warns_but_still_copies() {
        let borg = make_borg_dir();
        let src = TempDir::new().unwrap();
        let huge = "word ".repeat(crate::constants::MEMORY_ENTRY_REJECT_TOKENS * 2);
        std::fs::write(src.path().join("big.md"), &huge).unwrap();

        let plan = MigrationPlan {
            source: MigrationSource::Cursor,
            config_changes: vec![],
            credentials_to_add: vec![],
            memory_files: vec![crate::migrate::MemoryImport::global(
                src.path().join("big.md"),
                "cursor-big.md",
                "cursor",
            )],
            persona_file: None,
            skill_dirs: vec![],
        };

        let result = apply_plan(&plan, &SourceData::default(), borg.path())
            .await
            .unwrap();

        assert_eq!(result.memory_files_copied, 1, "import must still happen");
        assert!(
            result.warnings.iter().any(|w| w.contains("token")),
            "an over-cap import must warn, got {:?}",
            result.warnings
        );
        assert!(borg.path().join("memory/cursor-big.md").exists());
    }
}
