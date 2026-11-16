# @borg/pbt — property-based browser tests

[Bombadil](https://github.com/antithesishq/bombadil) explores the web and
desktop surfaces at random — clicks, typed text, key presses, resizes,
navigation — and checks a set of properties against every state it reaches.
Example-based suites cover the paths someone thought of; this covers the rest.

Nothing here touches the real daemon or the OS keychain. Each run starts a
scratch `borgd` under `/tmp/borg-pbt/home` (file-backed key store, its own
gateway port, onboarding done, a model endpoint that refuses connections so no
run can spend tokens) and tears it down afterwards.

## Run

```sh
just pbt-web                          # browser SPA, 3 minutes, headless
just pbt-desktop                      # Electron app, 3 minutes, visible window
PBT_TIME_LIMIT=15m just pbt-web       # longer soak
PBT_HEADED=1 just pbt-web             # watch it
PBT_EXIT_ON_VIOLATION=0 just pbt-web  # collect every violation instead of stopping at the first
```

Exit code 2 means a property was violated; 0 is a clean run; 1 is a harness
error. Output (trace, screenshots) lands in `packages/pbt/output/<surface>`.

The web run serves the SPA from a Vite dev server, so it tests the current
source without a Rust rebuild. `PBT_WEB_SERVER=gateway` tests the SPA the
scratch daemon embeds instead. The desktop run uses the built app
(`apps/desktop/dist`), building it when missing; `PBT_BUILD=1` forces a rebuild.

## When something fails

```sh
just pbt-web inspect                                  # timeline, screenshots, state per step
just pbt-web reproduce packages/pbt/output/web        # replay the same actions
just pbt-web shrink packages/pbt/output/web --property noUncaughtExceptions
just pbt-web inspect packages/pbt/output/web-shrunk   # the shortest failing trace found
```

A reproduction needs the same browser, viewport and origin as the original
run. The runner pins all three (the webui token lives in
`/tmp/borg-pbt/webui.token` so the origin URL stays stable between runs);
`PBT_FRESH=1` starts from an empty scratch home when the data itself is the
problem.

## Layout

- `spec/state.ts` — extractors: everything the properties read from the DOM.
  Selectors here are a contract with `packages/app` and `packages/ui`.
- `spec/properties.ts` — properties shared by both surfaces. Invariants use
  `always`; anything that takes a moment (a loader, a nav highlight after a
  route change) is a bounded guarantee, `now(...).implies(eventually(...).within(...))`,
  so a transient state is never a false alarm.
- `spec/actions.ts` — the default action generators, minus clicks that would
  end the run (external links, sign-out, restarts) or run something on the host
  (installers, scheduled runs), plus keys, app chords, direct route jumps,
  viewport changes, and a `Wait` while nothing is actionable.
- `spec/web.ts`, `spec/desktop.ts` — the top-level specifications. They may only
  export properties and action generators.
- `scripts/` — the runners. `lib.sh` holds the scratch daemon, token and
  browser pinning; the per-surface scripts add the SPA server or Electron.

## Writing a property

Extract first, then state the rule over the cell:

```ts
const toasts = extract((state) => state.document.querySelectorAll("[role=status]").length);
export const atMostOneToast = always(() => toasts.current <= 1);
```

Give every export a name that reads as the rule it checks; it is what the
report prints. Keep action generators sound: only return actions that are
valid in the current state, and never export an unfiltered default alongside
its filtered form.
