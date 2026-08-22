# apps/web

Local rules only. Workspace layout, the transport seam, build order, and `just` targets are in the root `CLAUDE.md`.

**Read "UX — Progressive Disclosure" in the root `CLAUDE.md` before you add UI here.** It is the design thesis for this surface.

## Invariants

**`base` stays `/ui/`.** The gateway serves the SPA under that prefix (`crates/gateway/src/webui/`). Changing it breaks asset URLs in the embedded build while dev mode keeps working — a failure you only see after `cargo build`.

**Auth is a client token, and only here.** `TokenGate` collects a token (`borg client-token issue <label>`) and the web transport sends it on every call. The desktop surface must never grow an equivalent prompt; its unix socket is already the proof.

**The token prompt is the only setup step.** `TokenGate` is the first thing a new user meets, so it must explain itself in one sentence and name the command that produces a token. No other blocking setup screen may grow in front of the app.

**`TokenGate` sits outside `BorgApp`.** `BorgApp` owns `ThemeProvider`, so the gate has no theme context. It reads the CSS variables the pre-paint script already applied. Keep it context-free.

## Gotchas

- `dist/.gitkeep` is committed so a clean checkout compiles. Do not delete it or add a `dist/**` ignore rule that removes it.
- `just web-dev` proxies `/grpc` to `127.0.0.1:7842` and needs a local `borgd`. Without one, calls fail at the proxy, not in app code — check there first.
- No CDN font or script tags in `index.html`. Fonts come from `@borg/ui/styles/fonts.css`; the SPA must work fully offline.

## Testing

**Line coverage stays above 90%.** All new or modified code needs corresponding test coverage. A change to `TokenGate`, the transport wiring, or any route added here is not done until a test covers it.
