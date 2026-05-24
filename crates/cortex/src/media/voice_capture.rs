//! Microphone capture, voice-activity detection, and WAV encoding for
//! `/voice`.
//!
//! The state machine is split from the audio I/O so the contract that
//! actually matters — *when do we decide the user has stopped talking?* —
//! is testable without `cpal`. [`VadFsm`] is the pure FSM, [`AudioRecorder`]
//! is the real `cpal` actor that drives it on a dedicated thread, and
//! [`Recorder`] is the trait the orchestrator (`voice_loop`) consumes so
//! tests can swap in [`MockRecorder`].
//!
//! Ported behavior-for-behavior from
//! `reference/hermes-agent/tools/voice_mode.py::AudioRecorder` —
//! including the "resume tracking with dip tolerance" sub-state at
//! Python lines 487–503, which prevents brief sub-threshold dips
//! mid-utterance from being mistaken for the user stopping. Don't
//! simplify that path without matching tests; it has regressed twice
//! in the reference implementation.
//!
//! The Termux/Android backend from the reference is intentionally
//! omitted — borg targets desktop and server hosts.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use hound::{SampleFormat, WavSpec, WavWriter};
use regex::Regex;

use crate::config::VoiceConfig;

/// Lock a `Mutex` without panicking on poisoning.
///
/// Poisoning means another thread panicked while holding the lock — for
/// the audio buffer / FSM / callback slot, the worst case is one lost
/// recording. Recovering the inner state lets the recorder thread keep
/// running so the *next* `/voice on` works. Panicking here would tear
/// down the whole TUI for a transient audio-thread fault.
fn lock_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Subdirectory under [`std::env::temp_dir`] where recordings land.
pub const TEMP_SUBDIR: &str = "borg_voice";

/// One decision from the silence FSM per audio chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadDecision {
    /// No action — keep capturing.
    Continue,
    /// User confirmed speech then went silent for
    /// [`VoiceConfig::silence_duration_secs`]. Stop and transcribe.
    FireSilence,
    /// User never produced speech within
    /// [`VoiceConfig::max_wait_secs`]. Stop without transcribing.
    FireMaxWait,
}

/// Pure silence-detection state machine. One instance per recording.
///
/// Inputs: RMS level per chunk + monotonic `Instant`.
/// Output: a [`VadDecision`] per chunk.
///
/// State transitions mirror
/// `reference/hermes-agent/tools/voice_mode.py:463-543` exactly.
#[derive(Debug)]
pub struct VadFsm {
    cfg: VoiceConfig,
    started_at: Instant,
    /// Confirmed speech has been observed.
    has_spoken: bool,
    /// Earliest instant of a current above-threshold burst (resets on dip).
    speech_start: Option<Instant>,
    /// Earliest instant of a current sub-threshold dip during an
    /// unconfirmed burst — used to swallow micro-pauses.
    dip_start: Option<Instant>,
    /// Earliest instant of the current silence after speech was confirmed.
    silence_start: Option<Instant>,
    /// Earliest instant of a sustained above-threshold burst *after*
    /// silence already started — i.e. the user resumed talking and we
    /// must cancel the silence timer.
    resume_start: Option<Instant>,
    /// Dip tracker for resume detection — natural speech has brief
    /// sub-threshold moments and we must tolerate them.
    resume_dip_start: Option<Instant>,
    /// Peak RMS seen during recording — drives the "too quiet, discard"
    /// gate in [`AudioRecorder::stop`].
    peak_rms: u16,
    /// Latest RMS for level meters.
    current_rms: u16,
}

impl VadFsm {
    pub fn new(cfg: VoiceConfig, now: Instant) -> Self {
        Self {
            cfg,
            started_at: now,
            has_spoken: false,
            speech_start: None,
            dip_start: None,
            silence_start: None,
            resume_start: None,
            resume_dip_start: None,
            peak_rms: 0,
            current_rms: 0,
        }
    }

    pub fn current_rms(&self) -> u16 {
        self.current_rms
    }

    pub fn peak_rms(&self) -> u16 {
        self.peak_rms
    }

    pub fn has_spoken(&self) -> bool {
        self.has_spoken
    }

