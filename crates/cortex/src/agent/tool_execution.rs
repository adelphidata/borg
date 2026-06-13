//! Tool-call execution for the agent loop.
//!
//! Three entry points from `finish_iteration_inner` in [`super`]:
//! - [`Agent::execute_tool_calls`] — invalid-call handling + group planning
//! - `Agent::run_tool_calls` — sequential per-group execution
//! - `Agent::run_tool_group_parallel` — bounded-parallel execution for read-safe groups
//!
//! The free function [`dispatch_parallel_safe_tool`] is invoked by the parallel
//! path on each worker; its closed-set `match` keeps the planner and dispatcher
//! in sync at compile time.

use std::time::Instant;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info_span, instrument, warn, Instrument};

use crate::config::Config;
use crate::hooks::{HookAction, HookData, HookPoint};
use crate::rate_guard::RateDecision;
use crate::types::{ContentPart, FunctionCall, Message, ToolCall, ToolOutput};

use super::tool_classification::{classify_action, is_mutating_tool};
use super::tool_validation::{
    format_invalid_tool_result, repeated_tool_call_block_message, unknown_tool_refusal_message,
    InvalidToolCall, RepeatDecision,
};
use super::{Agent, AgentEvent};

const GOALS_DISABLED_ERROR: &str = r#"{"error":"goals are disabled"}"#;
const DATABASE_UNAVAILABLE_ERROR: &str = r#"{"error":"database unavailable"}"#;

impl Agent {
    /// Persist the assistant message with tool calls, partition and execute them,
    /// running shell/browser sequentially and other tools in parallel.
    ///
    /// `invalid` holds tool calls the LLM emitted that failed validation
    /// (malformed JSON, oversized) but still have id+name. We surface them as
    /// synthetic `Error:`-prefixed tool_results both to the TUI (so the user
    /// sees the failure) and to the LLM history (so the next turn can retry).
    /// All shells — valid and invalid — go into the assistant message so the
    /// API contract "every tool_call has a matching tool_result" is preserved.
    pub(super) async fn execute_tool_calls(
        &mut self,
        tc: &[ToolCall],
        invalid: &[InvalidToolCall],
        text_content: &str,
        event_tx: &mpsc::Sender<AgentEvent>,
        cancel: &CancellationToken,
    ) {
        let mut tc_for_msg = tc.to_vec();
        for inv in invalid {
            tc_for_msg.push(ToolCall {
                id: inv.id.clone(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: inv.name.clone(),
                    arguments: inv.raw_arguments.clone(),
                },
            });
        }
        let assistant_msg = Message {
            role: crate::types::Role::Assistant,
            content: if text_content.is_empty() {
                None
            } else {
                Some(crate::types::MessageContent::Text(text_content.to_string()))
            },
            tool_calls: Some(tc_for_msg),
            tool_call_id: None,
            timestamp: Some(chrono::Local::now().to_rfc3339()),
        };
        self.log_and_persist(assistant_msg).await;

        for inv in invalid {
            let body = format_invalid_tool_result(inv);
            // Hooks intentionally NOT fired here: the tool never dispatched,
            // so emitting AfterToolCall without a matching BeforeToolCall
            // would skew hook accounting and surprise authors counting
            // Before↔After pairs.
            let _ = event_tx
                .send(AgentEvent::ToolExecuting {
                    tool_call_id: inv.id.clone(),
                    name: inv.name.clone(),
                    args: inv.raw_arguments.clone(),
                })
                .await;
            let _ = event_tx
                .send(AgentEvent::ToolResult {
                    tool_call_id: inv.id.clone(),
                    name: inv.name.clone(),
                    result: body.clone(),
                })
                .await;
            self.log_and_persist(Message::tool_result(&inv.id, &body))
                .await;
        }

        // Plan concurrent tool groups. The planner guarantees every group
        // either (a) contains a single Unknown-effect tool or (b) contains
        // only parallel-safe tools with no conflicting file-path writes.
        let concurrent_cfg = &self.config.conversation.concurrent_tools;
        let max_workers = concurrent_cfg.max_workers.max(1);
        let parallel_enabled = concurrent_cfg.enabled && max_workers > 1;

        let groups = crate::tool::effects::plan_groups(tc);
        for group in groups {
            if cancel.is_cancelled() {
                // Emit cancellation markers for the remaining calls so the LLM
                // sees a tool_result for every tool_call it issued.
                for idx in group {
                    self.skip_tool_call(&tc[idx].id, "[tool call cancelled by user]")
                        .await;
                }
                continue;
            }

            if group.len() == 1 || !parallel_enabled {
                let slice: Vec<&ToolCall> = group.iter().map(|i| &tc[*i]).collect();
                self.run_tool_calls(&slice, event_tx, cancel).await;
            } else {
                let slice: Vec<&ToolCall> = group.iter().map(|i| &tc[*i]).collect();
                self.run_tool_group_parallel(&slice, max_workers, event_tx, cancel)
                    .await;
            }
        }
    }

