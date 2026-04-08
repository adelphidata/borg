# Discord Setup

## 1. Create a Discord Application

Go to the [Discord Developer Portal](https://discord.com/developers/applications) and click **New Application**.

## 2. Create a Bot

Under your application, go to **Bot** and click **Add Bot**. Copy the **Bot Token**.

Under **Privileged Gateway Intents**, enable:

- **Message Content Intent** — required to read message text

## 3. Configure OAuth2

Under **OAuth2** > **URL Generator**:

- Select scopes: `bot`, `applications.commands`
- Select bot permissions: `Send Messages`, `Read Message History`, `Use Slash Commands`

Copy the generated URL and use it to invite the bot to your server.

## 4. Get the Public Key

Under **General Information**, copy the **Public Key** (used for interaction signature verification).

## 5. Install via Borg

Credentials are stored in your OS keychain (macOS Keychain / Linux `secret-tool`) and wired into the settings database automatically. No manual file editing required.

### TUI (recommended)

```sh
borg
```

Type `/plugins`, find **Discord**, press Space to select, Enter to install, and paste each credential when prompted.

### CLI

```sh
borg add discord
```

You will be prompted for:

- **Bot Token** — from Discord Developer Portal > Bot
- **Public Key** — from Discord Developer Portal > General Information

## 6. Set the Interactions Endpoint

Expose a public URL (e.g. via ngrok):

```sh
ngrok http 7842
```

In the Discord Developer Portal, go to **General Information** and set the **Interactions Endpoint URL** to:

```
https://your-domain.ngrok-free.app/webhook/discord
```

Discord will send a verification ping — the gateway handles this automatically.

## 7. Verify

Send a message to your bot in Discord (DM or in a server channel where the bot is present). You should get a response from your agent.

## Features

- Direct messages and server channel messages
- Ed25519 interaction signature verification
- Deferred responses for long-running agent turns
- Slash command support
- Automatic message chunking (2000 char limit)
- Typing indicators

## Additional configuration

### Guild allowlist

Restrict the bot to specific Discord servers:

```toml
[gateway]
discord_guild_allowlist = ["123456789012345678"]
```

### Group activation mode

```toml
[gateway]
group_activation = "mention"   # mention (default) | always
```

### Access control

```toml
[gateway.channel_policies]
discord = "pairing"   # pairing (default) | open | disabled
```
