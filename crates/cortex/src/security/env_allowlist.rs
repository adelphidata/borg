//! Env-var allowlist for agent-spawned subprocesses.
//!
//! `run_shell` and `pre_run_command` historically inherited the full daemon
//! env, which includes every provider API key the credential pool has loaded
//! plus any gateway secret in `borgd`'s environment. A subprocess that emits
//! `env` / `printenv` / `set -x` would leak those into stdout, which flows
//! back into the LLM context and the agent's reply — and then into
//! `~/.borg/logs/{date}.jsonl` and any channel relay.
//!
//! This module is the closed-set filter applied at every agent-spawned
//! subprocess site outside the sandbox crate (the sandbox already does its
//! own narrower `env_clear` + `SAFE_ENV_VARS` pass). It is intentionally
//! distinct from `crates/sandbox/src/constants.rs::SAFE_ENV_VARS` and from
//! `crates/cortex/src/security/skill.rs::SAFE_ENV_VARS` — they answer
//! different questions (sandbox jail env vs. skill-declared env vs. shell-
//! exec env) and must not be unified.
//!
//! Filter rules, in order:
//! 1. Explicit allowlist of known-safe names (categorized below).
//! 2. Prefix allowlist for variant-family vars (`LC_`, `XDG_`, build-script
//!    `CARGO_FEATURE_`, …).
//! 3. Defense-in-depth substring deny (`KEY`/`TOKEN`/`SECRET`/`PASSWORD`/
//!    `PASSWD`/`CREDENTIAL`/`AUTH`, case-insensitive). Applied AFTER the
//!    allow checks — a tripwire for refactor mistakes, not the main gate.
//!
//! Callers needing to forward a credential a skill explicitly declared pass
//! it via `extra_env` to [`apply_filtered_env`]; those bypass the deny rule
//! because they're an explicit opt-in via skill manifest.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// Explicit allowlist of env-var names that may be forwarded into an
/// agent-spawned subprocess. Grouped by category. Each entry must be
/// justified by a real workflow that `run_shell` legitimately needs — adding
/// a name here widens the trust surface, so any addition should come with a
/// rationale in the commit message.
const ALLOWED_NAMES: &[&str] = &[
    // -- Process basics: things any non-trivial shell needs. --
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TZ",
    "TMPDIR",
    "TEMP",
    "TMP",
    "PWD",
    "OLDPWD",
    "HOSTNAME",
    "HOSTTYPE",
    "MACHTYPE",
    "OSTYPE",
    "UID",
    "EUID",
    "GROUPS",
    "IFS",
    // -- Terminal / display: ncurses, color, X/Wayland for GUI subprocesses. --
    "TERM",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "TERM_SESSION_ID",
    "COLORTERM",
    "COLUMNS",
    "LINES",
    "LS_COLORS",
    "CLICOLOR",
    "CLICOLOR_FORCE",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    // -- Locale: LC_* family is also covered by ALLOWED_PREFIXES. --
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "LC_COLLATE",
    "LC_MESSAGES",
    "LC_MONETARY",
    "LC_NUMERIC",
    "LC_TIME",
    // -- Editor / pager. --
    "EDITOR",
    "VISUAL",
    "PAGER",
    "LESS",
    "MANPAGER",
    "MANPATH",
    "MANWIDTH",
    // -- Shell config. --
    "SHLVL",
    "PROMPT_COMMAND",
    "PS1",
    "PS2",
    "PS3",
    "PS4",
    "HISTFILE",
    "HISTSIZE",
    "HISTFILESIZE",
    "BASH_VERSION",
    "BASH_VERSINFO",
    "ZSH_VERSION",
    "ZDOTDIR",
    "USER_ZDOTDIR",
    // -- Rust toolchain: cargo invocations are the highest-frequency real use. --
    "RUST_BACKTRACE",
    "RUST_LOG",
    "RUST_LIB_BACKTRACE",
    "RUST_VERSION",
    "RUSTC",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "CARGO",
    "CARGO_HOME",
    "CARGO_TARGET_DIR",
    "CARGO_MANIFEST_DIR",
    "CARGO_PKG_VERSION",
    "CARGO_PKG_NAME",
    "CARGO_BUILD_JOBS",
    "CARGO_INCREMENTAL",
    "CARGO_NET_OFFLINE",
    "CARGO_TERM_COLOR",
    // -- Node / JS. --
    "NODE_ENV",
    "NODE_PATH",
    "NODE_OPTIONS",
    "NODE_VERSION",
    "NPM_CONFIG_PREFIX",
    "NPM_CONFIG_CACHE",
    "NPM_CONFIG_USERCONFIG",
    "NPM_CONFIG_LOGLEVEL",
    "NVM_DIR",
    "NVM_BIN",
    "NVM_INC",
    "YARN_CACHE_FOLDER",
    "PNPM_HOME",
    // -- Python. --
    "PYTHONPATH",
    "PYTHONHOME",
    "PYTHONUNBUFFERED",
    "PYTHONDONTWRITEBYTECODE",
    "PYTHONIOENCODING",
    "PYTHONBREAKPOINT",
    "PYTHONUSERBASE",
    "PIP_CACHE_DIR",
    "PIP_CONFIG_FILE",
    "PIP_DISABLE_PIP_VERSION_CHECK",
    "VIRTUAL_ENV",
    "VIRTUAL_ENV_PROMPT",
    "PIPX_HOME",
    "PIPX_BIN_DIR",
    "POETRY_HOME",
    "POETRY_VIRTUALENVS_PATH",
    "UV_CACHE_DIR",
    "CONDA_PREFIX",
    "CONDA_DEFAULT_ENV",
    "CONDA_PYTHON_EXE",
    "CONDA_SHLVL",
    "CONDA_EXE",
    // -- Go. --
    "GOPATH",
    "GOROOT",
    "GOBIN",
    "GOCACHE",
    "GOMODCACHE",
    "GOPROXY",
    "GOFLAGS",
    "GO111MODULE",
    "GOOS",
    "GOARCH",
    "GOVERSION",
    // -- Ruby. --
    "GEM_HOME",
    "GEM_PATH",
    "BUNDLE_PATH",
    "BUNDLE_GEMFILE",
    "RUBYOPT",
    "RBENV_VERSION",
    "RBENV_ROOT",
    // -- JVM / Java. --
    "JAVA_HOME",
    "JDK_HOME",
    "JRE_HOME",
    "JAVA_OPTS",
    "JAVA_TOOL_OPTIONS",
    "MAVEN_HOME",
    "M2_HOME",
    "GRADLE_HOME",
    "GRADLE_USER_HOME",
    // -- Build tools (compilers, linkers, pkg-config, cmake). --
    "MAKEFLAGS",
    "MAKELEVEL",
    "MFLAGS",
    "CC",
    "CXX",
    "CFLAGS",
    "CXXFLAGS",
    "LDFLAGS",
    "LD_LIBRARY_PATH",
    "LIBRARY_PATH",
    "INCLUDE_PATH",
    "PKG_CONFIG_PATH",
    "PKG_CONFIG_LIBDIR",
    "CMAKE_PREFIX_PATH",
    "CMAKE_BUILD_TYPE",
    "NINJA_STATUS",
    // -- Git (non-credential subset only). GIT_ASKPASS / *_TOKEN excluded. --
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_PAGER",
    "GIT_EDITOR",
    "GIT_TERMINAL_PROMPT",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_NOSYSTEM",
    // -- Docker / container (non-credential subset only). --
    "DOCKER_HOST",
    "DOCKER_BUILDKIT",
    "DOCKER_CONFIG",
    "DOCKER_DEFAULT_PLATFORM",
    "COMPOSE_FILE",
    "COMPOSE_PROJECT_NAME",
    "KUBECONFIG",
    "KUBE_CONFIG_PATH",
    // -- CI hints (harmless flags only). *_TOKEN explicitly excluded. --
    "CI",
    "CONTINUOUS_INTEGRATION",
    "GITHUB_ACTIONS",
    "RUNNER_OS",
    "RUNNER_ARCH",
    // -- macOS / Linux platform paths. --
    "XPC_FLAGS",
    "XPC_SERVICE_NAME",
    "__CF_USER_TEXT_ENCODING",
    "Apple_PubSub_Socket_Render",
    "SECURITYSESSIONID",
];

