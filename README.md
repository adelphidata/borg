![Borg](docs/assets/borg-logo.png)

# Borg

[![CI](https://github.com/adelphidata/borg/actions/workflows/ci.yml/badge.svg)](https://github.com/adelphidata/borg/actions/workflows/ci.yml)
[![Release](https://github.com/adelphidata/borg/actions/workflows/release.yml/badge.svg)](https://github.com/adelphidata/borg/releases)
[![Coverage](https://codecov.io/gh/adelphidata/borg/branch/main/graph/badge.svg)](https://codecov.io/gh/adelphidata/borg)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

**Borg is your personal assistant, employee, developer, analyst, and sidekick. Security-first by default.**

## Quick Start

```sh
curl -fsSL https://raw.githubusercontent.com/adelphidata/borg/main/scripts/install.sh | bash
borg
```

The installer detects your OS, downloads the right binary, and walks you through setup. Then run `borg` to start an interactive conversation.

Or download manually from [Releases](https://github.com/adelphidata/borg/releases), or [build from source](CONTRIBUTING.md#development-setup).

## Desktop App

A native window over the same Borg. Install the CLI first. The app talks to the daemon at `~/.borg/borgd.sock`.

```sh
borg service install     # keep the daemon running (launchd/systemd)
```

Then grab the installer for your OS from [Releases](https://github.com/adelphidata/borg/releases): `.dmg` (macOS), `.exe` (Windows), `.AppImage` or `.deb` (Linux). Builds are not code-signed yet: on macOS open the app once, then allow it under System Settings → Privacy & Security; on Windows choose More info → Run anyway.

From source:

```sh
just web-install         # bun install (Bun, not npm)
just desktop-build       # installer lands in apps/desktop/release/
just desktop-dev         # or run it with hot reload
```

## Commands

**Core**

| Command     | What it does                                     |
| ----------- | ------------------------------------------------ |
| `/plugins`  | Add, remove, and manage plugins and channels     |
| `/schedule` | Manage scheduled prompts and cron jobs           |
| `/settings` | Configure any setting from a popup               |
| `/model`    | Switch LLM provider and model                    |
| `/memory`   | Inspect long-term memory context                 |
| `/migrate`  | Import from another agent                        |

**Evolution**

| Command      | What it does                                            |
| ------------ | ------------------------------------------------------- |
| `/evolution` | See how your Borg has evolved over time                 |
| `/xp`        | Show XP summary and recent feed                         |
| `/stats`     | Borg vitals: stability, focus, sync, growth, happiness  |
| `/card`      | Print a shareable ASCII card of your Borg               |

**Conversation**

| Command    | What it does                                                 |
| ---------- | ------------------------------------------------------------ |
| `/btw <q>` | Ask a side question using current context, no history impact |
| `/poke`    | Trigger an immediate heartbeat check-in                      |
| `/usage`   | Token and cost usage for the session                         |

**Maintenance**

| Command   | What it does                                          |
| --------- | ----------------------------------------------------- |
| `/doctor` | Run diagnostics on Borg and host environment          |
| `/heal`   | Run the full self-healing maintenance sweep on demand |
| `/update` | Update Borg (`--prerelease` for experimental, `--check` to dry-run) |

...and many more. Type `/` in a conversation to browse them all.

## Plugins

Connect Borg to the apps you already use, all managed in-app with `/plugins`:

**Available:** Telegram, Slack, Discord, Teams, Google Chat, Twilio (WhatsApp + SMS), Gmail, Outlook, Google Calendar, Notion, Linear. iMessage works automatically on macOS.

Any skill can be added as a plugin too. Drop in a skill and Borg learns a new capability on the spot.

## Documentation

See the [docs/](docs/) directory for detailed guides on configuration, architecture, memory, skills, tools, sandboxing, and provider setup.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup, code style, and PR guidelines.

## Security

See [SECURITY.md](SECURITY.md) for the security architecture, vulnerability reporting, and response policy.

## License

[MIT](LICENSE)
