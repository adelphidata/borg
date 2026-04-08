use crate::config::Config;
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
        ToolDefinition::new("write_memory", "Write or append to a memory file. Use filename 'IDENTITY.md' to update personality, 'MEMORY.md' for the index, or any other name for topic-specific memories. Use scope='local' to write to project-local memory (.borg/ in CWD).", serde_json::json!({"type":"object","properties":{"filename":{"type":"string","description":"Name of the memory file"},"content":{"type":"string","description":"Content to write"},"append":{"type":"boolean","description":"Append instead of overwriting","default":false},"scope":{"type":"string","enum":["global","local"],"description":"Memory scope: 'global' (default, ~/.borg/) or 'local' (CWD/.borg/)","default":"global"}},"required":["filename","content"]})),
        ToolDefinition::new("read_memory", "Read a memory file.", serde_json::json!({"type":"object","properties":{"filename":{"type":"string","description":"Name of the memory file to read"}},"required":["filename"]})),
        ToolDefinition::new("memory_search", "Search memory files semantically. Use before answering questions about prior work, decisions, preferences, or anything previously discussed.", serde_json::json!({"type":"object","properties":{"query":{"type":"string","description":"Search query"},"max_results":{"type":"integer","description":"Maximum results to return (default: 5)","default":5},"min_score":{"type":"number","description":"Minimum relevance score 0-1 (default: 0.2)","default":0.2}},"required":["query"]})),
        ToolDefinition::new("list", "List resources. Specify what to list: skills, channels, or agents.", serde_json::json!({"type":"object","properties":{"what":{"type":"string","enum":["skills","channels","agents"],"description":"What to list"}},"required":["what"]})),
        ToolDefinition::new("apply_patch", "Create, update, or delete files using the patch DSL. File paths can be relative to cwd, absolute, or use ~/. Use target to choose location: cwd (default), skills (~/.borg/skills/), channels (~/.borg/channels/).", serde_json::json!({"type":"object","properties":{"patch":{"type":"string","description":"The patch content in the patch DSL format"},"target":{"type":"string","enum":["cwd","skills","channels"],"description":"Where to apply the patch (default: cwd)","default":"cwd"}},"required":["patch"]})),
        ToolDefinition::new("run_shell", "Execute a shell command. Requires user confirmation before execution.", serde_json::json!({"type":"object","properties":{"command":{"type":"string","description":"Shell command to execute"}},"required":["command"]})),
        ToolDefinition::new("read_file", "Read a file's contents. Returns text with line numbers for code files, renders images visually, and extracts text from PDFs. Use offset/limit to read specific line ranges of large files.", serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"File path (relative to cwd or absolute)"},"offset":{"type":"integer","description":"Start line, 1-based (default: 1)"},"limit":{"type":"integer","description":"Max lines to read (default: all, truncated at max_chars)"},"max_chars":{"type":"integer","description":"Max characters to return (default: 50000)"}},"required":["path"]})),
        ToolDefinition::new("list_dir", "List the contents of a directory. Returns file and subdirectory names with types and sizes. Use this to explore project structure.", serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Directory path (relative to cwd or absolute). Defaults to current directory."},"depth":{"type":"integer","description":"Maximum depth to recurse (default: 1, max: 3)"},"include_hidden":{"type":"boolean","description":"Include hidden files/dirs (default: false)"}}})),
    ];

    if config.web.enabled {
        defs.push(ToolDefinition::new("web_fetch", "Fetch a URL and return its text content. HTML pages are automatically converted to plain text.", serde_json::json!({"type":"object","properties":{"url":{"type":"string","description":"The URL to fetch"},"max_chars":{"type":"integer","description":"Maximum characters to return (default: 50000)","default":50000}},"required":["url"]})));
        defs.push(ToolDefinition::new("web_search", "Search the web and return results with titles, URLs, and snippets.", serde_json::json!({"type":"object","properties":{"query":{"type":"string","description":"The search query"}},"required":["query"]})));
    }

    defs.push(build_schedule_tool_def(config));

    defs.push(ToolDefinition::new(
        "projects",
        "Manage projects. Projects group related workflows and track workstreams. Actions: create, list, get, update, archive, delete.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "list", "get", "update", "archive", "delete"],
                    "description": "Action to perform"
                },
                "id": { "type": "string", "description": "Project ID (required for get/update/archive/delete)" },
                "name": { "type": "string", "description": "Project name (required for create, optional for update)" },
                "description": { "type": "string", "description": "Project description" },
                "status": { "type": "string", "enum": ["active", "archived"], "description": "Filter by status (for list) or set status (for update)" }
            },
            "required": ["action"]
        }),
    ));

    if config.browser.enabled {
        defs.push(ToolDefinition::new(
            "browser",
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
            "text_to_speech",
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

    if config.image_gen.enabled {
        defs.push(ToolDefinition::new(
            "generate_image",
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

    // Request user input — always available (disabled at gateway level)
    defs.push(ToolDefinition::new(
        "request_user_input",
        "Ask the user a question and wait for their response. Use when you need clarification or a decision before proceeding. Do not use for routine confirmations — only when genuinely blocked.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string", "description": "The question to ask the user" }
            },
            "required": ["prompt"]
        }),
    ));

    defs
}

/// Build the schedule tool definition, conditionally including workflow type
/// when workflows are active for the current model.
fn build_schedule_tool_def(config: &Config) -> ToolDefinition {
    let workflows_on = crate::workflow::workflows_active(config);

    let type_enum = if workflows_on {
        serde_json::json!(["prompt", "command", "workflow"])
    } else {
        serde_json::json!(["prompt", "command"])
    };

    let type_desc = if workflows_on {
        "Job type: 'prompt' for AI tasks, 'command' for shell cron jobs, 'workflow' for multi-step task orchestration (required for create, used as filter for list)"
    } else {
        "Job type: 'prompt' for AI tasks, 'command' for shell cron jobs (required for create, used as filter for list)"
    };

    let description = if workflows_on {
        "Manage scheduled jobs and workflows. Use type='prompt' for AI tasks, type='command' for shell cron jobs, or type='workflow' for multi-step task orchestration. Actions: create, list, get, update, pause, resume, cancel, delete, runs, run_now."
    } else {
        "Manage scheduled jobs. Use type='prompt' for AI tasks or type='command' for shell cron jobs. Actions: create, list, get, update, pause, resume, cancel, delete, runs, run_now."
    };

    let mut properties = serde_json::json!({
        "action": {"type":"string","enum":["create","list","get","update","pause","resume","cancel","delete","runs","run_now"],"description":"Action to perform"},
        "type": {"type":"string","enum": type_enum,"description": type_desc},
        "id": {"type":"string","description":"Job/workflow ID (required for get/update/pause/resume/cancel/delete/runs/run_now)"},
        "name": {"type":"string","description":"Job/workflow name (required for create, optional for update)"},
        "prompt": {"type":"string","description":"Prompt to execute (for type=prompt, required for create)"},
        "command": {"type":"string","description":"Shell command to execute (for type=command, required for create)"},
        "schedule": {"type":"string","description":"5-field cron expression (e.g. '*/5 * * * *'). Required for type=command create."},
        "schedule_type": {"type":"string","enum":["cron","interval","once"],"description":"Schedule type (for type=prompt, required for create)"},
        "schedule_expr": {"type":"string","description":"Cron expression or interval (for type=prompt, required for create)"},
        "timezone": {"type":"string","description":"Timezone (default: local)"},
        "max_retries": {"type":"integer","description":"Max retry attempts for transient failures (default: 3)"},
        "timeout_ms": {"type":"integer","description":"Timeout in milliseconds (default: 300000)"},
        "delivery_channel": {"type":"string","description":"Channel to deliver results to (telegram, slack, discord). Use 'origin' when scheduling from a chat message to reply back in the same channel/thread."},
        "delivery_target": {"type":"string","description":"Target chat/channel ID for delivery. Omit when delivery_channel='origin'."},
        "limit": {"type":"integer","description":"Number of runs to return (for runs action, default: 5)"}
    });

    if workflows_on {
        properties["goal"] = serde_json::json!({"type":"string","description":"Workflow goal — what the entire workflow should accomplish (required for type=workflow create)"});
        properties["steps"] = serde_json::json!({"type":"array","description":"Workflow steps (required for type=workflow create)","items":{"type":"object","properties":{"title":{"type":"string","description":"Step title"},"instructions":{"type":"string","description":"Detailed instructions for this step"},"max_retries":{"type":"integer","description":"Max retries for this step (default: 3)"},"timeout_ms":{"type":"integer","description":"Timeout for this step in ms (default: 300000)"}},"required":["title","instructions"]}});
        properties["status"] = serde_json::json!({"type":"string","description":"Filter workflows by status (for type=workflow list)"});
        properties["project_id"] = serde_json::json!({"type":"string","description":"Project ID to associate this workflow with (for type=workflow create)"});
    }

    ToolDefinition::new(
        "schedule",
        description,
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
        assert!(names.contains(&"projects"));
        assert!(names.contains(&"request_user_input"));
    }

    #[test]
    fn projects_tool_has_correct_schema() {
        let config = Config::default();
        let defs = core_tool_definitions(&config);
        let projects = defs
            .iter()
            .find(|d| d.function.name == "projects")
            .expect("should have projects tool");

        let props = &projects.function.parameters["properties"];
        assert!(props["action"].is_object());
        assert!(props["id"].is_object());
        assert!(props["name"].is_object());
        assert!(props["description"].is_object());
        assert!(props["status"].is_object());

        let actions: Vec<&str> = props["action"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(actions.contains(&"create"));
        assert!(actions.contains(&"list"));
        assert!(actions.contains(&"get"));
        assert!(actions.contains(&"update"));
        assert!(actions.contains(&"archive"));
        assert!(actions.contains(&"delete"));
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
    fn schedule_tool_excludes_workflow_when_disabled() {
        let mut config = Config::default();
        config.workflow.enabled = "off".to_string();
        let def = build_schedule_tool_def(&config);

        let type_enum = &def.function.parameters["properties"]["type"]["enum"];
        let types: Vec<&str> = type_enum
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(types.contains(&"prompt"));
        assert!(types.contains(&"command"));
        assert!(
            !types.contains(&"workflow"),
            "workflow type should be hidden"
        );
        assert!(
            def.function.parameters["properties"].get("steps").is_none(),
            "steps param should be absent"
        );
        assert!(
            def.function.parameters["properties"].get("goal").is_none(),
            "goal param should be absent"
        );
    }

    #[test]
    fn schedule_tool_includes_workflow_when_enabled() {
        let mut config = Config::default();
        config.workflow.enabled = "on".to_string();
        let def = build_schedule_tool_def(&config);

        let type_enum = &def.function.parameters["properties"]["type"]["enum"];
        let types: Vec<&str> = type_enum
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(
            types.contains(&"workflow"),
            "workflow type should be present"
        );
        assert!(
            def.function.parameters["properties"].get("steps").is_some(),
            "steps param should be present"
        );
        assert!(
            def.function.parameters["properties"].get("goal").is_some(),
            "goal param should be present"
        );
        assert!(
            def.function.description.contains("workflow"),
            "description should mention workflows"
        );
    }

    #[test]
    fn schedule_tool_auto_with_opus_excludes_workflow() {
        let mut config = Config::default();
        config.workflow.enabled = "auto".to_string();
        config.llm.model = "claude-opus-4".to_string();
        let def = build_schedule_tool_def(&config);

        let type_enum = &def.function.parameters["properties"]["type"]["enum"];
        let types: Vec<&str> = type_enum
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(!types.contains(&"workflow"));
    }

    #[test]
    fn schedule_tool_auto_with_weak_model_includes_workflow() {
        let mut config = Config::default();
        config.workflow.enabled = "auto".to_string();
        config.llm.model = "llama-3.3-70b".to_string();
        let def = build_schedule_tool_def(&config);

        let type_enum = &def.function.parameters["properties"]["type"]["enum"];
        let types: Vec<&str> = type_enum
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(types.contains(&"workflow"));
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
