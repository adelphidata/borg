//! Sequential tool dispatch for the agent loop.
//!
//! Hosts [`Agent::execute_tool`] — the per-call dispatcher invoked by
//! `run_tool_calls` for tools that are not parallel-safe. The matching for
//! parallel-safe tools lives in [`super::parallel_dispatch`]; both share the
//! `Tool::parse` entry point so schema validation is identical on either path.
//!
//! Stays on `impl Agent` so the dispatcher retains access to the full set of
//! per-session state (caches, sub-agent control, browser session, TTS
//! synthesizer, DB lock) without weakening field visibility.

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::instrument;

use crate::types::ToolOutput;

use super::{Agent, AgentEvent};

impl Agent {
    #[instrument(skip_all, fields(tool.name = %name))]
    pub(super) async fn execute_tool(
        &mut self,
        name: &str,
        args_json: &str,
        event_tx: &mpsc::Sender<AgentEvent>,
        tool_call_id: &str,
    ) -> Result<ToolOutput> {
        let raw_args: serde_json::Value = match serde_json::from_str(args_json) {
            Ok(v) => v,
            Err(e) => {
                return Ok(ToolOutput::Text(format!(
                    "Error: Invalid JSON arguments: {e}. Please provide valid JSON."
                )));
            }
        };

        // MCP tools live outside the typed `Tool` enum and use a reserved
        // `mcp_` prefix. Short-circuit before `Tool::parse` so the LLM
        // cannot shadow an MCP tool by reusing its name through the
        // tagged deser, and so MCP failures surface with structured error
        // shape rather than masquerading as `UnknownTool`.
        if name.starts_with("mcp_") {
            let result = crate::mcp_clients::try_execute_tool(name, &raw_args).await?;
            return match result {
                Some(text) => Ok(ToolOutput::Text(text)),
                None => Err(anyhow::anyhow!("Unknown MCP tool: {name}")),
            };
        }

        // Single tagged-deser entry point. Schema-validation errors surface
        // here as structured tool output so the LLM can recover; the dispatch
        // arms below see only typed `Args`.
        let tool = match crate::tool::Tool::parse(name, raw_args.clone()) {
            Ok(t) => t,
            Err(crate::tool::ToolParseError::UnknownTool(n)) => {
                // WASM plugin execution does sync DB + FS + wasmtime compile + run; offload
                // to a blocking thread so the agent loop's reactor isn't starved for up to
                // the manifest's timeout_ms budget.
                let args_owned = raw_args.clone();
                let n_owned = n.clone();
                let plugin_result = tokio::task::spawn_blocking(move || {
                    crate::wasm_plugins::try_execute_tool(&n_owned, &args_owned)
                })
                .await
                .map_err(|e| anyhow::anyhow!("WASM plugin task join failed: {e}"))??;
                if let Some(output) = plugin_result {
                    return Ok(ToolOutput::Text(output));
                }
                return Err(anyhow::anyhow!("Unknown tool: {n}"));
            }
            Err(crate::tool::ToolParseError::InvalidArgs { tool, source }) => {
                return Ok(ToolOutput::Text(format!(
                    "Error: Invalid arguments for tool '{tool}': {source}"
                )));
            }
        };

        // Variants returning ToolOutput directly (may contain multimodal content).
        match &tool {
            crate::tool::Tool::ReadFile(args) => {
                let out = crate::tool::read_file::handle_with_context(
                    args,
                    &self.config,
                    &self.execution_context,
                )?;
                self.maybe_refresh_skills_after_tool(name, &raw_args, "");
                return Ok(out);
            }
            crate::tool::Tool::Browser(args) => {
                let out =
                    crate::tool::browser::handle(args, &self.config, &mut self.browser_session)
                        .await?;
                self.maybe_refresh_skills_after_tool(name, &raw_args, "");
                return Ok(out);
            }
            crate::tool::Tool::TextToSpeech(args) => {
                if let Some(ref synth) = self.tts_synthesizer {
                    return Ok(crate::tool::text_to_speech::handle(args, synth).await);
                }
                return Ok(ToolOutput::Text(
                    "TTS is not configured. Enable it via: borg settings set tts.enabled true"
                        .into(),
                ));
            }
            crate::tool::Tool::RequestUserInput(args) => {
                return crate::tool::request_user_input::handle(args, event_tx).await;
            }
            crate::tool::Tool::VisionAnalyze(args) => {
                let out = crate::tool::vision_analyze::handle(args, &self.config).await?;
                self.maybe_refresh_skills_after_tool(name, &raw_args, "");
                return Ok(out);
            }
            crate::tool::Tool::VideoAnalyze(args) => {
                let out = crate::tool::video_analyze::handle(args, &self.config).await?;
                self.maybe_refresh_skills_after_tool(name, &raw_args, "");
                return Ok(out);
            }
            crate::tool::Tool::ComputerUse(args) => {
                let out = crate::tool::computer_use::handle(args, &self.config).await?;
                self.maybe_refresh_skills_after_tool(name, &raw_args, "");
                return Ok(out);
            }
            _ => {}
        }

        // Variants returning Result<String> wrap to ToolOutput::Text below.
        let text_result: Result<String> = match &tool {
            crate::tool::Tool::WriteMemory(args) => crate::tool::write_memory::handle(
                args,
                &self.config,
                &self.config_arc,
                &mut self.caches,
            ),
            crate::tool::Tool::ReadMemory(args) => crate::tool::read_memory::handle(args),
            crate::tool::Tool::MemorySearch(args) => {
                crate::tool::memory_search::handle(args, &self.config).await
            }
            crate::tool::Tool::List(args) => {
                crate::tool::list::handle(args, &self.config, self.agent_control.as_ref())
            }
            crate::tool::Tool::ListSkills => crate::tool::list::handle_list_skills(&self.config),
            crate::tool::Tool::ListChannels => crate::tool::list::handle_list_channels(&self.config),
            crate::tool::Tool::ApplyPatch(args) => {
                crate::tool::apply_patch::handle_apply_patch_with_context(
                    args,
                    &self.config,
                    &self.execution_context,
                )
            }
            crate::tool::Tool::ApplySkillPatch(args) => {
                crate::tool::apply_patch::handle_apply_skill_patch(args)
            }
            crate::tool::Tool::CreateChannel(args) => {
                crate::tool::apply_patch::handle_create_channel(args)
            }
            crate::tool::Tool::ListDir(args) => crate::tool::list_dir::handle_with_context(
                args,
                &self.config,
                &self.execution_context,
            ),
            crate::tool::Tool::RunShell(args) => {
                crate::tool::run_shell::handle(
                    args,
                    &self.config,
                    &self.policy,
                    event_tx,
                    tool_call_id,
                    Some(&self.skill_env_allowlist),
                    &self.execution_context,
                )
                .await
            }
            crate::tool::Tool::WebFetch(args) => {
                crate::tool::web_fetch::handle(args, &self.config).await
            }
            crate::tool::Tool::WebSearch(args) => {
                crate::tool::web_search::handle(args, &self.config).await
            }
            crate::tool::Tool::Schedule(args) => {
                crate::tool::schedule::handle_schedule(args, &self.config)
            }
            crate::tool::Tool::ManageTasks(args) => {
                crate::tool::schedule::handle_manage_tasks(args, &self.config)
            }
            crate::tool::Tool::ManageCron(args) => {
                crate::tool::schedule::handle_manage_cron(args, &self.config)
            }
            crate::tool::Tool::GenerateImage(args) => {
                crate::tool::generate_image::handle(args, &self.config).await
            }
            crate::tool::Tool::SpawnAgent(args) => {
                crate::tool::multi_agent::handle_spawn_agent(
                    args,
                    &mut self.agent_control,
                    &self.config,
                    &self.history,
                )
                .await
            }
            crate::tool::Tool::SendToAgent(args) => {
                crate::tool::multi_agent::handle_send_to_agent(args)
            }
            crate::tool::Tool::WaitForAgent(args) => {
                crate::tool::multi_agent::handle_wait_for_agent(args, &mut self.agent_control).await
            }
            crate::tool::Tool::ListAgents(_) => {
                crate::tool::multi_agent::handle_list_agents(self.agent_control.as_ref())
            }
            crate::tool::Tool::CloseAgent(args) => {
                crate::tool::multi_agent::handle_close_agent(args, &mut self.agent_control)
            }
            crate::tool::Tool::ManageRoles(args) => {
                crate::tool::multi_agent::handle_manage_roles(args)
            }
            crate::tool::Tool::GetGoal(args) => {
                if !self.config.goals.enabled {
                    return Ok(ToolOutput::Text(
                        serde_json::json!({"error": "goals are disabled"}).to_string(),
                    ));
                }
                let guard = self.db_guard();
                match guard.as_ref() {
                    Some(db) => crate::tool::get_goal::handle(args, db, &self.session.meta.id),
                    None => Ok("null".to_string()),
                }
            }
            crate::tool::Tool::CreateGoal(args) => {
                if !self.config.goals.enabled {
                    return Ok(ToolOutput::Text(
                        serde_json::json!({"error": "goals are disabled"}).to_string(),
                    ));
                }
                let handler_out = {
                    let guard = self.db_guard();
                    match guard.as_ref() {
                        Some(db) => crate::tool::create_goal::handle(
                            args,
                            db,
                            &self.session.meta.id,
                            self.config.goals.default_token_budget,
                        ),
                        None => Ok(serde_json::json!({"error":"database unavailable"}).to_string()),
                    }
                };
                if let Ok(ref text) = handler_out {
                    self.on_goal_created_via_tool(text);
                }
                handler_out
            }
            crate::tool::Tool::UpdateGoal(args) => {
                if !self.config.goals.enabled {
                    return Ok(ToolOutput::Text(
                        serde_json::json!({"error": "goals are disabled"}).to_string(),
                    ));
                }
                let handler_out = {
                    let guard = self.db_guard();
                    match guard.as_ref() {
                        Some(db) => {
                            crate::tool::update_goal::handle(args, db, &self.session.meta.id)
                        }
                        None => Ok(serde_json::json!({"error":"database unavailable"}).to_string()),
                    }
                };
                if let Ok(ref text) = handler_out {
                    self.on_goal_updated_via_tool(text);
                }
                handler_out
            }
            // Variants handled in the early-return block above.
            crate::tool::Tool::ReadFile(_)
            | crate::tool::Tool::Browser(_)
            | crate::tool::Tool::TextToSpeech(_)
            | crate::tool::Tool::RequestUserInput(_)
            | crate::tool::Tool::VisionAnalyze(_)
            | crate::tool::Tool::VideoAnalyze(_)
            | crate::tool::Tool::ComputerUse(_) => unreachable!(
                "ReadFile/Browser/TextToSpeech/RequestUserInput/VisionAnalyze/VideoAnalyze/ComputerUse are handled in the early-return match above"
            ),
        };
        let text = text_result?;
        self.maybe_refresh_skills_after_tool(name, &raw_args, &text);
        Ok(ToolOutput::Text(text))
    }
}
