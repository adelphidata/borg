//! Audio playback via system players (`afplay` / `ffplay` / `aplay`).
//!
//! Mirrors `reference/hermes-agent/tools/voice_mode.py::play_audio_file`
//! exactly — no in-process audio output crate, just a small fallthrough
//! over the players that exist on every supported host. Avoiding a
//! `rodio`/`cpal`-output dependency keeps the binary lean and dodges
//! the cross-platform output bugs that motivated hermes to use the
//! same strategy.
//!
//! [`stop_playback`] interrupts the currently playing process. The
//! voice loop calls it on `/voice off` so playback doesn't keep
//! talking after the user disabled the mode.
//!
//! Player resolution is dependency-injected via [`PlayerResolver`] so
//! tests can drive the resolution order without touching the real
//! filesystem.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

/// Lock the active-child slot, recovering on poisoning.
///
/// The audio-player process is owned by this slot; if the wait-loop
/// thread panics while holding it, we still want `stop_playback()` to
/// be able to kill the child the next time it's called. Recovering
/// the inner data is strictly better than tearing down the whole TUI.
fn lock_slot() -> MutexGuard<'static, Option<Child>> {
    match active_slot().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 5 minutes — longest plausible TTS reply. Same cap hermes uses.
const PLAYBACK_HARD_TIMEOUT: Duration = Duration::from_secs(300);

/// One playback command — `(program, args)`.
#[derive(Debug, Clone)]
pub struct PlayerInvocation {
    pub program: String,
    pub args: Vec<String>,
}

/// Looks up which players exist and in what order. The default impl
/// uses [`which::which`]; tests provide a stub.
pub trait PlayerResolver: Send + Sync {
    fn candidates(&self, file: &Path) -> Vec<PlayerInvocation>;
}

/// Default resolver: `afplay` (macOS), then `ffplay`, then `aplay`
/// (Linux). Same order as hermes voice_mode.py:1001-1006 so a host
/// with afplay+ffplay both installed picks afplay (matches CLI parity).
pub struct DefaultPlayerResolver;

impl PlayerResolver for DefaultPlayerResolver {
    fn candidates(&self, file: &Path) -> Vec<PlayerInvocation> {
        let mut out = Vec::new();
        let path = file.to_string_lossy().into_owned();
        if cfg!(target_os = "macos") && which::which("afplay").is_ok() {
            out.push(PlayerInvocation {
                program: "afplay".into(),
                args: vec![path.clone()],
            });
        }
        if which::which("ffplay").is_ok() {
            out.push(PlayerInvocation {
                program: "ffplay".into(),
                args: vec![
                    "-nodisp".into(),
                    "-autoexit".into(),
                    "-loglevel".into(),
                    "quiet".into(),
                    path.clone(),
                ],
            });
        }
        if cfg!(target_os = "linux") && which::which("aplay").is_ok() {
            out.push(PlayerInvocation {
                program: "aplay".into(),
                args: vec!["-q".into(), path],
            });
        }
        out
    }
}

static ACTIVE_CHILD: OnceLock<Mutex<Option<Child>>> = OnceLock::new();

fn active_slot() -> &'static Mutex<Option<Child>> {
    ACTIVE_CHILD.get_or_init(|| Mutex::new(None))
}

/// Play `file` through the first available system audio player. Blocks
/// the current task on a `spawn_blocking` boundary so async callers
/// don't stall the runtime. Returns `Err` only when no player is
/// available *or* every available player failed to spawn — a player
/// that started and then exited non-zero (e.g. unsupported codec) is
/// treated as success because the audio decision was the player's, not
/// ours.
pub async fn play_audio_file(file: &Path) -> Result<()> {
    let path = file.to_path_buf();
    let resolver = DefaultPlayerResolver;
    tokio::task::spawn_blocking(move || play_audio_file_with(&path, &resolver))
        .await
        .context("playback task panicked")?
}

/// Synchronous variant — used by `play_audio_file` and tests.
pub fn play_audio_file_with(file: &Path, resolver: &dyn PlayerResolver) -> Result<()> {
    if !file.exists() {
        return Err(anyhow!("audio file does not exist: {}", file.display()));
    }
    let candidates = resolver.candidates(file);
    if candidates.is_empty() {
        return Err(anyhow!(
            "no audio player available (need afplay, ffplay, or aplay)"
        ));
    }
    let mut last_err: Option<anyhow::Error> = None;
    for invocation in candidates {
        match spawn_and_wait(&invocation) {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::debug!(player = %invocation.program, error = %e, "player failed; trying next");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("playback failed with no error reported")))
}

