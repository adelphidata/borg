//! Kept dictation recordings: a copy of each recording the composer sent,
//! with its transcript beside it, pruned after the configured number of days.
//!
//! Off by default. Nothing here runs unless `audio.keep_recordings` is on.

use chrono::{DateTime, Utc};
use cortex::config::Config;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Where recordings go: the configured folder, else `recordings/` under
/// Borg's data directory.
pub fn recordings_dir(config: &Config) -> anyhow::Result<PathBuf> {
    match config.audio.recordings_dir.as_deref().map(str::trim) {
        // Settings suggests `~/.borg/recordings`; a shell would expand that.
        Some(dir) if !dir.is_empty() => {
            Ok(PathBuf::from(cortex::util::unix_ext::expand_tilde(dir)))
        }
        _ => Ok(Config::data_dir()?.join("recordings")),
    }
}

/// The file stem this module writes: `YYYY-MM-DD_HH-MM-SS.mmm`.
const STAMP_FORMAT: &str = "%Y-%m-%d_%H-%M-%S%.3f";

/// Whether `name` is a file this module wrote. The folder may be one the
/// user already keeps other things in; only our own stamps are ever pruned.
fn is_kept_recording(name: &str) -> bool {
    let Some(stem) = Path::new(name).file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    const SHAPE: &[u8] = b"0000-00-00_00-00-00.000";
    stem.len() == SHAPE.len()
        && stem.bytes().zip(SHAPE).all(|(c, want)| {
            if *want == b'0' {
                c.is_ascii_digit()
            } else {
                c == *want
            }
        })
}

/// Writes `<stamp>.<ext>` and `<stamp>.txt` into `dir`, then prunes what is
/// older than `keep_days` (0 keeps everything). Returns the audio path.
pub fn keep(
    dir: &Path,
    now: DateTime<Utc>,
    audio: &[u8],
    file_name: &str,
    transcript: &str,
    keep_days: u32,
) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let ext = Path::new(file_name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("webm");
    let stamp = now.format(STAMP_FORMAT).to_string();
    let audio_path = dir.join(format!("{stamp}.{ext}"));
    std::fs::write(&audio_path, audio)?;
    std::fs::write(dir.join(format!("{stamp}.txt")), transcript)?;
    prune(dir, keep_days)?;
    Ok(audio_path)
}

/// Removes recordings in `dir` whose modification time is older than
/// `keep_days`. Only files named by [`keep`] go: the folder may be one the
/// user pointed at, and nothing else in it is ours to delete. Subfolders
/// and files that cannot be read are left alone.
pub fn prune(dir: &Path, keep_days: u32) -> anyhow::Result<()> {
    if keep_days == 0 {
        return Ok(());
    }
    let cutoff = SystemTime::now() - Duration::from_secs(u64::from(keep_days) * 86_400);
    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        if !entry.file_name().to_str().is_some_and(is_kept_recording) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        if meta.modified().map(|m| m < cutoff).unwrap_or(false) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::FileTimes;

    #[test]
    fn keep_writes_the_audio_and_its_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nested").join("recordings");
        let now = DateTime::parse_from_rfc3339("2026-10-23T12:00:00.250Z")
            .unwrap()
            .with_timezone(&Utc);
        let path = keep(&dir, now, b"RIFF", "recording.wav", "hello there", 30).unwrap();
        assert_eq!(path, dir.join("2026-10-23_12-00-00.250.wav"));
        assert_eq!(std::fs::read(&path).unwrap(), b"RIFF");
        assert_eq!(
            std::fs::read_to_string(dir.join("2026-10-23_12-00-00.250.txt")).unwrap(),
            "hello there"
        );
    }

    #[test]
    fn prune_drops_only_files_older_than_the_window() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("2026-09-01_08-00-00.000.webm");
        let fresh = tmp.path().join("2026-10-23_08-00-00.000.webm");
        // Something the user keeps in the same folder, older than the window.
        let theirs = tmp.path().join("notes.webm");
        std::fs::write(&old, b"x").unwrap();
        std::fs::write(&fresh, b"x").unwrap();
        std::fs::write(&theirs, b"x").unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        let forty_days_ago = SystemTime::now() - Duration::from_secs(40 * 86_400);
        for path in [&old, &theirs] {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(FileTimes::new().set_modified(forty_days_ago))
                .unwrap();
        }
        prune(tmp.path(), 30).unwrap();
        assert!(!old.exists());
        assert!(fresh.exists());
        assert!(theirs.exists(), "only our own recordings are pruned");
        assert!(tmp.path().join("sub").is_dir());
        // 0 keeps everything, however old.
        std::fs::write(&old, b"x").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_times(FileTimes::new().set_modified(forty_days_ago))
            .unwrap();
        prune(tmp.path(), 0).unwrap();
        assert!(old.exists());
    }

    #[test]
    fn recordings_dir_prefers_the_configured_folder() {
        let rows = [(
            "audio.recordings_dir".to_string(),
            " /tmp/my-notes ".to_string(),
        )];
        let config = Config::from_setting_rows(rows);
        assert_eq!(
            recordings_dir(&config).unwrap(),
            PathBuf::from("/tmp/my-notes")
        );
        let blank =
            Config::from_setting_rows([("audio.recordings_dir".to_string(), String::new())]);
        assert!(recordings_dir(&blank).unwrap().ends_with("recordings"));
        let tilde = Config::from_setting_rows([(
            "audio.recordings_dir".to_string(),
            "~/notes".to_string(),
        )]);
        let expanded = recordings_dir(&tilde).unwrap();
        assert!(!expanded.starts_with("~"), "{}", expanded.display());
        assert!(expanded.ends_with("notes"));
    }

    #[test]
    fn only_our_own_stamps_count_as_recordings() {
        assert!(is_kept_recording("2026-10-23_12-00-00.250.wav"));
        assert!(is_kept_recording("2026-10-23_12-00-00.250.txt"));
        assert!(!is_kept_recording("notes.webm"));
        assert!(!is_kept_recording("2026-10-23.wav"));
        assert!(!is_kept_recording("2026-10-23_12-00-00.25x.wav"));
    }
}
