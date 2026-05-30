//! Voice-mode orchestrator: ties [`Recorder`](super::voice_capture::Recorder),
//! a transcription provider, and the TTS-playback feedback guard into the
//! continuous/single-shot loops that `/voice` exposes.
//!
//! `start_continuous` / `stop_continuous` / `_continuous_on_silence`
//! (lines 369–735). The state-machine invariants live there; if you
//! change one, change the matching guard test in this file.
//!
//! Why a dedicated module instead of `voice_capture`: the recorder is a
//! pure I/O actor (mic → WAV), while this module owns the *policy* —
//! when to auto-restart, when to enforce the three-strike silent
//! limit, when to pause for TTS feedback. Keeping them separate lets
//! voice_capture's tests stay pure-FSM and lets this module's tests
//! drive lifecycle without `cpal`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex, Notify};

use crate::config::VoiceConfig;
use crate::media::playback::Beeper;
use crate::media::understanding::AudioTranscriber;
use crate::media::voice_capture::{is_whisper_hallucination, Recorder};

/// Public status broadcast to the TUI's footer indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceStatus {
    Listening,
    Transcribing,
    Idle,
}

/// Lifecycle events the orchestrator publishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceEvent {
    Status(VoiceStatus),
    Transcript(String),
    /// Three consecutive empty cycles — loop auto-disabled.
    SilentLimitReached,
    Error(String),
}

/// Transcription provider abstraction. Production: an
/// [`AudioTranscriber`]. Tests: a stub returning canned strings.
#[async_trait]
pub trait Transcriber: Send + Sync + 'static {
    async fn transcribe_wav(&self, path: &std::path::Path) -> Result<String>;
}

#[async_trait]
impl Transcriber for AudioTranscriber {
    async fn transcribe_wav(&self, path: &std::path::Path) -> Result<String> {
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("read voice WAV at {}", path.display()))?;
        let filename = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "recording.wav".to_string());
        let (text, _attempts) = self
            .transcribe(&bytes, "audio/wav", &filename, None)
            .await?;
        Ok(text)
    }
}

/// Mutable runtime state guarded by a single mutex. Kept small so the
/// lock never spans I/O.
struct LoopState {
    active: bool,
    auto_restart: bool,
    no_speech_count: u8,
}

/// Voice loop handle. Clone-safely via `Arc` — all interior state is
/// behind `Arc<…>` so a TUI handler and the silence-callback dispatcher
/// can both hold references.
pub struct VoiceLoop {
    cfg: VoiceConfig,
    recorder: Arc<dyn Recorder>,
    transcriber: Arc<dyn Transcriber>,
    state: Arc<Mutex<LoopState>>,
    events_tx: mpsc::UnboundedSender<VoiceEvent>,
    /// `notify_one()` from the silence callback wakes the dispatcher
    /// task that runs the transcribe-and-restart cycle. Decoupling the
    /// audio-thread callback from any tokio call keeps `cpal`'s
    /// `!Send` constraint manageable.
    cycle_notify: Arc<Notify>,
    /// TTS-pause guard. Cleared while TTS plays; set otherwise. The
    /// auto-restart path waits on this before re-arming the mic so the
    /// agent's own voice doesn't feed back as a new user message
    ///.
    tts_idle: Arc<Notify>,
    tts_playing_flag: Arc<AtomicBool>,
    /// Set when shutdown is requested so the dispatcher task can exit.
    shutdown: Arc<AtomicBool>,
    /// Audio-cue player (880 Hz on arm, 660 Hz×2 on stop). No-op when
    /// `voice.beep_enabled` is false.
    beeper: Arc<Beeper>,
}

