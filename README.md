# Borg

[![CI](https://github.com/borganization/borg/actions/workflows/ci.yml/badge.svg)](https://github.com/borganization/borg/actions/workflows/ci.yml)
[![Release](https://github.com/borganization/borg/actions/workflows/release.yml/badge.svg)](https://github.com/borganization/borg/releases)
[![Coverage](https://codecov.io/gh/borganization/borg/branch/main/graph/badge.svg)](https://codecov.io/gh/borganization/borg)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

**Personal AI assistant that runs locally, remembers you, and gets better over time.**

## Quick Start

```sh
curl -fsSL https://raw.githubusercontent.com/borganization/borg/main/scripts/install.sh | bash
borg
```

The installer detects your OS, downloads the right binary, and walks you through setup. Then run `borg` to start an interactive conversation.

Or download manually from [Releases](https://github.com/borganization/borg/releases), or [build from source](CONTRIBUTING.md#development-setup).

## Commands

**Core**

| Command     | What it does                                     |
| ----------- | ------------------------------------------------ |
| `/plugins`  | Add, remove, and manage plugins and channels     |
| `/schedule` | Manage scheduled prompts, scripts, and workflows |
| `/projects` | Browse and switch between projects               |
| `/settings` | Configure any setting from a popup               |
| `/model`    | Switch LLM provider and model                    |
| `/memory`   | Inspect long-term memory context                 |
| `/migrate`  | Import from another agent                        |

**Conversation**

| Command    | What it does                                                 |
| ---------- | ------------------------------------------------------------ |
| `/btw <q>` | Ask a side question using current context, no history impact |
| `/poke`    | Trigger an immediate heartbeat check-in                      |
| `/usage`   | Token and cost usage for the session                         |

**Personality**

| Command      | What it does                                            |
| ------------ | ------------------------------------------------------- |
| `/evolution` | See how your Borg has evolved over time                 |
| `/xp`        | Show XP summary and recent feed                         |
| `/stats`     | Borg vitals — stability, focus, sync, growth, happiness |
| `/card`      | Print a shareable ASCII card of your Borg               |

**Maintenance**

| Command   | What it does                      |
| --------- | --------------------------------- |
| `/doctor` | Run diagnostics on Borg and host environment |
| `/update` | Update Borg to the latest release |

...and many more. Type `/` in a conversation to browse them all.

## Plugins

Connect Borg to the apps you already use — all managed in-app with `/plugins`:

**Available:** Telegram, Slack, Discord, Teams, Google Chat, Twilio (WhatsApp + SMS), Gmail, Outlook, Google Calendar, Notion, Linear. iMessage works automatically on macOS.

Any skill can be added as a plugin too — drop in a skill and Borg learns a new capability on the spot.

## Documentation

See the [docs/](docs/) directory for detailed guides on configuration, architecture, memory, skills, tools, sandboxing, and provider setup.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup, code style, and PR guidelines.

## Security

See [SECURITY.md](SECURITY.md) for the security architecture, vulnerability reporting, and response policy.

## License

[MIT](LICENSE)
