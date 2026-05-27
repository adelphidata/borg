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

/// Verify that an installed plugin is working.
///
/// For now, this checks that:
/// 1. Required binaries are available
/// 2. Required credentials are resolvable (env var or keychain)
///
/// Full API health checks (e.g., calling Telegram getMe) are done per-integration
/// when the credentials are available.
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
    let account = format!("borg-{key}");
    crate::keychain::check(&service, &account)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::CATALOG;

    #[test]
    fn verify_uninstalled_fails() {
        let def = CATALOG
            .iter()
            .find(|c| c.id == "messaging/whatsapp")
            .expect("whatsapp in catalog");
        let tmp = std::env::temp_dir().join("borg-verify-test");
        let result = verify(def, &tmp);
        assert!(!result.ok);
    }

    #[test]
    fn verify_native_without_creds_fails() {
        // Use discord to avoid env var races with other tests
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
    }

    #[test]
    fn verify_native_message_mentions_credentials() {
        // Use discord to avoid env var races with other tests
        let def = CATALOG
            .iter()
            .find(|c| c.id == "messaging/discord")
            .expect("discord in catalog");
        unsafe {
            std::env::remove_var("DISCORD_BOT_TOKEN");
            std::env::remove_var("DISCORD_PUBLIC_KEY");
        }
        let tmp = std::env::temp_dir().join("borg-verify-native-msg");
        let result = verify(def, &tmp);
        assert!(!result.ok);
        assert!(
            result.message.contains("not found"),
            "message should mention 'not found': {}",
            result.message
        );
    }
}
