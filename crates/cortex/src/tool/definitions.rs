use crate::config::Config;
use crate::tool::names as tool_names;
use crate::types::ToolDefinition;

/// Per-tool one-line summaries used in the system prompt's `<tooling>` section.
///
/// **Single source of truth.** The system prompt previously hand-maintained a
/// parallel table; keeping the summaries co-located with the definitions
/// keeps the prose for "what this tool does" in one place. Multi-agent tool
/// summaries live in [`crate::multi_agent::tools::MULTI_AGENT_TOOL_SUMMARIES`]
/// alongside their definitions; `agent::system_prompt` chains the two
/// sources. WASM plugins are dynamic and have no static summary — the
/// system prompt falls back to the first sentence of the tool's description
/// for any name not present in either table.
///
/// Adding a new core tool requires a new entry here. The
/// `tool_summaries_cover_all_core_tool_definitions` guard test in this
/// module fails if a tool definition is added without a matching summary,
/// so drift can't land silently.
pub const TOOL_SUMMARIES: &[(&str, &str)] = &[
    (tool_names::WRITE_MEMORY, "Write/append long-term memory entries"),
    (tool_names::READ_MEMORY, "Read a long-term memory entry"),
    (
        tool_names::MEMORY_SEARCH,
        "Search long-term memory, knowledge files, and sessions",
    ),
    (tool_names::LIST, "List resources (skills, channels, agents)"),
    (
        tool_names::SEARCH_TOOLS,
        "Search external (MCP) tools by keyword and get their specs on demand",
    ),
    (
        tool_names::APPLY_PATCH,
        "Create/update/delete files via patch DSL",
    ),
    (
        tool_names::RUN_SHELL,
        "Execute shell commands (full system access, not sandboxed)",
    ),
    (
        tool_names::READ_FILE,
        "Read file contents with line numbers, images, PDFs",
    ),
    (tool_names::LIST_DIR, "List directory contents"),
    (tool_names::WEB_FETCH, "Fetch URL content"),
    (tool_names::WEB_SEARCH, "Search the web"),
    (tool_names::BROWSER, "Control headless Chrome browser (web pages only, not the desktop)"),
    (
        tool_names::SCHEDULE,
        "Manage scheduled jobs: prompt tasks and cron commands",
    ),
    (
        tool_names::REQUEST_USER_INPUT,
        "Last-resort prompt for information no other tool can obtain (personal preference, credential, external decision). Do not use for questions answerable by reading files or running commands.",
    ),
    (
        tool_names::GENERATE_IMAGE,
        "Generate images from text descriptions",
    ),
    (tool_names::TEXT_TO_SPEECH, "Convert text to speech audio"),
    (
        tool_names::TRANSCRIBE_AUDIO,
        "Transcribe a local audio file to text",
    ),
    (tool_names::PHONE_CALL, "Place and follow outbound phone calls"),
    (
        tool_names::VISION_ANALYZE,
        "Describe or answer questions about a remote image",
    ),
    (
        tool_names::VIDEO_ANALYZE,
        "Describe or answer questions about a video (AI Studio Gemini only)",
    ),
    (
        tool_names::COMPUTER_USE,
        "Host macOS desktop: screenshot, click, type, scroll in native apps (not web pages)",
    ),
    (
        tool_names::GET_GOAL,
        "Read this session's active goal (status, budget, usage)",
    ),
    (
        tool_names::CREATE_GOAL,
        "Create a session goal (only when the user explicitly asks)",
    ),
    (
        tool_names::UPDATE_GOAL,
        "Mark this session's goal complete",
    ),
];

