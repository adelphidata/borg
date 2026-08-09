# @borg/web

Browser SPA shell for Borg.

## Layout

| Path                | Purpose                                             |
| ------------------- | --------------------------------------------------- |
| `src/main.tsx`      | Entry — web transport, platform, mounts `<BorgApp/>` |
| `src/TokenGate.tsx` | Client-token sign-in, wraps `<BorgApp/>`             |
| `vite.config.ts`    | `base: "/ui/"`, dev proxy for `/grpc`                |

## Sign-in

A browser cannot present a unix socket or an mTLS client cert, so it authenticates with a client token instead:

```sh
borg client-token issue web
```

`TokenGate` prompts for it on first load and keeps it in `localStorage`; the gateway checks it on every gRPC-Web call. The desktop app has no equivalent — the socket's file permissions already prove the caller.

## Development

`just web-dev` proxies `/grpc` to `127.0.0.1:7842`, so a local `borgd` must be running or every call fails at the proxy rather than in app code.

Fonts ship locally via `@borg/ui/styles/fonts.css`; the SPA works with no network. `vite.config.ts` injects the pre-paint theme script into `index.html`, so the first frame already carries the right `data-theme`.