    /// Feed one audio chunk's RMS at instant `now`.
    pub fn on_chunk(&mut self, rms: u16, now: Instant) -> VadDecision {
        self.current_rms = rms;
        if rms > self.peak_rms {
            self.peak_rms = rms;
        }

        let threshold = self.cfg.silence_threshold_rms;
        let min_speech = Duration::from_secs_f32(self.cfg.min_speech_duration_secs);
        let dip_tol = Duration::from_secs_f32(self.cfg.dip_tolerance_secs);
        let silence_dur = Duration::from_secs_f32(self.cfg.silence_duration_secs);
        let max_wait = Duration::from_secs_f32(self.cfg.max_wait_secs);

        if rms > threshold {
            // Above threshold — speech or noise.
            self.dip_start = None;
            match self.speech_start {
                None => self.speech_start = Some(now),
                Some(start) => {
                    if !self.has_spoken && now.duration_since(start) >= min_speech {
                        self.has_spoken = true;
                    }
                }
            }
            if !self.has_spoken {
                // Speech still unconfirmed — nothing to reset.
                self.silence_start = None;
            } else {
                // Speech confirmed: only reset silence if the resume
                // burst itself sustains past `min_speech`. Mirrors the
                // Python state at lines 482-492.
                self.resume_dip_start = None;
                match self.resume_start {
                    None => self.resume_start = Some(now),
                    Some(start) => {
                        if now.duration_since(start) >= min_speech {
                            self.silence_start = None;
                            self.resume_start = None;
                        }
                    }
                }
            }
        } else if self.has_spoken {
            // Below threshold after speech confirmed. Resume tracker
            // tolerates brief dips before declaring "user stopped
            // resuming". Mirrors Python lines 493-503.
            if self.resume_start.is_some() {
                match self.resume_dip_start {
                    None => self.resume_dip_start = Some(now),
                    Some(dip) => {
                        if now.duration_since(dip) >= dip_tol {
                            self.resume_start = None;
                            self.resume_dip_start = None;
                        }
                    }
                }
            }
        } else if let Some(start) = self.speech_start {
            // Pre-confirmation dip — tolerate `dip_tolerance_secs` of
            // sub-threshold before resetting the speech-attempt timer.
            // Mirrors Python lines 504-514.
            match self.dip_start {
                None => self.dip_start = Some(now),
                Some(dip) => {
                    if now.duration_since(dip) >= dip_tol {
                        self.speech_start = None;
                        self.dip_start = None;
                        let _ = start;
                    }
                }
            }
        }

        // Fire decisions.
        if self.has_spoken && rms <= threshold {
            match self.silence_start {
                None => {
                    self.silence_start = Some(now);
                }
                Some(start) => {
                    if now.duration_since(start) >= silence_dur {
                        return VadDecision::FireSilence;
                    }
                }
            }
        } else if !self.has_spoken && now.duration_since(self.started_at) >= max_wait {
            return VadDecision::FireMaxWait;
        }

        VadDecision::Continue
    }
}

/// Compute int16 PCM RMS for a chunk. Public for tests and feeders.
pub fn rms_i16(samples: &[i16]) -> u16 {
    if samples.is_empty() {
        return 0;
    }
    let mut sum: f64 = 0.0;
    for s in samples {
        let f = *s as f64;
        sum += f * f;
    }
    let mean = sum / samples.len() as f64;
    let rms = mean.sqrt();
    rms.clamp(0.0, u16::MAX as f64) as u16
}

/// Write captured int16 mono PCM frames to a WAV file in
/// `temp_dir/borg_voice/`. Returns the path.
pub fn write_wav(samples: &[i16], sample_rate_hz: u32) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(TEMP_SUBDIR);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create voice tmp dir at {}", dir.display()))?;
    let stamp = chrono::Utc::now().format("%Y%m%d_%H%M%S_%f");
    let path = dir.join(format!("recording_{stamp}.wav"));
    let spec = WavSpec {
        channels: 1,
        sample_rate: sample_rate_hz,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut writer = WavWriter::create(&path, spec)
        .with_context(|| format!("failed to open WAV writer at {}", path.display()))?;
    for s in samples {
        writer.write_sample(*s).context("write WAV sample")?;
    }
    writer.finalize().context("finalize WAV file")?;
    Ok(path)
}

// ─── Whisper hallucination filter ──────────────────────────────────────────

static HALLUCINATION_PHRASES: std::sync::OnceLock<HashSet<&'static str>> =
    std::sync::OnceLock::new();
static HALLUCINATION_REPEAT_RE: std::sync::OnceLock<Option<Regex>> = std::sync::OnceLock::new();