    #[instrument(skip_all)]
    async fn run_tool_calls(
        &mut self,
        tool_calls: &[&ToolCall],
        event_tx: &mpsc::Sender<AgentEvent>,
        cancel: &CancellationToken,
    ) {
        for tool_call in tool_calls {
            if cancel.is_cancelled() {
                self.skip_tool_call(&tool_call.id, "[tool call cancelled by user]")
                    .await;
                continue;
            }

            let name = &tool_call.function.name;
            let args = &tool_call.function.arguments;

            // Fire BeforeToolCall hook
            let hook_ctx = self.hook_ctx(
                HookPoint::BeforeToolCall,
                HookData::ToolCall {
                    name: name.clone(),
                    args: args.clone(),
                },
            );
            if matches!(
                self.hook_registry.dispatch(&hook_ctx).await,
                HookAction::Skip
            ) {
                self.skip_tool_call(&tool_call.id, "[tool call skipped by hook]")
                    .await;
                continue;
            }

            // Plan mode: block mutating tool calls
            if self
                .config
                .conversation
                .collaboration_mode
                .blocks_mutations()
                && is_mutating_tool(name)
            {
                self.skip_tool_call(
                    &tool_call.id,
                    "Plan mode: mutating operations are not allowed. Use read-only tools (read_file, list_dir, list, memory_search, web_fetch, web_search) to explore the codebase and formulate your plan.",
                )
                .await;
                continue;
            }

            // Identical-tool-call loop guard: must run BEFORE rate-guard so a
            // blocked repeat does not consume rate-guard budget. Backoff
            // delays the would-be-blocked call without surrendering the
            // execution slot, buying an operator-interrupt window before the
            // hard block fires.
            match self.tool_call_repeat.observe(name, args) {
                RepeatDecision::Allow => {}
                RepeatDecision::Backoff(delay) => {
                    let count = self.tool_call_repeat.consecutive_count;
                    warn!(
                        "Repeated identical tool call backoff: '{name}' x{count}, sleeping {}ms",
                        delay.as_millis()
                    );
                    tokio::time::sleep(delay).await;
                }
                RepeatDecision::Block => {
                    let count = self.tool_call_repeat.consecutive_count;
                    warn!("Repeated identical tool call blocked: '{name}' x{count}");
                    self.metrics.tool_repeats_blocked.add(1, &[]);
                    self.skip_tool_call(
                        &tool_call.id,
                        &repeated_tool_call_block_message(name, count),
                    )
                    .await;
                    continue;
                }
            }

            // Rate limiting
            let action_type = classify_action(name);
            match self.rate_guard.record(action_type) {
                RateDecision::Block(reason) => {
                    warn!("Rate limit blocked tool call '{name}': {reason}");
                    self.skip_tool_call(&tool_call.id, &format!("Error: {reason}"))
                        .await;
                    continue;
                }
                RateDecision::Warn(reason) => {
                    warn!("{reason}");
                }
                RateDecision::Allow => {}
            }

            let _ = event_tx
                .send(AgentEvent::ToolExecuting {
                    tool_call_id: tool_call.id.clone(),
                    name: name.clone(),
                    args: args.clone(),
                })
                .await;

            let tool_start = Instant::now();
            let tool_output = self
                .execute_tool(name, args, event_tx, &tool_call.id)
                .instrument(info_span!("tool.execute", tool.name = %name))
                .await
                .unwrap_or_else(|e| {
                    ToolOutput::Text(crate::security::sanitize::sanitize_tool_error(
                        &e.to_string(),
                    ))
                });
            let tool_elapsed = tool_start.elapsed().as_secs_f64();
            self.metrics.tool_executions.add(1, &[]);
            self.metrics.tool_duration.record(tool_elapsed, &[]);

            self.finalize_tool_output(&tool_call.id, name, tool_output, event_tx)
                .await;
        }
    }

