# @borg/desktop

Electron shell for Borg, packaged as the **Borg** desktop app (macOS, Windows, Linux).

## Layout

| Path                   | Purpose                                                          |
| ---------------------- | ---------------------------------------------------------------- |
| `electron/main.ts`     | Window creation, dev/prod loading, navigation and link hardening |
| `electron/daemon.ts`   | Finding, probing, and starting `borgd`                           |
| `electron/boot.ts`     | Boot status: one attempt, broadcast to windows, retryable        |
| `electron/ipc.ts`      | Main-process gRPC channel; unary + server-stream forwarding      |
| `electron/preload.ts`  | Context-bridge `BorgTransport` exposed as `window.borg`          |
| `electron/channels.ts` | Shared IPC channel names and wire types                          |
| `src/main.tsx`         | Renderer entry — mounts `<BorgApp/>`                             |
| `src/borg.d.ts`        | `Window.borg` typing                                             |
| `electron-builder.yml` | Packaging config (dmg/zip, nsis, AppImage/deb)                   |

## The IPC hop

```
renderer  ──IPC──▶  electron/main  ──gRPC over unix socket──▶  borgd
```

The renderer runs context-isolated with `nodeIntegration: false`, so it cannot open a socket itself. `ipc.ts` holds the real channel; `preload.ts` re-exposes it as a `BorgTransport`. Authentication is the socket's file permissions, so there is no sign-in screen.

## Starting the daemon

The window is a view onto `borgd`, so the shell brings `borgd` up rather than reporting its absence. On launch `daemon.ts` connects to `~/.borg/borgd.sock`; if nothing answers it resolves a `borgd` binary — `BORG_DAEMON_BIN`, then the packaged `Resources/bin`, then `target/debug` and `target/release` of a checkout, then `PATH` — and spawns it detached. Each step is published on `borg:native:boot:status`, and `<BootGate/>` in `@borg/app` renders the wait. A boot faster than 400 ms is never shown; a failed one offers **Try again** and **Look around anyway**.

## Scripts

Beyond the workspace `just` targets:

| Command             | What it does                                          |
| ------------------- | ----------------------------------------------------- |
| `bun run build`     | Renderer to `dist/`, main process to `dist-electron/` |
| `bun run start`     | Build, then launch the production app                 |
| `bun run typecheck` | `tsc --noEmit`                                        |
| `bun run test`      | Vitest over the main-process modules (`electron/`)    |
| `bun run dist`      | Installers via electron-builder into `release/`       |
| `bun run pack`      | Unpacked build (`--dir`), for packaging smoke tests   |

Installers come from `.github/workflows/release.yml`, in the same `v*` release as the CLI. Artifacts are currently **unsigned** (macOS builds are ad-hoc signed). Add the `MAC_CSC_LINK`/`MAC_CSC_KEY_PASSWORD`, `APPLE_ID`/`APPLE_APP_SPECIFIC_PASSWORD`/`APPLE_TEAM_ID`, and `WIN_CSC_LINK`/`WIN_CSC_KEY_PASSWORD` repository secrets and the same workflow signs and notarizes.