fn phrases() -> &'static HashSet<&'static str> {
    HALLUCINATION_PHRASES.get_or_init(|| {
        [
            "thank you.",
            "thank you",
            "thanks for watching.",
            "thanks for watching",
            "subscribe to my channel.",
            "subscribe to my channel",
            "like and subscribe.",
            "like and subscribe",
            "please subscribe.",
            "please subscribe",
            "thank you for watching.",
            "thank you for watching",
            "bye.",
            "bye",
            "you",
            "the end.",
            "the end",
            // Non-English hallucinations Whisper commonly emits on silence
            "продолжение следует",
            "продолжение следует...",
            "sous-titres",
            "sous-titres réalisés par la communauté d'amara.org",
            "sottotitoli creati dalla comunità amara.org",
            "untertitel von stephanie geiges",
            "amara.org",
            "www.mooji.org",
            "ご視聴ありがとうございました",
        ]
        .into_iter()
        .collect()
    })
}

fn repeat_re() -> Option<&'static Regex> {
    HALLUCINATION_REPEAT_RE
        .get_or_init(|| {
            // `(?i)` for case-insensitivity; the body matches strings made up
            // entirely of "thank you", "thanks", "bye", "you", "ok", "okay",
            // "the end", punctuation, or whitespace — the classic Whisper
            // silence babble. Mirror the project's regex-init pattern: log
            // and degrade rather than panic on a pattern-compile fault.
            Regex::new(r"(?i)^(?:thank you|thanks|bye|you|ok|okay|the end|\.|\s|,|!)+$")
                .map_err(|e| tracing::error!("hallucination regex failed to compile: {e}"))
                .ok()
        })
        .as_ref()
}

/// True when Whisper is hallucinating on silence — drop the transcript.
pub fn is_whisper_hallucination(transcript: &str) -> bool {
    let cleaned = transcript.trim().to_lowercase();
    if cleaned.is_empty() {
        return true;
    }
    let stripped = cleaned.trim_end_matches(['.', '!']);
    if phrases().contains(stripped) || phrases().contains(cleaned.as_str()) {
        return true;
    }
    repeat_re().map(|r| r.is_match(&cleaned)).unwrap_or(false)
}

// ─── Recorder trait + actor implementation ─────────────────────────────────

/// Recorder API consumed by `voice_loop`. The trait lets tests inject a
/// fake without spinning up `cpal`.
pub trait Recorder: Send + Sync + 'static {
    /// Begin capturing. The recorder takes ownership of an `on_silence`
    /// callback fired (on a non-blocking thread) when the FSM decides
    /// the user has stopped talking *or* the no-speech timeout elapses.
    fn start(&self, on_silence: Box<dyn Fn() + Send + Sync + 'static>) -> Result<()>;
    /// Stop capturing and write a WAV. `None` if the recording was too
    /// short, too quiet, or contained no audio at all.
    fn stop(&self) -> Result<Option<PathBuf>>;
    /// Stop capturing and discard the buffer.
    fn cancel(&self);
    /// Release the underlying audio stream entirely.
    fn shutdown(&self);
    /// Whether `start` has been called and `stop`/`cancel` has not.
    fn is_recording(&self) -> bool;
    /// Live RMS for UI level meters.
    fn current_rms(&self) -> u16;
    /// Peak RMS during the active or just-finished recording.
    fn peak_rms(&self) -> u16;
}

