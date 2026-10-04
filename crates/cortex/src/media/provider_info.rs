//! What a swappable voice provider (speech, transcription, telephony) declares
//! about itself.
//!
//! Every capability keeps its own registry of [`ProviderInfo`] rows next to
//! the implementations. The engine reads a row for defaults and credentials,
//! and the daemon serves the same rows to the Settings UI. So a provider list
//! exists once, in Rust, and never as a constant in TypeScript.

use crate::config::Config;

/// Static description of one provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderInfo {
    /// Id written in config rows (`tts.models[].provider`, `telephony.provider`).
    pub id: &'static str,
    /// Human name for pickers.
    pub label: &'static str,
    /// Credential names tried in order for the API key. Empty means the
    /// provider needs no key.
    pub key_env: &'static [&'static str],
    /// Where an operator gets a key; empty when there is none to get.
    pub key_url: &'static str,
    /// Default API endpoint; empty for providers that never touch the network.
    pub base_url: &'static str,
    /// Model used when a row names none; empty when the provider has no models.
    pub default_model: &'static str,
    /// Voice used when neither the call nor the row names one; empty when the
    /// provider has no voice catalog.
    pub default_voice: &'static str,
    /// True for providers that shell out to an operator-supplied `command`.
    pub requires_command: bool,
    /// What the provider calls its secret, when that is not "API key"
    /// (Twilio's is an Auth Token). Empty means "<label> API key".
    pub key_label: &'static str,
}

impl ProviderInfo {
    /// A keyless, networked-or-local provider with no defaults; rows fill in
    /// what applies.
    pub const fn new(id: &'static str, label: &'static str) -> Self {
        Self {
            id,
            label,
            key_env: &[],
            key_url: "",
            base_url: "",
            default_model: "",
            default_voice: "",
            requires_command: false,
            key_label: "",
        }
    }

    /// The name a settings field shows for this provider's secret.
    pub fn key_name(&self) -> String {
        if self.key_label.is_empty() {
            format!("{} API key", self.label)
        } else {
            self.key_label.to_string()
        }
    }

    /// Whether the provider needs an API key at all.
    pub fn needs_key(&self) -> bool {
        !self.key_env.is_empty()
    }

    /// Resolve this provider's API key.
    ///
    /// A row-level `override_env` replaces the conventional names. Every name
    /// is tried (credential store, then environment) before the key saved in
    /// Settings; see [`Config::resolve_provider_key`].
    /// Returns `Some("")` for keyless providers, `None` when a key is needed
    /// and none is configured.
    pub fn api_key(&self, config: &Config, override_env: Option<&str>) -> Option<String> {
        if !self.needs_key() {
            return Some(String::new());
        }
        match override_env.filter(|v| !v.is_empty()) {
            Some(name) => config.resolve_provider_key(self.id, &[name]),
            None => config.resolve_provider_key(self.id, self.key_env),
        }
    }

    /// The row's endpoint override, or the provider default.
    pub fn base_url_or(&self, row_override: Option<&str>) -> String {
        row_override
            .filter(|u| !u.is_empty())
            .unwrap_or(self.base_url)
            .trim_end_matches('/')
            .to_string()
    }
}

/// Find a provider by id, ignoring case.
pub fn find<'a, T>(rows: &'a [T], id: &str, info: impl Fn(&T) -> &ProviderInfo) -> Option<&'a T> {
    rows.iter().find(|r| info(r).id.eq_ignore_ascii_case(id))
}

