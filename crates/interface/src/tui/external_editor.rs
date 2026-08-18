use std::io::Write;
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Resolve the user's preferred editor from $VISUAL or $EDITOR, falling back to "vi".
fn resolve_editor() -> String {
    std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| {
            if cfg!(windows) {
                "notepad".to_string()
            } else {
                "vi".to_string()
            }
        })
}

/// Resolve `$BORG_HOME/editor`, creating it 0700 if absent.
///
/// The buffer must not live in the system temp dir: it holds the user's unsent
/// message, and the sandbox grants tools a writable `/tmp`, so a buffer there
/// is readable and rewritable by any sandboxed command for as long as the
/// editor is open. `$BORG_HOME` is deny-write as a whole and `editor/` is
/// deny-read by name (`SandboxPolicy::with_borg_dir_protected`).
///
/// Fails closed. Falling back to the temp dir would quietly reopen the hole
/// this function exists to close.
fn editor_buffer_dir() -> Result<std::path::PathBuf> {
    let home = cortex::paths::resolve_borg_home(None)
        .context("could not open the editor: no writable borg home directory")?;
    let dir = cortex::paths::editor_dir(&home);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating the editor scratch directory at {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting 0700 on {}", dir.display()))?;
    }
    prune_stale_buffers(&dir);
    Ok(dir)
}

/// How long an abandoned scratch buffer may sit before it is swept.
///
/// Comfortably longer than any real editing session, so this never deletes a
/// buffer someone is still typing into.
const STALE_BUFFER_AGE: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

/// Drop scratch buffers left behind by a TUI that never got to run `Drop`.
///
/// `tempfile` removes the buffer on a normal exit, but a SIGKILL or a panic in
/// the terminal restore path skips that — and unlike the system temp dir, which
/// the OS reaps, nothing else ever cleans `$BORG_HOME/editor`. Without this the
/// directory accumulates the user's abandoned drafts in cleartext forever.
///
/// Best-effort by design: a buffer we cannot stat or remove is left alone and
/// retried next time rather than failing the user's `$EDITOR` handoff.
fn prune_stale_buffers(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age >= STALE_BUFFER_AGE);
        if stale && std::fs::remove_file(&path).is_err() {
            tracing::debug!("could not remove stale editor buffer {}", path.display());
        }
    }
}

