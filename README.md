# Borg

[![CI](https://github.com/borganization/borg/actions/workflows/ci.yml/badge.svg)](https://github.com/borganization/borg/actions/workflows/ci.yml)
[![Release](https://github.com/borganization/borg/actions/workflows/release.yml/badge.svg)](https://github.com/borganization/borg/releases)
[![Coverage](https://codecov.io/gh/borganization/borg/branch/main/graph/badge.svg)](https://codecov.io/gh/borganization/borg)
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

Connect Borg to the apps you already use — all managed in-app with `/plugins`:

**Available:** Telegram, Slack, Discord, Teams, Google Chat, Twilio (WhatsApp + SMS), Gmail, Outlook, Google Calendar, Notion, Linear. iMessage works automatically on macOS.

Any skill can be added as a plugin too — drop in a skill and Borg learns a new capability on the spot.

## Commands

| Command      | What it does                                                 |
| ------------ | ------------------------------------------------------------ |
| `/evolution` | See how your Borg has evolved over time                      |
| `/xp`        | Show XP summary and recent feed                              |
| `/stats`     | Borg vitals — stability, focus, sync, growth, happiness      |
| `/card`      | Print a shareable ASCII card of your Borg                    |
| `/schedule`  | Manage scheduled prompts, scripts, and workflows             |
| `/plugins`   | Add, remove, and manage plugins and channels                 |
| `/projects`  | Browse and switch between projects                           |
| `/memory`    | Inspect long-term memory context                             |
| `/btw <q>`   | Ask a side question using current context, no history impact |
| `/poke`      | Trigger an immediate heartbeat check-in                      |
| `/migrate`   | Import from another agent                                    |
| `/model`     | Switch LLM provider and model                                |
| `/settings`  | Configure any setting from a popup                           |
| `/usage`     | Token and cost usage for the session                         |
| `/doctor`    | Run diagnostics                                              |
| `/update`    | Update Borg to the latest release                            |

...and many more. Type `/` in a conversation to browse them all.

## Documentation

See the [docs/](docs/) directory for detailed guides on configuration, architecture, memory, skills, tools, sandboxing, and provider setup.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup, code style, and PR guidelines.

## Security

See [SECURITY.md](SECURITY.md) for the security architecture, vulnerability reporting, and response policy.

## License

[MIT](LICENSE)
