# Borg

[![CI](https://github.com/borganization/borg/actions/workflows/ci.yml/badge.svg)](https://github.com/borganization/borg/actions/workflows/ci.yml)
[![Release](https://github.com/borganization/borg/actions/workflows/release.yml/badge.svg)](https://github.com/borganization/borg/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

**Personal AI assistant that runs locally, remembers you, and gets better over time.**

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/borganization/borg/main/scripts/install.sh | bash
```

The installer detects your OS, downloads the right binary, and walks you through setup.

Or download manually from [Releases](https://github.com/borganization/borg/releases).

Or [build from source](CONTRIBUTING.md#development-setup).

## Usage

```sh
borg                    # start interactive conversation
borg ask "..."          # one-shot question
borg doctor             # check configuration
```

## Plugins

Connect Borg to the apps you already use:

```sh
borg add telegram
borg add gmail
borg plugins            # see all available plugins
```

**Available:** Telegram, Slack, Discord, Teams, Google Chat, Twilio (WhatsApp + SMS), Gmail, Outlook, Google Calendar, Notion, Linear. iMessage works automatically on macOS.

## Commands

| Command | What it does |
|---------|-------------|
| `borg` | Start an interactive conversation |
| `borg ask "..."` | One-shot question |
| `borg add <name>` | Set up a plugin |
| `borg remove <name>` | Remove a plugin |
| `borg plugins` | List all plugins and their status |
| `borg doctor` | Run diagnostics |
| `borg tasks list` | List scheduled tasks |

## Documentation

See the [docs/](docs/) directory for detailed guides on configuration, architecture, memory, skills, tools, sandboxing, and provider setup.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup, code style, and PR guidelines.

## Security

See [SECURITY.md](SECURITY.md) for the security architecture, vulnerability reporting, and response policy.

## License

[MIT](LICENSE)
