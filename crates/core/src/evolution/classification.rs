//! Archetype classification for tool calls and shell commands.

use super::{Archetype, Stage};

// Keyword sets for shell command classification
const OPS_KEYWORDS: &[&str] = &[
    "deploy",
    "kubernetes",
    "k8s",
    "docker",
    "terraform",
    "ansible",
    "nginx",
    "systemctl",
    "journalctl",
    "helm",
    "pipeline",
    "prometheus",
    "grafana",
    "kubectl",
    "podman",
    "ci/cd",
    "jenkins",
    "github-actions",
    "circleci",
    "argocd",
    "istio",
    "envoy",
    "consul",
    "vault",
    "nomad",
    "pulumi",
    "cloudformation",
    "cdk",
    "aws",
    "gcloud",
    "azure",
    "s3",
    "ec2",
    "lambda",
    "ecs",
    "fargate",
    "systemd",
    "crontab",
    "uptime",
    "nagios",
    "datadog",
    "pagerduty",
    "rollback",
    "canary",
    "blue-green",
    "load-balancer",
    "haproxy",
    "traefik",
    "certbot",
    "letsencrypt",
];

const BUILDER_KEYWORDS: &[&str] = &[
    "cargo",
    "npm",
    "pip",
    "gcc",
    "make",
    "build",
    "compile",
    "lint",
    "rustc",
    "webpack",
    "vite",
    "esbuild",
    "yarn",
    "pnpm",
    "bun",
    "deno",
    "gradle",
    "maven",
    "cmake",
    "bazel",
    "meson",
    "clang",
    "g++",
    "javac",
    "tsc",
    "swc",
    "rollup",
    "parcel",
    "turbopack",
    "nx",
    "lerna",
    "monorepo",
    "prettier",
    "eslint",
    "clippy",
    "ruff",
    "mypy",
    "pytest",
    "jest",
    "vitest",
    "mocha",
    "cargo-test",
    "go-build",
    "dotnet",
    "msbuild",
    "xcodebuild",
    "swift-build",
    "pkg-config",
    "autoconf",
    "automake",
    "scons",
];

const ANALYST_KEYWORDS: &[&str] = &[
    "query",
    "select",
    "aggregate",
    "report",
    "analyze",
    "csv",
    "data",
    "metric",
    "psql",
    "sqlite3",
    "mysql",
    "postgres",
    "mongodb",
    "redis",
    "elasticsearch",
    "kibana",
    "pandas",
    "numpy",
    "jupyter",
    "notebook",
    "dataframe",
    "pivot",
    "dashboard",
    "visualization",
    "chart",
    "graph",
    "bigquery",
    "redshift",
    "snowflake",
    "dbt",
    "etl",
    "parquet",
    "arrow",
    "sql",
    "nosql",
    "olap",
    "warehouse",
    "tableau",
    "powerbi",
    "looker",
    "superset",
    "clickhouse",
    "timescaledb",
    "influxdb",
    "statistic",
    "regression",
    "forecast",
    "anomaly",
    "correlation",
];

const GUARDIAN_KEYWORDS: &[&str] = &[
    "firewall",
    "ufw",
    "iptables",
    "nmap",
    "chmod",
    "chown",
    "audit",
    "vulnerability",
    "cve",
    "openssl",
    "tls",
    "ssl",
    "certificate",
    "encrypt",
    "decrypt",
    "hash",
    "hmac",
    "jwt",
    "oauth",
    "saml",
    "ldap",
    "kerberos",
    "selinux",
    "apparmor",
    "seccomp",
    "fail2ban",
    "ossec",
    "snort",
    "suricata",
    "wireshark",
    "tcpdump",
    "pentest",
    "exploit",
    "payload",
    "metasploit",
    "burpsuite",
    "owasp",
    "sast",
    "dast",
    "sonarqube",
    "trivy",
    "grype",
    "cosign",
    "sigstore",
    "gpg",
    "keyring",
    "secret",
    "credential",
    "rotation",
    "compliance",
    "soc2",
    "gdpr",
    "hipaa",
];