/// Prefix matches for env-var families where exhaustive enumeration is
/// fragile (new locale categories, build-script-generated cargo vars, …).
/// A name passes the prefix gate if it starts with any entry here AND the
/// substring deny rule does not match.
const ALLOWED_PREFIXES: &[&str] = &[
    "LC_",
    "XDG_",
    "CARGO_FEATURE_",
    "CARGO_BIN_EXE_",
    "CARGO_CFG_",
    // Test-only injection point; setting any production env var with this
    // prefix would be a misuse. Tests in this module rely on it.
    "__BORG_TEST_",
];

/// Substring patterns that, when present in a var name (case-insensitive),
/// override an allow decision. This is the tripwire layer — primary gate is
/// the allowlist. Entries are pre-lowercased so the matching path doesn't
/// allocate per call.
const DENY_SUBSTRINGS: &[&str] = &[
    "key",
    "token",
    "secret",
    "password",
    "passwd",
    "credential",
    "auth",
];

/// Allowlist entries that legitimately contain a deny substring but are
/// documented non-credentials. Adding to this list widens the trust surface
/// — every entry needs a one-line rationale in the comment below.
///
/// - `XAUTHORITY`: path to the X11 magic-cookie file. Required for any GUI
///   subprocess on Linux desktops; without it `xeyes`/`xclock` and friends
///   fail with `cannot open display`. The cookie file is already protected
///   by filesystem perms, so leaking the path is not the secret.
const ALLOW_OVERRIDES_DENY: &[&str] = &["XAUTHORITY"];

