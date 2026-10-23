//! Voice mode configuration.
//!
//! Holds the tunables for `/voice` — mic capture VAD thresholds, the
//! three-strike silent-limit guard, TTS playback toggles, and the
//! auto-submit switch. STT provider chain lives in `audio.models`
//! ([`AudioConfig`](super::AudioConfig)) and TTS provider chain in
//! `tts.models` ([`TtsConfig`](super::TtsConfig)); this section only
//! owns the loop-level behavior.
//!
//! Defaults mirror standard voice-mode parameters (RMS threshold 200,
//! 3-second silence window, three-strike auto-disable) — those were
//! tuned against real microphones over many iterations and are the
//! contract `voice_capture` ports line-for-line.

use serde::{Deserialize, Serialize};

/// Voice-mode loop and UX tunables.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceConfig {
    /// Whether voice mode is permitted. `false` makes `/voice on` print a
    /// hint and refuse to start. Independent of `audio.enabled` (which
    /// gates the underlying STT provider chain).
    pub enabled: bool,
    /// RMS level (int16 range 0–32767) below which a chunk counts as
    /// silence. 200 is tuned against typical USB/laptop microphones.
    pub silence_threshold_rms: u16,
    /// Continuous seconds below `silence_threshold_rms` (after at least
    /// one confirmed speech burst) required before the recorder stops
    /// and triggers transcription.
    pub silence_duration_secs: f32,
    /// Seconds above `silence_threshold_rms` required before a burst is
    /// confirmed as speech. Prevents short clicks/coughs from arming
    /// the silence timer.
    pub min_speech_duration_secs: f32,
    /// Seconds of sub-threshold dip tolerated mid-speech before the
    /// speech-detection state resets. 0.3s is short enough to avoid
    /// false silences in natural speech, long enough to ignore
    /// micro-pauses between syllables.
    pub dip_tolerance_secs: f32,
    /// Maximum seconds to wait for any speech at all before the
    /// recorder gives up and fires the silence callback with an empty
    /// buffer. Bounds the worst case where the mic is dead/muted.
    pub max_wait_secs: f32,
    /// Consecutive empty/hallucinated transcription cycles before the
    /// continuous loop disables itself. Prevents runaway battery drain
    /// when the microphone is unplugged or the user has walked away.
    pub no_speech_limit: u8,
    /// PCM sample rate the recorder captures at. Whisper natively
    /// expects 16 kHz; changing this means resampling before STT.
    pub sample_rate_hz: u32,
    /// Whether assistant turn output is spoken via the configured TTS
    /// provider after each reply while a voice loop is active.
    /// Toggled by `/voice tts`.
    pub auto_tts: bool,
    /// Whether the recorder plays short start/stop beeps to signal
    /// capture state.
    pub beep_enabled: bool,
    /// Whether a recognized transcript is auto-submitted as the next
    /// user message. `false` puts the transcript in the composer for
    /// the user to review and submit manually.
    pub auto_submit: bool,
    /// Keybinding for push-to-talk single-utterance recording. The TUI
    /// arms a single-shot recorder on keydown and transcribes on keyup.
    /// Format is `<modifier>-<key>` lowercase, e.g. `ctrl-b`, `alt-v`.
    pub record_key: String,
    /// The app's dictation key, in the web chord syntax (`mod+shift+space`).
    /// A lone modifier code (`altright`) means hold that key to talk.
    pub dictation_shortcut: String,
    /// Hold the shortcut to record and release to stop; a quick tap still
    /// toggles. `false` makes every press a toggle.
    pub hold_to_record: bool,
    /// Send a dictated message as soon as it is transcribed, instead of
    /// leaving it in the composer for review.
    pub dictation_auto_send: bool,
    /// Also copy each transcript to the clipboard.
    pub copy_transcript: bool,
    /// Desktop only: the dictation shortcut also works while another app is
    /// in front, and brings Borg forward to listen.
    pub background_shortcut: bool,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            silence_threshold_rms: 200,
            silence_duration_secs: 3.0,
            min_speech_duration_secs: 0.3,
            dip_tolerance_secs: 0.3,
            max_wait_secs: 15.0,
            no_speech_limit: 3,
            sample_rate_hz: 16_000,
            auto_tts: false,
            beep_enabled: true,
            auto_submit: true,
            record_key: "ctrl-b".to_string(),
            dictation_shortcut: "mod+shift+space".to_string(),
            hold_to_record: true,
            dictation_auto_send: false,
            copy_transcript: false,
            background_shortcut: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// VAD defaults are a contract, not an accident. The voice-mode
    /// numbers were tuned against real mics over many rounds; changing
    /// one of these defaults must be a deliberate edit, not a typo
    /// that slips into a refactor.
    #[test]
    fn defaults_are_stable() {
        let cfg = VoiceConfig::default();
        assert_eq!(cfg.silence_threshold_rms, 200);
        assert!((cfg.silence_duration_secs - 3.0).abs() < f32::EPSILON);
        assert!((cfg.min_speech_duration_secs - 0.3).abs() < f32::EPSILON);
        assert!((cfg.dip_tolerance_secs - 0.3).abs() < f32::EPSILON);
        assert!((cfg.max_wait_secs - 15.0).abs() < f32::EPSILON);
        assert_eq!(cfg.no_speech_limit, 3);
        assert_eq!(cfg.sample_rate_hz, 16_000);
        assert!(cfg.beep_enabled);
        assert!(cfg.auto_submit);
        assert!(!cfg.auto_tts);
        assert!(!cfg.enabled);
        // the keybinding is part of the parity contract.
        assert_eq!(cfg.record_key, "ctrl-b");
        // The app's dictation defaults: review before sending, hold or tap.
        assert_eq!(cfg.dictation_shortcut, "mod+shift+space");
        assert!(cfg.hold_to_record);
        assert!(!cfg.dictation_auto_send);
        assert!(!cfg.copy_transcript);
        assert!(!cfg.background_shortcut);
    }

    #[test]
    fn deserializes_partial_section() {
        let cfg: VoiceConfig =
            serde_json::from_value(serde_json::json!({"enabled": true, "auto_tts": true})).unwrap();
        assert!(cfg.enabled);
        assert!(cfg.auto_tts);
        // Unspecified fields fall back to defaults — settings are
        // partial-overlay, not all-or-nothing.
        assert_eq!(cfg.silence_threshold_rms, 200);
        assert_eq!(cfg.no_speech_limit, 3);
    }
}
