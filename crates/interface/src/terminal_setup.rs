//! Install IDE terminal keybindings for VS Code / Cursor / Windsurf so borg's
//! TUI receives `Shift+Enter`, `Cmd+Enter`, `Cmd+Z`, `Shift+Cmd+Z` (and on macOS
//! `Cmd+C` when text is selected) as CSI-u sequences rather than having the IDE
//! swallow or repurpose them.
//!
//! Pure-logic core: detection, JSONC stripping, when-clause overlap analysis,
//! conflict detection, and binding merge run against an injected [`FileOps`] and
//! [`EnvLookup`] so the entire surface is unit-testable without touching the
//! real filesystem or process environment. Production uses [`RealFileOps`] +
//! [`ProcessEnv`].

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupportedTerminal {
    Vscode,
    Cursor,
    Windsurf,
}

impl SupportedTerminal {
    fn app_name(self) -> &'static str {
        match self {
            Self::Vscode => "Code",
            Self::Cursor => "Cursor",
            Self::Windsurf => "Windsurf",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Vscode => "VS Code",
            Self::Cursor => "Cursor",
            Self::Windsurf => "Windsurf",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Os {
    Darwin,
    Linux,
    Windows,
    Other,
}

impl Os {
    pub fn current() -> Self {
        match std::env::consts::OS {
            "macos" => Self::Darwin,
            "linux" => Self::Linux,
            "windows" => Self::Windows,
            _ => Self::Other,
        }
    }
}

#[derive(Debug)]
pub struct TerminalSetupResult {
    pub success: bool,
    pub requires_restart: bool,
    pub message: String,
}

pub trait EnvLookup {
    fn get(&self, key: &str) -> Option<String>;
}

pub struct ProcessEnv;

impl EnvLookup for ProcessEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

pub trait FileOps {
    fn mkdir_p(&self, path: &Path) -> io::Result<()>;
    fn read_to_string(&self, path: &Path) -> io::Result<String>;
    fn write(&self, path: &Path, contents: &str) -> io::Result<()>;
    fn copy(&self, from: &Path, to: &Path) -> io::Result<u64>;
}

pub struct RealFileOps;

impl FileOps for RealFileOps {
    fn mkdir_p(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }
    fn write(&self, path: &Path, contents: &str) -> io::Result<()> {
        std::fs::write(path, contents)
    }
    fn copy(&self, from: &Path, to: &Path) -> io::Result<u64> {
        std::fs::copy(from, to)
    }
}

// CSI-u sequences the TUI's input decoder expects. Keep these byte-identical
// to the values written by older clients so we don't churn user IDE configs.
const COPY_SEQUENCE: &str = "\u{1b}[99;13u";
const MULTILINE_SEQUENCE: &str = "\\\r\n";
const UNDO_SEQUENCE: &str = "\u{1b}[122;9u";
const REDO_SEQUENCE: &str = "\u{1b}[122;10u";

const SEND_SEQUENCE_COMMAND: &str = "workbench.action.terminal.sendSequence";

/// Logical view of a keybinding for comparison. We keep the full JSON [`Value`]
/// alongside so unknown user fields round-trip on disk untouched.
#[derive(Clone, Debug)]
struct Keybinding {
    key: String,
    command: String,
    when: String,
    args_text: String,
}

impl Keybinding {
    fn to_value(&self) -> Value {
        json!({
            "key": self.key,
            "command": self.command,
            "when": self.when,
            "args": { "text": self.args_text },
        })
    }

    fn from_value(v: &Value) -> Option<Self> {
        let obj = v.as_object()?;
        Some(Self {
            key: obj.get("key").and_then(Value::as_str)?.to_string(),
            command: obj
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            when: obj
                .get("when")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            args_text: obj
                .get("args")
                .and_then(|a| a.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        })
    }

    fn same(&self, other: &Self) -> bool {
        self.key == other.key
            && self.command == other.command
            && self.when == other.when
            && self.args_text == other.args_text
    }
}

fn target_bindings(os: Os) -> Vec<Keybinding> {
    let mut out: Vec<Keybinding> = Vec::new();

    if os == Os::Darwin {
        out.push(Keybinding {
            key: "cmd+c".into(),
            command: SEND_SEQUENCE_COMMAND.into(),
            when: "terminalFocus && terminalTextSelected".into(),
            args_text: COPY_SEQUENCE.into(),
        });
    }

    for key in ["shift+enter", "ctrl+enter", "cmd+enter"] {
        out.push(Keybinding {
            key: key.into(),
            command: SEND_SEQUENCE_COMMAND.into(),
            when: "terminalFocus".into(),
            args_text: MULTILINE_SEQUENCE.into(),
        });
    }

    out.push(Keybinding {
        key: "cmd+z".into(),
        command: SEND_SEQUENCE_COMMAND.into(),
        when: "terminalFocus".into(),
        args_text: UNDO_SEQUENCE.into(),
    });
    out.push(Keybinding {
        key: "shift+cmd+z".into(),
        command: SEND_SEQUENCE_COMMAND.into(),
        when: "terminalFocus".into(),
        args_text: REDO_SEQUENCE.into(),
    });

    out
}

pub fn detect_vscode_like_terminal<E: EnvLookup>(env: &E) -> Option<SupportedTerminal> {
    let askpass = env
        .get("VSCODE_GIT_ASKPASS_MAIN")
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();

    if env.get("CURSOR_TRACE_ID").is_some() || askpass.contains("cursor") {
        return Some(SupportedTerminal::Cursor);
    }

    if askpass.contains("windsurf") {
        return Some(SupportedTerminal::Windsurf);
    }

    if env.get("TERM_PROGRAM").as_deref() == Some("vscode")
        || env.get("VSCODE_GIT_IPC_HANDLE").is_some()
    {
        return Some(SupportedTerminal::Vscode);
    }

    None
}

pub fn is_remote_shell_session<E: EnvLookup>(env: &E) -> bool {
    env.get("SSH_CONNECTION").is_some()
        || env.get("SSH_TTY").is_some()
        || env.get("SSH_CLIENT").is_some()
}

pub fn vscode_style_config_dir<E: EnvLookup>(
    app_name: &str,
    os: Os,
    env: &E,
    home: &Path,
) -> Option<PathBuf> {
    match os {
        Os::Darwin => Some(
            home.join("Library")
                .join("Application Support")
                .join(app_name)
                .join("User"),
        ),
        Os::Windows => env
            .get("APPDATA")
            .map(|appdata| PathBuf::from(appdata).join(app_name).join("User")),
        Os::Linux | Os::Other => Some(home.join(".config").join(app_name).join("User")),
    }
}

/// Strip JSONC features (`// line`, `/* block */`, trailing commas) so the
/// result parses with `serde_json::from_str`. Preserves comment-like substrings
/// inside string literals and handles unterminated block comments (consumes
/// to EOF).
pub fn strip_jsonc(input: &str) -> String {
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut i = 0;

    while i < len {
        let ch = bytes[i];

        if ch == b'"' {
            let start = i;
            i += 1;
            while i < len {
                if bytes[i] == b'\\' && i + 1 < len {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push_str(&input[start..i]);
            continue;
        }

        if ch == b'/' && i + 1 < len && bytes[i + 1] == b'/' {
            // Line comment: consume to '\n' (but do NOT consume the newline
            // — the caller relies on line breaks staying in the stripped
            // output so subsequent line-based parsing still aligns).
            match input[i..].find('\n') {
                Some(rel) => i += rel,
                None => i = len,
            }
            continue;
        }

        if ch == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
            // Block comment: consume to '*/' or EOF.
            match input[i + 2..].find("*/") {
                Some(rel) => i = i + 2 + rel + 2,
                None => i = len,
            }
            continue;
        }

        out.push(ch as char);
        i += 1;
    }

    // Trailing commas before `}` or `]`.
    let mut cleaned = String::with_capacity(out.len());
    let bytes = out.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b',' {
            // Look ahead past whitespace.
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b'}' || bytes[j] == b']') {
                // Skip the comma; the whitespace is preserved by continuing
                // the outer loop at i+1.
                i += 1;
                continue;
            }
        }
        cleaned.push(bytes[i] as char);
        i += 1;
    }
    cleaned
}

#[derive(Debug)]
struct WhenRequirements {
    required: HashSet<String>,
    forbidden: HashSet<String>,
}

fn parse_when_requirements(when: &str) -> WhenRequirements {
    let mut required = HashSet::new();
    let mut forbidden = HashSet::new();

    // Tokens: optional leading `!`, then identifier consisting of ASCII
    // alphanumerics / `_` / `.`. The reference impl uses /!?[A-Za-z_][\w.]*/.
    let bytes = when.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let negated = bytes[i] == b'!';
        let id_start = if negated { i + 1 } else { i };
        if id_start >= bytes.len() {
            i += 1;
            continue;
        }
        let c0 = bytes[id_start];
        if !(c0.is_ascii_alphabetic() || c0 == b'_') {
            i += 1;
            continue;
        }
        let mut j = id_start + 1;
        while j < bytes.len() {
            let c = bytes[j];
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' {
                j += 1;
            } else {
                break;
            }
        }
        let token = when[id_start..j].to_string();
        if negated {
            forbidden.insert(token);
        } else {
            required.insert(token);
        }
        i = j;
    }

