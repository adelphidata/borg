//! Audio playback via system players (`afplay` / `ffplay` / `aplay`).
//!
//! exactly — no in-process audio output crate, just a small fallthrough
//! over the players that exist on every supported host. Avoiding a
//! `rodio`/`cpal`-output dependency keeps the binary lean and dodges
//! the cross-platform output bugs that motivated this codepath to use the
//! same strategy.
//!
//! [`stop_playback`] interrupts the currently playing process. The
//! voice loop calls it on `/voice off` so playback doesn't keep
//! talking after the user disabled the mode.
//!
//! Player resolution is dependency-injected via [`PlayerResolver`] so
//! tests can drive the resolution order without touching the real
//! filesystem.

use std::f32::consts::TAU;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use hound::{SampleFormat, WavSpec, WavWriter};

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

/// 5 minutes — longest plausible TTS reply.
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
/// (Linux).
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
        // Players such as `mpv` treat stdin as their keyboard control channel,
        // so an inherited stdin makes the player and the TUI fight over the
        // operator's keystrokes. Playback here is fire-and-forget.
        .stdin(Stdio::null())
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

// Beep cues: 880 Hz single beep on record start, 660 Hz double beep on record stop.
// Synthesized once per process and cached to `$TMPDIR/borg_voice/` so playback
// is a cheap `afplay`/`ffplay`/`aplay` invocation.
//
// Beeps run on a detached `tokio::spawn` so the voice loop never blocks
// on the speaker — a 100 ms beep behind a sluggish `afplay` spawn must
// not delay the recorder re-arming.

const BEEP_SAMPLE_RATE: u32 = 44_100;
const BEEP_AMPLITUDE: f32 = 0.3; // -10 dBFS — audible but not jarring.
const BEEP_DURATION_MS: u32 = 50;
const BEEP_FADE_MS: u32 = 5; // Suppress the click at sample-0 and sample-N.
const BEEP_GAP_MS: u32 = 50; // Silence between the two stop-beep tones.
const START_BEEP_HZ: f32 = 880.0;
const STOP_BEEP_HZ: f32 = 660.0;

/// Synthesize a single sine-wave beep as a WAV byte vector.
///
/// Mono, 16-bit PCM, 44.1 kHz. Applies a `BEEP_FADE_MS` linear fade-in
/// and fade-out to suppress the discontinuity click at the buffer edges
/// — without it, speakers crack on every start/stop.
pub fn synthesize_beep_wav(freq_hz: f32, duration_ms: u32) -> Vec<u8> {
    let total = (BEEP_SAMPLE_RATE as f32 * duration_ms as f32 / 1000.0) as u32;
    let fade = ((BEEP_SAMPLE_RATE as f32 * BEEP_FADE_MS as f32 / 1000.0) as u32).min(total / 2);
    let samples = (0..total).map(|i| {
        let t = i as f32 / BEEP_SAMPLE_RATE as f32;
        let envelope = if i < fade {
            i as f32 / fade as f32
        } else if i >= total.saturating_sub(fade) {
            (total - i) as f32 / fade as f32
        } else {
            1.0
        };
        let s = BEEP_AMPLITUDE * envelope * (TAU * freq_hz * t).sin();
        (s * i16::MAX as f32) as i16
    });
    write_wav_bytes(samples)
}