fn spawn_and_wait(invocation: &PlayerInvocation) -> Result<()> {
    let mut cmd = Command::new(&invocation.program);
    cmd.args(&invocation.args)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", invocation.program))?;
    {
        let mut slot = lock_slot();
        *slot = Some(child);
    }
    let start = std::time::Instant::now();
    loop {
        // Hold the slot's lock briefly per poll so stop_playback can
        // race in and kill the child without blocking forever.
        let exit = {
            let mut slot = lock_slot();
            match slot.as_mut() {
                Some(c) => c.try_wait().context("try_wait playback child")?,
                None => return Ok(()), // stop_playback already cleared
            }
        };
        match exit {
            Some(status) => {
                let _ = lock_slot().take();
                if !status.success() {
                    return Err(anyhow!(
                        "player {} exited with {:?}",
                        invocation.program,
                        status.code()
                    ));
                }
                return Ok(());
            }
            None => {
                if start.elapsed() >= PLAYBACK_HARD_TIMEOUT {
                    if let Some(mut c) = lock_slot().take() {
                        let _ = c.kill();
                        let _ = c.wait();
                    }
                    return Err(anyhow!(
                        "playback exceeded {}s hard cap; killed",
                        PLAYBACK_HARD_TIMEOUT.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Interrupt the currently playing audio. No-op if nothing is playing.
/// Best-effort — SIGTERM via `Child::kill`. Polling in
/// [`play_audio_file_with`] picks up the absent child on its next
/// iteration and returns.
pub fn stop_playback() {
    let mut slot = lock_slot();
    if let Some(mut c) = slot.take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubResolver(Vec<PlayerInvocation>);
    impl PlayerResolver for StubResolver {
        fn candidates(&self, _file: &Path) -> Vec<PlayerInvocation> {
            self.0.clone()
        }
    }

    /// On macOS the default resolver puts `afplay` first when it's on
    /// PATH. We can't assert *which* binaries are present on the test
    /// host, but we can assert that whatever it lists, afplay never
    /// appears after ffplay/aplay — that ordering is the contract.
    #[test]
    fn default_resolver_orders_afplay_before_ffplay_and_aplay() {
        let resolver = DefaultPlayerResolver;
        let path = std::env::temp_dir().join("nonexistent.mp3");
        let candidates = resolver.candidates(&path);
        let positions: Vec<usize> = ["afplay", "ffplay", "aplay"]
            .iter()
            .map(|p| {
                candidates
                    .iter()
                    .position(|c| c.program == *p)
                    .unwrap_or(usize::MAX)
            })
            .collect();
        assert!(
            positions[0] <= positions[1] && positions[1] <= positions[2],
            "expected afplay <= ffplay <= aplay; got {positions:?} from {candidates:?}"
        );
    }

    #[test]
    fn missing_file_errors_before_spawn() {
        let resolver = StubResolver(vec![PlayerInvocation {
            program: "/bin/true".into(),
            args: vec![],
        }]);
        let path = std::env::temp_dir().join("definitely-does-not-exist-borg.mp3");
        let err = play_audio_file_with(&path, &resolver).expect_err("should fail");
        assert!(format!("{err}").contains("does not exist"), "{err}");
    }

    #[test]
    fn falls_through_when_first_player_fails() -> Result<()> {
        let tmp = tempfile::NamedTempFile::new()?;
        // `true` is in /bin on Linux and /usr/bin on macOS — resolve
        // via which() so the test works on every CI host.
        let true_bin = which::which("true").expect("`true` binary must be on PATH for this test");
        let resolver = StubResolver(vec![
            PlayerInvocation {
                program: "/nonexistent/borg-test-player-12345".into(),
                args: vec![],
            },
            PlayerInvocation {
                program: true_bin.to_string_lossy().into_owned(),
                args: vec![],
            },
        ]);
        play_audio_file_with(tmp.path(), &resolver)?;
        Ok(())
    }

    #[test]
    fn stop_playback_terminates_active_child() -> Result<()> {
        let tmp = tempfile::NamedTempFile::new()?;
        let sleep_bin =
            which::which("sleep").expect("`sleep` binary must be on PATH for this test");
        let resolver = StubResolver(vec![PlayerInvocation {
            program: sleep_bin.to_string_lossy().into_owned(),
            args: vec!["5".into()],
        }]);
        let started = std::time::Instant::now();
        let path = tmp.path().to_path_buf();
        let handle = std::thread::spawn(move || {
            // We expect this to return after stop_playback kills the
            // child. The exit status of `sleep` after kill is non-zero
            // so play_audio_file_with will return Err — that's fine,
            // we're testing the termination behavior, not the success
            // path of the fake player.
            let _ = play_audio_file_with(&path, &resolver);
        });
        // Give the child time to spawn.
        std::thread::sleep(Duration::from_millis(150));
        stop_playback();
        handle.join().expect("playback thread");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "stop_playback should kill within ~1s, took {elapsed:?}"
        );
        Ok(())
    }
}