    /// Post-process a completed tool call: redact + sanitize the output, wrap
    /// it in a `<tool_output>` element, fire the after-tool hook, emit the
    /// `ToolResult` event, and persist the resulting message. Shared verbatim
    /// by the sequential and parallel execution paths so the two cannot drift.
    async fn finalize_tool_output(
        &mut self,
        tool_call_id: &str,
        name: &str,
        tool_output: ToolOutput,
        event_tx: &mpsc::Sender<AgentEvent>,
    ) {
        // Sanitize tool name for XML embedding to prevent injection
        let safe_name = crate::security::sanitize::escape_xml_attr(name);
        let (raw_text, extra_parts) = match tool_output {
            ToolOutput::Text(t) => (t, None),
            ToolOutput::Multimodal { text, parts } => (text, Some(parts)),
        };
        let redacted = self.truncate_and_redact(&raw_text);
        let sanitized = crate::security::sanitize::sanitize_xml_boundaries(&redacted);
        let xml = format!(
            "<tool_output name=\"{safe_name}\" trust=\"external\">\n{sanitized}\n</tool_output>"
        );
        Self::fire_after_tool_hook(
            &mut self.hook_registry,
            &self.session.meta.id,
            self.turn_count,
            name,
            &xml,
        )
        .await;
        let _ = event_tx
            .send(AgentEvent::ToolResult {
                tool_call_id: tool_call_id.to_string(),
                name: name.to_string(),
                result: redacted,
            })
            .await;
        let msg = if let Some(parts) = extra_parts {
            let mut msg_parts = vec![ContentPart::Text(xml)];
            msg_parts.extend(
                parts
                    .into_iter()
                    .filter(|p| !matches!(p, ContentPart::Text(_))),
            );
            Message::tool_result_multimodal(tool_call_id, msg_parts)
        } else {
            Message::tool_result(tool_call_id, &xml)
        };
        self.log_and_persist(msg).await;
    }

    /// Execute a batch of parallel-safe tool calls concurrently, preserving
    /// result order. Pre-spawn checks (hooks, plan-mode, rate-limit) run
    /// sequentially on the main task; execution fans out to a bounded
    /// `JoinSet`; post-work (hooks, persistence, events) runs sequentially
    /// in the group's original order.
    #[instrument(skip_all)]
    async fn run_tool_group_parallel(
        &mut self,
        tool_calls: &[&ToolCall],
        max_workers: usize,
        event_tx: &mpsc::Sender<AgentEvent>,
        cancel: &CancellationToken,
    ) {
        // Pre-spawn serial pass: dispatch hooks, check plan-mode, record
        // rate-limit budget. For each call, build a Slot describing whether
        // it will execute or be skipped. `ToolExecuting` events and
        // persistence are deferred to the post-fan-out merge so they fire
        // in original LLM-issued order regardless of interleaving.
        enum Slot {
            Approved { name: String, args: String },
            Skipped { reason: String },
        }
        let mut slots: Vec<Slot> = Vec::with_capacity(tool_calls.len());

        for tool_call in tool_calls {
            if cancel.is_cancelled() {
                slots.push(Slot::Skipped {
                    reason: "[tool call cancelled by user]".into(),
                });
                continue;
            }

            let name = tool_call.function.name.clone();
            let args = tool_call.function.arguments.clone();

            let hook_ctx = self.hook_ctx(
                HookPoint::BeforeToolCall,
                HookData::ToolCall {
                    name: name.clone(),
                    args: args.clone(),
                },
            );
            if matches!(
                self.hook_registry.dispatch(&hook_ctx).await,
                HookAction::Skip
            ) {
                slots.push(Slot::Skipped {
                    reason: "[tool call skipped by hook]".into(),
                });
                continue;
            }

            if self
                .config
                .conversation
                .collaboration_mode
                .blocks_mutations()
                && is_mutating_tool(&name)
            {
                slots.push(Slot::Skipped {
                    reason: "Plan mode: mutating operations are not allowed. Use read-only tools (read_file, list_dir, list, memory_search, web_fetch, web_search) to explore the codebase and formulate your plan.".into(),
                });
                continue;
            }

            // Identical-tool-call loop guard — must precede rate-guard so a
            // blocked repeat does not consume rate-guard budget. The sleep
            // serializes the planning loop on a backoff hit; that's
            // intentional — the whole point of the guard is to slow a
            // runaway batch down before the hard cap fires.
            match self.tool_call_repeat.observe(&name, &args) {
                RepeatDecision::Allow => {}
                RepeatDecision::Backoff(delay) => {
                    let count = self.tool_call_repeat.consecutive_count;
                    warn!(
                        "Repeated identical tool call backoff (parallel): '{name}' x{count}, sleeping {}ms",
                        delay.as_millis()
                    );
                    tokio::time::sleep(delay).await;
                }
                RepeatDecision::Block => {
                    let count = self.tool_call_repeat.consecutive_count;
                    warn!("Repeated identical tool call blocked: '{name}' x{count}");
                    self.metrics.tool_repeats_blocked.add(1, &[]);
                    slots.push(Slot::Skipped {
                        reason: repeated_tool_call_block_message(&name, count),
                    });
                    continue;
                }
            }

            let action_type = classify_action(&name);
            match self.rate_guard.record(action_type) {
                RateDecision::Block(reason) => {
                    warn!("Rate limit blocked tool call '{name}': {reason}");
                    slots.push(Slot::Skipped {
                        reason: format!("Error: {reason}"),
                    });
                    continue;
                }
                RateDecision::Warn(reason) => warn!("{reason}"),
                RateDecision::Allow => {}
            }

            slots.push(Slot::Approved { name, args });
        }

        // Fan-out: spawn approved slots onto JoinSet. Bounded by `max_workers`.
        let mut join_set: tokio::task::JoinSet<(usize, String, Instant, Result<ToolOutput>)> =
            tokio::task::JoinSet::new();
        let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(max_workers));

