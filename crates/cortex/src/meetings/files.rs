//! A meeting's folder: `<YYYY-MM-DD_HH-MM>_<slug>/` holding `transcript.md`,
//! `notes.md`, `summary.md` and, only when `meetings.keep_audio` is on, one
//! WAV per track. Every file is owner-only and never written through a link.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone};

use crate::config::Config;
use crate::meetings::Track;

/// The transcript, rewritten after every chunk.
pub const TRANSCRIPT_FILE: &str = "transcript.md";
/// What the user typed.
pub const NOTES_FILE: &str = "notes.md";
/// The written summary.
pub const SUMMARY_FILE: &str = "summary.md";

/// Where meeting folders go: `meetings.dir`, else `recordings/meetings/`
/// under Borg's data directory.
pub fn meetings_dir(config: &Config) -> Result<PathBuf> {
    match config.meetings.dir.as_deref().map(str::trim) {
        Some(dir) if !dir.is_empty() => Ok(PathBuf::from(crate::util::unix_ext::expand_tilde(dir))),
        _ => Ok(Config::data_dir()?.join("recordings").join("meetings")),
    }
}

/// A file-name-safe form of a title: lowercase ASCII words joined by `-`,
/// at most 40 characters, `meeting` when nothing is left.
pub fn slug(title: &str) -> String {
    let mut out = String::new();
    for word in title
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        let word = word.to_ascii_lowercase();
        let extra = usize::from(!out.is_empty()) + word.len();
        if out.len() + extra > 40 {
            if out.is_empty() {
                out.push_str(&word[..40]);
            }
            break;
        }
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(&word);
    }
    if out.is_empty() {
        "meeting".to_string()
    } else {
        out
    }
}

/// `YYYY-MM-DD_HH-MM_<slug>`, in the zone of `started`.
pub fn folder_name<Tz: TimeZone>(started: &DateTime<Tz>, title: &str) -> String
where
    Tz::Offset: std::fmt::Display,
{
    format!("{}_{}", started.format("%Y-%m-%d_%H-%M"), slug(title))
}

