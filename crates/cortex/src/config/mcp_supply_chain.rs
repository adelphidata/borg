//! Supply-chain risk analysis for stdio MCP servers.
//!
//! A stdio MCP server is launched as a subprocess, and the common spelling —
//! `npx -y @some/mcp-server` — resolves and executes third-party code from a
//! public registry at spawn time, in-process with the agent's environment.
//! Two properties of that command line decide how much trust it demands:
//!
//! - **Version pinning.** An unpinned spec resolves to whatever the registry
//!   serves *now*. A compromised or hijacked release is picked up on the next
//!   restart with no config change and nothing to diff.
//! - **Install scripts.** npm lifecycle scripts (`preinstall`/`postinstall`)
//!   run before the server's own entry point does, so a malicious package
//!   executes code even if its MCP surface is never called.
//!
//! Analysis only — this module never rewrites an operator's command line.
//! Silently injecting `--ignore-scripts` or a version could break a legitimate
//! server; the findings are surfaced as warnings at config-resolve time and as
//! `borg doctor` rows.

/// What is risky about one stdio server's launch command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpPackageRisk {
    /// Package spec exactly as the operator wrote it.
    pub package: String,
    /// The launcher that will fetch it (`npx`, `uvx`, …).
    pub launcher: String,
    /// No exact version pin — the resolved code can change under you.
    pub unpinned: bool,
    /// Install/lifecycle scripts are not suppressed.
    pub runs_install_scripts: bool,
    /// How an exact pin is spelled for this launcher's ecosystem, with any
    /// existing dist-tag or range already stripped — e.g.
    /// `@scope/srv@<version>` for npm, `mcp-git==<version>` for Python.
    /// Ecosystem-specific because telling a `uvx` operator to write
    /// `pkg@1.2.3` (or appending to `pkg@latest`) is an unusable hint.
    pub pin_example: String,
}

impl McpPackageRisk {
    /// Whether anything at all is worth reporting.
    pub fn is_empty(&self) -> bool {
        !self.unpinned && !self.runs_install_scripts
    }

    /// One-line operator-facing summary of the findings, or `None` when clean.
    pub fn summary(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut parts = Vec::new();
        if self.unpinned {
            parts.push(format!(
                "`{}` is not pinned to an exact version, so a restart can silently pull \
                 different code",
                self.package
            ));
        }
        if self.runs_install_scripts {
            parts.push(
                "install scripts are enabled, so package code runs before the server starts \
                 (add `--ignore-scripts`)"
                    .to_string(),
            );
        }
        Some(parts.join("; "))
    }
}

/// Package ecosystem a launcher fetches from — decides how a pin is spelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ecosystem {
    /// npm-style: `name@1.2.3`, scoped as `@scope/name@1.2.3`.
    Npm,
    /// Python-style: `name==1.2.3`.
    Python,
}

/// Strip directory and Windows executable suffix so `/usr/local/bin/npx.cmd`
/// and `npx` classify identically.
fn command_basename(command: &str) -> String {
    let tail = command
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(command)
        .to_ascii_lowercase();
    for suffix in [".exe", ".cmd", ".bat", ".ps1"] {
        if let Some(stripped) = tail.strip_suffix(suffix) {
            return stripped.to_string();
        }
    }
    tail
}

/// Classify the launcher and return the ecosystem plus how many leading
/// non-flag arguments are the launcher's own subcommand rather than the
/// package (`pnpm dlx <pkg>` burns one; `npx <pkg>` burns none).
fn classify_launcher(command: &str, args: &[String]) -> Option<(Ecosystem, usize)> {
    let first_word = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(|a| a.to_ascii_lowercase());

    match command_basename(command).as_str() {
        "npx" | "bunx" => Some((Ecosystem::Npm, 0)),
        "uvx" => Some((Ecosystem::Python, 0)),
        "pnpm" | "yarn" | "bun" if first_word.as_deref() == Some("dlx") => {
            Some((Ecosystem::Npm, 1))
        }
        "bun" if first_word.as_deref() == Some("x") => Some((Ecosystem::Npm, 1)),
        "pipx" | "uv" if first_word.as_deref() == Some("run") => Some((Ecosystem::Python, 1)),
        _ => None,
    }
}

