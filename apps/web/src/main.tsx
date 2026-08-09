import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { BorgApp, type Platform } from "@borg/app";
import { createWebTransport } from "@borg/client/web";
import { STORAGE_KEYS } from "@borg/ui";
import "@borg/ui/styles/fonts.css";
import "@borg/ui/styles/app.css";

/**
 * Browser shell. All product code lives in `@borg/app`; this file only says
 * *how* this surface reaches borgd.
 *
 * The browser can speak neither borgd's unix socket nor its mTLS TCP port, so
 * it goes through the gateway's gRPC-Web mount and authenticates with a client
 * token (issue one with `borg token issue`).
 */
const platform: Platform = {
  kind: "web",
  transport: createWebTransport({
    getToken: () => localStorage.getItem(STORAGE_KEYS.token),
  }),
  // A browser tab already has window chrome; no drag strip to reserve.
  titleBarDrag: false,
};

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <BorgApp platform={platform} />
  </StrictMode>,
);
