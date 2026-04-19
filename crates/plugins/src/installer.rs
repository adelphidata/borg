use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tracing::info;

use crate::catalog::PluginDef;
use crate::{InstallEvent, InstallResult, TemplateTarget};

/// Install a plugin: extract templates, set up credentials, record in DB.
///
/// Credential prompting is handled by the caller (TUI) — this function receives
/// pre-collected credentials as key-value pairs.
pub async fn install(
    def: &PluginDef,
    data_dir: &std::path::Path,
    credentials: &[(String, String)],
    progress_tx: Option<&mpsc::Sender<InstallEvent>>,
) -> Result<InstallResult> {
    let id = def.id.to_string();
    let name = def.name.to_string();

    send_event(
        progress_tx,
        InstallEvent::Starting {
            id: id.clone(),
            name,
        },
    )
    .await;

    // 1. Check prerequisites
    check_prerequisites(def)?;

    // 2. Write template files (skip for native integrations)
    if !def.is_native {
        send_event(progress_tx, InstallEvent::WritingFiles { id: id.clone() }).await;
        write_templates(def, data_dir)?;
    }

    // 3. Store credentials in keychain
    let service = def.service_name();
    for (key, value) in credentials {
        send_event(
            progress_tx,
            InstallEvent::CredentialPrompt {
                id: id.clone(),
                label: key.clone(),
            },
        )
        .await;

        store_credential(&service, key, value)?;

        send_event(
            progress_tx,
            InstallEvent::CredentialStored {
                id: id.clone(),
                key: key.clone(),
            },
        )
        .await;
    }

    // 4. Make shell scripts executable
    make_scripts_executable(def, data_dir)?;

    // 5. Compute file hashes for integrity verification
    let file_hashes = compute_file_hashes(def, data_dir);

    // 6. Build credential entries
    let credential_entries = credentials
        .iter()
        .map(|(key, _)| {
            let account = credential_account(key);
            crate::CredentialEntry {
                key: key.clone(),
                service: service.clone(),
                account,
            }
        })
        .collect();

    // 7. Post-install hooks (plugin-specific — e.g., iMessage seeds the
    //    Messages-DB cursor once Full Disk Access is confirmed).
    let notes = def
        .post_install
        .map(|hook| hook(data_dir))
        .unwrap_or_default();
    let result = InstallResult {
        notes,
        credential_entries,
        file_hashes,
    };

    send_event(progress_tx, InstallEvent::Complete { id }).await;
    Ok(result)
}

/// Uninstall a plugin: delete files and keychain entries.
pub fn uninstall(def: &PluginDef, data_dir: &std::path::Path) -> Result<()> {
    // Remove template files (skip for native integrations)
    if !def.is_native {
        let first_tmpl = def.templates.first().context("no templates")?;
        let dir_name = first_tmpl
            .relative_path
            .split('/')
            .next()
            .context("empty relative path")?;

        let target_dir = first_tmpl.target.base_dir(data_dir).join(dir_name);

        if target_dir.exists() {
            std::fs::remove_dir_all(&target_dir)
                .with_context(|| format!("Failed to remove {}", target_dir.display()))?;
            info!("Removed {}", target_dir.display());
        }
    }

    // Remove keychain entries and verify removal
    let service = def.service_name();
    for cred in def.required_credentials {
        remove_credential(&service, cred.key);
        let account = credential_account(cred.key);
        if crate::keychain::check(&service, &account) {
            tracing::warn!(
                "Keychain entry for {} still present after removal — may resurrect on restart",
                cred.key
            );
        }
    }

    Ok(())
}

/// Check if an integration is already installed on the filesystem.
///
/// For native integrations, checks that all required (non-optional) credentials
/// are available via environment variable or OS keychain.
pub fn is_installed(def: &PluginDef, _data_dir: &std::path::Path) -> bool {
    if def.is_native {
        return def.required_credentials.iter().all(|cred| {
            if cred.is_optional {
                return true;
            }
            let has_env = std::env::var(cred.key)
                .map(|v| !v.is_empty())
                .unwrap_or(false);
            let service = def.service_name();
            let account = credential_account(cred.key);
            has_env || crate::keychain::check(&service, &account)
        });
    }

    let first_tmpl = match def.templates.first() {
        Some(t) => t,
        None => return false,
    };

    let dir_name = match first_tmpl.relative_path.split('/').next() {
        Some(name) => name,
        None => return false,
    };

    let base = first_tmpl.target.base_dir(_data_dir).join(dir_name);
    let manifest = match first_tmpl.target {
        TemplateTarget::Channels => base.join("channel.toml"),
        TemplateTarget::Tools => base.join("tool.toml"),
    };

    manifest.exists()
}

// ── Internal helpers ──

