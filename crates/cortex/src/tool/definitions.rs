use crate::config::Config;
use crate::tool::names as tool_names;
use crate::types::ToolDefinition;

/// Build the core tool definitions sent to the LLM.
///
/// IMPORTANT: Be conservative adding new tools here. Every tool's JSON schema is
/// included in the system prompt on every turn, directly consuming context tokens.
/// Prefer adding actions/parameters to an existing tool over creating a new one.
/// If a new tool is truly needed, ensure it cannot be achieved via `run_shell` or
/// an existing tool with an extra action variant.
pub fn core_tool_definitions(config: &Config) -> Vec<ToolDefinition> {
    let mut defs = vec![
        ToolDefinition::new(tool_names::WRITE_MEMORY, "Write or append a long-term memory entry. Use scope='global' for shared memory or a scoped key such as 'project:abc' or a channel-specific memory scope when isolation matters.", serde_json::json!({"type":"object","properties":{"filename":{"type":"string","description":"Memory entry name. A trailing .md is stripped for backward compatibility."},"content":{"type":"string","description":"Content to write"},"append":{"type":"boolean","description":"Append instead of overwriting","default":false},"scope":{"type":"string","description":"Memory scope. Defaults to 'global'; may also be values like 'project:abc' or another configured scoped-memory key.","default":"global"}},"required":["filename","content"]})),
        ToolDefinition::new(tool_names::READ_MEMORY, "Read a long-term memory entry by name and optional scope.", serde_json::json!({"type":"object","properties":{"filename":{"type":"string","description":"Memory entry name to read"},"scope":{"type":"string","description":"Memory scope. Defaults to 'global'.","default":"global"}},"required":["filename"]})),
        ToolDefinition::new(tool_names::MEMORY_SEARCH, "Search memory context across long-term memory, markdown knowledge files, or past session transcripts with sources=[\"messages\"]. Use before answering questions about prior work, decisions, preferences, or anything previously discussed.", serde_json::json!({"type":"object","properties":{"query":{"type":"string","description":"Search query"},"max_results":{"type":"integer","description":"Maximum results to return (default: 5)","default":5},"min_score":{"type":"number","description":"Minimum relevance score 0-1 (default: 0.2)","default":0.2},"sources":{"type":"array","items":{"type":"string","enum":["memory","messages"]},"description":"Where to search. \"memory\" (default) searches long-term memory plus markdown knowledge files. \"messages\" searches raw past conversation transcripts. Combine to search both."}},"required":["query"]})),
        ToolDefinition::new(tool_names::LIST, "List resources. Specify what to list: skills, channels, or agents.", serde_json::json!({"type":"object","properties":{"what":{"type":"string","enum":["skills","channels","agents"],"description":"What to list"}},"required":["what"]})),
        ToolDefinition::new(tool_names::APPLY_PATCH, "Create, update, or delete files using the patch DSL. File paths can be relative to cwd, absolute, or use ~/. Use target to choose location: cwd (default), skills (~/.borg/skills/), channels (~/.borg/channels/). When the same context lines could match multiple places in a file, anchor the hunk by writing `@@ method_name` (or `@@ class Foo`, etc.) on the line before `@@` to disambiguate; multiple `@@` anchors can be stacked to navigate nested scopes.", serde_json::json!({"type":"object","properties":{"patch":{"type":"string","description":"The patch content in the patch DSL format"},"target":{"type":"string","enum":["cwd","skills","channels"],"description":"Where to apply the patch (default: cwd)","default":"cwd"}},"required":["patch"]})),
        ToolDefinition::new(tool_names::RUN_SHELL, "Execute a shell command. Do not use `sleep`, `at`, or background loops to emulate reminders or delayed tasks — use the `schedule` tool instead. Pass `tty=true` for commands that need a pseudo-terminal (REPLs, programs that detect `isatty`, paginating tools); `workdir` overrides cwd; `timeout_ms` overrides the default per-call timeout. Persistent interactive sessions are not supported — issue a single self-contained command per call.", serde_json::json!({"type":"object","properties":{"command":{"type":"string","description":"Shell command to execute"},"tty":{"type":"boolean","description":"Allocate a pseudo-terminal so the command sees stdout as a TTY (default: false)."},"workdir":{"type":"string","description":"Working directory for the command (default: process cwd)."},"timeout_ms":{"type":"integer","description":"Override the per-call timeout in milliseconds."}},"required":["command"]})),
        ToolDefinition::new(tool_names::READ_FILE, "Read a file's contents. Returns text with line numbers for code files, renders images visually, and extracts text from PDFs. Use offset/limit to read specific line ranges of large files.", serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"File path (relative to cwd or absolute)"},"offset":{"type":"integer","description":"Start line, 1-based (default: 1)"},"limit":{"type":"integer","description":"Max lines to read (default: all, truncated at max_chars)"},"max_chars":{"type":"integer","description":"Max characters to return (default: 50000)"}},"required":["path"]})),
        ToolDefinition::new(tool_names::LIST_DIR, "List the contents of a directory. Returns file and subdirectory names with types and sizes. Pass `fuzzy_match` to instead get a ranked, flat list of files whose paths fuzzy-match the query (e.g. \"parser\", \"agent loop\") — useful for locating files by approximate name without invoking shell `find`/`rg`.", serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Directory path (relative to cwd or absolute). Defaults to current directory."},"depth":{"type":"integer","description":"Maximum depth to recurse (default: 1, max: 3). Ignored when fuzzy_match is set (walk uses an internal noisy-dir filter)."},"include_hidden":{"type":"boolean","description":"Include hidden files/dirs (default: false)"},"fuzzy_match":{"type":"string","description":"Optional fuzzy query. When provided, returns the top-ranked file paths under `path` instead of the directory tree."},"max_results":{"type":"integer","description":"Cap on fuzzy-match results (default: 50, max: 500)."},"offset":{"type":"integer","description":"0-based line offset into the rendered listing for pagination."},"limit":{"type":"integer","description":"Maximum number of lines to return after offset (max: 500). Output will include a footer reporting the window."}}})),
    ];

    if config.web.enabled {
        defs.push(ToolDefinition::new(tool_names::WEB_FETCH, "Fetch a URL and return its text content. HTML pages are automatically converted to plain text.", serde_json::json!({"type":"object","properties":{"url":{"type":"string","description":"The URL to fetch"},"max_chars":{"type":"integer","description":"Maximum characters to return (default: 50000)","default":50000}},"required":["url"]})));
        defs.push(ToolDefinition::new(tool_names::WEB_SEARCH, "Search the web and return results with titles, URLs, and snippets.", serde_json::json!({"type":"object","properties":{"query":{"type":"string","description":"The search query"},"mode":{"type":"string","enum":["auto","live","cached"],"description":"Cache strategy: auto (cache hit if fresh else live + write), live (always hit network + refresh cache), cached (cache lookup only). Default: auto"}},"required":["query"]})));
    }

    defs.push(build_schedule_tool_def());

    defs.extend(crate::wasm_plugins::tool_definitions());

    if config.browser.enabled {
        defs.push(ToolDefinition::new(
            tool_names::BROWSER,
            "Control a headless Chrome browser. Actions: navigate, click, type, screenshot, get_text, evaluate_js, hover, select, press, drag, fill, wait, resize, new_tab, list_tabs, switch_tab, close_tab, get_console_logs, close.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["navigate", "click", "type", "screenshot", "get_text", "evaluate_js", "hover", "select", "press", "drag", "fill", "wait", "resize", "new_tab", "list_tabs", "switch_tab", "close_tab", "get_console_logs", "close"],
                        "description": "Browser action to perform"
                    },
                    "url": { "type": "string", "description": "URL (for navigate, new_tab)" },
                    "selector": { "type": "string", "description": "CSS selector (for click, type, hover, select, get_text, screenshot)" },
                    "text": { "type": "string", "description": "Text to type (for type action)" },
                    "expression": { "type": "string", "description": "JavaScript expression (for evaluate_js)" },
                    "value": { "type": "string", "description": "Value to select or wait for (for select, wait)" },
                    "key": { "type": "string", "description": "Key name to press (for press, e.g. 'Enter', 'Tab')" },
                    "source": { "type": "string", "description": "Source CSS selector (for drag)" },
                    "target": { "type": "string", "description": "Target CSS selector (for drag)" },
                    "fields": { "type": "object", "description": "Map of CSS selector to value (for fill)" },
                    "condition": { "type": "string", "enum": ["text", "element", "url", "load", "js"], "description": "Wait condition type (for wait)" },
                    "width": { "type": "integer", "description": "Viewport width (for resize)" },
                    "height": { "type": "integer", "description": "Viewport height (for resize)" },
                    "tab_index": { "type": "integer", "description": "Tab index (for switch_tab)" }
                },
                "required": ["action"]
            }),
        ));
    }

    if config.tts.enabled {
        defs.push(ToolDefinition::new(
            tool_names::TEXT_TO_SPEECH,
            "Convert text to speech audio. Returns base64-encoded audio data. Use for voice messages, audio responses, or accessibility.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "Text to convert to speech (max 4096 characters)"
                    },
                    "voice": {
                        "type": "string",
                        "description": "Voice name/ID (optional, uses default if omitted)"
                    },
                    "format": {
                        "type": "string",
                        "enum": ["mp3", "opus", "aac", "flac", "wav"],
                        "description": "Audio output format (optional, default: mp3)"
                    }
                },
                "required": ["text"]
            }),
        ));
    }

    // Vision analyze — gated by `web.enabled` since it fetches remote URLs.
    // Data URIs work even with web disabled, but keeping the gate behind a
    // single network-policy switch is the safer default.
    if config.web.enabled {
        defs.push(ToolDefinition::new(
            tool_names::VISION_ANALYZE,
            "Fetch an image from an http(s) URL or data: URI and return it inline so multimodal LLMs can describe it. Local files should be opened with read_file instead.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "image_url": {
                        "type": "string",
                        "description": "https URL or data:image/... base64 URI"
                    },
                    "question": {
                        "type": "string",
                        "description": "Optional question to focus the visual analysis"
                    }
                },
                "required": ["image_url"]
            }),
        ));
    }

    if config.image_gen.enabled {
        defs.push(ToolDefinition::new(
            tool_names::GENERATE_IMAGE,
            "Generate images from a text description using AI. Returns base64-encoded image data.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "Text description of the image to generate"
                    },
                    "count": {
                        "type": "integer",
                        "description": "Number of images to generate (1-4, default: 1)"
                    },
                    "size": {
                        "type": "string",
                        "description": "Image size (e.g. 1024x1024, 1792x1024, 1024x1792)"
                    }
                },
                "required": ["prompt"]
            }),
        ));
    }

    // Request user input — always available (disabled at gateway level).
    // Batched 1–4 question carousel; UI auto-appends an "Other" row per
    // question and exposes a Tab-toggled notes box.
    defs.push(ToolDefinition::new(
        tool_names::REQUEST_USER_INPUT,
        "Ask the user 1–4 short multiple-choice questions in one batch and wait for answers. The UI auto-appends an 'Other' option per question and shows a Tab-toggled notes field. Use this in Plan mode to clarify requirements, in onboarding to gather setup choices, or any time you're genuinely blocked on a decision. Don't use for routine confirmations.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 4,
                    "description": "Questions to show the user. Prefer 1; never exceed 4.",
                    "items": {
                        "type": "object",
                        "required": ["id", "header", "question", "options"],
                        "properties": {
                            "id": { "type": "string", "description": "Stable snake_case id for mapping the answer back." },
                            "header": { "type": "string", "description": "Short header chip label shown in the UI (12 chars or fewer, e.g. 'Timezone')." },
                            "question": { "type": "string", "description": "Single-sentence prompt shown to the user." },
                            "multi_select": { "type": "boolean", "description": "Allow multiple selections (default false)." },
                            "options": {
                                "type": "array",
                                "minItems": 2,
                                "maxItems": 4,
                                "description": "Provide 2–4 mutually exclusive choices. Put the recommended option first and suffix its label with '(Recommended)'. Do not include an 'Other' option — the client adds a free-form one automatically.",
                                "items": {
                                    "type": "object",
                                    "required": ["label", "description"],
                                    "properties": {
                                        "label": { "type": "string", "description": "1–5 word user-facing label, returned verbatim as the answer." },
                                        "description": { "type": "string", "description": "One short sentence explaining the tradeoff." }
                                    }
                                }
                            }
                        }
                    }
                }
            },
            "required": ["questions"]
        }),
    ));

    // Per-session goals. The three tools form the agent-visible surface for
    // the persisted goal: read state, create when none exists, and mark
    // complete. Pause/resume/budget changes and objective edits are
    // user-controlled and are *not* exposed here. Gated by `goals.enabled`
    // so disabling the subsystem also removes ~1.5 KB of schema from every
    // turn (Tool Conservation invariant, CLAUDE.md).
    if config.goals.enabled {
        defs.push(ToolDefinition::new(
        tool_names::GET_GOAL,
        "Get the current goal for this session, including status, objective, token budget and tokens used, elapsed time, and remaining token budget. Returns null when no goal is set.",
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
    ));
        defs.push(ToolDefinition::new(
        tool_names::CREATE_GOAL,
        "Create a goal only when explicitly requested by the user or developer instructions; do not infer goals from ordinary tasks. Set token_budget only when an explicit token budget is requested. Fails if a goal already exists; the user must edit or clear the existing goal first.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "objective": {"type": "string", "description": "Required. The concrete objective to start pursuing. This starts a new active goal only when no goal is currently defined."},
                "token_budget": {"type": "integer", "description": "Optional positive token budget for the new active goal."}
            },
            "required": ["objective"],
            "additionalProperties": false
        }),
    ));
        defs.push(ToolDefinition::new(
        tool_names::UPDATE_GOAL,
        "Mark the existing goal as achieved. The only accepted value for `status` is \"complete\". Pause, resume, budget edits, and objective replacement are user-controlled and not reachable through this tool. Do not mark a goal complete merely because its budget is nearly exhausted or because you are stopping work. When marking a budgeted goal complete, report the final token usage from the tool result to the user.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "status": {"type": "string", "enum": ["complete"], "description": "Required. Set to complete only when the objective is achieved and no required work remains."}
            },
            "required": ["status"],
            "additionalProperties": false
        }),
    ));
    }

    defs
}

