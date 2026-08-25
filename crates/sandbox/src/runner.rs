use anyhow::{bail, Context, Result};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::{debug, instrument};

use crate::policy::SandboxPolicy;

/// Output from a script execution.
#[derive(Debug)]
pub struct ScriptOutput {
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
    /// Process exit code (`None` if terminated by signal).
    pub exit_code: Option<i32>,
}

/// Shared subprocess runner for tools and channel scripts.
pub struct ScriptRunner<'a> {
    /// Script runtime (e.g., "python", "node", "bash").
    pub runtime: &'a str,
    /// Path to the script file to execute.
    pub script_path: &'a Path,
    /// Working directory for the subprocess.
    pub work_dir: &'a Path,
    /// Sandbox policy applied to the subprocess.
    pub sandbox_policy: SandboxPolicy,
    /// Maximum execution time in milliseconds.
    pub timeout_ms: u64,
    /// Additional environment variables injected into the subprocess.
    pub extra_env: &'a [(String, String)],
    /// Human-readable name for logging.
    pub name: &'a str,
}

/// SIGKILL the process group led by `pid`, reaching descendants the script
/// backgrounded before it timed out.
///
/// Best-effort: a group that has already exited is not an error. `None` means
/// the child was reaped before we could read its pid, so there is no group left
/// to name.
fn kill_process_group(pid: Option<u32>) {
    match pid {
        #[cfg(unix)]
        Some(pid) => crate::process_group::signal_group_or_members(pid as i32, libc::SIGKILL),
        #[cfg(not(unix))]
        Some(_) => {}
        None => {}
    }
}

impl<'a> ScriptRunner<'a> {
    /// Build a sandboxed command, spawn it, and write input to stdin.
    fn spawn_child(&self, label: &str) -> Result<(tokio::process::Child, std::time::Duration)> {
        let (program, base_args) = resolve_runtime(self.runtime, self.work_dir)?;

        if !self.script_path.exists() {
            bail!("Script not found: {}", self.script_path.display());
        }

        let mut cmd_args = base_args;
        cmd_args.push(self.script_path.to_string_lossy().to_string());

        let sandboxed = self
            .sandbox_policy
            .wrap_command(&program, &cmd_args, self.work_dir);

        if label.is_empty() {
            debug!(
                "Executing '{}': {} {:?}",
                self.name, sandboxed.program, sandboxed.args
            );
        } else {
            debug!(
                "Executing {label} '{}': {} {:?}",
                self.name, sandboxed.program, sandboxed.args
            );
        }

        let mut cmd = Command::new(&sandboxed.program);
        cmd.args(&sandboxed.args)
            .current_dir(self.work_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // Security: clear the parent environment so sandboxed scripts never
        // inherit provider API keys (ANTHROPIC_API_KEY, OPENAI_API_KEY, ...),
        // SSH_AUTH_SOCK, AWS credentials, or any other ambient secret.
        // Only a minimal safe set plus explicitly-declared `extra_env` entries
        // (from the manifest's credential bindings) are forwarded.
        cmd.env_clear();
        apply_minimal_safe_env(&mut cmd);
        for (key, val) in self.extra_env {
            cmd.env(key, val);
        }

        // `kill_on_drop` only reaches the direct child. A script that
        // backgrounds work (`foo &`) leaves those descendants running when the
        // timeout fires. On Linux the bwrap PID namespace collapses the tree
        // for us, but macOS Seatbelt has no namespace — so lead a fresh process
        // group here and signal the whole group on the timeout paths below.
        #[cfg(unix)]
        cmd.process_group(0);

        let child = cmd.kill_on_drop(true).spawn().with_context(|| {
            format!(
                "Failed to spawn '{}' (runtime: {})",
                self.name, self.runtime
            )
        })?;

        let timeout = std::time::Duration::from_millis(self.timeout_ms);
        Ok((child, timeout))
    }

    /// Write input JSON to a child process's stdin.
    async fn write_stdin(&self, child: &mut tokio::process::Child, input_json: &str) {
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            if let Err(e) = stdin.write_all(input_json.as_bytes()).await {
                tracing::warn!("Failed to write input to '{}': {e}", self.name);
            }
            drop(stdin);
        }
    }

