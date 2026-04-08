# Getting Started

Borg is an AI personal assistant agent built in Rust. The agent writes its own tools at runtime rather than relying on a static plugin framework.

## Prerequisites

- **LLM API key**: One of `OPENROUTER_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `DEEPSEEK_API_KEY`, `GROQ_API_KEY`, or a running Ollama instance (no key needed)
- **Linux**: `bwrap` (bubblewrap) for tool sandboxing — install via your package manager
- **macOS**: `sandbox-exec` is included with the OS

## Installation

### Quick install (recommended)

```sh
curl -fsSL https://raw.githubusercontent.com/borganization/borg/main/scripts/install.sh | bash
```

The installer detects OS/arch, downloads a pre-built binary from GitHub Releases, verifies checksums, installs to `~/.local/bin/`, and runs `borg init` for first-time setup.

### Build from source

Requires the [Rust toolchain](https://rustup.rs/):

```sh
git clone https://github.com/borganization/borg.git
cd borg
cargo build --release
```

The binary is located at `target/release/borg`.

## Setup

1. Set your API key (any one of the supported providers):

```sh
export OPENROUTER_API_KEY="sk-or-..."
# or
export ANTHROPIC_API_KEY="sk-ant-..."
# or
export OPENAI_API_KEY="sk-..."
# or
export GEMINI_API_KEY="..."
# or
export DEEPSEEK_API_KEY="..."
# or
export GROQ_API_KEY="..."
```

Or add it to your shell profile / `.env` file. See `.env.example` for the template. The provider is auto-detected based on which key is set. Ollama requires no API key — just a running instance.

2. Initialize with the onboarding wizard:

```sh
borg init
```

This launches an interactive wizard that walks you through setup:

1. **Welcome** — Your name + agent name
2. **Security** — Security warning acknowledgment (required)
3. **Provider** — Select LLM provider (OpenRouter, OpenAI, Anthropic, Gemini, DeepSeek, Groq, Ollama)
4. **Model** — Select model for the chosen provider
5. **API Key** — Enter API key (auto-detects existing keys)
6. **Channels** — Configure messaging channels (Telegram, Slack, Discord, etc.)
7. **Summary** — Review all settings including defaults, confirm and launch

Defaults applied automatically: Professional personality, recommended model per provider, 1M token/month budget, gateway at 127.0.0.1:7842, strict sandbox. Customize via `borg settings`.

This creates `~/.borg/` with your personality, memory files, and SQLite database:

```
~/.borg/
├── IDENTITY.md       # Personality prompt
├── MEMORY.md         # Memory index
├── memory/           # Topic-specific memories
├── scripts/          # Agent-created scripts (HMAC-verified)
├── skills/           # Bundled + user-created skills
├── logs/             # Daily JSONL debug logs
├── cache/
└── borg.db           # SQLite database (sessions, tasks, settings, embeddings, config)
```

## Usage

### Interactive TUI (default)

```sh
borg
# or
borg start
```

Launches the full TUI with markdown rendering, slash commands, and an auto-started gateway for messaging channels. Use slash commands like `/compact`, `/undo`, `/settings`, `/plugins`, `/pairing`, and `/doctor`.

### One-shot query

```sh
borg ask "What's the weather in Tokyo?"
```

Send a single message, get a response, and exit.

Flags:
- `--yes` / `-y` — auto-approve all tool executions (skip confirmation prompts)
- `--json` / `-j` — output response as JSON

### Daemon mode

```sh
borg daemon
```

Run the agent in the background for scheduled tasks, heartbeat check-ins, and messaging gateway without the interactive TUI.

### System service

```sh
borg service install    # install as a system service (launchd/systemd)
borg service uninstall  # remove the service
borg service status     # check service status
```

### All CLI commands

| Command | Description |
|---------|-------------|
| `borg` / `borg start` | Interactive TUI with auto-gateway (default) |
| `borg ask "message"` | One-shot query |
| `borg init` | Interactive onboarding wizard |
| `borg doctor` | Run diagnostics |
| `borg daemon` | Run background service |
| `borg gateway` | Start webhook gateway standalone |
| `borg poke` | Trigger immediate heartbeat check-in |
| `borg tasks list/create/run/runs/status/pause/resume/delete` | Manage scheduled tasks |
| `borg pairing list/approve/revoke/approved` | Manage sender access |
| `borg settings get/set/unset` | Manage settings |
| `borg logs` | Show conversation history |
| `borg usage` | Token usage and cost breakdown |
| `borg add <name>` | Set up an integration's credentials |
| `borg remove <name>` | Remove an integration's credentials |
| `borg plugins` | List available integrations |
| `borg away [message]` | Enable auto-reply |
| `borg available` | Disable auto-reply |
| `borg stop` / `borg restart` | Daemon lifecycle |
| `borg service install/uninstall/status` | System service management |
| `borg uninstall` | Delete all Borg data |

## What happens when you chat

1. Your message is added to the conversation history
2. A system prompt is assembled from: `IDENTITY.md` + security policy + current time + memory context (semantic search) + skills context
3. The message is streamed to the LLM via the configured provider
4. If the LLM responds with tool calls, each tool is executed and results are fed back
5. The loop continues until the LLM responds with text only
6. Each message is persisted to SQLite for crash recovery

## Next steps

- [Configuration](configuration.md) — customize the provider, model, timeouts, and all settings
- [Skills](skills.md) — instruction bundles for teaching the agent CLI workflows
- [Memory](memory.md) — how the agent remembers things across sessions
- [Heartbeat](heartbeat.md) — proactive check-ins and daemon mode
- [Architecture](architecture.md) — how the codebase is structured
- [Providers](providers/) — setup guides for Telegram, Slack, Discord, and more
