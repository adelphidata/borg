use anyhow::{bail, Context, Result};
use chrono::TimeZone;
use reqwest::header::HeaderMap;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::Read;

const GITHUB_API: &str = "https://api.github.com/repos/borganization/borg";

/// Public URL for human-readable release notes, surfaced in the TUI-open
/// update prompt.
pub const RELEASE_NOTES_URL: &str = "https://github.com/borganization/borg/releases/latest";

/// How a future "Update now" action would install. Only `Tarball` is wired up
/// today — `Homebrew` is a stub for an upcoming `brew install borg` formula.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallSource {
    /// Current GitHub-release flow (`perform_update`): download tarball,
    /// verify checksum, swap in place.
    Tarball,
    /// Stub for a future Homebrew formula. Not yet wired up.
    Homebrew,
}

/// Detect how the running binary should self-update. Today everyone uses the
/// tarball flow; the Homebrew arm is reserved for a future formula.
pub fn detect_install_source() -> InstallSource {
    // TODO(brew): switch to InstallSource::Homebrew when the formula ships and
    // we can reliably detect a brew-installed binary (e.g. canonical path
    // under HOMEBREW_PREFIX, or a build-time feature flag).
    InstallSource::Tarball
}

/// Human-readable command rendered in the prompt's "Update now (runs `…`)"
/// row.
pub fn update_command_str(src: InstallSource) -> &'static str {
    match src {
        InstallSource::Tarball => "borg /update",
        InstallSource::Homebrew => "brew upgrade borg",
    }
}

/// Returns the current binary version from Cargo.toml.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Map std::env::consts to the asset naming convention used in GitHub Releases.
pub fn platform_asset_name() -> Result<String> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        o => bail!("Unsupported OS: {o}"),
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        a => bail!("Unsupported architecture: {a}"),
    };
    Ok(format!("borg-{os}-{arch}.tar.gz"))
}

/// Metadata for a GitHub release.
#[derive(Debug, Deserialize)]
pub struct ReleaseInfo {
    /// Git tag (e.g. "v0.2.0").
    pub tag_name: String,
    /// Whether this is a pre-release.
    pub prerelease: bool,
    /// Downloadable assets attached to the release.
    pub assets: Vec<ReleaseAsset>,
}

/// A single downloadable file in a GitHub release.
#[derive(Debug, Deserialize)]
pub struct ReleaseAsset {
    /// Filename of the asset (e.g. "borg-darwin-arm64.tar.gz").
    pub name: String,
    /// Direct download URL.
    pub browser_download_url: String,
}

/// Outcome of an update check or operation.
pub enum UpdateStatus {
    /// The installed version is already the latest.
    AlreadyUpToDate,
    /// A new version was installed.
    Updated {
        /// Previous version string.
        from: String,
        /// Newly installed version string.
        to: String,
    },
}

/// Result of a self-update operation.
pub struct UpdateResult {
    /// Version that was installed before the update.
    pub current_version: String,
    /// Latest version available on GitHub.
    pub latest_version: String,
    /// Whether the update was applied or skipped.
    pub status: UpdateStatus,
}

/// Fetch the latest release from GitHub. If `dev` is true, includes pre-releases.
pub async fn fetch_latest_release(dev: bool) -> Result<ReleaseInfo> {
    fetch_latest_release_with_base(GITHUB_API, dev).await
}

async fn fetch_latest_release_with_base(base_url: &str, dev: bool) -> Result<ReleaseInfo> {
    let client = reqwest::Client::new();

    if dev {
        fetch_release_list(&client, base_url, ReleaseKind::Prerelease).await
    } else {
        let latest_url = format!("{base_url}/releases/latest");
        match get_github_json::<ReleaseInfo>(&client, &latest_url, "latest release").await {
            Ok(release) => Ok(release),
            Err(primary) => {
                tracing::warn!(
                    "latest release endpoint failed, falling back to release list: {primary:#}"
                );
                fetch_release_list(&client, base_url, ReleaseKind::Stable)
                    .await
                    .with_context(|| format!("Failed to resolve latest stable release after /releases/latest failed: {primary}"))
            }
        }
    }
}

#[derive(Clone, Copy)]
enum ReleaseKind {
    Stable,
    Prerelease,
}

async fn fetch_release_list(
    client: &reqwest::Client,
    base_url: &str,
    kind: ReleaseKind,
) -> Result<ReleaseInfo> {
    let url = format!("{base_url}/releases?per_page=10");
    let releases: Vec<ReleaseInfo> = get_github_json(client, &url, "release list").await?;
    select_release(releases, kind)
}