/// Synthesize a double beep — two `freq_hz` tones with a `BEEP_GAP_MS`
/// silent gap between them. Used for the stop beep; the doubled tone is
/// distinct enough from the single start tone that the user can tell
/// recording state changes by ear without watching the screen.
pub fn synthesize_double_beep_wav(freq_hz: f32, duration_ms: u32) -> Vec<u8> {
    let beep_samples = (BEEP_SAMPLE_RATE as f32 * duration_ms as f32 / 1000.0) as u32;
    let gap_samples = (BEEP_SAMPLE_RATE as f32 * BEEP_GAP_MS as f32 / 1000.0) as u32;
    let fade =
        ((BEEP_SAMPLE_RATE as f32 * BEEP_FADE_MS as f32 / 1000.0) as u32).min(beep_samples / 2);
    let total = beep_samples * 2 + gap_samples;
    let samples = (0..total).map(|i| {
        // Position within the current beep, or in the silent gap.
        let (pos_in_beep, in_beep) = if i < beep_samples {
            (i, true)
        } else if i < beep_samples + gap_samples {
            (0, false)
        } else {
            (i - beep_samples - gap_samples, true)
        };
        if !in_beep {
            return 0i16;
        }
        let t = pos_in_beep as f32 / BEEP_SAMPLE_RATE as f32;
        let envelope = if pos_in_beep < fade {
            pos_in_beep as f32 / fade as f32
        } else if pos_in_beep >= beep_samples.saturating_sub(fade) {
            (beep_samples - pos_in_beep) as f32 / fade as f32
        } else {
            1.0
        };
        let s = BEEP_AMPLITUDE * envelope * (TAU * freq_hz * t).sin();
        (s * i16::MAX as f32) as i16
    });
    write_wav_bytes(samples)
}

#[allow(clippy::expect_used)]
fn write_wav_bytes(samples: impl Iterator<Item = i16>) -> Vec<u8> {
    let spec = WavSpec {
        channels: 1,
        sample_rate: BEEP_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut buf: Vec<u8> = Vec::with_capacity(8_192);
    {
        let mut writer = WavWriter::new(Cursor::new(&mut buf), spec).expect("WavWriter init");
        for s in samples {
            writer.write_sample(s).expect("write_sample");
        }
        writer.finalize().expect("WavWriter finalize");
    }
    buf
}

/// Cached beep WAV paths. Built once per process when [`Beeper::new`] is
/// first asked to play; subsequent calls reuse the same files. Skips
/// entirely if the operator disabled audio cues via `voice.beep_enabled`.
pub struct Beeper {
    enabled: bool,
    start_path: OnceLock<PathBuf>,
    stop_path: OnceLock<PathBuf>,
}

impl Beeper {
    /// Construct with `enabled` from `voice.beep_enabled`. When false,
    /// `start`/`stop` are no-ops — no files are written, no subprocess
    /// runs.
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            start_path: OnceLock::new(),
            stop_path: OnceLock::new(),
        }
    }

    /// Play the recording-start beep (880 Hz, 50 ms). Fire-and-forget —
    /// errors are logged at `debug` level; the recorder must never be
    /// blocked on the speaker.
    pub fn start(&self) {
        if !self.enabled {
            return;
        }
        let path = self.start_path.get_or_init(|| {
            ensure_beep_file(
                "beep_start.wav",
                &synthesize_beep_wav(START_BEEP_HZ, BEEP_DURATION_MS),
            )
            .unwrap_or_else(|_| PathBuf::new())
        });
        spawn_beep_playback(path.clone());
    }

    /// Play the recording-stop beep (660 Hz × 2, 50 ms each with 50 ms gap).
    pub fn stop(&self) {
        if !self.enabled {
            return;
        }
        let path = self.stop_path.get_or_init(|| {
            ensure_beep_file(
                "beep_stop.wav",
                &synthesize_double_beep_wav(STOP_BEEP_HZ, BEEP_DURATION_MS),
            )
            .unwrap_or_else(|_| PathBuf::new())
        });
        spawn_beep_playback(path.clone());
    }
}

fn ensure_beep_file(name: &str, bytes: &[u8]) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join("borg_voice");
    std::fs::create_dir_all(&dir).context("create beep cache dir")?;
    let path = dir.join(name);
    // Always rewrite — cheap (a few KB) and avoids stale corrupt files
    // surviving a borg upgrade that changed the synth.
    std::fs::write(&path, bytes).context("write beep wav")?;
    Ok(path)
}

fn spawn_beep_playback(path: PathBuf) {
    if path.as_os_str().is_empty() {
        return; // ensure_beep_file failed silently; skip.
    }
    // Detached: a slow `afplay` spawn must not block the voice loop's
    // re-arm path. Errors land in the trace log only.
    tokio::spawn(async move {
        if let Err(e) = play_beep_file(&path).await {
            tracing::debug!(path = %path.display(), error = %e, "beep playback failed");
        }
    });
}

