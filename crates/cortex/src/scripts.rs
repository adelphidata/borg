use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::config::Config;
use crate::constants::{KEYCHAIN_ACCOUNT_SCRIPTS_HMAC, KEYCHAIN_SERVICE_SCRIPTS};
use crate::db::{Database, NewScript, ScriptRow};
use crate::util::hex::{hex_decode, hex_encode};

type HmacSha256 = Hmac<Sha256>;

const KEYCHAIN_SERVICE: &str = KEYCHAIN_SERVICE_SCRIPTS;
const KEYCHAIN_ACCOUNT: &str = KEYCHAIN_ACCOUNT_SCRIPTS_HMAC;
const KEY_FILE_NAME: &str = ".scripts_key";
const KEY_LEN: usize = 32;

/// Validate script name to prevent path traversal and injection.
fn validate_script_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Script name cannot be empty");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("Script name must contain only alphanumeric characters, hyphens, and underscores");
    }
    Ok(())
}

// ── HMAC Key Management ──

/// Get or create the HMAC-SHA256 signing key.
/// Tries OS keychain first, falls back to file at `~/.borg/.scripts_key` (mode 0600).
pub fn get_or_create_hmac_key() -> Result<Vec<u8>> {
    // Try reading from keychain
    if let Some(key) = read_key_keychain() {
        return Ok(key);
    }

    // Try reading from file
    let key_path = key_file_path()?;
    if key_path.exists() {
        let hex = std::fs::read_to_string(&key_path).context("Failed to read HMAC key file")?;
        return hex_decode(hex.trim());
    }

    // Generate new key
    let key = generate_random_key();

    // Try storing in keychain first
    if store_key_keychain(&key) {
        return Ok(key);
    }

    // Fall back to file
    write_key_file(&key_path, &key)?;
    Ok(key)
}

fn generate_random_key() -> Vec<u8> {
    use rand::RngCore;
    let mut key = vec![0u8; KEY_LEN];
    rand::rng().fill_bytes(&mut key);
    key
}

fn key_file_path() -> Result<PathBuf> {
    Ok(Config::data_dir()?.join(KEY_FILE_NAME))
}

fn write_key_file(path: &Path, key: &[u8]) -> Result<()> {
    let hex = hex_encode(key);
    // Ensure parent dir exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, &hex)?;
    // Set permissions to 0600 on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn read_key_keychain() -> Option<Vec<u8>> {
    if cfg!(target_os = "macos") {
        let output = std::process::Command::new("security")
            .args([
                "find-generic-password",
                "-s",
                KEYCHAIN_SERVICE,
                "-a",
                KEYCHAIN_ACCOUNT,
                "-w",
            ])
            .output()
            .ok()?;
        if output.status.success() {
            let hex = String::from_utf8(output.stdout).ok()?;
            return hex_decode(hex.trim()).ok();
        }
    } else if cfg!(target_os = "linux") {
        let output = std::process::Command::new("secret-tool")
            .args([
                "lookup",
                "service",
                KEYCHAIN_SERVICE,
                "key",
                KEYCHAIN_ACCOUNT,
            ])
            .output()
            .ok()?;
        if output.status.success() {
            let hex = String::from_utf8(output.stdout).ok()?;
            return hex_decode(hex.trim()).ok();
        }
    }
    None
}

/// Store key in keychain. Returns true on success.
fn store_key_keychain(key: &[u8]) -> bool {
    let hex = hex_encode(key);
    if cfg!(target_os = "macos") {
        std::process::Command::new("security")
            .args([
                "add-generic-password",
                "-U",
                "-s",
                KEYCHAIN_SERVICE,
                "-a",
                KEYCHAIN_ACCOUNT,
                "-w",
                &hex,
            ])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    } else if cfg!(target_os = "linux") {
        use std::io::Write;
        let mut child = match std::process::Command::new("secret-tool")
            .args([
                "store",
                "--label=borg-scripts-hmac",
                "service",
                KEYCHAIN_SERVICE,
                "key",
                KEYCHAIN_ACCOUNT,
            ])
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => return false,
        };
        if let Some(ref mut stdin) = child.stdin {
            let _ = stdin.write_all(hex.as_bytes());
        }
        child.wait().map(|s| s.success()).unwrap_or(false)
    } else {
        false
    }
}

// ── HMAC Computation ──

/// Compute HMAC-SHA256 of content, returning hex string.
#[allow(clippy::expect_used)]
pub fn compute_hmac(key: &[u8], content: &[u8]) -> String {
    // HMAC-SHA256 accepts keys of any length; new_from_slice never fails for Sha256.
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC-SHA256 accepts any key size");
    mac.update(content);
    hex_encode(&mac.finalize().into_bytes())
}

