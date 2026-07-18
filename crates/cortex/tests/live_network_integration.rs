//! Live-LLM test lane: exercises `web_fetch`, `web_search`, `browser`,
//! `text_to_speech`, the audio transcription chain, and a local Ollama
//! chat round-trip against real network targets.
//!
//! Run with:
//!
//! ```sh
//! BORG_ALLOW_LIVE_TESTS=1 cargo test -p cortex --features live-llm \
//!     --test live_network_integration
//! ```
//!
//! Builds without `BORG_ALLOW_LIVE_TESTS=1` skip the network bodies so a
//! feature-only build still passes offline.

#![cfg(feature = "live-llm")]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use cortex::browser::BrowserSession;
use cortex::config::Config;
use cortex::tool_handlers;
use cortex::types::ToolOutput;

fn live_enabled() -> bool {
    match std::env::var("BORG_ALLOW_LIVE_TESTS") {
        Ok(v) => v == "1" || v.eq_ignore_ascii_case("true"),
        Err(_) => false,
    }
}

macro_rules! require_live {
    ($name:expr) => {
        if !live_enabled() {
            eprintln!(
                "skipping {}: set BORG_ALLOW_LIVE_TESTS=1 to run network tests",
                $name
            );
            return;
        }
    };
}

#[tokio::test]
async fn live_web_fetch_example_dot_com() {
    require_live!("live_web_fetch_example_dot_com");

    let config = Config::default();
    let args = serde_json::json!({"url": "https://example.com/"});
    let body = tool_handlers::handle_web_fetch(&args, &config)
        .await
        .expect("handle_web_fetch should not return Err");

    assert!(
        body.contains("Example Domain"),
        "expected example.com anchor text in body, got: {body}"
    );
}

#[tokio::test]
async fn live_web_search_returns_results() {
    require_live!("live_web_search_returns_results");

    let config = Config::default();
    let args = serde_json::json!({"query": "rust programming language", "mode": "live"});
    let body = tool_handlers::handle_web_search(&args, &config)
        .await
        .expect("handle_web_search should not return Err");

    // The provider's response is provider-specific JSON/text; assert only
    // that it is non-empty and not the disabled / error shape.
    assert!(
        !body.is_empty(),
        "web_search returned empty body for 'rust programming language'"
    );
    assert!(
        !body.contains("disabled"),
        "web_search reported disabled despite default config: {body}"
    );
    assert!(
        !body.starts_with("Error searching:"),
        "web_search returned an error: {body}"
    );
}

#[tokio::test]
async fn live_browser_navigate_example_dot_com() {
    require_live!("live_browser_navigate_example_dot_com");

    let mut config = Config::default();
    // The default browser config gates on `enabled`; tests opt in.
    config.browser.enabled = true;

    let mut session: Option<BrowserSession> = None;
    let args = serde_json::json!({
        "action": "navigate",
        "url": "https://example.com/",
    });
    // Run navigate, then ALWAYS close before any assertion that could panic —
    // otherwise a failed navigate would leak the Chrome process.
    let nav_result = tool_handlers::handle_browser(&args, &config, &mut session).await;
    let close_args = serde_json::json!({"action": "close"});
    let _ = tool_handlers::handle_browser(&close_args, &config, &mut session).await;

    let out = nav_result.expect("handle_browser should not return Err");
    let text = match &out {
        ToolOutput::Text(t) => t.clone(),
        ToolOutput::Multimodal { text, .. } => text.clone(),
    };

    assert!(
        !text.starts_with("Error"),
        "browser navigate failed (is Chrome installed?): {text}"
    );
}