/// Commands sent to the recorder thread.
enum RecorderCmd {
    Start(Box<dyn Fn() + Send + Sync + 'static>),
    Stop(std::sync::mpsc::Sender<Result<Option<PathBuf>>>),
    Cancel,
    Shutdown,
}

/// Shared state between the actor thread and the public API. All fields
/// are touched from both the cpal callback (audio thread) and the
/// command-processing thread, hence the mutex.
struct RecorderShared {
    cfg: VoiceConfig,
    recording: AtomicBool,
    /// Live PCM accumulator. `Some` while recording, `None` otherwise.
    frames: Mutex<Option<Vec<i16>>>,
    fsm: Mutex<Option<VadFsm>>,
    current_rms: AtomicU16,
    peak_rms: AtomicU16,
    /// Silence callback for the active recording, if any. Wrapped in a
    /// mutex so the audio callback can take it atomically and only fire
    /// once. Cleared on stop/cancel/shutdown.
    silence_cb: Mutex<Option<Box<dyn Fn() + Send + Sync + 'static>>>,
}

/// Production recorder backed by `cpal`. The audio thread is spawned on
/// `new` and lives until [`Recorder::shutdown`] is called.
pub struct AudioRecorder {
    shared: Arc<RecorderShared>,
    cmd_tx: std::sync::mpsc::Sender<RecorderCmd>,
    join: Mutex<Option<thread::JoinHandle<()>>>,
}

impl AudioRecorder {
    /// Spawn the audio actor and return a handle. Returns an error if no
    /// default input device is available at construction time so the
    /// caller can surface a "no microphone" message instead of a silent
    /// no-op.
    pub fn new(cfg: VoiceConfig) -> Result<Self> {
        let shared = Arc::new(RecorderShared {
            cfg,
            recording: AtomicBool::new(false),
            frames: Mutex::new(None),
            fsm: Mutex::new(None),
            current_rms: AtomicU16::new(0),
            peak_rms: AtomicU16::new(0),
            silence_cb: Mutex::new(None),
        });

        // Probe for a default input device up front. Don't *keep* the
        // stream — building it can be expensive on macOS — but failing
        // here means the user gets a clear error instead of a recorder
        // that silently never produces samples.
        detect_audio_env()?;

        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<RecorderCmd>();
        let actor_shared = Arc::clone(&shared);
        let join = thread::Builder::new()
            .name("borg-voice-recorder".into())
            .spawn(move || recorder_actor(actor_shared, cmd_rx))
            .context("spawn voice recorder thread")?;

        Ok(Self {
            shared,
            cmd_tx,
            join: Mutex::new(Some(join)),
        })
    }
}

impl Recorder for AudioRecorder {
    fn start(&self, on_silence: Box<dyn Fn() + Send + Sync + 'static>) -> Result<()> {
        // Idempotent — already-recording start is a no-op (mirrors
        // hermes voice.py:316).
        if self.shared.recording.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.cmd_tx
            .send(RecorderCmd::Start(on_silence))
            .map_err(|_| anyhow!("recorder thread is gone"))?;
        Ok(())
    }

    fn stop(&self) -> Result<Option<PathBuf>> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.cmd_tx
            .send(RecorderCmd::Stop(tx))
            .map_err(|_| anyhow!("recorder thread is gone"))?;
        rx.recv()
            .map_err(|_| anyhow!("recorder thread dropped reply channel"))?
    }

    fn cancel(&self) {
        let _ = self.cmd_tx.send(RecorderCmd::Cancel);
    }

    fn shutdown(&self) {
        let _ = self.cmd_tx.send(RecorderCmd::Shutdown);
        if let Some(h) = self.join.lock().ok().and_then(|mut g| g.take()) {
            let _ = h.join();
        }
    }

    fn is_recording(&self) -> bool {
        self.shared.recording.load(Ordering::SeqCst)
    }

    fn current_rms(&self) -> u16 {
        self.shared.current_rms.load(Ordering::Relaxed)
    }

    fn peak_rms(&self) -> u16 {
        self.shared.peak_rms.load(Ordering::Relaxed)
    }
}

impl Drop for AudioRecorder {
    fn drop(&mut self) {
        // Ask the actor to exit. Channel-closed error means the actor
        // already returned (clean prior shutdown or panic); nothing
        // more to do on that path.
        let _ = self.cmd_tx.send(RecorderCmd::Shutdown);
        // Block Drop until the actor returns so the cpal stream is
        // released deterministically before this handle is gone. The
        // actor processes Shutdown immediately (the audio callback
        // runs on its own OS-managed thread, not the actor loop), so
        // this wait is effectively instant. Callers that want a
        // cancelable teardown should still call `shutdown()` first.
        let handle_opt = match self.join.lock() {
            Ok(mut g) => g.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(h) = handle_opt {
            let _ = h.join();
        }
    }
}

// ─── Audio environment preflight ────────────────────────────────────────────
//
// `cpal::default_host().default_input_device()` returning `None` is the
// generic failure mode; the value of this preflight is converting it into a
// platform-specific remediation string the user can act on directly. Hermes's
// `detect_audio_environment()` at `tools/voice_mode.py` does the same thing
// in Python.
//
// We intentionally do NOT call `AVCaptureDevice.authorizationStatus` on
// macOS — that pulls in objc bindings for a single bit of info, and a denied
// permission shows up as "no input device" in cpal anyway. The hint covers
// both the missing-device and missing-permission cases.

/// Probe the audio host and return Ok if a default input device exists.
/// On failure, the error carries a platform-specific remediation hint so
/// the user knows what to fix instead of seeing a generic cpal error.
pub fn detect_audio_env() -> Result<()> {
    use cpal::traits::HostTrait;
    let host = cpal::default_host();
    if host.default_input_device().is_some() {
        return Ok(());
    }
    let hint = audio_env_remediation();
    Err(anyhow!("no audio input device available — {hint}"))
}

#[cfg(target_os = "macos")]
fn audio_env_remediation() -> &'static str {
    "grant Microphone permission in System Settings → Privacy & Security → Microphone, \
     then restart borg"
}

