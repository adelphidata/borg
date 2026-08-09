import type { BorgTransport } from "@borg/client";
import type { NativeBridge } from "@borg/app";

declare global {
  interface Window {
    /** Exposed by electron/preload.ts through the context bridge. */
    borg: {
      platform: NodeJS.Platform;
      transport: BorgTransport;
      native: NativeBridge;
    };
  }
}

export {};