/// Build the core tool definitions sent to the LLM.
///
/// IMPORTANT: Be conservative adding new tools here. Every tool's JSON schema is
/// included in the system prompt on every turn, directly consuming context tokens.
/// Prefer adding actions/parameters to an existing tool over creating a new one.
/// If a new tool is truly needed, ensure it cannot be achieved via `run_shell` or
/// an existing tool with an extra action variant.
pub async fn core_tool_definitions(config: &Config) -> Vec<ToolDefinition> {
    let mut defs = vec![
        ToolDefinition::new(tool_names::WRITE_MEMORY, "Write/append/patch/remove long-term memory. SINGLE op: give 'filename'+'content' (overwrites; set 'append' to add, or 'old_text' to replace exactly one matching span and keep the rest). BATCH: give 'operations' to apply several add/replace/remove changes in ONE atomic call (e.g. free space by removing/replacing AND add a new entry) instead of spreading them over multiple turns — if any op is invalid the whole batch is rejected and nothing is written. A successful batch is final; do not repeat it. Use scope='global' or a scoped key such as 'project:abc' when isolation matters; a top-level scope applies to every op.", serde_json::json!({"type":"object","properties":{"filename":{"type":"string","description":"Memory entry name (single-op mode). A trailing .md is stripped for backward compatibility."},"content":{"type":"string","description":"Content to write (single-op mode)."},"old_text":{"type":"string","description":"Exact current text that 'content' replaces; must match exactly once (single-op mode)."},"append":{"type":"boolean","description":"Append instead of overwriting (single-op mode).","default":false},"scope":{"type":"string","description":"Memory scope. Defaults to 'global'; may also be values like 'project:abc' or another configured scoped-memory key.","default":"global"},"operations":{"type":"array","description":"Batch mode: a list of memory changes applied atomically against the final per-entry token budget. When present, the single-op fields above are ignored.","items":{"type":"object","properties":{"action":{"type":"string","enum":["add","replace","remove"],"description":"add appends (creates if missing); replace overwrites; remove deletes. With old_text either verb patches one span."},"filename":{"type":"string","description":"Memory entry name."},"content":{"type":"string","description":"Content for add/replace (omit for remove)."},"old_text":{"type":"string","description":"Exact current text that 'content' replaces; must match exactly once."},"append":{"type":"boolean","description":"Append instead of overwrite for add/replace.","default":false},"scope":{"type":"string","description":"Memory scope; inherits the top-level scope (default 'global'). Must not conflict with it."}},"required":["action","filename"]}}}})),
        ToolDefinition::new(tool_names::READ_MEMORY, "Read a long-term memory entry by name and optional scope.", serde_json::json!({"type":"object","properties":{"filename":{"type":"string","description":"Memory entry name to read"},"scope":{"type":"string","description":"Memory scope. Defaults to 'global'.","default":"global"}},"required":["filename"]})),
        ToolDefinition::new(tool_names::MEMORY_SEARCH, "Search memory context across long-term memory, markdown knowledge files, or past session transcripts with sources=[\"messages\"]. Use before answering questions about prior work, decisions, preferences, or anything previously discussed.", serde_json::json!({"type":"object","properties":{"query":{"type":"string","description":"Search query"},"max_results":{"type":"integer","description":"Maximum results to return (default: 5)","default":5},"min_score":{"type":"number","description":"Minimum relevance score 0-1 (default: 0.2)","default":0.2},"sources":{"type":"array","items":{"type":"string","enum":["memory","messages"]},"description":"Where to search. \"memory\" (default) searches long-term memory plus markdown knowledge files. \"messages\" searches raw past conversation transcripts. Combine to search both."}},"required":["query"]})),
        ToolDefinition::new(tool_names::LIST, "List resources. Specify what to list: skills, channels, or agents.", serde_json::json!({"type":"object","properties":{"what":{"type":"string","enum":["skills","channels","agents"],"description":"What to list"}},"required":["what"]})),
        ToolDefinition::new(tool_names::APPLY_PATCH, "Create, update, delete, or rename files using the patch DSL. File paths can be relative to cwd, absolute, or use ~/. Use target to choose location: cwd (default), skills (~/.borg/skills/), channels (~/.borg/channels/). To rename or move a file, add a `*** Move to: <new_path>` line immediately after `*** Update File: <old_path>`; hunks are optional, so a move with no hunks is a pure rename. When the same context lines could match multiple places in a file, anchor the hunk by writing `@@ method_name` (or `@@ class Foo`, etc.) on the line before `@@` to disambiguate; multiple `@@` anchors can be stacked to navigate nested scopes.", serde_json::json!({"type":"object","properties":{"patch":{"type":"string","description":"The patch content in the patch DSL format"},"target":{"type":"string","enum":["cwd","skills","channels"],"description":"Where to apply the patch (default: cwd)","default":"cwd"}},"required":["patch"]})),
        ToolDefinition::new(tool_names::RUN_SHELL, "Execute a shell command. Do not use `sleep`, `at`, or background loops to emulate reminders or delayed tasks — use the `schedule` tool instead. Pass `tty=true` for commands that need a pseudo-terminal (REPLs, programs that detect `isatty`, paginating tools); `workdir` overrides cwd; `timeout_ms` overrides the default per-call timeout. Prefer `rg`/`rg --files` over `grep`/`find` for search, and set `workdir` rather than `cd`-ing inside the command. Never pipe builds/tests through `tee`/`tail`/`head` nor append `|| true`: output is already truncated, and both make exit_code report the consumer, hiding failures. For long-running or interactive programs, set action='start' to launch a persistent session: it returns a session_id you then drive with action='write_stdin' (send `stdin`), action='poll' (read new output since the last poll), and action='kill' (stop). Omit action (or use 'run', the default) for a single self-contained command.", serde_json::json!({"type":"object","properties":{"command":{"type":"string","description":"Shell command to execute. Required for action 'run' and 'start'."},"action":{"type":"string","enum":["run","start","write_stdin","poll","kill"],"description":"'run' (default): execute to completion. 'start': launch a persistent session and return a session_id. 'write_stdin'/'poll'/'kill': operate on an existing session_id.","default":"run"},"session_id":{"type":"string","description":"Session id returned by action='start'. Required for write_stdin/poll/kill."},"stdin":{"type":"string","description":"Text to write to the session's stdin (action='write_stdin'). Include a trailing newline to submit a line."},"tty":{"type":"boolean","description":"Allocate a pseudo-terminal so the command sees stdout as a TTY (default: false)."},"workdir":{"type":"string","description":"Working directory for the command (default: process cwd)."},"timeout_ms":{"type":"integer","description":"Override the per-call timeout in milliseconds."}},"required":[]})),
        ToolDefinition::new(tool_names::READ_FILE, "Read a file's contents. Returns raw file text for code and data files (including CSV), renders images visually, and auto-converts documents to Markdown — PDF, Word (.doc/.docx), PowerPoint (.ppt/.pptx), Excel (.xls/.xlsx), OpenDocument (.odt/.ods/.odp), RTF and EPUB. Conversion happens locally. A converted document is a read-only view: the file on disk stays binary and cannot be edited with apply_patch. Use offset/limit to read specific line ranges of large files.", serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"File path (relative to cwd or absolute)"},"offset":{"type":"integer","description":"Start line, 1-based (default: 1)"},"limit":{"type":"integer","description":"Max lines to read (default: all, truncated at max_chars)"},"max_chars":{"type":"integer","description":"Max characters to return (default: 50000)"}},"required":["path"]})),
        ToolDefinition::new(tool_names::LIST_DIR, "List the contents of a directory. Returns file and subdirectory names with types and sizes. Pass `fuzzy_match` to instead get a ranked, flat list of files whose paths fuzzy-match the query (e.g. \"parser\", \"agent loop\") — useful for locating files by approximate name without invoking shell `find`/`rg`.", serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Directory path (relative to cwd or absolute). Defaults to current directory."},"depth":{"type":"integer","description":"Maximum depth to recurse (default: 1, max: 3). Ignored when fuzzy_match is set (walk uses an internal noisy-dir filter)."},"include_hidden":{"type":"boolean","description":"Include hidden files/dirs (default: false)"},"fuzzy_match":{"type":"string","description":"Optional fuzzy query. When provided, returns the top-ranked file paths under `path` instead of the directory tree."},"max_results":{"type":"integer","description":"Cap on fuzzy-match results (default: 50, max: 500)."},"offset":{"type":"integer","description":"0-based line offset into the rendered listing for pagination."},"limit":{"type":"integer","description":"Maximum number of lines to return after offset (max: 500). Output will include a footer reporting the window."}}})),
    ];

    if config.web.enabled {
        defs.push(ToolDefinition::new(tool_names::WEB_FETCH, "Fetch a URL and return its text content. HTML pages are automatically converted to plain text.", serde_json::json!({"type":"object","properties":{"url":{"type":"string","description":"The URL to fetch"},"max_chars":{"type":"integer","description":"Maximum characters to return (default: 50000)","default":50000}},"required":["url"]})));
        defs.push(ToolDefinition::new(tool_names::WEB_SEARCH, "Search the web and return results with titles, URLs, and snippets.", serde_json::json!({"type":"object","properties":{"query":{"type":"string","description":"The search query"},"mode":{"type":"string","enum":["auto","live","cached"],"description":"Cache strategy: auto (cache hit if fresh else live + write), live (always hit network + refresh cache), cached (cache lookup only). Default: auto"}},"required":["query"]})));
    }

    defs.push(build_schedule_tool_def());

    defs.extend(crate::wasm_plugins::tool_definitions().await);
    // MCP tools: tiered disclosure. Small catalogs are advertised directly
    // (eager, unchanged behaviour); a catalog whose schemas outgrow the
    // listing budget is held behind the `search_tools` meta-tool, carrying a
    // budget-scaled catalog listing that degrades to names-only and then to
    // the bare meta-tool. Only the MCP surface is gated — native and
    // WASM-plugin tools above are always advertised. Dispatch is
    // registry-keyed, so a deferred tool stays callable once discovered.
    {
        use crate::config::mcp::McpDisclosureTier;
        let by_server = crate::mcp_clients::tool_definitions_by_server();
        let mcp_defs: Vec<ToolDefinition> = by_server
            .iter()
            .flat_map(|(_, defs)| defs.iter().cloned())
            .collect();
        let schema_tokens = estimate_defs_tokens(&mcp_defs);
        // The tier decision keeps its existing semantics (the budget knob is
        // `listing_max_tokens`); the fullest and most-degraded per-server
        // renderings stand in for the old flat listing/names estimates.
        let fullest = render_deferred_mcp_listing(&by_server, usize::MAX);
        let most_degraded = render_deferred_mcp_listing(&by_server, 0);
        let tier = config.mcp.disclosure_tier(
            schema_tokens,
            crate::memory::tokenizer::estimate_tokens(&fullest),
            crate::memory::tokenizer::estimate_tokens(&most_degraded),
        );
        if tier == McpDisclosureTier::Eager {
            defs.extend(mcp_defs);
        } else {
            // Deferred: per-server, largest-first degradation against the
            // listing budget. Even when nothing fits, every server keeps a
            // one-line summary so the model knows the tools exist and
            // searches instead of substituting a generic tool.
            let listing = render_deferred_mcp_listing(&by_server, config.mcp.listing_max_tokens);
            let listing = if listing.is_empty() {
                None
            } else {
                Some(listing)
            };
            defs.push(build_search_tools_def(listing.as_deref()));
        }
    }

    if config.browser.enabled {
        defs.push(ToolDefinition::new(
            tool_names::BROWSER,
            "Control a headless Chrome browser for web pages only — it cannot see or touch the host desktop or native apps (use `computer_use` for those). Actions: navigate, click, type, screenshot, snapshot, get_text, get_images, evaluate_js, hover, select, press, drag, fill, upload, scroll, back, forward, wait, resize, new_tab, list_tabs, switch_tab, close_tab, get_console_logs, close. Use 'snapshot' first to enumerate interactive elements as `@e<N>` refs, then pass `ref: \"@e5\"` to click/type instead of inventing a CSS selector.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["navigate", "click", "type", "screenshot", "snapshot", "get_text", "get_images", "evaluate_js", "hover", "select", "press", "drag", "fill", "upload", "scroll", "back", "forward", "wait", "resize", "new_tab", "list_tabs", "switch_tab", "close_tab", "get_console_logs", "close"],
                        "description": "Browser action to perform"
                    },
                    "url": { "type": "string", "description": "URL (for navigate, new_tab)" },
                    "selector": { "type": "string", "description": "CSS selector (for click, type, hover, select, get_text, screenshot, upload)" },
                    "ref": { "type": "string", "description": "Snapshot ref like '@e5' (for click, type) — pairs with a prior 'snapshot' call. Use instead of 'selector' when the model can't reliably write CSS." },
                    "text": { "type": "string", "description": "Text to type (for type action)" },
                    "expression": { "type": "string", "description": "JavaScript expression (for evaluate_js)" },
                    "value": { "type": "string", "description": "Value to select or wait for (for select, wait)" },
                    "key": { "type": "string", "description": "Key name to press (for press, e.g. 'Enter', 'Tab')" },
                    "source": { "type": "string", "description": "Source CSS selector (for drag)" },
                    "target": { "type": "string", "description": "Target CSS selector (for drag)" },
                    "fields": { "type": "object", "description": "Map of CSS selector to value (for fill)" },
                    "files": { "type": "array", "items": {"type": "string"}, "description": "Absolute file paths to upload to an <input type=\"file\"> (for upload)" },
                    "condition": { "type": "string", "enum": ["text", "element", "url", "load", "js"], "description": "Wait condition type (for wait)" },
                    "width": { "type": "integer", "description": "Viewport width (for resize)" },
                    "height": { "type": "integer", "description": "Viewport height (for resize)" },
                    "tab_index": { "type": "integer", "description": "Tab index (for switch_tab)" },
                    "x": { "type": "integer", "description": "Wheel x-coordinate (for scroll). Defaults to 0." },
                    "y": { "type": "integer", "description": "Wheel y-coordinate (for scroll). Defaults to 0." },
                    "delta_x": { "type": "integer", "description": "Horizontal scroll delta in CSS pixels (for scroll)." },
                    "delta_y": { "type": "integer", "description": "Vertical scroll delta in CSS pixels (for scroll). Positive scrolls down." },
                    "max_elements": { "type": "integer", "description": "Cap on elements returned by snapshot (default: browser.snapshot_max_elements)." }
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

    if config.telephony.enabled() {
        defs.push(ToolDefinition::new(
            tool_names::PHONE_CALL,
            "Place an outbound phone call and follow it. `call` dials `to` (E.164, e.g. +14155550123): plain-call providers speak `message` verbatim; voice-agent providers hold a conversation with `message` as their task. Calls cost money and reach real people — only call numbers the user asked for. `status` returns progress, transcript and summary; `hang_up` ends a live call.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["call", "status", "hang_up"] },
                    "to": { "type": "string", "description": "Callee in E.164 (for call)" },
                    "message": { "type": "string", "description": "What to say, or the voice agent's task (for call)" },
                    "call_id": { "type": "string", "description": "Id returned by call (for status, hang_up)" }
                },
                "required": ["action"]
            }),
        ));
    }

    if config.audio.enabled {
        defs.push(ToolDefinition::new(
            tool_names::TRANSCRIBE_AUDIO,
            "Transcribe a local audio file using the configured STT provider chain. Respects workspace boundaries, blocked paths, and audio size limits.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Local audio file path (relative to cwd, absolute, or ~)"
                    },
                    "mime_type": {
                        "type": "string",
                        "description": "Optional MIME type override, e.g. audio/wav"
                    },
                    "language": {
                        "type": "string",
                        "description": "Optional language hint, e.g. en"
                    }
                },
                "required": ["path"]
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

        defs.push(ToolDefinition::new(
            tool_names::VIDEO_ANALYZE,
            "Fetch a video from an http(s) URL or data: URI and forward it inline to a Gemini model for description. Supports mp4, quicktime, webm, mkv. Requires `[llm.aux]` (or `[llm]`) configured for AI Studio Gemini (`provider = \"gemini\"`); fails fast on other providers, including Vertex-Gemini.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "video_url": {
                        "type": "string",
                        "description": "https URL or data:video/... base64 URI"
                    },
                    "question": {
                        "type": "string",
                        "description": "Optional question to focus the analysis"
                    }
                },
                "required": ["video_url"]
            }),
        ));
    }

    if config.computer_use.enabled {
        defs.push(ToolDefinition::new(
            tool_names::COMPUTER_USE,
            "Drive the host macOS desktop: native apps, windows, menus, dialogs — anything on the user's screen that is not a web page. Use this (never shell `screencapture`/`osascript`, never `browser`) when the user asks to see, screenshot, click, or type into an app on their Mac. The driver is window-oriented and works in the background — it never raises a window to the foreground. Readonly: `capture` grabs one window — pass `app` (an app-name substring, e.g. \"Finder\") to auto-resolve the frontmost matching window, or omit it to capture the frontmost window overall, or pass an explicit `window_id`; capture reports the resolved `pid`+`window_id` so follow-up actions can target it. default `mode: \"som\"` returns a screenshot with a numbered overlay (then target with `element_index`), `mode: \"vision\"` is plain pixels, `mode: \"ax\"` returns the text-only accessibility tree. `focus_app` selects an app by `app` name and reports its `pid`+`window_id` for background driving (it does not raise the window — the driver never steals focus). `list_apps` returns running applications with pids; `list_windows` returns windows with window_ids; `wait` blocks for duration_ms. Mutating (all need `pid`): `click`/`double_click`/`right_click`/`middle_click` at (x,y) OR by `element_index`+`window_id` from a prior SOM capture — pass `modifiers` (e.g. [\"cmd\"], [\"shift\"]) to hold keys during a pixel-addressed click; `drag` from (x,y) to (to_x,to_y), also honouring `modifiers` (option-drag duplicates, shift-drag constrains); `scroll` by (delta_x,delta_y) intent (translated to a cardinal direction + keystroke count; optionally focus an `element_index`+`window_id`); `type` literal text; `key` a single key via `key` or a hotkey combo via `keys`; `set_value` writes directly to a focused AX text field (requires `window_id`+`element_index`; preferred for forms — bypasses typing entirely). Pass `capture_after: true` on a mutating action to receive a follow-up screenshot in the same envelope. Denylist refuses logout/lock/force-quit hotkeys and catastrophic typed commands before touching the driver.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": [
                            "capture", "list_apps", "list_windows", "wait", "focus_app",
                            "click", "double_click", "right_click", "middle_click",
                            "drag", "scroll", "type", "key", "set_value"
                        ],
                        "description": "Which action to take."
                    },
                    "pid": {"type": "integer", "description": "Target process id (from list_apps/list_windows, or from a prior capture/focus_app resolved-target note). Required for every mutating action; for capture som/ax it is auto-resolved when you use `app`/no id. Optional filter for list_windows."},
                    "window_id": {"type": "integer", "description": "Target window id. Optional for capture (auto-resolved via `app`/frontmost when omitted); required for set_value and any element_index-addressed action."},
                    "app": {"type": "string", "description": "capture + focus_app. App-name substring (case-insensitive, e.g. \"Finder\", \"chrome\") to resolve the frontmost matching window. For capture, used when `window_id` is omitted. Pass \"screen\" (or \"desktop\") to target the OS desktop/shell surface (wallpaper, Dock) instead of an app window."},
                    "duration_ms": {"type": "integer", "description": "Wait only. Milliseconds to sleep (capped at 300000)."},
                    "x": {"type": "integer"},
                    "y": {"type": "integer"},
                    "to_x": {"type": "integer", "description": "Drag only."},
                    "to_y": {"type": "integer", "description": "Drag only."},
                    "delta_x": {"type": "integer", "description": "Scroll only. Horizontal intent: negative scrolls left, positive right. Magnitude becomes a 1–50 keystroke count; the dominant axis wins."},
                    "delta_y": {"type": "integer", "description": "Scroll only. Vertical intent: negative scrolls up, positive down. Magnitude becomes a 1–50 keystroke count; ties with delta_x resolve vertical."},
                    "modifiers": {"type": "array", "items": {"type": "string"}, "description": "Click family + drag, pixel-addressed only. Modifier keys held during the gesture: cmd, shift, option, ctrl. Ignored on the element_index path and on scroll."},
                    "text": {"type": "string", "description": "Type only."},
                    "key": {"type": "string", "description": "Key only. A single key name, e.g. 'Return' or 'a'."},
                    "keys": {"type": "array", "items": {"type": "string"}, "description": "Key only. A hotkey combo pressed together, e.g. [\"cmd\", \"c\"]."},
                    "element_index": {"type": "integer", "description": "Set_value + click family + type + scroll. The index a SOM-mode capture assigned the element; pair with window_id."},
                    "value": {"type": "string", "description": "Set_value only."},
                    "mode": {"type": "string", "enum": ["som", "vision", "ax"], "description": "Capture only. `som` = PNG + numbered overlay (default; target follow-ups with element_index); `vision` = plain PNG inline; `ax` = text-only accessibility tree, no image bytes."},
                    "capture_after": {"type": "boolean", "description": "Mutating actions only. When true, after the action the handler appends a fresh capture to the multimodal envelope so the model can verify state without a second tool call."}
                },
                "required": ["action"]
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
        "Mark the existing goal as achieved (\"complete\") or report a repeated impasse (\"blocked\"). Pause, resume, budget edits, and objective replacement are user-controlled and not reachable through this tool. Do not mark a goal complete merely because its budget is nearly exhausted or because you are stopping work. Use \"blocked\" only after at least three attempts past the same wall (denied permission, missing external resource, persistent tool failure) — it stops auto-continuation until the user resumes, so reserve it for true impasses. When marking a budgeted goal complete, report the final token usage from the tool result to the user.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "status": {"type": "string", "enum": ["complete", "blocked"], "description": "Required. Use \"complete\" only when every requirement is verified; use \"blocked\" only after ≥3 failed attempts at the same impasse."}
            },
            "required": ["status"],
            "additionalProperties": false
        }),
    ));
    }

    defs
}