#[cfg(target_os = "linux")]
fn audio_env_remediation() -> &'static str {
    // PulseAudio is the modern default on Ubuntu/Debian/Fedora desktops;
    // ALSA-only setups (headless / minimal) need alsa-utils. We can't tell
    // which the operator has without shelling out, so name both.
    "install audio support (Debian/Ubuntu: `sudo apt install pulseaudio-utils alsa-utils`; \
     headless: ensure your shell has a working ALSA/PulseAudio session)"
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn audio_env_remediation() -> &'static str {
    "no default audio input device — check your OS audio configuration"
}

/// Audio actor — owns the cpal stream because `cpal::Stream` is `!Send`
/// on macOS. Receives commands from the public handle, drives the FSM
/// from the audio callback, fires the silence callback through a small
/// helper thread so the audio callback never blocks.
fn recorder_actor(shared: Arc<RecorderShared>, cmd_rx: std::sync::mpsc::Receiver<RecorderCmd>) {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let device = match host.default_input_device() {
        Some(d) => d,
        None => {
            // No input device — drain commands and exit.
            while let Ok(cmd) = cmd_rx.recv() {
                if matches!(cmd, RecorderCmd::Shutdown) {
                    return;
                }
            }
            return;
        }
    };

    let target_rate = shared.cfg.sample_rate_hz;
    let supported = match device.default_input_config() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "voice recorder: default_input_config failed");
            return;
        }
    };
    let device_rate = supported.sample_rate().0;
    let device_channels = supported.channels();
    let sample_format = supported.sample_format();
    let stream_cfg: cpal::StreamConfig = supported.into();

    let shared_cb = Arc::clone(&shared);
    let resample_ratio = device_rate as f32 / target_rate as f32;

    let err_fn = |e| tracing::warn!(error = ?e, "cpal input stream error");

    let stream_result = match sample_format {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &stream_cfg,
            move |data: &[f32], _: &_| {
                let mono = downmix_and_resample_f32(data, device_channels, resample_ratio);
                process_chunk(&shared_cb, &mono);
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            &stream_cfg,
            move |data: &[i16], _: &_| {
                let mono = downmix_and_resample_i16(data, device_channels, resample_ratio);
                process_chunk(&shared_cb, &mono);
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream(
            &stream_cfg,
            move |data: &[u16], _: &_| {
                let mono: Vec<i16> = data.iter().map(|s| (*s as i32 - 32768) as i16).collect();
                let mono = resample_i16(&mono, device_channels, resample_ratio);
                process_chunk(&shared_cb, &mono);
            },
            err_fn,
            None,
        ),
        _ => {
            tracing::warn!(
                "voice recorder: unsupported sample format {:?}",
                sample_format
            );
            return;
        }
    };

    let stream = match stream_result {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "voice recorder: failed to build input stream");
            return;
        }
    };
    if let Err(e) = stream.play() {
        tracing::warn!(error = %e, "voice recorder: stream.play failed");
        return;
    }

    // Command loop.
    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            RecorderCmd::Start(cb) => {
                let mut frames = lock_recover(&shared.frames);
                *frames = Some(Vec::new());
                drop(frames);
                *lock_recover(&shared.fsm) = Some(VadFsm::new(shared.cfg.clone(), Instant::now()));
                shared.current_rms.store(0, Ordering::Relaxed);
                shared.peak_rms.store(0, Ordering::Relaxed);
                *lock_recover(&shared.silence_cb) = Some(cb);
                shared.recording.store(true, Ordering::SeqCst);
            }
            RecorderCmd::Stop(reply) => {
                let path = finalize_recording(&shared, /*discard=*/ false);
                let _ = reply.send(Ok(path));
            }
            RecorderCmd::Cancel => {
                let _ = finalize_recording(&shared, /*discard=*/ true);
            }
            RecorderCmd::Shutdown => {
                let _ = finalize_recording(&shared, /*discard=*/ true);
                drop(stream);
                return;
            }
        }
    }
}

