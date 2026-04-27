use std::path::PathBuf;

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
    fn detect_agent_browser_returns_bool() {
        let _result = detect_agent_browser();
    }

    #[test]
    fn find_chrome_with_no_configured_path() {
        let detection = find_chrome(None);
        if let Some(ref exe) = detection.executable {
            assert!(detection.all_found.contains(exe));
        }
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

    #[test]
    fn platform_candidates_returns_vec() {
        let candidates = platform_candidates();
        // Just verify it doesn't panic and returns a vec
        let _ = candidates.len();
    }

    #[test]
    fn which_candidates_returns_vec() {
        let candidates = which_candidates();
        assert!(
            !candidates.is_empty(),
            "which_candidates should return at least one name"
        );
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