/// Open the user's external editor with `initial_text` pre-filled.
/// Returns the edited text, or an error if the editor failed.
pub fn open_external_editor(initial_text: &str) -> Result<String> {
    let editor = resolve_editor();

    let mut tmpfile = tempfile::Builder::new()
        .prefix("borg-")
        .suffix(".md")
        .tempfile_in(editor_buffer_dir()?)?;
    tmpfile.write_all(initial_text.as_bytes())?;
    tmpfile.flush()?;
    let path = tmpfile.path().to_path_buf();

    // Split editor command in case it contains args (e.g. "code --wait")
    let parts: Vec<&str> = editor.split_whitespace().collect();
    let default_editor = if cfg!(windows) { "notepad" } else { "vi" };
    let (cmd, args) = parts.split_first().unwrap_or((&default_editor, &[]));

    // stdio is inherited on purpose: `$EDITOR` is a full-screen interactive
    // program and must own the terminal for the duration. Do not null stdin
    // here — unlike the non-interactive helpers elsewhere, this one needs it.
    let status = Command::new(cmd).args(args).arg(&path).status()?;

    if !status.success() {
        bail!("Editor exited with status: {status}");
    }

    let content = std::fs::read_to_string(&path)?;
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    // VISUAL/EDITOR are process-global; `cargo test` runs tests in parallel on
    // one process, so any case that mutates them must hold this lock to avoid
    // observing another case's intermediate state.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        old_visual: Option<String>,
        old_editor: Option<String>,
        old_borg_home: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn acquire() -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            Self {
                old_visual: std::env::var("VISUAL").ok(),
                old_editor: std::env::var("EDITOR").ok(),
                // Restored here too: a case that repoints BORG_HOME at a
                // tempdir must not leave the rest of the suite resolving the
                // real data dir (or a path that no longer exists) if it panics
                // before cleaning up.
                old_borg_home: std::env::var("BORG_HOME").ok(),
                _lock: lock,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.old_visual {
                Some(v) => std::env::set_var("VISUAL", v),
                None => std::env::remove_var("VISUAL"),
            }
            match &self.old_editor {
                Some(v) => std::env::set_var("EDITOR", v),
                None => std::env::remove_var("EDITOR"),
            }
            match &self.old_borg_home {
                Some(v) => std::env::set_var("BORG_HOME", v),
                None => std::env::remove_var("BORG_HOME"),
            }
        }
    }

    #[test]
    fn resolve_editor_defaults_to_platform_editor() {
        let _g = EnvGuard::acquire();
        std::env::remove_var("VISUAL");
        std::env::remove_var("EDITOR");

        let editor = resolve_editor();
        if cfg!(windows) {
            assert_eq!(editor, "notepad");
        } else {
            assert_eq!(editor, "vi");
        }
    }

    #[test]
    fn resolve_editor_uses_visual_first() {
        let _g = EnvGuard::acquire();
        std::env::set_var("VISUAL", "nvim");
        std::env::set_var("EDITOR", "nano");

        let editor = resolve_editor();
        assert_eq!(editor, "nvim");
    }

    #[test]
    fn resolve_editor_falls_back_to_editor() {
        let _g = EnvGuard::acquire();
        std::env::remove_var("VISUAL");
        std::env::set_var("EDITOR", "emacs");

        let editor = resolve_editor();
        assert_eq!(editor, "emacs");
    }

    #[test]
    fn open_external_editor_with_nonexistent_editor_returns_error() {
        let _g = EnvGuard::acquire();
        std::env::set_var("VISUAL", "nonexistent_editor_binary_xyz_42");
        std::env::remove_var("EDITOR");

        let result = open_external_editor("test content");
        assert!(result.is_err());
    }

    /// Spec: the scratch buffer holds the user's unsent message, so it must
    /// live under `$BORG_HOME` — deny-write as a whole and deny-read by name —
    /// and never in the system temp dir, which the sandbox hands tools as
    /// writable. A regression that restores `tempfile()` (no `_in`) puts the
    /// buffer back where a sandboxed command can read and rewrite it.
    #[test]
    fn editor_buffer_dir_is_inside_borg_home_not_the_system_temp_dir() {
        let _g = EnvGuard::acquire();
        let home = tempfile::tempdir().expect("temp borg home");
        std::env::set_var("BORG_HOME", home.path());

        let dir = editor_buffer_dir().expect("resolve editor buffer dir");

        assert!(
            dir.starts_with(home.path()),
            "buffer dir {} escaped BORG_HOME {}",
            dir.display(),
            home.path().display()
        );
        assert_eq!(dir.file_name().and_then(|s| s.to_str()), Some("editor"));
        assert!(dir.is_dir(), "buffer dir must be created, not just named");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir).expect("stat").permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "buffer dir must be owner-only");
        }
    }

    /// Spec: a buffer abandoned by a killed TUI must not live in `$BORG_HOME`
    /// forever. The system temp dir used to be reaped by the OS; nothing reaps
    /// `$BORG_HOME/editor`, so without a sweep every crash leaves the user's
    /// draft in cleartext on disk indefinitely. A buffer someone may still be
    /// editing — anything recent — must survive the sweep.
    #[test]
    fn stale_buffers_are_swept_and_live_ones_are_kept() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();

        let fresh = dir.join("borg-fresh.md");
        std::fs::write(&fresh, b"still editing").expect("write fresh");

        let stale = dir.join("borg-stale.md");
        std::fs::write(&stale, b"abandoned draft").expect("write stale");
        let old = std::time::SystemTime::now() - (STALE_BUFFER_AGE + Duration::from_secs(60));
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .expect("reopen stale")
            .set_modified(old)
            .expect("backdate stale");

        prune_stale_buffers(dir);

        assert!(fresh.exists(), "a recent buffer may still be open");
        assert!(!stale.exists(), "an abandoned buffer must be swept");
    }
}