        for (idx, slot) in slots.iter().enumerate() {
            if let Slot::Approved { name, args } = slot {
                let config = std::sync::Arc::clone(&self.config_arc);
                let execution_context = self.execution_context.clone();
                let tx = event_tx.clone();
                let permits = std::sync::Arc::clone(&permits);
                let name_clone = name.clone();
                let args_clone = args.clone();
                join_set.spawn(async move {
                    let _permit = permits.acquire_owned().await.ok();
                    let start = Instant::now();
                    let result = dispatch_parallel_safe_tool(
                        &name_clone,
                        &args_clone,
                        config.as_ref(),
                        &tx,
                        Some(&execution_context),
                    )
                    .await;
                    (idx, name_clone, start, result)
                });
            }
        }

        // Collect completed tasks into a vec indexed by `idx` so we can
        // re-interleave with skip slots in original order.
        let mut results: Vec<Option<(String, f64, Result<ToolOutput>)>> =
            (0..slots.len()).map(|_| None).collect::<Vec<_>>();
        while let Some(joined) = join_set.join_next().await {
            match joined {
                Ok((idx, name, start, result)) => {
                    let elapsed = start.elapsed().as_secs_f64();
                    results[idx] = Some((name, elapsed, result));
                }
                Err(e) => {
                    tracing::error!("parallel tool worker panicked or was cancelled: {e}");
                }
            }
        }

