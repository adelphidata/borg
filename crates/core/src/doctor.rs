use crate::config::Config;
use crate::db::Database;
use crate::provider::Provider;
use crate::skills::load_all_skills;

/// Result status for a single diagnostic check.
#[derive(Debug, Clone, PartialEq)]
pub enum CheckStatus {
    /// Check passed successfully.
    Pass,
    /// Check passed with a warning.
    Warn(String),
    /// Check failed.
    Fail(String),
}

/// A single diagnostic check result with category, name, and status.
#[derive(Debug, Clone)]
pub struct DiagnosticCheck {
    pub category: &'static str,
    pub name: String,
    pub status: CheckStatus,
}

impl DiagnosticCheck {
    /// Create a passing check.
    pub fn pass(category: &'static str, name: impl Into<String>) -> Self {
        Self {
            category,
            name: name.into(),
            status: CheckStatus::Pass,
        }
    }

    /// Create a warning check.
    pub fn warn(category: &'static str, name: impl Into<String>, msg: impl Into<String>) -> Self {
        Self {
            category,
            name: name.into(),
            status: CheckStatus::Warn(msg.into()),
        }
    }

    /// Create a failing check.
    pub fn fail(category: &'static str, name: impl Into<String>, msg: impl Into<String>) -> Self {
        Self {
            category,
            name: name.into(),
            status: CheckStatus::Fail(msg.into()),
        }
    }

    /// Create a check with an explicit status.
    pub fn with_status(
        category: &'static str,
        name: impl Into<String>,
        status: CheckStatus,
    ) -> Self {
        Self {
            category,
            name: name.into(),
            status,
        }
    }

    /// Create a check from a Result: pass on Ok, fail on Err.
    pub fn from_result<T, E: std::fmt::Display>(
        category: &'static str,
        name: impl Into<String>,
        result: Result<T, E>,
    ) -> Self {
        match result {
            Ok(_) => Self::pass(category, name),
            Err(e) => Self::fail(category, name, format!("{e}")),
        }
    }

    /// Format a single check result line with status icon.
    pub fn format_line(&self) -> String {
        match &self.status {
            CheckStatus::Pass => format!("  ✓ {}", self.name),
            CheckStatus::Warn(msg) => format!("  ⚠ {} — {msg}", self.name),
            CheckStatus::Fail(msg) => format!("  ✗ {} — {msg}", self.name),
        }
    }
}

/// Collection of diagnostic checks grouped by category.
pub struct DiagnosticReport {
    pub checks: Vec<DiagnosticCheck>,
}

impl DiagnosticReport {
    /// Format the report as a human-readable string with status icons.
    pub fn format(&self) -> String {
        let mut output = String::from("Borg Doctor\n───────────\n");
        let mut current_category = "";

        for check in &self.checks {
            if check.category != current_category {
                current_category = check.category;
                output.push_str(&format!("\n{current_category}\n"));
            }
            output.push_str(&check.format_line());
            output.push('\n');
        }

        let (pass_count, warn_count, fail_count) = self.counts();
        output.push_str(&format!(
            "\nSummary: {pass_count} passed, {warn_count} warning(s), {fail_count} failed"
        ));
        output
    }

    /// Count (pass, warn, fail) results.
    pub fn counts(&self) -> (usize, usize, usize) {
        let mut pass = 0;
        let mut warn = 0;
        let mut fail = 0;
        for check in &self.checks {
            match &check.status {
                CheckStatus::Pass => pass += 1,
                CheckStatus::Warn(_) => warn += 1,
                CheckStatus::Fail(_) => fail += 1,
            }
        }
        (pass, warn, fail)
    }
}

/// Stepped diagnostic runner that yields one check category at a time.
/// Used by the TUI to show progressive output.
#[derive(Default)]
pub struct DiagnosticRunner {
    /// Current step index.
    step: usize,
}

const STEP_COUNT: usize = 17;

impl DiagnosticRunner {
    /// Create a new diagnostic runner starting at step 0.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the label of the next step without running it, or `None` if done.
    pub fn peek_label(&self, config: &Config) -> Option<&'static str> {
        match self.step {
            0 => Some("Config"),
            1 => Some("Provider"),
            2 => Some("Secrets"),
            3 => Some("Sandbox"),
            4 => Some("Skills"),
            5 => Some("Memory"),
            6 => Some("Embeddings"),
            7 => Some("Data"),
            8 => Some("Gateway"),
            9 => Some("Heartbeat"),
            10 => Some("Budget"),
            11 => Some("Prompt Cache"),
            12 => Some("Plugins"),
            13 => Some("Browser"),
            14 => Some("Agents"),
            15 => Some("Security"),
            16 if config.security.host_audit => Some("Host Audit"),
            16 => None,
            _ => None,
        }
    }

    /// Run the next check group. Returns `None` when all groups are done.
    pub fn next_step(&mut self, config: &Config) -> Option<(&'static str, Vec<DiagnosticCheck>)> {
        if self.step >= STEP_COUNT {
            return None;
        }
        let mut checks = Vec::new();
        let label = match self.step {
            0 => {
                check_config(&mut checks);
                "Config"
            }
            1 => {
                check_provider(config, &mut checks);
                "Provider"
            }
            2 => {
                check_secrets(config, &mut checks);
                "Secrets"
            }
            3 => {
                check_sandbox(&mut checks);
                "Sandbox"
            }
            4 => {
                check_skills(config, &mut checks);
                "Skills"
            }
            5 => {
                check_memory(&mut checks);
                "Memory"
            }
            6 => {
                check_embeddings(config, &mut checks);
                "Embeddings"
            }
            7 => {
                check_data_dir(&mut checks);
                "Data"
            }
            8 => {
                check_gateway(config, &mut checks);
                "Gateway"
            }
            9 => {
                check_heartbeat(config, &mut checks);
                "Heartbeat"
            }
            10 => {
                check_budget(config, &mut checks);
                "Budget"
            }
            11 => {
                check_prompt_cache(config, &mut checks);
                "Prompt Cache"
            }
            12 => {
                check_plugins(&mut checks);
                "Plugins"
            }
            13 => {
                check_browser(config, &mut checks);
                "Browser"
            }
            14 => {
                check_agents(config, &mut checks);
                "Agents"
            }
            15 => {
                check_config_security(config, &mut checks);
                "Security"
            }
            16 => {
                if config.security.host_audit {
                    crate::host_audit::run_host_security_checks(&mut checks);
                    self.step += 1;
                    return if checks.is_empty() {
                        None
                    } else {
                        Some(("Host Audit", checks))
                    };
                } else {
                    self.step += 1;
                    return None;
                }
            }
            _ => {
                return None;
            }
        };
        self.step += 1;
        Some((label, checks))
    }
}

