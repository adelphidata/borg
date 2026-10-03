use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::process::Command;

/// Commands allowed in `SecretRef::Exec`. Only well-known credential helpers
/// are permitted to prevent arbitrary command execution via config compromise.
const EXEC_ALLOWLIST: &[&str] = &[
    "security",    // macOS Keychain CLI
    "secret-tool", // GNOME/freedesktop secret store
    "pass",        // password-store
    "op",          // 1Password CLI
    "gpg",         // GnuPG
    "gpg2",        // GnuPG v2
    "age",         // age encryption
    "gopass",      // gopass
    "bw",          // Bitwarden CLI
    "vault",       // HashiCorp Vault
    "aws",         // AWS CLI (for secrets-manager)
];

/// Characters forbidden in exec args to prevent shell metacharacter injection.
/// Characters forbidden in exec args. While `Command::new` does not invoke
/// a shell (so these are passed literally), we reject them as defense-in-depth
/// in case the execution mechanism changes.
const FORBIDDEN_ARG_CHARS: &[char] = &['`', '$', '|', ';', '&', '\n', '\r', '(', ')', '>', '<'];

fn validate_exec_command(command: &str, args: &[String]) -> Result<()> {
    let basename = std::path::Path::new(command)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(command);

    if !EXEC_ALLOWLIST.contains(&basename) {
        bail!(
            "Command '{}' is not in the exec allowlist. Allowed: {}",
            command,
            EXEC_ALLOWLIST.join(", ")
        );
    }

    for arg in args {
        if arg.chars().any(|c| FORBIDDEN_ARG_CHARS.contains(&c)) {
            bail!("Exec argument contains forbidden shell metacharacters: {arg}");
        }
    }
    Ok(())
}

fn validate_file_path(expanded: &str) -> Result<std::path::PathBuf> {
    let canonical = std::fs::canonicalize(expanded)
        .with_context(|| format!("Failed to resolve secret file path: {expanded}"))?;

    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;

    let mut trusted = canonical.starts_with(&home);

    // Allow temp directories (needed for tests and tmpfile-based secrets).
    // Canonicalize temp_dir too since on macOS /var -> /private/var.
    if !trusted {
        if let Ok(canon_tmp) = std::fs::canonicalize(std::env::temp_dir()) {
            trusted = canonical.starts_with(canon_tmp);
        }
    }

    if !trusted {
        bail!(
            "Secret file path '{}' resolves to '{}' which is outside the home directory",
            expanded,
            canonical.display()
        );
    }

    Ok(canonical)
}

/// A reference to a secret value that can be resolved at runtime.
/// Supports multiple backends: environment variables, files, and external commands.
///
/// `Debug` is hand-rolled (see below) — `Exec.args` can plausibly contain a
/// raw passphrase if a user wires `pass`/`bw`/`security` with inline tokens,
/// and `File.path` can leak filesystem layout. The masked impl prints only
/// the variant tag plus a coarse identifier so `{:?}` is safe in logs.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "source")]
pub enum SecretRef {
    /// Read from an environment variable.
    #[serde(rename = "env")]
    Env { var: String },

    /// Read from a file. Optionally extract a JSON key.
    #[serde(rename = "file")]
    File {
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },

    /// Execute a command and use its stdout as the secret.
    #[serde(rename = "exec")]
    Exec {
        command: String,
        #[serde(default)]
        args: Vec<String>,
    },

    /// Look up a credential in the OS keychain (macOS Keychain / Linux secret-tool).
    #[serde(rename = "keychain")]
    Keychain { service: String, account: String },

    /// The secret itself, stored in the settings table. This is what the
    /// desktop and web onboarding write when no keychain is reachable from
    /// the daemon; the row already sits behind the data dir's 0700 perms.
    #[serde(rename = "literal")]
    Literal { value: String },
}

impl SecretRef {
    /// What `settings list`, `borg settings get` and `Admin.GetSettings` may
    /// show for a field holding a secret reference.
    ///
    /// Every other variant only names where the secret lives, so its JSON is
    /// safe to print and round-trips back through `set`. [`SecretRef::Literal`]
    /// carries the credential itself, and `GetSettings` reaches a browser, so
    /// it is reported as set and nothing more.
    pub fn display_value(&self) -> String {
        match self {
            SecretRef::Literal { .. } => LITERAL_PLACEHOLDER.to_string(),
            other => serde_json::to_string(other).unwrap_or_default(),
        }
    }
}