/// Compute SHA-256 hashes of all template files after installation.
/// Returns `(relative_path, hex_hash)` pairs.
pub fn compute_file_hashes(def: &PluginDef, data_dir: &std::path::Path) -> Vec<(String, String)> {
    let mut hashes = Vec::new();
    for tmpl in def.templates {
        let full_path = tmpl.target.base_dir(data_dir).join(tmpl.relative_path);
        match std::fs::read(&full_path) {
            Ok(content) => {
                let mut hasher = Sha256::new();
                hasher.update(&content);
                let hex = format!("{:x}", hasher.finalize());
                hashes.push((tmpl.relative_path.to_string(), hex));
            }
            Err(e) => {
                tracing::warn!("Failed to read {} for hashing: {e}", full_path.display());
            }
        }
    }
    hashes
}

fn check_prerequisites(def: &PluginDef) -> Result<()> {
    if !def.platform.is_available() {
        anyhow::bail!(
            "{} requires {}",
            def.name,
            def.platform.label().unwrap_or("a different platform")
        );
    }

    for bin in def.required_bins {
        which::which(bin).with_context(|| {
            if *bin == "python3" {
                format!("Required binary not found: {bin}. Install via: brew install python3 (macOS) or apt install python3 (Linux)")
            } else {
                format!("Required binary not found: {bin}")
            }
        })?;
    }

    Ok(())
}

fn write_templates(def: &PluginDef, data_dir: &std::path::Path) -> Result<()> {
    for tmpl in def.templates {
        let full_path = tmpl.target.base_dir(data_dir).join(tmpl.relative_path);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }

        std::fs::write(&full_path, tmpl.content)
            .with_context(|| format!("Failed to write {}", full_path.display()))?;
        info!("Wrote {}", full_path.display());
    }
    Ok(())
}

fn make_scripts_executable(def: &PluginDef, data_dir: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for tmpl in def.templates {
            if tmpl.relative_path.ends_with(".sh") || tmpl.relative_path.ends_with(".py") {
                let path = tmpl.target.base_dir(data_dir).join(tmpl.relative_path);
                if path.exists() {
                    let mut perms = std::fs::metadata(&path)?.permissions();
                    perms.set_mode(0o755);
                    std::fs::set_permissions(&path, perms)?;
                }
            }
        }
    }
    Ok(())
}

fn credential_account(key: &str) -> String {
    format!("borg-{key}")
}

fn store_credential(service: &str, key: &str, value: &str) -> Result<()> {
    let account = credential_account(key);
    crate::keychain::store(service, &account, value)
}

fn remove_credential(service: &str, key: &str) {
    let account = credential_account(key);
    crate::keychain::remove(service, &account);
}

