use super::*;

#[test]
fn default_browser_config_values() {
    let cfg = BrowserConfig::default();
    assert!(cfg.enabled);
    assert!(cfg.headless);
    assert!(cfg.executable.is_none());
    assert_eq!(cfg.cdp_port, 9222);
    assert!(!cfg.no_sandbox);
    assert_eq!(cfg.timeout_ms, 30000);
    assert_eq!(cfg.startup_timeout_ms, 15000);
}

#[test]
fn parse_browser_config_toml() {
    let toml_str = r#"
[browser]
enabled = false
headless = false
executable = "/usr/bin/chromium"
cdp_port = 9333
no_sandbox = true
timeout_ms = 60000
startup_timeout_ms = 20000
"#;
    let cfg: Config = toml::from_str(toml_str).expect("should parse");
    assert!(!cfg.browser.enabled);
    assert!(!cfg.browser.headless);
    assert_eq!(cfg.browser.executable.as_deref(), Some("/usr/bin/chromium"));
    assert_eq!(cfg.browser.cdp_port, 9333);
    assert!(cfg.browser.no_sandbox);
    assert_eq!(cfg.browser.timeout_ms, 60000);
    assert_eq!(cfg.browser.startup_timeout_ms, 20000);
}

#[test]
fn parse_empty_toml_yields_browser_defaults() {
    let cfg: Config = toml::from_str("").expect("should parse");
    assert!(cfg.browser.enabled);
    assert!(cfg.browser.headless);
    assert_eq!(cfg.browser.cdp_port, 9222);
}

#[test]
fn apply_setting_browser_headless() {
    let mut cfg = Config::default();
    cfg.apply_setting("browser.headless", "false").unwrap();
    assert!(!cfg.browser.headless);
}

#[test]
fn apply_setting_browser_cdp_port_hidden() {
    let mut cfg = Config::default();
    assert!(cfg.apply_setting("browser.cdp_port", "9333").is_err());
}

#[test]
fn display_settings_contains_browser() {
    let cfg = Config::default();
    let display = cfg.display_settings();
    assert!(display.contains("browser.enabled"));
    assert!(display.contains("browser.headless"));
    assert!(!display.contains("browser.cdp_port"));
}

#[test]
fn tts_config_defaults() {
    let cfg = TtsConfig::default();
    assert!(!cfg.enabled);
    assert!(cfg.models.is_empty());
    assert_eq!(cfg.default_voice, "alloy");
    assert_eq!(cfg.default_format, "mp3");
    assert_eq!(cfg.max_text_length, 4096);
    assert_eq!(cfg.timeout_ms, 30_000);
    assert!(!cfg.auto_mode);
}

#[test]
fn apply_setting_tts_enabled() {
    let mut cfg = Config::default();
    cfg.apply_setting("tts.enabled", "true").unwrap();
    assert!(cfg.tts.enabled);
}

#[test]
fn apply_setting_tts_auto_mode() {
    let mut cfg = Config::default();
    cfg.apply_setting("tts.auto_mode", "true").unwrap();
    assert!(cfg.tts.auto_mode);
}

#[test]
fn apply_setting_tts_default_voice() {
    let mut cfg = Config::default();
    cfg.apply_setting("tts.default_voice", "nova").unwrap();
    assert_eq!(cfg.tts.default_voice, "nova");
}

#[test]
fn apply_setting_tts_default_format() {
    let mut cfg = Config::default();
    cfg.apply_setting("tts.default_format", "opus").unwrap();
    assert_eq!(cfg.tts.default_format, "opus");
}

#[test]
fn apply_setting_tts_default_format_invalid() {
    let mut cfg = Config::default();
    assert!(cfg.apply_setting("tts.default_format", "ogg").is_err());
}

#[test]
fn parse_tts_config() {
    let toml_str = r#"
[tts]
enabled = true
default_voice = "nova"
default_format = "opus"
auto_mode = true

[[tts.models]]
provider = "openai"
model = "tts-1"

[[tts.models]]
provider = "elevenlabs"
voice = "21m00Tcm4TlvDq8ikWAM"
api_key_env = "ELEVENLABS_API_KEY"
"#;
    let config: Config = toml::from_str(toml_str).unwrap();
    assert!(config.tts.enabled);
    assert!(config.tts.auto_mode);
    assert_eq!(config.tts.default_voice, "nova");
    assert_eq!(config.tts.default_format, "opus");
    assert_eq!(config.tts.models.len(), 2);
    assert_eq!(config.tts.models[0].provider, "openai");
    assert_eq!(config.tts.models[0].model.as_deref(), Some("tts-1"));
    assert_eq!(config.tts.models[1].provider, "elevenlabs");
    assert_eq!(
        config.tts.models[1].voice.as_deref(),
        Some("21m00Tcm4TlvDq8ikWAM")
    );
}

// ── Feature #10: Audio config tests ──

#[test]
fn parse_audio_config() {
    let toml_str = r#"
[audio]
enabled = true
max_file_size = 20971520
min_file_size = 1024
language = "en"
timeout_ms = 60000

[[audio.models]]
provider = "openai"
model = "whisper-1"

[[audio.models]]
provider = "groq"
model = "whisper-large-v3-turbo"
api_key_env = "GROQ_API_KEY"
"#;
    let cfg: Config = toml::from_str(toml_str).unwrap();
    assert!(cfg.audio.enabled);
    assert_eq!(cfg.audio.max_file_size, 20_971_520);
    assert_eq!(cfg.audio.min_file_size, 1024);
    assert_eq!(cfg.audio.language.as_deref(), Some("en"));
    assert_eq!(cfg.audio.timeout_ms, 60_000);
    assert_eq!(cfg.audio.models.len(), 2);
    assert_eq!(cfg.audio.models[0].provider, "openai");
    assert_eq!(cfg.audio.models[0].model.as_deref(), Some("whisper-1"));
    assert_eq!(cfg.audio.models[1].provider, "groq");
    assert_eq!(
        cfg.audio.models[1].api_key_env.as_deref(),
        Some("GROQ_API_KEY")
    );
}

#[test]
fn audio_config_defaults() {
    let cfg = AudioConfig::default();
    assert!(!cfg.enabled);
    assert!(cfg.models.is_empty());
    assert_eq!(cfg.max_file_size, 20 * 1024 * 1024);
    assert_eq!(cfg.min_file_size, 1024);
    assert!(!cfg.echo_transcript);
}
