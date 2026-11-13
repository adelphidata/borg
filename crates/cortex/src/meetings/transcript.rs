//! Turning stored segments into something to read: the `transcript.md` file
//! and the plain text the summary prompt carries.
//!
//! Both tracks arrive as fixed-length chunks, so a "turn" here is a run of
//! consecutive chunks from one side. That is as fine-grained as the
//! transcribers' plain-text output allows.

use crate::db::SegmentRow;
use crate::meetings::Track;

/// Two chunks from the same side closer than this read as one paragraph.
const MERGE_GAP_MS: i64 = 1_500;

/// A run of speech from one side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paragraph {
    /// Who.
    pub track: Track,
    /// Milliseconds from the start.
    pub start_ms: i64,
    /// Milliseconds from the start.
    pub end_ms: i64,
    /// The words.
    pub text: String,
}

/// Segments in reading order: by start, the user before the other side on a
/// tie, then by chunk number.
pub fn order(segments: &mut [SegmentRow]) {
    segments.sort_by_key(|s| (s.start_ms, s.track, s.seq));
}

/// Merge consecutive same-side segments separated by less than
/// [`MERGE_GAP_MS`] into paragraphs.
pub fn paragraphs(segments: &[SegmentRow]) -> Vec<Paragraph> {
    let mut sorted = segments.to_vec();
    order(&mut sorted);
    let mut out: Vec<Paragraph> = Vec::new();
    for seg in sorted {
        let text = seg.text.trim();
        if text.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some(p) if p.track == seg.track && seg.start_ms - p.end_ms < MERGE_GAP_MS => {
                p.text.push(' ');
                p.text.push_str(text);
                p.end_ms = p.end_ms.max(seg.end_ms);
            }
            _ => out.push(Paragraph {
                track: seg.track,
                start_ms: seg.start_ms,
                end_ms: seg.end_ms,
                text: text.to_string(),
            }),
        }
    }
    out
}

