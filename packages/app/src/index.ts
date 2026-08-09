/**
 * `@borg/app` — the Borg product, shared verbatim by every surface.
 *
 * `apps/desktop` and `apps/web` are thin shells: they build a transport, name
 * their platform, and mount `<BorgApp/>`. All routes, pages, and state live here
 * so a feature is written once and appears on both.
 */

export { BorgApp } from "./BorgApp";
export { routes } from "./routes";
export { usePlatform, useTransport } from "./lib/platform";
export { useBorg, useQuery } from "./lib/useDaemon";
export type { Query } from "./lib/useDaemon";
export { signOut } from "./lib/session";
export { DaemonState } from "./components/DaemonState";
export type { Platform } from "./lib/platform";