impl VoiceLoop {
    /// Build a loop. The returned receiver is drained by the TUI event
    /// loop; the loop publishes lifecycle events onto it.
    pub fn new(
        cfg: VoiceConfig,
        recorder: Arc<dyn Recorder>,
        transcriber: Arc<dyn Transcriber>,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<VoiceEvent>) {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let beeper = Arc::new(Beeper::new(cfg.beep_enabled));
        let me = Arc::new(Self {
            cfg,
            recorder,
            transcriber,
            state: Arc::new(Mutex::new(LoopState {
                active: false,
                auto_restart: true,
                no_speech_count: 0,
            })),
            events_tx,
            cycle_notify: Arc::new(Notify::new()),
            tts_idle: Arc::new(Notify::new()),
            tts_playing_flag: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(AtomicBool::new(false)),
            beeper,
        });
        // Spawn the cycle-dispatcher. It awaits `cycle_notify` and
        // runs one transcribe-and-maybe-restart cycle per wakeup.
        let me_clone = Arc::clone(&me);
        tokio::spawn(async move { me_clone.dispatcher_loop().await });
        (me, events_rx)
    }

    /// Start the continuous (auto-restarting) loop. Idempotent — a
    /// second call while already active returns Ok without re-arming.
    pub async fn start_continuous(self: &Arc<Self>) -> Result<()> {
        self.start_internal(true).await
    }

    /// Start a single-shot recording — first silence triggers one
    /// transcription, then the loop returns to idle without
    /// re-arming.Single-press Ctrl+B.
    pub async fn start_single(self: &Arc<Self>) -> Result<()> {
        self.start_internal(false).await
    }

    async fn start_internal(self: &Arc<Self>, auto_restart: bool) -> Result<()> {
        {
            let mut s = self.state.lock().await;
            if s.active {
                return Ok(());
            }
            s.active = true;
            s.auto_restart = auto_restart;
            // Continuous reset; single-shot retains so a pre-existing
            // /voice rec pattern can still hit the silent-limit guard
            // across invocations.
            if auto_restart {
                s.no_speech_count = 0;
            }
        }
        self.arm_recorder().await?;
        let _ = self
            .events_tx
            .send(VoiceEvent::Status(VoiceStatus::Listening));
        Ok(())
    }

    /// Stop the loop and discard any in-flight buffer. The recorder is
    /// cancelled (not finalized) so a transcript that was about to be
    /// delivered is suppressed — `/voice off` means off, including any
    /// half-captured utterance the user didn't intend to send.
    ///
    /// Originally took a `force_transcribe` parameter to allow a
    /// "final-shot transcribe on stop" path. That path raced the
    /// dispatcher's silence-callback for the same buffer (could
    /// double-deliver a transcript), and no production caller used it,
    /// so the parameter was removed. Reintroduce only with explicit
    /// serialization against `run_cycle`.
    pub async fn stop(self: &Arc<Self>) {
        let was_active = {
            let mut s = self.state.lock().await;
            let prev = s.active;
            s.active = false;
            prev
        };
        if !was_active {
            return;
        }
        self.recorder.cancel();
        let _ = self.events_tx.send(VoiceEvent::Status(VoiceStatus::Idle));
    }

    /// Release the recorder and stop the dispatcher task. Called from
    /// `App::exit`; safe to call multiple times.
    pub async fn shutdown(self: &Arc<Self>) {
        {
            let mut s = self.state.lock().await;
            s.active = false;
        }
        self.recorder.shutdown();
        self.shutdown.store(true, Ordering::SeqCst);
        // Wake the dispatcher so it observes the shutdown flag.
        self.cycle_notify.notify_one();
    }

    pub async fn is_active(&self) -> bool {
        self.state.lock().await.active
    }

    pub fn status_sender(&self) -> mpsc::UnboundedSender<VoiceEvent> {
        self.events_tx.clone()
    }

    /// Current RMS reading from the live recording, scaled to int16
    /// amplitude (0..=32767). Returns 0 when the recorder is idle.
    /// Used by the TUI footer to render a live level meter while
    /// `voice_status == Listening`.
    pub fn current_rms(&self) -> u16 {
        self.recorder.current_rms()
    }

    /// Peak RMS sticky high-water mark for the current recording.
    /// Resets when a new recording starts.
    pub fn peak_rms(&self) -> u16 {
        self.recorder.peak_rms()
    }

    /// Mark TTS as playing — pauses the active recording so the
    /// speaker output doesn't leak into the mic. The reference implementation calls the
    /// equivalent path on every `speak_text` start.
    pub async fn notify_tts_started(self: &Arc<Self>) {
        self.tts_playing_flag.store(true, Ordering::SeqCst);
        // Cancel any live capture; the dispatcher will re-arm after
        // TTS finishes.
        if self.recorder.is_recording() {
            self.recorder.cancel();
        }
    }