fn allowed_names_set() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| ALLOWED_NAMES.iter().copied().collect())
}

/// True iff `name` would be forwarded by the filter when no caller-supplied
/// extras are in play.
///
/// The check is layered: explicit allowlist OR prefix allowlist must accept,
/// then the substring deny must NOT match. Empty names are always denied —
/// the OS will never hand us one, but a malformed caller might.
pub fn is_allowed(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    // Explicit overrides bypass the deny tripwire but still require an
    // allowlist entry (override on its own does not constitute permission).
    if ALLOW_OVERRIDES_DENY.contains(&name) {
        return allowed_names_set().contains(name);
    }
    let on_allowlist =
        allowed_names_set().contains(name) || ALLOWED_PREFIXES.iter().any(|p| name.starts_with(p));
    if !on_allowlist {
        return false;
    }
    !contains_deny_substring(name)
}

fn contains_deny_substring(name: &str) -> bool {
    // Names are conventionally ASCII; lowercase once for the substring scan.
    // DENY_SUBSTRINGS is already lowercase so this is a single pass.
    let lowered = name.to_ascii_lowercase();
    DENY_SUBSTRINGS.iter().any(|s| lowered.contains(s))
}

/// Apply the filter to a pre-collected env iterator. Pulled out so unit
/// tests can exercise the rules without mutating the process env (which
/// would race with other parallel tests).
fn filter_iter<I>(iter: I, extra_allowed: &HashSet<String>) -> HashMap<String, String>
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut out = HashMap::new();
    for (k, v) in iter {
        if is_allowed(&k) || extra_allowed.contains(&k) {
            out.insert(k, v);
        }
    }
    out
}

/// Returns the filtered subset of the parent process env. `extra_allowed`
/// names bypass the filter — callers pass the union of skill-declared env
/// vars (`requires.env` from skill manifests) plus any explicit credentials
/// they intend to forward.
///
/// Non-UTF-8 keys or values are silently skipped: agent shell exec assumes
/// UTF-8 throughout, and a malformed env entry has no business reaching a
/// child anyway.
pub fn filter_parent_env(extra_allowed: &HashSet<String>) -> HashMap<String, String> {
    let env_pairs = std::env::vars_os().filter_map(|(k, v)| {
        let k = k.into_string().ok()?;
        let v = v.into_string().ok()?;
        Some((k, v))
    });
    filter_iter(env_pairs, extra_allowed)
}