/// Build the schedule tool definition.
///
/// Supports two task kinds — `prompt` (LLM-driven) and `command` (shell cron).
/// Prompt tasks may optionally specify a `pre_run_command` whose stdout is
/// injected into the prompt as `<preprocessed_context>`, plus a `[SILENT]`
/// sentinel the model can return to suppress channel delivery.
fn build_schedule_tool_def() -> ToolDefinition {
    let properties = serde_json::json!({
        "action": {"type":"string","enum":["create","list","get","update","pause","resume","cancel","delete","runs","run_now"],"description":"Action to perform"},
        "type": {"type":"string","enum":["prompt","command"],"description":"Job type: 'prompt' for AI tasks, 'command' for shell cron jobs (required for create, used as filter for list)"},
        "id": {"type":"string","description":"Job ID (required for get/update/pause/resume/cancel/delete/runs/run_now)"},
        "name": {"type":"string","description":"Job name (required for create, optional for update)"},
        "prompt": {"type":"string","description":"Prompt to execute (for type=prompt, required for create). The model may return exactly '[SILENT]' on its first non-empty line to suppress channel delivery for that run."},
        "command": {"type":"string","description":"Shell command to execute (for type=command, required for create)"},
        "schedule": {"type":"string","description":"5-field cron expression (e.g. '*/5 * * * *'). Required for type=command create."},
        "schedule_type": {"type":"string","enum":["cron","interval","once"],"description":"For type=prompt create: 'once' = fire one time at a specific future datetime (use for reminders). 'interval' = repeat every fixed duration. 'cron' = 5-field cron expression."},
        "schedule_expr": {"type":"string","description":"For type=prompt create. once: ISO-8601 timestamp like '2026-04-22T09:00:00' (local tz if no offset) or '2026-04-22T09:00:00Z'. interval: '30m', '2h', '1d'. cron: '0 9 * * 1-5' (weekdays 9am)."},
        "timezone": {"type":"string","description":"Timezone (default: local)"},
        "max_retries": {"type":"integer","description":"Max retry attempts for transient failures (default: 3)"},
        "timeout_ms": {"type":"integer","description":"Timeout in milliseconds (default: 300000)"},
        "delivery_channel": {"type":"string","description":"Channel to deliver results to (telegram, slack, discord). Use 'origin' when scheduling from a chat message to reply back in the same channel/thread."},
        "delivery_target": {"type":"string","description":"Target chat/channel ID for delivery. Omit when delivery_channel='origin'."},
        "limit": {"type":"integer","description":"Number of runs to return (for runs action, default: 5)"},
        "pre_run_command": {"type":"string","description":"Optional shell command (only for type=prompt) that runs before the LLM. Its stdout is prepended to the prompt as a <preprocessed_context source=\"pre_run\"> XML block. On non-zero exit or timeout the LLM still runs and the block is marked status=\"failed\". Use this to do mechanical fetching/parsing in shell so the model only has to reason."},
        "pre_run_cwd": {"type":"string","description":"Optional working directory for pre_run_command (default: $HOME)."},
        "pre_run_timeout_ms": {"type":"integer","description":"Timeout (ms) for pre_run_command (default: 60000)."}
    });

    ToolDefinition::new(
        tool_names::SCHEDULE,
        "Schedule reminders, one-shot tasks, recurring AI prompts, and cron commands. USE THIS whenever the user asks to be reminded, wants something to run later, or sets up any recurring automation (\"remind me tomorrow at 9am\", \"every weekday at 8\", \"in 30 minutes\", \"ping me when…\"). Do NOT emulate with run_shell sleep loops. type='prompt' = AI task, type='command' = shell cron. Optional pre_run_command runs a shell preamble whose stdout is injected into the prompt — use it instead of asking the model to fetch/parse data itself. Actions: create, list, get, update, pause, resume, cancel, delete, runs, run_now.",
        serde_json::json!({"type":"object","properties": properties,"required":["action"]}),
    )
}