/// npx-family flags that consume the following argument. `--package`'s value
/// IS the package spec, so it is handled separately by the caller.
fn flag_takes_value(flag: &str) -> bool {
    matches!(
        flag,
        "-p" | "--package" | "-c" | "--call" | "--registry" | "--userconfig" | "--from" | "--with"
    )
}

/// Flags that name the package to fetch rather than a mere option.
fn flag_names_package(flag: &str) -> bool {
    matches!(flag, "-p" | "--package" | "--from")
}

/// Is this npm spec pinned to one exact, immutable version?
///
/// Only a bare `MAJOR.MINOR.PATCH` (optionally with a prerelease/build suffix)
/// counts. Range operators (`^`, `~`, `>=`, `*`, `x`) and dist-tags (`latest`,
/// `next`) all resolve differently over time, which is exactly the property
/// that makes an unpinned server risky.
fn npm_spec_is_pinned(spec: &str) -> bool {
    // A scoped package leads with '@' that is part of the NAME, not a version
    // separator — skip it before looking for the separator.
    let searchable = spec.strip_prefix('@').unwrap_or(spec);
    let Some(idx) = searchable.rfind('@') else {
        return false;
    };
    let version = &searchable[idx + 1..];
    version_is_exact(version)
}

/// Is this Python spec pinned via `==`?
///
/// `>=`, `~=`, `!=` and a bare name all leave the resolved version free.
fn python_spec_is_pinned(spec: &str) -> bool {
    let Some(idx) = spec.find("==") else {
        return false;
    };
    // `!=` would be caught by `find("==")` at index-1 of the '!'; reject it.
    if idx > 0 && matches!(spec.as_bytes()[idx - 1], b'!' | b'<' | b'>' | b'~') {
        return false;
    }
    version_is_exact(&spec[idx + 2..])
}

/// The bare package name, with any npm version/dist-tag suffix removed.
fn npm_spec_name(spec: &str) -> &str {
    let scoped = spec.starts_with('@');
    let searchable = if scoped { &spec[1..] } else { spec };
    match searchable.rfind('@') {
        Some(idx) => &spec[..usize::from(scoped) + idx],
        None => spec,
    }
}

/// The bare distribution name, with any PEP 440 version specifier removed.
fn python_spec_name(spec: &str) -> &str {
    spec.find(['=', '<', '>', '!', '~', '['])
        .map_or(spec, |idx| &spec[..idx])
}