    WhenRequirements {
        required,
        forbidden,
    }
}

fn requirements_contradict(a: &WhenRequirements, b: &WhenRequirements) -> bool {
    a.required.iter().any(|t| b.forbidden.contains(t))
        || b.required.iter().any(|t| a.forbidden.contains(t))
}

fn whens_overlap(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    // Empty when = global, overlaps every context.
    if a.is_empty() || b.is_empty() {
        return true;
    }
    let left = parse_when_requirements(a);
    let right = parse_when_requirements(b);
    if requirements_contradict(&left, &right) {
        return false;
    }
    left.required.iter().any(|t| right.required.contains(t))
}

fn bindings_conflict(existing: &Keybinding, target: &Keybinding) -> bool {
    if existing.key != target.key {
        return false;
    }
    if !whens_overlap(&existing.when, &target.when) {
        return false;
    }
    !existing.same(target)
}

fn sanitize_iso_for_filename(iso: &str) -> String {
    iso.replace([':', '.'], "-")
}

pub fn configure_terminal_keybindings<E: EnvLookup>(
    terminal: SupportedTerminal,
    os: Os,
    env: &E,
    home: &Path,
    fs: &dyn FileOps,
    now_iso: &str,
) -> TerminalSetupResult {
    if is_remote_shell_session(env) {
        return TerminalSetupResult {
            success: false,
            requires_restart: false,
            message: format!(
                "{} terminal setup must be run on the local machine, not inside an SSH session.",
                terminal.label()
            ),
        };
    }

    let config_dir = match vscode_style_config_dir(terminal.app_name(), os, env, home) {
        Some(p) => p,
        None => {
            return TerminalSetupResult {
                success: false,
                requires_restart: false,
                message: format!(
                    "Could not determine {} settings path on this platform.",
                    terminal.label()
                ),
            };
        }
    };

    let keybindings_file = config_dir.join("keybindings.json");

    if let Err(e) = fs.mkdir_p(&config_dir) {
        return TerminalSetupResult {
            success: false,
            requires_restart: false,
            message: format!("Failed to create {} settings dir: {e}", terminal.label()),
        };
    }

    let (mut existing_array, has_existing_file) = match fs.read_to_string(&keybindings_file) {
        Ok(content) => match serde_json::from_str::<Value>(&strip_jsonc(&content)) {
            Ok(Value::Array(arr)) => (arr, true),
            Ok(_) => {
                return TerminalSetupResult {
                    success: false,
                    requires_restart: false,
                    message: format!(
                        "{} keybindings.json is not a JSON array: {}",
                        terminal.label(),
                        keybindings_file.display()
                    ),
                };
            }
            Err(e) => {
                return TerminalSetupResult {
                    success: false,
                    requires_restart: false,
                    message: format!("Failed to parse {} keybindings: {e}", terminal.label()),
                };
            }
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => (Vec::new(), false),
        Err(e) => {
            return TerminalSetupResult {
                success: false,
                requires_restart: false,
                message: format!("Failed to read {} keybindings: {e}", terminal.label()),
            };
        }
    };

    let targets = target_bindings(os);

    let existing_typed: Vec<Keybinding> = existing_array
        .iter()
        .filter_map(Keybinding::from_value)
        .collect();

    let conflicts: Vec<&Keybinding> = targets
        .iter()
        .filter(|t| existing_typed.iter().any(|e| bindings_conflict(e, t)))
        .collect();

    if !conflicts.is_empty() {
        let names: Vec<&str> = conflicts.iter().map(|c| c.key.as_str()).collect();
        return TerminalSetupResult {
            success: false,
            requires_restart: false,
            message: format!(
                "Existing terminal keybindings would conflict in {}: {}",
                keybindings_file.display(),
                names.join(", ")
            ),
        };
    }

    // Determine which targets need to be inserted. Iterate in reverse and
    // prepend so the final order in the file matches `targets` (borg's
    // bindings come before user bindings on the same key).
    let mut added = 0u32;
    for target in targets.iter().rev() {
        let already = existing_typed.iter().any(|e| e.same(target));
        if !already {
            existing_array.insert(0, target.to_value());
            added += 1;
        }
    }

    if added == 0 {
        return TerminalSetupResult {
            success: true,
            requires_restart: false,
            message: format!(
                "{} terminal keybindings already configured.",
                terminal.label()
            ),
        };
    }

    if has_existing_file {
        let backup_path = PathBuf::from(format!(
            "{}.backup.{}",
            keybindings_file.display(),
            sanitize_iso_for_filename(now_iso)
        ));
        if let Err(e) = fs.copy(&keybindings_file, &backup_path) {
            return TerminalSetupResult {
                success: false,
                requires_restart: false,
                message: format!(
                    "Failed to back up {} keybindings before writing: {e}",
                    terminal.label()
                ),
            };
        }
    }

    let pretty = match serde_json::to_string_pretty(&Value::Array(existing_array)) {
        Ok(s) => format!("{s}\n"),
        Err(e) => {
            return TerminalSetupResult {
                success: false,
                requires_restart: false,
                message: format!("Failed to serialize keybindings: {e}"),
            };
        }
    };

    if let Err(e) = fs.write(&keybindings_file, &pretty) {
        return TerminalSetupResult {
            success: false,
            requires_restart: false,
            message: format!("Failed to write {} keybindings: {e}", terminal.label()),
        };
    }

    let suffix = if added == 1 { "" } else { "s" };
    TerminalSetupResult {
        success: true,
        requires_restart: true,
        message: format!(
            "Added {added} {} terminal keybinding{suffix} in {}",
            terminal.label(),
            keybindings_file.display()
        ),
    }
}

pub fn configure_detected_terminal_keybindings<E: EnvLookup>(
    os: Os,
    env: &E,
    home: &Path,
    fs: &dyn FileOps,
    now_iso: &str,
) -> TerminalSetupResult {
    match detect_vscode_like_terminal(env) {
        Some(t) => configure_terminal_keybindings(t, os, env, home, fs, now_iso),
        None => TerminalSetupResult {
            success: false,
            requires_restart: false,
            message: "No supported IDE terminal detected. Supported: VS Code, Cursor, Windsurf."
                .to_string(),
        },
    }
}

// Exposed for future onboarding / heartbeat nudges — no caller in tree yet,
// but the dedicated unit tests below pin its semantics so a wire-up is safe.
#[allow(dead_code)]
pub fn should_prompt_for_terminal_setup<E: EnvLookup>(
    os: Os,
    env: &E,
    home: &Path,
    fs: &dyn FileOps,
) -> bool {
    let Some(terminal) = detect_vscode_like_terminal(env) else {
        return false;
    };
    if is_remote_shell_session(env) {
        return false;
    }
    let Some(config_dir) = vscode_style_config_dir(terminal.app_name(), os, env, home) else {
        return false;
    };
    let path = config_dir.join("keybindings.json");
    let content = match fs.read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return true,
    };
    let parsed = match serde_json::from_str::<Value>(&strip_jsonc(&content)) {
        Ok(v) => v,
        Err(_) => return true,
    };
    let Some(arr) = parsed.as_array() else {
        return true;
    };
    let existing: Vec<Keybinding> = arr.iter().filter_map(Keybinding::from_value).collect();
    target_bindings(os)
        .iter()
        .any(|t| !existing.iter().any(|e| e.same(t)))
}