/// Strip redundant metadata from a tool schema to reduce token overhead.
///
/// - Removes `"default"` keys (LLM infers from description)
/// - For properties with an `"enum"` constraint, removes parenthetical enum
///   listings from the description (the constraint already communicates valid values)
/// - Strips trailing whitespace from descriptions
pub fn compact_tool_schema(schema: &mut serde_json::Value) {
    if let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) {
        for (_key, prop) in props.iter_mut() {
            if let Some(obj) = prop.as_object_mut() {
                // Remove "default" keys — LLM can infer defaults from description
                obj.remove("default");

                // If property has enum constraint, trim redundant enum listing from description
                if obj.contains_key("enum") {
                    if let Some(desc) = obj.get_mut("description") {
                        if let Some(s) = desc.as_str() {
                            let trimmed = strip_enum_parenthetical(s);
                            *desc = serde_json::Value::String(trimmed);
                        }
                    }
                }

                // Recurse into nested object schemas (e.g. items in arrays)
                if let Some(items) = obj.get_mut("items") {
                    compact_tool_schema(items);
                }
            }
        }
    }
}

/// Remove parenthetical enum listings like "(one of: a, b, c)" or
/// "(e.g. 1024x1024, 1792x1024)" from a description string. Only strips
/// parentheticals that start with known enum-listing prefixes to avoid
/// removing meaningful context like "(relative to project root)".
fn strip_enum_parenthetical(desc: &str) -> String {
    let trimmed = desc.trim_end();
    if let Some(paren_start) = trimmed.rfind(" (") {
        let after = &trimmed[paren_start + 2..]; // skip " ("
        let is_enum_listing = after.starts_with("one of:")
            || after.starts_with("e.g.")
            || after.starts_with("e.g ")
            || after.starts_with("options:")
            || after.starts_with("default:");
        if is_enum_listing && trimmed.ends_with(')') {
            return trimmed[..paren_start].trim_end().to_string();
        }
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_tools_include_essential_tools() {
        let config = Config::default();
        let defs = core_tool_definitions(&config);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();

        // Essential tools always present
        assert!(names.contains(&"write_memory"));
        assert!(names.contains(&"read_memory"));
        assert!(names.contains(&"memory_search"));
        assert!(names.contains(&"list"));
        assert!(names.contains(&"apply_patch"));
        assert!(names.contains(&"run_shell"));
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"list_dir"));
        assert!(names.contains(&"schedule"));
        assert!(!names.contains(&"projects"), "projects tool was demolished");
        assert!(!names.contains(&"workflow"), "workflow tool was demolished");
        assert!(names.contains(&"request_user_input"));
    }

    #[test]
    fn schedule_tool_schema_has_no_workflow_type() {
        let def = build_schedule_tool_def();
        let types: Vec<&str> = def.function.parameters["properties"]["type"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(types, vec!["prompt", "command"]);
    }

    #[test]
    fn schedule_tool_schema_has_no_project_id_param() {
        let def = build_schedule_tool_def();
        let props = &def.function.parameters["properties"];
        assert!(props.get("project_id").is_none());
        assert!(props.get("steps").is_none());
        assert!(props.get("goal").is_none());
    }

    #[test]
    fn schedule_tool_schema_includes_pre_run_params() {
        let def = build_schedule_tool_def();
        let props = &def.function.parameters["properties"];
        assert!(props.get("pre_run_command").is_some());
        assert!(props.get("pre_run_cwd").is_some());
        assert!(props.get("pre_run_timeout_ms").is_some());
    }

    #[test]
    fn projects_tool_is_not_registered() {
        let config = Config::default();
        let defs = core_tool_definitions(&config);
        assert!(defs.iter().all(|d| d.function.name != "projects"));
    }

    #[test]
    fn browser_tool_excluded_when_disabled() {
        let mut config = Config::default();
        config.browser.enabled = false;
        let defs = core_tool_definitions(&config);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(!names.contains(&"browser"));
    }

    #[test]
    fn browser_tool_included_when_enabled() {
        let mut config = Config::default();
        config.browser.enabled = true;
        let defs = core_tool_definitions(&config);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(names.contains(&"browser"));
    }

    #[test]
    fn web_tools_excluded_when_disabled() {
        let mut config = Config::default();
        config.web.enabled = false;
        let defs = core_tool_definitions(&config);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(!names.contains(&"web_fetch"));
        assert!(!names.contains(&"web_search"));
    }

    #[test]
    fn web_tools_included_when_enabled() {
        let mut config = Config::default();
        config.web.enabled = true;
        let defs = core_tool_definitions(&config);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(names.contains(&"web_fetch"));
        assert!(names.contains(&"web_search"));
    }

    #[test]
    fn tts_tool_excluded_when_disabled() {
        let mut config = Config::default();
        config.tts.enabled = false;
        let defs = core_tool_definitions(&config);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(!names.contains(&"text_to_speech"));
    }

    #[test]
    fn tts_tool_included_when_enabled() {
        let mut config = Config::default();
        config.tts.enabled = true;
        let defs = core_tool_definitions(&config);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(names.contains(&"text_to_speech"));
    }

    #[test]
    fn image_gen_tool_excluded_when_disabled() {
        let mut config = Config::default();
        config.image_gen.enabled = false;
        let defs = core_tool_definitions(&config);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(!names.contains(&"generate_image"));
    }

    #[test]
    fn image_gen_tool_included_when_enabled() {
        let mut config = Config::default();
        config.image_gen.enabled = true;
        let defs = core_tool_definitions(&config);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(names.contains(&"generate_image"));
    }

    #[test]
    fn all_tool_definitions_have_valid_schema() {
        let mut config = Config::default();
        config.browser.enabled = true;
        config.web.enabled = true;
        config.tts.enabled = true;
        config.image_gen.enabled = true;
        let defs = core_tool_definitions(&config);

        for def in &defs {
            assert!(!def.function.name.is_empty(), "tool name must not be empty");
            assert!(
                !def.function.description.is_empty(),
                "tool {} has empty description",
                def.function.name
            );
            // schema must be a JSON object with "type": "object"
            assert_eq!(
                def.function.parameters["type"].as_str(),
                Some("object"),
                "tool {} schema must have type=object",
                def.function.name,
            );
        }
    }

    #[test]
    fn no_duplicate_tool_names() {
        let mut config = Config::default();
        config.browser.enabled = true;
        config.web.enabled = true;
        config.tts.enabled = true;
        config.image_gen.enabled = true;
        let defs = core_tool_definitions(&config);
        let mut seen = std::collections::HashSet::new();
        for def in &defs {
            assert!(
                seen.insert(&def.function.name),
                "duplicate tool name: {}",
                def.function.name
            );
        }
    }

    #[test]
    fn tool_definitions_sorted_deterministically_after_sort() {
        let mut config = Config::default();
        config.browser.enabled = true;
        config.web.enabled = true;
        config.tts.enabled = true;
        config.image_gen.enabled = true;
        let mut defs = core_tool_definitions(&config);

        // Apply the same sort used in Agent::build_tool_definitions
        defs.sort_by(|a, b| a.function.name.cmp(&b.function.name));

        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        let mut sorted_names = names.clone();
        sorted_names.sort();
        assert_eq!(names, sorted_names, "tools should be in alphabetical order");
    }

    #[test]
    fn tool_sort_is_stable_across_config_variants() {
        // Tools with all optional features enabled
        let mut config_full = Config::default();
        config_full.browser.enabled = true;
        config_full.web.enabled = true;
        config_full.tts.enabled = true;
        config_full.image_gen.enabled = true;
        let mut defs_full = core_tool_definitions(&config_full);
        defs_full.sort_by(|a, b| a.function.name.cmp(&b.function.name));

        // Same config again — order must be identical
        let mut defs_full2 = core_tool_definitions(&config_full);
        defs_full2.sort_by(|a, b| a.function.name.cmp(&b.function.name));

        let names1: Vec<&str> = defs_full.iter().map(|d| d.function.name.as_str()).collect();
        let names2: Vec<&str> = defs_full2
            .iter()
            .map(|d| d.function.name.as_str())
            .collect();
        assert_eq!(names1, names2, "tool order must be identical across calls");
    }

    #[test]
    fn compact_schema_removes_defaults() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "count": {"type": "integer", "description": "Number of items", "default": 5},
                "name": {"type": "string", "description": "The name"}
            }
        });
        compact_tool_schema(&mut schema);
        assert!(schema["properties"]["count"].get("default").is_none());
        assert!(schema["properties"]["name"].get("default").is_none());
    }

    #[test]
    fn compact_schema_shortens_enum_descriptions() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "format": {
                    "type": "string",
                    "enum": ["mp3", "wav"],
                    "description": "Audio format (one of: mp3, wav)"
                }
            }
        });
        compact_tool_schema(&mut schema);
        assert_eq!(
            schema["properties"]["format"]["description"]
                .as_str()
                .unwrap(),
            "Audio format"
        );
    }

    #[test]
    fn compact_schema_preserves_required_fields() {
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "description": "Action to perform", "enum": ["a","b"]}
            },
            "required": ["action"]
        });
        compact_tool_schema(&mut schema);
        assert!(schema["properties"]["action"]["type"].is_string());
        assert!(schema["properties"]["action"]["description"].is_string());
        assert!(schema["required"].is_array());
    }

    #[test]
    fn compact_schema_round_trip_valid() {
        let mut config = Config::default();
        config.browser.enabled = true;
        config.web.enabled = true;
        let defs = core_tool_definitions(&config);

        for def in &defs {
            let mut schema = def.function.parameters.clone();
            compact_tool_schema(&mut schema);
            assert_eq!(
                schema["type"].as_str(),
                Some("object"),
                "tool {} schema must remain type=object after compaction",
                def.function.name,
            );
        }
    }

    #[test]
    fn compact_schema_reduces_token_count() {
        let config = Config::default();
        let defs = core_tool_definitions(&config);
        // write_memory has "default" keys (append: false, scope: "global")
        let write_mem = defs
            .iter()
            .find(|d| d.function.name == "write_memory")
            .expect("should have write_memory tool");

        let before = write_mem.function.parameters.to_string().len();
        let mut schema = write_mem.function.parameters.clone();
        compact_tool_schema(&mut schema);
        let after = schema.to_string().len();

        assert!(
            after < before,
            "compacted schema should be smaller: {after} >= {before}"
        );
    }

    #[test]
    fn strip_enum_parenthetical_removes_trailing() {
        assert_eq!(
            strip_enum_parenthetical("Audio format (one of: mp3, wav)"),
            "Audio format"
        );
        assert_eq!(strip_enum_parenthetical("No parens here"), "No parens here");
        assert_eq!(
            strip_enum_parenthetical("Size (e.g. 1024x1024, 1792x1024)"),
            "Size"
        );
    }
}
