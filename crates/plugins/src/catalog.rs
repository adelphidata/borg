use crate::{Category, CredentialSpec, Platform, PluginKind, TemplateFile, TemplateTarget};

/// Future returned by [`PostInstallHook`]. Boxed so the hook type can stay a
/// plain function pointer while still allowing `.await` inside.
pub type PostInstallFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Vec<String>> + Send>>;

/// Optional post-install hook run after templates and credentials are in
/// place. Returns a list of notes (e.g. OS-permission prompts) shown to the
/// user on completion. The hook is async so it can perform I/O (e.g. read
/// the macOS Messages DB via libsql) without blocking the installer task.
pub type PostInstallHook = fn(std::path::PathBuf) -> PostInstallFuture;

/// A static plugin definition — embedded in the binary.
#[derive(Debug, Clone)]
pub struct PluginDef {
    /// Unique plugin identifier (e.g., "telegram", "gmail").
    pub id: &'static str,
    /// Human-readable display name.
    pub name: &'static str,
    /// UI grouping category.
    pub category: Category,
    /// Channel or tool integration.
    pub kind: PluginKind,
    /// Short description shown in the marketplace.
    pub description: &'static str,
    /// Credentials the user must provide during setup.
    pub required_credentials: &'static [CredentialSpec],
    /// External binaries that must be installed.
    pub required_bins: &'static [&'static str],
    /// Template files extracted to disk during installation.
    pub templates: &'static [TemplateFile],
    /// Platform restriction.
    pub platform: Platform,
    /// Native integrations are handled in Rust (gateway crate) and only need credentials, not template files.
    pub is_native: bool,
    /// Optional plugin-specific post-install hook (OS permission checks,
    /// state-file seeding, etc.). Runs after templates and credentials.
    pub post_install: Option<PostInstallHook>,
}

impl PluginDef {
    /// Keychain/credential-store service name derived from the plugin ID.
    pub fn service_name(&self) -> String {
        format!("borg-{}", self.id.replace('/', "-"))
    }
}

const WHATSAPP_CHANNEL_TOML: &str = include_str!("../templates/messaging/whatsapp/channel.toml");
const WHATSAPP_PARSE_INBOUND: &str =
    include_str!("../templates/messaging/whatsapp/parse_inbound.py");
const WHATSAPP_SEND_OUTBOUND: &str =
    include_str!("../templates/messaging/whatsapp/send_outbound.py");
const WHATSAPP_VERIFY: &str = include_str!("../templates/messaging/whatsapp/verify.py");

const IMESSAGE_CHANNEL_TOML: &str = include_str!("../templates/messaging/imessage/channel.toml");
const IMESSAGE_POLL: &str = include_str!("../templates/messaging/imessage/poll_messages.py");
const IMESSAGE_OUTBOUND: &str = include_str!("../templates/messaging/imessage/send_outbound.sh");
const IMESSAGE_POLICY: &str = include_str!("../templates/messaging/imessage/policy.json");
const IMESSAGE_STATE: &str = include_str!("../templates/messaging/imessage/state.json");

const SMS_CHANNEL_TOML: &str = include_str!("../templates/messaging/sms/channel.toml");
const SMS_PARSE_INBOUND: &str = include_str!("../templates/messaging/sms/parse_inbound.py");
const SMS_SEND_OUTBOUND: &str = include_str!("../templates/messaging/sms/send_outbound.py");
const SMS_VERIFY: &str = include_str!("../templates/messaging/sms/verify.py");