/// Whether `name` has the shape [`folder_name`] produces (with an optional
/// `-N` collision suffix on the slug).
pub fn is_meeting_folder_name(name: &str) -> bool {
    const SHAPE: &[u8] = b"0000-00-00_00-00_";
    name.len() > SHAPE.len()
        && name.bytes().zip(SHAPE).all(|(c, want)| {
            if *want == b'0' {
                c.is_ascii_digit()
            } else {
                c == *want
            }
        })
        && name[SHAPE.len()..]
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

/// Create `<root>/<name>` (owner-only), or `<name>-2`, `-3`… when taken.
pub fn create_meeting_dir(root: &Path, name: &str) -> Result<PathBuf> {
    // Folder names carry meeting titles, so a root Borg creates is
    // owner-only. A folder the person already had keeps its permissions.
    let created = !root.exists();
    std::fs::create_dir_all(root)
        .with_context(|| format!("could not create the meetings folder {}", root.display()))?;
    #[cfg(unix)]
    if created {
        crate::util::unix_ext::set_unix_mode(root, 0o700)?;
    }
    for n in 1..1000 {
        let candidate = if n == 1 {
            root.join(name)
        } else {
            root.join(format!("{name}-{n}"))
        };
        match std::fs::create_dir(&candidate) {
            Ok(()) => {
                #[cfg(unix)]
                crate::util::unix_ext::set_unix_mode(&candidate, 0o700)?;
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(e).with_context(|| format!("could not create {}", candidate.display()))
            }
        }
    }
    anyhow::bail!("too many meetings named {name} in {}", root.display())
}

/// Replace `dir/file` with `contents`, owner-only, refusing a link.
pub fn write_file(dir: &Path, file: &str, contents: &str) -> Result<()> {
    let path = dir.join(file);
    crate::util::unix_ext::replace_private(&path, contents.as_bytes())
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(())
}

/// The kept audio file for a track.
pub fn audio_file(track: Track) -> String {
    format!("audio-{}.wav", track.as_str())
}

/// Append samples to a track's kept WAV, creating it (owner-only) on the
/// first chunk. Gaps between chunks are not padded: the file is a record of
/// what was heard, not a timeline.
pub fn append_track_audio(
    dir: &Path,
    track: Track,
    spec: hound::WavSpec,
    samples: &[i16],
) -> Result<()> {
    let path = dir.join(audio_file(track));
    if path.exists() {
        let mut w = hound::WavWriter::append(&path)
            .with_context(|| format!("could not reopen {}", path.display()))?;
        anyhow::ensure!(
            w.spec() == spec,
            "this chunk's format differs from the earlier audio in {}",
            path.display()
        );
        for s in samples {
            w.write_sample(*s)?;
        }
        w.finalize()?;
    } else {
        let file = crate::util::unix_ext::create_private_new(&path)
            .with_context(|| format!("could not create {}", path.display()))?;
        let mut w = hound::WavWriter::new(std::io::BufWriter::new(file), spec)?;
        for s in samples {
            w.write_sample(*s)?;
        }
        w.finalize()?;
    }
    Ok(())
}

/// Remove what this module wrote in a meeting folder, then the folder if it
/// is empty. Anything else the user put there stays, and a folder whose name
/// is not one of ours is refused outright.
pub fn remove_meeting_dir(dir: &Path) -> Result<()> {
    let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    anyhow::ensure!(
        is_meeting_folder_name(name),
        "refusing to remove {}: not a meeting folder",
        dir.display()
    );
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_symlink() => {
            anyhow::bail!("refusing to remove {}: it is a link", dir.display())
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    }
    let ours = [
        TRANSCRIPT_FILE.to_string(),
        NOTES_FILE.to_string(),
        SUMMARY_FILE.to_string(),
        audio_file(Track::Mic),
        audio_file(Track::System),
    ];
    for file in &ours {
        match std::fs::remove_file(dir.join(file)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    if let Err(e) = std::fs::remove_dir(dir) {
        tracing::warn!(
            dir = %dir.display(),
            "meeting folder kept because it holds other files: {e}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_is_safe_short_and_never_empty() {
        let cases = [
            ("Weekly Sync / Q3?", "weekly-sync-q3"),
            (
                "ISSM Project - Harley Coplan - ISSM",
                "issm-project-harley-coplan-issm",
            ),
            ("", "meeting"),
            ("¿¿??", "meeting"),
            (
                "one two three four five six seven eight nine ten",
                "one-two-three-four-five-six-seven-eight",
            ),
        ];
        for (title, want) in cases {
            assert_eq!(slug(title), want, "{title:?}");
        }
        assert_eq!(slug(&"x".repeat(80)).len(), 40);
    }

    #[test]
    fn folder_names_are_stamped_and_recognised() {
        let at = chrono::Utc
            .with_ymd_and_hms(2026, 11, 12, 16, 5, 0)
            .unwrap();
        let name = folder_name(&at, "Interview with Adelphi");
        assert_eq!(name, "2026-11-12_16-05_interview-with-adelphi");
        assert!(is_meeting_folder_name(&name));
        assert!(is_meeting_folder_name(&format!("{name}-2")));
        for foreign in [
            "Documents",
            "2026-11-12_16-05_",
            "2026-11-12_16-05_Has Space",
            "../x",
        ] {
            assert!(!is_meeting_folder_name(foreign), "{foreign}");
        }
    }

    #[test]
    fn meetings_dir_expands_the_setting_and_ignores_blank() {
        let mut config = Config::default();
        config.meetings.dir = Some("/srv/notes".into());
        assert_eq!(meetings_dir(&config).unwrap(), PathBuf::from("/srv/notes"));
        config.meetings.dir = Some("~/Documents/Meetings".into());
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            meetings_dir(&config).unwrap(),
            home.join("Documents/Meetings")
        );
        config.meetings.dir = Some("  ".into());
        assert!(meetings_dir(&config)
            .unwrap()
            .ends_with("recordings/meetings"));
    }

    #[cfg(unix)]
    #[test]
    fn a_meetings_root_borg_creates_is_owner_only_and_an_existing_one_is_left_alone() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let tmp = tempfile::tempdir().unwrap();
        let fresh = tmp.path().join("Documents/Meetings");
        create_meeting_dir(&fresh, "2026-11-12_16-05_standup").unwrap();
        assert_eq!(mode(&fresh), 0o700, "titles in folder names stay private");

        let theirs = tmp.path().join("shared");
        std::fs::create_dir(&theirs).unwrap();
        std::fs::set_permissions(&theirs, std::fs::Permissions::from_mode(0o755)).unwrap();
        create_meeting_dir(&theirs, "2026-11-12_16-05_standup").unwrap();
        assert_eq!(mode(&theirs), 0o755, "a folder the person chose is theirs");
    }

    #[cfg(unix)]
    #[test]
    fn folders_and_files_are_owner_only_and_collisions_get_a_suffix() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let a = create_meeting_dir(tmp.path(), "2026-11-12_16-05_standup").unwrap();
        let b = create_meeting_dir(tmp.path(), "2026-11-12_16-05_standup").unwrap();
        assert!(b.ends_with("2026-11-12_16-05_standup-2"));
        assert_eq!(
            std::fs::metadata(&a).unwrap().permissions().mode() & 0o777,
            0o700
        );
        write_file(&a, NOTES_FILE, "first").unwrap();
        write_file(&a, NOTES_FILE, "second").unwrap();
        assert_eq!(
            std::fs::read_to_string(a.join(NOTES_FILE)).unwrap(),
            "second"
        );
        assert_eq!(
            std::fs::metadata(a.join(NOTES_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_link_is_never_written_through() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = create_meeting_dir(tmp.path(), "2026-11-12_16-05_x").unwrap();
        let target = tmp.path().join("elsewhere.txt");
        std::fs::write(&target, "untouched").unwrap();
        std::os::unix::fs::symlink(&target, dir.join(SUMMARY_FILE)).unwrap();
        assert!(write_file(&dir, SUMMARY_FILE, "pwned").is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched");
    }

    #[test]
    fn kept_audio_grows_chunk_by_chunk() {
        let tmp = tempfile::tempdir().unwrap();
        let wav = crate::meetings::audio::test_wav::wav_ms(500);
        let (spec, samples) = crate::meetings::audio::pcm_i16(&wav).unwrap();
        append_track_audio(tmp.path(), Track::Mic, spec, &samples).unwrap();
        append_track_audio(tmp.path(), Track::Mic, spec, &samples).unwrap();
        let reader = hound::WavReader::open(tmp.path().join("audio-mic.wav")).unwrap();
        assert_eq!(reader.duration() as usize, samples.len() * 2);
    }

    #[test]
    fn removal_keeps_foreign_files_and_refuses_foreign_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = create_meeting_dir(tmp.path(), "2026-11-12_16-05_x").unwrap();
        write_file(&dir, TRANSCRIPT_FILE, "t").unwrap();
        std::fs::write(dir.join("mine.txt"), "user file").unwrap();
        remove_meeting_dir(&dir).unwrap();
        assert!(!dir.join(TRANSCRIPT_FILE).exists());
        assert!(dir.join("mine.txt").exists(), "the user's file survives");

        let clean = create_meeting_dir(tmp.path(), "2026-11-12_16-06_y").unwrap();
        write_file(&clean, NOTES_FILE, "n").unwrap();
        remove_meeting_dir(&clean).unwrap();
        assert!(!clean.exists());

        assert!(
            remove_meeting_dir(tmp.path()).is_err(),
            "not a meeting folder"
        );
    }
}
