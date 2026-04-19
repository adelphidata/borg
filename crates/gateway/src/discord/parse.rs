use super::types::{Interaction, InteractionType};
use crate::constants::{PEER_KIND_DIRECT, PEER_KIND_GROUP};
use crate::handler::InboundMessage;

/// Parse a Discord interaction into an `InboundMessage`.
///
/// Returns `None` for:
/// - Ping interactions (handled separately as pong)
/// - Bot users
/// - Autocomplete interactions
/// - Unknown interaction types
/// - Interactions with no identifiable user
pub fn parse_interaction(interaction: &Interaction) -> Option<InboundMessage> {
    // Skip Ping — handled as pong in mod.rs
    if interaction.interaction_type == InteractionType::Ping {
        return None;
    }

    // Skip bot users
    if is_bot(interaction) {
        return None;
    }

    let text = match &interaction.interaction_type {
        InteractionType::ApplicationCommand => extract_command_text(interaction),
        InteractionType::MessageComponent | InteractionType::ModalSubmit => {
            interaction.data.as_ref().and_then(|d| d.custom_id.clone())
        }
        InteractionType::ApplicationCommandAutocomplete | InteractionType::Unknown(_) => {
            return None;
        }
        InteractionType::Ping => unreachable!(),
    }?;

    let sender_id = extract_user_id(interaction)?;

    Some(InboundMessage {
        sender_id,
        text,
        channel_id: interaction.channel_id.clone(),
        // Discord threads are first-class channels with their own channel_id.
        // Mirror channel_id into thread_id so the gateway handler isolates
        // sessions per Discord channel/thread (otherwise every Discord
        // interaction from the same sender collapses into one history).
        thread_id: interaction.channel_id.clone(),
        message_id: Some(interaction.id.clone()),
        thread_ts: None,
        attachments: Vec::new(),
        reaction: None,
        metadata: serde_json::Value::Null,
        peer_kind: if interaction.guild_id.is_some() {
            Some(PEER_KIND_GROUP.to_string())
        } else {
            Some(PEER_KIND_DIRECT.to_string())
        },
    })
}

/// Check if the interaction originates from a bot user.
fn is_bot(interaction: &Interaction) -> bool {
    if let Some(member) = &interaction.member {
        if let Some(user) = &member.user {
            if user.bot == Some(true) {
                return true;
            }
        }
    }
    if let Some(user) = &interaction.user {
        if user.bot == Some(true) {
            return true;
        }
    }
    false
}

/// Extract user ID from member.user or interaction.user.
fn extract_user_id(interaction: &Interaction) -> Option<String> {
    if let Some(member) = &interaction.member {
        if let Some(user) = &member.user {
            return Some(user.id.clone());
        }
    }
    interaction.user.as_ref().map(|u| u.id.clone())
}