        // Single in-order pass: emit ToolExecuting, fire hooks, and persist
        // results in the LLM's original call order. Skip slots and completed
        // slots are interleaved at their original positions.
        for (idx, slot) in slots.into_iter().enumerate() {
            let tool_call = tool_calls[idx];
            let (name, elapsed, result, args_for_event) = match slot {
                Slot::Skipped { reason } => {
                    self.skip_tool_call(&tool_call.id, &reason).await;
                    continue;
                }
                Slot::Approved { name: _, args } => match results[idx].take() {
                    Some((nm, el, res)) => (nm, el, res, args),
                    None => {
                        // Worker panicked — surface a placeholder error so the
                        // LLM still sees a tool_result for this call.
                        self.skip_tool_call(&tool_call.id, "[tool call failed — worker panicked]")
                            .await;
                        continue;
                    }
                },
            };

            let _ = event_tx
                .send(AgentEvent::ToolExecuting {
                    tool_call_id: tool_call.id.clone(),
                    name: name.clone(),
                    args: args_for_event,
                })
                .await;

            let tool_output = result.unwrap_or_else(|e| {
                ToolOutput::Text(crate::security::sanitize::sanitize_tool_error(
                    &e.to_string(),
                ))
            });
            self.metrics.tool_executions.add(1, &[]);
            self.metrics.tool_duration.record(elapsed, &[]);

            self.finalize_tool_output(&tool_call.id, &name, tool_output, event_tx)
                .await;
        }
    }

    #[instrument(skip_all, fields(tool.name = %name))]
    async fn execute_tool(
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
                // WASM plugin execution does async DB + FS + wasmtime compile + run.
                // The wasmtime portion is CPU-bound; tolerate the in-band await
                // here for now — the manifest's timeout_ms budget already bounds it.
                // Plugin lookup runs BEFORE the unknown-tool counter so a
                // legitimate plugin name never trips the guard.
                let plugin_result = crate::wasm_plugins::try_execute_tool(&n, &raw_args).await?;
                if let Some(output) = plugin_result {
                    return Ok(ToolOutput::Text(output));
                }
                let count = self.unknown_tool_tracker.observe(&n);
                if self.unknown_tool_tracker.should_refuse(count) {
                    warn!("Unknown tool '{n}' refused after {count} attempts this turn");
                    self.metrics.unknown_tools_refused.add(1, &[]);
                    return Ok(ToolOutput::Text(unknown_tool_refusal_message(&n, count)));
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
            crate::tool::Tool::WriteMemory(args) => {
                crate::tool::write_memory::handle(
                    args,
                    &self.config,
                    &self.config_arc,
                    &mut self.caches,
                )
                .await
            }
            crate::tool::Tool::ReadMemory(args) => {
                crate::tool::read_memory::handle(args, &self.config).await
            }
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
                crate::tool::schedule::handle_schedule(args, &self.config).await
            }
            crate::tool::Tool::ManageTasks(args) => {
                crate::tool::schedule::handle_manage_tasks(args, &self.config).await
            }
            crate::tool::Tool::ManageCron(args) => {
                crate::tool::schedule::handle_manage_cron(args, &self.config).await
            }
            crate::tool::Tool::GenerateImage(args) => {
                crate::tool::generate_image::handle(args, &self.config).await
            }
            crate::tool::Tool::TranscribeAudio(args) => {
                crate::tool::transcribe_audio::handle(
                    args,
                    &self.config,
                    &self.execution_context,
                )
                .await
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
                crate::tool::multi_agent::handle_close_agent(args, &mut self.agent_control).await
            }
            crate::tool::Tool::ManageRoles(args) => {
                crate::tool::multi_agent::handle_manage_roles(args).await
            }
            crate::tool::Tool::GetGoal(args) => {
                if !self.config.goals.enabled {
                    return Ok(ToolOutput::Text(GOALS_DISABLED_ERROR.to_string()));
                }
                let guard = self.db_guard().await;
                match guard.as_ref() {
                    Some(db) => {
                        crate::tool::get_goal::handle(args, db, &self.session.meta.id).await
                    }
                    None => Ok("null".to_string()),
                }
            }
            crate::tool::Tool::CreateGoal(args) => {
                if !self.config.goals.enabled {
                    return Ok(ToolOutput::Text(GOALS_DISABLED_ERROR.to_string()));
                }
                let handler_out = {
                    let guard = self.db_guard().await;
                    match guard.as_ref() {
                        Some(db) => {
                            crate::tool::create_goal::handle(
                                args,
                                db,
                                &self.session.meta.id,
                                self.config.goals.default_token_budget,
                            )
                            .await
                        }
                        None => Ok(DATABASE_UNAVAILABLE_ERROR.to_string()),
                    }
                };
                if let Ok(ref text) = handler_out {
                    self.on_goal_created_via_tool(text).await;
                }
                handler_out
            }
            crate::tool::Tool::UpdateGoal(args) => {
                if !self.config.goals.enabled {
                    return Ok(ToolOutput::Text(GOALS_DISABLED_ERROR.to_string()));
                }
                let handler_out = {
                    let guard = self.db_guard().await;
                    match guard.as_ref() {
                        Some(db) => {
                            crate::tool::update_goal::handle(args, db, &self.session.meta.id)
                                .await
                        }
                        None => Ok(DATABASE_UNAVAILABLE_ERROR.to_string()),
                    }
                };
                if let Ok(ref text) = handler_out {
                    self.on_goal_updated_via_tool(text).await;
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
                "ReadFile/Browser/TextToSpeech/RequestUserInput/Vision/Video/ComputerUse are handled in the early-return match above"
            ),
        };
        let text = text_result?;
        self.maybe_refresh_skills_after_tool(name, &raw_args, &text);
        Ok(ToolOutput::Text(text))
    }
}

