# Telegram Setup

## 1. Create a Bot

Open Telegram, message [@BotFather](https://t.me/BotFather), and run `/newbot`. Copy the bot token it gives you.

## 2. Install via Borg

Credentials are stored in your OS keychain (macOS Keychain / Linux `secret-tool`) and wired into the settings database automatically. No manual file editing required.

### TUI (recommended)

```sh
borg
```

Type `/plugins`, find **Telegram**, press Space to select, Enter to install, and paste your bot token when prompted.

### CLI

```sh
borg add telegram
```

You will be prompted for:

- **Bot Token** — from @BotFather

## 3. Choose a Mode

### Webhook mode (recommended for production)

Expose the gateway publicly (e.g. via ngrok):

```sh
ngrok http 7842
```

Set `gateway.public_url`:

```sh
borg settings set gateway.public_url "https://your-domain.ngrok-free.app"
```

Borg will automatically register `https://your-domain.ngrok-free.app/webhook/telegram` with Telegram on startup.

### Polling mode (no public URL needed)

If `public_url` is not set, the gateway automatically uses long-polling via `getUpdates`. No port forwarding or public server required.

## 4. Verify

Open your Borg in Telegram and send `/start`. You should get a response from your agent.

## Features

- Text messages, edited messages, callback queries
- Photo, document, video, audio, voice, and sticker messages (with placeholders)
- Audio message transcription (when `[audio] enabled = true`)
- Per-chat voice reply mode via `/voice`
- Forum/topic support in supergroups
- Automatic message chunking (4000 char limit)
- Markdown-to-HTML response formatting
- Rate-limit retry with exponential backoff
- Update deduplication
- Typing indicators with circuit breaker protection
- Sequential per-chat message processing

## Additional configuration

### Webhook secret (optional)

For webhook mode you can verify incoming requests with a shared secret. Export it before running Borg and the gateway will use it when registering the webhook:

```sh
export TELEGRAM_WEBHOOK_SECRET="your-random-secret-string"
```

The secret is verified on each incoming request via the `X-Telegram-Bot-Api-Secret-Token` header.

### Group activation mode

Control how the bot responds in group chats:

```sh
borg settings set gateway.group_activation mention   # mention (default) | always
```

- `mention` — only responds when @mentioned in groups; DMs always activate
- `always` — responds to all messages in groups the bot is in

### Access control

```sh
borg settings set gateway.channel_policies.telegram pairing   # pairing (default) | open | disabled
```

See [getting-started.md](../getting-started.md) and `borg pairing --help` for the pairing flow.

### Telegram-specific tuning

```sh
borg settings set gateway.telegram_poll_timeout_secs 30            # long-polling timeout
borg settings set gateway.telegram_circuit_failure_threshold 10    # failures before circuit breaker trips
borg settings set gateway.telegram_circuit_suspension_secs 300     # suspension after circuit break
borg settings set gateway.telegram_dedup_capacity 1000             # dedup cache size
```

## Voice and transcription

Inbound voice and audio transcription uses Borg's global audio settings:

```sh
borg settings set audio.enabled true
borg settings set audio.echo_transcript false
```

- `audio.enabled = true` lets Borg download and transcribe Telegram `voice` and `audio` messages before sending them to the agent.
- `audio.echo_transcript = true` also posts the raw transcript back into the chat as a separate Telegram message.

Outbound voice replies use Borg's TTS settings plus a per-chat Telegram mode:

```sh
borg settings set tts.enabled true
borg settings set tts.auto_mode false
```

- `tts.enabled = true` enables TTS capability.
- `tts.auto_mode = true` makes the default mode for chats without an override `TTS`, which means all replies include a voice message.
- `tts.auto_mode = false` makes the default mode `Off`.

Telegram chats can override that default with `/voice`:

- `/voice on` — reply with voice only when you send a Telegram voice message
- `/voice tts` — reply with voice for all messages in that chat
- `/voice off` — text-only replies
- `/voice status` — show the current chat's mode

`/voice on` only triggers on Telegram `voice` messages. Regular Telegram `audio` uploads are still transcribed, but they do not trigger `voice_only` replies.
