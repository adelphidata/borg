# apps/desktop

Local rules only. Workspace layout, the transport seam, `just` targets, and release trains are in the root `CLAUDE.md`.

## Invariants

**The renderer never touches Node.** `contextIsolation: true`, `nodeIntegration: false`. The gRPC channel lives in `electron/ipc.ts`.

**`window.borg` is `{ platform, transport, native }` — and nothing else.** `native` is the OS-affordance bridge (menu commands, notifications, dock badge, updater) declared as `NativeBridge` in `packages/app/src/lib/native.ts`. It may carry only things borgd has no part in, and never a Node API, a filesystem path, or a shell. Anything the daemon can answer belongs on the transport instead.

Feature code must not read `window.borg`. It calls `useNative()`, which hands back the real bridge on desktop and `NO_NATIVE` no-ops in the browser — that is what keeps `packages/app` identical on both surfaces. Adding a capability means extending `NativeBridge` *and* giving `NO_NATIVE` a no-op for it, or the web build breaks at runtime rather than at the type level.

**Errors cross IPC as data.** An `Error` instance loses its gRPC status through structured clone. Keep the `UnaryReply` / `StreamEvent` envelopes in `channels.ts` and rehydrate `BorgError` on the preload side.

**Resolve paths with `app.getAppPath()`, never `__dirname`.** Bun's bundler rewrites `__dirname` to the *source* directory, which pointed the preload path at a file that exists only in `dist-electron/`. This has regressed once.

**Navigation hardening is security, not polish.** `setWindowOpenHandler` denies every in-app window; `will-navigate` compares exact origin (dev) or exact file href (prod) — a prefix check would let any local `file://` path count as internal. `shell.openExternal` accepts `http:`/`https:` only.

## Gotchas

- A new IPC channel means editing all three of `channels.ts`, `ipc.ts`, and `preload.ts`. A renamed channel is a type error only because `CHANNELS` is shared — keep it that way.
- Every server stream in `ipc.ts` must be cancellable and removed from the `streams` map on error and on complete, or a window reload leaks subscriptions.
- `just desktop-dev` builds the main process once up front. Edits under `electron/` need a restart; only the renderer hot-reloads.
- `bun run pack` is the cheap check that a packaging change works — `bun run dist` builds full installers and is slow.
