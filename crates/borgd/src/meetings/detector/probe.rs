//! Which apps are using the microphone, and which of those are meeting apps.
//!
//! The probe reports bundle ids; [`app_label`] keeps only apps people hold
//! calls in. Anything else using the mic (Borg's own dictation, a voice
//! memo, a DAW) is not a meeting and is dropped here.

/// Reads which processes are capturing audio right now.
pub trait MicProbe: Send + Sync {
    /// Whether this machine can answer at all (macOS 14+ for CoreAudio's
    /// process objects).
    fn supported(&self) -> bool;
    /// Bundle ids of processes currently taking microphone input.
    fn snapshot(&self) -> anyhow::Result<Vec<String>>;
}

/// Meeting apps by bundle-id prefix, most surely a meeting first. Prefixes,
/// because browsers capture from helper processes
/// (`com.google.Chrome.helper`).
const APPS: &[(&str, &str)] = &[
    ("us.zoom.xos", "Zoom"),
    ("com.microsoft.teams2", "Teams"),
    ("com.microsoft.teams", "Teams"),
    ("com.cisco.webexmeetingsapp", "Webex"),
    ("Cisco-Systems.Spark", "Webex"),
    ("com.tinyspeck.slackmacgap", "Slack"),
    ("com.apple.FaceTime", "FaceTime"),
    ("com.hnc.Discord", "Discord"),
    ("com.google.Chrome", "Chrome"),
    ("company.thebrowser.Browser", "Arc"),
    ("com.brave.Browser", "Brave"),
    ("com.microsoft.edgemac", "Edge"),
    ("com.apple.Safari", "Safari"),
    ("com.apple.WebKit", "Safari"),
    ("org.mozilla.firefox", "Firefox"),
];

/// Labels shown to the person, for every bundle that counts.
pub fn app_label(bundle_id: &str) -> Option<&'static str> {
    APPS.iter()
        .find(|(prefix, _)| {
            bundle_id == *prefix
                || bundle_id
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with('.'))
        })
        .map(|(_, label)| *label)
}

/// Lower is more surely a meeting.
pub fn rank(label: &str) -> usize {
    APPS.iter()
        .position(|(_, l)| *l == label)
        .unwrap_or(usize::MAX)
}

/// The labels of `bundles`, deduplicated, order kept.
pub fn labels(bundles: &[String]) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for b in bundles {
        if let Some(l) = app_label(b) {
            if !out.contains(&l) {
                out.push(l);
            }
        }
    }
    out
}

/// A probe for platforms without detection.
pub struct Unsupported;

impl MicProbe for Unsupported {
    fn supported(&self) -> bool {
        false
    }
    fn snapshot(&self) -> anyhow::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meeting_apps_are_named_and_everything_else_is_ignored() {
        let cases = [
            ("us.zoom.xos", Some("Zoom")),
            ("com.microsoft.teams2", Some("Teams")),
            ("com.microsoft.teams", Some("Teams")),
            ("com.google.Chrome", Some("Chrome")),
            ("com.google.Chrome.helper", Some("Chrome")),
            ("com.google.Chrome.helper.Renderer", Some("Chrome")),
            ("company.thebrowser.Browser", Some("Arc")),
            ("com.apple.WebKit.GPU", Some("Safari")),
            // Borg itself, and lookalike prefixes.
            ("dev.borg.desktop", None),
            ("dev.borg.desktop.helper", None),
            ("com.google.Chromecast", None),
            ("com.apple.VoiceMemos", None),
            ("", None),
        ];
        for (bundle, want) in cases {
            assert_eq!(app_label(bundle), want, "{bundle}");
        }
    }

    #[test]
    fn labels_dedupe_helpers_of_the_same_app() {
        let bundles = [
            "com.google.Chrome.helper".to_string(),
            "com.google.Chrome.helper.Renderer".to_string(),
            "dev.borg.desktop".to_string(),
            "us.zoom.xos".to_string(),
        ];
        assert_eq!(labels(&bundles), ["Chrome", "Zoom"]);
        assert!(rank("Zoom") < rank("Chrome"));
        assert_eq!(rank("Nope"), usize::MAX);
    }
}