    /// Execute the script with the given JSON input and return captured output.
    #[instrument(skip_all, fields(sandbox.name = %self.name))]
    pub async fn run(&self, input_json: &str) -> Result<ScriptOutput> {
        let (mut child, timeout) = self.spawn_child("")?;
        let pid = child.id();
        self.write_stdin(&mut child, input_json).await;

        // Pinned rather than passed by value: on expiry the future — and with
        // it the unreaped child — is still alive, so the group signal lands
        // before anything can reap the pid and let the pgid be recycled.
        let output = {
            let wait = child.wait_with_output();
            tokio::pin!(wait);
            match tokio::time::timeout(timeout, &mut wait).await {
                Ok(result) => result.context("Failed to wait for script process")?,
                Err(_) => {
                    kill_process_group(pid);
                    bail!("'{}' timed out after {}ms", self.name, self.timeout_ms);
                }
            }
        };

        Ok(ScriptOutput {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            exit_code: output.status.code(),
        })
    }

    /// Like `run()`, but invokes `on_output` for each line of stdout/stderr as it arrives.
    #[instrument(skip_all, fields(sandbox.name = %self.name))]
    pub async fn run_streaming<F>(&self, input_json: &str, mut on_output: F) -> Result<ScriptOutput>
    where
        F: FnMut(&str, bool) + Send,
    {
        let (mut child, timeout) = self.spawn_child("streaming")?;
        let pid = child.id();
        self.write_stdin(&mut child, input_json).await;

        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();

        let mut stdout_buf = String::new();
        let mut stderr_buf = String::new();
        let mut callback_bytes: usize = 0;
        let mut stdout_truncated = false;
        let mut stderr_truncated = false;
        // Stop streaming live output to the callback past this point to bound UI/log volume;
        // the full stream is still captured up to MAX_OUTPUT_BYTES.
        const STREAM_CALLBACK_LIMIT: usize = 120 * 1024;
        // 10 MB cap per stream guards against a runaway child exhausting memory.
        const MAX_OUTPUT_BYTES: usize = 10 * 1024 * 1024;

        let streaming_future = async {
            let mut stdout_lines = stdout_pipe.map(|p| BufReader::new(p).lines());
            let mut stderr_lines = stderr_pipe.map(|p| BufReader::new(p).lines());
            let mut stdout_done = stdout_lines.is_none();
            let mut stderr_done = stderr_lines.is_none();

            while !stdout_done || !stderr_done {
                tokio::select! {
                    line = async {
                        match stdout_lines.as_mut() {
                            Some(reader) => reader.next_line().await,
                            None => std::future::pending().await,
                        }
                    }, if !stdout_done => {
                        match line {
                            Ok(Some(l)) => {
                                if callback_bytes < STREAM_CALLBACK_LIMIT {
                                    on_output(&l, false);
                                    callback_bytes += l.len();
                                }
                                if stdout_buf.len() + l.len() < MAX_OUTPUT_BYTES {
                                    stdout_buf.push_str(&l);
                                    stdout_buf.push('\n');
                                } else {
                                    stdout_truncated = true;
                                }
                            }
                            _ => { stdout_done = true; }
                        }
                    }
                    line = async {
                        match stderr_lines.as_mut() {
                            Some(reader) => reader.next_line().await,
                            None => std::future::pending().await,
                        }
                    }, if !stderr_done => {
                        match line {
                            Ok(Some(l)) => {
                                if callback_bytes < STREAM_CALLBACK_LIMIT {
                                    on_output(&l, true);
                                    callback_bytes += l.len();
                                }
                                if stderr_buf.len() + l.len() < MAX_OUTPUT_BYTES {
                                    stderr_buf.push_str(&l);
                                    stderr_buf.push('\n');
                                } else {
                                    stderr_truncated = true;
                                }
                            }
                            _ => { stderr_done = true; }
                        }
                    }
                }
            }

            child.wait().await
        };

        // Pinned for the same reason as in `run`: the group must be signalled
        // while the child is still unreaped. Scoped so the future — and the
        // output buffers it borrows — are released before they are read below.
        let status = {
            tokio::pin!(streaming_future);
            match tokio::time::timeout(timeout, &mut streaming_future).await {
                Ok(result) => result.context("Failed to wait for script process")?,
                Err(_) => {
                    kill_process_group(pid);
                    bail!("'{}' timed out after {}ms", self.name, self.timeout_ms);
                }
            }
        };

        let truncation_note = format!(
            "\n[output truncated at {}MB]",
            MAX_OUTPUT_BYTES / (1024 * 1024)
        );
        if stdout_truncated {
            stdout_buf.push_str(&truncation_note);
        }
        if stderr_truncated {
            stderr_buf.push_str(&truncation_note);
        }

        Ok(ScriptOutput {
            stdout: stdout_buf,
            stderr: stderr_buf,
            exit_code: status.code(),
        })
    }
}