/// Post-install hook for iMessage: initialize state.json with current max ROWID
/// so only future messages are processed. Returns user-facing notes.
pub(crate) fn imessage_post_install(data_dir: &std::path::Path) -> Vec<String> {
    let mut notes = Vec::new();
    let state_path = data_dir
        .join("channels")
        .join("imessage")
        .join("state.json");
    let db_path = dirs::home_dir()
        .map(|h| h.join("Library/Messages/chat.db"))
        .unwrap_or_default();

    if !db_path.exists() {
        notes.push("Full Disk Access required:".to_string());
        notes.push("  System Settings > Privacy & Security > Full Disk Access".to_string());
        notes.push("  Add your terminal app, then restart Borg".to_string());
        return notes;
    }

    let db_uri = format!("file:{}?mode=ro", db_path.display());
    match rusqlite::Connection::open_with_flags(
        &db_uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    ) {
        Ok(conn) => {
            // Initialize state with current max ROWID
            match conn.query_row("SELECT MAX(ROWID) FROM message", [], |row| {
                row.get::<_, Option<i64>>(0)
            }) {
                Ok(Some(max_rowid)) => {
                    let state = format!("{{\"last_rowid\": {max_rowid}}}");
                    if let Err(e) = std::fs::write(&state_path, &state) {
                        notes.push(format!("Warning: failed to write state file: {e}"));
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    notes.push(format!("Warning: could not query Messages DB: {e}"));
                }
            }

            notes.push("Full Disk Access detected".to_string());

            // Try to detect the user's iMessage address
            if let Ok(address) = conn.query_row(
                "SELECT DISTINCT h.id FROM message m JOIN handle h ON m.handle_id = h.ROWID WHERE m.is_from_me = 1 LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            ) {
                notes.push(format!("Your iMessage address: {address}"));
            }

            notes.push("Messages will be processed automatically when Borg is running".to_string());
            notes.push(
                "Note: Messages you send to yourself are ignored (prevents loops). Test by texting from another device.".to_string(),
            );
        }
        Err(_) => {
            notes.push("Full Disk Access required:".to_string());
            notes.push("  System Settings > Privacy & Security > Full Disk Access".to_string());
            notes.push("  Add your terminal app, then restart Borg".to_string());
        }
    }

    notes
}

async fn send_event(tx: Option<&mpsc::Sender<InstallEvent>>, event: InstallEvent) {
    if let Some(tx) = tx {
        if let Err(e) = tx.send(event).await {
            tracing::warn!("Failed to send install event: {e}");
        }
    }
}

/// Check if the OS keychain is available.
pub fn keychain_available() -> bool {
    crate::keychain::available()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::CATALOG;

    #[test]
    fn prerequisites_check_bins() {
        // python3 should be available on most dev machines
        let telegram = CATALOG.iter().find(|c| c.id == "messaging/telegram");
        if let Some(def) = telegram {
            // Don't assert — just verify it doesn't panic
            let _ = check_prerequisites(def);
        }
    }

    #[test]
    fn is_installed_returns_false_for_missing() {
        let def = CATALOG
            .iter()
            .find(|c| c.id == "messaging/whatsapp")
            .expect("whatsapp in catalog");
        let tmp = std::env::temp_dir().join("borg-test-nonexistent");
        assert!(!is_installed(def, &tmp));
    }

    #[test]
    fn compute_file_hashes_returns_correct_sha256() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let data_dir = tmp.path();

        let def = CATALOG
            .iter()
            .find(|c| c.id == "messaging/whatsapp")
            .expect("whatsapp in catalog");

        // Write templates
        write_templates(def, data_dir).expect("write");

        let hashes = compute_file_hashes(def, data_dir);
        assert!(!hashes.is_empty());

        // Verify each hash is a valid 64-char hex string (SHA-256)
        for (path, hash) in &hashes {
            assert!(!path.is_empty());
            assert_eq!(hash.len(), 64, "SHA-256 hex should be 64 chars for {path}");
            assert!(
                hash.chars().all(|c| c.is_ascii_hexdigit()),
                "hash should be hex for {path}"
            );
        }
    }

    #[test]
    fn compute_file_hashes_covers_all_templates() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let data_dir = tmp.path();

        let def = CATALOG
            .iter()
            .find(|c| c.id == "messaging/whatsapp")
            .expect("whatsapp in catalog");
        write_templates(def, data_dir).expect("write");

        let hashes = compute_file_hashes(def, data_dir);
        assert_eq!(hashes.len(), def.templates.len());
    }

    #[tokio::test]
    async fn install_result_includes_file_hashes() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let data_dir = tmp.path();

        let def = CATALOG
            .iter()
            .find(|c| c.id == "messaging/whatsapp")
            .expect("whatsapp in catalog");
        let result = install(def, data_dir, &[], None).await.expect("install");

        assert!(!result.file_hashes.is_empty());
        for (_, hash) in &result.file_hashes {
            assert_eq!(hash.len(), 64);
            assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    #[tokio::test]
    async fn install_result_includes_credential_entries() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let data_dir = tmp.path();

        let def = CATALOG
            .iter()
            .find(|c| c.id == "messaging/whatsapp")
            .expect("whatsapp in catalog");
        let result = install(def, data_dir, &[], None).await.expect("install");

        // No credentials passed = no credential entries
        assert!(result.credential_entries.is_empty());
    }

    #[test]
    fn install_result_default_has_empty_vecs() {
        let result = InstallResult::default();
        assert!(result.notes.is_empty());
        assert!(result.credential_entries.is_empty());
        assert!(result.file_hashes.is_empty());
    }

    #[test]
    fn is_installed_native_without_creds() {
        let def = CATALOG
            .iter()
            .find(|c| c.id == "messaging/teams")
            .expect("teams in catalog");
        let tmp = std::env::temp_dir().join("borg-test-native-nocreds");
        unsafe {
            std::env::remove_var("TEAMS_APP_ID");
            std::env::remove_var("TEAMS_APP_SECRET");
        }
        assert!(!is_installed(def, &tmp));
    }

    #[test]
    fn is_installed_native_with_env() {
        // Use google-chat (single cred) to avoid race with other tests that touch TELEGRAM_BOT_TOKEN
        let def = CATALOG
            .iter()
            .find(|c| c.id == "messaging/google-chat")
            .expect("google-chat in catalog");
        let tmp = std::env::temp_dir().join("borg-test-native-env");
        unsafe {
            std::env::set_var("GOOGLE_CHAT_WEBHOOK_TOKEN", "test-token-12345");
        }
        let result = is_installed(def, &tmp);
        unsafe {
            std::env::remove_var("GOOGLE_CHAT_WEBHOOK_TOKEN");
        }
        assert!(result);
    }

    #[tokio::test]
    async fn install_native_skips_template_write() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let data_dir = tmp.path();

        let def = CATALOG
            .iter()
            .find(|c| c.id == "messaging/telegram")
            .expect("telegram in catalog");
        let result = install(def, data_dir, &[], None).await.expect("install");

        assert!(result.file_hashes.is_empty());
        // No template directories should have been created
        assert!(!data_dir.join("channels").join("telegram").exists());
    }

    #[test]
    fn uninstall_native_succeeds_without_files() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let def = CATALOG
            .iter()
            .find(|c| c.id == "messaging/telegram")
            .expect("telegram in catalog");
        let result = uninstall(def, tmp.path());
        assert!(result.is_ok());
    }
}