const STRATEGIST_KEYWORDS: &[&str] = &[
    "plan",
    "prioritize",
    "compare",
    "evaluate",
    "decision",
    "roadmap",
    "okr",
    "kpi",
    "milestone",
    "sprint",
    "backlog",
    "epic",
    "story",
    "kanban",
    "scrum",
    "agile",
    "retro",
    "standup",
    "stakeholder",
    "budget",
    "forecast",
    "estimate",
    "timeline",
    "deadline",
    "dependency",
    "risk",
    "tradeoff",
    "proposal",
    "rfc",
    "adr",
    "strategy",
    "objective",
    "initiative",
    "quarterly",
    "review",
    "assessment",
    "benchmark",
];

const TINKERER_KEYWORDS: &[&str] = &[
    "homelab",
    "proxmox",
    "pve",
    "esxi",
    "truenas",
    "pihole",
    "wireguard",
    "tailscale",
    "raspberry",
    "arduino",
    "serial",
    "gpio",
    "mqtt",
    "zigbee",
    "zwave",
    "esp32",
    "esp8266",
    "stm32",
    "fpga",
    "verilog",
    "soldering",
    "oscilloscope",
    "multimeter",
    "i2c",
    "spi",
    "uart",
    "can-bus",
    "modbus",
    "home-assistant",
    "hass",
    "openwrt",
    "pfsense",
    "opnsense",
    "unifi",
    "mikrotik",
    "vlan",
    "nas",
    "raid",
    "zfs",
    "btrfs",
    "3dprint",
    "octoprint",
    "klipper",
    "qemu",
    "libvirt",
    "lxc",
    "synology",
    "nut",
    "ups",
];

/// Deterministic tool-name → archetype mapping.
pub fn classify_tool_archetype(tool_name: &str, metadata: Option<&str>) -> Option<Archetype> {
    // Direct tool name mapping
    let archetype = match tool_name {
        "apply_patch" | "apply_skill_patch" | "create_channel" => Some(Archetype::Builder),
        "browser" | "search" | "memory_search" => Some(Archetype::Analyst),
        "calendar" | "notion" | "linear" | "schedule" | "manage_tasks" => {
            Some(Archetype::Strategist)
        }
        "gmail" => Some(Archetype::Communicator),
        "write_memory" => Some(Archetype::Creator),
        "run_shell" => classify_shell_command(metadata),
        _ => None,
    };

    if archetype.is_some() {
        return archetype;
    }

    // Check if tool name matches a known channel/integration
    let name_lower = tool_name.to_lowercase();
    if name_lower.contains("telegram")
        || name_lower.contains("slack")
        || name_lower.contains("discord")
        || name_lower.contains("whatsapp")
        || name_lower.contains("sms")
    {
        return Some(Archetype::Communicator);
    }

    if name_lower.contains("docker")
        || name_lower.contains("git")
        || name_lower.contains("database")
    {
        return Some(Archetype::Ops);
    }

    None
}

/// Classify a shell command by scanning its content for archetype keywords.
fn classify_shell_command(metadata: Option<&str>) -> Option<Archetype> {
    let text = metadata?.to_lowercase();

    let keyword_sets: &[(&[&str], Archetype)] = &[
        (OPS_KEYWORDS, Archetype::Ops),
        (BUILDER_KEYWORDS, Archetype::Builder),
        (ANALYST_KEYWORDS, Archetype::Analyst),
        (GUARDIAN_KEYWORDS, Archetype::Guardian),
        (STRATEGIST_KEYWORDS, Archetype::Strategist),
        (TINKERER_KEYWORDS, Archetype::Tinkerer),
    ];

    for (keywords, archetype) in keyword_sets {
        if keywords.iter().any(|kw| text.contains(kw)) {
            return Some(*archetype);
        }
    }

    None
}

// ── Fallback evolution naming ──
//
// LLM generation is best-effort; when it fails or is disabled these
// deterministic names guarantee the user is never left with "Base Borg"
// after a stage transition.

/// Deterministic fallback name + description for a stage/archetype pair.
/// Returns `(name, description)`. Stage::Base returns a generic label.
pub fn fallback_evolution_name(archetype: Option<Archetype>, stage: Stage) -> (String, String) {
    let arch = match archetype {
        Some(a) => a,
        None => {
            return (
                "Unbound Borg".to_string(),
                "An adaptable agent still discovering its specialization.".to_string(),
            );
        }
    };
    let (name_s2, name_s3) = archetype_fallback_names(arch);
    let name = match stage {
        Stage::Base => name_s2, // pre-evolution — reuse stage 2 name
        Stage::Evolved => name_s2,
        Stage::Final => name_s3,
    };
    let description = archetype_fallback_description(arch);
    (name.to_string(), description.to_string())
}