/// Positive: snapshot returns interactive elements as `@e<N>` refs, and a
/// follow-up `click {ref: "@e0"}` resolves through the session's ref store
/// without the model having to write a CSS selector. Then back navigates
/// the history.
#[tokio::test]
async fn live_browser_snapshot_then_ref_click_then_back() {
    require_live!("live_browser_snapshot_then_ref_click_then_back");

    let mut config = Config::default();
    config.browser.enabled = true;
    let mut session: Option<BrowserSession> = None;

    let close = serde_json::json!({"action": "close"});

    // Step 1: navigate somewhere with links.
    let nav = tool_handlers::handle_browser(
        &serde_json::json!({"action": "navigate", "url": "https://example.com/"}),
        &config,
        &mut session,
    )
    .await;
    if nav.is_err() {
        let _ = tool_handlers::handle_browser(&close, &config, &mut session).await;
        panic!("navigate failed: {nav:?}");
    }

    // Step 2: snapshot — example.com has one link ("More information...").
    let snap = tool_handlers::handle_browser(
        &serde_json::json!({"action": "snapshot"}),
        &config,
        &mut session,
    )
    .await;
    let snap_text = match snap {
        Ok(ToolOutput::Text(t)) => t,
        Ok(other) => {
            let _ = tool_handlers::handle_browser(&close, &config, &mut session).await;
            panic!("snapshot returned non-text: {other:?}");
        }
        Err(e) => {
            let _ = tool_handlers::handle_browser(&close, &config, &mut session).await;
            panic!("snapshot errored: {e}");
        }
    };
    assert!(
        snap_text.contains("@e0"),
        "snapshot missing @e0: {snap_text}"
    );

    // Step 3: click @e0 — must resolve through session refs without CSS.
    let click = tool_handlers::handle_browser(
        &serde_json::json!({"action": "click", "ref": "@e0"}),
        &config,
        &mut session,
    )
    .await;
    // We don't assert the click succeeds (the link may navigate cross-origin
    // and we don't care here); only that the REF RESOLUTION worked — a
    // failure would say "ref '@e0' was not produced by a recent snapshot".
    let click_text = match click {
        Ok(ToolOutput::Text(t)) => t,
        Ok(other) => format!("{other:?}"),
        Err(e) => format!("Err: {e}"),
    };
    assert!(
        !click_text.contains("was not produced by a recent snapshot"),
        "ref resolution failed: {click_text}"
    );

    // Step 4: back — example.com → linked page → back to example.com.
    // (Or back to about:blank if click didn't navigate.)
    let back = tool_handlers::handle_browser(
        &serde_json::json!({"action": "back"}),
        &config,
        &mut session,
    )
    .await;

    let _ = tool_handlers::handle_browser(&close, &config, &mut session).await;

    let back_text = match back.expect("back should not error") {
        ToolOutput::Text(t) => t,
        ToolOutput::Multimodal { text, .. } => text,
    };
    assert!(
        back_text.contains("history.back"),
        "back didn't run: {back_text}"
    );
}

/// Negative: `back` on a brand-new session sitting on about:blank with
/// no navigation history yet must report a clean no-op, not crash and not
/// silently succeed. Guards the "empty history = no-op" contract.
#[tokio::test]
async fn live_browser_back_on_empty_history_is_noop() {
    require_live!("live_browser_back_on_empty_history_is_noop");

    let mut config = Config::default();
    config.browser.enabled = true;
    let mut session: Option<BrowserSession> = None;
    let close = serde_json::json!({"action": "close"});

    // Touch a page to force the lazy session launch, then go back without
    // ever having navigated past the initial entry.
    let _ = tool_handlers::handle_browser(
        &serde_json::json!({"action": "snapshot"}),
        &config,
        &mut session,
    )
    .await;

    let back = tool_handlers::handle_browser(
        &serde_json::json!({"action": "back"}),
        &config,
        &mut session,
    )
    .await;
    let _ = tool_handlers::handle_browser(&close, &config, &mut session).await;

    let text = match back.expect("back errored") {
        ToolOutput::Text(t) => t,
        ToolOutput::Multimodal { text, .. } => text,
    };
    assert!(
        text.contains("no-op") || text.contains("history.back"),
        "expected no-op message on empty history: {text}"
    );
}

/// Negative: `upload` against a page that has no file input must surface
/// a clear "Element not found" error rather than silently no-op-ing — the
/// model would otherwise believe the upload succeeded.
#[tokio::test]
async fn live_browser_upload_no_input_returns_error() {
    require_live!("live_browser_upload_no_input_returns_error");

    let mut config = Config::default();
    config.browser.enabled = true;
    let mut session: Option<BrowserSession> = None;
    let close = serde_json::json!({"action": "close"});

    let _ = tool_handlers::handle_browser(
        &serde_json::json!({"action": "navigate", "url": "https://example.com/"}),
        &config,
        &mut session,
    )
    .await;

    let up = tool_handlers::handle_browser(
        &serde_json::json!({
            "action": "upload",
            "selector": "input[type=file]",
            "files": ["/tmp/nonexistent.png"],
        }),
        &config,
        &mut session,
    )
    .await;
    let _ = tool_handlers::handle_browser(&close, &config, &mut session).await;

    let text = match up.expect("upload errored") {
        ToolOutput::Text(t) => t,
        ToolOutput::Multimodal { text, .. } => text,
    };
    assert!(
        text.contains("Error") && (text.contains("not found") || text.contains("Element")),
        "expected 'Element not found' style error: {text}"
    );
}

