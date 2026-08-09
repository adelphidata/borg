# @borg/desktop

Electron shell for Borg, packaged as the **Borg** desktop app (macOS, Windows, Linux).

## Layout

| Path                   | Purpose                                                         |
| ---------------------- | --------------------------------------------------------------- |
| `electron/main.ts`     | Window creation, dev/prod loading, navigation and link hardening |
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

## Scripts

Beyond the workspace `just` targets:

| Command             | What it does                                          |
| ------------------- | ----------------------------------------------------- |
| `bun run build`     | Renderer to `dist/`, main process to `dist-electron/` |
| `bun run start`     | Build, then launch the production app                 |
| `bun run typecheck` | `tsc --noEmit`                                        |
| `bun run dist`      | Installers via electron-builder into `release/`       |
| `bun run pack`      | Unpacked build (`--dir`), for packaging smoke tests   |

Installers come from `.github/workflows/release-desktop.yml`. Artifacts are currently **unsigned** — see the `TODO(signing)` note in `electron-builder.yml`.