/// Stands in for a stored key wherever a settings value is displayed. Not
/// valid JSON for a `SecretRef`, so it cannot be written back by accident.
pub const LITERAL_PLACEHOLDER: &str = "(set)";

impl std::fmt::Debug for SecretRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecretRef::Env { var } => write!(f, "SecretRef::Env({var})"),
            SecretRef::File { .. } => write!(f, "SecretRef::File(<redacted path>)"),
            SecretRef::Exec { command, .. } => {
                // Show the command bin only — args may contain a passphrase.
                write!(f, "SecretRef::Exec({command} <redacted args>)")
            }
            SecretRef::Keychain { service, account } => {
                write!(f, "SecretRef::Keychain({service}/{account})")
            }
            SecretRef::Literal { .. } => write!(f, "SecretRef::Literal(<redacted>)"),
        }
    }
}

/// Routing JSON persisted at `credentials.{provider}` by `Admin.SetApiKey`.
///
/// Tagged with `type` (not `source` like [`SecretRef`]) because the cli and
/// the daemon already agree on this discriminator; the conversion below is
/// how the LLM config resolves the row without a second parser.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ApiKeyRouting {
    /// The key itself.
    Literal { value: String },
    /// Where in the OS keychain the key lives.
    Keychain { service: String, account: String },
}

impl std::fmt::Debug for ApiKeyRouting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        SecretRef::from(self.clone()).fmt(f)
    }
}

impl From<ApiKeyRouting> for SecretRef {
    fn from(routing: ApiKeyRouting) -> Self {
        match routing {
            ApiKeyRouting::Literal { value } => SecretRef::Literal { value },
            ApiKeyRouting::Keychain { service, account } => {
                SecretRef::Keychain { service, account }
            }
        }
    }
}

/// Build a keychain secret reference using Borg's canonical service/account
/// addressing for provider API keys.
pub fn provider_api_key_secret_ref(provider_id: &str) -> SecretRef {
    SecretRef::Keychain {
        service: format!("borg-{provider_id}"),
        account: "borg".to_string(),
    }
}

/// Check that a command succeeded and return its stdout as a trimmed string.
fn extract_secret(output: std::process::Output, fail_context: &str) -> Result<String> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{fail_context}: {}", humanize_keychain_error(&stderr));
    }
    String::from_utf8(output.stdout)
        .map(|s| s.trim().to_string())
        .context("Command output is not valid UTF-8")
}

/// The macOS `security` CLI prints "The specified item could not be found in
/// the keychain" both when the item is genuinely absent AND when it exists but
/// the calling process is not on its ACL. The latter is the common failure
/// mode for the daemon (items are written by the TUI with a binary-scoped ACL
/// that pre-fix entries lack). Rewrite that specific stderr into an actionable
/// remediation hint so users know to reinstall the channel from the plugins
/// popup rather than chasing a phantom "missing item".
pub(crate) fn humanize_keychain_error(stderr: &str) -> String {
    if stderr.contains("could not be found in the keychain") {
        format!(
            "{} (hint: the keychain item may exist but be unreadable by this process — reinstall the channel from the plugins popup to refresh its ACL)",
            stderr.trim()
        )
    } else {
        stderr.trim().to_string()
    }
}