    /// Mark TTS as finished — wakes the dispatcher's TTS-idle gate.
    /// If the loop is still active we re-arm the recorder so the user
    /// can answer without retyping `/voice on`. Uses `notify_one` (not
    /// `notify_waiters`) so a wake that lands *before* the dispatcher
    /// hits `notified().await` is latched into a permit and consumed
    /// on the next call — preventing a 60 s lost-wakeup stall.
    pub async fn notify_tts_finished(self: &Arc<Self>) {
        self.tts_playing_flag.store(false, Ordering::SeqCst);
        self.tts_idle.notify_one();
        if !self.state.lock().await.active {
            return;
        }
        // Brief delay — lets the OS flush
        // the speaker buffer before the mic re-opens.
        tokio::time::sleep(Duration::from_millis(300)).await;
        // Re-check `active` AFTER the sleep: a `/voice off` that lands
        // during the 300 ms window must NOT silently re-arm the mic.
        // Without this check, the user disables voice mode and the
        // recorder keeps capturing — the most user-hostile failure
        // mode this module has.
        if !self.state.lock().await.active {
            return;
        }
        let _ = self.arm_recorder().await;
        let _ = self
            .events_tx
            .send(VoiceEvent::Status(VoiceStatus::Listening));
    }

    // ── internal ────────────────────────────────────────────────────

