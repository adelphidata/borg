# Slack Setup

## 1. Create a Slack App

Go to [api.slack.com/apps](https://api.slack.com/apps) and click **Create New App** > **From scratch**.

## 2. Configure Bot Token Scopes

Under **OAuth & Permissions**, add the following Bot Token Scopes:

- `chat:write` — send messages
- `app_mentions:read` — receive @mentions
- `channels:history` — read messages in public channels the bot is in
- `im:history` — read direct messages

Install the app to your workspace and copy the **Bot User OAuth Token** (`xoxb-...`).

The **Signing Secret** is found under **Basic Information** > **App Credentials**.

## 3. Install via Borg

Credentials are stored in your OS keychain (macOS Keychain / Linux `secret-tool`) and wired into the settings database automatically. No manual file editing required.

### TUI (recommended)

```sh
borg
```

Type `/plugins`, find **Slack**, press Space to select, Enter to install, and paste each credential when prompted.

### CLI

```sh
borg add slack
```

You will be prompted for:

- **Bot Token** — from Slack App settings > OAuth & Permissions
- **Signing Secret** — from Slack App settings > Basic Information

## 4. Enable Event Subscriptions

Expose a public URL (e.g. via ngrok):

```sh
ngrok http 7842
```

In your Slack app settings, go to **Event Subscriptions**:

1. Toggle **Enable Events** to On
2. Set the Request URL to `https://your-domain.ngrok-free.app/webhook/slack`
3. Slack will send a challenge request — the gateway handles this automatically
4. Subscribe to bot events:
   - `message.im` — direct messages
   - `app_mention` — @mentions in channels

## 5. Verify

Invite the bot to a channel or send it a direct message. You should get a response from your agent.

## Features

- Direct messages and @mention events
- HMAC-SHA256 request signature verification
- Replay protection (5-minute timestamp window)
- Thread-aware replies (responds in-thread when messaged in a thread)
- Automatic message chunking (4000 char limit)
- Rate-limit retry with exponential backoff
- Bot message filtering (prevents self-reply loops)
- Message deduplication

## Additional configuration

### Channel allowlist

Restrict the bot to specific Slack channels:

```toml
[gateway]
slack_channel_allowlist = ["C01234567", "C89012345"]
```

### Group activation mode

Control how the bot responds in channels (vs DMs):

```toml
[gateway]
group_activation = "mention"   # mention (default) | always
```

- `mention` — only responds when @mentioned in channels; DMs always activate
- `always` — responds to all messages in channels the bot is in

### Access control

```toml
[gateway.channel_policies]
slack = "open"    # trust Slack workspace auth (no pairing needed)
```

Options: `pairing` (default), `open`, `disabled`. See [Configuration](../configuration.md#gateway) for details.