/// Run all diagnostic checks and return a report.
pub fn run_diagnostics(config: &Config) -> DiagnosticReport {
    let mut runner = DiagnosticRunner::new();
    let mut checks = Vec::new();
    while let Some((_label, step_checks)) = runner.next_step(config) {
        checks.extend(step_checks);
    }
    DiagnosticReport { checks }
}

fn check_config(checks: &mut Vec<DiagnosticCheck>) {
    match Config::data_dir() {
        Ok(data_dir) => {
            let config_path = data_dir.join("config.toml");
            if config_path.exists() {
                checks.push(DiagnosticCheck::pass("Config", "config.toml exists"));
                match Config::load_from_db() {
                    Ok(_) => {
                        checks.push(DiagnosticCheck::pass("Config", "config.toml valid"));
                    }
                    Err(e) => {
                        checks.push(DiagnosticCheck::fail(
                            "Config",
                            "config.toml valid",
                            format!("{e}"),
                        ));
                    }
                }
            } else {
                checks.push(DiagnosticCheck::warn(
                    "Config",
                    "config.toml exists",
                    "not found, using defaults",
                ));
            }
        }
        Err(e) => {
            checks.push(DiagnosticCheck::fail(
                "Config",
                "data directory",
                format!("{e}"),
            ));
        }
    }
}

fn check_provider(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    match config.resolve_provider() {
        Ok((provider, _key)) => {
            if provider == Provider::ClaudeCli {
                if let Some(path) = crate::claude_cli::detect_cli_path() {
                    checks.push(DiagnosticCheck::pass(
                        "Provider",
                        format!("Claude CLI found at {}", path.display()),
                    ));
                } else {
                    checks.push(DiagnosticCheck::fail(
                        "Provider",
                        "Claude CLI binary",
                        "not found — install Claude Code or set CLAUDE_CLI_PATH",
                    ));
                }
                if crate::claude_cli::has_valid_auth() {
                    checks.push(DiagnosticCheck::pass(
                        "Provider",
                        "Claude CLI authenticated",
                    ));
                } else {
                    checks.push(DiagnosticCheck::warn(
                        "Provider",
                        "Claude CLI auth",
                        "not authenticated or expired — run `claude login`",
                    ));
                }
            } else if provider.requires_api_key() {
                checks.push(DiagnosticCheck::pass("Provider", "API key set"));
            } else if Provider::ollama_available() {
                checks.push(DiagnosticCheck::pass("Provider", "Ollama server reachable"));
            } else {
                checks.push(DiagnosticCheck::warn(
                    "Provider",
                    "Ollama server",
                    format!(
                        "not reachable at localhost:{} — run `ollama serve`",
                        crate::constants::OLLAMA_PORT_DEFAULT
                    ),
                ));
            }
            checks.push(DiagnosticCheck::pass(
                "Provider",
                format!("Provider: {provider}"),
            ));
        }
        Err(e) => {
            checks.push(DiagnosticCheck::fail(
                "Provider",
                "API key set",
                format!("{e}"),
            ));
        }
    }

    // Also report Claude CLI availability even if not the active provider
    if config
        .llm
        .provider
        .as_deref()
        .is_none_or(|p| p != "claude-cli")
        && crate::claude_cli::detect_cli_path().is_some()
        && crate::claude_cli::has_valid_auth()
    {
        checks.push(DiagnosticCheck::pass(
            "Provider",
            "Claude CLI available (alternative provider)",
        ));
    }

    checks.push(DiagnosticCheck::warn(
        "Provider",
        "API connectivity",
        "skipped (use --online for live check)",
    ));
}