impl ScriptOutput {
    /// Returns `true` if the process exited with code 0.
    pub fn success(&self) -> bool {
        self.exit_code == Some(0)
    }

    /// Consume the output and return `(success, text)` for tool/channel responses.
    pub fn into_result_string(self) -> (bool, String) {
        if self.success() {
            (true, self.stdout)
        } else {
            let code = self.exit_code.unwrap_or(-1);
            let detail = if !self.stderr.is_empty() {
                self.stderr
            } else {
                self.stdout
            };
            (false, format!("Error (exit {code}): {detail}"))
        }
    }
}

/// Run a script in a sandbox, returning (success, output_text).
///
/// Shared core logic used by both tool executors and channel executors.
#[allow(clippy::too_many_arguments)]
#[instrument(skip_all)]
pub async fn run_sandboxed_script(
    runtime: &str,
    script_path: &Path,
    work_dir: &Path,
    sandbox_policy: SandboxPolicy,
    timeout_ms: u64,
    extra_env: &[(String, String)],
    name: &str,
    input_json: &str,
) -> Result<(bool, String)> {
    let output = run_sandboxed_script_detailed(
        runtime,
        script_path,
        work_dir,
        sandbox_policy,
        timeout_ms,
        extra_env,
        name,
        input_json,
    )
    .await?;
    Ok(output.into_result_string())
}

/// As [`run_sandboxed_script`], but keeping the exit code and stderr apart.
///
/// [`ScriptOutput::into_result_string`] folds both into one human-readable
/// string, which is all a tool response needs but not enough to classify a
/// sandbox denial — [`crate::violation::classify`] wants the raw stderr and the
/// exit status separately.
#[allow(clippy::too_many_arguments)]
#[instrument(skip_all)]
pub async fn run_sandboxed_script_detailed(
    runtime: &str,
    script_path: &Path,
    work_dir: &Path,
    sandbox_policy: SandboxPolicy,
    timeout_ms: u64,
    extra_env: &[(String, String)],
    name: &str,
    input_json: &str,
) -> Result<ScriptOutput> {
    let runner = ScriptRunner {
        runtime,
        script_path,
        work_dir,
        sandbox_policy,
        timeout_ms,
        extra_env,
        name,
    };

    runner.run(input_json).await
}

/// Validate that a script path stays within its base directory.
/// Prevents path traversal attacks via symlinks or `..` components.
pub fn validate_script_path(base_dir: &Path, script_name: &str) -> Result<std::path::PathBuf> {
    let script_path = base_dir.join(script_name);
    // A not-yet-created script can't be canonicalized, so the lexical `..` check
    // is all we have. Safe because the caller writes the file at this exact path
    // immediately after; a symlink planted in between is caught on the next call,
    // which does canonicalize.
    if !script_path.exists() {
        if script_name.contains("..") {
            bail!("Script '{script_name}' contains path traversal");
        }
        return Ok(script_path);
    }
    let canonical_script = script_path.canonicalize()?;
    let canonical_dir = base_dir.canonicalize()?;
    if !canonical_script.starts_with(&canonical_dir) {
        bail!("Script '{script_name}' escapes base directory");
    }
    Ok(canonical_script)
}

/// Forward only the parent environment variables that runtimes genuinely
/// need (PATH, HOME, locale, temp dir). Everything else is stripped.
fn apply_minimal_safe_env(cmd: &mut Command) {
    for key in crate::constants::SAFE_ENV_VARS {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }
}

