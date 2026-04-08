//! Archetype classification for tool calls and shell commands.

use super::Archetype;

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
];

const BUILDER_KEYWORDS: &[&str] = &[
    "cargo", "npm", "pip", "gcc", "make", "build", "compile", "lint", "rustc", "webpack", "vite",
    "esbuild",
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
];

const STRATEGIST_KEYWORDS: &[&str] = &[
    "plan",
    "prioritize",
    "compare",
    "evaluate",
    "decision",
    "roadmap",
    "okr",
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