/// Execute a tool call that the planner classified as parallel-safe.
///
/// Parses through the typed [`crate::tool::Tool`] enum so the parallel-safety
/// gate is enforced by exhaustive `match` on `Tool::parallel_safe` — drift
/// between the planner and the dispatcher is impossible without a compile
/// error. Non-parallel-safe variants return a hard error so the planner bug
/// surfaces in tests instead of silent misbehaviour.
///
/// **No `ToolOutputDelta` from this path.** The surrounding loop emits
/// `ToolExecuting` / `ToolResult` events with the correct `tool_call_id`,
/// but the parallel dispatcher does not thread that id into the inner
/// handlers. None of the parallel-safe tools today (`read_file`,
/// `read_memory`, `memory_search`, `list_skills`, `list_channels`,
/// `list_dir`, `web_fetch`, `web_search`, `apply_patch`) emit streaming
/// output deltas, so the gap is invisible. Adding a tool here that wants
/// `ToolOutputDelta` requires extending this signature with `tool_call_id`
/// first, otherwise the delta will land at the OpenAI compat layer with an
/// empty id (see `agent_dispatcher::translate_events`).
pub(super) async fn dispatch_parallel_safe_tool(
    name: &str,
    args_json: &str,
    config: &Config,
    _event_tx: &mpsc::Sender<AgentEvent>,
    execution_context: Option<&crate::execution_context::ExecutionContext>,
) -> Result<ToolOutput> {
    // Reject unknown / non-parallel-safe tools by *name* before parsing args.
    // Surface as a loud Err so a planner/dispatcher mismatch fails tests rather
    // than masquerading as an arg-validation error.
    if !crate::tool::is_parallel_safe_name(name) {
        return Err(anyhow::anyhow!(
            "dispatch_parallel_safe_tool called with non-parallel-safe tool '{name}' — planner/dispatch mismatch"
        ));
    }
    let raw_args: serde_json::Value = match serde_json::from_str(args_json) {
        Ok(v) => v,
        Err(e) => {
            return Ok(ToolOutput::Text(format!(
                "Error: Invalid JSON arguments: {e}. Please provide valid JSON."
            )));
        }
    };
    let tool = match crate::tool::Tool::parse(name, raw_args) {
        Ok(t) => t,
        Err(crate::tool::ToolParseError::UnknownTool(n)) => {
            return Err(anyhow::anyhow!(
                "dispatch_parallel_safe_tool called with unknown tool '{n}' — planner/dispatch mismatch"
            ));
        }
        Err(crate::tool::ToolParseError::InvalidArgs { tool, source }) => {
            return Ok(ToolOutput::Text(format!(
                "Error: Invalid arguments for tool '{tool}': {source}"
            )));
        }
    };

    debug_assert!(tool.parallel_safe(), "is_parallel_safe_name drift");

    match &tool {
        crate::tool::Tool::ReadFile(args) => match execution_context {
            Some(ctx) => crate::tool::read_file::handle_with_context(args, config, ctx),
            None => crate::tool::read_file::handle(args, config),
        },
        crate::tool::Tool::ReadMemory(args) => crate::tool::read_memory::handle(args, config)
            .await
            .map(ToolOutput::Text),
        crate::tool::Tool::MemorySearch(args) => crate::tool::memory_search::handle(args, config)
            .await
            .map(ToolOutput::Text),
        crate::tool::Tool::ListSkills => {
            crate::tool::list::handle_list_skills(config).map(ToolOutput::Text)
        }
        crate::tool::Tool::ListChannels => {
            crate::tool::list::handle_list_channels(config).map(ToolOutput::Text)
        }
        crate::tool::Tool::ListDir(args) => match execution_context {
            Some(ctx) => crate::tool::list_dir::handle_with_context(args, config, ctx),
            None => crate::tool::list_dir::handle(args, config),
        }
        .map(ToolOutput::Text),
        crate::tool::Tool::WebFetch(args) => crate::tool::web_fetch::handle(args, config)
            .await
            .map(ToolOutput::Text),
        crate::tool::Tool::WebSearch(args) => crate::tool::web_search::handle(args, config)
            .await
            .map(ToolOutput::Text),
        crate::tool::Tool::ApplyPatch(args) => match execution_context {
            Some(ctx) => {
                crate::tool::apply_patch::handle_apply_patch_with_context(args, config, ctx)
            }
            None => crate::tool::apply_patch::handle_apply_patch(args, config),
        }
        .map(ToolOutput::Text),
        crate::tool::Tool::ApplySkillPatch(args) => {
            crate::tool::apply_patch::handle_apply_skill_patch(args).map(ToolOutput::Text)
        }
        // Anything still reaching here despite `parallel_safe()` returning true
        // is a closed-set inconsistency — fail loudly so tests catch it.
        other => Err(anyhow::anyhow!(
            "dispatch_parallel_safe_tool: variant '{}' is parallel_safe() but lacks a handler arm",
            other.name()
        )),
    }
}
