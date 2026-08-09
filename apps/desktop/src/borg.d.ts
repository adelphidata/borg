import type { BorgTransport } from "@borg/client";

declare global {
  interface Window {
    /** Exposed by electron/preload.ts through the context bridge. */
    borg: {
      platform: NodeJS.Platform;
      transport: BorgTransport;
    };
  }
}

export {};