/// Estimated token cost of advertising one tool definition (name +
/// description + serialized schema). Single owner for this formula so the
/// disclosure-tier activation numbers and the `/context` breakdown can't
/// drift apart.
pub(crate) fn estimate_def_tokens(def: &ToolDefinition) -> usize {
    let schema = serde_json::to_string(&def.function.parameters).unwrap_or_default();
    crate::memory::tokenizer::estimate_tokens(&def.function.name)
        + crate::memory::tokenizer::estimate_tokens(&def.function.description)
        + crate::memory::tokenizer::estimate_tokens(&schema)
}

/// Estimated token cost of advertising `defs` eagerly (serialized schemas).
fn estimate_defs_tokens(defs: &[ToolDefinition]) -> usize {
    defs.iter().map(estimate_def_tokens).sum()
}

/// Nudge appended to the deferred catalog whenever at least one server's
/// listing is collapsed to a summary line, so the model searches for the
/// hidden tools instead of substituting a generic tool.
const MCP_COLLAPSED_NUDGE: &str = "Servers marked \"discover via tool search\" have tools not \
     listed here — call search_tools FIRST to find them before falling back to a generic tool.";

/// Render the deferred-mode MCP catalog listing with per-server, largest-first
/// degradation.
///
/// Each server starts at its full form (a name-sorted `name — one-line
/// description` block). While the rendered text exceeds `budget_tokens`,
/// servers collapse one at a time — largest first — to a one-line summary
/// (`<server> (<N> tools — discover via tool search)`), so one oversized
/// server no longer drags every co-attached server down to a terse form.
/// Even when every server is collapsed the summary lines all remain, plus a
/// nudge to use the tool-search meta-tool — the floor is never "nothing".
///
/// Byte-stable for a given catalog: servers are ordered by
/// `(full-form size desc, name asc)` — never map iteration order — and the
/// degradation walk is deterministic, so the rendered text (and with it the
/// prompt-cache prefix) is identical across turns.
fn render_deferred_mcp_listing(
    servers: &[(String, Vec<ToolDefinition>)],
    budget_tokens: usize,
) -> String {
    if servers.is_empty() {
        return String::new();
    }
    struct Entry<'a> {
        name: &'a str,
        full_tokens: usize,
        full: String,
        summary: String,
    }
    let mut entries: Vec<Entry<'_>> = servers
        .iter()
        .map(|(name, defs)| {
            let mut rows: Vec<(&str, &str)> = defs
                .iter()
                .map(|d| {
                    let first_line = d.function.description.lines().next().unwrap_or_default();
                    (d.function.name.as_str(), first_line)
                })
                .collect();
            rows.sort_unstable_by(|a, b| a.0.cmp(b.0));
            let full = std::iter::once(format!("{name}:"))
                .chain(rows.iter().map(|(n, desc)| format!("  {n} — {desc}")))
                .collect::<Vec<_>>()
                .join("\n");
            Entry {
                name,
                full_tokens: crate::memory::tokenizer::estimate_tokens(&full),
                full,
                summary: format!("{name} ({} tools — discover via tool search)", defs.len()),
            }
        })
        .collect();
    // Largest first, name as the deterministic tie-break. This is both the
    // degradation order and the render order.
    entries.sort_by(|a, b| {
        b.full_tokens
            .cmp(&a.full_tokens)
            .then_with(|| a.name.cmp(b.name))
    });

    let render = |collapsed: usize| -> String {
        let mut blocks: Vec<&str> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                if i < collapsed {
                    e.summary.as_str()
                } else {
                    e.full.as_str()
                }
            })
            .collect();
        if collapsed > 0 {
            blocks.push(MCP_COLLAPSED_NUDGE);
        }
        blocks.join("\n")
    };

    for collapsed in 0..=entries.len() {
        let text = render(collapsed);
        // The all-collapsed floor is returned even over budget: one summary
        // line per server is the minimum disclosure.
        if crate::memory::tokenizer::estimate_tokens(&text) <= budget_tokens
            || collapsed == entries.len()
        {
            return text;
        }
    }
    unreachable!("loop returns at collapsed == entries.len()")
}

