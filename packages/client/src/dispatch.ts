import { SERVICES, type ServiceName } from "./services.js";
import { BorgError, GrpcStatus, type MethodPath } from "./transport.js";

/**
 * Shared plumbing for the nice-grpc transports.
 *
 * `nice-grpc` and `nice-grpc-web` expose the same `createClient(definition, channel)`
 * surface, so both transports differ only in which channel they build and which
 * services they may reach. Everything below is written once for both.
 */

/** A nice-grpc client: camelCase methods returning promises or async iterables. */
type RawClient = Record<string, (request: unknown, options?: unknown) => unknown>;

/** `borg.status.v1.Status/GetVitals` → the camelCase key ts-proto generated. */
export function resolveMethod(
  clients: Map<string, RawClient>,
  path: MethodPath,
): { client: RawClient; key: string } {
  const slash = path.lastIndexOf("/");
  const fullName = path.slice(0, slash);
  const rpcName = path.slice(slash + 1);

  const client = clients.get(fullName);
  if (!client) {
    throw new BorgError(
      GrpcStatus.Unimplemented,
      `service ${fullName} is not reachable from this surface`,
      path,
    );
  }

  const key = rpcName.charAt(0).toLowerCase() + rpcName.slice(1);
  if (typeof client[key] !== "function") {
    throw new BorgError(GrpcStatus.Unimplemented, `unknown method ${path}`, path);
  }

  return { client, key };
}

/** Indexes clients by proto full name so `resolveMethod` can find them by path. */
export function indexClients(
  names: readonly ServiceName[],
  create: (definition: (typeof SERVICES)[ServiceName]) => RawClient,
): Map<string, RawClient> {
  const clients = new Map<string, RawClient>();
  for (const name of names) {
    const definition = SERVICES[name];
    clients.set(definition.fullName, create(definition));
  }
  return clients;
}

/** Normalizes anything thrown by nice-grpc into a `BorgError` with its status intact. */
export function toBorgError(error: unknown, path: MethodPath): BorgError {
  if (error instanceof BorgError) return error;

  const e = error as { code?: unknown; details?: unknown; message?: unknown };
  const code = typeof e?.code === "number" ? e.code : GrpcStatus.Internal;
  const message =
    (typeof e?.details === "string" && e.details) ||
    (typeof e?.message === "string" && e.message) ||
    "unknown daemon error";

  return new BorgError(code, message, path);
}
