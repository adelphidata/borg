use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};

/// Per-session execution context for workspace-sensitive operations.
#[derive(Clone)]
pub struct ExecutionContext {
    workspace_cwd: PathBuf,
    workspace_root: PathBuf,
    enforce_workspace_root: bool,
    text_file_bridge: Option<Arc<dyn TextFileBridge>>,
    /// Optional handle to the daemon-side subprocess registry. When
    /// present, tool handlers that spawn child processes (`run_shell`,
    /// browser launch, hooks) register their PID for visibility in
    /// `borg status` / `/doctor` and inclusion in the self-healing
    /// sweep. `None` for CLI/test paths that don't have a running
    /// daemon — the spawn proceeds as before, just without tracking.
    process_registry: Option<crate::process_registry::ProcessRegistry>,
    /// Optional session id used when registering spawned processes;
    /// drives the "kill all my children" UX when implemented and is
    /// surfaced in `borg status` so the operator can attribute an
    /// orphan to its conversation.
    session_id: Option<String>,
}

/// Optional ACP-backed text file access for IDE buffer reads/writes.
pub trait TextFileBridge: Send + Sync {
    /// Whether IDE-backed text reads are supported.
    fn supports_read_text_file(&self) -> bool;
    /// Whether IDE-backed text writes are supported.
    fn supports_write_text_file(&self) -> bool;
    /// Read a text buffer from the client-side IDE.
    fn read_text_file(&self, path: &Path) -> Result<String>;
    /// Write a full text buffer back to the client-side IDE.
    fn write_text_file(&self, path: &Path, content: &str) -> Result<()>;
}

impl std::fmt::Debug for ExecutionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionContext")
            .field("workspace_cwd", &self.workspace_cwd)
            .field("workspace_root", &self.workspace_root)
            .field("enforce_workspace_root", &self.enforce_workspace_root)
            .field("has_text_file_bridge", &self.text_file_bridge.is_some())
            .finish()
    }
}

impl ExecutionContext {
    /// Build an unrestricted context from the provided cwd without canonicalization.
    pub fn unrestricted(workspace_cwd: PathBuf) -> Self {
        Self {
            workspace_root: workspace_cwd.clone(),
            workspace_cwd,
            enforce_workspace_root: false,
            text_file_bridge: None,
            process_registry: None,
            session_id: None,
        }
    }

    /// Build the default context from the process cwd with no root enforcement.
    pub fn from_process() -> Result<Self> {
        let cwd =
            std::env::current_dir().context("Failed to determine current working directory")?;
        let canonical_cwd = cwd.canonicalize().unwrap_or(cwd);
        Ok(Self::unrestricted(canonical_cwd))
    }

    /// Build a workspace-scoped context that enforces containment within `workspace_root`.
    pub fn for_workspace(workspace_cwd: PathBuf, workspace_root: PathBuf) -> Result<Self> {
        if !workspace_cwd.is_absolute() {
            bail!("workspace_cwd must be absolute");
        }
        if !workspace_root.is_absolute() {
            bail!("workspace_root must be absolute");
        }
        let workspace_root = workspace_root
            .canonicalize()
            .context("Failed to canonicalize workspace root")?;
        let workspace_cwd = workspace_cwd
            .canonicalize()
            .context("Failed to canonicalize workspace cwd")?;
        if !path_within_root(&workspace_cwd, &workspace_root) {
            bail!(
                "workspace_cwd {} is outside workspace_root {}",
                workspace_cwd.display(),
                workspace_root.display()
            );
        }
        Ok(Self {
            workspace_cwd,
            workspace_root,
            enforce_workspace_root: true,
            text_file_bridge: None,
            process_registry: None,
            session_id: None,
        })
    }