    async fn arm_recorder(self: &Arc<Self>) -> Result<()> {
        let notify = Arc::clone(&self.cycle_notify);
        let cb: Box<dyn Fn() + Send + Sync + 'static> = Box::new(move || {
            notify.notify_one();
        });
        self.recorder.start(cb).context("start recorder")?;
        // Audible cue only after the mic actually opened — beeping
        // before a failed `start()` would mislead the user about
        // recorder state.
        self.beeper.start();
        Ok(())
    }

    async fn dispatcher_loop(self: Arc<Self>) {
        while !self.shutdown.load(Ordering::SeqCst) {
            self.cycle_notify.notified().await;
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            self.run_cycle().await;
        }
    }

    /// One transcribe-and-maybe-restart cycle. Pure async — never
    /// touches `cpal` directly.
    async fn run_cycle(self: &Arc<Self>) {
        // If we were stopped between the silence callback and now,
        // bail without speaking for the user.
        if !self.state.lock().await.active {
            return;
        }
        let _ = self
            .events_tx
            .send(VoiceEvent::Status(VoiceStatus::Transcribing));
        let wav = match self.recorder.stop() {
            Ok(p) => p,
            Err(e) => {
                let _ = self
                    .events_tx
                    .send(VoiceEvent::Error(format!("recorder stop failed: {e}")));
                None
            }
        };
        // Only beep when there's actually captured audio to process — a
        // silence-only cycle would announce "stop" without anything
        // having started in the user's perception.
        if wav.is_some() {
            self.beeper.stop();
        }

        let transcript = if let Some(p) = wav {
            self.transcribe_filter(p).await
        } else {
            None
        };

        // Re-check active under the lock so a stop() that landed mid-
        // transcription suppresses the event.
        let (still_active, halt, auto_restart) = {
            let mut s = self.state.lock().await;
            if !s.active {
                (false, false, false)
            } else if transcript.is_some() {
                s.no_speech_count = 0;
                (true, false, s.auto_restart)
            } else {
                s.no_speech_count = s.no_speech_count.saturating_add(1);
                let halt = s.no_speech_count >= self.cfg.no_speech_limit;
                if halt {
                    s.active = false;
                    s.no_speech_count = 0;
                }
                (!halt, halt, s.auto_restart)
            }
        };

        // Note: `halt` is only set when `transcript.is_none()` (the
        // empty-cycle branch is the only place that increments the
        // strike counter). So this just delivers the transcript when
        // the loop is still active — `|| halt` would be dead code.
        if let Some(t) = transcript {
            if still_active {
                let _ = self.events_tx.send(VoiceEvent::Transcript(t));
            }
        }

        if halt {
            let _ = self.events_tx.send(VoiceEvent::SilentLimitReached);
            let _ = self.events_tx.send(VoiceEvent::Status(VoiceStatus::Idle));
            return;
        }

        if !still_active {
            let _ = self.events_tx.send(VoiceEvent::Status(VoiceStatus::Idle));
            return;
        }

        // Wait for TTS to finish before re-arming, then either restart
        // (continuous) or go idle (single-shot). The `Notified` future
        // must be created *before* the flag check so a
        // `notify_one`-stored permit isn't missed between load and
        // await — this was a real 60-s lost-wakeup bug pre-fix. The
        // 60-s timeout is a backstop for crashed TTS, not the normal
        // path.
        let notified = self.tts_idle.notified();
        tokio::pin!(notified);
        if self.tts_playing_flag.load(Ordering::SeqCst) {
            let _ = tokio::time::timeout(Duration::from_secs(60), notified).await;
        }

        // Re-check after possibly waiting.
        if !self.state.lock().await.active {
            let _ = self.events_tx.send(VoiceEvent::Status(VoiceStatus::Idle));
            return;
        }

        if auto_restart {
            if let Err(e) = self.arm_recorder().await {
                let _ = self
                    .events_tx
                    .send(VoiceEvent::Error(format!("re-arm recorder failed: {e}")));
                let mut s = self.state.lock().await;
                s.active = false;
                let _ = self.events_tx.send(VoiceEvent::Status(VoiceStatus::Idle));
                return;
            }
            let _ = self
                .events_tx
                .send(VoiceEvent::Status(VoiceStatus::Listening));
        } else {
            // Single-shot: cycle done. Mark inactive and report idle.
            self.state.lock().await.active = false;
            let _ = self.events_tx.send(VoiceEvent::Status(VoiceStatus::Idle));
        }
    }

    async fn transcribe_filter(self: &Arc<Self>, path: std::path::PathBuf) -> Option<String> {
        let result = self.transcriber.transcribe_wav(&path).await;
        // Best-effort cleanup; cleanup failure is not fatal.
        let _ = tokio::fs::remove_file(&path).await;
        match result {
            Ok(text) => {
                let trimmed = text.trim().to_string();
                if trimmed.is_empty() || is_whisper_hallucination(&trimmed) {
                    None
                } else {
                    Some(trimmed)
                }
            }
            Err(e) => {
                let _ = self
                    .events_tx
                    .send(VoiceEvent::Error(format!("transcription failed: {e}")));
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::voice_capture::{write_wav, MockRecorder};
    use std::sync::Mutex as StdMutex;

    /// Stub transcriber returning the next queued text per call. Lets
    /// tests deterministically drive empty/non-empty cycles.
    struct StubTranscriber {
        queue: StdMutex<std::collections::VecDeque<Result<String>>>,
        /// Optional per-call delay so we can test "stop during
        /// transcribe" races.
        delay_ms: u64,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl StubTranscriber {
        fn new(replies: Vec<&str>) -> Arc<Self> {
            Arc::new(Self {
                queue: StdMutex::new(
                    replies
                        .into_iter()
                        .map(|s| Ok::<_, anyhow::Error>(s.to_string()))
                        .collect(),
                ),
                delay_ms: 0,
                calls: Default::default(),
            })
        }
        fn with_delay(replies: Vec<&str>, delay_ms: u64) -> Arc<Self> {
            Arc::new(Self {
                queue: StdMutex::new(
                    replies
                        .into_iter()
                        .map(|s| Ok::<_, anyhow::Error>(s.to_string()))
                        .collect(),
                ),
                delay_ms,
                calls: Default::default(),
            })
        }
    }

    #[async_trait]
    impl Transcriber for StubTranscriber {
        async fn transcribe_wav(&self, _path: &std::path::Path) -> Result<String> {
            if self.delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            }
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.queue
                .lock()
                .expect("queue")
                .pop_front()
                .unwrap_or_else(|| Ok(String::new()))
        }
    }

    fn fixture_wav() -> std::path::PathBuf {
        // Make a real WAV file so any code path that tries to read it
        // (e.g. the real transcriber, even though we use a stub here)
        // doesn't blow up. Short, peak-RMS above threshold so the
        // recorder's drop gate wouldn't reject it either.
        let samples: Vec<i16> = (0..16_000)
            .map(|i| ((i as f32 * 0.1).sin() * 8000.0) as i16)
            .collect();
        write_wav(&samples, 16_000).expect("write fixture")
    }

    fn cfg_no_speech(limit: u8) -> VoiceConfig {
        let mut c = VoiceConfig::default();
        c.no_speech_limit = limit;
        c
    }

    async fn drain_transcript(rx: &mut mpsc::UnboundedReceiver<VoiceEvent>) -> Option<String> {
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .ok()??;
            if let VoiceEvent::Transcript(t) = ev {
                return Some(t);
            }
        }
    }

    async fn wait_for(
        rx: &mut mpsc::UnboundedReceiver<VoiceEvent>,
        matcher: impl Fn(&VoiceEvent) -> bool,
    ) -> Option<VoiceEvent> {
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .ok()??;
            if matcher(&ev) {
                return Some(ev);
            }
        }
    }

    #[tokio::test]
    async fn start_continuous_is_idempotent() {
        let rec = Arc::new(MockRecorder::default());
        let trans = StubTranscriber::new(vec!["hello"]);
        let (vl, _rx) = VoiceLoop::new(VoiceConfig::default(), rec.clone(), trans);
        vl.start_continuous().await.unwrap();
        vl.start_continuous().await.unwrap(); // second call: no-op
        assert_eq!(
            rec.starts.load(Ordering::SeqCst),
            1,
            "second start_continuous must not re-arm the recorder"
        );
        vl.shutdown().await;
    }

    #[tokio::test]
    async fn successful_transcript_resets_no_speech_count() {
        let rec = Arc::new(MockRecorder::default());
        let trans = StubTranscriber::new(vec!["", "", "hello there", "", ""]);
        // limit=3 — we should NOT halt after 5 cycles because the
        // middle "hello there" resets the counter to 0.
        let (vl, mut rx) = VoiceLoop::new(cfg_no_speech(3), rec.clone(), trans);
        vl.start_continuous().await.unwrap();

        for _ in 0..5 {
            let wav = fixture_wav();
            rec.set_next_stop_path(Some(wav));
            rec.fire_silence();
            // Give the dispatcher a tick to process the cycle.
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Drain the event stream and verify exactly one transcript
        // arrived and SilentLimitReached did NOT fire.
        let mut got_transcript = 0;
        let mut got_limit = 0;
        while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
            match ev {
                VoiceEvent::Transcript(t) => {
                    assert_eq!(t, "hello there");
                    got_transcript += 1;
                }
                VoiceEvent::SilentLimitReached => got_limit += 1,
                _ => {}
            }
        }
        assert_eq!(got_transcript, 1);
        assert_eq!(
            got_limit, 0,
            "counter must reset on successful transcript so the 3-strike guard doesn't trip"
        );
        vl.shutdown().await;
    }

    #[tokio::test]
    async fn three_consecutive_empty_cycles_halts_loop() {
        let rec = Arc::new(MockRecorder::default());
        let trans = StubTranscriber::new(vec!["", "", ""]);
        let (vl, mut rx) = VoiceLoop::new(cfg_no_speech(3), rec.clone(), trans);
        vl.start_continuous().await.unwrap();
        for _ in 0..3 {
            rec.set_next_stop_path(Some(fixture_wav()));
            rec.fire_silence();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let ev = wait_for(&mut rx, |e| matches!(e, VoiceEvent::SilentLimitReached))
            .await
            .expect("SilentLimitReached should fire after 3 empty cycles");
        assert_eq!(ev, VoiceEvent::SilentLimitReached);
        assert!(!vl.is_active().await);
        vl.shutdown().await;
    }

    #[tokio::test]
    async fn stop_during_transcribe_discards_result() {
        let rec = Arc::new(MockRecorder::default());
        // 300ms delay gives us a window to call stop() while the
        // transcriber is still computing.
        let trans = StubTranscriber::with_delay(vec!["leaked transcript"], 300);
        let (vl, mut rx) = VoiceLoop::new(VoiceConfig::default(), rec.clone(), trans);
        vl.start_continuous().await.unwrap();
        rec.set_next_stop_path(Some(fixture_wav()));
        rec.fire_silence();
        // Stop immediately while transcribe is mid-flight.
        tokio::time::sleep(Duration::from_millis(50)).await;
        vl.stop().await;
        // Wait long enough for the transcriber to finish.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut got_transcript = false;
        while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
            if matches!(ev, VoiceEvent::Transcript(_)) {
                got_transcript = true;
            }
        }
        assert!(
            !got_transcript,
            "transcript that landed after stop() must not be delivered"
        );
        vl.shutdown().await;
    }

    #[tokio::test]
    async fn single_shot_mode_does_not_restart() {
        let rec = Arc::new(MockRecorder::default());
        let trans = StubTranscriber::new(vec!["one"]);
        let (vl, mut rx) = VoiceLoop::new(VoiceConfig::default(), rec.clone(), trans);
        vl.start_single().await.unwrap();
        rec.set_next_stop_path(Some(fixture_wav()));
        rec.fire_silence();
        let t = drain_transcript(&mut rx).await.expect("transcript");
        assert_eq!(t, "one");
        // Cycle done — recorder should not have been re-armed.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            rec.starts.load(Ordering::SeqCst),
            1,
            "single-shot must not auto-restart"
        );
        assert!(!vl.is_active().await);
        vl.shutdown().await;
    }

    #[tokio::test]
    async fn tts_notify_pauses_recorder_during_playback() {
        let rec = Arc::new(MockRecorder::default());
        let trans = StubTranscriber::new(vec!["hello", "world"]);
        let (vl, mut rx) = VoiceLoop::new(VoiceConfig::default(), rec.clone(), trans);
        vl.start_continuous().await.unwrap();
        // First utterance.
        rec.set_next_stop_path(Some(fixture_wav()));
        rec.fire_silence();
        let _ = drain_transcript(&mut rx).await.expect("first transcript");
        // Mark TTS playing — recorder must be cancelled.
        vl.notify_tts_started().await;
        assert!(
            !rec.is_recording(),
            "notify_tts_started must cancel the live recording"
        );
        assert!(rec.cancels.load(Ordering::SeqCst) >= 1);
        // After TTS finishes, recorder re-arms.
        vl.notify_tts_finished().await;
        // Give the re-arm + 300ms sleep time to complete.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(rec.is_recording(), "loop must re-arm mic after TTS done");
        vl.shutdown().await;
    }

    /// Regression for the bug where `/voice off` during the 300 ms
    /// post-TTS sleep was lost: `notify_tts_finished` re-armed the
    /// recorder even though the user had disabled voice mode. Most
    /// user-hostile failure mode in the module — mic re-opens after
    /// the user told it not to.
    #[tokio::test]
    async fn stop_during_post_tts_sleep_does_not_rearm_mic() {
        let rec = Arc::new(MockRecorder::default());
        let trans = StubTranscriber::new(vec!["hi"]);
        let (vl, _rx) = VoiceLoop::new(VoiceConfig::default(), rec.clone(), trans);
        vl.start_continuous().await.unwrap();
        vl.notify_tts_started().await;
        assert!(!rec.is_recording(), "tts_started must cancel mic");
        let starts_before = rec.starts.load(Ordering::SeqCst);

        // Fire notify_tts_finished + stop() concurrently — stop must
        // win the post-sleep recheck.
        let vl_clone = Arc::clone(&vl);
        let finish_handle = tokio::spawn(async move {
            vl_clone.notify_tts_finished().await;
        });
        // Land /voice off well within the 300 ms sleep window.
        tokio::time::sleep(Duration::from_millis(50)).await;
        vl.stop().await;
        // Let notify_tts_finished's sleep elapse.
        let _ = finish_handle.await;
        tokio::time::sleep(Duration::from_millis(400)).await;

        assert!(
            !rec.is_recording(),
            "recorder must NOT be re-armed when /voice off lands during post-TTS sleep"
        );
        assert_eq!(
            rec.starts.load(Ordering::SeqCst),
            starts_before,
            "no new recorder.start() call after stop()"
        );
        vl.shutdown().await;
    }

    /// Spec: when Whisper hallucinates on near-silence (the classic
    /// "Thanks for watching!" / "Subscribe to my channel" outputs that the
    /// `HALLUCINATION_PHRASES` set in `voice_capture` enumerates), the
    /// orchestrator must filter the transcript at `transcribe_filter` and
    /// suppress the `VoiceEvent::Transcript` event entirely. Otherwise the
    /// agent would receive bogus user messages every time the room goes
    /// quiet. This guard rides the same dispatch path as a real cycle so a
    /// future refactor that bypasses `transcribe_filter` will break it.
    #[tokio::test]
    async fn whisper_hallucinations_are_filtered_to_empty_transcript() {
        let rec = Arc::new(MockRecorder::default());
        // Three classic hallucinations followed by a real utterance. The
        // first three must be suppressed; the fourth must surface.
        let trans = StubTranscriber::new(vec![
            "Thanks for watching!",
            "Subscribe to my channel.",
            "you",
            "schedule a meeting tomorrow",
        ]);
        // limit=10 so the silent-cycle guard doesn't preempt the test
        // before the real transcript fires (each hallucination still
        // counts as an empty cycle in the no-speech counter).
        let mut cfg = VoiceConfig::default();
        cfg.no_speech_limit = 10;
        let (vl, mut rx) = VoiceLoop::new(cfg, rec.clone(), trans);
        vl.start_continuous().await.unwrap();

        for _ in 0..4 {
            rec.set_next_stop_path(Some(fixture_wav()));
            rec.fire_silence();
            tokio::time::sleep(Duration::from_millis(60)).await;
        }

        // Collect every Transcript that came through. Only the last
        // utterance should appear — the three hallucinations must be
        // dropped by `is_whisper_hallucination`.
        let mut transcripts: Vec<String> = Vec::new();
        while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(120), rx.recv()).await {
            if let VoiceEvent::Transcript(t) = ev {
                transcripts.push(t);
            }
        }
        assert_eq!(
            transcripts.len(),
            1,
            "expected exactly one transcript through the filter, got {transcripts:?}"
        );
        assert_eq!(transcripts[0], "schedule a meeting tomorrow");
        vl.shutdown().await;
    }

    /// Regression for the Notify lost-wakeup race: if
    /// `notify_tts_finished` fires *before* the dispatcher's TTS-wait
    /// reaches `notified().await`, the wakeup must be latched (not
    /// dropped) so the dispatcher doesn't stall on its 60-s timeout.
    /// Implementation contract: `Notified` is created before the flag
    /// check and `notify_one` (which stores a permit) replaces
    /// `notify_waiters` (which doesn't).
    #[tokio::test]
    async fn tts_finish_before_dispatcher_awaits_does_not_stall() {
        let rec = Arc::new(MockRecorder::default());
        let trans = StubTranscriber::new(vec!["hello"]);
        let (vl, mut rx) = VoiceLoop::new(VoiceConfig::default(), rec.clone(), trans);
        vl.start_continuous().await.unwrap();

        // Force the dispatcher into the TTS-wait branch: start TTS,
        // fire silence (transcript will deliver, then dispatcher
        // checks tts_playing_flag and enters the wait).
        vl.notify_tts_started().await;
        rec.set_next_stop_path(Some(fixture_wav()));
        rec.fire_silence();

        // Immediately mark TTS as finished — the wakeup is racing
        // the dispatcher's pin+await. Without notify_one's permit
        // semantics the wakeup is lost and the test hangs ~60 s.
        vl.notify_tts_finished().await;

        // The dispatcher should re-arm well within a reasonable
        // window. A pre-fix run blew past this and only completed
        // after the 60-s backstop.
        let armed_within_window = async {
            for _ in 0..40 {
                if rec.is_recording() {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            false
        }
        .await;
        assert!(
            armed_within_window,
            "dispatcher must re-arm within ~2 s; lost-wakeup bug stalls 60 s"
        );

        // Drain any pending events so shutdown is clean.
        while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(10), rx.recv()).await {}
        vl.shutdown().await;
    }
}