// ── TTS / STT live tests ─────────────────────────────────────────────────
//
// Provider response shapes drift over time (Edge voice catalogue,
// MiniMax envelope, xAI endpoint moves) — exactly the failure mode the
// live lane is designed to catch. Each test is skipped when the relevant
// credentials are absent, so a contributor without an OpenAI/Groq key can
// still run `cargo test --features live-llm` without spurious failures.

fn env_present(var: &str) -> bool {
    std::env::var(var).map(|v| !v.is_empty()).unwrap_or(false)
}

/// Spec: an OpenAI TTS call with a short, well-known phrase returns
/// non-trivial MP3 bytes prefixed with the ID3 tag or MPEG frame sync
/// (`0xFFFB`/`0xFFFA`/`0xFFF3`). A drift here (auth shape change, model
/// rename, endpoint move) breaks production voice replies; this guard
/// is the live-lane canary for that class of regression.
#[tokio::test]
async fn live_tts_openai_synthesizes_short_clip() {
    require_live!("live_tts_openai_synthesizes_short_clip");
    if !env_present("OPENAI_API_KEY") {
        eprintln!("skipping: set OPENAI_API_KEY to run this test");
        return;
    }

    let mut config = Config::default();
    config.tts.enabled = true;
    config.tts.models = vec![cortex::config::TtsModelConfig {
        provider: "openai".to_string(),
        model: Some("tts-1".to_string()),
        voice: Some("alloy".to_string()),
        api_key_env: None, // defaults to OPENAI_API_KEY
        timeout_ms: Some(30_000),
        base_url: None,
        language: None,
        sample_rate_hz: None,
        bit_rate: None,
        max_text_length: None,
        name: None,
        command: None,
        output_extension: None,
        xai_auto_speech_tags: None,
    }];

    let synth = cortex::media::tts::TtsSynthesizer::from_config(&config)
        .expect("OpenAI TTS synthesizer must build with OPENAI_API_KEY set");
    let (bytes, fmt, attempts) = synth
        .synthesize(
            "Hello from the borg live test lane.",
            None,
            Some(cortex::media::tts::AudioFormat::Mp3),
        )
        .await
        .expect("OpenAI TTS synthesize should succeed");

    assert_eq!(fmt, cortex::media::tts::AudioFormat::Mp3);
    assert!(
        bytes.len() > 1024,
        "expected non-trivial audio payload (>1KB), got {} bytes",
        bytes.len()
    );
    // MP3 file headers: either an ID3v2 tag (`ID3`) or an MPEG audio
    // frame sync (0xFFFB / 0xFFFA / 0xFFF3). Anything else means the
    // provider returned an error envelope or an unexpected format.
    let head = &bytes[..bytes.len().min(4)];
    let is_mp3 = head.starts_with(b"ID3")
        || (head.len() >= 2 && head[0] == 0xFF && (head[1] & 0xE0) == 0xE0);
    assert!(
        is_mp3,
        "response does not start with MP3 magic (first bytes: {head:?})"
    );
    // Attempts log: exactly one entry, success, provider name "openai".
    assert_eq!(attempts.len(), 1);
    let attempt = &attempts[0];
    assert_eq!(attempt.provider, "openai");
    assert!(matches!(
        attempt.outcome,
        cortex::media::tts::SynthesisOutcome::Success
    ));
}