fn finalize_recording(shared: &Arc<RecorderShared>, discard: bool) -> Option<PathBuf> {
    if !shared.recording.swap(false, Ordering::SeqCst) {
        return None;
    }
    let frames = lock_recover(&shared.frames).take().unwrap_or_default();
    *lock_recover(&shared.fsm) = None;
    *lock_recover(&shared.silence_cb) = None;
    shared.current_rms.store(0, Ordering::Relaxed);

    if discard {
        return None;
    }

    if frames.is_empty() {
        return None;
    }
    let min_samples = (shared.cfg.sample_rate_hz as f32 * 0.3) as usize;
    if frames.len() < min_samples {
        return None;
    }
    let peak = shared.peak_rms.load(Ordering::Relaxed);
    if peak < shared.cfg.silence_threshold_rms {
        // Whole recording was below the speech-detection floor —
        // drop it rather than ship silence to Whisper, where it
        // would hallucinate (hermes voice_mode.py:673).
        return None;
    }
    match write_wav(&frames, shared.cfg.sample_rate_hz) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(error = %e, "failed to write voice WAV");
            None
        }
    }
}

fn process_chunk(shared: &Arc<RecorderShared>, samples: &[i16]) {
    if !shared.recording.load(Ordering::SeqCst) {
        return;
    }
    let rms = rms_i16(samples);
    shared.current_rms.store(rms, Ordering::Relaxed);
    {
        let mut peak = shared.peak_rms.load(Ordering::Relaxed);
        if rms > peak {
            peak = rms;
            shared.peak_rms.store(peak, Ordering::Relaxed);
        }
    }

    {
        let mut frames = lock_recover(&shared.frames);
        if let Some(buf) = frames.as_mut() {
            buf.extend_from_slice(samples);
        }
    }

    let decision = {
        let mut fsm_lock = lock_recover(&shared.fsm);
        if let Some(fsm) = fsm_lock.as_mut() {
            fsm.on_chunk(rms, Instant::now())
        } else {
            VadDecision::Continue
        }
    };

    if matches!(
        decision,
        VadDecision::FireSilence | VadDecision::FireMaxWait
    ) {
        // Take the callback so it fires exactly once.
        let cb = lock_recover(&shared.silence_cb).take();
        if let Some(cb) = cb {
            // The audio callback runs on the cpal thread — never block
            // it. Hand the user callback to a fresh thread that calls
            // it and exits.
            thread::Builder::new()
                .name("borg-voice-silence-cb".into())
                .spawn(cb)
                .map(|h| {
                    // Detach.
                    let _ = h;
                })
                .ok();
        }
    }
}

