# apps/desktop

Local rules only. Workspace layout, the transport seam, `just` targets, and release trains are in the root `CLAUDE.md`.

**Read "UX — Progressive Disclosure" in the root `CLAUDE.md` before you add UI here.** It is the design thesis for this surface.

## Invariants

**The renderer never touches Node.** `contextIsolation: true`, `nodeIntegration: false`. The gRPC channel lives in `electron/ipc.ts`.

**`window.borg` is `{ platform, transport, native }` — and nothing else.** `native` is the OS-affordance bridge (menu commands, notifications, dock badge, updater) declared as `NativeBridge` in `packages/app/src/lib/native.ts`. It may carry only things borgd has no part in, and never a Node API, a filesystem path, or a shell. Anything the daemon can answer belongs on the transport instead.

Feature code must not read `window.borg`. It calls `useNative()`, which hands back the real bridge on desktop and `NO_NATIVE` no-ops in the browser — that is what keeps `packages/app` identical on both surfaces. Adding a capability means extending `NativeBridge` _and_ giving `NO_NATIVE` a no-op for it, or the web build breaks at runtime rather than at the type level.

**The desktop app installs and runs with zero setup.** The unix socket is the auth, so no token screen, no config file, no terminal step. That extends to the daemon itself: `electron/boot.ts` probes `borgd.sock` on launch and starts `borgd` when nothing answers, so the app never tells anyone to open a terminal. A daemon it started is left running on quit — scheduled tasks and channels are the product, and closing a window is not a request to stop being an assistant. Native affordances (menus, notifications, dock badge) should make the app feel like a Mac app, not a browser in a frame — but each one still needs a `NO_NATIVE` no-op, so it can never be the only way to reach a feature.

**Errors cross IPC as data.** An `Error` instance loses its gRPC status through structured clone. Keep the `UnaryReply` / `StreamEvent` envelopes in `channels.ts` and rehydrate `BorgError` on the preload side.

**Resolve paths with `app.getAppPath()`, never `__dirname`.** Bun's bundler rewrites `__dirname` to the _source_ directory, which pointed the preload path at a file that exists only in `dist-electron/`. This has regressed once.

**Navigation hardening is security, not polish.** `setWindowOpenHandler` denies every in-app window; `will-navigate` compares exact origin (dev) or exact file href (prod) — a prefix check would let any local `file://` path count as internal. `shell.openExternal` accepts `http:`/`https:` only.

## Gotchas

- A new IPC channel means editing all three of `channels.ts`, `ipc.ts`, and `preload.ts`. A renamed channel is a type error only because `CHANNELS` is shared — keep it that way.
- Every server stream in `ipc.ts` must be cancellable and removed from the `streams` map on error and on complete, or a window reload leaks subscriptions.
- `just desktop-dev` builds the main process once up front. Edits under `electron/` need a restart; only the renderer hot-reloads.
- Boot statuses are user-facing copy: plain language, and never a path, a command, or a code. Paths belong in `console.error`, which lands in the app's stderr.
- `bun run pack` is the cheap check that a packaging change works — `bun run dist` builds full installers and is slow.

## Testing

`bun run --cwd apps/desktop test` (or `just web-test` for every package). The suite runs in node and covers `electron/` only — the renderer is `@borg/app` and has its own jsdom suite.

**Line coverage stays above 90%**, enforced by the thresholds in `vitest.config.mts`. All new or modified code needs corresponding test coverage. A change that adds a channel, a `NativeBridge` capability, or a `NO_NATIVE` no-op is not done until a test covers it.

`electron/test/electron-stub.ts` stands in for the `electron` module: `vi.mock("electron", () => import("./test/electron-stub"))`. Two things about it are load-bearing:

- Its state lives on a `globalThis` singleton. `main.ts` does its wiring at import time, so testing it means re-importing under `vi.resetModules()` — which hands the code under test a *second* copy of the stub. Without the shared store the test would read an empty registry while the code wrote to another one.
- `ipcMain.handle` really keeps a registry, because the `dispose()` paths are tested by asserting the handler is gone.