impl SecretRef {
    /// Resolve the secret reference to its plaintext value.
    pub fn resolve(&self) -> Result<String> {
        match self {
            SecretRef::Literal { value } => Ok(value.clone()),

            SecretRef::Env { var } => {
                std::env::var(var).with_context(|| format!("Environment variable {var} not set"))
            }

            SecretRef::File { path, key } => {
                let expanded = crate::util::unix_ext::expand_tilde(path);
                let canonical = validate_file_path(&expanded)?;
                let content = std::fs::read_to_string(&canonical)
                    .with_context(|| format!("Failed to read secret file: {path}"))?;

                match key {
                    Some(json_key) => {
                        let value: serde_json::Value = serde_json::from_str(&content)
                            .with_context(|| format!("Failed to parse {path} as JSON"))?;
                        value[json_key]
                            .as_str()
                            .map(std::string::ToString::to_string)
                            .with_context(|| {
                                format!("Key '{json_key}' not found or not a string in {path}")
                            })
                    }
                    None => Ok(content.trim().to_string()),
                }
            }

            SecretRef::Exec { command, args } => {
                validate_exec_command(command, args)?;

                let output = Command::new(command)
                    .args(args)
                    .output()
                    .with_context(|| format!("Failed to execute: {command}"))?;

                let exit_code = output.status.code().unwrap_or(-1);
                extract_secret(
                    output,
                    &format!("Secret command `{command}` failed (exit {exit_code})"),
                )
            }

            SecretRef::Keychain { service, account } => {
                if cfg!(target_os = "macos") {
                    let output = Command::new("security")
                        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
                        .output()
                        .with_context(|| format!("Failed to query macOS Keychain for service={service} account={account}"))?;
                    extract_secret(
                        output,
                        &format!("Keychain lookup failed for service={service} account={account}"),
                    )
                } else if cfg!(target_os = "linux") {
                    // Borg's keychain writer files the account under the
                    // `account` attribute; older entries used `key`.
                    let lookup = |attr: &str| {
                        Command::new("secret-tool")
                            .args(["lookup", "service", service, attr, account])
                            .output()
                            .with_context(|| {
                                format!(
                                    "Failed to query secret-tool for service={service} {attr}={account}"
                                )
                            })
                    };
                    let fail_context = format!(
                        "secret-tool lookup failed for service={service} account={account}"
                    );
                    match extract_secret(lookup("account")?, &fail_context) {
                        Ok(secret) if !secret.is_empty() => Ok(secret),
                        _ => extract_secret(lookup("key")?, &fail_context),
                    }
                } else if cfg!(target_os = "windows") {
                    let entry = plugins::keychain::windows_entry(service, account).with_context(|| {
                        format!(
                            "Failed to open Windows Credential Manager entry for service={service} account={account}"
                        )
                    })?;
                    entry
                        .get_password()
                        .with_context(|| {
                            format!(
                                "Windows Credential Manager lookup failed for service={service} account={account}"
                            )
                        })
                } else {
                    bail!(
                        "Keychain lookup is not supported on this platform (only macOS, Linux, and Windows)"
                    )
                }
            }
        }
    }
}

