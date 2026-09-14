//! What a first run can find on this machine without asking.
//!
//! Filesystem, process environment and local-network probes only — no
//! database — so the TUI wizard and the daemon's `Admin.GetSetupStatus` share
//! one answer to "is there already something usable here?". Everything
//! returned is a *pointer* to a credential (a variable name, a file path, a
//! command path, an origin). The secret itself never leaves this module.

use super::provider::{ollama_origin, Provider};
use super::provider_catalog::ONBOARDABLE;
use std::path::Path;

/// Where a local credential was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalSource {
    /// A variable in this process's environment.
    Env,
    /// A line in `<data_dir>/.env`.
    Dotenv,
    /// The `claude` command is installed and signed in.
    ClaudeCli,
    /// An Ollama server answered on its port.
    Ollama,
}

impl LocalSource {
    /// Wire spelling, shared with the `DetectedCredential.source` field.
    pub fn as_str(self) -> &'static str {
        match self {
            LocalSource::Env => "env",
            LocalSource::Dotenv => "dotenv",
            LocalSource::ClaudeCli => "claude_cli",
            LocalSource::Ollama => "ollama",
        }
    }
}

/// One thing found. `detail` is safe to show: a variable name, a path, or
/// an origin — never the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalCredential {
    pub provider: Provider,
    pub source: LocalSource,
    pub detail: String,
}

