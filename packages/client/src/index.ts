/**
 * `@borg/client` — the single typed way to reach borgd.
 *
 * Generated from `crates/protocols/proto` by `just gen` and committed, so the
 * Rust server and every TypeScript surface always agree on the wire contract.
 * Regenerate with `just gen`; CI fails on drift.
 *
 * Surfaces pick a transport (`./node` for Electron main, `./web` for the
 * browser) and pass it to `createBorgClient`. Nothing else differs.
 */

export { createBorgClient } from "./client.js";
export type { BorgClient, ServiceClient } from "./client.js";

export { BorgError, GrpcStatus } from "./transport.js";
export type { BorgTransport, MethodPath, StreamHandlers, Unsubscribe } from "./transport.js";

export { SERVICES, WEB_SERVICES } from "./services.js";
export type { ServiceName } from "./services.js";

export * as admin from "./gen/admin.js";
export * as pki from "./gen/pki.js";
export * as session from "./gen/session.js";
export * as status from "./gen/status.js";
export * as telemetry from "./gen/telemetry.js";