/// Try to resolve the first successful key from a list of SecretRefs.
/// Returns the resolved key and the index that succeeded.
pub fn resolve_first(refs: &[SecretRef]) -> Result<(String, usize)> {
    let mut last_err = None;
    for (i, secret_ref) in refs.iter().enumerate() {
        match secret_ref.resolve() {
            Ok(key) if !key.is_empty() => return Ok((key, i)),
            Ok(_) => {
                last_err = Some(anyhow::anyhow!(
                    "SecretRef at index {i} resolved to empty string"
                ));
            }
            Err(e) => {
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("No secret references provided")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_secret_resolves_to_itself_but_never_prints() {
        let sr = SecretRef::Literal {
            value: "sk-live-DO-NOT-LEAK".into(),
        };
        assert_eq!(
            sr.resolve().expect("literal resolves"),
            "sk-live-DO-NOT-LEAK"
        );
        let dbg = format!("{sr:?}");
        assert!(
            !dbg.contains("sk-live"),
            "literal leaked through Debug: {dbg}"
        );

        // The serde tag is what the settings table stores; a renamed tag
        // would orphan every key already written.
        let json = serde_json::to_string(&sr).expect("serialize");
        assert_eq!(
            json,
            r#"{"source":"literal","value":"sk-live-DO-NOT-LEAK"}"#
        );
        let back: SecretRef = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.resolve().expect("round trip"), "sk-live-DO-NOT-LEAK");
    }

    /// `settings list` and `Admin.GetSettings` render whatever the extractor
    /// returns, and `GetSettings` answers a browser. A literal key must not
    /// travel that path; every other variant only names a location.
    #[test]
    fn only_a_location_is_ever_displayed_for_a_secret() {
        let literal = SecretRef::Literal {
            value: "sk-live-DO-NOT-LEAK".into(),
        };
        assert_eq!(literal.display_value(), LITERAL_PLACEHOLDER);
        assert!(
            serde_json::from_str::<SecretRef>(&literal.display_value()).is_err(),
            "the placeholder must not deserialize back into a key"
        );

        let env = SecretRef::Env {
            var: "OPENAI_API_KEY".into(),
        };
        assert_eq!(
            env.display_value(),
            r#"{"source":"env","var":"OPENAI_API_KEY"}"#
        );
        assert_eq!(
            serde_json::from_str::<SecretRef>(&env.display_value())
                .expect("still round-trips")
                .display_value(),
            env.display_value()
        );
    }

    #[test]
    fn api_key_routing_converts_to_the_matching_secret_ref() {
        // The daemon writes routing with a `type` tag; the config must read
        // exactly that shape or the fold in `Config::load_from_db` is dead.
        let literal: ApiKeyRouting =
            serde_json::from_str(r#"{"type":"literal","value":"k"}"#).expect("literal");
        assert_eq!(SecretRef::from(literal).resolve().expect("value"), "k");
        let keychain: ApiKeyRouting =
            serde_json::from_str(r#"{"type":"keychain","service":"s","account":"a"}"#)
                .expect("keychain");
        match SecretRef::from(keychain) {
            SecretRef::Keychain { service, account } => {
                assert_eq!((service, account), ("s".into(), "a".into()))
            }
            other => panic!("expected keychain, got {other:?}"),
        }
        let dbg = format!(
            "{:?}",
            ApiKeyRouting::Literal {
                value: "hidden".into()
            }
        );
        assert!(!dbg.contains("hidden"), "routing Debug leaked: {dbg}");
    }

    #[test]
    fn debug_redacts_exec_args_and_file_path() {
        // Exec.args can carry a raw passphrase; File.path can leak FS layout.
        // Both must be masked when SecretRef is Debug-formatted (e.g. into
        // a tracing! macro by accident).
        let sr_exec = SecretRef::Exec {
            command: "pass".into(),
            args: vec!["my-secret-passphrase-DO-NOT-LEAK".into()],
        };
        let dbg = format!("{sr_exec:?}");
        assert!(
            !dbg.contains("my-secret-passphrase"),
            "Exec args leaked: {dbg}"
        );
        assert!(dbg.contains("redacted"), "expected marker: {dbg}");

        let sr_file = SecretRef::File {
            path: "/Users/leakme/.borg/secrets/key.txt".into(),
            key: None,
        };
        let dbg = format!("{sr_file:?}");
        assert!(!dbg.contains("/Users/leakme"), "File path leaked: {dbg}");
    }

    #[test]
    fn humanize_keychain_not_found_adds_remediation_hint() {
        // The actual stderr emitted by macOS `security` when the item exists
        // but the caller isn't on the ACL (same text as genuinely-missing).
        let stderr =
            "security: SecKeychainSearchCopyNext: The specified item could not be found in the keychain.\n";
        let humanized = humanize_keychain_error(stderr);
        assert!(
            humanized.contains("reinstall"),
            "humanized error should suggest reinstalling: {humanized}"
        );
        assert!(
            humanized.contains("plugins popup"),
            "humanized error should point to the plugins popup: {humanized}"
        );
    }

    #[test]
    fn humanize_keychain_error_passthrough_for_other_stderr() {
        let stderr = "security: some other error\n";
        let humanized = humanize_keychain_error(stderr);
        assert_eq!(humanized, "security: some other error");
        assert!(!humanized.contains("reinstall"));
    }

    #[test]
    fn resolve_env_var() {
        let var_name = "BORG_TEST_SECRET_REF_ENV";
        std::env::set_var(var_name, "test-secret-value");
        let sr = SecretRef::Env {
            var: var_name.to_string(),
        };
        let val = sr.resolve().expect("should resolve");
        assert_eq!(val, "test-secret-value");
        std::env::remove_var(var_name);
    }

    #[test]
    fn resolve_env_var_missing() {
        let sr = SecretRef::Env {
            var: "BORG_TEST_SECRET_REF_MISSING".to_string(),
        };
        assert!(sr.resolve().is_err());
    }

    #[test]
    fn resolve_file_plaintext() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let file_path = dir.path().join("secret.txt");
        std::fs::write(&file_path, "my-api-key\n").expect("write");

        let sr = SecretRef::File {
            path: file_path.to_string_lossy().to_string(),
            key: None,
        };
        let val = sr.resolve().expect("should resolve");
        assert_eq!(val, "my-api-key");
    }

    #[test]
    fn resolve_file_json_key() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let file_path = dir.path().join("secrets.json");
        std::fs::write(&file_path, r#"{"api_key": "json-secret-123"}"#).expect("write");

        let sr = SecretRef::File {
            path: file_path.to_string_lossy().to_string(),
            key: Some("api_key".to_string()),
        };
        let val = sr.resolve().expect("should resolve");
        assert_eq!(val, "json-secret-123");
    }

    #[test]
    fn resolve_file_json_missing_key() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let file_path = dir.path().join("secrets.json");
        std::fs::write(&file_path, r#"{"other": "value"}"#).expect("write");

        let sr = SecretRef::File {
            path: file_path.to_string_lossy().to_string(),
            key: Some("api_key".to_string()),
        };
        assert!(sr.resolve().is_err());
    }

    #[test]
    fn resolve_file_missing() {
        let sr = SecretRef::File {
            path: "/tmp/borg_test_nonexistent_secret_file.txt".to_string(),
            key: None,
        };
        assert!(sr.resolve().is_err());
    }

    #[test]
    fn resolve_exec_blocked_command() {
        // "echo" is not in the exec allowlist
        let sr = SecretRef::Exec {
            command: "echo".to_string(),
            args: vec!["exec-secret-value".to_string()],
        };
        let err = sr.resolve().unwrap_err();
        assert!(
            err.to_string().contains("not in the exec allowlist"),
            "expected allowlist error, got: {err}"
        );
    }

    #[test]
    fn resolve_exec_nonexistent_command() {
        let sr = SecretRef::Exec {
            command: "borg_nonexistent_command_12345".to_string(),
            args: vec![],
        };
        let err = sr.resolve().unwrap_err();
        assert!(err.to_string().contains("not in the exec allowlist"));
    }

    #[test]
    fn exec_metachar_in_args_rejected() {
        let cases = vec![
            (
                "security",
                vec!["find-generic-password; rm -rf /".to_string()],
            ),
            ("pass", vec!["show".to_string(), "$(whoami)".to_string()]),
            ("op", vec!["`id`".to_string()]),
            (
                "gpg",
                vec!["--decrypt".to_string(), "key\ninjected".to_string()],
            ),
            (
                "vault",
                vec!["read".to_string(), "secret | cat".to_string()],
            ),
        ];
        for (cmd, args) in cases {
            let sr = SecretRef::Exec {
                command: cmd.to_string(),
                args,
            };
            let err = sr.resolve().unwrap_err();
            assert!(
                err.to_string().contains("forbidden shell metacharacters"),
                "expected metachar error for {cmd}, got: {err}"
            );
        }
    }

    #[test]
    fn exec_allowed_by_basename() {
        // validate_exec_command should accept allowlisted commands
        assert!(validate_exec_command("security", &[]).is_ok());
        assert!(validate_exec_command("/usr/bin/security", &[]).is_ok());
        assert!(validate_exec_command("pass", &["show".to_string(), "email".to_string()]).is_ok());
        assert!(validate_exec_command("op", &["read".to_string()]).is_ok());
    }

    #[test]
    fn exec_blocked_by_basename() {
        assert!(validate_exec_command("rm", &[]).is_err());
        assert!(validate_exec_command("curl", &[]).is_err());
        assert!(validate_exec_command("/bin/sh", &[]).is_err());
        assert!(validate_exec_command("python3", &[]).is_err());
    }

    #[test]
    fn resolve_first_picks_first_success() {
        let var_name = "BORG_TEST_RESOLVE_FIRST";
        std::env::set_var(var_name, "first-key");
        let refs = vec![
            SecretRef::Env {
                var: "NONEXISTENT_VAR_12345".to_string(),
            },
            SecretRef::Env {
                var: var_name.to_string(),
            },
        ];
        let (val, idx) = resolve_first(&refs).expect("should resolve");
        assert_eq!(val, "first-key");
        assert_eq!(idx, 1);
        std::env::remove_var(var_name);
    }

    #[test]
    fn resolve_first_all_fail() {
        let refs = vec![
            SecretRef::Env {
                var: "NONEXISTENT_A_12345".to_string(),
            },
            SecretRef::Env {
                var: "NONEXISTENT_B_12345".to_string(),
            },
        ];
        assert!(resolve_first(&refs).is_err());
    }

    #[test]
    fn resolve_first_empty_list() {
        assert!(resolve_first(&[]).is_err());
    }

    #[test]
    fn serde_roundtrip_env() {
        let sr = SecretRef::Env {
            var: "MY_KEY".to_string(),
        };
        let toml_str = toml::to_string(&sr).expect("serialize");
        let parsed: SecretRef = toml::from_str(&toml_str).expect("deserialize");
        if let SecretRef::Env { var } = parsed {
            assert_eq!(var, "MY_KEY");
        } else {
            panic!("expected Env variant");
        }
    }

    #[test]
    fn serde_roundtrip_exec() {
        let sr = SecretRef::Exec {
            command: "security".to_string(),
            args: vec!["find-generic-password".to_string(), "-w".to_string()],
        };
        let toml_str = toml::to_string(&sr).expect("serialize");
        let parsed: SecretRef = toml::from_str(&toml_str).expect("deserialize");
        if let SecretRef::Exec { command, args } = parsed {
            assert_eq!(command, "security");
            assert_eq!(args.len(), 2);
        } else {
            panic!("expected Exec variant");
        }
    }

    #[test]
    fn deserialize_inline_table_env() {
        // This is the format used in config.toml inline tables
        let toml_str = r#"secret = { source = "env", var = "MY_API_KEY" }"#;
        #[derive(Deserialize)]
        struct Wrapper {
            secret: SecretRef,
        }
        let w: Wrapper = toml::from_str(toml_str).expect("parse");
        if let SecretRef::Env { var } = w.secret {
            assert_eq!(var, "MY_API_KEY");
        } else {
            panic!("expected Env variant");
        }
    }

    #[test]
    fn deserialize_inline_table_exec() {
        let toml_str = r#"secret = { source = "exec", command = "security", args = ["find-generic-password", "-s", "borg", "-w"] }"#;
        #[derive(Deserialize)]
        struct Wrapper {
            secret: SecretRef,
        }
        let w: Wrapper = toml::from_str(toml_str).expect("parse");
        if let SecretRef::Exec { command, args } = w.secret {
            assert_eq!(command, "security");
            assert_eq!(args.len(), 4);
        } else {
            panic!("expected Exec variant");
        }
    }

    #[test]
    fn serde_roundtrip_keychain() {
        let sr = SecretRef::Keychain {
            service: "borg-messaging-telegram".to_string(),
            account: "borg-TELEGRAM_BOT_TOKEN".to_string(),
        };
        let toml_str = toml::to_string(&sr).expect("serialize");
        let parsed: SecretRef = toml::from_str(&toml_str).expect("deserialize");
        if let SecretRef::Keychain { service, account } = parsed {
            assert_eq!(service, "borg-messaging-telegram");
            assert_eq!(account, "borg-TELEGRAM_BOT_TOKEN");
        } else {
            panic!("expected Keychain variant");
        }
    }

    #[test]
    fn deserialize_inline_table_keychain() {
        let toml_str =
            r#"secret = { source = "keychain", service = "borg-svc", account = "borg-acct" }"#;
        #[derive(Deserialize)]
        struct Wrapper {
            secret: SecretRef,
        }
        let w: Wrapper = toml::from_str(toml_str).expect("parse");
        if let SecretRef::Keychain { service, account } = w.secret {
            assert_eq!(service, "borg-svc");
            assert_eq!(account, "borg-acct");
        } else {
            panic!("expected Keychain variant");
        }
    }

    #[test]
    fn provider_api_key_secret_ref_uses_canonical_service_and_account() {
        let secret_ref = provider_api_key_secret_ref("openrouter");
        match secret_ref {
            SecretRef::Keychain { service, account } => {
                assert_eq!(service, "borg-openrouter");
                assert_eq!(account, "borg");
            }
            other => panic!("expected Keychain variant, got {other:?}"),
        }
    }

    #[test]
    fn resolve_keychain_missing_entry() {
        let sr = SecretRef::Keychain {
            service: "borg-nonexistent-test-service-12345".to_string(),
            account: "borg-nonexistent-test-account-12345".to_string(),
        };
        assert!(sr.resolve().is_err());
    }

    #[test]
    fn resolve_file_traversal_rejected() {
        // /etc/hosts exists on macOS/Linux and is outside home dir
        let sr = SecretRef::File {
            path: "/etc/hosts".to_string(),
            key: None,
        };
        let err = sr.resolve().unwrap_err();
        assert!(
            err.to_string().contains("outside the home directory"),
            "expected path traversal error, got: {err}"
        );
    }

    #[test]
    fn resolve_file_relative_traversal_rejected() {
        let sr = SecretRef::File {
            path: "../../../etc/hosts".to_string(),
            key: None,
        };
        // Either fails to canonicalize or is rejected as outside home
        assert!(sr.resolve().is_err());
    }

    #[test]
    fn resolve_file_in_tempdir_ok() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let file_path = dir.path().join("ok-secret.txt");
        std::fs::write(&file_path, "temp-secret\n").expect("write");

        let sr = SecretRef::File {
            path: file_path.to_string_lossy().to_string(),
            key: None,
        };
        let val = sr.resolve().expect("should resolve from temp dir");
        assert_eq!(val, "temp-secret");
    }

    #[test]
    fn validate_file_path_rejects_system_paths() {
        assert!(validate_file_path("/etc/passwd").is_err());
        assert!(validate_file_path("/var/log/system.log").is_err());
    }
}
