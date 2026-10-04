import { AdminDefinition } from "./gen/admin.js";
import { PkiDefinition } from "./gen/pki.js";
import { SessionDefinition } from "./gen/session.js";
import { StatusDefinition } from "./gen/status.js";
import { TelemetryDefinition } from "./gen/telemetry.js";
import { VoiceDefinition } from "./gen/voice.js";

/**
 * The borgd services a UI may call, keyed by the name used everywhere else.
 *
 * `capability.Channel` is deliberately absent: it is bidirectional streaming,
 * which gRPC-Web cannot carry, and it is an agent-to-agent channel rather than
 * a UI surface. Adding it here would make `apps/web` fail at runtime only.
 */
export const SERVICES = {
  admin: AdminDefinition,
  pki: PkiDefinition,
  session: SessionDefinition,
  status: StatusDefinition,
  telemetry: TelemetryDefinition,
  voice: VoiceDefinition,
} as const;

export type ServiceName = keyof typeof SERVICES;

/** Services reachable from the browser. `pki` bootstraps pairing over the socket only. */
export const WEB_SERVICES = [
  "admin",
  "session",
  "status",
  "telemetry",
  "voice",
] as const satisfies readonly ServiceName[];
