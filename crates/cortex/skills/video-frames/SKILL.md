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
install:
  ffmpeg:
    brew: ffmpeg
    apt: ffmpeg
  ffprobe:
    brew: ffmpeg
    apt: ffmpeg
---

# Video Frames

Default artifacts go to `~/.borg/cache/video-frames/<basename>/`:

```bash
base=$(basename "$VIDEO"); base=${base%.*}
out=~/.borg/cache/video-frames/$base; mkdir -p "$out"
```

`-ss` **before** `-i` = fast keyframe seek; after `-i` = exact but decodes
from start. MJPEG `-q:v`: 2 (best) – 31; 2–4 is the sweet spot. Quote paths.

## Probe

```bash
ffprobe -v error -show_entries format=duration,bit_rate \
  -show_entries stream=codec_name,width,height,r_frame_rate -of json input.mp4
ffprobe -v error -show_entries format=duration -of csv=p=0 input.mp4   # scriptable
```

## Single frame

```bash
ffmpeg -ss 00:00:10 -i input.mp4 -frames:v 1 -q:v 2 frame.jpg          # fast
ffmpeg -i input.mp4 -ss 00:00:10 -frames:v 1 frame.png                  # exact, lossless
```

## N evenly-spaced thumbs

```bash
N=8
dur=$(ffprobe -v error -show_entries format=duration -of csv=p=0 input.mp4)
fps=$(awk -v d="$dur" -v n="$N" 'BEGIN{printf "%.6f", n/d}')
ffmpeg -i input.mp4 -vf "fps=$fps,scale=320:-1" -q:v 3 thumb-%02d.jpg
```

## Periodic frames / contact sheet

```bash
ffmpeg -i input.mp4 -vf "fps=1/5,scale=480:-1" -q:v 3 frame-%04d.jpg     # every 5s
ffmpeg -i input.mp4 -vf "fps=1/30,scale=320:-1,tile=4x3" -frames:v 1 sheet.jpg
```

## Clip

```bash
ffmpeg -ss 00:01:00 -to 00:01:30 -i input.mp4 -c copy clip.mp4           # keyframe-aligned
ffmpeg -ss 00:01:00 -to 00:01:30 -i input.mp4 -c:v libx264 -crf 20 -c:a aac clip.mp4  # exact
```

## GIF (two-pass palette = better quality)

```bash
ffmpeg -ss 00:00:05 -t 4 -i input.mp4 \
  -vf "fps=15,scale=480:-1:flags=lanczos,split[s0][s1];[s0]palettegen[p];[s1][p]paletteuse" \
  out.gif
```

## Mute / web-safe

```bash
ffmpeg -i input.mp4 -c:v copy -an muted.mp4
ffmpeg -i input.mov -c:v libx264 -crf 23 -preset medium -pix_fmt yuv420p \
  -c:a aac -b:a 128k -movflags +faststart web.mp4
```