/// (stage2 name, stage3 name) for each archetype.
fn archetype_fallback_names(archetype: Archetype) -> (&'static str, &'static str) {
    match archetype {
        Archetype::Ops => ("Pipeline Warden", "Infrastructure Sovereign"),
        Archetype::Builder => ("Tool Forgemaster", "Automation Architect"),
        Archetype::Analyst => ("Insight Diviner", "Pattern Oracle"),
        Archetype::Communicator => ("Outreach Operative", "Signal Weaver"),
        Archetype::Guardian => ("Vigilant Sentinel", "Fortress Keeper"),
        Archetype::Strategist => ("Path Finder", "Grand Planner"),
        Archetype::Creator => ("Word Smith", "Narrative Architect"),
        Archetype::Caretaker => ("Gentle Steward", "Household Guardian"),
        Archetype::Merchant => ("Ledger Keeper", "Commerce Sage"),
        Archetype::Tinkerer => ("Bench Wizard", "Homelab Artisan"),
    }
}

/// Attempt to generate an evolution name + description via the configured LLM.
/// Returns `(name, description)` on success, falling back to the deterministic
/// table on any error (config load, LLM failure, parse failure, timeout).
///
/// `top_tools` is `(tool_name, count)` sorted descending — the LLM uses this
/// to personalize the name. Pass an empty slice if unavailable.
pub async fn generate_evolution_name(
    archetype: Option<Archetype>,
    stage: Stage,
    top_tools: &[(String, u32)],
) -> (String, String) {
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    let fallback = fallback_evolution_name(archetype, stage);

    let attempt = async {
        let config = crate::config::Config::load_from_db()
            .map_err(|e| anyhow::anyhow!("config load: {e}"))?;
        let mut client =
            crate::llm::LlmClient::new(&config).map_err(|e| anyhow::anyhow!("llm client: {e}"))?;

        let archetype_str = archetype
            .map(|a| a.to_string())
            .unwrap_or_else(|| "unspecified".to_string());
        let stage_str = match stage {
            Stage::Base => "Base",
            Stage::Evolved => "Evolved",
            Stage::Final => "Final",
        };
        let tools_line = if top_tools.is_empty() {
            "none reported".to_string()
        } else {
            top_tools
                .iter()
                .take(5)
                .map(|(n, c)| format!("{n} (×{c})"))
                .collect::<Vec<_>>()
                .join(", ")
        };

        let system = "You name AI agent evolutions. Respond with STRICT JSON: \
            {\"name\": \"<2-4 words, evocative, title case>\", \
            \"description\": \"<1-2 sentences, warm, references the specialization>\"} \
            and nothing else.";
        let user = format!(
            "Archetype: {archetype_str}\nStage: {stage_str}\nTop tools: {tools_line}\n\n\
             Generate a unique evolution name and description.",
        );

        let messages = vec![
            crate::types::Message::system(system),
            crate::types::Message::user(user),
        ];

        let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::llm::StreamEvent>(64);
        let cancel = tokio_util::sync::CancellationToken::new();

        let llm_task = async {
            client
                .stream_chat_with_cancel(&messages, None, tx, cancel.clone())
                .await
                .map_err(|e| anyhow::anyhow!("stream_chat: {e}"))
        };
        let collect_task = async {
            let mut acc = String::new();
            while let Some(ev) = rx.recv().await {
                match ev {
                    crate::llm::StreamEvent::TextDelta(s) => acc.push_str(&s),
                    crate::llm::StreamEvent::Done => break,
                    crate::llm::StreamEvent::Error(e) => {
                        return Err(anyhow::anyhow!("stream error: {e}"))
                    }
                    _ => {}
                }
            }
            Ok::<String, anyhow::Error>(acc)
        };

        let (_, accumulated) = tokio::try_join!(llm_task, collect_task)?;
        parse_name_response(&accumulated)
    };

    match tokio::time::timeout(TIMEOUT, attempt).await {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => {
            tracing::warn!("evolution: LLM naming failed ({e}) — using fallback");
            fallback
        }
        Err(_) => {
            tracing::warn!("evolution: LLM naming timed out — using fallback");
            fallback
        }
    }
}

