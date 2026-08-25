use crate::catalog::PluginDef;

/// Result of a verification check.
#[derive(Debug, Clone)]
pub struct VerifyResult {
    /// Plugin identifier that was checked.
    pub id: String,
    /// Whether verification passed.
    pub ok: bool,
    /// Human-readable status message.
    pub message: String,
}

/// Verify an installed plugin: required binaries present and required
/// credentials resolvable (env var or keychain). Full API health checks
/// (e.g. Telegram getMe) are done per-integration when credentials exist.
pub fn verify(def: &PluginDef, data_dir: &std::path::Path) -> VerifyResult {
    let id = def.id.to_string();

    for bin in def.required_bins {
        if which::which(bin).is_err() {
            return VerifyResult {
                id,
                ok: false,
                message: format!("Required binary not found: {bin}"),
            };
        }
    }

    // Native integrations have no template files — only credential checks apply.
    if !def.is_native && !crate::installer::is_installed(def, data_dir) {
        return VerifyResult {
            id,
            ok: false,
            message: "Integration files not found".to_string(),
        };
    }

    for cred in def.required_credentials {
        if cred.is_optional {
            continue;
        }

        let has_env = std::env::var(cred.key)
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        let has_keychain = check_keychain_credential(def, cred.key);

        if !has_env && !has_keychain {
            return VerifyResult {
                id,
                ok: false,
                message: format!(
                    "Credential {} not found in environment or keychain",
                    cred.key
                ),
            };
        }
    }

    VerifyResult {
        id,
        ok: true,
        message: "All checks passed".to_string(),
    }
}

/// Verify all installed plugins and return results.
pub fn verify_all(installed_ids: &[String], data_dir: &std::path::Path) -> Vec<VerifyResult> {
    installed_ids
        .iter()
        .filter_map(|id| crate::catalog::find_by_id(id).map(|def| verify(def, data_dir)))
        .collect()
}

