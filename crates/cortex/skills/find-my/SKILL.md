---
name: find-my
description: "Locate the user's Apple devices, AirTags, and shared friends with the Find My app on this Mac, by reading its window on screen. Use for 'where is my iPhone / keys / bag'. macOS only; Apple offers no Find My API, so this drives the app's screen."
category: utilities
triggers:
  - "where is my iphone"
  - "find my airtag"
  - "where are my keys"
  - "locate my airpods"
  - "check find my"
os: [macos]
---

# Find My

Apple has no Find My API or CLI. Open the app and read its window. Only look
up devices, items, and people the user owns or who share with them.

## With `computer_use` (preferred)

1. Open the app: `run_shell` with `open -a FindMy`, then `computer_use`
   `{"action": "wait", "duration_ms": 3000}` while it loads the map.
2. `{"action": "capture", "app": "Find My", "mode": "som"}`. If no window
   resolves, call `list_windows` and capture the Find My `window_id`.
3. Pick the tab: click **People**, **Devices**, or **Items** by its
   `element_index`, with `"capture_after": true`.
4. Click the device or item in the list to open its card (address, "last
   seen", battery), then capture again with `mode: "vision"`.
5. Report: name, address or place, how old the location is ("2 min ago"),
   and battery if shown. Say when a location is old or the item is offline.

AirTags and other items update only while their card is open and an Apple
device is nearby, so wait 20 to 60 seconds and capture again before calling
a location stale.

## Without `computer_use`

If the tool is not in your list (it loads for requests that mention the
screen, a window, or clicking, and needs `brew install cua-driver`), fall back
to a full-screen capture. It needs Screen Recording permission for Borg and
shows whatever else is on screen.

```bash
open -a FindMy && sleep 4
mkdir -p ~/.borg/cache/find-my
screencapture -x ~/.borg/cache/find-my/now.png
```

Read the image with `read_file`, then delete it. This path cannot click, so
only the list Find My already shows is readable.

## Tracking over time

For "where has my bag been today", create a `schedule` job (for example every
15 minutes for a few hours) that captures the item's card and appends one
line to `~/.borg/find-my/<item>.csv`: time, address, and age of fix. The Mac
must stay awake and logged in. Cancel the job when the user is done.

## Limits

- Find My must be signed in to the user's Apple Account on this Mac.
- Location accuracy depends on nearby Apple devices.
- Marking a device lost or erasing it changes the user's devices: never do
  it without an explicit request and a second confirmation.