/// Value of `var` in `<data_dir>/.env`, if set to something non-empty.
///
/// Tolerates `export ` prefixes, surrounding whitespace, single or double
/// quotes and `#` comment lines. The name must match exactly — `KEY=` must
/// not be satisfied by `MY_KEY=`.
pub fn dotenv_value(data_dir: &Path, var: &str) -> Option<String> {
    let contents = std::fs::read_to_string(data_dir.join(".env")).ok()?;
    contents.lines().find_map(|raw| {
        let line = raw.trim();
        if line.starts_with('#') {
            return None;
        }
        let line = line
            .strip_prefix("export ")
            .map(str::trim_start)
            .unwrap_or(line);
        let (name, value) = line.split_once('=')?;
        if name.trim() != var {
            return None;
        }
        let value = value.trim().trim_matches('"').trim_matches('\'').trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

/// Environment-variable and `.env` detections for every keyed provider the
/// first run offers. Cheap and synchronous; no network.
pub fn detect_keys(data_dir: &Path) -> Vec<LocalCredential> {
    let mut out = Vec::new();
    for choice in ONBOARDABLE {
        let provider = choice.provider();
        if !provider.requires_api_key() {
            continue;
        }
        let var = provider.default_env_var();
        if var.is_empty() {
            continue;
        }
        // The daemon loads `~/.borg/.env` into its own environment at
        // startup, so a key in that file shows up in both probes. Report the
        // one that explains where it came from, never the same key twice.
        if dotenv_value(data_dir, var).is_some() {
            out.push(LocalCredential {
                provider,
                source: LocalSource::Dotenv,
                detail: data_dir.join(".env").display().to_string(),
            });
        } else if std::env::var(var).is_ok_and(|v| !v.trim().is_empty()) {
            out.push(LocalCredential {
                provider,
                source: LocalSource::Env,
                detail: var.to_string(),
            });
        }
    }
    out
}

/// Keyless runtimes that are usable right now: a reachable Ollama server and
/// a signed-in Claude Code CLI.
///
/// Blocking — a TCP connect, a `which`, and on macOS a keychain subprocess.
/// The daemon runs this on a blocking thread.
pub fn detect_local_runtimes(ollama_base_url: Option<&str>) -> Vec<LocalCredential> {
    let mut out = Vec::new();
    if Provider::ollama_available_at(ollama_base_url) {
        out.push(LocalCredential {
            provider: Provider::Ollama,
            source: LocalSource::Ollama,
            detail: ollama_origin(ollama_base_url),
        });
    }
    if let Some(path) = crate::claude_cli::detect_cli_path() {
        if crate::claude_cli::has_valid_auth() {
            out.push(LocalCredential {
                provider: Provider::ClaudeCli,
                source: LocalSource::ClaudeCli,
                detail: path.display().to_string(),
            });
        }
    }
    out
}

/// The wizard's question: can `provider` be used without asking for a key?
///
/// The AWS chain is assumed present because probing it costs a network
/// round trip on every keypress of the wizard; a wrong assumption surfaces
/// on the first request with the SDK's own message.
pub fn provider_ready_locally(provider: Provider, data_dir: &Path) -> bool {
    if provider.uses_aws_credential_chain() {
        return true;
    }
    match provider {
        Provider::Ollama => Provider::ollama_available(),
        Provider::ClaudeCli => {
            crate::claude_cli::detect_cli_path().is_some() && crate::claude_cli::has_valid_auth()
        }
        p if !p.requires_api_key() => false,
        p => {
            let var = p.default_env_var();
            if var.is_empty() {
                return false;
            }
            // Same two sources the daemon accepts, in the same order, so the
            // wizard and the desktop app cannot disagree about whether this
            // machine already has a usable key.
            dotenv_value(data_dir, var).is_some()
                || std::env::var(var).is_ok_and(|v| !v.trim().is_empty())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes tests that touch the process environment.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn dir_with_env(contents: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(".env"), contents).expect("write .env");
        dir
    }

    #[test]
    fn dotenv_value_reads_the_shapes_people_actually_write() {
        let cases: &[(&str, Option<&str>)] = &[
            ("KEY=plain", Some("plain")),
            ("KEY=\"quoted\"", Some("quoted")),
            ("KEY='single'", Some("single")),
            ("export KEY=exported", Some("exported")),
            ("  KEY = spaced  ", Some("spaced")),
            ("KEY=", None),
            ("# KEY=commented", None),
            ("MY_KEY=other", None),
            ("KEYS=other", None),
            ("OTHER=x\nKEY=second", Some("second")),
        ];
        for (contents, want) in cases {
            let dir = dir_with_env(contents);
            assert_eq!(
                dotenv_value(dir.path(), "KEY").as_deref(),
                *want,
                "contents: {contents:?}"
            );
        }
        let empty = tempfile::tempdir().expect("tempdir");
        assert_eq!(dotenv_value(empty.path(), "KEY"), None, "no file, no value");
    }

    #[test]
    fn detect_keys_names_the_source_and_never_the_value() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Real provider variables, isolated: clear every one the catalog
        // could pick up so a developer's shell cannot leak into the assert.
        let vars: Vec<&str> = ONBOARDABLE
            .iter()
            .map(|c| c.provider().default_env_var())
            .filter(|v| !v.is_empty())
            .collect();
        let saved: Vec<(&str, Option<String>)> =
            vars.iter().map(|v| (*v, std::env::var(v).ok())).collect();
        for v in &vars {
            std::env::remove_var(v);
        }

        let dir = dir_with_env("GROQ_API_KEY=gsk-secret-value\n");
        // The daemon loads `.env` into its own environment, so this is what
        // a dotenv key looks like to `detect_keys` at runtime: both probes
        // see it, and it must still be reported once.
        std::env::set_var("GROQ_API_KEY", "gsk-secret-value");
        std::env::set_var("XAI_API_KEY", "xai-secret-value");
        let found = detect_keys(dir.path());

        for (v, old) in saved {
            match old {
                Some(val) => std::env::set_var(v, val),
                None => std::env::remove_var(v),
            }
        }

        let groq = found
            .iter()
            .find(|c| c.provider == Provider::Groq)
            .expect("groq found from .env");
        assert_eq!(groq.source, LocalSource::Dotenv);
        assert!(
            groq.detail.ends_with(".env"),
            "detail is the file: {}",
            groq.detail
        );
        let xai = found
            .iter()
            .find(|c| c.provider == Provider::XAi)
            .expect("xai found from env");
        assert_eq!(
            (xai.source, xai.detail.as_str()),
            (LocalSource::Env, "XAI_API_KEY")
        );
        assert_eq!(
            found
                .iter()
                .filter(|c| c.provider == Provider::Groq)
                .count(),
            1,
            "one key, reported once: {found:?}"
        );
        assert_eq!(found.len(), 2, "nothing else was set: {found:?}");
        for c in &found {
            assert!(!c.detail.contains("secret"), "value leaked: {c:?}");
        }
    }

    #[test]
    fn a_closed_port_is_not_a_running_ollama() {
        // Port 1 is never an Ollama server; a probe that reports it reachable
        // would tell a first run to skip the key step for nothing.
        let found = detect_local_runtimes(Some("http://127.0.0.1:1"));
        assert!(
            !found.iter().any(|c| c.source == LocalSource::Ollama),
            "closed port reported reachable: {found:?}"
        );
    }

    #[test]
    fn readiness_follows_the_provider_kind() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let saved = std::env::var("ANTHROPIC_API_KEY").ok();
        std::env::remove_var("ANTHROPIC_API_KEY");

        let dir = dir_with_env("OPENAI_API_KEY=sk-x\n");
        assert!(provider_ready_locally(Provider::Bedrock, dir.path()));
        assert!(provider_ready_locally(Provider::OpenAi, dir.path()));
        assert!(!provider_ready_locally(Provider::Anthropic, dir.path()));
        // An exported key counts here exactly as it counts for the daemon;
        // otherwise the wizard asks for a key the app says it already has.
        std::env::set_var("ANTHROPIC_API_KEY", "sk-exported");
        let exported = provider_ready_locally(Provider::Anthropic, dir.path());
        match saved {
            Some(v) => std::env::set_var("ANTHROPIC_API_KEY", v),
            None => std::env::remove_var("ANTHROPIC_API_KEY"),
        }
        assert!(exported, "an exported key is a usable key");

        // Ollama on a closed port is not ready, whatever .env says.
        std::env::set_var("OLLAMA_HOST", "127.0.0.1:1");
        let ready = provider_ready_locally(Provider::Ollama, dir.path());
        std::env::remove_var("OLLAMA_HOST");
        assert!(!ready);
    }
}
