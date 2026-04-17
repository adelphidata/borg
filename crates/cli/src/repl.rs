use anyhow::Result;
use std::io::{self, Write};
use tokio::sync::mpsc;

use borg_core::agent::{Agent, AgentEvent};
use borg_core::config::Config;
use borg_core::telemetry::BorgMetrics;

pub async fn run(resume: Option<String>) -> Result<Option<crate::tui::ResumeHint>> {
    crate::tui::run(resume).await
}

pub async fn one_shot(
    message: &str,
    auto_approve: bool,
    json_output: bool,
    mode: Option<&str>,
) -> Result<()> {
    let mut config = Config::load_from_db()?;
    if let Some(mode_str) = mode {
        config.conversation.collaboration_mode = mode_str.parse()?;
    }
    let metrics = BorgMetrics::from_config(&config);
    let mut agent = Agent::new(config, metrics)?;

    // Register vitals hook for passive health tracking
    if let Ok(vitals_hook) = borg_core::vitals::VitalsHook::new() {
        agent.hook_registry_mut().register(Box::new(vitals_hook));
    }

    // Register activity log hook for structured event logging
    if let Ok(activity_hook) = borg_core::activity_log::ActivityHook::new() {
        agent.hook_registry_mut().register(Box::new(activity_hook));
    }

    // Register bond hook for trust tracking (after vitals so events are available)
    if let Ok(bond_hook) = borg_core::bond::BondHook::new() {
        agent.hook_registry_mut().register(Box::new(bond_hook));
    }

    // Register evolution hook for XP tracking and specialization
    if agent.config().evolution.enabled {
        if let Ok(evolution_hook) = borg_core::evolution::EvolutionHook::new() {
            agent.hook_registry_mut().register(Box::new(evolution_hook));
        }
    }

    // Register user-authored script hooks loaded from ~/.borg/hooks.json
    for hook in borg_core::hooks::ScriptHook::load_all(agent.config().hooks.enabled) {
        agent.hook_registry_mut().register(Box::new(hook));
    }

    let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(256);

    let send_future = agent.send_message(message, event_tx);

    let render_handle = tokio::spawn(async move {
        let mut stdout = io::stdout();
        let mut full_response = String::new();
        let mut exit_code: i32 = 0;

        while let Some(event) = event_rx.recv().await {
            match event {
                AgentEvent::TextDelta(delta) => {
                    if json_output {
                        full_response.push_str(&delta);
                    } else {
                        print!("{delta}");
                        let _ = stdout.flush();
                    }
                }
                AgentEvent::TurnComplete => {
                    if json_output {
                        let output = serde_json::json!({
                            "response": full_response,
                            "exit_code": exit_code,
                        });
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&output).unwrap_or_default()
                        );
                    } else {
                        println!();
                    }
                }
                AgentEvent::ShellConfirmation { respond, command } => {
                    if auto_approve {
                        let _ = respond.send(true);
                        if !json_output {
                            eprintln!("[auto-approved] {command}");
                        }
                    } else {
                        let _ = respond.send(false);
                        if !json_output {
                            eprintln!("Shell command denied in one-shot mode. Use --yes to allow.");
                        }
                    }
                }
                AgentEvent::Error(e) => {
                    eprintln!("Error: {e}");
                    exit_code = 1;
                }
                AgentEvent::ToolExecuting { name, .. } => {
                    if !json_output {
                        eprintln!("[running {name}]");
                    }
                }
                AgentEvent::ToolResult { name, result } => {
                    if !json_output {
                        let preview =
                            if result.len() > borg_core::constants::TOOL_RESULT_PREVIEW_CHARS {
                                let mut end = borg_core::constants::TOOL_RESULT_PREVIEW_CHARS;
                                while end > 0 && !result.is_char_boundary(end) {
                                    end -= 1;
                                }
                                &result[..end]
                            } else {
                                &result
                            };
                        eprintln!("[{name} done] {preview}");
                    }
                }
                AgentEvent::ToolOutputDelta {
                    delta, is_stderr, ..
                } => {
                    if !json_output {
                        let prefix = if is_stderr { "! " } else { "" };
                        eprintln!("  {prefix}{delta}");
                    }
                }
                _ => {}
            }
        }

        exit_code
    });

    if let Err(e) = send_future.await {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }

    let exit_code = render_handle.await.unwrap_or(1);
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}
