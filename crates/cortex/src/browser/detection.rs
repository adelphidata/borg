use std::path::PathBuf;

use crate::config::BrowserEngine;

/// Result of Chrome/Chromium executable detection.
pub struct ChromeDetection {
    /// The best candidate executable (first found in priority order).
    pub executable: Option<PathBuf>,
    /// All Chrome-like executables found on this system.
    pub all_found: Vec<PathBuf>,
}

/// Detect Chrome/Chromium executables on the system.
///
/// If `configured_path` is provided and the file exists, it is used as the primary
/// executable. Otherwise, platform-specific detection runs through known paths.
pub fn find_chrome(configured_path: Option<&str>) -> ChromeDetection {
    let mut all_found = Vec::new();

    // If a configured path is provided and exists, use it as primary.
    if let Some(path) = configured_path {
        let p = PathBuf::from(path);
        if p.exists() {
            all_found.push(p.clone());
            return ChromeDetection {
                executable: Some(p),
                all_found,
            };
        }
    }

    // Platform-specific known paths
    let candidates = platform_candidates();

    for candidate in &candidates {
        let p = PathBuf::from(candidate);
        if p.exists() {
            all_found.push(p);
        }
    }

    // Also check PATH via `which`
    let which_names = which_candidates();
    for name in &which_names {
        if let Ok(p) = which::which(name) {
            if !all_found.contains(&p) {
                all_found.push(p);
            }
        }
    }

    let executable = all_found.first().cloned();
    ChromeDetection {
        executable,
        all_found,
    }
}

/// Check whether the `agent-browser` CLI is available on PATH.
pub fn detect_agent_browser() -> bool {
    which::which("agent-browser").is_ok()
}

/// Resolve the executable to launch for a given [`BrowserEngine`].
///
/// Precedence:
/// 1. `Custom` engine → the configured `browser.executable` verbatim (the
///    launcher validates existence and surfaces a clear error if wrong).
/// 2. An explicit `browser.executable` that exists → used regardless of engine.
/// 3. The engine's own known install paths / PATH names.
/// 4. Fallback to generic Chrome/Chromium discovery ([`find_chrome`]).
///
/// Returns `None` only when nothing is found; the caller turns that into a
/// user-facing "install a browser or set browser.executable" error.
pub fn resolve_engine_executable(engine: BrowserEngine, explicit: Option<&str>) -> Option<String> {
    if engine == BrowserEngine::Custom {
        return explicit.map(str::to_string);
    }

    // An explicit, existing path always wins.
    if let Some(path) = explicit {
        if PathBuf::from(path).exists() {
            return Some(path.to_string());
        }
    }

    // Engine-specific absolute install locations.
    for candidate in engine_candidates(engine) {
        if PathBuf::from(&candidate).exists() {
            return Some(candidate);
        }
    }

    // Engine-specific PATH names.
    for name in engine_which(engine) {
        if let Ok(p) = which::which(name) {
            return Some(p.to_string_lossy().into_owned());
        }
    }

    // Nothing engine-specific — fall back to generic Chrome discovery so that,
    // e.g., "brave" on a machine with only Chrome still gets a working browser.
    find_chrome(explicit)
        .executable
        .map(|p| p.to_string_lossy().into_owned())
}