/// Compute a combined HMAC for all files in a script directory.
/// Files are sorted by relative path and concatenated as `path\0content` to produce
/// a single deterministic hash for the entire directory.
#[allow(clippy::expect_used)]
pub fn compute_directory_hmac(key: &[u8], script_dir: &Path) -> Result<String> {
    let mut entries = Vec::new();
    collect_files(script_dir, script_dir, &mut entries)?;
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));

    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC-SHA256 accepts any key size");
    for (rel_path, content) in &entries {
        mac.update(rel_path.as_bytes());
        mac.update(b"\0");
        mac.update(content);
    }
    Ok(hex_encode(&mac.finalize().into_bytes()))
}

fn collect_files(base: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("Failed to read directory {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(base, &path, out)?;
        } else if path.is_file() {
            let rel = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            let content = std::fs::read(&path)
                .with_context(|| format!("Failed to read {}", path.display()))?;
            out.push((rel, content));
        }
    }
    Ok(())
}

/// Verify a script's HMAC against the stored value using constant-time comparison.
#[allow(clippy::expect_used)]
pub fn verify_script_hmac(key: &[u8], script_dir: &Path, expected_hmac: &str) -> Result<bool> {
    let expected_bytes = hex_decode(expected_hmac)?;

    let mut entries = Vec::new();
    collect_files(script_dir, script_dir, &mut entries)?;
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));

    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC-SHA256 accepts any key size");
    for (rel_path, content) in &entries {
        mac.update(rel_path.as_bytes());
        mac.update(b"\0");
        mac.update(content);
    }

    Ok(mac.verify_slice(&expected_bytes).is_ok())
}

// ── Script CRUD Operations ──

/// Parameters for creating a new script via the `manage_scripts` tool.
pub struct CreateScriptParams<'a> {
    /// Script name (alphanumeric, hyphens, underscores only).
    pub name: &'a str,
    /// Human-readable description of the script.
    pub description: &'a str,
    /// Patch DSL content to apply to the script directory.
    pub patch: &'a str,
    /// Script runtime (e.g. "python", "node", "bash").
    pub runtime: &'a str,
    /// Filename of the script entrypoint.
    pub entrypoint: &'a str,
    /// Sandbox profile name ("default", "trusted", or "custom").
    pub sandbox_profile: &'a str,
    /// Whether the script is allowed network access.
    pub network_access: bool,
    /// Filesystem paths the script may read.
    pub fs_read: &'a [String],
    /// Filesystem paths the script may write.
    pub fs_write: &'a [String],
    /// Whether the script is auto-deleted after execution.
    pub ephemeral: bool,
    /// Maximum number of scripts allowed before creation is rejected.
    pub max_scripts: usize,
}

/// Create a new script: apply patch to `~/.borg/scripts/<name>/`, compute HMAC, store in DB.
pub async fn create_script(db: &Database, params: &CreateScriptParams<'_>) -> Result<String> {
    validate_script_name(params.name)?;
    let CreateScriptParams {
        name,
        description,
        patch,
        runtime,
        entrypoint,
        sandbox_profile,
        network_access,
        fs_read,
        fs_write,
        ephemeral,
        max_scripts,
    } = params;
    // Check script limit
    let existing = db.list_scripts().await?;
    if existing.len() >= *max_scripts {
        bail!("Script limit reached ({max_scripts}). Delete unused scripts first.");
    }

    // Check name uniqueness
    if db.get_script_by_name(name).await?.is_some() {
        bail!("Script '{name}' already exists. Use update action to modify it.");
    }

    let scripts_dir = Config::scripts_dir()?;
    let script_dir = scripts_dir.join(name);
    std::fs::create_dir_all(&script_dir)?;

    // Apply patch
    match coding::apply_patch_to_dir(patch, &script_dir) {
        Ok(affected) => {
            // Compute HMAC
            let key = get_or_create_hmac_key()?;
            let hmac = compute_directory_hmac(&key, &script_dir)?;
            let now = chrono::Utc::now().timestamp();

            let new_script = NewScript {
                id: &uuid::Uuid::new_v4().to_string(),
                name,
                description,
                runtime,
                entrypoint,
                sandbox_profile,
                network_access: *network_access,
                fs_read: &serde_json::to_string(fs_read)?,
                fs_write: &serde_json::to_string(fs_write)?,
                ephemeral: *ephemeral,
                hmac: &hmac,
                created_at: now,
                updated_at: now,
            };
            db.create_script(&new_script).await?;

            Ok(format!(
                "Script '{name}' created successfully. HMAC integrity recorded.\n{}",
                affected.format_summary()
            ))
        }
        Err(e) => {
            // Clean up directory on patch failure
            let _ = std::fs::remove_dir_all(&script_dir);
            Ok(format!("Error creating script: {e}"))
        }
    }
}