fn check_secrets(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    if config.llm.api_key.is_some() {
        checks.push(DiagnosticCheck::pass("Secrets", "API key via SecretRef"));
    }

    if let Ok(data_dir) = Config::data_dir() {
        let env_path = data_dir.join(".env");
        if env_path.exists() {
            match std::fs::read_to_string(&env_path) {
                Ok(contents) => {
                    let has_plaintext_keys = contents.lines().any(|line| {
                        let trimmed = line.trim();
                        !trimmed.is_empty()
                            && !trimmed.starts_with('#')
                            && trimmed.contains('=')
                            && (trimmed.contains("API_KEY") || trimmed.contains("api_key"))
                    });

                    if has_plaintext_keys {
                        let hint = if config.llm.api_key.is_some() {
                            "plaintext .env still present — consider removing it"
                        } else {
                            "plaintext API keys in .env — consider using SecretRef in config.toml (e.g., api_key = { source = \"exec\", command = \"security\", args = [\"find-generic-password\", \"-s\", \"borg\", \"-w\"] })"
                        };
                        checks.push(DiagnosticCheck::warn(
                            "Secrets",
                            "plaintext keys in .env",
                            hint,
                        ));
                    } else {
                        checks.push(DiagnosticCheck::pass(
                            "Secrets",
                            "no plaintext API keys in .env",
                        ));
                    }
                }
                Err(_) => {
                    checks.push(DiagnosticCheck::warn(
                        "Secrets",
                        ".env readable",
                        "could not read .env file",
                    ));
                }
            }

            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if let Ok(meta) = std::fs::metadata(&env_path) {
                    let mode = meta.mode() & 0o777;
                    if mode & 0o077 != 0 {
                        checks.push(DiagnosticCheck::warn(
                            "Secrets",
                            ".env file permissions",
                            format!("permissions are {mode:04o} — should be 0600 or stricter"),
                        ));
                    } else {
                        checks.push(DiagnosticCheck::pass("Secrets", ".env file permissions"));
                    }
                }
            }
        }
    }

    if !config.llm.api_keys.is_empty() {
        let resolved_count = config
            .llm
            .api_keys
            .iter()
            .filter(|sr| sr.resolve().is_ok())
            .count();
        let name = format!(
            "multi-key fallback: {resolved_count}/{} keys resolvable",
            config.llm.api_keys.len()
        );
        if resolved_count > 0 {
            checks.push(DiagnosticCheck::pass("Secrets", name));
        } else {
            checks.push(DiagnosticCheck::warn(
                "Secrets",
                name,
                "no fallback keys could be resolved",
            ));
        }
    }
}

fn check_sandbox(checks: &mut Vec<DiagnosticCheck>) {
    if cfg!(target_os = "macos") {
        if which::which("sandbox-exec").is_ok() {
            checks.push(DiagnosticCheck::pass("Sandbox", "sandbox-exec available"));
        } else {
            checks.push(DiagnosticCheck::warn(
                "Sandbox",
                "sandbox-exec available",
                "not found",
            ));
        }
    } else if cfg!(target_os = "linux") {
        if which::which("bwrap").is_ok() {
            checks.push(DiagnosticCheck::pass("Sandbox", "bwrap available"));
        } else {
            checks.push(DiagnosticCheck::warn(
                "Sandbox",
                "bwrap available",
                "not found — sandboxing disabled",
            ));
        }
    } else {
        checks.push(DiagnosticCheck::warn(
            "Sandbox",
            "sandbox support",
            "not available on this platform",
        ));
    }
}

fn check_skills(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    let resolved_creds = config.resolve_credentials();
    match load_all_skills(&resolved_creds, &config.skills) {
        Ok(skills) => {
            let available = skills.iter().filter(|s| s.available).count();
            let total = skills.len();
            checks.push(DiagnosticCheck::pass(
                "Skills",
                format!("{available}/{total} skills available"),
            ));

            let missing: Vec<String> = skills
                .iter()
                .filter(|s| !s.available)
                .map(|s| s.manifest.name.clone())
                .collect();
            if !missing.is_empty() {
                checks.push(DiagnosticCheck::warn(
                    "Skills",
                    format!("missing requirements: {}", missing.join(", ")),
                    "some skills unavailable",
                ));
            }
        }
        Err(e) => {
            checks.push(DiagnosticCheck::fail(
                "Skills",
                "skills loading",
                format!("{e}"),
            ));
        }
    }
}

fn check_memory(checks: &mut Vec<DiagnosticCheck>) {
    match Database::open() {
        Ok(db) => match db.list_memory_entries("global") {
            Ok(entries) => {
                let has_index = entries.iter().any(|e| e.name == "INDEX");
                let total = entries.len();
                if has_index {
                    checks.push(DiagnosticCheck::pass(
                        "Memory",
                        format!("{total} memory entries (INDEX present)"),
                    ));
                } else if total == 0 {
                    checks.push(DiagnosticCheck::warn(
                        "Memory",
                        "memory entries",
                        "no entries in DB yet",
                    ));
                } else {
                    checks.push(DiagnosticCheck::warn(
                        "Memory",
                        format!("{total} memory entries"),
                        "no INDEX entry",
                    ));
                }
            }
            Err(e) => {
                checks.push(DiagnosticCheck::fail(
                    "Memory",
                    "list memory entries",
                    format!("{e}"),
                ));
            }
        },
        Err(e) => {
            checks.push(DiagnosticCheck::fail(
                "Memory",
                "open database",
                format!("{e}"),
            ));
        }
    }

    match Config::identity_path() {
        Ok(path) => {
            if path.exists() {
                checks.push(DiagnosticCheck::pass("Memory", "IDENTITY.md exists"));
            } else {
                checks.push(DiagnosticCheck::warn(
                    "Memory",
                    "IDENTITY.md exists",
                    "not found",
                ));
            }
        }
        Err(e) => {
            checks.push(DiagnosticCheck::fail(
                "Memory",
                "IDENTITY.md",
                format!("{e}"),
            ));
        }
    }
}