fn downmix_and_resample_f32(data: &[f32], channels: u16, ratio: f32) -> Vec<i16> {
    let mono: Vec<i16> = data
        .chunks(channels.max(1) as usize)
        .map(|frame| {
            let avg: f32 = frame.iter().sum::<f32>() / frame.len().max(1) as f32;
            (avg.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
        })
        .collect();
    resample_i16(&mono, 1, ratio)
}

fn downmix_and_resample_i16(data: &[i16], channels: u16, ratio: f32) -> Vec<i16> {
    if channels <= 1 {
        return resample_i16(data, 1, ratio);
    }
    let mono: Vec<i16> = data
        .chunks(channels as usize)
        .map(|frame| {
            let sum: i32 = frame.iter().map(|s| *s as i32).sum();
            (sum / frame.len() as i32) as i16
        })
        .collect();
    resample_i16(&mono, 1, ratio)
}

/// Linear-interpolation resampler. `ratio` = src_rate / dst_rate; values
/// near 1.0 (e.g. 44100→16000 ≈ 2.76) work well enough for STT — Whisper
/// is robust to mild aliasing. Bypasses when ratio is ~1.
fn resample_i16(samples: &[i16], _channels: u16, ratio: f32) -> Vec<i16> {
    if (ratio - 1.0).abs() < 1e-3 || samples.is_empty() {
        return samples.to_vec();
    }
    let out_len = ((samples.len() as f32) / ratio) as usize;
    let mut out = Vec::with_capacity(out_len);
    let last = *samples.last().unwrap_or(&0);
    for i in 0..out_len {
        let src = i as f32 * ratio;
        let idx = src as usize;
        let frac = src - idx as f32;
        let a = *samples.get(idx).unwrap_or(&last) as f32;
        let b = *samples.get(idx + 1).unwrap_or(&last) as f32;
        out.push((a + (b - a) * frac) as i16);
    }
    out
}

// ─── Mock recorder for tests ───────────────────────────────────────────────

/// Test-only recorder. Captures the `on_silence` callback so tests can
/// fire silence on demand and inspect lifecycle calls.
#[cfg(any(test, feature = "test-support"))]
pub struct MockRecorder {
    recording: AtomicBool,
    cb: Mutex<Option<Box<dyn Fn() + Send + Sync + 'static>>>,
    transcript_wav: Mutex<Option<PathBuf>>,
    pub starts: std::sync::atomic::AtomicUsize,
    pub stops: std::sync::atomic::AtomicUsize,
    pub cancels: std::sync::atomic::AtomicUsize,
    pub shutdowns: std::sync::atomic::AtomicUsize,
}

#[cfg(any(test, feature = "test-support"))]
impl Default for MockRecorder {
    fn default() -> Self {
        Self {
            recording: AtomicBool::new(false),
            cb: Mutex::new(None),
            transcript_wav: Mutex::new(None),
            starts: std::sync::atomic::AtomicUsize::new(0),
            stops: std::sync::atomic::AtomicUsize::new(0),
            cancels: std::sync::atomic::AtomicUsize::new(0),
            shutdowns: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl MockRecorder {
    /// Stage a WAV path that the next `stop()` call will return.
    pub fn set_next_stop_path(&self, path: Option<PathBuf>) {
        *lock_recover(&self.transcript_wav) = path;
    }
    /// Trigger the silence callback as if the FSM had fired it.
    pub fn fire_silence(&self) {
        if let Some(cb) = lock_recover(&self.cb).take() {
            cb();
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Recorder for MockRecorder {
    fn start(&self, on_silence: Box<dyn Fn() + Send + Sync + 'static>) -> Result<()> {
        if self.recording.load(Ordering::SeqCst) {
            return Ok(());
        }
        *lock_recover(&self.cb) = Some(on_silence);
        self.recording.store(true, Ordering::SeqCst);
        self.starts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn stop(&self) -> Result<Option<PathBuf>> {
        self.recording.store(false, Ordering::SeqCst);
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(lock_recover(&self.transcript_wav).clone())
    }
    fn cancel(&self) {
        self.recording.store(false, Ordering::SeqCst);
        *lock_recover(&self.cb) = None;
        self.cancels.fetch_add(1, Ordering::SeqCst);
    }
    fn shutdown(&self) {
        self.recording.store(false, Ordering::SeqCst);
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
    }
    fn is_recording(&self) -> bool {
        self.recording.load(Ordering::SeqCst)
    }
    fn current_rms(&self) -> u16 {
        0
    }
    fn peak_rms(&self) -> u16 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cfg() -> VoiceConfig {
        VoiceConfig::default()
    }

    fn at(start: Instant, secs: f32) -> Instant {
        start + Duration::from_secs_f32(secs)
    }

    #[test]
    fn audio_env_remediation_is_platform_specific() {
        // Vague messages like "audio failed" defeat the purpose of a
        // preflight; the hint must name something the user can act on.
        let hint = audio_env_remediation();
        if cfg!(target_os = "macos") {
            assert!(
                hint.contains("Microphone permission"),
                "macOS hint must mention Microphone permission, got: {hint}"
            );
        } else if cfg!(target_os = "linux") {
            assert!(
                hint.contains("pulseaudio") || hint.contains("ALSA"),
                "Linux hint must name the audio stack, got: {hint}"
            );
        }
        assert!(!hint.is_empty());
    }

    #[test]
    fn detect_audio_env_error_includes_remediation_hint() {
        // We can't reliably force a no-device state on every CI host, so
        // assert the wiring contract: any failure must embed the
        // platform remediation rather than only "no device". On a host
        // with a mic, Ok is expected and the assertion is skipped.
        match detect_audio_env() {
            Ok(()) => {}
            Err(e) => {
                let msg = format!("{e}");
                let hint = audio_env_remediation();
                assert!(
                    msg.contains(hint),
                    "error message must embed remediation hint;\n  msg:  {msg}\n  hint: {hint}"
                );
            }
        }
    }

    #[test]
    fn silence_callback_fires_after_speech_then_silence_duration() {
        let start = Instant::now();
        let mut fsm = VadFsm::new(cfg(), start);
        // 0.4s of confirmed speech (above the 0.3s min_speech threshold).
        assert_eq!(fsm.on_chunk(2000, at(start, 0.0)), VadDecision::Continue);
        assert_eq!(fsm.on_chunk(2000, at(start, 0.4)), VadDecision::Continue);
        assert!(fsm.has_spoken());
        // 3.0s of silence — at exactly silence_duration the decision flips.
        assert_eq!(fsm.on_chunk(50, at(start, 0.5)), VadDecision::Continue);
        assert_eq!(fsm.on_chunk(50, at(start, 3.5)), VadDecision::FireSilence);
    }

    #[test]
    fn silence_callback_does_not_fire_on_brief_dips() {
        let start = Instant::now();
        let mut fsm = VadFsm::new(cfg(), start);
        // Confirm speech.
        assert_eq!(fsm.on_chunk(2000, at(start, 0.0)), VadDecision::Continue);
        assert_eq!(fsm.on_chunk(2000, at(start, 0.4)), VadDecision::Continue);
        assert!(fsm.has_spoken());

        // Alternate 0.2s dip / 0.4s speech over 4 seconds (longer than
        // silence_duration). The resume tracker tolerates the dips
        // because each one is shorter than dip_tolerance_secs (0.3s)
        // and each speech burst sustains past min_speech (0.3s).
        let mut t = 0.5_f32;
        for _ in 0..5 {
            let r = fsm.on_chunk(50, at(start, t)); // dip start
            assert_eq!(r, VadDecision::Continue);
            t += 0.2;
            let r = fsm.on_chunk(2000, at(start, t)); // resume
            assert_eq!(r, VadDecision::Continue);
            t += 0.4;
        }
        assert!(
            t > 3.0,
            "test should span longer than silence_duration_secs to be meaningful"
        );
    }

    #[test]
    fn max_wait_fires_on_pure_silence() {
        let start = Instant::now();
        let mut fsm = VadFsm::new(cfg(), start);
        // No speech at all for 15s — the no-speech timeout fires.
        for s in 0..15 {
            assert_eq!(
                fsm.on_chunk(20, at(start, s as f32)),
                VadDecision::Continue,
                "no fire before max_wait"
            );
        }
        assert!(!fsm.has_spoken());
        assert_eq!(fsm.on_chunk(20, at(start, 15.1)), VadDecision::FireMaxWait);
    }

    #[test]
    fn known_hallucinations_filtered() {
        for s in [
            "thank you",
            "Thank you.",
            "thanks for watching",
            "subscribe to my channel.",
            "sous-titres",
            "продолжение следует",
            "  ",
            "",
        ] {
            assert!(
                is_whisper_hallucination(s),
                "expected hallucination filter to drop {s:?}"
            );
        }
    }

    #[test]
    fn repetitive_thank_you_filtered() {
        assert!(is_whisper_hallucination(
            "Thank you. Thank you. Thank you. you"
        ));
        assert!(is_whisper_hallucination("Ok ok ok ok"));
    }

    #[test]
    fn real_transcript_passes() {
        for s in [
            "schedule a meeting tomorrow at 3pm",
            "open the project repository",
            "what is the current goal",
        ] {
            assert!(
                !is_whisper_hallucination(s),
                "real transcript {s:?} must not be dropped"
            );
        }
    }

    #[test]
    fn rms_zero_for_silence_high_for_full_scale() {
        assert_eq!(rms_i16(&[]), 0);
        assert_eq!(rms_i16(&[0; 64]), 0);
        let full: Vec<i16> = (0..64).map(|_| i16::MAX).collect();
        assert!(rms_i16(&full) > 30_000);
    }

    #[test]
    fn wav_roundtrip_preserves_samples() -> Result<()> {
        // Generate a short sine wave at 440Hz, 16kHz, 0.1s.
        let rate = 16_000_u32;
        let n = (rate as f32 * 0.1) as usize;
        let mut samples = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f32 / rate as f32;
            let v = (2.0 * std::f32::consts::PI * 440.0 * t).sin();
            samples.push((v * 8000.0) as i16);
        }
        let path = write_wav(&samples, rate)?;
        let reader = hound::WavReader::open(&path).context("reopen WAV")?;
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, rate);
        assert_eq!(spec.bits_per_sample, 16);
        let decoded: Vec<i16> = hound::WavReader::open(&path)?
            .into_samples::<i16>()
            .collect::<Result<_, _>>()?;
        assert_eq!(decoded.len(), samples.len());
        for (a, b) in samples.iter().zip(decoded.iter()) {
            assert!((*a - *b).abs() <= 1, "sample drift {a} vs {b}");
        }
        let _ = std::fs::remove_file(&path);
        Ok(())
    }
}
