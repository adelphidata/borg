# Voice & phone

Borg can listen (speech-to-text), speak (text-to-speech), and place phone calls.
Each capability picks its provider from a registry, so you can switch vendors
without code changes.

> See also: [Getting started](./getting-started.md#running-fully-local-no-cloud-keys) for
> local STT/TTS command templates.

## Settings page

Open **Settings → Advanced → Voice & Media → Voice & phone** (`/settings/voice`
in the web and desktop apps). The page has three sections:

| Section        | Switch                      | Settings it writes                                                                        |
| -------------- | --------------------------- | ----------------------------------------------------------------------------------------- |
| Voice input    | Talk to Borg                | `audio.enabled`, first row of `audio.models`                                              |
| Spoken replies | Read replies aloud          | `voice.auto_tts`; the Voice picker writes `tts.enabled` and the first row of `tts.models` |
| Phone calls    | Provider (Off turns it off) | `telephony.*`                                                                             |

The page edits only the first row of a chain. Fallback rows and fields the page
does not show stay as they are, so a hand-built chain survives. Switching the
provider starts a fresh first row, because a voice id or model from one vendor
fails on another.

Each section shows a status line from the daemon: **Ready**, or what is still
missing. The provider lists, key links, default models and default voices all
come from the daemon (`Voice.GetVoiceCatalog`); the app has no list of its own.

Turning **Talk to Borg** on with no provider picks the first transcription
provider a saved chat key already serves, so one switch is the whole setup.
Everyday choices show at once: provider, language, shortcut, hold to talk,
send right away, microphone. Everything else sits under **More options**.
Nothing on this page appears on the main Settings screen; the raw keys are
listed only under Settings → Advanced → Voice & Media.

## Dictation

The composer shows a microphone next to Send whenever the browser can record.
Before voice input is set up, the microphone opens a card that turns it on in
one press (when a saved key can transcribe) or points to Settings. Once ready:

- Click the microphone, or press the shortcut, to record. The box becomes a
  recording bar: a cancel button, a live level meter and a clock. The
  microphone turns into a green check; click it, press the shortcut again, or
  let the held key go to finish. Escape cancels and drops the audio.
- The recording is converted to 16 kHz mono WAV in the browser and sent to
  `Voice.Transcribe`, which runs the `audio.models` chain. The words land in
  the box after whatever was typed, or go straight out when **Send right away**
  is on.
- Any reply has a speaker button that reads it aloud, through `Voice.Synthesize`
  when a provider voice is picked, else through the device's own voice. **Read
  replies aloud** speaks each reply as it finishes.

### Settings

| Setting                     | Default           | Where                                               | What it does                                                                         |
| --------------------------- | ----------------- | --------------------------------------------------- | ------------------------------------------------------------------------------------ |
| `voice.dictation_shortcut`  | `mod+shift+space` | Shortcut                                            | A key combination, a lone modifier (`altright`), or a mouse side button (`mouse4`)   |
| `voice.hold_to_record`      | `true`            | Hold to talk                                        | Hold to record and release to finish; a quick tap keeps recording until the next tap |
| `voice.dictation_auto_send` | `false`           | Send right away                                     | Send the words as soon as they are recognised                                        |
| `audio.language`            | unset             | Language                                            | Hint for the transcriber                                                             |
| `audio.prompt`              | unset             | More options → Words Borg should know               | Names and jargon, sent as the Whisper prompt                                         |
| `audio.temperature`         | `0`               | More options → Temperature                          | Decoding temperature for Whisper-shaped providers and the on-device decoder          |
| `audio.beam_size`           | `5`               | More options → Beam size (local only)               | Beam width for whisper.cpp                                                           |
| `audio.no_speech_threshold` | `0.6`             | More options → Silence threshold (local only)       | How sure whisper.cpp must be before dropping a stretch as silence                    |
| `voice.beep_enabled`        | `true`            | More options → Sounds                               | A tick when recording starts and stops                                               |
| `voice.copy_transcript`     | `false`           | More options → Copy to clipboard                    | Keep each transcript on the clipboard                                                |
| `voice.background_shortcut` | `false`           | More options → Listen from the background (desktop) | The chord also works from another app: Borg comes forward and starts listening       |
| `voice.auto_tts`            | `false`           | Read replies aloud                                  | Speak each reply as it finishes                                                      |

The microphone and the device voice are kept in the browser's local storage,
not in the daemon, because they name hardware on this machine.

### Compared with OpenSuperWhisper

The dictation feature set was checked against OpenSuperWhisper's preferences
(`OpenSuperWhisper/Utils/AppPreferences.swift`), which has 24 user-facing
fields. Eighteen have an equivalent here:

| OpenSuperWhisper                                       | Borg                                                  |
| ------------------------------------------------------ | ----------------------------------------------------- |
| selectedEngine, selectedModelPath                      | `audio.models` provider and model                     |
| whisperLanguage                                        | `audio.language`                                      |
| initialPrompt                                          | `audio.prompt`                                        |
| temperature                                            | `audio.temperature`                                   |
| beamSize, useBeamSearch                                | `audio.beam_size`                                     |
| noSpeechThreshold                                      | `audio.no_speech_threshold`                           |
| suppressBlankAudio                                     | an empty transcript is dropped with a notice          |
| addSpaceAfterSentence                                  | spoken words join the draft with one space            |
| autoCopyToClipboard                                    | `voice.copy_transcript`                               |
| autoPasteTranscription                                 | words land in the box, or `voice.dictation_auto_send` |
| key combination, modifierOnlyHotkey, mouseButtonHotkey | `voice.dictation_shortcut`                            |
| holdToRecord with one-tap toggle                       | `voice.hold_to_record`                                |
| playSoundOnRecordStart                                 | `voice.beep_enabled`                                  |
| escCancelWithoutConfirmation                           | Escape cancels a recording                            |
| selectedMicrophone                                     | Microphone                                            |

Not carried over: showTimestamps and useAsianAutocorrect (no use in a chat
box), debugMode, and the three that belong to a menu-bar app that stores
recordings on disk (startHiddenInMenuBar, autoDeleteRecordings, the history
directory). Borg keeps no recordings.

## Keys

A provider's key is looked up in this order:

1. Each credential name in the table below: the credentials map, then the
   environment, then the OS keychain.
2. The key saved in Settings (`credentials.<provider>`), the same store the chat
   providers use. A key field on the Voice & phone page writes here.

So an `OPENAI_API_KEY` that already serves chat also serves OpenAI speech and
transcription.

## Providers

### Speech (`tts.models`)

| id                              | Provider              | Key                                |
| ------------------------------- | --------------------- | ---------------------------------- |
| `elevenlabs`                    | ElevenLabs            | `ELEVENLABS_API_KEY`               |
| `openai`                        | OpenAI                | `OPENAI_API_KEY`                   |
| `edge`                          | Microsoft Edge (free) | none                               |
| `gemini`                        | Google Gemini         | `GEMINI_API_KEY`, `GOOGLE_API_KEY` |
| `xai`                           | xAI Grok              | `XAI_API_KEY`                      |
| `mistral`                       | Mistral Voxtral       | `MISTRAL_API_KEY`                  |
| `minimax`                       | MiniMax               | `MINIMAX_API_KEY`                  |
| `piper`                         | Piper (local)         | none                               |
| `kittentts`, `neutts`, `custom` | Local command         | none; set `command`                |

The voice is chosen in this order: the call's voice, the row's `voice`,
`tts.default_voice`, then the provider's own default.

### Transcription (`audio.models`)

| id           | Provider               | Key                                        |
| ------------ | ---------------------- | ------------------------------------------ |
| `openai`     | OpenAI Whisper         | `OPENAI_API_KEY`                           |
| `elevenlabs` | ElevenLabs Scribe      | `ELEVENLABS_API_KEY`                       |
| `groq`       | Groq                   | `GROQ_API_KEY`                             |
| `deepgram`   | Deepgram               | `DEEPGRAM_API_KEY`                         |
| `mistral`    | Mistral Voxtral        | `MISTRAL_API_KEY`                          |
| `xai`        | xAI Grok               | `XAI_API_KEY`                              |
| `local`      | Whisper on this device | none (needs the `local-stt` build feature) |
| `custom`     | Local command          | none; set `command` with `{audio_file}`    |

### Phone calls (`telephony.provider`)

| id           | Kind             | Secret               | Also needs                                                                  |
| ------------ | ---------------- | -------------------- | --------------------------------------------------------------------------- |
| `twilio`     | Speaks a message | `TWILIO_AUTH_TOKEN`  | `telephony.account_id` (Account SID), `telephony.caller_id` (Twilio number) |
| `vapi`       | Voice agent      | `VAPI_API_KEY`       | `telephony.caller_id` (phone number id); `telephony.agent_id` optional      |
| `bland`      | Voice agent      | `BLAND_API_KEY`      | `telephony.caller_id` and `telephony.agent_id` optional                     |
| `elevenlabs` | Voice agent      | `ELEVENLABS_API_KEY` | `telephony.caller_id` (phone number id), `telephony.agent_id`               |

A plain-call provider reads the message aloud. A voice-agent provider holds the
conversation and gets the message as its task; a saved agent sees it as
`{{task}}` in its prompt.

## The `phone_call` tool

The agent sees `phone_call` only when `telephony.provider` is set. It has three
actions:

- `call`: dial `to` (E.164, like `+14155550123`) with `message`. Returns a call id.
- `status`: progress, transcript and summary for a call id.
- `hang_up`: end a live call.

Guards that run before any dial:

- The number must be valid E.164.
- `telephony.allowed_numbers` (comma-separated numbers or prefixes) limits who
  Borg may call. Empty allows any number.
- `telephony.max_call_secs` (default 300) caps each call's length.

The tool is classified as mutating and plan mode blocks it.

```sh
borg settings set telephony.provider twilio
borg settings set telephony.account_id ACxxxxxxxx
borg settings set telephony.caller_id +14155550100
borg settings set telephony.allowed_numbers "+1415"
```

## Adding a provider

Each capability keeps one static table: `PROVIDERS` in
`crates/cortex/src/media/tts/providers.rs`,
`crates/cortex/src/media/understanding/providers.rs` and
`crates/cortex/src/telephony/providers.rs`. A row holds a `ProviderInfo` (id,
label, key names, key link, base URL, defaults) and a `build` function that
returns the trait object (`TtsProvider`, `TranscriptionProvider` or
`TelephonyProvider`). Add a row and a `build` function; the engine, the settings
catalog and the app pick it up. The registry tests check that ids are unique and
that every row builds.
