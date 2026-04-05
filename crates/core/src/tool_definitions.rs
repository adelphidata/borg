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
        ToolDefinition::new("apply_patch", "Create, update, or delete files using the patch DSL. Use target to choose location: cwd (default), skills (~/.borg/skills/), channels (~/.borg/channels/).", serde_json::json!({"type":"object","properties":{"patch":{"type":"string","description":"The patch content in the patch DSL format"},"target":{"type":"string","enum":["cwd","skills","channels"],"description":"Where to apply the patch (default: cwd)","default":"cwd"}},"required":["patch"]})),
        ToolDefinition::new("run_shell", "Execute a shell command. Requires user confirmation before execution.", serde_json::json!({"type":"object","properties":{"command":{"type":"string","description":"Shell command to execute"}},"required":["command"]})),
        ToolDefinition::new("read_file", "Read a file's contents. Returns text with line numbers for code files, renders images visually, and extracts text from PDFs. Use offset/limit to read specific line ranges of large files.", serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"File path (relative to cwd or absolute)"},"offset":{"type":"integer","description":"Start line, 1-based (default: 1)"},"limit":{"type":"integer","description":"Max lines to read (default: all, truncated at max_chars)"},"max_chars":{"type":"integer","description":"Max characters to return (default: 50000)"}},"required":["path"]})),
        ToolDefinition::new("list_dir", "List the contents of a directory. Returns file and subdirectory names with types and sizes. Use this to explore project structure.", serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Directory path (relative to cwd or absolute). Defaults to current directory."},"depth":{"type":"integer","description":"Maximum depth to recurse (default: 1, max: 3)"},"include_hidden":{"type":"boolean","description":"Include hidden files/dirs (default: false)"}}})),
    ];

    if config.web.enabled {
        defs.push(ToolDefinition::new("web_fetch", "Fetch a URL and return its text content. HTML pages are automatically converted to plain text.", serde_json::json!({"type":"object","properties":{"url":{"type":"string","description":"The URL to fetch"},"max_chars":{"type":"integer","description":"Maximum characters to return (default: 50000)","default":50000}},"required":["url"]})));
        defs.push(ToolDefinition::new("web_search", "Search the web and return results with titles, URLs, and snippets.", serde_json::json!({"type":"object","properties":{"query":{"type":"string","description":"The search query"}},"required":["query"]})));
    }

    defs.push(ToolDefinition::new("schedule", "Manage scheduled jobs. Use type='prompt' for AI tasks or type='command' for shell cron jobs. Actions: create, list, get, update, pause, resume, cancel, delete, runs, run_now.", serde_json::json!({"type":"object","properties":{"action":{"type":"string","enum":["create","list","get","update","pause","resume","cancel","delete","runs","run_now"],"description":"Action to perform"},"type":{"type":"string","enum":["prompt","command"],"description":"Job type: 'prompt' for AI tasks, 'command' for shell cron jobs (required for create, used as filter for list)"},"id":{"type":"string","description":"Job ID (required for get/update/pause/resume/cancel/delete/runs/run_now)"},"name":{"type":"string","description":"Job name (required for create, optional for update)"},"prompt":{"type":"string","description":"Prompt to execute (for type=prompt, required for create)"},"command":{"type":"string","description":"Shell command to execute (for type=command, required for create)"},"schedule":{"type":"string","description":"5-field cron expression (e.g. '*/5 * * * *'). Required for type=command create."},"schedule_type":{"type":"string","enum":["cron","interval","once"],"description":"Schedule type (for type=prompt, required for create)"},"schedule_expr":{"type":"string","description":"Cron expression or interval (for type=prompt, required for create)"},"timezone":{"type":"string","description":"Timezone (default: local)"},"max_retries":{"type":"integer","description":"Max retry attempts for transient failures (default: 3)"},"timeout_ms":{"type":"integer","description":"Timeout in milliseconds (default: 300000)"},"delivery_channel":{"type":"string","description":"Channel to deliver results to (telegram, slack, discord). Use 'origin' when scheduling from a chat message to reply back in the same channel/thread."},"delivery_target":{"type":"string","description":"Target chat/channel ID for delivery. Omit when delivery_channel='origin'."},"limit":{"type":"integer","description":"Number of runs to return (for runs action, default: 5)"}},"required":["action"]})));

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
        assert!(names.contains(&"request_user_input"));
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
}