/// Absolute install paths to probe for a specific engine, per platform.
fn engine_candidates(engine: BrowserEngine) -> Vec<String> {
    #[cfg(target_os = "macos")]
    let paths: &[&str] = match engine {
        BrowserEngine::Chrome => &["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"],
        BrowserEngine::Brave => &["/Applications/Brave Browser.app/Contents/MacOS/Brave Browser"],
        BrowserEngine::Edge => &["/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"],
        BrowserEngine::Chromium => &["/Applications/Chromium.app/Contents/MacOS/Chromium"],
        BrowserEngine::Lightpanda | BrowserEngine::Custom => &[],
    };
    #[cfg(target_os = "linux")]
    let paths: &[&str] = match engine {
        BrowserEngine::Chrome => &["/opt/google/chrome/chrome"],
        BrowserEngine::Brave => &["/opt/brave.com/brave/brave-browser"],
        BrowserEngine::Edge => &["/opt/microsoft/msedge/msedge"],
        BrowserEngine::Chromium => &[
            "/usr/lib/chromium/chromium",
            "/usr/lib/chromium-browser/chromium-browser",
        ],
        BrowserEngine::Lightpanda | BrowserEngine::Custom => &[],
    };
    #[cfg(target_os = "windows")]
    let paths: &[&str] = match engine {
        BrowserEngine::Chrome => &[r"C:\Program Files\Google\Chrome\Application\chrome.exe"],
        BrowserEngine::Brave => {
            &[r"C:\Program Files\BraveSoftware\Brave-Browser\Application\brave.exe"]
        }
        BrowserEngine::Edge => &[r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"],
        BrowserEngine::Chromium => &[r"C:\Program Files\Chromium\Application\chrome.exe"],
        BrowserEngine::Lightpanda | BrowserEngine::Custom => &[],
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let paths: &[&str] = &[];

    let _ = engine;
    paths.iter().map(|s| (*s).to_string()).collect()
}

/// PATH binary names to probe for a specific engine.
fn engine_which(engine: BrowserEngine) -> Vec<&'static str> {
    match engine {
        BrowserEngine::Chrome => vec!["google-chrome-stable", "google-chrome", "chrome"],
        BrowserEngine::Brave => vec!["brave-browser", "brave"],
        BrowserEngine::Edge => vec!["microsoft-edge", "msedge"],
        BrowserEngine::Chromium => vec!["chromium-browser", "chromium"],
        BrowserEngine::Lightpanda => vec!["lightpanda"],
        BrowserEngine::Custom => vec![],
    }
}

#[cfg(target_os = "macos")]
fn platform_candidates() -> Vec<String> {
    vec![
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into(),
        "/Applications/Chromium.app/Contents/MacOS/Chromium".into(),
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser".into(),
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge".into(),
    ]
}

#[cfg(target_os = "linux")]
fn platform_candidates() -> Vec<String> {
    vec![]
}

#[cfg(target_os = "windows")]
fn platform_candidates() -> Vec<String> {
    let mut paths = vec![
        r"C:\Program Files\Google\Chrome\Application\chrome.exe".into(),
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe".into(),
        r"C:\Program Files\Chromium\Application\chrome.exe".into(),
        r"C:\Program Files\BraveSoftware\Brave-Browser\Application\brave.exe".into(),
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe".into(),
    ];
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        paths.push(format!(r"{local}\Google\Chrome\Application\chrome.exe"));
    }
    paths
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn platform_candidates() -> Vec<String> {
    vec![]
}

#[cfg(target_os = "macos")]
fn which_candidates() -> Vec<&'static str> {
    vec!["google-chrome", "chromium"]
}

#[cfg(target_os = "linux")]
fn which_candidates() -> Vec<&'static str> {
    vec![
        "google-chrome-stable",
        "google-chrome",
        "chromium-browser",
        "chromium",
    ]
}

#[cfg(target_os = "windows")]
fn which_candidates() -> Vec<&'static str> {
    vec!["chrome", "chromium", "msedge"]
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn which_candidates() -> Vec<&'static str> {
    vec!["google-chrome", "chromium"]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_engine_custom_returns_explicit_path_verbatim() {
        // Custom must hand back exactly what the user configured — even a path
        // that doesn't exist yet — because launch validates it and surfaces a
        // precise error. This encodes the "Custom = trust the operator" rule.
        let path = "/opt/my-browser/chrome";
        assert_eq!(
            resolve_engine_executable(BrowserEngine::Custom, Some(path)).as_deref(),
            Some(path)
        );
        // Custom with nothing configured resolves to nothing.
        assert_eq!(resolve_engine_executable(BrowserEngine::Custom, None), None);
    }

    #[test]
    fn resolve_engine_prefers_existing_explicit_path() {
        // A real, existing executable passed explicitly wins for any engine.
        #[cfg(unix)]
        let real = "/bin/sh";
        #[cfg(windows)]
        let real = r"C:\Windows\System32\cmd.exe";
        assert_eq!(
            resolve_engine_executable(BrowserEngine::Chrome, Some(real)).as_deref(),
            Some(real)
        );
    }

    #[test]
    fn resolve_engine_ignores_nonexistent_explicit_and_probes() {
        // A bogus explicit path for a non-Custom engine must not be returned
        // verbatim; resolution falls through to discovery (which may be None on
        // a headless CI box — that's fine, it must simply not echo the bogus
        // path back).
        let bogus = "/definitely/not/here/chrome";
        assert_ne!(
            resolve_engine_executable(BrowserEngine::Chrome, Some(bogus)).as_deref(),
            Some(bogus)
        );
    }

    #[test]
    fn find_chrome_with_invalid_configured_path() {
        let detection = find_chrome(Some("/nonexistent/path/to/chrome"));
        assert!(!detection
            .all_found
            .contains(&PathBuf::from("/nonexistent/path/to/chrome")));
    }

    #[test]
    fn find_chrome_with_valid_configured_path() {
        // Use a binary that exists on the current platform
        #[cfg(unix)]
        let path = "/bin/sh";
        #[cfg(windows)]
        let path = r"C:\Windows\System32\cmd.exe";
        let detection = find_chrome(Some(path));
        assert_eq!(detection.executable, Some(PathBuf::from(path)));
        assert!(detection.all_found.contains(&PathBuf::from(path)));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_platform_candidates_include_program_files() {
        let candidates = platform_candidates();
        assert!(
            candidates.iter().any(|c| c.contains("Program Files")),
            "Windows candidates should include Program Files paths"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_which_candidates_include_edge() {
        let candidates = which_candidates();
        assert!(
            candidates.contains(&"msedge"),
            "Windows which_candidates should include msedge"
        );
    }
}
