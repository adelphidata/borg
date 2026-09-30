import type { NativeBridge } from "@borg/app";
import type { WireTransport } from "../electron/channels";

declare global {
  interface Window {
    /** Exposed by electron/preload.ts through the context bridge. */
    borg: {
      platform: NodeJS.Platform;
      /** Errors arrive as data; wrap with `fromWire` before use. */
      transport: WireTransport;
      native: NativeBridge;
    };
  }
}

export {};