fn select_release(releases: Vec<ReleaseInfo>, kind: ReleaseKind) -> Result<ReleaseInfo> {
    let mut saw_matching_channel = false;
    for release in releases {
        let matches_channel = match kind {
            ReleaseKind::Stable => !release.prerelease,
            ReleaseKind::Prerelease => true,
        };
        if !matches_channel {
            continue;
        }
        saw_matching_channel = true;
        if !release.assets.is_empty() {
            return Ok(release);
        }
    }

    match kind {
        ReleaseKind::Stable if saw_matching_channel => {
            bail!("No usable stable releases found")
        }
        ReleaseKind::Stable => bail!("No stable releases found"),
        ReleaseKind::Prerelease if saw_matching_channel => {
            bail!("No usable releases found")
        }
        ReleaseKind::Prerelease => bail!("No releases found"),
    }
}

async fn get_github_json<T: DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    resource: &str,
) -> Result<T> {
    let response = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "borg-updater")
        .send()
        .await
        .with_context(|| format!("Failed to fetch {resource}"))?;
    parse_github_json(response, resource).await
}

async fn parse_github_json<T: DeserializeOwned>(
    response: reqwest::Response,
    resource: &str,
) -> Result<T> {
    let status = response.status();
    if !status.is_success() {
        let headers = response.headers().clone();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<response body unavailable>".to_string());
        bail!("{}", format_github_api_error(status, &headers, &body));
    }
    response
        .json()
        .await
        .with_context(|| format!("Failed to parse {resource} JSON"))
}

fn format_github_api_error(status: reqwest::StatusCode, headers: &HeaderMap, body: &str) -> String {
    let snippet = summarize_error_body(body);
    let status_text = status.canonical_reason().unwrap_or("unknown status");
    let mut msg = format!(
        "GitHub API returned HTTP {} {}",
        status.as_u16(),
        status_text
    );

    if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::TOO_MANY_REQUESTS
    {
        let remaining = headers
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown");
        msg = format!("GitHub API rate limit exceeded ({msg}, remaining {remaining})");
        if let Some(reset) = headers
            .get("x-ratelimit-reset")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_rate_limit_reset)
        {
            msg.push_str(&format!(", resets at {reset}"));
        }
    }

    if !snippet.is_empty() {
        msg.push_str(&format!(": {snippet}"));
    }
    msg
}

fn summarize_error_body(body: &str) -> String {
    const MAX_CHARS: usize = 200;
    let condensed = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if condensed.chars().count() <= MAX_CHARS {
        return condensed;
    }
    let mut out: String = condensed
        .chars()
        .take(MAX_CHARS.saturating_sub(1))
        .collect();
    out.push('…');
    out
}

fn parse_rate_limit_reset(raw: &str) -> Option<String> {
    let unix = raw.parse::<i64>().ok()?;
    let dt = chrono::Utc.timestamp_opt(unix, 0).single()?;
    Some(dt.to_rfc3339())
}

/// Locate the platform asset URL on a release, rejecting any non-HTTPS scheme.
///
/// Extracted from `perform_update` so the security guard is unit-testable
/// without standing up a network mock.
pub fn select_asset_url<'a>(release: &'a ReleaseInfo, asset_name: &str) -> Result<&'a str> {
    let url = release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .map(|a| a.browser_download_url.as_str())
        .with_context(|| format!("No release asset found for {asset_name}"))?;
    if !url.starts_with("https://") {
        bail!("Refusing to download from non-HTTPS URL: {url}");
    }
    Ok(url)
}

/// Compare two semver strings. Returns true if `latest` is newer than `current`.
pub fn is_newer(current: &str, latest: &str) -> bool {
    let parse = |v: &str| -> (Vec<u64>, Option<String>) {
        let v = v.strip_prefix('v').unwrap_or(v);
        let (version_part, pre) = match v.split_once('-') {
            Some((ver, pre)) => (ver, Some(pre.to_string())),
            None => (v, None),
        };
        let nums: Vec<u64> = version_part
            .split('.')
            .filter_map(|s| s.parse().ok())
            .collect();
        (nums, pre)
    };

    let (cur_nums, cur_pre) = parse(current);
    let (lat_nums, lat_pre) = parse(latest);

    // Compare numeric components
    let max_len = cur_nums.len().max(lat_nums.len());
    for i in 0..max_len {
        let c = cur_nums.get(i).copied().unwrap_or(0);
        let l = lat_nums.get(i).copied().unwrap_or(0);
        if l > c {
            return true;
        }
        if l < c {
            return false;
        }
    }

    // Same numeric version: release (no pre) is newer than pre-release
    match (&cur_pre, &lat_pre) {
        (Some(_), None) => true, // current is pre, latest is release → newer
        _ => false,              // same version, same pre status (or both release)
    }
}