/// Extract text from an application command interaction.
///
/// Joins string option values with spaces. Falls back to the command name
/// if no string options are present.
fn extract_command_text(interaction: &Interaction) -> Option<String> {
    let data = interaction.data.as_ref()?;

    // Try to join string values from options
    if let Some(options) = &data.options {
        let texts: Vec<String> = options
            .iter()
            .filter_map(|opt| opt.value.as_str().map(String::from))
            .collect();
        if !texts.is_empty() {
            return Some(texts.join(" "));
        }
    }

    // Fall back to command name
    data.name.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discord::types::*;

    fn make_interaction(interaction_type: InteractionType) -> Interaction {
        Interaction {
            id: "int1".into(),
            interaction_type,
            application_id: Some("app1".into()),
            data: None,
            member: Some(GuildMember {
                user: Some(DiscordUser {
                    id: "u1".into(),
                    username: "alice".into(),
                    bot: Some(false),
                }),
            }),
            user: None,
            token: "tok".into(),
            guild_id: Some("g1".into()),
            channel_id: Some("ch1".into()),
        }
    }

    #[test]
    fn ping_returns_none() {
        let interaction = make_interaction(InteractionType::Ping);
        assert!(parse_interaction(&interaction).is_none());
    }

    #[test]
    fn application_command_with_options() {
        let mut interaction = make_interaction(InteractionType::ApplicationCommand);
        interaction.data = Some(InteractionData {
            id: Some("cmd1".into()),
            name: Some("ask".into()),
            options: Some(vec![CommandOption {
                name: "question".into(),
                value: serde_json::Value::String("What is Rust?".into()),
            }]),
            custom_id: None,
        });

        let msg = parse_interaction(&interaction).unwrap();
        assert_eq!(msg.sender_id, "u1");
        assert_eq!(msg.text, "What is Rust?");
        assert_eq!(msg.channel_id.as_deref(), Some("ch1"));
        assert_eq!(msg.message_id.as_deref(), Some("int1"));
        // thread_id mirrors channel_id so per-thread sessions are isolated
        // (Discord threads are first-class channels with their own channel_id).
        assert_eq!(msg.thread_id.as_deref(), Some("ch1"));
    }

    #[test]
    fn application_command_falls_back_to_name() {
        let mut interaction = make_interaction(InteractionType::ApplicationCommand);
        interaction.data = Some(InteractionData {
            id: Some("cmd1".into()),
            name: Some("help".into()),
            options: None,
            custom_id: None,
        });

        let msg = parse_interaction(&interaction).unwrap();
        assert_eq!(msg.text, "help");
    }

    #[test]
    fn application_command_multiple_options() {
        let mut interaction = make_interaction(InteractionType::ApplicationCommand);
        interaction.data = Some(InteractionData {
            id: Some("cmd1".into()),
            name: Some("ask".into()),
            options: Some(vec![
                CommandOption {
                    name: "topic".into(),
                    value: serde_json::Value::String("rust".into()),
                },
                CommandOption {
                    name: "detail".into(),
                    value: serde_json::Value::String("lifetimes".into()),
                },
            ]),
            custom_id: None,
        });

        let msg = parse_interaction(&interaction).unwrap();
        assert_eq!(msg.text, "rust lifetimes");
    }

    #[test]
    fn application_command_non_string_options_fall_back_to_name() {
        let mut interaction = make_interaction(InteractionType::ApplicationCommand);
        interaction.data = Some(InteractionData {
            id: Some("cmd1".into()),
            name: Some("config".into()),
            options: Some(vec![CommandOption {
                name: "count".into(),
                value: serde_json::Value::Number(42.into()),
            }]),
            custom_id: None,
        });

        let msg = parse_interaction(&interaction).unwrap();
        assert_eq!(msg.text, "config");
    }

    #[test]
    fn message_component_uses_custom_id() {
        let mut interaction = make_interaction(InteractionType::MessageComponent);
        interaction.data = Some(InteractionData {
            id: None,
            name: None,
            options: None,
            custom_id: Some("btn_confirm".into()),
        });

        let msg = parse_interaction(&interaction).unwrap();
        assert_eq!(msg.text, "btn_confirm");
    }

    #[test]
    fn modal_submit_uses_custom_id() {
        let mut interaction = make_interaction(InteractionType::ModalSubmit);
        interaction.data = Some(InteractionData {
            id: None,
            name: None,
            options: None,
            custom_id: Some("feedback_form".into()),
        });

        let msg = parse_interaction(&interaction).unwrap();
        assert_eq!(msg.text, "feedback_form");
    }

    #[test]
    fn autocomplete_returns_none() {
        let interaction = make_interaction(InteractionType::ApplicationCommandAutocomplete);
        assert!(parse_interaction(&interaction).is_none());
    }

    #[test]
    fn unknown_type_returns_none() {
        let interaction = make_interaction(InteractionType::Unknown(99));
        assert!(parse_interaction(&interaction).is_none());
    }

    #[test]
    fn bot_member_returns_none() {
        let mut interaction = make_interaction(InteractionType::ApplicationCommand);
        interaction.data = Some(InteractionData {
            id: Some("cmd1".into()),
            name: Some("ask".into()),
            options: None,
            custom_id: None,
        });
        interaction.member = Some(GuildMember {
            user: Some(DiscordUser {
                id: "bot1".into(),
                username: "mybot".into(),
                bot: Some(true),
            }),
        });

        assert!(parse_interaction(&interaction).is_none());
    }

    #[test]
    fn bot_user_returns_none() {
        let mut interaction = make_interaction(InteractionType::ApplicationCommand);
        interaction.data = Some(InteractionData {
            id: Some("cmd1".into()),
            name: Some("ask".into()),
            options: None,
            custom_id: None,
        });
        interaction.member = None;
        interaction.user = Some(DiscordUser {
            id: "bot1".into(),
            username: "mybot".into(),
            bot: Some(true),
        });

        assert!(parse_interaction(&interaction).is_none());
    }

    #[test]
    fn dm_interaction_uses_user_field() {
        let mut interaction = make_interaction(InteractionType::ApplicationCommand);
        interaction.member = None;
        interaction.user = Some(DiscordUser {
            id: "u2".into(),
            username: "bob".into(),
            bot: Some(false),
        });
        interaction.data = Some(InteractionData {
            id: Some("cmd1".into()),
            name: Some("hello".into()),
            options: None,
            custom_id: None,
        });

        let msg = parse_interaction(&interaction).unwrap();
        assert_eq!(msg.sender_id, "u2");
    }

    #[test]
    fn no_user_anywhere_returns_none() {
        let mut interaction = make_interaction(InteractionType::ApplicationCommand);
        interaction.member = None;
        interaction.user = None;
        interaction.data = Some(InteractionData {
            id: Some("cmd1".into()),
            name: Some("ask".into()),
            options: None,
            custom_id: None,
        });

        assert!(parse_interaction(&interaction).is_none());
    }

    #[test]
    fn no_data_returns_none() {
        let mut interaction = make_interaction(InteractionType::ApplicationCommand);
        interaction.data = None;

        assert!(parse_interaction(&interaction).is_none());
    }

    #[test]
    fn peer_kind_reflects_guild_id_presence() {
        // peer_kind drives downstream dm_policy enforcement: a DM (guild_id=None)
        // must be classified as `direct` so the gateway gates unknown senders
        // through pairing. If this conditional inverts, DMs get treated as
        // group messages and dm_policy never applies → unknown senders bypass.
        let mut group = make_interaction(InteractionType::ApplicationCommand);
        group.guild_id = Some("g1".into());
        group.data = Some(InteractionData {
            id: Some("cmd1".into()),
            name: Some("hello".into()),
            options: None,
            custom_id: None,
        });
        let group_msg = parse_interaction(&group).unwrap();
        assert_eq!(group_msg.peer_kind.as_deref(), Some(PEER_KIND_GROUP));

        let mut dm = make_interaction(InteractionType::ApplicationCommand);
        dm.guild_id = None;
        dm.member = None;
        dm.user = Some(DiscordUser {
            id: "u_dm".into(),
            username: "dm_user".into(),
            bot: Some(false),
        });
        dm.data = Some(InteractionData {
            id: Some("cmd1".into()),
            name: Some("hello".into()),
            options: None,
            custom_id: None,
        });
        let dm_msg = parse_interaction(&dm).unwrap();
        assert_eq!(dm_msg.peer_kind.as_deref(), Some(PEER_KIND_DIRECT));
        assert_eq!(dm_msg.sender_id, "u_dm");
    }

    #[test]
    fn message_component_no_custom_id_returns_none() {
        let mut interaction = make_interaction(InteractionType::MessageComponent);
        interaction.data = Some(InteractionData {
            id: None,
            name: None,
            options: None,
            custom_id: None,
        });

        assert!(parse_interaction(&interaction).is_none());
    }
}