    pub fn workspace_cwd(&self) -> &Path {
        &self.workspace_cwd
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn enforce_workspace_root(&self) -> bool {
        self.enforce_workspace_root
    }

    /// Attach an optional IDE-backed text-file bridge.
    pub fn with_text_file_bridge(mut self, bridge: Option<Arc<dyn TextFileBridge>>) -> Self {
        self.text_file_bridge = bridge;
        self
    }

    /// Access the optional IDE-backed text-file bridge.
    pub fn text_file_bridge(&self) -> Option<&Arc<dyn TextFileBridge>> {
        self.text_file_bridge.as_ref()
    }

    /// Attach a process registry handle. Tool handlers that spawn
    /// children will register their PID against this registry.
    pub fn with_process_registry(
        mut self,
        registry: Option<crate::process_registry::ProcessRegistry>,
    ) -> Self {
        self.process_registry = registry;
        self
    }

    /// Access the optional process registry.
    pub fn process_registry(&self) -> Option<&crate::process_registry::ProcessRegistry> {
        self.process_registry.as_ref()
    }

    /// Attach an owning session id. Used as the `session_id` field on
    /// registered subprocess entries.
    pub fn with_session_id(mut self, session_id: Option<String>) -> Self {
        self.session_id = session_id;
        self
    }

    /// Access the owning session id, if any.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Resolve a user-provided path that must already exist.
    pub fn resolve_existing_path(&self, raw: &str) -> Result<PathBuf> {
        let candidate = self.resolve_raw_path(raw);
        let canonical = candidate
            .canonicalize()
            .with_context(|| format!("Failed to resolve path {}", candidate.display()))?;
        self.ensure_within_workspace(&canonical)?;
        Ok(canonical)
    }

    /// Resolve a user-provided path for a write operation. The final path may not exist yet.
    ///
    /// Containment check: walk upward from `parent` to the nearest existing
    /// ancestor, canonicalize *that* (so symlinks in the existing portion
    /// resolve), and then re-attach the missing tail. The synthesized full
    /// path is what we test for containment within the workspace root.
    pub fn resolve_path_for_write(&self, raw: &str) -> Result<PathBuf> {
        let candidate = self.resolve_raw_path(raw);
        let containment_target = synthesize_canonical_for_write(&candidate)?;
        self.ensure_within_workspace(&containment_target)?;
        Ok(candidate)
    }

    /// Resolve a user-provided path that may not exist yet.
    pub fn resolve_path_maybe_missing(&self, raw: &str) -> Result<PathBuf> {
        let candidate = self.resolve_raw_path(raw);
        if candidate.exists() {
            let canonical = candidate
                .canonicalize()
                .with_context(|| format!("Failed to resolve path {}", candidate.display()))?;
            self.ensure_within_workspace(&canonical)?;
            Ok(canonical)
        } else {
            let containment_target = synthesize_canonical_for_write(&candidate)?;
            self.ensure_within_workspace(&containment_target)?;
            Ok(candidate)
        }
    }

    pub fn ensure_within_workspace(&self, path: &Path) -> Result<()> {
        if self.enforce_workspace_root && !path_within_root(path, &self.workspace_root) {
            bail!(
                "Path {} is outside the workspace root {}",
                path.display(),
                self.workspace_root.display()
            );
        }
        Ok(())
    }

    fn resolve_raw_path(&self, raw: &str) -> PathBuf {
        let expanded = shellexpand::tilde(raw).to_string();
        let path = Path::new(&expanded);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace_cwd.join(path)
        }
    }
}