/// Registry invariants every capability's table must hold; each registry's
/// tests call this on its own rows.
#[cfg(test)]
pub(crate) fn assert_registry_well_formed<'a>(rows: impl IntoIterator<Item = &'a ProviderInfo>) {
    let mut seen = std::collections::HashSet::new();
    for info in rows {
        assert!(
            seen.insert(info.id),
            "duplicate provider id {:?}: config rows could not tell them apart",
            info.id
        );
        assert_eq!(info.id, info.id.to_ascii_lowercase(), "ids are lowercase");
        assert!(
            !info.label.is_empty(),
            "{} needs a label for pickers",
            info.id
        );
        if info.needs_key() {
            assert!(
                !info.base_url.is_empty(),
                "{} takes a key but has no endpoint to send it to",
                info.id
            );
            assert!(
                !info.key_url.is_empty(),
                "{} needs a 'get a key' link",
                info.id
            );
        }
        assert!(
            !(info.requires_command && info.needs_key()),
            "{}: command providers run locally and take no key",
            info.id
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::secrets_resolve::ApiKeyRouting;

    const KEYED: ProviderInfo = ProviderInfo {
        key_env: &["BORG_PI_TEST_PRIMARY", "BORG_PI_TEST_SECONDARY"],
        ..ProviderInfo::new("acme", "Acme")
    };

    fn literal(value: &str) -> ApiKeyRouting {
        ApiKeyRouting::Literal {
            value: value.to_string(),
        }
    }

    #[test]
    fn key_name_defaults_to_an_api_key_and_honors_an_override() {
        assert_eq!(KEYED.key_name(), format!("{} API key", KEYED.label));
        let twilio = ProviderInfo {
            key_label: "Twilio Auth Token",
            ..ProviderInfo::new("twilio", "Twilio")
        };
        assert_eq!(twilio.key_name(), "Twilio Auth Token");
    }

    #[test]
    fn keyless_provider_resolves_to_empty_key() {
        let info = ProviderInfo::new("edge", "Edge");
        assert_eq!(info.api_key(&Config::default(), None), Some(String::new()));
    }

    #[test]
    fn keyed_provider_without_any_key_is_unavailable() {
        assert_eq!(KEYED.api_key(&Config::default(), None), None);
    }

    #[test]
    fn key_saved_in_settings_serves_the_provider() {
        let mut config = Config::default();
        config
            .stored_api_keys
            .insert("acme".into(), literal("saved-key"));
        assert_eq!(KEYED.api_key(&config, None).as_deref(), Some("saved-key"));
    }

    #[test]
    fn explicit_credential_beats_the_saved_key() {
        let mut config = Config::default();
        config
            .stored_api_keys
            .insert("acme".into(), literal("saved-key"));
        config.credentials.insert(
            "BORG_PI_TEST_SECONDARY".into(),
            crate::config::CredentialValue::Ref(
                crate::security::secrets_resolve::SecretRef::Literal {
                    value: "explicit".into(),
                },
            ),
        );
        assert_eq!(KEYED.api_key(&config, None).as_deref(), Some("explicit"));
    }

    #[test]
    fn row_override_replaces_conventional_names() {
        let mut config = Config::default();
        config.credentials.insert(
            "BORG_PI_TEST_PRIMARY".into(),
            crate::config::CredentialValue::Ref(
                crate::security::secrets_resolve::SecretRef::Literal {
                    value: "conventional".into(),
                },
            ),
        );
        // The override names a variable nobody set, and no key is saved, so
        // the conventional variable must not leak back in.
        assert_eq!(KEYED.api_key(&config, Some("BORG_PI_TEST_UNSET")), None);
    }

    #[test]
    fn base_url_prefers_non_empty_override_and_trims_slash() {
        let info = ProviderInfo {
            base_url: "https://api.example.com/",
            ..ProviderInfo::new("x", "X")
        };
        assert_eq!(info.base_url_or(None), "https://api.example.com");
        assert_eq!(info.base_url_or(Some("")), "https://api.example.com");
        assert_eq!(
            info.base_url_or(Some("http://localhost:8880/")),
            "http://localhost:8880"
        );
    }

    #[test]
    fn find_ignores_case() {
        let rows = [ProviderInfo::new("elevenlabs", "ElevenLabs")];
        assert!(find(&rows, "ElevenLabs", |r| r).is_some());
        assert!(find(&rows, "eleven", |r| r).is_none());
    }
}