/// Parse the LLM response into (name, description). Accepts raw JSON or JSON
/// embedded in markdown code fences / surrounding text.
fn parse_name_response(raw: &str) -> anyhow::Result<(String, String)> {
    let trimmed = raw.trim();
    let json_str = extract_json_object(trimmed).unwrap_or(trimmed);
    let parsed: serde_json::Value =
        serde_json::from_str(json_str).map_err(|e| anyhow::anyhow!("not valid JSON: {e}"))?;
    let name = parsed
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing name"))?
        .trim()
        .to_string();
    let description = parsed
        .get("description")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing description"))?
        .trim()
        .to_string();
    if name.is_empty() || description.is_empty() {
        return Err(anyhow::anyhow!("empty name or description"));
    }
    Ok((name, description))
}

/// Find the first balanced `{...}` block in `text`. Handles strings and escapes.
fn extract_json_object(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            match b {
                b'\\' => escape = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

fn archetype_fallback_description(archetype: Archetype) -> &'static str {
    match archetype {
        Archetype::Ops => {
            "A vigilant DevOps guardian keeping your builds green and deploys smooth."
        }
        Archetype::Builder => {
            "A restless builder who'd rather automate a task once than do it twice."
        }
        Archetype::Analyst => "A patient investigator who turns raw signal into decisions.",
        Archetype::Communicator => {
            "A relentless communicator turning cold leads warm and inboxes manageable."
        }
        Archetype::Guardian => "A careful sentinel watching the gates so you don't have to.",
        Archetype::Strategist => "A calm planner laying out the next move before it's needed.",
        Archetype::Creator => "A thoughtful writer shaping words and narratives with care.",
        Archetype::Caretaker => "A quiet steward keeping the household rhythms on beat.",
        Archetype::Merchant => "A meticulous keeper of ledgers and commerce flows.",
        Archetype::Tinkerer => "A curious hacker who can't leave a homelab alone for five minutes.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_strict_json() {
        let raw = r#"{"name":"Pipeline Warden","description":"A guardian."}"#;
        let (n, d) = parse_name_response(raw).unwrap();
        assert_eq!(n, "Pipeline Warden");
        assert_eq!(d, "A guardian.");
    }

    #[test]
    fn parse_json_in_markdown_fences() {
        let raw = r#"Sure, here:
```json
{"name": "Tool Forgemaster", "description": "Automates things."}
```
"#;
        let (n, d) = parse_name_response(raw).unwrap();
        assert_eq!(n, "Tool Forgemaster");
        assert_eq!(d, "Automates things.");
    }

    #[test]
    fn parse_json_embedded_in_prose() {
        let raw = r#"I think {"name":"Signal Weaver","description":"Warm outreach."} fits."#;
        let (n, _) = parse_name_response(raw).unwrap();
        assert_eq!(n, "Signal Weaver");
    }

    #[test]
    fn parse_rejects_missing_fields() {
        assert!(parse_name_response(r#"{"name":"X"}"#).is_err());
        assert!(parse_name_response(r#"{"description":"Y"}"#).is_err());
    }

    #[test]
    fn parse_rejects_empty_strings() {
        assert!(parse_name_response(r#"{"name":"","description":"d"}"#).is_err());
        assert!(parse_name_response(r#"{"name":"n","description":""}"#).is_err());
    }

    #[test]
    fn parse_rejects_plain_text() {
        assert!(parse_name_response("sorry, I cannot").is_err());
    }

    #[test]
    fn extract_json_handles_strings_with_braces() {
        // Make sure `}` inside a string doesn't close the object early.
        let raw = r#"{"name":"n {}","description":"d"}"#;
        let (n, _) = parse_name_response(raw).unwrap();
        assert_eq!(n, "n {}");
    }

    #[test]
    fn extract_json_handles_escaped_quotes() {
        let raw = r#"{"name":"n\"q","description":"d"}"#;
        let (n, _) = parse_name_response(raw).unwrap();
        assert_eq!(n, "n\"q");
    }
}
