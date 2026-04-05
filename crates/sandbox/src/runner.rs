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
        self.write_stdin(&mut child, input_json).await;

        let output = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| anyhow::anyhow!("'{}' timed out after {}ms", self.name, self.timeout_ms))?
            .context("Failed to wait for script process")?;

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
        self.write_stdin(&mut child, input_json).await;

        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();

        let mut stdout_buf = String::new();
        let mut stderr_buf = String::new();
        let mut callback_bytes: usize = 0;
        let mut stdout_truncated = false;
        let mut stderr_truncated = false;
        const STREAM_CALLBACK_LIMIT: usize = 120 * 1024;
        const MAX_OUTPUT_BYTES: usize = 10 * 1024 * 1024; // 10 MB cap per stream

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

        let status = tokio::time::timeout(timeout, streaming_future)
            .await
            .map_err(|_| anyhow::anyhow!("'{}' timed out after {}ms", self.name, self.timeout_ms))?
            .context("Failed to wait for script process")?;

        if stdout_truncated {
            stdout_buf.push_str("\n[output truncated at 10MB]");
        }
        if stderr_truncated {
            stderr_buf.push_str("\n[output truncated at 10MB]");
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
    let runner = ScriptRunner {
        runtime,
        script_path,
        work_dir,
        sandbox_policy,
        timeout_ms,
        extra_env,
        name,
    };

    let output = runner.run(input_json).await?;
    Ok(output.into_result_string())
}

/// Validate that a script path stays within its base directory.
/// Prevents path traversal attacks via symlinks or `..` components.
pub fn validate_script_path(base_dir: &Path, script_name: &str) -> Result<std::path::PathBuf> {
    let script_path = base_dir.join(script_name);
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
    const SAFE_VARS: &[&str] = &[
        "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "TMPDIR",
        "TEMP", "TMP", "TERM",
    ];
    for key in SAFE_VARS {
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

    // -- ScriptOutput --

    #[test]
    fn script_output_success_with_zero_exit() {
        let output = ScriptOutput {
            stdout: "ok".into(),
            stderr: String::new(),
            exit_code: Some(0),
        };
        assert!(output.success());
    }

    #[test]
    fn script_output_failure_with_nonzero_exit() {
        let output = ScriptOutput {
            stdout: String::new(),
            stderr: "err".into(),
            exit_code: Some(1),
        };
        assert!(!output.success());
    }

    #[test]
    fn script_output_failure_with_none_exit() {
        let output = ScriptOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
        };
        assert!(!output.success());
    }

    #[test]
    fn script_output_failure_with_signal_exit() {
        let output = ScriptOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(-1),
        };
        assert!(!output.success());
    }

    // -- resolve_runtime --

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
    fn resolve_python_returns_absolute_path() {
        // python3 should be available in most test environments
        if let Ok((program, args)) = resolve_runtime("python", Path::new("/tmp")) {
            assert!(
                program.starts_with('/'),
                "python should resolve to absolute path: {program}"
            );
            assert!(args.is_empty());
        }
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

    // -- ScriptRunner::run --

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

    // -- ScriptRunner::run_streaming --

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