fn path_within_root(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

/// Build a canonicalized path suitable for workspace containment checks when
/// the candidate (or some of its ancestors) does not yet exist on disk.
///
/// Walks upward to the deepest existing ancestor, canonicalizes that (so any
/// symlinks in the existing portion resolve), then re-attaches the missing
/// tail components. Returns the canonical existing prefix joined with the
/// non-existent suffix — a path safe to compare against `workspace_root` via
/// `path_within_root`. `..` components in the suffix are normalized.
///
/// **Caller invariant:** `candidate` must be absolute. Both call sites in
/// this module funnel through `resolve_raw_path`, which always anchors
/// relative inputs at `workspace_cwd` (an absolute path), so this holds.
/// If a future caller passes a relative path the walk-up would terminate
/// at the empty parent and we'd silently accept a lexically-normalized
/// path with no security context — bail loudly instead.
fn synthesize_canonical_for_write(candidate: &Path) -> Result<PathBuf> {
    if !candidate.is_absolute() {
        bail!(
            "synthesize_canonical_for_write requires an absolute candidate; got {}",
            candidate.display()
        );
    }
    let mut probe: &Path = candidate;
    let existing: &Path = loop {
        if probe.exists() {
            break probe;
        }
        match probe.parent() {
            Some(p) if !p.as_os_str().is_empty() => probe = p,
            // `/` always exists on Unix, so this is unreachable for absolute
            // inputs. Keep it as a defensive bail rather than silent fallback.
            _ => bail!(
                "no existing ancestor for {} — cannot canonicalize for containment check",
                candidate.display()
            ),
        }
    };
    let canonical_prefix = existing
        .canonicalize()
        .with_context(|| format!("Failed to resolve existing ancestor {}", existing.display()))?;
    let suffix = candidate.strip_prefix(existing).unwrap_or(Path::new(""));
    let mut joined = canonical_prefix;
    for component in suffix.components() {
        match component {
            std::path::Component::ParentDir => {
                joined.pop();
            }
            std::path::Component::CurDir => {}
            other => joined.push(other.as_os_str()),
        }
    }
    Ok(joined)
}

#[cfg(test)]
mod tests {
    use super::ExecutionContext;
    use std::path::PathBuf;

    fn workspace_ctx() -> (tempfile::TempDir, ExecutionContext) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).expect("mkdir root");
        let ctx = ExecutionContext::for_workspace(root.clone(), root.clone()).expect("ctx");
        (tmp, ctx)
    }

    #[test]
    fn workspace_context_rejects_relative_escape() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("root");
        let inside = root.join("inside");
        std::fs::create_dir_all(&inside).expect("mkdir");
        let ctx = ExecutionContext::for_workspace(inside.clone(), root.clone()).expect("context");
        let err = ctx
            .resolve_path_for_write("../../outside.txt")
            .expect_err("must fail");
        assert!(err.to_string().contains("outside the workspace root"));
    }

    #[test]
    fn workspace_context_rejects_absolute_outside_root() {
        let (_tmp, ctx) = workspace_ctx();
        let err = ctx
            .resolve_path_for_write("/etc/passwd")
            .expect_err("must reject absolute outside root");
        assert!(err.to_string().contains("outside the workspace root"));
    }

    #[test]
    fn workspace_context_allows_writes_to_nonexistent_nested_dirs() {
        // Regression: parent.canonicalize() used to fail on missing ancestors,
        // making legitimate writes to new subdirectories impossible inside an
        // ACP workspace.
        let (_tmp, ctx) = workspace_ctx();
        let path = ctx
            .resolve_path_for_write("new_dir/nested/file.txt")
            .expect("nested write must be permitted");
        assert!(path.ends_with("new_dir/nested/file.txt"));
    }

    #[test]
    fn workspace_context_rejects_traversal_when_prefix_exists() {
        // Anchor the walk-up mid-path: create `a/b` so the helper canonicalizes
        // there, then the suffix `../../../outside/file.txt` walks past the
        // root via the pop-loop. Catches a refactor that processes the suffix
        // textually before the canonical-prefix join.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(root.join("a/b")).expect("mkdir a/b");
        std::fs::create_dir_all(&outside).expect("mkdir outside");
        let ctx = ExecutionContext::for_workspace(root.clone(), root.clone()).expect("ctx");
        let err = ctx
            .resolve_path_for_write("a/b/../../../outside/file.txt")
            .expect_err("traversal must be rejected");
        assert!(err.to_string().contains("outside the workspace root"));
    }

    #[test]
    fn resolve_path_maybe_missing_allows_nested_nonexistent() {
        // Direct coverage for the walk-up branch in `resolve_path_maybe_missing`
        // (the integration tests cover it indirectly via apply_patch).
        let (_tmp, ctx) = workspace_ctx();
        let path = ctx
            .resolve_path_maybe_missing("a/b/c/buffer.txt")
            .expect("missing nested path under root must be permitted");
        assert!(path.ends_with("a/b/c/buffer.txt"));
    }

    #[test]
    fn resolve_path_maybe_missing_rejects_traversal_via_missing_tail() {
        let (_tmp, ctx) = workspace_ctx();
        let err = ctx
            .resolve_path_maybe_missing("a/../../../escape.txt")
            .expect_err("traversal via missing tail must be rejected");
        assert!(err.to_string().contains("outside the workspace root"));
    }

    #[test]
    fn synthesize_canonical_rejects_relative_input() {
        // The helper requires absolute inputs; relative paths would silently
        // terminate the walk-up at the empty parent. Bail instead.
        use std::path::Path;
        let err = super::synthesize_canonical_for_write(Path::new("foo/bar"))
            .expect_err("relative input must be rejected");
        assert!(err.to_string().contains("requires an absolute candidate"));
    }

    #[cfg(unix)]
    #[test]
    fn workspace_context_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&root).expect("mkdir root");
        std::fs::create_dir_all(&outside).expect("mkdir outside");
        symlink(&outside, root.join("escape")).expect("symlink");

        let ctx = ExecutionContext::for_workspace(root.clone(), root.clone()).expect("ctx");
        let err = ctx
            .resolve_path_for_write("escape/file.txt")
            .expect_err("symlink to outside must be rejected");
        assert!(err.to_string().contains("outside the workspace root"));

        // resolve_existing_path on the symlink target itself: also rejected.
        std::fs::write(outside.join("file.txt"), "x").expect("write");
        let err = ctx
            .resolve_existing_path("escape/file.txt")
            .expect_err("existing symlink-traversed read must be rejected");
        assert!(err.to_string().contains("outside the workspace root"));
    }

    #[test]
    fn workspace_context_accepts_in_root_writes() {
        let (_tmp, ctx) = workspace_ctx();
        let path = ctx
            .resolve_path_for_write("file.txt")
            .expect("in-root write must succeed");
        assert!(path.ends_with("file.txt"));
    }

    #[test]
    fn process_context_allows_absolute_path_without_root_enforcement() {
        // Documents the contract: resolve_path_for_write returns the user's
        // literal candidate; canonicalization is internal to the containment
        // check. macOS resolves /tmp -> /private/tmp under the hood, but the
        // returned path must still equal the input verbatim so callers don't
        // observe symlink rewrites they didn't ask for.
        let ctx = ExecutionContext::from_process().expect("process context");
        let path = ctx
            .resolve_path_for_write("/tmp/demo.txt")
            .expect("resolved");
        assert_eq!(path, PathBuf::from("/tmp/demo.txt"));
        assert!(!ctx.enforce_workspace_root());
    }

    #[test]
    fn for_workspace_rejects_non_absolute_inputs() {
        let err = ExecutionContext::for_workspace(PathBuf::from("relative"), PathBuf::from("/tmp"))
            .expect_err("relative cwd must fail");
        assert!(err.to_string().contains("workspace_cwd must be absolute"));
        let err = ExecutionContext::for_workspace(PathBuf::from("/tmp"), PathBuf::from("relative"))
            .expect_err("relative root must fail");
        assert!(err.to_string().contains("workspace_root must be absolute"));
    }

    #[test]
    fn for_workspace_rejects_cwd_outside_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("root");
        let stranger = tmp.path().join("stranger");
        std::fs::create_dir_all(&root).expect("mkdir root");
        std::fs::create_dir_all(&stranger).expect("mkdir stranger");
        let err = ExecutionContext::for_workspace(stranger, root)
            .expect_err("cwd outside root must fail");
        assert!(err.to_string().contains("is outside workspace_root"));
    }

    #[test]
    fn resolve_existing_path_errors_when_target_missing() {
        // resolve_existing_path is the read-side contract: caller relies on the
        // returned path actually existing. Distinct from resolve_path_for_write,
        // which intentionally tolerates missing leaves.
        let (_tmp, ctx) = workspace_ctx();
        let err = ctx
            .resolve_existing_path("missing.txt")
            .expect_err("missing path must error");
        assert!(
            err.to_string().contains("Failed to resolve path"),
            "got: {err}"
        );
    }
}
