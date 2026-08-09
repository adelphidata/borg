# Borgbase

Electron desktop client for Borg (the Rust agent at `../borg`). React + TypeScript renderer built with Vite; Electron main process bundled with Bun. Currently renders mock data — `borgd` (gRPC daemon) integration is planned.

## Commands

```sh
bun install          # install deps (also installs husky git hooks)
bun run dev          # Vite dev server (:5173) + Electron with hot reload
bun run build        # production build → dist/ (renderer) + dist-electron/ (main)
bun run start        # build + launch production app
bun run typecheck    # tsc --noEmit
bun run format       # prettier --write .
```

No test suite yet.

## Structure

- `electron/main.ts` — window creation; loads `VITE_DEV_SERVER_URL` in dev, `dist/index.html` in prod
- `electron/preload.ts` — context-isolated bridge, exposes `window.borg`
- `src/main.tsx` — renderer entry
- `src/app/routes.tsx` — **single source of truth for navigation**: Sidebar, mobile TabBar, and router all render from the `routes` array. Add a page there and it appears everywhere.
- `src/features/<page>/` — one folder per page
- `src/components/` — shared UI
- `src/lib/theme.tsx` — light/dark ThemeProvider (persisted to localStorage under `borg.theme`)
- `src/lib/useIsMobile.ts` — responsive breakpoint hook; mobile gets a bottom TabBar instead of Sidebar
- `src/data/mock.ts` — dummy data mirroring the design mockup; replace with real daemon data later

## Conventions

- Package manager is **Bun** (`bun install`, `bunx`), not npm/yarn/pnpm
- Formatting: Prettier (printWidth 100), enforced on staged files via husky + lint-staged
- TypeScript strict mode; `tsc` is typecheck-only (`noEmit`) — Vite and Bun do the transpiling
- Renderer uses `HashRouter` (file:// compatibility in packaged builds)
- Electron security posture: `contextIsolation: true`, `nodeIntegration: false`; external links open in the system browser via `setWindowOpenHandler`
- Onboarding completion is persisted to localStorage under `borg.onboarded`

## Environment

No environment variables required. `VITE_DEV_SERVER_URL` is set internally by the dev script.