fn check_data_dir(checks: &mut Vec<DiagnosticCheck>) {
    match Config::data_dir() {
        Ok(data_dir) => {
            if data_dir.exists() {
                let test_file = data_dir.join(".doctor_write_test");
                match std::fs::write(&test_file, "test") {
                    Ok(()) => {
                        let _ = std::fs::remove_file(&test_file);
                        checks.push(DiagnosticCheck::pass("Data", "data directory writable"));
                    }
                    Err(e) => {
                        checks.push(DiagnosticCheck::fail(
                            "Data",
                            "data directory writable",
                            format!("{e}"),
                        ));
                    }
                }
            } else {
                checks.push(DiagnosticCheck::warn(
                    "Data",
                    "data directory exists",
                    "~/.borg not found — run `borg init`",
                ));
            }
        }
        Err(e) => {
            checks.push(DiagnosticCheck::fail(
                "Data",
                "data directory",
                format!("{e}"),
            ));
        }
    }

    checks.push(DiagnosticCheck::from_result(
        "Data",
        "database accessible",
        Database::open(),
    ));
}

fn check_gateway(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    checks.push(DiagnosticCheck::pass(
        "Gateway",
        format!(
            "gateway config: {}:{}",
            config.gateway.host, config.gateway.port
        ),
    ));

    match Config::channels_dir() {
        Ok(channels_dir) => {
            if channels_dir.exists() {
                let mut count = 0;
                let mut errors = Vec::new();
                if let Ok(entries) = std::fs::read_dir(&channels_dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.is_dir() {
                            let manifest_path = path.join("channel.toml");
                            if manifest_path.exists() {
                                match std::fs::read_to_string(&manifest_path) {
                                    Ok(content) => match toml::from_str::<toml::Value>(&content) {
                                        Ok(_) => count += 1,
                                        Err(e) => {
                                            errors.push(format!("{}: {e}", manifest_path.display()))
                                        }
                                    },
                                    Err(e) => {
                                        errors.push(format!("{}: {e}", manifest_path.display()))
                                    }
                                }
                            }
                        }
                    }
                }

                checks.push(DiagnosticCheck::pass(
                    "Gateway",
                    format!("{count} channel(s) found"),
                ));

                for error in errors {
                    checks.push(DiagnosticCheck::fail("Gateway", "channel manifest", error));
                }
            } else {
                checks.push(DiagnosticCheck::warn(
                    "Gateway",
                    "channels directory",
                    "not found",
                ));
            }
        }
        Err(e) => {
            checks.push(DiagnosticCheck::fail(
                "Gateway",
                "channels directory",
                format!("{e}"),
            ));
        }
    }
}

/// Format a unix timestamp as a human-readable relative time string.
pub fn format_relative_time(timestamp: i64) -> String {
    let now = chrono::Utc::now().timestamp();
    let delta = now - timestamp;
    if delta < 0 {
        return "just now".to_string();
    }
    let secs = delta as u64;
    if secs < 60 {
        if secs == 1 {
            "1 second ago".to_string()
        } else {
            format!("{secs} seconds ago")
        }
    } else if secs < 3600 {
        let mins = secs / 60;
        if mins == 1 {
            "1 minute ago".to_string()
        } else {
            format!("{mins} minutes ago")
        }
    } else if secs < 86400 {
        let hours = secs / 3600;
        if hours == 1 {
            "1 hour ago".to_string()
        } else {
            format!("{hours} hours ago")
        }
    } else {
        let days = secs / 86400;
        if days == 1 {
            "1 day ago".to_string()
        } else {
            format!("{days} days ago")
        }
    }
}

fn check_heartbeat(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    checks.push(DiagnosticCheck::pass(
        "Heartbeat",
        format!("interval: {}", config.heartbeat.interval),
    ));

    // Check last heartbeat from activity log
    match crate::db::Database::open() {
        Ok(db) => {
            match db.query_activity(1, Some("info"), Some("heartbeat")) {
                Ok(entries) if !entries.is_empty() => {
                    let last = &entries[0];
                    let age_secs = chrono::Utc::now().timestamp() - last.created_at;

                    // Stale threshold: 2x configured interval (default 1h for 30m interval)
                    let interval_secs = crate::tasks::parse_interval(&config.heartbeat.interval)
                        .unwrap_or(std::time::Duration::from_secs(1800))
                        .as_secs() as i64;
                    let stale_threshold = interval_secs * 2;

                    let relative = format_relative_time(last.created_at);
                    if age_secs <= stale_threshold {
                        checks.push(DiagnosticCheck::pass(
                            "Heartbeat",
                            format!("last heartbeat: {relative}"),
                        ));
                    } else {
                        checks.push(DiagnosticCheck::warn(
                            "Heartbeat",
                            "last heartbeat",
                            format!("{relative} — may not be running"),
                        ));
                    }
                }
                Ok(_) => {
                    checks.push(DiagnosticCheck::warn(
                        "Heartbeat",
                        "last heartbeat",
                        "no heartbeat activity recorded",
                    ));
                }
                Err(e) => {
                    checks.push(DiagnosticCheck::fail(
                        "Heartbeat",
                        "activity query",
                        format!("{e}"),
                    ));
                }
            }

            // Check if daemon is running (heartbeat runs in daemon or TUI)
            if db.is_daemon_lock_held() {
                checks.push(DiagnosticCheck::pass(
                    "Heartbeat",
                    "daemon is running (heartbeat active)".to_string(),
                ));
            } else {
                checks.push(DiagnosticCheck::warn(
                    "Heartbeat",
                    "daemon status",
                    "daemon not running — heartbeat only active during TUI sessions",
                ));
            }
        }
        Err(e) => {
            checks.push(DiagnosticCheck::fail(
                "Heartbeat",
                "database",
                format!("cannot check heartbeat status: {e}"),
            ));
        }
    }
}