/// `env_clear()` the command, then re-apply the filtered parent env and
/// overlay `extra_env`. `extra_env` is the single source for skill-declared
/// credentials — those names are excluded from the parent-env pass so a
/// caller-controlled value is never accidentally shadowed by an ambient
/// parent-env entry of the same name.
pub fn apply_filtered_env(cmd: &mut tokio::process::Command, extra_env: &HashMap<String, String>) {
    cmd.env_clear();
    for (k, v) in filter_parent_env(&HashSet::new()) {
        if !extra_env.contains_key(&k) {
            cmd.env(k, v);
        }
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
}

/// Build a HashMap of env vars suitable for a `portable_pty::CommandBuilder`.
/// Same semantics as [`apply_filtered_env`] but returns the map instead of
/// mutating a `Command`, since `CommandBuilder` has a different API surface.
pub fn build_filtered_env(extra_env: &HashMap<String, String>) -> HashMap<String, String> {
    let mut env = filter_parent_env(&HashSet::new());
    for (k, v) in extra_env {
        env.insert(k.clone(), v.clone());
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- is_allowed: positive coverage ----

    #[test]
    fn allows_process_basics() {
        // If a refactor strips PATH/HOME/SHELL/USER, every spawned shell
        // breaks — these are the irreducible floor.
        for name in ["PATH", "HOME", "SHELL", "USER", "TERM", "TZ"] {
            assert!(is_allowed(name), "expected {name} to be allowed");
        }
    }

    #[test]
    fn allows_xauthority_despite_auth_substring() {
        // Regression: `XAUTHORITY` is on the allowlist (X11 GUI forwarding
        // needs it) but contains "AUTH". Without ALLOW_OVERRIDES_DENY the
        // tripwire would silently strip it and break every X subprocess.
        assert!(is_allowed("XAUTHORITY"));
        // Sanity check that the override is narrow: it must NOT relax the
        // deny rule for similarly-named-but-uninvited variables.
        assert!(!is_allowed("XAUTH"));
        assert!(!is_allowed("XAUTHORITY_FAKE"));
    }

    #[test]
    fn no_allowlist_entry_is_silently_shadowed_by_deny() {
        // Cross-table invariant: an explicit-list entry caught by the
        // deny substring would be a silent leak of intent — someone added
        // it because they wanted it forwarded. Either the name needs an
        // ALLOW_OVERRIDES_DENY entry with a rationale, or the name should
        // not be on the allowlist at all. This test catches the next
        // XAUTHORITY-shaped mistake at PR time.
        let shadowed: Vec<&&str> = ALLOWED_NAMES
            .iter()
            .filter(|name| contains_deny_substring(name))
            .filter(|name| !ALLOW_OVERRIDES_DENY.contains(name))
            .collect();
        assert!(
            shadowed.is_empty(),
            "allowlist entries silently denied by substring rule: {shadowed:?} — \
             add to ALLOW_OVERRIDES_DENY with a rationale, or remove from ALLOWED_NAMES"
        );
    }

    #[test]
    fn allows_via_prefix_rule() {
        // Names deliberately NOT in the explicit list, to prove the prefix
        // arm of the filter actually runs.
        for name in [
            "LC_PAPER",
            "LC_ADDRESS",
            "XDG_CONFIG_HOME",
            "XDG_RUNTIME_DIR",
            "CARGO_FEATURE_DEFAULT",
            "CARGO_CFG_TARGET_OS",
        ] {
            assert!(
                !ALLOWED_NAMES.contains(&name),
                "test setup bug: {name} is on the explicit list; pick a different name"
            );
            assert!(is_allowed(name), "expected prefix rule to allow {name}");
        }
    }

    // ---- is_allowed: negative coverage (the actual leak regression) ----

    #[test]
    fn denies_provider_api_keys() {
        // Every provider key borg's credential pool may load. If any of
        // these returns true, the leak fix has been undone.
        for name in [
            "OPENROUTER_API_KEY",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_API_KEY_OLD",
            "ANTHROPIC_TOKEN",
            "OPENAI_API_KEY",
            "OPENAI_ORGANIZATION",
            "GEMINI_API_KEY",
            "DEEPSEEK_API_KEY",
            "GROQ_API_KEY",
            "XAI_API_KEY",
            "AZURE_OPENAI_API_KEY",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "COHERE_API_KEY",
            "FIREWORKS_API_KEY",
            "MISTRAL_API_KEY",
            "TOGETHER_API_KEY",
            "PERPLEXITY_API_KEY",
            "GOOGLE_API_KEY",
        ] {
            assert!(!is_allowed(name), "provider key {name} leaked through");
        }
    }

    #[test]
    fn denies_gateway_and_channel_secrets() {
        for name in [
            "SLACK_BOT_TOKEN",
            "SLACK_SIGNING_SECRET",
            "TELEGRAM_BOT_TOKEN",
            "DISCORD_BOT_TOKEN",
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "GITHUB_APP_PRIVATE_KEY_PATH",
            "GIT_ASKPASS",
            "TWILIO_AUTH_TOKEN",
            "SIGNAL_HTTP_URL",
            "EMAIL_PASSWORD",
        ] {
            assert!(!is_allowed(name), "gateway secret {name} leaked through");
        }
    }

    #[test]
    fn substring_deny_trips_on_arbitrary_names() {
        // None of these are on the explicit list, none would pass the
        // prefix rule. They're denied via the allowlist gate. But also
        // verify the deny words themselves catch matching shapes when
        // overlaid on a hypothetically-allowed name (see overrides test).
        for name in [
            "MY_CUSTOM_KEY",
            "DEPLOY_TOKEN",
            "FOO_SECRET",
            "BAR_PASSWORD",
            "AUTH_HEADER",
            "BAZ_CREDENTIAL",
            "ROOT_PASSWD",
        ] {
            assert!(!is_allowed(name), "{name} should be denied");
        }
    }

    #[test]
    fn substring_deny_overrides_prefix_allow() {
        // A name that passes the prefix rule but matches a deny substring
        // must still be rejected. This is the actual tripwire — verifies
        // the deny check runs AFTER the allow check, not before.
        for name in [
            "__BORG_TEST_API_KEY",
            "__BORG_TEST_FAKE_TOKEN",
            "XDG_AUTH_DIR",
            "LC_SECRET_LOCALE",
            "CARGO_FEATURE_TOKEN_FOO",
        ] {
            // Sanity: prove the prefix WOULD allow if not for the deny.
            assert!(
                ALLOWED_PREFIXES.iter().any(|p| name.starts_with(p)),
                "test setup bug: {name} doesn't start with any allowed prefix"
            );
            assert!(!is_allowed(name), "deny tripwire missed {name}");
        }
    }

    #[test]
    fn substring_deny_is_case_insensitive() {
        // Real env names are conventionally uppercase, but a contributor
        // might add a mixed-case entry; lowercase comparison protects us.
        for name in [
            "my_token",
            "Some_Secret_Var",
            "lowercase_password",
            "MixedCase_Auth_Var",
        ] {
            assert!(!is_allowed(name), "case-insensitive deny missed {name}");
        }
    }

    #[test]
    fn empty_name_denied() {
        assert!(!is_allowed(""));
    }

    #[test]
    fn unknown_name_denied_even_when_innocuous() {
        // Important: an unfamiliar name is denied by default. Skill
        // manifests are the documented escape hatch.
        for name in [
            "MY_HARMLESS_THING",
            "PROJECT_ROOT",
            "WORKSPACE_NAME",
            "RANDOM_NONSENSE_XYZZY",
        ] {
            assert!(!is_allowed(name), "{name} should require explicit opt-in");
        }
    }

    // ---- filter_iter: the pure unit. ----

    #[test]
    fn filter_keeps_allowed_drops_rest() {
        let pairs = vec![
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("HOME".to_string(), "/home/u".to_string()),
            ("OPENROUTER_API_KEY".to_string(), "sk-leak".to_string()),
            ("ANTHROPIC_API_KEY".to_string(), "sk-leak2".to_string()),
            ("SOMETHING_RANDOM".to_string(), "x".to_string()),
            ("XDG_CONFIG_HOME".to_string(), "/x".to_string()),
        ];
        let out = filter_iter(pairs.clone(), &HashSet::new());

        assert_eq!(out.get("PATH").map(String::as_str), Some("/usr/bin"));
        assert_eq!(out.get("HOME").map(String::as_str), Some("/home/u"));
        assert_eq!(out.get("XDG_CONFIG_HOME").map(String::as_str), Some("/x"));
        assert!(!out.contains_key("OPENROUTER_API_KEY"));
        assert!(!out.contains_key("ANTHROPIC_API_KEY"));
        assert!(!out.contains_key("SOMETHING_RANDOM"));
    }

    #[test]
    fn filter_extras_bypass_deny_for_skill_declared_creds() {
        // A skill that declares `requires.env = ["MY_SKILL_TOKEN"]` and
        // is approved via [skills.<name>] allowed_env must be able to
        // forward that var even though "TOKEN" is in the deny list.
        let pairs = vec![("MY_SKILL_TOKEN".to_string(), "hunter2".to_string())];
        let mut extras = HashSet::new();
        extras.insert("MY_SKILL_TOKEN".to_string());

        let out = filter_iter(pairs.clone(), &extras);
        assert_eq!(
            out.get("MY_SKILL_TOKEN").map(String::as_str),
            Some("hunter2")
        );

        // Without the extras opt-in, same input is denied.
        let out_no_extras = filter_iter(pairs, &HashSet::new());
        assert!(!out_no_extras.contains_key("MY_SKILL_TOKEN"));
    }

    #[test]
    fn filter_drops_known_secrets_even_when_extras_unrelated() {
        // Extras should opt in *specific* names, not loosen the filter
        // globally. A skill declaring FOO must not accidentally let
        // ANTHROPIC_API_KEY through.
        let pairs = vec![
            ("FOO".to_string(), "bar".to_string()),
            ("ANTHROPIC_API_KEY".to_string(), "leak".to_string()),
        ];
        let mut extras = HashSet::new();
        extras.insert("FOO".to_string());

        let out = filter_iter(pairs, &extras);
        assert_eq!(out.get("FOO").map(String::as_str), Some("bar"));
        assert!(
            !out.contains_key("ANTHROPIC_API_KEY"),
            "extras opt-in must be name-specific, not global"
        );
    }

    // ---- End-to-end: actually spawn a subprocess. ----
    //
    // This is the single most important test in the file: it's the only
    // one that catches a regression where someone accidentally forgets
    // env_clear() but the unit tests still pass.

    #[tokio::test]
    async fn apply_filtered_env_to_real_subprocess_strips_secrets() {
        // Use names that won't collide with any other parallel test in
        // this crate, and that are themselves NOT on the allowlist (so
        // the test result depends on the filter, not coincidence).
        let leak_name = "BORG_TEST_LEAK_FAKE_KEY_DO_NOT_FORWARD";
        let leak_value = "this-must-not-appear-in-child-stdout";
        let extra_name = "BORG_TEST_E2E_SKILL_TOKEN";
        let extra_value = "skill-supplied-value";

        // Pre-condition: leak_name passes the substring deny (contains
        // "KEY"), so even if a future contributor adds BORG_TEST_* to
        // the allowlist, this test still verifies the deny path.
        assert!(!is_allowed(leak_name), "test invariant broken");

        // SAFETY: env mutation is unsafe in multi-threaded Rust 2024;
        // unique names keep us from clobbering anyone else.
        unsafe {
            std::env::set_var(leak_name, leak_value);
        }

        let mut extras = HashMap::new();
        extras.insert(extra_name.to_string(), extra_value.to_string());

        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg("env");
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        apply_filtered_env(&mut cmd, &extras);

        let output = cmd.output().await.expect("spawn sh failed");

        // Cleanup before assertions so a panic doesn't leak the var.
        unsafe {
            std::env::remove_var(leak_name);
        }

        let stdout = std::str::from_utf8(&output.stdout).expect("sh wrote non-UTF-8");

        assert!(
            stdout.contains("PATH="),
            "PATH must be forwarded for sh to be usable; child env was:\n{stdout}"
        );
        assert!(
            !stdout.contains(leak_value),
            "leak_value appeared in child env — env_clear or filter is broken:\n{stdout}"
        );
        assert!(
            !stdout.contains(leak_name),
            "leak_name appeared in child env — env_clear or filter is broken:\n{stdout}"
        );
        assert!(
            stdout.contains(&format!("{extra_name}={extra_value}")),
            "extras must override the filter; child env was:\n{stdout}"
        );
    }
}