fn check_keychain_credential(def: &PluginDef, key: &str) -> bool {
    let service = def.service_name();
    let account = crate::installer::credential_account(key);
    crate::keychain::check(&service, &account)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::CATALOG;
    use crate::{Category, CredentialSpec, Platform, PluginKind};

    fn native_def(id: &'static str, creds: &'static [CredentialSpec]) -> PluginDef {
        PluginDef {
            id,
            name: "Fixture",
            category: Category::Channels,
            kind: PluginKind::Channel,
            description: "test fixture",
            required_credentials: creds,
            required_bins: &[],
            templates: &[],
            platform: Platform::All,
            is_native: true,
            post_install: None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn verify_treats_empty_env_var_as_missing_credential() {
        // `KEY=""` in the environment (a common .envrc placeholder) is not a
        // usable credential; counting it as present would report a channel as
        // healthy that can never authenticate.
        const CREDS: &[CredentialSpec] = &[CredentialSpec {
            key: "BORG_TEST_VERIFY_EMPTY",
            label: "Token",
            help_url: "",
            is_optional: false,
        }];
        let _lock = crate::test_support::env_lock();
        let _shim = crate::test_support::KeychainShim::failing();
        let _env = crate::test_support::EnvVarGuard::set("BORG_TEST_VERIFY_EMPTY", "");

        let result = verify(&native_def("test/empty-env", CREDS), &std::env::temp_dir());
        assert!(!result.ok, "empty env var must not satisfy verification");
        assert!(
            result.message.contains("BORG_TEST_VERIFY_EMPTY"),
            "message must name the missing credential: {}",
            result.message
        );
    }

    #[cfg(unix)]
    #[test]
    fn verify_passes_with_env_credential_alone() {
        // Env var OR keychain suffices — a user who exports the token but
        // never stored it in the keychain must still verify clean.
        const CREDS: &[CredentialSpec] = &[CredentialSpec {
            key: "BORG_TEST_VERIFY_SET",
            label: "Token",
            help_url: "",
            is_optional: false,
        }];
        let _lock = crate::test_support::env_lock();
        let _shim = crate::test_support::KeychainShim::failing();
        let _env = crate::test_support::EnvVarGuard::set("BORG_TEST_VERIFY_SET", "tok-123");

        let result = verify(&native_def("test/env-only", CREDS), &std::env::temp_dir());
        assert!(
            result.ok,
            "env credential alone must satisfy verification: {}",
            result.message
        );
        assert_eq!(result.message, "All checks passed");
    }

    #[cfg(unix)]
    #[test]
    fn verify_passes_with_keychain_credential_alone() {
        // The mirror case: token stored in the keychain, nothing exported.
        const CREDS: &[CredentialSpec] = &[CredentialSpec {
            key: "BORG_TEST_VERIFY_KC",
            label: "Token",
            help_url: "",
            is_optional: false,
        }];
        let _lock = crate::test_support::env_lock();
        let _shim = crate::test_support::KeychainShim::find_succeeds();
        let _env = crate::test_support::EnvVarGuard::unset("BORG_TEST_VERIFY_KC");

        // id "shim-svc" -> service_name "borg-shim-svc", the service the fake
        // keychain helper answers for.
        let result = verify(&native_def("shim-svc", CREDS), &std::env::temp_dir());
        assert!(
            result.ok,
            "keychain credential alone must satisfy verification: {}",
            result.message
        );
    }

    #[test]
    fn verify_all_resolves_known_ids_and_skips_unknown() {
        // Stale installed-plugin rows (a catalog entry that was removed) must
        // be skipped, not crash or block verification of the rest.
        let tmp = std::env::temp_dir().join("borg-verify-all-test");
        let ids = vec!["messaging/whatsapp".to_string(), "not/a-plugin".to_string()];
        let results = verify_all(&ids, &tmp);
        assert_eq!(
            results.len(),
            1,
            "one result per known id, unknown ids skipped"
        );
        assert_eq!(results[0].id, "messaging/whatsapp");
        assert!(
            !results[0].ok,
            "whatsapp is not installed under this temp dir"
        );
    }

    #[test]
    fn verify_template_plugin_fails_on_missing_files_not_some_other_reason() {
        // whatsapp is a template (non-native) plugin: with no files on disk
        // verify must fail at the install-files gate. Asserting the exact
        // reason guards against the test passing for an unrelated failure
        // (e.g. a missing binary check short-circuiting first).
        let def = CATALOG
            .iter()
            .find(|c| c.id == "messaging/whatsapp")
            .expect("whatsapp in catalog");
        assert!(
            !def.is_native,
            "fixture assumption: whatsapp must be a template plugin"
        );
        let tmp = std::env::temp_dir().join("borg-verify-test");
        let result = verify(def, &tmp);
        assert!(!result.ok);
        assert_eq!(result.id, "messaging/whatsapp");
        assert_eq!(
            result.message, "Integration files not found",
            "uninstalled template plugin must fail at the files gate"
        );
    }

    #[test]
    fn verify_native_plugin_fails_naming_the_missing_credential() {
        // Native plugins skip the files gate, so an absent credential is the
        // only failure path. Assert the message names the specific missing
        // credential, not just "something failed". (discord avoids env races.)
        let def = CATALOG
            .iter()
            .find(|c| c.id == "messaging/discord")
            .expect("discord in catalog");
        unsafe {
            std::env::remove_var("DISCORD_BOT_TOKEN");
            std::env::remove_var("DISCORD_PUBLIC_KEY");
        }
        let tmp = std::env::temp_dir().join("borg-verify-native");
        let result = verify(def, &tmp);
        assert!(!result.ok);
        assert_eq!(result.id, "messaging/discord");
        // The first required, non-optional credential must be named in the
        // message together with the "not found" cause.
        let first_required = def
            .required_credentials
            .iter()
            .find(|c| !c.is_optional)
            .expect("discord must declare a required credential");
        assert!(
            result.message.contains(first_required.key) && result.message.contains("not found"),
            "message must name the missing credential and the cause: {}",
            result.message
        );
    }
}