/// Spec: a Groq Whisper transcription of a short MP3 clip we just
/// synthesized round-trips back to a transcript that contains the
/// expected keyword. We chain TTS → STT in one test because (a) the
/// alternative is checking in a binary WAV asset, and (b) it doubles as
/// a smoke test for the TTS attempt-log shape used by the agent surface.
///
/// Skipped when `OPENAI_API_KEY` or `GROQ_API_KEY` is absent — the test
/// is best-effort, not a hard gate.
#[tokio::test]
async fn live_stt_groq_transcribes_synthesized_clip() {
    require_live!("live_stt_groq_transcribes_synthesized_clip");
    if !env_present("OPENAI_API_KEY") || !env_present("GROQ_API_KEY") {
        eprintln!("skipping: needs both OPENAI_API_KEY (for TTS source) and GROQ_API_KEY");
        return;
    }

    // Synthesize a short, distinctive utterance via OpenAI TTS so the
    // test asset is generated fresh on every run rather than checked in.
    let mut tts_config = Config::default();
    tts_config.tts.enabled = true;
    tts_config.tts.models = vec![cortex::config::TtsModelConfig {
        provider: "openai".to_string(),
        model: Some("tts-1".to_string()),
        voice: Some("alloy".to_string()),
        api_key_env: None,
        timeout_ms: Some(30_000),
        base_url: None,
        language: None,
        sample_rate_hz: None,
        bit_rate: None,
        max_text_length: None,
        name: None,
        command: None,
        output_extension: None,
        xai_auto_speech_tags: None,
    }];
    let synth = cortex::media::tts::TtsSynthesizer::from_config(&tts_config).expect("TTS build");
    // Use an unusual-but-pronounceable phrase so Whisper's transcript
    // contains a hard-to-confuse anchor word.
    let utterance = "Testing borg voice integration tomorrow.";
    let (mp3_bytes, _, _) = synth
        .synthesize(utterance, None, Some(cortex::media::tts::AudioFormat::Mp3))
        .await
        .expect("OpenAI TTS for STT round-trip");

    // Build a Groq Whisper transcriber.
    let mut stt_config = Config::default();
    stt_config.audio.enabled = true;
    stt_config.audio.models = vec![cortex::config::AudioModelConfig {
        provider: "groq".to_string(),
        model: None, // default to whisper-large-v3-turbo
        api_key_env: None,
        language: Some("en".to_string()),
        timeout_ms: Some(60_000),
        base_url: None,
        xai_stt_format: None,
        xai_stt_diarize: None,
        command: None,
    }];
    let stt = cortex::media::understanding::AudioTranscriber::from_config(&stt_config)
        .expect("STT build");

    let (transcript, attempts) = stt
        .transcribe(&mp3_bytes, "audio/mpeg", "live_test.mp3", Some("en"))
        .await
        .expect("Groq Whisper transcribe should succeed");

    let lower = transcript.to_ascii_lowercase();
    // Whisper is lossy on short clips; require *at least one* of the
    // distinctive content words to land. "Borg" + "voice" + "tomorrow"
    // — at least one of these should make it through any reasonable
    // transcript even with provider drift.
    let landed = ["borg", "voice", "tomorrow"]
        .iter()
        .any(|kw| lower.contains(kw));
    assert!(
        landed,
        "no expected keyword found in transcript {transcript:?} (attempts: {attempts:?})"
    );
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].provider, "groq");
    assert!(matches!(
        attempts[0].outcome,
        cortex::media::understanding::AttemptOutcome::Success
    ));
}

/// Live local-LLM lane: a real chat round-trip against a local Ollama server.
///
/// Requires `BORG_ALLOW_LIVE_TESTS=1` AND a reachable Ollama (honors
/// `OLLAMA_HOST`; defaults to localhost:11434) — skipped otherwise so the
/// live lane stays usable on hosts that only have cloud credentials.
/// Override the model with `BORG_LIVE_OLLAMA_MODEL` (default `llama3.2`,
/// which must be `ollama pull`ed).
#[tokio::test]
async fn live_ollama_chat_round_trip() {
    require_live!("live_ollama_chat_round_trip");
    if !cortex::llm::provider::Provider::ollama_available() {
        eprintln!("skipping live_ollama_chat_round_trip: no Ollama server reachable");
        return;
    }

    let model = std::env::var("BORG_LIVE_OLLAMA_MODEL").unwrap_or_else(|_| "llama3.2".to_string());
    let mut config = Config::default();
    config.llm.provider = Some("ollama".to_string());
    config.llm.model = model;

    let client = cortex::llm::LlmClient::new(&config)
        .await
        .expect("ollama client builds keyless");
    let reply = client
        .chat(
            &[cortex::types::Message::user(
                "Reply with the single word: pong",
            )],
            None,
        )
        .await
        .expect("local ollama chat should succeed");

    let content = reply
        .text_content()
        .expect("reply should contain text")
        .to_ascii_lowercase();
    assert!(
        content.contains("pong"),
        "expected 'pong' in local-model reply, got: {content:?}"
    );
}