/// A concrete `MAJOR.MINOR.PATCH[-pre][+build]` with no range syntax.
fn version_is_exact(version: &str) -> bool {
    if version.is_empty() {
        return false;
    }
    // Any range/wildcard character disqualifies it outright.
    if version
        .chars()
        .any(|c| matches!(c, '^' | '~' | '>' | '<' | '=' | '*' | ' ' | '|'))
    {
        return false;
    }
    // Require all three numeric components; `1` and `1.2` are ranges in npm.
    let core = version
        .split(['-', '+'])
        .next()
        .unwrap_or(version)
        .to_string();
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// Analyse a stdio launch command for supply-chain risk.
///
/// Returns `None` when the command is not a known package-fetching launcher —
/// a server run from an absolute path or a local script pulls nothing from a
/// registry and has nothing to pin.
pub fn stdio_package_risk(command: &str, args: &[String]) -> Option<McpPackageRisk> {
    let (ecosystem, subcommands_to_skip) = classify_launcher(command, args)?;

    let ignore_scripts = args
        .iter()
        .any(|a| a == "--ignore-scripts" || a == "--no-scripts");

    // Walk the argument list to find the package spec: the first positional
    // after the launcher's own subcommand, or the value of an explicit
    // package-naming flag if one appears first.
    let mut package: Option<String> = None;
    let mut skipped = 0usize;
    let mut idx = 0usize;
    while idx < args.len() {
        let arg = &args[idx];
        if arg.starts_with('-') {
            if flag_takes_value(arg) {
                let value = args.get(idx + 1);
                if flag_names_package(arg) {
                    if let Some(v) = value {
                        package = Some(v.clone());
                        break;
                    }
                }
                idx += 2;
                continue;
            }
            idx += 1;
            continue;
        }
        if skipped < subcommands_to_skip {
            skipped += 1;
            idx += 1;
            continue;
        }
        package = Some(arg.clone());
        break;
    }

    // A launcher with no package argument at all (`npx` alone, or only flags)
    // isn't fetching anything identifiable — nothing to report.
    let package = package?;

    let (pinned, pin_example) = match ecosystem {
        Ecosystem::Npm => (
            npm_spec_is_pinned(&package),
            format!("{}@<version>", npm_spec_name(&package)),
        ),
        Ecosystem::Python => (
            python_spec_is_pinned(&package),
            format!("{}==<version>", python_spec_name(&package)),
        ),
    };

    Some(McpPackageRisk {
        package,
        pin_example,
        launcher: command_basename(command),
        unpinned: !pinned,
        // Only the npm ecosystem runs arbitrary lifecycle scripts on install;
        // uvx/pipx build isolated and do not.
        runs_install_scripts: ecosystem == Ecosystem::Npm && !ignore_scripts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    /// The canonical risky spelling: unpinned, scripts enabled. Both findings
    /// must fire — this is the command a copy-pasted MCP README hands you.
    #[test]
    fn unpinned_npx_server_reports_both_risks() {
        let risk = stdio_package_risk("npx", &args(&["-y", "@scope/mcp-server"]))
            .expect("npx is a package-fetching launcher");
        assert_eq!(risk.package, "@scope/mcp-server");
        assert!(
            risk.unpinned,
            "no version means the code can change per boot"
        );
        assert!(
            risk.runs_install_scripts,
            "npx runs lifecycle scripts unless --ignore-scripts is passed"
        );
        assert!(!risk.is_empty());
    }

    /// A fully hardened command line must report nothing, or the warning is
    /// noise the operator learns to ignore.
    #[test]
    fn pinned_scoped_package_with_ignore_scripts_is_clean() {
        let risk = stdio_package_risk(
            "npx",
            &args(&["-y", "--ignore-scripts", "@scope/mcp-server@1.4.2"]),
        )
        .expect("still a launcher");
        assert!(!risk.unpinned, "@scope/name@1.4.2 is an exact pin");
        assert!(!risk.runs_install_scripts);
        assert!(
            risk.is_empty(),
            "a hardened command must produce no finding"
        );
        assert!(risk.summary().is_none());
    }

    /// Scoped names lead with '@' that belongs to the NAME. Treating it as the
    /// version separator would read every scoped package as pinned — exactly
    /// backwards, and it would silence the warning on the most common form.
    #[test]
    fn scoped_name_without_version_is_not_mistaken_for_a_pin() {
        assert!(!npm_spec_is_pinned("@modelcontextprotocol/server-github"));
        assert!(npm_spec_is_pinned(
            "@modelcontextprotocol/server-github@0.6.2"
        ));
        assert!(!npm_spec_is_pinned("server-github"));
        assert!(npm_spec_is_pinned("server-github@2.0.0"));
    }

    /// Ranges and dist-tags resolve differently over time, so none of them is
    /// a pin — this is the whole point of the check.
    #[test]
    fn ranges_and_dist_tags_are_not_pins() {
        for spec in [
            "pkg@latest",
            "pkg@next",
            "pkg@^1.2.3",
            "pkg@~1.2.3",
            "pkg@>=1.0.0",
            "pkg@1.x",
            "pkg@*",
            "pkg@1",
            "pkg@1.2",
            "pkg@",
        ] {
            assert!(!npm_spec_is_pinned(spec), "{spec} must not count as pinned");
        }
        // Prerelease and build metadata are still exact.
        assert!(npm_spec_is_pinned("pkg@1.2.3-rc.1"));
        assert!(npm_spec_is_pinned("pkg@1.2.3+build.5"));
    }

    /// Python launchers pin with `==`; near-miss operators must not pass.
    #[test]
    fn python_pins_require_double_equals() {
        let risk = stdio_package_risk("uvx", &args(&["mcp-server-git"])).expect("uvx launcher");
        assert!(risk.unpinned);
        assert!(
            !risk.runs_install_scripts,
            "uvx builds isolated — flagging install scripts here would be a false positive"
        );

        assert!(python_spec_is_pinned("mcp-server-git==0.6.2"));
        for spec in [
            "mcp-server-git>=0.6.2",
            "mcp-server-git~=0.6.2",
            "mcp-server-git!=0.6.2",
            "mcp-server-git",
        ] {
            assert!(
                !python_spec_is_pinned(spec),
                "{spec} must not count as pinned"
            );
        }
    }

    /// `--package`/`--from` name the package explicitly; reading the following
    /// positional instead would analyse the *binary* name and miss the pin.
    #[test]
    fn explicit_package_flag_wins_over_the_positional() {
        let risk = stdio_package_risk(
            "npx",
            &args(&["-p", "@scope/tools@2.1.0", "--ignore-scripts", "some-bin"]),
        )
        .expect("launcher");
        assert_eq!(risk.package, "@scope/tools@2.1.0");
        assert!(!risk.unpinned);

        let uv = stdio_package_risk("uvx", &args(&["--from", "mcp-git==1.0.0", "mcp-git"]))
            .expect("launcher");
        assert_eq!(uv.package, "mcp-git==1.0.0");
        assert!(!uv.unpinned);
    }

    /// A subcommand-style launcher must not have its subcommand analysed as
    /// the package — `dlx` is not a package name.
    #[test]
    fn subcommand_launchers_skip_their_subcommand() {
        let risk = stdio_package_risk("pnpm", &args(&["dlx", "mcp-server@1.0.0"]))
            .expect("pnpm dlx is a launcher");
        assert_eq!(risk.package, "mcp-server@1.0.0");
        assert!(!risk.unpinned);

        let pipx = stdio_package_risk("pipx", &args(&["run", "mcp-thing"])).expect("pipx run");
        assert_eq!(pipx.package, "mcp-thing");
        assert!(pipx.unpinned);
    }

    /// A server launched from a real path fetches nothing, so there is no pin
    /// to demand. Warning here would be pure noise on the safest setup there
    /// is.
    #[test]
    fn local_binaries_are_not_analysed() {
        assert!(stdio_package_risk("/usr/local/bin/my-mcp-server", &args(&["--stdio"])).is_none());
        assert!(stdio_package_risk("python3", &args(&["server.py"])).is_none());
        assert!(stdio_package_risk("node", &args(&["./dist/index.js"])).is_none());
    }

    /// Path and Windows-suffixed spellings of the same launcher must classify
    /// identically, or the check is trivially bypassed by how it was invoked.
    #[test]
    fn launcher_detection_ignores_path_and_extension() {
        for command in ["npx", "/opt/homebrew/bin/npx", "npx.cmd", r"C:\bin\npx.exe"] {
            let risk = stdio_package_risk(command, &args(&["-y", "pkg"]))
                .unwrap_or_else(|| panic!("{command} must be recognised as npx"));
            assert!(risk.unpinned);
        }
    }

    /// A launcher invoked with no package can't be analysed; returning a
    /// finding with an empty package would be a meaningless warning.
    #[test]
    fn launcher_without_a_package_reports_nothing() {
        assert!(stdio_package_risk("npx", &args(&[])).is_none());
        assert!(stdio_package_risk("npx", &args(&["-y"])).is_none());
    }

    /// The pin example has to be spelled in the launcher's own ecosystem and
    /// must not append to an existing tag/range. `uvx pkg@1.2.3` is not valid
    /// Python, and `pkg@latest@<version>` is not valid anywhere — either one
    /// sends the operator to a broken command line.
    #[test]
    fn pin_example_matches_the_ecosystem_and_strips_existing_specifiers() {
        let npm = stdio_package_risk("npx", &args(&["-y", "@scope/srv@latest"])).expect("launcher");
        assert_eq!(npm.pin_example, "@scope/srv@<version>");

        let unscoped = stdio_package_risk("npx", &args(&["-y", "srv"])).expect("launcher");
        assert_eq!(unscoped.pin_example, "srv@<version>");

        let py = stdio_package_risk("uvx", &args(&["mcp-server-git>=0.6"])).expect("launcher");
        assert_eq!(
            py.pin_example, "mcp-server-git==<version>",
            "a Python launcher must never be told to use npm's `@` pin syntax"
        );
    }

    /// The summary must name the actual package and the actionable fix —
    /// "supply chain risk" alone leaves the operator nowhere to go.
    #[test]
    fn summary_names_the_package_and_the_fix() {
        let risk = stdio_package_risk("npx", &args(&["-y", "@scope/srv"])).expect("launcher");
        let summary = risk.summary().expect("risky command must summarise");
        assert!(summary.contains("@scope/srv"), "got: {summary}");
        assert!(summary.contains("--ignore-scripts"), "got: {summary}");
    }
}
