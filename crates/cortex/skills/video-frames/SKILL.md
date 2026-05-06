---
name: video-frames
description: "Extract frames, thumbnails, contact sheets, GIFs, and short clips from videos using ffmpeg"
triggers:
  - "extract frames from this video"
  - "thumbnail this video"
  - "make a gif from"
  - "clip this video"
requires:
  bins: ["ffmpeg", "ffprobe"]
---

# Video Frames Skill

Pull stills and short clips out of video files with `ffmpeg` and `ffprobe`. Install on macOS with `brew install ffmpeg`; on Debian/Ubuntu with `apt install ffmpeg`.

Default output goes under `~/.borg/cache/video-frames/<basename>/` so artifacts stay discoverable and prunable. Create the directory before writing:

```bash
base=$(basename "$VIDEO"); base=${base%.*}
out=~/.borg/cache/video-frames/$base
mkdir -p "$out"
```

## Probe First

```bash
# Duration, resolution, codec, fps in one shot
ffprobe -v error -show_entries format=duration,bit_rate \
  -show_entries stream=codec_name,width,height,r_frame_rate \
  -of json input.mp4

# Just the duration in seconds (scriptable)
ffprobe -v error -show_entries format=duration -of csv=p=0 input.mp4
```

## Single Frame at a Timestamp

`-ss` before `-i` is fast (keyframe-accurate seek); after `-i` is slow but exact.

```bash
# Fast — keyframe-accurate (good enough for most thumbnails)
ffmpeg -ss 00:00:10 -i input.mp4 -frames:v 1 -q:v 2 frame-10s.jpg

# Exact — decodes from the start
ffmpeg -i input.mp4 -ss 00:00:10 -frames:v 1 -q:v 2 frame-exact.jpg

# Crisp PNG (no JPEG artifacts) for UI / OCR
ffmpeg -ss 00:00:10 -i input.mp4 -frames:v 1 frame.png
```

## N Evenly-Spaced Thumbnails

Compute the interval from duration:

```bash
N=8
dur=$(ffprobe -v error -show_entries format=duration -of csv=p=0 input.mp4)
fps=$(awk -v d="$dur" -v n="$N" 'BEGIN{printf "%.6f", n/d}')
ffmpeg -i input.mp4 -vf "fps=$fps,scale=320:-1" -q:v 3 thumb-%02d.jpg
```

## Every Nth Second

```bash
# One frame per second
ffmpeg -i input.mp4 -vf fps=1 -q:v 3 sec-%04d.jpg

# One frame every 5 seconds, scaled to 480 wide
ffmpeg -i input.mp4 -vf "fps=1/5,scale=480:-1" -q:v 3 frame-%04d.jpg
```

## Contact Sheet

```bash
# 4×3 grid of frames sampled across the video, 320px wide tiles
ffmpeg -i input.mp4 -vf "fps=1/30,scale=320:-1,tile=4x3" -frames:v 1 sheet.jpg
```

## Clip a Sub-range

Stream-copy is instant but cuts on keyframes only; re-encode for frame accuracy.

```bash
# Fast (keyframe-aligned) — no re-encode
ffmpeg -ss 00:01:00 -to 00:01:30 -i input.mp4 -c copy clip.mp4

# Exact — re-encodes
ffmpeg -ss 00:01:00 -to 00:01:30 -i input.mp4 -c:v libx264 -crf 20 -c:a aac clip.mp4
```

## Make a GIF

Two-pass with a generated palette gives noticeably better quality than the one-liner.

```bash
ffmpeg -ss 00:00:05 -t 4 -i input.mp4 \
  -vf "fps=15,scale=480:-1:flags=lanczos,split[s0][s1];[s0]palettegen[p];[s1][p]paletteuse" \
  out.gif
```

## Strip Audio / Normalize for Web

```bash
# Mute
ffmpeg -i input.mp4 -c:v copy -an muted.mp4

# Web-safe H.264 + AAC, faststart for streaming
ffmpeg -i input.mov -c:v libx264 -crf 23 -preset medium -pix_fmt yuv420p \
  -c:a aac -b:a 128k -movflags +faststart web.mp4
```

## Notes

- Quality scale `-q:v` is 2 (best) → 31 (worst) for MJPEG; 2–4 is the usual sweet spot.
- Always quote paths — spaces are common in video filenames.
- For the *very* first frame use `-ss 0` rather than no `-ss`; some containers won't yield a frame from `-frames:v 1` alone.
- Long videos: prefer pre-`-i` `-ss` so ffmpeg seeks instead of decoding 30 minutes to reach minute 28.