/// Resolve a runtime string to a (program, base_args) pair.
pub fn resolve_runtime(runtime: &str, work_dir: &Path) -> Result<(String, Vec<String>)> {
    match runtime {
        "python" => {
            let python = which::which("python3")
                .or_else(|_| which::which("python"))
                .context("Python not found. Install python3 to use Python tools.")?;
            Ok((python.to_string_lossy().to_string(), vec![]))
        }
        "node" => {
            let node = which::which("node")
                .context("Node.js not found. Install node to use Node tools.")?;
            Ok((node.to_string_lossy().to_string(), vec![]))
        }
        "deno" => {
            let deno =
                which::which("deno").context("Deno not found. Install deno to use Deno tools.")?;
            let allow_read = format!("--allow-read={}", work_dir.display());
            Ok((
                deno.to_string_lossy().to_string(),
                vec!["run".to_string(), allow_read],
            ))
        }
        "bash" => {
            let bash =
                which::which("bash").context("Bash not found. Install bash to use Bash tools.")?;
            Ok((bash.to_string_lossy().to_string(), vec![]))
        }
        other => bail!("Unsupported runtime: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn script_output_success_only_for_exit_zero() {
        // success() is true iff the process exited normally with code 0.
        // Signal termination (None) and any non-zero code (positive or
        // negative) must report failure.
        let cases: &[(Option<i32>, bool)] = &[
            (Some(0), true),
            (Some(1), false),
            (Some(-1), false),
            (Some(127), false),
            (None, false),
        ];
        for (exit_code, expected) in cases.iter().copied() {
            let output = ScriptOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code,
            };
            assert_eq!(output.success(), expected, "exit_code={exit_code:?}");
        }
    }

    #[test]
    fn into_result_string_reports_exit_code_and_prefers_stderr() {
        // Spec: success passes stdout through verbatim; failure folds the exit
        // code and the most useful stream into one message — stderr when the
        // process wrote any, stdout otherwise, and -1 stands in for a
        // signal-terminated process with no exit code at all.
        let cases: &[(Option<i32>, &str, &str, bool, &str)] = &[
            (
                Some(0),
                "tool output",
                "ignored warning",
                true,
                "tool output",
            ),
            (
                Some(3),
                "partial out",
                "boom",
                false,
                "Error (exit 3): boom",
            ),
            (
                Some(7),
                "only stdout",
                "",
                false,
                "Error (exit 7): only stdout",
            ),
            (None, "", "killed", false, "Error (exit -1): killed"),
        ];
        for (exit_code, stdout, stderr, want_ok, want_text) in cases.iter().copied() {
            let output = ScriptOutput {
                stdout: stdout.to_string(),
                stderr: stderr.to_string(),
                exit_code,
            };
            assert_eq!(
                output.into_result_string(),
                (want_ok, want_text.to_string()),
                "exit_code={exit_code:?} stdout={stdout:?} stderr={stderr:?}"
            );
        }
    }

    #[tokio::test]
    async fn run_sandboxed_script_returns_the_real_success_and_output() {
        let dir = tempfile::tempdir().unwrap();
        let ok_script = dir.path().join("ok.sh");
        std::fs::write(&ok_script, "#!/bin/bash\necho 'marker42'\n").unwrap();
        let fail_script = dir.path().join("fail.sh");
        std::fs::write(&fail_script, "#!/bin/bash\necho 'boom' >&2\nexit 7\n").unwrap();

        let (ok, text) = run_sandboxed_script(
            "bash",
            &ok_script,
            dir.path(),
            SandboxPolicy::default(),
            5000,
            &[],
            "ok-test",
            "{}",
        )
        .await
        .unwrap();
        assert!(ok, "a zero-exit script must report success: {text}");
        assert_eq!(text.trim(), "marker42");

        let (ok, text) = run_sandboxed_script(
            "bash",
            &fail_script,
            dir.path(),
            SandboxPolicy::default(),
            5000,
            &[],
            "fail-test",
            "{}",
        )
        .await
        .unwrap();
        assert!(!ok, "a non-zero script must report failure: {text}");
        assert!(
            text.contains("Error (exit 7)") && text.contains("boom"),
            "failure text must carry the exit code and stderr: {text}"
        );
    }

    #[test]
    fn validate_script_path_returns_the_canonical_path_for_an_existing_script() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("tool.sh");
        std::fs::write(&script, "#!/bin/bash\n").unwrap();
        let validated = validate_script_path(dir.path(), "tool.sh").unwrap();
        assert_eq!(
            validated,
            dir.path().canonicalize().unwrap().join("tool.sh"),
            "an in-base script must come back canonicalized, not defaulted"
        );
    }

    #[test]
    fn validate_script_path_allows_a_not_yet_created_script_but_not_traversal() {
        let dir = tempfile::tempdir().unwrap();
        // The caller writes the file at this exact path right after, so the
        // joined (un-canonicalized) path must come back as-is.
        let validated = validate_script_path(dir.path(), "new.sh").unwrap();
        assert_eq!(validated, dir.path().join("new.sh"));

        let err = validate_script_path(dir.path(), "../evil.sh").unwrap_err();
        assert!(
            err.to_string().contains("path traversal"),
            "a dot-dot name must be rejected: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn validate_script_path_rejects_a_symlink_that_escapes_the_base_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base");
        std::fs::create_dir(&base).unwrap();
        let outside = tmp.path().join("outside.sh");
        std::fs::write(&outside, "#!/bin/bash\n").unwrap();
        std::os::unix::fs::symlink(&outside, base.join("evil.sh")).unwrap();

        let err = validate_script_path(&base, "evil.sh").unwrap_err();
        assert!(
            err.to_string().contains("escapes base directory"),
            "a symlink out of the base dir must be rejected: {err}"
        );
    }

    #[tokio::test]
    async fn run_forwards_the_minimal_safe_env_after_clearing() {
        // Spec: `env_clear()` strips everything, and the SAFE_ENV_VARS
        // allowlist restores the handful a runtime genuinely needs. Without
        // the restore, the child would see an empty PATH and HOME.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("safe_env.sh");
        std::fs::write(&script, "#!/bin/bash\necho \"P=$PATH\"\necho \"H=$HOME\"\n").unwrap();
        let runner = ScriptRunner {
            runtime: "bash",
            script_path: &script,
            work_dir: dir.path(),
            sandbox_policy: SandboxPolicy::default(),
            timeout_ms: 5000,
            extra_env: &[],
            name: "safe-env-test",
        };
        let output = runner.run("{}").await.unwrap();
        assert!(output.success(), "script failed: {}", output.stderr);
        let parent_path = std::env::var("PATH").expect("test env has PATH");
        assert!(
            output.stdout.contains(&format!("P={parent_path}")),
            "PATH must be forwarded into the child: {}",
            output.stdout
        );
        let parent_home = std::env::var("HOME").expect("test env has HOME");
        assert!(
            output.stdout.contains(&format!("H={parent_home}")),
            "HOME must be forwarded into the child: {}",
            output.stdout
        );
    }

    #[test]
    fn resolve_bash_returns_absolute_path() {
        let (program, args) = resolve_runtime("bash", Path::new("/tmp")).unwrap();
        assert!(
            program.starts_with('/'),
            "bash should resolve to absolute path: {program}"
        );
        assert!(args.is_empty());
    }

    #[test]
    fn resolve_unsupported_runtime_errors() {
        let result = resolve_runtime("cobol", Path::new("/tmp"));
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Unsupported runtime"));
    }

    #[test]
    fn resolve_empty_runtime_errors() {
        let result = resolve_runtime("", Path::new("/tmp"));
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn run_missing_script_errors() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nonexistent.sh");
        let runner = ScriptRunner {
            runtime: "bash",
            script_path: &missing,
            work_dir: dir.path(),
            sandbox_policy: SandboxPolicy::default(),
            timeout_ms: 5000,
            extra_env: &[],
            name: "test",
        };
        let result = runner.run("{}").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn run_captures_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("hello.sh");
        std::fs::write(&script, "#!/bin/bash\necho 'hello world'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let runner = ScriptRunner {
            runtime: "bash",
            script_path: &script,
            work_dir: dir.path(),
            sandbox_policy: SandboxPolicy::default(),
            timeout_ms: 5000,
            extra_env: &[],
            name: "test",
        };
        let output = runner.run("{}").await.unwrap();
        assert!(output.success());
        assert_eq!(output.stdout.trim(), "hello world");
    }

    #[tokio::test]
    async fn run_captures_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("stderr.sh");
        std::fs::write(&script, "#!/bin/bash\necho 'error msg' >&2\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let runner = ScriptRunner {
            runtime: "bash",
            script_path: &script,
            work_dir: dir.path(),
            sandbox_policy: SandboxPolicy::default(),
            timeout_ms: 5000,
            extra_env: &[],
            name: "test",
        };
        let output = runner.run("{}").await.unwrap();
        assert!(!output.success());
        assert!(output.stderr.contains("error msg"));
    }

    #[tokio::test]
    async fn run_passes_stdin_to_script() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("cat.sh");
        std::fs::write(&script, "#!/bin/bash\ncat\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let runner = ScriptRunner {
            runtime: "bash",
            script_path: &script,
            work_dir: dir.path(),
            sandbox_policy: SandboxPolicy::default(),
            timeout_ms: 5000,
            extra_env: &[],
            name: "test",
        };
        let output = runner.run(r#"{"key":"value"}"#).await.unwrap();
        assert!(output.success());
        assert_eq!(output.stdout.trim(), r#"{"key":"value"}"#);
    }

    #[tokio::test]
    async fn run_does_not_leak_parent_env_to_child() {
        let leak_key = format!("BORG_TEST_SECRET_LEAK_{}", std::process::id());
        // SAFETY: single-threaded test setup; no other thread is reading env.
        unsafe {
            std::env::set_var(&leak_key, "should-not-leak");
        }

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("dump.sh");
        std::fs::write(
            &script,
            format!("#!/bin/bash\necho \"VAL=${{{leak_key}:-(unset)}}\"\n"),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let runner = ScriptRunner {
            runtime: "bash",
            script_path: &script,
            work_dir: dir.path(),
            sandbox_policy: SandboxPolicy::default(),
            timeout_ms: 5000,
            extra_env: &[],
            name: "leak-test",
        };
        let output = runner.run("{}").await.unwrap();

        // SAFETY: cleanup
        unsafe {
            std::env::remove_var(&leak_key);
        }

        assert!(output.success(), "script failed: {}", output.stderr);
        assert!(
            output.stdout.contains("VAL=(unset)"),
            "parent env leaked into sandboxed child: {}",
            output.stdout
        );
    }

    #[tokio::test]
    async fn run_injects_extra_env() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("env.sh");
        std::fs::write(&script, "#!/bin/bash\necho \"$TEST_VAR\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let env = vec![("TEST_VAR".to_string(), "injected_value".to_string())];
        let runner = ScriptRunner {
            runtime: "bash",
            script_path: &script,
            work_dir: dir.path(),
            sandbox_policy: SandboxPolicy::default(),
            timeout_ms: 5000,
            extra_env: &env,
            name: "test",
        };
        let output = runner.run("{}").await.unwrap();
        assert!(output.success());
        assert_eq!(output.stdout.trim(), "injected_value");
    }

    #[tokio::test]
    async fn run_timeout_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("slow.sh");
        std::fs::write(&script, "#!/bin/bash\nsleep 60\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let runner = ScriptRunner {
            runtime: "bash",
            script_path: &script,
            work_dir: dir.path(),
            sandbox_policy: SandboxPolicy::default(),
            timeout_ms: 100, // very short timeout
            extra_env: &[],
            name: "slow-test",
        };
        let result = runner.run("{}").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("timed out"),
            "expected timeout error, got: {err}"
        );
    }

    /// Spec: a timed-out script must not leave the work it backgrounded
    /// running. `kill_on_drop` alone reaches only the direct child, and macOS
    /// Seatbelt has no PID namespace to collapse the rest of the tree — so
    /// without the process group the `sleep 300` here outlives the timeout.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_timeout_reaps_backgrounded_grandchildren() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("grandchild.pid");
        let script = dir.path().join("background.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/bash\nsleep 300 &\necho $! > '{}'\nsleep 300\n",
                pid_file.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let work_dir = dir.path().to_string_lossy().to_string();
        let runner = ScriptRunner {
            runtime: "bash",
            script_path: &script,
            work_dir: dir.path(),
            // The script must be able to report the grandchild's pid back to us.
            sandbox_policy: SandboxPolicy {
                fs_read: vec![work_dir.clone()],
                fs_write: vec![work_dir],
                ..Default::default()
            },
            timeout_ms: 1500,
            extra_env: &[],
            name: "background-test",
        };

        let result = runner.run("{}").await;
        assert!(result.is_err(), "the script should have timed out");

        let recorded = std::fs::read_to_string(&pid_file)
            .expect("the script must have recorded its grandchild's pid");
        let grandchild: i32 = recorded.trim().parse().expect("a pid");

        // SIGKILL delivery is asynchronous, so poll rather than probe once.
        let mut alive = true;
        for _ in 0..100 {
            // SAFETY: FFI call to `kill` with signal 0 — an existence probe
            // that delivers nothing and cannot affect the target.
            if unsafe { libc::kill(grandchild, 0) } != 0 {
                alive = false;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            !alive,
            "grandchild {grandchild} survived the timeout's group kill"
        );
    }

    #[tokio::test]
    async fn run_streaming_captures_lines() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("multi.sh");
        std::fs::write(
            &script,
            "#!/bin/bash\necho 'line1'\necho 'line2'\necho 'line3'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut lines = Vec::new();
        let runner = ScriptRunner {
            runtime: "bash",
            script_path: &script,
            work_dir: dir.path(),
            sandbox_policy: SandboxPolicy::default(),
            timeout_ms: 5000,
            extra_env: &[],
            name: "test",
        };
        let output = runner
            .run_streaming("{}", |line, is_stderr| {
                lines.push((line.to_string(), is_stderr));
            })
            .await
            .unwrap();
        assert!(output.success());
        assert!(output.stdout.contains("line1"));
        assert!(output.stdout.contains("line2"));
        assert!(output.stdout.contains("line3"));
        assert!(lines.iter().any(|(l, s)| l == "line1" && !s));
    }

    #[tokio::test]
    async fn run_streaming_bounds_the_callback_but_captures_multi_megabyte_output() {
        // Spec: live streaming to the callback stops at 120KB to bound UI/log
        // volume, while the captured buffer keeps everything up to the 10MB
        // cap — so a ~2.6MB stream must arrive complete and untruncated.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("big.sh");
        const LINE_LEN: usize = 64 * 1024;
        const LINES: usize = 40;
        std::fs::write(
            &script,
            format!(
                "#!/bin/bash\nline=$(head -c {LINE_LEN} /dev/zero | tr '\\0' x)\n\
                 for i in $(seq 1 {LINES}); do echo \"$line\"; done\n"
            ),
        )
        .unwrap();
        let mut callback_lines = 0usize;
        let runner = ScriptRunner {
            runtime: "bash",
            script_path: &script,
            work_dir: dir.path(),
            sandbox_policy: SandboxPolicy::default(),
            timeout_ms: 30_000,
            extra_env: &[],
            name: "big-output-test",
        };
        let output = runner
            .run_streaming("{}", |line, _is_stderr| {
                assert_eq!(line.len(), LINE_LEN);
                callback_lines += 1;
            })
            .await
            .unwrap();
        assert!(output.success(), "script failed: {}", output.stderr);
        // 64KB lines against the 120KB callback budget: the first two lines
        // stream (0 and 64KB are under the limit), the third does not.
        assert_eq!(
            callback_lines, 2,
            "the callback budget must be 120KB — enough for exactly two 64KB lines"
        );
        // The full stream stays captured: 40 lines of 64KB plus a newline each.
        assert_eq!(
            output.stdout.len(),
            LINES * (LINE_LEN + 1),
            "the capture cap is 10MB, so ~2.6MB must survive untruncated"
        );
        assert!(
            !output.stdout.contains("[output truncated"),
            "no truncation note may appear below the 10MB cap"
        );
    }

    #[tokio::test]
    async fn run_streaming_captures_stderr_lines() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("mixed.sh");
        std::fs::write(
            &script,
            "#!/bin/bash\necho 'stdout_line'\necho 'stderr_line' >&2\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut stderr_seen = false;
        let runner = ScriptRunner {
            runtime: "bash",
            script_path: &script,
            work_dir: dir.path(),
            sandbox_policy: SandboxPolicy::default(),
            timeout_ms: 5000,
            extra_env: &[],
            name: "test",
        };
        let output = runner
            .run_streaming("{}", |_line, is_stderr| {
                if is_stderr {
                    stderr_seen = true;
                }
            })
            .await
            .unwrap();
        assert!(output.success());
        assert!(stderr_seen, "should have received stderr callback");
        assert!(output.stderr.contains("stderr_line"));
    }
}