fn check_budget(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    let limit = config.budget.monthly_token_limit;
    if limit == 0 {
        checks.push(DiagnosticCheck::warn(
            "Budget",
            "monthly limit",
            "unlimited (no budget set)",
        ));
        return;
    }

    match Database::open() {
        Ok(db) => match db.monthly_token_total() {
            Ok(used) => {
                let pct = (used as f64 / limit as f64 * 100.0) as u64;
                let name = format!("monthly usage: {used}/{limit} ({pct}%)");
                if used >= limit {
                    checks.push(DiagnosticCheck::fail("Budget", name, "budget exceeded"));
                } else if (used as f64 / limit as f64) >= config.budget.warning_threshold {
                    checks.push(DiagnosticCheck::warn("Budget", name, "approaching limit"));
                } else {
                    checks.push(DiagnosticCheck::pass("Budget", name));
                }
            }
            Err(e) => {
                checks.push(DiagnosticCheck::warn(
                    "Budget",
                    "monthly usage",
                    format!("could not read: {e}"),
                ));
            }
        },
        Err(e) => {
            checks.push(DiagnosticCheck::warn(
                "Budget",
                "monthly usage",
                format!("database unavailable: {e}"),
            ));
        }
    }
}

fn check_prompt_cache(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    if !config.llm.cache.enabled {
        checks.push(DiagnosticCheck::warn(
            "Prompt Cache",
            "status",
            "disabled (llm.cache.enabled = false)",
        ));
        return;
    }
    checks.push(DiagnosticCheck::pass(
        "Prompt Cache",
        format!(
            "enabled (ttl={}, tools={}, system={}, rolling={})",
            config.llm.cache.ttl,
            config.llm.cache.cache_tools,
            config.llm.cache.cache_system,
            config.llm.cache.rolling_messages_clamped(),
        ),
    ));

    // Hit ratio over the last 24 hours — informational. Providers that do not
    // support caching (Ollama, Groq, Gemini non-cached) will report 0, which
    // we surface as a warning only when the primary provider is known to
    // support caching.
    let since = chrono::Utc::now().timestamp().saturating_sub(24 * 3600);
    match Database::open().and_then(|db| db.cache_token_summary_since(since)) {
        Ok((prompt, cached, created)) => {
            if prompt == 0 {
                checks.push(DiagnosticCheck::pass(
                    "Prompt Cache",
                    "24h hit ratio: no traffic yet",
                ));
                return;
            }
            let pct = (cached as f64 / prompt as f64 * 100.0) as u64;
            let name =
                format!("24h hit ratio: {pct}% ({cached}/{prompt} cached, {created} created)");
            let supports_cache = provider_supports_cache(&config.llm);
            if supports_cache && pct < 20 && prompt > 10_000 {
                checks.push(DiagnosticCheck::warn(
                    "Prompt Cache",
                    name,
                    "low hit ratio — check system prompt stability",
                ));
            } else {
                checks.push(DiagnosticCheck::pass("Prompt Cache", name));
            }
        }
        Err(e) => {
            checks.push(DiagnosticCheck::warn(
                "Prompt Cache",
                "24h hit ratio",
                format!("could not read: {e}"),
            ));
        }
    }
}

/// Returns true if the configured provider is known to honor prompt caching
/// (either via explicit `cache_control` markers or implicit auto-caching).
fn provider_supports_cache(llm: &crate::config::LlmConfig) -> bool {
    let provider = llm.provider.as_deref().unwrap_or("");
    matches!(
        provider,
        "anthropic" | "openai" | "openrouter" | "deepseek" | "claude-cli"
    )
}