/// Build the `search_tools` meta-tool definition, advertised in place of the
/// individual MCP tool schemas on the deferred disclosure tiers, optionally
/// carrying the budget-scaled catalog listing.
fn build_search_tools_def(catalog_listing: Option<&str>) -> ToolDefinition {
    // The base text is static (no dynamic counts) so the definition stays
    // byte-identical across turns for a given registry state — provider
    // prompt caches key on the tools array. The optional listing only
    // changes when the registry itself changes (server install/teardown).
    let mut description = "Search external tools provided by connected MCP servers, which are not \
         listed directly here. Call with a `query` of keywords (e.g. \"create \
         github issue\") to get matching tools and their input schemas, then call \
         the returned tool by its `name` on your next turn. Call with no arguments \
         to browse the full list of available external tools."
        .to_string();
    if let Some(listing) = catalog_listing {
        if !listing.is_empty() {
            description.push_str("\n\nAvailable external tools:\n");
            description.push_str(listing);
        }
    }
    ToolDefinition::new(
        tool_names::SEARCH_TOOLS,
        description,
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keywords matched against external tool names and descriptions. Omit to browse all available external tools."
                },
                "limit": {
                    "type": "integer",
                    // Built from the same constants that drive the default and the
                    // clamp, so the advertised contract can't drift from behavior.
                    "description": format!(
                        "Maximum number of matching tools to return (default: {}, max: {}).",
                        crate::tool::search_tools::DEFAULT_LIMIT,
                        crate::mcp_clients::TOOL_SEARCH_LIMIT_MAX,
                    )
                }
            }
        }),
    )
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
        "prompt": {"type":"string","description":"Prompt to execute (for type=prompt, required for create). It runs as one completion with NO tools: anything it must read, run, or fetch goes in pre_run_command. The model may return '[SILENT]' alone on its first or last line to suppress channel delivery for that run."},
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
        "pre_run_command": {"type":"string","description":"Optional shell command (only for type=prompt) that runs before the LLM. Its stdout is prepended to the prompt as a <preprocessed_context source=\"pre_run\"> XML block. On non-zero exit or timeout the LLM still runs and the block is marked status=\"failed\". Use this to do mechanical fetching/parsing in shell so the model only has to reason. Wake gate: if the command exits 0 and its LAST stdout line is exactly {\"wakeAgent\": false}, the LLM turn and delivery are skipped entirely for that run (recorded as completed, result 'skipped by wake gate'). A last line of {\"wakeAgent\": true} (extra keys allowed) proceeds normally with the gate line stripped from the injected context. Use the gate for polling tasks so runs cost nothing when nothing changed."},
        "pre_run_cwd": {"type":"string","description":"Optional working directory for pre_run_command (default: $HOME)."},
        "pre_run_timeout_ms": {"type":"integer","description":"Timeout (ms) for pre_run_command (default: 60000)."},
        "continuity": {"type":"boolean","description":"Only for type=prompt: let each run see what the previous runs of this same task produced, so it can say what changed instead of repeating itself. Off by default — carrying prior output costs context on every fire, and most scheduled tasks are stateless (\"summarize today's mail\"). Use it for tasks that track something over time (\"what moved on my open PRs since yesterday\"). Settable on create and update."},
        "monitor_mode": {"type":"boolean","description":"Only for type=prompt with a pre_run_command: hash the pre-run output each run and skip the LLM turn entirely when it is unchanged from the last run that reached the model (recorded as a no_change run). The first run always reaches the model and stores the baseline; a pre-run failure never counts as a change. Use for watch-for-changes jobs so unchanged polls cost nothing."},
        "paused": {"type":"boolean","description":"For create only: insert the task already paused (default false). It never fires until action='resume'. Use when the user wants a task set up now but switched on later — one atomic write, no window where it could fire in between."},
        "failure_delivery_channel": {"type":"string","description":"Where the '[FAILED]' notice for this task goes, for create and update. Omit to send failures to delivery_channel/delivery_target (the default). Set to a channel (telegram, slack, discord, or 'origin') to route failure notices there instead — e.g. an ops channel while results go to the user. Set to the literal 'none' to suppress failure notices entirely; the failure is still recorded in last_error and the run history. On update, pass '' (empty string) to clear an override."},
        "failure_delivery_target": {"type":"string","description":"Target chat/channel ID within failure_delivery_channel. Omit to reuse delivery_target (handy when the failure channel is the same channel) or when failure_delivery_channel is 'origin' or 'none'."}
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

    #[tokio::test]
    async fn core_tools_include_essential_tools() {
        let config = Config::default();
        let defs = core_tool_definitions(&config).await;
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

    /// A scheduled task's model pin is user-owned: it exists so the operator
    /// can keep the scheduled fleet on a cheap model independent of the chat
    /// model. Exposing it in the agent-facing schema would let a model
    /// re-point its own recurring work — silently changing what the fleet
    /// costs and how it behaves, on a surface nobody is watching in real time.
    #[test]
    fn schedule_tool_cannot_set_the_model_pin() {
        let def = build_schedule_tool_def();
        let properties = def.function.parameters["properties"]
            .as_object()
            .expect("schedule tool exposes an object schema");
        for forbidden in ["model", "model_provider", "provider", "base_url"] {
            assert!(
                !properties.contains_key(forbidden),
                "`{forbidden}` must stay out of the agent-facing schedule schema — \
                 the pin is set by the user via settings/CLI"
            );
        }
    }

    /// Guard against drift between the system-prompt summary table and the
    /// actual tool definitions. Adding a new core tool definition without a
    /// matching `TOOL_SUMMARIES` entry would silently produce a `- name:`
    /// line in the prompt with no human-readable hint; missing the line
    /// would force the LLM to fall back to the verbose schema description.
    ///
    /// `goals.enabled` is off by default — flip every gate to ensure every
    /// optional tool that ships a definition also ships a summary.
    #[tokio::test]
    async fn tool_summaries_cover_all_core_tool_definitions() {
        let mut config = Config::default();
        config.browser.enabled = true;
        config.web.enabled = true;
        config.tts.enabled = true;
        config.audio.enabled = true;
        config.telephony.provider = "twilio".into();
        config.image_gen.enabled = true;
        config.goals.enabled = true;
        // Defaults on only on macOS; its summary must still match on Linux.
        config.computer_use.enabled = true;
        // Turn on MCP tool-search deferral so `search_tools` is produced and
        // matched against its summary (both directions of the guard).
        config.mcp.enabled = true;
        config.mcp.tool_search_mode = crate::config::mcp::McpToolSearchMode::On;
        let defs = core_tool_definitions(&config).await;

        let summary_names: std::collections::HashSet<&str> =
            TOOL_SUMMARIES.iter().map(|(n, _)| *n).collect();

        for def in &defs {
            let name = def.function.name.as_str();
            assert!(
                summary_names.contains(name),
                "tool `{name}` has a definition but no entry in TOOL_SUMMARIES — \
                 add one in tool/definitions.rs"
            );
        }

        // Reverse: every summary entry must correspond to a name some
        // configuration of `core_tool_definitions` produces. A summary without
        // a definition is dead text in the system prompt.
        let all_names: std::collections::HashSet<&str> =
            defs.iter().map(|d| d.function.name.as_str()).collect();
        for (name, _) in TOOL_SUMMARIES {
            assert!(
                all_names.contains(name),
                "TOOL_SUMMARIES has `{name}` but no Config combo produces \
                 that tool definition — remove the stale summary"
            );
        }
    }

    #[tokio::test]
    async fn search_tools_meta_tool_gated_by_tool_search_flag() {
        // The gate is observable without a populated registry: when deferral is
        // active the `search_tools` meta-tool is advertised (in place of the MCP
        // surface); when inactive it must be absent and the direct MCP extend
        // path is taken instead. Guards the flag being honored in both directions.
        let mut config = Config::default();
        config.mcp.enabled = true;

        config.mcp.tool_search_mode = crate::config::mcp::McpToolSearchMode::On;
        let deferred = core_tool_definitions(&config).await;
        assert!(
            deferred
                .iter()
                .any(|d| d.function.name == tool_names::SEARCH_TOOLS),
            "search_tools must be advertised when tool_search_mode is on"
        );

        config.mcp.tool_search_mode = crate::config::mcp::McpToolSearchMode::Off;
        let direct = core_tool_definitions(&config).await;
        assert!(
            !direct
                .iter()
                .any(|d| d.function.name == tool_names::SEARCH_TOOLS),
            "search_tools must NOT be advertised when tool_search_mode is off"
        );
    }

    #[tokio::test]
    async fn search_tools_requires_mcp_enabled() {
        // Deferral with mcp disabled is a no-op — no sessions exist, so
        // deferring would hide nothing and strand the model with a search over an
        // empty set. The meta-tool must only appear when MCP is actually on.
        let mut config = Config::default();
        config.mcp.enabled = false;
        config.mcp.tool_search_mode = crate::config::mcp::McpToolSearchMode::On;
        let defs = core_tool_definitions(&config).await;
        assert!(
            !defs
                .iter()
                .any(|d| d.function.name == tool_names::SEARCH_TOOLS),
            "search_tools must not appear when mcp.enabled is false"
        );
    }

    #[tokio::test]
    async fn native_tools_present_regardless_of_deferral() {
        // Deferral only gates the MCP surface — native/compiled-in tools must be
        // advertised in both modes so the agent never loses its core toolset.
        for mode in [
            crate::config::mcp::McpToolSearchMode::On,
            crate::config::mcp::McpToolSearchMode::Off,
        ] {
            let mut config = Config::default();
            config.mcp.enabled = true;
            config.mcp.tool_search_mode = mode;
            let defs = core_tool_definitions(&config).await;
            let names: std::collections::HashSet<&str> =
                defs.iter().map(|d| d.function.name.as_str()).collect();
            for expected in [
                tool_names::WRITE_MEMORY,
                tool_names::RUN_SHELL,
                tool_names::READ_FILE,
            ] {
                assert!(
                    names.contains(expected),
                    "native tool `{expected}` must be present (tool_search_mode={mode:?})"
                );
            }
        }
    }

    #[test]
    fn search_tools_def_schema_is_static_and_valid() {
        // The advertised base description must stay static (no dynamic tool
        // count) so the tools array — and thus the prompt-cache prefix — is
        // byte-identical every turn for a given registry state. Also lock the
        // optional-args schema the model relies on.
        let def = build_search_tools_def(None);
        assert_eq!(def.function.name, tool_names::SEARCH_TOOLS);
        let props = &def.function.parameters["properties"];
        assert_eq!(props["query"]["type"], "string");
        assert_eq!(props["limit"]["type"], "integer");
        // No required fields — the tool must be callable with `{}` to browse.
        assert!(
            def.function.parameters.get("required").is_none(),
            "search_tools must have no required args"
        );
        // Steers the model to the discover→call flow. The optional catalog
        // listing appends after this base and only varies with registry
        // state, never per turn (see `search_tools_def_embeds_listing_after_
        // stable_base`).
        assert!(
            def.function.description.contains("connected MCP servers"),
            "description must explain these are external MCP tools"
        );
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
        // The LLM relies on these types being correct to emit valid args.
        assert_eq!(props["pre_run_command"]["type"], "string");
        assert_eq!(props["pre_run_cwd"]["type"], "string");
        assert_eq!(props["pre_run_timeout_ms"]["type"], "integer");
        // pre_run_command's description must signal it's prompt-only (drives routing).
        assert!(
            props["pre_run_command"]["description"]
                .as_str()
                .unwrap()
                .contains("type=prompt"),
            "pre_run_command description should restrict it to prompt tasks"
        );
    }

    /// The model can only use what the schema names: the paused-create flag
    /// and the failure route must be typed correctly and their descriptions
    /// must teach the `none` sentinel and the fallback, or the parameters
    /// exist without ever being discovered.
    #[test]
    fn schedule_tool_schema_teaches_paused_create_and_failure_routing() {
        let def = build_schedule_tool_def();
        let props = &def.function.parameters["properties"];
        assert_eq!(props["paused"]["type"], "boolean");
        assert_eq!(props["failure_delivery_channel"]["type"], "string");
        assert_eq!(props["failure_delivery_target"]["type"], "string");
        let paused = props["paused"]["description"].as_str().unwrap();
        assert!(
            paused.contains("create") && paused.contains("resume"),
            "paused must say it is a create-time flag lifted by resume: {paused}"
        );
        let route = props["failure_delivery_channel"]["description"]
            .as_str()
            .unwrap();
        assert!(
            route.contains(&format!("'{}'", crate::tasks::FAILURE_DELIVERY_NONE)),
            "must document the suppression sentinel: {route}"
        );
        assert!(
            route.contains("delivery_channel") && route.contains("default"),
            "must document the fallback to the success route: {route}"
        );
        assert!(
            route.contains("last_error"),
            "must say a suppressed notice is still recorded: {route}"
        );
    }

    #[tokio::test]
    async fn projects_tool_is_not_registered() {
        let config = Config::default();
        let defs = core_tool_definitions(&config).await;
        assert!(defs.iter().all(|d| d.function.name != "projects"));
    }

    #[tokio::test]
    async fn browser_tool_excluded_when_disabled() {
        let mut config = Config::default();
        config.browser.enabled = false;
        let defs = core_tool_definitions(&config).await;
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(!names.contains(&"browser"));
    }

    #[tokio::test]
    async fn browser_tool_included_when_enabled() {
        let mut config = Config::default();
        config.browser.enabled = true;
        let defs = core_tool_definitions(&config).await;
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(names.contains(&"browser"));
    }

    #[tokio::test]
    async fn web_tools_excluded_when_disabled() {
        let mut config = Config::default();
        config.web.enabled = false;
        let defs = core_tool_definitions(&config).await;
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(!names.contains(&"web_fetch"));
        assert!(!names.contains(&"web_search"));
    }

    #[tokio::test]
    async fn web_tools_included_when_enabled() {
        let mut config = Config::default();
        config.web.enabled = true;
        let defs = core_tool_definitions(&config).await;
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(names.contains(&"web_fetch"));
        assert!(names.contains(&"web_search"));
    }

    #[tokio::test]
    async fn tts_tool_excluded_when_disabled() {
        let mut config = Config::default();
        config.tts.enabled = false;
        let defs = core_tool_definitions(&config).await;
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(!names.contains(&"text_to_speech"));
    }

    #[tokio::test]
    async fn tts_tool_included_when_enabled() {
        let mut config = Config::default();
        config.tts.enabled = true;
        let defs = core_tool_definitions(&config).await;
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(names.contains(&"text_to_speech"));
    }

    #[tokio::test]
    async fn image_gen_tool_excluded_when_disabled() {
        let mut config = Config::default();
        config.image_gen.enabled = false;
        let defs = core_tool_definitions(&config).await;
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(!names.contains(&"generate_image"));
    }

    #[tokio::test]
    async fn image_gen_tool_included_when_enabled() {
        let mut config = Config::default();
        config.image_gen.enabled = true;
        let defs = core_tool_definitions(&config).await;
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(names.contains(&"generate_image"));
    }

    #[tokio::test]
    async fn all_tool_definitions_have_valid_schema() {
        let mut config = Config::default();
        config.browser.enabled = true;
        config.web.enabled = true;
        config.tts.enabled = true;
        config.image_gen.enabled = true;
        let defs = core_tool_definitions(&config).await;

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

    #[tokio::test]
    async fn no_duplicate_tool_names() {
        let mut config = Config::default();
        config.browser.enabled = true;
        config.web.enabled = true;
        config.tts.enabled = true;
        config.image_gen.enabled = true;
        let defs = core_tool_definitions(&config).await;
        let mut seen = std::collections::HashSet::new();
        for def in &defs {
            assert!(
                seen.insert(&def.function.name),
                "duplicate tool name: {}",
                def.function.name
            );
        }
    }

    #[tokio::test]
    async fn tool_sort_is_stable_across_config_variants() {
        // Tools with all optional features enabled
        let mut config_full = Config::default();
        config_full.browser.enabled = true;
        config_full.web.enabled = true;
        config_full.tts.enabled = true;
        config_full.image_gen.enabled = true;
        let mut defs_full = core_tool_definitions(&config_full).await;
        defs_full.sort_by(|a, b| a.function.name.cmp(&b.function.name));

        // Same config again — order must be identical
        let mut defs_full2 = core_tool_definitions(&config_full).await;
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

    #[tokio::test]
    async fn compact_schema_round_trip_valid() {
        let mut config = Config::default();
        config.browser.enabled = true;
        config.web.enabled = true;
        let defs = core_tool_definitions(&config).await;

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

    #[tokio::test]
    async fn compact_schema_reduces_token_count() {
        let config = Config::default();
        let defs = core_tool_definitions(&config).await;
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

// Active guard (the `mod tests` above is gated behind a disabled feature). The
// `*** Move to:` rename directive is fully implemented in the patch applicator,
// but the model can only reach it if the `apply_patch` tool description
// advertises it. This guards the model-facing contract — a description that
// drifts away from documenting rename would silently strand the capability.
#[cfg(test)]
mod move_to_discoverability_tests {
    const SRC: &str = include_str!("definitions.rs");

    #[test]
    fn apply_patch_description_advertises_move_to() {
        let line = SRC
            .lines()
            .find(|l| l.contains("ToolDefinition::new(tool_names::APPLY_PATCH,"))
            .expect("apply_patch definition should be present");
        assert!(
            line.contains("Move to:"),
            "apply_patch tool description must document the `*** Move to:` rename directive"
        );
    }
}

#[cfg(test)]
mod mcp_disclosure_tests {
    use super::*;

    fn def(name: &str, desc: &str) -> ToolDefinition {
        ToolDefinition::new(
            name,
            desc,
            serde_json::json!({"type":"object","properties":{"q":{"type":"string"}}}),
        )
    }

    /// Three fake servers: one huge, two small. `(server, tools)` in the
    /// name-sorted order `tool_definitions_by_server` produces.
    fn three_servers() -> Vec<(String, Vec<ToolDefinition>)> {
        let huge_tools: Vec<ToolDefinition> = (0..40)
            .map(|i| {
                def(
                    &format!("mcp_huge_tool_{i:02}"),
                    "A verbosely described operation on the oversized flat-API server.",
                )
            })
            .collect();
        vec![
            ("huge".to_string(), huge_tools),
            (
                "small_a".to_string(),
                vec![
                    def("mcp_small_a_get", "Get a record."),
                    def("mcp_small_a_list", "List records."),
                ],
            ),
            (
                "small_b".to_string(),
                vec![def("mcp_small_b_search", "Search things.")],
            ),
        ]
    }

    #[test]
    fn oversized_server_collapses_while_small_servers_keep_listings() {
        let servers = three_servers();
        // Budget fits both small servers' full blocks but not the huge one.
        let listing = render_deferred_mcp_listing(&servers, 120);
        assert!(
            listing.contains("huge (40 tools — discover via tool search)"),
            "largest server must collapse to a one-line summary:\n{listing}"
        );
        assert!(
            !listing.contains("mcp_huge_tool_00"),
            "collapsed server must not list individual tools:\n{listing}"
        );
        for kept in [
            "small_a:",
            "  mcp_small_a_get — Get a record.",
            "  mcp_small_a_list — List records.",
            "small_b:",
            "  mcp_small_b_search — Search things.",
        ] {
            assert!(
                listing.contains(kept),
                "small servers must keep full listings ({kept:?} missing):\n{listing}"
            );
        }
        assert!(
            listing.contains("search_tools FIRST"),
            "a partially collapsed catalog must carry the search-first nudge:\n{listing}"
        );
    }

    #[test]
    fn deferred_listing_is_byte_identical_across_renders() {
        // Prompt caches key on the tools array byte-for-byte; the ordering is
        // (size desc, name asc), never map iteration order.
        let a = render_deferred_mcp_listing(&three_servers(), 120);
        let b = render_deferred_mcp_listing(&three_servers(), 120);
        assert_eq!(a, b, "same catalog + budget must render byte-identically");
        // Largest-first render order: huge's summary precedes both small blocks.
        let huge_pos = a.find("huge (").expect("huge summary present");
        assert!(huge_pos < a.find("small_a:").expect("small_a present"));
        assert!(a.find("small_a:").unwrap() < a.find("small_b:").expect("small_b present"));
    }

    #[test]
    fn most_degraded_tier_still_lists_every_server_summary() {
        // Zero budget: nothing fits, but the floor is one summary line per
        // server plus the nudge — never the old empty "Bare" form where the
        // model didn't know the tools existed.
        let listing = render_deferred_mcp_listing(&three_servers(), 0);
        for line in [
            "huge (40 tools — discover via tool search)",
            "small_a (2 tools — discover via tool search)",
            "small_b (1 tools — discover via tool search)",
        ] {
            assert!(
                listing.contains(line),
                "every server keeps its summary line ({line:?} missing):\n{listing}"
            );
        }
        assert!(
            !listing.contains("mcp_small_b_search —"),
            "no per-tool rows at the floor:\n{listing}"
        );
        assert!(listing.contains("search_tools FIRST"));
    }

    #[test]
    fn generous_budget_keeps_every_server_at_full_form() {
        let listing = render_deferred_mcp_listing(&three_servers(), usize::MAX);
        assert!(
            !listing.contains("discover via tool search"),
            "no server collapses when everything fits:\n{listing}"
        );
        // Full blocks are name-sorted within each server.
        let get = listing.find("mcp_small_a_get").unwrap();
        let list = listing.find("mcp_small_a_list").unwrap();
        assert!(get < list, "tools within a server are name-sorted");
        // First line only: multi-line descriptions must not leak detail.
        let servers = vec![(
            "s".to_string(),
            vec![def(
                "mcp_s_t",
                "First line.\nExtra detail that must not leak.",
            )],
        )];
        let one = render_deferred_mcp_listing(&servers, usize::MAX);
        assert!(one.contains("mcp_s_t — First line."));
        assert!(!one.contains("Extra detail"));
    }

    #[test]
    fn empty_catalog_renders_empty() {
        assert_eq!(render_deferred_mcp_listing(&[], 100), "");
        assert_eq!(render_deferred_mcp_listing(&[], 0), "");
    }

    #[test]
    fn search_tools_def_embeds_listing_after_stable_base() {
        let bare = build_search_tools_def(None);
        let listed = build_search_tools_def(Some("mcp_a_one — First tool."));
        assert!(
            listed
                .function
                .description
                .starts_with(&bare.function.description),
            "the static base text must be a stable prefix in every tier"
        );
        assert!(
            listed
                .function
                .description
                .contains("Available external tools:")
                && listed
                    .function
                    .description
                    .contains("mcp_a_one — First tool."),
            "listed tier must carry the catalog"
        );
        assert!(
            !bare
                .function
                .description
                .contains("Available external tools:"),
            "bare tier carries no catalog block"
        );
        // An empty listing must not leave a dangling header.
        let empty = build_search_tools_def(Some(""));
        assert_eq!(empty.function.description, bare.function.description);
    }

    #[test]
    fn disclosure_tier_matrix() {
        use crate::config::mcp::{McpConfig, McpDisclosureTier, McpToolSearchMode};
        let mut cfg = McpConfig {
            enabled: true,
            listing_max_tokens: 100,
            ..McpConfig::default()
        };

        // Auto: small catalogs stay eager, oversized ones defer and degrade.
        cfg.tool_search_mode = McpToolSearchMode::Auto;
        let cases: &[(usize, usize, usize, McpDisclosureTier)] = &[
            (50, 20, 5, McpDisclosureTier::Eager),    // schemas fit → eager
            (500, 80, 20, McpDisclosureTier::Listed), // schemas big, listing fits
            (500, 300, 60, McpDisclosureTier::NamesOnly), // only names fit
            (500, 300, 200, McpDisclosureTier::Bare), // nothing fits
        ];
        for (schema, listing, names, want) in cases {
            let got = cfg.disclosure_tier(*schema, *listing, *names);
            assert_eq!(
                got, *want,
                "auto tier for schema={schema} listing={listing} names={names}"
            );
        }

        // Off: always eager, no matter the size.
        cfg.tool_search_mode = McpToolSearchMode::Off;
        assert_eq!(
            cfg.disclosure_tier(1_000_000, 300, 200),
            McpDisclosureTier::Eager
        );

        // On: always deferred, even when tiny.
        cfg.tool_search_mode = McpToolSearchMode::On;
        assert_eq!(cfg.disclosure_tier(10, 5, 2), McpDisclosureTier::Listed);

        // MCP disabled → moot, eager passthrough.
        cfg.enabled = false;
        assert_eq!(
            cfg.disclosure_tier(1_000_000, 300, 200),
            McpDisclosureTier::Eager
        );
    }
}