/// `m:ss`, or `h:mm:ss` past the hour.
pub fn clock(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// The `transcript.md` file.
pub fn render_markdown(title: &str, started: &str, segments: &[SegmentRow]) -> String {
    let mut out = format!("# {title}\n\n_{started}_\n\n");
    let paras = paragraphs(segments);
    if paras.is_empty() {
        out.push_str("_Nothing transcribed yet._\n");
        return out;
    }
    for p in paras {
        out.push_str(&format!(
            "**[{}] {}:** {}\n\n",
            clock(p.start_ms),
            p.track.label(),
            p.text
        ));
    }
    out
}

/// Plain `[m:ss] Me: …` lines, at most `budget` characters. When the whole
/// meeting does not fit, the opening 40% and the closing 60% are kept, with
/// a line saying how much was left out: decisions and action items cluster
/// at the end of a call.
///
/// Trimming works chunk by chunk, not paragraph by paragraph: one person
/// talking for the whole call is a single paragraph, and keeping only whole
/// paragraphs would drop all of it.
pub fn render_for_llm(segments: &[SegmentRow], budget: usize) -> String {
    let line = |start_ms: i64, track: Track, text: &str| {
        format!("[{}] {}: {}", clock(start_ms), track.label(), text)
    };
    let whole: Vec<String> = paragraphs(segments)
        .iter()
        .map(|p| line(p.start_ms, p.track, &p.text))
        .collect();
    if whole.iter().map(|l| l.len() + 1).sum::<usize>() <= budget {
        return whole.join("\n");
    }

    let mut sorted = segments.to_vec();
    order(&mut sorted);
    let lines: Vec<(i64, String)> = sorted
        .iter()
        .filter(|s| !s.text.trim().is_empty())
        .map(|s| (s.start_ms, line(s.start_ms, s.track, s.text.trim())))
        .collect();
    let head_budget = budget * 2 / 5;
    let tail_budget = budget - head_budget;
    let head = take_within(lines.iter(), head_budget);
    let mut tail = take_within(lines.iter().skip(head.len()).rev(), tail_budget);
    tail.reverse();

    let gap_from = head
        .last()
        .map(|(s, _)| *s)
        .or_else(|| lines.first().map(|(s, _)| *s))
        .unwrap_or(0);
    let gap_to = tail
        .first()
        .map(|(s, _)| *s)
        .or_else(|| lines.last().map(|(s, _)| *s))
        .unwrap_or(gap_from);
    let minutes = ((gap_to - gap_from).max(0) + 30_000) / 60_000;
    let mut out: Vec<String> = head.into_iter().map(|(_, l)| l).collect();
    out.push(format!("[… about {minutes} minutes omitted …]"));
    out.extend(tail.into_iter().map(|(_, l)| l));
    out.join("\n")
}

/// Lines from `lines` while they fit in `budget`. A first line that alone is
/// too long is cut to fit rather than lost.
fn take_within<'a>(
    lines: impl Iterator<Item = &'a (i64, String)>,
    budget: usize,
) -> Vec<(i64, String)> {
    let mut out = Vec::new();
    let mut used = 0;
    for (start, l) in lines {
        if used + l.len() < budget {
            used += l.len() + 1;
            out.push((*start, l.clone()));
            continue;
        }
        if out.is_empty() && budget > 1 {
            let mut cut = budget - 1;
            while !l.is_char_boundary(cut) {
                cut -= 1;
            }
            out.push((*start, l[..cut].to_string()));
        }
        break;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(track: Track, seq: u32, start_ms: i64, end_ms: i64, text: &str) -> SegmentRow {
        SegmentRow {
            track,
            seq,
            start_ms,
            end_ms,
            text: text.into(),
        }
    }

    #[test]
    fn same_start_reads_me_before_them_whatever_the_arrival_order() {
        let segs = [
            seg(Track::System, 0, 0, 25_000, "hello from them"),
            seg(Track::Mic, 0, 0, 25_000, "hello from me"),
        ];
        let md = render_markdown("Standup", "Thu 12 Nov", &segs);
        let me = md.find("Me:").unwrap();
        let them = md.find("Them:").unwrap();
        assert!(me < them, "{md}");
    }

    #[test]
    fn paragraphs_merge_only_close_same_side_chunks() {
        // (second chunk start, expected paragraph count)
        let cases = [(25_000, 1), (26_000, 1), (27_000, 2)];
        for (start, want) in cases {
            let segs = [
                seg(Track::Mic, 0, 0, 25_000, "one"),
                seg(Track::Mic, 1, start, start + 25_000, "two"),
            ];
            assert_eq!(paragraphs(&segs).len(), want, "second chunk at {start}");
        }
        // A chunk from the other side always starts a new paragraph.
        let segs = [
            seg(Track::Mic, 0, 0, 25_000, "one"),
            seg(Track::System, 0, 25_000, 50_000, "two"),
            seg(Track::Mic, 1, 50_000, 75_000, "three"),
        ];
        let p = paragraphs(&segs);
        assert_eq!(p.len(), 3);
        assert_eq!(p[0].text, "one");
    }

    #[test]
    fn blank_chunks_are_skipped() {
        let segs = [seg(Track::Mic, 0, 0, 1_000, "   ")];
        assert!(paragraphs(&segs).is_empty());
        assert!(render_markdown("t", "d", &segs).contains("Nothing transcribed yet"));
    }

    #[test]
    fn clock_reads_minutes_then_hours() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(65_000), "1:05");
        assert_eq!(clock(3_725_000), "1:02:05");
        assert_eq!(clock(-5), "0:00");
    }

    #[test]
    fn llm_text_fits_the_budget_and_keeps_both_ends() {
        // 60 alternating paragraphs, ~70 chars each.
        let segs: Vec<_> = (0..60)
            .map(|i| {
                let track = if i % 2 == 0 {
                    Track::Mic
                } else {
                    Track::System
                };
                seg(
                    track,
                    i,
                    i64::from(i) * 60_000,
                    i64::from(i) * 60_000 + 25_000,
                    &format!("paragraph number {i:02} with some filler words in it"),
                )
            })
            .collect();
        let full = render_for_llm(&segs, usize::MAX);
        assert!(!full.contains("omitted"));
        assert_eq!(full.lines().count(), 60);

        let cut = render_for_llm(&segs, 1_500);
        assert!(cut.len() <= 1_500 + 40, "{}", cut.len());
        assert!(cut.contains("number 00"), "opening kept");
        assert!(cut.contains("number 59"), "closing kept");
        assert!(cut.contains("minutes omitted"));
        assert!(!cut.contains("number 30"), "middle dropped");
    }

    #[test]
    fn one_voice_for_the_whole_call_is_trimmed_not_dropped() {
        // Mic only (or everyone else muted): every chunk merges into one
        // paragraph far larger than either half of the budget.
        let segs: Vec<_> = (0..120u32)
            .map(|i| {
                seg(
                    Track::Mic,
                    i,
                    i64::from(i) * 25_000,
                    i64::from(i) * 25_000 + 25_000,
                    &format!("chunk {i:03} where I keep talking about the rollout plan"),
                )
            })
            .collect();
        assert_eq!(paragraphs(&segs).len(), 1);
        let cut = render_for_llm(&segs, 2_000);
        assert!(cut.len() <= 2_000 + 40, "{}", cut.len());
        assert!(cut.contains("chunk 000"), "opening kept: {cut}");
        assert!(cut.contains("chunk 119"), "closing kept: {cut}");
        assert!(cut.contains("minutes omitted"));
    }

    #[test]
    fn a_single_chunk_longer_than_the_budget_is_cut_on_a_character_boundary() {
        let long = "é".repeat(400);
        let segs = [seg(Track::System, 0, 0, 25_000, &long)];
        let cut = render_for_llm(&segs, 100);
        assert!(cut.starts_with("[0:00] Them: é"), "{cut}");
        assert!(cut.len() <= 140, "{}", cut.len());
    }
}