/// Parse the slash-command argument into an optional explicit target.
/// `""` / `"auto"` → `Ok(None)` (auto-detect). `"vscode"` / `"cursor"` /
/// `"windsurf"` → `Ok(Some(...))`. Anything else returns `Err(arg)` so the
/// caller can render a usage message naming the bad token.
pub fn parse_terminal_setup_target(arg: &str) -> Result<Option<SupportedTerminal>, String> {
    let token = arg.trim().to_ascii_lowercase();
    match token.as_str() {
        "" | "auto" => Ok(None),
        "vscode" => Ok(Some(SupportedTerminal::Vscode)),
        "cursor" => Ok(Some(SupportedTerminal::Cursor)),
        "windsurf" => Ok(Some(SupportedTerminal::Windsurf)),
        _ => Err(arg.trim().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    struct MapEnv(HashMap<String, String>);

    impl MapEnv {
        fn new() -> Self {
            Self(HashMap::new())
        }
        fn with(mut self, k: &str, v: &str) -> Self {
            self.0.insert(k.to_string(), v.to_string());
            self
        }
    }

    impl EnvLookup for MapEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    #[derive(Default)]
    struct MockFs {
        files: RefCell<HashMap<PathBuf, String>>,
        dirs: RefCell<Vec<PathBuf>>,
        copies: RefCell<Vec<(PathBuf, PathBuf)>>,
        // If set, read_to_string returns this error for any path.
        force_read_error: RefCell<Option<io::ErrorKind>>,
    }

    impl MockFs {
        fn with_file(self, path: &Path, contents: &str) -> Self {
            self.files
                .borrow_mut()
                .insert(path.to_path_buf(), contents.to_string());
            self
        }
        fn write_count(&self) -> usize {
            // Every successful `write` overwrites the entry; we count by
            // distinct write events tracked via a side log instead.
            // Simpler: report total files that have content (post-test).
            self.files.borrow().len()
        }
        fn writes(&self) -> HashMap<PathBuf, String> {
            self.files.borrow().clone()
        }
        fn copies_made(&self) -> Vec<(PathBuf, PathBuf)> {
            self.copies.borrow().clone()
        }
    }

    impl FileOps for MockFs {
        fn mkdir_p(&self, path: &Path) -> io::Result<()> {
            self.dirs.borrow_mut().push(path.to_path_buf());
            Ok(())
        }
        fn read_to_string(&self, path: &Path) -> io::Result<String> {
            if let Some(kind) = *self.force_read_error.borrow() {
                return Err(io::Error::new(kind, "forced"));
            }
            match self.files.borrow().get(path) {
                Some(s) => Ok(s.clone()),
                None => Err(io::Error::new(io::ErrorKind::NotFound, "missing")),
            }
        }
        fn write(&self, path: &Path, contents: &str) -> io::Result<()> {
            self.files
                .borrow_mut()
                .insert(path.to_path_buf(), contents.to_string());
            Ok(())
        }
        fn copy(&self, from: &Path, to: &Path) -> io::Result<u64> {
            self.copies
                .borrow_mut()
                .push((from.to_path_buf(), to.to_path_buf()));
            // Mirror the file contents into the destination so subsequent
            // reads of either path remain coherent.
            let content = self.files.borrow().get(from).cloned();
            if let Some(content) = content {
                self.files.borrow_mut().insert(to.to_path_buf(), content);
            }
            Ok(0)
        }
    }

    fn fixed_iso() -> &'static str {
        "2026-05-24T12:34:56.789Z"
    }

    #[test]
    fn detect_returns_cursor_when_cursor_trace_id_set() {
        let env = MapEnv::new().with("CURSOR_TRACE_ID", "abc");
        assert_eq!(
            detect_vscode_like_terminal(&env),
            Some(SupportedTerminal::Cursor)
        );
    }

    #[test]
    fn detect_returns_cursor_from_askpass_case_insensitively() {
        let env = MapEnv::new().with("VSCODE_GIT_ASKPASS_MAIN", "/Applications/Cursor.app/...");
        assert_eq!(
            detect_vscode_like_terminal(&env),
            Some(SupportedTerminal::Cursor)
        );
    }

    #[test]
    fn detect_returns_windsurf_from_askpass() {
        let env = MapEnv::new().with("VSCODE_GIT_ASKPASS_MAIN", "/tmp/Windsurf-askpass");
        assert_eq!(
            detect_vscode_like_terminal(&env),
            Some(SupportedTerminal::Windsurf)
        );
    }

    #[test]
    fn detect_returns_vscode_from_term_program() {
        let env = MapEnv::new().with("TERM_PROGRAM", "vscode");
        assert_eq!(
            detect_vscode_like_terminal(&env),
            Some(SupportedTerminal::Vscode)
        );
    }

    #[test]
    fn detect_returns_vscode_from_ipc_handle() {
        let env = MapEnv::new().with("VSCODE_GIT_IPC_HANDLE", "/tmp/sock");
        assert_eq!(
            detect_vscode_like_terminal(&env),
            Some(SupportedTerminal::Vscode)
        );
    }

    #[test]
    fn detect_returns_none_for_empty_env() {
        let env = MapEnv::new();
        assert_eq!(detect_vscode_like_terminal(&env), None);
    }

    #[test]
    fn detect_prefers_cursor_over_windsurf_when_askpass_matches_both() {
        // Regression guard: order of the `if` chain matters. An askpass path
        // containing both substrings must resolve to Cursor (closer to the
        // user's most-likely fork), not whichever happens to come first
        // alphabetically in some hash iteration.
        let env = MapEnv::new().with("VSCODE_GIT_ASKPASS_MAIN", "/opt/cursor-windsurf-askpass");
        assert_eq!(
            detect_vscode_like_terminal(&env),
            Some(SupportedTerminal::Cursor)
        );
    }

    #[test]
    fn config_dir_darwin_is_application_support() {
        let env = MapEnv::new();
        let dir = vscode_style_config_dir("Code", Os::Darwin, &env, Path::new("/Users/me"));
        assert_eq!(
            dir,
            Some(PathBuf::from(
                "/Users/me/Library/Application Support/Code/User"
            ))
        );
    }

    #[test]
    fn config_dir_linux_is_dot_config() {
        let env = MapEnv::new();
        let dir = vscode_style_config_dir("Code", Os::Linux, &env, Path::new("/home/me"));
        assert_eq!(dir, Some(PathBuf::from("/home/me/.config/Code/User")));
    }

    #[test]
    fn config_dir_windows_uses_appdata() {
        let env = MapEnv::new().with("APPDATA", "C:/Users/me/AppData/Roaming");
        let dir = vscode_style_config_dir("Code", Os::Windows, &env, Path::new("C:/Users/me"));
        assert_eq!(
            dir,
            Some(PathBuf::from("C:/Users/me/AppData/Roaming/Code/User"))
        );
    }

    #[test]
    fn config_dir_windows_without_appdata_is_none() {
        let env = MapEnv::new();
        let dir = vscode_style_config_dir("Code", Os::Windows, &env, Path::new("C:/Users/me"));
        assert_eq!(dir, None);
    }

    #[test]
    fn strip_jsonc_strips_line_comments() {
        let s = strip_jsonc("// comment\n[{\"key\":\"shift+enter\"}]");
        // The newline is preserved; only the comment text is removed.
        assert_eq!(s, "\n[{\"key\":\"shift+enter\"}]");
    }

    #[test]
    fn strip_jsonc_strips_inline_and_block_comments() {
        assert_eq!(
            strip_jsonc("[{\"key\":\"a\"} // inline\n]"),
            "[{\"key\":\"a\"} \n]"
        );
        assert_eq!(
            strip_jsonc("[/* block */{\"key\":\"a\"}]"),
            "[{\"key\":\"a\"}]"
        );
    }

    #[test]
    fn strip_jsonc_removes_trailing_commas() {
        let v: Value = serde_json::from_str(&strip_jsonc("[{\"key\":\"a\"},]")).unwrap();
        assert_eq!(v, json!([{"key": "a"}]));
        let v: Value = serde_json::from_str(&strip_jsonc("[{\"key\":\"a\",}]")).unwrap();
        assert_eq!(v, json!([{"key": "a"}]));
    }

    #[test]
    fn strip_jsonc_preserves_comment_like_text_inside_strings() {
        let input = "[{\"key\":\"a\",\"args\":{\"text\":\"// not a comment\"}}]";
        let v: Value = serde_json::from_str(&strip_jsonc(input)).unwrap();
        assert_eq!(
            v,
            json!([{"key": "a", "args": {"text": "// not a comment"}}])
        );
    }

    #[test]
    fn strip_jsonc_handles_unterminated_block_comment() {
        let stripped = strip_jsonc("[{\"key\":\"a\"} /* never closed");
        assert_eq!(stripped, "[{\"key\":\"a\"} ");
    }

    #[test]
    fn strip_jsonc_handles_escaped_quotes_inside_strings() {
        // Regression: the string-skip loop must consume `\"` as one unit so it
        // doesn't mistakenly treat the escaped quote as the string terminator
        // and start interpreting the rest as code.
        let input = "[{\"text\":\"a\\\"b // not a comment\"}]";
        let stripped = strip_jsonc(input);
        let v: Value = serde_json::from_str(&stripped).unwrap();
        assert_eq!(v, json!([{"text": "a\"b // not a comment"}]));
    }

    fn keybindings_path_for(terminal: SupportedTerminal, os: Os, home: &Path) -> PathBuf {
        vscode_style_config_dir(terminal.app_name(), os, &MapEnv::new(), home)
            .unwrap()
            .join("keybindings.json")
    }

    #[test]
    fn configure_writes_all_six_bindings_on_darwin_when_missing() {
        let env = MapEnv::new();
        let home = Path::new("/Users/me");
        let fs = MockFs::default();
        let result = configure_terminal_keybindings(
            SupportedTerminal::Vscode,
            Os::Darwin,
            &env,
            home,
            &fs,
            fixed_iso(),
        );
        assert!(result.success, "msg: {}", result.message);
        assert!(result.requires_restart);

        let path = keybindings_path_for(SupportedTerminal::Vscode, Os::Darwin, home);
        let written = fs.writes().get(&path).cloned().expect("file written");
        assert!(written.contains("terminalTextSelected"));
        assert!(written.contains("cmd+c"));
        assert!(written.contains("shift+enter"));
        assert!(written.contains("ctrl+enter"));
        assert!(written.contains("cmd+enter"));
        assert!(written.contains("cmd+z"));
        assert!(written.contains("shift+cmd+z"));
        // CSI-u sequences appear as escape-encoded JSON strings.
        assert!(written.contains("\\u001b[99;13u"));
        assert!(written.contains("\\u001b[122;9u"));
        assert!(written.contains("\\u001b[122;10u"));

        // No backup because there was no existing file.
        assert!(fs.copies_made().is_empty());
    }

    #[test]
    fn configure_omits_cmdc_on_linux() {
        let env = MapEnv::new();
        let home = Path::new("/home/me");
        let fs = MockFs::default();
        let result = configure_terminal_keybindings(
            SupportedTerminal::Vscode,
            Os::Linux,
            &env,
            home,
            &fs,
            fixed_iso(),
        );
        assert!(result.success);
        let path = keybindings_path_for(SupportedTerminal::Vscode, Os::Linux, home);
        let written = fs.writes().get(&path).cloned().expect("file written");
        assert!(!written.contains("cmd+c"), "linux must not install Cmd+C");
        assert!(!written.contains("terminalTextSelected"));
        assert!(written.contains("shift+enter"));
    }

    #[test]
    fn configure_prepends_bindings_so_borg_wins_over_user_bindings() {
        // Existing user binding on `cmd+enter` with a disjoint `when` clause —
        // not a conflict, but we still must come first in array order so the
        // IDE prefers ours when contexts overlap.
        let env = MapEnv::new();
        let home = Path::new("/Users/me");
        let path = keybindings_path_for(SupportedTerminal::Vscode, Os::Darwin, home);
        let existing = json!([
            { "key": "cmd+enter", "command": "user.somethingElse", "when": "editorFocus" }
        ]);
        let fs = MockFs::default().with_file(&path, &existing.to_string());

        let result = configure_terminal_keybindings(
            SupportedTerminal::Vscode,
            Os::Darwin,
            &env,
            home,
            &fs,
            fixed_iso(),
        );
        assert!(result.success, "msg: {}", result.message);

        let written = fs.writes().get(&path).cloned().expect("file written");
        let parsed: Value = serde_json::from_str(&written).unwrap();
        let arr = parsed.as_array().unwrap();
        // First entry must be a borg-installed binding (any of ours).
        let first = &arr[0];
        assert_eq!(
            first.get("command").and_then(Value::as_str),
            Some(SEND_SEQUENCE_COMMAND)
        );
        // The user binding survives and is now last (was index 0 → pushed down).
        let last = arr.last().unwrap();
        assert_eq!(
            last.get("command").and_then(Value::as_str),
            Some("user.somethingElse")
        );
    }

    #[test]
    fn configure_reports_conflict_on_cmd_z_with_overlapping_when() {
        let env = MapEnv::new();
        let home = Path::new("/Users/me");
        let path = keybindings_path_for(SupportedTerminal::Cursor, Os::Darwin, home);
        let existing = json!([
            {
                "key": "cmd+z",
                "command": "something.else",
                "when": "terminalFocus",
                "args": { "text": "noop" }
            }
        ]);
        let fs = MockFs::default().with_file(&path, &existing.to_string());

        let result = configure_terminal_keybindings(
            SupportedTerminal::Cursor,
            Os::Darwin,
            &env,
            home,
            &fs,
            fixed_iso(),
        );
        assert!(!result.success);
        assert!(result.message.contains("cmd+z"));
        // File unchanged: we still see the original JSON.
        let written = fs.writes().get(&path).cloned().unwrap();
        let v: Value = serde_json::from_str(&written).unwrap();
        assert_eq!(v, existing);
        assert!(fs.copies_made().is_empty());
    }

    #[test]
    fn configure_flags_global_cmd_c_as_conflict() {
        let env = MapEnv::new();
        let home = Path::new("/Users/me");
        let path = keybindings_path_for(SupportedTerminal::Vscode, Os::Darwin, home);
        let existing = json!([
            { "key": "cmd+c", "command": "myExtension.smartCopy" }
        ]);
        let fs = MockFs::default().with_file(&path, &existing.to_string());

        let result = configure_terminal_keybindings(
            SupportedTerminal::Vscode,
            Os::Darwin,
            &env,
            home,
            &fs,
            fixed_iso(),
        );
        assert!(!result.success);
        assert!(result.message.contains("cmd+c"));
    }

    #[test]
    fn configure_flags_overlapping_terminal_focus_cmd_c_as_conflict() {
        let env = MapEnv::new();
        let home = Path::new("/Users/me");
        let path = keybindings_path_for(SupportedTerminal::Vscode, Os::Darwin, home);
        let existing = json!([
            {
                "key": "cmd+c",
                "command": "workbench.action.terminal.copySelection",
                "when": "terminalFocus"
            }
        ]);
        let fs = MockFs::default().with_file(&path, &existing.to_string());

        let result = configure_terminal_keybindings(
            SupportedTerminal::Vscode,
            Os::Darwin,
            &env,
            home,
            &fs,
            fixed_iso(),
        );
        assert!(!result.success);
        assert!(result.message.contains("cmd+c"));
    }

    #[test]
    fn configure_treats_negated_terminal_text_selected_as_disjoint() {
        let env = MapEnv::new();
        let home = Path::new("/Users/me");
        let path = keybindings_path_for(SupportedTerminal::Vscode, Os::Darwin, home);
        let existing = json!([
            {
                "key": "cmd+c",
                "command": "workbench.action.terminal.sendSequence",
                "when": "terminalFocus && !terminalTextSelected",
                "args": { "text": "" }
            }
        ]);
        let fs = MockFs::default().with_file(&path, &existing.to_string());

        let result = configure_terminal_keybindings(
            SupportedTerminal::Vscode,
            Os::Darwin,
            &env,
            home,
            &fs,
            fixed_iso(),
        );
        assert!(result.success, "msg: {}", result.message);
    }

    #[test]
    fn configure_treats_editor_focus_cmd_c_as_disjoint() {
        let env = MapEnv::new();
        let home = Path::new("/Users/me");
        let path = keybindings_path_for(SupportedTerminal::Vscode, Os::Darwin, home);
        let existing = json!([
            {
                "key": "cmd+c",
                "command": "editor.action.clipboardCopyAction",
                "when": "editorFocus"
            }
        ]);
        let fs = MockFs::default().with_file(&path, &existing.to_string());

        let result = configure_terminal_keybindings(
            SupportedTerminal::Vscode,
            Os::Darwin,
            &env,
            home,
            &fs,
            fixed_iso(),
        );
        assert!(result.success, "msg: {}", result.message);
    }

    #[test]
    fn configure_backs_up_existing_file_before_writing() {
        let env = MapEnv::new();
        let home = Path::new("/Users/me");
        let path = keybindings_path_for(SupportedTerminal::Vscode, Os::Darwin, home);
        let fs = MockFs::default().with_file(&path, "[]");

        let result = configure_terminal_keybindings(
            SupportedTerminal::Vscode,
            Os::Darwin,
            &env,
            home,
            &fs,
            fixed_iso(),
        );
        assert!(result.success);
        let copies = fs.copies_made();
        assert_eq!(copies.len(), 1, "backup must be created once");
        let (from, to) = &copies[0];
        assert_eq!(from, &path);
        // Backup filename must not contain raw `:` (colons) — those break
        // round-trips on Windows and look noisy on POSIX.
        let backup_name = to.file_name().unwrap().to_string_lossy().into_owned();
        assert!(backup_name.starts_with("keybindings.json.backup."));
        assert!(!backup_name.contains(':'));
    }

    #[test]
    fn configure_is_idempotent_when_all_bindings_already_present() {
        let env = MapEnv::new();
        let home = Path::new("/Users/me");
        let path = keybindings_path_for(SupportedTerminal::Vscode, Os::Darwin, home);
        // Seed the file with the full set of bindings by running once.
        let fs = MockFs::default();
        let first = configure_terminal_keybindings(
            SupportedTerminal::Vscode,
            Os::Darwin,
            &env,
            home,
            &fs,
            fixed_iso(),
        );
        assert!(first.success);
        let count_before = fs.copies_made().len();
        let written_before = fs.writes().get(&path).cloned().unwrap();

        // Re-run: must report "already configured", make no new copy, leave
        // the file content untouched.
        let second = configure_terminal_keybindings(
            SupportedTerminal::Vscode,
            Os::Darwin,
            &env,
            home,
            &fs,
            "2099-01-01T00-00-00.000Z",
        );
        assert!(second.success);
        assert!(!second.requires_restart);
        assert!(second.message.contains("already configured"));
        assert_eq!(fs.copies_made().len(), count_before);
        assert_eq!(fs.writes().get(&path).cloned().unwrap(), written_before);
    }

    #[test]
    fn configure_surfaces_read_permission_error() {
        let env = MapEnv::new();
        let home = Path::new("/Users/me");
        let path = keybindings_path_for(SupportedTerminal::Vscode, Os::Darwin, home);
        let fs = MockFs::default().with_file(&path, "[]");
        *fs.force_read_error.borrow_mut() = Some(io::ErrorKind::PermissionDenied);

        let result = configure_terminal_keybindings(
            SupportedTerminal::Vscode,
            Os::Darwin,
            &env,
            home,
            &fs,
            fixed_iso(),
        );
        assert!(!result.success);
        assert!(result.message.contains("Failed to read"));
    }

    #[test]
    fn configure_rejects_non_array_keybindings_file() {
        let env = MapEnv::new();
        let home = Path::new("/Users/me");
        let path = keybindings_path_for(SupportedTerminal::Vscode, Os::Darwin, home);
        let fs = MockFs::default().with_file(&path, "{\"not\": \"an array\"}");

        let result = configure_terminal_keybindings(
            SupportedTerminal::Vscode,
            Os::Darwin,
            &env,
            home,
            &fs,
            fixed_iso(),
        );
        assert!(!result.success);
        assert!(result.message.contains("not a JSON array"));
        // File untouched.
        assert_eq!(fs.write_count(), 1);
    }

    #[test]
    fn configure_refuses_ssh_connection() {
        let env = MapEnv::new().with("SSH_CONNECTION", "1 2 3 4");
        let fs = MockFs::default();
        let result = configure_terminal_keybindings(
            SupportedTerminal::Vscode,
            Os::Darwin,
            &env,
            Path::new("/Users/me"),
            &fs,
            fixed_iso(),
        );
        assert!(!result.success);
        assert!(result.message.contains("local machine"));
        assert!(fs.writes().is_empty());
    }

    #[test]
    fn configure_refuses_ssh_tty() {
        let env = MapEnv::new().with("SSH_TTY", "/dev/pts/0");
        let fs = MockFs::default();
        let result = configure_terminal_keybindings(
            SupportedTerminal::Vscode,
            Os::Darwin,
            &env,
            Path::new("/Users/me"),
            &fs,
            fixed_iso(),
        );
        assert!(!result.success);
        assert!(result.message.contains("local machine"));
    }

    #[test]
    fn configure_refuses_ssh_client() {
        let env = MapEnv::new().with("SSH_CLIENT", "1.2.3.4 22");
        let fs = MockFs::default();
        let result = configure_terminal_keybindings(
            SupportedTerminal::Vscode,
            Os::Darwin,
            &env,
            Path::new("/Users/me"),
            &fs,
            fixed_iso(),
        );
        assert!(!result.success);
    }

    #[test]
    fn detected_configure_uses_cursor_path_when_cursor_detected() {
        let env = MapEnv::new().with("CURSOR_TRACE_ID", "x");
        let home = Path::new("/Users/me");
        let fs = MockFs::default();
        let result =
            configure_detected_terminal_keybindings(Os::Darwin, &env, home, &fs, fixed_iso());
        assert!(result.success);
        let cursor_path = keybindings_path_for(SupportedTerminal::Cursor, Os::Darwin, home);
        assert!(
            fs.writes().contains_key(&cursor_path),
            "expected write to Cursor config dir, got: {:?}",
            fs.writes().keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn detected_configure_reports_no_supported_when_undetected() {
        let env = MapEnv::new();
        let fs = MockFs::default();
        let result = configure_detected_terminal_keybindings(
            Os::Darwin,
            &env,
            Path::new("/Users/me"),
            &fs,
            fixed_iso(),
        );
        assert!(!result.success);
        assert!(result.message.contains("Supported"));
    }

    #[test]
    fn should_prompt_true_when_detected_and_file_missing() {
        let env = MapEnv::new().with("TERM_PROGRAM", "vscode");
        let fs = MockFs::default();
        assert!(should_prompt_for_terminal_setup(
            Os::Darwin,
            &env,
            Path::new("/Users/me"),
            &fs,
        ));
    }

    #[test]
    fn should_prompt_false_when_all_bindings_present() {
        let env = MapEnv::new().with("TERM_PROGRAM", "vscode");
        let home = Path::new("/Users/me");
        // Run configure once to seed the file with the full set.
        let fs = MockFs::default();
        let _ = configure_terminal_keybindings(
            SupportedTerminal::Vscode,
            Os::Darwin,
            &env,
            home,
            &fs,
            fixed_iso(),
        );
        assert!(!should_prompt_for_terminal_setup(
            Os::Darwin,
            &env,
            home,
            &fs
        ));
    }

    #[test]
    fn should_prompt_false_in_ssh_even_when_detected() {
        let env = MapEnv::new()
            .with("TERM_PROGRAM", "vscode")
            .with("SSH_CONNECTION", "1 2 3 4");
        let fs = MockFs::default();
        assert!(!should_prompt_for_terminal_setup(
            Os::Darwin,
            &env,
            Path::new("/Users/me"),
            &fs,
        ));
    }

    #[test]
    fn should_prompt_false_when_not_detected() {
        let env = MapEnv::new();
        let fs = MockFs::default();
        assert!(!should_prompt_for_terminal_setup(
            Os::Darwin,
            &env,
            Path::new("/Users/me"),
            &fs,
        ));
    }

    #[test]
    fn parse_target_empty_and_auto_yield_none() {
        assert_eq!(parse_terminal_setup_target(""), Ok(None));
        assert_eq!(parse_terminal_setup_target("  "), Ok(None));
        assert_eq!(parse_terminal_setup_target("auto"), Ok(None));
        assert_eq!(parse_terminal_setup_target("AUTO"), Ok(None));
    }

    #[test]
    fn parse_target_known_names_case_insensitive() {
        assert_eq!(
            parse_terminal_setup_target("vscode"),
            Ok(Some(SupportedTerminal::Vscode))
        );
        assert_eq!(
            parse_terminal_setup_target("Cursor"),
            Ok(Some(SupportedTerminal::Cursor))
        );
        assert_eq!(
            parse_terminal_setup_target("WINDSURF"),
            Ok(Some(SupportedTerminal::Windsurf))
        );
    }

    #[test]
    fn parse_target_unknown_returns_err_with_token() {
        assert_eq!(
            parse_terminal_setup_target("emacs"),
            Err("emacs".to_string())
        );
        assert_eq!(
            parse_terminal_setup_target("  foo  "),
            Err("foo".to_string())
        );
    }
}