fn check_plugins(checks: &mut Vec<DiagnosticCheck>) {
    match Database::open() {
        Ok(db) => match db.list_plugins() {
            Ok(plugins) => {
                if plugins.is_empty() {
                    checks.push(DiagnosticCheck::warn(
                        "Plugins",
                        "installed integrations",
                        "none installed (use /plugins)",
                    ));
                } else {
                    let verified = plugins.iter().filter(|c| c.verified_at.is_some()).count();
                    checks.push(DiagnosticCheck::pass(
                        "Plugins",
                        format!(
                            "{} integration(s) installed, {verified} verified",
                            plugins.len()
                        ),
                    ));

                    for c in &plugins {
                        let name = format!("{} ({})", c.name, c.kind);
                        if c.verified_at.is_some() {
                            checks.push(DiagnosticCheck::pass("Plugins", name));
                        } else {
                            checks.push(DiagnosticCheck::warn("Plugins", name, "not verified"));
                        }

                        if let Ok(data_dir) = Config::data_dir() {
                            if let Ok(result) =
                                crate::integrity::verify_integrity(&db, &c.id, &data_dir)
                            {
                                let integrity_name = format!("{} file integrity", c.name);
                                if result.ok {
                                    checks.push(DiagnosticCheck::pass("Plugins", integrity_name));
                                } else {
                                    let mut issues = Vec::new();
                                    if !result.tampered.is_empty() {
                                        issues.push(format!(
                                            "tampered: {}",
                                            result.tampered.join(", ")
                                        ));
                                    }
                                    if !result.missing.is_empty() {
                                        issues.push(format!(
                                            "missing: {}",
                                            result.missing.join(", ")
                                        ));
                                    }
                                    checks.push(DiagnosticCheck::fail(
                                        "Plugins",
                                        integrity_name,
                                        issues.join("; "),
                                    ));
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                checks.push(DiagnosticCheck::warn(
                    "Plugins",
                    "plugins table",
                    format!("could not query: {e}"),
                ));
            }
        },
        Err(e) => {
            checks.push(DiagnosticCheck::warn(
                "Plugins",
                "database",
                format!("unavailable: {e}"),
            ));
        }
    }
}

fn check_browser(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    if !config.browser.enabled {
        checks.push(DiagnosticCheck::warn(
            "Browser",
            "browser automation",
            "disabled in config",
        ));
        return;
    }

    let detection = crate::browser::find_chrome(config.browser.executable.as_deref());
    if let Some(ref exe) = detection.executable {
        checks.push(DiagnosticCheck::pass(
            "Browser",
            format!("Chrome detected: {}", exe.display()),
        ));
    } else {
        checks.push(DiagnosticCheck::warn(
            "Browser",
            "Chrome/Chromium detection",
            "no Chrome-like browser found",
        ));
    }

    if detection.executable.is_some() {
        checks.push(DiagnosticCheck::pass(
            "Browser",
            "native CDP browser tool available",
        ));
    }

    if crate::browser::detect_agent_browser() {
        checks.push(DiagnosticCheck::pass(
            "Browser",
            "agent-browser CLI also available (legacy)",
        ));
    }

    checks.push(DiagnosticCheck::pass(
        "Browser",
        format!(
            "config: headless={}, cdp_port={}, timeout={}ms",
            config.browser.headless, config.browser.cdp_port, config.browser.timeout_ms
        ),
    ));
}

fn check_agents(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    if !config.agents.enabled {
        checks.push(DiagnosticCheck::warn(
            "Agents",
            "multi-agent system",
            "disabled",
        ));
        return;
    }
    checks.push(DiagnosticCheck::pass(
        "Agents",
        format!(
            "multi-agent enabled (depth={}, children={}, concurrent={})",
            config.agents.max_spawn_depth,
            config.agents.max_children_per_agent,
            config.agents.max_concurrent,
        ),
    ));
    if config.agents.max_spawn_depth > 3 {
        checks.push(DiagnosticCheck::warn(
            "Agents",
            "spawn depth",
            format!(
                "max_spawn_depth={} is high (recommended ≤3)",
                config.agents.max_spawn_depth
            ),
        ));
    }
    let roles = crate::multi_agent::roles::list_all_roles();
    checks.push(DiagnosticCheck::pass(
        "Agents",
        format!("{} role(s) available", roles.len()),
    ));
}

fn check_embeddings(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    if !config.memory.embeddings.enabled {
        checks.push(DiagnosticCheck::warn(
            "Embeddings",
            "semantic memory search",
            "disabled in config",
        ));
        return;
    }

    match crate::embeddings::EmbeddingProvider::from_config(&config.memory.embeddings) {
        Some(provider) => {
            checks.push(DiagnosticCheck::pass(
                "Embeddings",
                format!(
                    "provider: {} (model: {})",
                    provider.endpoint, provider.model
                ),
            ));
        }
        None => {
            checks.push(DiagnosticCheck::warn(
                "Embeddings",
                "embedding provider",
                "no embedding-capable API key found, using recency-based loading",
            ));
            return;
        }
    }

    match Database::open() {
        Ok(db) => {
            let global_count = db.count_embeddings("global").unwrap_or(0);
            let entry_count = db
                .list_memory_entries("global")
                .map(|v| v.len())
                .unwrap_or(0);

            let name = format!("embeddings: {global_count}/{entry_count} memory entries indexed");
            if entry_count == 0 || global_count >= entry_count {
                checks.push(DiagnosticCheck::pass("Embeddings", name));
            } else {
                checks.push(DiagnosticCheck::warn(
                    "Embeddings",
                    name,
                    "some memory entries not yet embedded (will be indexed on next write)",
                ));
            }
        }
        Err(e) => {
            checks.push(DiagnosticCheck::warn(
                "Embeddings",
                "embedding count",
                format!("database unavailable: {e}"),
            ));
        }
    }
}

fn check_config_security(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    use crate::pairing::DmPolicy;

    // DM policy (three-way match — not table-driven)
    match config.gateway.dm_policy {
        DmPolicy::Open => checks.push(DiagnosticCheck::warn(
            "Security",
            "DM policy",
            "set to \"open\" — all senders can interact without approval. Set to \"pairing\" for access control",
        )),
        DmPolicy::Pairing => checks.push(DiagnosticCheck::pass("Security", "DM policy (pairing)")),
        DmPolicy::Disabled => checks.push(DiagnosticCheck::pass("Security", "DM policy (disabled)")),
    }

    // Per-channel open policies (dynamic list — not table-driven)
    let open_channels: Vec<&String> = config
        .gateway
        .channel_policies
        .iter()
        .filter(|(_, policy)| matches!(policy, DmPolicy::Open))
        .map(|(name, _)| name)
        .collect();
    if open_channels.is_empty() {
        checks.push(DiagnosticCheck::pass(
            "Security",
            "channel policies (no open overrides)",
        ));
    } else {
        checks.push(DiagnosticCheck::warn(
            "Security",
            "channel policies",
            format!(
                "open DM policy on: {}. Consider \"pairing\" for untrusted channels",
                open_channels
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }

    // Table-driven boolean/simple checks
    struct BoolCheck {
        ok: bool,
        pass_label: &'static str,
        warn_label: &'static str,
        warn_detail: &'static str,
    }

    let bool_checks = [
        BoolCheck {
            ok: config.sandbox.enabled && config.sandbox.mode != "permissive",
            pass_label: if config.sandbox.enabled { "sandbox enabled (strict)" } else { "sandbox" },
            warn_label: if !config.sandbox.enabled { "sandbox" } else { "sandbox mode" },
            warn_detail: if !config.sandbox.enabled {
                "disabled — user tools run without isolation. Set sandbox.enabled = true"
            } else {
                "set to \"permissive\" — weaker isolation. Consider \"strict\""
            },
        },
        BoolCheck {
            ok: config.security.secret_detection,
            pass_label: "secret detection enabled",
            warn_label: "secret detection",
            warn_detail: "disabled — API keys and tokens may leak in tool output. Set security.secret_detection = true",
        },
        BoolCheck {
            ok: !config.security.blocked_paths.is_empty(),
            pass_label: "blocked paths",
            warn_label: "blocked paths",
            warn_detail: "empty — sensitive directories (.ssh, .aws, .gnupg) are not protected. Add entries to security.blocked_paths",
        },
        BoolCheck {
            ok: config.security.action_limits.tool_calls_block <= 1000
                && config.security.action_limits.shell_commands_block <= 500
                && config.security.action_limits.file_writes_block <= 300,
            pass_label: "rate limits within bounds",
            warn_label: "rate limits",
            warn_detail: "block thresholds are very high — consider lowering for tighter safety bounds",
        },
        BoolCheck {
            ok: config.budget.monthly_token_limit != 0,
            pass_label: "budget",
            warn_label: "budget",
            warn_detail: "unlimited (monthly_token_limit = 0) — no spend cap. Set a limit to prevent runaway usage",
        },
    ];

    for check in &bool_checks {
        if check.ok {
            // Dynamic pass labels for blocked_paths count and budget cap
            let label = match check.pass_label {
                "blocked paths" => format!(
                    "blocked paths ({} entries)",
                    config.security.blocked_paths.len()
                ),
                "budget" => format!(
                    "budget capped ({} tokens/month)",
                    config.budget.monthly_token_limit
                ),
                other => other.to_string(),
            };
            checks.push(DiagnosticCheck::pass("Security", label));
        } else {
            checks.push(DiagnosticCheck::warn(
                "Security",
                check.warn_label,
                check.warn_detail,
            ));
        }
    }

    // Browser no-sandbox (conditional on browser being enabled)
    if config.browser.enabled && config.browser.no_sandbox {
        checks.push(DiagnosticCheck::warn(
            "Security",
            "browser sandbox",
            "Chrome --no-sandbox is enabled — browser runs without isolation. Set browser.no_sandbox = false",
        ));
    } else if config.browser.enabled {
        checks.push(DiagnosticCheck::pass("Security", "browser sandbox enabled"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_format_output() {
        let report = DiagnosticReport {
            checks: vec![
                DiagnosticCheck::pass("Config", "config.toml exists"),
                DiagnosticCheck::pass("Config", "config.toml valid"),
                DiagnosticCheck::fail("Provider", "API key set", "not found"),
                DiagnosticCheck::warn("Sandbox", "sandbox-exec", "not available"),
            ],
        };
        let output = report.format();
        assert!(output.contains("Borg Doctor"));
        assert!(output.contains("✓ config.toml exists"));
        assert!(output.contains("✗ API key set"));
        assert!(output.contains("⚠ sandbox-exec"));
        assert!(output.contains("2 passed, 1 warning(s), 1 failed"));
    }

    #[test]
    fn report_counts() {
        let report = DiagnosticReport {
            checks: vec![
                DiagnosticCheck::pass("Test", "pass"),
                DiagnosticCheck::warn("Test", "warn", "w"),
                DiagnosticCheck::fail("Test", "fail", "f"),
            ],
        };
        assert_eq!(report.counts(), (1, 1, 1));
    }

    #[test]
    fn empty_report() {
        let report = DiagnosticReport { checks: vec![] };
        let output = report.format();
        assert!(output.contains("0 passed"));
        assert_eq!(report.counts(), (0, 0, 0));
    }

    #[test]
    fn run_diagnostics_produces_checks() {
        let config = Config::default();
        let report = run_diagnostics(&config);
        // Should always produce at least the config and provider checks
        assert!(!report.checks.is_empty());
        // Should have at least config, provider, sandbox, tools, skills, memory, data categories
        let categories: std::collections::HashSet<&str> =
            report.checks.iter().map(|c| c.category).collect();
        assert!(categories.contains("Config"));
        assert!(categories.contains("Provider"));
    }

    #[test]
    fn diagnostic_check_pass_fields() {
        let check = DiagnosticCheck::pass("Config", "config.toml exists");
        assert_eq!(check.category, "Config");
        assert_eq!(check.name, "config.toml exists");
        assert_eq!(check.status, CheckStatus::Pass);
    }

    #[test]
    fn diagnostic_check_fail_includes_detail() {
        let check = DiagnosticCheck::fail("Provider", "API key", "not found in env");
        assert_eq!(check.category, "Provider");
        assert!(matches!(check.status, CheckStatus::Fail(ref msg) if msg == "not found in env"));
    }

    #[test]
    fn diagnostic_check_warn_includes_detail() {
        let check = DiagnosticCheck::warn("Sandbox", "sandbox-exec", "not available on Linux");
        assert_eq!(check.category, "Sandbox");
        assert!(
            matches!(check.status, CheckStatus::Warn(ref msg) if msg == "not available on Linux")
        );
    }

    #[test]
    fn report_format_groups_consecutive_same_category() {
        let report = DiagnosticReport {
            checks: vec![
                DiagnosticCheck::pass("Config", "check A"),
                DiagnosticCheck::pass("Config", "check C"),
                DiagnosticCheck::pass("Provider", "check B"),
            ],
        };
        let output = report.format();
        // Consecutive Config checks should appear under a single Config heading
        let config_heading_count = output.matches("\nConfig\n").count();
        assert_eq!(
            config_heading_count, 1,
            "Consecutive same-category checks should share one heading"
        );
        // check A and check C should both appear before Provider
        let check_a_pos = output.find("check A").unwrap();
        let check_c_pos = output.find("check C").unwrap();
        let provider_pos = output.find("Provider").unwrap();
        assert!(check_a_pos < check_c_pos);
        assert!(check_c_pos < provider_pos);
    }

    #[test]
    fn run_diagnostics_includes_browser_category() {
        let config = Config::default();
        let report = run_diagnostics(&config);
        let categories: std::collections::HashSet<&str> =
            report.checks.iter().map(|c| c.category).collect();
        assert!(categories.contains("Browser"));
    }

    #[test]
    fn browser_disabled_produces_warn() {
        let mut checks = Vec::new();
        let mut config = Config::default();
        config.browser.enabled = false;
        check_browser(&config, &mut checks);
        assert_eq!(checks.len(), 1);
        assert!(matches!(checks[0].status, CheckStatus::Warn(_)));
        assert_eq!(checks[0].category, "Browser");
    }

    #[test]
    fn config_security_secure_defaults_all_pass() {
        let config = Config::default();
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        // All checks should be in the Security category
        assert!(checks.iter().all(|c| c.category == "Security"));
        // Default config is secure — no warnings expected
        let warns: Vec<_> = checks
            .iter()
            .filter(|c| matches!(c.status, CheckStatus::Warn(_)))
            .collect();
        assert!(
            warns.is_empty(),
            "Secure defaults should produce no warnings: {warns:?}"
        );
    }

    #[test]
    fn config_security_open_dm_policy_warns() {
        let mut config = Config::default();
        config.gateway.dm_policy = crate::pairing::DmPolicy::Open;
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let dm_check = checks.iter().find(|c| c.name == "DM policy").unwrap();
        assert!(matches!(dm_check.status, CheckStatus::Warn(_)));
    }

    #[test]
    fn config_security_sandbox_disabled_warns() {
        let mut config = Config::default();
        config.sandbox.enabled = false;
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let sandbox_check = checks.iter().find(|c| c.name == "sandbox").unwrap();
        assert!(matches!(sandbox_check.status, CheckStatus::Warn(_)));
    }

    #[test]
    fn config_security_secret_detection_off_warns() {
        let mut config = Config::default();
        config.security.secret_detection = false;
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let check = checks
            .iter()
            .find(|c| c.name == "secret detection")
            .unwrap();
        assert!(matches!(check.status, CheckStatus::Warn(_)));
    }

    #[test]
    fn config_security_empty_blocked_paths_warns() {
        let mut config = Config::default();
        config.security.blocked_paths.clear();
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let check = checks.iter().find(|c| c.name == "blocked paths").unwrap();
        assert!(matches!(check.status, CheckStatus::Warn(_)));
    }

    #[test]
    fn config_security_browser_no_sandbox_warns() {
        let mut config = Config::default();
        config.browser.enabled = true;
        config.browser.no_sandbox = true;
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let check = checks.iter().find(|c| c.name == "browser sandbox").unwrap();
        assert!(matches!(check.status, CheckStatus::Warn(_)));
    }

    #[test]
    fn config_security_open_channel_policy_warns() {
        let mut config = Config::default();
        config
            .gateway
            .channel_policies
            .insert("telegram".into(), crate::pairing::DmPolicy::Open);
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let check = checks
            .iter()
            .find(|c| c.name == "channel policies")
            .unwrap();
        assert!(matches!(check.status, CheckStatus::Warn(ref msg) if msg.contains("telegram")));
    }

    #[test]
    fn config_security_high_rate_limits_warns() {
        let mut config = Config::default();
        config.security.action_limits.tool_calls_block = 1500;
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let check = checks.iter().find(|c| c.name == "rate limits").unwrap();
        assert!(matches!(check.status, CheckStatus::Warn(_)));
    }

    #[test]
    fn config_security_budget_set_passes() {
        let mut config = Config::default();
        config.budget.monthly_token_limit = 1_000_000;
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let check = checks.iter().find(|c| c.name.contains("budget")).unwrap();
        assert!(matches!(check.status, CheckStatus::Pass));
    }

    #[test]
    fn run_diagnostics_includes_security_category() {
        let config = Config::default();
        let report = run_diagnostics(&config);
        let categories: std::collections::HashSet<&str> =
            report.checks.iter().map(|c| c.category).collect();
        assert!(categories.contains("Security"));
    }

    #[test]
    fn format_relative_time_seconds() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(format_relative_time(now - 30), "30 seconds ago");
        assert_eq!(format_relative_time(now - 1), "1 second ago");
    }

    #[test]
    fn format_relative_time_minutes() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(format_relative_time(now - 60), "1 minute ago");
        assert_eq!(format_relative_time(now - 120), "2 minutes ago");
        assert_eq!(format_relative_time(now - 1800), "30 minutes ago");
    }

    #[test]
    fn format_relative_time_hours() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(format_relative_time(now - 3600), "1 hour ago");
        assert_eq!(format_relative_time(now - 7200), "2 hours ago");
    }

    #[test]
    fn format_relative_time_days() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(format_relative_time(now - 86400), "1 day ago");
        assert_eq!(format_relative_time(now - 172800), "2 days ago");
    }

    #[test]
    fn format_relative_time_future() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(format_relative_time(now + 100), "just now");
    }

    #[test]
    fn run_diagnostics_includes_heartbeat_category() {
        let config = Config::default();
        let report = run_diagnostics(&config);
        let categories: std::collections::HashSet<&str> =
            report.checks.iter().map(|c| c.category).collect();
        assert!(categories.contains("Heartbeat"));
    }

    #[test]
    fn heartbeat_check_shows_interval() {
        let config = Config::default();
        let mut checks = Vec::new();
        check_heartbeat(&config, &mut checks);
        assert!(checks.iter().any(|c| c.name.contains("interval: 30m")));
    }
}
