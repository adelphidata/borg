# Borgbase

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Desktop client for [Borg](https://github.com/adelphidata/borg) — your personal assistant, employee, developer, analyst, and sidekick.

## Overview

Borgbase is an Electron app that puts a graphical face on Borg. Where the main Borg project ships a terminal client (`borg`) and a gRPC daemon (`borgd`), Borgbase provides a desktop UI for conversing with your Borg, managing connections, monitoring health, and adjusting settings. The layout is responsive, so the same UI scales down to mobile-sized windows.

> **Status: early scaffold.** The UI currently renders mock data (`src/data/mock.ts`); wiring to the real `borgd` daemon is planned but not yet implemented.

## Features

- **Conversation** — chat with your Borg
- **Connections** — view and manage channel/plugin connections
- **Health** — daemon and host diagnostics at a glance
- **Settings** — configure Borg from the UI
- **Onboarding** — first-run setup flow
- Light/dark theme, responsive desktop and mobile layouts

## Prerequisites

- [Bun](https://bun.sh) 1.x (package manager and bundler for the Electron main process)
- macOS, Linux, or Windows

## Quick Start

```sh
git clone <repository-url> borgbase
cd borgbase
bun install
bun run dev
```

`bun run dev` builds the Electron main process, starts the Vite dev server on port 5173, and launches Electron pointed at it, with hot reload for the renderer.

## Scripts

| Command                | What it does                                                     |
| ---------------------- | ---------------------------------------------------------------- |
| `bun run dev`          | Dev mode: Vite dev server + Electron with hot reload             |
| `bun run build`        | Production build (renderer to `dist/`, main to `dist-electron/`) |
| `bun run start`        | Build and launch the production app                              |
| `bun run typecheck`    | TypeScript type checking (`tsc --noEmit`)                        |
| `bun run format`       | Format all files with Prettier                                   |
| `bun run format:check` | Check formatting without writing                                 |

## Architecture

```
electron/          Electron main process
  main.ts          Window creation, dev/prod loading
  preload.ts       Context-isolated bridge (window.borg)
src/
  main.tsx         Renderer entry
  app/             App shell and route table (routes.tsx is the single source of truth for navigation)
  features/        One folder per page (conversation, connections, health, settings, onboarding)
  components/      Shared UI (Sidebar, TabBar, PageChrome, icons, ...)
  lib/             Theme provider, responsive hooks
  data/            Mock data (to be replaced by daemon integration)
  styles/          Global CSS
```

The renderer is a React SPA using `HashRouter`. The Electron main process loads the Vite dev server in development (`VITE_DEV_SERVER_URL`) and the built `dist/index.html` in production. The preload script exposes a minimal `window.borg` API with context isolation enabled and node integration disabled.

## Configuration

No environment variables are required. `VITE_DEV_SERVER_URL` is set internally by the dev script.

## Development

```sh
bun install          # installs deps and git hooks (husky)
bun run dev          # develop with hot reload
bun run typecheck    # type check
bun run format       # format with Prettier
```

A pre-commit hook (husky + lint-staged) formats staged files with Prettier automatically.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[MIT](LICENSE)
