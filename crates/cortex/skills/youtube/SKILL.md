---
name: youtube
description: "Summarize or repurpose a YouTube video from its captions: a summary, chapters with timestamps, key quotes, notes, a blog post, or a thread. Use whenever the user shares a YouTube link or asks what a video says."
category: utilities
triggers:
  - "summarize this youtube video"
  - "get the transcript of this video"
  - "what does this video say"
  - "make chapters for this video"
  - "turn this video into a blog post"
requires:
  bins: ["python3"]
install:
  python3:
    brew: python
    apt: python3-venv
    url: "https://www.python.org/downloads/"
---

# YouTube

Works from the video's captions (manual or auto-generated); it does not watch
the video. Set up `youtube-transcript-api` once in Borg's own Python
environment:

```bash
[ -x ~/.borg/venv/bin/python3 ] || python3 -m venv ~/.borg/venv
~/.borg/venv/bin/python3 -c "import youtube_transcript_api" 2>/dev/null || ~/.borg/venv/bin/python3 -m pip install -q youtube-transcript-api
```

## Fetch the transcript

Takes a watch, youtu.be, shorts, embed, or live link, or a bare 11-character
id, then preferred caption languages (default `en`). It falls back to any
language the video has.

```bash
mkdir -p ~/.borg/cache/youtube
~/.borg/venv/bin/python3 - "https://www.youtube.com/watch?v=VIDEO_ID" en > ~/.borg/cache/youtube/VIDEO_ID.txt <<'EOF'
import json, re, sys, urllib.parse, urllib.request
from youtube_transcript_api import NoTranscriptFound, YouTubeTranscriptApi
url, langs = sys.argv[1], sys.argv[2:] or ["en"]
m = re.search(r"(?:v=|youtu\.be/|shorts/|embed/|live/)([\w-]{11})", url)
vid = m.group(1) if m else url.strip()
watch = "https://www.youtube.com/watch?v=" + vid
try:
    meta = json.load(urllib.request.urlopen(
        "https://www.youtube.com/oembed?format=json&url=" + urllib.parse.quote(watch), timeout=15))
    print(f"# {meta['title']} | {meta['author_name']} | {watch}")
except Exception:
    print(f"# {watch}")
api = YouTubeTranscriptApi()
try:
    t = api.fetch(vid, languages=langs)
except NoTranscriptFound:
    t = next(iter(api.list(vid))).fetch()
    print(f"(no {'/'.join(langs)} captions; using {t.language_code})")
def ts(x):
    h, r = divmod(int(x), 3600)
    return f"{h}:{r // 60:02d}:{r % 60:02d}" if h else f"{r // 60}:{r % 60:02d}"
for s in t:
    print(ts(s.start), s.text.replace("\n", " "))
EOF
wc -c ~/.borg/cache/youtube/VIDEO_ID.txt
```

Then `read_file` the transcript. Long ones (more than about 40,000
characters): read in parts with `offset`/`limit`, summarize each part, then
merge the summaries.

## If it fails

- **Transcripts disabled / no captions**: say so. If `yt-dlp` and `ffmpeg` are
  installed, offer to download the audio and use `transcribe_audio`:
  `yt-dlp -x --audio-format mp3 -o "$HOME/.borg/cache/youtube/%(id)s.%(ext)s" URL`.
- **`RequestBlocked` / `IpBlocked`**: YouTube blocks many cloud and VPN
  addresses. Retry from another network, or get the captions with `yt-dlp`:
  `yt-dlp --skip-download --write-auto-subs --sub-langs en --sub-format vtt -o "$HOME/.borg/cache/youtube/%(id)s" URL`.
- **Private, members-only, or removed**: report it and ask for another link.

## Formats (default: summary)

- **Summary**: 5 to 10 sentences on the main argument and conclusions; third
  person, present tense.
- **Chapters**: `M:SS Title, one line on what happens` at each topic shift.
- **Chapter summaries**: each chapter heading plus a short paragraph.
- **Key quotes**: the speaker's exact words with timestamps. Never put a
  paraphrase inside quote marks.
- **Notes or action items**: bullets; for owners and deadlines use the
  `action-items` skill.
- **Blog post**: title, intro, sections, a few quotes, takeaways.
- **Thread**: numbered posts of at most 280 characters (post them with the
  `x-twitter` skill only if asked).

Link a moment as `https://youtu.be/VIDEO_ID?t=SECONDS`. Auto-captions garble
names and jargon; check them against the title and flag any you are unsure of.