/// Extract the expected SHA256 hash for `asset_name` from checksums content.
pub fn parse_checksum(checksums_content: &str, asset_name: &str) -> Option<String> {
    for line in checksums_content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 && parts[1] == asset_name {
            return Some(parts[0].to_string());
        }
    }
    None
}

/// Download, verify, and install the latest release.
pub async fn perform_update(dev: bool) -> Result<UpdateResult> {
    let current = current_version().to_string();
    let release = fetch_latest_release(dev).await?;
    let latest = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name)
        .to_string();

    if !is_newer(&current, &latest) {
        return Ok(UpdateResult {
            current_version: current,
            latest_version: latest,
            status: UpdateStatus::AlreadyUpToDate,
        });
    }

    let from_version = current.clone();

    let asset_name = platform_asset_name()?;
    let asset_url = select_asset_url(&release, &asset_name)?;

    let checksums_url = release
        .assets
        .iter()
        .find(|a| a.name == "checksums.txt")
        .map(|a| &a.browser_download_url);

    let client = reqwest::Client::new();
    let tmp_dir = tempfile::tempdir().context("Failed to create temp directory")?;

    // Download the tarball
    let archive_path = tmp_dir.path().join(&asset_name);
    let bytes = client
        .get(asset_url)
        .header("User-Agent", "borg-updater")
        .send()
        .await
        .context("Failed to download release")?
        .error_for_status()?
        .bytes()
        .await
        .context("Failed to read release bytes")?;
    std::fs::write(&archive_path, &bytes).context("Failed to write archive")?;

    // Verify checksum if available
    if let Some(checksums_url) = checksums_url {
        let checksums_text = client
            .get(checksums_url)
            .header("User-Agent", "borg-updater")
            .send()
            .await
            .context("Failed to download checksums")?
            .error_for_status()?
            .text()
            .await
            .context("Failed to read checksums")?;

        let expected = parse_checksum(&checksums_text, &asset_name)
            .with_context(|| format!("Asset {asset_name} not found in checksums.txt"))?;
        let mut file = std::fs::File::open(&archive_path)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 8192];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let actual = format!("{:x}", hasher.finalize());
        if actual != expected {
            bail!("Checksum verification failed:\n  expected: {expected}\n  got:      {actual}");
        }
    } else {
        tracing::warn!("No checksums.txt in release — skipping integrity verification");
    }

    // Extract tarball
    let extract_dir = tmp_dir.path().join("extract");
    std::fs::create_dir_all(&extract_dir)?;
    let archive_str = archive_path
        .to_str()
        .context("Archive path contains non-UTF8 characters")?;
    let status = std::process::Command::new("tar")
        .args(["xzf", archive_str])
        .current_dir(&extract_dir)
        .status()
        .context("Failed to run tar")?;
    if !status.success() {
        bail!("tar extraction failed with status {status}");
    }

    // Find the extracted binary
    let new_binary = extract_dir.join("borg");
    if !new_binary.exists() {
        bail!("Extracted archive does not contain 'borg' binary");
    }

    // Replace the current binary
    let current_exe =
        std::env::current_exe().context("Failed to determine current executable path")?;
    let current_exe = current_exe
        .canonicalize()
        .unwrap_or_else(|_| current_exe.clone());
    let backup = current_exe.with_extension("old");

    std::fs::rename(&current_exe, &backup)
        .with_context(|| format!("Failed to rename current binary to {}", backup.display()))?;

    if let Err(e) = std::fs::copy(&new_binary, &current_exe) {
        // Attempt to restore backup
        let _ = std::fs::rename(&backup, &current_exe);
        return Err(e).context("Failed to install new binary");
    }

    // Set executable permissions
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&current_exe, perms)
            .context("Failed to set executable permissions")?;
    }

    // Clean up backup
    let _ = std::fs::remove_file(&backup);

    Ok(UpdateResult {
        current_version: from_version.clone(),
        latest_version: latest.clone(),
        status: UpdateStatus::Updated {
            from: from_version,
            to: latest,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    #[test]
    fn update_command_str_distinguishes_install_sources() {
        // Guard the brew stub: when the Homebrew formula lands and someone
        // wires up `detect_install_source`, the prompt's command string must
        // change with it. Testing the mapping prevents the stub from going
        // out of sync silently.
        assert_eq!(update_command_str(InstallSource::Tarball), "borg /update");
        assert_eq!(
            update_command_str(InstallSource::Homebrew),
            "brew upgrade borg"
        );
    }

    #[test]
    fn detect_install_source_is_tarball_until_brew_lands() {
        // Lock in the current default. Flip this assertion in the same PR
        // that ships the brew formula so the change is auditable.
        assert_eq!(detect_install_source(), InstallSource::Tarball);
    }

    #[test]
    fn test_platform_asset_name() {
        let name = platform_asset_name().unwrap();
        assert!(name.starts_with("borg-"));
        assert!(name.ends_with(".tar.gz"));

        #[cfg(target_os = "macos")]
        assert!(name.contains("darwin"));

        #[cfg(target_os = "linux")]
        assert!(name.contains("linux"));

        #[cfg(target_arch = "aarch64")]
        assert!(name.contains("arm64"));

        #[cfg(target_arch = "x86_64")]
        assert!(name.contains("x86_64"));
    }

    #[test]
    fn test_is_newer() {
        // Newer
        assert!(is_newer("0.1.0", "0.2.0"));
        assert!(is_newer("0.1.0", "0.1.1"));
        assert!(is_newer("0.1.0", "1.0.0"));
        assert!(is_newer("0.1.9", "0.2.0"));

        // Same
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("1.0.0", "1.0.0"));

        // Older
        assert!(!is_newer("0.3.0", "0.2.0"));
        assert!(!is_newer("1.0.0", "0.9.0"));

        // Pre-release handling
        assert!(is_newer("0.2.0-dev.1", "0.2.0")); // release is newer than its pre-release
        assert!(is_newer("0.1.0", "0.2.0-dev.1")); // dev of higher version is newer
        assert!(!is_newer("0.2.0", "0.2.0-dev.1")); // pre-release is not newer than release

        // v prefix
        assert!(is_newer("v0.1.0", "v0.2.0"));
        assert!(!is_newer("v0.2.0", "v0.2.0"));
    }

    #[test]
    fn test_parse_checksums() {
        let content = "\
abc123def456  borg-darwin-arm64.tar.gz
789xyz000111  borg-linux-x86_64.tar.gz
fedcba987654  checksums.txt
";
        assert_eq!(
            parse_checksum(content, "borg-darwin-arm64.tar.gz"),
            Some("abc123def456".to_string())
        );
        assert_eq!(
            parse_checksum(content, "borg-linux-x86_64.tar.gz"),
            Some("789xyz000111".to_string())
        );
        assert_eq!(parse_checksum(content, "borg-windows-x86_64.tar.gz"), None);
    }

    #[test]
    fn test_platform_mapping_coverage() {
        // Verify the current platform produces a valid asset name
        let name = platform_asset_name().unwrap();
        let valid_names = [
            "borg-darwin-arm64.tar.gz",
            "borg-darwin-x86_64.tar.gz",
            "borg-linux-arm64.tar.gz",
            "borg-linux-x86_64.tar.gz",
        ];
        assert!(
            valid_names.contains(&name.as_str()),
            "Unexpected asset name: {name}"
        );
    }

    #[test]
    fn test_parse_release_json() {
        let json = r#"{
            "tag_name": "v0.2.0",
            "prerelease": false,
            "assets": [
                {
                    "name": "borg-darwin-arm64.tar.gz",
                    "browser_download_url": "https://example.com/borg-darwin-arm64.tar.gz"
                },
                {
                    "name": "checksums.txt",
                    "browser_download_url": "https://example.com/checksums.txt"
                }
            ]
        }"#;
        let release: ReleaseInfo = serde_json::from_str(json).unwrap();
        assert_eq!(release.tag_name, "v0.2.0");
        assert!(!release.prerelease);
        assert_eq!(release.assets.len(), 2);
        assert_eq!(release.assets[0].name, "borg-darwin-arm64.tar.gz");
    }

    fn release_with(asset_name: &str, url: &str) -> ReleaseInfo {
        ReleaseInfo {
            tag_name: "v0.2.0".into(),
            prerelease: false,
            assets: vec![ReleaseAsset {
                name: asset_name.into(),
                browser_download_url: url.into(),
            }],
        }
    }

    #[test]
    fn test_select_asset_url_accepts_https() {
        let r = release_with(
            "borg-darwin-arm64.tar.gz",
            "https://example.com/borg-darwin-arm64.tar.gz",
        );
        let url = select_asset_url(&r, "borg-darwin-arm64.tar.gz").unwrap();
        assert!(url.starts_with("https://"));
    }

    #[test]
    fn test_select_asset_url_rejects_http() {
        let r = release_with(
            "borg-darwin-arm64.tar.gz",
            "http://example.com/borg-darwin-arm64.tar.gz",
        );
        let err = select_asset_url(&r, "borg-darwin-arm64.tar.gz").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("non-HTTPS"),
            "must surface the security reason, got: {msg}"
        );
    }

    #[test]
    fn test_select_asset_url_rejects_file_scheme() {
        // Defense-in-depth: a malicious release manifest must not be able to
        // point us at a local path.
        let r = release_with("borg-darwin-arm64.tar.gz", "file:///etc/passwd");
        assert!(select_asset_url(&r, "borg-darwin-arm64.tar.gz").is_err());
    }

    #[test]
    fn test_select_asset_url_missing_asset() {
        let r = release_with(
            "borg-darwin-arm64.tar.gz",
            "https://example.com/borg-darwin-arm64.tar.gz",
        );
        let err = select_asset_url(&r, "borg-windows-x86_64.tar.gz").unwrap_err();
        assert!(format!("{err:#}").contains("No release asset"));
    }

    #[test]
    fn test_parse_prerelease_list() {
        let json = r#"[
            {
                "tag_name": "v0.3.0-dev.1",
                "prerelease": true,
                "assets": []
            },
            {
                "tag_name": "v0.2.0",
                "prerelease": false,
                "assets": []
            }
        ]"#;
        let releases: Vec<ReleaseInfo> = serde_json::from_str(json).unwrap();

        // dev=true picks the first (latest by date)
        let dev_pick = releases.first().unwrap();
        assert_eq!(dev_pick.tag_name, "v0.3.0-dev.1");
        assert!(dev_pick.prerelease);

        // dev=false would use /releases/latest which only returns non-prerelease
        // (tested via the API, not list parsing)
        let stable_pick = releases.iter().find(|r| !r.prerelease).unwrap();
        assert_eq!(stable_pick.tag_name, "v0.2.0");
    }

    fn release(tag: &str, prerelease: bool, assets: usize) -> ReleaseInfo {
        ReleaseInfo {
            tag_name: tag.into(),
            prerelease,
            assets: (0..assets)
                .map(|i| ReleaseAsset {
                    name: format!("asset-{i}.tar.gz"),
                    browser_download_url: format!("https://example.com/asset-{i}.tar.gz"),
                })
                .collect(),
        }
    }

    #[test]
    fn select_release_stable_skips_prerelease_and_empty_assets() {
        let release = select_release(
            vec![
                release("v0.4.0-dev.1", true, 1),
                release("v0.3.0", false, 0),
                release("v0.2.0", false, 1),
            ],
            ReleaseKind::Stable,
        )
        .unwrap();
        assert_eq!(release.tag_name, "v0.2.0");
        assert!(!release.prerelease);
    }

    #[test]
    fn select_release_prerelease_ignores_entries_without_assets() {
        let release = select_release(
            vec![
                release("v0.5.0-dev.1", true, 0),
                release("v0.5.0-dev.2", true, 1),
            ],
            ReleaseKind::Prerelease,
        )
        .unwrap();
        assert_eq!(release.tag_name, "v0.5.0-dev.2");
    }

    #[test]
    fn github_http_errors_include_status_and_body() {
        let msg = format_github_api_error(
            reqwest::StatusCode::NOT_FOUND,
            &HeaderMap::new(),
            r#"{"message":"Repository renamed during migration"}"#,
        );
        assert!(msg.contains("HTTP 404"), "{msg}");
        assert!(msg.contains("Repository renamed"), "{msg}");
    }

    #[test]
    fn github_rate_limit_errors_are_specific() {
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
        headers.insert("x-ratelimit-reset", HeaderValue::from_static("1777777777"));
        let msg = format_github_api_error(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            &headers,
            r#"{"message":"API rate limit exceeded"}"#,
        );
        assert!(msg.contains("rate limit exceeded"), "{msg}");
        assert!(msg.contains("remaining 0"), "{msg}");
        assert!(msg.contains("resets at"), "{msg}");
    }
}