/// Update an existing script: apply patch, recompute HMAC, update DB.
pub async fn update_script(db: &Database, name: &str, patch: &str) -> Result<String> {
    validate_script_name(name)?;
    let script = db
        .get_script_by_name(name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Script '{name}' not found."))?;

    let scripts_dir = Config::scripts_dir()?;
    let script_dir = scripts_dir.join(name);

    match coding::apply_patch_to_dir(patch, &script_dir) {
        Ok(affected) => {
            let key = get_or_create_hmac_key()?;
            let hmac = compute_directory_hmac(&key, &script_dir)?;
            let now = chrono::Utc::now().timestamp();
            db.update_script_hmac(&script.id, &hmac, now).await?;

            Ok(format!(
                "Script '{name}' updated. HMAC re-signed.\n{}",
                affected.format_summary()
            ))
        }
        Err(e) => Ok(format!("Error updating script: {e}")),
    }
}

/// Execute a script: verify HMAC, build sandbox policy, run, record execution.
pub async fn execute_script(config: &Config, name: &str, args_json: &str) -> Result<String> {
    validate_script_name(name)?;
    // Read script metadata and verify HMAC before the async boundary.
    // Database is not Send, so we do all DB work in a sync block first.
    let (runtime, script_dir, sandbox_policy, entrypoint_path, timeout, script_id) = {
        let db = Database::open().await?;
        let script = db
            .get_script_by_name(name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Script '{name}' not found."))?;

        let scripts_dir = Config::scripts_dir()?;
        let script_dir = scripts_dir.join(name);

        if !script_dir.exists() {
            bail!("Script directory for '{name}' not found on disk.");
        }

        // HMAC verification — refuse to run tampered scripts
        let key = get_or_create_hmac_key()?;
        if !verify_script_hmac(&key, &script_dir, &script.hmac)? {
            bail!(
                "INTEGRITY CHECK FAILED for script '{name}'. The script files have been \
                 modified outside of Borg. Refusing to execute. Delete and recreate the script."
            );
        }

        let sandbox_policy = build_script_sandbox_policy(&script, &script_dir)
            .with_blocked_paths_filtered(&config.security.blocked_paths);

        let entrypoint_path = script_dir.join(&script.entrypoint);
        if !entrypoint_path.exists() {
            bail!(
                "Entrypoint '{}' not found in script '{name}'.",
                script.entrypoint
            );
        }

        let timeout = config.scripts.default_timeout_ms;
        let script_id = script.id;
        let runtime = script.runtime;

        (
            runtime,
            script_dir,
            sandbox_policy,
            entrypoint_path,
            timeout,
            script_id,
        )
    };

    let (ok, output) = sandbox::runner::run_sandboxed_script(
        &runtime,
        &entrypoint_path,
        &script_dir,
        sandbox_policy,
        timeout,
        &[],
        name,
        args_json,
    )
    .await?;

    // Record execution (open DB again after the await)
    match Database::open().await {
        Ok(db) => {
            if let Err(e) = db.record_script_run(&script_id).await {
                tracing::warn!("Failed to record script run for '{name}': {e}");
            }
        }
        Err(e) => tracing::warn!("Failed to open DB to record script run for '{name}': {e}"),
    }

    if ok {
        Ok(output)
    } else {
        Ok(format!("Script '{name}' exited with error:\n{output}"))
    }
}

/// Delete a script: remove DB entry first, then files (safer ordering).
pub async fn delete_script(db: &Database, name: &str) -> Result<String> {
    validate_script_name(name)?;
    let script = db
        .get_script_by_name(name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Script '{name}' not found."))?;

    // Remove DB entry first (safer: orphaned files are benign, orphaned DB rows are not)
    db.delete_script(&script.id).await?;

    // Remove files
    let scripts_dir = Config::scripts_dir()?;
    let script_dir = scripts_dir.join(name);
    if script_dir.exists() {
        std::fs::remove_dir_all(&script_dir)?;
    }

    Ok(format!("Script '{name}' deleted."))
}

/// Get script details from DB.
pub async fn get_script(db: &Database, name: &str) -> Result<String> {
    validate_script_name(name)?;
    let script = db
        .get_script_by_name(name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Script '{name}' not found."))?;

    Ok(format_script_detail(&script))
}

/// List all scripts from DB.
pub async fn list_scripts(db: &Database) -> Result<String> {
    let scripts = db.list_scripts().await?;
    if scripts.is_empty() {
        return Ok("No scripts created.".to_string());
    }
    let mut out = format!("Scripts ({}):\n", scripts.len());
    for s in &scripts {
        let ephemeral_tag = if s.ephemeral { " [ephemeral]" } else { "" };
        out.push_str(&format!(
            "  {} ({}){} — {} | runs: {} | sandbox: {}\n",
            s.name, s.runtime, ephemeral_tag, s.description, s.run_count, s.sandbox_profile,
        ));
    }
    Ok(out)
}

// ── Sandbox Policy ──

fn build_script_sandbox_policy(
    script: &ScriptRow,
    script_dir: &Path,
) -> sandbox::policy::SandboxPolicy {
    use sandbox::policy::SandboxPolicy;

    let dir_str = || script_dir.to_string_lossy().to_string();

    match script.sandbox_profile.as_str() {
        "trusted" => SandboxPolicy {
            network: true,
            fs_read: vec!["/".to_string()],
            fs_write: vec!["/tmp".to_string(), dir_str()],
            ..Default::default()
        }
        .with_borg_dir_protected(),
        "custom" => {
            let fs_read: Vec<String> = serde_json::from_str(&script.fs_read).unwrap_or_default();
            let fs_write: Vec<String> = serde_json::from_str(&script.fs_write).unwrap_or_default();
            SandboxPolicy {
                network: script.network_access,
                fs_read,
                fs_write,
                ..Default::default()
            }
            .with_borg_dir_protected()
            .with_tildes_expanded()
            .with_defaults_applied()
        }
        // "default" and anything else
        _ => SandboxPolicy {
            fs_read: vec![dir_str(), "/tmp".to_string()],
            fs_write: vec!["/tmp".to_string()],
            ..Default::default()
        }
        .with_borg_dir_protected()
        .with_defaults_applied(),
    }
}

// ── Helpers ──

fn format_script_detail(s: &ScriptRow) -> String {
    let last_run = s
        .last_run_at
        .map(|ts| {
            chrono::DateTime::from_timestamp(ts, 0)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_else(|| "unknown".to_string())
        })
        .unwrap_or_else(|| "never".to_string());

    format!(
        "Script: {}\n  ID: {}\n  Description: {}\n  Runtime: {}\n  Entrypoint: {}\n  \
         Sandbox: {}\n  Network: {}\n  Ephemeral: {}\n  Runs: {}\n  Last run: {}\n  \
         Created: {}",
        s.name,
        s.id,
        s.description,
        s.runtime,
        s.entrypoint,
        s.sandbox_profile,
        s.network_access,
        s.ephemeral,
        s.run_count,
        last_run,
        chrono::DateTime::from_timestamp(s.created_at, 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_else(|| "unknown".to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn compute_hmac_is_deterministic() {
        let key = b"test-key-12345";
        let content = b"hello world";
        let h1 = compute_hmac(key, content);
        let h2 = compute_hmac(key, content);
        assert_eq!(h1, h2);
        assert!(!h1.is_empty());
    }

    #[test]
    fn compute_hmac_different_content_different_hash() {
        let key = b"test-key";
        let h1 = compute_hmac(key, b"content A");
        let h2 = compute_hmac(key, b"content B");
        assert_ne!(h1, h2);
    }

    #[test]
    fn compute_hmac_different_key_different_hash() {
        let content = b"same content";
        let h1 = compute_hmac(b"key-1", content);
        let h2 = compute_hmac(b"key-2", content);
        assert_ne!(h1, h2);
    }

    #[test]
    fn directory_hmac_is_deterministic() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.py"), b"print('hello')").unwrap();
        std::fs::write(dir.path().join("helper.py"), b"def foo(): pass").unwrap();

        let key = b"test-key";
        let h1 = compute_directory_hmac(key, dir.path()).unwrap();
        let h2 = compute_directory_hmac(key, dir.path()).unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn directory_hmac_detects_tampering() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.py"), b"print('hello')").unwrap();

        let key = b"test-key";
        let original = compute_directory_hmac(key, dir.path()).unwrap();

        // Tamper with file
        std::fs::write(dir.path().join("main.py"), b"print('HACKED')").unwrap();
        let tampered = compute_directory_hmac(key, dir.path()).unwrap();
        assert_ne!(original, tampered);
    }

    #[test]
    fn directory_hmac_detects_new_file() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.py"), b"print('hello')").unwrap();

        let key = b"test-key";
        let original = compute_directory_hmac(key, dir.path()).unwrap();

        // Add new file
        std::fs::write(dir.path().join("malicious.py"), b"evil()").unwrap();
        let with_extra = compute_directory_hmac(key, dir.path()).unwrap();
        assert_ne!(original, with_extra);
    }

    #[test]
    fn verify_script_hmac_passes_for_untouched() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.py"), b"print('hello')").unwrap();

        let key = b"test-key";
        let hmac = compute_directory_hmac(key, dir.path()).unwrap();
        assert!(verify_script_hmac(key, dir.path(), &hmac).unwrap());
    }

    #[test]
    fn verify_script_hmac_fails_for_tampered() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.py"), b"print('hello')").unwrap();

        let key = b"test-key";
        let hmac = compute_directory_hmac(key, dir.path()).unwrap();

        // Tamper
        std::fs::write(dir.path().join("main.py"), b"print('evil')").unwrap();
        assert!(!verify_script_hmac(key, dir.path(), &hmac).unwrap());
    }

    #[test]
    fn hex_roundtrip() {
        let original = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x23];
        let hex = hex_encode(&original);
        assert_eq!(hex, "deadbeef0123");
        let decoded = hex_decode(&hex).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn key_file_permissions() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_key");
        let key = generate_random_key();
        write_key_file(&path, &key).unwrap();

        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        let decoded = hex_decode(content.trim()).unwrap();
        assert_eq!(decoded, key);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::metadata(&path).unwrap().permissions();
            assert_eq!(perms.mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn build_default_sandbox_policy_denies_network() {
        let script = ScriptRow {
            id: "test".to_string(),
            name: "test".to_string(),
            description: String::new(),
            runtime: "python".to_string(),
            entrypoint: "main.py".to_string(),
            sandbox_profile: "default".to_string(),
            network_access: false,
            fs_read: "[]".to_string(),
            fs_write: "[]".to_string(),
            ephemeral: false,
            hmac: String::new(),
            created_at: 0,
            updated_at: 0,
            last_run_at: None,
            run_count: 0,
        };
        let dir = TempDir::new().unwrap();
        let policy = build_script_sandbox_policy(&script, dir.path());
        assert!(!policy.network);
    }

    #[test]
    fn build_trusted_sandbox_policy_allows_network() {
        let script = ScriptRow {
            id: "test".to_string(),
            name: "test".to_string(),
            description: String::new(),
            runtime: "python".to_string(),
            entrypoint: "main.py".to_string(),
            sandbox_profile: "trusted".to_string(),
            network_access: true,
            fs_read: "[]".to_string(),
            fs_write: "[]".to_string(),
            ephemeral: false,
            hmac: String::new(),
            created_at: 0,
            updated_at: 0,
            last_run_at: None,
            run_count: 0,
        };
        let dir = TempDir::new().unwrap();
        let policy = build_script_sandbox_policy(&script, dir.path());
        assert!(policy.network);
    }

    #[test]
    fn build_custom_sandbox_policy_uses_fields() {
        let script = ScriptRow {
            id: "test".to_string(),
            name: "test".to_string(),
            description: String::new(),
            runtime: "python".to_string(),
            entrypoint: "main.py".to_string(),
            sandbox_profile: "custom".to_string(),
            network_access: true,
            fs_read: r#"["/data"]"#.to_string(),
            fs_write: r#"["/output"]"#.to_string(),
            ephemeral: false,
            hmac: String::new(),
            created_at: 0,
            updated_at: 0,
            last_run_at: None,
            run_count: 0,
        };
        let dir = TempDir::new().unwrap();
        let policy = build_script_sandbox_policy(&script, dir.path());
        assert!(policy.network);
        assert!(policy.fs_read.iter().any(|p| p == "/data"));
        assert!(policy.fs_write.iter().any(|p| p == "/output"));
    }

    #[test]
    fn validate_name_rejects_path_traversal() {
        assert!(validate_script_name("../etc").is_err());
        assert!(validate_script_name("foo/bar").is_err());
        assert!(validate_script_name("foo\\bar").is_err());
        assert!(validate_script_name("..").is_err());
        assert!(validate_script_name(".").is_err());
        assert!(validate_script_name("").is_err());
        assert!(validate_script_name("hello world").is_err());
    }

    #[test]
    fn validate_name_accepts_valid() {
        assert!(validate_script_name("my-script").is_ok());
        assert!(validate_script_name("scrape_amazon_v2").is_ok());
        assert!(validate_script_name("test123").is_ok());
    }
}
