# iMessage Setup

iMessage integration is macOS-only. It monitors the iMessage database directly — no webhook or external service needed, and no credentials to configure.

## Requirements

- **macOS only** (uses `#[cfg(target_os = "macos")]`)
- **Full Disk Access**: The terminal running Borg needs Full Disk Access to read `~/Library/Messages/chat.db`
- **osascript**: Used to send replies via AppleScript (included with macOS)
- **python3**: Used for message parsing

## 1. Grant Full Disk Access

Go to **System Settings** > **Privacy & Security** > **Full Disk Access** and add your terminal application (Terminal.app, iTerm2, etc.).

## 2. Install via Borg

iMessage is installed through the TUI plugin marketplace. No credentials are required — the plugin installs the channel templates and hooks directly into the local Messages database.

```sh
borg
```

Type `/plugins`, find **iMessage**, press Space to select, and Enter to install. The gateway will start monitoring `~/Library/Messages/chat.db` automatically.

## 3. Verify

Send an iMessage to the Mac running Borg. You should get a response from your agent.

## Features

- Direct iMessage monitoring (no external API)
- Automatic echo detection (won't reply to its own messages)
- Reflection guards (prevents reply loops)
- Self-chat cache (handles messages to/from self)
- Message sanitization

## Additional configuration

### Access control

```sh
borg settings set gateway.channel_policies.imessage pairing   # pairing (default) | open | disabled
```

## Limitations

- macOS only — not available on Linux or Windows
- Requires Full Disk Access permission
- Uses AppleScript for sending, which may prompt for accessibility permissions
