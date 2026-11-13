# Meetings

Borg takes notes in the calls you are already in: Google Meet, Zoom, Teams, or
anything else on your Mac. Nothing joins the call. The desktop app records your
microphone ("Me") and what your computer plays ("Them"). It sends short chunks
to `borgd`, which transcribes them as you talk. When the call ends, the model
writes a summary that builds on the notes you typed during it.

Meetings are off until you turn them on in **Settings → Meetings**, which asks
for one thing: a transcription provider.

## What happens during a call

1. **Detection (macOS 14 and later).** `borgd` checks every two seconds which
   apps hold the microphone. When Zoom, Teams, a browser, Webex, Slack or
   FaceTime has held it for five seconds, the desktop app shows **Meeting
   detected** in the top-right corner. Nothing records yet.
2. **Take notes.** Clicking **Take notes** starts the meeting and opens a small
   floating notes panel. The panel captures both tracks in 25-second chunks and
   shows a level meter. Type in it whenever you like. Closing the panel hides
   it, and recording continues.
3. **Stop.** Click **Stop**, or leave the call. Thirty seconds after the
   microphone goes quiet the panel stops by itself, unless the call comes back.
4. **Summary.** The meeting shows "Writing notes…" until the summary lands. A
   green dot then appears next to **Meetings** in the sidebar until you open
   it.

With **Ask before recording** turned off, a call that lines up with a
calendar event starts recording on its own, with no pop-up. A call with no
matching event always asks first.

A recording that nothing writes to for five minutes is stopped and summarised.
This covers a desktop app that crashed mid-call.

## Where the files go

Each meeting gets its own folder, readable only by you:

```
~/.borg/recordings/meetings/2026-11-12_09-45_tiger-sync/
  transcript.md   the transcript, Me / Them, with timestamps
  notes.md        what you typed
  summary.md      the summary, once written
  audio-mic.wav   only with "Keep audio files" on
  audio-system.wav
```

**Meetings folder** in Settings moves this anywhere, for example
`~/Documents/Meetings`. `borg backup` includes the default folder because it
lives inside `~/.borg`. A folder elsewhere is not backed up by Borg.

Deleting a meeting removes those files and then the folder if it is empty.
Anything else you put in the folder stays.

## Transcription

Meetings use the same provider list as dictation (`audio.models`): OpenAI,
Groq, Deepgram, ElevenLabs, Mistral, xAI, a local whisper.cpp, or your own
command. Dictation itself (`audio.enabled`) does not need to be on. Ollama
has no speech-to-text API, so for fully local transcription use whisper.cpp or
a custom command.

The other people on the call are recorded as one "Them" track. The summary
tells them apart only where the words make it clear.

## Calendar

A connected calendar fills the **Coming up** list on the Meetings page and lets
Borg name a meeting after the event it lines up with, with its attendees. Pick
one source under **Connections → Calendar**. Connecting one replaces the other.

### Google Calendar

Borg ships no Google client, so you use your own. It is free and takes a few
minutes:

1. In the [Google Cloud console](https://console.cloud.google.com/), create a
   project and enable the **Google Calendar API**.
2. Under **APIs & Services → OAuth consent screen**, choose *External* and add
   yourself as a test user.
3. Under **Credentials**, create an **OAuth client ID** of type **Desktop
   app**.
4. Paste its client ID and secret into **Connections → Calendar** and click
   **Connect Google Calendar**. Approve read-only calendar access in the
   browser tab that opens.

Borg asks only for `calendar.readonly` and stores only the refresh token. The
sign-in comes back to a one-time listener on `127.0.0.1`.

An app left in **Testing** status loses access after seven days, and Google
does not explain why. Borg then shows the calendar as disconnected. Publish
the app (it can stay unverified for your own account) or connect again.

### ICS feed

Any calendar's secret address works:

- Google: *Settings → your calendar → Secret address in iCal format*.
- Outlook: *Settings → Calendar → Shared calendars → Publish a calendar*.
- iCloud: a public calendar's `webcal://` link.

The address is a secret, because anyone holding it can read the whole
calendar. Borg stores it as one and never prints it. Only `https://` and
`webcal://` addresses on the public internet are accepted. Recurring events,
moved and cancelled occurrences, all-day events, and Outlook's Windows
time-zone names are all handled.

The cache refreshes every five minutes while meetings are on. Turning meetings
off stops all calendar traffic.

## Asking about a meeting

Each meeting page ends in an **Ask anything** box. It opens a chat that names
the meeting. The agent reads meetings through the read-only `meetings` tool,
whose actions are `list`, `search` and `get`. The tool is offered only while
meetings are on, so an install that never uses them pays nothing for its
schema. It also works in any chat: "what did we decide in Tuesday's standup?"

## macOS permissions

- **Microphone**, asked the first time you take notes.
- **System Audio Recording**, asked the first time the panel captures the
  other side. If it is refused, or on Linux, Borg records the microphone only
  and says so in the panel.

## Settings

| Key | Default | Meaning |
|---|---|---|
| `meetings.enabled` | `false` | The feature, the sidebar item and the `meetings` tool |
| `meetings.ask_before_recording` | `true` | Off: calendar meetings start recording on their own |
| `meetings.keep_audio` | `false` | Keep each track's WAV next to the transcript |
| `meetings.dir` | `recordings/meetings` under `~/.borg` | Where meeting folders go; `~` expands |
| `meetings.calendar.ics_url` | unset | The ICS feed address, stored as a secret |
| `meetings.calendar.google` | unset | The Google sign-in, written by **Connect**, never typed |

## Platforms

| | Detection | Capture | Calendar |
|---|---|---|---|
| macOS 14+ | yes | mic + system audio | yes |
| Windows, Linux | not yet | the desktop panel works, but nothing opens it yet | yes |
| Browser / TUI | no | no; settings, notes and summaries only | yes |

Detection is the only way a meeting starts in this version, so recording works
on a Mac only. The calendar, past meetings, summaries and the `meetings` tool
work everywhere.