/// Complete catalog of available channel plugins, embedded at compile time.
pub static CATALOG: &[PluginDef] = &[
    // Native integrations (handled in Rust, credential-only)
    PluginDef {
        id: "messaging/telegram",
        name: "Telegram",
        category: Category::Channels,
        kind: PluginKind::Channel,
        description: "Telegram",
        required_credentials: &[CredentialSpec {
            key: "TELEGRAM_BOT_TOKEN",
            label: "Bot Token",
            help_url: "https://core.telegram.org/bots#botfather",
            is_optional: false,
        }],
        required_bins: &[],
        templates: &[],
        platform: Platform::All,
        is_native: true,
        post_install: None,
    },
    PluginDef {
        id: "messaging/slack",
        name: "Slack",
        category: Category::Channels,
        kind: PluginKind::Channel,
        description: "Slack",
        required_credentials: &[
            CredentialSpec {
                key: "SLACK_BOT_TOKEN",
                label: "Bot Token",
                help_url: "https://api.slack.com/apps",
                is_optional: false,
            },
            CredentialSpec {
                key: "SLACK_SIGNING_SECRET",
                label: "Signing Secret",
                help_url: "https://api.slack.com/apps",
                is_optional: false,
            },
        ],
        required_bins: &[],
        templates: &[],
        platform: Platform::All,
        is_native: true,
        post_install: None,
    },
    PluginDef {
        id: "messaging/discord",
        name: "Discord",
        category: Category::Channels,
        kind: PluginKind::Channel,
        description: "Discord",
        required_credentials: &[
            CredentialSpec {
                key: "DISCORD_BOT_TOKEN",
                label: "Bot Token",
                help_url: "https://discord.com/developers/applications",
                is_optional: false,
            },
            CredentialSpec {
                key: "DISCORD_PUBLIC_KEY",
                label: "Public Key",
                help_url: "https://discord.com/developers/applications",
                is_optional: false,
            },
        ],
        required_bins: &[],
        templates: &[],
        platform: Platform::All,
        is_native: true,
        post_install: None,
    },
    PluginDef {
        id: "messaging/teams",
        name: "Teams",
        category: Category::Channels,
        kind: PluginKind::Channel,
        description: "Microsoft Teams",
        required_credentials: &[
            CredentialSpec {
                key: "TEAMS_APP_ID",
                label: "App ID",
                help_url: "https://portal.azure.com/",
                is_optional: false,
            },
            CredentialSpec {
                key: "TEAMS_APP_SECRET",
                label: "App Secret",
                help_url: "https://portal.azure.com/",
                is_optional: false,
            },
        ],
        required_bins: &[],
        templates: &[],
        platform: Platform::All,
        is_native: true,
        post_install: None,
    },
    PluginDef {
        id: "messaging/google-chat",
        name: "Google Chat",
        category: Category::Channels,
        kind: PluginKind::Channel,
        description: "Google Chat",
        required_credentials: &[CredentialSpec {
            key: "GOOGLE_CHAT_WEBHOOK_TOKEN",
            label: "Verification Token",
            help_url: "https://console.cloud.google.com/",
            is_optional: false,
        }],
        required_bins: &[],
        templates: &[],
        platform: Platform::All,
        is_native: true,
        post_install: None,
    },
    PluginDef {
        id: "messaging/signal",
        name: "Signal",
        category: Category::Channels,
        kind: PluginKind::Channel,
        description: "Signal Messenger via signal-cli daemon",
        required_credentials: &[CredentialSpec {
            key: "SIGNAL_ACCOUNT",
            label: "Phone Number (e.g., +1234567890)",
            help_url: "https://github.com/AsamK/signal-cli",
            is_optional: false,
        }],
        required_bins: &["signal-cli"],
        templates: &[],
        platform: Platform::All,
        is_native: true,
        post_install: None,
    },
    // Template-based channel plugins
    PluginDef {
        id: "messaging/whatsapp",
        name: "WhatsApp",
        category: Category::Channels,
        kind: PluginKind::Channel,
        description: "WhatsApp Business API via Twilio",
        required_credentials: &[
            CredentialSpec {
                key: "TWILIO_ACCOUNT_SID",
                label: "Twilio Account SID",
                help_url: "https://www.twilio.com/docs/iam/api/account",
                is_optional: false,
            },
            CredentialSpec {
                key: "TWILIO_AUTH_TOKEN",
                label: "Twilio Auth Token",
                help_url: "https://www.twilio.com/docs/iam/api/account",
                is_optional: false,
            },
        ],
        required_bins: &["python3"],
        templates: &[
            TemplateFile {
                relative_path: "whatsapp/channel.toml",
                content: WHATSAPP_CHANNEL_TOML,
                target: TemplateTarget::Channels,
            },
            TemplateFile {
                relative_path: "whatsapp/parse_inbound.py",
                content: WHATSAPP_PARSE_INBOUND,
                target: TemplateTarget::Channels,
            },
            TemplateFile {
                relative_path: "whatsapp/send_outbound.py",
                content: WHATSAPP_SEND_OUTBOUND,
                target: TemplateTarget::Channels,
            },
            TemplateFile {
                relative_path: "whatsapp/verify.py",
                content: WHATSAPP_VERIFY,
                target: TemplateTarget::Channels,
            },
        ],
        platform: Platform::All,
        is_native: false,
        post_install: None,
    },
    PluginDef {
        id: "messaging/imessage",
        name: "iMessage",
        category: Category::Channels,
        kind: PluginKind::Channel,
        description: "Bidirectional iMessage via macOS Messages (macOS only)",
        required_credentials: &[],
        required_bins: &["osascript", "python3"],
        templates: &[
            TemplateFile {
                relative_path: "imessage/channel.toml",
                content: IMESSAGE_CHANNEL_TOML,
                target: TemplateTarget::Channels,
            },
            TemplateFile {
                relative_path: "imessage/poll_messages.py",
                content: IMESSAGE_POLL,
                target: TemplateTarget::Channels,
            },
            TemplateFile {
                relative_path: "imessage/send_outbound.sh",
                content: IMESSAGE_OUTBOUND,
                target: TemplateTarget::Channels,
            },
            TemplateFile {
                relative_path: "imessage/policy.json",
                content: IMESSAGE_POLICY,
                target: TemplateTarget::Channels,
            },
            TemplateFile {
                relative_path: "imessage/state.json",
                content: IMESSAGE_STATE,
                target: TemplateTarget::Channels,
            },
        ],
        platform: Platform::MacOS,
        is_native: false,
        post_install: Some(crate::installer::imessage_post_install),
    },
    PluginDef {
        id: "messaging/sms",
        name: "SMS",
        category: Category::Channels,
        kind: PluginKind::Channel,
        description: "SMS messaging via Twilio",
        required_credentials: &[
            CredentialSpec {
                key: "TWILIO_ACCOUNT_SID",
                label: "Twilio Account SID",
                help_url: "https://www.twilio.com/docs/iam/api/account",
                is_optional: false,
            },
            CredentialSpec {
                key: "TWILIO_AUTH_TOKEN",
                label: "Twilio Auth Token",
                help_url: "https://www.twilio.com/docs/iam/api/account",
                is_optional: false,
            },
        ],
        required_bins: &["python3"],
        templates: &[
            TemplateFile {
                relative_path: "sms/channel.toml",
                content: SMS_CHANNEL_TOML,
                target: TemplateTarget::Channels,
            },
            TemplateFile {
                relative_path: "sms/parse_inbound.py",
                content: SMS_PARSE_INBOUND,
                target: TemplateTarget::Channels,
            },
            TemplateFile {
                relative_path: "sms/send_outbound.py",
                content: SMS_SEND_OUTBOUND,
                target: TemplateTarget::Channels,
            },
            TemplateFile {
                relative_path: "sms/verify.py",
                content: SMS_VERIFY,
                target: TemplateTarget::Channels,
            },
        ],
        platform: Platform::All,
        is_native: false,
        post_install: None,
    },
];