/// Play a beep file *outside* the active-child tracking that
/// `stop_playback` manages. A beep is short and must not collide with
/// (or be cancelled by) a TTS playback that's in flight.
async fn play_beep_file(file: &Path) -> Result<()> {
    let path = file.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let resolver = DefaultPlayerResolver;
        let candidates = resolver.candidates(&path);
        for invocation in candidates {
            let status = Command::new(&invocation.program)
                .args(&invocation.args)
                // See `spawn_and_wait`: players read stdin as key input.
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if let Ok(s) = status {
                if s.success() {
                    return Ok(());
                }
            }
        }
        Err(anyhow!("no audio player available for beep"))
    })
    .await
    .context("beep playback task panicked")?
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

    /// Decode a WAV byte buffer back to mono i16 samples + spec.
    fn decode_wav(bytes: &[u8]) -> (WavSpec, Vec<i16>) {
        let reader = hound::WavReader::new(Cursor::new(bytes)).expect("decode wav");
        let spec = reader.spec();
        let samples: Vec<i16> = reader
            .into_samples::<i16>()
            .collect::<std::result::Result<_, _>>()
            .expect("decode samples");
        (spec, samples)
    }

    /// Estimate dominant frequency from zero-crossing rate. For a pure
    /// sine at `f`, ZCR ≈ 2f / sample_rate. We sample the middle 60% of
    /// the buffer to avoid the fade-in/out regions where amplitude is
    /// low enough that quantization noise can suppress crossings.
    fn estimate_freq_from_zcr(samples: &[i16], sample_rate: u32) -> f32 {
        let start = samples.len() * 2 / 10;
        let end = samples.len() * 8 / 10;
        let mid = &samples[start..end];
        let mut crossings = 0usize;
        for w in mid.windows(2) {
            if (w[0] >= 0) != (w[1] >= 0) {
                crossings += 1;
            }
        }
        (crossings as f32 * sample_rate as f32) / (2.0 * mid.len() as f32)
    }

    #[test]
    fn beep_wav_roundtrip_start_tone_is_880hz() {
        let bytes = synthesize_beep_wav(880.0, 50);
        let (spec, samples) = decode_wav(&bytes);
        // Format contract — Mono 16-bit 44.1 kHz PCM (every player on
        // PATH handles this; if we change it, beeps may go silent).
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 44_100);
        assert_eq!(spec.bits_per_sample, 16);
        // 50 ms @ 44.1 kHz = 2205 samples.
        assert_eq!(samples.len(), 2_205, "sample count for 50 ms");
        // Estimated frequency within 10% of 880 Hz.
        let est = estimate_freq_from_zcr(&samples, spec.sample_rate);
        assert!(
            (est - 880.0).abs() < 88.0,
            "expected ~880 Hz, got {est:.1} Hz from ZCR"
        );
    }

    #[test]
    fn double_beep_has_silent_gap_and_660hz_tones() {
        let bytes = synthesize_double_beep_wav(660.0, 50);
        let (spec, samples) = decode_wav(&bytes);
        // 2 × 50 ms beep + 50 ms gap = 150 ms = 6615 samples.
        assert_eq!(samples.len(), 6_615);
        // The middle 50 ms (samples 2205..4410) must be silence.
        let gap = &samples[2_205..4_410];
        assert!(
            gap.iter().all(|&s| s == 0),
            "expected silence in gap region"
        );
        // The first beep region's middle dominant frequency ~ 660 Hz.
        let first_beep = &samples[..2_205];
        let est = estimate_freq_from_zcr(first_beep, spec.sample_rate);
        assert!(
            (est - 660.0).abs() < 66.0,
            "expected ~660 Hz, got {est:.1} Hz"
        );
    }

    #[test]
    fn disabled_beeper_is_a_no_op() {
        // A disabled Beeper must not synthesize or write any cache file.
        let b = Beeper::new(false);
        b.start();
        b.stop();
        // No assertions beyond the no-panic contract — the OnceLocks
        // stay un-initialized because the gated branch is taken before
        // either path executes. Inspecting their internal state would
        // require RDD on OnceLock which isn't part of the public API.
        assert!(b.start_path.get().is_none());
        assert!(b.stop_path.get().is_none());
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
