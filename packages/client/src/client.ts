import { SERVICES, type ServiceName } from "./services.js";
import type { BorgTransport, StreamHandlers, Unsubscribe } from "./transport.js";

/* ── Structural views of the ts-proto generated shapes ─────────────────────── */

/** Matches the generated `MessageFns<T>` without importing from one arbitrary module. */
interface Codec<T> {
  decode(input: never, length?: number): T;
}

type Decoded<C> = C extends { decode(input: never, length?: number): infer T } ? T : never;

type Builtin = Date | Uint8Array | string | number | boolean | undefined;
type DeepPartial<T> = T extends Builtin
  ? T
  : T extends ReadonlyArray<infer U>
    ? Array<DeepPartial<U>>
    : T extends object
      ? { [K in keyof T]?: DeepPartial<T[K]> }
      : Partial<T>;

interface MethodDefinition {
  readonly name: string;
  readonly requestType: Codec<unknown>;
  readonly requestStream: boolean;
  readonly responseType: Codec<unknown>;
  readonly responseStream: boolean;
}

interface ServiceDefinition {
  readonly fullName: string;
  readonly methods: { readonly [k: string]: MethodDefinition };
}

/* ── The typed client ──────────────────────────────────────────────────────── */

/**
 * A service's methods, each already bound to the surface's transport.
 * Server-streaming RPCs take handlers and return an unsubscribe; the rest
 * return a promise.
 */
export type ServiceClient<D extends ServiceDefinition> = {
  [K in keyof D["methods"]]: D["methods"][K]["responseStream"] extends true
    ? (
        request: DeepPartial<Decoded<D["methods"][K]["requestType"]>>,
        handlers: StreamHandlers<Decoded<D["methods"][K]["responseType"]>>,
      ) => Unsubscribe
    : (
        request: DeepPartial<Decoded<D["methods"][K]["requestType"]>>,
        signal?: AbortSignal,
      ) => Promise<Decoded<D["methods"][K]["responseType"]>>;
};

export type BorgClient = {
  [S in ServiceName]: ServiceClient<(typeof SERVICES)[S]>;
};

/**
 * Binds every borgd RPC to one transport.
 *
 * The result is identical on desktop and web — only the transport differs — so
 * feature code imports this and never learns which surface it is running on.
 */
export function createBorgClient(transport: BorgTransport): BorgClient {
  const client = {} as Record<string, Record<string, unknown>>;

  for (const [service, definition] of Object.entries(SERVICES) as [
    ServiceName,
    ServiceDefinition,
  ][]) {
    const methods: Record<string, unknown> = {};

    for (const [key, method] of Object.entries(definition.methods)) {
      const path = `${definition.fullName}/${method.name}`;

      methods[key] = method.responseStream
        ? (request: unknown, handlers: StreamHandlers<unknown>) =>
            transport.serverStream(path, request, handlers)
        : (request: unknown, signal?: AbortSignal) => transport.unary(path, request, signal);
    }

    client[service] = methods;
  }

  return client as BorgClient;
}