/// Look up a plugin by ID.
pub fn find_by_id(id: &str) -> Option<&'static PluginDef> {
    CATALOG.iter().find(|c| c.id == id)
}

/// All plugins matching `category`, preserving catalog order.
pub fn by_category(category: Category) -> Vec<&'static PluginDef> {
    CATALOG.iter().filter(|c| c.category == category).collect()
}

/// All categories in display order.
pub fn categories() -> &'static [Category] {
    &[
        Category::Channels,
        Category::Core,
        Category::Email,
        Category::Developer,
        Category::Productivity,
        Category::Utilities,
        Category::Payments,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_ids_unique() {
        let mut ids: Vec<&str> = CATALOG.iter().map(|c| c.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), CATALOG.len());
    }

    #[test]
    fn find_by_id_returns_the_matching_entry() {
        // Real failure mode: a lookup that returned the wrong (or first)
        // entry would still be `is_some()`. Assert identity round-trips.
        let found = find_by_id("messaging/whatsapp").expect("whatsapp present");
        assert_eq!(found.id, "messaging/whatsapp");
        assert_eq!(found.service_name(), "borg-messaging-whatsapp");
        assert_eq!(found.category, Category::Channels);
        assert!(find_by_id("nonexistent").is_none());
    }

    #[test]
    fn templates_have_content() {
        for entry in CATALOG {
            if entry.is_native {
                assert!(
                    entry.templates.is_empty(),
                    "native {} should have no templates",
                    entry.id
                );
                continue;
            }
            assert!(!entry.templates.is_empty(), "no templates for {}", entry.id);
            for tmpl in entry.templates {
                assert!(
                    !tmpl.content.is_empty(),
                    "empty template {}",
                    tmpl.relative_path
                );
            }
        }
    }

    #[test]
    fn find_native_by_id_returns_native_channel_entries() {
        for id in &[
            "messaging/telegram",
            "messaging/slack",
            "messaging/discord",
            "messaging/teams",
            "messaging/google-chat",
        ] {
            let entry = find_by_id(id).unwrap_or_else(|| panic!("native plugin {id} not found"));
            assert_eq!(&entry.id, id);
            assert!(entry.is_native, "{id} must be a native integration");
            assert_eq!(entry.category, Category::Channels);
        }
    }

    #[test]
    fn native_plugins_have_credentials_and_no_bins() {
        // Signal is native (Rust code) but requires an external signal-cli daemon
        const NATIVE_WITH_BINS: &[&str] = &["messaging/signal"];

        for entry in CATALOG.iter().filter(|e| e.is_native) {
            assert!(
                !entry.required_credentials.is_empty(),
                "native {} should have credentials",
                entry.id
            );
            if !NATIVE_WITH_BINS.contains(&entry.id) {
                assert!(
                    entry.required_bins.is_empty(),
                    "native {} should have no required bins",
                    entry.id
                );
            }
        }
    }

    #[test]
    fn by_category_filters_matching_entries_in_order() {
        // Every catalog entry is a Channels plugin today, so filtering by
        // Channels must return the whole catalog in declaration order (the
        // marketplace renders in this order), and filtering by a category
        // with no plugins must return nothing — not everything.
        let channel_ids: Vec<&str> = by_category(Category::Channels)
            .iter()
            .map(|d| d.id)
            .collect();
        let all_ids: Vec<&str> = CATALOG.iter().map(|d| d.id).collect();
        assert_eq!(channel_ids, all_ids);
        assert!(
            by_category(Category::Payments).is_empty(),
            "no payments plugins exist yet — an inverted filter would return the full catalog"
        );
    }

    #[test]
    fn categories_lists_each_ui_section_once_channels_first() {
        let cats = categories();
        assert_eq!(
            cats.first(),
            Some(&Category::Channels),
            "channels lead the marketplace display order"
        );
        assert_eq!(cats.len(), 7, "one entry per UI section");
        for (i, cat) in cats.iter().enumerate() {
            assert!(
                !cats[i + 1..].contains(cat),
                "duplicate category {cat} would render its section twice"
            );
        }
        for def in CATALOG {
            assert!(
                cats.contains(&def.category),
                "catalog entry {} has a category missing from the display list",
                def.id
            );
        }
    }

    #[test]
    fn all_catalog_entries_are_channels() {
        for entry in CATALOG {
            assert_eq!(
                entry.category,
                Category::Channels,
                "catalog entry {} should be Channels category",
                entry.id
            );
        }
    }
}