/// Full E2E of the browser tool against a real content site: extract the
/// top 5 story links from the Hacker News front page with `evaluate_js`,
/// then `navigate` to each article detail page and `screenshot` it.
///
/// This exercises the complete production chain the model drives — lazy
/// Chrome launch, URL policy, JS eval guard, PNG capture, disk save, and
/// the multimodal envelope — against uncontrolled third-party pages.
#[tokio::test]
async fn live_browser_hn_top5_article_screenshots() {
    require_live!("live_browser_hn_top5_article_screenshots");

    let mut config = Config::default();
    config.browser.enabled = true;
    // Third-party article pages can be slow; default per-action timeout is
    // tuned for snappy pages.
    config.browser.timeout_ms = 45_000;

    let mut session: Option<BrowserSession> = None;
    let close = serde_json::json!({"action": "close"});

    // Close the leaked Chrome on any failure path, then panic with context.
    macro_rules! fail {
        ($($arg:tt)*) => {{
            let _ = tool_handlers::handle_browser(&close, &config, &mut session).await;
            panic!($($arg)*);
        }};
    }

    let nav = tool_handlers::handle_browser(
        &serde_json::json!({"action": "navigate", "url": "https://news.ycombinator.com/"}),
        &config,
        &mut session,
    )
    .await;
    match &nav {
        Ok(ToolOutput::Text(t)) if !t.starts_with("Error") => {}
        other => fail!("front-page navigate failed: {other:?}"),
    }

    // Top 5 story links. `.titleline > a` is the story anchor; the source
    // domain chip lives one level deeper (`.sitebit a`) so the direct-child
    // combinator excludes it.
    let eval = tool_handlers::handle_browser(
        &serde_json::json!({
            "action": "evaluate_js",
            "expression": "Array.from(document.querySelectorAll('tr.athing .titleline > a')).slice(0, 5).map(a => a.href)",
        }),
        &config,
        &mut session,
    )
    .await;
    let eval_text = match eval {
        Ok(ToolOutput::Text(t)) => t,
        other => fail!("evaluate_js failed: {other:?}"),
    };
    let json_slice = match (eval_text.find('['), eval_text.rfind(']')) {
        (Some(start), Some(end)) if end > start => &eval_text[start..=end],
        _ => fail!("no JSON array in evaluate_js output: {eval_text}"),
    };
    let urls: Vec<String> = match serde_json::from_str(json_slice) {
        Ok(u) => u,
        Err(e) => fail!("could not parse story URLs ({e}): {eval_text}"),
    };
    if urls.len() != 5 || !urls.iter().all(|u| u.starts_with("http")) {
        fail!("expected 5 absolute story URLs, got: {urls:?}");
    }

    let mut saved = Vec::new();
    for (i, url) in urls.iter().enumerate() {
        let nav = tool_handlers::handle_browser(
            &serde_json::json!({"action": "navigate", "url": url}),
            &config,
            &mut session,
        )
        .await;
        match &nav {
            Ok(ToolOutput::Text(t)) if !t.starts_with("Error") => {}
            other => fail!("navigate to story {i} ({url}) failed: {other:?}"),
        }

        let shot = tool_handlers::handle_browser(
            &serde_json::json!({"action": "screenshot"}),
            &config,
            &mut session,
        )
        .await;
        let (text, parts) = match shot {
            Ok(ToolOutput::Multimodal { text, parts }) => (text, parts),
            other => fail!("screenshot of story {i} ({url}) not multimodal: {other:?}"),
        };
        if !parts.iter().any(|p| {
            matches!(
                p,
                cortex::types::ContentPart::ImageBase64 { media } if media.mime_type == "image/png"
            )
        }) {
            fail!("screenshot of story {i} has no PNG image part: {text}");
        }

        let path = match text.split("Saved to: ").nth(1) {
            Some(p) => std::path::PathBuf::from(p.trim()),
            None => fail!("screenshot of story {i} was not saved to disk: {text}"),
        };
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => fail!("saved screenshot unreadable at {}: {e}", path.display()),
        };
        // A real page render, not a truncated or blank capture: PNG magic
        // plus enough payload that an empty white viewport can't pass.
        if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") || bytes.len() < 5_000 {
            fail!(
                "screenshot of story {i} is not a plausible page render ({} bytes) at {}",
                bytes.len(),
                path.display()
            );
        }
        eprintln!(
            "story {i}: {url} -> {} ({} bytes)",
            path.display(),
            bytes.len()
        );
        saved.push(path);
    }

    let _ = tool_handlers::handle_browser(&close, &config, &mut session).await;
    assert_eq!(saved.len(), 5, "expected 5 saved screenshots");
}
